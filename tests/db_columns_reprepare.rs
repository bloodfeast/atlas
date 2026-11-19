//! A statement's column count follows SQLite's own. SQLite compiles a
//! statement again, inside a step, when the schema has changed since it was
//! prepared; a `SELECT *` then has the table's new columns. Both backends
//! must read them: the count a statement answers with, and the bound
//! `Row::get` checks an index against, are the statement's as it last
//! stepped, never the count it was prepared with (DEPS2 stage 1, the
//! performance review of #248, finding 3: the owned binding keeps the count
//! on the statement).

use atlas::db::Connection;

fn table() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL); INSERT INTO t (id, name) VALUES (1, 'one');").unwrap();
    conn
}

#[test]
fn a_held_statement_reads_a_column_added_after_it_was_prepared() {
    let conn = table();
    let mut st = conn.prepare("SELECT * FROM t").unwrap();
    assert_eq!(st.column_count(), 2);
    {
        let mut rows = st.query([]).unwrap();
        let row = rows.next().unwrap().unwrap();
        assert_eq!(row.get::<_, String>(1).unwrap(), "one");
        assert_eq!(row.get::<_, String>(2).unwrap_err().to_string(), "Invalid column index: 2");
    }
    conn.execute_batch("ALTER TABLE t ADD COLUMN extra TEXT NOT NULL DEFAULT 'more';").unwrap();
    {
        let mut rows = st.query([]).unwrap();
        let row = rows.next().unwrap().unwrap();
        assert_eq!(row.get::<_, String>(2).unwrap(), "more", "the step compiled the statement again: three columns");
        assert_eq!(row.get::<_, String>(3).unwrap_err().to_string(), "Invalid column index: 3");
    }
    assert_eq!(st.column_count(), 3);
}

#[test]
fn a_cached_statement_reads_a_column_added_while_it_sat_in_the_cache() {
    let conn = table();
    conn.set_prepared_statement_cache_capacity(8);
    {
        let mut st = conn.prepare_cached("SELECT * FROM t").unwrap();
        let name: String = st.query_row([], |r| r.get(1)).unwrap();
        assert_eq!(name, "one");
    }
    conn.execute_batch("ALTER TABLE t ADD COLUMN extra TEXT NOT NULL DEFAULT 'more';").unwrap();
    let mut st = conn.prepare_cached("SELECT * FROM t").unwrap();
    let extra: String = st.query_row([], |r| r.get(2)).unwrap();
    assert_eq!(extra, "more");
    assert_eq!(st.column_count(), 3);
}

#[test]
fn a_statement_run_to_its_end_has_the_new_count_too() {
    let conn = table();
    let mut st = conn.prepare("SELECT * FROM t WHERE id = 99").unwrap();
    assert!(!st.exists([]).unwrap());
    conn.execute_batch("ALTER TABLE t ADD COLUMN extra TEXT;").unwrap();
    assert!(!st.exists([]).unwrap(), "no row, but the step compiled the statement again");
    assert_eq!(st.column_count(), 3);
}
