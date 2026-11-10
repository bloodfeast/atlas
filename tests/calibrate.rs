//! Self-calibration through the binary: the server calibrates in the
//! background unless turned off, a hook never does, `--calibrate` runs it by
//! hand, and `--token-report` says what the baseline was measured from.
//! Every process here reads a synthetic Claude Code folder, never the
//! user's.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use atlas::json;
use common::{bin, write, Temp};

/// A configuration with a `notes` vault, and five transcripts of six vault
/// reads each in a Claude Code folder of its own.
fn fixture(t: &Temp) -> (PathBuf, PathBuf, PathBuf) {
    let root = t.path().join("work");
    write(&root.join("notes").join("a.md"), "# A\n");
    let s = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let db = t.path().join("state").join("atlas.sqlite");
    let cfg = t.path().join("atlas.json");
    // The default skips would leave out every folder under the temporary folder.
    write(&cfg, &format!(r#"{{"db": "{}", "vaults": [{{"name": "notes", "path": "{}"}}], "calibrate": {{"skip": ["scratch"]}}}}"#, s(&db), s(&root.join("notes"))));
    let claude = t.path().join("claude");
    let folder = claude.join("projects").join(atlas::transcripts::project_of_cwd(&root));
    for f in 0..5 {
        let mut lines = Vec::new();
        for i in 0..6 {
            let ts = format!("2026-01-01T00:{f}{i}:00Z");
            let id = format!("s{f}-{i}");
            lines.push(json!({"type": "assistant", "timestamp": ts, "cwd": s(&root), "message": {"content": [{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": s(&root.join("notes").join("a.md"))}}]}}).to_string());
            lines.push(json!({"type": "user", "timestamp": ts, "cwd": s(&root), "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "x".repeat(100)}]}}).to_string());
            lines.push(json!({"type": "assistant", "timestamp": ts, "cwd": s(&root), "message": {"content": [{"type": "tool_use", "id": format!("{id}b"), "name": "Bash", "input": {"command": "cargo build"}}]}}).to_string());
        }
        write(&folder.join(format!("s{f}.jsonl")), &lines.join("\n"));
    }
    (cfg, claude, db)
}

fn command(t: &Temp, cfg: &Path, claude: &Path, off: bool) -> Command {
    let mut c = Command::new(bin());
    c.arg("--config")
        .arg(cfg)
        .current_dir(t.path())
        .env("CLAUDE_CONFIG_DIR", claude)
        .env("ATLAS_HOME", t.path().join("home-atlas"))
        .env("HOME", t.path())
        .env("USERPROFILE", t.path())
        .env_remove("ATLAS_CONFIG")
        .env_remove("ATLAS_PROJECT");
    if off {
        c.env("ATLAS_CALIBRATE", "off");
    } else {
        c.env_remove("ATLAS_CALIBRATE");
    }
    c
}

/// A server that has answered `initialize`: its start is done.
fn serve(t: &Temp, cfg: &Path, claude: &Path, off: bool) -> Child {
    let mut child = command(t, cfg, claude, off).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"clientInfo": {"name": "t"}}});
    writeln!(child.stdin.as_mut().unwrap(), "{init}").unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    assert!(line.contains("\"id\":1"), "{line}");
    child
}

fn rows(db: &Path) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row("SELECT count(*) FROM baseline_log", [], |r| r.get(0)).unwrap_or(0)
}

/// How many tables named like `pattern` the store has (none without a store).
fn tables(db: &Path, pattern: &str) -> i64 {
    let Ok(conn) = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return 0 };
    conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name LIKE ?1", [pattern], |r| r.get(0)).unwrap_or(0)
}

#[test]
fn test_processes_have_background_calibration_off() {
    // .cargo/config.toml sets it for every process cargo runs, so no server
    // a test starts reads the user's own transcripts.
    assert_eq!(std::env::var("ATLAS_CALIBRATE").as_deref(), Ok("off"));
}

#[test]
fn the_server_calibrates_in_the_background_unless_turned_off() {
    let t = Temp::new("calibrate-serve");
    let (cfg, claude, db) = fixture(&t);

    // Turned off: the server starts, answers and writes no baseline.
    let mut off = serve(&t, &cfg, &claude, true);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    drop(off.stdin.take());
    off.wait().unwrap();
    assert_eq!(rows(&db), 0);
    assert_eq!(tables(&db, "calibrate_%"), 0);

    // A hook never calibrates.
    let mut hook = command(&t, &cfg, &claude, false).arg("--hook").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let ev = json!({"hook_event_name": "SessionStart", "session_id": "s", "source": "startup", "cwd": t.path().to_string_lossy().to_string()});
    hook.stdin.take().unwrap().write_all(ev.to_string().as_bytes()).unwrap();
    hook.wait().unwrap();
    assert_eq!(rows(&db), 0);
    // A hook exits at once, which would end a calibration thread before it
    // saved anything: so also no sign that one began (it takes the lock,
    // then creates its tables, before reading a transcript).
    assert!(!atlas::fsio::lock_path(&db.parent().unwrap().join("calibrate")).exists(), "a hook took the calibration lock");
    assert_eq!(tables(&db, "calibrate_%"), 0, "a hook created the calibration tables");
    // Nothing else a hook does reads the baseline, so its table is a sign too.
    assert_eq!(tables(&db, "baseline_log"), 0, "a hook read the baseline");

    // On: the baseline appears while the server serves.
    let mut on = serve(&t, &cfg, &claude, false);
    let start = std::time::Instant::now();
    while rows(&db) == 0 && start.elapsed() < std::time::Duration::from_secs(60) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(on.stdin.take());
    let out = on.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(rows(&db), 1, "{err}");

    let report = command(&t, &cfg, &claude, true).arg("--token-report").output().unwrap();
    let text = String::from_utf8_lossy(&report.stdout);
    assert!(text.contains("30 pre-atlas lookup episodes in 5 of 5 transcripts before "), "{text}");
    // The hook above logged the store's first atlas answer: that is the cutoff.
    assert!(text.contains("(the store's first atlas answer)") && text.contains(&format!("lookup rules v{}", atlas::calibrate::RULES_VERSION)), "{text}");
    assert!(!text.contains("stale"), "{text}");

    // By hand: progress and the result, the same numbers from the stored calls.
    let by_hand = command(&t, &cfg, &claude, true).arg("--calibrate").output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&by_hand.stdout), String::from_utf8_lossy(&by_hand.stderr));
    assert!(by_hand.status.success(), "{stderr}");
    assert!(stderr.contains("calibrating (by hand)"), "{stderr}");
    assert!(stdout.contains("30 pre-atlas lookup episodes in 5 of 5 transcripts") && stdout.contains("0 transcripts read"), "{stdout}");
    assert_eq!(rows(&db), 2);
}

#[test]
fn too_little_history_gives_the_upper_bound_only() {
    let t = Temp::new("calibrate-few");
    let (cfg, claude, db) = fixture(&t);
    // Two of the five transcripts: 12 episodes.
    for f in 2..5 {
        std::fs::remove_file(claude.join("projects").join(atlas::transcripts::project_of_cwd(&t.path().join("work"))).join(format!("s{f}.jsonl"))).unwrap();
    }
    let out = command(&t, &cfg, &claude, true).arg("--calibrate").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(rows(&db), 1);
    let report = command(&t, &cfg, &claude, true).arg("--token-report").output().unwrap();
    let text = String::from_utf8_lossy(&report.stdout);
    assert!(text.contains("measured ratio per answer: none, too little pre-atlas history: 12 pre-atlas lookup episodes in 2 of 2 transcripts"), "{text}");
    assert!(text.contains("needs 30 episodes in 5 transcripts") && text.contains("runs again when the lookup rules or the configured targets change"), "{text}");
    assert!(text.contains("upper bound per answer"), "{text}");
}
