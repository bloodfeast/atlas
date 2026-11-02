//! Agent discussion threads: immutability, status, the conclude gate,
//! unresolved threads and decisions, citations and concurrent posting.

mod common;

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use common::*;
use atlas::json::{Value};
use atlas::json;
use atlas::tools::Server;

fn open(s: &mut Server, vault: &str, topic: &str) -> String {
    let r = call(
        s,
        "open_discussion",
        json!({
            "vault": vault, "topic": topic,
            "subjects": [{"vault": "perf", "path": "Ideas/PI-002 Throttle.md"}],
            "participants": ["bob", "alice"],
            "opening_post": {"author": "bob", "kind": "claim", "body": "The throttle saves 0.1 ms at the 4K peak."}
        }),
    );
    r["thread"].as_str().unwrap().to_string()
}

fn post(s: &mut Server, thread: &str, author: &str, kind: &str, re: Option<u32>) -> u32 {
    let mut a = json!({"thread": thread, "author": author, "kind": kind, "body": format!("{kind} from {author}, long enough to count")});
    if let Some(r) = re {
        a["re"] = json!(r);
    }
    call(s, "post", a)["post"].as_u64().unwrap() as u32
}

fn thread_path(fx: &Fx, s: &mut Server, thread: &str) -> std::path::PathBuf {
    let r = call(s, "read_discussion", json!({"thread": thread}));
    let rel = r["path"].as_str().unwrap().to_string();
    let vault = thread.split(':').next().unwrap();
    let root = fx.specs.iter().find(|(n, _)| n == vault).unwrap().1.clone();
    root.join(rel)
}

#[test]
fn open_post_read_and_since_post() {
    let fx = Fx::new("d-basic");
    let mut s = fx.server();
    let t = open(&mut s, "perf", "Throttle: is the saving real?");
    assert_eq!(t, "perf:D-001");
    let p = thread_path(&fx, &mut s, &t);
    assert!(p.ends_with("Discussions/D-001 Throttle is the saving real.md"), "{}", p.display());
    assert_eq!(post(&mut s, &t, "alice", "challenge", Some(1)), 2);
    assert_eq!(post(&mut s, &t, "bob", "answer", Some(2)), 3);
    let r = call(&mut s, "read_discussion", json!({"thread": t, "since_post": 1}));
    let ns: Vec<u64> = r["posts"].as_array().unwrap().iter().map(|p| p["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![2, 3]);
    assert_eq!(r["posts"][0]["re"], json!(1));
    assert_eq!(r["posts"][0]["kind"], json!("challenge"));
    assert_eq!(r["integrity"], json!("ok"));
    assert_eq!(r["post_count"], json!(3));
    assert!(call(&mut s, "read_discussion", json!({"thread": t, "since_post": 3}))["posts"].as_array().unwrap().is_empty());
    // A reply kind without re is refused, as is re to a post that does not exist yet.
    let e = call_err(&mut s, "post", json!({"thread": t, "author": "x", "kind": "answer", "body": "an answer to nothing at all"}));
    assert!(e.contains("give re"), "{e}");
    let e = call_err(&mut s, "post", json!({"thread": t, "author": "x", "kind": "answer", "re": 9, "body": "an answer to the future"}));
    assert!(e.contains("not an earlier post"), "{e}");
    let e = call_err(&mut s, "post", json!({"thread": t, "author": "x", "kind": "claim", "body": "short"}));
    assert!(e.contains("12 characters"), "{e}");
    let e =
        call_err(&mut s, "post", json!({"thread": t, "author": "x", "kind": "claim", "body": "## a heading that would\nsplit the thread"}));
    assert!(e.contains("headings"), "{e}");
    // The subject's backlinks pick the thread up, and the audit row names the client and the thread.
    let b = call(&mut s, "backlinks", json!({"vault": "perf", "path": "Ideas/PI-002 Throttle.md"}));
    assert!(b["links"].as_array().unwrap().iter().any(|l| l["from_path"].as_str().unwrap().starts_with("Discussions/D-001")));
    let log = call(&mut s, "write_log", json!({"discussion": t}));
    let e = log["entries"].as_array().unwrap();
    assert_eq!(e.len(), 3);
    assert_eq!(e[0]["client"], json!("test-client"));
    assert!(e[0]["reason"].as_str().unwrap().starts_with("answer by bob: "));
    assert_eq!(e[2]["tool"], json!("open_discussion"));
    // Participants grow with new authors.
    post(&mut s, &t, "carol", "question", Some(1));
    let r = call(&mut s, "read_discussion", json!({"thread": t}));
    assert_eq!(r["frontmatter"]["participants"], json!(["bob", "alice", "carol"]));
}

#[test]
fn hand_edits_to_earlier_posts_are_caught_and_named() {
    let fx = Fx::new("d-immutable");
    let mut s = fx.server();
    let t = open(&mut s, "perf", "Immutability");
    post(&mut s, &t, "alice", "challenge", Some(1));
    post(&mut s, &t, "bob", "answer", Some(2));
    let p = thread_path(&fx, &mut s, &t);
    let good = fs::read_to_string(&p).unwrap();
    // Edit P-2's body by hand.
    fs::write(&p, good.replace("challenge from alice", "challenge from ALICE")).unwrap();
    let e = call_err(&mut s, "post", json!({"thread": t, "author": "bob", "kind": "claim", "body": "another claim long enough"}));
    assert!(e.contains("P-2 (alice - challenge) was edited by hand"), "{e}");
    let r = call(&mut s, "read_discussion", json!({"thread": t}));
    assert!(r["integrity"].as_str().unwrap().contains("P-2"));
    // Remove the last post by hand.
    let cut = good.find("\n### P-3").unwrap();
    fs::write(&p, &good[..cut + 1]).unwrap();
    let e = call_err(&mut s, "post", json!({"thread": t, "author": "bob", "kind": "claim", "body": "another claim long enough"}));
    assert!(e.contains("frontmatter records 3"), "{e}");
    // Text slipped in after the last post becomes part of it and is caught.
    fs::write(&p, format!("{good}\nan unsigned addendum\n")).unwrap();
    let e = call_err(&mut s, "post", json!({"thread": t, "author": "bob", "kind": "claim", "body": "another claim long enough"}));
    assert!(e.contains("P-3"), "{e}");
    // Restored, the thread accepts posts again, and a correction is a retract.
    fs::write(&p, &good).unwrap();
    assert_eq!(post(&mut s, &t, "bob", "retract", Some(3)), 4);
    // Frontmatter edits by hand (a tag, a status) do not break the chain.
    call(&mut s, "set_frontmatter", json!({"vault": "perf", "path": r["path"], "values": {"tags": ["discussion", "perf"]}}));
    assert_eq!(post(&mut s, &t, "alice", "claim", None), 5);
}

#[test]
fn status_is_derived_from_re_chains() {
    let fx = Fx::new("d-status");
    let mut s = fx.server();
    let t = open(&mut s, "perf", "Status derivation");
    post(&mut s, &t, "alice", "challenge", Some(1)); // 2
    post(&mut s, &t, "bob", "answer", Some(2)); // 3
    post(&mut s, &t, "alice", "challenge", Some(3)); // 4: challenges the answer
    post(&mut s, &t, "bob", "claim", None); // 5
    post(&mut s, &t, "alice", "challenge", Some(5)); // 6
    post(&mut s, &t, "bob", "concede", Some(6)); // 7
    post(&mut s, &t, "alice", "question", Some(5)); // 8
    let r = call(&mut s, "discussion_status", json!({"thread": t}));
    let states: Vec<(u64, String)> =
        r["points"].as_array().unwrap().iter().map(|p| (p["n"].as_u64().unwrap(), p["state"].as_str().unwrap().to_string())).collect();
    assert_eq!(
        states,
        vec![
            (1, "contested".into()),
            (2, "contested".into()),
            (4, "contested".into()),
            (5, "conceded".into()),
            (6, "conceded".into()),
            (8, "open".into())
        ]
    );
    assert_eq!(r["contested_challenges"], json!([2, 4]));
    post(&mut s, &t, "bob", "answer", Some(4)); // 9 answers the challenge to the answer
    let r = call(&mut s, "discussion_status", json!({"thread": t}));
    assert_eq!(r["contested_challenges"], json!([]));
    assert_eq!(r["points"][0]["state"], json!("answered"));
    // awaiting: alice's question 8 is unanswered by bob.
    let w = call(&mut s, "awaiting", json!({"author": "bob"}));
    let ns: Vec<u64> = w["awaiting"].as_array().unwrap().iter().map(|p| p["post"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![8]);
    assert!(call(&mut s, "awaiting", json!({"author": "alice"}))["awaiting"].as_array().unwrap().is_empty());
}

#[test]
fn conclude_gate_and_verdict_link() {
    let fx = Fx::new("d-conclude");
    let mut s = fx.server();
    let t = open(&mut s, "perf", "Conclude gate");
    post(&mut s, &t, "alice", "challenge", Some(1));
    let e = call_err(&mut s, "conclude_discussion", json!({"thread": t, "verdict_body": "The throttle saving stands as measured."}));
    assert!(e.contains("P-2 (challenge by alice to P-1)") && e.contains("force_reason") && e.contains("mark_unresolved"), "{e}");
    let e =
        call_err(&mut s, "conclude_discussion", json!({"thread": t, "verdict_body": "The throttle saving stands.", "force_reason": "n/a"}));
    assert!(e.contains("force_reason"), "{e}");
    let r = call(
        &mut s,
        "conclude_discussion",
        json!({"thread": t, "verdict_body": "The throttle saving stands as measured.", "force_reason": "the retake is queued and will settle it"}),
    );
    assert_eq!(r["post"], json!(3));
    assert_eq!(r["still_contested"][0]["n"], json!(2));
    let link = r["verdict_link"].as_str().unwrap();
    assert_eq!(link, "[[D-001 Conclude gate#P-3 orchestrator - verdict|D-001 P-3]]");
    let rd = call(&mut s, "read_discussion", json!({"thread": t}));
    assert_eq!(rd["frontmatter"]["status"], json!("concluded"));
    let verdict = rd["posts"][2]["body"].as_str().unwrap();
    assert!(verdict.contains("**Still contested:** P-2") && verdict.contains("the retake is queued"), "{verdict}");
    let e = call_err(&mut s, "post", json!({"thread": t, "author": "alice", "kind": "claim", "body": "one more thing to say here"}));
    assert!(e.contains("concluded"), "{e}");

    // An uncontested thread concludes without force; the verdict link resolves to the thread.
    let t2 = open(&mut s, "perf", "Clean conclude");
    post(&mut s, &t2, "alice", "challenge", Some(1));
    post(&mut s, &t2, "bob", "answer", Some(2));
    let r = call(&mut s, "conclude_discussion", json!({"thread": t2, "verdict_body": "Agreed: keep the throttle as is."}));
    let link = r["verdict_link"].as_str().unwrap().to_string();
    call(&mut s, "create_note", json!({"vault": "perf", "path": "Finding.md", "content": format!("Settled in {link}.\n")}));
    let out = call(&mut s, "outlinks", json!({"vault": "perf", "path": "Finding.md"}));
    assert_eq!(out["links"][0]["to_path"], json!("Discussions/D-002 Clean conclude.md"));
    assert_eq!(out["links"][0]["heading"], json!("P-4 orchestrator - verdict"));
}

#[test]
fn writes_cite_a_discussion_and_threads_show_their_outcomes() {
    let fx = Fx::new("d-cite");
    let mut s = fx.server();
    let t = open(&mut s, "perf", "Park the throttle");
    let p = "Ideas/PI-002 Throttle.md";
    // A short reason that points at the thread is enough.
    call(
        &mut s,
        "set_frontmatter",
        json!({"vault": "perf", "path": p, "values": {"status": "queued 139"}, "reason": "per perf:D-001", "discussion": "perf:D-001"}),
    );
    call(
        &mut s,
        "append_history",
        json!({"vault": "perf", "path": p, "text": "parked (D-001)", "reason": "verdict of D-001", "discussion": t}),
    );
    let e = call_err(
        &mut s,
        "append_to_note",
        json!({"vault": "perf", "path": p, "text": "x", "reason": "per perf:D-404", "discussion": "perf:D-404"}),
    );
    assert!(e.contains("not found"), "{e}");
    let e = call_err(&mut s, "append_to_note", json!({"vault": "perf", "path": p, "text": "x", "reason": "D-001", "discussion": t}));
    assert!(e.contains("reason"), "{e}");
    // In the database, the jsonl, write_log and the thread's outcomes.
    let canon = "perf:Discussions/D-001 Park the throttle.md";
    let n: i64 = s
        .store_mut()
        .conn()
        .query_row("SELECT COUNT(*) FROM writes WHERE discussion = ?1 AND path = ?2", [canon, p], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
    let log = fs::read_to_string(fx.db.with_file_name(atlas::store::AUDIT_LOG)).unwrap();
    assert_eq!(log.lines().filter(|l| l.contains(&format!("\"discussion\":\"{canon}\"")) && l.contains("PI-002")).count(), 2);
    let w = call(&mut s, "write_log", json!({"discussion": t, "vault": "perf", "path": p}));
    assert_eq!(w["total"], json!(2));
    let r = call(&mut s, "read_discussion", json!({"thread": t}));
    let outcomes: Vec<(&str, &str)> =
        r["outcomes"].as_array().unwrap().iter().map(|o| (o["tool"].as_str().unwrap(), o["path"].as_str().unwrap())).collect();
    assert_eq!(outcomes, vec![("append_history", p), ("set_frontmatter", p)]);
}

#[test]
fn unresolved_threads_queue_reopen_and_decide() {
    let fx = Fx::new("d-unresolved");
    let mut s = fx.server();
    let a = open(&mut s, "perf", "First disagreement");
    post(&mut s, &a, "alice", "challenge", Some(1));
    let b = open(&mut s, "campaign", "Second disagreement");
    post(&mut s, &b, "alice", "challenge", Some(1));
    let unresolved = |s: &mut Server, t: &str, summary: &str| {
        call(
            s,
            "mark_unresolved",
            json!({
                "thread": t, "summary": summary, "open_questions": ["Is the 0.1 ms inside the noise?", "Does it hold at 1080p?"],
                "positions": [
                    {"participant": "bob", "position": "The saving is real at the 4K peak.", "post": 1},
                    {"participant": "alice", "position": "It is noise until a block-alternated retake.", "post": 2}
                ],
                "settle_by": "An ABBA retake at the 4K peak with 1,024 frames per arm.",
                "reason": "reviewers disagree after two rounds"
            }),
        )
    };
    // The gate's alternative to force_reason.
    assert!(call_err(&mut s, "conclude_discussion", json!({"thread": a, "verdict_body": "no agreement reached here"}))
        .contains("mark_unresolved"));
    let e = call_err(
        &mut s,
        "mark_unresolved",
        json!({"thread": a, "summary": "a summary long enough", "open_questions": ["what now?"], "settle_by": "a retake at 4K",
               "positions": [{"participant": "bob", "position": "the saving is real", "post": 1}], "reason": "reviewers disagree"}),
    );
    assert!(e.contains("alice"), "{e}");
    let r = unresolved(&mut s, &a, "Whether the throttle saving is real.");
    assert_eq!(r["post"], json!(3));
    unresolved(&mut s, &b, "Whether the mission timing holds.");
    let rd = call(&mut s, "read_discussion", json!({"thread": a}));
    assert_eq!(rd["frontmatter"]["status"], json!("unresolved"));
    assert_eq!(rd["frontmatter"]["needs_decision"], json!(true));
    let body = rd["posts"][2]["body"].as_str().unwrap();
    assert!(body.contains("- **alice:** It is noise until a block-alternated retake. (strongest: [[D-001 First disagreement#P-2 alice - challenge|P-2]])"), "{body}");
    assert!(body.contains("**What would settle it.** An ABBA retake"));

    // The queue spans vaults, oldest first.
    let q = call(&mut s, "decision_queue", json!({}));
    let threads: Vec<&str> = q["queue"].as_array().unwrap().iter().map(|e| e["thread"].as_str().unwrap()).collect();
    assert_eq!(threads, vec!["perf:D-001", "campaign:D-001"]);
    assert_eq!(q["queue"][0]["open_questions"], json!(2));
    assert_eq!(q["queue"][0]["summary"], json!("Whether the throttle saving is real."));
    let l = call(&mut s, "list_discussions", json!({"needs_decision": true}));
    assert_eq!(l["discussions"].as_array().unwrap().len(), 2);
    // The queue as an index note.
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": "Decisions.md", "name": "queue", "reason": "decision queue for Dana",
        "spec": {"sources": [], "rows": "decision_queue", "columns": ["thread", "link", "decision_summary", "open_questions"]}}),
    );
    let idx = fs::read_to_string(fx.perf().join("Decisions.md")).unwrap();
    let pa = idx.find("| perf:D-001 |").unwrap();
    let pb = idx.find("| campaign:D-001 |").unwrap();
    assert!(pa < pb, "{idx}");
    assert!(idx.contains("obsidian://open?vault=Field%20Notes&file=Discussions%2FD-001%20Second%20disagreement"));

    // Reopen by posting.
    post(&mut s, &b, "bob", "evidence", Some(2));
    let rd = call(&mut s, "read_discussion", json!({"thread": b}));
    assert_eq!(rd["frontmatter"]["status"], json!("open"));
    assert_eq!(rd["frontmatter"]["needs_decision"], json!(false));
    let q = call(&mut s, "decision_queue", json!({}));
    assert_eq!(q["queue"].as_array().unwrap().len(), 1);
    let e = call_err(
        &mut s,
        "record_decision",
        json!({"thread": b, "decided_by": "Dana", "decision": "keep the timing as is", "reason": "Dana ruled on the thread"}),
    );
    assert!(e.contains("unresolved"), "{e}");

    // Decide the other.
    let r = call(
        &mut s,
        "record_decision",
        json!({"thread": a, "decided_by": "Dana", "decision": "Keep the throttle; queue the ABBA retake.", "reason": "Dana ruled on D-001"}),
    );
    assert_eq!(r["post"], json!(4));
    let rd = call(&mut s, "read_discussion", json!({"thread": a}));
    assert_eq!(rd["frontmatter"]["status"], json!("concluded"));
    assert_eq!(rd["frontmatter"]["needs_decision"], json!(false));
    // The unresolved markers set by mark_unresolved are cleared, not left behind on a decided thread.
    assert_eq!(rd["frontmatter"]["unresolved_at"], json!(""));
    assert_eq!(rd["frontmatter"]["open_questions"], json!(0));
    assert_eq!(rd["frontmatter"]["decision_summary"], json!(""));
    assert_eq!(rd["posts"][3]["kind"], json!("decision"));
    assert_eq!(rd["posts"][3]["author"], json!("Dana"));
    assert_eq!(rd["posts"][3]["re"], json!(3));
    assert!(call(&mut s, "decision_queue", json!({}))["queue"].as_array().unwrap().is_empty());
    assert!(call(&mut s, "list_discussions", json!({"needs_decision": true}))["discussions"].as_array().unwrap().is_empty());
    assert_eq!(call(&mut s, "list_discussions", json!({"status": "concluded"}))["discussions"].as_array().unwrap().len(), 1);
    assert_eq!(call(&mut s, "list_discussions", json!({"subject": "PI-002"}))["discussions"].as_array().unwrap().len(), 2);
}

#[test]
fn discussions_index_counts_contested() {
    let fx = Fx::new("d-index");
    let mut s = fx.server();
    let t = open(&mut s, "perf", "Indexed thread");
    post(&mut s, &t, "alice", "challenge", Some(1));
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": "Threads.md", "name": "open", "reason": "open threads at a glance",
        "spec": {"sources": [{"vault": "perf", "folder": "Discussions"}], "rows": "discussions",
                 "filter": [{"key": "status", "value": "open"}], "columns": ["id", "link", "topic", "posts", "contested"]}}),
    );
    let idx = fs::read_to_string(fx.perf().join("Threads.md")).unwrap();
    assert!(idx.contains("| D-001 | [[D-001 Indexed thread]] | Indexed thread | 2 | 1 |"), "{idx}");
}

#[test]
fn twelve_processes_post_to_one_thread() {
    let fx = Fx::new("d-twelve");
    let t = {
        let mut s = fx.server();
        open(&mut s, "perf", "Concurrent posting")
    };
    const PROCS: usize = 12;
    const EACH: usize = 5;
    let mut handles = Vec::new();
    for p in 0..PROCS {
        let specs = fx.specs.clone();
        let db = fx.db.clone();
        let t = t.clone();
        handles.push(std::thread::spawn(move || {
            let mut cmd = Command::new(bin());
            for (n, path) in &specs {
                cmd.arg("--vault").arg(format!("{n}={}", path.display()));
            }
            let mut child = cmd.arg("--db").arg(&db).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            {
                let mut stdin = child.stdin.take().unwrap();
                writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"clientInfo": {"name": format!("proc-{p}")}}})).unwrap();
                for i in 0..EACH {
                    let msg = json!({"jsonrpc": "2.0", "id": 1 + i, "method": "tools/call", "params": {"name": "post", "arguments": {
                        "thread": t, "author": format!("agent{p}"), "kind": "answer", "re": 1, "body": format!("answer {i} from agent {p}, long enough")}}});
                    writeln!(stdin, "{msg}").unwrap();
                }
            }
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap().lines().map(|l| atlas::json::parse(l).unwrap()).collect::<Vec<_>>()
        }));
    }
    let mut numbers = Vec::new();
    for h in handles {
        for r in h.join().unwrap() {
            if r["id"].as_u64().unwrap_or(0) >= 1 {
                assert_eq!(r["result"]["isError"], json!(false), "{r}");
                let body: Value = atlas::json::parse(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
                numbers.push(body["post"].as_u64().unwrap());
            }
        }
    }
    let unique: HashSet<u64> = numbers.iter().copied().collect();
    assert_eq!(unique.len(), PROCS * EACH, "two posts got the same number");
    let mut sorted = numbers.clone();
    sorted.sort();
    assert_eq!(sorted, (2..2 + (PROCS * EACH) as u64).collect::<Vec<_>>());
    let mut s = fx.server();
    let r = call(&mut s, "read_discussion", json!({"thread": t}));
    assert_eq!(r["integrity"], json!("ok"));
    assert_eq!(r["post_count"], json!(1 + PROCS * EACH));
    assert_eq!(r["frontmatter"]["participants"].as_array().unwrap().len(), 2 + PROCS);
}

#[test]
fn only_the_challenged_author_can_concede() {
    let fx = Fx::new("d-concede");
    let mut s = fx.server();
    let t = open(&mut s, "perf", "Who may concede");
    post(&mut s, &t, "alice", "challenge", Some(1)); // 2: challenges bob's claim
                                                        // The challenger, and a bystander, "concede" on bob's behalf: kept, but they decide nothing.
    post(&mut s, &t, "alice", "concede", Some(2)); // 3
    post(&mut s, &t, "carol", "concede", Some(2)); // 4
    let r = call(&mut s, "discussion_status", json!({"thread": t}));
    assert_eq!(r["contested_challenges"], json!([2]));
    assert_eq!(r["points"][0]["state"], json!("contested"));
    let flags = r["flags"].as_array().unwrap();
    let flagged: Vec<u64> = flags.iter().map(|f| f["post"].as_u64().unwrap()).collect();
    assert_eq!(flagged, vec![3, 4]);
    assert!(flags.iter().all(|f| f["concede_ignored"] == json!("not the challenged author")));
    let rd = call(&mut s, "read_discussion", json!({"thread": t}));
    assert_eq!(rd["post_count"], json!(4), "ignored concedes stay in the thread");
    assert!(call_err(&mut s, "conclude_discussion", json!({"thread": t, "verdict_body": "the claim stands as written"})).contains("P-2"));
    // The challenged author's concede decides it.
    post(&mut s, &t, "Bob", "concede", Some(2)); // 5, case-insensitive author match
    let r = call(&mut s, "discussion_status", json!({"thread": t}));
    assert_eq!(r["contested_challenges"], json!([]));
    assert_eq!(r["points"][1]["state"], json!("conceded"));
    assert_eq!(r["points"][1]["by"], json!([5]));
    assert_eq!(r["points"][0]["state"], json!("conceded"));
    assert_eq!(r["flags"].as_array().unwrap().len(), 2);
}
