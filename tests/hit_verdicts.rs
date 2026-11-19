//! #348: the token report's hook hit verdicts are stored once their
//! transcript has settled, so a repeat run reads only the transcripts still
//! changing, and gives the same credit as a run with nothing stored.

mod common;

use std::path::Path;
use std::time::{Duration, SystemTime};

use atlas::hook_hits::{hit_credit, HitCredit, Judge};
use atlas::json;
use atlas::memory::MemoryStore;
use common::*;

/// One assistant line using `key` in a tool input at `time`.
fn used(time: &str, key: &str) -> String {
    format!("{}\n", json!({"type": "assistant", "timestamp": time, "message": {"role": "assistant", "content": [{"type": "tool_use", "name": "mcp__atlas__recall", "input": {"key": key}}]}}))
}

fn inject(conn: &atlas::db::Connection, transcript: &Path, key: &str, read: i64) {
    conn.execute(
        "INSERT INTO hook_inject(time, session, agent, transcript, hook, key, fts, cos, bytes, read) \
         VALUES('2026-01-01T00:00:00.000Z', 's', '', ?1, 'UserPromptSubmit', ?2, NULL, NULL, 10, ?3)",
        atlas::db::params![transcript.to_string_lossy(), key, read],
    )
    .unwrap();
}

/// What a run credits, without its cache counters.
fn credited(c: &HitCredit) -> impl PartialEq + std::fmt::Debug {
    (c.by.clone(), c.injected, c.hits, c.at.clone())
}

#[test]
fn settled_hit_verdicts_are_stored_and_not_judged_again() {
    let t = Temp::new("hit-verdicts");
    let store = MemoryStore::open(&t.path().join("memory.sqlite")).unwrap();
    let conn = atlas::db::Connection::open(t.path().join("atlas.sqlite")).unwrap();
    atlas::hook_hits::logged(&conn, "").unwrap();
    let (old, new) = (t.path().join("old.jsonl"), t.path().join("new.jsonl"));
    let at = "2026-01-01T00:10:00.000Z";
    write(&old, &used(at, "widget-cache"));
    write(&new, &used(at, "widget-cache"));
    // The old transcript has been still for a day; the new one is written now.
    let day_ago = SystemTime::now() - Duration::from_secs(86_400);
    std::fs::File::options().write(true).open(&old).unwrap().set_modified(day_ago).unwrap();
    inject(&conn, &old, "widget-cache", 100);
    inject(&conn, &old, "unused-key", 50);
    inject(&conn, &new, "widget-cache", 70);
    let run = |until: &str| {
        let mut judge = Judge::new(&store, &[]).until(until);
        let c = hit_credit(&conn, &mut judge).unwrap();
        assert_eq!(c.parsed, judge.parsed());
        c
    };

    // Bounded before the old transcript's last change, nothing is stored:
    // its verdicts could still change once the bound moves on.
    let bounded = run("2026-01-01T00:20:00.000Z");
    assert_eq!((bounded.hits, bounded.stored, bounded.parsed), (2, 0, 2));

    // First unbounded run: nothing stored yet, both transcripts read.
    let first = run("");
    assert_eq!((first.injected, first.hits, first.stored, first.parsed), (3, 2, 0, 2), "{first:?}");
    // Each hit's read credited once, by hook and day (the two hits, 100 + 70).
    let by: Vec<_> = first.by.iter().map(|((h, d), n)| (h.as_str(), d.as_str(), *n)).collect();
    assert_eq!(by, [("hook:UserPromptSubmit", "2026-01-01", 170)]);
    // Second run: the old transcript's two verdicts (a hit and a miss) come
    // from the store; only the new transcript is read.
    let second = run("");
    assert_eq!((second.stored, second.parsed), (2, 1), "{second:?}");
    assert_eq!(credited(&second), credited(&first));

    // The stored verdict stands without its transcript: the old one is
    // deleted (as Claude Code deletes old ones) and its hit is still
    // credited. (Emptied in place, it would be judged again, #357.)
    std::fs::remove_file(&old).unwrap();
    // The unsettled transcript is judged again: its use is gone.
    write(&new, "");
    let third = run("");
    assert_eq!((third.injected, third.hits, third.stored, third.parsed), (3, 1, 2, 1), "{third:?}");
    assert_eq!(third.at.iter().map(|h| h.read).collect::<Vec<_>>(), [100]);
}

#[test]
fn a_stored_verdict_with_an_unknown_rule_is_judged_again() {
    let t = Temp::new("hit-verdicts-rule");
    let store = MemoryStore::open(&t.path().join("memory.sqlite")).unwrap();
    let conn = atlas::db::Connection::open(t.path().join("atlas.sqlite")).unwrap();
    atlas::hook_hits::logged(&conn, "").unwrap();
    let old = t.path().join("old.jsonl");
    write(&old, &used("2026-01-01T00:10:00.000Z", "widget-cache"));
    let day_ago = SystemTime::now() - Duration::from_secs(86_400);
    std::fs::File::options().write(true).open(&old).unwrap().set_modified(day_ago).unwrap();
    inject(&conn, &old, "widget-cache", 100);
    let run = || hit_credit(&conn, &mut Judge::new(&store, &[])).unwrap();
    let first = run();
    assert_eq!((first.hits, first.stored, first.parsed), (1, 0, 1), "{first:?}");
    let second = run();
    assert_eq!((second.stored, second.parsed), (1, 0), "{second:?}");
    // A rule the judge does not name (a newer or damaged row) is not trusted.
    conn.execute("UPDATE hit_verdict SET rule = 'bogus'", []).unwrap();
    let again = run();
    assert_eq!((again.hits, again.stored, again.parsed), (1, 0, 1), "{again:?}");
    // Judged again, its verdict replaces the row.
    let last = run();
    assert_eq!((last.hits, last.stored, last.parsed), (1, 1, 0), "{last:?}");
}

/// #357: a stored verdict is judged again when its transcript changes after
/// it settled (appended to, or only touched), when the judge's version
/// differs, when its injection's key differs (an id reused), and by a run
/// bounded before the transcript's last change; it stands when the
/// transcript is gone.
#[test]
fn a_stored_verdict_is_judged_again_when_its_transcript_or_judge_changes() {
    let t = Temp::new("hit-verdicts-stamp");
    let store = MemoryStore::open(&t.path().join("memory.sqlite")).unwrap();
    let conn = atlas::db::Connection::open(t.path().join("atlas.sqlite")).unwrap();
    atlas::hook_hits::logged(&conn, "").unwrap();
    let old = t.path().join("old.jsonl");
    write(&old, &used("2026-01-01T00:10:00.000Z", "widget-cache"));
    let day_ago = SystemTime::now() - Duration::from_secs(86_400);
    let settle = |m: SystemTime| std::fs::File::options().write(true).open(&old).unwrap().set_modified(m).unwrap();
    settle(day_ago);
    inject(&conn, &old, "widget-cache", 100);
    inject(&conn, &old, "late-key", 50);
    let run = |until: &str| hit_credit(&conn, &mut Judge::new(&store, &[]).until(until)).unwrap();
    let counts = |c: &HitCredit| (c.hits, c.stored, c.parsed, c.by.values().sum::<i64>());
    assert_eq!(counts(&run("")), (1, 0, 1, 100));
    assert_eq!(counts(&run("")), (1, 2, 0, 100));

    // Bounded before the transcript's last change: judged within the bound,
    // and the stored rows are left as they are.
    assert_eq!(counts(&run("2026-01-01T00:20:00.000Z")), (1, 0, 1, 100));
    assert_eq!(counts(&run("")), (1, 2, 0, 100));

    // Resumed after it settled: the miss is now a hit. The mtime is put
    // back, so only the size tells.
    std::io::Write::write_all(&mut std::fs::OpenOptions::new().append(true).open(&old).unwrap(), used("2026-01-01T01:00:00.000Z", "late-key").as_bytes())
        .unwrap();
    settle(day_ago);
    assert_eq!(counts(&run("")), (2, 0, 1, 150), "the appended use is credited");
    assert_eq!(counts(&run("")), (2, 2, 0, 150));

    // Only touched: a new mtime, the same size.
    settle(day_ago - Duration::from_secs(60));
    assert_eq!(counts(&run("")), (2, 0, 1, 150));

    // Another judge version, or another injection's key under the same id.
    conn.execute("UPDATE hit_verdict SET judge = judge - 1", []).unwrap();
    assert_eq!(counts(&run("")), (2, 0, 1, 150), "another judge version");
    conn.execute("UPDATE hit_verdict SET key = 'other-key' WHERE key = 'late-key'", []).unwrap();
    assert_eq!(counts(&run("")), (2, 1, 1, 150), "another key");
    assert_eq!(counts(&run("")), (2, 2, 0, 150));

    // One judge over two runs: `parsed` counts this run's reads only (the
    // transcript is still in the judge's cache, so none).
    conn.execute("UPDATE hit_verdict SET judge = judge - 1", []).unwrap();
    let mut judge = Judge::new(&store, &[]);
    assert_eq!(hit_credit(&conn, &mut judge).unwrap().parsed, 1);
    conn.execute("UPDATE hit_verdict SET judge = judge - 1", []).unwrap();
    assert_eq!(hit_credit(&conn, &mut judge).unwrap().parsed, 0, "cached in the judge");

    // Gone: even an older version's verdicts stand, nothing can judge them again.
    conn.execute("UPDATE hit_verdict SET judge = judge - 1", []).unwrap();
    std::fs::remove_file(&old).unwrap();
    assert_eq!(counts(&run("")), (2, 2, 0, 150));
}
