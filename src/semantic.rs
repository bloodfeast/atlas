//! Semantic search over sections: a vector per window of a section (see
//! [`crate::embed::window`]), cached by the hash of the window's text,
//! searched brute force in memory; a section scores by its best window.
//!
//! * Window 0 (the section's own text, cut by the encoder) is cached in
//!   `vec_cache(model, sha, vec)`, keyed by the model id and the section
//!   text's SHA-256 (the `sections.sha` column), so a re-index re-embeds
//!   only sections whose text changed, and a rebuilt database keeps its
//!   vectors. It is pruned of hashes no section has.
//! * Windows 1..n of a long section live beside it, in tables an older
//!   atlas never reads or prunes: `sec_window(model, sha, ix, wsha, rule)`
//!   lists a section's windows (row 0 marks it split, with `wsha = sha`;
//!   `rule` is [`WINDOW_RULES_VERSION`], and a row with an older one is
//!   split again), and `win_vec(model, wsha, vec)` holds their vectors,
//!   keyed by the window text's SHA-256. Windows depend on the model's
//!   tokenizer, so both are per model. Splitting needs the tokenizer, so it
//!   runs in the embedder, not the indexer.
//! * Embedding runs on the caller's thread for a few sections, and on a
//!   background thread ([`Embedder`]) for a cold corpus; answers say how
//!   many sections are still without a vector and how many windows are
//!   still pending.
//! * Search is a dot product of the query vector with every window vector
//!   (unit vectors, so it is the cosine), using the same kernels as the
//!   encoder, reduced to each section's best window; at about ten thousand
//!   windows of 384 floats that is about a millisecond, so no approximate
//!   index.
//! * Hybrid ranks fuse the FTS5 ranking and the vector ranking by
//!   reciprocal rank (`1 / (60 + rank)` summed).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rusqlite::{params, Connection};

use crate::embed::gemm;
use crate::embed::window::{self, WINDOW_RULES_VERSION};
use crate::embed::Model;
use crate::error::Result;

/// The reciprocal-rank-fusion constant.
pub const RRF_K: f64 = 60.0;

/// Windows embedded on the query's own thread before the rest is left to
/// the background embedder (pending windows, not sections, are compared
/// with it).
pub const INLINE_BUDGET: usize = 32;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS vec_cache(
    model TEXT NOT NULL,
    sha TEXT NOT NULL,
    vec BLOB NOT NULL,
    PRIMARY KEY(model, sha)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS win_vec(
    model TEXT NOT NULL,
    wsha TEXT NOT NULL,
    vec BLOB NOT NULL,
    PRIMARY KEY(model, wsha)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS sec_window(
    model TEXT NOT NULL,
    sha TEXT NOT NULL,
    ix INTEGER NOT NULL,
    wsha TEXT NOT NULL,
    rule INTEGER NOT NULL,
    PRIMARY KEY(model, sha, ix)
);
CREATE INDEX IF NOT EXISTS sec_window_wsha ON sec_window(model, wsha);";

/// The text a section is embedded as: its heading path, a newline, its
/// body.
pub fn section_text(heading: &str, body: &str) -> String {
    format!("{heading}\n{body}")
}

/// The cache key of a section: the SHA-256 of [`section_text`].
pub fn section_sha(heading: &str, body: &str) -> String {
    crate::sha256::hex(section_text(heading, body).as_bytes())
}

/// Creates the vector cache tables (additive caches: no store version
/// changes with them, so an older atlas keeps opening the store).
///
/// # Errors
///
/// [`crate::error::Error::Db`] from SQLite.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8], out: &mut Vec<f32>) {
    out.extend(b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
}

/// Sections without a vector for the model: `(sha, text)`, distinct by
/// hash, at most `limit`.
///
/// # Errors
///
/// [`crate::error::Error::Db`] from SQLite.
pub fn pending(conn: &Connection, model: &str, limit: usize) -> Result<Vec<(String, String)>> {
    ensure_schema(conn)?;
    let mut st = conn.prepare_cached(
        "SELECT s.sha, MIN(s.heading_path), MIN(f.body) FROM sections s JOIN sections_fts f ON f.rowid = s.id
         WHERE s.sha <> '' AND NOT EXISTS (SELECT 1 FROM vec_cache v WHERE v.model = ?1 AND v.sha = s.sha)
         GROUP BY s.sha LIMIT ?2",
    )?;
    let rows = st.query_map(params![model, limit as i64], |r| Ok((r.get::<_, String>(0)?, section_text(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?))))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// How many sections lack a vector for the model.
///
/// # Errors
///
/// [`crate::error::Error::Db`] from SQLite.
pub fn missing(conn: &Connection, model: &str) -> Result<usize> {
    ensure_schema(conn)?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT s.sha) FROM sections s WHERE s.sha <> '' AND NOT EXISTS (SELECT 1 FROM vec_cache v WHERE v.model = ?1 AND v.sha = s.sha)",
        [model],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Windows 1..n still to embed: `(sections not yet split, windows split
/// but without a vector)`. SQL only (nothing is tokenized), so it is safe on
/// the query's thread. A section not yet split may turn out to have window
/// 0 only.
///
/// # Errors
///
/// [`crate::error::Error::Db`] from SQLite.
pub fn windows_pending(conn: &Connection, model: &str) -> Result<(usize, usize)> {
    ensure_schema(conn)?;
    let (a, b): (i64, i64) = conn.query_row(
        "SELECT (SELECT COUNT(DISTINCT s.sha) FROM sections s WHERE s.sha <> ''
                 AND s.sha NOT IN (SELECT w.sha FROM sec_window w WHERE w.model = ?1 AND w.ix = 0 AND w.rule = ?2)),
                (SELECT COUNT(DISTINCT w.wsha) FROM sec_window w WHERE w.model = ?1 AND w.ix > 0 AND w.rule = ?2
                 AND w.sha IN (SELECT sha FROM sections)
                 AND NOT EXISTS (SELECT 1 FROM win_vec v WHERE v.model = ?1 AND v.wsha = w.wsha))",
        params![model, WINDOW_RULES_VERSION],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((a as usize, b as usize))
}

/// What one [`embed_pending`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    /// Window-0 vectors embedded (sections that had none).
    pub sections: usize,
    /// Sections split into windows (their `sec_window` rows written; a
    /// section with windows still without a vector has its rows written
    /// again, uncounted).
    pub split: usize,
    /// Window 1..n vectors embedded.
    pub windows: usize,
}

impl Progress {
    /// All the work done: 0 when nothing was pending.
    pub fn total(&self) -> usize {
        self.sections + self.split + self.windows
    }

    /// Vectors embedded.
    pub fn embedded(&self) -> usize {
        self.sections + self.windows
    }
}

/// Sections to split (none, or an older rule) or with split windows still
/// without a vector: `(sha, heading, body, needs split)`, distinct by
/// hash, at most `limit` of each kind.
fn to_window(conn: &Connection, model: &str, limit: usize) -> Result<Vec<(String, String, String, bool)>> {
    let mut out = Vec::new();
    let mut st = conn.prepare_cached(
        "SELECT s.sha, MIN(s.heading_path), MIN(f.body) FROM sections s JOIN sections_fts f ON f.rowid = s.id
         WHERE s.sha <> '' AND s.sha NOT IN (SELECT w.sha FROM sec_window w WHERE w.model = ?1 AND w.ix = 0 AND w.rule = ?2)
         GROUP BY s.sha LIMIT ?3",
    )?;
    for r in st.query_map(params![model, WINDOW_RULES_VERSION, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, true)))? {
        out.push(r?);
    }
    let mut st = conn.prepare_cached(
        "SELECT s.sha, MIN(s.heading_path), MIN(f.body) FROM sections s JOIN sections_fts f ON f.rowid = s.id
         WHERE s.sha IN (SELECT w.sha FROM sec_window w WHERE w.model = ?1 AND w.ix > 0 AND w.rule = ?2
                         AND NOT EXISTS (SELECT 1 FROM win_vec v WHERE v.model = ?1 AND v.wsha = w.wsha))
         GROUP BY s.sha LIMIT ?3",
    )?;
    for r in st.query_map(params![model, WINDOW_RULES_VERSION, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, false)))? {
        out.push(r?);
    }
    Ok(out)
}

/// Embeds up to `limit` pending vectors and stores them: window 0 of
/// sections without one first, then the windows 1..n of sections not yet
/// split under [`WINDOW_RULES_VERSION`] (splitting them) or split with
/// windows still without a vector. A section split again has all its
/// `sec_window` rows replaced, so no row of an older split stays; a window
/// whose text did not change keeps its vector (keyed by the text's hash).
///
/// # Arguments
///
/// * `conn` - a connection (the background embedder's own, or the store's).
/// * `model` - the model.
/// * `limit` - the most vectors this call embeds (and the most sections it
///   splits).
/// * `threads` - encoder threads.
///
/// # Returns
///
/// What was done ([`Progress::total`] is 0 when nothing was pending).
///
/// # Errors
///
/// [`crate::error::Error::StoreTooNew`] (nothing written), or
/// [`crate::error::Error::Db`] from SQLite.
pub fn embed_pending(conn: &mut Connection, model: &Model, limit: usize, threads: usize) -> Result<Progress> {
    let mut done = Progress::default();
    let todo = pending(conn, &model.id, limit)?;
    if !todo.is_empty() {
        let texts: Vec<String> = todo.iter().map(|t| t.1.clone()).collect();
        let vecs = model.embed_many(&texts, threads);
        // `begin` refuses a store a newer atlas upgraded since this one opened it.
        let tx = crate::store::begin(conn)?;
        {
            let mut st = tx.prepare_cached("INSERT OR REPLACE INTO vec_cache(model, sha, vec) VALUES(?1, ?2, ?3)")?;
            for ((sha, _), v) in todo.iter().zip(&vecs) {
                st.execute(params![model.id, sha, to_blob(v)])?;
            }
        }
        tx.commit()?;
        done.sections = todo.len();
    }
    let budget = limit.saturating_sub(done.sections);
    if budget == 0 {
        return Ok(done);
    }
    // Split (on this thread: the tokenizer is fast next to the encoder),
    // then embed the windows that have no vector, up to the budget.
    let secs = to_window(conn, &model.id, budget)?;
    let mut plans: Vec<(String, Vec<String>)> = Vec::new();
    let mut texts: Vec<(String, String)> = Vec::new();
    {
        let mut have = conn.prepare_cached("SELECT 1 FROM win_vec WHERE model = ?1 AND wsha = ?2")?;
        let mut seen = std::collections::HashSet::new();
        for (sha, heading, body, split) in &secs {
            let wins = window::split(&model.tok, model.max_seq, heading, body);
            let mut wshas = vec![sha.clone()];
            for w in &wins[1..] {
                let wsha = crate::sha256::hex(w.text.as_bytes());
                if texts.len() < budget && seen.insert(wsha.clone()) && !have.exists(params![model.id, wsha])? {
                    texts.push((wsha.clone(), w.text.clone()));
                }
                wshas.push(wsha);
            }
            // A section split before is written again too: if what decides a
            // window's text changed without a rule bump (the tokenizer, the
            // section text), its rows now name the windows just embedded.
            done.split += usize::from(*split);
            plans.push((sha.clone(), wshas));
        }
    }
    if plans.is_empty() {
        return Ok(done);
    }
    let vecs = model.embed_many(&texts.iter().map(|t| t.1.clone()).collect::<Vec<_>>(), threads);
    let tx = crate::store::begin(conn)?;
    {
        let mut del = tx.prepare_cached("DELETE FROM sec_window WHERE model = ?1 AND sha = ?2")?;
        let mut ins = tx.prepare_cached("INSERT INTO sec_window(model, sha, ix, wsha, rule) VALUES(?1, ?2, ?3, ?4, ?5)")?;
        for (sha, wshas) in &plans {
            del.execute(params![model.id, sha])?;
            for (ix, wsha) in wshas.iter().enumerate() {
                ins.execute(params![model.id, sha, ix as i64, wsha, WINDOW_RULES_VERSION])?;
            }
        }
        let mut st = tx.prepare_cached("INSERT OR REPLACE INTO win_vec(model, wsha, vec) VALUES(?1, ?2, ?3)")?;
        for ((wsha, _), v) in texts.iter().zip(&vecs) {
            st.execute(params![model.id, wsha, to_blob(v)])?;
        }
    }
    tx.commit()?;
    done.windows = texts.len();
    Ok(done)
}

/// Deletes what no section uses any more: the model's `sec_window` rows
/// whose section hash no section has, then its window-0 vectors whose hash
/// no section has (the same delete an older atlas runs) and its window
/// vectors no `sec_window` row names.
///
/// # Returns
///
/// The vectors deleted.
///
/// # Errors
///
/// [`crate::error::Error::StoreTooNew`] (nothing deleted, no table made), or
/// [`crate::error::Error::Db`] from SQLite.
pub fn prune(conn: &Connection, model: &str) -> Result<usize> {
    // The tables are made inside the checked transaction too.
    crate::store::guarded(conn, || {
        ensure_schema(conn)?;
        conn.execute("DELETE FROM sec_window WHERE model = ?1 AND sha NOT IN (SELECT sha FROM sections)", [model])?;
        let a = conn.execute("DELETE FROM vec_cache WHERE model = ?1 AND sha NOT IN (SELECT sha FROM sections)", [model])?;
        let b =
            conn.execute("DELETE FROM win_vec WHERE model = ?1 AND wsha NOT IN (SELECT wsha FROM sec_window WHERE model = ?1)", [model])?;
        Ok(a + b)
    })
}

/// The window vectors of one model, as one row-major matrix, with the
/// section of each row.
#[derive(Debug, Default)]
pub struct VecIndex {
    /// The model the vectors belong to.
    pub model: String,
    /// Section ids with at least one vector (those with window 0 first, in
    /// id order).
    pub ids: Vec<i64>,
    /// Per matrix row, the index in [`VecIndex::ids`] of its section.
    pub row_sec: Vec<u32>,
    /// `row_sec.len()` rows of `dim` floats.
    pub mat: Vec<f32>,
    /// The vector width.
    pub dim: usize,
    /// Sections that had no window-0 vector when loaded.
    pub missing: usize,
    /// When loaded, sections not yet split into windows.
    pub unsplit: usize,
    /// When loaded, windows 1..n split but without a vector.
    pub windows_pending: usize,
    stamp: Stamp,
}

type Stamp = (i64, i64, i64, i64, i64, i64, i64);

fn stamp(conn: &Connection, model: &str) -> Result<Stamp> {
    Ok(conn.query_row(
        "SELECT (SELECT COUNT(*) FROM sections), (SELECT TOTAL(id) FROM sections), (SELECT MAX(id) FROM sections), (SELECT COUNT(*) FROM vec_cache WHERE model = ?1),
                (SELECT COUNT(*) FROM win_vec WHERE model = ?1), (SELECT COUNT(*) FROM sec_window), (SELECT MAX(rowid) FROM sec_window)",
        [model],
        |r| Ok((r.get(0)?, r.get::<_, f64>(1)? as i64, r.get::<_, Option<i64>>(2)?.unwrap_or(0), r.get(3)?, r.get(4)?, r.get(5)?, r.get::<_, Option<i64>>(6)?.unwrap_or(0))),
    )?)
}

impl VecIndex {
    /// Loads every window vector of every section for the model.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Db`] from SQLite.
    pub fn load(conn: &Connection, model: &str, dim: usize) -> Result<VecIndex> {
        ensure_schema(conn)?;
        let st = stamp(conn, model)?;
        let mut ix = VecIndex { model: model.to_string(), dim, stamp: st, ..VecIndex::default() };
        // Plain scans and key lookups, no sort of the blobs and no join on
        // `sections.sha` (it has no index): the split sections, window 0 of
        // every section (in id order, as before), then every window 1..n,
        // matched to its sections here. A section's rows need not be
        // adjacent: `row_sec` maps each row to its section.
        let split: std::collections::HashSet<String> = {
            let mut q = conn.prepare_cached("SELECT sha FROM sec_window WHERE model = ?1 AND ix = 0 AND rule = ?2")?;
            let rows = q.query_map(params![model, WINDOW_RULES_VERSION], |r| r.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut at: HashMap<i64, u32> = HashMap::new();
        let mut by_sha: HashMap<String, Vec<i64>> = HashMap::new();
        let mut unsplit = std::collections::HashSet::new();
        let mut q = conn.prepare_cached(
            "SELECT s.id, s.sha, v.vec FROM sections s LEFT JOIN vec_cache v ON v.model = ?1 AND v.sha = s.sha ORDER BY s.id",
        )?;
        let mut rows = q.query([model])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let sha: String = r.get(1)?;
            match r.get_ref(2)? {
                rusqlite::types::ValueRef::Blob(b) if b.len() == dim * 4 => {
                    ix.push(&mut at, id, b);
                }
                _ => ix.missing += 1,
            }
            if !sha.is_empty() {
                if !split.contains(&sha) {
                    unsplit.insert(sha.clone());
                }
                by_sha.entry(sha).or_default().push(id);
            }
        }
        // Windows of an older split still score until the section is split again.
        let mut pending = std::collections::HashSet::new();
        let mut q = conn.prepare_cached("SELECT w.sha, w.wsha, w.rule, x.vec FROM sec_window w LEFT JOIN win_vec x ON x.model = ?1 AND x.wsha = w.wsha WHERE w.model = ?1 AND w.ix > 0")?;
        let mut rows = q.query([model])?;
        while let Some(r) = rows.next()? {
            let sha: String = r.get(0)?;
            let Some(ids) = by_sha.get(&sha) else { continue };
            match r.get_ref(3)? {
                rusqlite::types::ValueRef::Blob(b) if b.len() == dim * 4 => {
                    for &id in ids {
                        ix.push(&mut at, id, b);
                    }
                }
                _ if r.get::<_, i64>(2)? == i64::from(WINDOW_RULES_VERSION) => {
                    pending.insert(r.get::<_, String>(1)?);
                }
                _ => {}
            }
        }
        (ix.unsplit, ix.windows_pending) = (unsplit.len(), pending.len());
        Ok(ix)
    }

    fn push(&mut self, at: &mut HashMap<i64, u32>, id: i64, b: &[u8]) {
        let n = self.ids.len() as u32;
        let k = *at.entry(id).or_insert(n);
        if k == n {
            self.ids.push(id);
        }
        self.row_sec.push(k);
        from_blob(b, &mut self.mat);
    }

    /// True when sections, vectors or windows changed since the load.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Db`] from SQLite.
    pub fn stale(&self, conn: &Connection) -> Result<bool> {
        Ok(stamp(conn, &self.model)? != self.stamp)
    }

    /// Windows still to embed when loaded: sections not yet split plus
    /// split windows without a vector (what [`INLINE_BUDGET`] is compared
    /// with, with [`VecIndex::missing`]).
    pub fn pending(&self) -> usize {
        self.missing + self.unsplit + self.windows_pending
    }

    /// The `k` sections most similar to a query vector, best first: each
    /// section scores by its best window, reduced before the cut, so `k`
    /// counts sections.
    ///
    /// # Arguments
    ///
    /// * `isa` - the kernels.
    /// * `q` - the query's unit vector.
    /// * `k` - how many.
    pub fn top(&self, isa: gemm::Isa, q: &[f32], k: usize) -> Vec<(i64, f32)> {
        let mut best = vec![f32::NEG_INFINITY; self.ids.len()];
        for (i, &s) in self.row_sec.iter().enumerate() {
            let d = gemm::dot(isa, q, &self.mat[i * self.dim..(i + 1) * self.dim]);
            let b = &mut best[s as usize];
            if d > *b {
                *b = d;
            }
        }
        let mut scored: Vec<(i64, f32)> = self.ids.iter().copied().zip(best).collect();
        let k = k.min(scored.len());
        if k == 0 {
            return Vec::new();
        }
        scored.select_nth_unstable_by(k - 1, |a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored
    }
}

/// Reciprocal rank fusion of ranked id lists: each id scores
/// `sum 1 / (RRF_K + rank)` (rank from 1) over the lists it appears in.
///
/// # Returns
///
/// Ids, best first (ties by first appearance).
pub fn rrf(lists: &[&[i64]]) -> Vec<i64> {
    let mut score: HashMap<i64, (f64, usize)> = HashMap::new();
    let mut order = 0;
    for l in lists {
        for (r, &id) in l.iter().enumerate() {
            let e = score.entry(id).or_insert_with(|| {
                order += 1;
                (0.0, order)
            });
            e.0 += 1.0 / (RRF_K + (r + 1) as f64);
        }
    }
    let mut v: Vec<(i64, f64, usize)> = score.into_iter().map(|(id, (s, o))| (id, s, o)).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
    v.into_iter().map(|x| x.0).collect()
}

/// A background thread that embeds every pending section and window, then
/// prunes.
#[derive(Debug)]
pub struct Embedder {
    done: Arc<(Mutex<bool>, Condvar)>,
    embedded: Arc<AtomicUsize>,
    failed: Arc<Mutex<Option<String>>>,
}

impl Embedder {
    /// Starts it on its own connection.
    ///
    /// # Arguments
    ///
    /// * `db` - the database file.
    /// * `model` - the shared model.
    /// * `threads` - encoder threads.
    pub fn start(db: PathBuf, model: Arc<Model>, threads: usize) -> Embedder {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let embedded = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(Mutex::new(None));
        let (d, e, f) = (done.clone(), embedded.clone(), failed.clone());
        let run = move || -> Result<()> {
            let mut conn = crate::store::open_raw(&db)?;
            conn.busy_timeout(Duration::from_secs(30))?;
            loop {
                let p = embed_pending(&mut conn, &model, 256, threads)?;
                if p.total() == 0 {
                    break;
                }
                e.fetch_add(p.embedded(), Ordering::Relaxed);
            }
            prune(&conn, &model.id)?;
            Ok(())
        };
        std::thread::Builder::new()
            .name("atlas-embed".into())
            .spawn(move || {
                if let Err(err) = run() {
                    if let Ok(mut g) = f.lock() {
                        *g = Some(err.to_string());
                    }
                }
                if let Ok(mut g) = d.0.lock() {
                    *g = true;
                }
                d.1.notify_all();
            })
            .ok();
        Embedder { done, embedded, failed }
    }

    /// True once every pending section and window is embedded (or it
    /// failed).
    pub fn done(&self) -> bool {
        self.done.0.lock().map(|g| *g).unwrap_or(true)
    }

    /// Blocks until it is done or the timeout passes (a condition-variable
    /// wait).
    ///
    /// # Returns
    ///
    /// True when done.
    pub fn wait(&self, timeout: Duration) -> bool {
        let Ok(g) = self.done.0.lock() else { return true };
        self.done.1.wait_timeout_while(g, timeout, |d| !*d).map(|(g, _)| *g).unwrap_or(true)
    }

    /// Vectors (window 0 and windows 1..n) embedded so far.
    pub fn embedded(&self) -> usize {
        self.embedded.load(Ordering::Relaxed)
    }

    /// The error it stopped on, if any.
    pub fn error(&self) -> Option<String> {
        self.failed.lock().ok().and_then(|g| g.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_fuses_by_rank() {
        assert_eq!(rrf(&[&[1, 2, 3], &[3, 1, 4]]), [1, 3, 2, 4]);
        assert_eq!(rrf(&[&[], &[7]]), [7]);
    }

    #[test]
    fn top_is_ordered_and_capped() {
        let ix = VecIndex {
            ids: vec![10, 11, 12],
            row_sec: vec![0, 1, 2],
            mat: vec![1.0, 0.0, 0.0, 1.0, 0.6, 0.8],
            dim: 2,
            ..VecIndex::default()
        };
        let t = ix.top(gemm::Isa::detect(), &[0.6, 0.8], 2);
        assert_eq!(t.iter().map(|x| x.0).collect::<Vec<_>>(), [12, 11]);
        assert!(ix.top(gemm::Isa::Scalar, &[1.0, 0.0], 0).is_empty());
    }
}

#[cfg(test)]
mod storeguard_tests {
    use super::*;
    use crate::store::skew_support::{dump, make_index_newer};

    /// `prune` on a store a newer atlas upgraded since this one opened it
    /// deletes nothing and makes no table.
    #[test]
    fn prune_checks_the_version_under_the_lock() {
        let c = Connection::open_in_memory().unwrap();
        make_index_newer(&c);
        let before = dump(&c);
        assert!(prune(&c, "m").expect_err("refused").is_store_too_new());
        assert_eq!(dump(&c), before, "no table made");
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c).unwrap();
        c.execute_batch("CREATE TABLE sections(sha TEXT);").unwrap();
        c.execute("INSERT INTO vec_cache(model, sha, vec) VALUES('m', 'gone', x'00')", []).unwrap();
        make_index_newer(&c);
        let before = dump(&c);
        assert!(prune(&c, "m").expect_err("refused").is_store_too_new());
        assert!(c.is_autocommit(), "rolled back");
        assert_eq!(dump(&c), before, "the stale vector stays");
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::store::skew_support::{dump, make_index_newer};

    const OLD_PRUNE: &str = "DELETE FROM vec_cache WHERE model = ?1 AND sha NOT IN (SELECT sha FROM sections)";

    fn model() -> Model {
        Model::parse(&crate::embed::tiny_model_bytes(false, 7), gemm::Isa::Scalar).unwrap()
    }

    fn store() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE sections(id INTEGER PRIMARY KEY, sha TEXT NOT NULL, heading_path TEXT NOT NULL); CREATE VIRTUAL TABLE sections_fts USING fts5(heading_path, body);").unwrap();
        c
    }

    fn add(c: &Connection, id: i64, heading: &str, body: &str) -> String {
        let sha = section_sha(heading, body);
        c.execute("INSERT INTO sections(id, sha, heading_path) VALUES(?1, ?2, ?3)", params![id, sha, heading]).unwrap();
        c.execute("INSERT INTO sections_fts(rowid, heading_path, body) VALUES(?1, ?2, ?3)", params![id, heading, body]).unwrap();
        sha
    }

    /// A body of `n` one-token words cycling through the tiny vocabulary.
    fn long(n: usize, shift: usize) -> String {
        (0..n).map(|i| format!("w{}", (i * 5 + shift) % 24)).collect::<Vec<_>>().join(" ")
    }

    fn all(c: &mut Connection, m: &Model) -> Progress {
        let mut sum = Progress::default();
        loop {
            let p = embed_pending(c, m, 7, 1).unwrap();
            if p.total() == 0 {
                return sum;
            }
            sum.sections += p.sections;
            sum.split += p.split;
            sum.windows += p.windows;
        }
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn put0(c: &Connection, m: &Model, heading: &str, body: &str) {
        let mut s = crate::embed::bert::Scratch::default();
        ensure_schema(c).unwrap();
        c.execute(
            "INSERT INTO vec_cache VALUES(?1, ?2, ?3)",
            params![m.id, section_sha(heading, body), to_blob(&m.embed(&section_text(heading, body), &mut s))],
        )
        .unwrap();
    }

    #[test]
    fn a_long_section_scores_by_its_best_window() {
        let m = model();
        let mut c = store();
        let body = long(100, 0);
        add(&c, 1, "the", &body);
        add(&c, 2, "dog", "cat sat on the mat");
        let p = all(&mut c, &m);
        let wins = window::split(&m.tok, m.max_seq, "the", &body);
        assert!(wins.len() >= 4, "{} windows", wins.len());
        assert_eq!(p, Progress { sections: 2, split: 2, windows: wins.len() - 1 });
        let ix = VecIndex::load(&c, &m.id, m.dim).unwrap();
        assert_eq!((ix.missing, ix.unsplit, ix.windows_pending, ix.pending()), (0, 0, 0, 0));
        assert_eq!(ix.ids, [1, 2]);
        assert_eq!(ix.row_sec.len(), wins.len() + 1);
        let mut s = crate::embed::bert::Scratch::default();
        let tail = m.embed(&wins[wins.len() - 1].text, &mut s);
        let t = ix.top(m.isa, &tail, 5);
        assert_eq!(t.len(), 2, "each section once");
        assert_eq!(t[0].0, 1);
        assert!(t[0].1 > 0.999, "scored by its tail window: {}", t[0].1);
        let head = m.embed(&wins[0].text, &mut s);
        assert!(gemm::dot(m.isa, &head, &tail) < t[0].1);
    }

    #[test]
    fn a_fresh_load_is_not_stale_until_a_window_changes() {
        let m = model();
        let mut c = store();
        add(&c, 1, "the", &long(100, 6));
        all(&mut c, &m);
        let ix = VecIndex::load(&c, &m.id, m.dim).unwrap();
        assert!(!ix.stale(&c).unwrap(), "stale right after a load");
        c.execute("DELETE FROM win_vec WHERE wsha = (SELECT MAX(wsha) FROM win_vec)", []).unwrap();
        assert!(ix.stale(&c).unwrap(), "a removed window vector");
    }

    #[test]
    fn load_skips_bad_blobs_and_counts_what_is_owed() {
        let m = model();
        let mut c = store();
        let s1 = add(&c, 1, "the", &long(100, 7));
        add(&c, 2, "dog", "cat sat");
        add(&c, 3, "mat", "the dog");
        all(&mut c, &m);
        let rows = count(&c, "SELECT COUNT(*) FROM sec_window WHERE ix > 0");
        assert!(rows >= 3, "{rows} windows");
        // Window 0 of section 2 the wrong width, section 3 with none.
        c.execute("UPDATE vec_cache SET vec = x'00' WHERE sha = (SELECT sha FROM sections WHERE id = 2)", []).unwrap();
        c.execute("DELETE FROM vec_cache WHERE sha = (SELECT sha FROM sections WHERE id = 3)", []).unwrap();
        // Window 1 the wrong width, window 2 gone, window 3 gone under an older rule.
        let w = |ix: i64| -> String {
            c.query_row("SELECT wsha FROM sec_window WHERE sha = ?1 AND ix = ?2", params![s1, ix], |r| r.get(0)).unwrap()
        };
        let (w1, w2, w3) = (w(1), w(2), w(3));
        c.execute("UPDATE win_vec SET vec = x'00' WHERE wsha = ?1", [&w1]).unwrap();
        c.execute("DELETE FROM win_vec WHERE wsha IN (?1, ?2)", [&w2, &w3]).unwrap();
        c.execute("UPDATE sec_window SET rule = 0 WHERE wsha = ?1", [&w3]).unwrap();
        let ix = VecIndex::load(&c, &m.id, m.dim).unwrap();
        assert_eq!(ix.missing, 2, "a bad and an absent window 0");
        assert_eq!(ix.ids, [1]);
        assert_eq!(ix.row_sec.len() as i64, 1 + rows - 3, "no row for a bad or absent window");
        assert_eq!(ix.mat.len(), ix.row_sec.len() * m.dim);
        assert_eq!(ix.windows_pending, 2, "the bad and the absent window under the current rule");
    }

    #[test]
    fn an_older_prune_leaves_every_window_and_its_score() {
        // An atlas before windows prunes vec_cache only: had windows 1..n
        // been stored there, keyed by their own hash, it would delete them all.
        let m = model();
        let mut c = store();
        let body = long(90, 3);
        add(&c, 1, "the", &body);
        all(&mut c, &m);
        let before = count(&c, "SELECT COUNT(*) FROM win_vec");
        assert!(before >= 3);
        assert_eq!(c.execute(OLD_PRUNE, [&m.id]).unwrap(), 0);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM win_vec"), before);
        let wins = window::split(&m.tok, m.max_seq, "the", &body);
        let mut s = crate::embed::bert::Scratch::default();
        let ix = VecIndex::load(&c, &m.id, m.dim).unwrap();
        for w in &wins[1..] {
            let t = ix.top(m.isa, &m.embed(&w.text, &mut s), 1);
            assert!(t[0].1 > 0.999, "window still scores: {}", t[0].1);
        }
        assert_eq!(prune(&c, &m.id).unwrap(), 0, "nothing is stale");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM win_vec"), before);
    }

    #[test]
    fn prune_deletes_orphan_windows_and_their_vectors() {
        let m = model();
        let mut c = store();
        add(&c, 1, "the", &long(80, 1));
        add(&c, 2, "dog", &long(70, 2));
        all(&mut c, &m);
        let w2 = count(&c, "SELECT COUNT(*) FROM sec_window WHERE sha = (SELECT sha FROM sections WHERE id = 2)");
        let v = count(&c, "SELECT COUNT(*) FROM win_vec");
        c.execute("DELETE FROM sections WHERE id = 2", []).unwrap();
        let n = prune(&c, &m.id).unwrap() as i64;
        assert_eq!(n, 1 + (w2 - 1), "window 0 and the section's windows 1..n");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM sec_window WHERE sha NOT IN (SELECT sha FROM sections)"), 0);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM win_vec"), v - (w2 - 1));
        assert_eq!(count(&c, "SELECT COUNT(*) FROM vec_cache"), 1);
    }

    #[test]
    fn a_new_rule_resplits_without_re_embedding_unchanged_windows() {
        let m = model();
        let mut c = store();
        let sha = add(&c, 1, "the", &long(100, 4));
        all(&mut c, &m);
        let rows = count(&c, "SELECT COUNT(*) FROM sec_window");
        // An older split: an older rule and a stale higher row.
        c.execute("UPDATE sec_window SET rule = 0", []).unwrap();
        c.execute("INSERT INTO sec_window VALUES(?1, ?2, 99, 'old', 0)", params![m.id, sha]).unwrap();
        assert_eq!(windows_pending(&c, &m.id).unwrap(), (1, 0));
        let ix = VecIndex::load(&c, &m.id, m.dim).unwrap();
        assert_eq!(ix.pending(), 1);
        assert_eq!(ix.row_sec.len() as i64, rows, "old windows still score meanwhile");
        let p = all(&mut c, &m);
        assert_eq!(p, Progress { sections: 0, split: 1, windows: 0 });
        assert_eq!(count(&c, "SELECT COUNT(*) FROM sec_window"), rows);
        assert_eq!(count(&c, "SELECT MAX(ix) FROM sec_window") + 1, rows, "no stale higher row");
        assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM sec_window WHERE rule = {WINDOW_RULES_VERSION}")), rows);
    }

    #[test]
    fn an_upgrade_counts_windows_not_missing_sections() {
        let m = model();
        let mut c = store();
        let body = long(100, 5);
        add(&c, 1, "the", &body);
        add(&c, 2, "dog", "the cat");
        // What an older atlas left: window 0 of each, nothing else.
        put0(&c, &m, "the", &body);
        put0(&c, &m, "dog", "the cat");
        assert_eq!(missing(&c, &m.id).unwrap(), 0);
        assert_eq!(windows_pending(&c, &m.id).unwrap(), (2, 0));
        // A budget of two: both split, two windows embedded, the rest counted.
        let p = embed_pending(&mut c, &m, 2, 1).unwrap();
        assert_eq!(p, Progress { sections: 0, split: 2, windows: 2 });
        let wins = window::split(&m.tok, m.max_seq, "the", &body).len();
        assert_eq!(windows_pending(&c, &m.id).unwrap(), (0, wins - 3));
        let ix = VecIndex::load(&c, &m.id, m.dim).unwrap();
        assert_eq!(ix.missing, 0);
        assert_eq!(ix.ids, [1, 2]);
        assert_eq!(ix.row_sec.len(), 4, "a section scores by the windows it has");
        all(&mut c, &m);
        assert_eq!(windows_pending(&c, &m.id).unwrap(), (0, 0));
    }

    #[test]
    fn k_counts_sections_and_ids_are_distinct() {
        // Section 1 has three windows, all near the query.
        let ix = VecIndex {
            ids: vec![1, 2, 3],
            row_sec: vec![0, 0, 0, 1, 2],
            mat: vec![0.9, 0.436, 0.95, 0.312, 0.8, 0.6, 0.6, 0.8, 0.0, 1.0],
            dim: 2,
            ..VecIndex::default()
        };
        let q = [1.0, 0.0];
        let t = ix.top(gemm::Isa::Scalar, &q, 2);
        assert_eq!(t.iter().map(|x| x.0).collect::<Vec<_>>(), [1, 2], "k counts sections");
        assert!((t[0].1 - 0.95).abs() < 1e-6, "the best window's score");
        let every = ix.top(gemm::Isa::Scalar, &q, 200);
        assert_eq!(every.len(), 3, "the filtered path's large k still counts sections");
        let ids: Vec<i64> = every.iter().map(|x| x.0).collect();
        assert_eq!(rrf(&[&[3, 1], &ids]), [1, 3, 2]);
    }

    #[test]
    fn embed_pending_checks_the_version_before_writing_windows() {
        let m = model();
        let mut c = store();
        let body = long(80, 6);
        add(&c, 1, "the", &body);
        put0(&c, &m, "the", &body);
        make_index_newer(&c);
        let before = dump(&c);
        assert!(embed_pending(&mut c, &m, 8, 1).expect_err("refused").is_store_too_new());
        assert_eq!(dump(&c), before, "no window written");
    }
}
