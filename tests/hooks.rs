//! The hooks: relevance-gated recall with its budget, the subagent brief
//! hand-over, the context cap, the token log, and the hook worker over the
//! real binary's stdio.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use atlas::hooks::{HookEvent, Hooks};
use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
use atlas::memory::{Draft, MemoryStore};
use common::*;

fn settings(dir: &Path) -> Settings {
    let mut s = Settings::from_config(None, &dir.join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    // The floor is calibrated on a real corpus; bm25 scale grows with corpus size.
    s.recall.fts_floor = 0.0;
    s.recall.hybrid_fts_floor = 0.0;
    s
}

/// [`settings`] with the experimental compaction features on, in a project
/// (whose store is its own, so the card lists its lanes).
fn compaction(dir: &Path) -> Settings {
    let mut s = settings(dir);
    s.compact.enabled = true;
    s.graph = Some(std::sync::Arc::new(atlas::project::Graph { name: "proj".into(), ..atlas::project::Graph::default() }));
    s
}

fn seed(s: &Settings) {
    let mut store = MemoryStore::open(&s.store).unwrap();
    let mut w = store.writer().unwrap();
    let mut add = |key: &str, kind: &str, scope: &str, sub: &str, title: &str, body: &str| {
        let d = Draft { title: title.into(), body: body.into(), reason: "seeding hook tests".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
        w.create(key, kind, sub, scope, &d, None).unwrap();
    };
    add("widget-cache", "memory", "", "feedback", "- [Widget cache](widget-cache.md) - bound", "\nThe widget cache evicts least recently used entries past sixty four widgets.\n");
    add("rules:r00001", "rule", "rules", "", "gadget exporter rule", "- the gadget exporter writes with temp files and renames them into place.\n");
    add("rules:r00002", "rule", "rules", "", "unrelated", "- the garden is watered on mondays.\n");
    add("journal:build-7:0001", "journal", "build-7", "brief", "Build the gadget exporter", "");
    add("journal:build-7:0002", "journal", "build-7", "next", "Wire the hooks", "");
    drop(w.commit());
}

fn ev(event: &str) -> HookEvent {
    HookEvent { event: event.into(), session_id: "s1".into(), ..HookEvent::default() }
}

#[test]
fn recall_is_gated_budgeted_and_not_repeated() {
    let t = Temp::new("hook-recall");
    let s = settings(t.path());
    seed(&s);
    let mut h = Hooks::open(s.clone()).unwrap();
    // Unrelated prompt: nothing.
    let a = h.handle(&HookEvent { prompt: "what is the weather like on the coast".into(), ..ev("UserPromptSubmit") });
    assert_eq!(a.to_json("UserPromptSubmit"), "");
    // A prompt about the widget cache gets that memory, not the garden.
    let a = h.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("widget-cache"), "{}", a.context);
    assert!(!a.context.contains("garden"), "{}", a.context);
    assert!(a.context.len() <= s.bytes_for(s.recall.budget_tokens) + 128);
    // Same session, same question: not injected twice.
    let again = h.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), ..ev("UserPromptSubmit") });
    assert!(!again.context.contains("widget-cache"), "{}", again.context);
    // A tiny budget injects nothing whole.
    let mut tight = s.clone();
    tight.recall.budget_tokens = 5;
    let mut h2 = Hooks::open(tight).unwrap();
    let a = h2.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), ..ev("UserPromptSubmit") });
    assert!(!a.context.contains("== widget-cache"), "{}", a.context);
}

#[test]
fn a_session_budget_stops_injection_once_spent() {
    let t = Temp::new("hook-session-budget");
    let mut s = settings(t.path());
    seed(&s);
    let widget = "why does the widget cache evict entries past sixty four widgets";
    let gadget = "how does the gadget exporter write with temp files and rename them";
    // No cap (the default): both prompts get their record.
    let mut h = Hooks::open(s.clone()).unwrap();
    assert!(h.handle(&HookEvent { prompt: widget.into(), ..ev("UserPromptSubmit") }).context.contains("widget-cache"));
    assert!(h.handle(&HookEvent { prompt: gadget.into(), ..ev("UserPromptSubmit") }).context.contains("rules:r00001"));
    // A cap the first injection spends: the second prompt gets nothing.
    s.recall.session_budget_tokens = 1;
    let mut h = Hooks::open(s.clone()).unwrap();
    let first = h.handle(&HookEvent { prompt: widget.into(), ..ev("UserPromptSubmit") });
    assert!(first.context.is_empty() || first.context.len() <= s.bytes_for(s.recall.budget_tokens) + 128, "{}", first.context);
    s.recall.session_budget_tokens = 40;
    let mut h = Hooks::open(s.clone()).unwrap();
    let first = h.handle(&HookEvent { prompt: widget.into(), ..ev("UserPromptSubmit") });
    assert!(first.context.contains("widget-cache"), "{}", first.context);
    let second = h.handle(&HookEvent { prompt: gadget.into(), ..ev("UserPromptSubmit") });
    assert!(second.context.is_empty(), "the session spent its budget: {}", second.context);
    // Another session has its own.
    let other = h.handle(&HookEvent { prompt: gadget.into(), session_id: "s2".into(), ..ev("UserPromptSubmit") });
    assert!(other.context.contains("rules:r00001"), "{}", other.context);
}

#[test]
fn a_subagent_gets_recall_from_the_parents_brief_and_its_lane() {
    let t = Temp::new("hook-brief");
    let s = settings(t.path());
    seed(&s);
    let mut h = Hooks::open(s).unwrap();
    let brief = "You are build-7. Continue the gadget exporter: it writes with temp files and renames them into place.";
    let pre = h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: brief.into(), ..ev("PreToolUse") });
    assert_eq!(pre, Default::default());
    let start = h.handle(&HookEvent { agent_id: "a1".into(), agent_type: "general-purpose".into(), ..ev("SubagentStart") });
    assert!(start.context.contains("lane build-7"), "{}", start.context);
    assert!(start.context.contains("# brief") && start.context.contains("# next"), "{}", start.context);
    assert!(start.context.contains("rules:r00001"), "{}", start.context);
    // No stashed brief: the subagent's first tool call reads its own transcript.
    let dir = t.path().join("proj");
    let parent = dir.join("sess.jsonl");
    std::fs::create_dir_all(dir.join("sess").join("subagents")).unwrap();
    std::fs::write(&parent, "").unwrap();
    std::fs::write(
        dir.join("sess").join("subagents").join("agent-a2.jsonl"),
        format!("{}\n", json!({"type": "user", "message": {"role": "user", "content": brief}})),
    )
    .unwrap();
    let start = h.handle(&HookEvent { agent_id: "a2".into(), ..ev("SubagentStart") });
    assert!(start.context.is_empty());
    let first = h.handle(&HookEvent { agent_id: "a2".into(), tool_name: "Bash".into(), transcript_path: parent.to_string_lossy().into_owned(), ..ev("PreToolUse") });
    assert!(first.context.contains("lane build-7"), "{}", first.context);
    let second = h.handle(&HookEvent { agent_id: "a2".into(), tool_name: "Bash".into(), transcript_path: parent.to_string_lossy().into_owned(), ..ev("PreToolUse") });
    assert!(second.context.is_empty());
}

/// A `<task-notification>` as Claude Code sends it (shapes read from real
/// transcripts 2026-11-03): an interim stop while the agent's own
/// background work runs, or a real finish.
fn task_note(id: &str, interim: bool) -> String {
    let (summary, note) = if interim {
        ("is waiting on its own background work", "This agent stopped with background work of its own still running. It may resume on its own when that work completes or reports, and the same task-id notifies again if it does; the result below may be interim.")
    } else {
        ("finished", "A task-notification fires each time this agent stops with no live background children of its own.")
    };
    format!("<task-notification>\n<task-id>{id}</task-id>\n<status>completed</status>\n<summary>Agent \"x\" {summary}</summary>\n<note>{note}</note>\n<result>r</result>\n</task-notification>")
}

fn journal_entry(s: &Settings, key: &str, lane: &str) {
    let mut store = MemoryStore::open(&s.store).unwrap();
    let mut w = store.writer().unwrap();
    let d = Draft { title: "an entry".into(), reason: "hook test".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
    w.create(key, "journal", "note", lane, &d, None).unwrap();
    w.commit().unwrap();
}

/// #79: an interim stop says nothing; a real finish with no entry names the
/// lane the brief assigns, not a neighbouring or more recent lane it also
/// mentions; an entry since the start quiets it; a brief that names no one
/// lane gives no warning.
#[test]
fn the_end_of_agent_check_skips_interim_stops_and_resolves_the_agents_own_lane() {
    let t = Temp::new("hook-finish");
    let s = settings(t.path());
    seed(&s);
    // A more recent neighbouring lane, the one the old check picked.
    journal_entry(&s, "journal:SIDE-TASKS:0001", "SIDE-TASKS");
    let mut h = Hooks::open(s.clone()).unwrap();
    let spawn = |h: &mut Hooks, id: &str, brief: &str| {
        h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: brief.into(), ..ev("PreToolUse") });
        h.handle(&HookEvent { agent_id: id.into(), ..ev("SubagentStart") });
    };
    let note = |h: &mut Hooks, id: &str, interim: bool| h.handle(&HookEvent { prompt: task_note(id, interim), ..ev("UserPromptSubmit") }).context;
    spawn(&mut h, "fd1", "You are builder LF1, a builder. Journal lane: LIGHT-FADE (atlas `journal` tool). See SIDE-TASKS and lane build-7 for context.");
    assert_eq!(note(&mut h, "fd1", true), "", "an interim stop is not a finish");
    let w = note(&mut h, "fd1", false);
    assert!(w.contains("agent fd1 finished") && w.contains("lane LIGHT-FADE"), "{w}");
    assert!(!w.contains("SIDE-TASKS") && !w.contains("build-7"), "{w}");
    journal_entry(&s, "journal:LIGHT-FADE:0001", "LIGHT-FADE");
    assert_eq!(note(&mut h, "fd1", false), "", "an entry since the start");
    // Two lanes and none assigned: no lane resolves, no warning.
    spawn(&mut h, "rv1", "Review what build-7 and SIDE-TASKS changed.");
    assert_eq!(note(&mut h, "rv1", false), "");
    // One lane mentioned: that lane.
    spawn(&mut h, "b7", "Continue build-7: the gadget exporter.");
    assert!(note(&mut h, "b7", false).contains("lane build-7"));
    // An unknown task (a background command): nothing.
    assert_eq!(note(&mut h, "bqu7y4h25", false), "");
}

/// #79: the lane the agent itself wrote under (its own journal calls, read
/// from its transcript) wins over the brief, and a server that never saw
/// the agent start reads its start and brief from the transcript.
#[test]
fn the_end_of_agent_check_reads_the_agents_own_transcript() {
    let t = Temp::new("hook-finish-transcript");
    let s = settings(t.path());
    seed(&s);
    let dir = t.path().join("proj");
    let parent = dir.join("sess.jsonl");
    std::fs::create_dir_all(dir.join("sess").join("subagents")).unwrap();
    std::fs::write(&parent, "").unwrap();
    let call = json!({"type": "assistant", "timestamp": "2020-01-01T00:00:05.000Z", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "mcp__atlas__journal", "input": {"lane": "GADGET-2", "entry": "brief", "text": "x"}}]}});
    let user = json!({"type": "user", "timestamp": "2020-01-01T00:00:00.000Z", "message": {"role": "user", "content": "You are a builder. Continue build-7."}});
    std::fs::write(dir.join("sess").join("subagents").join("agent-w1.jsonl"), format!("{user}\n{call}\n")).unwrap();
    std::fs::write(dir.join("sess").join("subagents").join("agent-w2.jsonl"), format!("{user}\n")).unwrap();
    let mut h = Hooks::open(s.clone()).unwrap();
    let tp = parent.to_string_lossy().into_owned();
    let note = |h: &mut Hooks, id: &str| h.handle(&HookEvent { prompt: task_note(id, false), transcript_path: tp.clone(), ..ev("UserPromptSubmit") }).context;
    // It called journal for GADGET-2 (the call failed: no entry exists).
    let w = note(&mut h, "w1");
    assert!(w.contains("lane GADGET-2") && !w.contains("build-7"), "{w}");
    // No journal calls: the brief's lane, with the start from the transcript
    // (build-7's seeded entries are newer than it, so it is quiet).
    assert_eq!(note(&mut h, "w2"), "");
}

#[test]
fn a_brief_names_its_own_lane() {
    let known = vec!["SIDE-TASKS".to_string(), "build-7".to_string(), "COMPACT-FIX".to_string()];
    let lane = |b: &str| atlas::hooks::brief_lane(b, &known);
    assert_eq!(lane("You are LANE HOOK-FIX. Journal under lane HOOK-FIX. The warning says 'for lane Y since'."), Some("HOOK-FIX".into()));
    assert_eq!(lane("You are builder LF1. Journal lane: LIGHT-FADE. Builds on SIDE-TASKS."), Some("LIGHT-FADE".into()));
    assert_eq!(lane("COMPACT-FIX filed a note; journal lane `HOOK-FIX`."), Some("HOOK-FIX".into()));
    assert_eq!(lane("Compare lane COMPACT-FIX with lane SIDE-TASKS."), None);
    assert_eq!(lane("Your lane is free: see build-7 and COMPACT-FIX."), None);
    assert_eq!(lane("Continue build-7."), Some("build-7".into()));
    assert_eq!(lane("lane journals are append-only"), None);
    assert!(atlas::hooks::is_interim(&task_note("a", true)) && !atlas::hooks::is_interim(&task_note("a", false)));
}

fn usage_line(id: &str, ctx: u64) -> String {
    format!("{}\n", json!({"type": "assistant", "message": {"id": id, "usage": {"input_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": ctx - 2, "output_tokens": 0}}}))
}

#[test]
fn the_cap_warns_then_blocks_all_but_handoff_in_subagents_only() {
    let t = Temp::new("hook-cap");
    let s = settings(t.path());
    let mut h = Hooks::open(s).unwrap();
    let dir = t.path().join("proj");
    let parent = dir.join("sess.jsonl");
    let sub = dir.join("sess").join("subagents").join("agent-a9.jsonl");
    std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
    std::fs::write(&parent, usage_line("p1", 600_000)).unwrap();
    let call = |h: &mut Hooks, agent: &str, tool: &str| {
        h.handle(&HookEvent { agent_id: agent.into(), tool_name: tool.into(), transcript_path: parent.to_string_lossy().into_owned(), ..ev("PreToolUse") })
    };
    // The main session is never capped by default, whatever its size.
    assert_eq!(call(&mut h, "", "Bash"), Default::default());
    std::fs::write(&sub, usage_line("m1", 100_000)).unwrap();
    assert_eq!(call(&mut h, "a9", "Bash"), Default::default());
    std::fs::write(&sub, usage_line("m1", 100_000) + &usage_line("m2", 451_000)).unwrap();
    let w = call(&mut h, "a9", "Bash");
    assert!(w.deny.is_none() && w.context.contains("451000 tokens"), "{w:?}");
    // Not repeated until another 25k tokens.
    std::fs::write(&sub, usage_line("m2", 451_000) + &usage_line("m3", 460_000)).unwrap();
    assert!(call(&mut h, "a9", "Bash").context.is_empty());
    std::fs::write(&sub, usage_line("m3", 460_000) + &usage_line("m4", 501_000) + &usage_line("m4", 501_000)).unwrap();
    let b = call(&mut h, "a9", "Bash");
    assert!(b.deny.as_deref().is_some_and(|r| r.contains("handoff")), "{b:?}");
    assert!(b.to_json("PreToolUse").contains("\"permissionDecision\":\"deny\""));
    let ok = call(&mut h, "a9", "mcp__plugin_atlas_atlas__handoff");
    assert!(ok.deny.is_none(), "{ok:?}");
    // Loading the deferred handoff schema is allowed too; anything else is not.
    assert!(call(&mut h, "a9", "ToolSearch").deny.is_none());
    assert!(call(&mut h, "a9", "Read").deny.is_some());
    // Both crossings are journal entries for the agent.
    let lane = h.store().lane("agent:a9").unwrap();
    let subs: Vec<&str> = lane.iter().map(|x| x.title.as_str()).collect();
    assert_eq!(subs, vec!["context cap warned at 451000 tokens", "context cap blocked at 501000 tokens"]);
}

#[test]
fn every_prompt_answer_is_token_logged_even_when_empty() {
    let t = Temp::new("hook-log");
    let s = settings(t.path());
    seed(&s);
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&HookEvent { prompt: "nothing related at all here".into(), ..ev("UserPromptSubmit") });
        h.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), ..ev("UserPromptSubmit") });
    }
    // The log thread writes after the answer; wait for both rows.
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let mut rows: Vec<(String, i64, Option<i64>)> = Vec::new();
    for _ in 0..200 {
        rows = conn
            .prepare("SELECT tool, returned, read FROM token_log ORDER BY id")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
            .unwrap_or_default();
        if rows.len() == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0], ("hook:UserPromptSubmit".to_string(), 0, None));
    assert!(rows[1].1 > 0 && rows[1].2.unwrap_or(0) > 0, "{rows:?}");
}

/// Each subagent start logs the rulebook's size at that moment to its own
/// table, never to the per-answer ledger.
#[test]
fn a_subagent_start_logs_the_brief_without_atlas() {
    let t = Temp::new("hook-brief");
    let mut s = settings(t.path());
    let rulebook = t.path().join("RULES.md");
    write(&rulebook, &"- a rule\n".repeat(100));
    s.brief_reference = Some(rulebook);
    seed(&s);
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&HookEvent { agent_id: "a1".into(), ..ev("SubagentStart") });
    }
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let mut rows: Vec<(String, String, i64)> = Vec::new();
    for _ in 0..200 {
        rows = conn
            .prepare("SELECT session, agent, bytes FROM brief_log ORDER BY id")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
            .unwrap_or_default();
        if !rows.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(rows, vec![("s1".to_string(), "a1".to_string(), 900)]);
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM token_log WHERE tool = 'hook:SubagentStart'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

fn assistant(time: &str, item: Value) -> String {
    format!("{}\n", json!({"type": "assistant", "timestamp": time, "message": {"role": "assistant", "content": [item]}}))
}

/// Every injected key is logged with its bytes and counterfactual, and the
/// report marks it hit by the fixed rules: the key in a later tool input, or
/// a quoted run of its words; an unused key is not a hit, nor a use before
/// the injection.
#[test]
fn injected_keys_are_logged_and_judged_by_later_use() {
    let t = Temp::new("hook-hits");
    let s = settings(t.path());
    seed(&s);
    let main = t.path().join("main.jsonl");
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), transcript_path: main.to_string_lossy().into(), ..ev("UserPromptSubmit") });
        h.handle(&HookEvent { prompt: "does the gadget exporter write with temp files and rename them".into(), transcript_path: main.to_string_lossy().into(), ..ev("UserPromptSubmit") });
    }
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let mut rows = Vec::new();
    for _ in 0..200 {
        rows = atlas::hook_hits::logged(&conn, "").unwrap();
        if rows.len() >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    assert!(keys.contains(&"widget-cache") && keys.contains(&"rules:r00001"), "{keys:?}");
    for r in &rows {
        assert_eq!(r.hook, "UserPromptSubmit");
        assert_eq!(r.transcript, main.to_string_lossy());
        assert!(r.bytes > 0 && r.read > 0 && r.fts.is_some(), "{r:?}");
    }
    let late = "2999-01-01T00:00:00.000Z";
    let mut text = assistant("2000-01-01T00:00:00.000Z", json!({"type": "tool_use", "name": "recall", "input": {"key": "rules:r00001"}}));
    text.push_str(&assistant(late, json!({"type": "tool_use", "name": "mcp__atlas__recall", "input": {"key": "widget-cache"}})));
    write(&main, &text);
    let store = MemoryStore::open(&s.store).unwrap();
    let mut judge = atlas::hook_hits::Judge::new(&store, &s.exports);
    let verdict = |judge: &mut atlas::hook_hits::Judge, key: &str| judge.hit(rows.iter().find(|r| r.key == key).unwrap());
    assert_eq!(verdict(&mut judge, "widget-cache"), Some("key"));
    // Recalled only before the injection: not a hit.
    assert_eq!(verdict(&mut judge, "rules:r00001"), None);
    text.push_str(&assistant(late, json!({"type": "text", "text": "Per the rule, the gadget exporter writes with temp files and renames them into place."})));
    write(&main, &text);
    let mut judge = atlas::hook_hits::Judge::new(&store, &s.exports);
    assert_eq!(verdict(&mut judge, "rules:r00001"), Some("quote"));
}

/// A past injection is read back out of the transcript's `hook_success`
/// attachment, with the prompt it answered.
#[test]
fn past_injections_are_found_in_transcripts() {
    let t = Temp::new("hook-found");
    let p = t.path().join("s.jsonl");
    let ctx = "atlas recall (related records; read one with recall key=KEY):\n- widget-cache: Widget cache - bound\n";
    let out = json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": ctx}}).to_string();
    let hook = json!({"type": "attachment", "timestamp": "2026-11-01T00:00:00.000Z", "sessionId": "s9", "attachment": {"type": "hook_success", "hookEvent": "UserPromptSubmit", "stdout": out}});
    let user = json!({"type": "user", "timestamp": "2026-11-01T00:00:00.100Z", "message": {"role": "user", "content": "why does the widget cache evict"}});
    write(&p, &format!("{hook}\n{user}\n"));
    let f = atlas::hook_hits::found_in(&p, "2026-10-01");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].injection.key, "widget-cache");
    assert_eq!(f[0].injection.session, "s9");
    assert_eq!(f[0].injection.bytes, "- widget-cache: Widget cache - bound\n".len());
    assert_eq!(f[0].prompt, "why does the widget cache evict");
    assert!(atlas::hook_hits::found_in(&p, "2026-12-01").is_empty());
}

/// The binary over stdio: a `hook` call is answered by the worker with the
/// hook JSON, and ordinary calls still answer in order.
#[test]
fn the_server_answers_hook_calls_with_hook_json() {
    let t = Temp::new("hook-stdio");
    let vault = t.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let cfg = t.path().join("atlas.json");
    write(&cfg, &format!("{}", json!({"db": "state/index.sqlite", "vaults": [{"name": "v", "path": "vault"}], "watch": false, "recall": {"fts_floor": 0.0}})));
    seed(&settings(t.path()));
    let mut child = Command::new(bin()).arg("--config").arg(&cfg).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    {
        let mut i = child.stdin.take().unwrap();
        let lines = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "t"}}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "hook", "arguments": {"event": "UserPromptSubmit", "session_id": "s", "prompt": "why does the widget cache evict entries past sixty four widgets"}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "recall", "arguments": {"key": "widget-cache"}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "hook", "arguments": {"event": "UserPromptSubmit", "session_id": "s", "prompt": "the weather on the coast"}}}),
        ];
        for l in lines {
            writeln!(i, "{l}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let replies: Vec<Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| json::parse(l).unwrap()).collect();
    let by = |id: i64| replies.iter().find(|r| r["id"].as_i64() == Some(id)).cloned().unwrap();
    let hook = by(2)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let hv = json::parse(&hook).unwrap();
    assert_eq!(hv["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(hv["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("widget-cache"));
    assert!(by(3)["result"]["content"][0]["text"].as_str().unwrap().contains("== widget-cache memory"));
    assert_eq!(by(4)["result"]["content"][0]["text"], "");
    let _: Option<PathBuf> = None;
}

/// With a model file present the gate also scores meaning: records get
/// vectors (journals do not), a cosine floor above every record keeps a
/// lexical match out, and a `cos_alone` below every cosine lets a record in
/// on meaning alone.
#[test]
fn the_gate_scores_meaning_with_a_model() {
    use atlas::memory::vectors::{self, RecVecs};
    let t = Temp::new("hook-sem");
    let mut s = settings(t.path());
    std::fs::create_dir_all(s.model.parent().unwrap()).unwrap();
    std::fs::write(&s.model, atlas::embed::tiny_model_bytes(true, 7)).unwrap();
    seed(&s);
    {
        // Embedded here, as the background thread would, so the test does not wait on it.
        let store = MemoryStore::open(&s.store).unwrap();
        let m = atlas::embed::Model::load(&s.model).unwrap();
        while vectors::embed_pending(&store, &m, 2, 1).unwrap() > 0 {}
        let v = RecVecs::load(&store, &m.id, m.dim).unwrap();
        assert_eq!((v.rids.len(), v.missing), (3, 0), "one memory and two rules; journals are not embedded");
    }
    let widget = "why does the widget cache evict entries past sixty four widgets";
    s.recall.cos_floor = 1.01;
    s.recall.cos_alone = 2.0;
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = h.handle(&HookEvent { prompt: widget.into(), ..ev("UserPromptSubmit") });
    assert!(!a.context.contains("widget-cache"), "a lexical match below the cosine floor passed: {}", a.context);
    s.recall.cos_floor = 0.0;
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = h.handle(&HookEvent { prompt: widget.into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("widget-cache"), "{}", a.context);
    s.recall.cos_alone = -1.01;
    let mut h = Hooks::open(s).unwrap();
    let a = h.handle(&HookEvent { prompt: "what is the weather like on the coast".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("atlas recall"), "nothing passed on meaning alone: {}", a.context);
}

/// R6: recall injects pointers (key and title), within 150 tokens, never a
/// body; and on a new store of ten records the default gate (calibrated on
/// a large store) still recalls the related record and nothing for an
/// unrelated prompt, because the bm25 floor follows the store's size.
#[test]
fn a_ten_record_store_recalls_pointers_with_the_default_gate() {
    use atlas::memory::settings::floor_scale;
    assert!((floor_scale(1400) - 1.0).abs() < 1e-9);
    assert!((floor_scale(10) - 0.27).abs() < 0.01, "{}", floor_scale(10));
    assert_eq!(floor_scale(2), 0.0);
    let t = Temp::new("hook-ten");
    let mut s = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    assert_eq!(s.recall.budget_tokens, 150);
    {
        let mut store = MemoryStore::open(&s.store).unwrap();
        let mut w = store.writer().unwrap();
        let topics = [
            ("widget-cache", "memory", "- [Widget cache](widget-cache.md) - eviction bound", "The widget cache evicts least recently used entries past sixty four widgets, oldest first."),
            ("rules:r00001", "rule", "", "- the gadget exporter writes temp files and renames them into place."),
            ("rules:r00002", "rule", "", "- the garden is watered on mondays and fridays."),
            ("rules:r00003", "rule", "", "- release builds are signed before upload."),
            ("rules:r00004", "rule", "", "- the parser rejects tabs inside quoted keys."),
            ("rules:r00005", "rule", "", "- backups rotate weekly and keep four copies."),
            ("rules:r00006", "rule", "", "- the scheduler wakes every five minutes."),
            ("rules:r00007", "rule", "", "- error messages name the file and the line."),
            ("rules:r00008", "rule", "", "- the font cache is rebuilt at install."),
            ("rules:r00009", "rule", "", "- logs older than thirty days are compressed."),
        ];
        for (key, kind, title, body) in topics {
            let d = Draft { title: title.into(), body: body.into(), reason: "ten-record test".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
            w.create(key, kind, if kind == "memory" { "feedback" } else { "" }, if kind == "rule" { "rules" } else { "" }, &d, None).unwrap();
        }
        w.commit().unwrap();
    }
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    let a = h.handle(&HookEvent { prompt: "why does the widget cache evict the least recently used widgets".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.starts_with(atlas::hooks::POINTER_HEAD), "{:?}", a.context);
    assert!(a.context.contains("- widget-cache: Widget cache - eviction bound\n"), "{}", a.context);
    assert!(!a.context.contains("sixty four"), "a pointer, not the body: {}", a.context);
    assert!(a.context.len() <= s.bytes_for(150), "{}", a.context.len());
    let a = h.handle(&HookEvent { prompt: "what time does the train to the coast leave".into(), ..ev("UserPromptSubmit") });
    assert_eq!(a.context, "", "unrelated: nothing");
    let a = h.handle(&HookEvent { prompt: "the gadget exporter writes temp files, then renames them into place?".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("- rules:r00001: the gadget exporter writes temp files and renames them into place.\n"), "{}", a.context);
    // Already injected in this session: not again.
    let a = h.handle(&HookEvent { prompt: "why does the widget cache evict the least recently used widgets".into(), ..ev("UserPromptSubmit") });
    assert!(!a.context.contains("widget-cache"), "{}", a.context);
}

#[test]
fn an_edit_reindexes_the_file_once_the_answer_is_out() {
    let t = Temp::new("h-r7");
    let s = settings(t.path());
    // A code root, indexed by the line scan, and a vault.
    let root = t.path().join("app");
    write(&root.join("src/a.rs"), "pub struct A;\n\nimpl A {\n    pub fn one(&self) -> u32 {\n        1\n    }\n}\n");
    let notes = t.path().join("notes");
    write(&notes.join("n.md"), "# N\n\nfirst words\n");
    let vaults = || atlas::paths::Vaults::new(&[("notes".to_string(), notes.clone())]).unwrap();
    drop(atlas::store::Store::open(&s.index_db, vaults()).unwrap());
    let spec = atlas::config::CodeSpec { roots: vec![atlas::config::CodeRootSpec { name: "app".into(), root: root.clone(), build_scripts: false, scip: Default::default() }], ..Default::default() };
    let code = atlas::code::CodeSetup::from_spec(&spec, &s.index_db, false);
    let mut conn = rusqlite::Connection::open(&s.index_db).unwrap();
    atlas::code_index::index_root(&code, &mut conn, &code.roots[0], &atlas::code_index::RustSource::ScanOnly("test".into())).unwrap();
    let mut st = atlas::store::Store::open(&s.index_db, vaults()).unwrap();
    st.rescan().unwrap();
    drop(st);
    let sha = |c: &rusqlite::Connection| -> String { c.query_row("SELECT sha256 FROM notes WHERE path = 'n.md'", [], |r| r.get(0)).unwrap() };
    let before = sha(&conn);
    let mut h = Hooks::open(s.clone()).unwrap();
    let db = s.index_db.clone();
    let nv = notes.clone();
    h.set_index_opener(Some(Box::new(move || atlas::store::Store::open(&db, atlas::paths::Vaults::new(&[("notes".to_string(), nv.clone())])?))));
    // The edit moves `one` down two lines and adds `two`.
    let a = root.join("src/a.rs");
    // Past the root's indexing time (milliseconds).
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&a, "pub struct A;\n\n// moved\n// down\nimpl A {\n    pub fn one(&self) -> u32 {\n        1\n    }\n    pub fn two(&self) -> u32 {\n        2\n    }\n}\n").unwrap();
    let path = a.to_string_lossy().to_string();
    assert!(atlas::code::file_items(&conn, &path).unwrap().is_none(), "stale before the edit's re-index");
    let edit = |tool: &str, file: &str| HookEvent {
        event: "PostToolUse".into(),
        session_id: "s1".into(),
        tool_name: tool.into(),
        tool_input: json!({"file_path": file, "old_string": "x", "new_string": "y"}),
        tool_response: json!({"filePath": file}),
        ..HookEvent::default()
    };
    let ans = h.handle(&edit("Edit", &path));
    // The answer does not wait for the re-index, and says nothing.
    assert!(ans.to_json("PostToolUse").len() < 80, "{}", ans.to_json("PostToolUse"));
    h.after();
    let items = atlas::code::file_items(&conn, &path).unwrap().expect("covered again");
    let labels: Vec<String> = items.iter().map(|i| i.label()).collect();
    assert!(labels.contains(&"fn A::one 6-8".to_string()) && labels.contains(&"fn A::two 9-11".to_string()) && labels.contains(&"impl A 5-12".to_string()), "{labels:?}");
    let v = atlas::code::symbol(&conn, "two", atlas::code::Limits { defs: 5, refs: 5, impls: 5 }).unwrap();
    assert_eq!(v.totals[0], 1);
    // A note in a vault: rescanned.
    let n = notes.join("n.md");
    std::fs::write(&n, "# N\n\nfirst words, then more\n").unwrap();
    h.handle(&edit("Write", &n.to_string_lossy()));
    h.after();
    assert_ne!(sha(&conn), before);
    // A deleted code file loses its rows; a file nothing covers is left alone.
    std::fs::remove_file(&a).unwrap();
    assert_eq!(h_reindex(&path, &s), atlas::code::Reindexed::Removed);
    assert_eq!(h_reindex(&t.path().join("elsewhere.rs").to_string_lossy(), &s), atlas::code::Reindexed::NotCovered);
    assert!(atlas::code::symbol(&conn, "two", atlas::code::Limits { defs: 5, refs: 5, impls: 5 }).unwrap().totals == [0, 0, 0]);
}

fn h_reindex(path: &str, s: &Settings) -> atlas::code::Reindexed {
    atlas::hook_rewrite::Router::new(s).reindex_code(path).unwrap()
}
/// A transcript holding one compaction: the boundary with Claude Code's
/// metadata, then the summary.
fn compacted_transcript(path: &Path, summary: &str) {
    let mut f = std::fs::File::create(path).unwrap();
    let boundary = json!({"type": "system", "subtype": "compact_boundary", "timestamp": "2020-01-01T00:00:00.000Z",
        "compactMetadata": {"trigger": "auto", "preTokens": 200000, "postTokens": 10000, "durationMs": 40000}});
    let s = json!({"type": "user", "isCompactSummary": true, "message": {"role": "user", "content": summary}});
    write!(f, "{}\n{}\n", boundary, s).unwrap();
}

#[test]
fn after_a_compaction_the_summary_refs_are_checked_and_logged() {
    let t = Temp::new("hook-compact");
    let s = compaction(t.path());
    seed(&s);
    let mut store = MemoryStore::open(&s.store).unwrap();
    let old = store.head("widget-cache").unwrap().unwrap().sha;
    {
        let mut w = store.writer().unwrap();
        let d = Draft { title: "- [Widget cache](widget-cache.md) - bound".into(), body: "\nThe widget cache evicts past ninety widgets.\n".into(), reason: "compaction test".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
        w.revise("widget-cache", None, &d).unwrap();
        w.commit().unwrap();
    }
    let new = store.head("widget-cache").unwrap().unwrap().sha;
    let rule = store.head("rules:r00001").unwrap().unwrap().sha;
    let summary = format!(
        "Work so far: rules:r00001@{} holds; cache bound was widget-cache@{}; also rules:r00009@0123456789ab.",
        &rule[..12],
        &old[..12]
    );
    let tp = t.path().join("session.jsonl");
    compacted_transcript(&tp, &summary);
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    let a = h.handle(&HookEvent { source: "compact".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("SessionStart") });
    assert!(a.context.contains(&format!("atlas ref changed: widget-cache@{} -> widget-cache@{}", &old[..12], &new[..12])), "{}", a.context);
    assert!(a.context.contains("atlas ref dangling: rules:r00009@0123456789ab"), "{}", a.context);
    assert!(!a.context.contains("rules:r00001@"), "a current ref is not reported: {}", a.context);
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let row: (i64, i64, i64, i64, i64) = conn
        .query_row("SELECT pre_tokens, post_tokens, refs, changed, dangling FROM compact_log", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap();
    assert_eq!(row, (200000, 10000, 3, 1, 1));
    let tot = atlas::compact::totals(&conn, 4.0).unwrap().unwrap();
    assert_eq!((tot.n, tot.refs, tot.dangling), (1, 3, 1));
    // A startup SessionStart checks nothing.
    let a = h.handle(&HookEvent { source: "startup".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("SessionStart") });
    assert!(!a.context.contains("atlas ref"), "{}", a.context);
}

#[test]
fn a_lookup_made_again_after_a_compaction_is_a_logged_miss() {
    let t = Temp::new("hook-compact-miss");
    let s = compaction(t.path());
    seed(&s);
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        let read = |p: &str| HookEvent { tool_name: "Read".into(), tool_input: json!({"file_path": p}), ..ev("PreToolUse") };
        h.handle(&read("src/a.rs"));
        h.handle(&read("src/a.rs"));
        h.handle(&HookEvent { trigger: "auto".into(), ..ev("PreCompact") });
        h.handle(&read("src/a.rs"));
        h.handle(&read("src/b.rs"));
        // A subagent's lookups are not the session's.
        h.handle(&HookEvent { agent_id: "x".into(), ..read("src/a.rs") });
    }
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let mut n = 0i64;
    for _ in 0..200 {
        n = conn.query_row("SELECT COUNT(*) FROM token_log WHERE tool = 'compact:miss'", [], |r| r.get(0)).unwrap_or(0);
        if n > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    n = conn.query_row("SELECT COUNT(*) FROM token_log WHERE tool = 'compact:miss'", [], |r| r.get(0)).unwrap_or(n);
    assert_eq!(n, 1);
}

/// A synthetic session segment: a prompt, a read with its result, an edit,
/// then a running tool call (no result yet). Nothing here is a real
/// transcript.
fn working_transcript(path: &Path, ctx: u64) {
    let rec = |kind: &str, id: &str, content: atlas::json::Value, c: u64| {
        json!({"type": kind, "timestamp": "2020-01-01T00:00:00.000Z", "message": {"id": id, "usage": {"input_tokens": c}, "content": content}})
    };
    let lines = [
        rec("user", "", json!([{"type": "text", "text": "fix the exporter"}]), 0),
        rec("assistant", "m1", json!([{"type": "thinking", "thinking": "private chain of thought"}, {"type": "text", "text": "I will read the exporter first."}]), ctx - 30000),
        rec("assistant", "m2", json!([{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "src/export.rs"}}]), ctx - 20000),
        rec("user", "", json!([{"type": "tool_result", "tool_use_id": "t1", "content": "fn export() {}"}]), 0),
        rec("assistant", "m3", json!([{"type": "tool_use", "id": "t2", "name": "Edit", "input": {"file_path": "src/export.rs"}}]), ctx - 10000),
        rec("user", "", json!([{"type": "tool_result", "tool_use_id": "t2", "content": "ok"}]), 0),
        rec("assistant", "m4", json!([{"type": "tool_use", "id": "t3", "name": "Bash", "input": {"command": "cargo test --no-fail-fast"}}]), ctx),
    ];
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    std::fs::write(path, text.join("\n") + "\n").unwrap();
}

#[test]
fn precompact_writes_a_checkpoint_and_the_card_follows_the_compaction() {
    let t = Temp::new("hook-checkpoint");
    let s = compaction(t.path());
    seed(&s);
    let tp = t.path().join("s1.jsonl");
    working_transcript(&tp, 300_000);
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    let a = h.handle(&HookEvent { trigger: "auto".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") });
    assert!(a.block.is_none(), "compact.defer is off by default");
    let store = MemoryStore::open(&s.store).unwrap();
    let ck = atlas::checkpoint::newest(&store, "s1").unwrap().expect("a checkpoint");
    assert_eq!((ck.kind.as_str(), ck.sub.as_str()), ("journal", "checkpoint"));
    assert!(ck.title.contains("at 300000 context tokens"), "{}", ck.title);
    let next = store.head("journal:build-7:0002").unwrap().unwrap();
    let r00001 = store.head("rules:r00001").unwrap().unwrap();
    for want in [
        format!("lanes: build-7 (step journal:build-7:0002@{})", &next.sha[..12]),
        format!("owed: journal:build-7:0002@{}", &next.sha[..12]),
        format!("rules:r00001@{}", &r00001.sha[..12]),
        "files: src/export.rs".to_string(),
        "tools: Read src/export.rs -> turn:s1:4@".to_string(),
        "pending: Bash cargo test --no-fail-fast".to_string(),
        "intent: I will read the exporter first.".to_string(),
    ] {
        assert!(ck.body.contains(&want), "missing {want:?} in\n{}", ck.body);
    }
    assert!(!ck.body.contains("chain of thought"), "thinking is never stored");
    // After the compaction: the card, with the checkpoint's own ref.
    compacted_transcript(&t.path().join("after.jsonl"), "summary text");
    let a = h.handle(&HookEvent { source: "compact".into(), transcript_path: t.path().join("after.jsonl").to_string_lossy().into_owned(), ..ev("SessionStart") });
    assert!(a.context.starts_with(&format!("atlas checkpoint {}@{}", ck.key, &ck.sha[..12])), "{}", a.context);
    assert!(a.context.contains("pending: Bash cargo test"), "{}", a.context);
    // Every ref in the card resolves.
    let check = atlas::compact::check(&store, &[], &a.context).unwrap();
    assert!(check.dangling.is_empty() && check.changed.is_empty() && check.refs >= 4, "{check:?}");
    // A second checkpoint points at the first.
    drop(store);
    h.handle(&HookEvent { trigger: "manual".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") });
    let store = MemoryStore::open(&s.store).unwrap();
    let ck2 = atlas::checkpoint::newest(&store, "s1").unwrap().unwrap();
    assert!(ck2.body.starts_with(&format!("since: {}@{}", ck.key, &ck.sha[..12])), "{}", ck2.body);
}

#[test]
fn compact_defer_blocks_an_auto_compaction_mid_task_only() {
    let t = Temp::new("hook-defer");
    let mut s = compaction(t.path());
    s.compact.defer = true;
    seed(&s);
    let tp = t.path().join("s1.jsonl");
    working_transcript(&tp, 300_000);
    let mut h = Hooks::open(s.clone()).unwrap();
    let pre = |trigger: &str| HookEvent { trigger: trigger.into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") };
    let a = h.handle(&pre("auto"));
    let reason = a.block.clone().expect("blocked mid-task");
    assert!(reason.contains("next quiet point"), "{reason}");
    assert_eq!(atlas::json::parse(&a.to_json("PreCompact")).unwrap()["decision"].as_str(), Some("block"));
    let store = MemoryStore::open(&s.store).unwrap();
    assert!(atlas::checkpoint::newest(&store, "s1").unwrap().is_none(), "no checkpoint for a deferred compaction");
    drop(store);
    // A manual compaction is never blocked.
    assert!(h.handle(&pre("manual")).block.is_none());
    // Past the ceiling (1M less 40k of headroom), the compaction goes ahead.
    working_transcript(&tp, 970_000);
    assert!(h.handle(&pre("auto")).block.is_none());
}

#[test]
fn with_the_transcript_source_on_precompact_indexes_turns_and_turn_refs_are_checked() {
    let t = Temp::new("hook-turns");
    let mut s = compaction(t.path());
    s.transcripts = true;
    seed(&s);
    let dir = t.path().join("H--work-proj");
    std::fs::create_dir_all(&dir).unwrap();
    let tp = dir.join("s1.jsonl");
    working_transcript(&tp, 300_000);
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    h.handle(&HookEvent { trigger: "auto".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") });
    h.after();
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let turn = atlas::transcripts::get(&conn, "H--work-proj", "s1", 4).unwrap().expect("the read's result is indexed");
    assert_eq!(turn.text, "fn export() {}");
    // The card's result ref is that turn's.
    let store = MemoryStore::open(&s.store).unwrap();
    let ck = atlas::checkpoint::newest(&store, "s1").unwrap().unwrap();
    assert!(ck.body.contains(&format!("turn:s1:4@{}", turn.hash)), "{}", ck.body);
    drop(store);
    // A summary citing it resolves; a wrong hash is dangling.
    let after = dir.join("after.jsonl");
    compacted_transcript(&after, &format!("read turn:s1:4@{} and turn:s1:4@0123456789ab", turn.hash));
    let a = h.handle(&HookEvent { source: "compact".into(), transcript_path: after.to_string_lossy().into_owned(), ..ev("SessionStart") });
    assert!(a.context.contains("atlas ref dangling: turn:s1:4@0123456789ab"), "{}", a.context);
    assert!(!a.context.contains(&format!("dangling: turn:s1:4@{}", turn.hash)), "{}", a.context);
}

#[test]
fn with_compaction_off_the_hooks_write_and_inject_nothing_for_it() {
    let t = Temp::new("hook-compact-off");
    let s = settings(t.path());
    assert!(!s.compact.enabled, "compaction is off by default");
    seed(&s);
    let tp = t.path().join("s1.jsonl");
    working_transcript(&tp, 300_000);
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    let a = h.handle(&HookEvent { trigger: "auto".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") });
    assert!(a.block.is_none() && a.to_json("PreCompact").is_empty());
    let store = MemoryStore::open(&s.store).unwrap();
    assert!(atlas::checkpoint::newest(&store, "s1").unwrap().is_none(), "no checkpoint while off");
    assert!(store.lane("session:s1").unwrap().is_empty(), "nothing written in the session lane");
    drop(store);
    // After the compaction: no card, no ref report, nothing logged.
    let after = t.path().join("after.jsonl");
    compacted_transcript(&after, "cache bound was widget-cache@0123456789ab");
    let a = h.handle(&HookEvent { source: "compact".into(), transcript_path: after.to_string_lossy().into_owned(), ..ev("SessionStart") });
    assert!(!a.context.contains("atlas checkpoint") && !a.context.contains("atlas ref"), "{}", a.context);
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM compact_log", [], |r| r.get(0)).unwrap_or(0);
    assert_eq!(n, 0);
    // defer does nothing without the switch either.
    let mut s2 = settings(t.path());
    s2.compact.defer = true;
    let mut h2 = Hooks::open(s2).unwrap();
    assert!(h2.handle(&HookEvent { trigger: "auto".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") }).block.is_none());
}

#[test]
fn without_a_project_the_card_lists_no_other_lanes() {
    // One store shared by every folder (no `projects`): another project's
    // lanes and rulings are in it, so the card keeps to the session.
    let t = Temp::new("hook-card-noproject");
    let mut s = settings(t.path());
    s.compact.enabled = true;
    seed(&s);
    let tp = t.path().join("s1.jsonl");
    working_transcript(&tp, 300_000);
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    let pre = HookEvent { trigger: "manual".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") };
    h.handle(&pre);
    let store = MemoryStore::open(&s.store).unwrap();
    let ck = atlas::checkpoint::newest(&store, "s1").unwrap().unwrap();
    assert!(!ck.body.contains("build-7") && !ck.body.contains("rules:"), "{}", ck.body);
    assert!(ck.body.contains("files: src/export.rs"), "the transcript side stays: {}", ck.body);
    drop(store);
    // The session's own lane still links the previous checkpoint.
    h.handle(&pre);
    let store = MemoryStore::open(&s.store).unwrap();
    let ck2 = atlas::checkpoint::newest(&store, "s1").unwrap().unwrap();
    assert!(ck2.body.starts_with(&format!("since: {}@", ck.key)), "{}", ck2.body);
}

#[test]
fn an_empty_start_is_bounded_never_all_time() {
    let t = Temp::new("hook-since-empty");
    let s = settings(t.path());
    seed(&s);
    let mut store = MemoryStore::open(&s.store).unwrap();
    {
        let mut w = store.writer().unwrap();
        let d = Draft { title: "an old lane".into(), reason: "test".into(), author: "t".into(), client: "t".into(), time: Some("2020-01-01T00:00:00.000Z".into()), ..Draft::default() };
        w.create("journal:OLD-LANE:0001", "journal", "next", "OLD-LANE", &d, None).unwrap();
        w.commit().unwrap();
    }
    let lanes = |st: &atlas::checkpoint::State| st.lanes.iter().map(|(l, _)| l.clone()).collect::<Vec<_>>();
    // Unknown start, live: the last day only (build-7 was written now).
    let st = atlas::checkpoint::state(&store, "s1", "", "", atlas::checkpoint::Lanes::Store).unwrap();
    assert_eq!(lanes(&st), ["build-7"]);
    // Unknown start, as of a past time: the day before it.
    let st = atlas::checkpoint::state(&store, "s1", "", "2020-01-01T12:00:00Z", atlas::checkpoint::Lanes::Store).unwrap();
    assert_eq!(lanes(&st), ["OLD-LANE"]);
    let st = atlas::checkpoint::state(&store, "s1", "", "2020-01-03T00:00:00Z", atlas::checkpoint::Lanes::Store).unwrap();
    assert!(lanes(&st).is_empty(), "{:?}", lanes(&st));
    // A known start reaches back as far as it says.
    let st = atlas::checkpoint::state(&store, "s1", "2019-12-31T00:00:00Z", "", atlas::checkpoint::Lanes::Store).unwrap();
    assert_eq!(lanes(&st), ["build-7", "OLD-LANE"]);
    // The hook path: a PreCompact with no transcript lists no old lane.
    let mut s = s.clone();
    s.compact.enabled = true;
    s.graph = Some(std::sync::Arc::new(atlas::project::Graph { name: "proj".into(), ..atlas::project::Graph::default() }));
    drop(store);
    let mut h = Hooks::open(s.clone()).unwrap();
    h.handle(&HookEvent { trigger: "manual".into(), transcript_path: t.path().join("none").join("x.jsonl").to_string_lossy().into_owned(), ..ev("PreCompact") });
    let store = MemoryStore::open(&s.store).unwrap();
    let ck = atlas::checkpoint::newest(&store, "s1").unwrap().unwrap();
    assert!(ck.body.contains("build-7") && !ck.body.contains("OLD-LANE"), "{}", ck.body);
}

#[test]
fn a_transcript_not_yet_written_is_read_from_its_parent_or_said_unread() {
    let t = Temp::new("hook-race");
    let s = compaction(t.path());
    seed(&s);
    let dir = t.path().join("H--work-proj");
    std::fs::create_dir_all(&dir).unwrap();
    // The forked session's own file does not exist yet; its parent does.
    let fork = dir.join("s1.jsonl");
    working_transcript(&dir.join("parent0.jsonl"), 300_000);
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    let pre = HookEvent { trigger: "manual".into(), transcript_path: fork.to_string_lossy().into_owned(), ..ev("PreCompact") };
    h.handle(&pre);
    let store = MemoryStore::open(&s.store).unwrap();
    let ck = atlas::checkpoint::newest(&store, "s1").unwrap().unwrap();
    assert!(ck.title.contains("at 300000 context tokens"), "{}", ck.title);
    assert!(ck.meta.contains("tail_from:") && ck.meta.contains("parent0.jsonl"), "{}", ck.meta);
    for want in ["files: src/export.rs", "tools: Read src/export.rs -> turn:parent0:4@", "pending: Bash cargo test", "intent: I will read the exporter first."] {
        assert!(ck.body.contains(want), "missing {want:?} in\n{}", ck.body);
    }
    drop(store);
    // Two recent siblings: which one is the parent is unknown, so the card
    // says the transcript was unread instead of claiming 0 context tokens.
    working_transcript(&dir.join("other.jsonl"), 100_000);
    h.handle(&pre);
    let store = MemoryStore::open(&s.store).unwrap();
    let ck2 = atlas::checkpoint::newest(&store, "s1").unwrap().unwrap();
    assert!(ck2.title.contains("transcript unread") && !ck2.title.contains("0 context tokens"), "{}", ck2.title);
    assert!(ck2.body.contains(atlas::checkpoint::UNREAD_LINE), "{}", ck2.body);
    assert!(!ck2.body.contains("files:"), "{}", ck2.body);
    // The store side is still there: the previous checkpoint.
    assert!(ck2.body.starts_with(&format!("since: {}@", ck.key)), "{}", ck2.body);
    drop(store);
    // Written late, within the wait: the session's own file is read.
    let late = dir.join("s2.jsonl");
    let late2 = late.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        working_transcript(&late2, 200_000);
    });
    let (tail, from) = atlas::checkpoint::tail_settled(&late, "s2", atlas::checkpoint::SETTLE_WAIT);
    writer.join().unwrap();
    assert_eq!(from, atlas::checkpoint::TailFrom::Own);
    assert_eq!(tail.last_ctx, 200_000);
}

/// Context selection (`recall.select`): a record whose words the one sent
/// before it holds, at a cosine over `dup_cos`, is sent as a ref to it; a
/// pointer carries the clause that matches the prompt, verbatim; a lane
/// brief keeps its state whole and trims older entries; and with selection
/// off every pointer is as before.
#[test]
fn context_selection_collapses_trims_and_keeps_lane_state_whole() {
    use atlas::hooks::Sel;
    use atlas::memory::vectors;
    let t = Temp::new("hook-select");
    let mut s = settings(t.path());
    std::fs::create_dir_all(s.model.parent().unwrap()).unwrap();
    std::fs::write(&s.model, atlas::embed::tiny_model_bytes(true, 7)).unwrap();
    s.recall.cos_floor = -1.0;
    s.recall.cos_alone = 2.0;
    s.recall.cos_margin = 2.0;
    s.recall.budget_tokens = 600;
    let zebra = "- 2026-11-02 Tess: the opening line of this rule is about the barn roof. The zebra quota is twelve per pen. A closing sentence about the gate paint.";
    {
        let mut store = MemoryStore::open(&s.store).unwrap();
        let mut w = store.writer().unwrap();
        let mut add = |key: &str, kind: &str, scope: &str, sub: &str, title: &str, body: &str| {
            let d = Draft { title: title.into(), body: body.into(), reason: "select test".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
            w.create(key, kind, sub, scope, &d, None).unwrap();
        };
        add("rules:r00011", "rule", "rules", "", "", "- The wombat ledger rotates every ninety days.");
        add("wombat-ledger", "memory", "", "feedback", "- [Wombat ledger](wombat-ledger.md) - rotates every ninety days", "The wombat ledger rotates every ninety days.");
        add("rules:r00012", "rule", "rules", "", "", zebra);
        add("journal:zoo-3:0001", "journal", "zoo-3", "brief", "Count the zebra pens", "Brief body stays whole: every pen, every zebra.");
        add("journal:zoo-3:0002", "journal", "zoo-3", "measurement", "pen census", "Pens one to four hold the herd. The zebra quota held at twelve per pen in every census. Fence posts were repainted on Tuesday.");
        w.commit().unwrap();
        let m = atlas::embed::Model::load(&s.model).unwrap();
        while vectors::embed_pending(&store, &m, 8, 1).unwrap() > 0 {}
    }
    // Collapse by words alone (any cosine), then never (no cosine reaches 1.01).
    s.recall.select.dup_cos = -1.0;
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = h.handle(&HookEvent { prompt: "how often does the wombat ledger rotate, every ninety days?".into(), ..ev("UserPromptSubmit") });
    let lines: Vec<&str> = a.context.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(lines.len(), 2, "{}", a.context);
    let first = if lines[0].starts_with("- rules:r00011:") { "rules:r00011" } else { "wombat-ledger" };
    assert!(lines[1].ends_with(&format!(": near-duplicate of {first}")), "{}", a.context);
    let how: Vec<Sel> = h.last_injected().iter().map(|k| k.how).collect();
    assert!(how.contains(&Sel::Collapsed), "{how:?}");
    s.recall.select.dup_cos = 1.01;
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = h.handle(&HookEvent { prompt: "how often does the wombat ledger rotate, every ninety days?".into(), ..ev("UserPromptSubmit") });
    assert!(!a.context.contains("near-duplicate"), "{}", a.context);
    // Trim: the matching clause after the dated lead, verbatim.
    let a = h.handle(&HookEvent { prompt: "what is the zebra quota per pen?".into(), session_id: "s-trim".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("- rules:r00012: 2026-11-02 Tess: ... The zebra quota is twelve per pen. ...\n"), "{}", a.context);
    // Off: the first line, as before.
    let mut off = s.clone();
    off.recall.select.enabled = false;
    let mut h0 = Hooks::open(off).unwrap();
    let a = h0.handle(&HookEvent { prompt: "what is the zebra quota per pen?".into(), session_id: "s-trim".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("- rules:r00012: 2026-11-02 Tess: the opening line of this rule is about the barn roof. The zebra quota ...
"), "{}", a.context);
    let b = h0.handle(&HookEvent { prompt: "what did zoo-3 count about the zebra quota per pen?".into(), session_id: "s-lane".into(), ..ev("UserPromptSubmit") });
    assert!(b.context.contains("Fence posts were repainted"), "off, the lane entry is whole: {}", b.context);
    // A lane brief: the brief whole, the measurement cut to its matching clause.
    let a = h.handle(&HookEvent { prompt: "what did zoo-3 count about the zebra quota per pen?".into(), session_id: "s-lane".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("Brief body stays whole: every pen, every zebra.\n"), "{}", a.context);
    assert!(a.context.contains("pen census\n... The zebra quota held at twelve per pen in every census. ...\n"), "{}", a.context);
    assert!(!a.context.contains("Fence posts"), "{}", a.context);
    assert!(a.context.contains("+1 trimmed above"), "{}", a.context);
    let lane = h.last_injected().iter().find(|k| k.key == "lane:zoo-3").unwrap();
    assert_eq!(lane.how, Sel::Trimmed);
    assert!(lane.full > lane.bytes, "{lane:?}");
}

/// `atlas --hook` as Claude Code runs it, one process per event: each
/// answer's ledger row is in `token_log` when the process has exited (the
/// log thread is joined, not left to die with the process), and a fresh
/// state folder is left with a whole database, not an empty file beside a
/// hot journal.
#[test]
fn command_hook_rows_reach_the_token_log_before_the_process_exits() {
    let t = Temp::new("hook-cmd-log");
    write(&t.path().join("vault").join("Index.md"), "# Index\n");
    let cfg = t.path().join("atlas.json");
    write(&cfg, r#"{"db": "state/atlas.sqlite", "vaults": [{"name": "notes", "path": "vault"}], "models": {"auto_download": false, "files": []}}"#);
    const RUNS: i64 = 4;
    for i in 0..RUNS {
        let mut child = Command::new(bin())
            .arg("--config")
            .arg(&cfg)
            .arg("--hook")
            .current_dir(t.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ev = json!({"hook_event_name": if i == 0 { "SessionStart" } else { "UserPromptSubmit" }, "prompt": format!("a prompt about nothing in particular {i}"), "session_id": "s-cmd", "cwd": t.path().to_string_lossy().into_owned()});
        child.stdin.take().unwrap().write_all(ev.to_string().as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    }
    let db = t.path().join("state").join("atlas.sqlite");
    assert!(!db.with_file_name("atlas.sqlite-journal").exists(), "a hot journal was left behind");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM token_log WHERE tool LIKE 'hook:%'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, RUNS, "one ledger row per hook answer");
}

/// The hook tool writes journal entries (context caps, checkpoints), so it
/// is not annotated read-only.
#[test]
fn the_hook_tool_is_not_marked_read_only() {
    for list in [atlas::tools::full_tool_list(), atlas::tools::tool_list()] {
        let hook = list.iter().find(|t| t["name"] == json!("hook")).expect("hook is listed");
        assert_ne!(hook["annotations"]["readOnlyHint"], json!(true), "{hook}");
    }
}
