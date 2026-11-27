//! ATLAS-GOAL build step 2 (docs/design/atlas-goal.md section 7): metric
//! lines and `goal::evaluate`, on section 3.1's spec and sitting B.

use super::super::{set, Who};
use super::*;
use crate::memory::{Draft, MemoryStore};

/// Section 3.1's spec (as in goal_tests.rs).
const SPEC: &str = "gate G1 frame.gpu.p99@1080p < 4170 us over r1080,s1080,b1080 take worst quiet feeds FRAME-SET
gate G2 frame.gpu.p99@1440p < 4170 us over r1440,s1440,b1440 take worst quiet feeds FRAME-SET
gate G3 parity.rows.open = 0 rows feeds PARITY-ROWS
gate G4 h2h.ratio.p99 <= 0.667 x take worst quiet feeds H2H
aim  A1 frame.gpu.p99@4k < 4250 us over r4k,s4k,b4k take worst quiet feeds FRAME-SET
aim  A2 frame.gpu.p99@4k-ultra < 6940 us over h4k quiet feeds FRAME-SET
report A1 A2
step 1 PARITY rows done-when parity.rows.open = 0
step 2 Bevy head-to-head done-when h2h.ratio.p99 <= 0.667
step 3 networked 1v1
step 4 campaign vertical slice
lane PARITY-ROWS step 1
lane PARITY-BAR step 1
lane FRAME-SET step 1
lane H2H step 2
cadence every 120m
stall 24h
";

/// Quiet sitting B, 2026-11-12, head 763e4d3a (section 3.1): the worst
/// arms are section 3.1's values; the other arms are below them.
const SITTING_B: &str = "metric: frame.gpu.p99@1080p 3998 us arm r1080 @763e4d3a quiet
metric: frame.gpu.p99@1080p 4060 us arm s1080 @763e4d3a quiet
metric: frame.gpu.p99@1080p 4147 us arm b1080 @763e4d3a quiet
metric: frame.gpu.p99@1440p 5102 us arm r1440 @763e4d3a quiet
metric: frame.gpu.p99@1440p 5240 us arm s1440 @763e4d3a quiet
metric: frame.gpu.p99@1440p 5578 us arm b1440 @763e4d3a quiet
metric: frame.gpu.p99@4k 9810 us arm r4k @763e4d3a quiet
metric: frame.gpu.p99@4k 10120 us arm s4k @763e4d3a quiet
metric: frame.gpu.p99@4k 10493 us arm b4k @763e4d3a quiet
metric: frame.gpu.p99@4k-ultra 19675 us arm h4k @763e4d3a quiet
metric: parity.rows.open 19 rows @763e4d3a quiet
";

fn store_in(name: &str) -> (std::path::PathBuf, MemoryStore) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("atlas-goal-eval-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let s = MemoryStore::open(&d.join("memory.sqlite")).unwrap();
    (d, s)
}

fn who() -> Who<'static> {
    Who { author: "orchestrator", client: "test", reason: "goal eval test" }
}

/// A store with goal:g active on `spec`, in lane GOAL.
fn goal_store(name: &str, spec: &str) -> (std::path::PathBuf, MemoryStore) {
    let (d, mut s) = store_in(name);
    set(&mut s, "g", "GOAL", "Make it fast.", Some(spec), &who()).unwrap();
    (d, s)
}

/// Appends a journal entry of `sub` in `lane` with this body.
fn entry(s: &mut MemoryStore, lane: &str, sub: &str, meta: &str, body: &str) -> String {
    let d = Draft { title: format!("{sub} entry"), meta: meta.into(), body: body.into(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() };
    let mut w = s.writer().unwrap();
    let k = w.next_key(&format!("journal:{lane}:"), 4).unwrap();
    w.create(&k, "journal", sub, lane, &d, None).unwrap();
    w.add_refs(&k, &[("lane".to_string(), lane.to_string())]).unwrap();
    w.commit().unwrap();
    k
}

fn measure(s: &mut MemoryStore, lane: &str, lines: &str) -> String {
    entry(s, lane, MEASUREMENT, "", lines)
}

#[test]
fn sitting_b_gives_g1_met_g2_not() {
    let (_d, mut s) = goal_store("sitting-b", SPEC);
    let src = measure(&mut s, "FRAME-SET", SITTING_B);
    let e = evaluate(&s, "engine-parity").err();
    assert!(e.is_some(), "an unknown goal is NotFound");
    let e = evaluate(&s, "g").unwrap();
    let g1 = e.bar("G1").unwrap();
    assert_eq!(g1.verdict, Verdict::Met);
    let t1 = g1.taken.as_ref().unwrap();
    assert_eq!((t1.value, t1.arm.as_deref(), t1.commit.as_str(), t1.quiet, t1.source.as_str()), (4147.0, Some("b1080"), "763e4d3a", true, src.as_str()));
    assert_eq!(g1.over(), Some(-23.0));
    let g2 = e.bar("G2").unwrap();
    assert_eq!((g2.verdict, g2.taken.as_ref().unwrap().arm.as_deref(), g2.over()), (Verdict::Not, Some("b1440"), Some(1408.0)));
    assert_eq!(e.bar("G3").unwrap().verdict, Verdict::Not);
    assert_eq!(e.bar("G3").unwrap().taken.as_ref().unwrap().value, 19.0);
    assert_eq!(e.bar("G4").unwrap().verdict, Verdict::NotMeasured);
    assert_eq!((e.bar("A1").unwrap().over(), e.bar("A2").unwrap().over()), (Some(6243.0), Some(12735.0)));
    // Section 3.1: 1 of 4 gates met, 0 of 2 aims, step 1 of 4.
    assert_eq!((e.gates_met, e.gates, e.aims_met, e.aims), (1, 4, 0, 2));
    assert_eq!((e.step, e.steps), (Some(1), 4));
    assert!(!e.all_met());
    assert!(e.skipped.is_empty(), "{:?}", e.skipped);
    assert_eq!(e.lanes.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(), ["PARITY-ROWS", "PARITY-BAR", "FRAME-SET", "H2H"]);
    assert!(e.lanes[2].1.is_some() && e.lanes[0].1.is_none());
    // Deltas are against the goal's last checkpoint (Q-GS7).
    assert!(e.checkpoint.is_none() && e.bars.iter().all(|b| b.delta.is_none()));
    let values = checkpoint_values(&e);
    assert!(values.contains("value: G2 5578 us @763e4d3a\n"), "{values}");
    let cp = entry(&mut s, "GOAL", "checkpoint", "goal: goal:g\n", &values.replace("G2 5578", "G2 5990"));
    // Written after the real one: a compaction checkpoint (no goal meta) and another goal's checkpoint are newer and ignored.
    entry(&mut s, "GOAL", "checkpoint", "transcript: x\n", "value: G2 9999 us @0000000\n");
    entry(&mut s, "GOAL", "checkpoint", "goal: goal:other\n", "value: G2 8888 us @0000000\n");
    let later = "metric: frame.gpu.p99@1440p 5100 us arm r1440 @9c1e07a2 quiet\nmetric: frame.gpu.p99@1440p 5166 us arm s1440 @9c1e07a2 quiet\nmetric: frame.gpu.p99@1440p 5020 us arm b1440 @9c1e07a2 quiet\n";
    measure(&mut s, "FRAME-SET", later);
    let e2 = evaluate(&s, "goal:g").unwrap();
    assert_eq!(e2.checkpoint.as_deref(), Some(cp.as_str()));
    let g2 = e2.bar("G2").unwrap();
    assert_eq!((g2.taken.as_ref().unwrap().arm.as_deref(), g2.delta), (Some("s1440"), Some(5166.0 - 5990.0)));
    assert_eq!(e2.bar("G1").unwrap().delta, Some(0.0));
    let shown = super::super::show(&s, Some("g")).unwrap();
    assert!(shown.contains("s1440 -824 since checkpoint)"), "{shown}");
    assert!(shown.contains("b1080 +0 since checkpoint)"), "{shown}");
}

#[test]
fn a_contended_line_does_not_feed_a_quiet_gate() {
    let (_d, mut s) = goal_store("contended", "gate G1 frame.gpu.p99@1080p < 4170 us quiet\ngate G2 frame.gpu.p99@1440p < 4170 us\n");
    measure(&mut s, "FRAME-SET", "metric: frame.gpu.p99@1080p 4100 us @aaaaaaa contended\nmetric: frame.gpu.p99@1440p 4100 us @aaaaaaa contended\n");
    let e = evaluate(&s, "g").unwrap();
    assert_eq!(e.bar("G1").unwrap().verdict, Verdict::NotMeasured, "contended alone: not measured");
    // A gate not marked quiet takes it, and says it was contended.
    let g2 = e.bar("G2").unwrap();
    assert_eq!((g2.verdict, g2.taken.as_ref().unwrap().quiet), (Verdict::Met, false));
    // An older quiet line feeds G1 past a newer contended one.
    measure(&mut s, "FRAME-SET", "metric: frame.gpu.p99@1080p 4300 us @bbbbbbb quiet\n");
    measure(&mut s, "FRAME-SET", "metric: frame.gpu.p99@1080p 4000 us @ccccccc contended\n");
    let g1 = evaluate(&s, "g").unwrap().bar("G1").unwrap().clone();
    assert_eq!((g1.verdict, g1.taken.unwrap().value), (Verdict::Not, 4300.0));
    // Over arms: a contended arm leaves the quiet set partial.
    let (_d2, mut s2) = goal_store("contended-arms", "gate G1 frame.gpu.p99@1080p < 4170 us over r,b quiet\n");
    measure(&mut s2, "F", "metric: frame.gpu.p99@1080p 4000 us arm r @ddddddd quiet\nmetric: frame.gpu.p99@1080p 4010 us arm b @ddddddd contended\n");
    let g = evaluate(&s2, "g").unwrap().bar("G1").unwrap().clone();
    assert_eq!(g.verdict, Verdict::NotMeasured);
    assert_eq!(g.partial.unwrap().missing, ["b"]);
}

#[test]
fn a_partial_set_is_reported_not_used() {
    let (_d, mut s) = goal_store("partial", "gate G1 frame.gpu.p99@1080p < 4170 us over r1080,s1080,b1080 quiet\n");
    measure(&mut s, "F", "metric: frame.gpu.p99@1080p 4000 us arm r1080 @1111111 quiet\nmetric: frame.gpu.p99@1080p 4050 us arm s1080 @1111111 quiet\nmetric: frame.gpu.p99@1080p 4100 us arm b1080 @1111111 quiet\n");
    // A newer commit with two of three arms, the missing one would fail.
    measure(&mut s, "F", "metric: frame.gpu.p99@1080p 3000 us arm r1080 @2222222 quiet\nmetric: frame.gpu.p99@1080p 3100 us arm s1080 @2222222 quiet\n");
    let e = evaluate(&s, "g").unwrap();
    let g = e.bar("G1").unwrap();
    let t = g.taken.as_ref().unwrap();
    assert_eq!((t.commit.as_str(), t.value, t.arm.as_deref()), ("1111111", 4100.0, Some("b1080")), "the older full set decides");
    let p = g.partial.as_ref().unwrap();
    assert_eq!((p.commit.as_str(), p.have.clone(), p.missing.clone()), ("2222222", vec!["r1080".to_string(), "s1080".to_string()], vec!["b1080".to_string()]));
    let shown = super::super::show(&s, Some("g")).unwrap();
    assert!(shown.contains("[newer @2222222 lacks b1080];"), "{shown}");
    // Only a partial set: not measured, the partial reported.
    let (_d2, mut s2) = goal_store("partial-only", "gate G1 frame.gpu.p99@1080p < 4170 us over r1080,b1080 quiet\n");
    measure(&mut s2, "F", "metric: frame.gpu.p99@1080p 3000 us arm r1080 @3333333 quiet\n");
    let g = evaluate(&s2, "g").unwrap().bar("G1").unwrap().clone();
    assert_eq!((g.verdict, g.partial.map(|p| p.missing)), (Verdict::NotMeasured, Some(vec!["b1080".to_string()])));
    // The missing arm landing later, in another entry, completes the set.
    measure(&mut s2, "F", "metric: frame.gpu.p99@1080p 4200 us arm b1080 @3333333 quiet\n");
    let g = evaluate(&s2, "g").unwrap().bar("G1").unwrap().clone();
    assert_eq!((g.verdict, g.taken.unwrap().value, g.partial), (Verdict::Not, 4200.0, None));
}

#[test]
fn an_aim_never_decides_met() {
    // Every gate met, every aim missed: met.
    let (_d, mut s) = goal_store("aim-miss", "gate G1 a.b < 10 us\naim A1 a.c < 10 us\nreport A1\n");
    measure(&mut s, "F", "metric: a.b 5 us @aaaaaaa quiet\nmetric: a.c 50 us @aaaaaaa quiet\n");
    let e = evaluate(&s, "g").unwrap();
    assert_eq!((e.gates_met, e.gates, e.aims_met, e.aims), (1, 1, 0, 1));
    assert!(e.all_met(), "a missed aim does not hold met back");
    // Only aims, all met: never met (no gate).
    let (_d2, mut s2) = goal_store("aim-only", "aim A1 a.c < 10 us\n");
    measure(&mut s2, "F", "metric: a.c 5 us @aaaaaaa quiet\n");
    let e = evaluate(&s2, "g").unwrap();
    assert_eq!((e.aims_met, e.gates), (1, 0));
    assert!(!e.all_met(), "an aim met does not make a goal met");
    // A gate missed, its aim met: not met.
    let (_d3, mut s3) = goal_store("aim-met", "gate G1 a.b < 10 us\naim A1 a.c < 10 us\n");
    measure(&mut s3, "F", "metric: a.b 50 us @aaaaaaa quiet\nmetric: a.c 5 us @aaaaaaa quiet\n");
    let e = evaluate(&s3, "g").unwrap();
    assert!(!e.all_met() && e.aims_met == 1 && e.gates_met == 0);
}

#[test]
fn an_unfed_gate_is_not_measured_never_met() {
    let (_d, mut s) = goal_store("unfed", "gate G1 a.b < 10 us\n");
    let e = evaluate(&s, "g").unwrap();
    assert_eq!(e.bar("G1").unwrap().verdict, Verdict::NotMeasured);
    assert!(!e.all_met());
    // Prose, other metrics, other units and other entry kinds feed nothing.
    measure(&mut s, "F", "a.b came in at 5 us on a quiet run\nmetric: a.c 5 us @aaaaaaa quiet\nmetric: a.b 5 ms @aaaaaaa quiet\n");
    entry(&mut s, "F", "note", "", "metric: a.b 5 us @aaaaaaa quiet\n");
    let e = evaluate(&s, "g").unwrap();
    assert_eq!(e.bar("G1").unwrap().verdict, Verdict::NotMeasured);
    assert!(!e.all_met());
    assert!(e.skipped.iter().any(|x| x.contains("ms") && x.contains("G1")), "{:?}", e.skipped);
    // A malformed line that reached the store is skipped and named.
    measure(&mut s, "F", "metric: a.b 5 us @aaaaaaa\n");
    let e = evaluate(&s, "g").unwrap();
    assert_eq!(e.bar("G1").unwrap().verdict, Verdict::NotMeasured);
    assert!(e.skipped.iter().any(|x| x.contains("quiet or contended")), "{:?}", e.skipped);
    // A goal with no spec has no gates and is never met.
    let (_d2, mut s2) = store_in("unfed-nospec");
    set(&mut s2, "h", "L", "Text.", None, &who()).unwrap();
    let e = evaluate(&s2, "h").unwrap();
    assert!(e.bars.is_empty() && !e.all_met());
}

#[test]
fn g4_without_arms_takes_the_newest_quiet_line() {
    let (_d, mut s) = goal_store("g4", SPEC);
    measure(&mut s, "H2H", "metric: h2h.ratio.p99 0.61 x arm m1 @aaaaaaa quiet\n");
    measure(&mut s, "H2H", "metric: h2h.ratio.p99 0.72 x @bbbbbbb quiet\n");
    measure(&mut s, "H2H", "metric: h2h.ratio.p99 0.50 x @ccccccc contended\n");
    let g4 = evaluate(&s, "g").unwrap().bar("G4").unwrap().clone();
    let t = g4.taken.unwrap();
    assert_eq!((t.value, t.commit.as_str(), g4.verdict), (0.72, "bbbbbbb", Verdict::Not), "the newest quiet line, the contended one skipped");
    measure(&mut s, "H2H", "metric: h2h.ratio.p99 0.66 x @ddddddd quiet\n");
    let e = evaluate(&s, "g").unwrap();
    assert_eq!(e.bar("G4").unwrap().verdict, Verdict::Met);
    assert_eq!(e.step, Some(1), "step 1's done-when is unfed");
}

#[test]
fn worst_arm_decides_1080p_and_1440p() {
    let (_d, mut s) = goal_store("worst", SPEC);
    // Every arm but one under the bar, at both cases: the one decides.
    measure(
        &mut s,
        "FRAME-SET",
        "metric: frame.gpu.p99@1080p 3000 us arm r1080 @abcdef12 quiet
metric: frame.gpu.p99@1080p 4171 us arm s1080 @abcdef12 quiet
metric: frame.gpu.p99@1080p 3100 us arm b1080 @abcdef12 quiet
metric: frame.gpu.p99@1440p 4180 us arm r1440 @abcdef12 quiet
metric: frame.gpu.p99@1440p 3000 us arm s1440 @abcdef12 quiet
metric: frame.gpu.p99@1440p 3100 us arm b1440 @abcdef12 quiet
",
    );
    let e = evaluate(&s, "g").unwrap();
    let (g1, g2) = (e.bar("G1").unwrap(), e.bar("G2").unwrap());
    assert_eq!((g1.verdict, g1.taken.as_ref().unwrap().arm.as_deref()), (Verdict::Not, Some("s1080")));
    assert_eq!((g2.verdict, g2.taken.as_ref().unwrap().arm.as_deref()), (Verdict::Not, Some("r1440")));
    // The next set, every arm under: both met.
    measure(
        &mut s,
        "FRAME-SET",
        "metric: frame.gpu.p99@1080p 4101 us arm r1080 @abcdef13 quiet
metric: frame.gpu.p99@1080p 4000 us arm s1080 @abcdef13 quiet
metric: frame.gpu.p99@1080p 4100 us arm b1080 @abcdef13 quiet
metric: frame.gpu.p99@1440p 4152 us arm r1440 @abcdef13 quiet
metric: frame.gpu.p99@1440p 4000 us arm s1440 @abcdef13 quiet
metric: frame.gpu.p99@1440p 4151 us arm b1440 @abcdef13 quiet
",
    );
    let e = evaluate(&s, "g").unwrap();
    let (g1, g2) = (e.bar("G1").unwrap(), e.bar("G2").unwrap());
    assert_eq!((g1.verdict, g1.taken.as_ref().unwrap().value, g1.taken.as_ref().unwrap().arm.as_deref()), (Verdict::Met, 4101.0, Some("r1080")));
    assert_eq!((g2.verdict, g2.taken.as_ref().unwrap().value), (Verdict::Met, 4152.0));
    // take median takes the middle arm, not the worst.
    let (_d2, mut s2) = goal_store("median", "gate G1 m.x < 10 us over a,b,c take median\n");
    measure(&mut s2, "F", "metric: m.x 1 us arm a @aaaaaaa quiet\nmetric: m.x 50 us arm b @aaaaaaa quiet\nmetric: m.x 5 us arm c @aaaaaaa quiet\n");
    let g = evaluate(&s2, "g").unwrap().bar("G1").unwrap().clone();
    assert_eq!((g.verdict, g.taken.unwrap().arm.as_deref()), (Verdict::Met, Some("c")));
}

#[test]
fn a_metric_line_parses_in_the_section_3_form_only() {
    let m = parse_metric("metric: frame.gpu.p99@1440p 5578 us arm b1440 @763E4D3A quiet").unwrap();
    assert_eq!((m.metric.as_str(), m.value, m.unit.as_str(), m.arm.as_deref(), m.commit.as_str(), m.quiet), ("frame.gpu.p99@1440p", 5578.0, "us", Some("b1440"), "763e4d3a", true));
    let m = parse_metric("  metric: parity.rows.open 19 rows @763e4d3a contended").unwrap();
    assert_eq!((m.arm, m.quiet), (None, false));
    for (bad, why) in [
        ("metric: 9fps 5 us @763e4d3a quiet", "metric name"),
        ("metric: a.b five us @763e4d3a quiet", "numeric value"),
        ("metric: a.b 5 @763e4d3a quiet", "has no unit"),
        ("metric: a.b 5 us @abc12 quiet", "@<commit>"),
        ("metric: a.b 5 us @zzzzzzzz quiet", "@<commit>"),
        ("metric: a.b 5 us arm @763e4d3a quiet", "arm names no arm"),
        ("metric: a.b 5 us 763e4d3a quiet", "@<commit>"),
        ("metric: a.b 5 us @xyz quiet", "@<commit>"),
        ("metric: a.b 5 us @763e4d3a", "quiet or contended"),
        ("metric: a.b 5 us @763e4d3a loud", "quiet or contended"),
        ("metric: a.b 5 us @763e4d3a quiet extra", "after the machine state"),
        ("metric: a.b inf us @763e4d3a quiet", "numeric value"),
    ] {
        let e = parse_metric(bad).unwrap_err();
        assert!(e.contains(why) && e.contains(METRIC_FORM), "{bad}: {e}");
    }
    assert_eq!(check_metric_lines("headline\nprose 5 us\nmetric: a.b 5 us @763e4d3a quiet\n").unwrap(), 1);
    let e = check_metric_lines("headline\nmetric: a.b 5 us @763e4d3a quiet\nmetric: a.b 5 us\n").unwrap_err().to_string();
    assert!(e.contains("line 3") && e.contains("nothing written"), "{e}");
}

#[test]
fn holds_at_and_around_the_bar_for_every_op() {
    // (op, holds at bar-1, bar, bar+1)
    for (cmp, below, at, above) in [
        (Cmp::Lt, true, false, false),
        (Cmp::Le, true, true, false),
        (Cmp::Gt, false, false, true),
        (Cmp::Ge, false, true, true),
        (Cmp::Eq, false, true, false),
    ] {
        let got = (holds(cmp, 99.0, 100.0), holds(cmp, 100.0, 100.0), holds(cmp, 101.0, 100.0));
        assert_eq!(got, (below, at, above), "{cmp:?}");
    }
}

/// One set of arms under `spec`; the arm the gate took and its verdict.
fn taken_arm(name: &str, spec: &str, lines: &str) -> (Verdict, Option<String>) {
    let (_d, mut s) = goal_store(name, spec);
    measure(&mut s, "F", lines);
    let b = evaluate(&s, "g").unwrap().bar("G1").unwrap().clone();
    (b.verdict, b.taken.and_then(|t| t.arm))
}

#[test]
fn worst_arm_of_a_ge_gate_is_the_lowest() {
    let (v, arm) = taken_arm("ge", "gate G1 m.fps >= 240 fps over a,b take worst\n", "metric: m.fps 300 fps arm a @aaaaaaa quiet\nmetric: m.fps 200 fps arm b @aaaaaaa quiet\n");
    assert_eq!((v, arm.as_deref()), (Verdict::Not, Some("b")));
    let (v, arm) = taken_arm("gt", "gate G1 m.fps > 240 fps over a,b take worst\n", "metric: m.fps 300 fps arm a @aaaaaaa quiet\nmetric: m.fps 250 fps arm b @aaaaaaa quiet\n");
    assert_eq!((v, arm.as_deref()), (Verdict::Met, Some("b")));
    // Eq: the worst arm is the farthest from a nonzero bar, either side.
    let (_, arm) = taken_arm("eq", "gate G1 m.x = 5 u over a,b take worst\n", "metric: m.x 0 u arm a @aaaaaaa quiet\nmetric: m.x 8 u arm b @aaaaaaa quiet\n");
    assert_eq!(arm.as_deref(), Some("a"));
}

#[test]
fn a_negative_bar_keeps_the_worst_arm() {
    // is_number accepts negatives: -10 is farther below the bar -5 than -2 is.
    let (v, arm) = taken_arm("negbar", "gate G1 m.d < -5 u over a,b take worst\n", "metric: m.d -10 u arm a @aaaaaaa quiet\nmetric: m.d -2 u arm b @aaaaaaa quiet\n");
    assert_eq!((v, arm.as_deref()), (Verdict::Not, Some("b")));
}

#[test]
fn a_negative_value_against_a_ge_bar_is_the_worst_arm() {
    let (v, arm) = taken_arm("negval", "gate G1 m.fps >= 240 fps over a,b take worst\n", "metric: m.fps 300 fps arm a @aaaaaaa quiet\nmetric: m.fps -10 fps arm b @aaaaaaa quiet\n");
    assert_eq!((v, arm.as_deref()), (Verdict::Not, Some("b")));
}

#[test]
fn take_median_of_two_arms_is_the_worse_one() {
    let (_, arm) = taken_arm("median2", "gate G1 m.x < 10 us over a,b take median\n", "metric: m.x 9 us arm a @aaaaaaa quiet\nmetric: m.x 2 us arm b @aaaaaaa quiet\n");
    assert_eq!(arm.as_deref(), Some("a"), "index len/2 of the best-to-worst order");
}

#[test]
fn a_step_passes_only_on_a_quiet_line_of_its_own_metric() {
    let spec = "gate G1 m.x < 10 us\nstep 1 rows done-when parity.rows.open = 0\nstep 2 last\n";
    let (_d, mut s) = goal_store("step", spec);
    measure(&mut s, "P", "metric: parity.rows.open 0 rows @aaaaaaa contended\nmetric: other.m 0 rows @aaaaaaa quiet\n");
    assert_eq!(evaluate(&s, "g").unwrap().step, Some(1), "a contended line or another metric does not pass the step");
    measure(&mut s, "P", "metric: parity.rows.open 0 rows @bbbbbbb quiet\n");
    assert_eq!(evaluate(&s, "g").unwrap().step, Some(2));
}
