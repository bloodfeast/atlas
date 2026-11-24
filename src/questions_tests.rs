use super::*;
use crate::memory::{Draft, MemoryStore};
use crate::orchestrate::OrchSettings;
use std::path::PathBuf;

const NOW: &str = "2026-11-22T18:00:00Z";

fn store_in(name: &str) -> (PathBuf, MemoryStore) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("atlas-q406-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let s = MemoryStore::open(&d.join("memory.sqlite")).unwrap();
    (d, s)
}

/// Writes a journal record at `time` with `title`, `meta` and `refs`.
#[allow(clippy::too_many_arguments)]
fn add(s: &mut MemoryStore, key: &str, sub: &str, lane: &str, time: &str, title: &str, meta: &str, refs: &[(&str, &str)]) {
    let d = Draft { title: title.into(), meta: meta.into(), reason: "t".into(), author: "t".into(), client: "t".into(), time: Some(time.into()), ..Default::default() };
    let mut w = s.writer().unwrap();
    w.create(key, "journal", sub, lane, &d, None).unwrap();
    let mut all: Vec<(String, String)> = refs.iter().map(|(k, t)| ((*k).to_string(), (*t).to_string())).collect();
    all.push(("lane".into(), lane.into()));
    w.add_refs(key, &all).unwrap();
    w.commit().unwrap();
}

fn card(s: &MemoryStore) -> String {
    crate::resume::card(s, &OrchSettings::default(), "", NOW, false).unwrap().unwrap_or_default()
}

fn q_line(card: &str) -> String {
    card.lines().find(|l| l.starts_with("- open questions")).unwrap_or("").to_string()
}

#[test]
fn a_due_question_is_cut_after_the_lower_value_lines() {
    let (d, mut s) = store_in("due-budget");
    add(&mut s, "journal:L:0001", "question", "L", "2026-11-22T10:00:00Z", "Q-L-1: which road? A) north B) south", "", &[]);
    add(&mut s, "journal:L:0002", "defer", "L", "2026-11-22T11:00:00Z", "later", "until: 2026-11-22T12:00", &[("defers", "journal:L:0001")]);
    for i in 0..10 {
        let lane = format!("H{i}");
        let key = format!("handoff:{lane}:001");
        let dr = Draft { title: format!("handoff {i} with a title long enough to fill the line"), reason: "t".into(), author: "t".into(), client: "t".into(), time: Some(NOW.into()), ..Default::default() };
        let mut w = s.writer().unwrap();
        w.create(&key, "handoff", "", &lane, &dr, None).unwrap();
        w.add_refs(&key, &[("lane".into(), lane.clone())]).unwrap();
        w.commit().unwrap();
    }
    let full = crate::resume::card_within(&s, &OrchSettings::default(), "", NOW, false, 10_000).unwrap().unwrap();
    let due = full.lines().find(|l| l.starts_with("- due questions")).expect("due line").to_string();
    assert!(full.contains("- other handoffs"), "{full}");
    // Whatever the budget, a later line is placed only once the due line is whole.
    let mut cut_later = false;
    for budget in (400..=crate::resume::CARD_BYTES).step_by(15) {
        let c = crate::resume::card_within(&s, &OrchSettings::default(), "", NOW, false, budget).unwrap().unwrap();
        let later = c.lines().find(|l| l.starts_with("- other handoffs")).unwrap_or("");
        if !later.is_empty() {
            assert!(c.lines().any(|l| l == due), "budget {budget}: {c}");
        }
        cut_later |= c.lines().any(|l| l == due) && full.lines().find(|l| l.starts_with("- other handoffs")) != Some(later);
    }
    assert!(cut_later, "some budget keeps the due line whole and cuts the handoffs");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_qid_answers_only_where_it_leads() {
    assert_eq!(lead_qid("  - **Q-A-1**: north").as_deref(), Some("Q-A-1"));
    assert_eq!(lead_qid("2) Q-A-1 north").as_deref(), Some("Q-A-1"));
    assert_eq!(lead_qid("(Q-A-1) north").as_deref(), Some("Q-A-1"));
    assert_eq!(lead_qid("Q-A-12, north").as_deref(), Some("Q-A-12"));
    assert!(lead_qid("settled Q-A-1").is_none() && lead_qid("FAQ-1").is_none() && lead_qid("-Q-A-1").is_none());
    let q = |title: &str, body: &str| Question { lane: "L".into(), key: "journal:L:0001".into(), title: title.into(), body: body.into(), created: NOW.into(), defer: None };
    assert_eq!(q("Q-A-2: follow-up to Q-A-1", "").qid().as_deref(), Some("Q-A-2"));
    assert_eq!(q("which road?", "\nQ-A-3: north or south").qid().as_deref(), Some("Q-A-3"));
    assert!(q("follow-up to Q-A-1", "").qid().is_none());
    let a = q("Q-A-1: which road?", "");
    assert!(a.answered_in("Q-A-1: north") && a.answered_in("done\n- Q-A-1 north"));
    assert!(a.answered_in("this answers journal:L:0001: north") && a.answered_in("answers: Q-A-1"));
    assert!(!a.answered_in("Q-A-2 settled; Q-A-1 still open") && !a.answered_in("see journal:L:0001"));
    assert!(!a.answered_in("answers journal:L:00012") && !a.answered_in("reanswers journal:L:0001"));
}

#[test]
fn qids_are_q_tokens_with_a_digit() {
    assert_eq!(qids("Q-RIVER-3: and Q-CE-5, not Q-ABC or Q-; again Q-RIVER-3."), ["Q-RIVER-3", "Q-CE-5"]);
    assert!(qids("FAQ-1 q-2").is_empty());
}

#[test]
fn a_key_is_named_whole() {
    assert!(names_key("see journal:L:0001.", "journal:L:0001"));
    assert!(!names_key("journal:L:00012", "journal:L:0001"));
    assert!(!names_key("subjournal:L:0001", "journal:L:0001"));
    assert!(!names_key("journal:LL:0001", "journal:L:0001"));
}

#[test]
fn options_read_inline_and_by_line() {
    assert_eq!(options("Which? A) north B) south, safer"), ["north", "south, safer"]);
    assert_eq!(options("Which?\nA) north.\nB) south\nnotes"), ["north", "south"]);
    assert!(options("no labels here").is_empty());
}

#[test]
fn links_keep_to_the_lane_family() {
    let q = |lane: &str, key: &str, title: &str| Question { lane: lane.into(), key: key.into(), title: title.into(), body: String::new(), created: NOW.into(), defer: None };
    let open = [q("L", "journal:L:0001", "Q-A-1: one"), q("L/2", "journal:L/2:0001", "Q-A-2: two"), q("M", "journal:M:0001", "Q-A-1: other lane")];
    assert_eq!(links(&open, "L", "Q-A-1 settled\n* **Q-A-2**: settled", &[]), ["journal:L:0001", "journal:L/2:0001"]);
    assert_eq!(links(&open, "L", "Q-A-1 and Q-A-2 settled", &[]), ["journal:L:0001"]);
    assert_eq!(links(&open, "L", "nothing", &[("re".into(), "journal:M:0001".into())]), Vec::<String>::new());
    // A question already answered by ref is not linked twice.
    assert!(links(&open, "L", "Q-A-1", &[("answers".into(), "journal:L:0001".into())]).is_empty());
}

#[test]
fn a_fresh_question_leaves_the_card_line_as_it_was() {
    let (d, mut s) = store_in("fresh");
    add(&mut s, "journal:L:0001", "question", "L", NOW, "Which shape?", "", &[]);
    assert_eq!(q_line(&card(&s)), "- open questions for the user: journal:L:0001");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_stale_question_carries_its_age_on_the_card() {
    let (d, mut s) = store_in("stale");
    add(&mut s, "journal:L:0001", "question", "L", "2026-11-17T18:00:00Z", "Which shape?", "", &[]);
    add(&mut s, "journal:L:0002", "question", "L", "2026-11-21T18:00:00Z", "Which colour?", "", &[]);
    add(&mut s, "journal:L:0003", "note", "L", NOW, "still here", "", &[]);
    let c = card(&s);
    assert_eq!(q_line(&c), "- open questions for the user: journal:L:0002, journal:L:0001 (5d, stale)", "{c}");
    let open = open(&s).unwrap();
    assert!(open[1].stale(NOW, None, STALE_DAYS) && !open[0].stale(NOW, None, STALE_DAYS));
    assert!(!open[1].stale(NOW, None, 7), "the stale age is configurable");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_deferred_question_leaves_the_stale_count_and_comes_back_when_due() {
    let (d, mut s) = store_in("defer");
    add(&mut s, "journal:L:0001", "question", "L", "2026-11-10T18:00:00Z", "Q-L-1: which road? A) north B) south", "", &[]);
    add(&mut s, "journal:L:0002", "defer", "L", "2026-11-21T18:00:00Z", "overnight", "until: 2026-11-23T09:00\nproceeding_with: B", &[("defers", "journal:L:0001")]);
    add(&mut s, "journal:L:0003", "note", "L", NOW, "working on B", "", &[]);
    let qs = open(&s).unwrap();
    assert_eq!(qs[0].state(NOW, None), State::Deferred);
    assert!(!qs[0].stale(NOW, None, STALE_DAYS));
    let c = card(&s);
    assert_eq!(q_line(&c), "- open questions for the user: +1 deferred (recall op=deferred)", "{c}");
    assert!(!c.contains("- due questions"), "{c}");
    // The next morning it is due, with its options.
    let later = "2026-11-23T10:00:00Z";
    assert_eq!(qs[0].state(later, None), State::Due);
    let due = due_line(&qs, later, None, 3);
    assert_eq!(due, "- due questions, ask as multiple choice (recall op=deferred): journal:L:0001 \"Q-L-1: which road? A) north B) south\" [north | south] proceeding with B");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn next_session_comes_due_on_a_main_session_start_only() {
    let (d, mut s) = store_in("next");
    add(&mut s, "journal:L:0001", "question", "L", "2026-11-21T18:00:00Z", "Which road?", "", &[]);
    add(&mut s, "journal:L:0002", "defer", "L", "2026-11-21T19:00:00Z", "overnight", "until: next-session", &[("defers", "journal:L:0001")]);
    let qs = open(&s).unwrap();
    assert_eq!(qs[0].state(NOW, last_start(&s).unwrap()), State::Deferred);
    // The deferring session's own start is before the defer.
    let at = |t: &str| std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(crate::checkpoint::epoch_secs(t).unwrap()).unwrap());
    crate::orchestrate::record_session_start(&s, "s1", at("2026-11-21T17:00:00Z")).unwrap();
    assert_eq!(qs[0].state(NOW, last_start(&s).unwrap()), State::Deferred);
    // A subagent records no session start, so nothing changes; the owner's next session does.
    crate::orchestrate::record_session_start(&s, "s2", at("2026-11-22T08:00:00Z")).unwrap();
    assert_eq!(qs[0].state(NOW, last_start(&s).unwrap()), State::Due);
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_card_counts_settled_questions_and_lists_due_ones() {
    let (d, mut s) = store_in("settled");
    add(&mut s, "journal:L:0001", "question", "L", "2026-11-22T08:00:00Z", "Which exporter format for widgets?", "", &[]);
    add(&mut s, "journal:L:0002", "decision", "L", "2026-11-22T09:00:00Z", "Widgets: the exporter format is JSON lines", "", &[]);
    add(&mut s, "journal:L:0003", "question", "L", "2026-11-22T10:00:00Z", "Q-L-3: A) one B) two", "", &[]);
    add(&mut s, "journal:L:0004", "defer", "L", "2026-11-22T11:00:00Z", "later", "until: 2026-11-22T12:00", &[("defers", "journal:L:0003")]);
    let c = card(&s);
    assert!(c.contains("- 1 question looks settled: recall op=sweep"), "{c}");
    assert!(c.contains("- due questions, ask as multiple choice (recall op=deferred): journal:L:0003"), "{c}");
    assert_eq!(q_line(&c), "- open questions for the user: journal:L:0001", "{c}");
    drop(s);
    let _ = std::fs::remove_dir_all(&d);
}
