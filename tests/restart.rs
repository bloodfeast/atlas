//! #221, the restart point end to end: from the hooks the resident server
//! sees to the bytes its catch would write (its own test binary, since the
//! catch's slot is process-wide), and the real server's stdin close.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
use atlas::orchestrate::{Kind, KindSpec, OrchestrateSpec};
use atlas::restart::Point;
use common::*;

/// Reads what the test sends, until the sender is dropped (stdin's EOF).
struct ChanReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    at: usize,
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.at == self.buf.len() {
            match self.rx.recv() {
                Ok(b) => {
                    self.buf = b;
                    self.at = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

fn settings(dir: &Path) -> Settings {
    let mut s = Settings::from_config(None, &dir.join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![(Kind::Builder, KindSpec { max: Some(4), ..Default::default() })],
        ..Default::default()
    };
    s.orchestrate = atlas::orchestrate::resolve(Some(&o), None, None, |_| None);
    s
}

/// What the server answered, read as it comes.
struct Answers {
    rx: mpsc::Receiver<Vec<u8>>,
    seen: String,
}

impl Answers {
    /// The answer line to request `id`, waiting up to 30 s for it.
    fn to(&mut self, id: &str) -> String {
        let want = format!("\"{id}\"");
        loop {
            let whole = &self.seen[..self.seen.rfind('\n').map_or(0, |i| i + 1)];
            if let Some(l) = whole.lines().find(|l| l.contains(&want)) {
                return l.to_string();
            }
            let b = self.rx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("no answer to {id}"));
            self.seen.push_str(&String::from_utf8_lossy(&b));
        }
    }
}

/// `serve_with_hooks_every` on its own thread, over a one-note vault, fed
/// by the returned sender (dropping it closes its stdin), with its answers.
fn serve_answers(dir: &Path, set: Settings, every: Duration) -> (mpsc::Sender<Vec<u8>>, std::thread::JoinHandle<()>, Answers) {
    let vault = dir.join("vault");
    write(&vault.join("A.md"), "# A\n\nalpha\n");
    let db = dir.join("state").join("vaults.sqlite");
    let mut server = server_with(&[("v".into(), vault)], &db);
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let input = BufReader::new(ChanReader { rx, buf: Vec::new(), at: 0 });
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let h = std::thread::spawn(move || {
        atlas::protocol::serve_with_hooks_every(&mut server, Some(set), None, input, ChanWriter(out_tx), every).unwrap();
    });
    // The hook state opens at the first hook and counts only agents that
    // write after it opened: the agents' files come after this answer.
    hook(
        &tx,
        "warm",
        json!({"event": "PreToolUse", "session_id": "s1", "tool_name": "TodoWrite", "tool_input": {"todos": []}, "tool_use_id": "warm"}),
    );
    let mut answers = Answers { rx: out_rx, seen: String::new() };
    let _ = answers.to("warm");
    // The file times' resolution, as the orchestrate tests allow.
    std::thread::sleep(Duration::from_millis(50));
    (tx, h, answers)
}

/// Sends what the server writes to the test.
struct ChanWriter(mpsc::Sender<Vec<u8>>);

impl Write for ChanWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.send(b.to_vec());
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn hook(tx: &mpsc::Sender<Vec<u8>>, id: &str, args: Value) {
    let m = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hook", "arguments": args}});
    tx.send(format!("{m}\n").into_bytes()).unwrap();
}

/// The transcript of session `s1` and its agents' folder.
fn session(root: &Path) -> (String, PathBuf) {
    let parent = root.join("proj").join("sess.jsonl");
    let dir = root.join("proj").join("sess").join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&parent, "").unwrap();
    (parent.to_string_lossy().into_owned(), dir)
}

/// Launches agent `id` (launch `tu`) through the hooks and, once the launch
/// is answered (and let through), starts it, its
/// meta file and transcript written as Claude Code writes them.
fn launch_and_start(tx: &mpsc::Sender<Vec<u8>>, answers: &mut Answers, tp: &str, dir: &Path, id: &str, tu: &str) {
    launch_only(tx, tp, id, tu);
    let a = answers.to(&format!("l-{id}"));
    assert!(!denied(&a), "the launch of {id} was denied: {a}");
    agent_files(dir, id, tu);
    start_only(tx, tp, id);
}

/// Agent `id`'s brief.
fn brief_of(id: &str) -> String {
    // b4 waits beside b3 on another resource, so neither is queued.
    let uses = if id == "b4" { "docs" } else { "compile" };
    format!("Kind: builder\nLane: WALRUS\nUses: {uses}\n\nBuild {id}.")
}

/// The launch (request `l-<id>`) of agent `id` as tool use `tu`.
fn launch_only(tx: &mpsc::Sender<Vec<u8>>, tp: &str, id: &str, tu: &str) {
    let brief = brief_of(id);
    let input = json!({"description": format!("job {id}"), "subagent_type": "general-purpose", "prompt": brief.as_str()});
    hook(
        tx,
        &format!("l-{id}"),
        json!({"event": "PreToolUse", "session_id": "s1", "tool_name": "Agent", "tool_use_id": tu, "transcript_path": tp, "tool_input": input}),
    );
}

/// Agent `id`'s meta file and transcript, as Claude Code writes them.
fn agent_files(dir: &Path, id: &str, tu: &str) {
    let brief = brief_of(id);
    let meta =
        json!({"agentType": "general-purpose", "isBuiltIn": true, "description": format!("job {id}"), "toolUseId": tu, "model": "opus"});
    std::fs::write(dir.join(format!("agent-{id}.meta.json")), meta.to_string()).unwrap();
    let user = json!({"type": "user", "message": {"role": "user", "content": brief.as_str()}});
    let usage = json!({"type": "assistant", "message": {"id": "m1", "usage": {"input_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 998, "output_tokens": 0}}});
    std::fs::write(dir.join(format!("agent-{id}.jsonl")), format!("{user}\n{usage}\n")).unwrap();
}

/// Agent `id`'s SubagentStart (request `s-<id>`).
fn start_only(tx: &mpsc::Sender<Vec<u8>>, tp: &str, id: &str) {
    hook(
        tx,
        &format!("s-{id}"),
        json!({"event": "SubagentStart", "session_id": "s1", "agent_id": id, "agent_type": "general-purpose", "transcript_path": tp}),
    );
}

/// Whether a hook answer denies.
fn denied(answer: &str) -> bool {
    answer.contains("permissionDecision") && answer.contains("deny")
}

fn prepared() -> Option<Point> {
    let mut got = None;
    atlas::restart::wait_prepared(Duration::ZERO, |b| {
        got = b.and_then(|b| Point::parse(&String::from_utf8_lossy(b)));
        true
    });
    got
}

fn names(b: Option<&[u8]>, id: &str) -> bool {
    b.and_then(|b| Point::parse(&String::from_utf8_lossy(b))).is_some_and(|p| p.agents.iter().any(|a| a.id == id))
}

/// S2 and S6: the resident server's hooks keep the catch's bytes current.
/// A SubagentStart prepares a point naming the agent, with the runq holders
/// under this process's parent; its SubagentStop clears it. Under steady
/// hook traffic (a hook every 50 ms) the periodic refresh still fires.
#[test]
fn the_server_keeps_the_restart_point_current_from_its_hooks() {
    let t = Temp::new("restart-e2e");
    let set = settings(t.path());
    atlas::restart::arm_slot(&set.store);
    // runq's state: one holder under this process's parent, one elsewhere.
    let runq = t.path().join("runq");
    let parent = atlas::restart::server_parent().unwrap_or(0);
    write(
        &runq.join("state"),
        &format!(
            "runq-state 1\nmeta\tnext_id=3\nentry\tid=1\towner=atlas-mine\tres=light-compile\theld=1\tchain=4294967290:1:bash,{parent}:2:claude\nentry\tid=2\towner=atlas-theirs\tres=linux-test\theld=1\tchain=4294967291:1:bash,4294967294:2:claude\n"
        ),
    );
    atlas::restart::set_runq_root(runq);
    let (tp, dir) = session(t.path());

    // Phase 1: no periodic refresh in reach; only the hooks prepare.
    let (tx, server, mut answers) = serve_answers(t.path(), set.clone(), Duration::from_secs(3600));
    launch_and_start(&tx, &mut answers, &tp, &dir, "b1", "t1");
    assert!(
        atlas::restart::wait_prepared(Duration::from_secs(20), |b| names(b, "b1")),
        "SubagentStart prepared no point naming b1: {:?}",
        prepared()
    );
    let p = prepared().unwrap();
    assert_eq!(p.holders, vec!["atlas-mine light-compile".to_string()], "the runq holders: {p:?}");
    let b1 = p.agents.iter().find(|a| a.id == "b1").unwrap();
    assert!(b1.launch.starts_with("journal:WALRUS:"), "the point names no launch key: {p:?}");
    assert_eq!(b1.uses, vec!["compile".to_string()], "{p:?}");
    hook(
        &tx,
        "stop-b1",
        json!({"event": "SubagentStop", "session_id": "s1", "agent_id": "b1", "agent_type": "general-purpose", "last_assistant_message": "done", "transcript_path": tp.as_str()}),
    );
    assert!(atlas::restart::wait_prepared(Duration::from_secs(20), |b| b.is_none()), "SubagentStop left the point: {:?}", prepared());

    // R2: two launches wait and agent b3 starts before its meta file is
    // written, so its start cannot tell its launch. Its first tool call
    // settles it, and the point follows with no other hook.
    launch_only(&tx, &tp, "b3", "t3");
    launch_only(&tx, &tp, "b4", "t4");
    for l in ["l-b3", "l-b4"] {
        let a = answers.to(l);
        assert!(!denied(&a), "{l} was denied: {a}");
    }
    let user = json!({"type": "user", "message": {"role": "user", "content": brief_of("b3").as_str()}});
    std::fs::write(dir.join("agent-b3.jsonl"), format!("{user}\n")).unwrap();
    start_only(&tx, &tp, "b3");
    let _ = answers.to("s-b3");
    let at_start = prepared();
    agent_files(&dir, "b3", "t3");
    hook(
        &tx,
        "call-b3",
        json!({"event": "PreToolUse", "session_id": "s1", "agent_id": "b3", "tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_use_id": "u-b3", "transcript_path": tp.as_str()}),
    );
    let _ = answers.to("call-b3");
    let settled = |b: Option<&[u8]>| {
        b.and_then(|b| Point::parse(&String::from_utf8_lossy(b)))
            .is_some_and(|p| p.agents.iter().any(|a| a.id == "b3" && a.launch.starts_with("journal:WALRUS:") && a.uses == ["compile"]))
    };
    assert!(
        atlas::restart::wait_prepared(Duration::from_secs(20), settled),
        "the first tool call settled b3's launch, but the point does not name it: at the start {at_start:?}, now {:?}",
        prepared()
    );
    hook(
        &tx,
        "stop-b3",
        json!({"event": "SubagentStop", "session_id": "s1", "agent_id": "b3", "agent_type": "general-purpose", "last_assistant_message": "done", "transcript_path": tp.as_str()}),
    );
    let _ = answers.to("stop-b3");
    drop(tx);
    server.join().unwrap();

    // Phase 2: a 300 ms refresh under a hook every 50 ms.
    let (tx, server, mut answers) = serve_answers(t.path(), set, Duration::from_millis(300));
    launch_and_start(&tx, &mut answers, &tp, &dir, "b2", "t2");
    let stop = Arc::new(AtomicBool::new(false));
    let traffic = {
        let (tx, stop) = (tx.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::SeqCst) {
                i += 1;
                let args = json!({"event": "PreToolUse", "session_id": "s1", "tool_name": "TodoWrite", "tool_input": {"todos": []}, "tool_use_id": format!("todo{i}")});
                hook(&tx, &format!("todo{i}"), args);
                // Pacing the traffic, not waiting on anything.
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    assert!(atlas::restart::wait_prepared(Duration::from_secs(20), |b| names(b, "b2")), "SubagentStart prepared no point naming b2");
    let first = prepared().unwrap().at;
    let refreshed = atlas::restart::wait_prepared(Duration::from_secs(10), |b| {
        b.and_then(|b| Point::parse(&String::from_utf8_lossy(b))).is_some_and(|p| p.at != first && p.agents.iter().any(|a| a.id == "b2"))
    });
    stop.store(true, Ordering::SeqCst);
    traffic.join().unwrap();
    drop(tx);
    server.join().unwrap();
    assert!(refreshed, "no periodic refresh in 10 s under steady hook traffic (every 300 ms asked); the point is still from {first}");
}

/// Every file under `root` whose name starts with `prefix`.
fn files_named(root: &Path, prefix: &str, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            files_named(&p, prefix, out);
        } else if e.file_name().to_string_lossy().starts_with(prefix) {
            out.push(p);
        }
    }
}

/// The real server, armed and with a restart point prepared (seeded by
/// `ATLAS_TEST_RESTART_SEED`), answering an initialize.
fn real_server(t: &Temp) -> (std::process::Child, std::process::ChildStdin) {
    write(&t.path().join("vault").join("Index.md"), "# Index\n");
    let cfg = t.path().join("atlas.json");
    write(
        &cfg,
        r#"{"db": "state/atlas.sqlite", "vaults": [{"name": "v", "path": "vault"}], "watch": false, "models": {"auto_download": false, "files": []}, "update": {"enabled": false}}"#,
    );
    let seed = Point {
        at: "2026-11-16T20:00:00.000Z".into(),
        agents: vec![atlas::restart::AgentPoint { id: "a1".into(), launch: "journal:WALRUS:0001".into(), ..Default::default() }],
        lanes: vec!["WALRUS".into()],
        holders: Vec::new(),
        server: String::new(),
        ..Point::default()
    };
    let mut child = Command::new(bin())
        .arg("--config")
        .arg(&cfg)
        .env("ATLAS_HOME", t.path().join("home-atlas"))
        .env("ATLAS_TEST_RESTART_SEED", seed.to_json())
        .current_dir(t.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "t"}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    // The server arms the catch before it serves, so an answer means armed.
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    assert!(line.contains("\"id\":1") || line.contains("\"id\": 1"), "no initialize answer: {line}");
    (child, stdin)
}

/// S3 and Q10: the real `atlas serve`, with agents running (a point
/// prepared), whose stdin closes (a plain end of session) exits 0 and
/// writes no restart point; its shutdown log notes the close. On Unix the
/// same server sent SIGTERM writes the seeded point, so the seed is live.
#[test]
fn a_closed_stdin_ends_the_real_server_without_a_restart_point() {
    let t = Temp::new("restart-eof");
    let (mut child, stdin) = real_server(&t);
    drop(stdin);
    let st = child.wait().unwrap();
    assert!(st.success(), "{st:?}");
    let mut points = Vec::new();
    files_named(t.path(), "restart-point", &mut points);
    assert!(points.is_empty(), "a closed stdin wrote a restart point: {points:?}");
    let mut logs = Vec::new();
    files_named(t.path(), atlas::restart::LOG_FILE, &mut logs);
    assert_eq!(logs.len(), 1, "{logs:?}");
    let logged = std::fs::read_to_string(&logs[0]).unwrap();
    assert!(logged.contains("catch installed") && logged.contains("stdin closed"), "{logged}");
    // R5: the seed is live in this test build (the `test-seams` feature the
    // tests turn on), so agents were running when stdin closed.
    assert!(logged.contains("test seed prepared"), "the test seed was not prepared: {logged}");
    assert!(!logged.contains("restart point written"), "{logged}");

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let t = Temp::new("restart-term");
        let (mut child, _stdin) = real_server(&t);
        let sent = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
        assert!(sent.success());
        let st = child.wait().unwrap();
        assert_eq!(st.signal(), Some(15), "{st:?}");
        let mut points = Vec::new();
        files_named(t.path(), "restart-point", &mut points);
        assert_eq!(points.len(), 1, "{points:?}");
        let p = Point::parse(&std::fs::read_to_string(&points[0]).unwrap()).unwrap();
        assert_eq!(p.agents[0].id, "a1");
    }
}
