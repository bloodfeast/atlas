//! TASK-JUDGE cadence and climb (journal:TASK-JUDGE:0303-0307): the explore
//! launch every `orchestrate.judge.explore_every` routed launches, a climb
//! exactly one ladder rung above the lane's last (failed) run, fable/high
//! the top rung, an xhigh/max effort only by Brandon's choice, and
//! `Judge: continue` re-judged on its own brief.

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
/// holds `rows`.
fn fx(tag: &str, rows: &str) -> Fx {
    let t = Temp::new(tag);
    let mut s = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.compact.claude = atlas::autocompact::ClaudeCompact { window: 1_000_000, ..Default::default() };
    let log = t.path().join("dogfood").join("log.jsonl");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, rows).unwrap();
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

/// `n` routed rows on another lane.
fn routed(n: usize) -> String {
    (0..n).map(|_| "{\"lane\": \"X\", \"explore\": false, \"brandon_named\": false, \"outcome\": null}\n").collect()
}

/// One GADGET row, picked at `pick` and launched at `launched`, with extra
/// fields (`, "explore": true`).
fn gadget(pick: &str, launched: &str, extra: &str) -> String {
    format!(
        "{{\"lane\": \"GADGET\", \"pick\": \"{pick}\", \"launched\": \"{launched}\", \"brandon_named\": false, \"outcome\": null, \"agent_id\": \"ag1\", \"tool_use_id\": \"tu1\"{extra}}}\n"
    )
}

const SPECIFIED: &str = "Acceptance: tests/gadget.rs::spins\nDeliverables: 2\nCaptures: no\nArt: no\nBuild the gadget.\n";

/// The launch prompt for a brief file, with a `Judge:` line.
fn prompt(dir: &Path, judge: &str) -> String {
    let p = dir.join("BRIEF_GADGET.md");
    std::fs::write(&p, SPECIFIED).unwrap();
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

/// The denial for a `Judge:` line at `m/e`, or a panic naming it.
fn denied(h: &mut Hooks, f: &Fx, judge: &str, m: &str, e: &str) -> String {
    launch(h, &prompt(f.t.path(), judge), m, e, "d").deny.unwrap_or_else(|| panic!("`{judge}` at {m}/{e} was admitted"))
}

fn admitted(h: &mut Hooks, f: &Fx, judge: &str, m: &str, e: &str, id: &str) -> HookAnswer {
    let a = launch(h, &prompt(f.t.path(), judge), m, e, id);
    assert!(a.deny.is_none(), "`{judge}` at {m}/{e}: {a:?}");
    a
}

fn every(text: &str) -> Result<usize, String> {
    let c = atlas::config::Config::parse(text, Path::new(".")).map_err(|e| e.to_string())?;
    Ok(atlas::orchestrate::resolve(c.orchestrate.as_ref(), None, None, |_| None).judge.explore_every())
}

#[test]
fn explore_every_reads_config() {
    assert_eq!(every(r#"{"orchestrate": {"judge": {"enabled": true}}}"#), Ok(5));
    assert_eq!(every(r#"{"orchestrate": {"enabled": true}}"#), Ok(5));
    assert_eq!(every(r#"{"orchestrate": {"judge": {"explore_every": 3}}}"#), Ok(3));
    // Set to 3: after two routed launches the 3rd is the explore launch.
    let mut f = fx("tjc-every3", &routed(2));
    f.s.orchestrate.judge.explore_every = Some(3);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "", "opus", "medium");
    assert!(d.contains("the explore launch is due") && d.contains("sonnet/medium"), "{d}");
    let a = admitted(&mut h, &f, "Judge: explore", "sonnet", "medium", "x1");
    assert!(a.context.contains("explore launch at"), "{}", a.context);
    // Reset: this routed launch and one more, then the explore.
    let a = admitted(&mut h, &f, "", "opus", "medium", "p1");
    assert!(a.context.contains("(2 routed launches until it)"), "{}", a.context);
    admitted(&mut h, &f, "", "opus", "medium", "p2");
    let d = denied(&mut h, &f, "", "opus", "medium");
    assert!(d.contains("the explore launch is due"), "{d}");
    // Absent (5): two routed rows leave the explore two launches off.
    let f = fx("tjc-every5", &routed(2));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let a = admitted(&mut h, &f, "", "opus", "medium", "p1");
    assert!(a.context.contains("(2 routed launches until it)"), "{}", a.context);
}

#[test]
fn explore_every_rejects_below_two() {
    for n in [0, 1] {
        let e = every(&format!(r#"{{"orchestrate": {{"judge": {{"explore_every": {n}}}}}}}"#)).unwrap_err();
        assert!(e.contains("orchestrate.judge.explore_every"), "{n}: {e}");
    }
    let e = every(r#"{"orchestrate": {"judge": {"explore_every": "three"}}}"#).unwrap_err();
    assert!(e.contains("orchestrate.judge.explore_every"), "{e}");
    assert_eq!(every(r#"{"orchestrate": {"judge": {"explore_every": 2}}}"#), Ok(2));
}

#[test]
fn climb_is_one_rung_above_the_failed_run() {
    let f = fx("tjc-rung", &format!("{}{}", routed(1), gadget("opus/medium", "opus/medium", ", \"explore\": false")));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    for (m, e) in [("fable", "medium"), ("fable", "high"), ("fable", "low"), ("opus", "medium"), ("sonnet", "high")] {
        let d = denied(&mut h, &f, &format!("Judge: climb {m}/{e}"), m, e);
        assert!(d.contains("one rung above") && d.contains("opus/high"), "{m}/{e}: {d}");
    }
    assert_eq!(rows(&f.log).len(), 2, "no row for a denied climb");
    let a = admitted(&mut h, &f, "Judge: climb opus/high", "opus", "high", "c1");
    assert!(a.context.contains("climb launch at opus/high"), "{}", a.context);
    let r = rows(&f.log);
    assert_eq!((r.len(), r[2]["climb"].as_bool(), r[2]["launched"].as_str()), (3, Some(true), Some("opus/high")));
    // Never counted toward the explore.
    assert_eq!(atlas::judge_enforce::seed(&std::fs::read_to_string(&f.log).unwrap()), 2);
    // The ladder and the off-ladder step.
    use atlas::judge_enforce::next_rung;
    let p = |m: &str, e: &str| (m.to_string(), e.to_string());
    assert_eq!(next_rung(&p("haiku", "medium")), Some(p("sonnet", "medium")));
    assert_eq!(next_rung(&p("opus", "high")), Some(p("fable", "medium")));
    assert_eq!(next_rung(&p("opus", "low")), Some(p("opus", "medium")));
    assert_eq!(next_rung(&p("haiku", "max")), Some(p("sonnet", "medium")));
    assert_eq!(next_rung(&p("fable", "high")), None);
    assert_eq!(next_rung(&p("fable", "max")), None);
}

#[test]
fn climb_after_failed_explore_returns_to_pick() {
    let f = fx("tjc-back", &gadget("opus/medium", "sonnet/medium", ", \"explore\": true"));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb opus/high", "opus", "high");
    assert!(d.contains("climbs back to the pick, opus/medium"), "{d}");
    let a = admitted(&mut h, &f, "Judge: climb opus/medium", "opus", "medium", "c1");
    assert!(a.context.contains("climb launch at opus/medium"), "{}", a.context);
    assert_eq!(rows(&f.log)[1]["climb"].as_bool(), Some(true));
}

#[test]
fn second_climb_steps_again() {
    let f = fx("tjc-again", &gadget("opus/medium", "opus/high", ", \"explore\": false, \"climb\": true"));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb opus/high", "opus", "high");
    assert!(d.contains("fable/medium"), "{d}");
    admitted(&mut h, &f, "Judge: climb fable/medium", "fable", "medium", "c1");
    // And again from the climb just logged.
    admitted(&mut h, &f, "Judge: climb fable/high", "fable", "high", "c2");
    let d = denied(&mut h, &f, "Judge: climb fable/max", "fable", "max");
    assert!(d.contains("top rung reached"), "{d}");
}

#[test]
fn climb_stops_at_fable_high() {
    let f = fx("tjc-top", &format!("{}{}", routed(4), gadget("opus/medium", "fable/high", ", \"explore\": false, \"climb\": true")));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb fable/max", "fable", "max");
    assert!(d.contains("top rung reached") && d.contains("ask Brandon") && d.contains("Judge: brandon-named"), "{d}");
    // No explore or climb ever names fable/max or an xhigh/max effort.
    for (judge, m, e) in [
        ("Judge: explore fable/max", "fable", "max"),
        ("Judge: explore opus/xhigh", "opus", "xhigh"),
        ("Judge: explore sonnet/max", "sonnet", "max"),
        ("Judge: climb opus/xhigh", "opus", "xhigh"),
    ] {
        let d = denied(&mut h, &f, judge, m, e);
        assert!(d.contains("ask Brandon") && d.contains("Judge: brandon-named"), "{judge}: {d}");
    }
    // A climb below the top from an earlier rung: opus/xhigh is no rung.
    let f = fx("tjc-xhigh", &gadget("opus/medium", "opus/medium", ", \"explore\": false"));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb opus/xhigh", "opus", "xhigh");
    assert!(d.contains("ask Brandon") && d.contains("opus/high"), "{d}");
    // Brandon's choice admits it.
    admitted(&mut h, &f, "Judge: brandon-named fable/max", "fable", "max", "b1");
}

#[test]
fn user_tier_pick_is_suggested_not_launched() {
    let mut f = fx("tjc-user", &routed(1));
    f.s.orchestrate.judge.classifier.map.base[5] = ("fable".into(), "max".into());
    // note op=judge suggests it, with the ask-Brandon line.
    let vaults = common::Fx::new("tjc-user-srv");
    let mut srv = vaults.server();
    srv.set_memory(f.s.clone());
    let j = call(&mut srv, "judge_task", json!({"prompt": prompt(f.t.path(), "").as_str(), "format": "json"}));
    assert_eq!((j["model"].as_str(), j["effort"].as_str()), (Some("fable"), Some("max")), "{j}");
    let line = j["line"].as_str().unwrap_or_default();
    assert!(line.contains("needs Brandon's choice: ask him, then launch with Judge: brandon-named"), "{line}");
    // The hook denies it without brandon-named, naming fable/high.
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "", "fable", "max");
    assert!(d.contains("ask Brandon") && d.contains("Judge: brandon-named") && d.contains("fable/high"), "{d}");
    // fable/high, the pair it may run at instead, is admitted.
    admitted(&mut h, &f, "", "fable", "high", "p1");
    // With brandon-named it is admitted.
    let a = admitted(&mut h, &f, "Judge: brandon-named fable/max", "fable", "max", "b1");
    assert!(a.context.contains("brandon-named fable/max"), "{}", a.context);
    let r = rows(&f.log);
    assert_eq!(
        (r[1]["launched"].as_str(), r[2]["launched"].as_str(), r[2]["brandon_named"].as_bool()),
        (Some("fable/high"), Some("fable/max"), Some(true))
    );
}

#[test]
fn climb_without_a_failed_run_is_denied() {
    // No GADGET row at all.
    let f = fx("tjc-none", &routed(3));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb opus/high", "opus", "high");
    assert!(d.contains("no prior row") && d.contains("GADGET"), "{d}");
    // The named run is not the lane's last row.
    let f = fx("tjc-named", &gadget("opus/medium", "opus/medium", ", \"explore\": false"));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb fable/medium after zz9", "fable", "medium");
    assert!(d.contains("zz9") && d.contains("not the lane's last row") && d.contains("a climb from it runs at opus/high"), "{d}");
    admitted(&mut h, &f, "Judge: climb opus/high after ag1", "opus", "high", "c1");
    // No Lane: line: the last row cannot be found.
    let p = f.t.path().join("BRIEF_GADGET.md");
    let bare = format!("Kind: builder\nYour brief is {}; read it.\nJudge: climb fable/medium", p.display()).replace('\\', "/");
    let d = launch(&mut h, &bare, "fable", "medium", "n1").deny.unwrap_or_else(|| panic!("a climb with no lane was admitted"));
    assert!(d.contains("Lane:"), "{d}");
}

#[test]
fn a_continuation_is_rejudged_from_its_brief() {
    // The last GADGET row is a sonnet/medium explore under an opus/medium
    // pick; four routed rows since, so the explore is due again. The
    // continuation is scored on its own brief (pick opus/medium): the admitted
    // pair is the pick, not the last row's pair.
    let f = fx("tjc-cont", &format!("{}{}", gadget("opus/medium", "sonnet/medium", ", \"explore\": true"), routed(4)));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    for (m, e) in [("sonnet", "medium"), ("sonnet", "high"), ("opus", "high")] {
        let d = denied(&mut h, &f, "Judge: continue", m, e);
        assert!(d.contains("continuation") && d.contains("the pick is opus/medium"), "{m}/{e}: {d}");
    }
    let a = admitted(&mut h, &f, "Judge: continue (context cap, handoff:GADGET:001)", "opus", "medium", "k1");
    assert!(a.context.contains("continue") && a.context.contains("re-judged"), "{}", a.context);
    // With no GADGET row there is nothing to continue.
    let f = fx("tjc-cont-none", &routed(1));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: continue", "opus", "medium");
    assert!(d.contains("no prior row"), "{d}");
}

#[test]
fn a_continuation_may_drop_or_climb() {
    // Continued run at opus/high: the pick for the leftover (opus/medium) is a
    // drop, and only the pick is admitted.
    let f = fx("tjc-drop", &gadget("opus/medium", "opus/high", ", \"explore\": false"));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    denied(&mut h, &f, "Judge: continue", "opus", "high");
    admitted(&mut h, &f, "Judge: continue", "opus", "medium", "k1");
    // Continued run at sonnet/medium: the pick is a climb.
    let f = fx("tjc-climb", &gadget("opus/medium", "sonnet/medium", ", \"explore\": false"));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    denied(&mut h, &f, "Judge: continue", "sonnet", "medium");
    admitted(&mut h, &f, "Judge: continue", "opus", "medium", "k1");
    // The cap is the ladder top: a fable/max pick runs at fable/high.
    let mut f = fx("tjc-top", &gadget("opus/medium", "fable/high", ", \"explore\": false"));
    f.s.orchestrate.judge.classifier.map.base[5] = ("fable".into(), "max".into());
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "Judge: continue", "fable", "high", "k1");
}

#[test]
fn a_user_tier_continuation_still_needs_brandon_named() {
    let mut f = fx("tjc-cont-user", &gadget("opus/medium", "opus/medium", ", \"explore\": false"));
    f.s.orchestrate.judge.classifier.map.base[5] = ("fable".into(), "max".into());
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: continue", "fable", "max");
    assert!(d.contains("brandon-named") && d.contains("top rung under"), "{d}");
    let d = denied(&mut h, &f, "Judge: continue", "opus", "medium");
    assert!(d.contains("pass model \"fable\" and effort \"high\""), "{d}");
    admitted(&mut h, &f, "Judge: brandon-named fable/max", "fable", "max", "n1");
}

#[test]
fn a_continuation_is_not_an_explore() {
    // Four routed rows since the last explore: the explore is due. A
    // continuation at the pick is admitted anyway and leaves the counter.
    let f = fx("tjc-cont-noexp", &format!("{}{}", gadget("opus/medium", "opus/medium", ", \"explore\": false"), routed(4)));
    let log = std::fs::read_to_string(&f.log).unwrap();
    let before = atlas::judge_enforce::seed(&log);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    // The explore rung is denied for a continuation, which is never an explore.
    denied(&mut h, &f, "Judge: continue", "sonnet", "medium");
    admitted(&mut h, &f, "Judge: continue", "opus", "medium", "k1");
    let r = rows(&f.log);
    assert_eq!((r.len(), r[5]["explore"].as_bool()), (6, Some(false)));
    assert_eq!(atlas::judge_enforce::seed(&std::fs::read_to_string(&f.log).unwrap()), before);
    let d = denied(&mut h, &f, "", "opus", "medium");
    assert!(d.contains("the explore launch is due"), "{d}");
    // The counter file: a continuation leaves it, a routed launch moves it.
    let since = |f: &Fx| atlas::judge_enforce::Counter::open(&f.s.store, Some(&f.log)).since;
    assert_eq!(since(&f), before);
    // (The counter file is written first, so the log's seed cannot stand in for it.)
    let f = fx("tjc-cont-counter", &gadget("opus/medium", "opus/medium", ", \"explore\": false"));
    let dir = f.s.store.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("judge-explore.json"), "{\"since_explore\": 3}").unwrap();
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "", "opus", "medium", "r1");
    assert_eq!(since(&f), 4);
    admitted(&mut h, &f, "Judge: continue", "opus", "medium", "k1");
    assert_eq!(since(&f), 4);
    // A climb off the continue row is not counted either, nor a brandon-named launch.
    admitted(&mut h, &f, "Judge: climb opus/high", "opus", "high", "c1");
    assert_eq!(since(&f), 4);
    admitted(&mut h, &f, "Judge: brandon-named opus/high", "opus", "high", "n1");
    assert_eq!(since(&f), 4);
}

#[test]
fn a_failed_continuation_climbs_from_its_own_pair_per_the_ladder() {
    // The run was opus/high; its continuation ran at opus/medium (a drop) and
    // failed: the climb is one rung above opus/medium, not above opus/high.
    let rows = format!("{}{}", gadget("opus/medium", "opus/high", ", \"explore\": false"), gadget("opus/medium", "opus/medium", CONT));
    let f = fx("tjc-cont-failed", &rows);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb fable/medium", "fable", "medium");
    assert!(d.contains("exactly opus/high"), "{d}");
    admitted(&mut h, &f, "Judge: climb opus/high", "opus", "high", "c1");
    // A continuation that climbed to the pick climbs from there too.
    let rows = format!("{}{}", gadget("opus/medium", "sonnet/medium", ", \"explore\": false"), gadget("opus/medium", "opus/medium", CONT));
    let f = fx("tjc-cont-failed-up", &rows);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "Judge: climb opus/high", "opus", "high", "c1");
}

#[test]
fn the_continue_row_logs_the_continued_pair() {
    use atlas::judge_enforce::{ask, Ask};
    assert_eq!(ask("Judge: continue after ag1."), Ask::Continue(Some("ag1".into())));
    assert_eq!(ask("Judge: continue (context cap)"), Ask::Continue(None));
    let f = fx("tjc-cont-row", &gadget("opus/medium", "sonnet/medium", ", \"explore\": false"));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "Judge: continue after ag1", "opus", "medium", "k1");
    let r = rows(&f.log);
    let c = &r[1];
    assert_eq!(
        (c["continue"].as_bool(), c["continued_pair"].as_str(), c["continued_run"].as_str(), c["launched"].as_str(), c["explore"].as_bool()),
        (Some(true), Some("sonnet/medium"), Some("ag1"), Some("opus/medium"), Some(false))
    );
    // A run that is not the lane's last row is denied.
    let d = denied(&mut h, &f, "Judge: continue after nope", "opus", "medium");
    assert!(d.contains("not the lane's last row"), "{d}");
    // A routed row carries no continued pair.
    let f = fx("tjc-cont-row-plain", "");
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "", "opus", "medium", "p1");
    let r = rows(&f.log);
    assert_eq!((r[0]["continue"].as_bool(), r[0]["continued_pair"].is_null()), (Some(false), true));
}

/// A GADGET row with `brandon_named` true at `launched`.
fn named(launched: &str) -> String {
    format!(
        "{{\"lane\": \"GADGET\", \"pick\": \"opus/medium\", \"launched\": \"{launched}\", \"brandon_named\": true, \"outcome\": null, \"agent_id\": \"ag2\", \"tool_use_id\": \"tu2\"}}\n"
    )
}

const CONT: &str = ", \"explore\": false, \"continue\": true";

#[test]
fn continue_after_explore_keeps_the_explore_for_the_climb() {
    // Pick opus/medium; the explore at sonnet/medium hit the context cap and
    // its continuation failed: the climb goes back to the pick (0304), as it
    // would after the explore itself.
    // (A continuation dropped to the explore's own pair, written by hand.)
    let f = fx("tjc-cont-climb1", &format!("{}{}", gadget("opus/medium", "sonnet/medium", ", \"explore\": true"), gadget("opus/medium", "sonnet/medium", CONT)));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb sonnet/high", "sonnet", "high");
    assert!(d.contains("climbs back to the pick, opus/medium"), "{d}");
    let a = admitted(&mut h, &f, "Judge: climb opus/medium", "opus", "medium", "c1");
    assert!(a.context.contains("climb launch at opus/medium"), "{}", a.context);
    // A continue of a continue keeps it too.
    let two = format!("{}{}{}", gadget("opus/medium", "sonnet/medium", ", \"explore\": true"), gadget("opus/medium", "sonnet/medium", CONT), gadget("opus/medium", "sonnet/medium", CONT));
    let f = fx("tjc-cont-climb2", &two);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "Judge: climb opus/medium", "opus", "medium", "c1");
}

#[test]
fn climb_back_is_capped_at_fable_high() {
    // Pick fable/max (Brandon's call); the explore at fable/medium failed:
    // the climb returns to fable/high, the top rung, never to fable/max.
    let mut f = fx("tjc-cap", &gadget("fable/max", "fable/medium", ", \"explore\": true"));
    f.s.orchestrate.judge.classifier.map.base[5] = ("fable".into(), "max".into());
    let mut h = Hooks::open(f.s.clone()).unwrap();
    for (m, e) in [("opus", "high"), ("sonnet", "high"), ("fable", "medium")] {
        let d = denied(&mut h, &f, &format!("Judge: climb {m}/{e}"), m, e);
        assert!(d.contains("climbs back to fable/high") && !d.contains("the pick, fable/max") && !d.contains("runs at fable/max"), "{m}/{e}: {d}");
    }
    let d = denied(&mut h, &f, "Judge: climb fable/max", "fable", "max");
    assert!(d.contains("ask Brandon") && d.contains("runs at fable/high"), "{d}");
    let a = admitted(&mut h, &f, "Judge: climb fable/high", "fable", "high", "c1");
    assert!(a.context.contains("climb launch at fable/high"), "{}", a.context);
    // An explore already at fable/high under fable/max: nothing to climb to.
    let mut f = fx("tjc-cap-top", &gadget("fable/max", "fable/high", ", \"explore\": true"));
    f.s.orchestrate.judge.classifier.map.base[5] = ("fable".into(), "max".into());
    let mut h = Hooks::open(f.s.clone()).unwrap();
    for (m, e) in [("fable", "high"), ("fable", "max")] {
        let d = denied(&mut h, &f, &format!("Judge: climb {m}/{e}"), m, e);
        assert!(d.contains("top rung reached"), "{m}/{e}: {d}");
    }
}

#[test]
fn climb_base_reads_brandon_named_and_continue_rows() {
    // A brandon-named run is a climb base like any run: the climb goes one
    // rung above sonnet/low (off the ladder: the lowest rung above it), not
    // above the routed opus/medium before it.
    let f = fx("tjc-base-named", &format!("{}{}", gadget("opus/medium", "opus/medium", ", \"explore\": false"), named("sonnet/low")));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb opus/high", "opus", "high");
    assert!(d.contains("one rung above the failed run's sonnet/low") && d.contains("exactly sonnet/medium"), "{d}");
    admitted(&mut h, &f, "Judge: climb sonnet/medium", "sonnet", "medium", "c1");
    // Only a brandon-named row: it is the base.
    let f = fx("tjc-base-named-only", &named("opus/high"));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "Judge: climb fable/medium", "fable", "medium", "c1");
    // A continue is re-judged: after a brandon-named run it runs at the pick.
    let f = fx("tjc-cont-named", &format!("{}{}", gadget("opus/medium", "opus/medium", ", \"explore\": false"), named("sonnet/low")));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: continue", "sonnet", "low");
    assert!(d.contains("the pick is opus/medium"), "{d}");
    admitted(&mut h, &f, "Judge: continue", "opus", "medium", "k1");
    // A continue row of a routed run is the base at its pair, one rung up;
    // the denial lists the ladder.
    let f = fx("tjc-base-cont", &format!("{}{}", gadget("opus/medium", "opus/medium", ", \"explore\": false"), gadget("opus/medium", "opus/medium", CONT)));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb fable/medium", "fable", "medium");
    assert!(d.contains("exactly opus/high") && d.contains("haiku/medium < sonnet/medium < sonnet/high < opus/medium"), "{d}");
    admitted(&mut h, &f, "Judge: climb opus/high", "opus", "high", "c1");
    // A routed run below its pick that was not an explore climbs one rung.
    let f = fx("tjc-base-below", &gadget("opus/high", "sonnet/medium", ", \"explore\": false"));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb opus/high", "opus", "high");
    assert!(d.contains("exactly sonnet/high"), "{d}");
    admitted(&mut h, &f, "Judge: climb sonnet/high", "sonnet", "high", "c1");
    // A legacy row: the pair is in its `explore` string.
    let legacy = "{\"lane\": \"GADGET\", \"pick\": \"opus/medium\", \"explore\": \"up: opus/high\", \"outcome\": null}\n";
    let f = fx("tjc-base-legacy", legacy);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "Judge: climb fable/medium", "fable", "medium", "c1");
    // A legacy explore string below the pick is an explore: back to the pick.
    let legacy = "{\"lane\": \"GADGET\", \"pick\": \"opus/medium\", \"launched\": \"sonnet/medium\", \"explore\": \"axis\", \"outcome\": null}\n";
    let f = fx("tjc-base-legacy-axis", legacy);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "Judge: climb opus/medium", "opus", "medium", "c1");
}

#[test]
fn judge_lines_parse_with_trailing_punctuation() {
    use atlas::judge_enforce::{ask, Ask};
    for l in ["Judge: continue, context cap", "Judge: continue: handoff:GADGET:001", "Judge: continue; infra stop", "Judge: continue."] {
        assert_eq!(ask(l), Ask::Continue(None), "{l}");
    }
    assert_eq!(ask("Judge: continued"), Ask::Plain);
    assert_eq!(ask("Judge: explore: sonnet/medium"), Ask::ExploreAt("sonnet".into(), "medium".into()));
    assert_eq!(ask("Judge: climb, opus/high"), Ask::Climb("opus".into(), "high".into(), None));
    assert_eq!(ask("Judge: explore, because"), Ask::Malformed("explore", "because".into()));
}

#[test]
fn a_failed_brandon_named_run_is_the_climb_base() {
    // Routed opus/medium, then Brandon names fable/high and it fails: the
    // climb base is the fable/high run (0304), so no climb is admitted and
    // the denial names the top rung, never opus/high.
    let f = fx("tjc-named-base", &format!("{}{}", gadget("opus/medium", "opus/medium", ", \"explore\": false"), named("fable/high")));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb opus/high", "opus", "high");
    assert!(d.contains("top rung reached") && !d.contains("exactly opus/high"), "{d}");
    // A named run below the top climbs one rung above its own pair.
    let f = fx("tjc-named-low", &format!("{}{}", gadget("opus/medium", "opus/medium", ", \"explore\": false"), named("sonnet/medium")));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: climb opus/high", "opus", "high");
    assert!(d.contains("exactly sonnet/high"), "{d}");
    admitted(&mut h, &f, "Judge: climb sonnet/high", "sonnet", "high", "c1");
    // Its continue rows behave the same: still one rung above Brandon's pair.
    // (A continue row is logged with `brandon_named` false.)
    let rows = format!("{}{}{}", gadget("opus/medium", "opus/medium", ", \"explore\": false"), named("sonnet/medium"), gadget("opus/medium", "sonnet/medium", CONT));
    let f = fx("tjc-named-cont", &rows);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "Judge: climb sonnet/high", "sonnet", "high", "c1");
}

#[test]
fn climb_after_a_named_run_id_is_admitted() {
    let f = fx("tjc-named-after", &format!("{}{}", gadget("opus/medium", "opus/medium", ", \"explore\": false"), named("sonnet/medium")));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    admitted(&mut h, &f, "Judge: climb sonnet/high after ag2", "sonnet", "high", "c1");
    let d = denied(&mut h, &f, "Judge: climb opus/high after ag1", "opus", "high");
    assert!(d.contains("not the lane's last row"), "{d}");
}

#[test]
fn continue_prefixed_word_is_not_continue() {
    use atlas::judge_enforce::{ask, Ask};
    for l in ["Judge: continue-foo", "Judge: continue_x"] {
        assert_ne!(ask(l), Ask::Continue(None), "{l}");
    }
    assert_eq!(ask("Judge: continue - context cap"), Ask::Continue(None));
}

#[test]
fn a_continuation_without_the_classifier_keeps_the_continued_pair() {
    // The head source with no weights: no pick. The continuation is admitted
    // at the continued run's pair (the pre-#435 rule), any other pair denied.
    let mut f = fx("tjc-cont-unavail", &gadget("opus/medium", "sonnet/high", ", \"explore\": false"));
    f.s.orchestrate.judge.classifier.source = ScoreSource::Head;
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = denied(&mut h, &f, "Judge: continue", "opus", "medium");
    assert!(d.contains("classifier unavailable") && d.contains("sonnet/high"), "{d}");
    let a = admitted(&mut h, &f, "Judge: continue", "sonnet", "high", "k1");
    assert!(a.context.contains("classifier unavailable"), "{}", a.context);
    let r = rows(&f.log);
    assert_eq!((r[1]["continue"].as_bool(), r[1]["launched"].as_str()), (Some(true), Some("sonnet/high")));
    assert!(r[1]["classifier_unavailable"].as_str().is_some(), "{:?}", r[1]);
    // A routed launch is still denied with no pick.
    let d = denied(&mut h, &f, "", "sonnet", "high");
    assert!(d.contains("classifier unavailable"), "{d}");
}

#[test]
fn the_after_keyword_is_case_insensitive() {
    use atlas::judge_enforce::{ask, Ask};
    assert_eq!(ask("Judge: continue After ag1"), Ask::Continue(Some("ag1".into())));
    assert_eq!(ask("Judge: continue AFTER ag1."), Ask::Continue(Some("ag1".into())));
    assert_eq!(ask("Judge: climb opus/high After ag1"), Ask::Climb("opus".into(), "high".into(), Some("ag1".into())));
}
