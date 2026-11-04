//! Several real server processes writing one memory store at once: a
//! committed write is never reported as an error, and a crash between the
//! commit and the `.jsonl` copy leaves nothing for `--memory-check` to fail
//! on.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use atlas::json;
use atlas::json::Value;
use common::*;

/// A project with one vault and the memory exports of a real setup, with a
/// snapshot due on the first write (a fresh store has never taken one).
fn project(root: &Path) -> PathBuf {
    write(&root.join("vault").join("Index.md"), "# Index\n");
    let cfg = root.join("atlas.json");
    write(
        &cfg,
        r#"{
  "db": "state/atlas.sqlite",
  "vaults": [{"name": "notes", "path": "vault"}],
  "models": {"auto_download": false, "files": []},
  "memory": {
    "export_dir": "exports",
    "exports": [
      {"name": "mem", "shape": "memory", "path": "memory"},
      {"name": "rules", "shape": "items", "path": "RULES.md"},
      {"name": "log", "shape": "headings", "path": "LOG.md", "handoffs": true}
    ]
  }
}"#,
    );
    cfg
}

/// Runs one server over `calls` (tool name, arguments) and returns every
/// tools/call answer as (text, isError).
fn serve(cfg: &Path, calls: Vec<(String, Value)>) -> Vec<(String, bool)> {
    let mut child = Command::new(bin())
        .arg("--config")
        .arg(cfg)
        .current_dir(cfg.parent().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn atlas");
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let mut ask = |id: usize, msg: Value, stdin: &mut std::process::ChildStdin| -> Value {
        writeln!(stdin, "{msg}").unwrap();
        loop {
            line.clear();
            assert!(out.read_line(&mut line).unwrap() > 0, "server closed stdout");
            let m = atlas::json::parse(line.trim()).unwrap();
            if m["id"].as_i64() == Some(id as i64) {
                return m;
            }
        }
    };
    ask(0, json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "race"}}}), &mut stdin);
    let mut answers = Vec::new();
    for (i, (tool, args)) in calls.into_iter().enumerate() {
        let id = i + 1;
        let m = ask(id, json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": args}}), &mut stdin);
        let text = m["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
        answers.push((text, m["result"]["isError"] == json!(true)));
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
    answers
}

fn writes(proc: usize, n: usize) -> Vec<(String, Value)> {
    (0..n)
        .map(|i| match i % 3 {
            0 => ("remember".to_string(), json!({"kind": "memory", "key": format!("race-{proc}-{i}"), "title": format!("race {proc} {i}"), "body": format!("race body {proc} {i}"), "reason": "concurrent memory write test"})),
            1 => ("remember".to_string(), json!({"kind": "rule", "body": format!("- Race rule {proc} {i}."), "reason": "concurrent rule write test"})),
            _ => ("handoff".to_string(), json!({"lane": format!("duo-{proc}"), "title": format!("h {proc} {i}"), "text": format!("handoff {proc} {i}")})),
        })
        .collect()
}

/// Two sessions writing at once: before the fix about a third of the writes
/// came back as errors (the snapshot's live count raced the other
/// process's commit) and the failed copies piled up in the snapshot folder.
#[test]
fn two_sessions_writing_at_once_never_see_an_error() {
    let t = Temp::new("race-two");
    let cfg = project(t.path());
    const EACH: usize = 40;
    let handles: Vec<_> = (0..2)
        .map(|p| {
            let cfg = cfg.clone();
            std::thread::spawn(move || serve(&cfg, writes(p, EACH)))
        })
        .collect();
    let mut errors = Vec::new();
    for h in handles {
        for (i, (text, err)) in h.join().unwrap().into_iter().enumerate() {
            if err || text.contains("failed verification") {
                errors.push(format!("write {i}: {text}"));
            }
        }
    }
    assert!(errors.is_empty(), "{} of {} writes reported an error: {:?}", errors.len(), 2 * EACH, &errors[..errors.len().min(3)]);
    // Every snapshot left on disk verifies: no failed copies pile up.
    let snaps = t.path().join("state").join("snapshots");
    for e in std::fs::read_dir(&snaps).into_iter().flatten().flatten() {
        let c = rusqlite::Connection::open_with_flags(e.path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let ic: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ic, "ok", "{}", e.path().display());
    }
    let check = Command::new(bin()).arg("--config").arg(&cfg).arg("--memory-check").current_dir(t.path()).output().unwrap();
    assert!(check.status.success(), "{}{}", String::from_utf8_lossy(&check.stdout), String::from_utf8_lossy(&check.stderr));
}

/// The snapshot itself, taken over and over by this process while a server
/// process commits writes: every copy that is kept verifies and no call
/// fails (before the fix the live count raced the other commit and the
/// call failed, leaving the bad copy behind).
#[test]
fn snapshots_taken_while_another_process_writes_never_fail() {
    let t = Temp::new("race-snap");
    let cfg = project(t.path());
    let store_path = t.path().join("state").join("memory.sqlite");
    let store = atlas::memory::MemoryStore::open(&store_path).unwrap();
    let writer = {
        let cfg = cfg.clone();
        std::thread::spawn(move || serve(&cfg, (0..150).map(|i| ("remember".to_string(), json!({"kind": "memory", "key": format!("snap-race-{i}"), "body": format!("snapshot race body {i}"), "reason": "snapshot race write test"}))).collect()))
    };
    let dir = t.path().join("race-snaps");
    let mut failures = Vec::new();
    let mut tries = 0;
    while !writer.is_finished() || tries < 20 {
        tries += 1;
        if let Err(e) = atlas::memory::check::snapshot(&store, &dir, u64::MAX) {
            failures.push(e.to_string());
        }
    }
    for (text, err) in writer.join().unwrap() {
        assert!(!err, "{text}");
    }
    assert!(failures.is_empty(), "{} of {tries} snapshots failed: {:?}", failures.len(), &failures[..failures.len().min(3)]);
    let live: i64 = store.conn().query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0)).unwrap();
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let c = rusqlite::Connection::open_with_flags(e.path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let ic: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0)).unwrap();
        assert!(ic == "ok" && n <= live, "{}: {ic}, {n} records", e.path().display());
    }
}

/// A server killed mid-stream, over and over (as a crash or a closed
/// terminal would): `--memory-check` still passes. Before the fix a kill
/// between the commit and the `.jsonl` append left the copy one batch short
/// for good.
#[test]
fn a_server_killed_mid_write_leaves_the_store_and_its_copy_agreeing() {
    let t = Temp::new("race-kill");
    let text = kill_rounds(t.path(), 8, |round| Kill::After(150 + round * 90));
    assert!(!text.contains("copy:"), "{text}");
}

/// When [`kill_rounds`] kills a server.
#[derive(Clone, Copy)]
enum Kill {
    /// This many ms after it starts on its stream of writes.
    After(u64),
    /// The moment the `.jsonl` copy grows: a batch has appended and (an
    /// fsync later) is about to commit.
    OnAppend,
}

/// Kills a server `rounds` times as `when(round)` says, while it works
/// through a stream of writes, then returns what `--memory-check` printed.
fn kill_rounds(root: &Path, rounds: u64, when: impl Fn(u64) -> Kill) -> String {
    let cfg = project(root);
    let copy = root.join("state").join("memory.sqlite.jsonl");
    for round in 0..rounds {
        let size = |p: &Path| std::fs::metadata(p).map_or(0, |m| m.len());
        let start = size(&copy);
        let mut child = Command::new(bin())
            .arg("--config")
            .arg(&cfg)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn atlas");
        {
            let stdin = child.stdin.as_mut().unwrap();
            writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "kill"}}})).unwrap();
            for i in 1..400 {
                let call = match i % 3 {
                    0 => json!({"name": "journal", "arguments": {"lane": "kill-lane", "entry": "note", "text": format!("kill {round} {i}")}}),
                    1 => json!({"name": "remember", "arguments": {"kind": "memory", "key": format!("kill-{round}-{i}"), "body": format!("kill body {i}"), "reason": "kill test write record"}}),
                    _ => json!({"name": "remember", "arguments": {"kind": "rule", "body": format!("- Kill rule {round} {i}."), "reason": "kill test rule write"}}),
                };
                let _ = writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": i, "method": "tools/call", "params": call}));
            }
        }
        match when(round) {
            Kill::After(ms) => std::thread::sleep(std::time::Duration::from_millis(ms)),
            Kill::OnAppend => {
                // A spin on the copy's size, bounded so a server that never
                // writes cannot hang the test.
                let t0 = std::time::Instant::now();
                while size(&copy) == start && t0.elapsed() < std::time::Duration::from_secs(20) {
                    std::hint::spin_loop();
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let check = Command::new(bin()).arg("--config").arg(&cfg).arg("--memory-check").current_dir(root).output().unwrap();
    String::from_utf8_lossy(&check.stdout).into_owned()
}

/// The kill test many times over (`ATLAS_KILL_RUNS` runs, default 40): odd
/// runs kill every other server the moment a batch appends to the copy
/// (the window between the append and the commit, a fresh store's first
/// batch included) and the rest after a delay; even runs kill after delays
/// spread from 10 ms to 1.5 s. A failing
/// run's folder is kept and named in the output.
#[test]
#[ignore = "stress: run with --ignored"]
fn stress_servers_killed_mid_write_leave_the_store_and_its_copy_agreeing() {
    let runs: u64 = std::env::var("ATLAS_KILL_RUNS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut failed = Vec::new();
    for run in 0..runs {
        let t = Temp::new("race-kill-stress");
        let text = if run % 2 == 1 {
            kill_rounds(t.path(), 8, |round| if round % 2 == 0 { Kill::OnAppend } else { Kill::After(100 + (run * 29 + round * 61) % 900) })
        } else {
            kill_rounds(t.path(), 8, |round| Kill::After(if round == 0 { 10 + (run * 13) % 500 } else { 20 + (run * 37 + round * 173) % 1500 }))
        };
        if text.contains("copy:") {
            let keep = t.path().with_extension("kept");
            let _ = std::fs::rename(t.path(), &keep);
            eprintln!("run {run}: {}\n{text}", keep.display());
            failed.push(run);
        }
    }
    eprintln!("kill stress: {runs} runs, {} failed", failed.len());
    assert!(failed.is_empty(), "kill stress: {} of {runs} runs failed: {failed:?}", failed.len());
}
