//! Record vectors for the recall gate: one vector per recallable record's
//! current version, cached in the store by the model id and the version's
//! SHA-256, held in memory as one row-major matrix.
//!
//! * Only kinds the gate can inject are embedded ([`recallable`]): not
//!   `block` (export layout) and not `journal` (a lane's own log, recalled by
//!   lane, not by relevance).
//! * The cache table, `rec_vec(model, sha, vec)`, is derived data: a record
//!   revised gets a new version SHA and so a new vector; the old row stays
//!   until [`prune`].
//! * A (model, sha) row is immutable; a changed encoder must change the
//!   model id. A row is written once and never rewritten ([`embed_pending`]
//!   inserts with `OR IGNORE`), and [`RecVecs::refresh`] keeps a row it
//!   holds for as long as the record's version SHA is unchanged, so a
//!   rewritten row would not be seen until the process restarts.
//! * Embedding a cold store (about 1,500 records) runs on a background
//!   thread ([`spawn_embedder`]); until a record has a vector, the gate
//!   judges it lexically alone.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rusqlite::params;

use crate::embed::{gemm, Model};
use crate::error::Result;

use super::MemoryStore;

/// The cache table. A (model, sha) row is immutable; a changed encoder must
/// change the model id (see the module doc).
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS rec_vec(model TEXT NOT NULL, sha TEXT NOT NULL, vec BLOB NOT NULL, PRIMARY KEY(model, sha)) WITHOUT ROWID;";

/// True for the kinds the relevance gate may inject.
pub fn recallable(kind: &str) -> bool {
    kind != "block" && kind != "journal"
}

/// The text a record is embedded from: its title, then its body.
pub fn record_text(title: &str, body: &str) -> String {
    format!("{}\n{}", title.trim(), body.trim_matches('\n'))
}

/// Makes the cache table if it is missing.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn ensure_schema(store: &MemoryStore) -> Result<()> {
    store.conn().execute_batch(SCHEMA)?;
    Ok(())
}

/// Recallable active records whose current version has no vector for
/// `model`, as (version sha, text), at most `limit`.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn pending(store: &MemoryStore, model: &str, limit: usize) -> Result<Vec<(String, String)>> {
    ensure_schema(store)?;
    let mut st = store.conn().prepare_cached(
        "SELECT DISTINCT v.sha, v.title, v.body FROM records r JOIN versions v ON v.vid = r.head
         WHERE r.status = 'active' AND r.kind NOT IN ('block', 'journal')
           AND NOT EXISTS (SELECT 1 FROM rec_vec x WHERE x.model = ?1 AND x.sha = v.sha) LIMIT ?2",
    )?;
    let rows = st.query_map(params![model, limit as i64], |r| Ok((r.get::<_, String>(0)?, record_text(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?))))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Embeds up to `limit` pending records and stores their vectors.
///
/// # Returns
///
/// How many were embedded (0 when none are pending).
///
/// # Errors
///
/// [`crate::Error::Db`]; [`crate::Error::InvalidArgument`] if the model
/// returns a different number of vectors than texts.
pub fn embed_pending(store: &MemoryStore, model: &Model, limit: usize, threads: usize) -> Result<usize> {
    let todo = pending(store, &model.id, limit)?;
    if todo.is_empty() {
        return Ok(0);
    }
    store_vectors(store, model, &todo, threads)
}

/// Embeds `todo` (as [`pending`] returns it) and stores the vectors. A row
/// another embedder stored since `todo` was read is left as it is: a
/// (model, sha) row is never rewritten.
fn store_vectors(store: &MemoryStore, model: &Model, todo: &[(String, String)], threads: usize) -> Result<usize> {
    let texts: Vec<String> = todo.iter().map(|t| t.1.clone()).collect();
    let vecs = model.embed_many(&texts, threads);
    // `embed_many` returns one vector per text today. A short batch would
    // leave records pending while this reported them embedded, and
    // `embed_all_pending` (xtask memory_eval.rs), which loops until this
    // returns 0, would never end: fail instead.
    if vecs.len() != todo.len() {
        return Err(crate::Error::InvalidArgument(format!("embedded {} vectors for {} texts", vecs.len(), todo.len())));
    }
    let tx = rusqlite::Transaction::new_unchecked(store.conn(), rusqlite::TransactionBehavior::Immediate)?;
    // A store a newer atlas upgraded since this one opened it is not written.
    super::check_locked(&tx, store.path())?;
    {
        let mut st = tx.prepare_cached("INSERT OR IGNORE INTO rec_vec(model, sha, vec) VALUES(?1, ?2, ?3)")?;
        for ((sha, _), v) in todo.iter().zip(&vecs) {
            let blob: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            st.execute(params![model.id, sha, blob])?;
        }
    }
    tx.commit()?;
    Ok(todo.len())
}

/// Deletes cached vectors of `model` that no record's current version has.
///
/// # Errors
///
/// [`crate::Error::StoreTooNew`] (nothing deleted, no table made), or
/// [`crate::Error::Db`].
pub fn prune(store: &MemoryStore, model: &str) -> Result<usize> {
    // The table is made inside the checked transaction too: a store a newer
    // atlas upgraded is not written, its schema included.
    super::guarded(store.conn(), store.path(), || {
        ensure_schema(store)?;
        Ok(store.conn().execute(
            "DELETE FROM rec_vec WHERE model = ?1 AND sha NOT IN (SELECT v.sha FROM records r JOIN versions v ON v.vid = r.head)",
            [model],
        )?)
    })
}

/// Embeds every pending record on a background thread with its own store
/// connection, in batches of 128.
///
/// # Arguments
///
/// * `path` - the store file.
/// * `model` - the encoder.
/// * `threads` - encoder threads.
///
/// # Returns
///
/// A flag set when the thread is done (whether it succeeded or not; an
/// error is written to stderr, and the gate stays lexical for the records
/// left without a vector).
pub fn spawn_embedder(path: PathBuf, model: Arc<Model>, threads: usize) -> Arc<AtomicBool> {
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    let run = move || -> Result<()> {
        let store = MemoryStore::open(&path)?;
        while embed_pending(&store, &model, 128, threads)? > 0 {}
        prune(&store, &model.id)?;
        Ok(())
    };
    let spawned = std::thread::Builder::new().name("atlas-rec-embed".into()).spawn(move || {
        if let Err(e) = run() {
            eprintln!("atlas recall vectors: {e}");
        }
        d.store(true, Ordering::Release);
    });
    if spawned.is_err() {
        done.store(true, Ordering::Release);
    }
    done
}

/// The vectors of every recallable active record, loaded at one store state.
#[derive(Debug, Default)]
pub struct RecVecs {
    /// Record row ids, one per matrix row.
    pub rids: Vec<i64>,
    /// Row-major unit vectors, `dim` floats each.
    pub mat: Vec<f32>,
    /// The vector width.
    pub dim: usize,
    /// Recallable active records with no vector yet.
    pub missing: usize,
    /// Rows looked up again (records with a new version or no vector) by
    /// the last [`RecVecs::load`] or [`RecVecs::refresh`]; a row kept from
    /// before is not looked up.
    pub read: usize,
    row: HashMap<i64, usize>,
    /// The version SHA each row's vector is for.
    shas: Vec<String>,
    model: String,
    stamp: (i64, u64),
}

impl RecVecs {
    /// Loads the vectors of `model` for every recallable active record.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn load(store: &MemoryStore, model: &str, dim: usize) -> Result<RecVecs> {
        let mut v = RecVecs { dim, model: model.to_string(), ..RecVecs::default() };
        v.refresh(store)?;
        Ok(v)
    }

    /// Brings the vectors up to the store's current state. Only the records
    /// whose current version changed, or that had no vector, are read again:
    /// a vector is keyed by its version's SHA, and a (model, sha) row is
    /// never rewritten (see the module doc), so a kept row is still right.
    /// After a commit that touched no recallable record (a journal entry, a
    /// token log line) this is one pass over the record heads, no vector read.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn refresh(&mut self, store: &MemoryStore) -> Result<()> {
        // A `RecVecs::default()` has no width, so every record would count
        // as missing: build one with `load`.
        debug_assert!(self.dim > 0, "RecVecs::refresh needs a width: build it with RecVecs::load");
        ensure_schema(store)?;
        let dim = self.dim;
        // Taken first: a commit during the pass leaves these stale, so the
        // next refresh looks again.
        let mut next = RecVecs { dim, stamp: store.stamp()?, ..RecVecs::default() };
        let model = self.model.clone();
        let mut heads = store.conn().prepare_cached(
            "SELECT r.rid, v.sha FROM records r JOIN versions v ON v.vid = r.head
             WHERE r.status = 'active' AND r.kind NOT IN ('block', 'journal') ORDER BY r.rid",
        )?;
        let mut get = store.conn().prepare_cached("SELECT vec FROM rec_vec WHERE model = ?1 AND sha = ?2")?;
        let mut rows = heads.query([])?;
        while let Some(r) = rows.next()? {
            let rid: i64 = r.get(0)?;
            let sha: String = r.get(1)?;
            if let Some(&i) = self.row.get(&rid).filter(|&&i| self.shas[i] == sha) {
                next.push(rid, sha, |mat| mat.extend_from_slice(&self.mat[i * dim..(i + 1) * dim]));
                continue;
            }
            next.read += 1;
            let mut vecs = get.query(params![model, sha])?;
            match vecs.next()?.map(|x| x.get_ref(0)).transpose()? {
                Some(rusqlite::types::ValueRef::Blob(b)) if b.len() == dim * 4 => {
                    let floats = b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c));
                    next.push(rid, sha, |mat| mat.extend(floats));
                }
                _ => next.missing += 1,
            }
        }
        next.model = model;
        *self = next;
        Ok(())
    }

    fn push(&mut self, rid: i64, sha: String, vector: impl FnOnce(&mut Vec<f32>)) {
        self.row.insert(rid, self.rids.len());
        self.rids.push(rid);
        self.shas.push(sha);
        vector(&mut self.mat);
    }

    /// True when the store changed since these vectors were loaded.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn stale(&self, store: &MemoryStore) -> Result<bool> {
        Ok(store.stamp()? != self.stamp)
    }

    /// The cosine of a record with the (unit) query vector, if it has a vector.
    pub fn cos(&self, isa: gemm::Isa, rid: i64, q: &[f32]) -> Option<f32> {
        let &i = self.row.get(&rid)?;
        Some(gemm::dot(isa, q, &self.mat[i * self.dim..(i + 1) * self.dim]))
    }

    /// The cosine of two records, when both have a vector.
    pub fn pair(&self, isa: gemm::Isa, a: i64, b: i64) -> Option<f32> {
        let (&i, &j) = (self.row.get(&a)?, self.row.get(&b)?);
        Some(gemm::dot(isa, &self.mat[i * self.dim..(i + 1) * self.dim], &self.mat[j * self.dim..(j + 1) * self.dim]))
    }

    /// The `k` records nearest the query, best first, as (rid, cosine).
    pub fn top(&self, isa: gemm::Isa, q: &[f32], k: usize) -> Vec<(i64, f32)> {
        let mut scored: Vec<(i64, f32)> =
            self.rids.iter().enumerate().map(|(i, &rid)| (rid, gemm::dot(isa, q, &self.mat[i * self.dim..(i + 1) * self.dim]))).collect();
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

#[cfg(test)]
mod immutable_tests {
    use super::*;

    /// A second embedder that read the same pending set stores nothing over
    /// the row the first one wrote: a (model, sha) row is never rewritten.
    #[test]
    fn a_stored_vector_is_never_rewritten() {
        let mut store = MemoryStore::in_memory().unwrap();
        let mut w = store.writer().unwrap();
        let d = super::super::Draft {
            title: "widget cache".into(),
            body: "the cat sat on the mat.\n".into(),
            reason: "t".into(),
            author: "t".into(),
            client: "t".into(),
            ..Default::default()
        };
        w.create("widget-cache", "memory", "", "feedback", &d, None).unwrap();
        w.commit().unwrap();
        let first = Model::parse(&crate::embed::tiny_model_bytes(false, 7), gemm::Isa::detect()).unwrap();
        // Another encoder under the same id: what a rewrite would store.
        let mut second = Model::parse(&crate::embed::tiny_model_bytes(false, 8), gemm::Isa::detect()).unwrap();
        second.id = first.id.clone();
        let todo = pending(&store, &first.id, 10).unwrap();
        assert_eq!(todo.len(), 1);
        let row = |s: &MemoryStore| -> Vec<u8> {
            s.conn().query_row("SELECT vec FROM rec_vec WHERE model = ?1", [&first.id], |r| r.get(0)).unwrap()
        };
        assert_eq!(store_vectors(&store, &first, &todo, 1).unwrap(), 1);
        let before = row(&store);
        let other: Vec<u8> = second.embed_many(&[todo[0].1.clone()], 1)[0].iter().flat_map(|x| x.to_le_bytes()).collect();
        assert_ne!(other, before, "the two encoders differ");
        store_vectors(&store, &second, &todo, 1).unwrap();
        assert_eq!(row(&store), before, "the stored row was rewritten");
        assert_eq!(embed_pending(&store, &second, 10, 1).unwrap(), 0, "nothing is pending");
        assert_eq!(row(&store), before);
    }
}

#[cfg(test)]
mod storeguard_tests {
    use super::*;
    use crate::store::skew_support::dump;

    /// `prune` on a store a newer atlas upgraded since this one opened it
    /// deletes nothing and makes no table; at the current version it prunes.
    #[test]
    fn prune_checks_the_version_under_the_lock() {
        let newer = format!("PRAGMA user_version = {};", super::super::SCHEMA_VERSION + 1);
        let store = MemoryStore::in_memory().unwrap();
        store.conn().execute_batch(&newer).unwrap();
        let before = dump(store.conn());
        assert!(prune(&store, "m").expect_err("refused").is_store_too_new());
        assert_eq!(dump(store.conn()), before, "no table made");
        let store = MemoryStore::in_memory().unwrap();
        ensure_schema(&store).unwrap();
        store.conn().execute("INSERT INTO rec_vec(model, sha, vec) VALUES('m', 'gone', x'00')", []).unwrap();
        store.conn().execute_batch(&newer).unwrap();
        let before = dump(store.conn());
        assert!(prune(&store, "m").expect_err("refused").is_store_too_new());
        assert!(store.conn().is_autocommit(), "rolled back");
        assert_eq!(dump(store.conn()), before, "the stale vector stays");
        store.conn().execute_batch(&format!("PRAGMA user_version = {};", super::super::SCHEMA_VERSION)).unwrap();
        assert_eq!(prune(&store, "m").unwrap(), 1);
    }
}
