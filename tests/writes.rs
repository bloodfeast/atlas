//! Writes are additive or exact, atomic, and audited.

mod common;

use std::fs;

use common::*;
use atlas::json;
use atlas::markdown;

const P1: &str = "Ideas/PI-001 Cache inside the loader.md";

#[test]
fn set_frontmatter_leaves_body_byte_identical() {
    let fx = Fx::new("w-fm");
    let mut s = fx.server();
    let path = fx.perf().join(P1);
    let before = fs::read(&path).unwrap();
    let r = call(
        &mut s,
        "set_frontmatter",
        json!({"vault": "perf", "path": P1, "values": {"status": "parked", "tags": ["perf", "area/fs", "status/parked"], "queue": "139"}}),
    );
    assert_eq!(r["changed"], json!(true));
    let after = fs::read(&path).unwrap();
    let b = String::from_utf8(before.clone()).unwrap();
    let a = String::from_utf8(after).unwrap();
    assert_eq!(markdown::body(&a).as_bytes(), markdown::body(&b).as_bytes());
    let fm = markdown::parse_frontmatter(&a);
    assert_eq!(fm["status"], json!("parked"));
    assert_eq!(fm["queue"], json!("139"));
    assert_eq!(fm["id"], json!("PI-001"));
    // The untouched keys keep their exact lines.
    assert!(a.contains("saving_median: \"-0.2 ms\"\nmeasured: true\n"));

    // CRLF files stay CRLF and keep their body.
    let crlf = IDEA_2.replace('\n', "\r\n");
    write(&fx.perf().join("Ideas/PI-003 Crlf.md"), &crlf);
    call(&mut s, "set_frontmatter", json!({"vault": "perf", "path": "Ideas/PI-003 Crlf.md", "values": {"status": "open"}}));
    let a = fs::read_to_string(fx.perf().join("Ideas/PI-003 Crlf.md")).unwrap();
    assert_eq!(markdown::body(&a), markdown::body(&crlf));
    assert!(!a.replace("\r\n", "").contains('\n'), "a bare LF was introduced");
}

#[test]
fn stale_sha_is_refused() {
    let fx = Fx::new("w-sha");
    let mut s = fx.server();
    let note = read(&mut s, "perf", P1);
    let sha = note["sha256"].as_str().unwrap().to_string();
    // Someone else edits the file after our read.
    let path = fx.perf().join(P1);
    let mut t = fs::read_to_string(&path).unwrap();
    t.push_str("- 2026-10-09: edited elsewhere\n");
    fs::write(&path, &t).unwrap();
    let e = call_err(
        &mut s,
        "replace_section",
        json!({"vault": "perf", "path": P1, "heading": "Entry", "content": "- new", "expected_sha256": sha}),
    );
    assert!(e.contains("stale edit"), "{e}");
    assert_eq!(fs::read_to_string(&path).unwrap(), t, "a refused edit changed the file");
    let e = call_err(&mut s, "set_frontmatter", json!({"vault": "perf", "path": P1, "values": {"status": "x"}, "expected_sha256": sha}));
    assert!(e.contains("stale edit"), "{e}");
    let e = call_err(&mut s, "append_to_note", json!({"vault": "perf", "path": P1, "text": "x", "expected_sha256": sha}));
    assert!(e.contains("stale edit"), "{e}");
    // replace_section has no default: the sha is required.
    let e = call_err(&mut s, "replace_section", json!({"vault": "perf", "path": P1, "heading": "Entry", "content": "x"}));
    assert!(e.contains("expected_sha256"), "{e}");
    // With the fresh sha it goes through and only that section changes.
    let fresh = read(&mut s, "perf", P1)["sha256"].as_str().unwrap().to_string();
    call(
        &mut s,
        "replace_section",
        json!({"vault": "perf", "path": P1, "heading": "Entry", "content": "- **Where:** throttle", "expected_sha256": fresh}),
    );
    let now = fs::read_to_string(&path).unwrap();
    assert_eq!(now, t.replace("- **Where:** presenter", "- **Where:** throttle"));
}

#[test]
fn create_is_exclusive_and_append_is_additive() {
    let fx = Fx::new("w-create");
    let mut s = fx.server();
    let e = call_err(&mut s, "create_note", json!({"vault": "perf", "path": "Index.md", "content": "overwrite"}));
    assert!(e.contains("already exists"), "{e}");
    assert!(fs::read_to_string(fx.perf().join("Index.md")).unwrap().contains("# Index"));
    let e = call_err(&mut s, "create_note", json!({"vault": "perf", "path": "Packages/NEW.md", "content": "x"}));
    assert!(e.contains("not found"), "{e}");
    call(&mut s, "create_note", json!({"vault": "perf", "path": "Packages/NEW.md", "content": "# NEW\n", "create_folders": true}));
    let r = call(&mut s, "append_to_note", json!({"vault": "perf", "path": "Packages/NEW.md", "text": "- PI-001"}));
    assert_eq!(r["changed"], json!(true));
    assert_eq!(fs::read_to_string(fx.perf().join("Packages/NEW.md")).unwrap(), "# NEW\n- PI-001\n");
    // No temp files are left behind.
    let leftovers: Vec<_> =
        fs::read_dir(fx.perf().join("Packages")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
    assert!(leftovers.is_empty());
}

#[test]
fn append_history_goes_under_history() {
    let fx = Fx::new("w-hist");
    let mut s = fx.server();
    let r = call(&mut s, "append_history", json!({"vault": "perf", "path": P1, "text": "parked: measured no win", "date": "2026-10-09"}));
    assert_eq!(r["line"], json!("- 2026-10-09: parked: measured no win"));
    let t = fs::read_to_string(fx.perf().join(P1)).unwrap();
    assert!(t.ends_with("- 2026-10-08: proposed by LOADER-WORK\n- 2026-10-09: parked: measured no win\n"), "{t}");
    // Missing heading is created at the end; default date is today.
    let r = call(&mut s, "append_history", json!({"vault": "perf", "path": "Areas/GPU passes.md", "text": "area created"}));
    let line = r["line"].as_str().unwrap();
    assert!(line.starts_with("- 20") && line.ends_with(": area created"));
    let t = fs::read_to_string(fx.perf().join("Areas/GPU passes.md")).unwrap();
    assert_eq!(t, format!("# GPU passes\n\nBack to [[Index]].\n\n## History\n\n{line}\n"));
    let e = call_err(&mut s, "append_history", json!({"vault": "perf", "path": P1, "text": "a\nb"}));
    assert!(e.contains("one non-empty line"), "{e}");
    let e = call_err(&mut s, "append_history", json!({"vault": "perf", "path": P1, "text": "x", "date": "10/09/2026"}));
    assert!(e.contains("YYYY-MM-DD"), "{e}");
}

#[test]
fn audit_log_records_every_write_tool() {
    let fx = Fx::new("w-audit");
    let mut s = fx.server();
    call(&mut s, "create_note", json!({"vault": "perf", "path": "A.md", "content": "# A\n"}));
    call(&mut s, "append_to_note", json!({"vault": "perf", "path": "A.md", "text": "more"}));
    call(&mut s, "append_history", json!({"vault": "perf", "path": "A.md", "text": "made"}));
    call(&mut s, "set_frontmatter", json!({"vault": "perf", "path": "A.md", "values": {"status": "open"}}));
    let sha = read(&mut s, "perf", "A.md")["sha256"].as_str().unwrap().to_string();
    call(
        &mut s,
        "replace_section",
        json!({"vault": "perf", "path": "A.md", "heading": "History", "content": "- 2026-10-08: x", "expected_sha256": sha}),
    );
    call(
        &mut s,
        "link_notes",
        json!({"from_vault": "perf", "from_path": "A.md", "to_vault": "campaign", "to_path": "Missions/M1 Hold.md", "reverse": true}),
    );
    call(&mut s, "next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": "PI-", "name": "Audit", "content": "# {{id}}\n"}));
    call(&mut s, "next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": "PI-"}));
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": "Idx.md", "name": "all", "spec": {"sources": [{"vault": "perf", "folder": "Ideas"}]}}),
    );
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": "A.md", "name": "all", "spec": {"sources": [{"vault": "perf", "folder": "Ideas"}]}}),
    );
    call(&mut s, "create_note", json!({"vault": "perf", "path": "Ideas/PI-050 Later.md", "content": "# later\n"}));
    call(&mut s, "refresh_index", json!({"vault": "perf", "path": "Idx.md"}));
    call(&mut s, "create_note", json!({"vault": "perf", "path": "Ideas/PI-051 Later.md", "content": "# later\n"}));
    call(&mut s, "refresh_all_indices", json!({}));

    let tools: Vec<String> = {
        let conn = s.store_mut().conn();
        let mut st = conn.prepare("SELECT DISTINCT tool FROM writes ORDER BY tool").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    for t in [
        "append_history",
        "append_to_note",
        "build_index",
        "create_note",
        "link_notes",
        "next_id",
        "refresh_all_indices",
        "refresh_index",
        "replace_section",
        "set_frontmatter",
    ] {
        assert!(tools.contains(&t.to_string()), "no audit row for {t}: {tools:?}");
    }
    let conn = s.store_mut().conn();
    let (client, before, after): (String, Option<String>, Option<String>) = conn
        .query_row("SELECT client, sha_before, sha_after FROM writes WHERE tool='append_to_note'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(client, "test-client");
    assert!(before.is_some() && after.is_some() && before != after);
    // link_notes wrote both notes.
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM writes WHERE tool='link_notes'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);
}

#[test]
fn unchanged_edits_write_nothing() {
    let fx = Fx::new("w-noop");
    let mut s = fx.server();
    let r = call(&mut s, "set_frontmatter", json!({"vault": "perf", "path": P1, "values": {"status": "in flight LOADER-WORK"}}));
    assert_eq!(r["changed"], json!(false));
    let n: i64 = s.store_mut().conn().query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}
