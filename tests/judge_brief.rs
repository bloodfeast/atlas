//! TASK-JUDGE fix: the classifier never fails silently, the judge scores the
//! named brief file, the speculative rule, and the brief rule at launches.

mod common;

use std::path::{Path, PathBuf};

use atlas::hooks::{HookAnswer, HookEvent, Hooks};
use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
use atlas::taskjudge::{ClassifierSettings, Header, JudgeMode, ScoreSource, CLASSIFIER_DIM};
use common::*;

/// Head weights: all zero but builder's kind weight 1.0, thresholds 0.5 up,
/// so any vector scores 1 for a builder.
fn weights(dir: &Path) -> PathBuf {
    let mut w = vec![0f32; CLASSIFIER_DIM + 4];
    w[CLASSIFIER_DIM + 1] = 1.0;
    w.extend([0.5f32, 1.5, 2.5, 3.5, 4.5]);
    let p = dir.join("weights.f32");
    std::fs::write(&p, w.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
    p
}

/// The score cache row `note op=judge` would write for `text`.
fn seed_cache(store: &Path, weights: &Path, kind: &str, text: &str, score: usize) {
    let mut b = std::fs::read(weights).unwrap();
    b.push(0);
    b.extend_from_slice(kind.as_bytes());
    b.push(0);
    b.extend_from_slice(text.as_bytes());
    let key = atlas::sha256::hex(&b);
    let cache = atlas::taskjudge::score_cache(store).unwrap();
    atlas::taskjudge::append_line(&cache, &format!("{{\"key\":\"{key}\",\"score\":{score}}}")).unwrap();
}

struct Fx {
    vaults: common::Fx,
    t: Temp,
    s: Settings,
    weights: PathBuf,
}

/// Memory settings with the judge on (a one-cell cells file), the head
/// classifier on with test weights, and the tiny test encoder (the wrong
/// encoder for the head).
fn fx(tag: &str) -> Fx {
    let t = Temp::new(tag);
    let mut s = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.compact.claude = atlas::autocompact::ClaudeCompact { window: 1_000_000, ..Default::default() };
    let cells = t.path().join("cells.json");
    std::fs::write(&cells, r#"{"cells": [{"key": {"kind": null, "has_brief": null}, "scope": "medium", "unk": "few"}]}"#).unwrap();
    let model = t.path().join("tiny.atm");
    std::fs::write(&model, atlas::embed::tiny_model_bytes(true, 7)).unwrap();
    s.model = model;
    let weights = weights(t.path());
    s.orchestrate.judge.enabled = true;
    s.orchestrate.judge.cells = Some(cells);
    s.orchestrate.judge.classifier =
        ClassifierSettings { enabled: true, source: ScoreSource::Head, weights: Some(weights.clone()), ..Default::default() };
    Fx { vaults: common::Fx::new(&format!("{tag}-srv")), t, s, weights }
}

fn brief(dir: &Path, name: &str, text: &str) -> (PathBuf, String) {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    let prompt = format!("Kind: builder\nLane: GADGET\nYour brief is {}; read it.", p.display()).replace('\\', "/");
    (p, prompt)
}

fn server(f: &Fx) -> atlas::tools::Server {
    let mut s = f.vaults.server();
    s.set_memory(f.s.clone());
    s
}

const SPECIFIED: &str =
    "Kind: builder\nAcceptance: tests/gadget.rs::spins fails before the fix\nDeliverables: 2\nCaptures: no\nArt: no\nBuild the gadget.\n";

#[test]
fn a_wrong_encoder_is_named_in_the_text_and_json_answers_never_dropped() {
    let f = fx("tj-wrong-encoder");
    let (_, prompt) = brief(f.t.path(), "BRIEF_GADGET.md", SPECIFIED);
    let mut s = server(&f);
    let j = call(&mut s, "judge_task", json!({"prompt": prompt.as_str(), "format": "json"}));
    let why = j["classifier"]["unavailable"].as_str().unwrap_or_else(|| panic!("no classifier key: {j}"));
    assert!(why.contains("the head needs 746bdb58bc259433-int8 (dim 384)") && why.contains("(dim 16)"), "{why}");
    assert!(j["line"].as_str().unwrap().starts_with("Judge: classifier unavailable: "), "{j}");
    assert_eq!(j["source"], "rule A (fallback: classifier unavailable)");
    let text = call(&mut s, "judge_task", json!({"prompt": prompt.as_str()}));
    let shown = text.as_str().map(str::to_string).unwrap_or_else(|| text.to_string());
    assert!(shown.contains("classifier unavailable: the encoder is"), "{shown}");
}

#[test]
fn the_classifier_pick_is_primary_scored_from_the_brief_file() {
    let f = fx("tj-primary");
    let (_, prompt) = brief(f.t.path(), "BRIEF_GADGET.md", SPECIFIED);
    // The cache holds a score for the brief FILE's text only: a judge that
    // scored the pointer prompt would miss it and fail on the tiny encoder.
    seed_cache(&f.s.store, &f.weights, "builder", SPECIFIED, 1);
    let mut s = server(&f);
    let j = call(&mut s, "judge_task", json!({"prompt": prompt.as_str(), "format": "json"}));
    assert_eq!(j["score"], 1, "{j}");
    assert_eq!(j["source"], "classifier (head)");
    assert!(j["line"].as_str().unwrap().starts_with("Judge: demand score 1 (head)"), "{j}");
    assert_eq!((j["model"].as_str(), j["effort"].as_str()), (Some("opus"), Some("medium")));
    assert_eq!(j["fallback"]["source"], "rule A");
    assert_eq!(j["classifier"]["score"], 1);
    assert!(j["brief"].as_str().unwrap().ends_with("BRIEF_GADGET.md"));
}

#[test]
fn a_named_brief_that_is_missing_is_an_error_not_a_fallback() {
    let f = fx("tj-missing");
    let gone = f.t.path().join("BRIEF_GONE.md");
    let prompt = format!("Kind: builder\nYour brief is {}; read it.", gone.display()).replace('\\', "/");
    let mut s = server(&f);
    let e = call_err(&mut s, "judge_task", json!({"prompt": prompt.as_str()}));
    assert!(e.contains("BRIEF_GONE.md"), "{e}");
    // A brief it builds on that is missing is an error too.
    let (_, prompt) = brief(f.t.path(), "BRIEF_NEXT.md", &format!("Builds on: {}\nx\n", gone.display()).replace('\\', "/"));
    let e = call_err(&mut s, "judge_task", json!({"prompt": prompt.as_str()}));
    assert!(e.contains("BRIEF_GONE.md") && e.contains("Builds on"), "{e}");
}

#[test]
fn a_brief_it_builds_on_is_scored_with_it() {
    let f = fx("tj-builds-on");
    let (first, _) = brief(f.t.path(), "BRIEF_ONE.md", "Phase one.\n");
    let own = format!("Builds on: {}\nPhase two.\n", first.display()).replace('\\', "/");
    let (_, prompt) = brief(f.t.path(), "BRIEF_TWO.md", &own);
    let b = atlas::taskjudge::read_brief(&prompt).unwrap().unwrap();
    assert_eq!(b.text, format!("{own}\n\nPhase one.\n"));
}

#[test]
fn the_speculative_rule_routes_only_briefs_that_name_no_acceptance_tests() {
    let mut f = fx("tj-speculative");
    f.s.orchestrate.judge.classifier.enabled = false;
    f.s.orchestrate.judge.speculative = Some(("fable".into(), "high".into()));
    let mut s = server(&f);
    let (_, spec) = brief(
        f.t.path(),
        "BRIEF_PERF.md",
        "Acceptance: none named (find frame time to win back at equal fidelity)\nDeliverables: 1\nCaptures: yes\nArt: no\n",
    );
    let j = call(&mut s, "judge_task", json!({"prompt": spec.as_str(), "format": "json"}));
    assert_eq!((j["model"].as_str(), j["effort"].as_str(), j["speculative"].as_bool()), (Some("fable"), Some("high"), Some(true)), "{j}");
    let (_, plain) = brief(f.t.path(), "BRIEF_PLAIN.md", SPECIFIED);
    let j = call(&mut s, "judge_task", json!({"prompt": plain.as_str(), "format": "json"}));
    assert_eq!((j["model"].as_str(), j.get("speculative")), (Some("opus"), None), "{j}");
    // Off (the default): the header changes nothing.
    f.s.orchestrate.judge.speculative = None;
    let mut s = server(&f);
    let j = call(&mut s, "judge_task", json!({"prompt": spec.as_str(), "format": "json"}));
    assert_eq!(j["model"], "opus");
}

#[test]
fn the_header_parses_and_names_what_is_missing() {
    let h = Header::parse(
        "# T\n- **Acceptance:** none named\nDeliverables: 3.\ncaptures: Yes\nArt: no\nBuilds on: Z:/b/BRIEF_A.md, Z:/b/BRIEF_B.md\n",
    );
    assert_eq!(h.acceptance.as_deref(), Some("none named"));
    assert_eq!((h.deliverables, h.captures, h.art), (Some(3), Some(true), Some(false)));
    assert_eq!(h.builds_on, ["Z:/b/BRIEF_A.md", "Z:/b/BRIEF_B.md"]);
    assert!(h.speculative() && h.missing().is_empty());
    let h = Header::parse("Acceptance: tests/x.rs::y\nCaptures: maybe\n");
    assert!(!h.speculative());
    assert_eq!(h.missing(), ["Deliverables", "Captures", "Art"]);
}

fn launch(h: &mut Hooks, prompt: &str, model: &str) -> HookAnswer {
    let input = json!({"prompt": prompt, "description": "gadget", "model": model, "subagent_type": "builder"});
    let ev = HookEvent {
        event: "PreToolUse".into(),
        session_id: "s1".into(),
        tool_name: "Agent".into(),
        tool_prompt: prompt.into(),
        tool_input: input,
        tool_use_id: "t1".into(),
        ..HookEvent::default()
    };
    h.handle(&ev)
}

#[test]
fn the_launch_hook_holds_builders_to_the_brief_rule() {
    let mut f = fx("tj-hook-rule");
    // A pointer to a missing file is denied, Brandon-named or not.
    let gone = f.t.path().join("BRIEF_GONE.md").display().to_string().replace('\\', "/");
    for p in [
        format!("Kind: builder\nYour brief is {gone}; read it."),
        format!("Kind: builder\nJudge: brandon-named fable/high\nYour brief is {gone}."),
    ] {
        let mut h = Hooks::open(f.s.clone()).unwrap();
        let d = launch(&mut h, &p, "opus").deny.unwrap_or_default();
        assert!(d.contains("BRIEF_GONE.md") && d.contains("Write the brief file first"), "{d}");
    }
    // No brief named: denied, with how to fix it.
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = launch(&mut h, "Kind: builder\nBuild the gadget.", "opus").deny.unwrap_or_default();
    assert!(d.contains("must name its brief file") && d.contains("BRIEF_"), "{d}");
    // A brief without the header: allowed until require_header is on, then denied.
    let (_, bare) = brief(f.t.path(), "BRIEF_BARE.md", "Kind: builder\nBuild it.\n");
    let mut h = Hooks::open(f.s.clone()).unwrap();
    assert!(launch(&mut h, &bare, "opus").deny.is_none());
    f.s.orchestrate.judge.require_header = true;
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = launch(&mut h, &bare, "opus").deny.unwrap_or_default();
    assert!(
        d.contains("lacks the judge header fields Acceptance, Deliverables, Captures, Art") && d.contains("Acceptance: <the named tests"),
        "{d}"
    );
    // A compliant brief passes and is scored from the file (the cache holds
    // the file text's score; the hook never embeds).
    let (_, good) = brief(f.t.path(), "BRIEF_GOOD.md", SPECIFIED);
    seed_cache(&f.s.store, &f.weights, "builder", SPECIFIED, 1);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let a = launch(&mut h, &good, "opus");
    assert!(a.deny.is_none(), "{a:?}");
    assert!(a.context.contains("Classifier: demand score 1 (head)"), "{}", a.context);
    // Off when the classifier is off and require_brief is unset.
    f.s.orchestrate.judge.classifier.enabled = false;
    f.s.orchestrate.judge.require_header = false;
    let mut h = Hooks::open(f.s.clone()).unwrap();
    assert!(launch(&mut h, "Kind: builder\nBuild the gadget.", "opus").deny.is_none());
}

#[test]
fn an_uncached_brief_at_the_hook_says_why_it_was_not_scored() {
    let f = fx("tj-hook-uncached");
    let (_, good) = brief(f.t.path(), "BRIEF_GOOD.md", SPECIFIED);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let a = launch(&mut h, &good, "opus");
    assert!(a.deny.is_none() && a.context.contains("classifier unavailable: this brief is not in the score cache"), "{}", a.context);
    // The pick log row records the reason, never a bare rule A pick.
    let row = std::fs::read_to_string(f.s.store.parent().unwrap().join("judge-picks.jsonl")).unwrap();
    let v: Value = json::parse(row.lines().last().unwrap()).unwrap();
    assert!(v["classifier_unavailable"].as_str().is_some_and(|w| w.contains("score cache")), "{row}");
}

#[test]
fn enforced_mode_denies_with_the_reason_the_classifier_could_not_score() {
    let mut f = fx("tj-enforced-why");
    f.s.orchestrate.judge.mode = JudgeMode::Enforced;
    let (_, good) = brief(f.t.path(), "BRIEF_GOOD.md", SPECIFIED);
    let mut h = Hooks::open(f.s.clone()).unwrap();
    let d = launch(&mut h, &good, "opus").deny.unwrap_or_default();
    assert!(d.contains("classifier unavailable:"), "{d}");
}

/// The live encoder and head (set ATLAS_JUDGE_ENCODER and
/// ATLAS_JUDGE_WEIGHTS to their files): the configured pair scores a brief.
/// Skips when unset.
#[test]
fn the_configured_encoder_and_weights_score_a_brief() {
    let (Ok(enc), Ok(w)) = (std::env::var("ATLAS_JUDGE_ENCODER"), std::env::var("ATLAS_JUDGE_WEIGHTS")) else {
        eprintln!("skipped: ATLAS_JUDGE_ENCODER / ATLAS_JUDGE_WEIGHTS unset");
        return;
    };
    let t = Temp::new("tj-live");
    let (_, prompt) = brief(t.path(), "BRIEF_GADGET.md", SPECIFIED);
    let s = ClassifierSettings { enabled: true, source: ScoreSource::Head, weights: Some(w.into()), ..Default::default() };
    let mut sc = atlas::embed::bert::Scratch::default();
    let p = atlas::taskjudge::classify_with(&s, &prompt, "builder", None, &mut |text| {
        atlas::taskjudge::embed_file(Some(Path::new(&enc)), text, &mut sc)
    });
    let p = p.unwrap_or_else(|e| panic!("classifier unavailable: {e}")).unwrap();
    assert!(p.score < 6 && p.source == "head", "{p:?}");
}
