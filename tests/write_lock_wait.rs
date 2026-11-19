//! A writer waiting on the store lock (issue #340).
//!
//! Under load the default SQLite busy handler backs off to 100 ms sleeps, so a
//! writer that has waited a while polls rarely and loses each free moment to
//! writers that just arrived; one was seen waiting out its whole 30 s and
//! failing with "database is locked" while no single hold was over 1.2 s.
//! Every tool-call store waits with [`atlas::hook_budget::busy`] instead.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use atlas::hook_budget::{busy, Active};
use common::*;

/// A long waiter polls as often as a new one: one call of the handler after
/// thousands of earlier calls still sleeps only a few milliseconds.
#[test]
fn a_long_wait_keeps_polling_often() {
    let mut least = Duration::MAX;
    for n in [12, 100, 1_000, 10_000, 100_000] {
        let t = Instant::now();
        assert!(busy(n), "call {n} gave up with no deadline");
        least = least.min(t.elapsed());
    }
    assert!(least < Duration::from_millis(15), "a waiter that has waited long sleeps {least:?} between tries");
}

/// Holds the write lock of `db` from another connection for `hold`.
fn hold(db: &Path, hold: Duration) -> std::thread::JoinHandle<()> {
    let db = db.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        let c = atlas::db::Connection::open(&db).unwrap();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(hold);
        c.execute_batch("COMMIT").unwrap();
    });
    rx.recv().unwrap();
    h
}

/// Takes the write lock on `conn` under a 200 ms deadline while another
/// connection holds it for 3 s: the store's handler is [`busy`] when it gives
/// up at the deadline, where SQLite's own timeout would wait the 3 s out.
fn waits_with_the_handler(what: &str, db: &Path, conn: &atlas::db::Connection) {
    let holder = hold(db, Duration::from_secs(3));
    let t = Instant::now();
    let got = {
        let _deadline = Active::until(Instant::now() + Duration::from_millis(200));
        conn.execute_batch("BEGIN IMMEDIATE")
    };
    let waited = t.elapsed();
    if got.is_ok() {
        conn.execute_batch("ROLLBACK").unwrap();
    }
    holder.join().unwrap();
    // At least most of the 200 ms: a connection with no handler at all
    // fails at once.
    assert!(
        got.is_err() && waited >= Duration::from_millis(150) && waited < Duration::from_millis(2_500),
        "{what}: waited {waited:?} for the lock ({got:?}), not with the store's busy handler"
    );
}

#[test]
fn tool_call_stores_wait_with_the_store_handler() {
    let t = Temp::new("lock-wait");
    let mem = t.path().join("memory.store");
    let m = atlas::memory::MemoryStore::open(&mem).unwrap();
    waits_with_the_handler("memory store", &mem, m.conn());
    let vault = t.path().join("v");
    std::fs::create_dir_all(&vault).unwrap();
    let db = t.path().join("index.db");
    let vaults = atlas::paths::Vaults::new(&[("v".to_string(), vault)]).unwrap();
    let s = atlas::store::Store::open(&db, vaults).unwrap();
    waits_with_the_handler("index store", &db, s.conn());
}

/// Raw index-db connections (a handle, the token log, a compaction row) share
/// the store's write lock, so they wait with the same handler: one left on
/// SQLite's back-off polls every 100 ms and loses each free moment to the
/// store's 2 ms polls (review S1 of #343).
#[test]
fn raw_connections_wait_with_the_store_handler() {
    let t = Temp::new("lock-wait-raw");
    let db = t.path().join("index.db");
    let raw = atlas::store::open_raw(&db).unwrap();
    waits_with_the_handler("raw connection", &db, &raw);
    let h = atlas::handles::Handles::open(&db, atlas::handles::HandleCfg::default()).unwrap();
    waits_with_the_handler("handles", &db, h.conn());
}

/// Smoke test: a raw connection contending with a store connection for a
/// lock held 300 ms each get it, well within the bound. It does not test
/// the handler or fairness (SQLite's own back-off clears a 300 ms hold as
/// well); [`raw_connections_wait_with_the_store_handler`] does.
#[test]
fn a_raw_connection_beside_a_store_gets_the_lock() {
    let t = Temp::new("lock-wait-both");
    let vault = t.path().join("v");
    std::fs::create_dir_all(&vault).unwrap();
    let db = t.path().join("index.db");
    let vaults = atlas::paths::Vaults::new(&[("v".to_string(), vault)]).unwrap();
    let s = atlas::store::Store::open(&db, vaults).unwrap();
    let holder = hold(&db, Duration::from_millis(300));
    let raw_db = db.clone();
    let raw = std::thread::spawn(move || {
        let c = atlas::store::open_raw(&raw_db).unwrap();
        let t = Instant::now();
        let got = c.execute_batch("BEGIN IMMEDIATE");
        let waited = t.elapsed();
        std::thread::sleep(Duration::from_millis(20));
        let _ = c.execute_batch("COMMIT");
        (got, waited)
    });
    let t0 = Instant::now();
    let got = s.conn().execute_batch("BEGIN IMMEDIATE");
    let waited = t0.elapsed();
    std::thread::sleep(Duration::from_millis(20));
    let _ = s.conn().execute_batch("COMMIT");
    let (raw_got, raw_waited) = raw.join().unwrap();
    holder.join().unwrap();
    assert!(got.is_ok() && waited < Duration::from_secs(5), "store: {got:?} after {waited:?}");
    assert!(raw_got.is_ok() && raw_waited < Duration::from_secs(5), "raw connection: {raw_got:?} after {raw_waited:?}");
}

/// The calibration opener, used by the server's background "atlas-calibrate"
/// thread, waits with the store's handler too, not SQLite's back-off
/// (review F1 of #343).
#[test]
fn the_calibration_connection_waits_with_the_store_handler() {
    let t = Temp::new("lock-wait-calibrate");
    let db = t.path().join("index.db");
    let conn = atlas::calibrate::open(&db).unwrap();
    waits_with_the_handler("calibration", &db, &conn);
}
