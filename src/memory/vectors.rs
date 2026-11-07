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

/// The cache table.
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
    {
        let mut st = tx.prepare_cached("INSERT OR REPLACE INTO rec_vec(model, sha, vec) VALUES(?1, ?2, ?3)")?;
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
/// [`crate::Error::Db`].
pub fn prune(store: &MemoryStore, model: &str) -> Result<usize> {
    ensure_schema(store)?;
    Ok(store.conn().execute(
        "DELETE FROM rec_vec WHERE model = ?1 AND sha NOT IN (SELECT v.sha FROM records r JOIN versions v ON v.vid = r.head)",
        [model],
    )?)
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
    row: HashMap<i64, usize>,
    stamp: (i64, u64),
}

impl RecVecs {
    /// Loads the vectors of `model` for every recallable active record.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn load(store: &MemoryStore, model: &str, dim: usize) -> Result<RecVecs> {
        ensure_schema(store)?;
        let mut v = RecVecs { dim, stamp: store.stamp()?, ..RecVecs::default() };
        let mut st = store.conn().prepare_cached(
            "SELECT r.rid, x.vec FROM records r JOIN versions v ON v.vid = r.head LEFT JOIN rec_vec x ON x.model = ?1 AND x.sha = v.sha
             WHERE r.status = 'active' AND r.kind NOT IN ('block', 'journal') ORDER BY r.rid",
        )?;
        let mut rows = st.query([model])?;
        while let Some(r) = rows.next()? {
            let rid: i64 = r.get(0)?;
            match r.get_ref(1)? {
                rusqlite::types::ValueRef::Blob(b) if b.len() == dim * 4 => {
                    v.row.insert(rid, v.rids.len());
                    v.rids.push(rid);
                    v.mat.extend(b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
                }
                _ => v.missing += 1,
            }
        }
        Ok(v)
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
