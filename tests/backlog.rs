//! `atlas backlog`: the comb over a store fixture (never the live store).
//! Every fixture is dated around one fixed time, [`T`], so the windows (live
//! 7 days, turn 2 days, overdue 14 days, stale 30 days) are exact.

mod common;

use atlas::backlog::{self, Input, Report, Settings, State};
use atlas::memory::{Draft, MemoryStore, Place};

/// The comb's time in every test.
const T: &str = "2026-11-10T14:00";

fn draft(title: &str, meta: &str, body: &str, time: &str) -> Draft {
    Draft {
        title: title.into(),
        meta: meta.into(),
        body: body.into(),
        reason: "fixture".into(),
        author: "test".into(),
        client: "claude-code".into(),
        provenance: None,
        time: Some(time.into()),
    }
}

/// A journal entry of kind `sub` in `lane`.
fn entry(s: &mut MemoryStore, key: &str, sub: &str, lane: &str, time: &str, title: &str, body: &str) {
    let mut w = s.writer().unwrap();
    w.create(key, "journal", sub, lane, &draft(title, "", body, time), None).unwrap();
    w.commit().unwrap();
}

/// A planned queue item: `queued`, `planned: true`, with extra meta lines.
fn planned(s: &mut MemoryStore, key: &str, lane: &str, time: &str, title: &str, meta: &str) {
    let meta = format!("kind: builder\npriority: normal\nuses: compile\n{meta}{}planned: true", if meta.is_empty() { "" } else { "\n" });
    let mut w = s.writer().unwrap();
    w.create(key, "journal", "queued", lane, &draft(title, &meta, "scope", time), None).unwrap();
    w.commit().unwrap();
}

/// An INTENT item (`intent:rNNNNN`, kind rule), placed at the export's end.
fn intent(s: &mut MemoryStore, key: &str, time: &str, text: &str) {
    let mut w = s.writer().unwrap();
    w.create(key, "rule", "", "intent", &draft(text.lines().next().unwrap_or(""), "", text, time), Some(("intent", Place::End))).unwrap();
    w.commit().unwrap();
}

/// An INTENT block (a section heading and its prose), placed at the end.
fn block(s: &mut MemoryStore, key: &str, time: &str, text: &str) {
    let mut w = s.writer().unwrap();
    w.create(key, "block", "", "intent", &draft("", "", text, time), Some(("intent", Place::End))).unwrap();
    w.commit().unwrap();
}

/// Retires a record as of `time`.
fn retire_at(s: &mut MemoryStore, key: &str, reason: &str, time: &str) {
    let mut w = s.writer().unwrap();
    w.retire(key, reason).unwrap();
    w.commit().unwrap();
    s.conn().execute("UPDATE retirements SET time = ?1 WHERE rid = (SELECT rid FROM records WHERE key = ?2)", [time, key]).unwrap();
}

/// The settings with `backlog.asker` set to `name`, through the config.
fn asked_by(name: &str) -> Settings {
    Settings::from_config(Some(&atlas::json::parse(&format!("{{\"asker\": {name:?}}}")).unwrap())).unwrap()
}

/// The fixtures below are led by this asker.
fn named() -> Settings {
    asked_by("Brandon")
}

fn comb_at(s: &MemoryStore, at: &str, running: &[String], all: bool) -> Report {
    backlog::comb(s, &named(), &Input { as_of: at, running, index_db: None, all }).unwrap()
}

fn comb(s: &MemoryStore) -> Report {
    comb_at(s, T, &[], false)
}

fn state(r: &Report, id: &str) -> Option<State> {
    r.get(id).map(|row| row.state)
}

#[test]
fn a_placed_item_is_not_live() {
    let mut s = MemoryStore::in_memory().unwrap();
    // The predecessor merged 4 days ago; TRACKS waits on it, so its turn came.
    entry(&mut s, "journal:RAIN-START:0001", "commit", "RAIN-START", "2026-11-06T10:00:00.000Z", "RAIN-START: PR #1 merged", "");
    planned(&mut s, "journal:TRACKS:0001", "TRACKS", "2026-11-04T15:54:00.000Z", "TRACKS: prints and ruts", "order: 3\nafter: RAIN-START");
    // Placed 15 days ago, nothing since.
    planned(&mut s, "journal:OLD-ONE:0001", "OLD-ONE", "2026-10-26T10:00:00.000Z", "OLD-ONE: an old item", "");
    // Placed 3 days ago, waiting on free text.
    planned(&mut s, "journal:FRESH:0001", "FRESH", "2026-11-07T10:00:00.000Z", "FRESH: a fresh item", "after: engine parity");
    let r = comb(&s);
    assert_eq!(state(&r, "TRACKS"), Some(State::Overdue), "{r:?}");
    assert_eq!(r.get("TRACKS").unwrap().turn_after.as_deref(), Some("RAIN-START"));
    assert_eq!(state(&r, "OLD-ONE"), Some(State::Overdue));
    assert_eq!(state(&r, "FRESH"), Some(State::Waiting));
    // A queue entry is placement, not liveness: the RAIN-START commit is
    // older than nothing here, but the item it waits on is listed, not hidden.
    assert!(r.get("RAIN-START").is_none(), "a merged lane with no queued item is not backlog");
}

#[test]
fn asked_again_ranks_first() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:OLDER:0001", "OLDER", "2026-10-28T10:00:00.000Z", "OLDER: placed long ago", "");
    planned(&mut s, "journal:ASKED:0001", "ASKED", "2026-11-05T10:00:00.000Z", "ASKED: placed later", "");
    // Two asks in Brandon's words, on distinct dates; the second is a
    // near-duplicate of the first.
    entry(&mut s, "journal:ASKED:0002", "decision", "ASKED", "2026-11-07T10:00:00.000Z", "Brandon 2026-11-07: \"still waiting on ASKED, it fell out of the queue\"", "");
    intent(&mut s, "intent:r00001", "2026-11-09T10:00:00.000Z", "- 2026-11-09 Brandon: still waiting on ASKED, it fell out of the queue again");
    // Status words are not asks, from anyone.
    entry(&mut s, "journal:ASKED:0003", "note", "ASKED", "2026-11-08T10:00:00.000Z", "ASKED has not started; no PR or commit", "");
    entry(&mut s, "journal:queue:0001", "decision", "queue", "2026-11-08T11:00:00.000Z", "Brandon 2026-11-08: where is the ASKED work", "");
    let r = comb(&s);
    let asked = r.get("ASKED").expect("ASKED is listed");
    assert_eq!(asked.asks, 2, "{asked:?}");
    assert_eq!(asked.state, State::Overdue);
    let pos = |id: &str| r.rows.iter().position(|x| x.id == id).unwrap();
    assert!(pos("ASKED") < pos("OLDER"), "{r:?}");
    // The near-duplicate is one more ask, not one more ruling.
    assert!(asked.rulings_dated <= 1, "{asked:?}");
}

#[test]
fn only_a_brief_commit_or_agent_is_live() {
    let mut s = MemoryStore::in_memory().unwrap();
    let recent = "2026-11-09T10:00:00.000Z";
    for (i, sub) in ["question", "decision", "note", "measurement", "review", "next"].iter().enumerate() {
        let lane = format!("NOT-LIVE-{i}");
        planned(&mut s, &format!("journal:{lane}:0001"), &lane, "2026-11-05T10:00:00.000Z", &format!("{lane}: item"), "");
        entry(&mut s, &format!("journal:{lane}:0002"), sub, &lane, recent, &format!("{lane} {sub}"), "");
    }
    planned(&mut s, "journal:HANDED:0001", "HANDED", "2026-11-05T10:00:00.000Z", "HANDED: item", "");
    {
        let mut w = s.writer().unwrap();
        w.create("handoff:HANDED:001", "handoff", "", "HANDED", &draft("HANDED handoff", "", "", recent), None).unwrap();
        w.commit().unwrap();
    }
    for (lane, sub, time) in [("BRIEFED", "brief", recent), ("COMMITTED", "commit", recent), ("STALE-BRIEF", "brief", "2026-11-02T10:00:00.000Z")] {
        planned(&mut s, &format!("journal:{lane}:0001"), lane, "2026-11-01T10:00:00.000Z", &format!("{lane}: item"), "");
        entry(&mut s, &format!("journal:{lane}:0002"), sub, lane, time, &format!("{lane} {sub}"), "");
    }
    planned(&mut s, "journal:RUNNING:0001", "RUNNING", "2026-11-01T10:00:00.000Z", "RUNNING: item", "");
    let running = vec!["Kind: builder\nLane: RUNNING\n\nDo the thing.".to_string()];
    let r = comb_at(&s, T, &running, false);
    for i in 0..6 {
        assert!(r.get(&format!("NOT-LIVE-{i}")).is_some(), "NOT-LIVE-{i} is flagged: {r:?}");
    }
    assert!(r.get("HANDED").is_some(), "a handoff is not live work");
    assert!(r.get("BRIEFED").is_none());
    assert!(r.get("COMMITTED").is_none());
    assert!(r.get("RUNNING").is_none(), "a running agent's Lane: makes it live");
    assert!(r.get("STALE-BRIEF").is_some(), "a brief older than 7 days does not count");
}

#[test]
fn a_section_heading_carries_the_ask() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:SECT-ID:0001", "SECT-ID", "2026-11-04T15:54:00.000Z", "SECT-ID: placed", "");
    block(&mut s, "intent:b00001", "2026-11-04T15:55:00.000Z", "## 2026-11-04 Brandon: SECT-ID queue placement
Brandon: \"still waiting on the thing we discussed\"");
    let r = comb(&s);
    let row = r.get("SECT-ID").unwrap();
    assert_eq!((row.asks, row.state), (1, State::Overdue), "{row:?}");
}

#[test]
fn fold_edges_hide_the_child() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:PARENT-X:0001", "PARENT-X", "2026-10-20T10:00:00.000Z", "PARENT-X: the parent", "");
    planned(&mut s, "journal:CHILD-Y:0001", "CHILD-Y", "2026-10-20T10:00:00.000Z", "CHILD-Y: the child", "");
    intent(&mut s, "intent:r00001", "2026-11-01T10:00:00.000Z", "- 2026-11-01 Brandon: CHILD-Y folds into PARENT-X.");
    let r = comb(&s);
    assert!(r.get("PARENT-X").is_some());
    assert!(r.get("CHILD-Y").is_none(), "{r:?}");
}

#[test]
fn a_deferral_waits_with_its_reason() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:AI-PREFER:0001", "AI-PREFER", "2026-10-20T10:00:00.000Z", "AI-PREFER: preferences", "");
    intent(&mut s, "intent:r00001", "2026-11-09T10:00:00.000Z", "- 2026-11-09 Brandon: AI-PREFER stays in the AI-MICRO backlog.");
    let r = comb(&s);
    let row = r.get("AI-PREFER").expect("listed as waiting");
    assert_eq!(row.state, State::Waiting);
    assert!(row.reason.contains("AI-MICRO"), "{row:?}");
}

#[test]
fn exact_joins_only() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:TRACKS:0001", "TRACKS", "2026-11-07T10:00:00.000Z", "TRACKS: prints and ruts", "");
    entry(&mut s, "journal:queue:0001", "decision", "queue", "2026-11-08T10:00:00.000Z", "Brandon 2026-11-08: still waiting on vehicle tracks", "");
    entry(&mut s, "journal:queue:0002", "decision", "queue", "2026-11-08T11:00:00.000Z", "Brandon 2026-11-08: still waiting on TRACKS-2 and UNTRACKS", "");
    entry(&mut s, "journal:OTHER:0001", "brief", "OTHER", "2026-11-09T10:00:00.000Z", "TRACKS-2 brief", "Lane: OTHER\nBuild TRACKS-2 and UNTRACKS.");
    let r = comb(&s);
    let row = r.get("TRACKS").expect("not live, not asked");
    assert_eq!((row.asks, row.state), (0, State::Waiting), "{row:?}");
    // The exact ID joins.
    entry(&mut s, "journal:queue:0003", "decision", "queue", "2026-11-09T11:00:00.000Z", "Brandon 2026-11-09: still waiting on TRACKS", "");
    let r = comb(&s);
    assert_eq!(r.get("TRACKS").unwrap().asks, 1);
}

#[test]
fn done_and_started_are_not_backlog() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:MARKED:0001", "MARKED", "2026-10-20T10:00:00.000Z", "MARKED: done by mark", "");
    {
        let mut w = s.writer().unwrap();
        w.create("journal:MARKED:0002", "journal", "note", "MARKED", &Draft { client: "atlas-queue".into(), ..draft("queue: journal:MARKED:0001 done", "done: journal:MARKED:0001", "", "2026-11-01T10:00:00.000Z") }, None).unwrap();
        w.commit().unwrap();
    }
    planned(&mut s, "journal:STARTED:0001", "STARTED", "2026-10-20T10:00:00.000Z", "STARTED: started", "");
    retire_at(&mut s, "journal:STARTED:0001", "started", "2026-11-01T10:00:00.000Z");
    planned(&mut s, "journal:MERGED:0001", "MERGED", "2026-10-20T10:00:00.000Z", "MERGED: merged long ago", "");
    entry(&mut s, "journal:MERGED:0002", "commit", "MERGED", "2026-10-25T10:00:00.000Z", "MERGED: PR #5 merged", "");
    let r = comb(&s);
    assert!(r.get("MARKED").is_none(), "{r:?}");
    assert!(r.get("STARTED").is_none(), "{r:?}");
    assert!(r.get("MERGED").is_none(), "{r:?}");
}

#[test]
fn as_of_reads_the_past() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:PAST:0001", "PAST", "2026-10-20T10:00:00.000Z", "PAST: placed long ago", "");
    // Written after T: a brief (live), an ask, and a new item.
    entry(&mut s, "journal:PAST:0002", "brief", "PAST", "2026-11-10T15:00:00.000Z", "PAST brief", "");
    entry(&mut s, "journal:PAST:0003", "decision", "PAST", "2026-11-10T15:01:00.000Z", "Brandon 2026-11-10: still waiting on PAST", "");
    planned(&mut s, "journal:LATER:0001", "LATER", "2026-11-10T15:02:00.000Z", "LATER: filed after T", "");
    // Retired after T: still waiting at T.
    planned(&mut s, "journal:GONE:0001", "GONE", "2026-10-20T10:00:00.000Z", "GONE: retired later", "");
    retire_at(&mut s, "journal:GONE:0001", "started", "2026-11-10T16:00:00.000Z");
    // An INTENT item imported after T, dated by the date it states.
    planned(&mut s, "journal:STATED:0001", "STATED", "2026-11-05T10:00:00.000Z", "STATED: placed", "");
    intent(&mut s, "intent:r00001", "2026-11-12T00:00:00.000Z", "- 2026-11-08 Brandon: still waiting on STATED");
    let r = comb(&s);
    assert_eq!(state(&r, "PAST"), Some(State::Overdue), "{r:?}");
    assert_eq!(r.get("PAST").unwrap().asks, 0);
    assert!(r.get("LATER").is_none());
    assert!(r.get("GONE").is_some());
    assert_eq!(r.get("STATED").unwrap().asks, 1);
    // Later, the brief makes PAST live and GONE has started.
    let r = comb_at(&s, "2026-11-10T17:00", &[], false);
    assert!(r.get("PAST").is_none());
    assert!(r.get("GONE").is_none());
    assert!(r.get("LATER").is_some());
}

#[test]
fn stale_unplaced_items_are_all_only() {
    let mut s = MemoryStore::in_memory().unwrap();
    intent(&mut s, "intent:r00001", "2026-10-01T10:00:00.000Z", "- 2026-10-01 Brandon: queue STALE-ONE after the rest.");
    intent(&mut s, "intent:r00002", "2026-11-08T10:00:00.000Z", "- 2026-11-08 Brandon: queue FRESH-ONE next.");
    let r = comb(&s);
    assert!(r.get("STALE-ONE").is_none() && r.get("FRESH-ONE").is_none(), "unplaced IDs are --all only: {r:?}");
    let r = comb_at(&s, T, &[], true);
    assert_eq!(state(&r, "STALE-ONE"), Some(State::Stale), "{r:?}");
    assert_eq!(r.get("STALE-ONE").unwrap().score, 0, "a stale item is unranked");
    assert_eq!(state(&r, "FRESH-ONE"), Some(State::Unplaced));
    assert!(r.get("FRESH-ONE").unwrap().score > 0);
}

#[test]
fn a_brief_naming_the_id_makes_it_live() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:BODIES:0003", "BODIES", "2026-10-20T10:00:00.000Z", "P7B-R3: ragdoll, with PI-313 alongside", "");
    planned(&mut s, "journal:BODIES:0004", "BODIES", "2026-10-20T10:00:00.000Z", "PI-313: the corpse lifecycle event ring", "");
    entry(&mut s, "journal:RAGDOLL:0001", "brief", "RAGDOLL", "2026-11-09T10:00:00.000Z", "Brief draft Z:/scratch/ragdoll/BRIEF_RAGDOLL_1.md", "Lane: RAGDOLL\nIt carries PI-313.");
    let r = comb(&s);
    assert!(r.get("PI-313").is_none(), "{r:?}");
    assert!(r.get("P7B-R3").is_some());
}

#[test]
fn a_later_closing_ruling_wins() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:KILLED-Z:0001", "KILLED-Z", "2026-10-20T10:00:00.000Z", "KILLED-Z: an item", "");
    intent(&mut s, "intent:r00001", "2026-10-21T10:00:00.000Z", "- 2026-10-21 Brandon: KILLED-Z goes to the front of the queue.");
    intent(&mut s, "intent:r00002", "2026-11-09T10:00:00.000Z", "- 2026-11-09 Brandon: KILLED-Z is dropped.");
    planned(&mut s, "journal:KEPT-Z:0001", "KEPT-Z", "2026-11-05T10:00:00.000Z", "KEPT-Z: placed after its drop", "");
    intent(&mut s, "intent:r00003", "2026-11-01T10:00:00.000Z", "- 2026-11-01 Brandon: KEPT-Z is parked.");
    let r = comb(&s);
    assert!(r.get("KILLED-Z").is_none(), "{r:?}");
    assert!(r.get("KEPT-Z").is_some(), "a placement after the closing ruling wins");
}

#[test]
fn overdue_by_ask_needs_a_recent_ask() {
    let mut s = MemoryStore::in_memory().unwrap();
    entry(&mut s, "journal:queue:0001", "decision", "queue", "2026-11-02T10:00:00.000Z", "Brandon 2026-11-02: still waiting on REPLACED", "");
    planned(&mut s, "journal:REPLACED:0001", "REPLACED", "2026-11-05T10:00:00.000Z", "REPLACED: placed after the ask", "");
    let r = comb(&s);
    let row = r.get("REPLACED").unwrap();
    assert_eq!((row.asks, row.state), (1, State::Waiting), "{row:?}");
}

#[test]
fn turn_come_uses_written_order_only() {
    let mut s = MemoryStore::in_memory().unwrap();
    entry(&mut s, "journal:FIRST:0001", "commit", "FIRST", "2026-11-05T10:00:00.000Z", "FIRST: PR #2 merged", "");
    // Order in INTENT prose only: not a written order.
    planned(&mut s, "journal:PROSE:0001", "PROSE", "2026-11-04T10:00:00.000Z", "PROSE: second in prose", "");
    intent(&mut s, "intent:r00001", "2026-11-04T10:00:00.000Z", "- 2026-11-04 Brandon: Queue order: FIRST, PROSE, THIRD.");
    // A lane chain by `order:`: the first started, so the second's turn came.
    planned(&mut s, "journal:CHAIN:0001", "CHAIN", "2026-11-04T10:00:00.000Z", "CHAIN-A: first", "order: 9.5");
    planned(&mut s, "journal:CHAIN:0002", "CHAIN", "2026-11-04T10:00:00.000Z", "CHAIN-B: second", "order: 9.6");
    retire_at(&mut s, "journal:CHAIN:0001", "started", "2026-11-06T10:00:00.000Z");
    let r = comb(&s);
    assert_eq!(state(&r, "PROSE"), Some(State::Waiting), "{r:?}");
    assert_eq!(state(&r, "CHAIN-B"), Some(State::Overdue), "{r:?}");
    assert_eq!(r.get("CHAIN-B").unwrap().turn_after.as_deref(), Some("CHAIN-A"));
}

#[test]
fn a_dismissal_is_permanent() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:RIGID-INSTANCES:0001", "RIGID-INSTANCES", "2026-10-20T10:00:00.000Z", "RIGID-INSTANCES: rigid parts", "");
    intent(&mut s, "intent:r00001", "2026-11-01T10:00:00.000Z", "- 2026-11-01 Brandon: RIGID-ORGANIC carries the rigid parts.");
    assert!(comb(&s).get("RIGID-INSTANCES").is_some());
    assert!(backlog::dismiss(&mut s, "RIGID-INSTANCES", "intent:r99999").is_err(), "the reason must be a record");
    let key = backlog::dismiss(&mut s, "RIGID-INSTANCES", "intent:r00001").unwrap();
    assert!(s.head(&key).unwrap().is_some());
    assert!(comb(&s).get("RIGID-INSTANCES").is_none());
    assert!(comb(&s).get("RIGID-INSTANCES").is_none(), "and on every later run");
}

#[test]
fn settings_names_and_states() {
    let v = atlas::json::parse(r#"{"w_ask": 77, "turn_days": 3}"#).unwrap();
    let s = Settings::from_config(Some(&v)).unwrap();
    assert_eq!((s.w_ask, s.turn_days), (77, 3));
    assert_eq!(Settings::from_config(None).unwrap().w_ask, Settings::default().w_ask);
    let bad = atlas::json::parse(r#"{"no_such_key": 1}"#).unwrap();
    assert!(Settings::from_config(Some(&bad)).is_err());
    let names: Vec<&str> = [State::Overdue, State::Waiting, State::Unplaced, State::Stale].iter().map(|s| s.name()).collect();
    assert_eq!(names, ["aging", "waiting", "unplaced", "stale"]);
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:OLD:0001", "OLD", "2026-10-01T10:00", "OLD: placed long ago", "");
    planned(&mut s, "journal:NEW:0001", "NEW", "2026-11-09T10:00", "NEW: placed yesterday", "");
    let r = comb(&s);
    let over: Vec<&str> = r.of(State::Overdue).map(|x| x.id.as_str()).collect();
    let wait: Vec<&str> = r.of(State::Waiting).map(|x| x.id.as_str()).collect();
    assert_eq!((over, wait), (vec!["OLD"], vec!["NEW"]));
}

/// End to end through the binary: `atlas backlog` reaches the comb (not the
/// grammar fallback) and prints the aging heading; `--json` prints rows.
#[test]
fn the_binary_runs_the_backlog() {
    let t = common::Temp::new("backlog-bin");
    let cfg = t.path().join("atlas.json");
    std::fs::write(&cfg, format!("{{\"db\": {:?}, \"vaults\": [], \"memory\": {{\"store\": {:?}}}}}", t.path().join("index.sqlite").display().to_string(), t.path().join("memory.sqlite").display().to_string())).unwrap();
    let backlog = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"))
            .arg("--config")
            .arg(&cfg)
            .arg("backlog")
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
    let (ok, out, err) = backlog(&[]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("backlog as of"), "{out}");
    assert!(out.contains("aging"), "{out}");
    let (ok, out, err) = backlog(&["--json"]);
    assert!(ok, "{out}{err}");
    let v = atlas::json::parse(out.trim()).unwrap_or_else(|e| panic!("{e:?}: {out}"));
    assert!(v.get("rows").and_then(|r| r.as_array()).is_some(), "{out}");
}

/// A `ruling:` record that defines `id` (a `defines:` ref).
fn ruling(s: &mut MemoryStore, key: &str, time: &str, text: &str, id: &str) {
    let mut w = s.writer().unwrap();
    w.create(key, "rule", "", "ruling", &draft(text, "", text, time), None).unwrap();
    w.add_refs(key, &[("defines".to_string(), id.to_string())]).unwrap();
    w.commit().unwrap();
}

#[test]
fn a_defined_id_honours_deferral_and_its_date() {
    let mut s = MemoryStore::in_memory().unwrap();
    // Defined long ago, then parked: waiting with the reason, not aging.
    ruling(&mut s, "ruling:00001", "2026-10-01T10:00:00.000Z", "- 2026-10-01 Brandon: DEF-ONE is the new piece.", "DEF-ONE");
    intent(&mut s, "intent:r00001", "2026-11-01T10:00:00.000Z", "- 2026-11-01 Brandon: DEF-ONE is parked.");
    // Dropped before it was defined: the later definition wins.
    intent(&mut s, "intent:r00002", "2026-10-15T10:00:00.000Z", "- 2026-10-15 Brandon: DEF-TWO is dropped.");
    ruling(&mut s, "ruling:00002", "2026-11-05T10:00:00.000Z", "- 2026-11-05 Brandon: DEF-TWO is the new piece.", "DEF-TWO");
    // Dropped after it was defined: hidden.
    ruling(&mut s, "ruling:00003", "2026-10-01T10:00:00.000Z", "- 2026-10-01 Brandon: DEF-THREE is the new piece.", "DEF-THREE");
    intent(&mut s, "intent:r00003", "2026-11-02T10:00:00.000Z", "- 2026-11-02 Brandon: DEF-THREE is dropped.");
    let r = comb(&s);
    let one = r.get("DEF-ONE").expect("DEF-ONE is listed");
    assert_eq!(one.state, State::Waiting, "{one:?}");
    assert!(one.reason.starts_with("deferred by intent:r00001"), "{one:?}");
    assert!(r.get("DEF-TWO").is_some(), "a close dated before the definition does not hide it: {r:?}");
    assert!(r.get("DEF-THREE").is_none(), "{r:?}");
}

/// Vault rows: a note whose `status` is queued is dropped by a closing
/// ruling, a commit that says merged, or a done mark, as a queue item is.
#[test]
fn a_vault_row_honours_closes_merges_and_marks() {
    let t = common::Temp::new("backlog-vault");
    let vault = t.path().join("plans");
    for id in ["VAULT-KEEP", "VAULT-DROP", "VAULT-MERGED", "VAULT-DONE"] {
        common::write(&vault.join(format!("{id}.md")), &format!("---\nid: {id}\nstatus: queued\n---\n\n# {id}\n"));
    }
    let db = t.path().join("index.sqlite");
    {
        let vaults = atlas::paths::Vaults::new(&[("plans".to_string(), vault.clone())]).unwrap();
        let mut store = atlas::store::Store::open(&db, vaults).unwrap();
        store.rescan().unwrap();
    }
    let mut s = MemoryStore::in_memory().unwrap();
    intent(&mut s, "intent:r00001", "2026-11-09T10:00:00.000Z", "- 2026-11-09 Brandon: VAULT-DROP is dropped.");
    entry(&mut s, "journal:VAULT-MERGED:0001", "commit", "VAULT-MERGED", "2026-11-09T10:00:00.000Z", "VAULT-MERGED: PR #9 merged", "");
    {
        let mut w = s.writer().unwrap();
        w.create("journal:queue:0001", "journal", "note", "queue", &Draft { client: "atlas-queue".into(), ..draft("queue: VAULT-DONE done", "done: VAULT-DONE", "", "2026-11-09T10:00:00.000Z") }, None).unwrap();
        w.commit().unwrap();
    }
    // After the notes' file times, and past the commit's live window.
    let at = "2030-01-01T00:00";
    let r = backlog::comb(&s, &named(), &Input { as_of: at, running: &[], index_db: Some(&db), all: false }).unwrap();
    assert!(r.get("VAULT-KEEP").is_some(), "{r:?}");
    assert!(r.get("VAULT-DROP").is_none(), "a closing ruling hides it: {r:?}");
    assert!(r.get("VAULT-MERGED").is_none(), "a merged commit hides it: {r:?}");
    assert!(r.get("VAULT-DONE").is_none(), "a done mark hides it: {r:?}");
}

#[test]
fn a_near_duplicate_ask_is_per_id_and_still_closes() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:AAA-1:0001", "AAA-1", "2026-11-01T10:00:00.000Z", "AAA-1: one item", "");
    planned(&mut s, "journal:BBB-2:0001", "BBB-2", "2026-11-01T10:00:00.000Z", "BBB-2: another item", "");
    entry(&mut s, "journal:queue:0001", "decision", "queue", "2026-11-07T10:00:00.000Z", "Brandon 2026-11-07: still waiting on AAA-1", "");
    // Jaccard 3/6 with the line above, but about another ID: not a
    // duplicate, and its close is read.
    entry(&mut s, "journal:queue:0002", "decision", "queue", "2026-11-08T10:00:00.000Z", "Brandon 2026-11-08: still waiting on BBB-2, dropped", "");
    let r = comb(&s);
    assert_eq!(r.get("AAA-1").map(|x| x.asks), Some(1), "{r:?}");
    assert!(r.get("BBB-2").is_none(), "{r:?}");
}

#[test]
fn placement_is_the_first_version() {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:REVISED:0001", "REVISED", "2026-10-20T10:00:00.000Z", "REVISED: an item", "");
    intent(&mut s, "intent:r00001", "2026-11-01T10:00:00.000Z", "- 2026-11-01 Brandon: REVISED is dropped.");
    // Relaunched as another kind after the drop: a v2, not a new placement.
    {
        let meta = "kind: reviewer\npriority: normal\nuses: compile\nplanned: true";
        let mut w = s.writer().unwrap();
        w.revise("journal:REVISED:0001", None, &draft("REVISED: an item", meta, "scope", "2026-11-05T10:00:00.000Z")).unwrap();
        w.commit().unwrap();
    }
    let r = comb(&s);
    assert!(r.get("REVISED").is_none(), "{r:?}");
}


#[test]
fn day_settings_are_capped_at_100_years() {
    for k in ["age_cap_days", "live_days", "turn_days", "overdue_days", "stale_days"] {
        let ok = atlas::json::parse(&format!("{{\"{k}\": 36500}}")).unwrap();
        assert!(Settings::from_config(Some(&ok)).is_ok(), "{k}");
        let bad = atlas::json::parse(&format!("{{\"{k}\": 36501}}")).unwrap();
        let e = Settings::from_config(Some(&bad)).unwrap_err();
        assert!(e.contains(&format!("backlog.{k}")), "{e}");
    }
    // Config::load rejects it too, naming the key.
    let t = common::Temp::new("backlog-cap");
    let cfg = t.path().join("atlas.json");
    std::fs::write(&cfg, format!("{{\"db\": {:?}, \"vaults\": [], \"memory\": {{\"store\": {:?}}}, \"backlog\": {{\"live_days\": 99999999999}}}}", t.path().join("index.sqlite").display().to_string(), t.path().join("memory.sqlite").display().to_string())).unwrap();
    let e = atlas::config::Config::load(&cfg).unwrap_err().to_string();
    assert!(e.contains("backlog.live_days"), "{e}");
}

/// The CLI over a filled store (PR #144 finding 9): `--json` carries an
/// aging row, `--all` adds the unplaced and stale rows, `dismiss` writes a
/// dismissal the next run honours, and an unknown option is refused.
#[test]
fn the_binary_lists_dismisses_and_refuses() {
    let t = common::Temp::new("backlog-cli");
    let cfg = t.path().join("atlas.json");
    let store = t.path().join("memory.sqlite");
    std::fs::write(&cfg, format!("{{\"db\": {:?}, \"vaults\": [], \"memory\": {{\"store\": {:?}}}}}", t.path().join("index.sqlite").display().to_string(), store.display().to_string())).unwrap();
    {
        let mut s = MemoryStore::open(&store).unwrap();
        planned(&mut s, "journal:OLD:0001", "OLD", "2026-10-01T10:00:00.000Z", "OLD: placed long ago", "");
        planned(&mut s, "journal:NEW:0001", "NEW", "2026-11-09T10:00:00.000Z", "NEW: placed yesterday", "");
        intent(&mut s, "intent:r00001", "2026-10-01T10:00:00.000Z", "- 2026-10-01 Brandon: queue STALE-ONE after the rest.");
        intent(&mut s, "intent:r00002", "2026-11-08T10:00:00.000Z", "- 2026-11-08 Brandon: queue FRESH-ONE next.");
    }
    let backlog = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"))
            .arg("--config")
            .arg(&cfg)
            .arg("backlog")
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
    let rows = |out: &str| -> Vec<(String, String)> {
        let v = atlas::json::parse(out.trim()).unwrap_or_else(|e| panic!("{e:?}: {out}"));
        let rows = v.get("rows").and_then(|r| r.as_array()).unwrap_or_else(|| panic!("{out}"));
        rows.iter().map(|r| (r.get("id").and_then(|x| x.as_str()).unwrap().to_string(), r.get("state").and_then(|x| x.as_str()).unwrap().to_string())).collect()
    };
    let pair = |id: &str, st: &str| (id.to_string(), st.to_string());

    let (ok, out, err) = backlog(&["--as-of", T, "--json"]);
    assert!(ok, "{out}{err}");
    let r = rows(&out);
    assert!(r.contains(&pair("OLD", "aging")), "{out}");
    assert!(r.contains(&pair("NEW", "waiting")), "{out}");
    assert!(!r.iter().any(|(id, _)| id == "STALE-ONE" || id == "FRESH-ONE"), "unplaced rows are --all only: {out}");

    let (ok, out, err) = backlog(&["--all", "--as-of", T, "--json"]);
    assert!(ok, "{out}{err}");
    let r = rows(&out);
    assert!(r.contains(&pair("STALE-ONE", "stale")) && r.contains(&pair("FRESH-ONE", "unplaced")), "{out}");
    let (ok, out, err) = backlog(&["--all", "--as-of", T]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("ruled, never placed") && out.contains("STALE-ONE"), "{out}");

    let (ok, out, err) = backlog(&["dismiss", "OLD", "--because", "intent:r99999"]);
    assert!(!ok, "the reason must be a record: {out}{err}");
    let (ok, out, err) = backlog(&["dismiss", "OLD", "--because", "intent:r00001"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("dismissed OLD"), "{out}");
    let (ok, out, err) = backlog(&["--as-of", T, "--json"]);
    assert!(ok, "{out}{err}");
    assert!(!rows(&out).iter().any(|(id, _)| id == "OLD"), "a dismissed row stays hidden: {out}");

    let (ok, out, err) = backlog(&["--no-such-option"]);
    assert!(!ok, "{out}{err}");
    assert!(err.contains("unknown option --no-such-option") && err.contains("usage: atlas backlog"), "{err}");
}

/// One placed item with asks led by two names, on distinct dates.
fn two_askers() -> MemoryStore {
    let mut s = MemoryStore::in_memory().unwrap();
    planned(&mut s, "journal:ASKED:0001", "ASKED", "2026-11-01T10:00:00.000Z", "ASKED: placed", "");
    entry(&mut s, "journal:ASKED:0002", "decision", "ASKED", "2026-11-07T10:00:00.000Z", "Alice 2026-11-07: still waiting on ASKED", "");
    intent(&mut s, "intent:r00001", "2026-11-09T10:00:00.000Z", "- 2026-11-09 Alice: ASKED fell out of the queue, asked again");
    entry(
        &mut s,
        "journal:ASKED:0003",
        "decision",
        "ASKED",
        "2026-11-08T10:00:00.000Z",
        "Carol 2026-11-08: ASKED is the second time I ask",
        "",
    );
    s
}

fn asks(s: &MemoryStore, settings: &Settings) -> usize {
    let r = backlog::comb(s, settings, &Input { as_of: T, running: &[], index_db: None, all: false }).unwrap();
    r.get("ASKED").map_or(0, |x| x.asks)
}

#[test]
fn a_set_asker_counts_only_that_names_leads() {
    let s = two_askers();
    // Both lead forms, `Name DATE:` and `- DATE Name:`.
    assert_eq!(asks(&s, &asked_by("Alice")), 2);
    assert_eq!(asks(&s, &asked_by("Carol")), 1);
    // A name that leads nothing counts nothing.
    assert_eq!(asks(&s, &asked_by("Brandon")), 0);
    assert_eq!(asks(&s, &asked_by("Ali")), 0, "a name is a whole word, in both forms");
}

#[test]
fn an_unset_asker_counts_no_lead() {
    let s = two_askers();
    assert_eq!(asks(&s, &Settings::default()), 0);
    // A blank name is no asker; a non-string is an error naming the key.
    assert_eq!(asks(&s, &asked_by("  ")), 0);
    let bad = atlas::json::parse(r#"{"asker": 7}"#).unwrap();
    assert!(Settings::from_config(Some(&bad)).unwrap_err().contains("backlog.asker"));
}
