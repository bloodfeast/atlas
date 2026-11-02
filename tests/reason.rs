//! Every write cites a reason; the reason is audited and queryable.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use common::*;
use rusqlite::Connection;
use atlas::json::{Value};
use atlas::json;

/// Every file under `root` with its bytes, to prove a refused call wrote nothing.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.to_string_lossy().to_string(), fs::read(&p).unwrap());
            }
        }
    }
    out
}

/// Valid arguments, minus `reason`, for every write tool.
fn write_calls(sha_of_index: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("create_note", json!({"vault": "perf", "path": "New.md", "content": "# New\n"})),
        ("append_to_note", json!({"vault": "perf", "path": "Index.md", "text": "more"})),
        ("append_history", json!({"vault": "perf", "path": "Index.md", "text": "something happened"})),
        ("set_frontmatter", json!({"vault": "perf", "path": "Index.md", "values": {"status": "open"}})),
        (
            "replace_section",
            json!({"vault": "perf", "path": "Index.md", "heading": "Index", "content": "x", "expected_sha256": sha_of_index}),
        ),
        ("link_notes", json!({"from_vault": "perf", "from_path": "Index.md", "to_vault": "perf", "to_path": "Ideas/PI-002 Throttle.md"})),
        ("next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": "PI-", "name": "X", "content": "x"})),
        ("build_index", json!({"vault": "perf", "path": "Idx.md", "name": "a", "spec": {"sources": [{"vault": "perf"}]}})),
        ("refresh_index", json!({"vault": "perf", "path": "Index.md"})),
        ("refresh_all_indices", json!({})),
        ("conclude_discussion", json!({"thread": "perf:D-999", "verdict_body": "a verdict long enough"})),
        (
            "mark_unresolved",
            json!({"thread": "perf:D-999", "summary": "a summary long enough", "open_questions": ["q?"], "positions": [], "settle_by": "a measurement of it"}),
        ),
        ("record_decision", json!({"thread": "perf:D-999", "decided_by": "Dana", "decision": "a decision long enough"})),
        ("report_miss", json!({"tool": "lookup", "query": "PI-002", "found": "Ideas/PI-002 Throttle.md:1"})),
        ("resolve_miss", json!({"id": 1})),
        ("remember", json!({"kind": "decision", "key": "decision:0001", "body": "use the bounded cache"})),
        ("supersede", json!({"old": "decision:0001", "body": "use the unbounded cache after all"})),
    ]
}

#[test]
fn every_write_tool_requires_a_real_reason() {
    let fx = Fx::new("r-reject");
    let mut s = fx.server();
    s.set_memory(atlas::memory::settings::Settings::from_config(None, &fx.db).unwrap());
    let sha = read(&mut s, "perf", "Index.md")["sha256"].as_str().unwrap().to_string();
    let calls = write_calls(&sha);

    // The list above covers every write tool the server has, and each one's schema requires `reason`.
    let mut listed: Vec<&str> = calls.iter().map(|c| c.0).collect();
    listed.sort();
    let tools = atlas::tools::tool_list();
    // Thread posts carry their own reason (kind and body); every other write needs one.
    let self_reasoned: Vec<&str> = tools
        .iter()
        .filter(|t| t["annotations"]["readOnlyHint"] != json!(true))
        .filter(|t| !t["inputSchema"]["required"].as_array().is_some_and(|r| r.contains(&json!("reason"))))
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    // Journal entries and handoffs record their entry kind as the reason.
    assert_eq!(self_reasoned, vec!["open_discussion", "post", "journal", "handoff"]);
    let mut writes: Vec<String> = tools
        .iter()
        .filter(|t| t["annotations"]["readOnlyHint"] != json!(true))
        .filter(|t| !self_reasoned.contains(&t["name"].as_str().unwrap()))
        .map(|t| {
            assert!(t["inputSchema"]["required"].as_array().unwrap().contains(&json!("reason")), "{}", t["name"]);
            // The reason's own schema says it is logged (the tool
            // descriptions stay short: they are sent on every tools/list).
            let d = t["inputSchema"]["properties"]["reason"]["description"].as_str().unwrap();
            assert!(d.contains("logged"), "{d}");
            t["name"].as_str().unwrap().to_string()
        })
        .collect();
    writes.sort();
    assert_eq!(listed, writes);

    let bad: Vec<Option<Value>> = vec![
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!("    ")),
        Some(json!("fixed typo")),
        Some(json!("update")),
        Some(json!("fix")),
        Some(json!("n/a")),
        Some(json!("None")),
        Some(json!("Edit.")),
        Some(json!("  no reason given  ")),
        Some(json!("Minor changes!")),
        Some(json!("not applicable")),
        Some(json!(42)),
        Some(json!("two lines of\nreason text here")),
    ];
    let vaults = |fx: &Fx| fx.specs.iter().map(|(_, p)| snapshot(p)).collect::<Vec<_>>();
    let before = vaults(&fx);
    for (tool, args) in &calls {
        for reason in &bad {
            let mut a = args.clone();
            if let Some(r) = reason {
                a["reason"] = r.clone();
            }
            let e = s.call(tool, &a).expect_err(&format!("{tool} accepted reason {reason:?}")).to_string();
            assert!(e.contains("reason"), "{tool} {reason:?}: {e}");
        }
    }
    assert!(vaults(&fx) == before, "a refused write changed a file");
    assert!(!fx.db.with_file_name(atlas::store::AUDIT_LOG).exists());
    let n: i64 = s.store_mut().conn().query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);

    // The same calls with a real reason go through.
    // (Thread tools are exercised with real threads in tests/discussions.rs.)
    for (tool, args) in calls.into_iter().filter(|(_, a)| a.get("thread").is_none()) {
        let mut a = args.clone();
        a["reason"] = json!("checking that every write tool accepts a real reason");
        if tool == "replace_section" {
            a["expected_sha256"] = json!(read(&mut s, "perf", "Index.md")["sha256"].as_str().unwrap());
        }
        s.call(tool, &a).unwrap_or_else(|e| panic!("{tool}: {e}"));
    }
}

#[test]
fn reason_lands_in_the_database_and_the_jsonl_but_not_the_note() {
    let fx = Fx::new("r-record");
    let mut s = fx.server();
    let reason = "parked after the LOADER-WORK retake measured no win";
    let p = "Ideas/PI-001 Cache inside the loader.md";
    s.call("set_frontmatter", &json!({"vault": "perf", "path": p, "values": {"status": "parked"}, "reason": reason})).unwrap();
    s.call(
        "append_history",
        &json!({"vault": "perf", "path": p, "text": "parked: no win", "date": "2026-10-09", "reason": format!("  {reason}  ")}),
    )
    .unwrap();
    let db: Vec<String> = {
        let conn = s.store_mut().conn();
        let mut st = conn.prepare("SELECT reason FROM writes ORDER BY id").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    assert_eq!(db, vec![reason, reason], "stored trimmed");
    let log = fs::read_to_string(fx.db.with_file_name(atlas::store::AUDIT_LOG)).unwrap();
    let lines: Vec<Value> = log.lines().map(|l| atlas::json::parse(l).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|l| l["reason"] == json!(reason)));
    let note = fs::read_to_string(fx.perf().join(p)).unwrap();
    assert!(!note.contains("LOADER-WORK retake"), "the reason leaked into the note");
    // append_history's own line is unchanged by the reason.
    assert!(note.ends_with("- 2026-10-09: parked: no win\n"));
}

#[test]
fn migration_adds_the_column_to_an_old_database_without_a_rebuild() {
    let fx = Fx::new("r-migrate");
    let ids_before: String = {
        let mut s = fx.server();
        call(&mut s, "append_to_note", json!({"vault": "perf", "path": "Index.md", "text": "before"}));
        s.store_mut().conn().query_row("SELECT group_concat(id || ':' || path) FROM notes", [], |r| r.get(0)).unwrap()
    };
    // Turn it into a database from before reasons existed (which also
    // predates the open stamp that lets an unchanged database skip the checks).
    {
        let c = Connection::open(&fx.db).unwrap();
        c.execute_batch(
            "DELETE FROM meta WHERE key = 'open_stamp'; DROP INDEX IF EXISTS writes_note; DROP INDEX IF EXISTS writes_time; DELETE FROM writes;
             ALTER TABLE writes DROP COLUMN reason;
             INSERT INTO writes(time, tool, vault, path, sha_before, sha_after, client, pid)
             VALUES('2026-10-01T09:00:00.000Z', 'append_to_note', 'perf', 'Index.md', 'a', 'b', 'old-client', 1);",
        )
        .unwrap();
        let cols: Vec<String> = c
            .prepare("SELECT name FROM pragma_table_info('writes')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(!cols.contains(&"reason".to_string()));
    }
    let mut s = fx.server();
    let ids_after: String = s.store_mut().conn().query_row("SELECT group_concat(id || ':' || path) FROM notes", [], |r| r.get(0)).unwrap();
    assert_eq!(ids_before, ids_after, "derived tables were rebuilt instead of migrated");
    call(
        &mut s,
        "append_to_note",
        json!({"vault": "perf", "path": "Index.md", "text": "after", "reason": "first write after the migration"}),
    );
    let log = call(&mut s, "write_log", json!({"vault": "perf", "path": "Index.md"}));
    let e = log["entries"].as_array().unwrap();
    assert_eq!(e.len(), 2);
    assert_eq!(e[0]["reason"], json!("first write after the migration"));
    assert_eq!(e[1]["reason"], json!(""));
    assert_eq!(e[1]["client"], json!("old-client"));
    // Opening again is a no-op.
    drop(s);
    let mut s = fx.server();
    assert_eq!(call(&mut s, "write_log", json!({}))["total"], json!(2));
}

#[test]
fn write_log_filters() {
    let fx = Fx::new("r-log");
    let mut s = fx.server();
    s.set_client("agent-a");
    call(&mut s, "append_to_note", json!({"vault": "perf", "path": "Index.md", "text": "a1", "reason": "agent a first index note"}));
    call(
        &mut s,
        "append_to_note",
        json!({"vault": "perf", "path": "Ideas/PI-002 Throttle.md", "text": "a2", "reason": "agent a evidence for PI-002"}),
    );
    s.set_client("agent-b");
    call(
        &mut s,
        "append_to_note",
        json!({"vault": "campaign", "path": "Missions/M1 Hold.md", "text": "b1", "reason": "agent b mission briefing"}),
    );
    call(&mut s, "append_to_note", json!({"vault": "perf", "path": "Index.md", "text": "b2", "reason": "agent b second index note"}));
    // Backdate the first write so the time filters have something to split.
    s.store_mut()
        .conn()
        .execute("UPDATE writes SET time = '2026-01-01T00:00:00.000Z' WHERE reason = 'agent a first index note'", [])
        .unwrap();

    let reasons = |s: &mut atlas::tools::Server, args: Value| -> Vec<String> {
        call(s, "write_log", args)["entries"].as_array().unwrap().iter().map(|e| e["reason"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(
        reasons(&mut s, json!({})),
        vec!["agent b second index note", "agent b mission briefing", "agent a evidence for PI-002", "agent a first index note"]
    );
    assert_eq!(reasons(&mut s, json!({"vault": "campaign"})), vec!["agent b mission briefing"]);
    assert_eq!(
        reasons(&mut s, json!({"vault": "perf", "path": "Index.md"})),
        vec!["agent b second index note", "agent a first index note"]
    );
    assert_eq!(reasons(&mut s, json!({"client": "agent-a"})), vec!["agent a evidence for PI-002", "agent a first index note"]);
    assert_eq!(reasons(&mut s, json!({"until": "2026-06-01"})), vec!["agent a first index note"]);
    assert_eq!(reasons(&mut s, json!({"since": "2026-06-01", "client": "agent-a"})), vec!["agent a evidence for PI-002"]);
    assert!(reasons(&mut s, json!({"since": "2999-01-01T00:00"})).is_empty());
    let r = call(&mut s, "write_log", json!({"limit": 1}));
    assert_eq!(r["total"], json!(4));
    assert_eq!(r["truncated"], json!(true));
    let e = &r["entries"][0];
    for k in ["time", "tool", "vault", "path", "client", "reason", "sha_before", "sha_after"] {
        assert!(!e[k].is_null(), "{k} missing");
    }
    let err = call_err(&mut s, "write_log", json!({"since": "last tuesday"}));
    assert!(err.contains("since"), "{err}");
    let err = call_err(&mut s, "write_log", json!({"path": "Index.md"}));
    assert!(err.contains("needs vault"), "{err}");
    // write_log is read-only: it takes no reason and writes nothing.
    let err = call_err(&mut s, "write_log", json!({"reason": "should not be accepted here"}));
    assert!(err.contains("unknown argument"), "{err}");
}
