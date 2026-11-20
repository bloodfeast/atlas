//! `Connection::busy_handler` of `atlas::db`, written against the surface
//! both backends give: rusqlite defines the outcomes asserted here and the
//! owned binding must give the same ones. The store's fair wait
//! (`atlas::hook_budget::bound`, #343) is a handler set through this call.
//!
//! SQLite calls the handler on the thread that is waiting, so each test
//! records the calls in a thread-local of its own thread.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use atlas::db::{self, Connection};

thread_local! {
    static CALLS: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

/// The counts the handlers below were called with since the last call of
/// this, in order.
fn calls() -> Vec<i32> {
    CALLS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

fn record(n: i32) {
    CALLS.with(|c| c.borrow_mut().push(n));
}

/// Tries again three times, then gives up.
fn three_tries(n: i32) -> bool {
    record(n);
    n < 3
}

/// Gives up at the first call.
fn gives_up(n: i32) -> bool {
    record(n);
    false
}

/// Waits for the lock however long it is held.
fn waits(n: i32) -> bool {
    record(n);
    std::thread::sleep(Duration::from_millis(1));
    true
}

fn panics(n: i32) -> bool {
    record(n);
    panic!("a busy handler that panics (expected by tests/db_busy_handler.rs)");
}

/// A database in a folder of its own, with table `t`, and a second
/// connection that holds its write lock.
struct Held {
    dir: PathBuf,
    path: PathBuf,
    holder: Option<Connection>,
}

impl Held {
    fn new(tag: &str) -> Held {
        let dir = std::env::temp_dir().join(format!("atlas-db-busy-handler-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("busy.sqlite");
        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (x); BEGIN IMMEDIATE; INSERT INTO t VALUES (1);").unwrap();
        Held { dir, path, holder: Some(holder) }
    }

    /// The connection holding the lock, to commit on or to move to a thread.
    fn take_holder(&mut self) -> Connection {
        self.holder.take().expect("the holder is taken once")
    }

    fn waiter(&self) -> Connection {
        Connection::open(&self.path).unwrap()
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The write the waiter tries: BUSY with SQLite's text while the lock is
/// held.
fn refused(waiter: &Connection) {
    let err = waiter.execute("INSERT INTO t VALUES (2)", []).expect_err("the write lock is held");
    assert!(db::is_busy(&err), "{err}");
    assert_eq!(err.to_string(), "database is locked");
}

#[test]
fn the_handler_gets_the_count_of_earlier_calls_until_it_gives_up() {
    let held = Held::new("count");
    let waiter = held.waiter();
    waiter.busy_handler(Some(three_tries)).unwrap();
    refused(&waiter);
    assert_eq!(calls(), [0, 1, 2, 3]);
    // The count is per lock wait: the next statement starts at 0 again.
    refused(&waiter);
    assert_eq!(calls(), [0, 1, 2, 3]);
    // BEGIN IMMEDIATE waits the same way.
    let err = waiter.execute_batch("BEGIN IMMEDIATE").expect_err("the write lock is held");
    assert!(db::is_busy(&err), "{err}");
    assert_eq!(calls(), [0, 1, 2, 3]);
}

#[test]
fn a_handler_that_keeps_trying_gets_the_lock_when_it_is_free() {
    let mut held = Held::new("waits");
    let waiter = held.waiter();
    waiter.busy_handler(Some(waits)).unwrap();
    let holder = held.take_holder();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        holder.execute_batch("COMMIT").unwrap();
    });
    assert_eq!(waiter.execute("INSERT INTO t VALUES (2)", []).unwrap(), 1);
    release.join().unwrap();
    let seen = calls();
    assert!(!seen.is_empty(), "the lock was held when the write began");
    assert_eq!(seen, (0..i32::try_from(seen.len()).unwrap()).collect::<Vec<_>>(), "one call per try, counted from 0");
    assert_eq!(waiter.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
}

#[test]
fn none_clears_the_handler_and_a_timeout_replaces_it() {
    let held = Held::new("replace");
    let waiter = held.waiter();
    waiter.busy_handler(Some(gives_up)).unwrap();
    refused(&waiter);
    assert_eq!(calls(), [0]);
    // No handler: BUSY at once, and the handler set before is not called.
    waiter.busy_handler(None).unwrap();
    refused(&waiter);
    assert_eq!(calls(), [0i32; 0]);
    // A connection has one busy handler: a timeout takes the place of a
    // handler, and a handler the place of a timeout.
    waiter.busy_handler(Some(three_tries)).unwrap();
    waiter.busy_timeout(Duration::from_millis(20)).unwrap();
    refused(&waiter);
    assert_eq!(calls(), [0i32; 0]);
    waiter.busy_handler(Some(gives_up)).unwrap();
    refused(&waiter);
    assert_eq!(calls(), [0]);
    // The handler is the connection's own: a second connection keeps the
    // default it opened with.
    let other = held.waiter();
    other.busy_timeout(Duration::ZERO).unwrap();
    refused(&other);
    assert_eq!(calls(), [0i32; 0]);
}

#[test]
fn a_handler_that_panics_gives_up_and_the_connection_still_works() {
    let mut held = Held::new("panic");
    let waiter = held.waiter();
    waiter.busy_handler(Some(panics)).unwrap();
    // The panic stops at the handler: the statement fails with BUSY.
    refused(&waiter);
    assert_eq!(calls(), [0]);
    held.take_holder().execute_batch("COMMIT").unwrap();
    assert_eq!(waiter.execute("INSERT INTO t VALUES (2)", []).unwrap(), 1);
    assert_eq!(calls(), [0i32; 0]);
}
