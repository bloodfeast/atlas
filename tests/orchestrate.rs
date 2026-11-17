//! Orchestrator mode through the hooks: the per-kind agent caps, the
//! per-kind context limits and their modes, the SendMessage refusal and the
//! fresh-agent line (`docs/design/orchestrate.md`).

mod common;

use std::path::{Path, PathBuf};

use atlas::hooks::{HookAnswer, HookEvent, Hooks};
use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
use atlas::orchestrate::{Fresh, Kind, KindSpec, Mode, OrchestrateSpec};
use common::*;

fn settings(dir: &Path, o: &OrchestrateSpec, env: &'static [(&'static str, &'static str)]) -> Settings {
    let mut s = Settings::from_config(None, &dir.join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.orchestrate = atlas::orchestrate::resolve(Some(o), None, None, |k| env.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_string()));
    s
}

/// A session transcript and its agents' folder.
struct Session {
    parent: PathBuf,
    dir: PathBuf,
}

impl Session {
    fn new(root: &Path) -> Session {
        let parent = root.join("proj").join("sess.jsonl");
        let dir = root.join("proj").join("sess").join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&parent, "").unwrap();
        Session { parent, dir }
    }

    fn tp(&self) -> String {
        self.parent.to_string_lossy().into_owned()
    }

    /// An agent as Claude Code records it: its meta file and transcript
    /// (the brief, then a request of `ctx` context tokens).
    fn agent(&self, id: &str, agent_type: &str, tool_use_id: &str, brief: &str, ctx: u64, finished: bool) {
        let mut meta = json!({"agentType": agent_type, "isBuiltIn": true, "description": format!("job {id}"), "toolUseId": tool_use_id, "model": "opus"});
        if finished {
            if let Value::Object(m) = &mut meta {
                m.insert("finished".into(), Value::Bool(true));
            }
        }
        std::fs::write(self.dir.join(format!("agent-{id}.meta.json")), meta.to_string()).unwrap();
        let user = json!({"type": "user", "message": {"role": "user", "content": brief}});
        let usage = json!({"type": "assistant", "message": {"id": "m1", "usage": {"input_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": ctx - 2, "output_tokens": 0}}});
        std::fs::write(self.dir.join(format!("agent-{id}.jsonl")), format!("{user}\n{usage}\n")).unwrap();
    }
}

fn ev(event: &str) -> HookEvent {
    HookEvent { event: event.into(), session_id: "s1".into(), ..HookEvent::default() }
}

fn launch(h: &mut Hooks, s: &Session, id: &str, subagent_type: &str, prompt: &str) -> HookAnswer {
    let input = json!({"description": "d", "subagent_type": subagent_type, "prompt": prompt});
    h.handle(&HookEvent {
        tool_name: "Agent".into(),
        tool_use_id: id.into(),
        transcript_path: s.tp(),
        tool_prompt: prompt.into(),
        tool_input: input,
        ..ev("PreToolUse")
    })
}

fn finished(h: &mut Hooks, s: &Session, id: &str) -> String {
    let note = format!("<task-notification>\n<task-id>{id}</task-id>\n<status>completed</status>\n<summary>Agent \"x\" finished</summary>\n<note>A task-notification fires each time this agent stops with no live background children of its own.</note>\n<result>r</result>\n</task-notification>");
    h.handle(&HookEvent { prompt: note, transcript_path: s.tp(), ..ev("UserPromptSubmit") }).context
}

#[test]
fn the_agent_cap_refuses_past_each_kind_and_counts_live_agents_only() {
    let t = Temp::new("orch-cap");
    let s = Session::new(t.path());
    let on = OrchestrateSpec { enabled: Some(true), ..Default::default() };
    // An unfinished agent of an earlier Claude Code process: dead, not counted.
    s.agent("old1", "general-purpose", "t-old1", "build", 1000, false);
    s.agent("old2", "general-purpose", "t-old2", "build", 1000, false);
    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut h = Hooks::open(settings(t.path(), &on, &[])).unwrap();
    // File times are coarser than the process clock: a file written at once
    // may read as older than the process.
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Build the exporter.", 1000, false);
    s.agent("b2", "general-purpose", "t-b2", "Build the importer.", 1000, true);
    s.agent("r1", "general-purpose", "t-r1", "Review it.\nKind: reviewer\n", 1000, false);

    // One live builder (b1): a second may start; it then counts at once,
    // before Claude Code writes its meta file, so a third is refused.
    // Admitted, with only the reminder to declare its lane.
    let a = launch(&mut h, &s, "t1", "general-purpose", "Build the parser.");
    assert_eq!(a, HookAnswer { context: a.context.clone(), ..HookAnswer::default() });
    assert!(a.context.starts_with("atlas lanes: this builder's prompt declares no lane"), "{a:?}");
    let d = launch(&mut h, &s, "t2", "general-purpose", "Build the printer.");
    let r = d.deny.clone().expect("the third builder is refused");
    assert!(
        r.contains("2 of 2 builder slots") && r.contains("job b1") && r.contains("launched moments ago") && r.contains("Do not retry"),
        "{r}"
    );
    assert!(d.to_json("PreToolUse").contains("\"permissionDecision\":\"deny\""));
    // The hook never allows: under the cap it says nothing at all.
    assert!(!launch(&mut h, &s, "t3", "Explore", "Find the parser.").to_json("PreToolUse").contains("allow"));
    // Research has one slot (t3 holds it); reviewers two (r1 holds one).
    assert!(launch(&mut h, &s, "t4", "Plan", "Plan it.").deny.is_some_and(|r| r.contains("1 of 1 research")));
    assert!(launch(&mut h, &s, "t5", "general-purpose", "Kind: reviewer\nReview the parser.").deny.is_none());
    assert!(launch(&mut h, &s, "t6", "general-purpose", "- **Kind:** reviewer").deny.is_some_and(|r| r.contains("2 of 2 reviewer")));
    // An uncounted type is never capped by kind.
    assert!(launch(&mut h, &s, "t7", "statusline-setup", "Set it up.").deny.is_none());

    // t1's meta appears (its agent b3 is the builder admitted as t1): still full.
    s.agent("b3", "general-purpose", "t1", "Build the parser.", 1000, false);
    assert!(launch(&mut h, &s, "t8", "general-purpose", "Build more.").deny.is_some());
    // A real finish frees b1's slot even before its meta says so.
    let _ = finished(&mut h, &s, "b1");
    assert!(launch(&mut h, &s, "t9", "general-purpose", "Build more.").deny.is_none());
}

/// A cap refusal names the rule that decided the launch's
/// kind; only a launch no `Kind:` line classified is told where one goes.
#[test]
fn a_cap_refusal_names_where_the_kind_came_from() {
    let t = Temp::new("orch-cap-source");
    let s = Session::new(t.path());
    let one = |k| (k, KindSpec { max: Some(1), ..Default::default() });
    let on = OrchestrateSpec { enabled: Some(true), kinds: vec![one(Kind::Builder), one(Kind::Reviewer), one(Kind::Research)], ..Default::default() };
    let st = settings(t.path(), &on, &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nBuild it.", 1000, false);
    s.agent("r1", "general-purpose", "t-r1", "Kind: reviewer\nReview it.", 1000, false);
    s.agent("e1", "general-purpose", "t-e1", "Kind: research\nFind it.", 1000, false);
    let hint = "put `Kind: builder`, `Kind: reviewer`, `Kind: research` or `Kind: brief-writer` in the prompt's first 40 lines (a brief file the prompt names is not read)";
    let line = launch(&mut h, &s, "t1", "general-purpose", "Kind: reviewer\nRead Z:/brief.md.").deny.unwrap();
    assert!(line.starts_with("atlas agent cap: 1 of 1 reviewer") && line.contains("This launch counts as a reviewer: its prompt's `Kind:` line says so."), "{line}");
    assert!(!line.contains(hint) && !line.contains("add `Queue:"), "{line}");
    let ty = launch(&mut h, &s, "t2", "Explore", "Read Z:/brief.md.").deny.unwrap();
    assert!(
        ty.contains("This launch counts as a research agent: ") && ty.contains("the agent-type map (orchestrate.types) gives its subagent_type `Explore` the kind research") && ty.contains(hint),
        "{ty}"
    );
    let input = json!({"description": "w", "subagent_type": "general-purpose", "isolation": "worktree", "prompt": "Read Z:/brief.md."});
    let wt = h.handle(&HookEvent { tool_name: "Agent".into(), tool_use_id: "t3".into(), transcript_path: s.tp(), tool_prompt: "Read Z:/brief.md.".into(), tool_input: input, ..ev("PreToolUse") });
    let wt = wt.deny.unwrap();
    assert!(wt.contains("an agent with isolation: \"worktree\" counts as a builder") && wt.contains(hint), "{wt}");
    let d = launch(&mut h, &s, "t4", "general-purpose", "Kind: whatever\nRead Z:/brief.md.").deny.unwrap();
    assert!(d.contains("no `Kind:` line naming a kind in its first 40 lines and no other rule classifies it, so orchestrate.default_kind (builder) applies") && d.contains(hint), "{d}");
    // The Explore launch again as a general agent: the same description and
    // brief find its entry, which keeps its kind (no `Queue:` line named it).
    let key = |r: &str| r.split("queued it as ").nth(1).unwrap().split(':').take(3).collect::<Vec<_>>().join(":");
    let k = key(&ty);
    let again = launch(&mut h, &s, "t5", "general-purpose", "Read Z:/brief.md.").deny.unwrap();
    assert_eq!(key(&again), k, "{again}");
    let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
    let w = atlas::queue::waiting(&m).unwrap();
    assert_eq!(w.iter().find(|h| h.key == k).and_then(atlas::queue::kind_of), Some(Kind::Research));
}

/// Q-QU3 with a `Kind:` line: a launch queued under the default kind and
/// relaunched with the advised `Kind:` and `Queue:` lines keeps its entry,
/// which takes the new kind; a relaunch of the same kind leaves the entry
/// as it was; either leaves the queue when it starts.
#[test]
fn a_relaunch_with_a_kind_line_keeps_and_rekinds_its_queue_entry() {
    let t = Temp::new("orch-cap-requeue");
    let s = Session::new(t.path());
    let one = |k| (k, KindSpec { max: Some(1), ..Default::default() });
    let on = OrchestrateSpec { enabled: Some(true), kinds: vec![one(Kind::Builder), one(Kind::Reviewer)], ..Default::default() };
    let st = settings(t.path(), &on, &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nBuild it.", 1000, false);
    s.agent("r1", "general-purpose", "t-r1", "Kind: reviewer\nReview it.", 1000, false);
    let r = launch(&mut h, &s, "t1", "general-purpose", "Review the parser; brief Z:/b.md.").deny.unwrap();
    let key = "journal:queue:0001";
    assert!(
        r.contains(&format!("atlas queued it as {key}")) && r.contains(&format!("If you relaunch it with a `Kind:` line, add `Queue: {key}` too")),
        "{r}"
    );
    let entry = |want_kind: Kind, want_body: &str| {
        let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
        let w = atlas::queue::waiting(&m).unwrap();
        assert_eq!(w.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(), [key]);
        assert_eq!(atlas::queue::kind_of(&w[0]), Some(want_kind));
        assert_eq!(m.head(key).unwrap().unwrap().body, want_body);
    };
    // The relaunch as advised: refused at the reviewer cap, the same entry,
    // now a reviewer with the new brief.
    let again = format!("Kind: reviewer\nQueue: {key}\nReview the parser; brief Z:/b.md.");
    let r = launch(&mut h, &s, "t2", "general-purpose", &again).deny.unwrap();
    assert!(r.contains(&format!("has already queued it as {key}")) && !r.contains("add `Queue:"), "{r}");
    entry(Kind::Reviewer, &again);
    // A relaunch of the same kind by its key: the entry stays as it was.
    let more = format!("{again}\nAlso read the tests.");
    assert!(launch(&mut h, &s, "t3", "general-purpose", &more).deny.unwrap().contains(&format!("has already queued it as {key}")));
    entry(Kind::Reviewer, &again);
    // r1 finishes: the relaunch starts and the entry leaves the queue.
    let _ = finished(&mut h, &s, "r1");
    assert!(launch(&mut h, &s, "t4", "general-purpose", &more).deny.is_none());
    let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
    assert!(atlas::queue::waiting(&m).unwrap().is_empty());
    assert_eq!(m.head(key).unwrap().unwrap().status, "retired");
}

/// A queue refusal is not caused by the kind: it gets no advice on
/// `Kind:` lines, even for a launch no `Kind:` line classified.
#[test]
fn a_queue_refusal_gives_no_kind_advice() {
    let t = Temp::new("orch-cap-no-kind-advice");
    let s = Session::new(t.path());
    let on = OrchestrateSpec { enabled: Some(true), ..Default::default() };
    let mut h = Hooks::open(settings(t.path(), &on, &[])).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: parser\nUses: compile\n\nFix the reader.", 1000, false);
    let r = launch(&mut h, &s, "t1", "general-purpose", "Lane: parser\nUses: compile\n\nFix the writer.").deny.unwrap();
    assert!(r.starts_with("atlas queue") && r.contains("atlas queued it as") && !r.contains("add `Queue:") && !r.contains("Kind:"), "{r}");
}

/// ORCH-RETRO F1, the phantom pending slot (journal ATLAS-V03:0058): a
/// launch that is finally denied never holds a slot, and a launch never
/// counts against itself when its PreToolUse is seen twice.
#[test]
fn a_denied_launch_never_holds_a_slot() {
    let t = Temp::new("orch-phantom");
    let s = Session::new(t.path());
    let on = OrchestrateSpec { enabled: Some(true), ..Default::default() };
    let mut h = Hooks::open(settings(t.path(), &on, &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    // One reviewer of two running, and a builder holding lane parser.
    s.agent("r1", "general-purpose", "t-r1", "Kind: reviewer\nReview it.", 1000, false);
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: parser\nUses: compile\n\nFix the reader.", 1000, false);
    // A builder the queue denies (compile declared on both sides) after the cap
    // let it through ...
    let q = launch(&mut h, &s, "t1", "general-purpose", "Kind: builder\nLane: parser\nUses: compile\n\nFix the writer.");
    assert!(q.deny.as_deref().is_some_and(|r| r.starts_with("atlas queue")), "{q:?}");
    // ... holds no slot: the next builder takes the second one.
    let a = launch(&mut h, &s, "t0", "general-purpose", "Kind: builder\nLane: printer\n\nFix the printer.");
    assert!(a.deny.is_none(), "a queue-denied launch held a builder slot: {a:?}");
    // A review in the builder's lane is no conflict (journal 0095).
    let a = launch(&mut h, &s, "t2", "general-purpose", "Kind: reviewer\nLane: parser\n\nReview the writer.");
    assert!(a.deny.is_none(), "a review waited on a lane: {a:?}");
    // The same launch seen twice (a second PreToolUse of t2) is not
    // refused by its own pending slot.
    let again = launch(&mut h, &s, "t2", "general-purpose", "Kind: reviewer\nLane: parser\n\nReview the writer.");
    assert!(again.deny.is_none(), "a launch counted against itself: {again:?}");
    // A third reviewer is refused: r1 and t2 hold both slots.
    assert!(launch(&mut h, &s, "t3", "general-purpose", "Kind: reviewer\nReview more.").deny.is_some_and(|r| r.contains("2 of 2 reviewer")));
}

/// The same, through `Agents::admit` with its clock: a refused launch
/// retried after the pending TTL is admitted, and a refusal does not
/// re-arm the slot it was refused by.
#[test]
fn a_refused_retry_does_not_re_arm_the_pending_slot() {
    use atlas::orchestrate::{Agents, Launch};
    use std::time::{Duration, Instant, SystemTime};
    let t = Temp::new("orch-phantom-ttl");
    let s = Session::new(t.path());
    let o = atlas::orchestrate::resolve(Some(&OrchestrateSpec { enabled: Some(true), ..Default::default() }), None, None, |_| None);
    let mut agents = Agents::new(Some(SystemTime::now() - Duration::from_secs(5)));
    s.agent("r1", "general-purpose", "t-r1", "Kind: reviewer\nReview it.", 1000, false);
    let tp = s.tp();
    let input = json!({"description": "d", "subagent_type": "general-purpose"});
    let brief = "Kind: reviewer\nReview the writer.";
    let at = |id: &'static str| Launch { transcript: &tp, tool_use_id: id, input: &input, brief, store: None };
    let t0 = Instant::now();
    // t1 takes the second slot; its agent never appears (a launch the
    // harness refused after the hook).
    assert!(agents.admit(&o, &at("t1"), SystemTime::now(), t0).is_none());
    // Refused while t1 is pending, again and again inside the TTL.
    for k in 0..3 {
        assert!(agents.admit(&o, &at("t2"), SystemTime::now(), t0 + Duration::from_secs(30 * k)).is_some());
    }
    // Past the TTL of t1's slot the retry starts: the refusals booked nothing.
    assert!(agents.admit(&o, &at("t2"), SystemTime::now(), t0 + Duration::from_secs(121)).is_none());
}

#[test]
fn the_agent_cap_is_off_by_default_and_clamped_to_claude_codes_limit() {
    let t = Temp::new("orch-limit");
    let s = Session::new(t.path());
    let mut off = Hooks::open(settings(t.path(), &OrchestrateSpec::default(), &[])).unwrap();
    for i in 0..4 {
        assert!(launch(&mut off, &s, &format!("u{i}"), "general-purpose", "Build.").deny.is_none());
    }
    // Claude Code's own limit of 3 clamps the total, whatever the kinds allow.
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![(Kind::Builder, KindSpec { max: Some(9), ..Default::default() })],
        ..Default::default()
    };
    let mut h = Hooks::open(settings(t.path(), &o, &[(atlas::orchestrate::LIMIT_ENV, "3")])).unwrap();
    assert!(launch(&mut h, &s, "a", "general-purpose", "Build.").deny.is_none());
    assert!(launch(&mut h, &s, "b", "general-purpose", "Build.").deny.is_none());
    assert!(launch(&mut h, &s, "c", "general-purpose", "Build.").deny.is_none());
    let r = launch(&mut h, &s, "d", "general-purpose", "Build.").deny.unwrap();
    assert!(r.contains("3 of 3 builder"), "{r}");
    assert!(launch(&mut h, &s, "e", "statusline-setup", "x").deny.is_some_and(|r| r.contains("Claude Code's own limit of 3")));
    // A missing session folder is no evidence: no decision.
    let mut h = Hooks::open(settings(t.path(), &o, &[])).unwrap();
    let none = HookEvent {
        tool_name: "Agent".into(),
        tool_use_id: "z".into(),
        transcript_path: t.path().join("nowhere.jsonl").to_string_lossy().into_owned(),
        tool_input: json!({"prompt": "Build the other one."}),
        ..ev("PreToolUse")
    };
    let a = h.handle(&none);
    assert_eq!(a, HookAnswer { context: a.context.clone(), ..HookAnswer::default() }, "only the lane reminder");
    assert!(a.context.starts_with("atlas lanes:"), "{a:?}");
}

/// F7: a brief writer has its own cap, uncapped by default (Claude Code's
/// own limit still applies), and never takes a builder's slot.
#[test]
fn a_brief_writer_has_its_own_cap_uncapped_by_default() {
    let t = Temp::new("orch-brief-writer");
    let s = Session::new(t.path());
    let on = OrchestrateSpec { enabled: Some(true), ..Default::default() };
    let mut h = Hooks::open(settings(t.path(), &on, &[(atlas::orchestrate::LIMIT_ENV, "6")])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nBuild it.", 1000, false);
    // Four brief writers: past the builder's 2 and research's 1, by either
    // spelling or by subagent_type.
    assert!(launch(&mut h, &s, "w1", "general-purpose", "Kind: brief writer\nWrite Z:/brief.md.").deny.is_none());
    assert!(launch(&mut h, &s, "w2", "brief-writer", "Write Z:/brief2.md.").deny.is_none());
    assert!(launch(&mut h, &s, "w3", "general-purpose", "Kind: brief-writers\nWrite Z:/brief3.md.").deny.is_none());
    assert!(launch(&mut h, &s, "w4", "general-purpose", "- **Kind:** brief-writer\nWrite Z:/brief4.md.").deny.is_none());
    // The builder slot b1 left free is still free: brief writers never count.
    assert!(launch(&mut h, &s, "b2", "general-purpose", "Kind: builder\nLane: X\nBuild more.").deny.is_none());
    // Six running: Claude Code's own limit refuses the next, not a kind cap.
    let r = launch(&mut h, &s, "w5", "general-purpose", "Kind: brief writer\nWrite Z:/brief5.md.").deny.unwrap();
    assert!(r.contains("6 agents are running, Claude Code's own limit of 6") && !r.contains("brief-writer slots"), "{r}");

    // A configured cap is enforced, and 0 launches none.
    for (max, running) in [(1usize, true), (0, false)] {
        let t = Temp::new(&format!("orch-brief-writer-{max}"));
        let s = Session::new(t.path());
        let o = OrchestrateSpec {
            enabled: Some(true),
            kinds: vec![(Kind::BriefWriter, KindSpec { max: Some(max), ..Default::default() })],
            ..Default::default()
        };
        let mut h = Hooks::open(settings(t.path(), &o, &[])).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        if running {
            s.agent("w0", "general-purpose", "t-w0", "Kind: brief writer\nWrite it.", 1000, false);
        }
        let r = launch(&mut h, &s, "w1", "brief-writer", "Write Z:/brief.md.").deny.unwrap();
        assert!(
            r.contains(&format!("{max} of {max} brief-writer slots"))
                && r.contains("orchestrate.kinds.brief-writer.max")
                && r.contains("counts as a brief writer"),
            "{r}"
        );
        // The builders' cap is untouched.
        assert!(launch(&mut h, &s, "b1", "general-purpose", "Kind: builder\nLane: X\nBuild it.").deny.is_none());
    }
}

#[test]
fn context_limits_follow_each_kinds_mode_and_gate_resumes() {
    let t = Temp::new("orch-ctx");
    let s = Session::new(t.path());
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![
            (Kind::Builder, KindSpec { fresh: Some(Fresh::Ask), ..Default::default() }),
            (Kind::Reviewer, KindSpec { mode: Some(Mode::Advisory), ..Default::default() }),
            (Kind::Research, KindSpec { mode: Some(Mode::Off), ..Default::default() }),
        ],
        ..Default::default()
    };
    let mut h = Hooks::open(settings(t.path(), &o, &[])).unwrap();
    s.agent("bld", "general-purpose", "t1", "Build it. Journal lane: PARSER-2.", 460_000, false);
    s.agent("rev", "general-purpose", "t2", "Kind: reviewer", 460_000, false);
    s.agent("res", "Explore", "t3", "Find it.", 460_000, false);
    s.agent("soft", "general-purpose", "t4", "Kind: reviewer", 300_000, false);
    let call = |h: &mut Hooks, agent: &str, tool: &str| {
        h.handle(&HookEvent {
            agent_id: agent.into(),
            agent_type: "general-purpose".into(),
            tool_name: tool.into(),
            transcript_path: s.tp(),
            ..ev("PreToolUse")
        })
    };
    // Enforced: past the hard limit (450k) only the handoff runs.
    let b = call(&mut h, "bld", "Bash");
    assert!(b.deny.as_deref().is_some_and(|r| r.contains("hard limit 450000, builder limits") && r.contains("handoff")), "{b:?}");
    assert!(call(&mut h, "bld", "mcp__plugin_atlas_atlas__handoff").deny.is_none());
    // Advisory: told, never denied.
    let r = call(&mut h, "rev", "Bash");
    assert!(r.deny.is_none() && r.context.contains("advisory") && r.context.contains("460000"), "{r:?}");
    // Off: nothing.
    assert_eq!(call(&mut h, "res", "Bash"), HookAnswer::default());
    // Between the points (285k soft): told to wrap up at a clean stopping point.
    let w = call(&mut h, "soft", "Bash");
    assert!(w.deny.is_none() && w.context.contains("handoff point 285000") && w.context.contains("next clean stopping point"), "{w:?}");

    // Resuming an agent past its hard limit is refused; past its handoff
    // point it is a wrap-up only; a name that is no agent id says nothing.
    let send = |h: &mut Hooks, to: &str| {
        h.handle(&HookEvent {
            tool_name: "SendMessage".into(),
            transcript_path: s.tp(),
            tool_input: json!({"to": to, "message": "more"}),
            ..ev("PreToolUse")
        })
    };
    assert!(send(&mut h, "bld").deny.is_some_and(|r| r.contains("Do not resume it")));
    let soft = send(&mut h, "soft");
    assert!(soft.deny.is_none() && soft.context.contains("wrap-up message only"), "{soft:?}");
    assert!(send(&mut h, "rev").deny.is_none());
    assert_eq!(send(&mut h, "main"), HookAnswer::default());
    assert_eq!(send(&mut h, "../x"), HookAnswer::default());

    // When an agent that passed its handoff point finishes, the orchestrator
    // is told to start a fresh one (or, with fresh: ask, to ask first).
    let f = finished(&mut h, &s, "bld");
    assert!(f.contains("do not resume it") && f.contains("Ask the user") && f.contains("recall lane=agent:bld"), "{f}");
    let g = finished(&mut h, &s, "soft");
    assert!(g.contains("Start a FRESH reviewer agent"), "{g}");
    // Once only.
    assert!(!finished(&mut h, &s, "soft").contains("FRESH"));
}

/// With orchestrated mode off the context cap is v0.2's for every agent:
/// `cap.warn_tokens` and `cap.block_tokens` (450k and 500k), enforced,
/// whatever its kind; and a SendMessage is never gated.
#[test]
fn with_orchestrate_off_every_agent_gets_the_v02_cap() {
    let t = Temp::new("orch-off-cap");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &OrchestrateSpec::default(), &[])).unwrap();
    s.agent("mid", "general-purpose", "t1", "Kind: research", 460_000, false);
    s.agent("big", "Explore", "t2", "Find it.", 510_000, false);
    let call = |h: &mut Hooks, agent: &str, tool: &str| {
        h.handle(&HookEvent { agent_id: agent.into(), agent_type: "general-purpose".into(), tool_name: tool.into(), transcript_path: s.tp(), ..ev("PreToolUse") })
    };
    let m = call(&mut h, "mid", "Bash");
    assert!(m.deny.is_none() && m.context.contains("450000"), "{m:?}");
    let b = call(&mut h, "big", "Bash");
    assert!(b.deny.as_deref().is_some_and(|r| r.contains("hard limit 500000, cap")), "{b:?}");
    let send = h.handle(&HookEvent { tool_name: "SendMessage".into(), transcript_path: s.tp(), tool_input: json!({"to": "big", "message": "more"}), ..ev("PreToolUse") });
    assert_eq!(send, HookAnswer::default());
}

/// A launch refused at its kind's cap is queued (once, however often it is
/// retried) under the lane its brief names; a finish notification lists
/// what waits; the same launch admitted later leaves the queue.
#[test]
fn a_refused_launch_is_queued_listed_and_leaves_when_it_starts() {
    let t = Temp::new("orch-queue");
    let s = Session::new(t.path());
    let o = OrchestrateSpec { enabled: Some(true), kinds: vec![(Kind::Builder, KindSpec { max: Some(1), ..Default::default() })], ..Default::default() };
    let st = settings(t.path(), &o, &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    // A command hook: b1 is not excluded for being older than this process
    // (a file time may read a few ms before the process clock's start).
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Build the exporter.", 1000, false);
    let brief = "Journal lane: ALPHA-ONE\nBuild the printer.";
    let r = launch(&mut h, &s, "t1", "general-purpose", brief).deny.expect("the builder slot is full");
    assert!(r.contains("atlas queued it as journal:ALPHA-ONE:0001"), "{r}");
    // A retry is not queued twice.
    let r = launch(&mut h, &s, "t2", "general-purpose", brief).deny.unwrap();
    assert!(r.contains("has already queued it as journal:ALPHA-ONE:0001"), "{r}");
    // No lane in the brief: the queue lane.
    let r = launch(&mut h, &s, "t3", "general-purpose", "Build the reader.").deny.unwrap();
    assert!(r.contains("journal:queue:0001"), "{r}");
    {
        let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
        let w = atlas::queue::waiting(&m).unwrap();
        assert_eq!(w.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(), ["journal:ALPHA-ONE:0001", "journal:queue:0001"]);
        assert_eq!(atlas::queue::kind_of(&w[0]), Some(Kind::Builder));
        assert_eq!(m.head(&w[0].key).unwrap().unwrap().body, brief, "the brief is kept whole");
    }
    // b1 finishes: the notification lists both, oldest first.
    let n = finished(&mut h, &s, "b1");
    assert!(n.contains("atlas queue: start together: journal:ALPHA-ONE:0001 (builder, lane ALPHA-ONE, d). Waiting: journal:queue:0001 (builder slots full)"), "{n}");
    // The queued launch starts: it leaves the queue.
    assert!(launch(&mut h, &s, "t4", "general-purpose", brief).deny.is_none());
    let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
    let w = atlas::queue::waiting(&m).unwrap();
    assert_eq!(w.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(), ["journal:queue:0001"]);
    assert_eq!(m.head("journal:ALPHA-ONE:0001").unwrap().unwrap().status, "retired");
}

/// With the caps off nothing is queued.
#[test]
fn nothing_is_queued_with_the_caps_off() {
    let t = Temp::new("orch-queue-off");
    let s = Session::new(t.path());
    let st = settings(t.path(), &OrchestrateSpec::default(), &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    assert!(launch(&mut h, &s, "t1", "general-purpose", "Build.").deny.is_none());
    let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
    assert!(atlas::queue::waiting(&m).unwrap().is_empty());
    assert_eq!(finished(&mut h, &s, "x"), "");
}

/// `atlas queue` lists, shows and drops waiting launches from the store.
#[test]
fn the_queue_command_lists_shows_and_drops() {
    let t = Temp::new("orch-queue-cli");
    let home = t.path().join("home");
    let run = |args: &[&str]| -> (bool, String) {
        let o = std::process::Command::new(bin())
            .arg("queue")
            .args(args)
            .env("ATLAS_HOME", &home)
            .env_remove("ATLAS_CONFIG")
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .output()
            .unwrap();
        (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    };
    // The first run writes the template configuration and an empty store.
    let (ok, out) = run(&[]);
    assert!(ok && out.contains("no launches are waiting"), "{out}");
    {
        let mut m = atlas::memory::MemoryStore::open(&home.join("state").join("memory.sqlite")).unwrap();
        let l = atlas::queue::Launch { kind: Kind::Reviewer, lane: Some("BETA-TWO"), description: "review the parser", subagent_type: "general-purpose", prompt: "Kind: reviewer\nReview it.", session: "s1", class_meta: "" };
        assert_eq!(atlas::queue::add(&mut m, &l).unwrap(), ("journal:BETA-TWO:0001".to_string(), true));
    }
    // Reviews are listed as their own queue, waiting for a reviewer slot.
    let (ok, out) = run(&[]);
    assert!(ok && out.contains("1 review is waiting for a reviewer slot") && out.contains("journal:BETA-TWO:0001  0 min ago  review the parser") && !out.contains("launch is waiting"), "{out}");
    let (ok, out) = run(&["show", "journal:BETA-TWO:0001"]);
    assert!(ok && out.contains("Review it.") && out.contains("subagent_type: general-purpose"), "{out}");
    let (ok, out) = run(&["drop", "journal:GAMMA-X:0009"]);
    assert!(!ok && out.contains("is not a waiting launch"), "{out}");
    let (ok, out) = run(&["drop", "journal:BETA-TWO:0001"]);
    assert!(ok && out.contains("dropped journal:BETA-TWO:0001"), "{out}");
    let (ok, out) = run(&["list"]);
    assert!(ok && out.contains("no launches are waiting"), "{out}");
    // `add` files planned work with its classification; `next` proposes.
    let brief = t.path().join("brief.md");
    std::fs::write(&brief, "Kind: builder\nLane: parser\nPriority: urgent\nUses: compile\n\nFix the reader.").unwrap();
    let (ok, out) = run(&["add", "--description", "fix the reader", brief.to_str().unwrap()]);
    assert!(ok && out.contains("queued journal:parser:0001 (builder, lane parser, urgent)"), "{out}");
    std::fs::write(&brief, "Kind: builder\nLane: docs\nUses: compile\n\nWrite the guide.").unwrap();
    let (ok, out) = run(&["add", "--description", "write the guide", brief.to_str().unwrap()]);
    assert!(ok && out.contains("queued journal:docs:0001"), "{out}");
    let (ok, out) = run(&["show", "journal:parser:0001"]);
    assert!(ok && out.contains("priority: urgent (declared)") && out.contains("uses: compile (declared)"), "{out}");
    let (ok, out) = run(&["next"]);
    assert!(ok && out.contains("start together: journal:parser:0001 (builder, lane parser, urgent, fix the reader)") && out.contains("Waiting: journal:docs:0001 (compile held by journal:parser:0001)"), "{out}");
    // Three reviews in the builder's lane: two take the reviewer slots, no
    // lane holds them, and the third waits for a reviewer slot only.
    for (i, what) in ["simplicity review of PR 7", "cost review of PR 7", "cost review of PR 8"].iter().enumerate() {
        std::fs::write(&brief, format!("Kind: reviewer
Lane: parser

Do the {what}.")).unwrap();
        let (ok, out) = run(&["add", "--description", what, brief.to_str().unwrap()]);
        assert!(ok && out.contains(&format!("journal:parser:000{}", i + 2)), "{out}");
    }
    let (ok, out) = run(&["next"]);
    assert!(ok && out.contains("journal:parser:0002 (reviewer, lane parser") && out.contains("journal:parser:0003 (reviewer, lane parser"), "{out}");
    assert!(out.contains("journal:parser:0004 (reviewer slots full)"), "{out}");
    // A row filed without a kind line whose brief says Kind: reviewer is a
    // review too, in the list and to the scheduler.
    {
        let mut m = atlas::memory::MemoryStore::open(&home.join("state").join("memory.sqlite")).unwrap();
        let d = atlas::memory::Draft { title: "legacy review".into(), meta: "brief_sha: 0".into(), body: "Kind: reviewer

Review the old one.".into(), reason: "test".into(), author: "test".into(), client: "test".into(), provenance: None, time: None };
        let mut w = m.writer().unwrap();
        w.create("journal:parser:0005", "journal", atlas::queue::ENTRY, "parser", &d, None).unwrap();
        w.commit().unwrap();
    }
    let (ok, out) = run(&["list"]);
    assert!(ok && out.contains("4 reviews are waiting for a reviewer slot") && out.contains("2 launches are waiting") && out.contains("journal:parser:0005  0 min ago  legacy review"), "{out}");
    let (ok, out) = run(&["next"]);
    assert!(ok && out.contains("journal:parser:0005 (reviewer slots full)"), "{out}");
}

/// Q-QL1 and Q-QL2 through the hook: the finish line proposes a batch that
/// keeps lanes and resources apart; a launch whose conflict is declared on
/// both sides is denied and queued, an inferred one is let through with a
/// warning, and a declared read-only reviewer shares the lane.
#[test]
fn lanes_and_resources_shape_the_batch_and_the_launch() {
    let t = Temp::new("orch-lanes");
    let s = Session::new(t.path());
    let o = OrchestrateSpec { enabled: Some(true), kinds: vec![(Kind::Builder, KindSpec { max: Some(6), ..Default::default() })], ..Default::default() };
    let st = settings(t.path(), &o, &[]);
    let mut h = Hooks::open(st).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: parser\nUses: gpu\n\nFix the reader.", 1000, false);
    // 2026-11-14: a lane name is a label, not a lock. A second builder in
    // the lane with disjoint uses starts, as the first free sub-lane; its
    // prompt is never rewritten, and its agent is told at its start.
    let a = launch(&mut h, &s, "t1", "general-purpose", "Kind: builder\nLane: parser\nUses: none\n\nFix the writer.");
    assert!(a.deny.is_none() && a.context.contains("this launch runs as sub-lane parser/2") && a.updated_input.is_none(), "{a:?}");
    s.agent("w1", "general-purpose", "t1", "Kind: builder\nLane: parser\nUses: none\n\nFix the writer.", 1000, false);
    let st = h.handle(&HookEvent { agent_id: "w1".into(), agent_type: "general-purpose".into(), transcript_path: s.tp(), ..ev("SubagentStart") });
    assert!(st.context.contains("you run as sub-lane parser/2 of parser"), "{st:?}");
    // A declared read-only reviewer shares the lane: no context, no rewrite.
    let a = launch(&mut h, &s, "t2", "general-purpose", "Kind: reviewer\nLane: parser\nUses: read-only\n\nReview the reader.");
    assert!(a.deny.is_none() && a.context.is_empty() && a.updated_input.is_none(), "{a:?}");
    // An inferred resource conflict is a warning.
    let a = launch(&mut h, &s, "t3", "general-purpose", "Kind: builder\nLane: docs\n\nTake a frame capture of the menu.");
    assert!(a.deny.is_none() && a.context.contains("may conflict with running work (gpu held by job b1 (b1))"), "{a:?}");
    // A queued gpu item waits while b1 holds the gpu; after b1 finishes the
    // finish line proposes it.
    let r = launch(&mut h, &s, "t4", "general-purpose", "Kind: builder\nLane: render\nUses: gpu\n\nRender the wreck.")
        .deny
        .expect("declared gpu on both sides");
    assert!(r.contains("gpu held by job b1 (b1)") && r.contains("atlas queued it as journal:render:0001"), "{r}");
    let n = finished(&mut h, &s, "b1");
    assert!(n.contains("start together: journal:render:0001 (builder, lane render, d)"), "{n}");
}

/// The defect of 2026-11-14, replayed: a builder runs in lane ATLAS-TOOLING
/// and a read-only research launch declares the same lane. It is let through
/// (it was refused and queued for the lane name alone), it takes the copy
/// the old rule queued off the queue, and the same brief relaunched under
/// another lane takes a waiting copy off by content.
#[test]
fn a_read_only_research_launch_beside_a_builder_in_its_lane_is_allowed() {
    let t = Temp::new("orch-ro-lane");
    let s = Session::new(t.path());
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![
            (Kind::Builder, KindSpec { max: Some(6), ..Default::default() }),
            (Kind::Research, KindSpec { max: Some(2), ..Default::default() }),
        ],
        ..Default::default()
    };
    let st = settings(t.path(), &o, &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    h.set_command_hook();
    s.agent(
        "b1",
        "general-purpose",
        "t-b1",
        "Kind: builder\nLane: ATLAS-TOOLING\nPriority: normal\nUses: compile (the atlas crate; one heavy compile at a time through runq)\n\n# atlas queue: no more false lane collisions\n\nRepo: D:/src/tools/atlas. Make a fresh worktree and run cargo test through runq.",
        1000,
        false,
    );
    let research = "Kind: research\nLane: ATLAS-TOOLING\nPriority: normal\nUses: none (read-only analysis; no cargo, no builds)\n\n# Do atlas's hooks earn their tokens?\n\nMeasure whether the hooks earn their tokens, one at a time through the logs. Read the hook logs; build nothing.";
    let file = |prompt: &str| {
        let mut m = atlas::memory::MemoryStore::open(&store_path).unwrap();
        let meta = atlas::lanes::classify(prompt, &[]).meta();
        let l = atlas::queue::Launch {
            kind: Kind::Research,
            lane: Some("ATLAS-TOOLING"),
            description: "Measure hook injection value",
            subagent_type: "general-purpose",
            prompt,
            session: "s1",
            class_meta: &meta,
        };
        atlas::queue::add(&mut m, &l).unwrap().0
    };
    // The copy the old rule queued.
    let queued = file(research);
    let a = launch(&mut h, &s, "t1", "general-purpose", research);
    assert!(a.deny.is_none(), "a read-only launch is never refused for its lane: {a:?}");
    assert!(!a.context.contains("conflict") && !a.context.contains("shares"), "{a:?}");
    assert!(a.context.contains(&format!("the same brief as waiting {queued} (lane ATLAS-TOOLING)")), "{a:?}");
    // Relaunched under another lane, its header and wrapping changed: the
    // waiting copy leaves by content, and the hook says so.
    let again = file(research);
    let moved = research.replace("Lane: ATLAS-TOOLING", "Lane: ATLAS-HOOK-VALUE").replace("Priority: normal", "Priority:   normal");
    let moved = moved.replace("Read the hook logs; build nothing.", "Read the hook logs;\n  build nothing.");
    let a = launch(&mut h, &s, "t2", "general-purpose", &moved);
    assert!(a.deny.is_none(), "{a:?}");
    assert!(
        a.context.contains(&format!("the same brief as waiting {again} (lane ATLAS-TOOLING), so atlas took it off the queue")),
        "{a:?}"
    );
    let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
    assert!(atlas::queue::waiting(&m).unwrap().is_empty());
}

/// atlas-dev #150 and the 2026-11-14 report: words on a `Uses:` line that
/// the policy does not list (repos, free text) are not tokens: they never
/// deny, queue or note anything; gpu, compile, worktrees and configured
/// words still deny, and a configured class limits each `name value` on
/// its own.
#[test]
fn unlisted_uses_words_are_not_tokens_through_the_hook() {
    let t = Temp::new("orch-uses150");
    let s = Session::new(t.path());
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![(Kind::Builder, KindSpec { max: Some(6), ..Default::default() })],
        resources: vec![("sqlport".into(), 1), ("cache".into(), 0), ("license".into(), 1)],
        ..Default::default()
    };
    let st = settings(t.path(), &o, &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    h.set_command_hook();
    s.agent(
        "a1",
        "general-purpose",
        "t-a1",
        "Kind: build\nLane: ES1-1663\nUses: db, contracts, api, react, sqlport 15126, cache, license\n\nBuild it.",
        1000,
        false,
    );
    // The issue's second header: let through, with no note about the words.
    let a = launch(
        &mut h,
        &s,
        "t1",
        "general-purpose",
        "Kind: build\nLane: ES1-1415\nUses: db, contracts, api, react, sqlport 15127\n\nBuild it.",
    );
    assert!(a.deny.is_none(), "{a:?}");
    assert!(!a.context.contains("shares") && !a.context.contains("api"), "{a:?}");
    assert!(!a.context.contains("atlas inferred it"), "{a:?}");
    // A word at capacity 0 gives no note.
    let a = launch(&mut h, &s, "t2", "general-purpose", "Kind: build\nLane: L2\nUses: cache\n\nBuild it.");
    assert!(a.deny.is_none() && a.context.is_empty(), "{a:?}");
    // A configured word and the same port are denied.
    let r = launch(&mut h, &s, "t3", "general-purpose", "Kind: build\nLane: L3\nUses: license\n\nBuild it.").deny.expect("license has capacity 1");
    assert!(r.contains("license held by job a1 (a1)"), "{r}");
    let r = launch(&mut h, &s, "t4", "general-purpose", "Kind: build\nLane: L4\nUses: sqlport: 15126\n\nBuild it.").deny.expect("the same port");
    assert!(r.contains("sqlport:15126 held by job a1 (a1)"), "{r}");
    let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
    let w = atlas::queue::waiting(&m).unwrap();
    assert_eq!(w.iter().map(|h| h.scope.as_str()).collect::<Vec<_>>(), ["L3", "L4"], "the shared-word launches were not queued");
}

/// atlas-dev #150: a queued launch leaves the queue when its lane starts
/// under a reworded brief with the same kind (the oldest, named), while a
/// `Queue:` key still wins, another kind and a planned item stay, and an
/// undeclared lane retires nothing.
#[test]
fn a_reworded_start_in_the_lane_takes_its_oldest_launch_off_the_queue() {
    let t = Temp::new("orch-stale150");
    let s = Session::new(t.path());
    let o = OrchestrateSpec { enabled: Some(true), kinds: vec![(Kind::Builder, KindSpec { max: Some(6), ..Default::default() })], ..Default::default() };
    let st = settings(t.path(), &o, &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: ES1-1415\nUses: compile\n\nHold the lane.", 1000, false);
    // A lane is no lock (2026-11-14): compile, declared on both sides, holds them.
    for (id, what) in [("t1", "tariff pricing"), ("t2", "pricing tests"), ("t3", "pricing docs")] {
        let r = launch(&mut h, &s, id, "general-purpose", &format!("Kind: builder\nLane: ES1-1415\nUses: compile\n\nBuild the {what}."))
            .deny
            .expect("compile is held");
        assert!(r.contains("atlas queued it"), "{r}");
    }
    let planned = {
        let mut m = atlas::memory::MemoryStore::open(&store_path).unwrap();
        let l = atlas::queue::Launch { kind: Kind::Reviewer, lane: Some("ES1-1415"), description: "review it", subagent_type: "general-purpose", prompt: "Kind: reviewer\nLane: ES1-1415\n\nReview it.", session: "s1", class_meta: "" };
        assert_eq!(atlas::queue::add(&mut m, &l).unwrap().0, "journal:ES1-1415:0004");
        atlas::queue::add_planned(&mut m, &atlas::queue::Planned { lane: "ES1-1415".into(), kind: "builder".into(), uses: vec!["sqlport 15128".into()], title: "planned".into(), ..Default::default() }).unwrap()
    };
    // A planned `uses: sqlport 15128` reads back as one resource.
    {
        let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
        let ph = atlas::queue::waiting(&m).unwrap().into_iter().find(|h| h.key == planned).unwrap();
        assert_eq!(atlas::queue::qmeta(&ph.meta).uses, Some(vec![("sqlport:15128".to_string(), true)]));
    }
    finished(&mut h, &s, "b1");
    let keys = || atlas::queue::waiting(&atlas::memory::MemoryStore::open(&store_path).unwrap()).unwrap().into_iter().map(|h| h.key).collect::<Vec<_>>();
    // No declared lane: nothing leaves.
    let a = launch(&mut h, &s, "t5", "general-purpose", "Kind: builder\n\nJournal to lane ES1-1415. Build the pricing.");
    assert!(a.deny.is_none() && !a.context.contains("off the queue"), "{a:?}");
    assert_eq!(keys().len(), 5);
    // A `Queue:` key wins over the oldest in the lane.
    let a = launch(&mut h, &s, "t6", "general-purpose", "Queue: journal:ES1-1415:0003\nKind: builder\nLane: ES1-1415\n\nWrite the pricing docs.");
    assert!(a.deny.is_none() && a.context.contains("took journal:ES1-1415:0003 off the queue."), "{a:?}");
    // A reworded brief in the lane: the oldest builder launch leaves, named.
    let a = launch(&mut h, &s, "t7", "general-purpose", "Kind: builder\nLane: es1-1415\n\nBuild ES1-1415 tariff pricing type.");
    assert!(a.deny.is_none() && a.context.contains("took journal:ES1-1415:0001 off the queue (the oldest waiting builder launch in its lane); 1 more"), "{a:?}");
    let a = launch(&mut h, &s, "t8", "general-purpose", "Kind: builder\nLane: ES1-1415\n\nAdd the pricing tests.");
    assert!(a.context.contains("took journal:ES1-1415:0002 off the queue (the oldest waiting builder launch in its lane)."), "{a:?}");
    // Only the review (another kind) and the planned item wait.
    assert_eq!(keys(), ["journal:ES1-1415:0004".to_string(), planned.clone()]);
    let a = launch(&mut h, &s, "t9", "general-purpose", "Kind: builder\nLane: ES1-1415\n\nMore pricing work.");
    assert!(a.deny.is_none() && !a.context.contains("off the queue"), "{a:?}");
    // A review shares the lane: another review starting there leaves the
    // waiting one alone.
    let a = launch(&mut h, &s, "t10", "general-purpose", "Kind: reviewer\nLane: ES1-1415\n\nReview PR 2.");
    assert!(a.deny.is_none() && !a.context.contains("off the queue"), "{a:?}");
    assert_eq!(keys().len(), 2);
}

/// Planned work: `after: KEY` waits until KEY is marked done (even when
/// that is against `order`), `done` finds an item that already started, a
/// launch retires a planned item only by its `Queue:` key, and missing
/// keys and cycles say so instead of waiting silently.
#[test]
fn planned_items_wait_on_done_keys_and_leave_by_their_key() {
    use atlas::queue::{add_planned, items, started, Planned, Started};
    let t = Temp::new("orch-planned");
    let mut m = atlas::memory::MemoryStore::open(&t.path().join("memory.sqlite")).unwrap();
    let orch = atlas::orchestrate::OrchSettings::default();
    let plan = |m: &mut atlas::memory::MemoryStore, lane: &str, title: &str, order: Option<&str>, after: &[&str]| {
        add_planned(m, &Planned { lane: lane.into(), kind: "builder".into(), uses: vec!["none".into()], after: after.iter().map(|s| s.to_string()).collect(), order: order.map(|o| o.parse().unwrap()), title: title.into(), ..Planned::default() }).unwrap()
    };
    let budget = plan(&mut m, "GPU-BUDGET", "gpu budget", Some("13"), &[]);
    let fire = plan(&mut m, "FIRE-LOOK", "fire look", Some("12"), &[&budget]);
    let now = atlas::memory::now();
    let start = |m: &atlas::memory::MemoryStore| {
        let (_, its) = items(m, &now).unwrap();
        let p = atlas::queue::propose(&orch, &its, &[]);
        (p.start.iter().map(|&i| its[i].key.clone()).collect::<Vec<_>>(), p.wait.iter().map(|(i, r)| (its[*i].key.clone(), r.clone())).collect::<Vec<_>>())
    };
    // `after` wins over `order`.
    let (s, w) = start(&m);
    assert_eq!(s, std::slice::from_ref(&budget));
    assert_eq!(w, [(fire.clone(), format!("after {budget}"))]);
    // A launch on its lane without a `Queue:` line leaves it waiting, even
    // with the same lane and kind.
    assert_eq!(started(&mut m, "other work", "Lane: GPU-BUDGET\nUses: read-only\nReview it.", Some("GPU-BUDGET"), Kind::Builder).unwrap(), Started::None);
    assert_eq!(started(&mut m, "gpu budget", &format!("Queue: {budget}\nLane: GPU-BUDGET\nDo it."), Some("GPU-BUDGET"), Kind::Builder).unwrap(), Started::Retired(budget.clone()));
    // Started is not done: the dependent still waits.
    assert_eq!(start(&m).1, [(fire.clone(), format!("after {budget}"))]);
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    atlas::queue::cli(&args(&["done", &budget]), &mut m, &orch, &[]).unwrap();
    assert_eq!(start(&m).0, std::slice::from_ref(&fire));
    // `plan` refuses a key that names no queued item.
    let e =
        atlas::queue::cli(&args(&["plan", "--lane", "X", "--title", "t", "--after", "journal:NOPE:0009"]), &mut m, &orch, &[]).unwrap_err();
    assert!(e.contains("no such queued item"), "{e}");
    // Filed by hand: a missing key and a cycle are named.
    let gone = plan(&mut m, "LOST", "lost", None, &["journal:NOPE:0009"]);
    let c1 = plan(&mut m, "CY", "c1", None, &["journal:CY:0002"]);
    let c2 = plan(&mut m, "CY", "c2", None, &[&c1]);
    let w = start(&m).1;
    assert!(w.contains(&(gone, "after journal:NOPE:0009 (no such queued item)".into())), "{w:?}");
    assert!(w.contains(&(c1.clone(), format!("after {c2} (a cycle)"))) && w.contains(&(c2.clone(), format!("after {c1} (a cycle)"))), "{w:?}");
    // An item after a cycle waits on it but is not part of it; a self-loop is one.
    let tail = plan(&mut m, "CY", "tail", None, &[&c2]);
    let me = plan(&mut m, "SELF", "self", None, &["journal:SELF:0001"]);
    let w = start(&m).1;
    assert!(w.contains(&(tail, format!("after {c2}"))), "{w:?}");
    assert!(w.contains(&(me, "after journal:SELF:0001 (a cycle)".into())), "{w:?}");
    // A dependency on a dropped item says so, instead of waiting.
    let doomed = plan(&mut m, "DROP", "doomed", None, &[]);
    let orphan = plan(&mut m, "DROP", "orphan", None, &[&doomed]);
    atlas::queue::cli(&args(&["drop", &doomed]), &mut m, &orch, &[]).unwrap();
    assert!(start(&m).1.contains(&(orphan, format!("after {doomed} (dropped from the queue)"))));
    // A dependency on a replaced item names its replacement.
    let old = plan(&mut m, "SUP", "old", None, &[]);
    let waiter = plan(&mut m, "SUP", "waiter", None, &[&old]);
    {
        let mut w = m.writer().unwrap();
        let d = atlas::memory::Draft { title: "new".into(), meta: "planned: true
".into(), reason: "t".into(), author: "t".into(), client: "t".into(), ..atlas::memory::Draft::default() };
        w.supersede(std::slice::from_ref(&old), "journal:SUP:0009", "journal", atlas::queue::ENTRY, "SUP", &d, None).unwrap();
        w.commit().unwrap();
    }
    let w = start(&m).1;
    assert!(w.contains(&(waiter, format!("after {old} (superseded by journal:SUP:0009)"))), "{w:?}");
    // A priority not given reads back as the default, not as declared.
    let h = m.head(&fire).unwrap().unwrap();
    assert_eq!(atlas::queue::qmeta(&h.meta).priority, Some((atlas::lanes::Priority::Normal, atlas::lanes::Source::Default)));
}

/// End to end through the binary: fractions are filed, listed in numeric order
/// among whole numbers and printed as written; junk is refused and files nothing.
#[test]
fn the_queue_files_and_lists_fractional_orders() {
    let t = Temp::new("orch-fraction");
    let cfg = t.path().join("atlas.json");
    std::fs::write(&cfg, format!("{{\"db\": {:?}, \"vaults\": [], \"memory\": {{\"store\": {:?}}}}}", t.path().join("index.sqlite").display().to_string(), t.path().join("memory.sqlite").display().to_string())).unwrap();
    let queue = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"))
            .arg("--config")
            .arg(&cfg)
            .arg("queue")
            .args(args)
            .env("ATLAS_HOME", t.path().join("home-atlas"))
            .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
            .env("HOME", t.path())
            .env("USERPROFILE", t.path())
            .env_remove("ATLAS_CONFIG")
            .env_remove("ATLAS_PROJECT")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let (ok, out, err) = queue(&["plan", "--lane", "F", "--title", "unplaced"]);
    assert!(ok, "{out}{err}");
    for (title, order) in [("ten", "10"), ("nine-96", "9.96"), ("nine", "9"), ("nine-94", "9.94"), ("twelve-05", "12.05"), ("nine-50", "9.50")] {
        let (ok, out, err) = queue(&["plan", "--lane", "F", "--title", title, "--order", order]);
        assert!(ok && out.contains(&format!("order {order}")), "{out}{err}");
    }
    for junk in ["first", "NaN", "inf", "1e3", ".5", "9.5.1"] {
        let (ok, out, err) = queue(&["plan", "--lane", "F", "--title", "junk", "--order", junk]);
        assert!(!ok && err.contains("--order needs a number") && err.contains(&format!("{junk:?}")), "{out}{err}");
    }
    let (ok, list, err) = queue(&["list"]);
    assert!(ok, "{list}{err}");
    assert!(list.contains("7 planned items"), "{list}");
    let want = ["#9  nine", "#9.50  nine-50", "#9.94  nine-94", "#9.96  nine-96", "#10  ten", "#12.05  twelve-05", "normal  unplaced"];
    let at: Vec<usize> = want.iter().map(|w| list.find(w).unwrap_or_else(|| panic!("{w} missing: {list}"))).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{list}");
}

/// A report of more than `over` bytes: one paragraph with an outcome, a
/// number and a question, padded.
fn long_report(over: usize) -> String {
    let mut r = String::from("Merged PR #12 as abc1234. Tests 40/0/1.\n\n## Questions\n\n- Q1. Ship it now? (a) yes, recommended.\n\n");
    while r.len() <= over {
        r.push_str("The reader walked the folder and nothing in this line changes what the caller does next.\n");
    }
    r
}

/// Q-SD3 as refined on 11-04: a report over twice the digest bound is sent
/// back once, filed whole, without a draft; one within it, a second stop
/// and the caps off pass.
#[test]
fn a_long_report_is_sent_back_once_without_a_draft() {
    let t = Temp::new("orch-report");
    let o = OrchestrateSpec { enabled: Some(true), ..Default::default() };
    let st = settings(t.path(), &o, &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    let stop = |h: &mut Hooks, id: &str, msg: &str, active: bool| {
        h.handle(&HookEvent { agent_id: id.into(), agent_type: "general-purpose".into(), last_message: msg.into(), stop_hook_active: active, ..ev("SubagentStop") })
    };
    assert!(stop(&mut h, "a1", "Merged PR #3. Done.", false).block.is_none(), "a short report passes");
    let mid = long_report(atlas::report::WHOLE_UP_TO + 4_000);
    assert!(mid.trim().len() <= atlas::report::SEND_BACK_OVER);
    assert!(stop(&mut h, "a0", &mid, false).block.is_none(), "a report under twice the bound passes");
    let long = long_report(atlas::report::SEND_BACK_OVER);
    let r = stop(&mut h, "a2", &long, false).block.expect("a long report is sent back");
    assert!(r.contains("atlas filed it whole as journal:reports:0001"), "{r}");
    assert!(!r.contains("Q1. Ship it now?") && r.len() < 1_000, "no draft: {r}");
    assert!(stop(&mut h, "a2", &long, true).block.is_none(), "a stop already blocked passes");
    assert!(stop(&mut h, "a2", &long, false).block.is_none(), "it is asked once per agent");
    {
        let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
        assert_eq!(m.head("journal:reports:0001").unwrap().unwrap().body, long.trim());
    }
    // Compaction's summariser has an agent id and no type: not a report, so not
    // filed (the next filing below still takes 0002) and not sent back.
    let summary = h.handle(&HookEvent { agent_id: "s1".into(), last_message: long.clone(), ..ev("SubagentStop") });
    assert!(summary.block.is_none(), "a summary is not blocked");
    // The same report through a hand-back tool is denied once.
    let hand = |h: &mut Hooks, id: &str| {
        h.handle(&HookEvent { agent_id: id.into(), tool_name: "SubagentHandback".into(), tool_input: json!({"message": long.clone()}), ..ev("PreToolUse") })
    };
    let r = hand(&mut h, "a3").deny.expect("a long hand-back is sent back");
    assert!(r.contains("journal:reports:0002"), "{r}");
    assert!(hand(&mut h, "a3").deny.is_none(), "once");
    // With the caps off nothing is sent back.
    let t2 = Temp::new("orch-report-off");
    let mut off = Hooks::open(settings(t2.path(), &OrchestrateSpec::default(), &[])).unwrap();
    assert!(stop(&mut off, "a4", &long, false).block.is_none());
}

/// #182: `met`, `done`, `show` and `drop` take a bare lane key (as
/// `atlas queue` shows each item's lane): its one waiting item is used,
/// several are listed with their full keys, and `--help` prints usage.
#[test]
fn queue_commands_take_a_bare_lane_key() {
    use atlas::queue::{add_planned, Planned};
    let t = Temp::new("orch-lane-key");
    let mut m = atlas::memory::MemoryStore::open(&t.path().join("memory.sqlite")).unwrap();
    let orch = atlas::orchestrate::OrchSettings::default();
    let plan = |m: &mut atlas::memory::MemoryStore, lane: &str, title: &str, after: &[&str]| {
        add_planned(m, &Planned { lane: lane.into(), kind: "builder".into(), uses: vec!["none".into()], after: after.iter().map(|s| s.to_string()).collect(), title: title.into(), ..Planned::default() }).unwrap()
    };
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let one = plan(&mut m, "ES1-1981", "geo dal", &["api#876 merged"]);
    // The bare lane key resolves to its one item, any case.
    atlas::queue::cli(&args(&["met", "ES1-1981", "api#876", "merged"]), &mut m, &orch, &[]).unwrap();
    atlas::queue::cli(&args(&["show", "es1-1981"]), &mut m, &orch, &[]).unwrap();
    // Two items on a lane: listed with their full keys, nothing done.
    let a = plan(&mut m, "ES1-2000", "a", &[]);
    let b = plan(&mut m, "ES1-2000", "b", &[]);
    let e = atlas::queue::cli(&args(&["done", "ES1-2000"]), &mut m, &orch, &[]).unwrap_err();
    assert!(e.contains(&a) && e.contains(&b) && e.contains("full key"), "{e}");
    // Nothing on the lane: says so, and suggests a key that contains it.
    let e = atlas::queue::cli(&args(&["drop", "1981"]), &mut m, &orch, &[]).unwrap_err();
    assert!(e.contains("no queued item keyed 1981") && e.contains(&format!("did you mean {one}")), "{e}");
    // A bare key works for drop and done too.
    atlas::queue::cli(&args(&["drop", "ES1-1981"]), &mut m, &orch, &[]).unwrap();
    assert!(!atlas::queue::waiting(&m).unwrap().iter().any(|h| h.key == one));
    // `--help` is usage on every subcommand, never a key.
    for w in ["met", "done", "show", "drop"] {
        atlas::queue::cli(&args(&[w, "--help"]), &mut m, &orch, &[]).unwrap();
        atlas::queue::cli(&args(&[w, "-h"]), &mut m, &orch, &[]).unwrap();
    }
    assert_eq!(atlas::queue::waiting(&m).unwrap().len(), 2);
}

/// Review M1 on #201: `done` and `met` act on started items too, so a bare
/// lane key counts the lane's started (not yet done) items: one started
/// item and one waiting follow-up are listed by full key, and nothing is
/// marked; once the started one is done, the lane key names the follow-up.
#[test]
fn a_bare_lane_key_counts_the_lane_s_started_items() {
    use atlas::queue::{add_planned, started, Marks, Planned, Started};
    let t = Temp::new("orch-lane-key-started");
    let mut m = atlas::memory::MemoryStore::open(&t.path().join("memory.sqlite")).unwrap();
    let orch = atlas::orchestrate::OrchSettings::default();
    let plan = |m: &mut atlas::memory::MemoryStore, title: &str, after: &[&str]| {
        add_planned(m, &Planned { lane: "ES1-1981".into(), kind: "builder".into(), uses: vec!["none".into()], after: after.iter().map(|s| s.to_string()).collect(), title: title.into(), ..Planned::default() }).unwrap()
    };
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let first = plan(&mut m, "geo dal", &[]);
    assert_eq!(started(&mut m, "geo dal", &format!("Queue: {first}\nLane: ES1-1981\nDo it."), Some("ES1-1981"), Kind::Builder).unwrap(), Started::Retired(first.clone()));
    let follow = plan(&mut m, "geo dal follow-up", &[first.as_str()]);
    for w in [&["done", "ES1-1981"][..], &["met", "es1-1981", "x"][..]] {
        let e = atlas::queue::cli(&args(w), &mut m, &orch, &[]).unwrap_err();
        assert!(e.contains(&first) && e.contains(&follow) && e.contains("full key"), "{e}");
    }
    let marks = Marks::read(&m).unwrap();
    assert!(marks.done.is_empty() && marks.met.is_empty(), "{marks:?}");
    assert!(atlas::queue::waiting(&m).unwrap().iter().any(|h| h.key == follow));
    // The started one, done by its full key, no longer counts.
    atlas::queue::cli(&args(&["done", &first]), &mut m, &orch, &[]).unwrap();
    atlas::queue::cli(&args(&["done", "ES1-1981"]), &mut m, &orch, &[]).unwrap();
    assert!(Marks::read(&m).unwrap().done.contains(&follow));
    assert!(atlas::queue::waiting(&m).unwrap().is_empty());
}

/// The required-reviews merge gate: `gh pr merge` waits for a passing
/// review of the PR's head from each role, filed as a `review` entry or
/// as a note with `review:` meta; a request for changes on that head
/// blocks its role; advisory warns; no roles is off.
#[test]
fn a_merge_waits_for_each_role_s_review_of_its_head() {
    use atlas::memory::Draft;
    let t = Temp::new("orch-reviews");
    let spec = |mode: Mode, roles: &[&str]| OrchestrateSpec { review_roles: roles.iter().map(|r| r.to_string()).collect(), review_mode: Some(mode), ..Default::default() };
    let st = settings(t.path(), &spec(Mode::Enforced, &["simplicity", "Cost"]), &[]);
    let store_path = st.store.clone();
    let file = |sub: &str, meta: &str| {
        let mut m = atlas::memory::MemoryStore::open(&store_path).unwrap();
        let mut w = m.writer().unwrap();
        let key = w.next_key("journal:L:", 4).unwrap();
        let d = Draft { title: "a review".into(), meta: format!("{meta}\n"), reason: "review gate test".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
        w.create(&key, "journal", sub, "L", &d, None).unwrap();
        w.commit().unwrap();
    };
    use atlas::reviews::Pr;
    fn head(m: &atlas::reviews::Merge, _cwd: &str) -> Result<(Pr, String), String> {
        match m.selector.as_deref() {
            Some("99") => Ok((Pr { repo: None, number: 99 }, "6e828cc0123456789abcdef".into())),
            other => Err(format!("no PR {other:?}")),
        }
    }
    let mut h = Hooks::open(st).unwrap();
    h.set_pr_head(head);
    let bash = |h: &mut Hooks, cmd: &str| h.handle(&HookEvent { tool_name: "Bash".into(), tool_input: json!({"command": cmd}), ..ev("PreToolUse") });
    // Passed: not denied, and no gate line (the router may still rewrite it).
    let passes = |a: &HookAnswer| a.deny.is_none() && !a.context.contains("atlas reviews");
    // No review of a numbered PR at all: named from the store, without gh
    // (PR 5 has no head here, so asking gh would say it cannot be checked).
    let d = bash(&mut h, "gh pr merge 99 --merge 2>&1").deny.expect("no reviews: denied");
    assert!(d.contains("PR #99 is missing: simplicity review; cost review"), "{d}");
    let d = bash(&mut h, "gh pr merge 5").deny.expect("no reviews of 5");
    assert!(d.contains("PR #5 is missing") && !d.contains("cannot be checked"), "{d}");
    // Other commands pass.
    assert!(passes(&bash(&mut h, "git merge main && gh pr view 99")));
    // A note in the older form counts; a review of an older head does not.
    file("note", "review: simplicity, cost\npr: 99\nhead: d84126e\nverdict: request-changes (simplicity), pass (cost)");
    file("note", "review: cost\npr: 99\nhead: 6e828cc\nverdict: merge with follow-ups");
    let d = bash(&mut h, "gh pr merge 99").deny.expect("simplicity is missing");
    assert!(d.contains("simplicity review for head 6e828cc") && !d.contains("cost review"), "{d}");
    // Changes requested on this head block, even beside a pass.
    file("review", "role: simplicity\npr: 99\nhead: 6e828cc\nverdict: request changes\nreport: r.md");
    file("review", "role: simplicity\npr: 99\nhead: 6e828cc\nverdict: pass");
    let d = bash(&mut h, "gh pr merge 99").deny.expect("changes requested");
    assert!(d.contains("simplicity review requested changes at head 6e828cc"), "{d}");
    // A PR whose head cannot be read is not merged blind.
    assert!(bash(&mut h, "gh pr merge https://github.com/o/r/pull/5").deny.expect("unreadable").contains("cannot be checked"));
    // A newer head that both roles pass merges.
    fn newer(_m: &atlas::reviews::Merge, _cwd: &str) -> Result<(Pr, String), String> {
        Ok((Pr { repo: None, number: 99 }, "abcdef0123".into()))
    }
    h.set_pr_head(newer);
    file("review", "role: simplicity, cost\npr: 99\nhead: abcdef0\nverdict: pass");
    assert!(passes(&bash(&mut h, "gh pr merge 99")));

    // #183: one PR number in two repos. A review of react#784 does not
    // pass api#784, even at a matching head prefix; api's own review does.
    fn in_api(_m: &atlas::reviews::Merge, _cwd: &str) -> Result<(Pr, String), String> {
        Ok((Pr { repo: Some("o/api".into()), number: 784 }, "7840000aaaa".into()))
    }
    h.set_pr_head(in_api);
    file("review", "role: simplicity, cost\npr: o/react#784\nhead: 7840000\nverdict: pass");
    let d = bash(&mut h, "gh pr merge 784 --repo o/api").deny.expect("only react's review");
    assert!(d.contains("PR o/api#784 is missing: simplicity review; cost review"), "{d}");
    file("review", "role: simplicity, cost\npr: api#784\nhead: 7840000\nverdict: pass");
    assert!(passes(&bash(&mut h, "gh pr merge 784 --repo o/api")));

    // Review S1 on #201: a repo is read as lowercase owner/repo whatever
    // its form, so `--repo` given with a host or a scheme matches a review
    // filed as `O/Api#786`; a `--repo` that lists several is refused.
    fn api_any(m: &atlas::reviews::Merge, _cwd: &str) -> Result<(Pr, String), String> {
        let number = m.selector.as_deref().and_then(|s| s.parse().ok()).ok_or("no number")?;
        Ok((Pr { repo: Some("o/api".into()), number }, "7840000aaaa".into()))
    }
    h.set_pr_head(api_any);
    file("review", "role: simplicity, cost\npr: O/Api#786\nhead: 7840000\nverdict: pass");
    for repo in ["github.com/o/api", "https://github.com/o/api", "https://github.com/O/api.git", "O/API"] {
        let a = bash(&mut h, &format!("gh pr merge 786 --repo {repo}"));
        assert!(passes(&a), "{repo}: {:?}", a.deny);
    }
    let d = bash(&mut h, "gh pr merge 786 --repo o/api,o/react").deny.expect("two repos");
    assert!(d.contains("several repos"), "{d}");
    // Review nit on #201: with no `--repo`, every repo's reviews of the
    // number pass the precheck, and gh's answer narrows them: react's
    // review of 787 does not pass api's 787.
    file("review", "role: simplicity, cost\npr: o/react#787\nhead: 7840000\nverdict: pass");
    let d = bash(&mut h, "gh pr merge 787").deny.expect("only react's review of 787");
    assert!(d.contains("PR o/api#787 is missing: simplicity review for head 7840000; cost review for head 7840000"), "{d}");
    file("review", "role: simplicity, cost\npr: o/api#787\nhead: 7840000\nverdict: pass");
    assert!(passes(&bash(&mut h, "gh pr merge 787")));

    // Advisory warns; no roles is off.
    let mut a = Hooks::open(settings(t.path(), &spec(Mode::Advisory, &["security"]), &[])).unwrap();
    a.set_pr_head(head);
    let w = bash(&mut a, "gh pr merge 99");
    assert!(w.deny.is_none() && w.context.contains("do not merge yet") && w.context.contains("security review"), "{w:?}");
    let mut off = Hooks::open(settings(t.path(), &spec(Mode::Enforced, &[]), &[])).unwrap();
    off.set_pr_head(head);
    assert!(passes(&bash(&mut off, "gh pr merge 99")));
}

/// CAP-PHANTOM (journal ATLAS-V03:0094, 0110), replayed from the live
/// session of 2026-11-04 22:16:53-22:18:41 UTC: two reviews launched in one
/// message (both PreToolUse calls run before either meta file exists), both
/// start, one finishes and notifies, and the next review must take the
/// freed slot. Run twice: as the live user settings send PreToolUse (no
/// `tool_use_id`) and as the plugin's hooks.json sends it (with one).
#[test]
fn a_started_or_finished_launch_holds_no_pending_slot() {
    for (case, ids) in [("live hook input, no tool_use_id", ["", "", ""]), ("plugin hook input, with tool_use_id", ["toolu_A", "toolu_B", "toolu_C"])] {
        let t = Temp::new("orch-phantom-replay");
        let s = Session::new(t.path());
        let on = OrchestrateSpec { enabled: Some(true), ..Default::default() };
        let mut h = Hooks::open(settings(t.path(), &on, &[])).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let simp = "Kind: reviewer\nReview PR 112 for simplicity.";
        let cost = "Kind: reviewer\nReview PR 112 for cost.";
        // 22:16:53 and 22:16:58: one message, two Agent calls; no meta yet.
        assert!(launch(&mut h, &s, ids[0], "general-purpose", simp).deny.is_none(), "{case}: first review");
        assert!(launch(&mut h, &s, ids[1], "general-purpose", cost).deny.is_none(), "{case}: second review");
        // 22:17:00: Claude Code writes both meta files (its own tool use ids).
        s.agent("simp", "general-purpose", "toolu_A", simp, 1000, false);
        s.agent("cost", "general-purpose", "toolu_B", cost, 1000, false);
        // 22:18:31: the simplicity review finishes and notifies.
        s.agent("simp", "general-purpose", "toolu_A", simp, 1000, true);
        let _ = finished(&mut h, &s, "simp");
        // 22:18:41: one reviewer runs (cost), so a second review starts.
        let next = launch(&mut h, &s, ids[2], "general-purpose", "Kind: reviewer\nReview PR 113 for simplicity.");
        assert!(next.deny.is_none(), "{case}: one reviewer runs, yet: {:?}", next.deny);
    }
}

/// Option B of journal ATLAS-V03:0137: the harness's SubagentStart retires
/// the launch's pending record (by the meta's toolUseId, else the oldest),
/// so a started agent is counted from its meta file alone, and a record
/// with no SubagentStart and no meta file still holds its slot.
#[test]
fn subagent_start_retires_the_pending_record() {
    let t = Temp::new("orch-phantom-start");
    let s = Session::new(t.path());
    let on = OrchestrateSpec { enabled: Some(true), ..Default::default() };
    let mut h = Hooks::open(settings(t.path(), &on, &[])).unwrap();
    let start = |h: &mut Hooks, id: &str| {
        let _ = h.handle(&HookEvent { agent_id: id.into(), agent_type: "general-purpose".into(), transcript_path: s.tp(), ..ev("SubagentStart") });
    };
    let review = "Kind: reviewer\nReview it.";
    // Without tool use ids: two launches, two starts, no meta file yet (the
    // pairing is by order). Both slots free again.
    assert!(launch(&mut h, &s, "", "general-purpose", review).deny.is_none());
    assert!(launch(&mut h, &s, "", "general-purpose", review).deny.is_none());
    assert!(launch(&mut h, &s, "", "general-purpose", review).deny.is_some(), "two pending reviews fill both slots");
    start(&mut h, "x1");
    start(&mut h, "x2");
    // A start seen twice retires nothing more.
    start(&mut h, "x2");
    assert!(launch(&mut h, &s, "", "general-purpose", review).deny.is_none(), "started agents with no meta file still pending");
    // With ids: the start pairs by the meta's toolUseId, not by order.
    assert!(launch(&mut h, &s, "toolu_P", "general-purpose", review).deny.is_none());
    s.agent("p", "general-purpose", "toolu_P", review, 1000, false);
    start(&mut h, "p");
    // Pending: the id-less launch admitted above (never started); running: p.
    let r = launch(&mut h, &s, "toolu_Q", "general-purpose", review).deny.expect("p runs and one launch is pending");
    assert!(r.contains("2 of 2 reviewer") && r.contains("1 launched moments ago"), "{r}");
}

/// Journal ATLAS-V03:0095 through the hooks: both reviews of one PR,
/// declared in the lane a builder holds, start together; a third review
/// waits for a reviewer slot only.
#[test]
fn both_reviews_of_one_pr_run_beside_the_lane_builder() {
    let t = Temp::new("orch-reviews");
    let s = Session::new(t.path());
    let on = OrchestrateSpec { enabled: Some(true), ..Default::default() };
    let mut h = Hooks::open(settings(t.path(), &on, &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: ATLAS-V03\n\nBuild CAP-PHANTOM.", 1000, false);
    let simplicity = launch(&mut h, &s, "t1", "general-purpose", "Kind: reviewer\nLane: ATLAS-V03\n\nSimplicity review of PR 113.");
    assert!(simplicity.deny.is_none() && !simplicity.context.contains("conflict"), "{simplicity:?}");
    let cost = launch(&mut h, &s, "t2", "general-purpose", "Kind: reviewer\nLane: ATLAS-V03\n\nCost review of PR 113.");
    assert!(cost.deny.is_none() && !cost.context.contains("conflict"), "{cost:?}");
    let third = launch(&mut h, &s, "t3", "general-purpose", "Kind: reviewer\nLane: ATLAS-V03\n\nCost review of PR 114.").deny.expect("the reviewer cap");
    assert!(third.starts_with("atlas agent cap: 2 of 2 reviewer"), "{third}");
    // A builder in the lane no longer conflicts with the lane's builder
    // (2026-11-14): it touches nothing b1 holds, so it runs as a sub-lane.
    let builder = launch(&mut h, &s, "t4", "general-purpose", "Kind: builder\nLane: ATLAS-V03\n\nBuild more.");
    assert!(builder.deny.is_none() && builder.context.contains("sub-lane ATLAS-V03/2"), "{builder:?}");
}

/// Automatic sub-lanes: a lane in use by running work, with no `brief`
/// entry of its own, gives a launch the first free `NAME/N`; a lane with a
/// brief keeps its lane; records in a sub-lane read under the parent.
#[test]
fn a_held_lane_without_a_brief_gives_the_first_free_sub_lane() {
    let t = Temp::new("orch-sublane");
    let s = Session::new(t.path());
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![(Kind::Builder, KindSpec { max: Some(12), ..Default::default() })],
        ..Default::default()
    };
    let st = settings(t.path(), &o, &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    s.agent("b2", "general-purpose", "t-b2", "Kind: builder\nLane: TOOLS/2\n\nBuild two.", 1000, false);
    s.agent("p1", "general-purpose", "t-p1", "Kind: builder\nLane: PLANNED\n\nBuild the plan.", 1000, false);
    let a = launch(&mut h, &s, "t1", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild three.");
    assert!(a.deny.is_none() && a.context.contains("this launch runs as sub-lane TOOLS/3") && a.updated_input.is_none(), "{a:?}");
    // A second launch of the same message sees the first one's sub-lane.
    let a = launch(&mut h, &s, "t1b", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild four.");
    assert!(a.deny.is_none() && a.context.contains("this launch runs as sub-lane TOOLS/4"), "{a:?}");
    // Both start; their briefs still say TOOLS. Each is told its sub-lane,
    // and a later launch does not take either again.
    s.agent("b3", "general-purpose", "t1", "Kind: builder\nLane: TOOLS\n\nBuild three.", 1000, false);
    s.agent("b4", "general-purpose", "t1b", "Kind: builder\nLane: TOOLS\n\nBuild four.", 1000, false);
    for (id, sub) in [("b3", "TOOLS/3"), ("b4", "TOOLS/4")] {
        let st = h.handle(&HookEvent { agent_id: id.into(), agent_type: "general-purpose".into(), transcript_path: s.tp(), ..ev("SubagentStart") });
        assert!(st.context.contains(&format!("you run as sub-lane {sub} of TOOLS")), "{st:?}");
    }
    let a = launch(&mut h, &s, "t1c", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild five.");
    assert!(a.deny.is_none() && a.context.contains("this launch runs as sub-lane TOOLS/5"), "{a:?}");
    // A running reviewer makes no sub-lane, and a read-only launch takes none.
    s.agent("r1", "general-purpose", "t-r1", "Kind: reviewer\nLane: REVIEWED\n\nReview it.", 1000, false);
    let a = launch(&mut h, &s, "t1d", "general-purpose", "Kind: builder\nLane: REVIEWED\n\nBuild it.");
    assert!(a.deny.is_none() && !a.context.contains("sub-lane"), "{a:?}");
    let a = launch(&mut h, &s, "t1e", "general-purpose", "Kind: research\nLane: TOOLS\n\nRead the parser.");
    assert!(a.deny.is_none() && a.context.is_empty() && a.updated_input.is_none(), "{a:?}");
    let d = |title: &str, body: &str| atlas::memory::Draft {
        title: title.into(),
        meta: String::new(),
        body: body.into(),
        reason: "test".into(),
        author: "test".into(),
        client: "test".into(),
        provenance: None,
        time: None,
    };
    {
        let mut m = atlas::memory::MemoryStore::open(&store_path).unwrap();
        let mut w = m.writer().unwrap();
        w.create("journal:PLANNED:0001", "journal", "brief", "PLANNED", &d("the plan", "Kind: builder\nLane: PLANNED"), None).unwrap();
        w.create("journal:TOOLS/2:0001", "journal", "decision", "TOOLS/2", &d("sub-lane walrus decision", "kept"), None).unwrap();
        w.create("journal:TOOLSET:0001", "journal", "decision", "TOOLSET", &d("another lane yak decision", "kept"), None).unwrap();
        w.create("journal:TOOLS/notes:0001", "journal", "decision", "TOOLS/notes", &d("another lane yak notes", "kept"), None).unwrap();
        w.commit().unwrap();
    }
    // A lane with a brief of its own is planned work: no sub-lane.
    let a = launch(&mut h, &s, "t2", "general-purpose", "Kind: builder\nLane: PLANNED\n\nBuild more of the plan.");
    assert!(a.deny.is_none() && !a.context.contains("sub-lane") && a.updated_input.is_none(), "{a:?}");
    // The parent's recall shows the sub-lane's record, not another lane's.
    let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
    let text = atlas::memory::recall::lane(&m, "TOOLS", 1 << 20).unwrap();
    assert!(text.contains("sub-lane walrus decision") && !text.contains("another lane yak"), "{text}");
    drop(m);
    // A brief filed under another spelling of the lane keeps it too.
    {
        let mut m = atlas::memory::MemoryStore::open(&store_path).unwrap();
        let mut w = m.writer().unwrap();
        w.create("journal:lowbrief:0001", "journal", "brief", "lowbrief", &d("the low plan", "Kind: builder\nLane: lowbrief"), None).unwrap();
        w.commit().unwrap();
    }
    s.agent("l1", "general-purpose", "t-l1", "Kind: builder\nLane: LOWBRIEF\n\nBuild the low plan.", 1000, false);
    let a = launch(&mut h, &s, "t3", "general-purpose", "Kind: builder\nLane: LOWBRIEF\n\nBuild more of it.");
    assert!(a.deny.is_none() && !a.context.contains("sub-lane"), "{a:?}");
}

/// The 2026-11-14 defect in auto mode: a changed tool input with no
/// permission decision can be denied there, so no Agent launch is ever
/// rewritten, the sub-lane case and the read-only research case included.
#[test]
fn no_agent_launch_is_rewritten_in_auto_mode() {
    let t = Temp::new("orch-auto");
    let s = Session::new(t.path());
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![(Kind::Builder, KindSpec { max: Some(6), ..Default::default() }), (Kind::Research, KindSpec { max: Some(2), ..Default::default() })],
        ..Default::default()
    };
    let mut h = Hooks::open(settings(t.path(), &o, &[])).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: ATLAS-TOOLING\nUses: compile\n\nBuild it.", 1000, false);
    for (id, prompt) in [
        ("t1", "Kind: builder\nLane: ATLAS-TOOLING\nUses: none\n\nBuild another part."),
        ("t2", "Kind: research\nLane: ATLAS-TOOLING\nUses: none\n\nRead the hook logs."),
    ] {
        let input = json!({"description": "d", "subagent_type": "general-purpose", "prompt": prompt});
        let a = h.handle(&HookEvent {
            tool_name: "Agent".into(),
            tool_use_id: id.into(),
            transcript_path: s.tp(),
            permission_mode: "auto".into(),
            tool_prompt: prompt.into(),
            tool_input: input,
            ..ev("PreToolUse")
        });
        assert!(a.deny.is_none() && a.updated_input.is_none(), "{a:?}");
        assert!(!a.to_json("PreToolUse").contains("updatedInput"), "{}", a.to_json("PreToolUse"));
    }
}

/// "Queue them" (2026-11-14): a writer on a branch a running writer holds
/// is denied and queued, not refused for good. The batch holds it while
/// the branch is held, so it is not offered, denied and filed again; a
/// relaunch meanwhile files nothing new; once the holder finishes it is
/// proposed. Two writers on one worktree sent in one message: the second
/// is queued as well.
#[test]
fn a_writer_on_a_held_branch_or_worktree_is_queued_and_waits() {
    let t = Temp::new("orch-branch");
    let s = Session::new(t.path());
    let o = OrchestrateSpec { enabled: Some(true), kinds: vec![(Kind::Builder, KindSpec { max: Some(6), ..Default::default() })], ..Default::default() };
    let st = settings(t.path(), &o, &[]);
    let store_path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: A\nRepo: D:/src/x\nBranch: feat\n\nBuild it.", 1000, false);
    let brief = "Kind: builder\nLane: B\nRepo: d:\\src\\x\\\nBranch: feat\n\nFix it.";
    let r = launch(&mut h, &s, "t1", "general-purpose", brief).deny.expect("one branch, two writers");
    assert!(r.contains("branch feat of d:/src/x held by job b1 (b1)") && r.contains("Do not retry now") && r.contains("atlas queued it as journal:B:0001"), "{r}");
    // Another agent finishes while b1 runs: the item waits on the branch.
    let n = finished(&mut h, &s, "other");
    assert!(n.contains("journal:B:0001 (branch feat of d:/src/x, after job b1 (b1))") && !n.contains("start together"), "{n}");
    // Relaunched too soon: denied again, and still the one entry.
    let r = launch(&mut h, &s, "t2", "general-purpose", brief).deny.expect("still held");
    assert!(r.contains("has already queued it as journal:B:0001"), "{r}");
    let m = atlas::memory::MemoryStore::open(&store_path).unwrap();
    assert_eq!(atlas::queue::waiting(&m).unwrap().len(), 1);
    drop(m);
    let n = finished(&mut h, &s, "b1");
    assert!(n.contains("start together: journal:B:0001"), "{n}");
    // Two writers on one worktree in one message: the first holds it until
    // its agent starts, so the second is queued.
    let a = launch(&mut h, &s, "t3", "general-purpose", "Kind: builder\nLane: C\nUses: worktree:w\n\nBuild one.");
    assert!(a.deny.is_none(), "{a:?}");
    let r = launch(&mut h, &s, "t4", "general-purpose", "Kind: builder\nLane: D\nUses: worktree:w\n\nBuild two.").deny.expect("the worktree is taken");
    assert!(r.contains("worktree:w held by a launch moments ago (t3)") && r.contains("atlas queued it as journal:D:0001"), "{r}");
}

/// Rule 3, at filing as well as at the start: a refused brief filed again
/// under another lane, description or header is the one entry.
#[test]
fn the_queue_files_one_brief_once_by_content() {
    let t = Temp::new("orch-dedupe");
    let st = settings(t.path(), &OrchestrateSpec { enabled: Some(true), ..Default::default() }, &[]);
    let mut m = atlas::memory::MemoryStore::open(&st.store).unwrap();
    let file = |m: &mut atlas::memory::MemoryStore, lane: &str, description: &str, prompt: &str| {
        let meta = atlas::lanes::classify(prompt, &[]).meta();
        let l = atlas::queue::Launch { kind: Kind::Builder, lane: Some(lane), description, subagent_type: "general-purpose", prompt, session: "s1", class_meta: &meta };
        atlas::queue::add(m, &l).unwrap()
    };
    let (k, new) = file(&mut m, "A", "Render the wreck", "Kind: builder\nLane: A\nUses: gpu\n\nRender the wreck.");
    assert!(new);
    let again = file(&mut m, "B", "Render the wreck again", "Kind: builder\nLane: B\nPriority: urgent\nUses: gpu\n\nRender   the\nwreck.");
    assert_eq!(again, (k.clone(), false));
    // Another brief is another entry.
    assert!(file(&mut m, "A", "Render the ship", "Kind: builder\nLane: A\nUses: gpu\n\nRender the ship.").1);
    // Only the header lines are left out: a body line that repeats one counts.
    let one = atlas::queue::content_sha("Kind: builder\nUses: gpu\n\n- Uses: gpu for the capture\nRender it.");
    let two = atlas::queue::content_sha("Kind: builder\nUses: gpu\n\nRender it.");
    assert_ne!(one, two);
}

/// S2 and S5 of the #180 review: only a declared use makes research or a
/// review a writer, and a word no policy limits (free text kept in a queued
/// item's `uses:` meta) never does; a builder queued behind read-only
/// research in its lane starts with it.
#[test]
fn read_only_research_never_holds_its_lane_in_the_batch() {
    use atlas::lanes::{classify, held_lane, propose, read_only, Class, Item, Policy, Source};
    let pol = Policy::default();
    // An inferred worktree or compile is no declared use.
    let inferred = classify("Kind: research\nLane: X\n\nRead D:/src/atlas-worktrees/x and report; run cargo test there.", &[]);
    assert!(!inferred.uses.is_empty(), "{inferred:?}");
    assert!(read_only(Kind::Research, &inferred) && held_lane(Kind::Research, &inferred).is_none());
    assert!(!read_only(Kind::Research, &classify("Kind: research\nLane: X\nUses: compile\n\nRun the tests.", &[])));
    let item = |key: &str, kind: Kind, class: Class| Item { key: key.into(), kind, class, waited_minutes: 10, order: None, takes_slot: true, unmet: Vec::new() };
    let lane = |uses: Vec<(String, Source)>| Class {
        lane: Some("X".into()),
        lane_src: Source::Declared,
        priority: atlas::lanes::Priority::Normal,
        priority_src: Source::Default,
        uses,
        read_only: false,
        repo: None,
        branch: None,
    };
    for uses in [
        Vec::new(),
        vec![("worktree:x".to_string(), Source::Inferred("filed".into()))],
        vec![("the".to_string(), Source::Declared), ("runq".to_string(), Source::Declared)],
    ] {
        let items = [item("r", Kind::Research, lane(uses.clone())), item("b", Kind::Builder, lane(Vec::new()))];
        let p = propose(&items, &[], &|_| 5, 10, &pol);
        assert_eq!(p.start, [0, 1], "{uses:?}: {p:?}");
    }
}

/// S4 of the #180 review: `Uses: worktree: NAME`, with a space, is the
/// worktree.
#[test]
fn a_spaced_worktree_use_keeps_its_name() {
    let pol = atlas::lanes::Policy::default();
    assert_eq!(atlas::lanes::parse_uses("worktree: queue-lanes", &pol).0, ["worktree:queue-lanes"]);
    assert_eq!(atlas::lanes::parse_uses("compile, worktree: a, gpu", &pol).0, ["compile", "gpu", "worktree:a"]);
}

/// Review holes A and B of #115 (journal ATLAS-V03:0141, 0142), through
/// `Agents` and its clock. A: launch A is refused at the permission prompt
/// (no agent ever), then B starts before its meta is written; B's start must
/// not retire A's record, and B pairs by id once its meta exists. B: a meta
/// that matches no record (an agent of another launch) retires nothing
/// while records carry ids.
#[test]
fn a_start_before_its_meta_pairs_by_id_later() {
    use atlas::orchestrate::{Agents, Launch};
    use std::time::{Duration, Instant, SystemTime};
    let t = Temp::new("orch-pair-race");
    let s = Session::new(t.path());
    let o = atlas::orchestrate::resolve(Some(&OrchestrateSpec { enabled: Some(true), ..Default::default() }), None, None, |_| None);
    let mut agents = Agents::new(Some(SystemTime::now() - Duration::from_secs(5)));
    let tp = s.tp();
    let input = json!({"description": "d", "subagent_type": "general-purpose"});
    let brief = "Kind: reviewer\nReview it.";
    let at = |id: &'static str| Launch { transcript: &tp, tool_use_id: id, input: &input, brief, store: None };
    let t0 = Instant::now();
    // A at t0 (refused later at the prompt: it never starts), B at t0 + 60 s.
    assert!(agents.admit(&o, &at("tA"), SystemTime::now(), t0).is_none());
    assert!(agents.admit(&o, &at("tB"), SystemTime::now(), t0 + Duration::from_secs(60)).is_none());
    // B starts; its meta is not written yet.
    agents.started(&s.dir, "b");
    // Hole B: an agent of no pending launch appears; it retires nothing.
    s.agent("other", "general-purpose", "t-other", "Build it.", 1000, false);
    // B's meta appears.
    s.agent("b", "general-purpose", "tB", brief, 1000, false);
    agents.started(&s.dir, "b");
    // At t0 + 70 s: b runs and A is pending (inside its TTL): full.
    let r = agents.admit(&o, &at("tC"), SystemTime::now(), t0 + Duration::from_secs(70)).expect("b and A's record fill both slots");
    assert!(r.contains("2 of 2 reviewer") && r.contains("1 launched moments ago"), "{r}");
    // At t0 + 130 s A's record is past its TTL; B's record was retired by
    // id, so only b counts. (Before the fix b's start retired A's record and
    // B's own lived on: 2 of 2 here.)
    assert!(agents.admit(&o, &at("tC"), SystemTime::now(), t0 + Duration::from_secs(130)).is_none(), "B's record outlived its start");
}

/// The cost of an Agent PreToolUse admit in a long session: 300 meta files
/// (half finished), with a pending launch, so the pairing pass runs too.
/// Prints the median and p99; the bar is 250 ms.
#[test]
fn admit_at_300_metas_is_cheap() {
    use atlas::orchestrate::{Agents, Launch};
    use std::time::{Duration, Instant, SystemTime};
    let t = Temp::new("orch-admit-cost");
    let s = Session::new(t.path());
    let o = atlas::orchestrate::resolve(Some(&OrchestrateSpec { enabled: Some(true), kinds: vec![(Kind::Builder, KindSpec { max: Some(400), ..Default::default() })], ..Default::default() }), None, None, |k| (k == atlas::orchestrate::LIMIT_ENV).then(|| "1000".to_string()));
    let mut agents = Agents::new(Some(SystemTime::now() - Duration::from_secs(5)));
    for i in 0..300 {
        s.agent(&format!("a{i:03}"), "general-purpose", &format!("t{i:03}"), "Kind: builder\nBuild it.", 1000, i % 2 == 0);
    }
    let tp = s.tp();
    let input = json!({"description": "d", "subagent_type": "general-purpose"});
    let at =
        |id: &'static str| Launch { transcript: &tp, tool_use_id: id, input: &input, brief: "Kind: builder\nBuild more.", store: None };
    // One launch stays pending throughout (its agent never starts).
    assert!(agents.admit(&o, &at("t-pending"), SystemTime::now(), Instant::now()).is_none());
    let mut times = Vec::new();
    for _ in 0..101 {
        let t0 = Instant::now();
        assert!(agents.admit(&o, &at("t-probe"), SystemTime::now(), Instant::now()).is_none());
        times.push(t0.elapsed());
        agents.release_last();
    }
    times.sort();
    let (p50, p99) = (times[50], times[99]);
    println!("admit at 300 metas: median {:.2} ms, p99 {:.2} ms", p50.as_secs_f64() * 1e3, p99.as_secs_f64() * 1e3);
    assert!(p99 < Duration::from_millis(250), "p99 {p99:?}");
}

/// Allow and remind: with orchestrate on, an
/// admitted builder whose prompt declares no `Lane:` gets one context line
/// naming the lines to add and the lane's `brief` entry; it is never denied.
/// A builder with a lane, a reviewer, an Explore agent and an uncounted agent
/// get no line, and orchestrate off says nothing.
#[test]
fn a_builder_with_no_lane_is_reminded_to_declare_one() {
    let t = Temp::new("orch-nolane");
    let s = Session::new(t.path());
    let six = KindSpec { max: Some(6), ..Default::default() };
    let o = OrchestrateSpec { enabled: Some(true), kinds: vec![(Kind::Builder, six.clone()), (Kind::Reviewer, six.clone()), (Kind::Research, six)], ..Default::default() };
    let mut h = Hooks::open(settings(t.path(), &o, &[])).unwrap();
    h.set_command_hook();
    let remind = "declares no lane";
    let a = launch(&mut h, &s, "t1", "general-purpose", "Kind: builder\n\nFix the reader.");
    assert!(a.deny.is_none() && a.context.contains(remind) && a.context.contains("`Lane:`") && a.context.contains("`Uses:`") && a.context.contains("`brief` journal entry"), "{a:?}");
    // The configured default kind (builder), with no Kind: line, is reminded too.
    let a = launch(&mut h, &s, "t2", "general-purpose", "Fix the writer.");
    assert!(a.deny.is_none() && a.context.contains(remind), "{a:?}");
    for (id, ty, prompt) in [
        ("t3", "general-purpose", "Kind: builder\nLane: FEATURE-1\nUses: none\n\nFix the parser."),
        ("t4", "general-purpose", "Kind: reviewer\n\nReview the reader."),
        ("t5", "Explore", "Find where the reader is defined."),
        ("t6", "statusline-setup", "Set up the status line."),
    ] {
        let a = launch(&mut h, &s, id, ty, prompt);
        assert!(a.deny.is_none() && !a.context.contains(remind), "{id}: {a:?}");
    }
    let t2 = Temp::new("orch-nolane-off");
    let s2 = Session::new(t2.path());
    let mut off = Hooks::open(settings(t2.path(), &OrchestrateSpec::default(), &[])).unwrap();
    off.set_command_hook();
    let a = launch(&mut off, &s2, "t1", "general-purpose", "Kind: builder\n\nFix the reader.");
    assert!(a.deny.is_none() && !a.context.contains(remind), "{a:?}");
}

/// The skill's lane declaration example is what the queue reads: every
/// field is classified as declared, and the kind line is read.
#[test]
fn the_skill_s_lane_declaration_round_trips() {
    let skill = include_str!("../skills/orchestrate/SKILL.md");
    let at = skill.find("**Declare a lane**").expect("the skill teaches declaring a lane");
    let block = skill[at..].split("```text\n").nth(1).and_then(|b| b.split("```").next()).expect("an example block");
    let c = atlas::lanes::classify(block, &[]);
    assert_eq!(c.lane.as_deref(), Some("FEATURE-1"));
    assert_eq!(c.lane_src, atlas::lanes::Source::Declared);
    assert_eq!(c.priority_src, atlas::lanes::Source::Declared);
    assert!(!c.uses.is_empty() && c.uses.iter().all(|(_, s)| *s == atlas::lanes::Source::Declared), "{c:?}");
    assert_eq!(c.resources().collect::<Vec<_>>(), ["compile", "worktree:feature-1"]);
    assert_eq!(atlas::orchestrate::brief_kind(block), Some(Kind::Builder));
}

/// The sentence of `doc` that states the SubagentStop backstop ("... back
/// once ..."), with its line breaks folded to spaces.
fn backstop_sentence(doc: &str) -> String {
    let flat = doc.split_whitespace().collect::<Vec<_>>().join(" ");
    let hits: Vec<&str> = flat.split(". ").filter(|s| s.contains("back once")).collect();
    assert_eq!(hits.len(), 1, "one backstop sentence: {hits:?}");
    hits[0].to_string()
}

/// The byte count after "over " and before " bytes" in a backstop sentence.
fn backstop_bytes(sentence: &str) -> usize {
    let (head, _) = sentence.split_once(" bytes").unwrap_or_else(|| panic!("no byte count: {sentence}"));
    let n = head.rsplit_once("over ").unwrap_or_else(|| panic!("no 'over N bytes': {sentence}")).1;
    n.replace(',', "").parse().unwrap_or_else(|e| panic!("{n:?}: {e}"))
}

/// The skill and the wiki state the backstop the code applies: a report over
/// `report::SEND_BACK_OVER` bytes is sent back, with no draft attached.
#[test]
fn the_docs_state_the_send_back_bound_and_no_draft() {
    for (name, doc) in [
        ("skills/orchestrate/SKILL.md", include_str!("../skills/orchestrate/SKILL.md")),
        ("wiki/Orchestration.md", include_str!("../wiki/Orchestration.md")),
    ] {
        let s = backstop_sentence(doc);
        assert_eq!(backstop_bytes(&s), atlas::report::SEND_BACK_OVER, "{name}: {s}");
        assert!(!s.contains("draft"), "{name} promises a draft: {s}");
    }
}

fn start(h: &mut Hooks, s: &Session, id: &str) -> HookAnswer {
    h.handle(&HookEvent { agent_id: id.into(), agent_type: "general-purpose".into(), transcript_path: s.tp(), ..ev("SubagentStart") })
}

fn builders(max: usize) -> OrchestrateSpec {
    OrchestrateSpec { enabled: Some(true), kinds: vec![(Kind::Builder, KindSpec { max: Some(max), ..Default::default() })], ..Default::default() }
}

/// R1 of the #180 re-review: an agent whose meta file names its launch is
/// told that launch's sub-lane or none, whatever order the launches of one
/// message start in; the oldest let-through launch is the answer only when
/// no meta file names a launch yet.
#[test]
fn a_sub_lane_goes_to_its_own_launch_whatever_the_start_order() {
    let t = Temp::new("orch-sublane-order");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(12), &[])).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    // One message: a builder into the held lane, then a reviewer.
    let a = launch(&mut h, &s, "tA", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild two.");
    assert!(a.deny.is_none() && a.context.contains("this launch runs as sub-lane TOOLS/2"), "{a:?}");
    let b = launch(&mut h, &s, "tB", "general-purpose", "Kind: reviewer\nLane: REVIEWS\n\nReview it.");
    assert!(b.deny.is_none() && !b.context.contains("sub-lane"), "{b:?}");
    // The reviewer starts first, its meta file written.
    s.agent("r", "general-purpose", "tB", "Kind: reviewer\nLane: REVIEWS\n\nReview it.", 1000, false);
    let st = start(&mut h, &s, "r");
    assert!(!st.context.contains("sub-lane"), "the reviewer was given the builder's sub-lane: {st:?}");
    s.agent("b2", "general-purpose", "tA", "Kind: builder\nLane: TOOLS\n\nBuild two.", 1000, false);
    let st = start(&mut h, &s, "b2");
    assert!(st.context.contains("you run as sub-lane TOOLS/2 of TOOLS"), "{st:?}");
    // With no meta file yet and one launch waiting, that launch is the
    // answer (journal:ATLAS-HOOK-VALUE:0006); the first tool call, by when
    // the meta file confirms it, adds nothing.
    let c = launch(&mut h, &s, "tC", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild three.");
    assert!(c.context.contains("this launch runs as sub-lane TOOLS/3"), "{c:?}");
    let st = start(&mut h, &s, "c-no-meta");
    assert!(st.context.contains("you run as sub-lane TOOLS/3 of TOOLS"), "{st:?}");
    s.agent("c-no-meta", "general-purpose", "tC", "Kind: builder\nLane: TOOLS\n\nBuild three.", 1000, false);
    let first = tool_call(&mut h, &s, "c-no-meta");
    assert!(!first.context.contains("sub-lane") && !first.context.contains("correction"), "{first:?}");
}

fn tool_call(h: &mut Hooks, s: &Session, id: &str) -> HookAnswer {
    h.handle(&HookEvent { agent_id: id.into(), tool_name: "Bash".into(), transcript_path: s.tp(), ..ev("PreToolUse") })
}

/// N3 of the #180 re-review: a start with no meta file yet takes no launch
/// blindly, so a launch let through and then refused at the permission
/// prompt (a ghost) gives its brief and sub-lane to no one; each agent is
/// matched at its first tool call, by the meta file written by then.
#[test]
fn a_start_with_no_meta_file_never_takes_a_ghosts_launch() {
    let t = Temp::new("orch-ghost-nometa");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(12), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    // Let through as TOOLS/2, then refused at the prompt: no agent.
    let g = launch(&mut h, &s, "tG", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild the ghost.");
    assert!(g.context.contains("this launch runs as sub-lane TOOLS/2"), "{g:?}");
    let r = launch(&mut h, &s, "tR", "general-purpose", "Kind: reviewer\nLane: REVIEWS\n\nReview it.");
    assert!(r.deny.is_none(), "{r:?}");
    // The reviewer starts before its meta file exists.
    let st = start(&mut h, &s, "r");
    assert!(!st.context.contains("sub-lane"), "the reviewer was given the ghost's sub-lane: {st:?}");
    s.agent("r", "general-purpose", "tR", "Kind: reviewer\nLane: REVIEWS\n\nReview it.", 1000, false);
    let first = tool_call(&mut h, &s, "r");
    assert!(!first.context.contains("sub-lane"), "{first:?}");
    // A later builder is still matched to its own launch.
    let c = launch(&mut h, &s, "tC", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild three.");
    assert!(c.context.contains("this launch runs as sub-lane TOOLS/3"), "{c:?}");
    s.agent("c", "general-purpose", "tC", "Kind: builder\nLane: TOOLS\n\nBuild three.", 1000, false);
    let st = start(&mut h, &s, "c");
    assert!(st.context.contains("you run as sub-lane TOOLS/3 of TOOLS") && !st.context.contains("TOOLS/2"), "{st:?}");
}

/// R1 of the #180 re-review, the ghost case: a launch atlas let through and
/// Claude Code then refused (the permission prompt) never starts, and its
/// entry must not shift the starts after it.
#[test]
fn a_launch_refused_after_atlas_answered_never_shifts_a_later_start() {
    let t = Temp::new("orch-sublane-ghost");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(12), &[])).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    // Let through as sub-lane TOOLS/2, then refused at the prompt: no agent.
    let g = launch(&mut h, &s, "tG", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild the ghost.");
    assert!(g.context.contains("this launch runs as sub-lane TOOLS/2"), "{g:?}");
    let b = launch(&mut h, &s, "tR", "general-purpose", "Kind: reviewer\nLane: REVIEWS\n\nReview it.");
    assert!(b.deny.is_none(), "{b:?}");
    s.agent("r", "general-purpose", "tR", "Kind: reviewer\nLane: REVIEWS\n\nReview it.", 1000, false);
    let st = start(&mut h, &s, "r");
    assert!(!st.context.contains("sub-lane"), "the reviewer was given the refused launch's sub-lane: {st:?}");
    let c = launch(&mut h, &s, "tC", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild three.");
    assert!(c.context.contains("this launch runs as sub-lane TOOLS/3"), "{c:?}");
    s.agent("c", "general-purpose", "tC", "Kind: builder\nLane: TOOLS\n\nBuild three.", 1000, false);
    let st = start(&mut h, &s, "c");
    assert!(st.context.contains("you run as sub-lane TOOLS/3 of TOOLS") && !st.context.contains("TOOLS/2"), "{st:?}");
}

/// S6 of the #180 re-review: a launch that never starts holds its class
/// (here its worktree) for later launches and the queue only briefly, and
/// its cap slot for the longer pending TTL. And a launch's sub-lane record
/// is pruned once it is neither pending nor running.
#[test]
fn a_pending_launch_holds_its_class_briefly_and_its_slot_longer() {
    use atlas::orchestrate::{Agents, Launch};
    use std::time::{Duration, Instant, SystemTime};
    let t = Temp::new("orch-class-hold");
    let s = Session::new(t.path());
    let o = atlas::orchestrate::resolve(Some(&builders(1)), None, None, |_| None);
    let tp = s.tp();
    let input = json!({"description": "d", "subagent_type": "general-purpose"});
    let brief = "Kind: builder\nLane: W\nUses: worktree:w\n\nBuild it.";
    let at = |id: &'static str| Launch { transcript: &tp, tool_use_id: id, input: &input, brief, store: None };
    let now = Instant::now();
    let held = |ago: u64| {
        let mut agents = Agents::new(None);
        assert!(agents.admit(&o, &at("tG"), SystemTime::now(), now.checked_sub(Duration::from_secs(ago)).unwrap()).is_none());
        agents.hold_last(atlas::lanes::classify(brief, &[]), None);
        agents
    };
    // Just admitted: it holds the worktree.
    assert_eq!(held(1).pending_holders(&[]).len(), 1);
    // Twenty seconds on, never started: the worktree is free again, so a
    // relaunch is not queued behind it, but its cap slot still counts.
    let mut agents = held(20);
    assert!(agents.pending_holders(&[]).is_empty(), "a refused launch held its worktree for 20 s");
    let r = agents.admit(&o, &at("tH"), SystemTime::now(), Instant::now()).expect("the slot is still held");
    assert!(r.contains("1 launched moments ago"), "{r}");
    // A sub-lane is kept while its agent runs and dropped once it ends.
    let mut agents = Agents::new(None);
    assert!(agents.admit(&o, &at("tA"), SystemTime::now(), Instant::now()).is_none());
    agents.hold_last(atlas::lanes::classify("Kind: builder\nLane: W\n\nBuild it.", &[]), Some("W/2".into()));
    s.agent("a", "general-purpose", "tA", brief, 1000, false);
    agents.started(&s.dir, "a");
    let _ = agents.running(&o, &s.dir, None, SystemTime::now()).unwrap();
    assert_eq!(agents.sub_lane("tA"), Some("W/2"));
    s.agent("a", "general-purpose", "tA", brief, 1000, true);
    let _ = agents.running(&o, &s.dir, None, SystemTime::now()).unwrap();
    assert_eq!(agents.sub_lane("tA"), None, "an ended agent's sub-lane was kept");
}

/// S7 of the #180 re-review: a command hook keeps no record between events,
/// so the line for the orchestrator does not promise that the agent is told
/// its sub-lane; a resident server does tell it.
#[test]
fn the_sub_lane_line_promises_a_telling_only_where_one_comes() {
    let line = |command_hook: bool| {
        let t = Temp::new(if command_hook { "orch-sublane-cmd" } else { "orch-sublane-res" });
        let s = Session::new(t.path());
        let mut h = Hooks::open(settings(t.path(), &builders(12), &[])).unwrap();
        if command_hook {
            h.set_command_hook();
        }
        s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
        // Written after the process started, whatever the file clock reads.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        for f in ["agent-b1.meta.json", "agent-b1.jsonl"] {
            std::fs::File::options().write(true).open(s.dir.join(f)).unwrap().set_modified(later).unwrap();
        }
        let a = launch(&mut h, &s, "t1", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild two.");
        assert!(a.context.contains("this launch runs as sub-lane TOOLS/2"), "{a:?}");
        a.context
    };
    let resident = line(false);
    assert!(resident.contains("its agent is told so when it starts"), "{resident}");
    let command = line(true);
    assert!(command.contains("its agent is not told") && !command.contains("is told so"), "{command}");
}

/// A nit of the #180 re-review: a re-file of a waiting brief by content at
/// a higher priority raises the entry's priority, keeping its brief; a
/// lower one leaves it.
#[test]
fn a_re_file_at_a_higher_priority_raises_the_waiting_entry() {
    let t = Temp::new("orch-dedupe-priority");
    let st = settings(t.path(), &OrchestrateSpec { enabled: Some(true), ..Default::default() }, &[]);
    let mut m = atlas::memory::MemoryStore::open(&st.store).unwrap();
    let file = |m: &mut atlas::memory::MemoryStore, description: &str, prompt: &str| {
        let meta = atlas::lanes::classify(prompt, &[]).meta();
        let l = atlas::queue::Launch { kind: Kind::Builder, lane: Some("A"), description, subagent_type: "general-purpose", prompt, session: "s1", class_meta: &meta };
        atlas::queue::add(m, &l).unwrap()
    };
    let prompt = "Kind: builder\nLane: A\n\nRender the wreck.";
    let (key, new) = file(&mut m, "Render the wreck", prompt);
    assert!(new);
    let priority = |m: &atlas::memory::MemoryStore| atlas::queue::qmeta(&m.head(&key).unwrap().unwrap().meta).priority.map(|p| p.0);
    assert_eq!(priority(&m), Some(atlas::lanes::Priority::Normal));
    let waited = m.head(&key).unwrap().unwrap().time;
    assert_eq!(file(&mut m, "Render it now", "Kind: builder\nLane: B\nPriority: urgent\n\nRender the wreck."), (key.clone(), false));
    assert_eq!(priority(&m), Some(atlas::lanes::Priority::Urgent));
    let h = m.head(&key).unwrap().unwrap();
    assert_eq!((h.body.as_str(), h.time.as_str()), (prompt, waited.as_str()), "the brief or the wait changed");
    assert_eq!(file(&mut m, "Render it later", "Kind: builder\nLane: B\nPriority: background\n\nRender the wreck."), (key.clone(), false));
    assert_eq!(priority(&m), Some(atlas::lanes::Priority::Urgent));
}

/// S3 of the #185 review: an agent in sub-lane `X/2` whose brief names `X`
/// gets the parent's view (which holds its own records) and, like an agent
/// given its own lane's view, no pointers beside it.
#[test]
fn a_sub_lane_agent_given_its_parent_view_gets_no_pointers() {
    let t = Temp::new("orch-sublane-own-view");
    let s = Session::new(t.path());
    let mut st = settings(t.path(), &builders(12), &[]);
    st.recall.fts_floor = 0.0;
    st.recall.hybrid_fts_floor = 0.0;
    {
        let mut m = atlas::memory::MemoryStore::open(&st.store).unwrap();
        let mut w = m.writer().unwrap();
        let mut add = |key: &str, kind: &str, sub: &str, scope: &str, title: &str, body: &str| {
            let d = atlas::memory::Draft { title: title.into(), body: body.into(), reason: "test".into(), author: "t".into(), client: "t".into(), ..Default::default() };
            w.create(key, kind, sub, scope, &d, None).unwrap();
        };
        add("journal:TOOLS:0001", "journal", "next", "TOOLS", "Wire the walrus parser", "");
        add("rules:r00001", "rule", "", "rules", "walrus parser rule", "- the walrus parser caches tokens in the walrus table.\n");
        w.commit().unwrap();
    }
    let mut h = Hooks::open(st).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    let brief = "Kind: builder\nLane: TOOLS\n\nFix how the walrus parser caches tokens in the walrus table.";
    let a = launch(&mut h, &s, "tA", "general-purpose", brief);
    assert!(a.context.contains("this launch runs as sub-lane TOOLS/2"), "{a:?}");
    s.agent("b2", "general-purpose", "tA", brief, 1000, false);
    let st = start(&mut h, &s, "b2");
    assert!(st.context.contains("you run as sub-lane TOOLS/2") && st.context.contains("Wire the walrus parser"), "{st:?}");
    assert!(!st.context.contains("atlas recall (related"), "pointers beside the parent's view: {}", st.context);
}

// ---- #188 review: the harness order (start, then the meta file, then the
// first tool call; journal:ATLAS-HOOK-VALUE:0006)

use atlas::memory::settings::SubagentContext;

impl Session {
    /// Records in the session transcript that launch `id` was refused at
    /// the permission prompt, as Claude Code writes it.
    fn refuse(&self, id: &str) {
        let r = json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "The user doesn't want to proceed with this tool use. The tool use was rejected.", "is_error": true, "tool_use_id": id}]}});
        let mut f = std::fs::File::options().append(true).open(&self.parent).unwrap();
        std::io::Write::write_all(&mut f, format!("{r}\n").as_bytes()).unwrap();
    }
}

/// A start that guessed wrong is told at its first tool call that the
/// sub-lane its start named was another launch's; that launch's own agent
/// is then told it.
#[test]
fn a_wrong_guess_s_sub_lane_is_corrected_at_the_first_tool_call() {
    let t = Temp::new("orch-wrong-guess");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(12), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    let a = launch(&mut h, &s, "tA", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild two.");
    assert!(a.context.contains("this launch runs as sub-lane TOOLS/2"), "{a:?}");
    // A reviewer whose launch this server never saw (let through before a
    // restart) starts first, while tA is the one launch waiting.
    let st = start(&mut h, &s, "r");
    assert!(st.context.contains("you run as sub-lane TOOLS/2 of TOOLS"), "{st:?}");
    s.agent("r", "general-purpose", "tR", "Kind: reviewer\nLane: REVIEWS\n\nReview it.", 1000, false);
    let first = tool_call(&mut h, &s, "r");
    assert!(first.context.starts_with("atlas: correction:") && first.context.contains("the sub-lane it named"), "{first:?}");
    assert!(!first.context.contains("you run as sub-lane"), "{first:?}");
    // tA goes back to the queue, for its own agent.
    let st = start(&mut h, &s, "b2");
    assert!(st.context.contains("you run as sub-lane TOOLS/2 of TOOLS"), "{st:?}");
    s.agent("b2", "general-purpose", "tA", "Kind: builder\nLane: TOOLS\n\nBuild two.", 1000, false);
    let first = tool_call(&mut h, &s, "b2");
    assert!(!first.context.contains("correction") && !first.context.contains("sub-lane"), "{first:?}");
}

/// A launch refused at the permission prompt, once the refusal is in the
/// session transcript, no longer makes a start ambiguous: the real launch
/// beside it is answered at its start, with its own sub-lane.
#[test]
fn a_refusal_in_the_transcript_leaves_the_real_launch_unambiguous() {
    let t = Temp::new("orch-refused-real");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(12), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    let g = launch(&mut h, &s, "tG", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild the ghost.");
    assert!(g.context.contains("this launch runs as sub-lane TOOLS/2"), "{g:?}");
    s.refuse("tG");
    let c = launch(&mut h, &s, "tC", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild three.");
    assert!(c.context.contains("this launch runs as sub-lane TOOLS/3"), "{c:?}");
    let st = start(&mut h, &s, "c");
    assert!(st.context.contains("you run as sub-lane TOOLS/3 of TOOLS") && !st.context.contains("TOOLS/2"), "{st:?}");
    s.agent("c", "general-purpose", "tC", "Kind: builder\nLane: TOOLS\n\nBuild three.", 1000, false);
    assert!(!tool_call(&mut h, &s, "c").context.contains("sub-lane"));
}

/// A Binding compiler that numbers its blocks, so a test can tell a block
/// stored at let-through from one compiled later.
fn numbered_binding() -> atlas::hooks::BindingFn {
    let n = std::sync::atomic::AtomicUsize::new(0);
    Box::new(move |_, brief, lane| Ok(format!("block {} of {lane}: {}\n", n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1, brief.lines().last().unwrap_or(""))))
}

/// S2 of the #188 review: launches held at the permission prompt for five
/// minutes (past the 120 s pending TTL) keep their stored Binding blocks and
/// their sub-lanes until their agents' first tool calls.
#[test]
fn a_launch_held_at_the_prompt_past_the_pending_ttl_keeps_its_brief_and_sub_lane() {
    let t = Temp::new("orch-held-prompt");
    let s = Session::new(t.path());
    let mut st = settings(t.path(), &builders(12), &[]);
    st.recall.subagent_context = SubagentContext::Binding;
    let mut h = Hooks::open(st).unwrap();
    h.set_binding(Some(numbered_binding()));
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    let a = launch(&mut h, &s, "tA", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild two.");
    assert!(a.context.contains("this launch runs as sub-lane TOOLS/2"), "{a:?}");
    let b = launch(&mut h, &s, "tB", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild three.");
    assert!(b.context.contains("this launch runs as sub-lane TOOLS/3"), "{b:?}");
    h.age_launches(std::time::Duration::from_secs(300));
    // Later launches prune what has lapsed.
    launch(&mut h, &s, "tC", "general-purpose", "Kind: reviewer\nLane: REVIEWS\n\nReview one.");
    launch(&mut h, &s, "tD", "general-purpose", "Kind: reviewer\nLane: REVIEWS\n\nReview two.");
    // Both are approved, and both start before their meta files.
    assert_eq!(start(&mut h, &s, "x").context, "");
    assert_eq!(start(&mut h, &s, "y").context, "");
    s.agent("x", "general-purpose", "tA", "Kind: builder\nLane: TOOLS\n\nBuild two.", 1000, false);
    s.agent("y", "general-purpose", "tB", "Kind: builder\nLane: TOOLS\n\nBuild three.", 1000, false);
    let x = tool_call(&mut h, &s, "x").context;
    assert!(x.contains("block 1 of TOOLS: Build two.") && x.contains("you run as sub-lane TOOLS/2 of TOOLS"), "{x}");
    let y = tool_call(&mut h, &s, "y").context;
    assert!(y.contains("block 2 of TOOLS: Build three.") && y.contains("you run as sub-lane TOOLS/3 of TOOLS"), "{y}");
}

/// N1 of the #188 review: in binding mode the first tool call's whole
/// answer, its other lines included, stays under the 9,000-character cap.
#[test]
fn a_first_tool_call_answer_stays_under_the_cap_beside_other_lines() {
    let t = Temp::new("orch-carrier-cap");
    let s = Session::new(t.path());
    let mut st = settings(t.path(), &builders(12), &[]);
    st.recall.subagent_context = SubagentContext::Binding;
    let mut h = Hooks::open(st).unwrap();
    let line = |i: usize| format!("- L{i:02} {}\n", "x".repeat(993));
    h.set_binding(Some(Box::new(move |_, _, _| Ok((0..20).map(line).collect()))));
    launch(&mut h, &s, "tA", "general-purpose", "Kind: reviewer\nLane: ALPHA\n\nReview alpha.");
    launch(&mut h, &s, "tB", "general-purpose", "Kind: reviewer\nLane: BETA\n\nReview beta.");
    assert_eq!(start(&mut h, &s, "x").context, "");
    // Past its handoff point by its first tool call: the cap adds a line.
    s.agent("x", "general-purpose", "tA", "Kind: reviewer\nLane: ALPHA\n\nReview alpha.", 300_000, false);
    let c = tool_call(&mut h, &s, "x").context;
    assert!(c.contains("handoff point") && c.contains("- L00 "), "{c}");
    assert!(c.chars().count() <= atlas::hooks::SUBAGENT_CONTEXT_CHARS, "{} characters", c.chars().count());
}

/// N2 of the #188 review: the sub-lane line follows a body that ends in a
/// newline with no blank line between.
#[test]
fn the_sub_lane_line_follows_its_body_with_no_blank_line() {
    let t = Temp::new("orch-sub-line-gap");
    let s = Session::new(t.path());
    let mut st = settings(t.path(), &builders(12), &[]);
    st.recall.subagent_context = SubagentContext::Binding;
    let mut h = Hooks::open(st).unwrap();
    h.set_binding(Some(Box::new(|_, brief, lane| Ok(format!("atlas binding: lane {lane}\n- task: {}\n", brief.lines().last().unwrap_or(""))))));
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    launch(&mut h, &s, "tA", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild two.");
    s.agent("b2", "general-purpose", "tA", "Kind: builder\nLane: TOOLS\n\nBuild two.", 1000, false);
    let st = start(&mut h, &s, "b2").context;
    assert!(st.starts_with("atlas binding: lane TOOLS\n- task: Build two.\natlas: you run as sub-lane TOOLS/2 of TOOLS"), "{st}");
}

// ---- #188 review, round 2

impl Session {
    /// Agent `id`'s transcript (its brief) with no meta file yet.
    fn transcript_only(&self, id: &str, brief: &str) {
        let user = json!({"type": "user", "message": {"role": "user", "content": brief}});
        std::fs::write(self.dir.join(format!("agent-{id}.jsonl")), format!("{user}\n")).unwrap();
    }

    /// Appends `bytes` of other turns to the session transcript.
    fn pad(&self, bytes: usize) {
        let line = json!({"type": "assistant", "message": {"role": "assistant", "content": "x".repeat(1000)}}).to_string();
        let n = bytes / (line.len() + 1) + 1;
        let mut f = std::fs::File::options().append(true).open(&self.parent).unwrap();
        std::io::Write::write_all(&mut f, format!("{line}\n").repeat(n).as_bytes()).unwrap();
    }
}

/// A Binding compiler that names the lane and the brief's last line.
fn lane_binding() -> atlas::hooks::BindingFn {
    Box::new(|_, brief, lane| Ok(format!("atlas binding: lane {lane}\n- task: {}\n", brief.lines().last().unwrap_or(""))))
}

/// Binding-mode hooks with one builder already running in TOOLS, so the
/// next launches run as TOOLS/2, TOOLS/3 and so on.
fn binding_hooks(t: &Temp, s: &Session) -> Hooks {
    let mut st = settings(t.path(), &builders(12), &[]);
    st.recall.subagent_context = SubagentContext::Binding;
    let mut h = Hooks::open(st).unwrap();
    h.set_binding(Some(lane_binding()));
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    h
}

const TWO: &str = "Kind: builder\nLane: TOOLS\n\nBuild two.";
const THREE: &str = "Kind: builder\nLane: TOOLS\n\nBuild three.";

/// M1: an ambiguous start whose agent's first tool call comes before its
/// meta file (and transcript) is answered at the next tool call, with its
/// block and its sub-lane, and its launch leaves the queue.
#[test]
fn a_first_tool_call_before_the_meta_file_is_answered_at_the_next_one() {
    let t = Temp::new("orch-m1-deferred");
    let s = Session::new(t.path());
    let mut h = binding_hooks(&t, &s);
    assert!(launch(&mut h, &s, "tA", "general-purpose", TWO).context.contains("sub-lane TOOLS/2"));
    assert!(launch(&mut h, &s, "tB", "general-purpose", THREE).context.contains("sub-lane TOOLS/3"));
    assert_eq!(start(&mut h, &s, "x").context, "");
    assert_eq!(start(&mut h, &s, "y").context, "");
    // x's first tool call beats Claude Code's write of its meta file.
    assert_eq!(tool_call(&mut h, &s, "x").context, "");
    s.agent("x", "general-purpose", "tA", TWO, 1000, false);
    let c = tool_call(&mut h, &s, "x").context;
    assert!(c.contains("atlas binding: lane TOOLS\n- task: Build two.\n") && c.contains("you run as sub-lane TOOLS/2 of TOOLS"), "{c}");
    assert!(!tool_call(&mut h, &s, "x").context.contains("atlas binding"));
    // tA has left the queue: y is answered with its own.
    s.agent("y", "general-purpose", "tB", THREE, 1000, false);
    let c = tool_call(&mut h, &s, "y").context;
    assert!(c.contains("- task: Build three.") && c.contains("you run as sub-lane TOOLS/3 of TOOLS"), "{c}");
}

/// M1, the guessed case: a guess whose agent's first tool call comes before
/// its meta file is checked once the meta file is there, not left to stand.
#[test]
fn a_guess_whose_first_tool_call_beats_its_meta_file_is_checked_later() {
    let t = Temp::new("orch-m1-guess");
    let s = Session::new(t.path());
    let mut h = binding_hooks(&t, &s);
    launch(&mut h, &s, "tA", "general-purpose", TWO);
    // r's launch was let through before a restart: tA is the one waiting.
    let st = start(&mut h, &s, "r").context;
    assert!(st.contains("- task: Build two.") && st.contains("sub-lane TOOLS/2"), "{st}");
    assert_eq!(tool_call(&mut h, &s, "r").context, "");
    s.agent("r", "general-purpose", "tR", "Kind: reviewer\nLane: REVIEWS\n\nReview it.", 1000, false);
    let c = tool_call(&mut h, &s, "r").context;
    assert!(c.starts_with("atlas: correction:") && c.contains("the sub-lane it named"), "{c}");
    assert!(c.contains("lane REVIEWS\n- task: Review it.") && !c.contains("Build two"), "{c}");
    // tA waits again for its own agent.
    let st = start(&mut h, &s, "b2").context;
    assert!(st.contains("- task: Build two.") && st.contains("sub-lane TOOLS/2 of TOOLS"), "{st}");
}

/// M1, the bound: an agent whose meta file never comes is retried for
/// FIRST_CALL_TRIES tool calls, then answered from its transcript; a launch
/// whose brief is that transcript's is released, not stranded.
#[test]
fn a_meta_file_that_never_comes_releases_the_launch_after_the_retries() {
    let t = Temp::new("orch-m1-bound");
    let s = Session::new(t.path());
    let mut h = binding_hooks(&t, &s);
    launch(&mut h, &s, "tA", "general-purpose", TWO);
    launch(&mut h, &s, "tB", "general-purpose", THREE);
    assert_eq!(start(&mut h, &s, "x").context, "");
    assert_eq!(start(&mut h, &s, "y").context, "");
    for _ in 0..atlas::hooks::FIRST_CALL_TRIES {
        assert_eq!(tool_call(&mut h, &s, "x").context, "");
    }
    s.transcript_only("x", TWO);
    let c = tool_call(&mut h, &s, "x").context;
    assert!(c.contains("- task: Build two.") && c.contains("you run as sub-lane TOOLS/2 of TOOLS"), "{c}");
    assert!(!tool_call(&mut h, &s, "x").context.contains("atlas binding"));
    // tA is gone: tB is the one launch waiting, so a start takes it.
    let st = start(&mut h, &s, "z").context;
    assert!(st.contains("- task: Build three."), "{st}");
}

/// S1: a refusal written more than 1 MiB of transcript ago is still seen.
#[test]
fn a_refusal_older_than_a_mebibyte_of_transcript_is_seen() {
    let t = Temp::new("orch-s1-old");
    let s = Session::new(t.path());
    let mut h = binding_hooks(&t, &s);
    launch(&mut h, &s, "tG", "general-purpose", TWO);
    s.refuse("tG");
    s.pad(3 << 20);
    launch(&mut h, &s, "tC", "general-purpose", THREE);
    let st = start(&mut h, &s, "c").context;
    assert!(st.contains("- task: Build three.") && st.contains("sub-lane TOOLS/3 of TOOLS") && !st.contains("TOOLS/2"), "{st}");
}

/// S1: a refused launch waiting alone is scanned too: a start whose own
/// launch this server never saw is not given the ghost's context.
#[test]
fn a_lone_refused_launch_is_given_to_no_one() {
    let t = Temp::new("orch-s1-lone");
    let s = Session::new(t.path());
    let mut h = binding_hooks(&t, &s);
    launch(&mut h, &s, "tG", "general-purpose", TWO);
    s.refuse("tG");
    assert_eq!(start(&mut h, &s, "r").context, "");
}

/// N4: a SubagentStart with no agent type (compaction's summariser) takes
/// no launch; the launch stays for its own agent.
#[test]
fn a_start_with_no_agent_type_takes_no_launch() {
    let t = Temp::new("orch-n4-type");
    let s = Session::new(t.path());
    let mut h = binding_hooks(&t, &s);
    launch(&mut h, &s, "tA", "general-purpose", TWO);
    let st = h.handle(&HookEvent { agent_id: "summ".into(), transcript_path: s.tp(), ..ev("SubagentStart") });
    assert_eq!(st.context, "");
    let st = start(&mut h, &s, "a").context;
    assert!(st.contains("- task: Build two.") && st.contains("sub-lane TOOLS/2 of TOOLS"), "{st}");
}

/// #204: a dead agent's files, last written `ago` before now.
fn written(s: &Session, id: &str, at: std::time::SystemTime) {
    for f in [format!("agent-{id}.meta.json"), format!("agent-{id}.jsonl")] {
        std::fs::File::options().write(true).open(s.dir.join(f)).unwrap().set_modified(at).unwrap();
    }
}

/// #204: appends a tool error to agent `id`'s transcript, its last line.
fn tool_error(s: &Session, id: &str, text: &str) {
    use std::io::Write;
    let line = json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": text}]}});
    let mut f = std::fs::File::options().append(true).open(s.dir.join(format!("agent-{id}.jsonl"))).unwrap();
    writeln!(f, "{line}").unwrap();
}

fn running_ids(o: &atlas::orchestrate::OrchSettings, s: &Session, now: std::time::SystemTime) -> Vec<String> {
    running_in(o, s, None, now)
}

/// The running agents' ids as a process with the memory store `store` counts them.
fn running_in(
    o: &atlas::orchestrate::OrchSettings,
    s: &Session,
    store: Option<&atlas::memory::MemoryStore>,
    now: std::time::SystemTime,
) -> Vec<String> {
    atlas::orchestrate::Agents::new(None).running(o, &s.dir, store, now).unwrap().into_iter().map(|r| r.id).collect()
}

/// The file named `name` under `root`, at any depth.
fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

/// #204 (2): a SubagentStop is atlas's own record of a finish, so an agent
/// whose meta file never got `"finished": true` (the disk was full) gives
/// its slot up at its stop, not `stale_minutes` later.
#[test]
fn a_subagent_stop_frees_the_slot_when_the_meta_was_never_finished() {
    let t = Temp::new("orch-204-stop");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(1), &[])).unwrap();
    // File times are coarser than the process clock (see the first cap test).
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: DISK\n\nBuild one.", 1000, false);
    let d = launch(&mut h, &s, "t2", "general-purpose", "Kind: builder\nLane: OTHER\n\nBuild two.");
    assert!(d.deny.as_deref().is_some_and(|r| r.contains("1 of 1 builder")), "{d:?}");
    let stop = h.handle(&HookEvent { agent_id: "b1".into(), agent_type: "general-purpose".into(), last_message: "done".into(), transcript_path: s.tp(), ..ev("SubagentStop") });
    assert!(stop.block.is_none(), "{stop:?}");
    let a = launch(&mut h, &s, "t3", "general-purpose", "Kind: builder\nLane: OTHER\n\nBuild two.");
    assert!(a.deny.is_none(), "{a:?}");
    // #207 review S2: the finish is in the store too, so another process
    // (atlas queue next, atlas backlog, a command hook) sees the slot free.
    let o = atlas::orchestrate::resolve(Some(&builders(1)), None, None, |_| None);
    assert!(running_in(&o, &s, Some(h.store()), std::time::SystemTime::now()).is_empty());
    assert_eq!(running_ids(&o, &s, std::time::SystemTime::now()), ["b1"]);
}

/// #204 (3): an agent whose transcript ends in a tool error that says the
/// disk is full stops counting after `orchestrate.disk_full_minutes`
/// (default 10), not `stale_minutes`; any other last line keeps the long window.
#[test]
fn a_transcript_ending_in_enospc_stops_counting_after_the_short_window() {
    let t = Temp::new("orch-204-enospc");
    let s = Session::new(t.path());
    let o = atlas::orchestrate::resolve(Some(&builders(4)), None, None, |_| None);
    s.agent("full", "general-purpose", "t-full", "Build one.", 1000, false);
    tool_error(&s, "full", "Error: ENOSPC: no space left on device, write");
    s.agent("lower", "general-purpose", "t-lower", "Build two.", 1000, false);
    tool_error(&s, "lower", "bash: echo: write error: No space left on device");
    s.agent("other", "general-purpose", "t-other", "Build three.", 1000, false);
    tool_error(&s, "other", "Exit code 101: test failed");
    s.agent("fine", "general-purpose", "t-fine", "Build four.", 1000, false);
    let now = std::time::SystemTime::now();
    let min = |m: u64| now + std::time::Duration::from_secs(m * 60);
    assert_eq!(running_ids(&o, &s, min(5)), ["fine", "full", "lower", "other"]);
    assert_eq!(running_ids(&o, &s, min(11)), ["fine", "other"]);
    // The window is a setting; 0 turns the rule off.
    let c = atlas::config::Config::parse(r#"{"orchestrate": {"enabled": true, "disk_full_minutes": 3}}"#, Path::new(".")).unwrap();
    let o3 = atlas::orchestrate::resolve(c.orchestrate.as_ref(), None, None, |_| None);
    assert_eq!(running_ids(&o3, &s, min(5)), ["fine", "other"]);
    let off = OrchestrateSpec { disk_full_minutes: Some(0), ..builders(4) };
    let o0 = atlas::orchestrate::resolve(Some(&off), None, None, |_| None);
    assert_eq!(running_ids(&o0, &s, min(30)), ["fine", "full", "lower", "other"]);
}

/// #204 (1): `atlas queue release AGENT` records a dead agent finished, so
/// its lane, slot and resources are free at once. It refuses an agent that
/// wrote in the last few minutes unless `--force`; a release is undone by
/// the agent writing again.
#[test]
fn queue_release_frees_a_dead_holder_and_refuses_a_live_one() {
    let t = Temp::new("orch-204-release");
    let home = t.path().join("home");
    let claude = home.join("claude");
    let s = Session::new(&claude.join("projects"));
    let run = |args: &[&str]| -> (bool, String) {
        let o = std::process::Command::new(bin())
            .arg("queue")
            .args(args)
            .env("ATLAS_HOME", &home)
            .env_remove("ATLAS_CONFIG")
            .env("CLAUDE_CONFIG_DIR", &claude)
            .output()
            .unwrap();
        (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    };
    let o = atlas::orchestrate::resolve(Some(&builders(4)), None, None, |_| None);
    let now = std::time::SystemTime::now();
    s.agent("dead1", "general-purpose", "t-dead1", "Kind: builder\nLane: ES1\n\nBuild one.", 1000, false);
    written(&s, "dead1", now - std::time::Duration::from_secs(20 * 60));
    s.agent("live1", "general-purpose", "t-live1", "Kind: builder\nLane: ES2\n\nBuild two.", 1000, false);
    assert_eq!(running_ids(&o, &s, now), ["dead1", "live1"]);

    // The folder is found from Claude Code's projects folder without --transcript.
    let (ok, out) = run(&["release", "dead1"]);
    assert!(ok && out.contains("released agent dead1") && out.contains("job dead1"), "{out}");
    // The record is in the CLI's memory store, read here through another handle.
    let store = atlas::memory::MemoryStore::open(&find_file(&home, "memory.sqlite").expect("the CLI's memory store")).unwrap();
    let running_ids = |o: &atlas::orchestrate::OrchSettings, s: &Session, now: std::time::SystemTime| running_in(o, s, Some(&store), now);
    assert_eq!(running_ids(&o, &s, std::time::SystemTime::now()), ["live1"]);
    // Claude Code's meta file is never edited.
    assert!(!std::fs::read_to_string(s.dir.join("agent-dead1.meta.json")).unwrap().contains("finished"));

    // A live agent is refused unless --force, here through --transcript.
    let (ok, out) = run(&["release", "agent-live1", "--transcript", &s.tp()]);
    assert!(!ok && out.contains("wrote") && out.contains("--force"), "{out}");
    assert_eq!(running_ids(&o, &s, std::time::SystemTime::now()), ["live1"]);
    let (ok, out) = run(&["release", "live1", "--force", "--transcript", &s.tp()]);
    assert!(ok && out.contains("released agent live1"), "{out}");
    assert!(running_ids(&o, &s, std::time::SystemTime::now()).is_empty());

    // An agent that writes again after its release counts again.
    written(&s, "dead1", std::time::SystemTime::now() + std::time::Duration::from_secs(5));
    assert_eq!(running_ids(&o, &s, std::time::SystemTime::now() + std::time::Duration::from_secs(6)), ["dead1"]);

    // #207 review N4: an agent's own transcript is not the session's.
    let own = s.dir.join("agent-live1.jsonl").to_string_lossy().into_owned();
    let (ok, out) = run(&["release", "live1", "--transcript", &own]);
    assert!(!ok && out.contains("the session transcript"), "{out}");
    let (ok, out) = run(&["release", "nosuch"]);
    assert!(!ok && out.contains("no agent nosuch"), "{out}");
    let (ok, out) = run(&["release"]);
    assert!(!ok && out.contains("usage: atlas queue"), "{out}");
    let (ok, out) = run(&["release", "--help"]);
    assert!(ok && out.contains("release AGENT [--force] [--transcript PATH]"), "{out}");
}

/// #207 review M1-M3: the lines Claude Code writes after a tool result, read
/// from real 2.1.322 transcripts (their structure only): a PostToolUse hook
/// attachment, then the next request's `api-request` line, both before the
/// model answers. A dead agent's last line is one of these, never the result.
fn hook_line(id: &str, event: &str) -> Value {
    json!({"type": "attachment", "isSidechain": true, "agentId": id, "uuid": "u-hook", "parentUuid": "u-result", "timestamp": "2026-11-14T10:00:01.000Z",
        "attachment": {"type": "hook_success", "hookEvent": event, "hookName": format!("{event}:Bash"), "toolUseID": "toolu_1", "command": "atlas hook", "content": "", "stdout": "", "stderr": "", "durationMs": 12}})
}

fn request_line(id: &str) -> Value {
    json!({"type": "api-request", "id": "req-1", "agentId": id, "sessionId": "s1", "querySource": "agent", "params": {}, "tail": [], "timestamp": "2026-11-14T10:00:02.000Z"})
}

/// Appends a tool error `text` for agent `id`, then `after`, each a line,
/// to its transcript.
fn tail_lines(s: &Session, id: &str, text: &str, after: &[Value]) {
    use std::io::Write;
    let result = json!({"type": "user", "isSidechain": true, "agentId": id, "uuid": "u-result", "timestamp": "2026-11-14T10:00:00.000Z",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": text}]},
        "toolUseResult": format!("Error: {text}"), "sourceToolAssistantUUID": "u-call"});
    let mut f = std::fs::File::options().append(true).open(s.dir.join(format!("agent-{id}.jsonl"))).unwrap();
    writeln!(f, "{result}").unwrap();
    for l in after {
        writeln!(f, "{l}").unwrap();
    }
}

/// #207 review M1-M3, N2: the disk-full rule reads the last message line,
/// past the hook and request lines that follow a tool result, matches the
/// Windows text, reads further back for a line longer than the first read,
/// and keeps counting an agent that answered the error.
#[test]
fn a_disk_full_error_under_real_tail_lines_still_stops_counting() {
    use std::io::Write;
    let t = Temp::new("orch-207-tail");
    let s = Session::new(t.path());
    let o = atlas::orchestrate::resolve(Some(&builders(9)), None, None, |_| None);
    let enospc = "Error: ENOSPC: no space left on device, write";
    // A successful result is followed by its PostToolUse hook, then a request.
    s.agent("hooked", "general-purpose", "t1", "Build.", 1000, false);
    tail_lines(&s, "hooked", enospc, &[hook_line("hooked", "PostToolUse"), request_line("hooked")]);
    // An error result is followed by the request at once.
    s.agent("errored", "general-purpose", "t2", "Build.", 1000, false);
    tail_lines(&s, "errored", "bash: echo: write error: No space left on device", &[request_line("errored")]);
    // Windows: Rust std, cargo, rustc and linkers report os error 112.
    s.agent("win", "general-purpose", "t3", "Build.", 1000, false);
    tail_lines(&s, "win", "error: failed to write `target/debug/deps/x.rlib`: There is not enough space on the disk. (os error 112)", &[request_line("win")]);
    s.agent("win112", "general-purpose", "t4", "Build.", 1000, false);
    tail_lines(&s, "win112", "LINK : fatal error LNK1180: insufficient disk space (os error 112)", &[request_line("win112")]);
    s.agent("wincase", "general-purpose", "t5", "Build.", 1000, false);
    tail_lines(&s, "wincase", "THERE IS NOT ENOUGH SPACE ON THE DISK.", &[request_line("wincase")]);
    // A result line longer than the first tail read (stdout carried twice).
    s.agent("long", "general-purpose", "t6", "Build.", 1000, false);
    tail_lines(&s, "long", &format!("{}\n{enospc}", "x".repeat(100 * 1024)), &[request_line("long")]);
    // A last line torn by the full disk does not hide the error.
    s.agent("torn", "general-purpose", "t7", "Build.", 1000, false);
    tail_lines(&s, "torn", enospc, &[request_line("torn")]);
    std::fs::OpenOptions::new().append(true).open(s.dir.join("agent-torn.jsonl")).unwrap().write_all(b"{\"type\":\"attachm").unwrap();
    // An agent that answered the error is alive: it keeps the long window.
    s.agent("answered", "general-purpose", "t8", "Build.", 1000, false);
    let reply = json!({"type": "assistant", "agentId": "answered", "message": {"role": "assistant", "content": [{"type": "text", "text": "Freeing space."}]}});
    tail_lines(&s, "answered", enospc, &[request_line("answered"), reply, hook_line("answered", "PreToolUse")]);
    // Any other error keeps the long window too.
    s.agent("other", "general-purpose", "t9", "Build.", 1000, false);
    tail_lines(&s, "other", "Exit code 101: test failed", &[request_line("other")]);
    let now = std::time::SystemTime::now();
    let min = |m: u64| now + std::time::Duration::from_secs(m * 60);
    assert_eq!(running_ids(&o, &s, min(5)), ["answered", "errored", "hooked", "long", "other", "torn", "win", "win112", "wincase"]);
    assert_eq!(running_ids(&o, &s, min(11)), ["answered", "other"]);
}

/// #207 review S1: an interim stop (the agent waits on its own background
/// work) fires SubagentStop too, and its notification undoes the finish
/// the stop recorded, in memory and on disk, whether or not Claude Code
/// writes to the transcript after the stop.
#[test]
fn an_interim_stop_keeps_the_agent_counted() {
    let t = Temp::new("orch-207-interim");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(1), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: WAIT\n\nBuild one.", 1000, false);
    let o = atlas::orchestrate::resolve(Some(&builders(1)), None, None, |_| None);
    let stop = h.handle(&HookEvent { agent_id: "b1".into(), agent_type: "general-purpose".into(), last_message: "waiting".into(), transcript_path: s.tp(), ..ev("SubagentStop") });
    assert!(stop.block.is_none(), "{stop:?}");
    let note = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n<summary>Agent \"x\" is waiting on its own background work</summary>\n<note>This agent stopped with background work of its own still running. It may resume on its own when that work completes or reports, and the same task-id notifies again if it does; the result below may be interim.</note>\n<result>r</result>\n</task-notification>";
    let _ = h.handle(&HookEvent { prompt: note.into(), transcript_path: s.tp(), ..ev("UserPromptSubmit") });
    let d = launch(&mut h, &s, "t2", "general-purpose", "Kind: builder\nLane: OTHER\n\nBuild two.");
    assert!(d.deny.as_deref().is_some_and(|r| r.contains("1 of 1 builder") && r.contains("job b1")), "{d:?}");
    // Another process (atlas queue next, a command hook) counts it too.
    assert_eq!(running_in(&o, &s, Some(h.store()), std::time::SystemTime::now()), ["b1"]);
    // Its real finish frees it.
    let _ = finished(&mut h, &s, "b1");
    assert!(launch(&mut h, &s, "t3", "general-purpose", "Kind: builder\nLane: OTHER\n\nBuild two.").deny.is_none());
}

// ---- #222: dead holders follow-up

/// A successful tool result line, as Claude Code writes one.
fn ok_result(id: &str) -> Value {
    json!({"type": "user", "isSidechain": true, "agentId": id, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2", "content": "ok"}]}})
}

/// #222 R1: parallel tool calls answer with one result line each after the
/// one assistant line; a disk-full error among them counts though a good
/// result follows it. One before the last assistant line does not.
#[test]
fn a_disk_full_result_among_parallel_results_reads_as_disk_full() {
    let t = Temp::new("orch-222-parallel");
    let s = Session::new(t.path());
    let o = atlas::orchestrate::resolve(Some(&builders(9)), None, None, |_| None);
    let enospc = "Error: ENOSPC: no space left on device, write";
    s.agent("par", "general-purpose", "t1", "Build.", 1000, false);
    tail_lines(&s, "par", enospc, &[ok_result("par"), request_line("par")]);
    s.agent("answered", "general-purpose", "t2", "Build.", 1000, false);
    let reply = json!({"type": "assistant", "agentId": "answered", "message": {"role": "assistant", "content": [{"type": "text", "text": "Freed space."}]}});
    tail_lines(&s, "answered", enospc, &[reply, ok_result("answered"), request_line("answered")]);
    let now = std::time::SystemTime::now();
    let min = |m: u64| now + std::time::Duration::from_secs(m * 60);
    assert_eq!(running_ids(&o, &s, min(5)), ["answered", "par"]);
    assert_eq!(running_ids(&o, &s, min(11)), ["answered"]);
}

/// A SubagentStop as the plugin's `mcp_tool` hook sends it: every field is
/// text, and `background_tasks` is the array's JSON text (or the
/// unsubstituted `${background_tasks}` when the input lacks it).
fn stop_input(s: &Session, id: &str, tasks: Option<&str>) -> HookEvent {
    let mut v = json!({"event": "SubagentStop", "session_id": "s1", "agent_id": id, "agent_type": "general-purpose", "transcript_path": s.tp(), "last_assistant_message": "waiting", "stop_hook_active": "false"});
    if let (Value::Object(m), Some(t)) = (&mut v, tasks) {
        m.insert("background_tasks".into(), Value::String(t.into()));
    }
    HookEvent::from_value(&v)
}

/// #222 R2: an agent that stops while its own background work runs keeps
/// its lane and slot (no finish is filed); an empty, absent or
/// unsubstituted `background_tasks` files the finish as before. The
/// plugin's SubagentStop hook passes the field.
#[test]
fn a_stop_with_background_work_of_its_own_keeps_the_slot() {
    for (tasks, held) in [(Some("[\"bash-1\"]"), true), (Some("[]"), false), (None, false), (Some("${background_tasks}"), false)] {
        let t = Temp::new("orch-222-bg");
        let s = Session::new(t.path());
        let mut h = Hooks::open(settings(t.path(), &builders(1), &[])).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: BG\n\nBuild one.", 1000, false);
        let stop = h.handle(&stop_input(&s, "b1", tasks));
        assert!(stop.block.is_none(), "{stop:?}");
        let d = launch(&mut h, &s, "t2", "general-purpose", "Kind: builder\nLane: OTHER\n\nBuild two.");
        assert_eq!(d.deny.is_some(), held, "{tasks:?}: {d:?}");
        let o = atlas::orchestrate::resolve(Some(&builders(1)), None, None, |_| None);
        assert_eq!(running_in(&o, &s, Some(h.store()), std::time::SystemTime::now()).is_empty(), !held, "{tasks:?}");
    }
    let plugin = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("hooks").join("hooks.json")).unwrap();
    let v = atlas::json::parse(&plugin).unwrap();
    let input = v.pointer("/hooks/SubagentStop/0/hooks/0/input").cloned().unwrap_or(Value::Null);
    assert_eq!(input.get("background_tasks").and_then(Value::as_str), Some("${background_tasks}"), "{input}");
}

/// #222 Q3: a finish is recorded in the memory store, where another handle
/// on it (another process) reads it, and no file is written beside Claude
/// Code's meta files.
#[test]
fn a_finish_is_recorded_in_the_store_not_beside_the_meta_files() {
    let t = Temp::new("orch-222-store");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(1), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: DISK\n\nBuild one.", 1000, false);
    let stop = h.handle(&stop_input(&s, "b1", Some("[]")));
    assert!(stop.block.is_none(), "{stop:?}");
    let files: Vec<String> = std::fs::read_dir(&s.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(files.iter().all(|f| !f.ends_with(".atlas-released")), "{files:?}");
    let second = atlas::memory::MemoryStore::open(h.store().path()).unwrap();
    let n: Result<i64, _> = second.conn().query_row("SELECT COUNT(*) FROM agent_releases WHERE agent = 'b1'", [], |r| r.get(0));
    assert_eq!(n.ok(), Some(1));
    let o = atlas::orchestrate::resolve(Some(&builders(1)), None, None, |_| None);
    assert!(running_in(&o, &s, Some(&second), std::time::SystemTime::now()).is_empty());
    // It counts again once it writes after the release, and the row goes.
    written(&s, "b1", std::time::SystemTime::now() + std::time::Duration::from_secs(5));
    assert_eq!(running_in(&o, &s, Some(&second), std::time::SystemTime::now() + std::time::Duration::from_secs(6)), ["b1"]);
    assert!(atlas::orchestrate::released(&second).unwrap().is_empty());
}

/// #222 Q3: a release record file of atlas 0.6.1 or earlier is read into the
/// store once and deleted, and still frees its agent.
#[test]
fn an_old_release_file_is_imported_once_and_deleted() {
    let t = Temp::new("orch-222-import");
    let s = Session::new(t.path());
    let store = atlas::memory::MemoryStore::open(&t.path().join("state").join("memory.sqlite")).unwrap();
    let o = atlas::orchestrate::resolve(Some(&builders(4)), None, None, |_| None);
    let now = std::time::SystemTime::now();
    s.agent("old", "general-purpose", "t-old", "Build.", 1000, false);
    written(&s, "old", now - std::time::Duration::from_secs(20 * 60));
    s.agent("live", "general-purpose", "t-live", "Build.", 1000, false);
    let file = s.dir.join("agent-old.atlas-released");
    std::fs::File::create(&file).unwrap().set_modified(now - std::time::Duration::from_secs(10 * 60)).unwrap();
    assert_eq!(running_in(&o, &s, Some(&store), now), ["live"]);
    assert!(!file.exists(), "the old file was kept");
    assert!(atlas::orchestrate::released(&store).unwrap().contains_key("old"));
    // Read from the store from now on.
    assert_eq!(running_in(&o, &s, Some(&store), now), ["live"]);
}

/// #222 R3: a quiet agent's transcript is read for the disk-full rule once,
/// not on every count, until it changes.
#[test]
fn an_unchanged_transcript_is_not_read_again() {
    use std::io::Write;
    let t = Temp::new("orch-222-cache");
    let s = Session::new(t.path());
    let o = atlas::orchestrate::resolve(Some(&builders(4)), None, None, |_| None);
    s.agent("quiet", "general-purpose", "t1", "Build.", 1000, false);
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(11 * 60);
    let mut agents = atlas::orchestrate::Agents::new(None);
    let before = atlas::orchestrate::tail_reads();
    assert_eq!(agents.running(&o, &s.dir, None, later).unwrap().len(), 1);
    assert_eq!(agents.running(&o, &s.dir, None, later).unwrap().len(), 1);
    assert_eq!(atlas::orchestrate::tail_reads() - before, 1);
    // A change is read.
    let mut f = std::fs::File::options().append(true).open(s.dir.join("agent-quiet.jsonl")).unwrap();
    writeln!(f, "{}", request_line("quiet")).unwrap();
    drop(f);
    assert_eq!(agents.running(&o, &s.dir, None, later).unwrap().len(), 1);
    assert_eq!(atlas::orchestrate::tail_reads() - before, 2);
}

// ---- #220: launch briefs kept at SubagentStart

/// An Agent launch with a model and a description.
fn launch_with(h: &mut Hooks, s: &Session, id: &str, prompt: &str) -> HookAnswer {
    let input = json!({"description": "build the walrus", "subagent_type": "general-purpose", "model": "opus", "prompt": prompt});
    h.handle(&HookEvent {
        tool_name: "Agent".into(),
        tool_use_id: id.into(),
        transcript_path: s.tp(),
        tool_prompt: prompt.into(),
        tool_input: input,
        ..ev("PreToolUse")
    })
}

/// The journal entries of `kind` on `lane` (its sub-lanes included).
fn entries(h: &Hooks, lane: &str, kind: &str) -> Vec<atlas::memory::Head> {
    h.store().lane_family(lane).unwrap().into_iter().filter(|x| x.kind == "journal" && x.sub == kind).collect()
}

/// #220: each agent's start files its launch brief, secrets redacted, as a
/// `launch` entry on its lane that `recall lane=` shows; the same brief
/// launched again is a pointer to the first; a launch with no lane files
/// on the agent's own; and a second writer into the lane still runs as a
/// sub-lane (a `launch` entry is no `brief`).
#[test]
fn an_agent_start_files_its_launch_brief_on_its_lane() {
    let t = Temp::new("orch-220-launch");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let brief = "Kind: builder\nLane: WALRUS\n\nBuild the walrus parser.\nexport API_TOKEN=notarealvalue123\nThen test it.";
    assert!(launch_with(&mut h, &s, "t1", brief).deny.is_none());
    s.agent("b1", "general-purpose", "t1", brief, 1000, false);
    let _ = start(&mut h, &s, "b1");
    let first = entries(&h, "WALRUS", "launch");
    assert_eq!(first.len(), 1, "{first:?}");
    let e = &first[0];
    assert_eq!(e.scope, "WALRUS");
    for line in ["agent: b1", "type: general-purpose", "model: opus", "description: build the walrus", "start: "] {
        assert!(e.meta.contains(line), "{line} missing in {}", e.meta);
    }
    assert!(
        e.body.contains("Build the walrus parser.") && e.body.contains("[redacted]") && !e.body.contains("notarealvalue123"),
        "{}",
        e.body
    );
    let view = atlas::memory::recall::lane(h.store(), "WALRUS", 100_000).unwrap();
    assert!(view.contains(&e.key) && view.contains("Build the walrus parser."), "{view}");

    // The same brief again, while b1 runs: a sub-lane, and a pointer.
    assert!(launch_with(&mut h, &s, "t2", brief).deny.is_none());
    s.agent("b2", "general-purpose", "t2", brief, 1000, false);
    let st = start(&mut h, &s, "b2");
    assert!(st.context.contains("sub-lane WALRUS/2"), "{st:?}");
    let all = entries(&h, "WALRUS", "launch");
    assert_eq!(all.len(), 2, "{all:?}");
    let again = all.iter().find(|x| x.key != e.key).unwrap();
    assert_eq!(again.scope, "WALRUS/2");
    assert!(
        again.body.contains(&e.key) && !again.body.contains("Build the walrus parser.") && again.body.lines().count() == 1,
        "{}",
        again.body
    );

    // No lane: the agent's own.
    let plain = "Kind: builder\n\nTidy the docs.";
    assert!(launch_with(&mut h, &s, "t3", plain).deny.is_none());
    s.agent("b3", "general-purpose", "t3", plain, 1000, false);
    let _ = start(&mut h, &s, "b3");
    assert_eq!(entries(&h, "agent:b3", "launch").len(), 1);
}

/// #220: a second concurrent writer launch into a lane runs as a sub-lane,
/// launch entries or not (green before and after).
#[test]
fn a_second_writer_into_a_launched_lane_still_gets_a_sub_lane() {
    let t = Temp::new("orch-220-sub");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let one = "Kind: builder\nLane: SEAL\n\nBuild one.";
    assert!(launch_with(&mut h, &s, "t1", one).deny.is_none());
    s.agent("b1", "general-purpose", "t1", one, 1000, false);
    let _ = start(&mut h, &s, "b1");
    let two = "Kind: builder\nLane: SEAL\n\nBuild two.";
    assert!(launch_with(&mut h, &s, "t2", two).deny.is_none());
    s.agent("b2", "general-purpose", "t2", two, 1000, false);
    let st = start(&mut h, &s, "b2");
    assert!(st.context.contains("sub-lane SEAL/2"), "{st:?}");
}

// ---- #221: the restart point

fn stop_agent(h: &mut Hooks, s: &Session, id: &str) -> HookAnswer {
    h.handle(&HookEvent {
        agent_id: id.into(),
        agent_type: "general-purpose".into(),
        last_message: "done".into(),
        transcript_path: s.tp(),
        ..ev("SubagentStop")
    })
}

fn session_start(h: &mut Hooks, session: &str) -> HookAnswer {
    h.handle(&HookEvent { source: "startup".into(), session_id: session.into(), ..ev("SessionStart") })
}

/// Moves an agent's files `by` into the past (an agent that stopped writing).
fn age_agent(s: &Session, id: &str, by: std::time::Duration) {
    let at = std::time::SystemTime::now() - by;
    for f in [format!("agent-{id}.jsonl"), format!("agent-{id}.meta.json")] {
        std::fs::File::options().write(true).open(s.dir.join(f)).unwrap().set_modified(at).unwrap();
    }
}

/// The restart point the server keeps: each running agent with its lane,
/// its launch entry's key and its declared resources; a stopped agent
/// leaves it.
#[test]
fn the_restart_point_lists_running_agents_with_lane_and_launch_key() {
    let t = Temp::new("orch-221-point");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let brief = "Kind: builder\nLane: WALRUS\nUses: compile\n\nBuild the walrus parser.";
    assert!(launch_with(&mut h, &s, "t1", brief).deny.is_none());
    s.agent("b1", "general-purpose", "t1", brief, 1000, false);
    let _ = start(&mut h, &s, "b1");
    let key = entries(&h, "WALRUS", "launch")[0].key.clone();
    let p = h.restart_point();
    assert_eq!(p.agents.len(), 1, "{p:?}");
    let a = &p.agents[0];
    assert_eq!((a.id.as_str(), a.lane.as_str(), a.launch.as_str()), ("b1", "WALRUS", key.as_str()), "{p:?}");
    assert_eq!(a.uses, vec!["compile".to_string()], "{p:?}");
    assert!(p.lanes.iter().any(|l| l == "WALRUS"), "{p:?}");
    assert!(stop_agent(&mut h, &s, "b1").block.is_none());
    assert!(h.restart_point().agents.is_empty());
}

/// W4d-2: a restart the poll saw is told once, in the main session's next
/// prompt, with the running agents counted; the prompt after it has
/// nothing.
#[test]
fn a_scheduled_restart_is_told_once_in_the_next_prompt() {
    let t = Temp::new("orch-221-warn");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let brief = "Kind: builder\nLane: WALRUS\nUses: compile\n\nBuild the walrus parser.";
    assert!(launch_with(&mut h, &s, "t1", brief).deny.is_none());
    s.agent("b1", "general-purpose", "t1", brief, 1000, false);
    let _ = start(&mut h, &s, "b1");
    let n = atlas::restart_warn::Notices::default();
    h.set_restart_warnings(n.clone());
    let tp = s.tp();
    let prompt = |h: &mut Hooks| h.handle(&HookEvent { prompt: "carry on".into(), transcript_path: tp.clone(), ..ev("UserPromptSubmit") });
    let before = prompt(&mut h);
    assert!(!before.context.contains("restart scheduled"), "{before:?}");
    n.post(atlas::restart_warn::systemd_scheduled("USEC=1763343420000000\nMODE=reboot\n"));
    let told = prompt(&mut h);
    assert!(told.context.contains("restart scheduled at 2025-11-17T01:37:00Z while 1 agent runs"), "{told:?}");
    let again = prompt(&mut h);
    assert!(!again.context.contains("restart scheduled"), "told twice: {again:?}");
}

/// A restart point the catch wrote is offered at the next session start
/// with its unfinished agents and their launch keys, filed as a
/// `restart:` record and deleted: a second start offers nothing.
#[test]
fn a_restart_point_is_offered_once_at_session_start() {
    use atlas::restart::{point_path, AgentPoint, Point};
    let t = Temp::new("orch-221-offer");
    let set = settings(t.path(), &builders(4), &[]);
    let file = point_path(&set.store);
    let mut h = Hooks::open(set).unwrap();
    let agent = |id: &str, launch: &str| AgentPoint {
        id: id.into(),
        description: format!("job {id}"),
        lane: "WALRUS".into(),
        launch: launch.into(),
        ..AgentPoint::default()
    };
    let p = Point {
        at: "2026-11-16T20:00:00Z".into(),
        agents: vec![agent("b9", "journal:WALRUS:0001"), agent("done1", "journal:WALRUS:0002")],
        lanes: vec!["WALRUS".into()],
        holders: Vec::new(),
        server: String::new(),
        ..Point::default()
    };
    atlas::orchestrate::record_finish(h.store(), "done1", std::time::SystemTime::now()).unwrap();
    std::fs::write(&file, p.to_json()).unwrap();
    let a = session_start(&mut h, "s2");
    assert!(
        a.context.contains("1 agent did not finish") && a.context.contains("agent b9") && a.context.contains("journal:WALRUS:0001"),
        "{a:?}"
    );
    assert!(!a.context.contains("done1"), "a finished agent is not offered: {a:?}");
    assert!(!file.exists(), "the point file is deleted once imported");
    let filed = h.store().heads(Some("journal"), Some("restart")).unwrap();
    assert_eq!(filed.len(), 1, "{filed:?}");
    assert!(filed[0].key.starts_with("restart:") && filed[0].body.contains("agent b9"), "{filed:?}");
    assert!(a.context.contains(&filed[0].key), "{a:?}");
    let again = session_start(&mut h, "s3");
    assert!(!again.context.contains("did not finish"), "offered twice: {again:?}");
}

/// With no restart point (a crash), the next session offers the last
/// session's agents that did not finish from their `launch` entries, once;
/// an agent that finished is not offered, nor one whose transcript changed
/// in the last few minutes (another session still runs it). The session
/// comes back 10 minutes after the crash, well inside the 6-hour stale
/// window that once hid every agent.
#[test]
fn the_crash_case_offers_unfinished_agents_from_launch_entries_once() {
    let t = Temp::new("orch-221-crash");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let walrus = "Kind: builder\nLane: WALRUS\n\nBuild the walrus parser.";
    assert!(launch_with(&mut h, &s, "t1", walrus).deny.is_none());
    s.agent("b1", "general-purpose", "t1", walrus, 1000, false);
    let _ = start(&mut h, &s, "b1");
    let otter = "Kind: builder\nLane: OTTER\n\nBuild the otter.";
    assert!(launch_with(&mut h, &s, "t2", otter).deny.is_none());
    s.agent("b2", "general-purpose", "t2", otter, 1000, false);
    let _ = start(&mut h, &s, "b2");
    assert!(stop_agent(&mut h, &s, "b2").block.is_none());
    let seal = "Kind: builder\nLane: SEAL\n\nBuild the seal.";
    assert!(launch_with(&mut h, &s, "t3", seal).deny.is_none());
    s.agent("b3", "general-purpose", "t3", seal, 1000, false);
    let _ = start(&mut h, &s, "b3");
    let key = entries(&h, "WALRUS", "launch")[0].key.clone();
    drop(h);
    // The session died 10 minutes ago, when b1 last wrote; b3 writes on.
    age_agent(&s, "b1", std::time::Duration::from_secs(10 * 60));
    let mut next = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    let a = session_start(&mut next, "s2");
    assert!(a.context.contains("no restart point") && a.context.contains("agent b1") && a.context.contains(&key), "{a:?}");
    assert!(!a.context.contains("agent b2"), "a finished agent is not offered: {a:?}");
    assert!(!a.context.contains("agent b3"), "an agent still writing is not offered: {a:?}");
    let again = session_start(&mut next, "s3");
    assert!(!again.context.contains("did not finish"), "offered twice: {again:?}");
}

/// Sets file `p`'s modified time to `at`.
fn set_mtime(p: &Path, at: std::time::SystemTime) {
    std::fs::File::options().write(true).open(p).unwrap().set_modified(at).unwrap();
}

/// An agent of a point, with the session folder that holds its files.
fn point_agent(s: &Session, id: &str, launch: &str) -> atlas::restart::AgentPoint {
    atlas::restart::AgentPoint {
        id: id.into(),
        description: format!("job {id}"),
        lane: "WALRUS".into(),
        launch: launch.into(),
        dir: s.dir.to_string_lossy().into_owned(),
        ..atlas::restart::AgentPoint::default()
    }
}

/// M1: a machine that went down 10 minutes ago, with agents writing until
/// the catch. Their transcripts changed just before the point was written,
/// well inside the 6-hour stale window: the agent is offered. One whose
/// transcript changed after the catch (another session runs it on) is not.
#[test]
fn a_point_offers_an_agent_that_wrote_until_the_catch() {
    use atlas::restart::{point_path, Point};
    let t = Temp::new("orch-221-quick");
    let s = Session::new(t.path());
    let set = settings(t.path(), &builders(4), &[]);
    let file = point_path(&set.store);
    let mut h = Hooks::open(set).unwrap();
    s.agent("q1", "general-purpose", "t1", "Kind: builder\nLane: WALRUS\n\nq1", 1000, false);
    s.agent("q2", "general-purpose", "t2", "Kind: builder\nLane: WALRUS\n\nq2", 1000, false);
    let caught = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 60);
    let p = Point {
        at: "2026-11-16T20:00:00.000Z".into(),
        agents: vec![point_agent(&s, "q1", "journal:WALRUS:0001"), point_agent(&s, "q2", "journal:WALRUS:0002")],
        lanes: vec!["WALRUS".into()],
        holders: Vec::new(),
        server: String::new(),
        ..Point::default()
    };
    std::fs::write(&file, p.to_json()).unwrap();
    set_mtime(&file, caught);
    set_mtime(&s.dir.join("agent-q1.jsonl"), caught - std::time::Duration::from_secs(2));
    set_mtime(&s.dir.join("agent-q2.jsonl"), caught + std::time::Duration::from_secs(5 * 60));
    let a = session_start(&mut h, "s2");
    assert!(a.context.contains("agent q1") && a.context.contains("journal:WALRUS:0001"), "an agent that wrote until the catch: {a:?}");
    assert!(!a.context.contains("agent q2"), "an agent written after the catch is not offered: {a:?}");
    assert!(!file.exists(), "the point file is deleted once imported");
}

/// #221 S1: a point kept while a restart was pending (rewritten every few
/// minutes), its server gone and no catch: the machine went down after its
/// last rewrite, so an agent that wrote after the point, before the boot,
/// worked until then and is offered; one that wrote after the boot runs in
/// this boot and is not. The offer does not say the server caught it.
#[test]
fn a_kept_point_offers_agents_that_wrote_after_it_once_the_machine_rebooted() {
    use atlas::restart::{point_path, Point};
    let t = Temp::new("orch-221-kept");
    let s = Session::new(t.path());
    let set = settings(t.path(), &builders(4), &[]);
    let (file, path) = (point_path(&set.store), set.store.clone());
    let h = Hooks::open(set).unwrap();
    s.agent("q1", "general-purpose", "t1", "Kind: builder\nLane: WALRUS\n\nq1", 1000, false);
    s.agent("q2", "general-purpose", "t2", "Kind: builder\nLane: WALRUS\n\nq2", 1000, false);
    drop(h);
    let now = std::time::SystemTime::now();
    let min = |m: u64| std::time::Duration::from_secs(m * 60);
    let p = Point {
        at: "2026-11-16T20:00:00.000Z".into(),
        agents: vec![point_agent(&s, "q1", "journal:WALRUS:0001"), point_agent(&s, "q2", "journal:WALRUS:0002")],
        held: true,
        ..Point::default()
    };
    std::fs::write(&file, p.to_json()).unwrap();
    set_mtime(&file, now - min(10));
    set_mtime(&s.dir.join("agent-q1.jsonl"), now - min(8));
    set_mtime(&s.dir.join("agent-q2.jsonl"), now - min(1));
    let mut store = atlas::memory::MemoryStore::open(&path).unwrap();
    let a = atlas::restart::offer_at(&mut store, "s2", now, Some(now - min(5))).unwrap().unwrap_or_default();
    assert!(a.contains("agent q1"), "an agent that worked until the machine went down was dropped: {a:?}");
    assert!(!a.contains("agent q2"), "an agent running in this boot was offered: {a:?}");
    assert!(a.contains("restarted while a restart was pending") && !a.contains("caught a shutdown"), "{a:?}");
    assert!(!file.exists(), "the point file is deleted once imported");
}

/// #221 S2: a kept point that its server marked ended (a plain end of
/// session while a restart was pending) is never offered as a caught
/// shutdown. With no reboot since, it is dropped and its agent is left to
/// the crash case, which offers it (it is not hidden); after a reboot, the
/// point offers it.
#[test]
fn a_kept_point_whose_session_ended_is_never_offered_as_a_caught_shutdown() {
    use atlas::restart::{point_path, Point};
    for rebooted in [false, true] {
        let t = Temp::new(if rebooted { "orch-221-ended-boot" } else { "orch-221-ended" });
        let s = Session::new(t.path());
        let set = settings(t.path(), &builders(4), &[]);
        let (file, path) = (point_path(&set.store), set.store.clone());
        let mut h = Hooks::open(set).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let walrus = "Kind: builder\nLane: WALRUS\n\nBuild the walrus parser.";
        assert!(launch_with(&mut h, &s, "t1", walrus).deny.is_none());
        s.agent("b1", "general-purpose", "t1", walrus, 1000, false);
        let _ = start(&mut h, &s, "b1");
        let key = entries(&h, "WALRUS", "launch")[0].key.clone();
        drop(h);
        let now = std::time::SystemTime::now();
        let min = |m: u64| std::time::Duration::from_secs(m * 60);
        let p = Point { agents: vec![point_agent(&s, "b1", &key)], held: true, ended: "stdin closed".into(), ..Point::default() };
        std::fs::write(&file, p.to_json()).unwrap();
        set_mtime(&file, now - min(10));
        set_mtime(&s.dir.join("agent-b1.jsonl"), now - min(12));
        let boot = if rebooted { now - min(5) } else { now - min(24 * 60) };
        let mut store = atlas::memory::MemoryStore::open(&path).unwrap();
        let a = atlas::restart::offer_at(&mut store, "s2", now, Some(boot)).unwrap().unwrap_or_default();
        assert!(!a.contains("caught a shutdown"), "a plain end of session was offered as a caught shutdown: {a:?}");
        assert!(a.contains("agent b1"), "rebooted {rebooted}: the agent was hidden: {a:?}");
        if rebooted {
            assert!(a.contains("restarted while a restart was pending"), "{a:?}");
        } else {
            assert!(a.contains("no restart point"), "not left to the crash case: {a:?}");
        }
        assert!(!file.exists(), "rebooted {rebooted}: the point file was left");
    }
}

/// S1: two servers (two sessions) each caught the shutdown and wrote their
/// own point, and a third session's agents crashed with no point. One start
/// offers all three, an agent named both by a point and by its launch entry
/// once, deletes both files, and a second start offers nothing.
#[test]
fn every_server_s_point_and_an_uncovered_crash_are_offered_once() {
    use atlas::restart::Point;
    let t = Temp::new("orch-221-many");
    let s = Session::new(t.path());
    let other = Session::new(&t.path().join("other"));
    let third = Session::new(&t.path().join("third"));
    let set = settings(t.path(), &builders(4), &[]);
    let dir = set.store.parent().unwrap().to_path_buf();
    let mut h = Hooks::open(set).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let walrus = "Kind: builder\nLane: WALRUS\n\nBuild the walrus parser.";
    assert!(launch_with(&mut h, &s, "t1", walrus).deny.is_none());
    s.agent("a1", "general-purpose", "t1", walrus, 1000, false);
    let _ = start(&mut h, &s, "a1");
    // Session sB's agent: launched and started there, never in a point.
    let otter = "Kind: builder\nLane: OTTER\n\nBuild the otter.";
    let input = json!({"description": "build the otter", "subagent_type": "general-purpose", "prompt": otter});
    let launch_b = HookEvent {
        tool_name: "Agent".into(),
        tool_use_id: "t9".into(),
        transcript_path: other.tp(),
        tool_prompt: otter.into(),
        tool_input: input,
        session_id: "sB".into(),
        ..ev("PreToolUse")
    };
    assert!(h.handle(&launch_b).deny.is_none());
    other.agent("c1", "general-purpose", "t9", otter, 1000, false);
    let _ = h.handle(&HookEvent {
        agent_id: "c1".into(),
        agent_type: "general-purpose".into(),
        transcript_path: other.tp(),
        session_id: "sB".into(),
        ..ev("SubagentStart")
    });
    third.agent("x2", "general-purpose", "t8", "Kind: builder\nLane: SEAL\n\nx2", 1000, false);
    let key = entries(&h, "WALRUS", "launch")[0].key.clone();
    drop(h);
    let now = std::time::SystemTime::now();
    let ago = |m: u64| now - std::time::Duration::from_secs(m * 60);
    for (sess, id) in [(&s, "a1"), (&other, "c1"), (&third, "x2")] {
        set_mtime(&sess.dir.join(format!("agent-{id}.jsonl")), ago(10));
    }
    let point = |a: atlas::restart::AgentPoint| Point {
        at: "2026-11-16T20:00:00.000Z".into(),
        agents: vec![a],
        lanes: Vec::new(),
        holders: Vec::new(),
        server: String::new(),
        ..Point::default()
    };
    let (f1, f2) = (dir.join("restart-point-111-1.json"), dir.join("restart-point-222-1.json"));
    std::fs::write(&f1, point(point_agent(&s, "a1", &key)).to_json()).unwrap();
    std::fs::write(&f2, point(point_agent(&third, "x2", "journal:SEAL:0001")).to_json()).unwrap();
    set_mtime(&f1, ago(9));
    set_mtime(&f2, ago(9));
    let mut next = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    let a = session_start(&mut next, "s3");
    for id in ["agent a1", "agent x2", "agent c1"] {
        assert!(a.context.contains(id), "{id} not offered: {a:?}");
    }
    assert!(a.context.contains("3 agents did not finish"), "{a:?}");
    assert_eq!(a.context.matches("agent a1").count(), 1, "{a:?}");
    assert!(!f1.exists() && !f2.exists(), "a point file was left");
    let again = session_start(&mut next, "s4");
    assert!(!again.context.contains("did not finish"), "offered twice: {again:?}");
}

/// The W4c caveat: a start that guessed another launch filed that launch's
/// brief under its own agent id. At its first tool call that entry is
/// retired and its own brief filed, so the crash offer names the right
/// brief; the guessed launch's own agent files its entry at its start.
#[test]
fn a_wrongly_guessed_launch_entry_is_corrected_at_the_first_tool_call() {
    let t = Temp::new("orch-221-guess");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(12), &[])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: TOOLS\n\nBuild one.", 1000, false);
    let _ = launch(&mut h, &s, "tA", "general-purpose", "Kind: builder\nLane: TOOLS\n\nBuild two.");
    let _ = start(&mut h, &s, "r");
    s.agent("r", "general-purpose", "tR", "Kind: reviewer\nLane: REVIEWS\n\nReview it.", 1000, false);
    let first = tool_call(&mut h, &s, "r");
    assert!(first.context.starts_with("atlas: correction:"), "{first:?}");
    let active = |h: &Hooks| h.store().active_entries(atlas::hooks::LAUNCH_ENTRY).unwrap();
    let of = |h: &Hooks, agent: &str, text: &str| {
        active(h).into_iter().filter(|e| e.meta.contains(&format!("agent: {agent}\n")) && e.body.contains(text)).count()
    };
    assert_eq!(of(&h, "r", "Build two."), 0, "the wrong entry is retired: {:?}", active(&h));
    assert_eq!(of(&h, "r", "Review it."), 1, "its own brief is filed: {:?}", active(&h));
    let _ = start(&mut h, &s, "b2");
    s.agent("b2", "general-purpose", "tA", "Kind: builder\nLane: TOOLS\n\nBuild two.", 1000, false);
    let _ = tool_call(&mut h, &s, "b2");
    assert_eq!(of(&h, "b2", "Build two."), 1, "{:?}", active(&h));
}

/// R1 of the second review: a builder in another session sits in one long
/// shell call (its transcript quiet for 10 minutes) while a new session
/// starts (a /clear, or a second window). Its launch entry names its
/// server, which still runs (this process), so it is not offered.
#[test]
fn the_crash_case_skips_an_agent_whose_server_still_runs() {
    let t = Temp::new("orch-221-live-server");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    h.enable_restart_point();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let walrus = "Kind: builder\nLane: WALRUS\n\nBuild the walrus parser.";
    assert!(launch_with(&mut h, &s, "t1", walrus).deny.is_none());
    s.agent("b1", "general-purpose", "t1", walrus, 1000, false);
    let _ = start(&mut h, &s, "b1");
    let entry = entries(&h, "WALRUS", "launch")[0].clone();
    assert!(entry.meta.contains(&format!("server: {}\n", atlas::restart::server_tag())), "{}", entry.meta);
    age_agent(&s, "b1", std::time::Duration::from_secs(10 * 60));
    let mut next = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    let a = session_start(&mut next, "s2");
    assert!(!a.context.contains("agent b1"), "an agent whose server runs was offered: {a:?}");
    assert!(next.store().heads(Some("journal"), Some("restart")).unwrap().is_empty(), "a restart record was filed");
}

/// R1, the second half: a crash offer filed while the agent in fact ran
/// on (its transcript changed after the offer) does not hide its real end
/// later; an offer that was right is not made again.
#[test]
fn a_wrong_crash_offer_does_not_hide_the_agent_s_later_end() {
    let t = Temp::new("orch-221-reoffer");
    let s = Session::new(t.path());
    let set = settings(t.path(), &builders(4), &[]);
    let path = set.store.clone();
    let mut h = Hooks::open(set).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let walrus = "Kind: builder\nLane: WALRUS\n\nBuild the walrus parser.";
    assert!(launch_with(&mut h, &s, "t1", walrus).deny.is_none());
    s.agent("b1", "general-purpose", "t1", walrus, 1000, false);
    let _ = start(&mut h, &s, "b1");
    drop(h);
    let now = std::time::SystemTime::now();
    let min = |m: u64| std::time::Duration::from_secs(m * 60);
    set_mtime(&s.dir.join("agent-b1.jsonl"), now - min(10));
    let mut store = atlas::memory::MemoryStore::open(&path).unwrap();
    let first = atlas::restart::offer(&mut store, "s2", now).unwrap().unwrap_or_default();
    assert!(first.contains("agent b1"), "{first}");
    let again = atlas::restart::offer(&mut store, "s3", now + min(10)).unwrap();
    assert!(again.is_none(), "offered twice: {again:?}");
    // b1 was running all along: it wrote a minute after the offer, then
    // its session died.
    set_mtime(&s.dir.join("agent-b1.jsonl"), now + min(1));
    let later = atlas::restart::offer(&mut store, "s4", now + min(20)).unwrap().unwrap_or_default();
    assert!(later.contains("agent b1"), "the agent's real end was hidden by the wrong offer: {later:?}");
}

/// R3: two session starts at once. The one that finds the crash case's
/// lock held by a live process leaves the crash case to it; a lock whose
/// process is gone is taken over.
#[test]
fn the_crash_case_is_offered_by_one_start_at_a_time() {
    let t = Temp::new("orch-221-lock");
    let s = Session::new(t.path());
    let set = settings(t.path(), &builders(4), &[]);
    let lock = set.store.with_file_name(atlas::restart::OFFER_LOCK);
    let mut h = Hooks::open(set).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let walrus = "Kind: builder\nLane: WALRUS\n\nBuild the walrus parser.";
    assert!(launch_with(&mut h, &s, "t1", walrus).deny.is_none());
    s.agent("b1", "general-purpose", "t1", walrus, 1000, false);
    let _ = start(&mut h, &s, "b1");
    drop(h);
    age_agent(&s, "b1", std::time::Duration::from_secs(10 * 60));
    // Another start (a live process: this one's parent) holds the lock, and
    // a point names b1: the point is left to the holder too (S3).
    let parent = atlas::restart::server_parent().expect("this process's parent");
    std::fs::write(&lock, format!("{parent} -")).unwrap();
    let point = atlas::restart::Point {
        at: "2026-11-16T20:00:00.000Z".into(),
        agents: vec![point_agent(&s, "b1", "journal:WALRUS:0001")],
        lanes: Vec::new(),
        holders: Vec::new(),
        server: String::new(),
        ..atlas::restart::Point::default()
    };
    let point_file = lock.with_file_name("restart-point-555-1.json");
    std::fs::write(&point_file, point.to_json()).unwrap();
    set_mtime(&point_file, std::time::SystemTime::now() - std::time::Duration::from_secs(9 * 60));
    let mut next = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    let a = session_start(&mut next, "s2");
    assert!(!a.context.contains("agent b1"), "offered while another start held the lock: {a:?}");
    assert!(lock.exists(), "another start's lock was removed");
    assert!(point_file.exists(), "a start without the lock claimed a point");
    std::fs::remove_file(&point_file).unwrap();
    // Its holder died mid-offer: the lock is taken over.
    std::fs::write(&lock, "4294967290 1").unwrap();
    let b = session_start(&mut next, "s3");
    assert!(b.context.contains("agent b1"), "a dead holder's lock blocked the offer: {b:?}");
    assert!(!lock.exists(), "the lock was left");
    // N2: a lock naming this process (its unlink failed) is stale: taken
    // over, then removed.
    std::fs::write(&lock, atlas::restart::server_tag()).unwrap();
    let _ = session_start(&mut next, "s4");
    assert!(!lock.exists(), "this process's own leftover lock was left in place");
}

/// R4: a point file another start claimed and then died before deleting
/// is claimed again once its claimer is gone; one whose claimer lives is
/// left to it.
#[test]
fn an_orphaned_claimed_point_is_claimed_again() {
    use atlas::restart::Point;
    let t = Temp::new("orch-221-orphan");
    let s = Session::new(t.path());
    let set = settings(t.path(), &builders(4), &[]);
    let dir = set.store.parent().unwrap().to_path_buf();
    let mut h = Hooks::open(set).unwrap();
    s.agent("q1", "general-purpose", "t1", "Kind: builder\nLane: WALRUS\n\nq1", 1000, false);
    s.agent("q2", "general-purpose", "t2", "Kind: builder\nLane: WALRUS\n\nq2", 1000, false);
    let caught = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 60);
    let point = |id: &str| Point {
        at: "2026-11-16T20:00:00.000Z".into(),
        agents: vec![point_agent(&s, id, "journal:WALRUS:0001")],
        lanes: Vec::new(),
        holders: Vec::new(),
        server: String::new(),
        ..Point::default()
    };
    let live = atlas::restart::server_parent().expect("this process's parent");
    let (dead, held) =
        (dir.join("restart-point-333-1.json.claimed-4294967290"), dir.join(format!("restart-point-444-1.json.claimed-{live}")));
    std::fs::write(&dead, point("q1").to_json()).unwrap();
    std::fs::write(&held, point("q2").to_json()).unwrap();
    for (f, id) in [(&dead, "q1"), (&held, "q2")] {
        set_mtime(f, caught);
        set_mtime(&s.dir.join(format!("agent-{id}.jsonl")), caught - std::time::Duration::from_secs(2));
    }
    let a = session_start(&mut h, "s2");
    assert!(a.context.contains("agent q1"), "an orphaned claimed point was not offered: {a:?}");
    assert!(!a.context.contains("agent q2"), "a live start's claimed point was taken: {a:?}");
    assert!(!dead.exists(), "the orphan was left");
    assert!(held.exists(), "a live start's claimed point was removed");
}
