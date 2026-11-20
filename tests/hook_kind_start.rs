//! #275: a subagent's start context in the default `recall` mode follows
//! its brief's `Kind:` line (reviewer and research starts run no lane recall
//! and get no pointers), and the records a brief names are not sent again.

mod common;

use std::path::Path;

use atlas::hooks::{HookEvent, Hooks};
use atlas::memory::settings::Settings;
use atlas::memory::{Draft, MemoryStore};
use common::*;

fn settings(dir: &Path) -> Settings {
    let mut s = Settings::from_config(None, &dir.join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.recall.fts_floor = 0.0;
    s.recall.hybrid_fts_floor = 0.0;
    s.compact.claude = atlas::autocompact::ClaudeCompact { window: 1_000_000, ..Default::default() };
    s
}

/// Stores `(key, kind, scope, sub, title, body)` records.
fn add(s: &Settings, recs: &[(&str, &str, &str, &str, &str, &str)]) {
    let mut store = MemoryStore::open(&s.store).unwrap();
    let mut w = store.writer().unwrap();
    for (key, kind, scope, sub, title, body) in recs {
        let d = Draft {
            title: (*title).into(),
            body: (*body).into(),
            reason: "kind start test".into(),
            author: "t".into(),
            client: "t".into(),
            ..Draft::default()
        };
        w.create(key, kind, sub, scope, &d, None).unwrap();
    }
    w.commit().unwrap();
}

fn ev(event: &str, session: &str) -> HookEvent {
    HookEvent { event: event.into(), session_id: session.into(), ..HookEvent::default() }
}

/// A launch of `brief` in session `sid`, then its agent's SubagentStart:
/// the context it is given.
fn start(h: &mut Hooks, sid: &str, brief: &str) -> String {
    h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: brief.into(), ..ev("PreToolUse", sid) });
    h.handle(&HookEvent { agent_id: format!("a-{sid}"), agent_type: "general-purpose".into(), ..ev("SubagentStart", sid) }).context
}

const PROSE: &str = "Check the gadget exporter for ALPHA and BETA: it writes with temp files and renames them into place.";

/// Q2 (journal:ATLAS-RELEASE:0156): a reviewer or research brief naming
/// two lanes gets no lane view and no pointer; a builder brief, today's
/// recall (guard).
#[test]
fn reviewer_and_research_starts_run_no_lane_recall() {
    let t = Temp::new("kind-start");
    let s = settings(t.path());
    add(
        &s,
        &[
            ("journal:ALPHA:0001", "journal", "ALPHA", "brief", "Ship the gadget exporter", ""),
            ("journal:BETA:0001", "journal", "BETA", "next", "Wire the gadget hooks", ""),
            (
                "rules:r00001",
                "rule",
                "rules",
                "",
                "gadget exporter rule",
                "- the gadget exporter writes with temp files and renames them into place.\n",
            ),
        ],
    );
    let mut h = Hooks::open(s).unwrap();
    for kind in ["reviewer", "research"] {
        let c = start(&mut h, kind, &format!("Kind: {kind}\n{PROSE}"));
        assert!(!c.contains("lane ALPHA") && !c.contains("lane BETA"), "{kind}: {c}");
        assert!(!c.contains("rules:r00001"), "{kind}: {c}");
    }
    let c = start(&mut h, "builder", &format!("Kind: builder\n{PROSE}"));
    assert!(c.contains("lane ALPHA") && c.contains("lane BETA"), "builder: {c}");
}

/// The records a builder brief names (a `recall key=`, a `Queue:` line, a
/// Gherkin tag) are left out of its lane view, and their room goes to the
/// next entry.
#[test]
fn keys_a_brief_names_are_not_injected_again() {
    let t = Temp::new("kind-named");
    let mut s = settings(t.path());
    s.bytes_per_token = 1.0;
    s.recall.lane_budget_tokens = 700;
    s.recall.select.enabled = false;
    let walrus = "The walrus format keeps every field. ".repeat(11);
    let yak = "The yak index is rebuilt each night. ".repeat(11);
    add(
        &s,
        &[
            ("journal:ALPHA:0014", "journal", "ALPHA", "decision", "Keep the yak index", &yak),
            ("journal:ALPHA:0013", "journal", "ALPHA", "decision", "Pick the walrus format", &walrus),
            ("journal:ALPHA:0012", "journal", "ALPHA", "next", "Port the walrus reader", ""),
            ("handoff:ALPHA:003", "handoff", "ALPHA", "", "Walrus half done", ""),
        ],
    );
    let brief = "Kind: builder\nLane: ALPHA\nFirst: recall key=handoff:ALPHA:003\nQueue: journal:ALPHA:0012\n\n@journal:ALPHA:0013\nScenario: the walrus reader\n  Given the walrus format\n";
    let mut h = Hooks::open(s).unwrap();
    let c = start(&mut h, "named", brief);
    for k in ["handoff:ALPHA:003", "journal:ALPHA:0012", "journal:ALPHA:0013"] {
        assert!(!c.contains(k), "{k} sent again: {c}");
    }
    assert!(c.contains("journal:ALPHA:0014"), "the freed room goes to the next entry: {c}");
}

/// A pointer that is a near-duplicate of a record the brief names (as the
/// collapse check judges it) is not sent, not even as a collapsed line.
#[test]
fn near_duplicates_of_named_keys_are_not_injected() {
    use atlas::memory::vectors;
    let t = Temp::new("kind-near");
    let mut s = settings(t.path());
    std::fs::create_dir_all(s.model.parent().unwrap()).unwrap();
    std::fs::write(&s.model, atlas::embed::tiny_model_bytes(true, 7)).unwrap();
    s.recall.cos_floor = -1.0;
    s.recall.cos_alone = 2.0;
    s.recall.cos_margin = 2.0;
    s.recall.select.dup_cos = -1.0;
    add(
        &s,
        &[
            ("rules:r00012", "rule", "rules", "", "", "- The wombat ledger rotates every ninety days, and the old ledger is archived."),
            ("rules:r00011", "rule", "rules", "", "", "- The wombat ledger rotates every ninety days."),
            (
                "wombat-feed",
                "memory",
                "",
                "feedback",
                "- [Wombat feed](wombat-feed.md) - dawn",
                "Wombats are fed at dawn; the feed ledger is checked weekly.",
            ),
        ],
    );
    {
        let store = MemoryStore::open(&s.store).unwrap();
        let m = atlas::embed::Model::load(&s.model).unwrap();
        while vectors::embed_pending(&store, &m, 8, 1).unwrap() > 0 {}
    }
    let mut h = Hooks::open(s).unwrap();
    let c = start(
        &mut h,
        "near",
        "Kind: builder\nPer rules:r00012, how often does the wombat ledger rotate, every ninety days? When are wombats fed?\n",
    );
    assert!(!c.contains("rules:r00012"), "{c}");
    assert!(!c.contains("rules:r00011"), "near-duplicate of a named key sent: {c}");
    assert!(c.contains("wombat-feed"), "{c}");
}

/// With no vectors (no collapse check to lean on), a record the brief
/// names is still not pointed to, and the next one is.
#[test]
fn a_named_key_is_not_pointed_to_without_vectors() {
    let t = Temp::new("kind-novec");
    let s = settings(t.path());
    add(
        &s,
        &[
            ("rules:r00021", "rule", "rules", "", "", "- The otter dam is inspected every spring."),
            ("rules:r00022", "rule", "rules", "", "", "- The otter dam gate is oiled every spring."),
        ],
    );
    let mut h = Hooks::open(s).unwrap();
    let c = start(&mut h, "novec", "Kind: builder\nPer rules:r00021, when is the otter dam inspected and its gate oiled every spring?\n");
    assert!(!c.contains("rules:r00021"), "{c}");
    assert!(c.contains("rules:r00022"), "{c}");
}

/// A launch of `brief` as `subagent_type`, then its agent's SubagentStart.
fn start_typed(h: &mut Hooks, sid: &str, subagent_type: &str, brief: &str) -> String {
    h.handle(&HookEvent {
        tool_name: "Agent".into(),
        tool_prompt: brief.into(),
        tool_input: atlas::json!({"subagent_type": subagent_type, "prompt": brief}),
        ..ev("PreToolUse", sid)
    });
    h.handle(&HookEvent { agent_id: format!("a-{sid}"), agent_type: subagent_type.into(), ..ev("SubagentStart", sid) }).context
}

/// S1 (review of #312): with no `Kind:` line, a start's kind comes from its
/// launch's type as the cap classifier reads it: an `Explore` launch is
/// research, so a brief naming lane ALPHA gets no lane view and no pointer;
/// a general-purpose launch of the same brief, today's recall (guard).
#[test]
fn an_explore_launch_with_no_kind_line_runs_no_lane_recall() {
    let t = Temp::new("kind-type");
    let s = settings(t.path());
    add(
        &s,
        &[
            ("journal:ALPHA:0001", "journal", "ALPHA", "brief", "Ship the gadget exporter", ""),
            (
                "rules:r00031",
                "rule",
                "rules",
                "",
                "gadget exporter rule",
                "- the gadget exporter writes with temp files and renames them into place.\n",
            ),
        ],
    );
    let mut h = Hooks::open(s).unwrap();
    let brief = "Lane: ALPHA\nFind where the gadget exporter for ALPHA writes with temp files and renames them into place.";
    let c = start_typed(&mut h, "explore", "Explore", brief);
    assert!(!c.contains("lane ALPHA"), "Explore: {c}");
    assert!(!c.contains("rules:r00031"), "Explore: {c}");
    let c = start_typed(&mut h, "gp", "general-purpose", brief);
    assert!(c.contains("lane ALPHA"), "general-purpose: {c}");
}

/// N1 (review of #312): when a brief names every entry of its lane, the
/// lane view is empty (no bare count line), so the pointers still come.
#[test]
fn a_lane_whose_entries_are_all_named_gives_no_view_and_pointers_still_come() {
    let t = Temp::new("kind-allnamed");
    let s = settings(t.path());
    add(
        &s,
        &[
            ("handoff:ALPHA:001", "handoff", "ALPHA", "", "Gadget half done", ""),
            ("journal:ALPHA:0002", "journal", "ALPHA", "next", "Port the gadget reader", ""),
            ("journal:ALPHA:0003", "journal", "ALPHA", "decision", "Keep the gadget format", ""),
            (
                "rules:r00041",
                "rule",
                "rules",
                "",
                "gadget exporter rule",
                "- the gadget exporter writes with temp files and renames them into place.\n",
            ),
        ],
    );
    let brief = format!(
        "Kind: builder\nLane: ALPHA\nFirst: recall key=handoff:ALPHA:001\nQueue: journal:ALPHA:0002\n\n@journal:ALPHA:0003\n{PROSE}\n"
    );
    let mut h = Hooks::open(s).unwrap();
    let c = start(&mut h, "allnamed", &brief);
    assert!(!c.contains("lane ALPHA:"), "bare lane header sent: {c}");
    assert!(c.contains("rules:r00041"), "pointers suppressed: {c}");
}
