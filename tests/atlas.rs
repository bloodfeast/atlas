//! The atlas layer: identifier registry, read-only sources, section search,
//! freshness, misses and the recall check, over a generic fixture corpus.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use atlas::json::{Value};
use atlas::json;

use atlas::config::Config;
use atlas::paths::Vaults;
use atlas::store::{AtlasSetup, DefText, Store};
use atlas::tools::Server;

const TICKET_1: &str = "---
id: TK-001
status: open
---
# TK-001 Login flow

Depends on TK-002 and ships in #12.
See [[TK-002 Cache]].
";

const LOG: &str = "# Log

## 2026-01-05

- shipped TK-001 in #12

## 2026-03-09

- reopened TK-001 after BIG-WORK landed
";

const RULES: &str = "# Rules

Intro text that belongs to no rule.

- **TK-003** (2026-02-01): every cache entry carries its owner.
  The owner is checked on read.
- A second rule that cites TK-003 and BIG-WORK.
- Third rule, about alpha-slug.
";

const GUIDE: &str = "# Guide

## Caching

The cache section explains eviction in detail.

## BIG-WORK

The work item that introduced the eviction rules.
";

const ALPHA: &str = "---
name: alpha-slug
description: \"The alpha memory\"
metadata:
  type: project
---
Alpha body.
";

/// The fixture corpus: a vault, two sources, and files that must never be
/// read. Returns the config path.
pub fn corpus(root: &Path) -> PathBuf {
    let notes = root.join("Notes");
    write(&notes.join("Tickets/TK-001 Login flow.md"), TICKET_1);
    write(&notes.join("Tickets/TK-002 Cache.md"), "---\nid: TK-002\n---\n# TK-002 Cache\n\nCache body, cited by TK-001.\n");
    write(&notes.join("Log.md"), LOG);
    let hb = root.join("handbook");
    write(&hb.join("RULES.md"), RULES);
    write(&hb.join("GUIDE.md"), GUIDE);
    write(&hb.join("deep/NESTED.md"), "# Nested\n\nTK-004 is only in a folder the globs do not reach.\n");
    write(&hb.join(".env"), "SECRET_TOKEN=TK-900 zebraquartz\n");
    write(&hb.join(".env.local"), "TK-901 zebraquartz\n");
    write(&hb.join("server.pem"), "TK-902 zebraquartz\n");
    write(&hb.join("signing.key"), "TK-903 zebraquartz\n");
    write(&hb.join("credentials.md"), "TK-904 zebraquartz\n");
    write(&hb.join("notes.txt"), "Plain text mentions TK-003.\n");
    let mem = root.join("memory");
    write(&mem.join("alpha-slug.md"), ALPHA);
    write(&mem.join("beta-slug.md"), "---\nname: beta-slug\n---\nBeta builds on alpha-slug (see alpha-slug.md).\n");
    write(&root.join("outside/TK-905 Outside.md"), "TK-905 zebraquartz\n");
    let cfg = json!({
        "db": "state/atlas.sqlite",
        "watch": false,
        "vaults": [{"name": "notes", "path": "Notes"}],
        "sources": [
            {"name": "handbook", "root": "handbook", "include": ["*.md", "*.txt"], "chunk": {"top_level_items": true}},
            {"name": "memory", "root": "memory", "include": ["*.md"], "id_keys": ["name"], "names_kind": "memory"}
        ],
        "ids": [
            {"name": "ticket", "regex": "\\bTK-[0-9]{3}\\b"},
            {"name": "pr", "regex": "(?:^|[^A-Za-z0-9_&/])(?P<id>#[0-9]{1,5})\\b"},
            {"name": "work", "regex": "\\b[A-Z][A-Z0-9]+(?:-[A-Z][A-Z0-9]+)+\\b"}
        ]
    });
    let path = root.join("atlas.json");
    fs::write(&path, atlas::json::to_string_pretty(&cfg)).unwrap();
    path
}

fn open(cfg: &Path) -> Server {
    let c = Config::load(cfg).unwrap();
    let specs: Vec<(String, PathBuf)> = c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
    let mut store = Store::open_with(c.db.as_ref().unwrap(), Vaults::new(&specs).unwrap(), AtlasSetup::from_config(&c).unwrap()).unwrap();
    store.rescan().unwrap();
    store.rescan_sources(None).unwrap();
    let mut s = Server::new(store);
    s.set_client("test-client");
    s
}

fn hits(v: &Value, key: &str) -> Vec<(String, String, i64, Option<String>)> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (h["source"].as_str().unwrap().into(), h["path"].as_str().unwrap().into(), h["line"].as_i64().unwrap(), h["def"].as_str().map(Into::into))
        })
        .collect()
}

#[test]
fn lookup_returns_definitions_first_then_mentions_newest_first() {
    let t = Temp::new("a-lookup");
    let cfg = corpus(t.path());
    let mut s = open(&cfg);
    let r = call(&mut s, "lookup", json!({"id": "tk-001"}));
    let defs = hits(&r, "definitions");
    assert_eq!(defs[0], ("notes".into(), "Tickets/TK-001 Login flow.md".into(), 2, Some("frontmatter".into())), "{r}");
    assert!(defs.iter().any(|d| d.3.as_deref() == Some("file")));
    assert!(defs.iter().any(|d| d.3.as_deref() == Some("heading") && d.2 == 5));
    let ment = hits(&r, "mentions");
    // Dated mentions newest first: the 2026-03-09 log line before 2026-01-05.
    let dated: Vec<i64> = ment.iter().filter(|m| m.1 == "Log.md").map(|m| m.2).collect();
    assert_eq!(dated, vec![9, 5], "{r}");
    assert!(r["definitions"][0]["text"].as_str().unwrap().contains("Login flow"));
    assert_eq!(r["found"], json!(true));
    // Fresh: the answer says nothing about freshness unless asked.
    assert!(r.get("freshness").is_none(), "{r}");

    // A list item that introduces an id defines it, with its continuation line.
    let r = call(&mut s, "lookup", json!({"id": "TK-003"}));
    let d = &r["definitions"][0];
    assert_eq!((d["source"].as_str(), d["def"].as_str(), d["line"].as_i64()), (Some("handbook"), Some("list_item"), Some(5)), "{r}");
    assert_eq!(d["heading_path"], json!("Rules"));
    assert!(d["text"].as_str().unwrap().ends_with("The owner is checked on read."), "{r}");
    let m = hits(&r, "mentions");
    assert!(m.iter().any(|x| x.0 == "handbook" && x.1 == "RULES.md" && x.2 == 7));
    assert!(m.iter().any(|x| x.1 == "notes.txt"), "txt files are indexed when included");

    // Memory names: defined by the file (and its name key), mentioned by word.
    let r = call(&mut s, "lookup", json!({"id": "alpha-slug"}));
    let defs = hits(&r, "definitions");
    assert!(defs.iter().any(|d| d.0 == "memory" && d.1 == "alpha-slug.md" && d.3.as_deref() == Some("frontmatter")), "{r}");
    assert!(defs.iter().any(|d| d.3.as_deref() == Some("file")));
    let m = hits(&r, "mentions");
    assert_eq!(m.iter().filter(|x| x.1 == "beta-slug.md").count(), 1, "one per line: {r}");
    assert!(m.iter().any(|x| x.1 == "RULES.md"));

    // PR numbers and work names.
    let r = call(&mut s, "lookup", json!({"id": "#12"}));
    assert_eq!(r["total_mentions"], json!(2), "{r}");
    let r = call(&mut s, "lookup", json!({"id": "BIG-WORK"}));
    assert_eq!(hits(&r, "definitions")[0].3.as_deref(), Some("heading"));

    // Wikilink targets resolve to the note of that name.
    let r = call(&mut s, "lookup", json!({"id": "TK-002 Cache"}));
    assert_eq!(hits(&r, "definitions")[0].1, "Tickets/TK-002 Cache.md");
    assert_eq!(hits(&r, "mentions")[0].1, "Tickets/TK-001 Login flow.md");

    // Unknown ids say so, with text hits as a fallback.
    let r = call(&mut s, "lookup", json!({"id": "eviction"}));
    assert_eq!(r["found"], json!(false));
    assert_eq!(r["text_hits"][0]["path"], json!("GUIDE.md"), "{r}");
}

#[test]
fn ids_lists_by_prefix_pattern_kind_and_source() {
    let t = Temp::new("a-ids");
    let cfg = corpus(t.path());
    let mut s = open(&cfg);
    let r = call(&mut s, "ids", json!({"prefix": "tk-", "kind": "ticket"}));
    let ids: Vec<&str> = r["ids"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["TK-001", "TK-002", "TK-003"], "{r}");
    let r = call(&mut s, "ids", json!({"pattern": "^#[0-9]+$"}));
    assert_eq!(r["ids"][0]["id"], json!("#12"));
    let r = call(&mut s, "ids", json!({"source": "memory", "kind": "memory"}));
    assert_eq!(r["total"], json!(2));
    let e = call_err(&mut s, "ids", json!({"source": "nowhere"}));
    assert!(e.contains("not found"), "{e}");
}

#[test]
fn section_search_returns_whole_sections_with_heading_paths() {
    let t = Temp::new("a-sections");
    let cfg = corpus(t.path());
    let mut s = open(&cfg);
    let r = call(&mut s, "search", json!({"mode": "sections", "query": "eviction detail"}));
    let h = &r["results"][0];
    assert_eq!((h["source"].as_str(), h["path"].as_str()), (Some("handbook"), Some("GUIDE.md")), "{r}");
    assert_eq!(h["heading_path"], json!("Guide > Caching"));
    assert_eq!((h["line_start"].as_i64(), h["line_end"].as_i64()), (Some(3), Some(6)));
    assert_eq!(h["text"], json!("## Caching\n\nThe cache section explains eviction in detail.\n\n"));
    // Top-level items are sections of their own in a source that asks for it.
    let r = call(&mut s, "search", json!({"mode": "sections", "query": "owner checked", "source": "handbook"}));
    assert_eq!(r["results"][0]["label"], json!("- **TK-003** (2026-02-01): every cache entry carries its owner."));
    assert_eq!(r["results"][0]["date"], json!("2026-02-01"));
    // Hyphenated work names search as phrases.
    let r = call(&mut s, "search", json!({"mode": "sections", "query": "BIG-WORK", "limit": 50}));
    assert!(r["results"].as_array().unwrap().len() >= 3, "{r}");
    // Sections-only arguments are refused in the line modes.
    let e = call_err(&mut s, "search", json!({"query": "x", "source": "handbook"}));
    assert!(e.contains("sections"), "{e}");
}

#[test]
fn deny_list_and_globs_keep_secrets_and_outside_files_unread() {
    let t = Temp::new("a-deny");
    let cfg = corpus(t.path());
    let mut s = open(&cfg);
    let docs = s.store_mut().docs().unwrap();
    let paths: Vec<&str> = docs.iter().map(|d| d.1.as_str()).collect();
    for bad in [".env", ".env.local", "server.pem", "signing.key", "credentials.md", "deep/NESTED.md", "TK-905 Outside.md"] {
        assert!(!paths.contains(&bad), "{bad} was indexed: {paths:?}");
    }
    let r = call(&mut s, "search", json!({"mode": "sections", "query": "zebraquartz"}));
    assert_eq!(r["results"], json!([]), "{r}");
    for id in ["TK-900", "TK-901", "TK-902", "TK-903", "TK-904", "TK-905", "TK-004"] {
        let r = call(&mut s, "lookup", json!({"id": id}));
        assert_eq!(r["found"], json!(false), "{id}: {r}");
    }
    // The readers refuse them even when asked directly.
    let src = &s.store_mut().sources()[0].clone();
    for bad in [".env", ".env.local", "server.pem", "signing.key", "credentials.md", "../outside/TK-905 Outside.md", "deep/NESTED.md"] {
        assert!(src.read(bad).is_err(), "read {bad}");
    }
    // And a configuration cannot name them.
    for inc in [".env", "secrets/.env", "server.pem", "x/signing.key", "../outside/*.md", "/abs/*.md"] {
        let r = atlas::sources::Source::new("x", t.path(), vec![inc.into()], vec![], Default::default(), vec![], None);
        assert!(r.is_err(), "accepted include {inc}");
    }
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.clone(), (fs::read(&p).unwrap(), fs::metadata(&p).unwrap().modified().unwrap()));
            }
        }
    }
    out
}

#[test]
fn no_write_tool_can_reach_a_source() {
    let t = Temp::new("a-readonly");
    let cfg = corpus(t.path());
    let mut s = open(&cfg);
    let before: Vec<_> = ["handbook", "memory"].iter().map(|d| snapshot(&t.path().join(d))).collect();
    let tools = atlas::tools::tool_list();
    let writes: Vec<String> = tools
        .iter()
        .filter(|t| t["annotations"]["readOnlyHint"] != json!(true))
        .map(|t| t["name"].as_str().unwrap().to_string())
        .filter(|n| !["report_miss", "resolve_miss", "refresh_all_indices"].contains(&n.as_str()))
        .collect();
    assert!(writes.len() >= 14, "{writes:?}");
    for src in ["handbook", "memory"] {
        for file in ["RULES.md", "alpha-slug.md", "../handbook/RULES.md"] {
            let args = json!({
                "vault": src, "path": file, "from_vault": src, "from_path": file, "to_vault": src, "to_path": file,
                "content": "x", "text": "x", "values": {"a": "b"}, "heading": "Rules", "expected_sha256": "0",
                "folder": "", "prefix": "TK-", "name": "n", "spec": {"sources": []}, "thread": format!("{src}:D-001"),
                "topic": "t", "subjects": [], "participants": [], "opening_post": {"author": "a", "body": "a long enough body"},
                "author": "a", "kind": "claim", "body": "a long enough body", "verdict_body": "v", "summary": "s",
                "open_questions": [], "positions": [], "settle_by": "s", "decided_by": "d", "decision": "d"
            });
            for tool in &writes {
                let schema = tools.iter().find(|t| t["name"] == tool.as_str()).unwrap()["inputSchema"]["properties"].clone();
                let mut a = atlas::json::Map::new();
                for (k, v) in args.as_object().unwrap() {
                    if schema.get(k).is_some() {
                        a.insert(k.clone(), v.clone());
                    }
                }
                if schema.get("reason").is_some() {
                    a.insert("reason".into(), json!("attempting to write into a read-only source"));
                }
                let r = s.call(tool, &Value::Object(a));
                assert!(r.is_err(), "{tool} on {src}/{file} succeeded: {r:?}");
            }
        }
    }
    let after: Vec<_> = ["handbook", "memory"].iter().map(|d| snapshot(&t.path().join(d))).collect();
    assert!(before == after, "a source file changed");
    // Structurally: sources are not vaults, and the only openers in the
    // sources module read.
    let src = include_str!("../src/sources.rs");
    for w in ["OpenOptions", "write(", "create(", "rename(", "remove_file", "set_len"] {
        assert!(!src.contains(w), "sources.rs uses {w}");
    }
}

/// Property: a lookup checks every source that holds the identifier,
/// whatever its mention limit. A stale file whose only rows fall past the
/// limit is still reported, and refreshed on the spot, so a definition it
/// gained on disk shows at every limit.
#[test]
fn lookup_freshness_covers_sources_past_the_mention_limit() {
    let t = Temp::new("a-fresh-limit");
    let cfg = corpus(t.path());
    // An undated mention, so it sorts after every dated one in the vault.
    let late = t.path().join("handbook/LATE.md");
    write(&late, "# Late

Cites TK-001 in passing.
");
    let mut s = open(&cfg);
    let all = call(&mut s, "lookup", json!({"id": "TK-001", "limit": 500, "refresh": "off"}));
    // Every limit up to LATE.md's place in the full list leaves it unshown.
    let place = all["mentions"].as_array().unwrap().iter().position(|h| h["path"] == "LATE.md").unwrap();
    assert!(place >= 2, "LATE.md sorts behind other mentions: {all}");
    std::thread::sleep(Duration::from_millis(20));
    fs::write(&late, "# Late

- TK-001: now introduced here.
").unwrap();
    for limit in 0..=place {
        let r = call(&mut s, "lookup", json!({"id": "TK-001", "limit": limit, "refresh": "check"}));
        let shown = r["definitions"].as_array().unwrap().iter().chain(r["mentions"].as_array().unwrap());
        assert!(!shown.clone().any(|h| h["path"] == "LATE.md"), "limit {limit} shows LATE.md, so it tests nothing: {r}");
        // An answer whose checked sources are all fresh carries no freshness.
        let hb = r["freshness"].as_array().into_iter().flatten().find(|f| f["source"] == "handbook").cloned();
        let hb = hb.unwrap_or_else(|| panic!("limit {limit}: handbook holds TK-001 but was not checked: {r}"));
        assert_eq!(hb["stale"][0]["path"], json!("LATE.md"), "limit {limit}: {r}");
    }
    let r = call(&mut s, "lookup", json!({"id": "TK-001", "limit": 0}));
    let defs: Vec<&str> = r["definitions"].as_array().unwrap().iter().map(|h| h["path"].as_str().unwrap()).collect();
    assert!(defs.contains(&"LATE.md"), "auto refreshed the file past the limit: {r}");
}

#[test]
fn freshness_reports_and_refreshes_stale_files() {
    let t = Temp::new("a-fresh");
    let cfg = corpus(t.path());
    let mut s = open(&cfg);
    let rules = t.path().join("handbook/RULES.md");
    std::thread::sleep(Duration::from_millis(20));
    fs::write(&rules, format!("{RULES}- **TK-006**: a rule added on disk.\n")).unwrap();
    write(&t.path().join("handbook/NEW.md"), "# New\n\nTK-007 arrived.\n");

    let r = call(&mut s, "lookup", json!({"id": "TK-006", "refresh": "off"}));
    assert_eq!(r["found"], json!(false), "off does not look at the disk");
    let r = call(&mut s, "lookup", json!({"id": "TK-006", "refresh": "check"}));
    assert_eq!(r["found"], json!(false));
    let hb = r["freshness"].as_array().unwrap().iter().find(|f| f["source"] == "handbook").unwrap().clone();
    let stale: Vec<(&str, &str)> = hb["stale"].as_array().unwrap().iter().map(|f| (f["path"].as_str().unwrap(), f["state"].as_str().unwrap())).collect();
    assert_eq!(stale, vec![("NEW.md", "new"), ("RULES.md", "changed")], "{r}");
    assert_eq!(hb["refreshed"], json!(false));

    let r = call(&mut s, "lookup", json!({"id": "TK-006"}));
    assert_eq!(r["found"], json!(true), "auto re-indexes on the spot: {r}");
    assert!(r["freshness"].as_array().unwrap().iter().any(|f| f["refreshed"] == json!(true)));
    let r = call(&mut s, "lookup", json!({"id": "TK-007"}));
    assert_eq!(r["found"], json!(true));

    // A removed file leaves the index.
    fs::remove_file(t.path().join("handbook/NEW.md")).unwrap();
    let r = call(&mut s, "lookup", json!({"id": "TK-007"}));
    assert_eq!(r["found"], json!(false), "{r}");

    // A new memory file is a new name: every document is re-read for it.
    write(&t.path().join("memory/gamma-slug.md"), "---\nname: gamma-slug\n---\nGamma.\n");
    write(&t.path().join("Notes/Gamma user.md"), "Uses gamma-slug.\n");
    s.store_mut().rescan_sources(None).unwrap();
    let r = call(&mut s, "lookup", json!({"id": "gamma-slug"}));
    assert!(hits(&r, "mentions").iter().any(|m| m.1 == "Gamma user.md"), "{r}");
}

/// A rebuild re-indexes every file at once. The freshness note past
/// `STALE_LIST_CAP` files is one count line with the reason, so the answer
/// stays the size of the answer, not the size of the corpus.
#[test]
fn freshness_after_a_rebuild_is_a_count_not_a_file_list() {
    let t = Temp::new("a-fresh-cap");
    let cfg = corpus(t.path());
    const BULK: usize = 300;
    for i in 0..BULK {
        write(&t.path().join(format!("handbook/BULK-{i:03}.md")), &format!("# Bulk {i}\n\nCites TK-003 in passing.\n"));
    }
    let db = t.path().join("state/atlas.sqlite");
    drop(open(&cfg));
    // An older derived schema on disk: the next open wipes the atlas rows.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("UPDATE meta SET value = 'older' WHERE key IN ('atlas', 'atlas_schema'); DELETE FROM meta WHERE key = 'open_stamp';")
            .unwrap();
    }
    let reopen = || {
        let c = Config::load(&cfg).unwrap();
        let specs: Vec<(String, PathBuf)> = c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
        let store = Store::open_with(c.db.as_ref().unwrap(), Vaults::new(&specs).unwrap(), AtlasSetup::from_config(&c).unwrap()).unwrap();
        let mut s = Server::new(store);
        s.set_client("test-client");
        s
    };
    let mut s = reopen();
    let text = s.answer("lookup", &json!({"id": "TK-003", "limit": 5})).unwrap().into_text();
    let stale: Vec<&str> = text.lines().filter(|l| l.starts_with("stale ")).collect();
    let line = format!("stale handbook {} files (schema rebuild), reindexed", BULK + 3);
    assert!(stale.contains(&line.as_str()), "{text}");
    // One count line for the handbook; the three notes and two memory files
    // are under the cap and keep their own lines.
    assert!(stale.len() <= 1 + 3 + 2, "{stale:?}");
    // The bound: without the cap this answer carried a line per file
    // (about 40 bytes each, 12 KB here).
    assert!(text.len() < 2048, "{} bytes:\n{text}", text.len());

    // Past the cap without a rebuild, the reason is the count per state.
    drop(s);
    let mut s = reopen();
    std::thread::sleep(Duration::from_millis(20));
    for i in 0..20 {
        write(&t.path().join(format!("handbook/BULK-{i:03}.md")), &format!("# Bulk {i}\n\nEdited, still cites TK-003.\n"));
    }
    let r = call(&mut s, "lookup", json!({"id": "TK-003", "refresh": "check", "format": "json"}));
    let hb = r["freshness"].as_array().unwrap().iter().find(|f| f["source"] == "handbook").unwrap().clone();
    assert_eq!((hb["stale_count"].as_i64(), hb["reason"].as_str()), (Some(20), Some("20 changed")), "{r}");
    assert!(hb.get("stale").is_none(), "no per-file list past the cap: {r}");
}

#[test]
fn misses_are_recorded_listed_resolved_and_survive_the_database() {
    let t = Temp::new("a-misses");
    let cfg = corpus(t.path());
    let mut s = open(&cfg);
    let r = call(
        &mut s,
        "report_miss",
        json!({"tool": "lookup", "query": "TK-004", "returned": "nothing", "found": "handbook/deep/NESTED.md:3",
               "reason": "the include glob does not reach subfolders"}),
    );
    let id = r["miss"]["id"].as_i64().unwrap();
    let e = call_err(&mut s, "report_miss", json!({"tool": "lookup", "query": "x", "found": "y", "reason": "short"}));
    assert!(e.contains("reason"), "{e}");
    assert_eq!(call(&mut s, "misses", json!({}))["total"], json!(1));
    // Not fixed yet: resolving re-runs the query and refuses, naming the answer.
    let e = call_err(&mut s, "resolve_miss", json!({"id": id, "reason": "claimed fixed without a change"}));
    assert!(e.contains("is not fixed") && e.contains("NESTED.md"), "{e}");
    // Fixed: the query now reaches the file that was found.
    write(&t.path().join("handbook/NESTED.md"), "# TK-004 Nested\n\nMoved up where the glob reaches.\n");
    call(&mut s, "resolve_miss", json!({"id": id, "reason": "widened the glob to **/*.md in the config"}));
    assert_eq!(call(&mut s, "misses", json!({}))["total"], json!(0));
    let all = call(&mut s, "misses", json!({"status": "all"}));
    assert_eq!(all["misses"][0]["status"], json!("resolved"));
    assert_eq!(all["misses"][0]["resolution"], json!("widened the glob to **/*.md in the config"));
    let e = call_err(&mut s, "resolve_miss", json!({"id": id, "reason": "resolving it a second time"}));
    assert!(e.contains("already resolved"), "{e}");
    drop(s);
    let db = t.path().join("state/atlas.sqlite");
    for ext in ["", "-wal", "-shm"] {
        let p = db.with_file_name(format!("atlas.sqlite{ext}"));
        if p.exists() {
            fs::remove_file(&p).unwrap();
        }
    }
    let mut s = open(&cfg);
    let all = call(&mut s, "misses", json!({"status": "all"}));
    assert_eq!(all["total"], json!(1), "restored from misses.jsonl: {all}");
    assert_eq!(all["misses"][0]["status"], json!("resolved"));
}

#[test]
fn recall_atlas_finds_a_superset_of_a_plain_scan() {
    let t = Temp::new("a-recall");
    let cfg = corpus(t.path());
    let s = open(&cfg);
    // The check itself, over the whole corpus, with extra phrases.
    let phrases = vec!["owner is checked".to_string(), "Cache body".to_string(), "alpha memory".to_string()];
    let mut s = s;
    let r = atlas::recall::check(s.store_mut(), &phrases).unwrap();
    assert!(r.complete(), "{:#?}", r);
    assert!(r.id_occurrences >= 20 && r.phrases >= 10 && r.docs == 8, "{r:?}");

    // An independent scan in this test: every configured regex, line by
    // line, over the raw files on disk.
    let c = Config::load(&cfg).unwrap();
    let res: Vec<atlas::regex::Regex> = c.ids.iter().map(|p| atlas::regex::Regex::new(&p.regex).unwrap()).collect();
    for (source, path) in s.store_mut().docs().unwrap() {
        let root = match source.as_str() {
            "notes" => t.path().join("Notes"),
            other => t.path().join(other),
        };
        let text = fs::read_to_string(root.join(&path)).unwrap();
        for (li, line) in text.lines().enumerate() {
            for re in &res {
                for cap in re.captures_iter(line) {
                    let m = cap.name("id").unwrap_or_else(|| cap.get(0).unwrap());
                    let l = s.store_mut().lookup(m.as_str(), 10_000, DefText::None).unwrap();
                    let line = li as i64 + 1;
                    // Repeats in one section are merged into one line with a
                    // count; the section range covers them.
                    let found = l.definitions.iter().chain(&l.mentions).any(|h| {
                        h.source == source && h.path == path && (h.line == line || h.count > 1 && (h.section_start..=h.section_end).contains(&line))
                    });
                    assert!(found, "{} at {source}:{path}:{} not returned", m.as_str(), li + 1);
                }
            }
        }
    }

    // The check can fail: drop one occurrence from the index and it is named.
    s.store_mut().conn().execute("DELETE FROM idents WHERE ident = 'TK-003' AND line = 7", []).unwrap();
    s.store_mut().conn().execute("DELETE FROM lookups WHERE fid = 'tk-003' AND line = 7", []).unwrap();
    let r = atlas::recall::check(s.store_mut(), &[]).unwrap();
    assert_eq!(r.id_misses.len(), 1, "{r:?}");
    assert_eq!((r.id_misses[0].id.as_str(), r.id_misses[0].line, r.id_misses[0].stage), ("tk-003", 7, "index"));
}

#[test]
fn binary_serves_from_a_config_and_runs_the_recall_check() {
    let t = Temp::new("a-bin");
    let cfg = corpus(t.path());
    let out = std::process::Command::new(bin()).arg("--config").arg(&cfg).arg("--recall-check").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("misses: 0 identifier, 0 phrase"), "{text}");

    let mut child = std::process::Command::new(bin())
        .arg("--config")
        .arg(&cfg)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::{BufRead, Write};
        let mut stdin = child.stdin.take().unwrap();
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"clientInfo": {"name": "t"}}});
        let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "lookup", "arguments": {"id": "TK-003"}}});
        let as_json = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "lookup", "arguments": {"id": "TK-003", "format": "json"}}});
        writeln!(stdin, "{init}\n{call}\n{as_json}").unwrap();
        drop(stdin);
        let lines: Vec<Value> =
            std::io::BufReader::new(child.stdout.take().unwrap()).lines().map(|l| atlas::json::parse(&l.unwrap()).unwrap()).collect();
        assert_eq!(lines[0]["result"]["serverInfo"]["name"], json!("atlas"));
        assert!(lines[0]["result"]["instructions"].as_str().unwrap().contains("Read-only sources: handbook, memory."));
        // Plain text by default: the definitions table first, one answer
        // only (no structuredContent copy).
        let text = lines[1]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("defs ") && text.contains("\nhandbook:RULES.md:"), "{text}");
        assert!(lines[1]["result"].get("structuredContent").is_none());
        let r: Value = atlas::json::parse(lines[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(r["definitions"][0]["source"], json!("handbook"), "{r}");
    }
    assert!(child.wait().unwrap().success());
}

/// Every identifier pattern of a real configuration compiles under the
/// owned regex subset. The configuration is named by `ATLAS_REAL_CONFIG`
/// (it lives outside the repository); without it the test is skipped with a
/// notice.
#[test]
fn every_configured_pattern_of_the_real_config_compiles() {
    let Some(p) = std::env::var_os("ATLAS_REAL_CONFIG").filter(|v| !v.is_empty()) else {
        eprintln!("skipped: ATLAS_REAL_CONFIG is not set");
        return;
    };
    let c = Config::load(Path::new(&p)).unwrap();
    assert!(!c.ids.is_empty(), "the configuration has no id patterns");
    for spec in &c.ids {
        let built = atlas::regex::RegexBuilder::new(&spec.regex).case_insensitive(spec.case_insensitive).build();
        assert!(built.is_ok(), "pattern {} ({}): {}", spec.name, spec.regex, built.err().map(|e| e.msg).unwrap_or_default());
    }
    AtlasSetup::from_config(&c).unwrap();
}

/// Every identifier the index holds, folded, plus a case variant of each.
fn every_id(s: &mut Server) -> Vec<String> {
    let conn = s.store_mut().conn();
    let mut st = conn.prepare("SELECT DISTINCT lower(ident) FROM idents ORDER BY 1").unwrap();
    let ids: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    ids.iter().flat_map(|i| [i.clone(), i.to_ascii_uppercase()]).collect()
}

fn assert_lookup_matches_reference(s: &mut Server, stage: &str) {
    let ids = every_id(s);
    assert!(ids.len() > 20, "{stage}: {} ids", ids.len());
    for id in &ids {
        for limit in [0, 1, 3, 8, 1_000_000] {
            let store = s.store_mut();
            // The stored definition text against the text read back from
            // the sections, for every definition, the first only, and none.
            for text in [DefText::All, DefText::First, DefText::None] {
                let new = store.lookup(id, limit, text).unwrap();
                let old = store.lookup_reference(id, limit, text).unwrap();
                assert_eq!(new, old, "{stage}: lookup {id} limit {limit} text {text:?}");
            }
        }
    }
}

#[test]
fn lookup_table_matches_the_reference_on_every_id_through_edits() {
    let t = Temp::new("a-lookup-table");
    let cfg = corpus(t.path());
    // Sixty list-item definitions of one id (past the 50 definition cap),
    // and case variants of a name merged within one section.
    let many: String = (0..60).map(|i| format!("- TK-010: rule {i}, 2026-01-{:02}\n", i % 28 + 1)).collect();
    write(&t.path().join("handbook/MANY.md"), &format!("# Many\n\n{many}\n## Names\n\nAlpha-Slug and alpha-slug and ALPHA-SLUG.\n"));
    // Every shape the stored definition text is cut for: a table row, a
    // heading whose section is past the text cap (multi-byte text at the
    // cut), a file definition whose first section is past the cap (so the
    // next is not appended), one with no next section, and a heading in a
    // document's last section.
    let big = "A long line with \u{e9}\u{e8} in it, repeated past the cap.\n".repeat(200);
    write(
        &t.path().join("handbook/SHAPES.md"),
        &format!("# Shapes\n\n| id | what |\n|---|---|\n| TK-020 | a row |\n\n## TK-021\n\n{big}\n## TK-022\n\nLast section.\n"),
    );
    write(&t.path().join("Notes/Tickets/TK-023 Big.md"), &format!("---\nid: TK-023\n---\n{big}# After\n\nMore.\n"));
    write(&t.path().join("Notes/Tickets/TK-024 Lone.md"), "---\nid: TK-024\n---\nOnly a preamble.\n");
    let mut s = open(&cfg);
    let kinds: Vec<String> = {
        let conn = s.store_mut().conn();
        let mut st = conn.prepare("SELECT DISTINCT def FROM lookups WHERE def IS NOT NULL ORDER BY 1").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    for k in ["file", "frontmatter", "heading", "list_item", "table_row"] {
        assert!(kinds.iter().any(|x| x == k), "no {k} definition in the fixture: {kinds:?}");
    }
    let capped: i64 = s.store_mut().conn().query_row("SELECT MAX(length(CAST(text AS BLOB))) FROM lookups", [], |r| r.get(0)).unwrap();
    assert!(capped > 4096 && capped <= atlas::store::MAX_SECTION_TEXT as i64, "longest stored text {capped}");
    assert_lookup_matches_reference(&mut s, "fresh index");
    let l = s.store_mut().lookup("tk-010", 8, DefText::None).unwrap();
    assert_eq!(l.definitions.len(), 50);

    std::thread::sleep(Duration::from_millis(20));
    // Changed, touched (same bytes, new mtime), removed and added files, in a
    // source and in the vault.
    fs::write(t.path().join("handbook/RULES.md"), format!("{RULES}- **TK-006**: added, cites TK-003 and TK-001.\n")).unwrap();
    fs::write(t.path().join("handbook/GUIDE.md"), GUIDE).unwrap();
    fs::remove_file(t.path().join("handbook/notes.txt")).unwrap();
    write(&t.path().join("handbook/LATE.md"), "# Late\n\n## 2026-05-01\n\nTK-001 again, and TK-001 twice in one section.\n");
    fs::write(t.path().join("Notes/Log.md"), format!("{LOG}\n## 2026-04-02\n\n- TK-001 and TK-003 together\n")).unwrap();
    fs::write(t.path().join("Notes/Tickets/TK-002 Cache.md"), "---\nid: TK-002\n---\n# TK-002 Cache\n\nCache body, cited by TK-001.\n").unwrap();
    s.store_mut().rescan().unwrap();
    s.store_mut().rescan_sources(None).unwrap();
    assert_lookup_matches_reference(&mut s, "after edits");

    fs::remove_file(t.path().join("handbook/MANY.md")).unwrap();
    s.store_mut().rescan_sources(None).unwrap();
    assert_lookup_matches_reference(&mut s, "after a removal");
    let l = s.store_mut().lookup("TK-010", 8, DefText::None).unwrap();
    assert!(l.definitions.is_empty() && l.mentions.is_empty() && l.total_mentions == 0 && l.mention_lines == 0, "{l:?}");
}

#[test]
fn shipped_generic_fixture_passes_the_recall_check() {
    // fixtures/generic is what cargo xtask verify-macos checks on a Mac; it
    // must stay complete everywhere.
    let t = Temp::new("a-generic-fixture");
    let cfg = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/generic/atlas.json");
    let db = t.path().join("atlas.sqlite");
    let out = std::process::Command::new(bin()).arg("--config").arg(&cfg).arg("--db").arg(&db).arg("--recall-check").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("misses: 0 identifier, 0 phrase"), "{text}");
    assert!(text.contains("recall: 7 documents"), "{text}");
}

/// An index an older binary wrote (its schema stamp is the bare table
/// version) is rebuilt on open, atlas rows included: an update re-reads
/// every file instead of keeping the old binary's reading of it.
#[test]
fn an_index_from_an_older_binary_rebuilds_itself() {
    let t = Temp::new("a-older-binary");
    let cfg = corpus(t.path());
    let db = t.path().join("state/atlas.sqlite");
    drop(open(&cfg));
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let schema: String = conn.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| r.get(0)).unwrap();
        assert!(schema.contains("/parse-") && schema.ends_with(env!("CARGO_PKG_VERSION")), "{schema}");
        // What an older binary left: its stamp, and a row its rules produced.
        conn.execute_batch(
            "UPDATE meta SET value = '1' WHERE key = 'schema'; DELETE FROM meta WHERE key = 'open_stamp';
             CREATE TEMP TABLE x AS SELECT * FROM idents LIMIT 1; UPDATE x SET ident = 'STALE-1'; INSERT INTO idents SELECT * FROM x;",
        )
        .unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM idents WHERE ident = 'STALE-1'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }
    let mut s = open(&cfg);
    let n: i64 = s.store_mut().conn().query_row("SELECT COUNT(*) FROM idents WHERE ident = 'STALE-1'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "the older binary's rows are gone");
    let defs: i64 = s.store_mut().conn().query_row("SELECT COUNT(*) FROM idents WHERE ident = 'TK-003' AND rank > 0", [], |r| r.get(0)).unwrap();
    assert!(defs > 0, "and the files are read again");
}

/// `section` returns one section exactly, by heading or by the id it
/// defines; `grep` searches the indexed files with context, as grep prints.
#[test]
fn section_and_grep_are_the_cheapest_reads() {
    let t = Temp::new("a-core-verbs");
    let cfg = corpus(t.path());
    let mut s = open(&cfg);
    let text = s.answer("section", &json!({"heading": "Caching", "source": "handbook"})).unwrap().into_text();
    assert!(text.starts_with("handbook/GUIDE.md:3-"), "{text}");
    assert!(text.contains("The cache section explains eviction in detail.") && !text.contains("introduced the eviction"), "{text}");
    let text = s.answer("section", &json!({"id": "BIG-WORK"})).unwrap().into_text();
    assert!(text.contains("The work item that introduced the eviction rules."), "{text}");
    assert!(s.call("section", &json!({"heading": "a", "id": "b"})).is_err(), "one of heading or id");
    let text = s.answer("section", &json!({"heading": "No such heading"})).unwrap().into_text();
    assert!(text.starts_with("no section"), "{text}");
    let text = s.answer("grep", &json!({"pattern": "eviction", "source": "handbook", "context": 1})).unwrap().into_text();
    assert!(text.contains("handbook/GUIDE.md:5:The cache section explains eviction in detail."), "{text}");
    assert!(text.contains("handbook/GUIDE.md-4-"), "context lines: {text}");
    let text = s.answer("grep", &json!({"pattern": "zzz-not-there"})).unwrap().into_text();
    assert!(text.contains("no matches"), "{text}");
    assert!(s.call("grep", &json!({"pattern": "("})).is_err());
}

/// A memory folder's index file (`MEMORY.md`, the memory export's `index`)
/// lists the memories and is not one: its stem is no name, so the word
/// "memory" in every document is no identifier. And the recall check on a
/// database whose code roots were never indexed reports them as not
/// indexed and carries on, as the `symbol` tool does.
#[test]
fn the_memory_index_is_no_name_and_an_unbuilt_code_index_is_reported() {
    let t = Temp::new("a-memidx");
    let root = t.path();
    let mem = root.join("memory");
    write(&mem.join("MEMORY.md"), "- [Alpha slug](alpha-slug.md) - the alpha rule\n");
    write(&mem.join("alpha-slug.md"), "---\nname: alpha-slug\n---\nAlpha rule.\n");
    write(&root.join("handbook").join("guide.md"), "# Memory\n\n| Memory | 41 KB |\n|---|---|\n\n- Memory: the bank tables.\n\nSee alpha-slug.\n");
    write(&root.join("app").join("src").join("lib.rs"), "pub fn alpha() {}\n");
    write(&root.join("Notes").join("Note.md"), "# Note\n");
    let cfg = json!({
        "db": "state/atlas.sqlite",
        "watch": false,
        "vaults": [{"name": "notes", "path": "Notes"}],
        "sources": [
            {"name": "handbook", "root": "handbook", "include": ["*.md"]},
            {"name": "memory", "root": "memory", "include": ["*.md"], "id_keys": ["name"], "names_kind": "memory"}
        ],
        "code": {"roots": [{"name": "app", "root": "app"}]},
        "memory": {
            "store": "state/memory.sqlite",
            "export_dir": "out",
            "exports": [{"name": "memory", "shape": "memory", "path": "memory", "index": "MEMORY.md"}]
        }
    });
    let path = root.join("atlas.json");
    fs::write(&path, atlas::json::to_string_pretty(&cfg)).unwrap();
    let c = Config::load(&path).unwrap();
    assert_eq!(AtlasSetup::from_config(&c).unwrap().name_indexes, vec![("memory".to_string(), "MEMORY.md".to_string())]);
    let mut s = open(&path);
    let store = s.store_mut();
    let l = store.lookup("memory", 1000, atlas::store::DefText::None).unwrap();
    // The file is a note like any other (its stem, by wikilink), not a
    // name: the word in the handbook's heading, row and item is no hit.
    assert!(l.mentions.is_empty(), "the index's stem is no name: {l:?}");
    assert_eq!(l.definitions.iter().map(|h| (h.path.as_str(), h.kind.as_str())).collect::<Vec<_>>(), vec![("MEMORY.md", "note")], "{l:?}");
    let l = store.lookup("alpha-slug", 1000, atlas::store::DefText::None).unwrap();
    assert!(l.definitions.iter().any(|h| h.source == "memory" && h.path == "alpha-slug.md"), "{l:?}");
    assert!(l.mentions.iter().any(|h| h.source == "handbook"), "{l:?}");

    let out = std::process::Command::new(bin()).arg("--config").arg(&path).arg("--recall-check").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("misses: 0 identifier, 0 phrase"), "{text}");
    assert!(text.contains("code recall: not indexed yet, not checked: app"), "{text}");
}
