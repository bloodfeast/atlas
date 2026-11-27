//! atlas-dev #424 fix round 2 (TASK-JUDGE 0288): a due `Judge: explore
//! <model>/<effort>` admits any valid pair but the pick, an uncounted
//! `Judge: climb <model>/<effort>`, string explores in the live log seed
//! the counter, and a malformed explore line is denied.

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

/// Enforced, the kind source (a builder scores 5), Brandon's 0280 map
/// (builder <=3 sonnet/medium, 5 opus/medium: the pick is opus/medium), the
/// dogfood log seeded with `rows`.
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

const SPECIFIED: &str = "Acceptance: tests/gadget.rs::spins\nDeliverables: 2\nCaptures: no\nArt: no\nBuild the gadget.\n";

/// The launch prompt for a brief file, with an extra `Judge:` line.
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

fn last_pick(f: &Fx) -> Value {
    let picks = std::fs::read_to_string(f.s.store.parent().unwrap().join("judge-picks.jsonl")).unwrap();
    json::parse(picks.lines().last().unwrap()).unwrap()
}

fn routed(n: usize) -> String {
    (0..n).map(|_| "{\"lane\": \"X\", \"explore\": false, \"brandon_named\": false, \"outcome\": null}\n").collect()
}

#[test]
fn explore_admits_any_valid_pair_when_due() {
    // Off the 0280 map's rungs (sonnet/medium, opus/medium), up or sideways.
    for (i, (m, e)) in [("opus", "high"), ("fable", "high"), ("fable", "low"), ("sonnet", "high")].into_iter().enumerate() {
        let f = fx(&format!("tj2-explore{i}"), &routed(4));
        let mut h = Hooks::open(f.s.clone()).unwrap();
        let p = prompt(f.t.path(), &format!("Judge: explore {m}/{e}"));
        let a = launch(&mut h, &p, m, e, "x1");
        assert!(a.deny.is_none(), "{m}/{e}: {a:?}");
        assert!(a.context.contains("explore launch at"), "{}", a.context);
        let r = rows(&f.log);
        let want = format!("{m}/{e}");
        assert_eq!((r.len(), r[4]["explore"].as_bool(), r[4]["launched"].as_str()), (5, Some(true), Some(want.as_str())));
        // Counted as the explore: the next one is not due.
        let d = launch(&mut h, &p, m, e, "x2").deny.unwrap_or_default();
        assert!(d.contains("explore launch not due"), "{d}");
    }
    // The pick itself is still no explore, and the launch must match.
    let f = fx("tj2-explore-pick", &routed(4));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = launch(&mut h, &prompt(f.t.path(), "Judge: explore opus/medium"), "opus", "medium", "p1").deny.unwrap_or_default();
    assert!(d.contains("is the pick"), "{d}");
    let d = launch(&mut h, &prompt(f.t.path(), "Judge: explore opus/high"), "opus", "medium", "p2").deny.unwrap_or_default();
    assert!(d.contains("names opus/high"), "{d}");
    // Not due: denied.
    let f = fx("tj2-explore-early", &routed(1));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = launch(&mut h, &prompt(f.t.path(), "Judge: explore opus/high"), "opus", "high", "e1").deny.unwrap_or_default();
    assert!(d.contains("explore launch not due"), "{d}");
}

#[test]
fn climb_marker_admits_uncounted() {
    // Not due (one routed row, the lane's failed run at opus/medium): a
    // climb is admitted at the rung above it.
    let f = fx("tj2-climb", &gadget(1));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let p = prompt(f.t.path(), "Judge: climb opus/high");
    let a = launch(&mut h, &p, "opus", "high", "c1");
    assert!(a.deny.is_none(), "{a:?}");
    assert!(a.context.contains("climb"), "{}", a.context);
    let r = rows(&f.log);
    assert_eq!(
        (r.len(), r[1]["climb"].as_bool(), r[1]["explore"].as_bool(), r[1]["launched"].as_str()),
        (2, Some(true), Some(false), Some("opus/high"))
    );
    // The pick log names the climb.
    let v = last_pick(&f);
    assert_eq!((v["climb"].as_str(), v["explore"].as_str()), (Some("opus/high"), None), "{v}");
    // Not counted, neither by the counter nor by a seed from the log.
    assert_eq!(atlas::judge_enforce::seed(&std::fs::read_to_string(&f.log).unwrap()), 1);
    let c = atlas::judge_enforce::Counter::open(&f.s.store, Some(&f.log));
    assert_eq!(c.since, 1);
    // The launch must match the named pair.
    let p = prompt(f.t.path(), "Judge: climb fable/medium");
    let d = launch(&mut h, &p, "fable", "high", "c2").deny.unwrap_or_default();
    assert!(d.contains("names climb fable/medium but the launch asks fable/high"), "{d}");

    // Due: a climb is still admitted and leaves the explore due.
    let f = fx("tj2-climb-due", &gadget(4));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let a = launch(&mut h, &prompt(f.t.path(), "Judge: climb opus/high"), "opus", "high", "d1");
    assert!(a.deny.is_none(), "{a:?}");
    let d = launch(&mut h, &prompt(f.t.path(), ""), "opus", "medium", "d2").deny.unwrap_or_default();
    assert!(d.contains("the explore launch is due"), "{d}");
    // A climb at the pick is denied: it is not the rung above the last run
    // (opus/high), so it never dodges the explore.
    let d = launch(&mut h, &prompt(f.t.path(), "Judge: climb opus/medium"), "opus", "medium", "d3").deny.unwrap_or_default();
    assert!(d.contains("one rung above") && d.contains("fable/medium"), "{d}");
}

/// `n` routed rows, the last of them the GADGET lane's run at opus/medium.
fn gadget(n: usize) -> String {
    format!(
        "{}{{\"lane\": \"GADGET\", \"launched\": \"opus/medium\", \"explore\": false, \"brandon_named\": false, \"outcome\": null}}\n",
        routed(n - 1)
    )
}

#[test]
fn seed_counts_string_explores() {
    use atlas::judge_enforce::seed;
    let row = |explore: &str| format!("{{\"lane\": \"X\", \"explore\": {explore}, \"brandon_named\": false}}\n");
    for s in ["\"axis\"", "\"up: opus/high (matches classifier head 5 advisory)\"", "\"hypothesis\"", "true"] {
        let log = format!("{}{}{}", routed(3), row(s), routed(2));
        assert_eq!(seed(&log), 2, "{s}");
    }
    // Not an explore: false, null, an empty string, missing.
    for s in ["false", "null", "\"\"", "\"  \""] {
        let log = format!("{}{}", routed(3), row(s));
        assert_eq!(seed(&log), 4, "{s}");
    }
    // A climb row is not counted.
    let log = format!("{}{{\"lane\": \"X\", \"explore\": false, \"climb\": true}}\n", routed(2));
    assert_eq!(seed(&log), 2);
}

#[test]
fn malformed_explore_is_denied_not_bare() {
    // Due, pick opus/medium: the bare marker would admit sonnet/medium.
    for line in ["Judge: explore opus, high", "Judge: explore opus-high", "Judge: explore gpt/high", "Judge: climb opus, high"] {
        let f = fx("tj2-malformed", &routed(4));
        let mut h = Hooks::open(f.s.clone()).unwrap();
        let d = launch(&mut h, &prompt(f.t.path(), line), "sonnet", "medium", "m1").deny.unwrap_or_default();
        let marker = if line.contains("climb") { "climb" } else { "explore" };
        assert!(
            d.contains(&format!("`Judge: {marker} <model>/<effort>`")) && d.contains("opus, high") == line.contains("opus, high"),
            "{line}: {d}"
        );
        assert!(rows(&f.log).len() == 4, "{line}: no row");
    }
    // The bare marker and a trailing full stop are still the bare marker.
    use atlas::judge_enforce::{ask, Ask};
    assert_eq!(ask("Judge: explore"), Ask::Explore);
    assert_eq!(ask("Judge: explore."), Ask::Explore);
}

#[test]
fn judge_task_reads_brief_only_when_something_scores_it() {
    // S5 (tools.rs judge_task): a prompt naming a missing brief is judged by
    // rule A when neither the classifier nor the speculative rule reads it.
    let f = fx("tj2-s5", "");
    let gone = f.t.path().join("BRIEF_GONE.md").display().to_string().replace('\\', "/");
    let p = format!("Kind: builder\nLane: GADGET\nYour brief is {gone}; read it.");
    let vaults = common::Fx::new("tj2-s5-srv");
    let serve = |c: bool, spec: bool| {
        let mut st = f.s.clone();
        st.orchestrate.judge.classifier.enabled = c;
        st.orchestrate.judge.speculative = spec.then(|| ("fable".to_string(), "high".to_string()));
        let mut s = vaults.server();
        s.set_memory(st);
        s
    };
    let j = call(&mut serve(false, false), "judge_task", json!({"prompt": p.as_str(), "format": "json"}));
    assert!(j["line"].as_str().is_some_and(|l| l.contains("rule A")), "{j}");
    for (c, spec) in [(true, false), (false, true)] {
        let e = call_err(&mut serve(c, spec), "judge_task", json!({"prompt": p.as_str(), "format": "json"}));
        assert!(e.contains("cannot read the named"), "classifier {c} speculative {spec}: {e}");
    }
}

#[test]
fn unavailable_line_only_for_launches_the_classifier_should_score() {
    // S4 (hooks.rs classify_launch), advisory, head source with no cache: a
    // launch that is not a builder's and names no brief gets no line; one
    // that names a brief, or a builder's, is told why it was not scored.
    let mut f = fx("tj2-s4", "");
    f.s.orchestrate.judge.mode = JudgeMode::Advisory;
    f.s.orchestrate.judge.classifier.source = ScoreSource::Head;
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let go = |h: &mut Hooks, prompt: &str, ty: &str, id: &str| {
        let ev = HookEvent {
            event: "PreToolUse".into(),
            session_id: "s1".into(),
            tool_name: "Agent".into(),
            tool_prompt: prompt.into(),
            tool_input: json!({"prompt": prompt, "description": "gadget", "model": "sonnet", "effort": "low", "subagent_type": ty}),
            tool_use_id: id.into(),
            ..HookEvent::default()
        };
        h.handle(&ev)
    };
    let a = go(&mut h, "Fetch https://example.com and summarise it.", "web-fetch", "f1");
    assert!(a.deny.is_none() && !a.context.contains("classifier unavailable"), "{a:?}");
    let named = prompt(f.t.path(), "").replace("Kind: builder", "Kind: research");
    let a = go(&mut h, &named, "research", "f2");
    assert!(a.context.contains("atlas judge: classifier unavailable: "), "{a:?}");
}
