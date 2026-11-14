//! Which rulings replace which: the link-only store ops through the check's
//! replay and the copy's tail, what a hit on an old ruling shows, the two-part
//! sitting and its answers. All text here is invented.

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use atlas::memory::check::check;
use atlas::memory::export::{ExportSpec, Shape};
use atlas::memory::rules::{self, Answer, Target};
use atlas::memory::settings::Settings;
use atlas::memory::sync::{self, Target as SyncTarget};
use atlas::memory::{import, MemoryStore};
use common::*;

const RULES: &str = "# Rules\n\n## Storage\n\
- 2026-01-02 Alex: the widget cache is bounded at 64 entries.\n\
- 2026-01-03 Blake: exports are written atomically through a rename.\n\
- 2026-01-04 Alex: this reverses r00001; the widget cache grows freely.\n\
- 2026-01-05 Blake: the older queue split is scrapped.\n\
- 2026-01-06 Alex: the gamma queue drains oldest first.\n\
- 2026-01-07 Alex: refined the \"written atomically\" line for temp files.\n";
const INDEX: &str = "# Memory\n- [Widget note](widget-note.md) - a widget note\n- [Export note](export-note.md) - an export note\n";
const MEM_A: &str = "---\nname: widget-note\ndescription: a widget note\nmetadata:\n  type: feedback\n---\n\nThe old cap was withdrawn.\n";
const MEM_B: &str = "---\nname: export-note\ndescription: an export note\nmetadata:\n  type: project\n---\n\nExports use a temp file.\n";

struct Fix {
    _tmp: Temp,
    root: PathBuf,
    settings: Settings,
}

impl Fix {
    fn targets(&self) -> Vec<SyncTarget> {
        sync::targets(&self.settings, &[("rules".into(), self.root.join("handbook")), ("memory".into(), self.root.join("memory"))])
    }
}

fn fixture(tag: &str) -> Fix {
    let tmp = Temp::new(tag);
    let root = tmp.path().to_path_buf();
    write(&root.join("handbook").join("RULES.md"), RULES);
    write(&root.join("memory").join("MEMORY.md"), INDEX);
    write(&root.join("memory").join("widget-note.md"), MEM_A);
    write(&root.join("memory").join("export-note.md"), MEM_B);
    let mut settings = Settings::from_config(None, &root.join("state").join("index.sqlite")).unwrap();
    settings.export_dir = Some(root.join("out"));
    settings.exports = specs();
    settings.snapshot_secs = 0;
    Fix { _tmp: tmp, root, settings }
}

fn specs() -> Vec<ExportSpec> {
    vec![
        ExportSpec {
            name: "memory".into(),
            shape: Shape::MemoryDir,
            path: "memory".into(),
            index: "MEMORY.md".into(),
            handoffs: false,
            index_cap: None,
            index_topics: None,
        },
        ExportSpec {
            name: "rules".into(),
            shape: Shape::Items,
            path: "RULES.md".into(),
            index: String::new(),
            handoffs: false,
            index_cap: None,
            index_topics: None,
        },
    ]
}

fn imported(f: &Fix) -> MemoryStore {
    let mut store = MemoryStore::open(&f.settings.store).unwrap();
    let mut w = store.writer().unwrap();
    import::import(&mut w, &f.settings.exports[0], &f.root.join("memory"), "test").unwrap();
    import::import(&mut w, &f.settings.exports[1], &f.root.join("handbook").join("RULES.md"), "test").unwrap();
    w.commit().unwrap();
    store
}

const R1: &str = "rules:r00001";
const R2: &str = "rules:r00002";
const R3: &str = "rules:r00003";
const R4: &str = "rules:r00004";
const R5: &str = "rules:r00005";
const R6: &str = "rules:r00006";

fn status(store: &MemoryStore, key: &str) -> String {
    store.head(key).unwrap().unwrap().status
}

fn mark(store: &mut MemoryStore, old: &str, new: &str) -> atlas::Result<()> {
    let mut w = store.writer().unwrap();
    w.mark_superseded(old, new, "an answer in a test")?;
    w.commit()
}

fn replays(store: &MemoryStore) {
    let rep = check(store, &specs(), None, true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
}

#[test]
fn a_mark_creates_nothing_and_replays_through_the_check() {
    let f = fixture("rules-mark");
    let mut store = imported(&f);
    let before = store.counts().unwrap();
    mark(&mut store, R1, R3).unwrap();
    assert_eq!(store.counts().unwrap(), before);
    let h = store.head(R1).unwrap().unwrap();
    assert_eq!(h.status, "superseded");
    assert_eq!(h.superseded_by.as_deref(), Some(R3));
    replays(&store);
    // The tamper the check exists for still shows: a status the log does not
    // hold differs from the replay.
    store.conn().execute("UPDATE records SET status = 'active' WHERE key = ?1", [R1]).unwrap();
    assert!(!check(&store, &specs(), None, true).unwrap().ok());
}

#[test]
fn an_answer_changed_and_back_replays_and_shows_only_what_holds() {
    let f = fixture("rules-unmark");
    let mut store = imported(&f);
    mark(&mut store, R1, R3).unwrap();
    let mut w = store.writer().unwrap();
    w.unmark_superseded(R1, "the answer changed").unwrap();
    w.commit().unwrap();
    let h = store.head(R1).unwrap().unwrap();
    assert_eq!((h.status.as_str(), h.superseded_by.as_deref(), h.superseded_time.as_deref()), ("active", None, None));
    replays(&store);
    // The same pair again, then another successor.
    mark(&mut store, R1, R3).unwrap();
    replays(&store);
    let mut w = store.writer().unwrap();
    w.unmark_superseded(R1, "the answer changed again").unwrap();
    w.commit().unwrap();
    mark(&mut store, R1, R4).unwrap();
    assert_eq!(store.head(R1).unwrap().unwrap().superseded_by.as_deref(), Some(R4));
    replays(&store);
}

#[test]
fn a_mark_past_the_recorded_end_is_held_and_kept() {
    let f = fixture("rules-tail");
    let mut store = imported(&f);
    let end0 = store.jsonl_end().unwrap().unwrap();
    mark(&mut store, R1, R3).unwrap();
    let mut w = store.writer().unwrap();
    w.link_amends(R2, R6).unwrap();
    w.commit().unwrap();
    store.conn().execute("UPDATE mem_meta SET value = ?1 WHERE key = 'jsonl_end'", [end0.to_string()]).unwrap();
    let bytes = fs::read(store.jsonl()).unwrap();
    // line_in_store reads both lines as the store's.
    let tail = store.copy_tail(&bytes).unwrap();
    assert_eq!(tail.end, bytes.len() as u64);
    assert!(tail.uncommitted.is_empty(), "{tail:?}");
    replays(&store);
    // The next write keeps them.
    let mut w = store.writer().unwrap();
    w.link_amends(R4, R5).unwrap();
    w.commit().unwrap();
    let copy = fs::read_to_string(store.jsonl()).unwrap();
    assert!(copy.contains("\"op\":\"supersede\"") && copy.contains("amended_by"), "{copy}");
    replays(&store);
}

#[test]
fn a_mark_refuses_what_would_lose_a_ruling() {
    let f = fixture("rules-refuse");
    let mut store = imported(&f);
    assert!(mark(&mut store, R1, R1).is_err());
    assert!(mark(&mut store, R1, "rules:r99999").is_err());
    assert!(mark(&mut store, "rules:r99999", R1).is_err());
    mark(&mut store, R1, R3).unwrap();
    assert!(mark(&mut store, R1, R4).is_err(), "already superseded");
    // A loop, direct and through a chain, would leave no live ruling.
    assert!(mark(&mut store, R3, R1).is_err());
    mark(&mut store, R3, R4).unwrap();
    assert!(mark(&mut store, R4, R1).is_err());
    assert_eq!(status(&store, R4), "active");
    replays(&store);
}

#[test]
fn an_amendment_links_both_ways_and_keeps_both_active() {
    let f = fixture("rules-amend");
    let mut store = imported(&f);
    let refs_before = store.counts().unwrap();
    let mut w = store.writer().unwrap();
    w.link_amends(R1, R3).unwrap();
    w.link_amends(R1, R3).unwrap();
    assert!(w.link_amends(R1, R1).is_err());
    assert!(w.link_amends(R1, "rules:r99999").is_err());
    w.commit().unwrap();
    assert_eq!(store.counts().unwrap(), refs_before);
    assert_eq!((status(&store, R1).as_str(), status(&store, R3).as_str()), ("active", "active"));
    assert_eq!(store.linked_to(rules::AMENDS, R1).unwrap(), vec![R3.to_string()]);
    assert_eq!(store.linked_to(rules::AMENDED_BY, R3).unwrap(), vec![R1.to_string()]);
    replays(&store);
}

fn view(store: &MemoryStore, key: &str) -> Vec<(String, Vec<String>)> {
    let h = store.head(key).unwrap().unwrap();
    rules::live_view(store, h).unwrap().into_iter().map(|(h, t)| (h.key, t)).collect()
}

#[test]
fn a_hit_shows_the_live_ruling_first() {
    let f = fixture("rules-view");
    let mut store = imported(&f);
    assert_eq!(view(&store, R2), vec![(R2.to_string(), vec![])]);
    // Superseded: the successor, tagged, and the old text stays by key.
    mark(&mut store, R1, R3).unwrap();
    assert_eq!(view(&store, R1), vec![(R3.to_string(), vec![format!("replaces {R1}")])]);
    assert!(store.head(R1).unwrap().unwrap().body.contains("64 entries"));
    // A chain ends at the live one, and the tag names the record hit.
    mark(&mut store, R3, R4).unwrap();
    assert_eq!(rules::live_of(&store, R1).unwrap().unwrap().key, R4);
    assert_eq!(view(&store, R1), vec![(R4.to_string(), vec![format!("replaces {R1}")])]);
    // Amended: the later record first, then the record, both kept.
    let mut w = store.writer().unwrap();
    w.link_amends(R2, R6).unwrap();
    w.commit().unwrap();
    assert_eq!(view(&store, R2), vec![(R6.to_string(), vec![format!("amends {R2}")]), (R2.to_string(), vec![format!("amended by {R6}")])]);
    assert_eq!(view(&store, R6), vec![(R6.to_string(), vec![format!("amends {R2}")])]);
}

#[test]
fn a_chain_with_no_live_end_shows_nothing_rather_than_a_dead_ruling() {
    let f = fixture("rules-dead");
    let mut store = imported(&f);
    mark(&mut store, R1, R3).unwrap();
    let mut w = store.writer().unwrap();
    w.retire(R3, "gone from the source").unwrap();
    w.commit().unwrap();
    assert!(rules::live_of(&store, R1).unwrap().is_none());
    assert!(view(&store, R1).is_empty());
}

#[test]
fn only_a_search_that_asks_returns_a_superseded_record() {
    let f = fixture("rules-search");
    let mut store = imported(&f);
    mark(&mut store, R1, R3).unwrap();
    let terms = vec!["bounded".to_string()];
    assert!(store.search(&terms, None, None, 5).unwrap().iter().all(|(h, _, _)| h.key != R1));
    assert!(store.search_with(&terms, None, None, 5, rules::Mode::Live).unwrap().iter().any(|(h, _, _)| h.key == R1));
}

#[test]
fn a_sync_leaves_marks_alone() {
    let f = fixture("rules-sync");
    let mut store = imported(&f);
    mark(&mut store, R1, R3).unwrap();
    let mut w = store.writer().unwrap();
    w.link_amends(R2, R6).unwrap();
    w.mark_superseded("widget-note", "export-note", "an answer in a test").unwrap();
    w.commit().unwrap();
    let before = store.counts().unwrap();
    for t in f.targets() {
        let r = sync::sync(&mut store, &t).unwrap();
        assert_eq!((r.new, r.changed, r.removed, r.returned), (0, 0, 0, 0), "{r:?}");
    }
    assert_eq!(store.counts().unwrap(), before);
    assert_eq!(status(&store, R1), "superseded");
    assert_eq!(status(&store, "widget-note"), "superseded");
    assert_eq!(sync::drift(&store, &f.targets()).unwrap(), Vec::<String>::new());
    replays(&store);
}

#[test]
fn the_word_list_is_the_tightened_one() {
    for gone in ["replaces", "replacing", "corrects", "corrected", "overrides", "overriding", "overruled"] {
        assert!(rules::word_hits(gone).is_empty(), "{gone}");
    }
    for kept in ["reverses", "reversal", "superseded", "amends", "struck", "scrapped", "withdrawn", "rescinded", "refined"] {
        assert_eq!(rules::word_hits(kept).len(), 1, "{kept}");
    }
}

#[test]
fn a_sitting_has_two_parts_and_takes_items_by_hand() {
    let f = fixture("rules-sitting");
    let mut store = imported(&f);
    // The key and the one-hit quote are linked, not asked.
    let scan = rules::scan(&store).unwrap();
    let of = |k: &str| scan.iter().find(|c| c.key == k).map(|c| c.target.clone());
    assert_eq!(of(R3), Some(Target::Key(R1.into())));
    assert_eq!(of(R6), Some(Target::Quote(R2.into())));
    assert_eq!(of(R5), None);
    let s = rules::sitting(&store, &[]).unwrap();
    assert_eq!(s.rules.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec![R4]);
    assert_eq!(s.memory.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec!["widget-note"]);
    // By hand: each to its own part, at the end, marked by an empty word; twice, or
    // one already asked, is one question.
    let added = ["export-note".to_string(), R5.to_string(), "export-note".to_string(), "widget-note".to_string()];
    let s = rules::sitting(&store, &added).unwrap();
    assert_eq!(s.rules.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec![R4, R5]);
    assert_eq!(s.memory.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec!["widget-note", "export-note"]);
    assert_eq!((s.rules[1].word.as_str(), s.memory[1].word.as_str()), ("", ""));
    assert!(!s.memory[0].word.is_empty());
    // A key that is unknown, or no longer active, is refused rather than dropped.
    assert!(rules::sitting(&store, &["no-such-memory".to_string()]).is_err());
    mark(&mut store, R5, R4).unwrap();
    assert!(rules::sitting(&store, &[R5.to_string()]).is_err());
    // Answered: gone from the sitting, hand-added or not.
    rules::answer(&mut store, "export-note", &Answer::default(), "not a reversal").unwrap();
    rules::answer(&mut store, "widget-note", &Answer::default(), "not a reversal").unwrap();
    let s = rules::sitting(&store, &["export-note".to_string()]).unwrap();
    assert!(s.memory.is_empty());
    replays(&store);
}

#[test]
fn an_answer_writes_whole_and_partial_or_nothing() {
    let f = fixture("rules-answer");
    let mut store = imported(&f);
    rules::answer(&mut store, R4, &Answer { whole: vec![R1.into()], partial: vec![R2.into()] }, "his answer").unwrap();
    assert_eq!(status(&store, R1), "superseded");
    assert_eq!(status(&store, R2), "active");
    assert_eq!(store.linked_to(rules::AMENDS, R2).unwrap(), vec![R4.to_string()]);
    assert!(store.keys_with_ref(rules::CHECKED).unwrap().contains(R4));
    // A bad part refuses the whole answer: the good part is not written.
    let bad = Answer { whole: vec![R5.into(), "rules:r99999".into()], partial: vec![] };
    assert!(rules::answer(&mut store, R6, &bad, "his answer").is_err());
    assert_eq!(status(&store, R5), "active");
    assert!(!store.keys_with_ref(rules::CHECKED).unwrap().contains(R6));
    replays(&store);
}

#[test]
fn found_pairs_link_once_and_change_no_status() {
    let f = fixture("rules-link");
    let mut store = imported(&f);
    assert_eq!(rules::link_found(&mut store).unwrap(), 2);
    assert_eq!(rules::link_found(&mut store).unwrap(), 0);
    assert_eq!(status(&store, R1), "active");
    assert_eq!(store.linked_to(rules::AMENDS, R1).unwrap(), vec![R3.to_string()]);
    assert_eq!(store.linked_to(rules::AMENDS, R2).unwrap(), vec![R6.to_string()]);
    replays(&store);
}

#[test]
fn the_cli_runs_a_sitting() {
    let tmp = Temp::new("rules-cli");
    let run = |args: &[&str]| {
        let o = Command::new(bin())
            .args(args)
            .env("ATLAS_HOME", tmp.path())
            .env_remove("ATLAS_CONFIG")
            .current_dir(tmp.path())
            .output()
            .unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr))
    };
    let (ok, out) = run(&["ruling", "--by", "Alex", "--", "the widget cache is bounded"]);
    assert!(ok, "{out}");
    let (ok, out) = run(&["ruling", "--by", "Alex", "--", "the older split is scrapped"]);
    assert!(ok, "{out}");
    let (ok, out) = run(&["rules", "counts"]);
    assert!(ok && out.contains("part 1, rules: 1") && out.contains("part 2, memories: 0"), "{out}");
    let (ok, out) = run(&["rules", "counts", "--add", "ruling:r00001"]);
    assert!(ok && out.contains("part 1, rules: 2"), "{out}");
    let (ok, out) = run(&["rules", "sitting", "1", "--add", "ruling:r00001"]);
    assert!(ok && out.contains("1. - ruling:r00002") && out.contains("[scrapped]") && out.contains("[added by hand]"), "{out}");
    let (ok, out) = run(&["rules", "answer", "ruling:r00002", "--whole", "ruling:r00001"]);
    assert!(ok, "{out}");
    let (ok, out) = run(&["rules", "counts"]);
    assert!(ok && out.contains("part 1, rules: 0"), "{out}");
    let (ok, out) = run(&["rules", "unmark", "ruling:r00001"]);
    assert!(ok, "{out}");
    assert!(!run(&["rules", "sitting", "3"]).0);
    assert!(!run(&["rules", "bogus"]).0);
    assert!(!run(&["rules", "counts", "--add"]).0);
}

#[test]
fn a_ruling_can_say_what_it_supersedes() {
    let tmp = Temp::new("rules-write");
    let run = |args: &[&str]| {
        let o = Command::new(bin()).args(args).env("ATLAS_HOME", tmp.path()).env_remove("ATLAS_CONFIG").current_dir(tmp.path()).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr))
    };
    let (ok, out) = run(&["ruling", "--by", "Alex", "--", "the widget cache is bounded"]);
    assert!(ok, "{out}");
    // A refused mark writes no record, and the sitting has nothing new.
    let (ok, out) = run(&["ruling", "--by", "Alex", "--supersedes", "ruling:r99999", "--", "this reverses nothing real"]);
    assert!(!ok, "{out}");
    let (ok, out) = run(&["rules", "counts"]);
    assert!(ok && out.contains("part 1, rules: 0"), "{out}");
    // With no reversal word at all, and with one: both mark, neither is asked.
    let (ok, out) = run(&["ruling", "--by", "Alex", "--supersedes", "ruling:r00001", "--", "the widget cache grows freely"]);
    assert!(ok, "{out}");
    let (ok, out) = run(&["ruling", "--by", "Alex", "--supersedes", "ruling:r00002", "--", "this reverses the free growth"]);
    assert!(ok, "{out}");
    let (ok, out) = run(&["rules", "counts"]);
    assert!(ok && out.contains("part 1, rules: 0"), "{out}");
    // Already superseded is refused.
    let (ok, out) = run(&["ruling", "--by", "Alex", "--supersedes", "ruling:r00001", "--", "again"]);
    assert!(!ok && out.contains("already superseded"), "{out}");
    // An entry cannot.
    assert!(!run(&["entry", "--title", "T", "--supersedes", "ruling:r00003", "--", "some text"]).0);
}

// ------------------------------------------------------------- memory.rules

const WIRED: &str = "# Rules\n\n## Storage\n\
- 2026-01-02 Alex: the widget cache is bounded at sixty four entries.\n\
- 2026-01-04 Alex: the widget cache grows freely from now on, no bound applies.\n";

/// A store with two rules, the first replaced by the second, and a journal
/// entry about the same widget cache.
fn wired(tag: &str, mode: rules::Mode) -> (Temp, Settings) {
    let tmp = Temp::new(tag);
    write(&tmp.path().join("handbook").join("RULES.md"), WIRED);
    let mut s = Settings::from_config(None, &tmp.path().join("state").join("index.sqlite")).unwrap();
    s.exports = vec![specs().remove(1)];
    s.snapshot_secs = 0;
    s.recall.fts_floor = 0.0;
    s.recall.hybrid_fts_floor = 0.0;
    s.recall.rules = mode;
    let mut store = MemoryStore::open(&s.store).unwrap();
    let mut w = store.writer().unwrap();
    import::import(&mut w, &s.exports[0], &tmp.path().join("handbook").join("RULES.md"), "test").unwrap();
    w.commit().unwrap();
    mark(&mut store, "rules:r00001", "rules:r00002").unwrap();
    let mut w = store.writer().unwrap();
    w.create("decision:widget-sizing", "decision", "", "project", &atlas::memory::Draft { title: "widget cache sized for the lane".into(), body: String::new(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() }, None).unwrap();
    w.commit().unwrap();
    (tmp, s)
}

fn prompt(s: &Settings, p: &str) -> String {
    let mut h = atlas::hooks::Hooks::open(s.clone()).unwrap();
    h.handle(&atlas::hooks::HookEvent { event: "UserPromptSubmit".into(), session_id: "s".into(), prompt: p.into(), ..Default::default() }).context
}

const ASK: &str = "what is the bound on the widget cache entries";

#[test]
fn with_rules_on_the_hook_gives_headlines_and_says_what_is_replaced() {
    let (_t, s) = wired("rules-hook-on", rules::Mode::Live);
    let c = prompt(&s, ASK);
    let live = c.find("- rules:r00002: 01-04 ").unwrap_or_else(|| panic!("{c}"));
    assert!(c[live..].lines().next().unwrap().contains("[replaces rules:r00001]"), "{c}");
    assert!(!c.contains("- rules:r00001"), "the old text stays one recall away: {c}");
}

#[test]
fn unset_changes_nothing_at_the_hook_and_off_removes_rules_only() {
    let (_t, today) = wired("rules-hook-today", rules::Mode::Today);
    let c = prompt(&today, ASK);
    assert!(c.contains("rules:r00002") && !c.contains("replaces") && !c.contains(": 01-04 "), "{c}");
    let (_t, off) = wired("rules-hook-off", rules::Mode::Off);
    let c = prompt(&off, ASK);
    assert!(!c.contains("rules:r0"), "{c}");
    let c = prompt(&off, "how was the widget cache sized for the lane");
    assert!(!c.contains("rules:r0") && c.contains("decision:widget-sizing"), "{c}");
    // The store is kept: the key still reads.
    assert_eq!(status(&MemoryStore::open(&off.store).unwrap(), "rules:r00002"), "active");
}

fn query(s: &Settings, tmp: &std::path::Path, args: atlas::json::Value) -> String {
    let vault = tmp.join("vault");
    fs::create_dir_all(&vault).unwrap();
    let store = atlas::store::Store::open_with(&tmp.join("state").join("index.sqlite"), atlas::paths::Vaults::new(&[("notes".to_string(), vault)]).unwrap(), Default::default()).unwrap();
    let mut server = atlas::tools::Server::new(store);
    server.set_memory(s.clone());
    server.answer("recall", &args).unwrap().into_text()
}

#[test]
fn query_recall_follows_the_setting() {
    let ask = || atlas::json!({"query": "widget cache bound entries"});
    let (t, on) = wired("rules-q-on", rules::Mode::Live);
    let a = query(&on, t.path(), ask());
    assert!(a.contains("rules:r00002") && a.contains("replaces rules:r00001"), "{a}");
    let (t, off) = wired("rules-q-off", rules::Mode::Off);
    let a = query(&off, t.path(), ask());
    assert!(!a.contains("rules:r0") && a.contains("decision:widget-sizing"), "{a}");
    let by_key = query(&off, t.path(), atlas::json!({"key": "rules:r00002"}));
    assert!(by_key.contains("grows freely"), "off keeps the store: {by_key}");
    let (t, today) = wired("rules-q-today", rules::Mode::Today);
    let a = query(&today, t.path(), ask());
    assert!(a.contains("rules:r00002") && !a.contains("replaces"), "{a}");
}

fn config_with(root: &std::path::Path, rules: &str) -> atlas::config::Config {
    write(&root.join("repo").join("CLAUDE.md"), "- a rule in the root file.\n");
    write(&root.join("repo").join("INTENT.md"), "- a named rule.\n");
    write(&root.join("mem").join("MEMORY.md"), "# Memory\n");
    config_only(root, rules)
}

/// The project configuration alone, over the files already there.
fn config_only(root: &std::path::Path, rules: &str) -> atlas::config::Config {
    let cfg = root.join("atlas.json");
    // "absent" leaves the block out altogether.
    let memory = if rules == "absent" { String::new() } else { format!(r#""memory": {{{rules}}}, "#) };
    write(&cfg, &format!(
        r#"{{{memory}"projects_dir": "state", "projects": [{{"name": "p", "roots": ["repo"], "memory_dir": "mem", "intent": "repo/INTENT.md", "vaults": [{{"name": "notes", "path": "vault"}}]}}], "watch": false}}"#
    ));
    atlas::config::Config::load(&cfg).unwrap()
}

#[test]
fn the_answer_decides_what_a_project_syncs() {
    let t = Temp::new("rules-resolve");
    let cwd = t.path().join("repo");
    let names = |rules: &str| {
        let e = atlas::project::resolve(&config_with(t.path(), rules), &cwd, None).unwrap().config;
        let mut exports: Vec<String> = e.memory.unwrap().exports.iter().map(|x| x.name.clone()).collect();
        exports.sort();
        let sources: Vec<String> = e.sources.iter().map(|x| x.name.clone()).collect();
        (exports, sources)
    };
    // Unset: what is named by hand, and CLAUDE.md is not picked up.
    let (exports, _) = names("");
    assert_eq!(exports, ["intent", "memory"]);
    // Yes: the root CLAUDE.md as well, beside what is named.
    let (exports, sources) = names(r#""rules": true"#);
    assert_eq!(exports, ["claude-md", "intent", "memory"]);
    assert!(sources.iter().any(|s| s == "claude-md"));
    // False: the same as unset.
    let (exports, sources) = names(r#""rules": false"#);
    assert_eq!(exports, ["intent", "memory"]);
    assert!(!sources.iter().any(|s| s == "claude-md"), "{sources:?}");
    // Off: none of them, source or export.
    let (exports, sources) = names(r#""rules": "off""#);
    assert!(exports.is_empty(), "{exports:?}");
    assert!(!sources.iter().any(|s| matches!(s.as_str(), "memory" | "intent" | "claude-md")), "{sources:?}");
}

#[test]
fn a_rule_file_is_only_read_by_a_sync_under_a_yes() {
    let t = Temp::new("rules-readonly");
    let cwd = t.path().join("repo");
    let c = config_with(t.path(), r#""rules": true"#);
    let e = atlas::project::resolve(&c, &cwd, None).unwrap().config;
    let db = e.db.clone().unwrap();
    let s = Settings::from_config(Some(&e), &db).unwrap();
    assert_eq!(s.recall.rules, rules::Mode::Live);
    let roots: Vec<(String, PathBuf)> = e.sources.iter().map(|x| (x.name.clone(), x.root.clone())).collect();
    let targets = sync::targets(&s, &roots);
    let before = fs::read(cwd.join("CLAUDE.md")).unwrap();
    let mut store = MemoryStore::open(&s.store).unwrap();
    for t in &targets {
        let mut w = store.writer().unwrap();
        import::import(&mut w, &t.spec, &t.path, "test").unwrap();
        w.commit().unwrap();
    }
    assert!(store.head("claude-md:r00001").unwrap().is_some());
    assert_eq!(fs::read(cwd.join("CLAUDE.md")).unwrap(), before);
    assert_eq!(fs::read(t.path().join("mem").join("MEMORY.md")).unwrap(), b"# Memory\n", "no tree configured: MEMORY.md stands");
}

/// `wired` plus records that out-match the rulings: their titles hold every word
/// of the query, and the rulings hold three of the four.
fn out_matched(tag: &str, mode: rules::Mode, n: usize) -> (Temp, Settings) {
    let (t, s) = wired(tag, mode);
    let mut store = MemoryStore::open(&s.store).unwrap();
    let mut w = store.writer().unwrap();
    for i in 0..n {
        let d = atlas::memory::Draft { title: "widget cache bound entries".into(), body: String::new(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() };
        w.create(&format!("decision:d{i}"), "decision", "", "project", &d, None).unwrap();
    }
    w.commit().unwrap();
    (t, s)
}

const OUT_MATCHED: &str = "widget cache bound entries";

#[test]
fn a_better_match_does_not_displace_the_live_ruling() {
    // One leaves it a pointer; three take a floor under it (bm25 near zero, the
    // words in over half the records); twelve fill the limit and more.
    for n in [1, 3, 12, 40] {
        let (t, on) = out_matched(&format!("rules-q-disp{n}"), rules::Mode::Live, n);
        let a = query(&on, t.path(), atlas::json!({"query": OUT_MATCHED}));
        assert!(a.starts_with("== rules:r00002 "), "{n} better matches, the live ruling leads: {a}");
        assert!(a.contains("[replaces rules:r00001]") && a.contains("grows freely"), "{n}: {a}");
        assert!(!a.contains("== rules:r00001") && !a.contains("- rules:r00001"), "{n}, the old text stays one recall away: {a}");
        assert!(a.contains("decision:d"), "{n}, nothing is lost for it: {a}");
    }
}

#[test]
fn other_rulings_that_match_better_do_not_displace_it_either() {
    let (t, on) = wired("rules-q-rulings", rules::Mode::Live);
    let mut store = MemoryStore::open(&on.store).unwrap();
    let mut w = store.writer().unwrap();
    for i in 0..90 {
        let d = atlas::memory::Draft { title: "widget cache bound entries".into(), body: String::new(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() };
        w.create(&format!("rule:d{i}"), "rule", "", "project", &d, None).unwrap();
    }
    w.commit().unwrap();
    drop(store);
    let a = query(&on, t.path(), atlas::json!({"query": OUT_MATCHED}));
    assert!(a.starts_with("== rules:r00002 ") && a.contains("[replaces rules:r00001]"), "{a}");
}

#[test]
fn an_unreplaced_ruling_stays_in_the_answer_in_its_rank() {
    let (t, on) = out_matched("rules-q-plain", rules::Mode::Live, 12);
    let mut store = MemoryStore::open(&on.store).unwrap();
    let mut w = store.writer().unwrap();
    w.unmark_superseded("rules:r00001", "not replaced in this test").unwrap();
    w.commit().unwrap();
    drop(store);
    let a = query(&on, t.path(), atlas::json!({"query": OUT_MATCHED}));
    assert!(a.starts_with("== decision:d"), "no pair, no lift: {a}");
    assert!(a.contains("- rules:r00001 ") && a.contains("- rules:r00002 "), "both rulings kept as pointers: {a}");
    assert!(!a.contains("replaces"), "{a}");
}

#[test]
fn a_ruling_about_little_of_the_query_is_not_kept_or_lifted() {
    let (t, on) = out_matched("rules-q-thin", rules::Mode::Live, 12);
    let a = query(&on, t.path(), atlas::json!({"query": "bound zebra quokka tapir"}));
    assert!(!a.starts_with("== rules:r00002"), "one word of four is not about the ruling: {a}");
}

#[test]
fn a_kept_ruling_survives_a_small_budget() {
    let (t, on) = out_matched("rules-q-budget", rules::Mode::Live, 12);
    let a = query(&on, t.path(), atlas::json!({"query": OUT_MATCHED, "budget_tokens": 1}));
    assert!(a.contains("rules:r00002"), "{a}");
}

/// `memory.rules: "off"` hides rules and memories from a query. They must not hold
/// the limit first, or the journal or decision ranked after them reads as no match.
#[test]
fn with_rules_off_what_is_hidden_does_not_use_up_the_limit() {
    let (t, off) = wired("rules-off-limit", rules::Mode::Off);
    let mut store = MemoryStore::open(&off.store).unwrap();
    let mut w = store.writer().unwrap();
    let draft = |title: &str, body: &str| atlas::memory::Draft { title: title.into(), body: body.into(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() };
    for i in 0..40 {
        w.create(&format!("note-{i}"), "memory", "project", "project", &draft(&format!("sprocket {i}"), "sprocket sprocket"), None).unwrap();
    }
    w.create("decision:far", "decision", "", "project", &draft("item", &format!("sprocket {}", "more other words ".repeat(30))), None).unwrap();
    w.commit().unwrap();
    let a = query(&off, t.path(), atlas::json!({"query": "sprocket", "budget_tokens": 5000}));
    assert!(a.contains("decision:far"), "{a}");
    assert!(!a.contains("no records match") && !a.contains("note-"), "{a}");
    // The hook's 24 rows, past the first window too.
    let g = atlas::memory::recall::Gate { rules: rules::Mode::Off, fts_floor: 0.0, min_terms: 1, cos_floor: 0.0, cos_alone: 1.0 };
    let c = atlas::memory::recall::candidates(&store, "sprocket", &g, None).unwrap();
    assert!(c.iter().any(|x| x.head.key == "decision:far"), "{}", c.len());
    // A list of the unhidden alone is what the limit counts.
    let terms = vec!["sprocket".to_string()];
    let keys: Vec<String> = store.search_with(&terms, None, None, 1, rules::Mode::Off).unwrap().into_iter().map(|(h, _, _)| h.key).collect();
    assert_eq!(keys, ["decision:far"]);
}

/// A replaced ruling whose successor is gone shows nothing: it must not hold a row
/// of the limit, or the answer can say nothing matches when something does.
#[test]
fn a_hit_with_no_live_successor_does_not_use_up_the_limit() {
    let (t, on) = wired("rules-dead-chain", rules::Mode::Live);
    let mut store = MemoryStore::open(&on.store).unwrap();
    let mut w = store.writer().unwrap();
    w.retire("rules:r00002", "gone from the source").unwrap();
    let draft = atlas::memory::Draft { title: "item".into(), body: format!("bounded {}", "more other words ".repeat(30)), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() };
    w.create("decision:far", "decision", "", "project", &draft, None).unwrap();
    w.commit().unwrap();
    assert!(rules::live_of(&store, "rules:r00001").unwrap().is_none());
    let a = query(&on, t.path(), atlas::json!({"query": "bounded", "limit": 1}));
    assert!(a.contains("decision:far"), "{a}");
    assert!(!a.contains("no records match"), "{a}");
    assert!(!a.contains("+more"), "nothing else can show: {a}");
    // A successor still live keeps the old record in the search, for its tag.
    let (_t, on) = wired("rules-live-chain", rules::Mode::Live);
    let store = MemoryStore::open(&on.store).unwrap();
    let terms = vec!["bounded".to_string()];
    assert!(store.search_with(&terms, None, None, 5, rules::Mode::Live).unwrap().iter().any(|(h, _, _)| h.key == "rules:r00001"));
}

/// The rulings a query may not lose come by the same words: with a filter, all of them.
#[test]
fn a_kept_ruling_under_a_filter_keeps_every_word() {
    let (_t, on) = wired("rules-sure-terms", rules::Mode::Live);
    let mut store = MemoryStore::open(&on.store).unwrap();
    let mut w = store.writer().unwrap();
    let rare: Vec<String> = (0..12).map(|i| format!("rareword{}", char::from(b'a' + i))).collect();
    for (i, word) in rare.iter().enumerate() {
        let d = atlas::memory::Draft { title: word.clone(), body: String::new(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() };
        w.create(&format!("decision:rare{i}"), "decision", "", "project", &d, None).unwrap();
    }
    w.commit().unwrap();
    let terms: Vec<String> = rare.iter().cloned().chain(["widget".to_string()]).collect();
    assert!(store.search_sure(&terms, Some("rule"), None, 1).unwrap().iter().any(|(h, _, _)| h.key == "rules:r00002"));
    assert!(store.search_sure(&terms, None, None, 1).unwrap().is_empty(), "no filter: the 12 rarest");
}

#[test]
fn a_filter_still_filters_with_rules_on() {
    let (t, on) = out_matched("rules-q-kind", rules::Mode::Live, 3);
    let a = query(&on, t.path(), atlas::json!({"query": OUT_MATCHED, "kind": "decision"}));
    assert!(!a.contains("rules:r0"), "{a}");
    let a = query(&on, t.path(), atlas::json!({"query": OUT_MATCHED, "kind": "rule"}));
    assert!(a.contains("rules:r00002") && !a.contains("decision:d"), "{a}");
}

#[test]
fn at_the_hook_a_better_match_does_not_displace_the_live_ruling() {
    for n in [1, 40] {
        let (_t, on) = out_matched(&format!("rules-hook-disp{n}"), rules::Mode::Live, n);
        let c = prompt(&on, OUT_MATCHED);
        let live = c.find("- rules:r00002: 01-04 ").unwrap_or_else(|| panic!("{n}: {c}"));
        assert!(c[live..].lines().next().unwrap().contains("[replaces rules:r00001]"), "{c}");
        assert!(c.find("decision:d").is_none_or(|d| live < d), "{n}, it comes first: {c}");
    }
}

/// Today has no such guarantee, and must not grow one: unset is what origin/main does.
#[test]
fn unset_keeps_todays_order_and_cut_off() {
    let (t, today) = out_matched("rules-q-today-disp", rules::Mode::Today, 12);
    let a = query(&today, t.path(), atlas::json!({"query": OUT_MATCHED}));
    assert!(a.starts_with("== decision:d") && !a.contains("rules:r0") && !a.contains("replaces"), "{a}");
    let (_t, today) = out_matched("rules-hook-today-disp", rules::Mode::Today, 1);
    let c = prompt(&today, OUT_MATCHED);
    assert!(!c.contains("replaces") && !c.contains(": 01-04 "), "{c}");
}

/// Resolves the project under this `memory.rules` and syncs what it names, as
/// a rescan would. The settings it read, ready for a query.
fn sync_under(root: &std::path::Path, rules: &str) -> Settings {
    let e = atlas::project::resolve(&config_only(root, rules), &root.join("repo"), None).unwrap().config;
    let mut s = Settings::from_config(Some(&e), &e.db.clone().unwrap()).unwrap();
    s.recall.fts_floor = 0.0;
    s.recall.hybrid_fts_floor = 0.0;
    let roots: Vec<(String, PathBuf)> = e.sources.iter().map(|x| (x.name.clone(), x.root.clone())).collect();
    let mut store = MemoryStore::open(&s.store).unwrap();
    for t in &sync::targets(&s, &roots) {
        sync::sync(&mut store, t).unwrap();
    }
    s
}

/// `false` is the old unset: the rule file and memory folder named by hand keep
/// syncing and are recalled. `"off"` is the old `false`: neither is synced or
/// recalled, and the store keeps what it holds.
#[test]
fn false_keeps_what_is_named_by_hand_and_off_stops_it() {
    let t = Temp::new("rules-false-off");
    let root = t.path();
    config_with(root, "");
    let intent = root.join("repo").join("INTENT.md");
    write(&intent, "- 2026-01-02 Alex: the wombat ledger rotates weekly.\n");
    write(&root.join("mem").join("quokka-rule.md"), "---\nname: quokka-rule\nmetadata:\n  type: feedback\n  modified: 2026-01-07T10:00:00.000Z\n---\n\nThe quokka archive is pruned nightly.\n");
    let hits = |s: &Settings, q: &str| query(s, root, atlas::json!({"query": q}));
    // False: synced and recalled, as unset.
    let s = sync_under(root, r#""rules": false"#);
    assert_eq!(s.recall.rules, rules::Mode::Today);
    assert_eq!(status(&MemoryStore::open(&s.store).unwrap(), "intent:r00001"), "active");
    let a = hits(&s, "wombat ledger rotates");
    assert!(a.contains("intent:r00001"), "{a}");
    let a = hits(&s, "quokka archive pruned");
    assert!(a.contains("quokka"), "{a}");
    // A new line under false is synced too.
    write(&intent, "- 2026-01-02 Alex: the wombat ledger rotates weekly.\n- 2026-01-03 Alex: the numbat queue drains daily.\n");
    let s = sync_under(root, r#""rules": false"#);
    let a = hits(&s, "numbat queue drains");
    assert!(a.contains("intent:r00002"), "{a}");
    // Off: no sync, no query recall, and the store is kept.
    write(&intent, "- 2026-01-02 Alex: the wombat ledger rotates weekly.\n- 2026-01-03 Alex: the numbat queue drains daily.\n- 2026-01-04 Alex: the bilby cache is cold.\n");
    let s = sync_under(root, r#""rules": "off""#);
    assert_eq!(s.recall.rules, rules::Mode::Off);
    let store = MemoryStore::open(&s.store).unwrap();
    assert!(store.head("intent:r00003").unwrap().is_none());
    assert_eq!(status(&store, "intent:r00001"), "active");
    drop(store);
    for q in ["wombat ledger rotates", "quokka archive pruned", "numbat queue drains"] {
        let a = hits(&s, q);
        assert!(!a.contains("intent:r0") && !a.contains("quokka"), "{q}: {a}");
    }
    let by_key = query(&s, root, atlas::json!({"key": "intent:r00001"}));
    assert!(by_key.contains("wombat ledger"), "off keeps the store: {by_key}");
    // Unset again: the sync resumes.
    let s = sync_under(root, "");
    assert_eq!(s.recall.rules, rules::Mode::Today);
    assert_eq!(status(&MemoryStore::open(&s.store).unwrap(), "intent:r00003"), "active");
}

/// `memory.rules` takes true, false or "off", nothing else.
#[test]
fn memory_rules_takes_true_false_or_off() {
    let t = Temp::new("rules-values");
    let cfg = t.path().join("atlas.json");
    for bad in [r#""rules": "on""#, r#""rules": "OFF""#, r#""rules": 0"#] {
        write(&cfg, &format!(r#"{{"memory": {{{bad}}}, "watch": false}}"#));
        let e = atlas::config::Config::load(&cfg).unwrap_err().to_string();
        assert!(e.contains("memory.rules") && e.contains("\"off\""), "{bad}: {e}");
    }
}

const THREE_RULES: &str = "- 2026-01-02 Alex: the widget cache is bounded.\n- 2026-01-03 Alex: the widget cache grows freely.\n- 2026-01-04 Alex: the gamma queue drains oldest first.\n";

#[test]
fn the_setting_can_be_left_and_come_back_with_nothing_lost() {
    let t = Temp::new("rules-transitions");
    let root = t.path();
    config_with(root, "");
    write(&root.join("repo").join("CLAUDE.md"), THREE_RULES);
    let status_of = |s: &Settings, k: &str| status(&MemoryStore::open(&s.store).unwrap(), k);
    // Unset: the root file is not read; what is named by hand is.
    let s = sync_under(root, "");
    let store = MemoryStore::open(&s.store).unwrap();
    assert!(store.head("claude-md:r00001").unwrap().is_none());
    assert_eq!(status(&store, "intent:r00001"), "active");
    drop(store);
    // True: it is, and a mark is made.
    let s = sync_under(root, r#""rules": true"#);
    assert_eq!(status_of(&s, "claude-md:r00003"), "active");
    let mut store = MemoryStore::open(&s.store).unwrap();
    mark(&mut store, "claude-md:r00001", "claude-md:r00002").unwrap();
    drop(store);
    // Back to unset, with no block at all, and a line leaves the file: nothing
    // retires or moves, and the stale record is still recalled, a limit the wiki
    // states. An empty block, what off can leave, reads the same.
    write(&root.join("repo").join("CLAUDE.md"), &THREE_RULES.replace("- 2026-01-04 Alex: the gamma queue drains oldest first.\n", ""));
    let s = sync_under(root, "");
    assert_eq!(status_of(&s, "claude-md:r00003"), "active");
    let s = sync_under(root, "absent");
    assert_eq!(s.recall.rules, rules::Mode::Today);
    assert_eq!(status_of(&s, "claude-md:r00003"), "active");
    assert_eq!(status_of(&s, "claude-md:r00001"), "superseded");
    assert_eq!(status_of(&s, "intent:r00001"), "active");
    let a = query(&s, root, atlas::json!({"query": "gamma queue drains"}));
    assert!(a.contains("claude-md:r00003"), "{a}");
    // False: the same as unset; the stale record is still recalled.
    let s = sync_under(root, r#""rules": false"#);
    assert_eq!(s.recall.rules, rules::Mode::Today);
    assert_eq!(status_of(&s, "claude-md:r00003"), "active");
    let a = query(&s, root, atlas::json!({"query": "gamma queue drains"}));
    assert!(a.contains("claude-md:r00003"), "{a}");
    // Off: nothing retires there either, and recall leaves the kind out.
    let s = sync_under(root, r#""rules": "off""#);
    assert_eq!(s.recall.rules, rules::Mode::Off);
    assert_eq!(status_of(&s, "claude-md:r00003"), "active");
    assert_eq!(status_of(&s, "intent:r00001"), "active");
    let a = query(&s, root, atlas::json!({"query": "gamma queue drains"}));
    assert!(!a.contains("claude-md:r0"), "{a}");
    // True again: the sync resumes, the removal retires, the mark holds.
    let s = sync_under(root, r#""rules": true"#);
    assert_eq!(status_of(&s, "claude-md:r00003"), "retired");
    assert_eq!(status_of(&s, "claude-md:r00001"), "superseded");
    assert_eq!(status_of(&s, "claude-md:r00002"), "active");
    let a = query(&s, root, atlas::json!({"query": "widget cache bounded"}));
    assert!(a.contains("claude-md:r00002") && a.contains("replaces claude-md:r00001"), "{a}");
}

/// A limit the wiki states: marks work with the flag unset, and a whole mark
/// then hides the old record with no pointer to what holds now.
#[test]
fn a_whole_mark_with_the_flag_unset_leaves_no_pointer_to_its_successor() {
    let (t, today) = wired("rules-mark-today", rules::Mode::Today);
    // These words are the old ruling's alone.
    let a = query(&today, t.path(), atlas::json!({"query": "bounded sixty four"}));
    assert!(!a.contains("rules:r00001") && !a.contains("rules:r00002"), "{a}");
    assert!(!a.contains("replaces"), "{a}");
    let by_key = query(&today, t.path(), atlas::json!({"key": "rules:r00001"}));
    assert!(by_key.contains("sixty four"), "the key still reads it: {by_key}");
    // The successor's own words find the successor, untagged.
    let a = query(&today, t.path(), atlas::json!({"query": "grows freely"}));
    assert!(a.contains("rules:r00002") && !a.contains("replaces"), "{a}");
}

/// What can be asserted without a main build; the comparison with main itself
/// is the differential in the release's PR.
#[test]
fn a_sync_under_unset_writes_no_rules_refs_and_no_new_exports() {
    let t = Temp::new("rules-unset-writes");
    let root = t.path();
    config_with(root, "");
    write(&root.join("repo").join("CLAUDE.md"), THREE_RULES);
    // Words the scan would take for a reversal, in what the sync does read.
    write(&root.join("repo").join("INTENT.md"), "- 2026-01-02 Alex: a named rule.\n- 2026-01-03 Alex: this reverses r00001; the named rule goes.\n");
    let claude = fs::read(root.join("repo").join("CLAUDE.md")).unwrap();
    let e = atlas::project::resolve(&config_only(root, ""), &root.join("repo"), None).unwrap().config;
    let s = Settings::from_config(Some(&e), &e.db.clone().unwrap()).unwrap();
    let mut names: Vec<String> = s.exports.iter().map(|x| x.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["intent", "memory"]);
    let roots: Vec<(String, PathBuf)> = e.sources.iter().map(|x| (x.name.clone(), x.root.clone())).collect();
    let mut syncer = sync::Syncer::new(s.clone(), &roots).unwrap();
    for source in ["intent", "memory"] {
        syncer.check(source, None).unwrap();
    }
    assert_eq!(syncer.take_reports().iter().filter(|r| r.export == "intent").map(|r| r.new).sum::<usize>(), 2, "the two named rules");
    drop(syncer);
    let store = MemoryStore::open(&s.store).unwrap();
    assert!(store.head("intent:r00002").unwrap().is_some());
    assert!(store.head("claude-md:r00001").unwrap().is_none(), "the root file is not read");
    assert!(store.linked_to(rules::AMENDS, "intent:r00001").unwrap().is_empty());
    drop(store);
    let conn = rusqlite::Connection::open(&s.store).unwrap();
    let refs: i64 = conn.query_row("SELECT COUNT(*) FROM refs WHERE kind IN (?1, ?2, ?3)", [rules::AMENDS, rules::AMENDED_BY, rules::CHECKED], |r| r.get(0)).unwrap();
    assert_eq!(refs, 0);
    drop(conn);
    // The exports written are the named ones' and nothing else, at any depth.
    let mut written = Vec::new();
    let mut folders = vec![s.export_dir.clone().expect("a project has an export folder")];
    while let Some(d) = folders.pop() {
        for f in fs::read_dir(&d).unwrap().flatten() {
            if f.path().is_dir() {
                folders.push(f.path());
            } else {
                written.push(f.file_name().to_string_lossy().to_lowercase());
            }
        }
    }
    assert!(written.iter().any(|n| n == "intent.md"), "the sync did write: {written:?}");
    assert!(!written.iter().any(|n| n.contains("claude")), "{written:?}");
    assert_eq!(fs::read(root.join("repo").join("CLAUDE.md")).unwrap(), claude);
    assert_eq!(fs::read(root.join("mem").join("MEMORY.md")).unwrap(), b"# Memory\n", "no tree configured: MEMORY.md stands");
}

#[test]
fn the_writing_commands_say_they_are_experimental() {
    let (t, s) = wired("rules-label", rules::Mode::Today);
    let cfg = t.path().join("atlas.json");
    write(&cfg, &format!("{{\"memory\": {{\"store\": {:?}}}, \"watch\": false, \"vaults\": [{{\"name\": \"notes\", \"path\": \"vault\"}}]}}", s.store.to_string_lossy().replace('\\', "/")));
    fs::create_dir_all(t.path().join("vault")).unwrap();
    let run = |a: &[&str]| {
        let out = Command::new(bin()).args(a).env("ATLAS_CONFIG", &cfg).env("ATLAS_HOME", t.path().join("home")).output().unwrap();
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    };
    for args in [&["rules", "link"][..], &["rules", "answer", "rules:r00002"], &["rules", "unmark", "rules:r00001"]] {
        let (ok, out) = run(args);
        assert!(ok && out.lines().filter(|l| l.contains("experimental, off by default")).count() == 1, "{args:?}: {out}");
    }
    // Reading commands stay quiet; usage says it.
    let (ok, out) = run(&["rules", "counts"]);
    assert!(ok && !out.contains("xperimental"), "{out}");
    let (ok, out) = run(&["rules", "nonsense"]);
    assert!(!ok && out.contains("Experimental, off by default"), "{out}");
    let (_, out) = run(&["ruling", "--help"]);
    assert!(out.contains("experimental and off by default"), "{out}");
    let (_, out) = run(&["--help"]);
    assert!(out.lines().any(|l| l.contains("atlas rules") && l.contains("experimental, off by default")), "{out}");
}
