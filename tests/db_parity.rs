//! Parity of the two SQLite backends, by what atlas answers and by the
//! database files themselves. The backends are never linked into one binary,
//! so each runs this same script and writes a transcript: the answers to the
//! fixture's queries (text and JSON), what a memory store gives back after a
//! round trip, the schema and row counts of both database files, and the
//! error texts atlas prints or matches on. The two transcripts must be the
//! same bytes, and each backend must be able to carry on from the files the
//! other wrote.
//!
//! Run plainly (no variable set), the test checks one backend against
//! itself: the script twice, the transcripts equal, then the first run's
//! files reopened. To compare the backends, name a folder and run each
//! backend's test binary in turn:
//!
//! ```text
//! ATLAS_PARITY_DIR=<dir>                           cargo test --test db_parity                        (writes <dir>/rusqlite.txt)
//! ATLAS_PARITY_DIR=<dir>                           cargo test --test db_parity <owned features>       (writes <dir>/owned.txt)
//! cmp <dir>/rusqlite.txt <dir>/owned.txt
//! ATLAS_PARITY_DIR=<dir> ATLAS_PARITY_STEP=reopen  cargo test --test db_parity                        (rusqlite on owned's files)
//! ATLAS_PARITY_DIR=<dir> ATLAS_PARITY_STEP=reopen  cargo test --test db_parity <owned features>       (owned on rusqlite's files)
//! ```
//!
//! A reopen works on a copy of the other backend's folder, so it can be run
//! again. Nothing in a transcript names the backend, the SQLite version or
//! its compile options: those are meant to change while the transcript stays
//! the same.

mod common;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use atlas::config::Config;
use atlas::db::{self, Connection};
use atlas::json;
use atlas::memory::check::check;
use atlas::memory::{Draft, MemoryStore};
use atlas::paths::Vaults;
use atlas::store::{AtlasSetup, Store};
use atlas::tools::Server;
use common::Temp;

/// This build's backend, and the one it is compared with.
const BACKEND: &str = if cfg!(feature = "owned-sqlite") { "owned" } else { "rusqlite" };
const OTHER: &str = if cfg!(feature = "owned-sqlite") { "rusqlite" } else { "owned" };

/// The fixed times of the memory versions, so their hashes and chains are
/// the same in every run.
const TIMES: [&str; 4] = ["2026-01-02T03:04:05.000Z", "2026-01-02T03:04:06.000Z", "2026-01-03T00:00:00.000Z", "2026-01-04T00:00:00.000Z"];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The fixture corpus, indexed in place into `db`. The files are the
/// checkout's own, so both backends see the same paths and modified times.
fn open_index(db: &Path) -> Server {
    let c = Config::load(&manifest().join("fixtures/generic/atlas.json")).unwrap();
    let specs: Vec<(String, PathBuf)> = c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
    let mut setup = AtlasSetup::from_config(&c).unwrap();
    // No watcher thread: the answers come from the walk alone.
    setup.watch = false;
    let mut store = Store::open_with(db, Vaults::new(&specs).unwrap(), setup).unwrap();
    store.rescan().unwrap();
    store.rescan_sources(None).unwrap();
    let mut server = Server::new(store);
    server.set_client("parity");
    server
}

/// A line of `fixtures/generic/queries.txt` as a tool call, as the bench
/// reads it.
fn query_args(line: &str) -> Option<(&'static str, json::Value)> {
    let (tool, rest) = line.trim().split_once(' ')?;
    let rest = rest.trim();
    match tool {
        "lookup" => Some(("lookup", json!({"id": rest}))),
        "search" => Some(("search", json!({"query": rest, "mode": "sections"}))),
        "ids" => Some(("ids", json!({"prefix": rest}))),
        _ => None,
    }
}

/// Every query answered as a client gets it, in text and in JSON.
fn answers(server: &mut Server) -> String {
    let queries = fs::read_to_string(manifest().join("fixtures/generic/queries.txt")).unwrap();
    let mut calls: Vec<(&str, json::Value)> = queries.lines().filter_map(query_args).collect();
    assert!(calls.len() >= 6, "the fixture's query set was read");
    // Beyond the bench's set: an identifier nothing defines, a word no
    // section holds, FTS5 syntax a query must not break on, and every
    // identifier.
    calls.push(("lookup", json!({"id": "TK-999"})));
    calls.push(("search", json!({"query": "zebraquartz", "mode": "sections"})));
    calls.push(("search", json!({"query": "\"unbalanced AND (cache", "mode": "sections"})));
    calls.push(("search", json!({"query": "cache owner", "mode": "sections"})));
    calls.push(("ids", json!({})));
    let mut out = String::new();
    for (tool, args) in &calls {
        for format in ["text", "json"] {
            let mut a = args.clone();
            if format == "json" {
                a["format"] = json!("json");
            }
            let answer = match server.answer(tool, &a) {
                Ok(answer) => answer.into_text(),
                Err(e) => format!("ERROR: {e}"),
            };
            let _ = writeln!(out, "-- {tool} {args} ({format})\n{answer}");
        }
    }
    out
}

/// One row of `sqlite_master`.
type Entry = (String, String, String, String);

/// A database file as either backend must read it: its schema text, the
/// rows of every table, and SQLite's own checks.
fn file_part(path: &Path) -> String {
    let conn = Connection::open(path).unwrap();
    let mut out = String::new();
    let entries: Vec<Entry> = {
        let mut st = conn.prepare("SELECT type, name, tbl_name, coalesce(sql, '') FROM sqlite_master ORDER BY type, name").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().collect::<db::Result<_>>().unwrap()
    };
    for (kind, name, table, sql) in &entries {
        let _ = writeln!(out, "{kind} {name} on {table}: {sql}");
    }
    for (kind, name, _, _) in &entries {
        if kind == "table" {
            match conn.query_row(&format!("SELECT count(*) FROM \"{name}\""), [], |r| r.get::<_, i64>(0)) {
                Ok(n) => {
                    let _ = writeln!(out, "rows {name}: {n}");
                }
                Err(e) => {
                    let _ = writeln!(out, "rows {name}: ERROR: {e}");
                }
            }
        }
    }
    for pragma in ["user_version", "journal_mode", "page_size", "encoding", "foreign_keys"] {
        let value = conn.query_row(&format!("SELECT CAST({pragma} AS TEXT) FROM pragma_{pragma}"), [], |r| r.get::<_, String>(0)).unwrap();
        let _ = writeln!(out, "pragma {pragma}: {value}");
    }
    let _ = writeln!(out, "integrity_check: {}", integrity(&conn));
    out
}

/// `PRAGMA integrity_check` and `foreign_key_check`, as one line.
fn integrity(conn: &Connection) -> String {
    let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    let broken: i64 = conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).unwrap();
    format!("{check}, {broken} foreign key violations")
}

fn draft(title: &str, body: &str, reason: &str, time: &str) -> Draft {
    Draft {
        title: title.into(),
        body: body.into(),
        reason: reason.into(),
        author: "parity".into(),
        client: "parity".into(),
        time: Some(time.into()),
        ..Draft::default()
    }
}

/// The memory round trip's writes: a memory, a journal entry, and a second
/// version of the memory.
fn memory_write(path: &Path) {
    let mut store = MemoryStore::open(path).unwrap();
    let mut w = store.writer().unwrap();
    let cache = draft("The widget cache bound", "The widget cache holds 64 entries.\n", "parity: a first version", TIMES[0]);
    let first = w.create("parity-widget-cache", "memory", "project", "", &cache, None).unwrap();
    let entry = draft(
        "Parity lane opened",
        "Parity lane opened\nText stays whole: na\u{ef}ve caf\u{e9}, \u{65e5}\u{672c}\u{8a9e}, a tab\tand a quote '.\n",
        "parity: a journal entry",
        TIMES[1],
    );
    w.create("journal:parity:0001", "journal", "note", "parity", &entry, None).unwrap();
    let grown = draft(
        "The widget cache bound",
        "The widget cache holds 128 entries and evicts the least recently used first.\n",
        "parity: a second version",
        TIMES[2],
    );
    let second = w.revise("parity-widget-cache", Some(&first.sha), &grown).unwrap();
    assert!(second.changed && second.seq == 2);
    w.commit().unwrap();
}

/// What the memory store gives back: heads, a history with its hash chain,
/// a lexical search, a lane, the counts, and the store's own check.
fn memory_read(path: &Path) -> String {
    let store = MemoryStore::open(path).unwrap();
    let mut out = String::new();
    for key in ["parity-widget-cache", "journal:parity:0001", "parity-nothing"] {
        match store.head(key).unwrap() {
            Some(h) => {
                let _ = writeln!(
                    out,
                    "head {key}: kind={} sub={} scope={} status={} seq={} time={} title={:?} meta={:?} body={:?} sha={} chain={}",
                    h.kind, h.sub, h.scope, h.status, h.seq, h.time, h.title, h.meta, h.body, h.sha, h.chain
                );
            }
            None => {
                let _ = writeln!(out, "head {key}: none");
            }
        }
    }
    for v in store.history("parity-widget-cache").unwrap() {
        let _ = writeln!(
            out,
            "version {} seq={} time={} title={:?} body={:?} sha={} chain={} reason={:?} author={:?}",
            v.key, v.seq, v.time, v.title, v.body, v.sha, v.chain, v.reason, v.author
        );
    }
    let terms = vec!["widget".to_string(), "cache".to_string()];
    for (h, score, matched) in store.search(&terms, None, None, 10).unwrap() {
        let _ = writeln!(out, "search widget cache: {} seq={} score={score:?} matched={matched}", h.key, h.seq);
    }
    for h in store.lane("parity").unwrap() {
        let _ = writeln!(out, "lane parity: {} {:?}", h.key, h.title);
    }
    for (kind, n) in store.counts().unwrap() {
        let _ = writeln!(out, "records {kind}: {n}");
    }
    let report = check(&store, &[], None, true).unwrap();
    let _ = writeln!(
        out,
        "check: {} records, {} versions, {} copy lines, problems {:?}, warnings {:?}",
        report.records, report.versions, report.copy_lines, report.problems, report.warnings
    );
    out
}

fn fails<T>(r: db::Result<T>) -> db::Error {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => e,
    }
}

/// The error texts atlas prints or matches on, and what `is_busy` and
/// `is_no_rows` say of each, on a scratch database in `dir`.
fn errors(dir: &Path) -> String {
    let path = dir.join("errors.sqlite");
    let mut out = String::new();
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
             CREATE VIRTUAL TABLE f USING fts5(body);
             INSERT INTO t VALUES (1, 'one');
             INSERT INTO f VALUES ('alpha beta');",
        )
        .unwrap();
        let other = Connection::open(&path).unwrap();
        other.busy_timeout(Duration::ZERO).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let busy_begin = fails(other.execute_batch("BEGIN IMMEDIATE"));
        let busy_write = fails(other.execute("INSERT INTO t VALUES (5, 'five')", []));
        conn.execute_batch("ROLLBACK").unwrap();
        let cases = [
            ("no such table", fails(conn.query_row("SELECT x FROM missing", [], |r| r.get::<_, i64>(0)))),
            ("no such column", fails(conn.prepare("SELECT nope FROM t"))),
            ("not SQL", fails(conn.execute_batch("SELEC 1"))),
            ("busy at BEGIN IMMEDIATE", busy_begin),
            ("busy at a write", busy_write),
            ("FTS5 syntax", fails(conn.query_row("SELECT count(*) FROM f WHERE f MATCH ?1", ["alpha AND ("], |r| r.get::<_, i64>(0)))),
            ("FTS5 string", fails(conn.query_row("SELECT count(*) FROM f WHERE f MATCH ?1", ["\"unbalanced"], |r| r.get::<_, i64>(0)))),
            ("FTS5 column", fails(conn.query_row("SELECT count(*) FROM f WHERE f MATCH ?1", ["nocol:alpha"], |r| r.get::<_, i64>(0)))),
            ("no rows", fails(conn.query_row("SELECT id FROM t WHERE id = 99", [], |r| r.get::<_, i64>(0)))),
            ("unique", fails(conn.execute("INSERT INTO t VALUES (2, 'one')", []))),
            ("not null", fails(conn.execute("INSERT INTO t VALUES (3, NULL)", []))),
            ("execute with rows", fails(conn.execute("SELECT id FROM t", []))),
            ("parameter count", fails(conn.execute("INSERT INTO t VALUES (?1, ?2)", ["3"]))),
            ("column type", fails(conn.query_row("SELECT name FROM t WHERE id = 1", [], |r| r.get::<_, i64>(0)))),
            ("open", fails(Connection::open(dir.join("no-such-folder").join("x.sqlite")))),
        ];
        for (what, e) in &cases {
            let _ = writeln!(out, "{what}: {e} [busy={} no_rows={}]", db::is_busy(e), db::is_no_rows(e));
        }
    }
    for ext in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(dir.join(format!("errors.sqlite{ext}")));
    }
    out
}

/// A transcript's parts, in the order they are written.
struct Parts {
    answers: String,
    index_file: String,
    memory: String,
    memory_file: String,
    errors: String,
}

impl Parts {
    /// The transcript, with `dir` (the one thing that differs between two
    /// runs) written as `<DIR>`.
    fn text(&self, dir: &Path) -> String {
        let text = format!(
            "== answers (fixtures/generic)\n{}== index.sqlite\n{}== memory\n{}== memory.sqlite\n{}== errors\n{}",
            self.answers, self.index_file, self.memory, self.memory_file, self.errors
        );
        let shown = dir.display().to_string();
        let mut text = text.replace(&shown, "<DIR>");
        for other in [shown.replace('\\', "/"), shown.replace('\\', "\\\\")] {
            text = text.replace(&other, "<DIR>");
        }
        text
    }
}

/// Runs the script in a new folder `dir` and returns its transcript.
fn transcript(dir: &Path) -> String {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();
    let (index, memory) = (dir.join("index.sqlite"), dir.join("memory.sqlite"));
    let answers = answers(&mut open_index(&index));
    let index_file = file_part(&index);
    memory_write(&memory);
    let memory_text = memory_read(&memory);
    let memory_file = file_part(&memory);
    Parts { answers, index_file, memory: memory_text, memory_file, errors: errors(dir) }.text(dir)
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Where two texts first differ, for a failure message.
fn first_difference(a: &str, b: &str) -> String {
    let (mut x, mut y) = (a.lines(), b.lines());
    let mut n = 1;
    loop {
        match (x.next(), y.next()) {
            (Some(l), Some(r)) if l == r => n += 1,
            (None, None) => return "no line differs (line endings?)".into(),
            (l, r) => return format!("line {n}:\n  {}\n  {}", l.unwrap_or("<end>"), r.unwrap_or("<end>")),
        }
    }
}

/// Carries on from the files in `theirs` (a run whose transcript is
/// `expected`), on a copy in `work`: the files read as that run left them,
/// the same answers come out of them, and a write through each store lands
/// and leaves both files sound.
fn reopen(theirs: &Path, expected: &str, work: &Path) {
    let _ = fs::remove_dir_all(work);
    copy_dir(theirs, work);
    let (index, memory) = (work.join("index.sqlite"), work.join("memory.sqlite"));
    // The files first, before anything of this run is written to them.
    let index_file = file_part(&index);
    let memory_file = file_part(&memory);
    let answers = answers(&mut open_index(&index));
    let memory_text = memory_read(&memory);
    let got = Parts { answers, index_file, memory: memory_text, memory_file, errors: errors(work) }.text(work);
    assert!(got == expected, "reading {} gives another transcript, {}", theirs.display(), first_difference(&got, expected));

    // A third version and a new record through the memory store's writer.
    {
        let mut store = MemoryStore::open(&memory).unwrap();
        let head = store.head("parity-widget-cache").unwrap().unwrap();
        let mut w = store.writer().unwrap();
        let third = draft("The widget cache bound", "The widget cache holds 256 entries.\n", "parity: written after the reopen", TIMES[3]);
        assert_eq!(w.revise("parity-widget-cache", Some(&head.sha), &third).unwrap().seq, 3);
        w.create(
            "parity-reopened",
            "memory",
            "project",
            "",
            &draft("Reopened", "Written after the reopen.\n", "parity: reopen", TIMES[3]),
            None,
        )
        .unwrap();
        w.commit().unwrap();
        assert_eq!(store.history("parity-widget-cache").unwrap().len(), 3);
        let report = check(&store, &[], None, true).unwrap();
        assert!(report.ok(), "{:?}", report.problems);
        assert_eq!(integrity(store.conn()), "ok, 0 foreign key violations");
    }
    // The index answers again after a write to its file.
    {
        let conn = Connection::open(&index).unwrap();
        conn.execute_batch(
            "BEGIN IMMEDIATE; CREATE TABLE parity_reopen (backend TEXT NOT NULL); INSERT INTO parity_reopen VALUES ('x'); COMMIT;",
        )
        .unwrap();
        assert_eq!(integrity(&conn), "ok, 0 foreign key violations");
    }
    let again = self::answers(&mut open_index(&index));
    let expected_answers = expected.split("== index.sqlite\n").next().unwrap().strip_prefix("== answers (fixtures/generic)\n").unwrap();
    assert!(again == expected_answers, "the answers changed after a write, {}", first_difference(&again, expected_answers));
}

#[test]
fn both_backends_answer_alike_and_read_each_others_files() {
    let Some(dir) = std::env::var_os("ATLAS_PARITY_DIR").map(PathBuf::from) else {
        // One backend against itself.
        let t = Temp::new("db-parity");
        let first = transcript(&t.path().join("first"));
        let second = transcript(&t.path().join("second"));
        assert!(first == second, "two runs of one backend differ, {}", first_difference(&first, &second));
        reopen(&t.path().join("first"), &first, &t.path().join("reopened"));
        return;
    };
    if std::env::var("ATLAS_PARITY_STEP").is_ok_and(|s| s == "reopen") {
        let file = dir.join(format!("{OTHER}.txt"));
        let theirs =
            fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e} (run the {OTHER} backend's transcript first)", file.display()));
        reopen(&dir.join(OTHER), &theirs, &dir.join(format!("{BACKEND}-reopens-{OTHER}")));
        println!("{BACKEND} reopened {}: same transcript, writes land, integrity ok", dir.join(OTHER).display());
    } else {
        let text = transcript(&dir.join(BACKEND));
        let file = dir.join(format!("{BACKEND}.txt"));
        fs::write(&file, &text).unwrap();
        println!("{BACKEND} transcript: {} ({} bytes)", file.display(), text.len());
    }
}
