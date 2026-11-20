//! The behaviour of `atlas::db` that atlas depends on, written against the
//! surface both backends give. Run with either backend: rusqlite defines the
//! texts and outcomes asserted here, and the owned binding must give the
//! same ones. Error texts are behaviour: atlas prints them and matches on a
//! few ("no such table", "locked").

use std::path::PathBuf;
use std::time::Duration;

use atlas::db::types::{Value, ValueRef};
use atlas::db::{self, params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};

/// A folder of its own under the system temp folder, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("atlas-db-surface-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn memory() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, score REAL, raw BLOB);").unwrap();
    conn
}

fn count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap()
}

/// A row of `t`, as read back.
type Stored = (i64, String, Option<f64>, Option<Vec<u8>>);

fn text<T>(r: db::Result<T>) -> String {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.to_string(),
    }
}

// ---- Parameters ----

#[test]
fn a_parameter_count_mismatch_is_an_error_before_anything_runs() {
    let conn = memory();
    let few = conn.execute("INSERT INTO t (name, score) VALUES (?1, ?2)", ["a"]);
    assert_eq!(text(few), "Wrong number of parameters passed to query. Got 1, needed 2");
    let many = conn.execute("INSERT INTO t (name, score) VALUES (?1, ?2)", params!["a", 1.5, 7]);
    assert_eq!(text(many), "Wrong number of parameters passed to query. Got 3, needed 2");
    let none = conn.execute("INSERT INTO t (name) VALUES (?1)", []);
    assert_eq!(text(none), "Wrong number of parameters passed to query. Got 0, needed 1");
    let tuple = conn.execute("INSERT INTO t (name) VALUES (?1)", ("a", 2));
    assert_eq!(text(tuple), "Wrong number of parameters passed to query. Got 2, needed 1");
    assert_eq!(count(&conn), 0, "a statement with the wrong parameter count never steps");

    // The same on a cached statement, which keeps no binding from the
    // failed call: the next, correct call inserts exactly its own values.
    {
        let mut st = conn.prepare_cached("INSERT INTO t (name, score) VALUES (?1, ?2)").unwrap();
        assert_eq!(text(st.execute(["only"])), "Wrong number of parameters passed to query. Got 1, needed 2");
    }
    let mut st = conn.prepare_cached("INSERT INTO t (name, score) VALUES (?1, ?2)").unwrap();
    assert_eq!(st.execute(params!["b", 2.5]).unwrap(), 1);
    let row: (String, f64) = conn.query_row("SELECT name, score FROM t", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(row, ("b".to_string(), 2.5));
}

#[test]
fn every_parameter_shape_binds_in_order() {
    let conn = memory();
    let name = String::from("s");
    let blob: Vec<u8> = vec![0, 1, 2, 255];
    assert_eq!(
        conn.execute("INSERT INTO t (id, name, score, raw) VALUES (?1, ?2, ?3, ?4)", params![1_i64, name, 0.5_f64, blob]).unwrap(),
        1
    );
    assert_eq!(
        conn.execute("INSERT INTO t (id, name, score, raw) VALUES (?1, ?2, ?3, ?4)", (2_u32, "tuple", None::<f64>, &blob[..2])).unwrap(),
        1
    );
    assert_eq!(conn.execute("INSERT INTO t (id, name) VALUES (?1, ?2)", ["3", "array"]).unwrap(), 1);
    let dynamic: Vec<&dyn db::ToSql> = vec![&4_i32, &"slice"];
    assert_eq!(conn.execute("INSERT INTO t (id, name) VALUES (?1, ?2)", dynamic.as_slice()).unwrap(), 1);
    let values = [Value::Integer(5), Value::Text("iter".into()), Value::Null, Value::Blob(Vec::new())];
    assert_eq!(
        conn.execute("INSERT INTO t (id, name, score, raw) VALUES (?1, ?2, ?3, ?4)", db::params_from_iter(values.iter())).unwrap(),
        1
    );
    assert_eq!(conn.execute("INSERT INTO t (id, name, score) VALUES (6, 'bool', ?1)", [true]).unwrap(), 1);
    assert_eq!(conn.last_insert_rowid(), 6);

    let mut st = conn.prepare("SELECT id, name, score, raw FROM t ORDER BY id").unwrap();
    let rows: Vec<Stored> =
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().collect::<db::Result<_>>().unwrap();
    assert_eq!(
        rows,
        vec![
            (1, "s".to_string(), Some(0.5), Some(vec![0, 1, 2, 255])),
            (2, "tuple".to_string(), None, Some(vec![0, 1])),
            (3, "array".to_string(), None, None),
            (4, "slice".to_string(), None, None),
            // An empty blob is a blob of length zero, not NULL.
            (5, "iter".to_string(), None, Some(Vec::new())),
            (6, "bool".to_string(), Some(1.0), None),
        ]
    );
}

#[test]
fn a_bound_text_is_copied_and_stays_valid_across_steps() {
    let conn = memory();
    for i in 0..200 {
        conn.execute("INSERT INTO t (id, name) VALUES (?1, ?2)", params![i, format!("name-{i:04}")]).unwrap();
    }
    let mut st = conn.prepare("SELECT name FROM t WHERE name >= ?1 AND length(?2) = 4096 ORDER BY id").unwrap();
    let mut rows = {
        // The parameters die at the end of this block, before the first step.
        let floor = String::from("name-0100");
        let pad = vec![7_u8; 4096];
        st.query(params![floor, pad]).unwrap()
    };
    // Reuse the freed memory, so a binding that kept the pointer reads junk.
    let junk: Vec<Vec<u8>> = (0..64).map(|i| vec![b'z' - (i % 3) as u8; 4096]).collect();
    let junk_text: Vec<String> = (0..64).map(|_| String::from("zzzz-9999")).collect();
    let mut n = 0;
    while let Some(row) = rows.next().unwrap() {
        let name: String = row.get(0).unwrap();
        assert_eq!(name, format!("name-{:04}", 100 + n));
        n += 1;
    }
    assert_eq!(n, 100);
    drop((junk, junk_text));
}

// ---- Columns ----

#[test]
fn columns_read_as_the_stored_type_or_fail_with_rusqlites_text() {
    let conn = memory();
    conn.execute("INSERT INTO t (id, name, score, raw) VALUES (300, 'n', 2.5, x'0102')", []).unwrap();
    let mut st = conn.prepare("SELECT id, name, score, raw, NULL FROM t").unwrap();
    assert_eq!(st.column_count(), 5);
    let mut rows = st.query([]).unwrap();
    let row = rows.next().unwrap().unwrap();
    assert_eq!(row.get::<_, i64>(0).unwrap(), 300);
    assert_eq!(row.get::<_, u32>(0).unwrap(), 300);
    assert_eq!(row.get::<_, i32>(0).unwrap(), 300);
    assert_eq!(row.get::<_, f64>(0).unwrap(), 300.0, "an INTEGER reads as a float");
    assert!(row.get::<_, bool>(0).unwrap());
    assert_eq!(row.get::<_, Option<String>>(1).unwrap().as_deref(), Some("n"));
    assert_eq!(row.get::<_, Option<String>>(4).unwrap(), None);
    assert_eq!(row.get::<_, Value>(2).unwrap(), Value::Real(2.5));
    assert!(matches!(row.get_ref(3).unwrap(), ValueRef::Blob(b) if b == [1, 2]));
    assert!(matches!(row.get_ref(1).unwrap(), ValueRef::Text(b) if b == b"n"));
    assert!(matches!(row.get_ref(0).unwrap(), ValueRef::Integer(300)));
    assert!(matches!(row.get_ref(4).unwrap(), ValueRef::Null));

    assert_eq!(text(row.get::<_, u8>(0)), "Integer 300 out of range at index 0");
    assert_eq!(text(row.get::<_, String>(0)), "Invalid column type Integer at index: 0, name: id");
    assert_eq!(text(row.get::<_, i64>(1)), "Invalid column type Text at index: 1, name: name");
    assert_eq!(text(row.get::<_, i64>(2)), "Invalid column type Real at index: 2, name: score");
    assert_eq!(text(row.get::<_, String>(3)), "Invalid column type Blob at index: 3, name: raw");
    assert_eq!(text(row.get::<_, i64>(4)), "Invalid column type Null at index: 4, name: NULL");
    assert_eq!(text(row.get::<_, i64>(5)), "Invalid column index: 5");
    assert!(row.get_ref(5).is_err());
}

#[test]
fn text_that_is_not_utf8_is_rejected_on_read() {
    let conn = memory();
    conn.execute("INSERT INTO t (id, name) VALUES (1, CAST(x'61ff62' AS TEXT))", []).unwrap();
    let stored: String = conn.query_row("SELECT typeof(name) FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(stored, "text", "SQLite stores the bytes as TEXT without checking them");
    let read = conn.query_row("SELECT name FROM t", [], |r| r.get::<_, String>(0));
    assert_eq!(text(read), "invalid utf-8 sequence of 1 bytes from index 1 at index 0");
    let value = conn.query_row("SELECT name FROM t", [], |r| r.get::<_, Value>(0));
    assert_eq!(text(value), "invalid utf-8 sequence of 1 bytes from index 1 at index 0");
    let option = conn.query_row("SELECT name FROM t", [], |r| r.get::<_, Option<String>>(0));
    assert_eq!(text(option), "invalid utf-8 sequence of 1 bytes from index 1 at index 0");
    // The bytes themselves are still readable.
    let bytes =
        conn.query_row("SELECT name FROM t", [], |r| Ok(matches!(r.get_ref(0)?, ValueRef::Text(b) if b == [0x61, 0xff, 0x62]))).unwrap();
    assert!(bytes);
}

// ---- Errors ----

#[test]
fn error_texts_are_rusqlites() {
    let conn = memory();
    assert_eq!(text(conn.prepare("SELECT * FROM missing")), "no such table: missing");
    assert_eq!(text(conn.execute("DELETE FROM missing", [])), "no such table: missing");
    assert_eq!(text(conn.execute_batch("CREATE TABLE u (x); INSERT INTO missing VALUES (1);")), "no such table: missing");
    assert_eq!(text(conn.prepare_cached("SELECT nope FROM t")), "no such column: nope in SELECT nope FROM t at offset 7");
    assert_eq!(text(conn.prepare("SELEC 1")), "near \"SELEC\": syntax error in SELEC 1 at offset 0");
    assert_eq!(text(conn.query_row("SELECT id FROM t WHERE id = ?1", [1], |r| r.get::<_, i64>(0))), "Query returned no rows");
    assert_eq!(text(conn.execute("SELECT 1", [])), "Execute returned results - did you mean to call query?");
    assert_eq!(text(conn.prepare("SELECT 1; SELECT 2")), "Multiple statements provided");
    assert_eq!(text(conn.execute("INSERT INTO t (id) VALUES (1)", [])), "NOT NULL constraint failed: t.name");
    conn.execute("INSERT INTO t (name) VALUES ('once')", []).unwrap();
    assert_eq!(text(conn.execute("INSERT INTO t (name) VALUES ('once')", [])), "UNIQUE constraint failed: t.name");
    let mut st = conn.prepare_cached("INSERT INTO t (name) VALUES (?1)").unwrap();
    assert_eq!(text(st.execute(["once"])), "UNIQUE constraint failed: t.name");
    assert_eq!(st.execute(["twice"]).unwrap(), 1, "a statement that failed runs again");

    conn.execute_batch("CREATE VIRTUAL TABLE notes USING fts5(title, body, tokenize='unicode61');").unwrap();
    let fts = conn.query_row("SELECT title FROM notes WHERE notes MATCH ?1", ["a AND"], |r| r.get::<_, String>(0));
    assert_eq!(text(fts), "fts5: syntax error near \"\"");
    let mut st = conn.prepare("SELECT title FROM notes WHERE notes MATCH ?1").unwrap();
    let collected = st.query_map(["\"open"], |r| r.get::<_, String>(0)).unwrap().collect::<db::Result<Vec<_>>>();
    assert_eq!(text(collected), "unterminated string");
}

#[test]
fn no_row_is_none_through_optional_and_is_no_rows() {
    let conn = memory();
    let none = conn.query_row("SELECT name FROM t WHERE id = 9", [], |r| r.get::<_, String>(0)).optional().unwrap();
    assert_eq!(none, None);
    conn.execute("INSERT INTO t (id, name) VALUES (9, 'nine')", []).unwrap();
    let some = conn.query_row("SELECT name FROM t WHERE id = 9", [], |r| r.get::<_, String>(0)).optional().unwrap();
    assert_eq!(some.as_deref(), Some("nine"));
    let err = conn.query_row("SELECT name FROM t WHERE id = 1", [], |r| r.get::<_, String>(0)).unwrap_err();
    assert!(db::is_no_rows(&err));
    assert!(!db::is_busy(&err));
    // Any other error passes through `optional` unchanged.
    let other = conn.query_row("SELECT name FROM missing", [], |r| r.get::<_, String>(0)).optional();
    assert_eq!(text(other), "no such table: missing");
    let mut st = conn.prepare("SELECT 1 FROM t WHERE id = ?1").unwrap();
    assert!(st.exists([9]).unwrap());
    assert!(!st.exists([1]).unwrap());
}

#[test]
fn a_held_write_lock_is_busy_with_the_locked_text() {
    let dir = Scratch::new("busy");
    let path = dir.file("busy.sqlite");
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (x);").unwrap();
    let other = Connection::open(&path).unwrap();
    other.busy_timeout(Duration::from_millis(0)).unwrap();
    let held = Transaction::new_unchecked(&holder, TransactionBehavior::Immediate).unwrap();
    held.execute("INSERT INTO t VALUES (1)", []).unwrap();

    let err = Transaction::new_unchecked(&other, TransactionBehavior::Immediate).expect_err("the write lock is held");
    assert!(db::is_busy(&err), "{err}");
    assert!(!db::is_no_rows(&err));
    assert_eq!(err.to_string(), "database is locked");
    let err = other.execute("INSERT INTO t VALUES (2)", []).unwrap_err();
    assert!(db::is_busy(&err), "{err}");
    assert_eq!(err.to_string(), "database is locked");
    // A reader is not blocked (WAL) and does not see the uncommitted row.
    assert_eq!(other.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    held.commit().unwrap();
    assert_eq!(other.execute("INSERT INTO t VALUES (2)", []).unwrap(), 1);
}

#[test]
fn open_failures_name_the_path() {
    let dir = Scratch::new("open");
    let missing = dir.file("no-such-folder").join("db.sqlite");
    let err = text(Connection::open(&missing));
    assert_eq!(err, format!("unable to open database file: {}", missing.display()));
    let absent = dir.file("absent.sqlite");
    let err = text(Connection::open_with_flags(&absent, OpenFlags::SQLITE_OPEN_READ_ONLY));
    assert_eq!(err, format!("unable to open database file: {}", absent.display()));

    let path = dir.file("ro.sqlite");
    Connection::open(&path).unwrap().execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);").unwrap();
    let ro = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).unwrap();
    assert_eq!(ro.query_row("SELECT x FROM t", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(text(ro.execute("INSERT INTO t VALUES (2)", [])), "attempt to write a readonly database");
}

#[test]
fn a_connection_knows_its_file() {
    let dir = Scratch::new("path");
    let path = dir.file("p.sqlite");
    let conn = Connection::open(&path).unwrap();
    // SQLite reports the resolved path; macOS temp dirs sit behind the /var -> /private/var link.
    let real = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
    assert_eq!(conn.path().map(|p| real(std::path::Path::new(p))), Some(real(&path)), "the full path of the main database");
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Deferred).unwrap();
    assert_eq!(tx.path(), conn.path());
    assert_eq!(Connection::open_in_memory().unwrap().path(), Some(""), "a database in memory has no file");
}

// ---- Statements ----

#[test]
fn one_cached_statement_used_twice_at_once_gives_both_their_rows() {
    let conn = memory();
    for i in 1..=4 {
        conn.execute("INSERT INTO t (id, name) VALUES (?1, ?2)", params![i, format!("n{i}")]).unwrap();
    }
    const SQL: &str = "SELECT id, name FROM t WHERE id >= ?1 ORDER BY id";
    let mut outer = conn.prepare_cached(SQL).unwrap();
    let mut outer_rows = outer.query([1]).unwrap();
    let mut seen = Vec::new();
    while let Some(row) = outer_rows.next().unwrap() {
        let id: i64 = row.get(0).unwrap();
        // The same SQL, while the outer statement is mid-iteration.
        let mut inner = conn.prepare_cached(SQL).unwrap();
        let later: Vec<String> = inner.query_map([id + 1], |r| r.get(1)).unwrap().collect::<db::Result<_>>().unwrap();
        seen.push((id, later.join(",")));
    }
    assert_eq!(seen, vec![(1, "n2,n3,n4".to_string()), (2, "n3,n4".to_string()), (3, "n4".to_string()), (4, String::new())]);
    drop(outer_rows);
    drop(outer);
    // Both copies went back; the statement still answers, with white space
    // around the text naming the same statement.
    let mut again = conn.prepare_cached(&format!("  {SQL}\n")).unwrap();
    assert_eq!(again.query_map([4], |r| r.get::<_, String>(1)).unwrap().collect::<db::Result<Vec<_>>>().unwrap(), vec!["n4".to_string()]);
}

#[test]
fn a_cached_statement_survives_eviction_and_a_small_cache() {
    let conn = memory();
    conn.set_prepared_statement_cache_capacity(2);
    conn.execute("INSERT INTO t (id, name) VALUES (1, 'a')", []).unwrap();
    for round in 0..3 {
        for n in 0..5 {
            let sql = format!("SELECT id + {n} FROM t");
            let got: i64 = conn.prepare_cached(&sql).unwrap().query_row([], |r| r.get(0)).unwrap();
            assert_eq!(got, 1 + n, "round {round}");
        }
    }
    conn.set_prepared_statement_cache_capacity(0);
    let got: i64 = conn.prepare_cached("SELECT id FROM t").unwrap().query_row([], |r| r.get(0)).unwrap();
    assert_eq!(got, 1);
}

#[test]
fn a_statement_reruns_after_a_partial_read_and_after_an_error() {
    let conn = memory();
    for i in 1..=3 {
        conn.execute("INSERT INTO t (id, name) VALUES (?1, ?2)", params![i, i.to_string()]).unwrap();
    }
    let mut st = conn.prepare("SELECT id FROM t WHERE id >= ?1 ORDER BY id").unwrap();
    {
        let mut rows = st.query([2]).unwrap();
        let first: i64 = rows.next().unwrap().unwrap().get(0).unwrap();
        assert_eq!(first, 2);
        // Dropped with a row still pending.
    }
    let all: Vec<i64> = st.query_map([1], |r| r.get(0)).unwrap().collect::<db::Result<_>>().unwrap();
    assert_eq!(all, vec![1, 2, 3]);
    let mut rows = st.query([3]).unwrap();
    assert!(rows.next().unwrap().is_some());
    assert!(rows.next().unwrap().is_none());
    assert!(rows.next().unwrap().is_none(), "past the end stays at the end");
    drop(rows);
    // The closure's error is the query's error.
    let failed = st.query_map([1], |r| r.get::<_, String>(0)).unwrap().collect::<db::Result<Vec<_>>>();
    assert_eq!(text(failed), "Invalid column type Integer at index: 0, name: id");
    assert_eq!(st.query_row([3], |r| r.get::<_, i64>(0)).unwrap(), 3);
}

#[test]
fn a_batch_runs_every_statement_and_ignores_rows() {
    let conn = memory();
    conn.execute_batch(
        "-- a comment first
         PRAGMA user_version = 7;
         SELECT 1;
         INSERT INTO t (name) VALUES ('a'); INSERT INTO t (name) VALUES ('b');
         PRAGMA user_version;
         ",
    )
    .unwrap();
    assert_eq!(count(&conn), 2);
    assert_eq!(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), 7);
    conn.execute_batch("").unwrap();
    conn.execute_batch("  \n -- nothing\n").unwrap();
    // The first failure stops the batch; what ran before it stays.
    assert_eq!(
        text(
            conn.execute_batch("INSERT INTO t (name) VALUES ('c'); INSERT INTO t (name) VALUES ('a'); INSERT INTO t (name) VALUES ('d');")
        ),
        "UNIQUE constraint failed: t.name"
    );
    assert_eq!(count(&conn), 3);

    let mut ran = Vec::new();
    db::execute_each(&conn, "INSERT INTO t (name) VALUES ('e'); -- note\n INSERT INTO t (name) VALUES ('f');\n", || {
        ran.push(count(&conn));
        Ok::<(), db::Error>(())
    })
    .unwrap();
    assert_eq!(ran, vec![4, 5]);
    assert_eq!(conn.total_changes(), 5);
}

// ---- Transactions ----

#[test]
fn an_uncommitted_transaction_rolls_back_on_drop() {
    let mut conn = memory();
    assert!(conn.is_autocommit());
    {
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        assert!(!conn.is_autocommit());
        tx.execute("INSERT INTO t (name) VALUES ('gone')", []).unwrap();
        assert_eq!(count(&tx), 1);
    }
    assert!(conn.is_autocommit(), "the drop ended the transaction");
    assert_eq!(count(&conn), 0, "and rolled it back");

    {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        tx.execute("INSERT INTO t (name) VALUES ('kept')", []).unwrap();
        tx.commit().unwrap();
    }
    assert!(conn.is_autocommit());
    assert_eq!(count(&conn), 1);

    {
        let tx = conn.transaction().unwrap();
        tx.execute("INSERT INTO t (name) VALUES ('gone')", []).unwrap();
    }
    assert_eq!(count(&conn), 1);
    {
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute("INSERT INTO t (name) VALUES ('kept too')", []).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(count(&conn), 2);
    // A second BEGIN inside a transaction fails and leaves the first intact.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    assert_eq!(text(Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)), "cannot start a transaction within a transaction");
    assert!(!conn.is_autocommit());
    drop(tx);
    assert!(conn.is_autocommit());
}

#[test]
fn a_connection_moves_to_another_thread() {
    let conn = memory();
    conn.execute("INSERT INTO t (name) VALUES ('here')", []).unwrap();
    let there = std::thread::spawn(move || {
        conn.execute("INSERT INTO t (name) VALUES ('there')", []).unwrap();
        count(&conn)
    })
    .join()
    .unwrap();
    assert_eq!(there, 2);
}
