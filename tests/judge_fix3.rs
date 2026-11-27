//! atlas-dev #424 fix round 3 (TASK-JUDGE 0294 M1): a `Judge: climb
//! <model>/<effort>` is admitted only at the next ladder rung above the
//! lane's last run (TASK-JUDGE 0304; was: above the pick), never dodges a due
//! explore, runs after the speculative and no-pick rules, and a legacy
//! "climb ..." string row does not seed the counter as an explore.

mod common;

use std::path::{Path, PathBuf};

use atlas::hooks::{HookAnswer, HookEvent, Hooks};
use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
use atlas::taskjudge::{ClassifierSettings, JudgeMode, Override, ScoreSource};
use common::*;

struct Fx {
    t: Temp,
    s: Settings,
    log: PathBuf,
}

/// Enforced, the kind source (a builder scores 5), Brandon's 0280 map: the
/// pick is opus/medium, the rung below it sonnet/medium. The dogfood log
/// holds `n` routed rows.
fn fx(tag: &str, n: usize) -> Fx {
    let t = Temp::new(tag);
    let mut s = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.compact.claude = atlas::autocompact::ClaudeCompact { window: 1_000_000, ..Default::default() };
    let log = t.path().join("dogfood").join("log.jsonl");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, routed(n)).unwrap();
    let cells = t.path().join("cells.json");
    std::fs::write(&cells, r#"{"cells": [{"key": {"kind": null, "has_brief": null}, "scope": "medium", "unk": "few"}]}"#).unwrap();
    s.orchestrate.judge.enabled = true;
    s.orchestrate.judge.cells = Some(cells);
    s.orchestrate.judge.mode = JudgeMode::Enforced;
    s.orchestrate.judge.log = Some(log.clone());
    s.orchestrate.judge.classifier = ClassifierSettings { enabled: true, source: ScoreSource::Kind, ..Default::default() };
    let m = &mut s.orchestrate.judge.classifier.map;
    m.builder = Some(Override { enabled: true, max_score: 3, pick: ("sonnet".into(), "medium".into()) });
    m.base[5] = ("opus".into(), "medium".into());
    Fx { t, s, log }
}

fn routed(n: usize) -> String {
    (0..n).map(|_| "{\"lane\": \"X\", \"explore\": false, \"brandon_named\": false, \"outcome\": null}\n").collect()
}

const SPECIFIED: &str = "Acceptance: tests/gadget.rs::spins\nDeliverables: 2\nCaptures: no\nArt: no\nBuild the gadget.\n";
const SPECULATIVE: &str = "Acceptance: none named\nDeliverables: 1\nCaptures: no\nArt: no\nTry the gadget.\n";

/// The launch prompt for a brief file holding `brief`, with a `Judge:` line.
fn prompt(dir: &Path, brief: &str, judge: &str) -> String {
    let p = dir.join("BRIEF_GADGET.md");
    std::fs::write(&p, brief).unwrap();
    format!("Kind: builder\nLane: GADGET\nYour brief is {}; read it.\n{judge}", p.display()).replace('\\', "/")
}

fn launch(h: &mut Hooks, prompt: &str, model: &str, effort: &str, id: &str) -> HookAnswer {
    let ev = HookEvent {
        event: "PreToolUse".into(),
        session_id: "s1".into(),
        tool_name: "Agent".into(),
        tool_prompt: prompt.into(),
        tool_input: json!({"prompt": prompt, "description": "gadget", "model": model, "effort": effort, "subagent_type": "builder"}),
        tool_use_id: id.into(),
        ..HookEvent::default()
    };
    h.handle(&ev)
}

fn rows(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()).map(|l| json::parse(l).unwrap()).collect()
}

/// Appends the GADGET lane's last run, at opus/medium, to the log.
fn failed_run(f: &Fx) {
    let mut t = std::fs::read_to_string(&f.log).unwrap();
    t.push_str("{\"lane\": \"GADGET\", \"launched\": \"opus/medium\", \"explore\": false, \"brandon_named\": false, \"outcome\": null}\n");
    std::fs::write(&f.log, t).unwrap();
}

#[test]
fn climb_must_be_the_next_rung() {
    // Not due (two routed rows), pick opus/medium, the lane's last run
    // opus/medium (TASK-JUDGE 0304 replaces 0294's "outranks the pick").
    let f = fx("tj3-outrank", 1);
    failed_run(&f);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    for (i, (m, e)) in [("sonnet", "low"), ("sonnet", "max"), ("opus", "low"), ("opus", "medium"), ("fable", "low")].into_iter().enumerate()
    {
        let d = launch(&mut h, &prompt(f.t.path(), SPECIFIED, &format!("Judge: climb {m}/{e}")), m, e, &format!("d{i}")).deny;
        let d = d.unwrap_or_else(|| panic!("climb {m}/{e} off the next rung was admitted"));
        assert!(d.contains("opus/high") && d.contains("opus/medium"), "{m}/{e}: {d}");
        // Not due: the denial does not offer an explore.
        assert!(!d.contains("Judge: explore"), "{m}/{e}: {d}");
    }
    assert_eq!(rows(&f.log).len(), 2, "no row for a denied climb");
    for (i, (m, e)) in [("opus", "high"), ("fable", "medium"), ("fable", "high")].into_iter().enumerate() {
        let a = launch(&mut h, &prompt(f.t.path(), SPECIFIED, &format!("Judge: climb {m}/{e}")), m, e, &format!("a{i}"));
        assert!(a.deny.is_none(), "{m}/{e}: {a:?}");
        assert!(a.context.contains("climb launch at"), "{}", a.context);
    }
    let r = rows(&f.log);
    assert_eq!((r.len(), r[4]["climb"].as_bool(), r[4]["launched"].as_str()), (5, Some(true), Some("fable/high")));
}

#[test]
fn climb_at_pick_does_not_dodge_due_explore() {
    // Due (four routed rows, the last the lane's run at opus/medium), pick
    // opus/medium, the rung below sonnet/medium.
    let f = fx("tj3-dodge", 3);
    failed_run(&f);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    for (i, (m, e)) in [("opus", "medium"), ("sonnet", "medium")].into_iter().enumerate() {
        let d = launch(&mut h, &prompt(f.t.path(), SPECIFIED, &format!("Judge: climb {m}/{e}")), m, e, &format!("d{i}")).deny;
        let d = d.unwrap_or_else(|| panic!("climb {m}/{e} dodged the due explore"));
        assert!(d.contains("one rung above") && d.contains("opus/high"), "{m}/{e}: {d}");
    }
    assert_eq!(rows(&f.log).len(), 4, "no row for a denied climb");
    // A climb above the pick is admitted and leaves the explore due.
    let a = launch(&mut h, &prompt(f.t.path(), SPECIFIED, "Judge: climb opus/high"), "opus", "high", "c1");
    assert!(a.deny.is_none(), "{a:?}");
    let d = launch(&mut h, &prompt(f.t.path(), SPECIFIED, ""), "opus", "medium", "p1").deny.unwrap_or_default();
    assert!(d.contains("the explore launch is due"), "{d}");
    let a = launch(&mut h, &prompt(f.t.path(), SPECIFIED, "Judge: explore"), "sonnet", "medium", "x1");
    assert!(a.deny.is_none() && a.context.contains("explore launch at"), "{a:?}");
}

#[test]
fn speculative_and_no_pick_rules_run_before_climb() {
    // Speculative brief, the rule's pair fable/high: a climb is no way around it.
    let mut f = fx("tj3-spec", 1);
    f.s.orchestrate.judge.speculative = Some(("fable".into(), "high".into()));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    for (i, (m, e)) in [("fable", "max"), ("opus", "high"), ("fable", "high")].into_iter().enumerate() {
        let d = launch(&mut h, &prompt(f.t.path(), SPECULATIVE, &format!("Judge: climb {m}/{e}")), m, e, &format!("s{i}")).deny;
        let d = d.unwrap_or_else(|| panic!("climb {m}/{e} on a speculative brief was admitted"));
        assert!(d.contains("speculative") && d.contains("fable/high"), "{m}/{e}: {d}");
    }
    let a = launch(&mut h, &prompt(f.t.path(), SPECULATIVE, ""), "fable", "high", "s9");
    assert!(a.deny.is_none() && a.context.contains("speculative"), "{a:?}");

    // No pick (the head source with no encoder): a climb is denied too.
    let mut f = fx("tj3-nopick", 1);
    f.s.orchestrate.judge.classifier.source = ScoreSource::Head;
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = launch(&mut h, &prompt(f.t.path(), SPECIFIED, "Judge: climb fable/high"), "fable", "high", "n1").deny;
    let d = d.unwrap_or_else(|| panic!("climb with no pick was admitted"));
    assert!(d.contains("classifier unavailable"), "{d}");
    assert_eq!(rows(&f.log).len(), 1);
}

#[test]
fn bare_climb_denial_quotes_the_line_exactly() {
    let f = fx("tj3-bare", 1);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = launch(&mut h, &prompt(f.t.path(), SPECIFIED, "Judge: climb"), "opus", "high", "b1").deny.unwrap_or_default();
    assert!(d.contains("`Judge: climb` names no model/effort pair"), "{d}");
}

#[test]
fn seed_skips_legacy_climb_strings() {
    use atlas::judge_enforce::seed;
    let row = |explore: &str| format!("{{\"lane\": \"X\", \"explore\": {explore}, \"brandon_named\": false}}\n");
    // A hand-written "climb ..." row (live row 44) is a climb: skipped, not an explore.
    for s in ["\"climb (row 41 axis explore failed review)\"", "\"Climb: opus/high\"", "\" climb\""] {
        let log = format!("{}{}{}", routed(3), row(s), routed(2));
        assert_eq!(seed(&log), 5, "{s}");
    }
    // Legacy "axis" rows still count as explores.
    let log = format!("{}{}{}", routed(3), row("\"axis\""), routed(2));
    assert_eq!(seed(&log), 2);
    // A word that only starts with the letters is not a climb.
    let log = format!("{}{}{}", routed(3), row("\"climber probe\""), routed(2));
    assert_eq!(seed(&log), 2);
}
