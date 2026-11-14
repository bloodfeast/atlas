//! Several real server processes writing one memory store at once: a
//! committed write is never reported as an error, and a crash between the
//! commit and the `.jsonl` copy leaves nothing for `--memory-check` to fail
//! on.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

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

/// How many calls each writer makes beside the checks at most. The checks
/// stop the writers when they end; this caps them too, so on a slow machine
/// the store (and with it each check) cannot grow for as long as the checks
/// take. Enough that writes still overlap the checks: each test asserts the
/// store grew between the first check and the last.
const CHECK_WRITES: usize = 400;

/// The wall-clock bound over the whole of [`checks_beside_writers`]: every
/// `--memory-check`, every writer's wait for an answer and every writer's
/// exit. Past it every child is killed and the test fails, naming the stage
/// that was stuck, instead of hanging until CI cancels the job.
const CHECK_DEADLINE: Duration = Duration::from_secs(300);

/// Every child [`checks_beside_writers`] starts: the owner kills and reaps
/// all of them when it is dropped, on a panic too, so none outlives the
/// test. A writer's share only adds to the list.
struct Reap {
    kids: Arc<Mutex<Vec<Kid>>>,
    owner: bool,
}

type Kid = Arc<Mutex<Child>>;

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Reap {
    fn owner() -> Reap {
        Reap { kids: Arc::default(), owner: true }
    }
    fn share(&self) -> Reap {
        Reap { kids: self.kids.clone(), owner: false }
    }
    fn add(&self, c: Child) -> Kid {
        let k = Arc::new(Mutex::new(c));
        locked(&self.kids).push(k.clone());
        k
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        if self.owner {
            for k in locked(&self.kids).iter() {
                let mut c = locked(k);
                if !matches!(c.try_wait(), Ok(Some(_))) {
                    let _ = c.kill();
                }
                let _ = c.wait();
            }
        }
    }
}

/// Waits for `kid` to exit until `deadline`: None if it has not.
fn exit_by(kid: &Kid, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        if let Ok(Some(s)) = locked(kid).try_wait() {
            return Some(s);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The versions count of a `--memory-check` report's first line.
fn versions_in(report: &str) -> i64 {
    report.split(" records, ").nth(1).and_then(|r| r.split(' ').next()).and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// What [`checks_beside_writers`] saw.
struct Beside {
    /// Each run's problem lines.
    runs: Vec<Vec<String>>,
    /// Versions the store grew by from before the writers started to after
    /// the checks.
    grown: i64,
    /// Versions it grew by from the first check to the last: the writes
    /// overlapped the checks.
    during: i64,
}

/// `--memory-check` run `count` times beside two writers, all of it bounded
/// by [`CHECK_DEADLINE`].
fn checks_beside_writers(cfg: &Path, count: usize, plant: impl FnOnce(&Path)) -> Beside {
    let t0 = Instant::now();
    let deadline = t0 + CHECK_DEADLINE;
    let reap = Reap::owner();
    // One `--memory-check`, waited for until the deadline (never
    // `.output()`, which waits for ever).
    let check = |stage: &str| -> (bool, String) {
        let mut c = Command::new(bin()).arg("--config").arg(cfg).arg("--memory-check").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("spawn atlas --memory-check");
        let mut out = c.stdout.take().unwrap();
        let kid = reap.add(c);
        let text = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        });
        let Some(status) = exit_by(&kid, deadline) else {
            panic!("checks beside writers: {stage} still running {} s after the start: every child is killed", CHECK_DEADLINE.as_secs());
        };
        (status.success(), text.join().unwrap())
    };
    let root = cfg.parent().unwrap().to_path_buf();
    plant(&root);
    let before = versions_in(&check("the check before the writers").1);
    let stop = Arc::new(AtomicBool::new(false));
    // Handoffs and journal entries alone: each server stops after
    // `CHECK_WRITES` calls, or when told to.
    let writers: Vec<_> = (0..2)
        .map(|p| {
            let (cfg, stop, reap) = (cfg.to_path_buf(), stop.clone(), reap.share());
            std::thread::spawn(move || keep_writing(&cfg, p, &stop, deadline, &reap))
        })
        .collect();
    let mut runs = Vec::new();
    let mut seen = Vec::new();
    for i in 0..count {
        let (ok, text) = check(&format!("check {} of {count}", i + 1));
        let problems: Vec<String> = text.lines().skip(1).filter(|l| !l.starts_with("warning:")).map(str::to_string).collect();
        assert_eq!(ok, problems.is_empty(), "{text}");
        seen.push(versions_in(&text));
        runs.push(problems);
    }
    let after = versions_in(&check("the check after the writers").1);
    stop.store(true, Ordering::Relaxed);
    let mut calls = Vec::new();
    for w in writers {
        match w.join().expect("a writer panicked") {
            Ok((n, wait)) => calls.push(format!("{n} calls, longest answer {:.1} s", wait.as_secs_f64())),
            Err(stage) => panic!("checks beside writers: {stage} ({} s after the start): every child is killed", t0.elapsed().as_secs()),
        }
    }
    eprintln!("checks beside writers: {count} checks in {:.1} s; writers (cap {CHECK_WRITES} calls): {}", t0.elapsed().as_secs_f64(), calls.join("; "));
    Beside { runs, grown: after - before, during: seen.last().copied().unwrap_or(0) - seen.first().copied().unwrap_or(0) }
}

/// One server writing until told to stop, or `CHECK_WRITES` calls, through
/// every export a writer may touch: a handoff (its own lane) and a journal
/// entry. Each answer and the exit are waited for until `deadline` only:
/// the error names the stage that was stuck. The number of calls made, and
/// the longest wait for an answer (a writer starved by the other shows).
fn keep_writing(cfg: &Path, proc: usize, stop: &AtomicBool, deadline: Instant, reap: &Reap) -> Result<(usize, Duration), String> {
    let mut child = Command::new(bin()).arg("--config").arg(cfg).current_dir(cfg.parent().unwrap()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let out = child.stdout.take().unwrap();
    let kid = reap.add(child);
    // The answers, read on their own thread so a wait can end at the
    // deadline; it ends when the server's stdout closes.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut longest = Duration::ZERO;
    let mut ask = |id: usize, msg: Value| -> Result<(), String> {
        let asked = Instant::now();
        writeln!(stdin, "{msg}").map_err(|e| format!("writer {proc}: call {id} could not be sent ({e})"))?;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(line)) => {
                    let m = atlas::json::parse(line.trim()).map_err(|e| format!("writer {proc}: an answer that is not JSON ({e})"))?;
                    if m["id"].as_i64() == Some(id as i64) {
                        longest = longest.max(asked.elapsed());
                        return Ok(());
                    }
                }
                Ok(Err(e)) => return Err(format!("writer {proc}: reading the answer to call {id}: {e}")),
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(format!("writer {proc}: no answer to call {id} by the deadline")),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(format!("writer {proc}: the server closed stdout before answering call {id}")),
            }
        }
    };
    ask(0, json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "writer"}}}))?;
    let mut i = 1;
    while i <= CHECK_WRITES && !stop.load(Ordering::Relaxed) {
        let call = if i % 2 == 0 {
            json!({"name": "handoff", "arguments": {"lane": "SHARED", "title": format!("h {proc} {i}"), "text": format!("handoff {proc} {i}")}})
        } else {
            json!({"name": "journal", "arguments": {"lane": "SHARED", "entry": "note", "text": format!("note {proc} {i}")}})
        };
        ask(i, json!({"jsonrpc": "2.0", "id": i, "method": "tools/call", "params": call}))?;
        i += 1;
    }
    drop(ask);
    drop(stdin);
    match exit_by(&kid, deadline) {
        Some(s) if s.success() => Ok((i - 1, longest)),
        Some(s) => Err(format!("writer {proc}: the server exited with {s} after {} calls", i - 1)),
        None => Err(format!("writer {proc}: the server did not exit by the deadline after its stdin closed ({} calls)", i - 1)),
    }
}

/// Seeds a store with records of every export, so `--memory-check` compares
/// real files and the writers have something to run beside.
fn seeded(root: &Path) -> PathBuf {
    let cfg = project(root);
    let seed: Vec<(String, Value)> = (0..6).flat_map(|i| writes(9, 3).into_iter().map(move |(t, mut a)| {
        let o = a.as_object_mut().unwrap();
        if let Some(Value::String(k)) = o.get("key").cloned() {
            o.insert("key".into(), Value::String(format!("{k}-{i}")));
        }
        if let Some(Value::String(k)) = o.get("body").cloned() {
            o.insert("body".into(), Value::String(format!("{k} {i}")));
        }
        (t, a)
    })).collect();
    for (text, err) in serve(&cfg, seed) {
        assert!(!err, "{text}");
    }
    cfg
}

/// The false alarm: each run beside writers reported corruption, 62% of runs
/// before the fix, so 10 clean runs in a row by luck are about 6e-5.
#[test]
fn a_check_beside_writers_reports_nothing_on_a_sound_store() {
    let t = Temp::new("check-sound");
    let cfg = seeded(t.path());
    let b = checks_beside_writers(&cfg, 10, |_| {});
    let bad: Vec<_> = b.runs.iter().filter(|r| !r.is_empty()).collect();
    assert!(bad.is_empty(), "{bad:?}");
    assert!(b.grown > 0, "the writers had stopped, so this proved nothing");
    assert!(b.during > 0, "no write between the first check and the last, so the checks ran beside nothing");
}

/// A real fault still shows while writers run, and it is the fault that shows:
/// the exact line of a copy with one flipped byte and the export file taken
/// away, and nothing else. Exit 1 alone proves nothing: the unfixed code exits
/// 1 anyway.
#[test]
fn a_check_beside_writers_names_the_real_faults_and_only_those() {
    let t = Temp::new("check-damaged");
    let cfg = seeded(t.path());
    let mut line_no = 0;
    let mut gone = PathBuf::new();
    let root = t.path().to_path_buf();
    // The first version line of the copy: writers append only past its end.
    let jsonl = walk_for(&root.join("state"), ".jsonl").or_else(|| walk_for(&root, ".jsonl")).expect("the copy");
    let b = checks_beside_writers(&cfg, 10, |root| {
        let text = std::fs::read_to_string(&jsonl).unwrap();
        let (i, line) = text.lines().enumerate().find(|(_, l)| l.contains("\"op\":\"create\"") && l.contains("race body")).expect("a seeded line");
        line_no = i + 1;
        let at = text.lines().take(i).map(|l| l.len() + 1).sum::<usize>() + line.find("race body").unwrap();
        let mut bytes = text.into_bytes();
        bytes[at] = b'R';
        std::fs::write(&jsonl, bytes).unwrap();
        // RULES.md: the writers write only handoffs and journal entries, so
        // no export heals it.
        gone = root.join("exports").join("RULES.md");
        std::fs::remove_file(&gone).unwrap();
    });
    assert!(b.grown > 0, "the writers had stopped, so this proved nothing");
    assert!(b.during > 0, "no write between the first check and the last, so the checks ran beside nothing");
    let want = vec![format!("copy: line {line_no}: text does not match its sha"), format!("disk: {} is missing", gone.display())];
    for (i, r) in b.runs.iter().enumerate() {
        assert_eq!(r, &want, "run {i}");
    }
}

/// With writers never pausing a commit can always be ahead of its export, so an
/// export file that stays wrong through every look is not damage while it keeps
/// changing. Here another process rewrites it throughout; left still, the same
/// wrong file is reported.
#[test]
fn a_check_does_not_report_an_export_that_is_being_written() {
    let t = Temp::new("check-moving");
    let cfg = seeded(t.path());
    let log = t.path().join("exports").join("LOG.md");
    let check = || {
        let out = Command::new(bin()).arg("--config").arg(&cfg).arg("--memory-check").output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    std::fs::write(&log, "wrong and still\n").unwrap();
    let still = check();
    assert!(still.contains("LOG.md differs from its export"), "{still}");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let moving = std::thread::scope(|s| {
        s.spawn(|| {
            let mut n = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                n += 1;
                let tmp = log.with_extension("tmp");
                std::fs::write(&tmp, format!("wrong {n}\n")).unwrap();
                let _ = std::fs::rename(&tmp, &log);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
        let text = check();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        text
    });
    assert!(!moving.contains("LOG.md"), "{moving}");
}

/// The first file under `dir` whose name ends with `suffix`.
fn walk_for(dir: &Path, suffix: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = walk_for(&p, suffix) {
                return Some(f);
            }
        } else if p.to_string_lossy().ends_with(suffix) {
            return Some(p);
        }
    }
    None
}

/// A server that stays up, one call at a time.
struct Live {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    out: BufReader<std::process::ChildStdout>,
    id: i64,
}

impl Live {
    fn start(cfg: &Path) -> Live {
        let mut child = Command::new(bin()).arg("--config").arg(cfg).current_dir(cfg.parent().unwrap()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let (stdin, out) = (child.stdin.take().unwrap(), BufReader::new(child.stdout.take().unwrap()));
        let mut l = Live { child, stdin, out, id: 0 };
        l.ask(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "live"}}}), 0);
        l
    }
    fn ask(&mut self, msg: Value, id: i64) -> Value {
        writeln!(self.stdin, "{msg}").unwrap();
        let mut line = String::new();
        loop {
            line.clear();
            assert!(self.out.read_line(&mut line).unwrap() > 0, "server closed stdout");
            let m = atlas::json::parse(line.trim()).unwrap();
            if m["id"].as_i64() == Some(id) {
                return m;
            }
        }
    }
    fn call(&mut self, tool: &str, args: Value) -> (String, bool) {
        self.id += 1;
        let id = self.id;
        let m = self.ask(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": args}}), id);
        (m["result"]["content"][0]["text"].as_str().unwrap_or("").to_string(), m["result"]["isError"] == json!(true))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A handoff sent while another session holds the write lock, and that session
/// then commits its own: the handoff reads the lane after the lock, so it is
/// neither refused nor left active beside the other. Deterministic, where two
/// racing threads catch this about 1 time in 200.
fn handoff_behind_another_session(tag: &str, prior: bool) {
    let t = Temp::new(tag);
    let cfg = project(t.path());
    let mut server = Live::start(&cfg);
    if prior {
        let (text, err) = server.call("handoff", json!({"lane": "L", "title": "first", "text": "first"}));
        assert!(!err, "{text}");
    } else {
        // A read: the store is open and any first-use write is done.
        let (text, err) = server.call("recall", json!({"lane": "L"}));
        assert!(!err, "{text}");
    }
    let c = atlas::config::Config::load(&cfg).unwrap();
    let store_path = atlas::memory::settings::Settings::from_config(Some(&c), &t.path().join("state").join("atlas.sqlite")).unwrap().store;
    let mut other = atlas::memory::MemoryStore::open(&store_path).unwrap();
    let before: Vec<String> = other.heads(Some("handoff"), Some("L")).unwrap().into_iter().filter(|h| h.status == "active").map(|h| h.key).collect();
    let mut w = other.writer().unwrap();
    let sent = std::thread::spawn(move || {
        let r = server.call("handoff", json!({"lane": "L", "title": "behind", "text": "behind the lock"}));
        (r, server)
    });
    // It matters only without the fix: the old read has time to happen first.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let key = w.next_key("handoff:L:", 3).unwrap();
    let draft = atlas::memory::Draft { title: "other".into(), meta: String::new(), body: "other session\n".into(), reason: "test".into(), author: "other".into(), client: "other".into(), provenance: None, time: None };
    let place = Some(("log", atlas::memory::Place::End));
    if before.is_empty() {
        w.create(&key, "handoff", "", "L", &draft, place).unwrap();
    } else {
        w.supersede(&before, &key, "handoff", "", "L", &draft, place).unwrap();
    }
    w.add_refs(&key, &[("lane".to_string(), "L".to_string())]).unwrap();
    w.commit().unwrap();
    let ((text, err), server) = sent.join().unwrap();
    assert!(!err, "the handoff was refused: {text}");
    drop(server);
    let heads = other.heads(Some("handoff"), Some("L")).unwrap();
    let active: Vec<&str> = heads.iter().filter(|h| h.status == "active").map(|h| h.title.as_str()).collect();
    assert_eq!(active, ["behind"], "exactly one active handoff");
    assert_eq!(heads.len(), if prior { 3 } else { 2 });
    let links: i64 = other.conn().query_row("SELECT COUNT(*) FROM supersedes", [], |r| r.get(0)).unwrap();
    assert_eq!(links, heads.len() as i64 - 1, "a chain through every handoff");
    drop(other);
    let out = Command::new(bin()).arg("--config").arg(&cfg).arg("--memory-check").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn a_handoff_behind_another_session_is_not_refused() {
    handoff_behind_another_session("handoff-prior", true);
}

#[test]
fn a_handoff_behind_another_on_an_empty_lane_leaves_one_active() {
    handoff_behind_another_session("handoff-empty", false);
}

/// Eight opens of one fresh memory store at once: every one succeeds and the
/// store ends at the current version, its schema made once under the write
/// lock (the others read the version again there and leave it). A store at
/// the current version is never refused as too new by a racing open.
#[test]
fn opens_of_a_fresh_store_at_once_make_the_schema_once() {
    let t = Temp::new("fresh-race");
    let path = t.path().join("memory.sqlite");
    let gate = std::sync::Arc::new(std::sync::Barrier::new(8));
    let opens: Vec<_> = (0..8)
        .map(|_| {
            let (gate, path) = (gate.clone(), path.clone());
            std::thread::spawn(move || {
                gate.wait();
                atlas::memory::MemoryStore::open(&path).map(drop)
            })
        })
        .collect();
    for o in opens {
        o.join().unwrap().expect("every open of a fresh store succeeds");
    }
    let c = rusqlite::Connection::open(&path).unwrap();
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v, atlas::memory::SCHEMA_VERSION);
    assert_eq!(atlas::memory::store_skew(&path), None);
}
