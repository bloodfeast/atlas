//! The Binding compiler: the few lines of a lane a launched agent is bound
//! by. Selection order, supersession followed, named keys skipped, the byte
//! budget and its `+N more` line, and byte stability. All lanes and text here
//! are invented.

use std::collections::HashSet;

use atlas::binding::{self, Rank, Recency, BUDGET, LINE_CHARS};
use atlas::memory::{Draft, Head, MemoryStore};

const LANE: &str = "WIDGET-CACHE";

fn at(day: u32) -> String {
    format!("2026-01-{day:02}T10:00:00.000Z")
}

fn draft(title: &str, body: &str, day: u32) -> Draft {
    Draft { title: title.into(), body: body.into(), reason: "testing the binding".into(), author: "test".into(), client: "test".into(), time: Some(at(day)), ..Draft::default() }
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
    put(&mut s, &format!("handoff:{LANE}:001"), "handoff", "", LANE, &draft("Cache half done, eviction left", "State: the cache is half done.", 5));
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
    w.supersede(std::slice::from_ref(&old), "journal:OTHER-LANE:0001", "journal", "decision", "OTHER-LANE", &draft("Cache grows freely", "The cache grows freely now.", 9), None).unwrap();
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
    w.supersede(std::slice::from_ref(&old), "journal:OTHER-LANE:0001", "journal", "decision", "OTHER-LANE", &draft("Middle", "Middle.", 9), None).unwrap();
    w.supersede(&["journal:OTHER-LANE:0001".into()], "journal:OTHER-LANE:0002", "journal", "decision", "OTHER-LANE", &draft("Last", "Last.", 10), None).unwrap();
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
    assert!(!text.contains(&d3), "{text}");
    assert!(!text.contains(&h), "{text}");
    assert!(text.contains(&format!("journal:{LANE}:0002@")), "{text}");
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
    w.supersede(std::slice::from_ref(&old), &format!("journal:{LANE}:0002"), "journal", "decision", LANE, &draft(&long, &long, 2), None).unwrap();
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
