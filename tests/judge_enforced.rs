//! #412: the enforced dogfood mode of the task judge at builder launches.

mod common;

use std::path::{Path, PathBuf};

use atlas::hooks::{HookAnswer, HookEvent, Hooks};
use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
use atlas::taskjudge::{ClassifierSettings, JudgeMode, ScoreSource};
use common::*;

struct Fx {
    _t: Temp,
    s: Settings,
    log: PathBuf,
    prompt: String,
}

/// Enforced, kind source (a builder scores 5: the pick is opus/high, the
/// rung below opus/medium), the log in the temp folder seeded with `rows`.
fn fx(tag: &str, mode: JudgeMode, rows: &str) -> Fx {
    let t = Temp::new(tag);
    let mut s = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.compact.claude = atlas::autocompact::ClaudeCompact { window: 1_000_000, ..Default::default() };
    let log = t.path().join("dogfood").join("log.jsonl");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, rows).unwrap();
    s.orchestrate.judge.mode = mode;
    s.orchestrate.judge.log = Some(log.clone());
    s.orchestrate.judge.classifier = ClassifierSettings { enabled: true, source: ScoreSource::Kind, ..Default::default() };
    let brief = t.path().join("BRIEF_GADGET.md");
    std::fs::write(&brief, "Kind: builder\nBuild the gadget.\n").unwrap();
    let prompt = format!("Kind: builder\nLane: GADGET\nRead {} in full.", brief.display()).replace('\\', "/");
    Fx { _t: t, s, log, prompt }
}

fn launch(h: &mut Hooks, prompt: &str, model: &str, effort: Option<&str>, id: &str) -> HookAnswer {
    let mut input = json!({"prompt": prompt, "description": "gadget", "model": model, "subagent_type": "builder"});
    if let (Some(e), Some(o)) = (effort, input.as_object_mut()) {
        o.insert("effort".into(), json!(e));
    }
    let ev = HookEvent { event: "PreToolUse".into(), session_id: "s1".into(), tool_name: "Agent".into(), tool_prompt: prompt.into(), tool_input: input, tool_use_id: id.into(), ..HookEvent::default() };
    h.handle(&ev)
}

fn rows(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()).map(|l| json::parse(l).unwrap()).collect()
}

fn routed(n: usize) -> String {
    (0..n).map(|_| "{\"lane\": \"X\", \"explore\": false, \"brandon_named\": false, \"outcome\": null}\n").collect()
}

#[test]
fn a_mismatch_and_a_missing_effort_are_denied_with_the_pick_named_and_write_no_row() {
    let f = fx("judge-enf-deny", JudgeMode::Enforced, "");
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let a = launch(&mut h, &f.prompt, "opus", Some("medium"), "t1");
    let deny = a.deny.expect("a mismatch is denied");
    assert!(deny.contains("the pick is opus/high") && deny.contains("effort \"high\""), "{deny}");
    let a = launch(&mut h, &f.prompt, "opus", None, "t2");
    let deny = a.deny.expect("a missing effort is denied");
    assert!(deny.contains("a launch must pass effort") && deny.contains("opus/high"), "{deny}");
    assert!(rows(&f.log).is_empty(), "a denied launch writes no row");
    // At the pick: allowed, exactly one row, in the log's format.
    let a = launch(&mut h, &f.prompt, "claude-opus-5-5", Some("high"), "t3");
    assert!(a.deny.is_none(), "{a:?}");
    assert!(a.context.contains("atlas judge (enforced): launch matches the pick"), "{}", a.context);
    let r = rows(&f.log);
    assert_eq!(r.len(), 1);
    let r = &r[0];
    assert_eq!((r["lane"].as_str(), r["pick"].as_str(), r["launched"].as_str(), r["source"].as_str()), (Some("GADGET"), Some("opus/high"), Some("opus/high"), Some("kind")));
    assert_eq!((r["score"].as_u64(), r["explore"].as_bool(), r["brandon_named"].as_bool()), (Some(5), Some(false), Some(false)));
    assert!(r["outcome"].is_null() && r["agent_id"].is_null() && r["ts"].as_str().is_some() && r["brief"].as_str().unwrap().ends_with("BRIEF_GADGET.md"));
    // The agent's start fills the agent id.
    assert!(atlas::judge_enforce::fill_agent(&f.log, "t3", "agent9").unwrap());
    assert_eq!(rows(&f.log)[0]["agent_id"].as_str(), Some("agent9"));
}

#[test]
fn a_brandon_named_launch_is_allowed_logged_and_not_counted() {
    let f = fx("judge-enf-brandon", JudgeMode::Enforced, &routed(3));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let p = format!("{}\nJudge: brandon-named fable/max", f.prompt);
    let a = launch(&mut h, &p, "fable", Some("max"), "b1");
    assert!(a.deny.is_none(), "{a:?}");
    // Off its own pair: denied.
    assert!(launch(&mut h, &p, "opus", Some("max"), "b2").deny.unwrap().contains("brandon-named"));
    let r = rows(&f.log);
    assert_eq!((r.len(), r[3]["brandon_named"].as_bool(), r[3]["launched"].as_str()), (4, Some(true), Some("fable/max")));
    // Not counted: three routed, so the 4th routed is still a plain launch,
    // and the 5th is the explore launch.
    assert!(launch(&mut h, &f.prompt, "opus", Some("high"), "b3").deny.is_none());
    let deny = launch(&mut h, &f.prompt, "opus", Some("high"), "b4").deny.unwrap();
    assert!(deny.contains("the explore launch is due: launch at opus/medium with a `Judge: explore` line"), "{deny}");
}

#[test]
fn explore_is_allowed_only_when_due_and_only_one_rung_below() {
    let f = fx("judge-enf-explore", JudgeMode::Enforced, &routed(4));
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let ex = format!("{}\nJudge: explore", f.prompt);
    // Due: the pick itself and a wrong rung are denied.
    assert!(launch(&mut h, &f.prompt, "opus", Some("high"), "e1").deny.unwrap().contains("explore launch is due"));
    let wrong = launch(&mut h, &ex, "sonnet", Some("medium"), "e2").deny.unwrap();
    assert!(wrong.contains("runs at exactly opus/medium"), "{wrong}");
    let a = launch(&mut h, &ex, "opus", Some("medium"), "e3");
    assert!(a.deny.is_none(), "{a:?}");
    let r = rows(&f.log);
    assert_eq!((r.len(), r[4]["explore"].as_bool(), r[4]["launched"].as_str()), (5, Some(true), Some("opus/medium")));
    // Not due after it.
    let not = launch(&mut h, &ex, "opus", Some("medium"), "e4").deny.unwrap();
    assert!(not.contains("explore launch not due (4 routed launches until it)"), "{not}");
}

#[test]
fn the_counter_survives_a_restart_and_is_seeded_from_the_log() {
    // The live log's shape: explores at rows 5 and 10, the one due at 15
    // missed, so the counter says due from row 15 on until row 29 takes it.
    let mut log = String::new();
    for i in 1..=30 {
        let explore = i == 5 || i == 10 || i == 29;
        log.push_str(&format!("{{\"lane\": \"X\", \"explore\": {explore}, \"brandon_named\": false, \"outcome\": null}}\n"));
        if i == 28 {
            assert_eq!(atlas::judge_enforce::seed(&log), 18);
            assert!(atlas::judge_enforce::due(18), "row 29 is the explore launch");
        }
    }
    assert_eq!(atlas::judge_enforce::seed(&log), 1);
    let f = fx("judge-enf-restart", JudgeMode::Enforced, &routed(2));
    for id in ["r1", "r2"] {
        let mut h = Hooks::open(f.s.clone()).unwrap();
        assert!(launch(&mut h, &f.prompt, "opus", Some("high"), id).deny.is_none());
    }
    // The counter file, not the log, holds the count now: emptying the log
    // changes nothing.
    std::fs::write(&f.log, "").unwrap();
    let mut h = Hooks::open(f.s.clone()).unwrap();
    assert!(launch(&mut h, &f.prompt, "opus", Some("high"), "r3").deny.unwrap().contains("explore launch is due"));
}

#[test]
fn advisory_mode_is_unchanged_and_writes_no_row() {
    let f = fx("judge-enf-advisory", JudgeMode::Advisory, "");
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let a = launch(&mut h, &f.prompt, "sonnet", None, "a1");
    assert!(a.deny.is_none() && a.updated_input.is_none(), "{a:?}");
    assert!(a.context.contains("Classifier: demand score 5 (kind) -> opus, high") && !a.context.contains("enforced"), "{}", a.context);
    assert!(rows(&f.log).is_empty());
    // A launch that is not a builder's is never judged in enforced mode.
    let f = fx("judge-enf-reviewer", JudgeMode::Enforced, "");
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let p = f.prompt.replace("Kind: builder", "Kind: reviewer");
    let ev = HookEvent { event: "PreToolUse".into(), session_id: "s1".into(), tool_name: "Agent".into(), tool_prompt: p.clone(), tool_input: json!({"prompt": p.as_str(), "model": "haiku"}), tool_use_id: "v1".into(), ..HookEvent::default() };
    assert!(h.handle(&ev).deny.is_none());
    assert!(rows(&f.log).is_empty());
}

#[test]
fn the_hook_and_note_judge_share_one_head_score_through_the_cache() {
    let t = Temp::new("judge-enf-cache");
    let weights = t.path().join("weights.f32");
    // w all zero, the kind one-hot weight of builder 1.0, thresholds
    // 0.5 and up: x . w = 1.0 scores 1.
    let mut w = vec![0f32; atlas::taskjudge::CLASSIFIER_DIM + 4];
    w[atlas::taskjudge::CLASSIFIER_DIM + 1] = 1.0;
    w.extend([0.5f32, 1.5, 2.5, 3.5, 4.5]);
    std::fs::write(&weights, w.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
    let brief = t.path().join("BRIEF_X.md");
    std::fs::write(&brief, "Kind: builder\nx\n").unwrap();
    let prompt = format!("Read {}.", brief.display()).replace('\\', "/");
    let s = ClassifierSettings { enabled: true, source: ScoreSource::Head, weights: Some(weights), ..Default::default() };
    let cache = t.path().join("judge-scores.jsonl");
    let mut calls = 0;
    let first = atlas::taskjudge::classify_with(&s, &prompt, "builder", Some(&cache), &mut |_| {
        calls += 1;
        Some(vec![0f32; atlas::taskjudge::CLASSIFIER_DIM])
    });
    assert_eq!((first.as_ref().map(|p| (p.score, p.source)), calls), (Some((1, "head")), 1));
    // The second scorer (the launch hook) gets the same pick with no embed.
    let second = atlas::taskjudge::classify_with(&s, &prompt, "builder", Some(&cache), &mut |_| panic!("cache miss"));
    assert_eq!(first, second);
}

#[test]
fn owed_rows_are_those_with_a_null_outcome() {
    let log = "{\"lane\": \"A\", \"outcome\": {\"gates\": \"pass\"}}\n{\"lane\": \"B\", \"outcome\": null}\n";
    assert_eq!(atlas::judge_enforce::owed(log), ["dogfood row 2 B (outcome owed)"]);
}
