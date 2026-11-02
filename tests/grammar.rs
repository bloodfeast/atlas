//! The whole-corpus recall test: id-shaped tokens in defining positions,
//! checked against the index; unmatched shapes suggested, never adopted;
//! the first-index check once per configuration; accept and reject.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use atlas::config::Config;
use atlas::grammar;
use atlas::paths::Vaults;
use atlas::store::{AtlasSetup, Store};
use common::*;

/// A corpus with one configured pattern (tickets) and three unconfigured
/// id shapes in defining positions: a dated heading, dated list items with
/// a letter suffix, and table rows. Mentions in prose are not counted.
fn corpus(root: &Path) -> PathBuf {
    write(&root.join("notes/TK-001 Login.md"), "# TK-001 Login\n\nThe login flow.\n");
    write(
        &root.join("notes/Log.md"),
        "# Log\n\n## 2026-10-23 ALPHA-REWORK\n\nText that mentions ZED-9 in passing.\n\n- 2026-10-30 TASK-28n: started the task\n- TASK-29: queued\n- see TASK-30 for the rest\n\n| id | what |\n|---|---|\n| TF-12 | first |\n| TF-13 | second |\n",
    );
    // A source whose id key is `name`: its `id:` line is a defining
    // position to the scan but not to the index, a configuration mismatch.
    write(&root.join("mem/entry.md"), "---\nname: entry\nid: TK-009\n---\n\nBody.\n");
    let cfg = root.join("atlas.json");
    write(
        &cfg,
        r#"{
  "db": "state/atlas.sqlite",
  "vaults": [{"name": "notes", "path": "notes"}],
  "sources": [{"name": "mem", "root": "mem", "include": ["*.md"], "id_keys": ["name"]}],
  "ids": [{"name": "ticket", "regex": "\\bTK-[0-9]{3}\\b"}],
  "watch": false
}
"#,
    );
    cfg
}

fn open(cfg: &Path) -> (Store, PathBuf) {
    let c = Config::load(cfg).unwrap();
    let db = c.db.clone().unwrap();
    let vaults = Vaults::new(&c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect::<Vec<_>>()).unwrap();
    let store = Store::open_with(&db, vaults, AtlasSetup::from_config(&c).unwrap()).unwrap();
    (store, db)
}

fn atlas(cfg: &Path, home: &Path, args: &[&str]) -> (bool, String) {
    // A subcommand is argv[1]; its configuration comes from ATLAS_CONFIG.
    let o = Command::new(bin())
        .args(args)
        .env("ATLAS_CONFIG", cfg)
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .unwrap();
    (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

#[test]
fn the_scan_suggests_unmatched_shapes_and_fails_configured_misses() {
    let t = Temp::new("grammar-scan");
    let cfg = corpus(t.path());
    let (mut store, _) = open(&cfg);
    store.rescan().unwrap();
    store.rescan_sources(None).unwrap();
    let r = grammar::scan(&store, &HashSet::new()).unwrap();
    assert_eq!(r.docs, 3);
    let got: Vec<(&str, &str, usize, usize)> = r.suggestions.iter().map(|s| (s.kind.as_str(), s.regex.as_str(), s.count, s.ids)).collect();
    assert_eq!(
        got,
        [("task", r"\bTASK-\d+[a-z]?\b", 2, 2), ("tf", r"\bTF-\d+\b", 2, 2), ("alpha", r"\bALPHA-REWORK\b", 1, 1)],
        "TASK-30 is cited, not introduced, and ZED-9 is a mention: {:?}",
        r.suggestions
    );
    assert_eq!(r.suggestions[0].samples, ["notes/Log.md:7", "notes/Log.md:8"]);
    assert_eq!(r.failures.len(), 1, "{:?}", r.failures);
    assert!(r.failures[0].starts_with("TK-009 at mem/entry.md:3: pattern ticket matches it"), "{}", r.failures[0]);
    // TK-001 (file and heading) is indexed as a definition.
    assert!(r.covered >= 1);
    // A rejected regex is not suggested.
    let rej: HashSet<String> = [r"\bTF-\d+\b".to_string()].into_iter().collect();
    assert!(grammar::scan(&store, &rej).unwrap().suggestions.iter().all(|s| s.kind != "tf"));
    assert_eq!(grammar::recall_check(&store).unwrap(), r.failures);
}

#[test]
fn the_first_index_check_logs_once_per_configuration() {
    let t = Temp::new("grammar-first");
    let cfg = corpus(t.path());
    let (mut store, db) = open(&cfg);
    // It indexes by itself: the store was never scanned.
    let r = grammar::first_index_check(&mut store, &db).unwrap().expect("first run checks");
    assert_eq!((r.failures.len(), r.suggestions.len()), (1, 3));
    // Only the failure is a miss; suggestions are not.
    let (open, _) = store.misses(Some("open"), 100).unwrap();
    let g: Vec<_> = open.iter().filter(|m| m.tool == "grammar").collect();
    assert_eq!(g.len(), 1, "{g:?}");
    assert!(g[0].query.starts_with("TK-009 at mem/entry.md:3"), "{}", g[0].query);
    assert!(grammar::first_index_check(&mut store, &db).unwrap().is_none(), "same configuration: not again");
    // A changed configuration checks again, without logging duplicates.
    std::fs::remove_file(db.parent().unwrap().join(grammar::CHECK_STATE)).unwrap();
    assert!(grammar::first_index_check(&mut store, &db).unwrap().is_some());
    let (open, _) = store.misses(Some("open"), 100).unwrap();
    assert_eq!(open.iter().filter(|m| m.tool == "grammar").count(), 1);
}

#[test]
fn listing_twice_files_no_misses_and_old_suggestion_misses_are_resolved() {
    let t = Temp::new("grammar-twice");
    let cfg = corpus(t.path());
    std::fs::create_dir_all(t.path().join("claude")).unwrap();
    let (mut store, db) = open(&cfg);
    // A suggestion an older version filed as a miss.
    store.report_miss("grammar", r"\bTF-\d+\b", "", "notes/Log.md:12", "2 defining occurrence(s) of an id shape no pattern matches", "atlas").unwrap();
    grammar::first_index_check(&mut store, &db).unwrap().expect("first run checks");
    let before = store.misses(None, 1000).unwrap().1;
    let (open_now, _) = store.misses(Some("open"), 100).unwrap();
    assert_eq!(open_now.iter().filter(|m| m.tool == "grammar").count(), 1, "only the failure stays open: {open_now:?}");
    drop(store);
    for _ in 0..2 {
        let (ok, out) = atlas(&cfg, t.path(), &["grammar", "list"]);
        assert!(!ok && out.contains("3 unmatched shape(s)"), "{out}");
    }
    let (store, _) = open(&cfg);
    assert_eq!(store.misses(None, 1000).unwrap().1, before, "list files no misses");
}

#[test]
fn accept_writes_the_pattern_with_a_backup_and_reject_hides_it() {
    let t = Temp::new("grammar-cli");
    let cfg = corpus(t.path());
    std::fs::create_dir_all(t.path().join("claude")).unwrap();
    let (ok, out) = atlas(&cfg, t.path(), &["grammar", "list"]);
    assert!(!ok, "a failure makes list exit 1: {out}");
    assert!(out.contains("3 unmatched shape(s)") && out.contains("FAIL TK-009"), "{out}");
    assert!(out.contains(r#"  1. kind "task" regex "\\bTASK-\\d+[a-z]?\\b": 2 hit(s)"#), "{out}");
    let before = std::fs::read_to_string(&cfg).unwrap();
    let (ok, out) = atlas(&cfg, t.path(), &["grammar", "accept", "1"]);
    assert!(ok, "{out}");
    assert!(out.contains("+  \"ids\": [{\"name\": \"ticket\""), "the diff is shown: {out}");
    let c = Config::load(&cfg).unwrap();
    assert_eq!(c.ids.len(), 2);
    assert_eq!((c.ids[1].name.as_str(), c.ids[1].regex.as_str()), ("task", r"\bTASK-\d+[a-z]?\b"));
    // Reindexed with it, TASK is no longer suggested; the definitions index.
    let (_, out) = atlas(&cfg, t.path(), &["grammar", "list"]);
    assert!(out.contains("2 unmatched shape(s)") && !out.contains("TASK"), "{out}");
    let (_, out) = atlas(&cfg, t.path(), &["grammar", "reject", "1"]);
    assert!(out.contains(r#"rejected "\\bTF-\\d+\\b""#), "{out}");
    let (_, out) = atlas(&cfg, t.path(), &["grammar", "list"]);
    assert!(out.contains("1 unmatched shape(s)") && !out.contains("TF-"), "{out}");
    let (ok, out) = atlas(&cfg, t.path(), &["grammar", "accept", "9"]);
    assert!(!ok && out.contains("no suggestion 9"), "{out}");
    // The accept is undone like any settings write.
    let (ok, out) = atlas(&cfg, t.path(), &["settings", "undo"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
}

#[test]
fn the_recall_check_fails_on_a_configured_shape_the_index_misses() {
    let t = Temp::new("grammar-rc");
    let cfg = corpus(t.path());
    let (ok, out) = atlas(&cfg, t.path(), &["--recall-check"]);
    assert!(!ok, "{out}");
    assert!(out.contains("grammar: TK-009 at mem/entry.md:3"), "{out}");
    // Without the mismatched file it passes; unmatched shapes alone never fail it.
    std::fs::remove_file(t.path().join("mem/entry.md")).unwrap();
    let (ok, out) = atlas(&cfg, t.path(), &["--recall-check"]);
    assert!(ok, "{out}");
}
