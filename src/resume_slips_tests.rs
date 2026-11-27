//! The nine resume slips of 2026-11-24 (journal:ATLAS-RESUME:0001 to 0004),
//! each replayed from the store as it stood that day: the card and lane
//! recall trusted the newest handoff over what came after it.

use crate::memory::{Draft, MemoryStore};
use crate::orchestrate::OrchSettings;

const NOW: &str = "2026-11-24T21:00:00Z";

fn store_in(name: &str) -> (std::path::PathBuf, MemoryStore) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("atlas-slips-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let s = MemoryStore::open(&d.join("memory.sqlite")).unwrap();
    (d, s)
}

/// One record: `kind/sub` in `scope` at `time`, with a title, meta, body
/// and refs.
#[allow(clippy::too_many_arguments)]
fn put(s: &mut MemoryStore, key: &str, kind: &str, sub: &str, scope: &str, time: &str, title: &str, meta: &str, body: &str, refs: &[(&str, &str)]) {
    let d = Draft {
        title: title.into(),
        meta: meta.into(),
        body: body.into(),
        reason: "t".into(),
        author: "t".into(),
        client: "t".into(),
        time: Some(time.into()),
        ..Default::default()
    };
    let mut w = s.writer().unwrap();
    w.create(key, kind, sub, scope, &d, None).unwrap();
    if !refs.is_empty() {
        let r: Vec<(String, String)> = refs.iter().map(|(k, t)| ((*k).to_string(), (*t).to_string())).collect();
        w.add_refs(key, &r).unwrap();
    }
    w.commit().unwrap();
}

fn entry(s: &mut MemoryStore, key: &str, sub: &str, time: &str, title: &str) {
    let lane = key.split(':').nth(1).unwrap();
    put(s, key, "journal", sub, lane, time, title, "", &format!("{title}\n"), &[]);
}

fn the_card(s: &MemoryStore) -> String {
    crate::resume::card(s, &OrchSettings::default(), "", NOW, false).unwrap().unwrap_or_default()
}

/// NEW-MAP as it stood: handoff 014 says the next step is the review of
/// #501 fix4; a launch, the review (MERGE) and the merge came after it.
fn new_map(s: &mut MemoryStore) {
    entry(s, "journal:NEW-MAP:0149", "decision", "2026-11-24T10:00:00Z", "fix4 scope: roads only");
    put(s, "handoff:NEW-MAP:014", "handoff", "", "NEW-MAP", "2026-11-24T11:00:00Z", "NM-ROADS fix4 pushed", "", "State: fix4 pushed.\nNext: review of #501 fix4\n", &[]);
    entry(s, "journal:NEW-MAP:0150", "launch", "2026-11-24T12:00:00Z", "launch: Review PR 501 fix4");
    put(s, "journal:NEW-MAP:0151", "journal", "review", "NEW-MAP", "2026-11-24T13:00:00Z", "review #501 fix4: MERGE", "role: reviewer\npr: 501\nverdict: MERGE", "MERGE\n", &[]);
    entry(s, "journal:NEW-MAP:0152", "commit", "2026-11-24T13:30:00Z", "#501 merged (9f3e2a1)");
}

#[test]
fn resume_card_shows_entries_after_handoff() {
    let (d, mut s) = store_in("after");
    new_map(&mut s);
    let c = the_card(&s);
    for k in ["journal:NEW-MAP:0151", "journal:NEW-MAP:0152", "journal:NEW-MAP:0150"] {
        assert!(c.contains(k), "{k} came after handoff:NEW-MAP:014: {c}");
    }
    // The decision before the handoff is not listed again.
    assert!(!c.contains("journal:NEW-MAP:0149"), "{c}");
    assert!(c.len() <= crate::resume::CARD_BYTES, "{} bytes: {c}", c.len());
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn resume_card_marks_superseded_next() {
    let (d, mut s) = store_in("superseded");
    new_map(&mut s);
    let c = the_card(&s);
    let r = c.lines().find(|l| l.starts_with("- to resume: ")).unwrap_or("");
    assert!(r.contains("handoff:NEW-MAP:014") && r.contains("superseded by journal:NEW-MAP:0152"), "{c}");
    // A handoff whose next step nothing later names is not marked.
    put(&mut s, "handoff:NEW-MAP:015", "handoff", "", "NEW-MAP", "2026-11-24T14:00:00Z", "fix5", "", "Next: open #520\n", &[]);
    let c = the_card(&s);
    assert!(!c.contains("superseded"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn resume_card_goal_has_amendments() {
    let (d, mut s) = store_in("goal");
    new_map(&mut s);
    let body = "Brandon's active /goal:\n\n\"The hard number is under 4.25 ms per frame at 4K.\"\n\nAMENDED 2026-11-04 (Brandon): the hard line is 240 fps (under 4.17 ms) at 1080p and 1440p; 4K is an aim.\n\nOrder of work under it: the PARITY rows.\n";
    put(&mut s, "active-goal-engine-parity", "memory", "project", "", "2026-11-04T16:36:37Z", "The /goal Brandon cleared", "", body, &[]);
    let c = card_after_goal(&s, &d, Some("under 4.25 ms per frame at 4K"));
    let g = c.lines().find(|l| l.contains("active-goal-engine-parity")).unwrap_or("");
    assert!(g.contains("AMENDED 2026-11-04") && g.contains("240 fps"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn resume_card_flags_orphaned_launches() {
    let (d, mut s) = store_in("orphans");
    new_map(&mut s);
    put(&mut s, "handoff:HUD-TEXT:005", "handoff", "", "HUD-TEXT", "2026-11-24T16:36:00Z", "HUD text fix1 reviewed", "", "Next: fix2\n", &[]);
    entry(&mut s, "journal:HUD-TEXT:0030", "launch", "2026-11-24T17:00:00Z", "launch: HUD-TEXT fix2");
    let c = the_card(&s);
    let o = c.lines().find(|l| l.contains("orphaned by restart?")).unwrap_or("");
    assert!(o.contains("journal:HUD-TEXT:0030"), "{c}");
    // NEW-MAP's launch was answered by its review.
    assert!(!o.contains("journal:NEW-MAP:0150"), "{c}");
    // A cleared session's agents may still run: no flag.
    let c = crate::resume::card(&s, &OrchSettings::default(), "", NOW, true).unwrap().unwrap_or_default();
    assert!(!c.contains("orphaned by restart?"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn resume_card_lists_released_holds() {
    let (d, mut s) = store_in("holds");
    put(
        &mut s,
        "journal:RUNQ:0048",
        "journal",
        "decision",
        "RUNQ",
        "2026-11-24T14:16:00Z",
        "drain and install",
        "held: SKY B4 fix, HUD fix2, TASK-JUDGE fix\nuntil: install done",
        "Brandon: lets drain and do the install.\n",
        &[],
    );
    put(&mut s, "journal:RUNQ:0047", "journal", "decision", "RUNQ", "2026-11-24T14:10:00Z", "hold the bake", "held: ROOF bake\nuntil: GPU free", "x\n", &[]);
    let c = the_card(&s);
    assert!(!c.contains("owed now"), "nothing released yet: {c}");
    entry(&mut s, "journal:RUNQ:0049", "note", "2026-11-24T15:00:00Z", "Install done: runq and atlas v0.11.0");
    let c = the_card(&s);
    let o = c.lines().find(|l| l.contains("owed now")).unwrap_or("");
    assert!(o.contains("journal:RUNQ:0048") && o.contains("journal:RUNQ:0049") && o.contains("SKY B4 fix") && o.contains("TASK-JUDGE fix"), "{c}");
    assert!(!o.contains("ROOF bake"), "its release has not come: {c}");
    // A hold closed by a later entry is no longer owed.
    put(&mut s, "journal:RUNQ:0050", "journal", "note", "RUNQ", "2026-11-24T15:30:00Z", "held launches started", "", "x\n", &[("closes", "journal:RUNQ:0048")]);
    let c = the_card(&s);
    assert!(!c.contains("owed now"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

/// HUD-TEXT as it stood: a dozen older decisions, handoff 005, then the
/// answers 0027 and 0029 and the review 0028.
fn hud_text(s: &mut MemoryStore) {
    for i in 1..=12 {
        put(s, &format!("journal:HUD-TEXT:{i:04}"), "journal", "decision", "HUD-TEXT", &format!("2026-11-2{}T09:{i:02}:00Z", 2 + i % 2), &format!("old ruling {i}"), "", &"older material ".repeat(40), &[]);
    }
    put(s, "handoff:HUD-TEXT:005", "handoff", "", "HUD-TEXT", "2026-11-24T16:36:00Z", "HUD text fix1 reviewed", "", "Next: review fix1\n", &[]);
    entry(s, "journal:HUD-TEXT:0027", "answered", "2026-11-24T16:50:00Z", "Brandon: glyph cache stays per atlas");
    put(s, "journal:HUD-TEXT:0028", "journal", "review", "HUD-TEXT", "2026-11-24T17:10:00Z", "review fix1: FIX", "verdict: FIX", "two findings\n", &[]);
    entry(s, "journal:HUD-TEXT:0029", "answered", "2026-11-24T17:20:00Z", "Brandon: outline width 1 px");
}

#[test]
fn lane_recall_lists_entries_newer_than_handoff() {
    let (d, mut s) = store_in("lane-new");
    hud_text(&mut s);
    let t = crate::memory::recall::lane(&s, "HUD-TEXT", 2_000).unwrap();
    let at = |k: &str| t.find(k).unwrap_or_else(|| panic!("{k} missing: {t}"));
    let first_old = t.find("journal:HUD-TEXT:0012").unwrap_or(usize::MAX);
    for k in ["journal:HUD-TEXT:0027", "journal:HUD-TEXT:0028", "journal:HUD-TEXT:0029"] {
        assert!(at(k) < first_old, "{k} before older material: {t}");
    }
    assert!(at("handoff:HUD-TEXT:005") < at("journal:HUD-TEXT:0029"), "{t}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn lane_recall_shows_cross_lane_rulings() {
    let (d, mut s) = store_in("lane-cross");
    hud_text(&mut s);
    let body = "Brandon 2026-11-24: Fable is for speculative work.\nHUD-TEXT's Fable rulings (0001-0003, 0009) end with the run in flight.\n";
    put(&mut s, "journal:TASK-JUDGE:0266", "journal", "decision", "TASK-JUDGE", "2026-11-24T14:09:00Z", "Fable only for speculative work", "", body, &[]);
    // Names HUD-TEXTURE, not HUD-TEXT.
    put(&mut s, "journal:TASK-JUDGE:0267", "journal", "decision", "TASK-JUDGE", "2026-11-24T14:10:00Z", "HUD-TEXTURE stays", "", "HUD-TEXTURE stays\n", &[]);
    // Supersedes a HUD-TEXT record by ref.
    put(&mut s, "journal:SKY:0010", "journal", "decision", "SKY", "2026-11-24T14:20:00Z", "sky text ruling", "", "x\n", &[("supersedes", "journal:HUD-TEXT:0003")]);
    let t = crate::memory::recall::lane(&s, "HUD-TEXT", 6_000).unwrap();
    assert!(t.contains("journal:TASK-JUDGE:0266") && t.contains("journal:SKY:0010"), "{t}");
    assert!(!t.contains("journal:TASK-JUDGE:0267"), "{t}");
    // The card names it beside HUD-TEXT's newer entries.
    let c = the_card(&s);
    let line = c.lines().find(|l| l.starts_with("- after the handoffs")).unwrap_or("");
    let hud = line.split("; ").find(|p| p.contains("handoff:HUD-TEXT:005")).unwrap_or("");
    // At most one ruling of another lane, the newest (fix round 1, M2).
    assert!(hud.contains("journal:SKY:0010") && !hud.contains("journal:TASK-JUDGE:0266"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn queue_closes_on_done_note() {
    let (d, mut s) = store_in("queue");
    for lane in ["RAIN-START", "MUD-SIM", "CLIFF-WET", "CANOPY-EDGE"] {
        put(&mut s, &format!("journal:{lane}:0001"), "journal", crate::queue::ENTRY, lane, "2026-11-01T10:00:00Z", &format!("{lane} start"), "kind: builder\nuses: light-compile", "Kind: builder\n", &[]);
    }
    let waiting = |s: &MemoryStore| crate::queue::waiting(s).unwrap().into_iter().map(|h| h.key).collect::<Vec<_>>();
    assert_eq!(waiting(&s).len(), 4);
    entry(&mut s, "journal:RAIN-START:0064", "note", "2026-11-24T18:01:00Z", "queue: journal:RAIN-START:0001 done (#465 641ad870)");
    entry(&mut s, "journal:MUD-SIM:0040", "note", "2026-11-24T18:02:00Z", "queue: journal:MUD-SIM:0001 done (#504)");
    entry(&mut s, "journal:CLIFF-WET:0030", "commit", "2026-11-24T18:03:00Z", "queue: journal:CLIFF-WET:0001 done");
    put(&mut s, "journal:CANOPY-EDGE:0020", "journal", "note", "CANOPY-EDGE", "2026-11-24T18:04:00Z", "#475 merged", "", "x\n", &[("closes", "journal:CANOPY-EDGE:0001")]);
    // A note that only mentions the key closes nothing.
    entry(&mut s, "journal:CANOPY-EDGE:0021", "note", "2026-11-24T18:05:00Z", "queue: journal:MUD-SIM:0002 waits");
    assert_eq!(waiting(&s), Vec::<String>::new());
    let c = the_card(&s);
    assert!(!c.contains("work queue"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn recall_accepts_memory_prefix() {
    let (d, mut s) = store_in("alias");
    put(&mut s, "active-goal-engine-parity", "memory", "project", "", "2026-11-04T16:36:37Z", "goal", "", "x\n", &[]);
    for k in ["memory:active-goal-engine-parity", "memory:active-goal-engine-parity.md", "active-goal-engine-parity"] {
        assert_eq!(crate::memory::recall::resolve_key(&s, k).unwrap(), "active-goal-engine-parity", "{k}");
    }
    // A pinned version keeps its hash (fix round 1, N2).
    assert_eq!(crate::memory::recall::resolve_key(&s, "memory:active-goal-engine-parity@77e06eace6fe").unwrap(), "active-goal-engine-parity@77e06eace6fe");
    // A key that exists as written is never rewritten.
    put(&mut s, "memory:odd", "memory", "project", "", "2026-11-04T16:36:37Z", "odd", "", "x\n", &[]);
    assert_eq!(crate::memory::recall::resolve_key(&s, "memory:odd").unwrap(), "memory:odd");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

// Fix round 1 of #425 (review journal:ATLAS-RESUME:0009, Brandon's answers
// journal:ATLAS-RESUME:0010).

/// The card for a new process whose previous session's transcript holds an
/// unmet `/goal` (`goal`), or no goal.
fn card_after_goal(s: &MemoryStore, d: &std::path::Path, goal: Option<&str>) -> String {
    let sessions = d.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let line = match goal {
        Some(g) => format!(r#"{{"type":"attachment","attachment":{{"type":"goal_status","met":false,"sentinel":true,"condition":"{g}"}}}}"#),
        None => "{}".to_string(),
    };
    std::fs::write(sessions.join("old.jsonl"), line + "\n").unwrap();
    let new = sessions.join("new.jsonl");
    crate::resume::card(s, &OrchSettings::default(), &new.to_string_lossy(), NOW, false).unwrap().unwrap_or_default()
}

fn after_line(c: &str) -> &str {
    c.lines().find(|l| l.starts_with("- after the handoffs")).unwrap_or("")
}

#[test]
fn goal_line_only_with_active_goal() {
    let (d, mut s) = store_in("goal-gate");
    new_map(&mut s);
    put(&mut s, "active-goal-engine-parity", "memory", "project", "", "2026-11-04T16:36:37Z", "goal", "", "goal\n\nAMENDED 2026-11-04: 240 fps at 1440p.\n", &[]);
    let c = card_after_goal(&s, &d, None);
    assert!(!c.contains("active goal record"), "no /goal, no amendment: {c}");
    let c = card_after_goal(&s, &d, Some("ship it"));
    assert!(c.contains("ship it") && c.contains("active goal record active-goal-engine-parity"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

/// GOAL and CANOPY-EDGE as they stood on the live store: other lanes'
/// decisions cite GOAL's records by key ("GOAL:0034 baked spline").
fn two_lanes(s: &mut MemoryStore) {
    for (i, lane) in ["CANOPY-EDGE", "ROOF", "SKY", "MUD-SIM", "CLIFF-WET"].iter().enumerate() {
        for j in 0..4 {
            let k = format!("journal:{lane}:{:04}", 10 + j);
            let t = format!("2026-11-23T0{i}:{:02}:00Z", 10 * j);
            put(s, &k, "journal", "decision", lane, &t, &format!("GOAL:00{} baked spline holds", 30 + j), "", &format!("per journal:GOAL:00{} the bake stays\n", 30 + j), &[]);
        }
    }
    put(s, "handoff:GOAL:003", "handoff", "", "GOAL", "2026-11-24T10:00:00Z", "GOAL parity rows", "", "Next: profile row 4\n", &[]);
    entry(s, "journal:GOAL:0035", "review", "2026-11-24T11:00:00Z", "review row 4: MERGE");
    entry(s, "journal:GOAL:0036", "note", "2026-11-24T12:00:00Z", "row 4 at 4.1 ms");
    put(s, "handoff:CANOPY-EDGE:002", "handoff", "", "CANOPY-EDGE", "2026-11-24T09:00:00Z", "canopy edge fix1", "", "Next: fix2\n", &[]);
    entry(s, "journal:CANOPY-EDGE:0021", "decision", "2026-11-24T09:30:00Z", "edge fade 2 m");
    entry(s, "journal:CANOPY-EDGE:0022", "commit", "2026-11-24T09:40:00Z", "edge fade shipped");
}

#[test]
fn two_lane_card_shows_each_lanes_entries_after_handoff() {
    let (d, mut s) = store_in("two-lanes");
    two_lanes(&mut s);
    let c = the_card(&s);
    let line = after_line(&c);
    let part = |h: &str| line.split("; ").find(|p| p.contains(h)).unwrap_or("").to_string();
    let goal = part("handoff:GOAL:003");
    assert!(goal.contains("journal:GOAL:0035") || goal.contains("journal:GOAL:0036"), "GOAL's own entries first: {c}");
    // A record cited by key is not a ruling about GOAL.
    assert!(!goal.contains("journal:ROOF:") && !goal.contains("journal:SKY:") && !goal.contains("journal:CANOPY-EDGE:"), "{c}");
    let canopy = part("handoff:CANOPY-EDGE:002");
    assert!(canopy.contains("journal:CANOPY-EDGE:0021") || canopy.contains("journal:CANOPY-EDGE:0022"), "CANOPY-EDGE keeps its share: {c}");
    assert!(c.len() <= crate::resume::CARD_BYTES, "{} bytes: {c}", c.len());
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn cross_lane_ruling_needs_whole_lane_name_not_key() {
    let (d, mut s) = store_in("whole-name");
    let t = "2026-11-24T10:00:00Z";
    put(&mut s, "journal:CANOPY-EDGE:0021", "journal", "decision", "CANOPY-EDGE", t, "GOAL:0034 baked spline", "", "see journal:GOAL:0034\n", &[]);
    put(&mut s, "journal:TASK-JUDGE:0266", "journal", "decision", "TASK-JUDGE", t, "frame target", "", "GOAL's frame target holds at 4.17 ms\n", &[]);
    put(&mut s, "journal:SKY:0010", "journal", "answered", "SKY", t, "sky budget", "lanes: ROOF, GOAL", "x\n", &[]);
    put(&mut s, "journal:ROOF:0004", "journal", "decision", "ROOF", t, "roof bake", "", "x\n", &[("supersedes", "journal:GOAL:0001")]);
    let keys: Vec<String> = s.rulings_naming("GOAL", "").unwrap().into_iter().map(|h| h.key).collect();
    assert_eq!(keys, ["journal:TASK-JUDGE:0266", "journal:SKY:0010", "journal:ROOF:0004"]);
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

/// HUD-TEXT's launch with nothing after it in the journal.
fn hud_launch(s: &mut MemoryStore) {
    new_map(s);
    put(s, "handoff:HUD-TEXT:005", "handoff", "", "HUD-TEXT", "2026-11-24T16:36:00Z", "HUD text fix1 reviewed", "", "Next: fix2\n", &[]);
    entry(s, "journal:HUD-TEXT:0030", "launch", "2026-11-24T17:00:00Z", "launch: HUD-TEXT fix2");
}

#[test]
fn orphans_read_from_previous_session_transcripts() {
    let (d, mut s) = store_in("orphan-transcripts");
    hud_launch(&mut s);
    let sub = d.join("sessions").join("old").join("subagents");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("agent-aaaa1111.meta.json"), r#"{"description":"HUD-TEXT fix2","finished":false}"#).unwrap();
    std::fs::write(sub.join("agent-bbbb2222.meta.json"), r#"{"description":"done already","finished":true}"#).unwrap();
    let c = card_after_goal(&s, &d, None);
    let o = c.lines().find(|l| l.contains("orphaned by restart?")).unwrap_or("");
    assert!(o.contains("HUD-TEXT fix2 (aaaa1111"), "{c}");
    assert!(!o.contains("done already"), "{c}");
    // The transcripts are exact: the journal guess is not used.
    assert!(!o.contains("journal:HUD-TEXT:0030"), "{c}");
    // Every agent of the previous session finished: no flag.
    std::fs::write(sub.join("agent-aaaa1111.meta.json"), r#"{"description":"HUD-TEXT fix2","finished":true}"#).unwrap();
    let c = card_after_goal(&s, &d, None);
    assert!(!c.contains("orphaned by restart?"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn orphan_fallback_counts_any_later_non_launch_entry() {
    let (d, mut s) = store_in("orphan-fallback");
    hud_launch(&mut s);
    // A brief-writer hands back with a note, research with an answer.
    entry(&mut s, "journal:HUD-TEXT:0031", "note", "2026-11-24T17:30:00Z", "fix2 brief written");
    entry(&mut s, "journal:RM-092:0005", "launch", "2026-11-24T18:00:00Z", "launch: research RM-092");
    entry(&mut s, "journal:RM-092:0006", "answered", "2026-11-24T18:10:00Z", "research: road mesh answers");
    entry(&mut s, "journal:ROOF:0007", "launch", "2026-11-24T19:00:00Z", "launch: ROOF bake");
    entry(&mut s, "journal:ROOF:0008", "launch", "2026-11-24T19:05:00Z", "launch: ROOF bake again");
    let c = the_card(&s);
    let o = c.lines().find(|l| l.contains("orphaned by restart?")).unwrap_or("");
    assert!(o.contains("journal:ROOF:0008") && o.contains("journal:ROOF:0007"), "a launch is no answer: {c}");
    assert!(!o.contains("journal:HUD-TEXT:0030") && !o.contains("journal:RM-092:0005"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hold_until_matches_whole_condition() {
    let (d, mut s) = store_in("hold-whole");
    put(&mut s, "journal:RUNQ:0060", "journal", "decision", "RUNQ", "2026-11-24T14:00:00Z", "wait for review", "held: SKY B5\nuntil: review", "x\n", &[]);
    put(&mut s, "journal:RUNQ:0061", "journal", "decision", "RUNQ", "2026-11-24T14:01:00Z", "wait for install", "held: HUD fix3\nuntil: install done", "x\n", &[]);
    // Not releases: a word inside a longer one, a one-word condition in
    // another lane, a phrase inside a longer word.
    entry(&mut s, "journal:SKY:0020", "note", "2026-11-24T15:00:00Z", "reviewer notes filed");
    entry(&mut s, "journal:TASK-JUDGE:0300", "review", "2026-11-24T15:10:00Z", "review #424: MERGE");
    entry(&mut s, "journal:ROOF:0010", "note", "2026-11-24T15:20:00Z", "Reinstall done for the bake box");
    let c = the_card(&s);
    assert!(!c.contains("owed now"), "{c}");
    // Releases: the one word in the hold's own lane; the phrase as words
    // in any lane.
    entry(&mut s, "journal:RUNQ:0062", "note", "2026-11-24T16:00:00Z", "review in: MERGE");
    entry(&mut s, "journal:ATLAS:0100", "note", "2026-11-24T16:10:00Z", "Install done: atlas v0.12.0");
    let c = the_card(&s);
    let o = c.lines().find(|l| l.contains("owed now")).unwrap_or("");
    assert!(o.contains("journal:RUNQ:0060 (released by journal:RUNQ:0062)"), "{c}");
    assert!(o.contains("journal:RUNQ:0061 (released by journal:ATLAS:0100)"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn fix_verdict_review_does_not_supersede_next() {
    let (d, mut s) = store_in("fix-verdict");
    put(&mut s, "handoff:ROADS:020", "handoff", "", "ROADS", "2026-11-24T14:00:00Z", "roads fix1 pushed", "", "Next: merge #501 after review\n", &[]);
    put(&mut s, "journal:ROADS:0101", "journal", "review", "ROADS", "2026-11-24T15:00:00Z", "review atlas-dev#501 fix1: FIX", "verdict: FIX", "two musts\n", &[]);
    put(&mut s, "journal:ROADS:0102", "journal", "review", "ROADS", "2026-11-24T15:10:00Z", "review #501 fix1: REQUEST CHANGES", "", "one must\n", &[]);
    let c = the_card(&s);
    assert!(!c.contains("superseded"), "a FIX review leaves the merge owed: {c}");
    entry(&mut s, "journal:ROADS:0103", "commit", "2026-11-24T16:00:00Z", "atlas-dev#501 merged (9f3e2a1)");
    let c = the_card(&s);
    let r = c.lines().find(|l| l.starts_with("- to resume: ")).unwrap_or("");
    assert!(r.contains("superseded by journal:ROADS:0103"), "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn queue_line_survives_budget() {
    let (d, mut s) = store_in("queue-budget");
    two_lanes(&mut s);
    hud_launch(&mut s);
    for lane in ["RAIN-START-PADDED-LANE", "MUD-SIM-PADDED-LANE"] {
        put(&mut s, &format!("journal:{lane}:0001"), "journal", crate::queue::ENTRY, lane, "2026-11-20T10:00:00Z", &format!("{lane} start"), "kind: builder\nuses: light-compile", "Kind: builder\n", &[]);
    }
    for i in 0..3 {
        entry(&mut s, &format!("journal:ORPHAN-WORK-{i}:0001"), "launch", &format!("2026-11-24T20:0{i}:00Z"), "launch: a long running builder agent");
    }
    let amended = format!("goal\n\nAMENDED 2026-11-04: {}\n", "the hard line is 240 fps ".repeat(8));
    put(&mut s, "active-goal-engine-parity", "memory", "project", "", "2026-11-04T16:36:37Z", "goal", "", &amended, &[]);
    let c = the_card(&s);
    assert!(c.contains("- work queue: "), "{c}");
    assert!(after_line(&c).contains("handoff:"), "{c}");
    assert!(c.len() <= crate::resume::CARD_BYTES, "{} bytes: {c}", c.len());
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}
