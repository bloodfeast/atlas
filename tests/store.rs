//! The SQLite store is derived from the files and heals itself.

mod common;

use std::fs;

use common::*;
use rusqlite::Connection;
use atlas::json;

fn dump(conn: &Connection) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    let q = |sql: &str| -> Vec<String> {
        let mut st = conn.prepare(sql).unwrap();
        st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    (
        q("SELECT vault||'|'||path||'|'||size||'|'||sha256||'|'||title||'|'||frontmatter FROM notes ORDER BY vault, path"),
        q("SELECT n.vault||'|'||n.path||'|'||l.line||'|'||l.kind||'|'||l.raw||'|'||l.target||'|'||ifnull(l.heading,'')||'|'||ifnull(l.to_vault,'')||'|'||ifnull(l.to_path,'')||'|'||l.attachment FROM links l JOIN notes n ON n.id=l.note_id ORDER BY 1"),
        q("SELECT n.vault||'|'||n.path||'|'||t.tag FROM tags t JOIN notes n ON n.id=t.note_id ORDER BY 1"),
        q("SELECT n.vault||'|'||n.path||'|'||f.title||'|'||f.body FROM fts f JOIN notes n ON n.id=f.rowid ORDER BY 1"),
    )
}

#[test]
fn deleting_the_database_rebuilds_the_same_rows() {
    let fx = Fx::new("s-rebuild");
    let first = {
        let mut s = fx.server();
        // Some writes so the rows are not just the fixture.
        call(&mut s, "append_history", json!({"vault": "perf", "path": "Index.md", "text": "x", "date": "2026-10-08"}));
        call(
            &mut s,
            "create_note",
            json!({"vault": "duel", "path": "Maps/Ridge.md", "content": "# Ridge\n[[Missing]] #map\n", "create_folders": true}),
        );
        dump(s.store_mut().conn())
    };
    assert!(!first.0.is_empty() && !first.1.is_empty() && !first.2.is_empty());
    // Delete the database (and its WAL files) mid-run, between servers.
    for ext in ["", "-wal", "-shm"] {
        let p = fx.db.with_file_name(format!("vaults.sqlite{ext}"));
        if p.exists() {
            fs::remove_file(&p).unwrap();
        }
    }
    let mut s = fx.server();
    let second = dump(s.store_mut().conn());
    assert_eq!(first, second);
}

#[test]
fn deleting_the_database_under_a_live_server_is_healed() {
    let fx = Fx::new("s-live");
    let mut s = fx.server();
    let before = dump(s.store_mut().conn());
    // Another process wipes the derived rows (as a fresh database would be).
    {
        let other = Connection::open(&fx.db).unwrap();
        other.execute_batch("DELETE FROM links; DELETE FROM tags; DELETE FROM fts; DELETE FROM notes;").unwrap();
    }
    let r = call(&mut s, "list_notes", json!({"vault": "perf"}));
    assert_eq!(r["total"], json!(5));
    assert_eq!(dump(s.store_mut().conn()), before);
}

#[test]
fn external_edits_are_seen_by_the_next_query() {
    let fx = Fx::new("s-external");
    let mut s = fx.server();
    let r = call(&mut s, "search", json!({"query": "zebra crossing"}));
    assert_eq!(r["results"].as_array().unwrap().len(), 0);
    let p = fx.perf().join("Ideas/PI-002 Throttle.md");
    let t = fs::read_to_string(&p).unwrap().replace("status: \"parked\"", "status: \"open\"") + "zebra crossing\n";
    fs::write(&p, t).unwrap();
    write(&fx.perf().join("Ideas/PI-009 Plain.md"), "---\nstatus: open\n---\n[[PI-002 Throttle]]\n");
    let r = call(&mut s, "search", json!({"query": "zebra crossing"}));
    assert_eq!(r["results"][0]["path"], json!("Ideas/PI-002 Throttle.md"));
    let r = call(&mut s, "find_by_frontmatter", json!({"key": "status", "value": "open"}));
    let paths: Vec<&str> = r["notes"].as_array().unwrap().iter().map(|n| n["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["Ideas/PI-002 Throttle.md", "Ideas/PI-009 Plain.md"]);
    let r = call(&mut s, "backlinks", json!({"vault": "perf", "path": "Ideas/PI-002 Throttle.md"}));
    let from: Vec<String> = r["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| format!("{}:{}", l["from_vault"].as_str().unwrap(), l["from_path"].as_str().unwrap()))
        .collect();
    assert_eq!(
        from,
        vec!["campaign:Missions/M1 Hold.md", "perf:Ideas/PI-001 Cache inside the loader.md", "perf:Ideas/PI-009 Plain.md"]
    );
    // A note removed outside the server leaves the index but not the audit log.
    call(&mut s, "append_to_note", json!({"vault": "perf", "path": "Ideas/PI-009 Plain.md", "text": "x"}));
    fs::remove_file(fx.perf().join("Ideas/PI-009 Plain.md")).unwrap();
    let r = call(&mut s, "list_notes", json!({"vault": "perf", "folder": "Ideas"}));
    assert_eq!(r["total"], json!(2));
    let n: i64 =
        s.store_mut().conn().query_row("SELECT COUNT(*) FROM writes WHERE path='Ideas/PI-009 Plain.md'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
    let n: i64 =
        s.store_mut().conn().query_row("SELECT COUNT(*) FROM links WHERE to_path='Ideas/PI-009 Plain.md'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn search_modes() {
    let fx = Fx::new("s-search");
    let mut s = fx.server();
    let r = call(&mut s, "search", json!({"query": "PI-002"}));
    assert!(!r["results"].as_array().unwrap().is_empty());
    let r = call(&mut s, "search", json!({"query": "measured at \\d+ us", "mode": "regex"}));
    assert_eq!(r["results"][0]["line"], json!(15));
    let r = call(&mut s, "search", json!({"query": "cache", "mode": "fts", "vault": "perf"}));
    assert_eq!(r["results"][0]["path"], json!("Ideas/PI-001 Cache inside the loader.md"));
    let e = call_err(&mut s, "search", json!({"query": "\"unbalanced", "mode": "fts"}));
    assert!(e.contains("fts"), "{e}");
    let e = call_err(&mut s, "search", json!({"query": "(", "mode": "regex"}));
    assert!(e.contains("regex"), "{e}");
    let r = call(&mut s, "search", json!({"query": "e", "limit": 3}));
    assert_eq!(r["results"].as_array().unwrap().len(), 3);
    assert_eq!(r["truncated"], json!(true));
}

fn jsonl(fx: &Fx) -> Vec<atlas::json::Value> {
    let p = fx.db.with_file_name(atlas::store::AUDIT_LOG);
    fs::read_to_string(p).unwrap().lines().map(|l| atlas::json::parse(l).unwrap()).collect()
}

#[test]
fn audit_jsonl_survives_a_rebuild_and_keeps_growing() {
    let fx = Fx::new("s-jsonl");
    let rows_before = {
        let mut s = fx.server();
        call(&mut s, "create_note", json!({"vault": "perf", "path": "J.md", "content": "# J\n"}));
        call(&mut s, "append_history", json!({"vault": "perf", "path": "J.md", "text": "one", "date": "2026-10-08"}));
        call(&mut s, "next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": "PI-"}));
        let conn = s.store_mut().conn();
        let mut st = conn.prepare("SELECT time, tool, vault, path FROM writes ORDER BY id").unwrap();
        st.query_map([], |r| Ok(json!({"time": r.get::<_, String>(0)?, "tool": r.get::<_, String>(1)?, "vault": r.get::<_, String>(2)?, "path": r.get::<_, String>(3)?})))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
    };
    let lines = jsonl(&fx);
    assert_eq!(lines.len(), 3);
    for (row, line) in rows_before.iter().zip(&lines) {
        for k in ["time", "tool", "vault", "path"] {
            assert_eq!(row[k], line[k], "{k}");
        }
        assert_eq!(line["client"], json!("test-client"));
    }
    let bytes_before = fs::read(fx.db.with_file_name(atlas::store::AUDIT_LOG)).unwrap();
    for ext in ["", "-wal", "-shm"] {
        let p = fx.db.with_file_name(format!("vaults.sqlite{ext}"));
        if p.exists() {
            fs::remove_file(&p).unwrap();
        }
    }
    // Rebuild (open + rescan) leaves the file byte-identical.
    let mut s = fx.server();
    call(&mut s, "list_notes", json!({"vault": "perf"}));
    assert_eq!(fs::read(fx.db.with_file_name(atlas::store::AUDIT_LOG)).unwrap(), bytes_before);
    let n: i64 = s.store_mut().conn().query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "the rebuilt database starts a fresh writes table");
    // And it keeps growing after the rebuild.
    call(&mut s, "append_to_note", json!({"vault": "perf", "path": "J.md", "text": "two"}));
    let after = jsonl(&fx);
    assert_eq!(after.len(), 4);
    assert_eq!(&after[..3], &lines[..]);
    assert_eq!(after[3]["tool"], json!("append_to_note"));
    assert!(after[3]["sha_before"].is_string() && after[3]["sha_after"].is_string());
}
