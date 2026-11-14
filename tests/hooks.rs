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

/// [`settings`] with the compaction features on, in a project
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
    // A session of its own: s1 already had both pointed to, and the
    // injected set is read back across processes.
    s.recall.session_budget_tokens = 40;
    let mut h = Hooks::open(s.clone()).unwrap();
    let first = h.handle(&HookEvent { prompt: widget.into(), session_id: "s3".into(), ..ev("UserPromptSubmit") });
    assert!(first.context.contains("widget-cache"), "{}", first.context);
    let second = h.handle(&HookEvent { prompt: gadget.into(), session_id: "s3".into(), ..ev("UserPromptSubmit") });
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
    // Its own lane's view was injected: no pointers beside it.
    assert!(!start.context.contains("atlas recall (related"), "{}", start.context);
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

/// Two more widget-cache records, so one widget prompt passes three.
fn seed_widgets(s: &Settings) {
    let mut store = MemoryStore::open(&s.store).unwrap();
    let mut w = store.writer().unwrap();
    for (key, body) in [
        ("widget-cache-size", "\nThe widget cache holds sixty four widgets and evicts entries past that.\n"),
        ("widget-cache-order", "\nWidget cache eviction takes the least recently used widgets past sixty four entries.\n"),
    ] {
        let d = Draft { title: format!("- [{key}]({key}.md) - widget cache"), body: body.into(), reason: "seeding hook tests".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
        w.create(key, "memory", "feedback", "", &d, None).unwrap();
    }
    drop(w.commit());
}

/// The pointer lines of an injected context.
fn pointer_lines(ctx: &str) -> Vec<&str> {
    ctx.lines().filter(|l| l.starts_with("- ")).collect()
}

const WIDGET: &str = "why does the widget cache evict entries past sixty four widgets";

/// The injected set is read back from `hook_inject`: a new process (a
/// server restart, or a command hook's process per event) after a
/// compaction does not point to a key the session already had, while
/// another session and a subagent keep their own.
#[test]
fn a_restart_or_compaction_keeps_the_injected_set() {
    let t = Temp::new("hook-dedup-restart");
    let s = settings(t.path());
    seed(&s);
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        assert!(h.handle(&HookEvent { prompt: WIDGET.into(), ..ev("UserPromptSubmit") }).context.contains("widget-cache"));
        let brief = "Look into the gadget exporter: it writes with temp files and renames them into place.";
        h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: brief.into(), ..ev("PreToolUse") });
        let start = h.handle(&HookEvent { agent_id: "a1".into(), agent_type: "general-purpose".into(), ..ev("SubagentStart") });
        assert!(start.context.contains("rules:r00001"), "{}", start.context);
    }
    // A restart: the same prompt in the same session points to nothing again.
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        let again = h.handle(&HookEvent { prompt: WIDGET.into(), ..ev("UserPromptSubmit") });
        assert!(!again.context.contains("widget-cache"), "{}", again.context);
    }
    // A compaction, then a restart again.
    let tp = t.path().join("session.jsonl");
    compacted_transcript(&tp, "Work so far: the widget cache.");
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    h.handle(&HookEvent { source: "compact".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("SessionStart") });
    let after = h.handle(&HookEvent { prompt: WIDGET.into(), ..ev("UserPromptSubmit") });
    assert!(!after.context.contains("widget-cache"), "{}", after.context);
    // The subagent's own set came back too: its brief read from its own
    // transcript at its first tool call points to nothing it already had.
    let dir = t.path().join("proj");
    let parent = dir.join("sess.jsonl");
    std::fs::create_dir_all(dir.join("sess").join("subagents")).unwrap();
    std::fs::write(&parent, "").unwrap();
    let brief = "Look into the gadget exporter: it writes with temp files and renames them into place.";
    std::fs::write(dir.join("sess").join("subagents").join("agent-a1.jsonl"), format!("{}\n", json!({"type": "user", "message": {"role": "user", "content": brief}}))).unwrap();
    assert!(h.handle(&HookEvent { agent_id: "a1".into(), ..ev("SubagentStart") }).context.is_empty());
    let pre = h.handle(&HookEvent { agent_id: "a1".into(), tool_name: "Bash".into(), transcript_path: parent.to_string_lossy().into_owned(), ..ev("PreToolUse") });
    assert!(!pre.context.contains("rules:r00001"), "{}", pre.context);
    // A new subagent with the same brief gets it.
    assert!(h.handle(&HookEvent { agent_id: "a2".into(), ..ev("SubagentStart") }).context.is_empty());
    std::fs::write(dir.join("sess").join("subagents").join("agent-a2.jsonl"), format!("{}\n", json!({"type": "user", "message": {"role": "user", "content": brief}}))).unwrap();
    let pre = h.handle(&HookEvent { agent_id: "a2".into(), tool_name: "Bash".into(), transcript_path: parent.to_string_lossy().into_owned(), ..ev("PreToolUse") });
    assert!(pre.context.contains("rules:r00001"), "{}", pre.context);
    // Another session has its own.
    let other = h.handle(&HookEvent { prompt: WIDGET.into(), session_id: "s2".into(), ..ev("UserPromptSubmit") });
    assert!(other.context.contains("widget-cache"), "{}", other.context);
}

/// A human prompt gets at most two pointers; the third is counted in the
/// `+N more` line. A subagent's brief is not capped.
#[test]
fn a_third_pointer_in_one_prompt_is_dropped() {
    let t = Temp::new("hook-pointer-cap");
    let s = settings(t.path());
    seed(&s);
    seed_widgets(&s);
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = h.handle(&HookEvent { prompt: WIDGET.into(), ..ev("UserPromptSubmit") });
    assert_eq!(s.recall.prompt_pointers, 2);
    assert_eq!(pointer_lines(&a.context).len(), s.recall.prompt_pointers, "{}", a.context);
    assert!(a.context.contains("+1 more: recall {query}"), "{}", a.context);
    // The dropped one was not pointed to, so the next prompt may point to it.
    let next = h.handle(&HookEvent { prompt: WIDGET.into(), ..ev("UserPromptSubmit") });
    assert_eq!(pointer_lines(&next.context).len(), 1, "{}", next.context);
    // A brief naming no lane: all three.
    h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: WIDGET.into(), ..ev("PreToolUse") });
    let start = h.handle(&HookEvent { agent_id: "a1".into(), agent_type: "general-purpose".into(), ..ev("SubagentStart") });
    assert_eq!(pointer_lines(&start.context).len(), 3, "{}", start.context);
    // `recall.prompt_pointers: 0` is no cap.
    let mut open = s.clone();
    open.recall.prompt_pointers = 0;
    let mut h = Hooks::open(open).unwrap();
    let all = h.handle(&HookEvent { prompt: WIDGET.into(), session_id: "s9".into(), ..ev("UserPromptSubmit") });
    assert_eq!(pointer_lines(&all.context).len(), 3, "{}", all.context);
}

/// A subagent whose brief names its own lane gets that lane's view and no
/// pointers; the same brief with no lane gets its pointers.
#[test]
fn a_subagent_start_with_an_own_lane_dump_gets_no_pointers() {
    let t = Temp::new("hook-own-lane");
    let s = settings(t.path());
    seed(&s);
    let mut h = Hooks::open(s).unwrap();
    let topic = "Continue the gadget exporter: it writes with temp files and renames them into place.";
    h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: format!("You are build-7. {topic}"), ..ev("PreToolUse") });
    let own = h.handle(&HookEvent { agent_id: "a1".into(), agent_type: "general-purpose".into(), ..ev("SubagentStart") });
    assert!(own.context.contains("lane build-7"), "{}", own.context);
    assert!(pointer_lines(&own.context).iter().all(|l| !l.contains("rules:r00001")), "{}", own.context);
    assert!(!own.context.contains(atlas::hooks::POINTER_HEAD.trim_end()), "{}", own.context);
    h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: topic.into(), ..ev("PreToolUse") });
    let plain = h.handle(&HookEvent { agent_id: "a2".into(), agent_type: "general-purpose".into(), ..ev("SubagentStart") });
    assert!(plain.context.contains("rules:r00001"), "{}", plain.context);
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

/// An `<agent-message` prompt as Claude Code frames a hand-back (shape read
/// from a real transcript 2026-11-11: the frame follows a line of its own).
fn agent_message(report: &str) -> String {
    format!("Another Claude session sent a message:\n<agent-message from=\"a0123456789abcdef\">\n[Subagent hand-back] The text below is the final report of a subagent this session delegated to. The report follows:\n  {report}\n</agent-message>\n\nThat \"other Claude session\" is an agent working inside this same session.")
}

/// 0048: a report gets one head line per lane it names (entries, newest
/// handoff, open questions, the recall that shows the lane), no lane view
/// and no pointers; a report naming no lane gets nothing; a task
/// notification and an ordinary prompt are as before.
#[test]
fn an_agent_message_gets_one_head_line_per_lane_it_names() {
    let t = Temp::new("hook-peer-heads");
    let s = settings(t.path());
    seed(&s);
    {
        let mut store = MemoryStore::open(&s.store).unwrap();
        let mut w = store.writer().unwrap();
        let mut add = |key: &str, kind: &str, sub: &str, scope: &str, title: &str, body: &str| {
            let d = Draft { title: title.into(), body: body.into(), reason: "hook test".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
            w.create(key, kind, sub, scope, &d, None).unwrap();
        };
        add("journal:ALPHA-1:0001", "journal", "brief", "ALPHA-1", "Build the alpha widget exporter", "The gadget exporter writes with temp files.\n".repeat(20).as_str());
        add("journal:ALPHA-1:0002", "journal", "decision", "ALPHA-1", "Temp files then rename", "The gadget exporter renames temp files into place.\n".repeat(20).as_str());
        add("handoff:ALPHA-1:001", "handoff", "", "ALPHA-1", "First handoff", "old");
        add("handoff:ALPHA-1:002", "handoff", "", "ALPHA-1", "Exporter pushed, waits on review of the rename path and two follow-up questions from the reviewer", "the state");
        add("journal:GAMMA-3:0001", "journal", "question", "GAMMA-3", "Which cache bound", "Sixty four or one hundred twenty eight widgets?");
        add("journal:GAMMA-3:0002", "journal", "question", "GAMMA-3", "Which eviction", "LRU or LFU?");
        // A lane the frame's own words name ("session"), not the report.
        add("journal:session:0001", "journal", "note", "session", "A session note", "");
        w.commit().unwrap();
    }
    let mut h = Hooks::open(s.clone()).unwrap();
    let report = agent_message("Done on ALPHA-1 and build-7; GAMMA-3 is blocked. The gadget exporter writes with temp files and renames them into place; the widget cache evicts entries past sixty four widgets.");
    let a = h.handle(&HookEvent { prompt: report.clone(), ..ev("UserPromptSubmit") });
    let lines: Vec<&str> = a.context.lines().collect();
    assert_eq!(lines.len(), 3, "{}", a.context);
    assert!(a.context.len() < 400, "{} B: {}", a.context.len(), a.context);
    assert!(lines.contains(&"lane ALPHA-1: 4 entries, handoff 002 Exporter pushed, waits on review of the rename..., 0 open; recall lane=ALPHA-1"), "{}", a.context);
    assert!(lines.contains(&"lane GAMMA-3: 2 entries, 2 open; recall lane=GAMMA-3"), "{}", a.context);
    assert!(lines.contains(&"lane build-7: 2 entries, 0 open; recall lane=build-7"), "{}", a.context);
    assert!(!a.context.contains("# brief") && !a.context.contains("==") && !a.context.contains("widget-cache"), "{}", a.context);
    // Again in the same session: the head lines again, and still no lane view.
    assert_eq!(h.handle(&HookEvent { prompt: report, ..ev("UserPromptSubmit") }).context, a.context);
    // A report that names no lane: nothing, even when it matches records.
    let none = h.handle(&HookEvent { prompt: agent_message("the widget cache evicts entries past sixty four widgets"), ..ev("UserPromptSubmit") });
    assert_eq!(none.context, "");
    // An ordinary prompt naming the lane still gets the lane view.
    let user = h.handle(&HookEvent { prompt: "what is left on ALPHA-1".into(), ..ev("UserPromptSubmit") });
    assert!(user.context.contains("lane ALPHA-1: 4 entries\n# handoff\n== handoff:ALPHA-1:002"), "{}", user.context);
    // A prompt that only mentions the tag is an ordinary prompt.
    let pasted = h.handle(&HookEvent { prompt: "why does an <agent-message frame skip the GAMMA-3 view".into(), ..ev("UserPromptSubmit") });
    assert!(pasted.context.contains("lane GAMMA-3: 2 entries\n# open questions"), "{}", pasted.context);
    // Two frames in one prompt: the lanes of both reports, logged as head lines.
    let two = format!("{}{}", agent_message("ALPHA-1 is done."), agent_message("GAMMA-3 is blocked."));
    let a = h.handle(&HookEvent { prompt: two, ..ev("UserPromptSubmit") });
    assert_eq!(a.context.lines().count(), 2, "{}", a.context);
    let mut logged: Vec<String> = h.last_injected().iter().map(|k| k.key.clone()).collect();
    logged.sort();
    assert_eq!(logged, vec!["lane-head:ALPHA-1".to_string(), "lane-head:GAMMA-3".to_string()]);
    // A task notification for an unknown task: nothing, as before.
    assert_eq!(h.handle(&HookEvent { prompt: task_note("bqu7y4h25", false), ..ev("UserPromptSubmit") }).context, "");
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
    let mut s = settings(t.path());
    // The per-kind defaults on a 1M window, whatever the environment says.
    // They apply in orchestrated mode only; off, the v0.2 cap applies.
    s.orchestrate = atlas::orchestrate::resolve(None, None, None, |_| None);
    s.orchestrate.enabled = true;
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
    std::fs::write(&sub, usage_line("m1", 100_000) + &usage_line("m2", 286_000)).unwrap();
    let w = call(&mut h, "a9", "Bash");
    assert!(w.deny.is_none() && w.context.contains("286000 tokens"), "{w:?}");
    // Not repeated until another 25k tokens.
    std::fs::write(&sub, usage_line("m2", 286_000) + &usage_line("m3", 300_000)).unwrap();
    assert!(call(&mut h, "a9", "Bash").context.is_empty());
    std::fs::write(&sub, usage_line("m3", 300_000) + &usage_line("m4", 451_000) + &usage_line("m4", 451_000)).unwrap();
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
    assert_eq!(subs, vec!["context cap warned at 286000 tokens", "context cap blocked at 451000 tokens"]);
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

/// A commit between prompts (another connection's journal entry) reads no
/// record vector again; a new record is the one vector read, and a record
/// revised or retired is updated, so the vectors match a fresh load.
#[test]
fn a_commit_between_prompts_reads_only_the_changed_vectors() {
    use atlas::memory::vectors::{self, RecVecs};
    let t = Temp::new("hook-sem-refresh");
    let s = settings(t.path());
    std::fs::create_dir_all(s.model.parent().unwrap()).unwrap();
    std::fs::write(&s.model, atlas::embed::tiny_model_bytes(true, 7)).unwrap();
    seed(&s);
    let m = atlas::embed::Model::load(&s.model).unwrap();
    let mut other = MemoryStore::open(&s.store).unwrap();
    while vectors::embed_pending(&other, &m, 2, 1).unwrap() > 0 {}
    let mut h = Hooks::open(s.clone()).unwrap();
    let prompt = |h: &mut Hooks, n: usize| {
        h.handle(&HookEvent { prompt: format!("widget cache eviction question {n}"), ..ev("UserPromptSubmit") });
        h.vectors_read()
    };
    assert_eq!(prompt(&mut h, 0), 3, "the first prompt reads every vector");
    let draft = |title: &str, body: &str| Draft { title: title.into(), body: body.into(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
    for n in 1..4 {
        let mut w = other.writer().unwrap();
        w.create(&format!("journal:build-7:{:04}", n + 2), "journal", "build-7", "note", &draft("progress", ""), None).unwrap();
        w.commit().unwrap();
        assert_eq!(prompt(&mut h, n), 3, "a journal entry between prompts read the vectors again");
    }
    let mut v = RecVecs::load(&other, &m.id, m.dim).unwrap();
    let mut w = other.writer().unwrap();
    w.create("rules:r00003", "rule", "rules", "", &draft("new rule", "- the sprocket list is sorted.\n"), None).unwrap();
    w.revise("rules:r00002", None, &draft("unrelated", "- the garden is watered on fridays.\n")).unwrap();
    w.retire("rules:r00001", "t").unwrap();
    w.commit().unwrap();
    while vectors::embed_pending(&other, &m, 2, 1).unwrap() > 0 {}
    assert_eq!(prompt(&mut h, 9), 5, "only the new and the revised record are read");
    let fresh = RecVecs::load(&other, &m.id, m.dim).unwrap();
    v.refresh(&other).unwrap();
    assert_eq!(v.read, 2);
    assert_eq!((&v.rids, &v.mat, v.missing), (&fresh.rids, &fresh.mat, fresh.missing));
    assert_eq!(fresh.rids.len(), 3, "the retired rule is gone, the new one is in");
}

/// A seeded store with every recallable record embedded by the tiny model.
fn embedded(tag: &str) -> (Temp, Settings, MemoryStore, atlas::embed::Model) {
    let t = Temp::new(tag);
    let s = settings(t.path());
    std::fs::create_dir_all(s.model.parent().unwrap()).unwrap();
    std::fs::write(&s.model, atlas::embed::tiny_model_bytes(true, 7)).unwrap();
    seed(&s);
    let m = atlas::embed::Model::load(&s.model).unwrap();
    let store = MemoryStore::open(&s.store).unwrap();
    while atlas::memory::vectors::embed_pending(&store, &m, 2, 1).unwrap() > 0 {}
    (t, s, store, m)
}

/// Refreshes `v` and checks it against a fresh load (rows, vectors and the
/// missing count); returns the rows looked up again.
fn refreshed(v: &mut atlas::memory::vectors::RecVecs, store: &MemoryStore, m: &atlas::embed::Model) -> usize {
    v.refresh(store).unwrap();
    let fresh = atlas::memory::vectors::RecVecs::load(store, &m.id, m.dim).unwrap();
    assert_eq!((&v.rids, &v.mat, v.missing), (&fresh.rids, &fresh.mat, fresh.missing));
    v.read
}

fn draft(title: &str, body: &str) -> Draft {
    Draft { title: title.into(), body: body.into(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Draft::default() }
}

/// A record with no vector yet counts as missing and is looked up on every
/// refresh until the embedder stores its vector; then it is read once.
#[test]
fn a_record_missing_its_vector_is_read_once_embedded() {
    use atlas::memory::vectors::{self, RecVecs};
    let (_t, _s, mut store, m) = embedded("vec-missing");
    let mut v = RecVecs::load(&store, &m.id, m.dim).unwrap();
    assert_eq!((v.rids.len(), v.missing), (3, 0));
    let mut w = store.writer().unwrap();
    w.create("rules:r00003", "rule", "rules", "", &draft("new rule", "- the sprocket list is sorted.\n"), None).unwrap();
    w.commit().unwrap();
    assert_eq!(refreshed(&mut v, &store, &m), 1);
    assert_eq!(v.missing, 1);
    assert_eq!(refreshed(&mut v, &store, &m), 1, "still no vector: looked up again");
    while vectors::embed_pending(&store, &m, 2, 1).unwrap() > 0 {}
    assert_eq!(refreshed(&mut v, &store, &m), 1);
    assert_eq!((v.rids.len(), v.missing), (4, 0));
    assert_eq!(refreshed(&mut v, &store, &m), 0, "now kept");
}

/// A retired record drops out, and reactivated it is read again.
#[test]
fn a_reactivated_record_is_read_again() {
    use atlas::memory::vectors::RecVecs;
    let (_t, _s, mut store, m) = embedded("vec-reactivate");
    let mut v = RecVecs::load(&store, &m.id, m.dim).unwrap();
    let mut w = store.writer().unwrap();
    w.retire("rules:r00001", "t").unwrap();
    w.commit().unwrap();
    assert_eq!(refreshed(&mut v, &store, &m), 0);
    assert_eq!(v.rids.len(), 2);
    let mut w = store.writer().unwrap();
    w.reactivate("rules:r00001", "t").unwrap();
    w.commit().unwrap();
    assert_eq!(refreshed(&mut v, &store, &m), 1);
    assert_eq!(v.rids.len(), 3);
}

/// A superseded record drops out and its replacement is read; `prune`
/// deletes only vectors no current version has, so no kept row goes stale.
#[test]
fn a_supersede_and_prune_keep_the_vectors_right() {
    use atlas::memory::vectors::{self, RecVecs};
    let (_t, _s, mut store, m) = embedded("vec-supersede");
    let mut v = RecVecs::load(&store, &m.id, m.dim).unwrap();
    let mut w = store.writer().unwrap();
    w.supersede(&["rules:r00002".to_string()], "rules:r00003", "rule", "rules", "", &draft("garden rule", "- the garden is watered on fridays.\n"), None).unwrap();
    w.revise("rules:r00001", None, &draft("gadget exporter rule", "- the gadget exporter writes to a temp file first.\n")).unwrap();
    w.commit().unwrap();
    while vectors::embed_pending(&store, &m, 2, 1).unwrap() > 0 {}
    assert_eq!(vectors::prune(&store, &m.id).unwrap(), 1, "the revised record's old version");
    assert_eq!(refreshed(&mut v, &store, &m), 2, "only the replacement and the revised record are read");
    assert_eq!(v.rids.len(), 3);
    assert_eq!(vectors::prune(&store, &m.id).unwrap(), 0);
    assert_eq!(refreshed(&mut v, &store, &m), 0);
}

/// A record's kind is fixed in the store (a trigger refuses the update), so
/// a kind change is a supersede by a record of the new kind: to a journal
/// or a block, kinds the gate never injects, the record drops out and
/// nothing is looked up.
#[test]
fn a_kind_change_drops_the_record() {
    use atlas::memory::vectors::RecVecs;
    let (_t, _s, mut store, m) = embedded("vec-kind");
    assert!(store.conn().execute("UPDATE records SET kind = 'journal' WHERE key = 'rules:r00001'", []).is_err(), "a kind never changes in place");
    let mut v = RecVecs::load(&store, &m.id, m.dim).unwrap();
    for (n, (old, key, kind)) in [("rules:r00001", "journal:build-7:0003", "journal"), ("rules:r00002", "layout:b00001", "block")].into_iter().enumerate() {
        let mut w = store.writer().unwrap();
        w.supersede(&[old.to_string()], key, kind, "build-7", "", &draft("moved", "- now a different kind.\n"), None).unwrap();
        w.commit().unwrap();
        assert_eq!(refreshed(&mut v, &store, &m), 0, "{kind}");
        assert_eq!(v.rids.len(), 2 - n, "{kind}");
    }
}

/// `warm()` loads the record vectors, so the first prompt reads none; with
/// no commit since, a store is not stale and no prompt reads any.
#[test]
fn after_warm_and_with_no_commit_a_prompt_reads_no_vector() {
    use atlas::memory::vectors::RecVecs;
    let (_t, s, store, m) = embedded("vec-warm");
    let v = RecVecs::load(&store, &m.id, m.dim).unwrap();
    assert!(!v.stale(&store).unwrap(), "an unchanged store is not stale");
    let mut kept = RecVecs::load(&store, &m.id, m.dim).unwrap();
    assert_eq!(refreshed(&mut kept, &store, &m), 0);
    let mut h = Hooks::open(s.clone()).unwrap();
    h.warm();
    assert_eq!(h.vectors_read(), 3, "the warm-up read them");
    for n in 0..3 {
        h.handle(&HookEvent { prompt: format!("widget cache eviction question {n}"), ..ev("UserPromptSubmit") });
        assert_eq!(h.vectors_read(), 3, "prompt {n} read vectors again");
    }
}

/// The shipped defaults: a 600-token recall budget, a 0.53 cosine floor, 2
/// prompt pointers and the compaction features on. They apply with no
/// configuration and with sections that leave the keys out; a value the user
/// sets always wins.
#[test]
fn the_shipped_defaults_apply_and_an_explicit_value_wins() {
    let t = Temp::new("hook-defaults");
    let db = t.path().join("state").join("index.sqlite");
    let parse = |text: &str| atlas::config::Config::parse(text, t.path()).unwrap();
    let none = Settings::from_config(None, &db).unwrap();
    let empty = parse(r#"{"recall": {"fts_floor": 9.72}, "compact": {"card_tokens": 400}}"#);
    let empty = Settings::from_config(Some(&empty), &db).unwrap();
    for (what, s) in [("no configuration", &none), ("sections without the keys", &empty)] {
        assert_eq!(s.recall.budget_tokens, 600, "{what}");
        assert!((s.recall.cos_floor - 0.53).abs() < 1e-6, "{what}: {}", s.recall.cos_floor);
        assert_eq!(s.recall.prompt_pointers, 2, "{what}");
        assert!(s.compact.enabled, "{what}: compaction is on by default");
    }
    let old = parse(r#"{"recall": {"budget_tokens": 150, "cos_floor": 0.57}, "compact": {"enabled": false}}"#);
    let old = Settings::from_config(Some(&old), &db).unwrap();
    assert_eq!(old.recall.budget_tokens, 150);
    assert!((old.recall.cos_floor - 0.57).abs() < 1e-6, "{}", old.recall.cos_floor);
    assert!(!old.compact.enabled, "an explicit false turns compaction off");
}

/// R6: recall injects pointers (key and title), within the default budget, never a
/// body; and on a new store of ten records the default gate (calibrated on
/// a large store) still recalls the related record and nothing for an
/// unrelated prompt, because the bm25 floor follows the store's size.
#[test]
fn a_ten_record_store_recalls_pointers_with_the_default_gate() {
    use atlas::memory::settings::{floor_scale, DEFAULT_BUDGET_TOKENS};
    assert!((floor_scale(1400) - 1.0).abs() < 1e-9);
    assert!((floor_scale(10) - 0.27).abs() < 0.01, "{}", floor_scale(10));
    assert_eq!(floor_scale(2), 0.0);
    let t = Temp::new("hook-ten");
    let mut s = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    assert_eq!(s.recall.budget_tokens, DEFAULT_BUDGET_TOKENS);
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
    assert!(a.context.len() <= s.bytes_for(DEFAULT_BUDGET_TOKENS), "{}", a.context.len());
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

/// The live miss rows, oldest first: (kind, sig, calls). Read after the
/// `Hooks` is dropped (its drop joins the log thread).
fn miss_rows(db: &Path) -> Vec<(String, String, i64)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut st = conn.prepare("SELECT kind, sig, calls FROM compact_miss ORDER BY id").unwrap();
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
    rows
}

fn legacy_misses(db: &Path) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row("SELECT COUNT(*) FROM token_log WHERE tool = 'compact:miss'", [], |r| r.get(0)).unwrap()
}

fn read_of(p: &Path) -> HookEvent {
    HookEvent { tool_name: "Read".into(), tool_input: json!({"file_path": p.to_string_lossy().into_owned()}), ..ev("PreToolUse") }
}

fn pre_compact() -> HookEvent {
    HookEvent { trigger: "auto".into(), ..ev("PreCompact") }
}

/// The normalized signature a lookup is stored under.
fn sig_of(tool: &str, input: &Value) -> String {
    atlas::compact::lookup_sig(tool, input).unwrap()
}

#[test]
fn a_lookup_made_again_after_a_compaction_is_a_logged_miss() {
    let t = Temp::new("hook-compact-miss");
    let s = compaction(t.path());
    seed(&s);
    let (a, b) = (t.path().join("src").join("a.rs"), t.path().join("src").join("b.rs"));
    write(&a, "fn secret_body_of_a() {}\n");
    write(&b, "fn b() {}\n");
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&read_of(&a));
        h.handle(&read_of(&a));
        h.handle(&pre_compact());
        h.handle(&read_of(&a));
        h.handle(&read_of(&b));
        // A subagent's lookups are not the session's.
        h.handle(&HookEvent { agent_id: "x".into(), ..read_of(&a) });
    }
    // The old series is unchanged: one row per repeat.
    assert_eq!(legacy_misses(&s.index_db), 1);
    let rows = miss_rows(&s.index_db);
    assert_eq!(rows, [("miss".to_string(), sig_of("Read", &read_of(&a).tool_input), 1), ("lookup".to_string(), String::new(), 2)]);
    // The target only: never the file's contents.
    assert!(rows.iter().all(|r| !r.1.contains("secret_body_of_a")), "{rows:?}");
}

#[test]
fn a_repeat_of_a_changed_file_is_not_a_miss() {
    let t = Temp::new("hook-compact-changed");
    let s = compaction(t.path());
    seed(&s);
    let (a, b, gone) = (t.path().join("a.rs"), t.path().join("b.rs"), t.path().join("gone.rs"));
    write(&a, "one\n");
    write(&b, "two\n");
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&read_of(&a));
        h.handle(&read_of(&b));
        h.handle(&read_of(&gone));
        h.handle(&pre_compact());
        write(&a, "one, and a longer line since\n");
        h.handle(&read_of(&a));
        h.handle(&read_of(&b));
        // Absent then and absent now: unchanged.
        h.handle(&read_of(&gone));
    }
    let kinds: Vec<String> = miss_rows(&s.index_db).into_iter().map(|r| r.0).collect();
    assert_eq!(kinds, ["changed", "miss", "miss"]);
    // The legacy series counts every repeat, changed or not.
    assert_eq!(legacy_misses(&s.index_db), 3);
}

#[test]
fn a_repeat_after_the_window_counts_only_in_the_uncapped_figure() {
    let t = Temp::new("hook-compact-window");
    let s = compaction(t.path());
    seed(&s);
    let (a, b) = (t.path().join("a.rs"), t.path().join("b.rs"));
    write(&a, "a\n");
    write(&b, "b\n");
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&read_of(&a));
        h.handle(&read_of(&b));
        h.handle(&pre_compact());
        h.handle(&read_of(&b));
        for _ in 0..39 {
            h.handle(&HookEvent { tool_name: "TodoWrite".into(), tool_input: json!({"todos": []}), ..ev("PreToolUse") });
        }
        h.handle(&read_of(&a));
    }
    let rows = miss_rows(&s.index_db);
    assert_eq!(rows.iter().map(|r| (r.0.as_str(), r.2)).collect::<Vec<_>>(), [("miss", 1), ("miss", 41)]);
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let tot = atlas::compact::totals(&conn, 4.0).unwrap().unwrap();
    assert_eq!((tot.window_lookups, tot.window_repeats, tot.window_misses, tot.stamped_misses), (1, 1, 1, 2));
}

#[test]
fn a_shell_repeat_is_unstamped_and_stores_no_command_text() {
    let t = Temp::new("hook-compact-shell");
    let s = compaction(t.path());
    seed(&s);
    let bash = |c: &str| HookEvent { tool_name: "Bash".into(), tool_input: json!({"command": c}), ..ev("PreToolUse") };
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&bash("gh pr view 145 --json state"));
        h.handle(&HookEvent { tool_name: "PowerShell".into(), tool_input: json!({"command": "Get-Content notes.txt"}), ..ev("PreToolUse") });
        h.handle(&pre_compact());
        h.handle(&bash("gh  pr view 145   --json state"));
        h.handle(&bash("gh pr view 146 --json state"));
        h.handle(&HookEvent { tool_name: "PowerShell".into(), tool_input: json!({"command": "Get-Content notes.txt"}), ..ev("PreToolUse") });
    }
    let rows = miss_rows(&s.index_db);
    assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["unstamped", "unstamped"], "{rows:?}");
    assert!(rows[0].1.starts_with("$ gh pr #"), "{rows:?}");
    assert!(rows[1].1.starts_with("$ Get-Content notes.txt #"), "{rows:?}");
    let head = rows[0].1.split_once(" #").unwrap().0;
    assert_eq!(head, "$ gh pr", "{rows:?}");
    // No legacy row: a shell command was never a lookup.
    assert_eq!(legacy_misses(&s.index_db), 0);
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let tot = atlas::compact::totals(&conn, 4.0).unwrap().unwrap();
    assert_eq!((tot.window_lookups, tot.window_repeats, tot.unstamped, tot.unstamped_shell), (0, 0, 2, 2));
}

#[test]
fn a_subagent_lookup_never_counts() {
    let t = Temp::new("hook-compact-sub");
    let s = compaction(t.path());
    seed(&s);
    let (a, b) = (t.path().join("a.rs"), t.path().join("b.rs"));
    write(&a, "a\n");
    write(&b, "b\n");
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&read_of(&a));
        h.handle(&HookEvent { agent_id: "x".into(), ..read_of(&b) });
        h.handle(&pre_compact());
        for _ in 0..5 {
            h.handle(&HookEvent { agent_id: "x".into(), ..read_of(&b) });
            h.handle(&HookEvent { agent_id: "x".into(), ..read_of(&a) });
        }
        h.handle(&read_of(&a));
    }
    // Only the main session's repeat, at its first call since the boundary.
    assert_eq!(miss_rows(&s.index_db).iter().map(|r| (r.0.as_str(), r.2)).collect::<Vec<_>>(), [("miss", 1)]);
}

#[test]
fn a_recall_of_a_record_that_changed_since_is_not_a_miss() {
    let t = Temp::new("hook-compact-recall");
    let s = compaction(t.path());
    seed(&s);
    let recall = |k: &str| HookEvent { tool_name: "mcp__atlas__recall".into(), tool_input: json!({"key": k}), ..ev("PreToolUse") };
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&recall("widget-cache"));
        h.handle(&recall("rules:r00001"));
        h.handle(&HookEvent { tool_name: "mcp__atlas__search".into(), tool_input: json!({"query": "widget"}), ..ev("PreToolUse") });
        h.handle(&pre_compact());
        let mut store = MemoryStore::open(&s.store).unwrap();
        let mut w = store.writer().unwrap();
        let d = Draft { title: "- [Widget cache](widget-cache.md) - bound".into(), body: "\nNinety widgets now.\n".into(), reason: "miss test".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
        w.revise("widget-cache", None, &d).unwrap();
        w.commit().unwrap();
        drop(store);
        h.handle(&recall("widget-cache"));
        h.handle(&recall("rules:r00001"));
        // An atlas call without a key has no stamp.
        h.handle(&HookEvent { tool_name: "mcp__atlas__search".into(), tool_input: json!({"query": "widget"}), ..ev("PreToolUse") });
    }
    let kinds: Vec<String> = miss_rows(&s.index_db).into_iter().map(|r| r.0).collect();
    assert_eq!(kinds, ["changed", "miss", "unstamped"]);
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let tot = atlas::compact::totals(&conn, 4.0).unwrap().unwrap();
    // The replay's rule counts all three; the stamped figure only the one.
    assert_eq!((tot.window_lookups, tot.window_repeats, tot.window_misses, tot.changed_targets, tot.unstamped_shell), (3, 3, 1, 1, 0));
}

#[test]
fn a_folder_search_repeat_is_changed_after_an_edit_or_a_shell_under_it() {
    let t = Temp::new("hook-compact-grep");
    let s = compaction(t.path());
    seed(&s);
    let (d, e) = (t.path().join("d"), t.path().join("e"));
    write(&d.join("x.rs"), "x\n");
    write(&e.join("y.rs"), "y\n");
    let grep = |p: &Path| HookEvent { tool_name: "Grep".into(), tool_input: json!({"pattern": "fn", "path": p.to_string_lossy().into_owned()}), ..ev("PreToolUse") };
    let glob = HookEvent { tool_name: "Glob".into(), tool_input: json!({"pattern": "**/*.rs"}), cwd: e.to_string_lossy().into_owned(), ..ev("PreToolUse") };
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&grep(&d));
        h.handle(&grep(&e));
        h.handle(&glob);
        h.handle(&pre_compact());
        h.handle(&HookEvent { tool_name: "Edit".into(), tool_input: json!({"file_path": d.join("x.rs").to_string_lossy().into_owned(), "old_string": "x", "new_string": "z"}), ..ev("PreToolUse") });
        h.handle(&grep(&d));
        h.handle(&grep(&e));
        h.handle(&glob);
        h.handle(&HookEvent { tool_name: "Bash".into(), tool_input: json!({"command": "cargo fmt"}), ..ev("PreToolUse") });
        h.handle(&grep(&e));
    }
    let kinds: Vec<String> = miss_rows(&s.index_db).into_iter().map(|r| r.0).collect();
    assert_eq!(kinds, ["changed", "miss", "miss", "changed"]);
}

/// A synthetic session segment: a prompt, a read with its result, an edit,
/// then a running tool call (no result yet), the last of it at a time
/// after anything a test seeds (the session is still running). Nothing here
/// is a real transcript.
fn working_transcript(path: &Path, ctx: u64) {
    working_transcript_until(path, ctx, "2099-12-31T23:59:59.000Z");
}

/// The byte offset of 1-based line `line` of a file: where a checkpoint's
/// byte ref (`turn:<s8>:b<offset>`) points.
fn offset_of_line(path: &Path, line: usize) -> u64 {
    let text = std::fs::read(path).unwrap();
    text.split_inclusive(|&b| b == b'\n').take(line - 1).map(|l| l.len() as u64).sum()
}

/// [`working_transcript`] whose newest record is at `end`.
fn working_transcript_until(path: &Path, ctx: u64, end: &str) {
    let rec = |kind: &str, id: &str, content: atlas::json::Value, c: u64| {
        let time = if id == "m4" { end } else { "2020-01-01T00:00:00.000Z" };
        json!({"type": kind, "timestamp": time, "message": {"id": id, "usage": {"input_tokens": c}, "content": content}})
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

/// Cost review item 2: the card is bounded by the transcript's newest
/// record, so it never lists a record newer than anything the session has
/// seen. (In the compact-live harness the newest record is the harness's
/// own `/goal clear`, so there the per-job store prune does the bounding.)
#[test]
fn the_card_never_lists_records_newer_than_the_transcript() {
    let t = Temp::new("hook-card-asof");
    let mut s = settings(t.path());
    s.compact.enabled = true;
    seed(&s);
    let tp = t.path().join("s1.jsonl");
    working_transcript_until(&tp, 300_000, "2020-01-01T00:00:09.000Z");
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    h.handle(&HookEvent { trigger: "manual".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") });
    let store = MemoryStore::open(&s.store).unwrap();
    let ck = atlas::checkpoint::newest(&store, "s1").unwrap().expect("a checkpoint");
    assert!(!ck.body.contains("build-7") && !ck.body.contains("rules:r00001"), "records seeded after the cut leak in:\n{}", ck.body);
    assert!(ck.body.contains("files: src/export.rs"), "the transcript side stays: {}", ck.body);
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
        "groups (recall a key for its digest and text):\n- turn:s1:b0-b".to_string(),
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
    // The card's group key resolves from the project folder, and its reply
    // holds the read's result.
    let key = atlas::chain::written_keys(&ck.body).into_iter().find(|k| k.starts_with("turn:s1:b0-b")).expect("a group key");
    let reply = atlas::chain::resolve(&dir, &key).unwrap();
    assert!(reply.contains("fn export() {}"), "{reply}");
    let b = offset_of_line(&tp, 4);
    drop(store);
    // A summary citing it (by byte offset, or by line as the index names it)
    // resolves; a wrong hash, or an offset inside a record, is dangling.
    let after = dir.join("after.jsonl");
    let cited = format!("read turn:s1:b{b}@{h}, turn:s1:4@{h}, turn:s1:b{b}@0123456789ab and turn:s1:b{}@{h}", b + 1, h = turn.hash);
    compacted_transcript(&after, &cited);
    let a = h.handle(&HookEvent { source: "compact".into(), transcript_path: after.to_string_lossy().into_owned(), ..ev("SessionStart") });
    assert!(a.context.contains(&format!("dangling: turn:s1:b{b}@0123456789ab")), "{}", a.context);
    assert!(a.context.contains(&format!("dangling: turn:s1:b{}@{}", b + 1, turn.hash)), "{}", a.context);
    assert!(!a.context.contains(&format!("dangling: turn:s1:b{b}@{}", turn.hash)), "{}", a.context);
    assert!(!a.context.contains(&format!("dangling: turn:s1:4@{}", turn.hash)), "{}", a.context);
}

#[test]
fn with_compaction_off_the_hooks_write_and_inject_nothing_for_it() {
    let t = Temp::new("hook-compact-off");
    let c = atlas::config::Config::parse(r#"{"compact": {"enabled": false}}"#, t.path()).unwrap();
    let mut s = Settings::from_config(Some(&c), &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    assert!(!s.compact.enabled, "an explicit false turns compaction off");
    seed(&s);
    let tp = t.path().join("s1.jsonl");
    working_transcript(&tp, 300_000);
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    for trigger in ["auto", "manual"] {
        let a = h.handle(&HookEvent { trigger: trigger.into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") });
        assert!(a.block.is_none() && a.to_json("PreCompact").is_empty(), "{trigger}");
    }
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
    let mut s2 = s.clone();
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
    for want in ["files: src/export.rs", "\n- turn:pare:b0-b", "pending: Bash cargo test", "intent: I will read the exporter first."] {
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
    // Every `Hooks` below asks in session ids of its own. A `Hooks` meeting
    // a session for the first time starts from the keys already pointed to
    // in it, read from `hook_inject` (src/hooks.rs, the `injected_keys`
    // seed in the selection loop), and those rows are written by the other
    // live `Hooks`' log thread at a time of its own (it is joined only on
    // drop). A session shared between two live `Hooks` made the second
    // answer depend on that thread: empty when the row had landed, whole
    // when it had not.
    // Collapse by words alone (any cosine), then never (no cosine reaches 1.01).
    s.recall.select.dup_cos = -1.0;
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = h.handle(&HookEvent {
        prompt: "how often does the wombat ledger rotate, every ninety days?".into(),
        session_id: "s-dup".into(),
        ..ev("UserPromptSubmit")
    });
    let lines: Vec<&str> = a.context.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(lines.len(), 2, "{}", a.context);
    // The memory ranks above the rule, so it is sent first and the rule
    // (whose words it holds) collapses into it.
    assert!(lines[0].starts_with("- wombat-ledger: "), "{}", a.context);
    assert!(lines[1].starts_with("- rules:r00011:"), "{}", a.context);
    assert!(lines[1].ends_with(": near-duplicate of wombat-ledger"), "{}", a.context);
    let how: Vec<Sel> = h.last_injected().iter().map(|k| k.how).collect();
    assert!(how.contains(&Sel::Collapsed), "{how:?}");
    s.recall.select.dup_cos = 1.01;
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = h.handle(&HookEvent {
        prompt: "how often does the wombat ledger rotate, every ninety days?".into(),
        session_id: "s-nodup".into(),
        ..ev("UserPromptSubmit")
    });
    let lines: Vec<&str> = a.context.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(lines.len(), 2, "both sent, neither collapsed: {}", a.context);
    assert!(!a.context.contains("near-duplicate"), "{}", a.context);
    // Trim: the matching clause after the dated lead, verbatim.
    let a = h.handle(&HookEvent { prompt: "what is the zebra quota per pen?".into(), session_id: "s-trim".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("- rules:r00012: 2026-11-02 Tess: ... The zebra quota is twelve per pen. ...\n"), "{}", a.context);
    // Off: the first line, as before.
    let mut off = s.clone();
    off.recall.select.enabled = false;
    let mut h0 = Hooks::open(off).unwrap();
    let a = h0.handle(&HookEvent {
        prompt: "what is the zebra quota per pen?".into(),
        session_id: "s-off-trim".into(),
        ..ev("UserPromptSubmit")
    });
    assert!(a.context.contains("- rules:r00012: 2026-11-02 Tess: the opening line of this rule is about the barn roof. The zebra quota ...
"), "{}", a.context);
    let b = h0.handle(&HookEvent {
        prompt: "what did zoo-3 count about the zebra quota per pen?".into(),
        session_id: "s-off-lane".into(),
        ..ev("UserPromptSubmit")
    });
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
            .env_remove("CLAUDE_CODE_SESSION_ATTENDED")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
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

/// What the real binary answers a PreCompact with, on each form Claude Code
/// can run it by: (command hook's stdout, `hook` tool's reply content).
fn precompact_replies(dir: &Path, cfg: &Path, tp: &Path, trigger: &str) -> (String, Value) {
    let event = |name: &str| json!({name: "PreCompact", "session_id": "s-pre", "trigger": trigger, "transcript_path": tp.to_string_lossy().into_owned(), "cwd": dir.to_string_lossy().into_owned()});
    let spawn = |a: &[&str]| Command::new(bin()).arg("--config").arg(cfg).args(a).current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut child = spawn(&["--hook"]);
    child.stdin.take().unwrap().write_all(event("hook_event_name").to_string().as_bytes()).unwrap();
    let stdout = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    let mut child = spawn(&[]);
    {
        let mut i = child.stdin.take().unwrap();
        writeln!(i, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "t"}}})).unwrap();
        writeln!(i, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "hook", "arguments": event("event")}})).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let replies: Vec<Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| json::parse(l).unwrap()).collect();
    let reply = replies.iter().find(|r| r["id"].as_i64() == Some(2)).unwrap();
    (stdout, reply["result"]["content"].clone())
}

/// `compact.enabled` on, or left out (on by default): both forms and both
/// triggers reply with the instructions and nothing around them, since all of
/// the reply lands in the summarizer's instructions. An explicit false: both
/// reply with no text.
#[test]
fn precompact_replies_with_the_plain_instructions_only_when_compaction_is_on() {
    let t = Temp::new("hook-pre-forms");
    write(&t.path().join("vault").join("Index.md"), "# Index\n");
    let tp = t.path().join("s-pre.jsonl");
    working_transcript(&tp, 300_000);
    // `compact` as written: Some(section) or no section at all.
    let cfg = |name: &str, compact: Option<Value>| {
        let c = t.path().join(name);
        let mut v = json!({"db": format!("state-{}/atlas.sqlite", name.trim_end_matches(".json")), "vaults": [{"name": "notes", "path": "vault"}], "watch": false, "models": {"auto_download": false, "files": []}});
        if let Some(x) = compact {
            v.as_object_mut().unwrap().insert("compact".into(), x);
        }
        write(&c, &format!("{v}"));
        c
    };
    let text = atlas::claude_settings::COMPACT_INSTRUCTIONS;
    assert!(!text.contains("<!--") && !text.contains("##") && text == text.trim() && !text.starts_with('{'), "{text}");
    let on = cfg("on.json", Some(json!({"enabled": true})));
    let off = cfg("off.json", Some(json!({"enabled": false})));
    // On by default: no `compact` section, or one without `enabled`.
    let absent = cfg("absent.json", None);
    let bare = cfg("bare.json", Some(json!({"card_tokens": 400})));
    for trigger in ["manual", "auto"] {
        for c in [&on, &absent, &bare] {
            let (stdout, content) = precompact_replies(t.path(), c, &tp, trigger);
            assert_eq!(stdout, text, "{trigger} {}", c.display());
            assert_eq!(content, json!([{"type": "text", "text": text}]), "{trigger} {}", c.display());
        }
        let (stdout, content) = precompact_replies(t.path(), &off, &tp, trigger);
        assert_eq!(stdout, "", "{trigger}");
        assert_eq!(content, json!([{"type": "text", "text": ""}]), "{trigger}");
    }
}

/// The instructions tell the summarizer to leave out what the card lists, so
/// with no card behind them the hook returns none: the checkpoint failed, or
/// it holds no chain card (a transcript atlas could not read).
#[test]
fn precompact_returns_no_instructions_without_a_card() {
    let t = Temp::new("hook-pre-fail");
    let s = compaction(t.path());
    seed(&s);
    let mut h = Hooks::open(s.clone()).unwrap();
    let missing = t.path().join("none").join("x.jsonl").to_string_lossy().into_owned();
    let a = h.handle(&HookEvent { trigger: "manual".into(), transcript_path: missing, ..ev("PreCompact") });
    assert!(a.block.is_none() && a.to_json("PreCompact").is_empty());
    let store = MemoryStore::open(&s.store).unwrap();
    assert!(atlas::checkpoint::newest(&store, "s1").unwrap().is_some(), "the checkpoint is still written");
    drop(store);

    let tp = t.path().join("s1.jsonl");
    working_transcript(&tp, 300_000);
    rusqlite::Connection::open(&s.store).unwrap().execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE records").unwrap();
    let a = h.handle(&HookEvent { trigger: "manual".into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("PreCompact") });
    assert!(a.block.is_none() && a.to_json("PreCompact").is_empty());
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

/// A tailed failure is said once. A routed result says it itself (the
/// `Exit code nonzero` prefix, the digest's `exit nonzero`), so it gets no
/// context line; a result routing failed on reaches the model raw, with
/// only the marker, and gets one (issue bloodfeast/atlas#3).
#[test]
fn a_tailed_failure_is_said_in_the_context_only_when_routing_failed() {
    let t = Temp::new("h-exit");
    let cmd = atlas::hook_rewrite::rewrite("Bash", "git log -1").unwrap();
    let post = |stdout: &str| HookEvent {
        event: "PostToolUse".into(),
        session_id: "s1".into(),
        tool_name: "Bash".into(),
        tool_input: json!({"command": cmd}),
        tool_response: json!({"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false}),
        ..HookEvent::default()
    };
    let mark = atlas::hook_rewrite::EXIT_MARK;
    let big = format!("{}{mark}\n", "fatal: bad object\n".repeat(4000));
    let mut h = Hooks::open(settings(t.path())).unwrap();
    let small = h.handle(&post(&format!("fatal: bad revision\n{mark}\n")));
    assert!(small.context.is_empty() && small.to_json("PostToolUse").contains("Exit code nonzero"), "{}", small.to_json("PostToolUse"));
    let digested = h.handle(&post(&big));
    assert!(digested.context.is_empty() && digested.to_json("PostToolUse").contains("exit nonzero"), "{}", digested.to_json("PostToolUse"));
    // Handles that cannot open: routing fails and the result passes raw.
    let mut s = settings(t.path());
    std::fs::write(t.path().join("blocker"), "a file, not a folder").unwrap();
    s.index_db = t.path().join("blocker").join("index.sqlite");
    let mut h = Hooks::open(s).unwrap();
    let raw = h.handle(&post(&big));
    assert_eq!(raw.updated_output, None);
    assert_eq!(raw.context, "atlas: exit nonzero");
    assert!(raw.to_json("PostToolUse").contains(r#""additionalContext":"atlas: exit nonzero""#), "{}", raw.to_json("PostToolUse"));
    // A success, a marker inside the output, or no tail: no line.
    let long_ok = "commit abc\n".repeat(4000);
    assert!(h.handle(&post(&long_ok)).context.is_empty());
    assert!(h.handle(&post(&format!("{long_ok}a line naming {mark} here\n"))).context.is_empty());
    let untailed = HookEvent { tool_input: json!({"command": "git log -1"}), ..post(&big) };
    assert!(h.handle(&untailed).context.is_empty());
}

#[test]
fn a_new_or_cleared_session_gets_the_resume_card() {
    let t = Temp::new("hook-resume");
    let s = settings(t.path());
    let tp = t.path().join("sessions").join("new.jsonl");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    let startup = |h: &mut Hooks, source: &str| h.handle(&HookEvent { source: source.into(), transcript_path: tp.to_string_lossy().into_owned(), ..ev("SessionStart") }).context;
    // An empty store and no previous session: no card.
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.set_offer_paths(None);
        assert!(!startup(&mut h, "startup").contains("resume card"));
    }
    {
        let mut store = MemoryStore::open(&s.store).unwrap();
        let mut w = store.writer().unwrap();
        let mut add = |key: &str, kind: &str, scope: &str, sub: &str, title: &str| {
            let d = Draft { title: title.into(), reason: "seeding".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
            w.create(key, kind, sub, scope, &d, None).unwrap();
        };
        add("journal:build-7:0001", "journal", "build-7", "brief", "Build the gadget exporter");
        add("journal:build-7:0002", "journal", "build-7", "question", "Which format does the exporter write?");
        add("journal:build-7:0003", "journal", "build-7", "question", "Answered already?");
        add("journal:build-7:0004", "journal", "build-7", "decision", "Yes");
        add("handoff:build-7:001", "handoff", "build-7", "", "Exporter half done");
        add("journal:session:abc:0001", "journal", "session:abc", "checkpoint", "a checkpoint");
        w.add_refs("journal:build-7:0004", &[("answers".into(), "journal:build-7:0003".into())]).unwrap();
        drop(w.commit());
        let launch = atlas::queue::Launch { kind: atlas::orchestrate::Kind::Builder, lane: Some("build-7"), description: "finish the exporter", subagent_type: "", prompt: "Lane: build-7\nFinish the exporter.", session: "s0", class_meta: "" };
        atlas::queue::add(&mut store, &launch).unwrap();
    }
    // The previous session, with an unmet goal.
    let prev = t.path().join("sessions").join("old.jsonl");
    std::fs::write(&prev, "{\"type\":\"attachment\",\"attachment\":{\"type\":\"goal_status\",\"met\":false,\"sentinel\":true,\"condition\":\"Ship the exporter\"}}\n").unwrap();
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    let card = startup(&mut h, "clear");
    assert!(card.contains("atlas resume card"), "{card}");
    assert!(card.contains("build-7 (1 question, handoff:build-7:001)"), "{card}");
    assert!(card.contains("- open questions for the user: journal:build-7:0002\n"), "{card}");
    assert!(!card.contains("journal:build-7:0003"), "an answered question is not open: {card}");
    assert!(!card.contains("session:abc"), "a session lane is not a live lane: {card}");
    assert!(card.contains("- work queue: 1 waiting"), "{card}");
    assert!(!card.contains("other handoffs"), "the lanes line names the only handoff: {card}");
    assert!(card.contains("not met") && card.contains("Ship the exporter"), "{card}");
    assert!(card.contains("(1) resume the work queue, (2) change it first, or (3) do something more pressing"), "{card}");
    assert!(card.len() <= atlas::resume::CARD_BYTES, "{} bytes", card.len());
    // Compaction keeps its own card; a resumed session needs none.
    assert!(!startup(&mut h, "compact").contains("resume card"));
    assert!(!startup(&mut h, "resume").contains("resume card"));
    // The switch.
    let mut off = s.clone();
    off.resume_card = false;
    let mut h = Hooks::open(off).unwrap();
    h.set_offer_paths(None);
    assert!(!startup(&mut h, "startup").contains("resume card"));
}

#[test]
fn the_task_judge_note_is_advisory_and_logged() {
    let t = Temp::new("hook-judge");
    let mut s = settings(t.path());
    let cells = t.path().join("cells.json");
    std::fs::write(
        &cells,
        r#"{"cells": [{"key": {"kind": null, "has_brief": null}, "scope": "trivial", "unk": "none"},
            {"key": {"kind": "builder", "has_brief": true}, "scope": "large", "unk": "several"}]}"#,
    )
    .unwrap();
    s.orchestrate.judge = atlas::taskjudge::JudgeSettings { enabled: true, cells: Some(cells.clone()), ..Default::default() };
    let mut h = Hooks::open(s.clone()).unwrap();
    let brief = "Kind: builder\nJudge: scope small, unknowns few -> sonnet, medium\nRead Z:/work/BRIEF_GADGET.md in full.";
    let input = json!({"prompt": brief, "description": "gadget exporter", "model": "sonnet"});
    let pre = h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: brief.into(), tool_input: input.clone(), tool_use_id: "tu1".into(), ..ev("PreToolUse") });
    assert!(pre.deny.is_none() && pre.updated_input.is_none(), "advisory only: {pre:?}");
    assert!(pre.context.contains("atlas judge (advisory): the launch asks sonnet and its Judge line says sonnet, medium; the pick is opus, high"), "{}", pre.context);
    let log = std::fs::read_to_string(s.store.parent().unwrap().join("judge-picks.jsonl")).unwrap();
    let row = json::parse(log.lines().last().unwrap()).unwrap();
    assert_eq!((row["tool_use_id"].as_str(), row["model"].as_str(), row["asked_model"].as_str()), (Some("tu1"), Some("opus"), Some("sonnet")));
    // A launch that asks for the pick gets no line; a missing cells file
    // gives none and denies nothing.
    let agree = "Kind: builder\nRead Z:/work/BRIEF_GADGET.md in full.";
    let pre = h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: agree.into(), tool_input: json!({"prompt": agree, "model": "opus"}), tool_use_id: "tu2".into(), ..ev("PreToolUse") });
    assert!(!pre.context.contains("atlas judge"), "{}", pre.context);
    std::fs::remove_file(&cells).unwrap();
    let pre = h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: brief.into(), tool_input: input, tool_use_id: "tu3".into(), ..ev("PreToolUse") });
    assert!(pre.deny.is_none() && !pre.context.contains("atlas judge"), "{pre:?}");
}

#[test]
fn the_work_classifier_is_off_by_default_and_advisory_when_on() {
    use atlas::taskjudge::{ClassifierSettings, ScoreSource};
    let t = Temp::new("hook-classifier");
    let s = settings(t.path());
    let brief_file = t.path().join("BRIEF_GADGET.md");
    std::fs::write(&brief_file, "Kind: builder\nBuild the gadget.\n").unwrap();
    let brief = format!("Kind: builder\nRead {} in full.", brief_file.display()).replace('\\', "/");
    let input = json!({"prompt": brief.as_str(), "description": "gadget", "model": "opus"});
    let run = |s: &Settings, id: &str| {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: brief.clone(), tool_input: input.clone(), tool_use_id: id.into(), ..ev("PreToolUse") })
    };
    // Absent and present-but-off answer the same.
    let absent = run(&s, "c1");
    let mut off = s.clone();
    off.orchestrate.judge.classifier = ClassifierSettings { enabled: false, source: ScoreSource::Head, ..Default::default() };
    let present_off = run(&off, "c1");
    assert_eq!(format!("{absent:?}"), format!("{present_off:?}"));
    assert!(!absent.context.contains("Classifier:"), "{}", absent.context);
    // On with the kind source: one advisory line, nothing denied or rewritten.
    let mut on = s.clone();
    on.orchestrate.judge.classifier = ClassifierSettings { enabled: true, source: ScoreSource::Kind, ..Default::default() };
    let pre = run(&on, "c2");
    assert!(pre.deny.is_none() && pre.updated_input.is_none(), "{pre:?}");
    assert!(pre.context.contains("Classifier: demand score 5 (kind) -> opus, high"), "{}", pre.context);
    // On with the head source and a missing weights file: no line, no error.
    let mut bad = s.clone();
    bad.orchestrate.judge.classifier = ClassifierSettings { enabled: true, source: ScoreSource::Head, weights: Some(t.path().join("missing.f32")), ..Default::default() };
    let pre = run(&bad, "c3");
    assert!(pre.deny.is_none() && !pre.context.contains("Classifier:"), "{pre:?}");
}
