//! The memory index as a tree: a flat index becomes a root, branches and
//! hubs every memory is reachable through; rebuilds are stable (nothing
//! changed writes nothing, a new memory touches only its path, a split
//! renames no untouched hub); a writer's root line is filed; the sync reads
//! the tree.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use atlas::memory::export::{ExportSpec, Shape};
use atlas::memory::settings::Settings;
use atlas::memory::sync;
use atlas::memory::tree::{self, BranchSpec, Churn, TreeSpec};
use atlas::memory::{import, MemoryStore};
use common::*;

fn spec() -> TreeSpec {
    let b = |name: &str, words: &[&str], types: &[&str]| BranchSpec {
        repo: None,
        name: name.into(),
        title: format!("{name} branch"),
        hook: format!("everything {name}"),
        words: words.iter().map(|w| w.to_string()).collect(),
        types: types.iter().map(|w| w.to_string()).collect(),
    };
    let mut t = TreeSpec::new("memory");
    t.hub_max = 6;
    t.hub_target = 6;
    t.hub_min = 2;
    t.format = tree::LineFormat::Link;
    t.branches = vec![b("alpha", &["alpha*"], &[]), b("beta", &["beta*"], &[]), b("notes", &[], &["user"])];
    t.validate().unwrap();
    t
}

fn mem(name: &str, desc: &str, kind: &str, pinned: bool, body: &str) -> String {
    let pin = if pinned { "pinned: true\n" } else { "" };
    format!("---\nname: {name}\ndescription: {desc}\n{pin}metadata:\n  type: {kind}\n---\n\n{body}\n")
}

/// Two alpha topics (cache, render), betas, and two memories no word
/// places (one by meaning near the cache memories).
fn folder(tag: &str) -> (Temp, PathBuf) {
    let tmp = Temp::new(tag);
    let dir = tmp.path().join("memory");
    let mut index = String::new();
    let mut add = |stem: &str, desc: &str, kind: &str, pinned: bool| {
        write(&dir.join(format!("{stem}.md")), &mem(stem, desc, kind, pinned, &format!("Body of {stem}: {desc}.")));
        index.push_str(&format!("- [{} title]({stem}.md) - {desc}\n", stem.replace('-', " ")));
    };
    for i in 0..5 {
        add(&format!("alpha-cache-{i}"), &format!("the cache eviction bound and cache warmup rule number {i}"), "feedback", i == 0);
        add(&format!("alpha-render-{i}"), &format!("the render shadow pass and render light budget fact {i}"), "project", false);
    }
    for i in 0..4 {
        add(&format!("beta-{i}"), &format!("the beta release checklist item {i}"), "project", i == 1);
    }
    add("eviction-warmup", "cache eviction warmup bound for the cache", "project", false);
    add("who-the-user-is", "the user prefers short answers", "user", false);
    write(&dir.join("MEMORY.md"), &index);
    (tmp, dir)
}

fn lexical() -> impl FnMut(&[String]) -> Vec<Vec<f32>> + Send {
    |t: &[String]| tree::lexical_vectors(t)
}

fn rebuild(dir: &Path, s: &TreeSpec) -> Churn {
    let mut e = lexical();
    tree::rebuild(dir, "MEMORY.md", s, &mut e, true).unwrap().0
}

fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for sub in ["", "index"] {
        let d = if sub.is_empty() { dir.to_path_buf() } else { dir.join(sub) };
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.path().is_file() {
                let name = e.file_name().to_string_lossy().into_owned();
                let rel = if sub.is_empty() { name } else { format!("{sub}/{name}") };
                out.insert(rel, fs::read(e.path()).unwrap());
            }
        }
    }
    out
}

fn changed(a: &BTreeMap<String, Vec<u8>>, b: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).cloned().collect()
}

/// The hub files on the path from the root to a memory.
fn path_to(dir: &Path, stem: &str) -> Vec<String> {
    let mut out = vec!["MEMORY.md".to_string()];
    for e in fs::read_dir(dir.join("index")).unwrap().flatten() {
        let t = fs::read_to_string(e.path()).unwrap_or_default();
        if t.contains(&format!("](../{stem}.md)")) {
            let f = e.file_name().to_string_lossy().into_owned();
            out.push(format!("index/{f}"));
            // Its parent branch, if it is a part of one.
            for p in fs::read_dir(dir.join("index")).unwrap().flatten() {
                if fs::read_to_string(p.path()).unwrap_or_default().contains(&format!("]({f})")) {
                    out.push(format!("index/{}", p.file_name().to_string_lossy()));
                }
            }
        }
    }
    out
}

#[test]
fn a_flat_index_becomes_a_tree_every_memory_is_reachable_through() {
    let (_t, dir) = folder("tree-build");
    let flat = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    let s = spec();
    let churn = rebuild(&dir, &s);
    assert_eq!(churn.added.len(), 16, "{churn:?}");
    assert_eq!(churn.backup.as_deref(), Some("index/MEMORY.md.before-tree"));
    assert_eq!(fs::read_to_string(dir.join("index").join("MEMORY.md.before-tree")).unwrap(), flat);
    let nav = tree::check(&dir, "MEMORY.md", &s).unwrap();
    assert!(nav.problems.is_empty(), "{:?}", nav.problems);
    assert_eq!(nav.leaves, 16);
    assert!(nav.depth.values().all(|&d| d <= 3), "{:?}", nav.depth);
    assert!(nav.fanout_max <= s.hub_max);
    // Alpha (11 memories with the one placed by meaning) was split; the
    // cache memories share a part.
    let root = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(root.contains(tree::MARKER) && root.contains("](index/alpha.md)") && root.contains("](index/notes.md)"), "{root}");
    let alpha = fs::read_to_string(dir.join("index").join("alpha.md")).unwrap();
    assert!(!alpha.contains("](../"), "a split branch lists its parts: {alpha}");
    let cache_hub = path_to(&dir, "alpha-cache-1")[1].clone();
    for i in 0..5 {
        assert_eq!(path_to(&dir, &format!("alpha-cache-{i}"))[1], cache_hub);
    }
    assert_eq!(path_to(&dir, "eviction-warmup")[1], cache_hub, "placed by meaning beside the cache memories");
    // Pins: in the root and in their branch.
    assert!(root.contains("- [alpha cache 0 title](alpha-cache-0.md)") && root.contains("- [beta 1 title](beta-1.md)"));
    assert_eq!(path_to(&dir, "beta-1").len(), 2);
    // Lines keep the text the flat index gave them.
    let beta = fs::read_to_string(dir.join("index").join("beta.md")).unwrap();
    assert!(beta.contains("- [beta 2 title](../beta-2.md) - the beta release checklist item 2\n"), "{beta}");
    // Types place too.
    assert!(fs::read_to_string(dir.join("index").join("notes.md")).unwrap().contains("who-the-user-is.md"));
    assert!(root.lines().count() < flat.lines().count());
}

#[test]
fn a_rebuild_with_nothing_changed_writes_nothing() {
    let (_t, dir) = folder("tree-idem");
    let s = spec();
    rebuild(&dir, &s);
    let before = snapshot(&dir);
    let churn = rebuild(&dir, &s);
    assert!(!churn.changed(), "{churn:?}");
    assert_eq!(changed(&before, &snapshot(&dir)), Vec::<String>::new());
    // The dry run agrees.
    let mut e = lexical();
    assert!(!tree::rebuild(&dir, "MEMORY.md", &s, &mut e, false).unwrap().0.changed());
}

#[test]
fn a_new_memory_touches_only_its_path_and_an_edit_touches_nothing() {
    let (_t, dir) = folder("tree-add");
    let s = spec();
    rebuild(&dir, &s);
    let before = snapshot(&dir);
    write(&dir.join("beta-new.md"), &mem("beta-new", "the beta release checklist item new", "project", false, "Body."));
    let churn = rebuild(&dir, &s);
    assert_eq!(churn.added, vec!["beta-new".to_string()]);
    let after = snapshot(&dir);
    let mut diff = changed(&before, &after);
    diff.retain(|f| f != "beta-new.md");
    assert_eq!(diff, vec!["index/beta.md".to_string()], "only its hub; the root lists branches, not memories");
    // A body edit changes no line; a description edit changes only its path.
    write(&dir.join("alpha-render-2.md"), &mem("alpha-render-2", "the render shadow pass and render light budget fact 2", "project", false, "A rewritten body."));
    let churn = rebuild(&dir, &s);
    assert!(!churn.changed(), "{churn:?}");
    let before = snapshot(&dir);
    let path = path_to(&dir, "alpha-render-2");
    write(&dir.join("alpha-render-2.md"), &mem("alpha-render-2", "a rewritten description of the render pass", "project", false, "A rewritten body."));
    rebuild(&dir, &s);
    let mut diff = changed(&before, &snapshot(&dir));
    diff.retain(|f| f != "alpha-render-2.md");
    assert!(!diff.is_empty() && diff.iter().all(|f| path.contains(f)), "{diff:?} outside {path:?}");
    assert!(fs::read_to_string(dir.join(&path[1])).unwrap().contains(" - a rewritten description of the render pass"));
    // A removal touches only its path.
    let before = snapshot(&dir);
    let path = path_to(&dir, "alpha-render-3");
    fs::remove_file(dir.join("alpha-render-3.md")).unwrap();
    let churn = rebuild(&dir, &s);
    assert_eq!(churn.removed, vec!["alpha-render-3".to_string()]);
    let mut diff = changed(&before, &snapshot(&dir));
    diff.retain(|f| f != "alpha-render-3.md");
    assert!(diff.iter().all(|f| path.contains(f)), "{diff:?} outside {path:?}");
    assert!(tree::check(&dir, "MEMORY.md", &s).unwrap().problems.is_empty());
}

#[test]
fn a_split_keeps_every_untouched_hub_and_its_name() {
    let (_t, dir) = folder("tree-split");
    let s = spec();
    rebuild(&dir, &s);
    let before = snapshot(&dir);
    let parts: Vec<String> = before.keys().filter(|k| k.starts_with("index/alpha-")).cloned().collect();
    assert!(parts.len() >= 2, "{parts:?}");
    // Grow the render part past hub_max.
    let render_hub = path_to(&dir, "alpha-render-0")[1].clone();
    for i in 5..9 {
        let stem = format!("alpha-render-{i}");
        write(&dir.join(format!("{stem}.md")), &mem(&stem, &format!("the render shadow pass and render light budget fact {i}"), "project", false, "Body."));
    }
    let churn = rebuild(&dir, &s);
    assert!(churn.splits >= 1, "{churn:?}");
    let after = snapshot(&dir);
    for p in &parts {
        assert!(after.contains_key(p), "{p} was renamed or deleted");
        if *p != render_hub {
            assert_eq!(before[p], after[p], "{p} was rewritten");
        }
    }
    let nav = tree::check(&dir, "MEMORY.md", &s).unwrap();
    assert!(nav.problems.is_empty(), "{:?}", nav.problems);
    assert!(!rebuild(&dir, &s).changed());
}

#[test]
fn a_line_a_writer_adds_to_the_root_is_filed_with_its_text() {
    let (_t, dir) = folder("tree-writer");
    let s = spec();
    rebuild(&dir, &s);
    // Claude Code's memory feature: a new file and a line appended to the index.
    write(&dir.join("beta-writer.md"), &mem("beta-writer", "a beta note from the writer", "project", false, "Body."));
    let mut root = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    root.push_str("- [Beta from the writer](beta-writer.md) - the hook the writer chose\n");
    fs::write(dir.join("MEMORY.md"), root).unwrap();
    let churn = rebuild(&dir, &s);
    assert_eq!(churn.added, vec!["beta-writer".to_string()]);
    let root = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(!root.contains("beta-writer.md"), "{root}");
    let beta = fs::read_to_string(dir.join("index").join("beta.md")).unwrap();
    // The writer's title, and the hook from the file's description.
    assert!(beta.contains("- [Beta from the writer](../beta-writer.md) - a beta note from the writer\n"), "{beta}");
    // A memory with no line anywhere gets one from its frontmatter, cut at a space.
    let long = "a beta memory with a very long description ".repeat(6);
    write(&dir.join("beta-bare.md"), &mem("beta-bare", long.trim(), "project", false, "Body."));
    rebuild(&dir, &s);
    let beta = fs::read_to_string(dir.join("index").join("beta.md")).unwrap();
    let line = beta.lines().find(|l| l.contains("beta-bare.md")).unwrap();
    assert!(line.starts_with("- [beta-bare](../beta-bare.md) - a beta memory") && line.chars().count() <= s.line_max + 3, "{line}");
    assert!(tree::check(&dir, "MEMORY.md", &s).unwrap().problems.is_empty());
}

#[test]
fn a_branch_flag_moves_a_memory_and_a_pin_shows_a_short_body() {
    let (_t, dir) = folder("tree-flags");
    let s = spec();
    rebuild(&dir, &s);
    let text = mem("beta-0", "the beta release checklist item 0", "project", true, "Short body.").replace("pinned: true\n", "pinned: true\nbranch: notes\n");
    write(&dir.join("beta-0.md"), &text);
    let churn = rebuild(&dir, &s);
    assert_eq!(churn.moved, vec!["beta-0".to_string()]);
    assert!(fs::read_to_string(dir.join("index").join("notes.md")).unwrap().contains("../beta-0.md"));
    let root = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(root.contains("](beta-0.md)") && root.contains("\n  Short body.\n"), "{root}");
    assert!(tree::check(&dir, "MEMORY.md", &s).unwrap().problems.is_empty());
}

#[test]
fn the_check_finds_orphans_unreachable_and_duplicate_memories() {
    let (_t, dir) = folder("tree-check");
    let s = spec();
    rebuild(&dir, &s);
    // A memory nobody lists, a hub nobody links, a memory listed twice.
    write(&dir.join("stray.md"), &mem("stray", "unlisted", "project", false, "Body."));
    write(&dir.join("index").join("lost.md"), &format!("# Lost\n{}\n", tree::MARKER));
    let mut beta = fs::read_to_string(dir.join("index").join("beta.md")).unwrap();
    beta.push_str("- [again](../alpha-cache-1.md) - listed twice\n");
    fs::write(dir.join("index").join("beta.md"), beta).unwrap();
    let p = tree::check(&dir, "MEMORY.md", &s).unwrap().problems;
    assert!(p.iter().any(|x| x.starts_with("stray.md: not reachable")), "{p:?}");
    assert!(p.iter().any(|x| x.starts_with("index/lost.md: a generated hub")), "{p:?}");
    assert!(p.iter().any(|x| x.starts_with("alpha-cache-1.md: listed by 2 hubs")), "{p:?}");
    // A rebuild repairs all three.
    rebuild(&dir, &s);
    assert!(tree::check(&dir, "MEMORY.md", &s).unwrap().problems.is_empty());
}

#[test]
fn the_sync_reads_the_tree_and_a_built_tree_drifts_nothing() {
    let (t, dir) = folder("tree-sync");
    let mut settings = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    settings.exports = vec![ExportSpec { name: "memory".into(), shape: Shape::MemoryDir, path: "memory".into(), index: "MEMORY.md".into(), handoffs: false, index_cap: None, index_topics: None }];
    settings.tree = Some(spec());
    settings.snapshot_secs = 0;
    let mut store = MemoryStore::open(&settings.store).unwrap();
    let mut w = store.writer().unwrap();
    import::import(&mut w, &settings.exports[0], &dir, "test").unwrap();
    w.commit().unwrap();
    let targets = sync::targets(&settings, &[("memory".into(), dir.clone())]);
    assert!(targets[0].tree.is_some());
    assert_eq!(sync::drift(&store, &targets).unwrap(), Vec::<String>::new());
    // Built: the root lists branches, yet every memory stays listed with its title.
    rebuild(&dir, settings.tree.as_ref().unwrap());
    assert_eq!(sync::drift(&store, &targets).unwrap(), Vec::<String>::new());
    // A memory a writer adds is new, and placed.
    write(&dir.join("beta-late.md"), &mem("beta-late", "a late beta note", "project", false, "Body."));
    rebuild(&dir, settings.tree.as_ref().unwrap());
    let r = sync::sync(&mut store, &targets[0]).unwrap();
    assert_eq!((r.new, r.changed, r.removed, r.placed), (1, 0, 0, 0), "{r:?}");
    assert_eq!(store.exports_of("beta-late").unwrap(), vec!["memory".to_string()]);
    assert_eq!(sync::drift(&store, &targets).unwrap(), Vec::<String>::new());
}

#[test]
fn a_config_pin_pins_and_a_dropped_flag_is_drift() {
    let (t, dir) = folder("tree-config-pin");
    let mut s = spec();
    s.pins = vec!["beta-2".into(), "beta-1".into()];
    rebuild(&dir, &s);
    let root = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(root.contains("](beta-2.md)") && root.contains("](beta-1.md)"), "{root}");
    assert!(tree::check(&dir, "MEMORY.md", &s).unwrap().problems.is_empty());
    // beta-1 carries the flag too; beta-2 only the config: the drift names beta-2.
    assert_eq!(tree::pin_drift(&dir, &s), vec!["pin beta-2: in memory.tree.pins, but beta-2.md has no `pinned: true`".to_string()]);
    let mut settings = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    settings.exports = vec![ExportSpec { name: "memory".into(), shape: Shape::MemoryDir, path: "memory".into(), index: "MEMORY.md".into(), handoffs: false, index_cap: None, index_topics: None }];
    settings.tree = Some(s);
    let store = MemoryStore::open(&settings.store).unwrap();
    let targets = sync::targets(&settings, &[("memory".into(), dir.clone())]);
    assert!(sync::drift(&store, &targets).unwrap().iter().any(|l| l.starts_with("memory pin beta-2:")));
}

#[test]
fn words_and_meaning_together_place_a_memory_no_word_names() {
    // "eviction-warmup" names no branch word; its meaning is the cache memories'.
    let (_t, dir) = folder("tree-blend");
    let s = spec();
    rebuild(&dir, &s);
    let alpha_parts: Vec<String> = fs::read_dir(dir.join("index")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|f| f.starts_with("alpha")).collect();
    let home = path_to(&dir, "eviction-warmup");
    assert!(home.iter().any(|f| alpha_parts.iter().any(|a| f.ends_with(a.as_str()))), "{home:?}");
}

#[test]
fn tree_settings_parse_from_the_config() {
    let tmp = Temp::new("tree-config");
    let cfg = tmp.path().join("atlas.json");
    write(
        &cfg,
        r#"{"vaults": [], "memory": {"exports": [{"name": "memory", "shape": "memory", "path": "memory"}],
            "tree": {"hub_max": 30, "branches": [{"name": "game-art", "title": "Game art", "hook": "models", "words": ["mesh*"]}]}}}"#,
    );
    let c = atlas::config::Config::load(&cfg).unwrap();
    let s = Settings::from_config(Some(&c), &tmp.path().join("db.sqlite")).unwrap();
    let t = s.tree.unwrap();
    assert_eq!((t.export.as_str(), t.hub_max, t.hub_dir.as_str(), t.branches.len()), ("memory", 30, "index", 1));
    write(&cfg, r#"{"vaults": [], "memory": {"exports": [{"name": "memory", "shape": "memory", "path": "memory"}], "tree": {"branches": [{"name": "Bad Name"}]}}}"#);
    assert!(atlas::config::Config::load(&cfg).is_err());
}

#[test]
fn switching_to_the_slug_format_keeps_titles_and_counts_removals() {
    let (t, dir) = folder("tree-slug-switch");
    let mut settings = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    settings.exports = vec![ExportSpec { name: "memory".into(), shape: Shape::MemoryDir, path: "memory".into(), index: "MEMORY.md".into(), handoffs: false, index_cap: None, index_topics: None }];
    let mut s = spec();
    s.format = tree::LineFormat::Slug;
    settings.tree = Some(s.clone());
    let mut store = MemoryStore::open(&settings.store).unwrap();
    let mut w = store.writer().unwrap();
    import::import(&mut w, &settings.exports[0], &dir, "test").unwrap();
    w.commit().unwrap();
    rebuild(&dir, &s);
    let targets = sync::targets(&settings, &[("memory".into(), dir.clone())]);
    let r = sync::sync(&mut store, &targets[0]).unwrap();
    assert_eq!((r.new, r.changed, r.removed), (0, 0, 0), "{r:?}");
    assert_eq!(store.head("beta-2").unwrap().unwrap().title, "- [beta 2 title](beta-2.md) - the beta release checklist item 2");
    fs::remove_file(dir.join("beta-3.md")).unwrap();
    assert_eq!(rebuild(&dir, &s).removed, vec!["beta-3".to_string()]);
}

#[test]
fn the_slug_format_navigates_rebuilds_stably_and_syncs() {
    let (t, dir) = folder("tree-slug");
    let mut s = spec();
    s.format = tree::LineFormat::Slug;
    assert_eq!(TreeSpec::new("memory").format, tree::LineFormat::Slug, "slug is the default");
    rebuild(&dir, &s);
    // Twice from the first build over the flat index: the second writes nothing.
    let first = snapshot(&dir);
    let again = rebuild(&dir, &s);
    assert!(!again.changed(), "{again:?}");
    assert_eq!(changed(&first, &snapshot(&dir)), Vec::<String>::new());
    let root = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(root.contains("- alpha: alpha branch - everything alpha\n"), "{root}");
    assert!(root.contains("- beta-1: the beta release checklist item 1\n"), "{root}");
    let beta = fs::read_to_string(dir.join("index").join("beta.md")).unwrap();
    assert!(beta.contains("- beta-2: the beta release checklist item 2\n"), "{beta}");
    let nav = tree::check(&dir, "MEMORY.md", &s).unwrap();
    assert!(nav.problems.is_empty(), "{:?}", nav.problems);
    assert!(nav.depth.values().all(|&d| d <= 3));
    let before = snapshot(&dir);
    assert!(!rebuild(&dir, &s).changed());
    assert_eq!(changed(&before, &snapshot(&dir)), Vec::<String>::new());
    // A new memory: only its hub.
    write(&dir.join("beta-new.md"), &mem("beta-new", "the beta release checklist item new", "project", false, "Body."));
    rebuild(&dir, &s);
    let mut diff = changed(&before, &snapshot(&dir));
    diff.retain(|f| f != "beta-new.md");
    assert_eq!(diff, vec!["index/beta.md".to_string()]);
    // The sync reads the slug lines: one sync settles the titles, then no drift.
    let mut settings = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    settings.exports = vec![ExportSpec { name: "memory".into(), shape: Shape::MemoryDir, path: "memory".into(), index: "MEMORY.md".into(), handoffs: false, index_cap: None, index_topics: None }];
    settings.tree = Some(s);
    let mut store = MemoryStore::open(&settings.store).unwrap();
    let targets = sync::targets(&settings, &[("memory".into(), dir.clone())]);
    sync::sync(&mut store, &targets[0]).unwrap();
    assert_eq!(sync::drift(&store, &targets).unwrap(), Vec::<String>::new());
    assert_eq!(store.exports_of("beta-new").unwrap(), vec!["memory".to_string()]);
}
