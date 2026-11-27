//! atlas-dev #424 fix round 1 (TASK-JUDGE 0281): the enforced judge applies
//! the speculative rule, the brief rule holds only real builders, a named
//! explore rung, the advisory note against the primary pick, a pasteable
//! `Judge:` line, and a Brandon-named launch that keeps the classifier's
//! reason.

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

/// Enforced (with `mode`), the kind source (a builder scores 5), the
/// dogfood log seeded with `rows`, the judge's cells file set.
fn fx(tag: &str, mode: JudgeMode, rows: &str) -> Fx {
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
    s.orchestrate.judge.mode = mode;
    s.orchestrate.judge.log = Some(log.clone());
    s.orchestrate.judge.classifier = ClassifierSettings { enabled: true, source: ScoreSource::Kind, ..Default::default() };
    Fx { t, s, log }
}

/// Brandon's 0280 map: builder scores <=3 sonnet/medium, 4-5 opus/medium.
fn map_0280(s: &mut Settings, max_score: usize) {
    let m = &mut s.orchestrate.judge.classifier.map;
    m.builder = Some(Override { enabled: true, max_score, pick: ("sonnet".into(), "medium".into()) });
    m.base[5] = ("opus".into(), "medium".into());
}

/// A brief file and the launch prompt that names it.
fn brief(dir: &Path, name: &str, text: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    format!("Kind: builder\nLane: GADGET\nYour brief is {}; read it.", p.display()).replace('\\', "/")
}

fn launch_as(h: &mut Hooks, prompt: &str, ty: &str, isolation: &str, model: &str, effort: &str, id: &str) -> HookAnswer {
    let mut input = json!({"prompt": prompt, "description": "gadget", "model": model, "effort": effort});
    if let Some(o) = input.as_object_mut() {
        if !ty.is_empty() {
            o.insert("subagent_type".into(), json!(ty));
        }
        if !isolation.is_empty() {
            o.insert("isolation".into(), json!(isolation));
        }
    }
    let ev = HookEvent {
        event: "PreToolUse".into(),
        session_id: "s1".into(),
        tool_name: "Agent".into(),
        tool_prompt: prompt.into(),
        tool_input: input,
        tool_use_id: id.into(),
        ..HookEvent::default()
    };
    h.handle(&ev)
}

fn launch(h: &mut Hooks, prompt: &str, model: &str, effort: &str, id: &str) -> HookAnswer {
    launch_as(h, prompt, "builder", "", model, effort, id)
}

fn rows(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()).map(|l| json::parse(l).unwrap()).collect()
}

fn routed(n: usize) -> String {
    (0..n).map(|_| "{\"lane\": \"X\", \"explore\": false, \"brandon_named\": false, \"outcome\": null}\n").collect()
}

const SPECULATIVE: &str = "Acceptance: none named (find frame time to win back)\nDeliverables: 1\nCaptures: yes\nArt: no\n";
const SPECIFIED: &str = "Acceptance: tests/gadget.rs::spins\nDeliverables: 2\nCaptures: no\nArt: no\nBuild the gadget.\n";

#[test]
fn enforce_admits_speculative_pair() {
    // Four routed rows: the explore launch is due on the next routed one.
    let mut f = fx("tj1-spec", JudgeMode::Enforced, &routed(4));
    f.s.orchestrate.judge.speculative = Some(("fable".into(), "high".into()));
    f.s.orchestrate.judge.require_header = true;
    let spec = brief(f.t.path(), "BRIEF_PERF.md", SPECULATIVE);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    // The classifier's pick is denied: the speculative pair wins.
    let d = launch(&mut h, &spec, "opus", "high", "s1").deny.unwrap_or_default();
    assert!(d.contains("fable/high") && d.contains("speculative"), "{d}");
    // At the speculative pair: admitted, logged speculative, no explore.
    let a = launch(&mut h, &spec, "fable", "high", "s2");
    assert!(a.deny.is_none(), "{a:?}");
    assert!(a.context.contains("speculative"), "{}", a.context);
    let r = rows(&f.log);
    assert_eq!(r.len(), 5);
    assert_eq!(
        (r[4]["speculative"].as_bool(), r[4]["explore"].as_bool(), r[4]["launched"].as_str()),
        (Some(true), Some(false), Some("fable/high"))
    );
    // Not counted: the explore launch is still due for the next routed one.
    let plain = brief(f.t.path(), "BRIEF_PLAIN.md", SPECIFIED);
    let d = launch(&mut h, &plain, "opus", "high", "s3").deny.unwrap_or_default();
    assert!(d.contains("the explore launch is due"), "{d}");
}

#[test]
fn brief_rule_skips_launches_without_kind_line() {
    let mut f = fx("tj1-kind", JudgeMode::Enforced, "");
    f.s.orchestrate.judge.require_header = true;
    let mut h = Hooks::open(f.s.clone()).unwrap();
    // No `Kind:` line, no worktree, no builder type: the default kind is
    // not enough to hold a launch to the brief rule or the pick.
    for (i, ty) in ["fork", "web-fetch", "Explore", "general-purpose", ""].into_iter().enumerate() {
        let a = launch_as(&mut h, "Fetch https://example.com and summarise it.", ty, "", "sonnet", "low", &format!("k{i}"));
        assert!(a.deny.is_none(), "{ty}: {a:?}");
    }
    // A worktree launch with no Kind line is a builder: denied, and the
    // deny says why it counts as one and how to re-kind it.
    let d = launch_as(&mut h, "Fix the gadget.", "general-purpose", "worktree", "opus", "high", "k9").deny.unwrap_or_default();
    assert!(d.contains("must name its brief file") && d.contains("isolation: \"worktree\" counts as a builder"), "{d}");
    // A `Kind: builder` line is held to the rule, the reason named.
    let d = launch_as(&mut h, "Kind: builder\nFix the gadget.", "fork", "", "opus", "high", "k10").deny.unwrap_or_default();
    assert!(d.contains("must name its brief file") && d.contains("`Kind:` line says so"), "{d}");
}

#[test]
fn explore_marker_allows_named_rung_when_due() {
    // 0280 map, score 5: the pick is opus/medium; rungs sonnet/medium, opus/medium.
    let mut f = fx("tj1-explore", JudgeMode::Enforced, &routed(4));
    map_0280(&mut f.s, 3);
    let p = brief(f.t.path(), "BRIEF_GADGET.md", SPECIFIED);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let at = |r: &str| format!("{p}\nJudge: explore {r}");
    // An off-map pair is admitted too (fix round 2): tests/judge_fix2.rs.
    // The pick itself is no explore.
    let d = launch(&mut h, &at("opus/medium"), "opus", "medium", "x2").deny.unwrap_or_default();
    assert!(d.contains("is the pick"), "{d}");
    // The launch must match the named rung.
    let d = launch(&mut h, &at("sonnet/medium"), "opus", "medium", "x3").deny.unwrap_or_default();
    assert!(d.contains("names sonnet/medium"), "{d}");
    // Down a rung, when due: admitted and logged as the explore.
    let a = launch(&mut h, &at("sonnet/medium"), "sonnet", "medium", "x4");
    assert!(a.deny.is_none(), "{a:?}");
    let r = rows(&f.log);
    assert_eq!((r.len(), r[4]["explore"].as_bool(), r[4]["launched"].as_str()), (5, Some(true), Some("sonnet/medium")));
    let picks = std::fs::read_to_string(f.s.store.parent().unwrap().join("judge-picks.jsonl")).unwrap();
    let v: Value = json::parse(picks.lines().last().unwrap()).unwrap();
    assert_eq!(v["explore"].as_str(), Some("sonnet/medium"), "{picks}");
    // Not due right after it.
    let d = launch(&mut h, &at("sonnet/medium"), "sonnet", "medium", "x5").deny.unwrap_or_default();
    assert!(d.contains("explore launch not due"), "{d}");

    // Up a rung: score 5 under an override to 5 picks sonnet/medium, and
    // the explore runs at opus/medium.
    let mut f = fx("tj1-explore-up", JudgeMode::Enforced, &routed(4));
    map_0280(&mut f.s, 5);
    let p = brief(f.t.path(), "BRIEF_GADGET.md", SPECIFIED);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let a = launch(&mut h, &format!("{p}\nJudge: explore opus/medium"), "opus", "medium", "u1");
    assert!(a.deny.is_none(), "{a:?}");
    assert_eq!(rows(&f.log)[4]["explore"].as_bool(), Some(true));
    // The bare marker keeps its rung-below meaning: none below sonnet/medium.
    let mut f = fx("tj1-explore-bare", JudgeMode::Enforced, &routed(4));
    map_0280(&mut f.s, 5);
    let p = brief(f.t.path(), "BRIEF_GADGET.md", SPECIFIED);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = launch(&mut h, &format!("{p}\nJudge: explore"), "sonnet", "low", "u2").deny.unwrap_or_default();
    assert!(d.contains("lowest rung"), "{d}");
    // The pick log's explore field for each marker.
    use atlas::judge_enforce::ask;
    assert_eq!(ask("Judge: explore").explore().as_deref(), Some("rung-below"));
    assert_eq!(ask("Judge: explore Opus/High.").explore().as_deref(), Some("opus/high"));
    assert_eq!(ask("Judge: explorer").explore(), None);
}

#[test]
fn advisory_note_compares_to_primary_pick() {
    // Advisory, kind source: the classifier's pick is opus/high (score 5);
    // rule A (cells medium/few) says opus/medium.
    let f = fx("tj1-advisory", JudgeMode::Advisory, "");
    let p = brief(f.t.path(), "BRIEF_GADGET.md", SPECIFIED);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let a = launch(&mut h, &format!("{p}\nJudge: demand score 5 (kind) -> opus, high (atlas rule C)."), "opus", "high", "a1");
    assert!(a.deny.is_none() && !a.context.contains("atlas judge (advisory)"), "{}", a.context);
    let a = launch(&mut h, &format!("{p}\nJudge: scope medium, unknowns few -> opus, medium (atlas rule A)."), "opus", "medium", "a2");
    assert!(a.context.contains("atlas judge (advisory)") && a.context.contains("the pick is opus, high"), "{}", a.context);
    // The pick log's model and source are the classifier's; rule A is kept.
    let picks = std::fs::read_to_string(f.s.store.parent().unwrap().join("judge-picks.jsonl")).unwrap();
    let v: Value = json::parse(picks.lines().last().unwrap()).unwrap();
    assert_eq!(
        (v["model"].as_str(), v["effort"].as_str(), v["source"].as_str()),
        (Some("opus"), Some("high"), Some("classifier (kind)")),
        "{picks}"
    );
    assert_eq!(v["rule_a"].as_str(), Some("opus/medium"), "{picks}");
}

#[test]
fn judge_line_is_pasteable() {
    let f = fx("tj1-paste", JudgeMode::Advisory, "");
    let p = brief(f.t.path(), "BRIEF_GADGET.md", SPECIFIED);
    let vaults = common::Fx::new("tj1-paste-srv");
    let mut s = vaults.server();
    s.set_memory(f.s.clone());
    let j = call(&mut s, "judge_task", json!({"prompt": p.as_str(), "format": "json"}));
    let line = j["line"].as_str().unwrap();
    assert!(line.starts_with("Judge: "), "{j}");
    assert_eq!(atlas::taskjudge::stated(line), Some(("opus".into(), "high".into())), "{line}");
    // Unavailable: still a Judge line, at the rule A fallback.
    let mut f2 = f.s.clone();
    f2.orchestrate.judge.classifier.source = ScoreSource::Head;
    let mut s = vaults.server();
    s.set_memory(f2);
    let j = call(&mut s, "judge_task", json!({"prompt": p.as_str(), "format": "json"}));
    let line = j["line"].as_str().unwrap();
    assert!(line.starts_with("Judge: ") && line.contains("classifier unavailable"), "{j}");
    assert_eq!(atlas::taskjudge::stated(line), Some(("opus".into(), "medium".into())), "{line}");
}

#[test]
fn brandon_named_keeps_classifier_reason() {
    // The head source with no weights: the classifier cannot score.
    let mut f = fx("tj1-brandon", JudgeMode::Enforced, "");
    f.s.orchestrate.judge.classifier.source = ScoreSource::Head;
    let p = brief(f.t.path(), "BRIEF_GADGET.md", SPECIFIED);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let a = launch(&mut h, &format!("{p}\nJudge: brandon-named fable/high"), "fable", "high", "n1");
    assert!(a.deny.is_none(), "{a:?}");
    assert!(a.context.contains("classifier unavailable: no weights file"), "{}", a.context);
    let r = rows(&f.log);
    assert_eq!(r[0]["brandon_named"].as_bool(), Some(true));
    assert!(r[0]["classifier_unavailable"].as_str().is_some_and(|w| w.contains("no weights file")), "{:?}", r[0]);
}
