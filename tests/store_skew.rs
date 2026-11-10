//! An older binary against stores a newer one made: it never opens them for
//! use and never writes them, and the refusal reaches the user at every entry
//! (the MCP server, the hook over it, the command hook, the CLI, the doctor),
//! never as a failed server start.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use atlas::json;
use atlas::json::Value;
use common::*;

/// A memory store a newer atlas made: its version above this binary's, one
/// table with one row, in rollback-journal mode (so a WAL switch would show).
fn newer_memory(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(&format!(
        "PRAGMA journal_mode=DELETE; CREATE TABLE future(x TEXT); INSERT INTO future VALUES('kept'); PRAGMA user_version = {};",
        atlas::memory::SCHEMA_VERSION + 1
    ))
    .unwrap();
}

/// An index store whose derived tables a newer atlas wrote with newer parse
/// rules, and the schema numbers its refusal names (never the crate version).
const NEWER_INDEX: &str = "1/parse-99/0.0.1";
const NEWER_FOUND: &str = "1/parse-99";

fn newer_index(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(&format!(
        "PRAGMA journal_mode=DELETE; CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES('schema', '{NEWER_INDEX}'); INSERT INTO meta VALUES('open_stamp', 'theirs');
         CREATE TABLE notes(id INTEGER PRIMARY KEY, path TEXT); INSERT INTO notes VALUES(1, 'kept.md');"
    ))
    .unwrap();
}

/// No `-wal` or `-journal` beside `path`.
fn no_sidecars(path: &Path) {
    for s in ["-wal", "-journal"] {
        let mut p = path.as_os_str().to_owned();
        p.push(s);
        assert!(!Path::new(&p).exists(), "{} was made", Path::new(&p).display());
    }
}

/// Runs the server over `msgs` with `home` as ATLAS_HOME; the replies by id.
fn serve(fx: &Fx, msgs: &[Value]) -> (bool, Vec<Value>, String) {
    let mut cmd = Command::new(bin());
    for (n, p) in &fx.specs {
        cmd.arg("--vault").arg(format!("{n}={}", p.display()));
    }
    let mut child = cmd
        .arg("--db")
        .arg(&fx.db)
        .env("ATLAS_HOME", fx.tmp.path().join("home"))
        .env_remove("ATLAS_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for m in msgs {
            writeln!(stdin, "{m}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let replies = String::from_utf8(out.stdout).unwrap().lines().map(|l| json::parse(l).unwrap()).collect();
    (out.status.success(), replies, String::from_utf8_lossy(&out.stderr).into_owned())
}

fn by_id(replies: &[Value], id: i64) -> &Value {
    replies.iter().find(|r| r["id"] == json!(id)).unwrap_or_else(|| panic!("no reply {id}: {replies:#?}"))
}

/// A tool call's text and whether it is an error.
fn text(r: &Value) -> (String, bool) {
    (r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string(), r["result"]["isError"] == json!(true))
}

fn init() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "skew-test", "version": "1"}}})
}

fn hook(id: i64, event: &str, session: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hook", "arguments": {"event": event, "session_id": session, "prompt": "hi", "transcript_path": ""}}})
}

/// The refusal a hook answer carries: the user's message and the model's.
fn told(answer: &str) -> Option<(String, String)> {
    if answer.is_empty() {
        return None;
    }
    let v = json::parse(answer).unwrap();
    Some((v["systemMessage"].as_str().unwrap().to_string(), v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string()))
}

#[test]
fn a_newer_index_store_starts_the_server_but_every_call_answers_the_refusal() {
    let fx = Fx::new("skew-index");
    newer_index(&fx.db);
    let before = std::fs::read(&fx.db).unwrap();
    let msgs = vec![
        init(),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "read_note", "arguments": {"vault": "perf", "path": "Index.md"}}}),
        hook(3, "UserPromptSubmit", "s1"),
        hook(4, "UserPromptSubmit", "s1"),
        hook(5, "UserPromptSubmit", "s2"),
        hook(6, "PreToolUse", "s3"),
        json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 8, "method": "tools/list"}),
    ];
    let (ok, replies, stderr) = serve(&fx, &msgs);
    assert!(ok, "never a failed start: {stderr}");
    assert!(stderr.contains("store too new"), "{stderr}");
    let instructions = by_id(&replies, 1)["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("store too new") && instructions.contains(NEWER_FOUND), "{instructions}");
    let (t, err) = text(by_id(&replies, 2));
    assert!(err && t.contains("the index store at") && t.contains(NEWER_FOUND) && t.contains("Update atlas"), "{t}");
    let (user, model) = told(&text(by_id(&replies, 3)).0).expect("the first hook of a session tells it");
    assert!(user.contains("store too new") && model == user, "{user}");
    assert_eq!(text(by_id(&replies, 4)).0, "", "once per session");
    assert!(told(&text(by_id(&replies, 5)).0).is_some(), "each session is told");
    assert_eq!(text(by_id(&replies, 6)).0, "", "an event that cannot carry context answers nothing");
    assert!(by_id(&replies, 7)["result"].is_object());
    assert!(by_id(&replies, 8)["result"]["tools"].as_array().is_some_and(|t| !t.is_empty()));
    assert_eq!(std::fs::read(&fx.db).unwrap(), before, "the index store is not changed");
    no_sidecars(&fx.db);
    assert!(!fx.db.with_file_name("memory.sqlite").exists(), "no store is opened while refused");
}

#[test]
fn a_newer_memory_store_is_told_by_the_hook_and_every_memory_tool() {
    let fx = Fx::new("skew-memory");
    let mem = fx.db.with_file_name("memory.sqlite");
    newer_memory(&mem);
    let before = std::fs::read(&mem).unwrap();
    let msgs = vec![
        init(),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        hook(2, "UserPromptSubmit", "s1"),
        hook(3, "UserPromptSubmit", "s1"),
        hook(4, "SessionStart", "s2"),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "recall", "arguments": {"query": "anything"}}}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "read_note", "arguments": {"vault": "perf", "path": "Index.md"}}}),
    ];
    let (ok, replies, stderr) = serve(&fx, &msgs);
    assert!(ok, "{stderr}");
    let (user, _) = told(&text(by_id(&replies, 2)).0).expect("told on the first hook");
    assert!(user.contains("the memory store at") && user.contains(&format!("up to schema {}", atlas::memory::SCHEMA_VERSION)), "{user}");
    assert_eq!(text(by_id(&replies, 3)).0, "", "once per session");
    assert!(told(&text(by_id(&replies, 4)).0).is_some());
    let (t, err) = text(by_id(&replies, 5));
    assert!(err && t.contains("store too new"), "a memory tool answers the refusal as its error: {t}");
    let (t, err) = text(by_id(&replies, 6));
    assert!(!err && t.contains("Index"), "the vault tools still work: {t}");
    assert_eq!(std::fs::read(&mem).unwrap(), before, "the memory store is not changed");
    no_sidecars(&mem);
}

/// `atlas` with `args` and `home` as ATLAS_HOME (and a fresh Claude folder):
/// whether it succeeded, and stdout then stderr.
fn cli(home: &Path, args: &[&str], stdin: &str) -> (bool, String, String) {
    let mut child = Command::new(bin())
        .args(args)
        .env("ATLAS_HOME", home)
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env_remove("ATLAS_CONFIG")
        .env_remove("CLAUDE_CODE_SESSION_ATTENDED")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

#[test]
fn the_command_hook_the_cli_and_the_doctor_each_say_so() {
    let t = Temp::new("skew-cli");
    let home = t.path().to_path_buf();
    let mem = home.join("state").join("memory.sqlite");
    newer_memory(&mem);
    let before = std::fs::read(&mem).unwrap();
    // The command hook (SessionStart): told, and a success, since a failing
    // command hook shows only in verbose mode.
    let ev = json!({"hook_event_name": "SessionStart", "session_id": "s", "source": "startup", "cwd": home.to_string_lossy().to_string()});
    let (ok, out, err) = cli(&home, &["--hook"], &ev.to_string());
    assert!(ok, "{err}");
    let (user, model) = told(&out).unwrap_or_else(|| panic!("the command hook tells it: {out:?} {err}"));
    assert!(user.contains("store too new") && model == user, "{user}");
    // A CLI subcommand: the message and a failure.
    let (ok, out, err) = cli(&home, &["rules", "counts"], "");
    assert!(!ok && (out + &err).contains("store too new"), "{err}");
    // The doctor: a line, and exit 1.
    let (ok, out, err) = cli(&home, &["doctor"], "");
    assert!(!ok && out.contains("store: store too new: the memory store at"), "{out}{err}");
    assert_eq!(std::fs::read(&mem).unwrap(), before, "the memory store is not changed");
    no_sidecars(&mem);

    // A newer index store beside no memory store (a current one would be
    // made): the command hook tells it before it opens anything, so its
    // token log never writes there; the raw-open CLI reports refuse too.
    let t = Temp::new("skew-cli-index");
    let home = t.path().to_path_buf();
    let db = home.join("state").join("atlas.sqlite");
    newer_index(&db);
    let before = std::fs::read(&db).unwrap();
    let (ok, out, err) = cli(&home, &["--hook"], &ev.to_string());
    assert!(ok, "{err}");
    let (user, _) = told(&out).unwrap_or_else(|| panic!("the command hook tells it: {out:?} {err}"));
    assert!(user.contains("the index store at") && user.contains(NEWER_FOUND), "{user}");
    assert!(!home.join("state").join("memory.sqlite").exists(), "nothing is opened while refused");
    for args in [&["--token-report"][..], &["--misses-week"], &["--hook-report"]] {
        let (ok, out, err) = cli(&home, args, "");
        assert!(!ok && (out + &err).contains("store too new"), "{args:?}: {err}");
    }
    let (ok, out, err) = cli(&home, &["doctor"], "");
    assert!(!ok && out.contains("store: store too new: the index store at"), "{out}{err}");
    assert_eq!(std::fs::read(&db).unwrap(), before, "the index store is not changed");
    no_sidecars(&db);
}

/// The stored `meta` value of `key` in the index database at `db`.
fn meta(db: &Path, key: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    rusqlite::Connection::open(db).unwrap().query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional().unwrap()
}

fn notes(db: &Path) -> i64 {
    rusqlite::Connection::open(db).unwrap().query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0)).unwrap()
}

/// Opens the store at `fx` and indexes the vaults.
fn open_and_scan(fx: &Fx) {
    let mut s = atlas::store::Store::open(&fx.db, atlas::paths::Vaults::new(&fx.specs).unwrap()).expect("opens");
    s.rescan().unwrap();
}

#[test]
fn two_crate_versions_with_one_index_schema_interoperate() {
    // Two binaries, simulated by the crate version in the stored `schema`
    // row: ours, and one with the same shape and parse rules a version
    // above or below it.
    let fx = Fx::new("skew-interop");
    open_and_scan(&fx);
    let ours = meta(&fx.db, "schema").unwrap();
    let (prefix, _) = ours.rsplit_once('/').unwrap();
    let indexed = notes(&fx.db);
    assert!(indexed > 0);

    // A newer crate version wrote the tables (with its own stamp): this
    // older binary opens them as they are, rewrites neither the `schema`
    // row nor the open stamp, and drops nothing, open after open, so that
    // binary never rebuilds after this one.
    let newer = format!("{prefix}/999.0.0-dev");
    {
        let c = rusqlite::Connection::open(&fx.db).unwrap();
        c.execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [&newer]).unwrap();
        c.execute("UPDATE meta SET value = 'theirs' WHERE key = 'open_stamp'", []).unwrap();
    }
    for _ in 0..2 {
        open_and_scan(&fx);
        assert_eq!(meta(&fx.db, "schema").as_deref(), Some(newer.as_str()), "the schema row is not rewritten");
        assert_eq!(meta(&fx.db, "open_stamp").as_deref(), Some("theirs"), "the open stamp is not rewritten");
        assert_eq!(notes(&fx.db), indexed, "nothing is dropped or rebuilt");
    }
    assert!(atlas::store::index_skew(&fx.db).is_empty(), "the doctor says nothing");
    // Its stamp, when it matches this binary's rules, opens on the fast path.
    {
        let c = rusqlite::Connection::open(&fx.db).unwrap();
        c.execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [&ours]).unwrap();
    }
    open_and_scan(&fx);
    let stamp = meta(&fx.db, "open_stamp").unwrap();
    let theirs_stamp = stamp.replacen(&ours, &newer, 1);
    {
        let c = rusqlite::Connection::open(&fx.db).unwrap();
        c.execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [&newer]).unwrap();
        c.execute("UPDATE meta SET value = ?1 WHERE key = 'open_stamp'", [&theirs_stamp]).unwrap();
    }
    open_and_scan(&fx);
    assert_eq!(meta(&fx.db, "open_stamp").as_deref(), Some(theirs_stamp.as_str()));
    assert_eq!(meta(&fx.db, "schema").as_deref(), Some(newer.as_str()));

    // An older crate version wrote them: rebuilt once, with this binary's
    // schema and stamp written, then opened on the fast path with no
    // second rebuild.
    let older = format!("{prefix}/0.0.1");
    rusqlite::Connection::open(&fx.db).unwrap().execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [&older]).unwrap();
    drop(atlas::store::Store::open(&fx.db, atlas::paths::Vaults::new(&fx.specs).unwrap()).unwrap());
    assert_eq!(meta(&fx.db, "schema").as_deref(), Some(ours.as_str()), "rebuilt to this binary's schema");
    assert_eq!(notes(&fx.db), 0, "the older derived tables were dropped");
    open_and_scan(&fx);
    assert_eq!(notes(&fx.db), indexed);
    let stamp = meta(&fx.db, "open_stamp");
    open_and_scan(&fx);
    assert_eq!(notes(&fx.db), indexed, "no second rebuild");
    assert_eq!(meta(&fx.db, "open_stamp"), stamp);

    // Higher parse rules or a higher table shape are refused, nothing written.
    for (schema, found) in [(format!("1/parse-{}/0.0.1", 99), "1/parse-99"), ("9/parse-0/0.0.1".to_string(), "9/parse-0")] {
        rusqlite::Connection::open(&fx.db).unwrap().execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [&schema]).unwrap();
        let before = std::fs::read(&fx.db).unwrap();
        let e = atlas::store::Store::open(&fx.db, atlas::paths::Vaults::new(&fx.specs).unwrap()).err().expect("refused");
        assert!(matches!(&e, atlas::Error::StoreTooNew { found: f, .. } if f == found), "{e}");
        assert_eq!(std::fs::read(&fx.db).unwrap(), before);
    }
}

#[test]
fn the_index_store_refuses_a_newer_schema_and_rebuilds_an_older_one() {
    let fx = Fx::new("skew-store");
    newer_index(&fx.db);
    let before = std::fs::read(&fx.db).unwrap();
    let e = atlas::store::Store::open(&fx.db, atlas::paths::Vaults::new(&fx.specs).unwrap()).err().expect("refused");
    assert!(matches!(&e, atlas::Error::StoreTooNew { store, found, .. } if store == "index store" && found == NEWER_FOUND), "{e}");
    assert_eq!(std::fs::read(&fx.db).unwrap(), before, "nothing written, not even the journal mode");
    no_sidecars(&fx.db);
    assert_eq!(atlas::store::index_skew(&fx.db), vec![e]);
    // An older crate version's tables are rebuilt as before.
    rusqlite::Connection::open(&fx.db).unwrap().execute("UPDATE meta SET value = '1/parse-1/0.0.1' WHERE key = 'schema'", []).unwrap();
    drop(atlas::store::Store::open(&fx.db, atlas::paths::Vaults::new(&fx.specs).unwrap()).expect("an older index opens"));
    assert!(atlas::store::index_skew(&fx.db).is_empty());
    let notes: i64 = rusqlite::Connection::open(&fx.db).unwrap().query_row("SELECT COUNT(*) FROM notes WHERE path = 'kept.md'", [], |r| r.get(0)).unwrap();
    assert_eq!(notes, 0, "the older derived tables were dropped and rebuilt");
}
