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

/// A lane refusal is not caused by the kind: it gets no advice on
/// `Kind:` lines, even for a launch no `Kind:` line classified.
#[test]
fn a_lane_refusal_gives_no_kind_advice() {
    let t = Temp::new("orch-cap-no-kind-advice");
    let s = Session::new(t.path());
    let on = OrchestrateSpec { enabled: Some(true), ..Default::default() };
    let mut h = Hooks::open(settings(t.path(), &on, &[])).unwrap();
    h.set_command_hook();
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: parser\n\nFix the reader.", 1000, false);
    let r = launch(&mut h, &s, "t1", "general-purpose", "Lane: parser\n\nFix the writer.").deny.unwrap();
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
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: parser\n\nFix the reader.", 1000, false);
    // A builder the queue denies (a declared lane conflict) after the cap
    // let it through ...
    let q = launch(&mut h, &s, "t1", "general-purpose", "Kind: builder\nLane: parser\n\nFix the writer.");
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
    let at = |id: &'static str| Launch { transcript: &tp, tool_use_id: id, input: &input, brief };
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
        tool_input: json!({"prompt": "Build."}),
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
    // Declared on both sides: the lane is held, so it is denied and queued.
    let r = launch(&mut h, &s, "t1", "general-purpose", "Kind: builder\nLane: parser\nUses: none\n\nFix the writer.").deny.expect("a declared lane conflict is denied");
    assert!(r.contains("lane parser held by job b1 (b1)") && r.contains("atlas queued it as journal:parser:0001"), "{r}");
    // A declared read-only reviewer shares the lane.
    let a = launch(&mut h, &s, "t2", "general-purpose", "Kind: reviewer\nLane: parser\nUses: read-only\n\nReview the reader.");
    assert!(a.deny.is_none() && a.context.is_empty(), "{a:?}");
    // An inferred resource conflict is a warning.
    let a = launch(&mut h, &s, "t3", "general-purpose", "Kind: builder\nLane: docs\n\nTake a frame capture of the menu.");
    assert!(a.deny.is_none() && a.context.contains("may conflict with running work (gpu held by job b1 (b1))"), "{a:?}");
    // A queued gpu item waits while b1 holds the gpu; after b1 finishes the
    // finish line proposes both queued items together.
    let r = launch(&mut h, &s, "t4", "general-purpose", "Kind: builder\nLane: render\nUses: gpu\n\nRender the wreck.").deny.expect("declared gpu on both sides");
    assert!(r.contains("gpu held by job b1 (b1)"), "{r}");
    let n = finished(&mut h, &s, "b1");
    assert!(n.contains("start together: journal:parser:0001 (builder, lane parser, d); journal:render:0001 (builder, lane render, d)"), "{n}");
}

/// atlas-dev #150: words on a `Uses:` line that the policy does not list
/// (repos) never deny or queue a launch, only note what it shares; gpu,
/// compile, worktrees and configured words still deny, and a configured
/// class limits each `name value` on its own.
#[test]
fn unlisted_uses_words_only_warn_through_the_hook() {
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
    s.agent("a1", "general-purpose", "t-a1", "Kind: build\nLane: ES1-1663\nUses: db, contracts, api, react, sqlport 15126, cache, license\n\nBuild it.", 1000, false);
    // The issue's second header: let through, with a note naming the words.
    let a = launch(&mut h, &s, "t1", "general-purpose", "Kind: build\nLane: ES1-1415\nUses: db, contracts, api, react, sqlport 15127\n\nBuild it.");
    assert!(a.deny.is_none(), "{a:?}");
    assert!(a.context.contains("shares api with job a1 (a1); contracts with job a1 (a1); db with job a1 (a1); react with job a1 (a1) with running work"), "{a:?}");
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
    s.agent("b1", "general-purpose", "t-b1", "Kind: builder\nLane: ES1-1415\n\nHold the lane.", 1000, false);
    for (id, what) in [("t1", "tariff pricing"), ("t2", "pricing tests"), ("t3", "pricing docs")] {
        let r = launch(&mut h, &s, id, "general-purpose", &format!("Kind: builder\nLane: ES1-1415\nUses: sqlport 15127\n\nBuild the {what}.")).deny.expect("the lane is held");
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
    atlas::queue::cli(&args(&["done", &budget]), &mut m, &orch).unwrap();
    assert_eq!(start(&m).0, std::slice::from_ref(&fire));
    // `plan` refuses a key that names no queued item.
    let e = atlas::queue::cli(&args(&["plan", "--lane", "X", "--title", "t", "--after", "journal:NOPE:0009"]), &mut m, &orch).unwrap_err();
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
    atlas::queue::cli(&args(&["drop", &doomed]), &mut m, &orch).unwrap();
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
    fn head(m: &atlas::reviews::Merge, _cwd: &str) -> Result<(u64, String), String> {
        match m.selector.as_deref() {
            Some("99") => Ok((99, "6e828cc0123456789abcdef".into())),
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
    fn newer(_m: &atlas::reviews::Merge, _cwd: &str) -> Result<(u64, String), String> {
        Ok((99, "abcdef0123".into()))
    }
    h.set_pr_head(newer);
    file("review", "role: simplicity, cost\npr: 99\nhead: abcdef0\nverdict: pass");
    assert!(passes(&bash(&mut h, "gh pr merge 99")));

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
    // A builder in the lane still conflicts with the lane's builder.
    let builder = launch(&mut h, &s, "t4", "general-purpose", "Kind: builder\nLane: ATLAS-V03\n\nBuild more.").deny.expect("the lane");
    assert!(builder.contains("lane ATLAS-V03 held by job b1"), "{builder}");
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
    let at = |id: &'static str| Launch { transcript: &tp, tool_use_id: id, input: &input, brief };
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
    let at = |id: &'static str| Launch { transcript: &tp, tool_use_id: id, input: &input, brief: "Kind: builder\nBuild more." };
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
