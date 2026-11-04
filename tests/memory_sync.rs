//! Keeping the memory store in step with the files it was imported from:
//! new, changed and removed items and memory files, history kept, records
//! with no source item left alone, the rescan gate, and drift.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use atlas::config::Config;
use atlas::json;
use atlas::memory::check::check;
use atlas::memory::export::{ExportSpec, Shape};
use atlas::memory::settings::Settings;
use atlas::memory::sync::{self, Target};
use atlas::memory::{import, Draft, MemoryStore, Place};
use atlas::paths::Vaults;
use atlas::store::{AtlasSetup, Store};
use common::*;

const RULES: &str = "# Rules\n\n## Storage\n- 2026-01-02 Alex: the widget cache is bounded at 64 entries.\n  It evicts least recently used first.\n- 2026-01-03 Blake: exports are written atomically through a temp file and a rename.\n\n## Other\n- 2026-01-04 Alex: the gamma queue drains oldest first.\n";
const INDEX: &str = "# Memory\n- [Widget cache](widget-cache.md) - the widget cache holds 64 entries, least recently used first\n- [Export rule](export-rule.md) - atomic exports\n";
const MEM_A: &str = "---\nname: widget-cache\ndescription: the widget cache bound\nmetadata:\n  type: feedback\n---\n\nThe widget cache holds 64 entries.\n";
const MEM_B: &str = "---\nname: export-rule\ndescription: atomic exports\nmetadata:\n  type: project\n---\n\nExports use temp and rename.\n";

struct Fix {
    _tmp: Temp,
    root: PathBuf,
    settings: Settings,
}

impl Fix {
    fn rules(&self) -> PathBuf {
        self.root.join("handbook").join("RULES.md")
    }
    fn mem(&self) -> PathBuf {
        self.root.join("memory")
    }
    fn targets(&self) -> Vec<Target> {
        sync::targets(&self.settings, &[("rules".into(), self.root.join("handbook")), ("memory".into(), self.mem())])
    }
}

fn fixture(tag: &str) -> Fix {
    let tmp = Temp::new(tag);
    let root = tmp.path().to_path_buf();
    write(&root.join("handbook").join("RULES.md"), RULES);
    write(&root.join("memory").join("MEMORY.md"), INDEX);
    write(&root.join("memory").join("widget-cache.md"), MEM_A);
    write(&root.join("memory").join("export-rule.md"), MEM_B);
    let mut settings = Settings::from_config(None, &root.join("state").join("index.sqlite")).unwrap();
    settings.export_dir = Some(root.join("out"));
    settings.exports = vec![
        ExportSpec { name: "memory".into(), shape: Shape::MemoryDir, path: "memory".into(), index: "MEMORY.md".into(), handoffs: false, index_cap: None, index_topics: None },
        ExportSpec { name: "rules".into(), shape: Shape::Items, path: "RULES.md".into(), index: String::new(), handoffs: false, index_cap: None, index_topics: None },
    ];
    settings.snapshot_secs = 0;
    Fix { _tmp: tmp, root, settings }
}

fn import_all(f: &Fix) -> MemoryStore {
    let mut store = MemoryStore::open(&f.settings.store).unwrap();
    let mut w = store.writer().unwrap();
    import::import(&mut w, &f.settings.exports[0], &f.mem(), "test").unwrap();
    import::import(&mut w, &f.settings.exports[1], &f.rules(), "test").unwrap();
    w.commit().unwrap();
    store
}

fn sync_all(store: &mut MemoryStore, f: &Fix) -> Vec<sync::Report> {
    f.targets().iter().map(|t| sync::sync(store, t).unwrap()).collect()
}

fn status(store: &MemoryStore, key: &str) -> String {
    store.head(key).unwrap().unwrap().status
}

fn active_rules(store: &MemoryStore) -> Vec<String> {
    store.heads(Some("rule"), None).unwrap().into_iter().filter(|h| h.status == "active").map(|h| h.body).collect()
}

#[test]
fn a_fresh_import_has_no_drift_and_a_sync_changes_nothing() {
    let f = fixture("sync-fresh");
    let mut store = import_all(&f);
    assert_eq!(f.targets().len(), 2);
    assert_eq!(sync::drift(&store, &f.targets()).unwrap(), Vec::<String>::new());
    let before = store.counts().unwrap();
    for r in sync_all(&mut store, &f) {
        assert_eq!((r.new, r.changed, r.removed, r.returned, r.placed), (0, 0, 0, 0, 0), "{r:?}");
    }
    assert_eq!(store.counts().unwrap(), before);
}

#[test]
fn items_new_changed_and_removed_keep_their_history() {
    let f = fixture("sync-items");
    let mut store = import_all(&f);
    let old_key = store.heads(Some("rule"), None).unwrap().into_iter().find(|h| h.body.contains("atomically")).unwrap().key;
    let gone_key = store.heads(Some("rule"), None).unwrap().into_iter().find(|h| h.body.contains("gamma")).unwrap().key;
    // Blake's rule is edited, the gamma rule removed, a new rule appended.
    let text = RULES
        .replace("written atomically through a temp file and a rename", "written atomically through a temp file, a flush and a rename")
        .replace("- 2026-01-04 Alex: the gamma queue drains oldest first.\n", "")
        + "- 2026-01-05 Casey on CACHE-WORK: the delta index is rebuilt nightly.\n";
    write(&f.rules(), &text);

    let drift = sync::drift(&store, &f.targets()).unwrap();
    assert_eq!(drift.len(), 3, "{drift:?}");
    assert!(drift.iter().any(|d| d.starts_with("rules new rule")), "{drift:?}");
    assert!(drift.iter().any(|d| d.starts_with(&format!("rules changed {old_key}"))), "{drift:?}");
    assert!(drift.iter().any(|d| d.starts_with(&format!("rules removed {gone_key}"))), "{drift:?}");

    let reps = sync_all(&mut store, &f);
    let r = reps.iter().find(|r| r.export == "rules").unwrap();
    assert_eq!((r.new, r.changed, r.removed), (1, 1, 1));
    assert_eq!(sync::drift(&store, &f.targets()).unwrap(), Vec::<String>::new());

    // The edit: the old record answers as superseded, by a record holding the new text.
    let old = store.head(&old_key).unwrap().unwrap();
    assert_eq!(old.status, "superseded");
    let by = old.superseded_by.clone().unwrap();
    assert!(store.head(&by).unwrap().unwrap().body.contains("a flush and a rename"));
    assert!(old.body.contains("a temp file and a rename"));
    // The removal: retired, every version kept, out of the export and of search.
    assert_eq!(status(&store, &gone_key), "retired");
    assert!(store.head(&gone_key).unwrap().unwrap().body.contains("gamma"));
    assert!(store.exports_of(&gone_key).unwrap().is_empty());
    let hits = store.search(&["gamma".to_string()], None, None, 10).unwrap();
    assert!(hits.iter().all(|(h, _, _)| h.key != gone_key));
    // The new item: the date and author it states.
    let new = store.heads(Some("rule"), None).unwrap().into_iter().find(|h| h.body.contains("delta index")).unwrap();
    let v = store.history(&new.key).unwrap();
    assert_eq!(v[0].author, "Casey");
    assert_eq!(v[0].time, "2026-01-05T00:00:00.000Z");
    assert_eq!(active_rules(&store).len(), 3);
    // The integrity check (chains and the .jsonl copy, retire lines included) holds.
    let rep = check(&store, &f.settings.exports, None, true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
    // A second sync has nothing to do.
    for r in sync_all(&mut store, &f) {
        assert_eq!((r.new, r.changed, r.removed), (0, 0, 0), "{r:?}");
    }
}

#[test]
fn new_items_are_placed_where_the_source_has_them() {
    let f = fixture("sync-place");
    let mut store = import_all(&f);
    let text = RULES.replace("## Other\n", "## Other\n- 2026-01-06 Alex: a rule at the head of Other.\n");
    write(&f.rules(), &text);
    sync_all(&mut store, &f);
    let rendered = atlas::memory::export::render(&store, &f.settings.exports[1]).unwrap();
    assert_eq!(rendered[0].1, text);
}

#[test]
fn records_with_no_source_item_are_left_alone() {
    let f = fixture("sync-alone");
    let mut store = import_all(&f);
    let mut w = store.writer().unwrap();
    let d = Draft { title: "a store-only rule".into(), body: "- a rule written through remember\n".into(), reason: "remembered".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
    w.create("rules:r00099", "rule", "", "rules", &d, Some(("rules", Place::End))).unwrap();
    let d = Draft { title: "- [Only here](only-here.md)".into(), meta: "---\nname: only-here\n---\n".into(), body: "\nA memory with no file.\n".into(), reason: "remembered".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
    w.create("only-here", "memory", "", "", &d, Some(("memory", Place::End))).unwrap();
    w.commit().unwrap();
    assert_eq!(sync::drift(&store, &f.targets()).unwrap(), Vec::<String>::new());
    sync_all(&mut store, &f);
    assert_eq!(status(&store, "rules:r00099"), "active");
    assert_eq!(status(&store, "only-here"), "active");
}

#[test]
fn memory_files_change_in_place_retire_and_come_back() {
    let f = fixture("sync-memory");
    let mut store = import_all(&f);
    // A changed file is a new version of the same key.
    write(&f.mem().join("widget-cache.md"), &MEM_A.replace("64 entries", "128 entries"));
    // An index line cut for length is not a new title; CRLF is not new text.
    // A removed file (and its index line) retires its record; a new listed
    // file is a new record.
    write(&f.mem().join("MEMORY.md"), "# Memory\n- [Widget cache](widget-cache.md) - the widget cache holds 64 entries\r\n- [Fresh rule](fresh-rule.md) - a fresh rule\r\n");
    fs::remove_file(f.mem().join("export-rule.md")).unwrap();
    write(&f.mem().join("fresh-rule.md"), "---\nname: fresh-rule\nmetadata:\n  type: feedback\n  modified: 2026-01-07T10:00:00.000Z\n---\n\nA fresh rule.\n");

    let r = sync_all(&mut store, &f).into_iter().find(|r| r.export == "memory").unwrap();
    assert_eq!((r.new, r.changed, r.removed), (1, 1, 1), "{r:?}");
    assert_eq!(sync::drift(&store, &f.targets()).unwrap(), Vec::<String>::new());
    let w = store.head("widget-cache").unwrap().unwrap();
    assert_eq!(w.seq, 2);
    assert!(w.body.contains("128 entries"));
    assert!(w.title.ends_with("least recently used first"), "{}", w.title);
    assert!(store.history("widget-cache").unwrap()[0].body.contains("64 entries"));
    assert_eq!(status(&store, "export-rule"), "retired");
    let fresh = store.history("fresh-rule").unwrap();
    assert_eq!(fresh[0].time, "2026-01-07T10:00:00.000Z");
    assert_eq!(store.head("fresh-rule").unwrap().unwrap().sub, "feedback");

    // The file comes back: the record is active again, not a new key.
    write(&f.mem().join("export-rule.md"), MEM_B);
    let r = sync_all(&mut store, &f).into_iter().find(|r| r.export == "memory").unwrap();
    assert_eq!(r.returned, 1, "{r:?}");
    assert_eq!(status(&store, "export-rule"), "active");
    let rep = check(&store, &f.settings.exports, None, true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
}

#[test]
fn an_export_written_over_its_own_source_is_not_synced() {
    let f = fixture("sync-self");
    let mut s = f.settings.clone();
    s.export_dir = Some(f.root.join("handbook"));
    let t = sync::targets(&s, &[("rules".into(), f.root.join("handbook"))]);
    assert!(t.is_empty());
}

fn config(f: &Fix) -> Config {
    let cfg = json!({
        "db": "state/atlas.sqlite",
        "watch": false,
        "vaults": [{"name": "notes", "path": "Notes"}],
        "sources": [
            {"name": "rules", "root": "handbook", "include": ["*.md"], "chunk": {"top_level_items": true}},
            {"name": "memory", "root": "memory", "include": ["*.md"], "id_keys": ["name"], "names_kind": "memory"}
        ],
        "memory": {
            "store": "state/memory.sqlite",
            "export_dir": "out",
            "exports": [
                {"name": "memory", "shape": "memory", "path": "memory", "index": "MEMORY.md"},
                {"name": "rules", "shape": "items", "path": "RULES.md"}
            ]
        }
    });
    write(&f.root.join("Notes").join("Note.md"), "# Note\n");
    let path = f.root.join("atlas.json");
    fs::write(&path, atlas::json::to_string_pretty(&cfg)).unwrap();
    Config::load(&path).unwrap()
}

fn open_index(c: &Config) -> Store {
    let db = c.db.clone().unwrap();
    let specs: Vec<(String, PathBuf)> = c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
    Store::open_with(&db, Vaults::new(&specs).unwrap(), AtlasSetup::from_config(c).unwrap()).unwrap()
}

fn rule_count(store_path: &Path) -> usize {
    MemoryStore::open(store_path).unwrap().heads(Some("rule"), None).unwrap().into_iter().filter(|h| h.status == "active").count()
}

#[test]
fn rescans_sync_a_changed_source_and_skip_an_unchanged_one() {
    let f = fixture("sync-rescan");
    let c = config(&f);
    let settings = Settings::from_config(Some(&c), c.db.as_ref().unwrap()).unwrap();
    {
        let mut ms = MemoryStore::open(&settings.store).unwrap();
        let mut w = ms.writer().unwrap();
        import::import(&mut w, &settings.exports[0], &f.mem(), "test").unwrap();
        import::import(&mut w, &settings.exports[1], &f.rules(), "test").unwrap();
        w.commit().unwrap();
    }
    let mut store = open_index(&c);
    store.set_memory_sync(Some(settings.clone()));
    assert_eq!(store.memory_sync_sources(), vec!["rules".to_string(), "memory".to_string()]);
    // The first rescan checks each source once: in step, so nothing is written.
    store.rescan_sources(None).unwrap();
    let first = store.take_memory_sync_reports();
    assert!(first.iter().all(|r| r.new + r.changed + r.removed == 0), "{first:?}");
    // Unchanged: no sync runs at all.
    store.rescan_sources(None).unwrap();
    assert!(store.take_memory_sync_reports().is_empty());
    // A new process finds the fingerprint stored and does not re-plan.
    let mut again = open_index(&c);
    again.set_memory_sync(Some(settings.clone()));
    again.rescan_sources(None).unwrap();
    assert!(again.take_memory_sync_reports().is_empty());
    // An appended item is synced by the next rescan, and the export regenerated.
    let text = RULES.to_string() + "- 2026-01-08 Alex: appended after the import.\n";
    write(&f.rules(), &text);
    store.rescan_sources(None).unwrap();
    let r = store.take_memory_sync_reports();
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].export.as_str(), r[0].new), ("rules", 1));
    assert_eq!(rule_count(&settings.store), 4);
    assert_eq!(fs::read_to_string(f.root.join("out").join("RULES.md")).unwrap(), text);
}

#[test]
fn a_pointer_anchor_names_the_lines_read_note_reads_from_the_source() {
    let f = fixture("sync-anchor");
    let c = config(&f);
    let settings = Settings::from_config(Some(&c), c.db.as_ref().unwrap()).unwrap();
    let ms = {
        let mut ms = MemoryStore::open(&settings.store).unwrap();
        let mut w = ms.writer().unwrap();
        import::import(&mut w, &settings.exports[1], &f.rules(), "test").unwrap();
        w.commit().unwrap();
        ms
    };
    let h = ms.heads(Some("rule"), None).unwrap().into_iter().find(|h| h.body.contains("gamma queue")).unwrap();
    let a = atlas::memory::recall::pointer_anchor(&h).expect("an anchor");
    let (src, rest) = a.split_once(':').unwrap();
    let (path, line) = rest.rsplit_once(':').unwrap();
    assert_eq!((src, path), ("rules", "RULES.md"), "{a}");
    let mut s = atlas::tools::Server::new(open_index(&c));
    let out = s.answer("note", &json!({"vault": src, "path": path, "lines": line})).unwrap().into_text();
    assert!(out.starts_with("rules:RULES.md sha256=") && out.contains("gamma queue drains oldest first"), "{out}");
    // Only the source's own files: not one its globs leave out.
    write(&f.root.join("handbook").join("notes.txt"), "private\n");
    assert!(s.answer("note", &json!({"vault": "rules", "path": "notes.txt"})).is_err());
    assert!(s.answer("note", &json!({"vault": "rules", "path": "../atlas.json"})).is_err());
}
