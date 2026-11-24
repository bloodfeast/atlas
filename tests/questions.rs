//! Open questions that keep up with themselves (#406): auto-link and nudge
//! on a decision, age in status, the sweep, and defer.

mod common;

use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
use common::*;

fn server(tag: &str) -> (atlas::tools::Server, Fx, Temp) {
    let tmp = Temp::new(tag);
    let settings = Settings::from_config(None, &tmp.path().join("state").join("index.sqlite")).unwrap();
    let fx = Fx::new(tag);
    let mut s = fx.server();
    s.set_memory(settings);
    (s, fx, tmp)
}

fn text(v: Value) -> String {
    v["text"].as_str().unwrap_or_default().to_string()
}

/// Writes a journal entry, returning the reply and the new key.
fn journal(s: &mut atlas::tools::Server, args: Value) -> (String, String) {
    let t = text(call(s, "journal", args));
    let key = t.split_whitespace().nth(1).unwrap_or_default().to_string();
    (t, key)
}

fn status(s: &mut atlas::tools::Server) -> String {
    text(call(s, "journal_status", json!({"all": true})))
}

#[test]
fn a_decision_naming_a_qid_answers_only_that_question_of_its_lane() {
    let (mut s, _fx, _t) = server("q406-link");
    let (_, river3) = journal(&mut s, json!({"lane": "MAPS", "entry": "question", "text": "Q-RIVER-3: ford or bridge at the river?"}));
    let (_, river4) = journal(&mut s, json!({"lane": "MAPS", "entry": "question", "text": "Q-RIVER-4: how wide is the river?"}));
    let (_, other) = journal(&mut s, json!({"lane": "OTHER", "entry": "question", "text": "Q-RIVER-3: the same id in another lane"}));
    // Names Q-RIVER-3 and the other lane's key: only this lane's question closes.
    let (reply, _) = journal(&mut s, json!({"lane": "MAPS/2", "entry": "decision", "text": format!("Q-RIVER-3 settled: ford. See also {other}.")}));
    assert!(reply.contains(&format!("answers {river3}")), "{reply}");
    assert!(!reply.contains(&format!("answers {river4}")) && !reply.contains(&format!("answers {other}")), "{reply}");
    // The nudge lists what is still open in the lane family, with how to close one.
    assert!(reply.contains("open questions in MAPS (1)") && reply.contains(&river4), "{reply}");
    assert!(reply.contains("answers:<key>"), "{reply}");
    let st = status(&mut s);
    assert!(!st.contains(&river3), "{st}");
    assert!(st.contains(&river4) && st.contains(&other), "{st}");
}

#[test]
fn a_decision_naming_a_question_key_answers_it_and_a_note_does_not() {
    let (mut s, _fx, _t) = server("q406-key");
    let (_, q) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Which shape?"}));
    let (note, _) = journal(&mut s, json!({"lane": "L", "entry": "note", "text": format!("thinking about {q}")}));
    assert!(!note.contains("answers"), "{note}");
    assert!(status(&mut s).contains(&q));
    let (reply, _) = journal(&mut s, json!({"lane": "L", "entry": "answered", "text": "Flat.", "refs": [format!("re:{q}")]}));
    assert!(reply.contains(&format!("answers {q}")), "{reply}");
    assert!(!reply.contains("open questions in"), "nothing left to nudge: {reply}");
    assert!(!status(&mut s).contains(&q));
}

#[test]
fn status_lists_each_open_question_with_its_age() {
    let (mut s, _fx, _t) = server("q406-age");
    let (_, q) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Which shape?"}));
    let st = status(&mut s);
    assert!(st.contains("open questions (1, 0 stale past 3d)"), "{st}");
    assert!(st.contains(&format!("{q} 0d \"Which shape?\"")), "{st}");
}

#[test]
fn the_sweep_reports_each_signal_and_closes_nothing() {
    let (mut s, _fx, _t) = server("q406-sweep");
    // A lane with no PRs: a decision sharing the key terms.
    let (_, terms) = journal(&mut s, json!({"lane": "KERNEL", "entry": "question", "text": "Which scheduler quantum for kernel threads?"}));
    journal(&mut s, json!({"lane": "KERNEL", "entry": "decision", "text": "Kernel threads get a 4 ms scheduler quantum"}));
    // A merged PR in the lane after the question.
    let (_, merged) = journal(&mut s, json!({"lane": "BUILD", "entry": "question", "text": "Do we cache the index?"}));
    let (_, commit) = journal(&mut s, json!({"lane": "BUILD", "entry": "commit", "text": "Merged #12 squash at abc1234"}));
    // Nothing points at this one.
    let (_, quiet) = journal(&mut s, json!({"lane": "QUIET", "entry": "question", "text": "Blue or green?"}));
    let out = text(call(&mut s, "question_sweep", json!({})));
    assert!(out.contains(&terms) && out.contains("shares its terms"), "{out}");
    assert!(out.contains(&merged) && out.contains(&format!("commit {commit} records a merged PR")), "{out}");
    assert!(!out.contains(&quiet), "{out}");
    assert!(out.contains(&format!("\"answers:{terms}\"")) && out.contains("entry=answered"), "{out}");
    // The public surface runs the same op.
    let public = text(call(&mut s, "recall", json!({"op": "sweep"})));
    assert_eq!(public, out);
    // Nothing closed.
    let st = status(&mut s);
    assert!(st.contains(&terms) && st.contains(&merged), "{st}");
    // One call closes the chosen ones.
    journal(&mut s, json!({"lane": "KERNEL", "entry": "answered", "text": "sweep", "refs": [format!("answers:{terms}"), format!("answers:{merged}")]}));
    let st = status(&mut s);
    assert!(!st.contains(&terms) && !st.contains(&merged) && st.contains(&quiet), "{st}");
}

#[test]
fn a_defer_parks_a_question_until_answered() {
    let (mut s, _fx, _t) = server("q406-defer");
    let (_, q) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Q-L-1: which road? A) north B) south"}));
    // Refused without an open question or an until line.
    let e = call_err(&mut s, "journal", json!({"lane": "L", "entry": "defer", "text": "later", "meta": "until: next-session"}));
    assert!(e.contains("defers:"), "{e}");
    let e = call_err(&mut s, "journal", json!({"lane": "L", "entry": "defer", "text": "later", "refs": [format!("defers:{q}")]}));
    assert!(e.contains("until"), "{e}");
    let e = call_err(&mut s, "journal", json!({"lane": "L", "entry": "defer", "text": "later", "refs": ["defers:journal:L:9999"], "meta": "until: next-session"}));
    assert!(e.contains("not an open question"), "{e}");
    let (ok, _) = journal(&mut s, json!({"lane": "L", "entry": "defer", "text": "ask in the morning", "refs": [format!("defers:{q}")], "meta": "until: 2999-01-01T09:00\nproceeding_with: B"}));
    assert!(ok.starts_with("ok "), "{ok}");
    let d = text(call(&mut s, "recall", json!({"op": "deferred"})));
    assert!(d.contains("waiting (1)") && d.contains(&q) && d.contains("until 2999-01-01T09:00"), "{d}");
    let st = status(&mut s);
    assert!(st.contains(&format!("{q} 0d deferred until 2999-01-01T09:00")), "{st}");
    // Out of the sweep's idle signal; still open.
    assert!(!text(call(&mut s, "question_sweep", json!({}))).contains(&q));
    // An answer closes it while deferred.
    journal(&mut s, json!({"lane": "L", "entry": "answered", "text": "north", "refs": [format!("answers:{q}")]}));
    assert_eq!(text(call(&mut s, "recall", json!({"op": "deferred"}))), "no deferred question\n");
}

#[test]
fn a_due_defer_lists_its_options_and_a_naming_decision_closes_it() {
    let (mut s, _fx, _t) = server("q406-due");
    let (_, q) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Q-L-2: which road?\nA) north, shorter\nB) south, safer"}));
    journal(&mut s, json!({"lane": "L", "entry": "defer", "text": "overnight", "refs": [format!("defers:{q}")], "meta": "until: 2000-01-01\nproceeding_with: B"}));
    let d = text(call(&mut s, "recall", json!({"op": "deferred"})));
    assert!(d.contains("due (1)") && d.contains("proceeding with B"), "{d}");
    assert!(d.contains("A) north, shorter") && d.contains("B) south, safer"), "{d}");
    let (reply, _) = journal(&mut s, json!({"lane": "L", "entry": "decision", "text": "Q-L-2: south it is"}));
    assert!(reply.contains(&format!("answers {q}")), "{reply}");
    assert_eq!(text(call(&mut s, "recall", json!({"op": "deferred"}))), "no deferred question\n");
}

#[test]
fn a_passing_mention_does_not_answer() {
    let (mut s, _fx, _t) = server("q406-negation");
    let (_, x1) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Q-X-1: ford or bridge?"}));
    let (_, x2) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Q-X-2: how wide?"}));
    let (_, x3) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Which colour?"}));
    // Only the Q-id leading the entry links; the rest of the sentence is a mention.
    let (reply, _) = journal(&mut s, json!({"lane": "L", "entry": "decision", "text": format!("Q-X-1 settled; Q-X-2 still open, and {x3} waits on it")}));
    assert!(reply.contains(&format!("answers {x1}")), "{reply}");
    assert!(!reply.contains(&format!("answers {x2}")) && !reply.contains(&format!("answers {x3}")), "{reply}");
    let st = status(&mut s);
    assert!(!st.contains(&x1) && st.contains(&x2) && st.contains(&x3), "{st}");
    // A bullet leading with a Q-id, and `answers <key>`, link.
    let (reply, _) = journal(&mut s, json!({"lane": "L", "entry": "decision", "text": format!("two settled\n- Q-X-2: ten metres\nthis answers {x3}: blue")}));
    assert!(reply.contains(&format!("answers {x2}")) && reply.contains(&format!("answers {x3}")), "{reply}");
}

#[test]
fn a_follow_up_titled_with_another_qid_keeps_its_own() {
    let (mut s, _fx, _t) = server("q406-followup");
    let (_, x1) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Q-X-1: ford or bridge?"}));
    let (_, x2) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Q-X-2: follow-up to Q-X-1, which bank?"}));
    let (reply, _) = journal(&mut s, json!({"lane": "L", "entry": "decision", "text": "Q-X-1: ford"}));
    assert!(reply.contains(&format!("answers {x1}")) && !reply.contains(&format!("answers {x2}")), "{reply}");
    assert!(status(&mut s).contains(&x2));
}

#[test]
fn a_reopen_undoes_a_false_close() {
    let (mut s, _fx, _t) = server("q406-reopen");
    let (_, q) = journal(&mut s, json!({"lane": "L", "entry": "question", "text": "Q-X-1: ford or bridge?"}));
    let e = call_err(&mut s, "journal", json!({"lane": "L", "entry": "reopen", "text": "x", "refs": [format!("reopens:{q}")]}));
    assert!(e.contains("already open"), "{e}");
    let e = call_err(&mut s, "journal", json!({"lane": "L", "entry": "reopen", "text": "x"}));
    assert!(e.contains("reopens:"), "{e}");
    let (reply, _) = journal(&mut s, json!({"lane": "L", "entry": "decision", "text": "Q-X-1 goes with the river plan"}));
    assert!(reply.contains(&format!("answers {q}")) && reply.contains("entry=reopen"), "{reply}");
    assert!(!status(&mut s).contains(&q));
    let (ok, _) = journal(&mut s, json!({"lane": "L", "entry": "reopen", "text": "closed by mistake", "refs": [format!("reopens:{q}")]}));
    assert!(ok.starts_with("ok "), "{ok}");
    assert!(status(&mut s).contains(&q));
    let r = text(call(&mut s, "recall", json!({"op": "status"})));
    assert!(r.contains(&q), "{r}");
    // A later answer closes it again.
    journal(&mut s, json!({"lane": "L", "entry": "decision", "text": "Q-X-1: bridge"}));
    assert!(!status(&mut s).contains(&q));
}
