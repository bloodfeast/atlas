//! ATLAS-GOAL build step 1 (docs/design/atlas-goal.md section 7): the goal
//! record, its body splitter and spec parser, `note op=goal` and the seed.

use super::*;
use crate::memory::{Draft, MemoryStore};

/// Memory active-goal-engine-parity v4's original text, as quoted there.
const TEXT: &str = "Reach parity with the-game-on-Bevy in the new engine with at least 50% better performance. Don't cut corners: higher fidelity and better performance. The hard number is under 4.25 ms per frame at 4K with ray tracing on, with traced bodies as the default.";

/// Memory active-goal-engine-parity v4's AMENDED paragraph, byte for byte.
const AMENDMENT: &str = "AMENDED 2026-11-04 (Brandon, confirming his 10-22 easing): the hard line is 240 fps (under 4.17 ms) at 1080p and 1440p, RT on, traced bodies default. 4K is reported every time; 4.25 ms at 4K stays the pressed AIM (Brandon 11-04: \"an aim but at the highest settings probably not feasible\"), never a gate. See [[4k-target-is-an-aim-not-a-gate]]. Brandon 11-04: \"If we can get my machine to run 4k over 144fps that would be great\" - so at 4K on his RTX 3090 at the HIGHEST settings (Ultra), the target is over 144 fps (< 6.94 ms); report the 4K Ultra time against it alongside the default-settings 4K time against 4.25 ms. When restating the goal, give the original text and this amendment.";

/// Section 3.1's proposed spec, as the orchestrator's `--spec` file.
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

fn memory_body() -> String {
    format!(
        "Brandon's active /goal (set before 2026-10-22, cleared 2026-10-23 ~11:00 only to restart the terminal; he will ask what it was):\n\n\"{TEXT}\"\n\n{AMENDMENT}\n\nOrder of work under it: the PARITY rows, then the Bevy head-to-head, then the networked 1v1, then the campaign vertical slice.\nStatus at clearing: 4K ~4.6 ms median (release, timing off), 1440p ~2.8 ms. Related: [[engine-parity-goal]], [[game-frame-rate-target]].\n"
    )
}

fn store_in(name: &str) -> (std::path::PathBuf, MemoryStore) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("atlas-goal-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let s = MemoryStore::open(&d.join("memory.sqlite")).unwrap();
    (d, s)
}

fn who() -> Who<'static> {
    Who { author: "orchestrator", client: "test", reason: "goal test write" }
}

fn put(s: &mut MemoryStore, key: &str, kind: &str, scope: &str, meta: &str, body: &str) {
    let d = Draft { title: key.into(), meta: meta.into(), body: body.into(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() };
    let mut w = s.writer().unwrap();
    w.create(key, kind, "", scope, &d, None).unwrap();
    w.commit().unwrap();
}

/// A store holding memory active-goal-engine-parity as v4 reads.
fn seeded_store(name: &str) -> (std::path::PathBuf, MemoryStore) {
    let (d, mut s) = store_in(name);
    put(&mut s, "active-goal-engine-parity", "memory", "project", "", &memory_body());
    (d, s)
}

fn goal_entries(s: &MemoryStore, lane: &str) -> Vec<crate::memory::Head> {
    s.lane(lane).unwrap().into_iter().filter(|h| h.kind == "journal" && h.sub == ENTRY).collect()
}

#[test]
fn the_seed_has_both_versions_and_the_spec() {
    let (_d, mut s) = seeded_store("seed-both");
    let w = seed(&mut s, "active-goal-engine-parity", Some(SPEC), "GOAL", &who()).unwrap();
    assert_eq!(w.iter().map(|w| w.seq).collect::<Vec<_>>(), [1, 2]);
    let h = s.head("goal:engine-parity").unwrap().unwrap();
    assert_eq!((h.kind.as_str(), h.scope.as_str(), h.seq), ("goal", "GOAL", 2));
    assert_eq!(state(&h.meta), State::Active);
    let hist = s.history("goal:engine-parity").unwrap();
    let v1 = split(&hist[0].body);
    assert_eq!((v1.text.as_str(), v1.amendments.len(), v1.spec.is_none()), (TEXT, 0, true));
    let v2 = split(&h.body);
    assert_eq!(v2.text, TEXT);
    assert_eq!(v2.amendments, [AMENDMENT]);
    let spec = parse_spec(v2.spec.as_deref().unwrap()).unwrap();
    assert_eq!(spec.gates.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), ["G1", "G2", "G3", "G4"]);
    assert_eq!(spec.aims.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), ["A1", "A2"]);
    assert_eq!(spec.report, ["A1", "A2"]);
    assert_eq!(spec.steps.len(), 4);
    assert_eq!(spec.lanes.len(), 4);
    // The seed is one-shot.
    assert!(seed(&mut s, "active-goal-engine-parity", Some(SPEC), "GOAL", &who()).is_err());
}

#[test]
fn the_seed_amendment_is_the_memory_paragraph_byte_for_byte() {
    let (_d, mut s) = seeded_store("seed-bytes");
    seed(&mut s, "active-goal-engine-parity", None, "GOAL", &who()).unwrap();
    let h = s.head("goal:engine-parity").unwrap().unwrap();
    let b = split(&h.body);
    assert_eq!(b.amendments.len(), 1);
    assert_eq!(b.amendments[0].as_bytes(), AMENDMENT.as_bytes());
    assert!(b.amendments[0].ends_with("give the original text and this amendment."));
    // The paragraphs that are neither the text nor an amendment stay in the memory.
    assert!(!h.body.contains("Order of work") && !h.body.contains("Status at clearing"));
}

#[test]
fn without_a_spec_file_the_seed_has_no_gates() {
    let (_d, mut s) = seeded_store("seed-nospec");
    let w = seed(&mut s, "active-goal-engine-parity", None, "GOAL", &who()).unwrap();
    assert_eq!(w.len(), 2);
    let h = s.head("goal:engine-parity").unwrap().unwrap();
    assert!(split(&h.body).spec.is_none());
    assert!(!h.body.contains("```"));
    assert!(goal_entries(&s, "GOAL").is_empty(), "no spec, no goal journal entry");
    let shown = show(&s, Some("engine-parity")).unwrap();
    assert!(shown.contains("no gates: not measured, never met"), "{shown}");
    assert!(!shown.contains("MET"));
}

#[test]
fn atlas_never_writes_a_spec_line_the_caller_did_not_send() {
    let (_d, mut s) = store_in("spec-verbatim");
    let sent = "gate G1 frame.gpu.p99@1080p < 4170 us quiet\naim A1 frame.gpu.p99@4k < 4250 us\n";
    set(&mut s, "g", "LANE", "Make it fast.", Some(sent), &who()).unwrap();
    let h = s.head("goal:g").unwrap().unwrap();
    let stored = split(&h.body).spec.unwrap();
    assert_eq!(stored, sent);
    // Every line between the fences is a line the caller sent.
    let inside: Vec<&str> = h.body.split("```goal\n").nth(1).unwrap().split("```").next().unwrap().lines().collect();
    assert_eq!(inside, sent.lines().collect::<Vec<_>>());
    // The journal entry holds the spec as written.
    let e = goal_entries(&s, "LANE");
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].body.trim_end(), sent.trim_end());
    // An amendment without a spec keeps the spec as it was, and adds none.
    let sha = h.sha.clone();
    amend(&mut s, "g", &sha, Some("AMENDED 2026-11-25 (Brandon): also at 1440p."), None, &who()).unwrap();
    let h2 = s.head("goal:g").unwrap().unwrap();
    assert_eq!(split(&h2.body).spec.as_deref(), Some(sent));
    // A goal set from prose alone has no spec: atlas derives none.
    set(&mut s, "p", "PL", "Under 4.17 ms at 1080p and 1440p, report 4K.", None, &who()).ok();
    let p = s.head("goal:p").unwrap();
    if let Some(p) = p {
        assert!(split(&p.body).spec.is_none() && !p.body.contains("gate "));
    }
}

#[test]
fn a_spec_write_appends_one_goal_journal_entry_with_the_version_sha() {
    let (_d, mut s) = store_in("spec-entry");
    let w = set(&mut s, "g", "LANE", "Make it fast.", Some("gate G1 a.b < 5 us\n"), &who()).unwrap();
    let e = goal_entries(&s, "LANE");
    assert_eq!(e.len(), 1);
    assert!(e[0].meta.lines().any(|l| l == format!("sha: {}", w.sha)), "{}", e[0].meta);
    assert!(e[0].meta.lines().any(|l| l == "version: 1"), "{}", e[0].meta);
    assert!(e[0].meta.lines().any(|l| l == "goal: goal:g"), "{}", e[0].meta);
    assert!(e[0].meta.lines().any(|l| l == "by: orchestrator"), "{}", e[0].meta);
    // A text-only amendment writes no entry; one with a spec writes one more.
    let w2 = amend(&mut s, "g", &w.sha, Some("AMENDED 2026-11-25 (Brandon): and 1440p."), None, &who()).unwrap();
    assert_eq!(goal_entries(&s, "LANE").len(), 1);
    let w3 = amend(&mut s, "g", &w2.sha, None, Some("gate G1 a.b < 4 us\n"), &who()).unwrap();
    let e = goal_entries(&s, "LANE");
    assert_eq!(e.len(), 2);
    assert!(e[1].meta.lines().any(|l| l == format!("sha: {}", w3.sha)), "{}", e[1].meta);
    assert!(e[1].meta.lines().any(|l| l == "version: 3"), "{}", e[1].meta);
    // A goal without a spec writes no entry.
    let (_d2, mut s2) = store_in("spec-entry-none");
    set(&mut s2, "h", "L2", "Text.", None, &who()).unwrap();
    assert!(goal_entries(&s2, "L2").is_empty());
}

#[test]
fn an_amendment_is_a_new_version_and_the_head_has_every_amendment() {
    let (_d, mut s) = store_in("amend");
    let w1 = set(&mut s, "g", "LANE", "Original text.\n\nSecond paragraph of it.", Some("gate G1 a.b < 5 us\n"), &who()).unwrap();
    let a1 = "AMENDED 2026-11-04 (Brandon): first.";
    let a2 = "AMENDED 2026-11-20 (Brandon): second.";
    let w2 = amend(&mut s, "g", &w1.sha, Some(a1), None, &who()).unwrap();
    let w3 = amend(&mut s, "g", &w2.sha, Some(a2), None, &who()).unwrap();
    assert_eq!((w2.seq, w3.seq), (2, 3));
    let b = split(&s.head("goal:g").unwrap().unwrap().body);
    assert_eq!(b.text, "Original text.\n\nSecond paragraph of it.");
    assert_eq!(b.amendments, [a1, a2]);
    assert!(b.spec.is_some());
    // A stale sha is refused, and text not in the AMENDED form is refused.
    assert!(amend(&mut s, "g", &w1.sha, Some("AMENDED 2026-11-21 (B): x."), None, &who()).is_err());
    assert!(amend(&mut s, "g", &w3.sha, Some("just more words"), None, &who()).is_err());
    assert_eq!(s.head("goal:g").unwrap().unwrap().seq, 3);
}

#[test]
fn a_gate_with_no_metric_is_refused() {
    let e = parse_spec("gate G1 < 4170 us\n").unwrap_err();
    assert!(e.contains("line 1") && e.contains("metric"), "{e}");
    assert!(parse_spec("gate G1\n").is_err());
    assert!(parse_spec("aim A1 < 4 us\n").is_err());
    assert!(parse_spec("wibble G1 a.b < 4 us\n").is_err(), "a malformed line is refused");
    assert!(parse_spec("gate G1 a.b < 4 us\ngate G1 a.c < 4 us\n").is_err(), "a duplicate id is refused");
    let (_d, mut s) = store_in("nometric");
    assert!(set(&mut s, "g", "LANE", "Text.", Some("gate G1 < 4170 us\n"), &who()).is_err());
    assert!(s.head("goal:g").unwrap().is_none(), "nothing written");
    assert!(goal_entries(&s, "LANE").is_empty());
}

#[test]
fn a_second_active_goal_is_refused() {
    let (_d, mut s) = store_in("second");
    set(&mut s, "a", "LA", "First.", None, &who()).unwrap();
    let e = set(&mut s, "b", "LB", "Second.", None, &who()).unwrap_err().to_string();
    assert!(e.contains("goal:a") && e.contains("pause"), "{e}");
    assert!(s.head("goal:b").unwrap().is_none());
    change(&mut s, "a", Action::Pause, &who()).unwrap();
    set(&mut s, "b", "LB", "Second.", None, &who()).unwrap();
    // Resuming the first while the second is active is refused too.
    assert!(change(&mut s, "a", Action::Resume, &who()).is_err());
    assert_eq!(state(&s.head("goal:a").unwrap().unwrap().meta), State::Paused);
    change(&mut s, "b", Action::Drop, &who()).unwrap();
    change(&mut s, "a", Action::Resume, &who()).unwrap();
    assert_eq!(state(&s.head("goal:a").unwrap().unwrap().meta), State::Active);
}

#[test]
fn report_lists_only_known_aims() {
    assert_eq!(parse_spec("aim A1 a.b < 4 us\naim A2 a.c < 5 us\nreport A1 A2\n").unwrap().report, ["A1", "A2"]);
    let e = parse_spec("aim A1 a.b < 4 us\nreport A1 A9\n").unwrap_err();
    assert!(e.contains("A9"), "{e}");
    assert!(parse_spec("gate G1 a.b < 4 us\naim A1 a.c < 4 us\nreport A1 G1\n").is_err(), "a gate is not an aim");
    assert!(parse_spec("report\n").is_err());
}

// Beyond the nine: the follow-ups of journal:ATLAS-GOAL:0029 item 1 and the
// rulings of journal:ATLAS-GOAL:0031.

#[test]
fn an_issued_met_token_reads_issued_at_the_store_and_only_met_token_reads_it() {
    let (_d, mut s) = store_in("redact");
    // Not the `met-token <12 hex>` form the design keeps out of fixtures.
    let nonce = "nonce-under-test";
    put(&mut s, "goal:x", "goal", "L", &format!("state: done\nmet_token: {nonce}\nmet_commit: abc\n"), "Text.\n");
    let h = s.head("goal:x").unwrap().unwrap();
    assert!(h.meta.contains("met_token: issued") && !h.meta.contains(nonce), "{}", h.meta);
    assert!(s.history("goal:x").unwrap().iter().all(|v| !v.meta.contains(nonce)));
    assert!(s.heads(Some(KIND), None).unwrap().iter().all(|h| !h.meta.contains(nonce)));
    assert_eq!(met_token(&s, "x").unwrap().as_deref(), Some(nonce));
    // Another kind's meta is never touched.
    put(&mut s, "m", "memory", "project", &format!("met_token: {nonce}\n"), "b\n");
    assert!(s.head("m").unwrap().unwrap().meta.contains(nonce));
}

#[test]
fn goal_records_reads_goal_kind_with_the_active_goal_fallback() {
    let (_d, mut s) = seeded_store("records");
    let keys = |s: &MemoryStore| s.goal_records().unwrap().into_iter().map(|h| h.key).collect::<Vec<_>>();
    assert_eq!(keys(&s), ["active-goal-engine-parity"], "no goal record yet: the memory fallback");
    seed(&mut s, "active-goal-engine-parity", Some(SPEC), "GOAL", &who()).unwrap();
    assert_eq!(keys(&s), ["goal:engine-parity"]);
    change(&mut s, "engine-parity", Action::Pause, &who()).unwrap();
    assert!(keys(&s).is_empty(), "a paused goal is not active, and the fallback is over once a goal record exists");
}

#[test]
fn done_is_refused_in_step_1_and_a_state_change_keeps_the_text() {
    let (_d, mut s) = store_in("done");
    let w = set(&mut s, "g", "L", "Text.", Some("gate G1 a.b < 5 us\n"), &who()).unwrap();
    let e = change(&mut s, "g", Action::Done, &who()).unwrap_err().to_string();
    assert!(e.contains("drop"), "{e}");
    let p = change(&mut s, "g", Action::Pause, &who()).unwrap();
    assert_eq!(p.seq, 2);
    let h = s.head("goal:g").unwrap().unwrap();
    assert_eq!((h.body.as_str(), state(&h.meta)), (s.history("goal:g").unwrap()[0].body.as_str(), State::Paused));
    assert_ne!(p.sha, w.sha);
    assert!(change(&mut s, "g", Action::Pause, &who()).is_err(), "pause applies to an active goal only");
    change(&mut s, "g", Action::Drop, &who()).unwrap();
    let sha = s.head("goal:g").unwrap().unwrap().sha;
    assert!(amend(&mut s, "g", &sha, Some("AMENDED 2026-11-25 (B): x."), None, &who()).is_err());
    assert!(show(&s, Some("g")).unwrap().starts_with("== goal:g goal dropped v3"));
}

#[test]
fn split_and_join_round_trip_the_design_example() {
    let b = Body { text: TEXT.into(), amendments: vec![AMENDMENT.into()], spec: Some(SPEC.into()), notes: "Notes: version 1 is the original text alone.".into() };
    assert_eq!(split(&join(&b)), b);
    assert!(parse_spec(SPEC).is_ok());
    assert_eq!(normalize_spec(&format!("```goal\n{SPEC}```\n")).unwrap(), SPEC);
}

#[test]
fn the_seed_cli_reads_the_spec_file() {
    let (d, mut s) = seeded_store("cli");
    let f = d.join("spec.txt");
    std::fs::write(&f, SPEC).unwrap();
    let args: Vec<String> = ["seed", "--from-memory", "active-goal-engine-parity", "--spec", f.to_str().unwrap()].iter().map(|x| x.to_string()).collect();
    cli(&args, &mut s).unwrap();
    assert_eq!(split(&s.head("goal:engine-parity").unwrap().unwrap().body).spec.as_deref(), Some(SPEC));
    assert!(cli(&["seed".to_string()], &mut s).is_err());
}

// Step 1 follow-ups (journal:ATLAS-GOAL:0037).

#[test]
fn archive_as_of_hides_the_met_token() {
    let (d, mut s) = store_in("asof");
    let nonce = "nonce-under-test";
    put(&mut s, "goal:x", "goal", "L", &format!("state: done\nmet_token: {nonce}\n"), "Text.\n");
    drop(s);
    let a = crate::memory::archive::as_of(&[d.join("memory.sqlite")], "goal:x", "9999").unwrap().unwrap();
    assert!(a.meta.contains("met_token: issued") && !a.meta.contains(nonce), "{}", a.meta);
}

#[test]
fn a_crlf_memory_body_seeds() {
    let (_d, mut s) = store_in("crlf");
    put(&mut s, "active-goal-engine-parity", "memory", "project", "", &memory_body().replace('\n', "\r\n"));
    let w = seed(&mut s, "active-goal-engine-parity", Some(SPEC), "GOAL", &who()).unwrap();
    assert_eq!(w.len(), 2);
    let b = split(&s.head("goal:engine-parity").unwrap().unwrap().body);
    assert_eq!((b.text.as_str(), b.amendments.len()), (TEXT, 1));
    assert_eq!(b.amendments[0].as_bytes(), AMENDMENT.as_bytes(), "the paragraph's bytes, its line ends LF");
    // split reads a CRLF body as it reads the LF one.
    let lf = join(&Body { text: "One.\n\nTwo.".into(), amendments: vec![AMENDMENT.into()], spec: Some(SPEC.into()), notes: String::new() });
    assert_eq!(split(&lf.replace('\n', "\r\n")), split(&lf));
}

#[test]
fn the_seed_applies_sets_checks_to_what_it_copies() {
    for (name, body) in [
        ("fence", format!("\"{TEXT}\n```goal\ngate G1 a.b < 1 us\n```\"\n\n{AMENDMENT}\n")),
        ("amended-in-text", format!("\"{TEXT}\nAMENDED 2026-11-04 (B): inside.\"\n")),
        ("fence-in-amendment", format!("\"{TEXT}\"\n\n{AMENDMENT}\n```goal\n")),
        ("amendment-form", format!("\"{TEXT}\"\n\nAMENDED soon: no date or author.\n")),
    ] {
        let (_d, mut s) = store_in(name);
        put(&mut s, "active-goal-x", "memory", "project", "", &body);
        let e = seed(&mut s, "active-goal-x", None, "GOAL", &who()).unwrap_err();
        assert!(matches!(e, Error::InvalidArgument(_)), "{name}: {e}");
        assert!(s.head("goal:x").unwrap().is_none(), "{name}: nothing written");
    }
}

#[test]
fn the_amended_form_needs_a_date_and_an_author() {
    let (_d, mut s) = store_in("form");
    let w = set(&mut s, "g", "L", "Text.", None, &who()).unwrap();
    for bad in ["AMENDED x", "AMENDED 2026-11-04: no author.", "AMENDED 2026-11-04 (): empty author.", "AMENDED 26-11-04 (B): short date.", "AMENDED 2026-11-04 (B) no colon."] {
        assert!(amend(&mut s, "g", &w.sha, Some(bad), None, &who()).is_err(), "{bad}");
    }
    amend(&mut s, "g", &w.sha, Some("AMENDED 2026-11-04 (Brandon, confirming his 10-22 easing): ok."), None, &who()).unwrap();
}
