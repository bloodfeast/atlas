//! Public #16: a launch the harness rejects holds no cap slot, and the
//! slots held are listed (kind, lane, description, since). Part of
//! `tests/orchestrate.rs` (its helpers).

use super::*;

fn one_reviewer() -> OrchestrateSpec {
    OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![(Kind::Reviewer, KindSpec { max: Some(1), ..Default::default() })],
        ..Default::default()
    }
}

impl Session {
    /// Records in the session transcript the harness's rejection of launch
    /// `id` (no agent started), as Claude Code writes an unknown agent type.
    fn reject(&self, id: &str) {
        let text = "<tool_use_error>Agent type 'on-track' not found. Available agents: general-purpose</tool_use_error>";
        let r = json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": text, "is_error": true, "tool_use_id": id}]}});
        let mut f = std::fs::File::options().append(true).open(&self.parent).unwrap();
        std::io::Write::write_all(&mut f, format!("{r}\n").as_bytes()).unwrap();
    }
}

fn reviewer(h: &mut Hooks, s: &Session, id: &str, agent_type: &str) -> HookAnswer {
    launch(h, s, id, agent_type, "Kind: reviewer\nLane: REVIEWS\n\nReview the writer.")
}

/// The issue's sequence: a reviewer launch the harness rejects, then the
/// same brief again at once. The second is admitted; a launch no tool
/// result refused still holds its slot.
#[test]
fn a_rejected_launch_frees_its_slot_for_the_relaunch() {
    let t = Temp::new("orch-cap-rejected");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &one_reviewer(), &[])).unwrap();
    assert!(reviewer(&mut h, &s, "t1", "on-track").deny.is_none());
    s.reject("t1");
    let a = reviewer(&mut h, &s, "t2", "general-purpose");
    assert!(a.deny.is_none(), "the rejected launch still held the slot: {a:?}");
    // t2 is not refused: it holds the slot, so a third waits.
    let a = reviewer(&mut h, &s, "t3", "general-purpose");
    assert!(a.deny.as_deref().is_some_and(|r| r.contains("1 of 1 reviewer")), "{a:?}");
}

/// The held slots, as `atlas queue list` and `recall op=status` list them:
/// the launch admitted and not started, with its kind, lane, description
/// and time; a rejected one is not among them, nor a started one twice.
#[test]
fn held_slots_name_each_holder() {
    let t = Temp::new("orch-cap-held");
    let s = Session::new(t.path());
    let st = settings(t.path(), &one_reviewer(), &[]);
    let orch = st.orchestrate.clone();
    let mut h = Hooks::open(st).unwrap();
    assert!(reviewer(&mut h, &s, "t1", "on-track").deny.is_none());
    s.reject("t1");
    assert!(reviewer(&mut h, &s, "t2", "general-purpose").deny.is_none());
    let store = atlas::memory::MemoryStore::open(&t.path().join("state").join("memory.sqlite")).unwrap();
    let held = atlas::queue::held_slots(&orch, &store, std::time::SystemTime::now()).unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    let slot = &held[0];
    assert!(
        slot.kind == Kind::Reviewer && slot.lane == "REVIEWS" && slot.description == "d" && !slot.running && slot.since.len() >= 16,
        "{slot:?}"
    );
    let text = atlas::queue::held_text(&held, &atlas::memory::now());
    assert!(
        text.starts_with("1 agent cap slot is held")
            && text.contains("reviewer  lane REVIEWS  since ")
            && text.contains("[launched, not started]"),
        "{text}"
    );
    // Its agent starts: the pending row goes, and a running agent is not
    // listed from the pending table again.
    s.agent("r2", "general-purpose", "t2", "Kind: reviewer\nLane: REVIEWS\n\nReview the writer.", 1000, false);
    let _ = start(&mut h, &s, "r2");
    let pending = atlas::orchestrate::pending_slots(&store, std::time::SystemTime::now());
    assert!(pending.is_empty(), "{pending:?}");
}

/// Review follow-up: a launch refused in another session's transcript frees
/// its slot when this session's launch is admitted (every pending
/// transcript is swept, not only the admitting one's).
#[test]
fn a_launch_refused_in_another_session_frees_its_slot() {
    let t = Temp::new("orch-cap-other-session");
    let a = Session::new(t.path());
    let b = Session { parent: t.path().join("proj").join("sess2.jsonl"), dir: t.path().join("proj").join("sess2").join("subagents") };
    std::fs::create_dir_all(&b.dir).unwrap();
    std::fs::write(&b.parent, "").unwrap();
    let mut h = Hooks::open(settings(t.path(), &one_reviewer(), &[])).unwrap();
    let prompt = "Kind: reviewer\nLane: REVIEWS\n\nReview the writer.";
    let input = json!({"description": "d", "subagent_type": "on-track", "prompt": prompt});
    let from_b = h.handle(&HookEvent {
        tool_name: "Agent".into(),
        tool_use_id: "b1".into(),
        transcript_path: b.tp(),
        tool_prompt: prompt.into(),
        tool_input: input,
        session_id: "s2".into(),
        ..ev("PreToolUse")
    });
    assert!(from_b.deny.is_none(), "{from_b:?}");
    b.reject("b1");
    let from_a = reviewer(&mut h, &a, "a1", "general-purpose");
    assert!(from_a.deny.is_none(), "session s2's rejected launch still held the slot: {from_a:?}");
}

/// #370: a rejected launch that is never relaunched leaves the held-slot
/// listing at once, not after the pending TTL: the listing reads the
/// session transcript from the launch's admission for its refusal. A
/// launch with no refusal stays listed.
#[test]
fn a_rejection_with_no_relaunch_is_not_listed() {
    let t = Temp::new("orch-cap-370-no-relaunch");
    let s = Session::new(t.path());
    let st = settings(t.path(), &builders_and_reviewer(), &[]);
    let mut h = Hooks::open(st).unwrap();
    assert!(reviewer(&mut h, &s, "t1", "on-track").deny.is_none());
    assert!(launch(&mut h, &s, "t2", "general-purpose", "Kind: builder\nLane: WALRUS\n\nBuild it.").deny.is_none());
    let store = atlas::memory::MemoryStore::open(&t.path().join("state").join("memory.sqlite")).unwrap();
    let ids = || atlas::orchestrate::pending_slots(&store, std::time::SystemTime::now()).into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    assert_eq!(ids(), ["t1", "t2"]);
    s.reject("t1");
    assert_eq!(ids(), ["t2"], "the rejected launch is still listed");
}

/// #370: a pending table an older build made (no `scan_from` column) is
/// listed, and gains the column on the next write.
#[test]
fn a_pending_table_from_an_older_build_is_read_and_upgraded() {
    let t = Temp::new("orch-cap-370-old-table");
    let s = Session::new(t.path());
    let st = settings(t.path(), &builders_and_reviewer(), &[]);
    let path = st.store.clone();
    let mut h = Hooks::open(st).unwrap();
    {
        let store = atlas::memory::MemoryStore::open(&path).unwrap();
        store.conn().execute_batch("CREATE TABLE agent_pending(tool_use_id TEXT NOT NULL, transcript TEXT NOT NULL, kind TEXT NOT NULL, lane TEXT NOT NULL, description TEXT NOT NULL, time INTEGER NOT NULL);").unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64;
        store
            .conn()
            .execute("INSERT INTO agent_pending VALUES('old1', ?1, 'builder', 'OLD', 'd', ?2)", atlas::db::params![s.tp(), now])
            .unwrap();
        let ids: Vec<String> = atlas::orchestrate::pending_slots(&store, std::time::SystemTime::now()).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["old1"]);
    }
    assert!(reviewer(&mut h, &s, "t1", "general-purpose").deny.is_none());
    let store = atlas::memory::MemoryStore::open(&path).unwrap();
    let ids: Vec<String> = atlas::orchestrate::pending_slots(&store, std::time::SystemTime::now()).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["old1", "t1"]);
}

fn builders_and_reviewer() -> OrchestrateSpec {
    OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![
            (Kind::Reviewer, KindSpec { max: Some(1), ..Default::default() }),
            (Kind::Builder, KindSpec { max: Some(4), ..Default::default() }),
        ],
        ..Default::default()
    }
}
