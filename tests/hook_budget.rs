//! The hooks' time budget end to end: a resident server answers every hook
//! within `hooks.budget_ms` while another connection holds the memory
//! store's write lock (journal:ATLAS-RELEASE:0221, "nothing should take 30s").

mod common;

use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
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

/// The answer lines, read as they come.
struct Answers {
    rx: mpsc::Receiver<Vec<u8>>,
    seen: String,
}

impl Answers {
    /// The answer line to request `id`, waiting up to `wait` for it.
    fn to(&mut self, id: &str, wait: Duration) -> Option<String> {
        let want = format!("\"id\":\"{id}\"");
        let end = Instant::now() + wait;
        loop {
            let whole = &self.seen[..self.seen.rfind('\n').map_or(0, |i| i + 1)];
            if let Some(l) = whole.lines().find(|l| l.contains(&want)) {
                return Some(l.to_string());
            }
            let b = self.rx.recv_timeout(end.saturating_duration_since(Instant::now())).ok()?;
            self.seen.push_str(&String::from_utf8_lossy(&b));
        }
    }
}

fn hook(tx: &mpsc::Sender<Vec<u8>>, id: &str, args: Value) {
    let m = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hook", "arguments": args}});
    tx.send(format!("{m}\n").into_bytes()).unwrap();
}

fn settings(dir: &Path, budget: Duration) -> Settings {
    let mut s = Settings::from_config(None, &dir.join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.hook_budget = budget;
    s
}

#[test]
fn a_locked_store_delays_no_hook_past_its_budget() {
    let t = Temp::new("hook-budget-locked");
    let budget = Duration::from_millis(1_000);
    let s = settings(t.path(), budget);
    let vault = t.path().join("vault");
    write(&vault.join("A.md"), "# A\n\nalpha\n");
    let mut server = server_with(&[("v".into(), vault)], &t.path().join("state").join("vaults.sqlite"));
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let set = s.clone();
    let h = std::thread::spawn(move || {
        let input = BufReader::new(ChanReader { rx, buf: Vec::new(), at: 0 });
        atlas::protocol::serve_with_hooks(&mut server, Some(set), None, input, ChanWriter(out_tx)).unwrap();
    });
    let mut answers = Answers { rx: out_rx, seen: String::new() };
    // The hook state is open (and the store made) before the lock is taken.
    hook(
        &tx,
        "warm",
        json!({"event": "PreToolUse", "session_id": "s1", "tool_name": "TodoWrite", "tool_input": {"todos": []}, "tool_use_id": "warm"}),
    );
    answers.to("warm", Duration::from_secs(60)).expect("the warm-up hook is answered");
    let transcript = t.path().join("sess.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let tp = transcript.to_string_lossy().into_owned();

    // An agent's launch: its brief is held for its start.
    let brief = "Kind: builder\nLane: WIDGETS\nBuild the widget cache.";
    hook(
        &tx,
        "launch",
        json!({"event": "PreToolUse", "session_id": "s2", "tool_name": "Agent", "tool_use_id": "toolu_1", "tool_prompt": brief, "tool_input": {"prompt": brief, "subagent_type": "builder", "description": "widgets"}}),
    );
    answers.to("launch", Duration::from_secs(60)).expect("the launch hook is answered");

    // Another process's writer holds the memory store.
    let holder = atlas::db::Connection::open(&s.store).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    // The agent's start files its launch in the store: it waits on the lock.
    let t0 = Instant::now();
    hook(&tx, "start", json!({"event": "SubagentStart", "session_id": "s2", "agent_id": "a1", "agent_type": "builder"}));
    // The prompt behind it waits on the worker.
    hook(
        &tx,
        "prompt",
        json!({"event": "UserPromptSubmit", "session_id": "s2", "prompt": "what is the widget cache", "transcript_path": tp}),
    );
    let slack = Duration::from_millis(1_500);
    let start = answers.to("start", budget + slack);
    let start_ms = t0.elapsed();
    let prompt = answers.to("prompt", budget + slack);
    let prompt_ms = t0.elapsed();
    holder.execute_batch("ROLLBACK").unwrap();
    drop(tx);
    h.join().unwrap();

    let start = start.unwrap_or_else(|| panic!("SubagentStart not answered within its budget ({start_ms:?})"));
    let prompt = prompt.unwrap_or_else(|| panic!("UserPromptSubmit not answered within its budget ({prompt_ms:?})"));
    assert!(prompt_ms < budget + slack, "{prompt_ms:?}");
    assert!(start.contains("\"isError\":false"), "{start}");
    // What the prompt skipped is said, in one line.
    assert!(prompt.contains("hook time budget (1000 ms) spent"), "{prompt}");
}

#[test]
fn a_session_start_does_not_wait_on_the_store_lock_and_is_written_later() {
    let t = Temp::new("hook-budget-start");
    let s = settings(t.path(), Duration::from_millis(10_000));
    let mut h = atlas::hooks::Hooks::open(s.clone()).unwrap();
    let transcript = t.path().join("sess.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let ev = atlas::hooks::HookEvent {
        event: "SessionStart".into(),
        session_id: "s3".into(),
        source: "startup".into(),
        transcript_path: transcript.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let holder = atlas::db::Connection::open(&s.store).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let t0 = Instant::now();
    let a = h.handle(&ev);
    let took = t0.elapsed();
    // Not the budget's 10 s, and nothing skipped: the write waits for later.
    assert!(took < Duration::from_millis(1_000), "{took:?}");
    assert!(!a.context.contains("hook time budget"), "{}", a.context);
    assert_eq!(h.starts_owed(), 1);
    h.after();
    assert_eq!(h.starts_owed(), 1, "still locked");
    holder.execute_batch("ROLLBACK").unwrap();
    h.after();
    assert_eq!(h.starts_owed(), 0);
    let store = atlas::memory::MemoryStore::open(&s.store).unwrap();
    assert!(atlas::orchestrate::session_start(&store, "s3").unwrap().is_some());
}

use atlas::hooks::{HookEvent, Hooks};
use atlas::reviews::{HeadResult, Merge, Pr};

fn merge_ev(cmd: &str) -> HookEvent {
    HookEvent {
        event: "PreToolUse".into(),
        session_id: "s1".into(),
        tool_name: "Bash".into(),
        tool_input: json!({"command": cmd}),
        ..HookEvent::default()
    }
}

/// A scripted `gh pr view` of PR 99 at head abcdef0 that answers after `d`,
/// waited on as the gate waits on gh.
fn gh_after(d: Duration) -> HeadResult {
    atlas::reviews::gh_wait(
        "gh pr view",
        move || {
            std::thread::sleep(d);
            Ok((Pr { repo: None, number: 99 }, "abcdef0123".into()))
        },
        || {},
    )
}

fn slow_gh(_m: &Merge, _cwd: &str) -> HeadResult {
    gh_after(Duration::from_millis(3_000))
}

fn quick_gh(_m: &Merge, _cwd: &str) -> HeadResult {
    gh_after(Duration::from_millis(50))
}

/// Review B1 of #329: the merge gate's gh waits only the budget left, so a
/// gh slower than the budget still gives the gate's own answer in time (a
/// refusal: the head cannot be checked), and a gh in time its decision.
#[test]
fn a_slow_gh_still_denies_the_merge_within_the_budget() {
    let t = Temp::new("hook-budget-gh");
    let budget = Duration::from_millis(1_000);
    let mut s = settings(t.path(), budget);
    s.orchestrate.reviews = atlas::reviews::ReviewSettings { roles: vec!["cost".into()], mode: atlas::orchestrate::Mode::Enforced };
    {
        let mut m = atlas::memory::MemoryStore::open(&s.store).unwrap();
        let mut w = m.writer().unwrap();
        let key = w.next_key("journal:L:", 4).unwrap();
        let meta = "role: cost\npr: 99\nhead: abcdef0\nverdict: pass\n";
        let d = atlas::memory::Draft {
            title: "a review".into(),
            meta: meta.into(),
            reason: "gate test".into(),
            author: "t".into(),
            client: "t".into(),
            ..Default::default()
        };
        w.create(&key, "journal", "review", "L", &d, None).unwrap();
        w.commit().unwrap();
    }
    let mut h = Hooks::open(s).unwrap();
    h.set_pr_head(slow_gh);
    let t0 = Instant::now();
    let a = h.handle(&merge_ev("gh pr merge 99"));
    let took = t0.elapsed();
    let deny = a.deny.unwrap_or_else(|| panic!("a merge whose head gh did not give in time passed ({took:?})"));
    assert!(deny.contains("cannot be checked") && deny.contains("no answer in"), "{deny}");
    assert!(took < budget + Duration::from_millis(500), "{took:?}");
    // In time: the gate's decision (its one review is of this head).
    h.set_pr_head(quick_gh);
    let a = h.handle(&merge_ev("gh pr merge 99"));
    assert!(a.deny.is_none(), "{:?}", a.deny);
}

/// Review B1 of #329, narrowed by #416: the answer given for a hook cut at
/// its budget refuses a launch, a resume and a merge, naming the slow
/// phase; a subagent's other calls go ahead with a line saying the check
/// was skipped; the rest pass through.
#[test]
fn an_overrun_answer_refuses_launches_and_merges_only() {
    let limit = Duration::from_millis(3_000);
    let why = "phase cap 2900 ms";
    let deny = |ev: &HookEvent| atlas::hooks::overrun_answer(ev, limit, why).contains(atlas::hooks::GATE_OVERRUN);
    let pre = |tool: &str, agent: &str, input: Value| HookEvent {
        event: "PreToolUse".into(),
        tool_name: tool.into(),
        agent_id: agent.into(),
        tool_input: input,
        ..HookEvent::default()
    };
    let gated = [
        merge_ev("gh pr merge 99 --squash"),
        merge_ev("cd x && gh pr merge"),
        pre("PowerShell", "", json!({"command": "gh pr merge 7"})),
        pre("Agent", "", json!({"prompt": "Kind: builder"})),
        pre("Task", "", json!({})),
        pre("SendMessage", "", json!({"to": "a1"})),
        // A subagent's launch and merge stay refused.
        pre("Agent", "a1", json!({"prompt": "Kind: builder"})),
        pre("Bash", "a1", json!({"command": "gh pr merge 3"})),
    ];
    for ev in &gated {
        let a = atlas::hooks::overrun_answer(ev, limit, why);
        assert!(deny(ev), "{ev:?}");
        assert!(a.contains("\"permissionDecision\":\"deny\""), "{ev:?}");
        // The reason names the slow phase and its time (#416).
        assert!(a.contains("slow: phase cap 2900 ms") && a.contains("retry the call"), "{a}");
    }
    // A subagent's other gated calls fail open (#416): the context cap and
    // the hand-back's backstop are skipped once, with a line saying so.
    let skipped = [
        pre("Read", "a1", json!({"file_path": "/a.rs"})),
        pre("Bash", "a1", json!({"command": "echo ok"})),
        pre("mcp__atlas__recall", "a1", json!({"op": "status"})),
        pre("SubagentHandback", "a1", json!({"message": "done"})),
    ];
    for ev in &skipped {
        let a = atlas::hooks::overrun_answer(ev, limit, why);
        assert!(!a.contains("\"permissionDecision\""), "{ev:?}: {a}");
        let v = json::parse(&a).unwrap_or_else(|e| panic!("{e}: {a}"));
        let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("");
        assert!(ctx.contains(atlas::hooks::GATE_SKIPPED) && ctx.contains("slow: phase cap 2900 ms") && ctx.contains("checked again"), "{a}");
    }
    let open = [
        merge_ev("gh pr view 99"),
        pre("Read", "", json!({})),
        // The handoff is never refused (the cap lets it through).
        pre("mcp__atlas__handoff", "a1", json!({})),
        HookEvent { event: "PostToolUse".into(), tool_name: "Agent".into(), ..HookEvent::default() },
        HookEvent { event: "UserPromptSubmit".into(), ..HookEvent::default() },
    ];
    for ev in &open {
        assert!(!deny(ev), "{ev:?}");
    }
}

/// Review N3 of #329: a budget of 0 would skip every hook's work; refused.
#[test]
fn a_zero_budget_is_refused() {
    let t = Temp::new("hook-budget-zero");
    let e = atlas::config::Config::parse(r#"{"hooks": {"budget_ms": 0}}"#, t.path()).expect_err("0 is refused");
    assert!(e.to_string().contains("hooks.budget_ms must be at least 1"), "{e}");
    let c = atlas::config::Config::parse(r#"{"hooks": {"budget_ms": 1}}"#, t.path()).unwrap();
    assert_eq!(c.hooks.and_then(|h| h.budget_ms), Some(1));
}

/// Review S1 of #329: a SessionStart whose budget was spent waiting in the
/// queue still records its start.
#[test]
fn a_session_start_spent_in_the_queue_is_still_recorded() {
    let t = Temp::new("hook-budget-queued-start");
    let s = settings(t.path(), Duration::from_millis(200));
    let mut h = Hooks::open(s.clone()).unwrap();
    let transcript = t.path().join("sess.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let ev = HookEvent {
        event: "SessionStart".into(),
        session_id: "s5".into(),
        source: "startup".into(),
        transcript_path: transcript.to_string_lossy().into_owned(),
        ..Default::default()
    };
    // Past even SessionStart's longer budget (#333 item 2).
    h.handle_from(&ev, Instant::now() - Duration::from_millis(atlas::hook_budget::LONG_MS + 1_000));
    assert_eq!(h.starts_owed(), 0);
    let store = atlas::memory::MemoryStore::open(&s.store).unwrap();
    assert!(atlas::orchestrate::session_start(&store, "s5").unwrap().is_some(), "the start was lost");
    // The session is noted for the agent cap from it.
    assert!(h.session_opened(&ev.transcript_path).is_some());
}

/// #333 item 2: SessionStart has the longer budget
/// ([`atlas::hook_budget::LONG_MS`]); a prompt that waited as long is cut.
#[test]
fn a_session_start_has_a_longer_budget() {
    let t = Temp::new("hook-budget-per-event");
    let s = settings(t.path(), Duration::from_millis(200));
    let mut h = Hooks::open(s).unwrap();
    let transcript = t.path().join("sess.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let ev = |event: &str| HookEvent {
        event: event.into(),
        session_id: "s9".into(),
        source: "startup".into(),
        trigger: "manual".into(),
        prompt: "what is the widget cache".into(),
        transcript_path: transcript.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let waited = Instant::now() - Duration::from_secs(2);
    // PreCompact enters no phase; its longer budget is the watchdog's
    // (protocol's `the_watchdog_waits_the_per_event_budget`).
    let a = h.handle_from(&ev("SessionStart"), waited);
    assert!(!a.context.contains("hook time budget"), "{}", a.context);
    let a = h.handle_from(&ev("UserPromptSubmit"), waited);
    assert!(a.context.contains("hook time budget (200 ms) spent"), "{}", a.context);
}

/// Review S2 of #329: once a deferred start is written, the agent cap
/// counts the session's agents from it, not from the start before.
#[test]
fn a_deferred_session_start_is_read_again_once_written() {
    let t = Temp::new("hook-budget-reread");
    let s = settings(t.path(), Duration::from_millis(10_000));
    let old = std::time::SystemTime::now() - Duration::from_secs(3_600);
    atlas::orchestrate::record_session_start(&atlas::memory::MemoryStore::open(&s.store).unwrap(), "s6", old).unwrap();
    let mut h = Hooks::open(s.clone()).unwrap();
    let transcript = t.path().join("sess.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let tp = transcript.to_string_lossy().into_owned();
    let ev = |event: &str| HookEvent {
        event: event.into(),
        session_id: "s6".into(),
        source: "resume".into(),
        transcript_path: tp.clone(),
        ..Default::default()
    };
    let holder = atlas::db::Connection::open(&s.store).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    h.handle(&ev("SessionStart"));
    assert_eq!(h.starts_owed(), 1);
    // Read before the write: the start before the resume.
    let before = h.session_opened(&tp).expect("noted");
    assert!(before <= old + Duration::from_secs(1), "{before:?}");
    holder.execute_batch("ROLLBACK").unwrap();
    h.after();
    assert_eq!(h.starts_owed(), 0);
    h.handle(&ev("Notification"));
    let after = h.session_opened(&tp).expect("noted");
    assert!(after > old + Duration::from_secs(60), "the agents still count from the start before the resume: {after:?}");
}

/// Review S5 of #329: `atlas --hook` past its budget answers by itself and
/// exits 0: a gate with its refusal, a prompt with the overrun line.
#[test]
fn a_command_hook_past_its_budget_answers_and_exits() {
    use std::process::{Command, Stdio};
    let t = Temp::new("hook-budget-cmd");
    write(&t.path().join("vault").join("Index.md"), "# Index\n");
    let cfg = t.path().join("atlas.json");
    write(
        &cfg,
        r#"{"db": "state/atlas.sqlite", "vaults": [{"name": "notes", "path": "vault"}], "models": {"auto_download": false, "files": []}, "hooks": {"budget_ms": 300}}"#,
    );
    let run = |ev: Value| {
        let mut child = Command::new(bin())
            .arg("--config")
            .arg(&cfg)
            .arg("--hook")
            .env("ATLAS_TEST_HOOK_STALL_MS", "5000")
            .env_remove("ATLAS_HOOK_BUDGET_OFF")
            .current_dir(t.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(ev.to_string().as_bytes()).unwrap();
        let t0 = Instant::now();
        let out = child.wait_with_output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), t0.elapsed())
    };
    let cwd = t.path().to_string_lossy().into_owned();
    let (ok, stdout, took) = run(
        json!({"hook_event_name": "PreToolUse", "session_id": "s-cmd", "cwd": cwd, "tool_name": "Bash", "tool_input": {"command": "gh pr merge 5"}}),
    );
    assert!(ok, "{stdout}");
    assert!(took < Duration::from_millis(4_000), "{took:?}");
    let v = json::parse(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout:?}"));
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"].as_str(), Some("deny"), "{stdout}");
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap_or("");
    assert!(reason.starts_with(atlas::hooks::GATE_OVERRUN) && reason.contains("slow: phase "), "{stdout}");
    // A subagent's plain shell call past the budget goes ahead (#416).
    let (ok, stdout, _) = run(
        json!({"hook_event_name": "PreToolUse", "session_id": "s-cmd", "agent_id": "a1", "cwd": cwd, "tool_name": "Bash", "tool_input": {"command": "echo ok"}}),
    );
    assert!(ok && !stdout.contains("permissionDecision") && stdout.contains(atlas::hooks::GATE_SKIPPED), "{stdout}");
    let (ok, stdout, _) = run(json!({"hook_event_name": "UserPromptSubmit", "session_id": "s-cmd", "cwd": cwd, "prompt": "a prompt"}));
    assert!(ok && stdout.contains("hook time budget (300 ms) spent; skipped: the whole answer"), "{stdout}");
}

/// #333 item 1: an edit's re-index that another writer's lock refused after
/// the answer is kept owed and done after a later answer, not dropped.
#[test]
fn a_reindex_refused_by_the_lock_stays_owed() {
    let t = Temp::new("hook-budget-reindex");
    let s = settings(t.path(), Duration::from_millis(3_000));
    let root = t.path().join("app");
    write(&root.join("src/a.rs"), "pub struct A;\n");
    let notes = t.path().join("notes");
    write(&notes.join("n.md"), "# N\n");
    drop(atlas::store::Store::open(&s.index_db, atlas::paths::Vaults::new(&[("notes".to_string(), notes.clone())]).unwrap()).unwrap());
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec {
            name: "app".into(),
            root: root.clone(),
            build_scripts: false,
            scip: Default::default(),
            cpp: Default::default(),
            exclude: Vec::new(),
        }],
        ..Default::default()
    };
    let code = atlas::code::CodeSetup::from_spec(&spec, &s.index_db, false);
    let mut conn = atlas::db::Connection::open(&s.index_db).unwrap();
    atlas::code_index::index_root(&code, &mut conn, &code.roots[0], &atlas::code_index::RustSource::ScanOnly("test".into())).unwrap();
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = root.join("src/a.rs");
    std::fs::write(&a, "pub struct A;\n\npub fn two() -> u32 {\n    2\n}\n").unwrap();
    let path = a.to_string_lossy().into_owned();
    let edit = HookEvent {
        event: "PostToolUse".into(),
        session_id: "s1".into(),
        tool_name: "Edit".into(),
        tool_input: json!({"file_path": path, "old_string": "x", "new_string": "y"}),
        ..HookEvent::default()
    };
    h.handle(&edit);
    // The server's work after an answer waits on a lock for AFTER_WAIT.
    let holder = atlas::db::Connection::open(&s.index_db).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    {
        let _wait = atlas::hook_budget::Active::until(Instant::now() + atlas::hook_budget::AFTER_WAIT);
        h.after();
    }
    assert_eq!(h.reindex_owed(), 1, "the refused re-index was dropped");
    holder.execute_batch("ROLLBACK").unwrap();
    h.after();
    assert_eq!(h.reindex_owed(), 0);
    let v = atlas::code::symbol(&conn, "two", atlas::code::Limits { defs: 5, refs: 5, impls: 5 }).unwrap();
    assert_eq!(v.totals[0], 1, "the edit was re-indexed once the lock was gone");
}

/// #333 item 1: opening the index store (a Markdown rescan after a hook's
/// answer) gives up at the active deadline, not after its own 30 s.
#[test]
fn opening_the_index_store_stops_at_the_active_deadline() {
    let t = Temp::new("hook-budget-open");
    let db = t.path().join("state").join("index.sqlite");
    let notes = t.path().join("notes");
    write(&notes.join("n.md"), "# N\n");
    let vaults = || atlas::paths::Vaults::new(&[("notes".to_string(), notes.clone())]).unwrap();
    drop(atlas::store::Store::open(&db, vaults()).unwrap());
    let holder = atlas::db::Connection::open(&db).unwrap();
    holder.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE").unwrap();
    let t0 = Instant::now();
    let r = {
        let _wait = atlas::hook_budget::Active::until(Instant::now() + atlas::hook_budget::AFTER_WAIT);
        atlas::store::Store::open(&db, vaults()).map(drop)
    };
    let took = t0.elapsed();
    holder.execute_batch("ROLLBACK").unwrap();
    assert!(took < Duration::from_secs(2), "{took:?} ({r:?})");
}

/// #333 item 2: `atlas --hook` for a SessionStart waits its longer budget,
/// so a slow first open (a 1 s stall, past the 300 ms of every other hook)
/// still answers with the work, not the overrun line.
#[test]
fn a_command_session_start_waits_its_longer_budget() {
    use std::process::{Command, Stdio};
    let t = Temp::new("hook-budget-cmd-start");
    write(&t.path().join("vault").join("Index.md"), "# Index\n");
    let cfg = t.path().join("atlas.json");
    write(
        &cfg,
        r#"{"db": "state/atlas.sqlite", "vaults": [{"name": "notes", "path": "vault"}], "models": {"auto_download": false, "files": []}, "hooks": {"budget_ms": 300}}"#,
    );
    let mut child = Command::new(bin())
        .arg("--config")
        .arg(&cfg)
        .arg("--hook")
        .env("ATLAS_TEST_HOOK_STALL_MS", "1000")
        .env_remove("ATLAS_HOOK_BUDGET_OFF")
        .current_dir(t.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let cwd = t.path().to_string_lossy().into_owned();
    let ev = json!({"hook_event_name": "SessionStart", "session_id": "s-cmd-start", "cwd": cwd, "source": "startup"});
    child.stdin.take().unwrap().write_all(ev.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(!stdout.contains("hook time budget"), "{stdout}");
}
