//! Semantic search over sections: one vector per section, cached by the
//! hash of the section's text, searched brute force in memory.
//!
//! * The cache, `vec_cache(model, sha, vec)`, is keyed by the model id and
//!   the section text's SHA-256 (the `sections.sha` column), so a re-index
//!   re-embeds only sections whose text changed, and a rebuilt database
//!   keeps its vectors. It is pruned of hashes no section has.
//! * Embedding runs on the caller's thread for a few sections, and on a
//!   background thread ([`Embedder`]) for a cold corpus; answers say how
//!   many sections are still without a vector.
//! * Search is a dot product of the query vector with every section vector
//!   (unit vectors, so it is the cosine), using the same kernels as the
//!   encoder; at a few thousand sections of 384 floats that is a few hundred
//!   microseconds, so no approximate index.
//! * Hybrid ranks fuse the FTS5 ranking and the vector ranking by
//!   reciprocal rank (`1 / (60 + rank)` summed).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rusqlite::{params, Connection};

use crate::embed::gemm;
use crate::embed::Model;
use crate::error::Result;

/// The reciprocal-rank-fusion constant.
pub const RRF_K: f64 = 60.0;

/// Sections embedded on the query's own thread before the rest is left to
/// the background embedder.
pub const INLINE_BUDGET: usize = 32;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS vec_cache(
    model TEXT NOT NULL,
    sha TEXT NOT NULL,
    vec BLOB NOT NULL,
    PRIMARY KEY(model, sha)
) WITHOUT ROWID;";

/// The text a section is embedded as: its heading path, a newline, its
/// body.
pub fn section_text(heading: &str, body: &str) -> String {
    format!("{heading}\n{body}")
}

/// The cache key of a section: the SHA-256 of [`section_text`].
pub fn section_sha(heading: &str, body: &str) -> String {
    crate::sha256::hex(section_text(heading, body).as_bytes())
}

/// Creates the vector cache table.
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

/// Embeds up to `limit` pending sections and stores their vectors.
///
/// # Arguments
///
/// * `conn` - a connection (the background embedder's own, or the store's).
/// * `model` - the model.
/// * `limit` - the most sections this call embeds.
/// * `threads` - encoder threads.
///
/// # Returns
///
/// How many were embedded (0 when none were pending).
///
/// # Errors
///
/// [`crate::error::Error::Db`] from SQLite.
pub fn embed_pending(conn: &mut Connection, model: &Model, limit: usize, threads: usize) -> Result<usize> {
    let todo = pending(conn, &model.id, limit)?;
    if todo.is_empty() {
        return Ok(0);
    }
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
    Ok(todo.len())
}

/// Deletes cached vectors of the model whose hash no section has.
///
/// # Errors
///
/// [`crate::error::Error::StoreTooNew`] (nothing deleted, no table made), or
/// [`crate::error::Error::Db`] from SQLite.
pub fn prune(conn: &Connection, model: &str) -> Result<usize> {
    // The table is made inside the checked transaction too.
    crate::store::guarded(conn, || {
        ensure_schema(conn)?;
        Ok(conn.execute("DELETE FROM vec_cache WHERE model = ?1 AND sha NOT IN (SELECT sha FROM sections)", [model])?)
    })
}

/// The section vectors of one model, as one row-major matrix.
#[derive(Debug, Default)]
pub struct VecIndex {
    /// The model the vectors belong to.
    pub model: String,
    /// Section ids, one per matrix row.
    pub ids: Vec<i64>,
    /// `ids.len()` rows of `dim` floats.
    pub mat: Vec<f32>,
    /// The vector width.
    pub dim: usize,
    /// Sections that had no vector when loaded.
    pub missing: usize,
    stamp: (i64, i64, i64, i64),
}

fn stamp(conn: &Connection, model: &str) -> Result<(i64, i64, i64, i64)> {
    Ok(conn.query_row(
        "SELECT (SELECT COUNT(*) FROM sections), (SELECT TOTAL(id) FROM sections), (SELECT MAX(id) FROM sections), (SELECT COUNT(*) FROM vec_cache WHERE model = ?1)",
        [model],
        |r| Ok((r.get(0)?, r.get::<_, f64>(1)? as i64, r.get::<_, Option<i64>>(2)?.unwrap_or(0), r.get(3)?)),
    )?)
}

impl VecIndex {
    /// Loads every section's vector for the model.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Db`] from SQLite.
    pub fn load(conn: &Connection, model: &str, dim: usize) -> Result<VecIndex> {
        ensure_schema(conn)?;
        let st = stamp(conn, model)?;
        let mut ix = VecIndex { model: model.to_string(), dim, stamp: st, ..VecIndex::default() };
        let mut q = conn.prepare_cached("SELECT s.id, v.vec FROM sections s LEFT JOIN vec_cache v ON v.model = ?1 AND v.sha = s.sha ORDER BY s.id")?;
        let mut rows = q.query([model])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            match r.get_ref(1)? {
                rusqlite::types::ValueRef::Blob(b) if b.len() == dim * 4 => {
                    ix.ids.push(id);
                    from_blob(b, &mut ix.mat);
                }
                _ => ix.missing += 1,
            }
        }
        Ok(ix)
    }

    /// True when sections or vectors changed since the load.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Db`] from SQLite.
    pub fn stale(&self, conn: &Connection) -> Result<bool> {
        Ok(stamp(conn, &self.model)? != self.stamp)
    }

    /// The `k` sections most similar to a query vector, best first.
    ///
    /// # Arguments
    ///
    /// * `isa` - the kernels.
    /// * `q` - the query's unit vector.
    /// * `k` - how many.
    pub fn top(&self, isa: gemm::Isa, q: &[f32], k: usize) -> Vec<(i64, f32)> {
        let mut scored: Vec<(i64, f32)> = self.ids.iter().enumerate().map(|(i, &id)| (id, gemm::dot(isa, q, &self.mat[i * self.dim..(i + 1) * self.dim]))).collect();
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

/// A background thread that embeds every pending section, then prunes.
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
                let n = embed_pending(&mut conn, &model, 256, threads)?;
                if n == 0 {
                    break;
                }
                e.fetch_add(n, Ordering::Relaxed);
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

    /// True once every pending section is embedded (or it failed).
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

    /// Sections embedded so far.
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
        let ix = VecIndex { ids: vec![10, 11, 12], mat: vec![1.0, 0.0, 0.0, 1.0, 0.6, 0.8], dim: 2, ..VecIndex::default() };
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
