//! The Binding compiler: the few lines of a lane a launched agent is bound
//! by. Selection order, supersession followed, named keys skipped, the byte
//! budget and its `+N more` line, and byte stability. All lanes and text here
//! are invented.

use std::collections::HashSet;

use atlas::binding::{self, BUDGET, LINE_CHARS, Rank, Recency};
use atlas::memory::{Draft, Head, MemoryStore};

const LANE: &str = "WIDGET-CACHE";

fn at(day: u32) -> String {
    format!("2026-01-{day:02}T10:00:00.000Z")
}

fn draft(title: &str, body: &str, day: u32) -> Draft {
    Draft {
        title: title.into(),
        body: body.into(),
        reason: "testing the binding".into(),
        author: "test".into(),
        client: "test".into(),
        time: Some(at(day)),
        ..Draft::default()
    }
}

fn put(store: &mut MemoryStore, key: &str, kind: &str, sub: &str, scope: &str, d: &Draft) {
    let mut w = store.writer().unwrap();
    w.create(key, kind, sub, scope, d, None).unwrap();
    w.commit().unwrap();
}

fn journal(store: &mut MemoryStore, n: u32, sub: &str, title: &str, body: &str, day: u32) -> String {
    let key = format!("journal:{LANE}:{n:04}");
    put(store, &key, "journal", sub, LANE, &draft(title, body, day));
    key
}

fn sha12(store: &MemoryStore, key: &str) -> String {
    store.head(key).unwrap().unwrap().sha[..12].to_string()
}

/// A lane with one of everything the block selects.
fn lane() -> MemoryStore {
    let mut s = MemoryStore::in_memory().unwrap();
    journal(&mut s, 1, "brief", "Build the widget cache", "Build the widget cache.", 1);
    journal(&mut s, 2, "decision", "Cache bounded at 64", "Owner 2026-01-02: \"bound the widget cache at 64 entries\", no growth.", 2);
    journal(&mut s, 3, "decision", "Exports go through a rename", "Exports are written atomically through a rename.", 3);
    journal(&mut s, 4, "question", "Should the cache prune on idle?", "Should the cache prune on idle?", 4);
    put(
        &mut s,
        &format!("handoff:{LANE}:001"),
        "handoff",
        "",
        LANE,
        &draft("Cache half done, eviction left", "State: the cache is half done.", 5),
    );
    journal(&mut s, 6, "measurement", "Cache hit rate 91%", "Hit rate 91%.", 6);
    journal(&mut s, 7, "next", "Write the eviction test", "Write the eviction test.", 7);
    s
}

fn compile(s: &MemoryStore, brief: &str) -> String {
    binding::compile(s, brief, LANE).unwrap().map(|b| b.text).unwrap_or_default()
}

#[test]
fn the_block_is_handoff_then_decisions_newest_first_then_open_then_next() {
    let s = lane();
    let k = |n: u32| format!("journal:{LANE}:{n:04}");
    let h = format!("handoff:{LANE}:001");
    let want = format!(
        "atlas binding: lane {LANE} (recall KEY for full text; these bind this task)\n\
         handoff: {h}@{}: Cache half done, eviction left\n\
         - {}@{}: Exports go through a rename\n\
         - {}@{}: \"bound the widget cache at 64 entries\"\n\
         open: {}@{}: Should the cache prune on idle?\n\
         next: {}@{}: Write the eviction test\n",
        sha12(&s, &h),
        k(3),
        sha12(&s, &k(3)),
        k(2),
        sha12(&s, &k(2)),
        k(4),
        sha12(&s, &k(4)),
        k(7),
        sha12(&s, &k(7)),
    );
    assert_eq!(compile(&s, &format!("Kind: builder\nLane: {LANE}\nDo the thing.")), want);
}

#[test]
fn briefs_measurements_and_answered_questions_are_not_bound() {
    let mut s = lane();
    let q = format!("journal:{LANE}:0004");
    let a = journal(&mut s, 8, "answered", "Prune on idle: yes", "Yes.", 8);
    s.writer()
        .and_then(|mut w| {
            w.add_refs(&a, &[("answers".into(), q.clone())])?;
            w.commit()
        })
        .unwrap();
    let text = compile(&s, "a task");
    assert!(!text.contains(&q), "{text}");
    assert!(!text.contains(":0001@") && !text.contains(":0006@") && !text.contains(":0008@"), "{text}");
}

#[test]
fn a_next_step_older_than_the_handoff_is_left_out() {
    let mut s = MemoryStore::in_memory().unwrap();
    journal(&mut s, 1, "decision", "A ruling", "A ruling.", 1);
    let next = journal(&mut s, 2, "next", "An old next step", "Old.", 2);
    put(&mut s, &format!("handoff:{LANE}:001"), "handoff", "", LANE, &draft("Newer handoff", "State.", 3));
    let text = compile(&s, "a task");
    assert!(!text.contains(&next), "{text}");
    assert!(text.contains("handoff: handoff:"), "{text}");
}

#[test]
fn a_superseded_decision_is_replaced_by_its_live_successor_even_in_another_lane() {
    let mut s = lane();
    let old = format!("journal:{LANE}:0002");
    let mut w = s.writer().unwrap();
    w.supersede(
        std::slice::from_ref(&old),
        "journal:OTHER-LANE:0001",
        "journal",
        "decision",
        "OTHER-LANE",
        &draft("Cache grows freely", "The cache grows freely now.", 9),
        None,
    )
    .unwrap();
    w.commit().unwrap();
    let text = compile(&s, "a task");
    assert!(!text.contains(&format!("{old}@")), "superseded key bound: {text}");
    let line = text.lines().find(|l| l.contains("journal:OTHER-LANE:0001@")).unwrap_or_else(|| panic!("no successor: {text}"));
    assert!(line.ends_with(&format!("[replaces {old}]")), "{line}");
    // Newest first: the successor (day 9) leads the decisions.
    let first = text.lines().find(|l| l.starts_with("- ")).unwrap();
    assert_eq!(first, line);
}

#[test]
fn a_chain_of_successors_is_followed_to_its_end() {
    let mut s = lane();
    let old = format!("journal:{LANE}:0002");
    let mut w = s.writer().unwrap();
    w.supersede(
        std::slice::from_ref(&old),
        "journal:OTHER-LANE:0001",
        "journal",
        "decision",
        "OTHER-LANE",
        &draft("Middle", "Middle.", 9),
        None,
    )
    .unwrap();
    w.supersede(
        &["journal:OTHER-LANE:0001".into()],
        "journal:OTHER-LANE:0002",
        "journal",
        "decision",
        "OTHER-LANE",
        &draft("Last", "Last.", 10),
        None,
    )
    .unwrap();
    w.commit().unwrap();
    // The chain is followed to its end.
    let text = compile(&s, "a task");
    assert!(text.contains("journal:OTHER-LANE:0002@"), "{text}");
    assert!(!text.contains("journal:OTHER-LANE:0001@"), "{text}");
}

#[test]
fn managed_rules_and_decision_records_of_the_lane_are_bound() {
    let mut s = lane();
    put(&mut s, "rules:r00001", "rule", "", LANE, &draft("", "- 2026-01-08 Owner: the widget cache never blocks a reader.\n", 8));
    put(&mut s, "decision:cache-shape", "decision", "", LANE, &draft("Cache is a ring", "The cache is a ring buffer.", 9));
    put(&mut s, "rules:r00002", "rule", "", "SOME-OTHER-LANE", &draft("", "- 2026-01-09 Owner: not this lane.\n", 9));
    let text = compile(&s, "a task");
    assert!(text.contains("- rules:r00001@"), "{text}");
    assert!(text.contains("never blocks a reader"), "{text}");
    assert!(text.contains("- decision:cache-shape@"), "{text}");
    assert!(!text.contains("rules:r00002"), "{text}");
}

#[test]
fn keys_the_brief_names_are_skipped_with_or_without_a_hash() {
    let s = lane();
    let d3 = format!("journal:{LANE}:0003");
    let h = format!("handoff:{LANE}:001");
    let brief = format!("Lane: {LANE}\nBackground: {d3}@{}: exports. Read {h} first.", sha12(&s, &d3));
    let text = compile(&s, &brief);
    // Listed once, as named, key and title; not again in the lane's part.
    assert!(text.contains(&format!("named: {d3}@{}: Exports go through a rename\n", sha12(&s, &d3))), "{text}");
    assert!(text.contains(&format!("named: {h}@")), "{text}");
    assert_eq!(text.matches(d3.as_str()).count(), 1, "{text}");
    assert!(!text.contains(&format!("handoff: {h}")), "{text}");
    assert!(text.contains(&format!("- journal:{LANE}:0002@")), "{text}");
    let named = binding::named_keys(&s, &brief).unwrap();
    assert_eq!(named, HashSet::from([d3, h]));
}

#[test]
fn a_word_that_looks_like_a_key_but_is_not_stored_is_not_named() {
    let s = lane();
    let named = binding::named_keys(&s, "see journal:NOPE:0001 and http://example.com and v1.2@deadbeef00").unwrap();
    assert!(named.is_empty(), "{named:?}");
}

#[test]
fn past_six_decisions_the_rest_are_counted_on_one_line() {
    let mut s = MemoryStore::in_memory().unwrap();
    for n in 1..=10 {
        journal(&mut s, n, "decision", &format!("Ruling number {n}"), "Short.", n);
    }
    let b = binding::compile(&s, "a task", LANE).unwrap().unwrap();
    let decisions: Vec<&str> = b.text.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(decisions.len(), 6);
    // Newest first: 10 down to 5.
    assert!(decisions[0].starts_with(&format!("- journal:{LANE}:0010@")), "{}", b.text);
    assert!(decisions[5].starts_with(&format!("- journal:{LANE}:0005@")), "{}", b.text);
    assert_eq!(b.text.lines().last().unwrap(), format!("+4 more decisions: recall lane={LANE}"));
    assert_eq!(b.more, 4);
    assert_eq!(b.keys.len(), 6);
}

#[test]
fn the_block_never_passes_the_budget_and_never_cuts_a_line() {
    let mut s = MemoryStore::in_memory().unwrap();
    let long = "word ".repeat(200);
    put(&mut s, &format!("handoff:{LANE}:001"), "handoff", "", LANE, &draft(&long, &long, 1));
    for n in 1..=12 {
        journal(&mut s, n, "decision", &long, &long, n + 1);
    }
    for n in 20..=25 {
        journal(&mut s, n, "question", &long, &long, n);
    }
    journal(&mut s, 30, "next", &long, &long, 30);
    let b = binding::compile(&s, "a task", LANE).unwrap().unwrap();
    assert!(b.text.len() <= BUDGET, "{} bytes", b.text.len());
    for l in b.text.lines() {
        assert!(l.chars().count() <= LINE_CHARS, "{} chars: {l}", l.chars().count());
    }
    assert!(b.text.ends_with('\n'));
    let last = b.text.lines().last().unwrap();
    assert!(last.starts_with('+') && last.ends_with(&format!(": recall lane={LANE}")), "{last}");
    // Every decision is a line or counted.
    let shown = b.text.lines().filter(|l| l.starts_with("- ")).count();
    let counted: usize = last.trim_start_matches('+').split_whitespace().next().unwrap().parse().unwrap();
    assert!(shown + counted >= 12, "{}", b.text);
    assert!(b.text.lines().filter(|l| l.starts_with("open: ")).count() <= 3);
}

#[test]
fn a_long_line_is_cut_to_the_cap_and_keeps_its_tag() {
    let mut s = MemoryStore::in_memory().unwrap();
    let old = journal(&mut s, 1, "decision", "Old", "Old.", 1);
    let mut w = s.writer().unwrap();
    let long = format!("New ruling {}", "with many words ".repeat(30));
    w.supersede(std::slice::from_ref(&old), &format!("journal:{LANE}:0002"), "journal", "decision", LANE, &draft(&long, &long, 2), None)
        .unwrap();
    w.commit().unwrap();
    let text = compile(&s, "a task");
    let line = text.lines().find(|l| l.starts_with("- ")).unwrap();
    assert!(line.chars().count() <= LINE_CHARS, "{line}");
    assert!(line.contains("...") && line.ends_with(&format!("[replaces {old}]")), "{line}");
}

#[test]
fn the_same_store_brief_and_lane_give_the_same_bytes_whatever_the_insert_order() {
    let build = |rev: bool| {
        let mut s = MemoryStore::in_memory().unwrap();
        let mut ns: Vec<u32> = (1..=8).collect();
        if rev {
            ns.reverse();
        }
        // Two pairs share a time: ties go by key.
        for n in ns {
            journal(&mut s, n, "decision", &format!("Ruling {n}"), &format!("Body {n}."), n.div_ceil(2));
        }
        s
    };
    let (a, b) = (build(false), build(true));
    let ta = compile(&a, "a task");
    assert_eq!(ta, compile(&a, "a task"));
    assert_eq!(ta, compile(&b, "a task"));
    let firsts: Vec<&str> = ta.lines().filter(|l| l.starts_with("- ")).take(2).collect();
    assert!(firsts[0].starts_with(&format!("- journal:{LANE}:0007@")), "{ta}");
    assert!(firsts[1].starts_with(&format!("- journal:{LANE}:0008@")), "{ta}");
}

#[test]
fn a_lane_with_nothing_to_bind_gives_no_block() {
    let mut s = MemoryStore::in_memory().unwrap();
    assert!(binding::compile(&s, "a task", LANE).unwrap().is_none());
    journal(&mut s, 1, "measurement", "A number", "42.", 1);
    assert!(binding::compile(&s, "a task", LANE).unwrap().is_none());
}

/// Reverses whatever it is given.
struct Backwards;
impl Rank for Backwards {
    fn order(&self, _brief: &str, cands: &[Head]) -> Vec<usize> {
        (0..cands.len()).rev().collect()
    }
}

/// Returns something that is not an order of its input.
struct Broken;
impl Rank for Broken {
    fn order(&self, _brief: &str, cands: &[Head]) -> Vec<usize> {
        vec![0; cands.len() + 1]
    }
}

#[test]
fn a_ranker_reorders_the_decisions_in_the_cap_and_a_broken_one_falls_back_to_recency() {
    let mut s = MemoryStore::in_memory().unwrap();
    for n in 1..=8 {
        journal(&mut s, n, "decision", &format!("Ruling {n}"), "Short.", n);
    }
    journal(&mut s, 20, "question", "An open one", "An open one?", 20);
    let lines = |t: &str| t.lines().filter(|l| l.starts_with("- ")).map(|l| l.split('@').next().unwrap().to_string()).collect::<Vec<_>>();
    let recency = binding::compile_with(&s, "a task", LANE, &Recency).unwrap().unwrap();
    let back = binding::compile_with(&s, "a task", LANE, &Backwards).unwrap().unwrap();
    let broken = binding::compile_with(&s, "a task", LANE, &Broken).unwrap().unwrap();
    let mut want = lines(&recency.text);
    assert_eq!(want.first().unwrap(), &format!("- journal:{LANE}:0008"));
    // The same six (the cap is chosen by recency), in reverse.
    want.reverse();
    assert_eq!(lines(&back.text), want);
    assert_eq!(broken.text, recency.text);
    // The question and the overflow line are untouched.
    assert!(back.text.contains("open: ") && back.text.ends_with(&format!("+2 more decisions: recall lane={LANE}\n")), "{}", back.text);
}

fn prefixes(ps: &[&str]) -> HashSet<String> {
    ps.iter().map(|p| p.to_string()).collect()
}

#[test]
fn a_key_written_short_after_a_full_one_is_named_within_its_line() {
    let s = lane();
    let k = |n: u32| format!("journal:{LANE}:{n:04}");
    let brief = format!("Lane: {LANE}\nRulings: {} and 0003 (the rename), not 0099.\n\nPer 0004 the cache prunes.", k(2));
    assert_eq!(binding::named_keys(&s, &brief).unwrap(), HashSet::from([k(2), k(3)]));
    let range = format!("Evidence: {}-0004, then 2026-01-02 and 16:00.", k(2));
    assert_eq!(binding::named_keys(&s, &range).unwrap(), HashSet::from([k(2), k(3), k(4)]));
    // Past MAX_RANGE a range gives its two ends; a number of another width
    // or after no key gives nothing.
    let p = prefixes(&["journal"]);
    assert_eq!(binding::abbreviated("journal:L:0001-0100", &p), vec!["journal:L:0001", "journal:L:0100"]);
    assert_eq!(binding::abbreviated("journal:L:0001 and 12 and 00012", &p), Vec::<String>::new());
    assert_eq!(binding::abbreviated("0002 before journal:L:0001", &p), Vec::<String>::new());
    assert_eq!(binding::abbreviated("journal:L:0001, 0002-0003", &p), vec!["journal:L:0002", "journal:L:0003"]);
}

#[test]
fn feature_tags_are_brief_wide() {
    let p = prefixes(&["journal", "handoff"]);
    let t = binding::tags("Kind: builder\n@journal:L:0001 @handoff:L:002\nFeature: Cache\n  Context: why\n", &p);
    let got: Vec<(&str, Option<&str>)> = t.iter().map(|n| (n.key.as_str(), n.scenario.as_deref())).collect();
    assert_eq!(got, vec![("journal:L:0001", None), ("handoff:L:002", None)]);
}

#[test]
fn scenario_tags_belong_to_their_scenario() {
    let p = prefixes(&["journal"]);
    let brief = "Feature: Cache\n  @journal:L:0003\n  Scenario: Exports rename\n    Then a test shows @journal:L:0004\n  Spike: Hit rate\n    Then a table\n  Rules: gates of X @journal:L:0005\n";
    let got: Vec<(String, Option<String>)> = binding::tags(brief, &p).into_iter().map(|n| (n.key, n.scenario)).collect();
    let sc = |n: &str| Some(n.to_string());
    assert_eq!(
        got,
        vec![
            ("journal:L:0003".into(), sc("Exports rename")),
            ("journal:L:0004".into(), sc("Exports rename")),
            ("journal:L:0005".into(), None),
        ]
    );
}

#[test]
fn a_pinned_tag_keeps_its_hash_written_either_way() {
    let p = prefixes(&["journal"]);
    let t = binding::tags("@journal:L:0001#ABCDEF12 @journal:L:0002@abcdef34\nFeature: x\n", &p);
    let got: Vec<(&str, Option<&str>)> = t.iter().map(|n| (n.key.as_str(), n.hash.as_deref())).collect();
    assert_eq!(got, vec![("journal:L:0001", Some("abcdef12")), ("journal:L:0002", Some("abcdef34"))]);
}

#[test]
fn an_unknown_prefix_or_an_email_is_no_tag() {
    let p = prefixes(&["journal"]);
    let t = binding::tags(
        "@pr:279 @src:src/x.rs @foo:bar\nFeature: x\n  Given mail someone@example.com or @example.com about @journal:L:0001\n",
        &p,
    );
    let keys: Vec<&str> = t.iter().map(|n| n.key.as_str()).collect();
    assert_eq!(keys, vec!["journal:L:0001"]);
}

#[test]
fn a_gherkin_brief_lists_feature_tags_then_each_scenario_under_its_name() {
    let s = lane();
    let k = |n: u32| format!("journal:{LANE}:{n:04}");
    let brief = format!(
        "Kind: builder\nLane: {LANE}\n@{}\nFeature: Cache\n  Context: why\n  @{}\n  Scenario: Exports rename\n    Then a test\n",
        k(2),
        k(3)
    );
    let text = compile(&s, &brief);
    let want = format!(
        "named: {}@{}: Cache bounded at 64\nScenario: Exports rename\nnamed: {}@{}: Exports go through a rename\n",
        k(2),
        sha12(&s, &k(2)),
        k(3),
        sha12(&s, &k(3))
    );
    assert!(text.contains(&want), "{text}");
    // A pin that is not the head's hash is shown.
    let pinned = compile(&s, &format!("Lane: {LANE}\n@{}#00000000\nFeature: f\n", k(2)));
    assert!(pinned.contains("[brief pins 00000000]"), "{pinned}");
}

#[test]
fn another_lane_a_named_key_is_filed_in_is_bound_too() {
    let mut s = lane();
    let o = |n: u32| format!("journal:OTHER-LANE:{n:04}");
    for n in 1..=5 {
        put(&mut s, &o(n), "journal", "decision", "OTHER-LANE", &draft(&format!("Other ruling {n}"), "Short.", 10 + n));
    }
    put(&mut s, "handoff:OTHER-LANE:001", "handoff", "", "OTHER-LANE", &draft("Other lane state", "State.", 20));
    let text = compile(&s, &format!("Lane: {LANE}\nGiven {} for the base.", o(1)));
    let at = text
        .find("lane OTHER-LANE (the brief names it; recall lane=OTHER-LANE for the rest):\nhandoff: handoff:OTHER-LANE:001@")
        .unwrap_or_else(|| panic!("{text}"));
    let tail = &text[at..];
    // The newest three not named, newest first.
    let ds: Vec<&str> = tail.lines().filter(|l| l.starts_with("- ")).map(|l| l.split('@').next().unwrap()).collect();
    assert_eq!(ds, vec![format!("- {}", o(5)), format!("- {}", o(4)), format!("- {}", o(3))]);
    assert!(text.contains(&format!("named: {}@", o(1))), "{text}");
}

#[test]
fn a_brief_with_no_lane_binds_the_keys_it_names() {
    let s = lane();
    let d = format!("journal:{LANE}:0002");
    let b = binding::compile(&s, &format!("Review it against {d}."), "").unwrap().unwrap();
    assert!(b.text.starts_with("atlas binding (recall KEY for full text; these bind this task)\nnamed: "), "{}", b.text);
    assert!(b.text.contains(&format!("named: {d}@")), "{}", b.text);
    // The key's own lane is this one, bound as another lane.
    assert!(b.text.contains(&format!("lane {LANE} (cited in passing")), "{}", b.text);
    assert!(binding::compile(&s, "Review it.", "").unwrap().is_none());
}

/// The lane of [`lane`] with six more decisions (days 10 to 15) and
/// twelve notes (`journal:LANE:0020` to `0031`), every title long.
fn crowded() -> MemoryStore {
    let mut s = lane();
    let long = |what: &str, n: u32| format!("{what} {n} {}", "with a title that fills its line ".repeat(6));
    for n in 10..=15 {
        journal(&mut s, n, "decision", &long("Ruling", n), "Short.", n);
    }
    for n in 20..=31 {
        journal(&mut s, n, "note", &long("Note", n), "Short.", 1);
    }
    s
}

#[test]
fn twelve_long_tags_never_crowd_out_the_lanes_handoff_and_newest_decisions() {
    let s = crowded();
    let k = |n: u32| format!("journal:{LANE}:{n:04}");
    let tags = |ns: std::ops::RangeInclusive<u32>| ns.map(|n| format!("@{}", k(n))).collect::<Vec<_>>().join(" ");
    let brief = format!(
        "Kind: builder\nLane: {LANE}\nFeature: Cache\n  {}\n  Scenario: First of three\n    Then a\n  {}\n  Scenario: Second of three\n    Then b\n  {}\n  Scenario: Third of three\n    Then c\n",
        tags(20..=23),
        tags(24..=27),
        tags(28..=31)
    );
    let b = binding::compile(&s, &brief, LANE).unwrap().unwrap();
    assert!(b.text.len() <= BUDGET, "{} bytes", b.text.len());
    assert!(b.text.contains(&format!("handoff: handoff:{LANE}:001@")), "{}", b.text);
    for n in [15, 14, 13] {
        assert!(b.text.contains(&format!("- {}@", k(n))), "decision {n} missing:\n{}", b.text);
    }
    // The named keys still take the rest, and the ones left are counted.
    assert!(b.text.matches("named: ").count() >= 6, "{}", b.text);
    assert!(b.text.lines().last().unwrap().contains("more named: recall KEY from the brief"), "{}", b.text);
}

#[test]
fn a_tag_line_inside_a_scenario_is_filed_under_it() {
    let p = prefixes(&["journal"]);
    let brief = "Feature: Cache\n  Scenario: Exports rename\n    @journal:L:0004\n    Then a test\n  @journal:L:0005\n  Scenario: Next\n    Then b\n";
    let got: Vec<(String, Option<String>)> = binding::tags(brief, &p).into_iter().map(|n| (n.key, n.scenario)).collect();
    let sc = |n: &str| Some(n.to_string());
    assert_eq!(got, vec![("journal:L:0004".into(), sc("Exports rename")), ("journal:L:0005".into(), sc("Next"))]);
}

#[test]
fn brief_wide_keys_print_before_any_scenario_header() {
    let s = lane();
    let k = |n: u32| format!("journal:{LANE}:{n:04}");
    let brief = format!("Kind: builder\nLane: {LANE}\nFeature: Cache\n  @{}\n  Scenario: A\n    Then a\n  Rules: see {}\n", k(3), k(2));
    let text = compile(&s, &brief);
    let wide = text.find(&format!("named: {}@", k(2))).unwrap_or_else(|| panic!("{text}"));
    let header = text.find("Scenario: A\n").unwrap_or_else(|| panic!("{text}"));
    assert!(wide < header, "a brief-wide key under a scenario:\n{text}");
    assert!(text.contains(&format!("Scenario: A\nnamed: {}@", k(3))), "{text}");
}

#[test]
fn a_tag_in_backticks_or_brackets_is_read() {
    let p = prefixes(&["journal"]);
    let t = binding::tags(
        "Feature: x\n  Given the ruling `@journal:L:0012` and (@journal:L:0013) [@journal:L:0014]\n  `@journal:L:0015`\n  Scenario: S\n    Then mail `someone@example.com`\n",
        &p,
    );
    let got: Vec<(&str, Option<&str>)> = t.iter().map(|n| (n.key.as_str(), n.scenario.as_deref())).collect();
    assert_eq!(got, vec![("journal:L:0012", None), ("journal:L:0013", None), ("journal:L:0014", None), ("journal:L:0015", Some("S"))]);
}

#[test]
fn a_short_number_continues_a_key_only_in_its_sentence_on_its_line() {
    let p = prefixes(&["journal", "handoff"]);
    let none = Vec::<String>::new();
    // A list joined by separators, asides in brackets and backticks kept.
    assert_eq!(
        binding::abbreviated("`journal:L:0001` (the brief), `0002` (notes. more) and 0003.", &p),
        vec!["journal:L:0002", "journal:L:0003"]
    );
    // The next line or the next sentence ends it; a `#` comes before a PR
    // number, never a key.
    assert_eq!(binding::abbreviated("journal:L:0001 holds it.\n0002 is next", &p), none);
    assert_eq!(binding::abbreviated("journal:L:0001 holds it. Then 0002, and 0003", &p), none);
    assert_eq!(binding::abbreviated("journal:L:0001. 0002", &p), none);
    assert_eq!(binding::abbreviated("handoff:L:020 and #279, #021", &p), none);
    // Without a leading zero, a word between ends the list.
    assert_eq!(binding::abbreviated("journal:L:1001, see 1002", &p), none);
}

#[test]
fn a_year_never_continues_a_key() {
    let p = prefixes(&["journal"]);
    assert_eq!(binding::abbreviated("journal:L:0004 and 2026", &p), Vec::<String>::new());
    assert_eq!(binding::abbreviated("journal:L:0004, 1999-2001", &p), Vec::<String>::new());
    assert_eq!(binding::abbreviated("journal:L:1002 and 2026", &p), Vec::<String>::new());
    assert_eq!(binding::abbreviated("journal:L:1002, 1999-2001", &p), Vec::<String>::new());
    assert_eq!(binding::abbreviated("journal:L:0004 and 0005", &p), vec!["journal:L:0005"]);
}

#[test]
fn a_count_after_a_key_never_continues_it() {
    let p = prefixes(&["journal", "handoff"]);
    let none = Vec::<String>::new();
    assert_eq!(binding::abbreviated("handoff:L:020, 176 facts", &p), none);
    assert_eq!(binding::abbreviated("journal:L:0004 and 1679 tokens", &p), none);
    assert_eq!(binding::abbreviated("journal:L:0004 and 3 of 5", &p), none);
    assert_eq!(binding::abbreviated("journal:L:1002 and 1679 tokens", &p), none);
    assert_eq!(binding::abbreviated("handoff:L:120, 176 facts", &p), none);
    assert_eq!(binding::abbreviated("journal:L:0004 and 0005 (the plan)", &p), vec!["journal:L:0005"]);
}

/// Five decisions and a handoff filed in `lane`.
fn with_other_lane(s: &mut MemoryStore, lane: &str) {
    for n in 1..=5 {
        put(s, &format!("journal:{lane}:{n:04}"), "journal", "decision", lane, &draft(&format!("{lane} ruling {n}"), "Short.", 10 + n));
    }
    put(s, &format!("handoff:{lane}:001"), "handoff", "", lane, &draft(&format!("{lane} state"), "State.", 20));
}

fn compile_lane(s: &MemoryStore, brief: &str, lane: &str) -> String {
    binding::compile(s, brief, lane).unwrap().map(|b| b.text).unwrap_or_default()
}

#[test]
fn a_lane_written_in_another_case_is_the_briefs_own_lane() {
    let s = lane();
    let d = format!("journal:{LANE}:0003");
    let lower = LANE.to_lowercase();
    let text = compile_lane(&s, &format!("Lane: {lower}\nGiven {d}.\n"), &lower);
    assert!(text.contains(&format!("named: {d}@")), "{text}");
    assert!(!text.contains(&format!("\nlane {LANE} (")), "its own lane bound as another:\n{text}");
}

#[test]
fn a_lane_cited_only_in_passing_is_context_not_binding() {
    let mut s = lane();
    with_other_lane(&mut s, "OTHER-LANE");
    with_other_lane(&mut s, "THIRD-LANE");
    let brief = format!("Lane: {LANE}\nUnlike journal:OTHER-LANE:0001, the cache bounds.\nGiven journal:THIRD-LANE:0001 for the base.\n");
    let text = compile(&s, &brief);
    assert!(text.contains("lane OTHER-LANE (cited in passing: context, not binding; recall lane=OTHER-LANE for the rest):\n"), "{text}");
    assert!(text.contains("lane THIRD-LANE (the brief names it; recall lane=THIRD-LANE for the rest):\n"), "{text}");
    // A declaring line that names another lane leaves this one in passing.
    let ruled =
        compile(&s, &format!("Lane: {LANE}\nUnlike journal:OTHER-LANE:0001, the cache bounds.\nRulings: recall lane=THIRD-LANE.\n"));
    assert!(ruled.contains("lane OTHER-LANE (cited in passing"), "{ruled}");
    // A tag declares too.
    let tagged = compile(&s, &format!("Lane: {LANE}\n@journal:OTHER-LANE:0001\nFeature: f\n"));
    assert!(tagged.contains("lane OTHER-LANE (the brief names it;"), "{tagged}");
}

#[test]
fn named_keys_past_the_cap_point_to_the_brief_not_a_lane() {
    let mut s = MemoryStore::in_memory().unwrap();
    journal(&mut s, 1, "decision", "A ruling", "Short.", 1);
    let keys: Vec<String> = (2..=15).map(|n| journal(&mut s, n, "note", &format!("Note {n}"), "Short.", 1)).collect();
    let brief = format!("Rules: {}", keys.join(" "));
    for lane in [LANE, ""] {
        let b = binding::compile(&s, &brief, lane).unwrap().unwrap();
        let last = b.text.lines().last().unwrap();
        assert_eq!(last, "+2 more named: recall KEY from the brief", "lane {lane:?}:\n{}", b.text);
    }
}

#[test]
fn the_block_fills_up_to_its_2400_byte_budget() {
    let mut s = MemoryStore::in_memory().unwrap();
    let long = "word ".repeat(200);
    for n in 1..=20 {
        journal(&mut s, n, "decision", &long, &long, n);
    }
    for n in 30..=45 {
        journal(&mut s, n, "question", &long, &long, n);
    }
    put(&mut s, &format!("handoff:{LANE}:001"), "handoff", "", LANE, &draft(&long, &long, 50));
    journal(&mut s, 60, "next", &long, &long, 60);
    let b = binding::compile(&s, "a task", LANE).unwrap().unwrap();
    // A handoff, six decisions, three questions and a next step, full
    // lines, need more than 1600 bytes and fit in 2400.
    assert_eq!(b.text.lines().filter(|l| l.starts_with("- ")).count(), 6, "{}", b.text);
    assert_eq!(b.text.lines().filter(|l| l.starts_with("open: ")).count(), 3, "{}", b.text);
    assert!(b.text.contains("\nhandoff: ") && b.text.contains("\nnext: "), "{}", b.text);
    assert!(b.text.len() > 1600 && b.text.len() <= 2400, "{} bytes", b.text.len());
}

#[test]
fn a_range_of_twelve_keys_expands_and_thirteen_gives_its_ends() {
    let p = prefixes(&["journal"]);
    let twelve: Vec<String> = (1..=12).map(|n| format!("journal:L:{n:04}")).collect();
    assert_eq!(binding::abbreviated("journal:L:0001-0012", &p), twelve);
    assert_eq!(binding::abbreviated("journal:L:0001-0013", &p), vec!["journal:L:0001", "journal:L:0013"]);
}

#[test]
fn at_most_two_other_lanes_are_bound() {
    let mut s = lane();
    for l in ["LANE-A", "LANE-B", "LANE-C"] {
        with_other_lane(&mut s, l);
    }
    let text = compile(&s, &format!("Lane: {LANE}\nGiven journal:LANE-A:0001, journal:LANE-B:0001 and journal:LANE-C:0001.\n"));
    let headers: Vec<&str> = text.lines().filter(|l| l.starts_with("lane ")).map(|l| l.split(' ').nth(1).unwrap()).collect();
    assert_eq!(headers, vec!["LANE-A", "LANE-B"], "{text}");
}

#[test]
fn a_zero_padded_number_after_a_list_word_names_its_key_before_a_plain_word() {
    let p = prefixes(&["journal"]);
    assert_eq!(binding::abbreviated("Rulings: recall journal:L:0507 and 0508 from atlas.", &p), vec!["journal:L:0508"]);
    let s = lane();
    let k = |n: u32| format!("journal:{LANE}:{n:04}");
    let brief = format!("Lane: {LANE}\nRulings: recall {} and 0003 from atlas.\n", k(2));
    assert_eq!(binding::named_keys(&s, &brief).unwrap(), HashSet::from([k(2), k(3)]));
}

#[test]
fn a_zero_padded_number_later_in_the_sentence_names_its_lanes_stored_key() {
    let p = prefixes(&["journal"]);
    assert_eq!(binding::abbreviated("Apply it: `recall journal:L:0035`; the base ruling is 0031.", &p), vec!["journal:L:0031"]);
    assert_eq!(binding::abbreviated("journal:L:0001, see 0002", &p), vec!["journal:L:0002"]);
    // Only a stored key of that lane, and only in that sentence.
    let s = lane();
    let k = |n: u32| format!("journal:{LANE}:{n:04}");
    let brief = format!("Apply `recall {}`; the base ruling is 0002, not 0099. Then 0004.\n", k(3));
    assert_eq!(binding::named_keys(&s, &brief).unwrap(), HashSet::from([k(3), k(2)]));
}

#[test]
fn a_number_without_a_leading_zero_continues_only_a_key_without_one() {
    let p = prefixes(&["journal"]);
    let none = Vec::<String>::new();
    assert_eq!(binding::abbreviated("journal:L:0004, 1600.", &p), none);
    assert_eq!(binding::abbreviated("journal:L:0004 or 3000", &p), none);
    assert_eq!(binding::abbreviated("journal:L:1002 and 1003.", &p), vec!["journal:L:1003"]);
}

#[test]
fn through_between_two_keys_is_a_range() {
    let p = prefixes(&["journal"]);
    assert_eq!(binding::abbreviated("journal:L:0004 through 0006 hold it", &p), vec!["journal:L:0004", "journal:L:0005", "journal:L:0006"]);
    assert_eq!(binding::abbreviated("journal:L:0001 through 0100", &p), vec!["journal:L:0001", "journal:L:0100"]);
    // `to` joins a list; it is no range.
    assert_eq!(binding::abbreviated("journal:L:0004 to 0006", &p), vec!["journal:L:0006"]);
}

#[test]
fn a_tag_under_a_later_heading_is_brief_wide() {
    let p = prefixes(&["journal"]);
    let brief = "Feature: f\n  Scenario: S\n    Then a\n## Gates\n@journal:L:0009\nRun them.\n";
    let got: Vec<(String, Option<String>)> = binding::tags(brief, &p).into_iter().map(|n| (n.key, n.scenario)).collect();
    assert_eq!(got, vec![("journal:L:0009".into(), None)]);
}

#[test]
fn a_lane_written_in_another_case_shows_its_own_handoff_and_rulings() {
    let s = lane();
    let d = format!("journal:{LANE}:0003");
    let lower = LANE.to_lowercase();
    let text = compile_lane(&s, &format!("Lane: {lower}\nGiven {d}.\n"), &lower);
    assert!(text.starts_with(&format!("atlas binding: lane {LANE} (")), "{text}");
    assert!(text.contains(&format!("handoff: handoff:{LANE}:001@")), "{text}");
    assert!(text.contains(&format!("- journal:{LANE}:0002@")), "{text}");
    assert!(text.contains(&format!("open: journal:{LANE}:0004@")), "{text}");
}

#[test]
fn a_lane_on_a_rulings_line_is_binding() {
    let mut s = lane();
    with_other_lane(&mut s, "OTHER-LANE");
    let text = compile(&s, &format!("Lane: {LANE}\nRulings: journal:OTHER-LANE:0001 and 0002 from atlas.\n"));
    assert!(text.contains("lane OTHER-LANE (the brief names it;"), "{text}");
}

#[test]
fn a_lane_under_a_rulings_heading_is_binding() {
    let mut s = lane();
    with_other_lane(&mut s, "OTHER-LANE");
    with_other_lane(&mut s, "THIRD-LANE");
    let brief =
        format!("Lane: {LANE}\n## Rulings stand\n- journal:OTHER-LANE:0001 bounds it.\n## Gates\n- journal:THIRD-LANE:0001 is the base.\n");
    let text = compile(&s, &brief);
    assert!(text.contains("lane OTHER-LANE (the brief names it;"), "{text}");
    assert!(text.contains("lane THIRD-LANE (cited in passing"), "{text}");
}

#[test]
fn a_key_the_brief_says_to_recall_binds_its_lane() {
    let mut s = lane();
    with_other_lane(&mut s, "OTHER-LANE");
    let text = compile(&s, &format!("Lane: {LANE}\nApply the answers: `recall journal:OTHER-LANE:0001`.\n"));
    assert!(text.contains("lane OTHER-LANE (the brief names it;"), "{text}");
}

#[test]
fn a_lane_named_on_a_rulings_line_binds_though_its_key_is_cited_in_prose() {
    let mut s = lane();
    with_other_lane(&mut s, "OTHER-LANE");
    let brief = format!("Lane: {LANE}\nUnlike journal:OTHER-LANE:0001, the cache bounds.\nRulings: follow recall lane=other-lane.\n");
    let text = compile(&s, &brief);
    assert!(text.contains("lane OTHER-LANE (the brief names it;"), "{text}");
}
