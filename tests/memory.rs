//! The memory store: append-only history, hash chains, the `.jsonl` copy,
//! the tools, exports byte-exact, the integrity check and snapshots.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use atlas::json;
use atlas::json::Value;
use atlas::memory::archive;
use atlas::memory::check::{check, snapshot};
use atlas::memory::export::{self, ExportSpec, Shape};
use atlas::memory::settings::Settings;
use atlas::memory::{import, Draft, MemoryStore, Place};
use common::*;

const RULES: &str = "# Rules\n\nIntro paragraph.\n\n## Storage\n- 2026-01-02 alpha: the widget cache is bounded at 64 entries.\n  It evicts least recently used first.\n- 2026-01-03 beta: exports are written atomically.\r\n\n## Other\n- gamma: unrelated item\n";
const RESTORE: &str = "Preamble line\r\n# Resume point\r\nstate one\r\n## Queue\nsecond\n";
const INDEX: &str = "- [Widget cache](widget-cache.md) - the widget cache bound\n- [Export rule](export-rule.md) - atomic exports\n";
const MEM_A: &str = "---\nname: widget-cache\ndescription: the widget cache bound\nmetadata:\n  type: feedback\n---\n\nThe widget cache holds 64 entries.\n";
const MEM_B: &str = "---\nname: export-rule\ndescription: atomic exports\nmetadata:\n  type: project\n---\n\nExports use temp and rename.\n";

struct Fix {
    _tmp: Temp,
    src: PathBuf,
    out: PathBuf,
    settings: Settings,
}

fn specs() -> Vec<ExportSpec> {
    vec![
        ExportSpec { name: "memory".into(), shape: Shape::MemoryDir, path: "memory".into(), index: "INDEX.md".into(), handoffs: false, index_cap: None, index_topics: None },
        ExportSpec { name: "rules".into(), shape: Shape::Items, path: "RULES.md".into(), index: String::new(), handoffs: false, index_cap: None, index_topics: None },
        ExportSpec { name: "restore".into(), shape: Shape::Headings, path: "RESUME.md".into(), index: String::new(), handoffs: true, index_cap: None, index_topics: None },
    ]
}

fn fixture(tag: &str) -> Fix {
    let tmp = Temp::new(tag);
    let src = tmp.path().join("src");
    write(&src.join("memory").join("INDEX.md"), INDEX);
    write(&src.join("memory").join("widget-cache.md"), MEM_A);
    write(&src.join("memory").join("export-rule.md"), MEM_B);
    write(&src.join("RULES.md"), RULES);
    write(&src.join("RESUME.md"), RESTORE);
    let out = tmp.path().join("out");
    let mut settings = Settings::from_config(None, &tmp.path().join("state").join("index.sqlite")).unwrap();
    settings.export_dir = Some(out.clone());
    settings.exports = specs();
    settings.snapshot_secs = 0;
    Fix { src, out, settings, _tmp: tmp }
}

fn import_all(f: &Fix) -> MemoryStore {
    let mut store = MemoryStore::open(&f.settings.store).unwrap();
    let mut w = store.writer().unwrap();
    let sp = specs();
    let a = import::import(&mut w, &sp[0], &f.src.join("memory"), "test").unwrap();
    let b = import::import(&mut w, &sp[1], &f.src.join("RULES.md"), "test").unwrap();
    let c = import::import(&mut w, &sp[2], &f.src.join("RESUME.md"), "test").unwrap();
    w.commit().unwrap();
    assert_eq!(a.of("memory"), 2);
    assert_eq!(b.of("rule"), 3);
    assert_eq!(b.of("block"), 2);
    assert_eq!(c.of("section"), 2);
    assert_eq!(c.of("block"), 1);
    store
}

fn draft(body: &str) -> Draft {
    Draft { title: "t".into(), body: body.into(), reason: "testing the store".into(), author: "test".into(), client: "test".into(), ..Draft::default() }
}

#[test]
fn history_is_append_only_in_sqlite_itself() {
    let f = fixture("mem-append");
    let mut store = import_all(&f);
    let c = store.conn();
    assert!(c.execute("UPDATE versions SET body = 'x'", []).is_err());
    assert!(c.execute("DELETE FROM versions", []).is_err());
    assert!(c.execute("DELETE FROM records", []).is_err());
    assert!(c.execute("UPDATE records SET key = 'other' WHERE key = 'widget-cache'", []).is_err());
    // A revision keeps the prior version and extends the chain.
    let h = store.head("widget-cache").unwrap().unwrap();
    let mut w = store.writer().unwrap();
    assert!(w.revise("widget-cache", Some("0000"), &draft("x")).is_err());
    let r = w.revise("widget-cache", Some(&h.sha), &Draft { title: h.title.clone(), meta: h.meta.clone(), ..draft("\nThe widget cache holds 128 entries.\n") }).unwrap();
    w.commit().unwrap();
    assert_eq!(r.seq, 2);
    assert_eq!(store.history("widget-cache").unwrap().len(), 2);
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
}

#[test]
fn exports_of_an_import_are_byte_identical_and_round_trip() {
    let f = fixture("mem-export");
    let store = import_all(&f);
    for spec in specs() {
        let rep = export::write(&store, &spec, &f.out, false).unwrap();
        assert!(rep.refused.is_empty(), "{:?}", rep.refused);
    }
    for rel in ["memory/INDEX.md", "memory/widget-cache.md", "memory/export-rule.md", "RULES.md", "RESUME.md"] {
        assert_eq!(fs::read(f.out.join(rel)).unwrap(), fs::read(f.src.join(rel)).unwrap(), "{rel}");
    }
    let rep = check(&store, &specs(), Some(&f.out), true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
    assert_eq!(rep.records, 10);
    assert_eq!(rep.copy_lines, 10);
}

#[test]
fn the_check_names_a_tampered_copy_and_a_hand_edit() {
    let f = fixture("mem-tamper");
    let store = import_all(&f);
    for spec in specs() {
        export::write(&store, &spec, &f.out, false).unwrap();
    }
    // A hand edit to an export is refused on the next write, and reported.
    let rules = f.out.join("RULES.md");
    fs::write(&rules, RULES.replace("64 entries", "65 entries")).unwrap();
    let rep = export::write(&store, &specs()[1], &f.out, false).unwrap();
    assert_eq!(rep.refused.len(), 1);
    assert!(rep.refused[0].1.contains("line 6"), "{:?}", rep.refused);
    let c = check(&store, &specs(), Some(&f.out), true).unwrap();
    assert!(c.problems.iter().any(|p| p.starts_with("disk:") && p.contains("RULES.md")), "{:?}", c.problems);
    // A changed copy line is caught by its sha.
    let jl = store.jsonl().to_path_buf();
    let text = fs::read_to_string(&jl).unwrap();
    fs::write(&jl, text.replacen("least recently used", "most recently used", 1)).unwrap();
    let c = check(&store, &specs(), None, true).unwrap();
    assert!(c.problems.iter().any(|p| p.starts_with("copy:")), "{:?}", c.problems);
}

#[test]
fn supersede_keeps_the_old_record_and_marks_the_export() {
    let f = fixture("mem-supersede");
    let mut store = import_all(&f);
    let mut w = store.writer().unwrap();
    w.supersede(&["rules:r00001".into()], "rules:r00004", "rule", "", "rules", &draft("- 2026-02-01 alpha2: the widget cache is bounded at 128 entries.\n"), Some(("rules", Place::After("rules:r00001".into()))))
        .unwrap();
    assert!(w.supersede(&["rules:r00001".into()], "rules:r00005", "rule", "", "rules", &draft("- again\n"), None).is_err());
    w.commit().unwrap();
    let old = store.head("rules:r00001").unwrap().unwrap();
    assert_eq!(old.status, "superseded");
    assert_eq!(old.superseded_by.as_deref(), Some("rules:r00004"));
    let text = &export::render(&store, &specs()[1]).unwrap()[0].1;
    let at_old = text.find("alpha:").unwrap();
    let marker = text.find("  (superseded ").unwrap();
    let at_new = text.find("alpha2:").unwrap();
    assert!(at_old < marker && marker < at_new, "{text}");
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
}

#[test]
fn snapshots_verify_and_roll_up_oldest_past_the_cap() {
    let f = fixture("mem-snap");
    let store = import_all(&f);
    let dir = f.out.join("snaps");
    let a = snapshot(&store, &dir, u64::MAX).unwrap().unwrap();
    assert!(a.bytes > 0);
    assert_eq!(a.records, 10);
    // Same second: the second name takes a counter and still sorts after.
    let b = snapshot(&store, &dir, a.bytes + a.bytes / 2).unwrap().unwrap();
    assert_eq!(b.rolled.iter().map(|r| r.snapshot.clone()).collect::<Vec<_>>(), vec![a.path.clone()]);
    assert_eq!((b.rolled[0].versions, b.rolled[0].added), (10, 10));
    assert!(b.path.exists() && !a.path.exists());
    assert!(dir.join(archive::ARCHIVE_FILE).exists());
}

/// Every answer `as_of` gives for every key at every time that matters:
/// before anything, after everything, and at and just before each write.
fn answers(sources: &[PathBuf]) -> Vec<(String, String, Option<archive::AsOf>)> {
    let keys = archive::keys(sources).unwrap();
    let mut times: Vec<String> = vec!["0000".into(), "9999".into()];
    for p in sources {
        let c = rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut st = c.prepare("SELECT time FROM versions UNION SELECT time FROM supersedes").unwrap();
        for t in st.query_map([], |r| r.get::<_, String>(0)).unwrap() {
            let t = t.unwrap();
            let mut before = t.clone();
            before.pop();
            times.push(before);
            times.push(t);
        }
    }
    times.sort();
    times.dedup();
    let mut out = Vec::new();
    for k in &keys {
        for t in &times {
            out.push((k.clone(), t.clone(), archive::as_of(sources, k, t).unwrap()));
        }
    }
    out
}

fn revise(store: &mut MemoryStore, key: &str, body: &str) {
    let h = store.head(key).unwrap().unwrap();
    let mut w = store.writer().unwrap();
    w.revise(key, Some(&h.sha), &Draft { title: h.title.clone(), meta: h.meta.clone(), ..draft(body) }).unwrap();
    w.commit().unwrap();
}

#[test]
fn roll_up_loses_no_version_and_keeps_every_answer() {
    let f = fixture("mem-archive");
    let mut store = import_all(&f);
    let dir = f.out.join("snaps");
    let mut snaps = vec![snapshot(&store, &dir, u64::MAX).unwrap().unwrap().path];
    revise(&mut store, "widget-cache", "\nThe widget cache holds 128 entries.\n");
    snaps.push(snapshot(&store, &dir, u64::MAX).unwrap().unwrap().path);
    {
        let mut w = store.writer().unwrap();
        w.supersede(&["rules:r00001".into()], "rules:r00004", "rule", "", "rules", &draft("- 2026-02-01 alpha2: bounded at 128.\n"), Some(("rules", Place::After("rules:r00001".into()))))
            .unwrap();
        w.create("journal:track-x:0001", "journal", "note", "track-x", &draft("first entry\n"), None).unwrap();
        w.add_refs("journal:track-x:0001", &[("lane".into(), "track-x".into())]).unwrap();
        w.commit().unwrap();
    }
    snaps.push(snapshot(&store, &dir, u64::MAX).unwrap().unwrap().path);
    revise(&mut store, "widget-cache", "\nThe widget cache holds 256 entries.\n");
    revise(&mut store, "journal:track-x:0001", "first entry, corrected\n");
    let last = snapshot(&store, &dir, u64::MAX).unwrap().unwrap();
    snaps.push(last.path.clone());

    let before = archive::sources(&dir, None).unwrap();
    assert_eq!(before.len(), 4);
    let versions_before = archive::version_set(&before).unwrap();
    let answers_before = answers(&before);
    // 10 imported, two revisions of widget-cache, the new rule, the journal
    // entry and its revision.
    assert_eq!(versions_before.len(), 15);
    assert!(answers_before.iter().any(|(k, _, a)| k == "rules:r00001" && a.as_ref().is_some_and(|a| a.superseded.is_some())));
    assert!(answers_before.iter().any(|(k, _, a)| k == "widget-cache" && a.as_ref().is_some_and(|a| a.seq == 2)));

    // A budget of zero folds everything but the newest snapshot.
    let rolled = archive::roll_up(&dir, 0, Some(&last.path)).unwrap();
    assert_eq!(rolled.iter().map(|r| r.snapshot.clone()).collect::<Vec<_>>(), snaps[..3].to_vec());
    // Each version is stored once: 10, then 1 new, then 2 new.
    assert_eq!(rolled.iter().map(|r| r.added).collect::<Vec<_>>(), vec![10, 1, 2]);
    assert!(snaps[..3].iter().all(|p| !p.exists()) && last.path.exists());
    let after = archive::sources(&dir, None).unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(archive::version_set(&after).unwrap(), versions_before);
    assert_eq!(answers(&after), answers_before);
    // The archive alone holds the folded history.
    let only = vec![dir.join(archive::ARCHIVE_FILE)];
    let w2 = archive::as_of(&only, "widget-cache", "9999").unwrap().unwrap();
    assert_eq!((w2.seq, w2.body.as_str()), (2, "\nThe widget cache holds 128 entries.\n"));
    // Folding the last one too: the archive is then the whole history.
    archive::fold(&dir.join(archive::ARCHIVE_FILE), &last.path).unwrap();
    assert_eq!(answers(&only), answers_before);
    assert_eq!(archive::version_set(&only).unwrap(), versions_before);
    // Folding keeps time order: the same snapshot again is refused.
    assert!(archive::fold(&dir.join(archive::ARCHIVE_FILE), &last.path).is_err());
    // The live store as a further source changes no answer.
    let mut with_store = only.clone();
    with_store.push(store.path().to_path_buf());
    assert_eq!(answers(&with_store), answers_before);
}

#[test]
fn a_damaged_snapshot_is_not_folded_or_removed() {
    let f = fixture("mem-archive-bad");
    let store = import_all(&f);
    let dir = f.out.join("snaps");
    let a = snapshot(&store, &dir, u64::MAX).unwrap().unwrap();
    let b = snapshot(&store, &dir, u64::MAX).unwrap().unwrap();
    {
        let c = rusqlite::Connection::open(&a.path).unwrap();
        c.execute_batch("DROP TRIGGER versions_no_update; UPDATE versions SET body = body || 'x' WHERE vid = 1;").unwrap();
    }
    let e = archive::roll_up(&dir, 0, Some(&b.path)).unwrap_err().to_string();
    assert!(e.contains("does not match its sha"), "{e}");
    assert!(a.path.exists());
    assert!(archive::version_set(&[dir.join(archive::ARCHIVE_FILE)]).unwrap().is_empty());
}

fn server(f: &Fix) -> (atlas::tools::Server, Fx) {
    let fx = Fx::new("mem-server");
    let mut s = fx.server();
    s.set_memory(f.settings.clone());
    (s, fx)
}

fn text(v: Value) -> String {
    v["text"].as_str().unwrap_or_default().to_string()
}

#[test]
fn tools_remember_recall_supersede_and_export() {
    let f = fixture("mem-tools");
    drop(import_all(&f));
    for spec in specs() {
        let store = MemoryStore::open(&f.settings.store).unwrap();
        export::write(&store, &spec, &f.out, false).unwrap();
    }
    let (mut s, _fx) = server(&f);
    let r = text(call(&mut s, "remember", json!({"kind": "memory", "key": "gadget-limit", "title": "Gadget limit", "summary": "gadgets cap at nine", "sub": "project", "body": "Gadgets are capped at nine per bay."})));
    assert!(r.starts_with("ok gadget-limit v1"), "{r}");
    assert!(r.contains("exported memory"), "{r}");
    let idx = fs::read_to_string(f.out.join("memory/INDEX.md")).unwrap();
    assert!(idx.ends_with("- [Gadget limit](gadget-limit.md) - gadgets cap at nine\n"), "{idx}");
    let file = fs::read_to_string(f.out.join("memory/gadget-limit.md")).unwrap();
    assert!(file.starts_with("---\nname: gadget-limit\ndescription: gadgets cap at nine\nmetadata:\n  type: project\n---\n\nGadgets"), "{file}");
    // A second remember on the key needs the head's sha.
    let e = call_err(&mut s, "remember", json!({"kind": "memory", "key": "gadget-limit", "body": "x"}));
    assert!(e.contains("expected_sha"), "{e}");
    let got = text(call(&mut s, "recall", json!({"key": "gadget-limit"})));
    assert!(got.contains("== gadget-limit memory/project active v1"), "{got}");
    let q = text(call(&mut s, "recall", json!({"query": "widget cache entries"})));
    assert!(q.contains("widget-cache") || q.contains("rules:r00001"), "{q}");
    // A query answers the leading records whole, the rest as pointers.
    assert!(q.starts_with("== "), "{q}");
    if let Some((_, pointers)) = q.split_once("more records, best first") {
        assert!(pointers.lines().skip(1).all(|l| l.starts_with("- ") || l.starts_with('+')), "{q}");
    }
    let sup = text(call(&mut s, "supersede", json!({"old": "rules:r00002", "body": "- 2026-02-02 beta2: exports are written atomically and synced."})));
    assert!(sup.starts_with("ok rules:r00004 v1"), "{sup}");
    let rules = fs::read_to_string(f.out.join("RULES.md")).unwrap();
    assert!(rules.contains("(superseded ") && rules.contains("beta2"), "{rules}");
    let log = text(call(&mut s, "memory_log", json!({"limit": 3})));
    assert!(log.starts_with("time\tkey\tkind\tv\tauthor\ttitle\treason\n"), "{log}");
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let rep = check(&store, &specs(), Some(&f.out), true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
}

/// `recall` answers are logged with a counterfactual by the scripted rule
/// (key: the record read where it is written; query: a grep of the
/// exports); `journal`, a write, logs none.
#[test]
fn recall_answers_carry_a_counterfactual_and_writes_do_not() {
    let f = fixture("mem-ledger");
    drop(import_all(&f));
    for spec in specs() {
        let store = MemoryStore::open(&f.settings.store).unwrap();
        export::write(&store, &spec, &f.out, false).unwrap();
    }
    let (mut s, fx) = server(&f);
    let ledger = atlas::tokens::Ledger::start(&fx.db, Vec::new()).unwrap();
    ledger.set_memory(atlas::tokens::MemoryPaths { store: f.settings.store.clone(), export_dir: Some(f.out.clone()) });
    s.set_ledger(ledger);
    s.answer("recall", &json!({"key": "widget-cache"})).unwrap();
    s.answer("recall", &json!({"query": "widget cache"})).unwrap();
    s.answer("recall", &json!({"query": "nothing matches this"})).unwrap();
    s.answer("journal", &json!({"lane": "L-1", "entry": "note", "text": "a note"})).unwrap();
    // Dropping the server joins the ledger thread, so every row is written.
    drop(s);
    let conn = rusqlite::Connection::open(&fx.db).unwrap();
    let rows: Vec<(String, Option<i64>, Option<i64>)> = conn
        .prepare("SELECT tool, grep, read FROM token_log ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 4, "{rows:?}");
    // The memory's whole file: meta and body.
    let mem = fs::read_to_string(f.out.join("memory/widget-cache.md")).unwrap();
    assert_eq!(rows[0], ("recall".to_string(), Some(0), Some(mem.len() as i64)));
    let want = atlas::tokens::measure_exports(&f.out, "widget cache");
    assert!(want.grep > 0, "{want:?}");
    assert_eq!(rows[1], ("recall".to_string(), Some(want.grep as i64), Some(want.read as i64)));
    assert_eq!(rows[2], ("recall".to_string(), Some(0), Some(0)));
    assert_eq!(rows[3], ("journal".to_string(), None, None));
}

#[test]
fn lane_journal_recall_order_and_budget() {
    let f = fixture("mem-lane");
    let (mut s, _fx) = server(&f);
    call(&mut s, "journal", json!({"lane": "build-7", "entry": "brief", "text": "Build the widget exporter.\nScope: exports only."}));
    call(&mut s, "journal", json!({"lane": "build-7", "entry": "decision", "text": "Use temp and rename for exports"}));
    let q = text(call(&mut s, "journal", json!({"lane": "build-7", "entry": "question", "text": "Should snapshots prune?"})));
    let qkey = q.split_whitespace().nth(1).unwrap().to_string();
    call(&mut s, "journal", json!({"lane": "build-7", "entry": "question", "text": "Which shape for handoffs?"}));
    call(&mut s, "journal", json!({"lane": "build-7", "entry": "answered", "text": "Prune past 1 GB", "refs": [format!("answers:{qkey}")]}));
    call(&mut s, "journal", json!({"lane": "build-7", "entry": "measurement", "text": "export 4.1 ms", "meta": "machine: test box\nruns: 20"}));
    call(&mut s, "journal", json!({"lane": "build-7", "entry": "commit", "text": "abc1234 exports"}));
    call(&mut s, "journal", json!({"lane": "build-7", "entry": "next", "text": "Wire the hooks"}));
    let h = text(call(&mut s, "handoff", json!({"lane": "build-7", "title": "stopping at the hooks", "text": "Exports done; hooks next."})));
    assert!(h.contains("exported restore"), "{h}");
    let lane = text(call(&mut s, "recall", json!({"lane": "build-7"})));
    let pos = |s: &str| lane.find(s).unwrap_or_else(|| panic!("{s} missing in {lane}"));
    assert!(pos("# handoff") < pos("# brief"));
    assert!(pos("# brief") < pos("# open questions"));
    assert!(pos("# open questions") < pos("# next"));
    assert!(pos("# next") < pos("# decisions"));
    assert!(pos("# decisions") < pos("# measurements"));
    assert!(pos("# measurements") < pos("# commits"));
    assert!(lane.contains("Which shape for handoffs?") && !lane.contains("Should snapshots prune?"), "{lane}");
    let small = text(call(&mut s, "recall", json!({"lane": "build-7", "budget_tokens": 60})));
    assert!(small.contains("more: memory_log"), "{small}");
    assert!(small.len() <= 60 * 4 + 64, "{} bytes", small.len());
    let st = text(call(&mut s, "journal_status", json!({})));
    assert!(st.contains("build-7\t9\t"), "{st}");
    assert!(st.contains("abc1234 exports\t1\t1"), "{st}");
    let resume = fs::read_to_string(f.out.join("RESUME.md")).unwrap();
    assert!(resume.contains("handoff build-7: stopping at the hooks"), "{resume}");
}

#[test]
fn index_cap_cuts_lines_only_when_set() {
    let f = fixture("mem-cap");
    let store = import_all(&f);
    let mut sp = specs()[0].clone();
    sp.index_cap = Some(20);
    let files = export::render(&store, &sp).unwrap();
    assert!(files[0].1.lines().all(|l| l.chars().count() <= 20), "{}", files[0].1);
    let _: &Path = f.src.as_path();
}

#[test]
fn the_clustered_index_names_every_memory_once() {
    let f = fixture("mem-clustered");
    let store = import_all(&f);
    let mut spec = specs()[0].clone();
    spec.index_topics = Some(vec![("Storage".into(), vec!["exports".into()]), ("Unused".into(), vec!["nothing".into()])]);
    let files = export::render(&store, &spec).unwrap();
    let index = &files[0].1;
    assert!(index.contains("\n## Standing rules\n\n- widget-cache: the widget cache bound\n"), "{index}");
    assert!(index.contains("\n## Project facts\n\n### Storage\n\nexport-rule\n"), "{index}");
    assert!(!index.contains("Unused"), "{index}");
    assert_eq!(export::slug_count(index, "widget-cache"), 1);
    assert!(export::round_trip(&store, &spec).unwrap().is_empty());
    // The memory files are the same as the line index's.
    assert_eq!(files[1..], export::render(&store, &specs()[0]).unwrap()[1..]);
    let rep = export::write(&store, &spec, &f.out, false).unwrap();
    assert!(rep.refused.is_empty());
    let c = check(&store, &[spec.clone()], Some(&f.out), true).unwrap();
    assert!(c.ok(), "{:?}", c.problems);
    // A word in the slug places it ahead of an earlier topic's word in the
    // description.
    spec.index_topics = Some(vec![("Atomics".into(), vec!["atomic".into()]), ("Exporting".into(), vec!["export".into()])]);
    let index = export::render(&store, &spec).unwrap()[0].1.clone();
    assert!(index.contains("\n### Exporting\n\nexport-rule\n") && !index.contains("Atomics"), "{index}");
}

#[test]
fn rule_statements_lose_only_their_attribution() {
    use export::{slug_count, trim_rule, unquote};
    assert_eq!(trim_rule("Dana 2026-09-30 - agents run one step per call"), "agents run one step per call");
    assert_eq!(trim_rule("Dana's rule from 2026-09-17 - no tabs"), "no tabs");
    assert_eq!(trim_rule("Standing rule, Dana 2026-09-24 - ray first"), "ray first");
    assert_eq!(trim_rule("2026-10-14: stop at the proof"), "stop at the proof");
    assert_eq!(trim_rule("Dana 2026-10-14 standing rule: two words stay"), "Dana 2026-10-14 standing rule: two words stay");
    assert_eq!(trim_rule("Dana 2026-10-01 calls the reviewer x-ray"), "Dana 2026-10-01 calls the reviewer x-ray");
    assert_eq!(trim_rule("REVERSED 2026-09-25 - the lock is scrapped"), "REVERSED 2026-09-25 - the lock is scrapped");
    assert_eq!(trim_rule("Keep the 2026-01-01 rule"), "Keep the 2026-01-01 rule");
    assert_eq!(trim_rule("no date here"), "no date here");
    assert_eq!(unquote(r#""a \"b\" c""#), r#"a "b" c"#);
    assert_eq!(unquote("'it''s'"), "it's");
    assert_eq!(unquote("plain"), "plain");
    assert_eq!(slug_count("a-b, a-b-c, x a-b.", "a-b"), 2);
    assert_eq!(slug_count("a-b-c", "a-b"), 0);
}

/// R9: a new handoff supersedes the lane's active one, and the lane view
/// resumes from the newest.
#[test]
fn a_new_handoff_supersedes_the_last_and_resume_shows_the_newest() {
    let f = fixture("mem-handoff2");
    let (mut s, _fx) = server(&f);
    call(&mut s, "journal", json!({"lane": "build-9", "entry": "brief", "text": "Build the gadget."}));
    call(&mut s, "handoff", json!({"lane": "build-9", "title": "first stop", "text": "Halfway."}));
    call(&mut s, "handoff", json!({"lane": "build-9", "title": "second stop", "text": "Nearly done."}));
    let lane = text(call(&mut s, "recall", json!({"lane": "build-9"})));
    assert!(lane.contains("second stop") && !lane.contains("first stop"), "{lane}");
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let hs = store.heads(Some("handoff"), Some("build-9")).unwrap();
    let st: Vec<(&str, &str)> = hs.iter().map(|h| (h.title.as_str(), h.status.as_str())).collect();
    assert!(st.contains(&("first stop", "superseded")) && st.contains(&("second stop", "active")), "{st:?}");
}

/// #80: a handoff with no title takes the text's first line as its title,
/// is filed as a handoff and shows in `journal_status` and the lane view;
/// one with no first line either is refused naming the field.
#[test]
fn a_handoff_without_a_title_takes_its_first_line() {
    let f = fixture("mem-handoff-notitle");
    let (mut s, _fx) = server(&f);
    call(&mut s, "journal", json!({"lane": "build-8", "entry": "brief", "text": "Build the gizmo."}));
    let h = text(call(&mut s, "handoff", json!({"lane": "build-8", "text": "Handoff: PR #7, head abc1234\nTests pass; the gate is off."})));
    assert!(h.contains("handoff:build-8:001"), "{h}");
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let hs = store.heads(Some("handoff"), Some("build-8")).unwrap();
    assert_eq!(hs.len(), 1);
    assert_eq!(hs[0].title, "Handoff: PR #7, head abc1234");
    assert!(hs[0].body.contains("Tests pass; the gate is off.") && !hs[0].body.contains("PR #7"), "{}", hs[0].body);
    let st = text(call(&mut s, "journal_status", json!({})));
    let row = st.lines().find(|l| l.starts_with("build-8\t")).unwrap_or_else(|| panic!("{st}"));
    assert!(row.ends_with("\t1"), "one handoff: {row}");
    let lane = text(call(&mut s, "recall", json!({"lane": "build-8"})));
    assert!(lane.contains("# handoff") && lane.contains("Handoff: PR #7, head abc1234"), "{lane}");
    // A blank text has no first line to take: the error names the field.
    let e = call_err(&mut s, "handoff", json!({"lane": "build-8", "text": "  \n"}));
    assert!(e.contains("title is required"), "{e}");
    assert_eq!(store.heads(Some("handoff"), Some("build-8")).unwrap().len(), 1);
}

#[test]
fn recall_of_a_key_at_hash_returns_that_version_verbatim() {
    let f = fixture("mem-refs");
    let mut store = import_all(&f);
    let old = store.head("widget-cache").unwrap().unwrap();
    revise(&mut store, "widget-cache", "\nThe widget cache now holds ninety widgets.\n");
    let new = store.head("widget-cache").unwrap().unwrap().sha;
    drop(store);
    let (mut s, _fx) = server(&f);
    // The old version, exact, with the current ref beside it.
    let r = s.answer("recall", &json!({"key": format!("widget-cache@{}", &old.sha[..12])})).unwrap().into_text();
    assert!(r.contains(old.body.trim()), "{r}");
    assert!(r.contains(&format!("changed: the current text is widget-cache@{}", &new[..12])), "{r}");
    // The head, by its own ref.
    let r = s.answer("recall", &json!({"key": format!("widget-cache@{}", &new[..12])})).unwrap().into_text();
    assert!(r.contains("ninety widgets") && r.contains("(current)"), "{r}");
    // A hash no version has.
    let e = s.answer("recall", &json!({"key": "widget-cache@0123456789ab"})).unwrap_err().to_string();
    assert!(e.contains("dangling"), "{e}");
}

#[test]
fn recall_of_a_lines_ref_returns_those_lines_or_says_they_changed() {
    let f = fixture("mem-lines-ref");
    let (mut s, fx) = server(&f);
    let name = fx.specs[0].0.clone();
    let root = fx.perf();
    // A ref path holds no spaces.
    let note = root.join("lines-ref.md");
    std::fs::write(&note, "# Lines

The frame budget is 4.2 ms.
Third line.
").unwrap();
    let rel = note.file_name().unwrap().to_string_lossy().to_string();
    let text = std::fs::read_to_string(&note).unwrap();
    let h = atlas::compact::lines_hash(&text, 1, 2).unwrap();
    let r = s.answer("recall", &json!({"key": format!("{name}:{rel}:1-2@{h}")})).unwrap().into_text();
    let want: Vec<&str> = text.lines().take(2).collect();
    assert!(r.contains("(current)") && r.contains(want.join("\n").as_str()), "{r}");
    let r = s.answer("recall", &json!({"key": format!("{name}:{rel}:1-2@0123456789ab")})).unwrap().into_text();
    assert!(r.contains(&format!("changed: these are the current lines, {name}:{rel}:1-2@{h}")), "{r}");
}

#[test]
fn the_transcript_source_is_off_by_default_and_serves_this_project_only() {
    let f = fixture("mem-turns");
    let tmp = Temp::new("mem-turns-t");
    // This process's project, as Claude Code names its folder, and another.
    let here = atlas::transcripts::project_of_cwd(&std::env::current_dir().unwrap());
    let mine = tmp.path().join(&here).join("abcdef12-0000.jsonl");
    let theirs = tmp.path().join("Z--other-project").join("abcdef12-0000.jsonl");
    // Synthesized records only.
    let rec = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"build failed: error E0599 in frame_pacer at step 7311"}]}}"#;
    for p in [&mine, &theirs] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{{\"type\":\"user\",\"message\":{{\"content\":\"go\"}}}}\n{rec}\n")).unwrap();
    }
    std::fs::create_dir_all(f.settings.index_db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&f.settings.index_db).unwrap();
    atlas::transcripts::index(&conn, &mine).unwrap();
    atlas::transcripts::index(&conn, &theirs).unwrap();
    let hash = atlas::transcripts::get(&conn, &here, "abcdef12", 2).unwrap().unwrap().hash;
    let r = format!("turn:abcdef12:2@{hash}");
    // Off by default: refused, nothing served.
    let (mut s, _fx) = server(&f);
    let e = s.answer("recall", &json!({"key": r})).unwrap_err().to_string();
    assert!(e.contains("transcript source is off"), "{e}");
    // On: the exact turn, and search over this project's turns only.
    let mut on = f.settings.clone();
    on.transcripts = true;
    s.set_memory(on);
    let t = s.answer("recall", &json!({"key": r})).unwrap().into_text();
    assert!(t.contains("(current)") && t.contains("error E0599 in frame_pacer at step 7311"), "{t}");
    let found = s.answer("recall", &json!({"query": "frame_pacer 7311", "kind": "turn"})).unwrap().into_text();
    assert_eq!(found.lines().count(), 1, "the other project's copy is never served: {found}");
    let e = s.answer("recall", &json!({"key": "turn:abcdef12:2@0123456789ab"})).unwrap_err().to_string();
    assert!(e.contains("dangling"), "{e}");
    // A checkpoint's byte ref names the same turn; an offset inside a record
    // names none.
    let off = "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n".len();
    let t = s.answer("recall", &json!({"key": format!("turn:abcdef12:b{off}@{hash}")})).unwrap().into_text();
    assert!(t.contains("(current)") && t.contains("error E0599 in frame_pacer at step 7311"), "{t}");
    let e = s.answer("recall", &json!({"key": format!("turn:abcdef12:b{}@{hash}", off + 1)})).unwrap_err().to_string();
    assert!(e.contains("dangling"), "{e}");
}

/// A fresh store whose exports were never written passes the check: a file
/// that would hold no records is not "missing" until atlas has written it.
/// Then rules written without a leading dash, in one line or several, keep
/// the items export splitting back into its records.
#[test]
fn plain_rule_bodies_keep_the_items_export_round_trip_and_a_fresh_store_checks_clean() {
    let f = fixture("mem-plain-rules");
    {
        let store = MemoryStore::open(&f.settings.store).unwrap();
        let rep = check(&store, &specs(), Some(&f.out), true).unwrap();
        assert!(rep.ok(), "fresh store: {:?}", rep.problems);
    }
    let (mut s, _fx) = server(&f);
    for body in ["Plain rule without a dash.", "Another plain rule.\nIt runs on to a second line.", "- 2026-03-04 gamma: a dashed rule stays as written."] {
        let r = text(call(&mut s, "remember", json!({"kind": "rule", "body": body, "author": "tester"})));
        assert!(r.starts_with("ok rules:r"), "{r}");
    }
    let rules = fs::read_to_string(f.out.join("RULES.md")).unwrap();
    let day: String = atlas::memory::now().chars().take(10).collect();
    assert!(rules.contains(&format!("- {day} tester: Plain rule without a dash.\n")), "{rules}");
    assert!(rules.contains(&format!("- {day} tester: Another plain rule.\n  It runs on to a second line.\n")), "{rules}");
    assert!(rules.contains("- 2026-03-04 gamma: a dashed rule stays as written.\n"), "{rules}");
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let rep = check(&store, &specs(), Some(&f.out), true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
    // What cannot be one item is refused, not stored.
    for (body, why) in [("First rule.\n\nSecond paragraph.", "no blank lines"), ("- One rule.\n- Two rules.", "each rule on its own")] {
        let e = call_err(&mut s, "remember", json!({"kind": "rule", "body": body}));
        assert!(e.contains(why), "{body:?}: {e}");
    }
}

/// The ok line prints 16 hex digits of the new head: that prefix (any of at
/// least 12, any case) revises the record, a shorter one is refused, and a
/// stale one gets memory wording (recall the record), not a note's.
#[test]
fn expected_sha_takes_the_printed_prefix() {
    let f = fixture("mem-sha-prefix");
    let (mut s, _fx) = server(&f);
    let r = text(call(&mut s, "remember", json!({"kind": "memory", "key": "prefix-test", "body": "first"})));
    let sha16 = r.split("sha=").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
    assert_eq!(sha16.len(), 16, "{r}");
    let r2 = text(call(&mut s, "remember", json!({"kind": "memory", "key": "prefix-test", "body": "second", "expected_sha": sha16})));
    assert!(r2.starts_with("ok prefix-test v2"), "{r2}");
    let sha12 = r2.split("sha=").nth(1).unwrap()[..12].to_ascii_uppercase();
    let r3 = text(call(&mut s, "remember", json!({"kind": "memory", "key": "prefix-test", "body": "third", "expected_sha": sha12})));
    assert!(r3.starts_with("ok prefix-test v3"), "{r3}");
    let e = call_err(&mut s, "remember", json!({"kind": "memory", "key": "prefix-test", "body": "fourth", "expected_sha": &r3.split("sha=").nth(1).unwrap()[..8]}));
    assert!(e.contains("at least its first 12 hex digits"), "{e}");
    let e = call_err(&mut s, "remember", json!({"kind": "memory", "key": "prefix-test", "body": "fourth", "expected_sha": sha16}));
    assert!(e.contains("stale revision") && e.contains("recall") && !e.contains("read the note"), "{e}");
}

/// Empty or blank lanes, texts and bodies are refused, not stored.
#[test]
fn empty_lanes_and_bodies_are_refused() {
    let f = fixture("mem-empty");
    let (mut s, _fx) = server(&f);
    for (tool, args, field) in [
        ("journal", json!({"lane": "", "entry": "note", "text": "x"}), "lane is empty"),
        ("journal", json!({"lane": "  ", "entry": "note", "text": "x"}), "lane is empty"),
        ("journal", json!({"lane": "some-lane", "entry": "note", "text": " \n"}), "text is empty"),
        ("handoff", json!({"lane": "", "title": "t", "text": "x"}), "lane is empty"),
        ("remember", json!({"kind": "rule", "body": ""}), "body is empty"),
        ("remember", json!({"kind": "decision", "body": "   "}), "body is empty"),
        ("remember", json!({"kind": "memory", "key": "empty-one", "body": ""}), "body is empty"),
    ] {
        let e = call_err(&mut s, tool, args.clone());
        assert!(e.contains(field), "{tool} {args}: {e}");
    }
    let log = text(call(&mut s, "memory_log", json!({"limit": 50})));
    assert!(!log.contains("empty-one"), "{log}");
}

/// `format: "json"` on the memory tools: a record by key and the lane status
/// as fields, every other answer as `{"text": ...}`.
#[test]
fn memory_tools_answer_json_when_asked() {
    let f = fixture("mem-json");
    let (mut s, _fx) = server(&f);
    let parse = |v: Value| -> Value {
        let t = v.as_str().map(str::to_string).or_else(|| v["text"].as_str().map(str::to_string)).unwrap_or_else(|| v.to_string());
        json::parse(&t).unwrap_or(v)
    };
    let w = call(&mut s, "remember", json!({"kind": "memory", "key": "json-rec", "title": "Json rec", "body": "a body", "format": "json"}));
    let w = parse(w);
    assert!(w["text"].as_str().is_some_and(|t| t.starts_with("ok json-rec v1")), "{w}");
    let r = parse(call(&mut s, "recall", json!({"key": "json-rec", "format": "json"})));
    assert_eq!((r["key"].as_str(), r["kind"].as_str(), r["version"].as_i64(), r["status"].as_str()), (Some("json-rec"), Some("memory"), Some(1), Some("active")), "{r}");
    assert!(r["body"].as_str().unwrap().contains("a body") && r["sha"].as_str().unwrap().len() == 64, "{r}");
    call(&mut s, "journal", json!({"lane": "json-lane", "entry": "note", "text": "a note"}));
    let st = parse(call(&mut s, "recall", json!({"op": "status", "format": "json"})));
    let lanes = st["lanes"].as_array().unwrap_or_else(|| panic!("{st}"));
    assert!(lanes.iter().any(|l| l["lane"] == json!("json-lane") && l["entries"] == json!(1)), "{st}");
    let e = call_err(&mut s, "recall", json!({"key": "json-rec", "format": "xml"}));
    assert!(e.contains("use text or json"), "{e}");
}

/// `recall {query}` lists no pointer that scores 0.0, and an empty store
/// says it holds no records instead of pointing at exports.
#[test]
fn recall_query_drops_zero_pointers_and_names_an_empty_store() {
    let f = fixture("mem-zero-pointers");
    let (mut s, _fx) = server(&f);
    let t = text(call(&mut s, "recall", json!({"query": "anything at all"})));
    assert_eq!(t, "no records match: the store holds none yet\n");
    drop(import_all(&f));
    let t = text(call(&mut s, "recall", json!({"query": "widget cache entries evict"})));
    assert!(!t.contains(" 0.0: "), "{t}");
}

/// Review finding 3: a lines ref was read by its given name alone, so it
/// reached dot folders, files the vault never serves and (through a link)
/// denied files, and a wrong hash still returned the current lines.
#[test]
fn a_lines_ref_reaches_only_what_read_note_serves() {
    let f = fixture("mem-lines-ref-confined");
    let (mut s, fx) = server(&f);
    let name = fx.specs[0].0.clone();
    let root = fx.perf();
    let secret = "orchidlantern5523";
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git").join("config"), format!("[remote]\n  url = {secret}\n")).unwrap();
    std::fs::write(root.join("notes.txt"), format!("plain {secret}\n")).unwrap();
    std::fs::write(root.join("Ops.env"), format!("TOKEN={secret}\n")).unwrap();
    let mut asks = vec![format!("{name}:.git/config:1-2@000000000000"), format!("{name}:notes.txt:1-1@000000000000"), format!("{name}:Ops.env:1-1@000000000000")];
    // A note that is a link to a denied file, where links can be made.
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(root.join("Ops.env"), root.join("linked.md")).is_ok();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(root.join("Ops.env"), root.join("linked.md")).is_ok();
    if linked {
        asks.push(format!("{name}:linked.md:1-1@000000000000"));
    } else {
        eprintln!("no symlink privilege here: the link case is checked by read_below's unit test only");
    }
    for key in asks {
        let (refused, text) = match s.answer("recall", &json!({"key": key.clone()})) {
            Ok(a) => (false, a.into_text()),
            Err(e) => (true, e.to_string()),
        };
        assert!(!text.contains(secret), "{key}: {text}");
        assert!(refused, "{key} must be refused: {text}");
    }
    // The hook's ref check reads through the same confinement.
    for rel in [".git/config", "notes.txt", "Ops.env"] {
        assert_eq!(atlas::compact::resolve_lines(&[atlas::compact::RefRoot::Vault(name.clone(), root.clone())], &name, rel, 1, 1, "000000000000"), atlas::compact::Status::Dangling, "{rel}");
    }
}

/// Review finding 4: an atlas older than 0.2 on the same store commits and
/// appends without moving `jsonl_end`, and the next 0.2 write cut that
/// committed line off, so `--memory-check` reported it missing. A crash
/// leftover (lines the store never committed) is still cut.
#[test]
fn a_line_an_older_writer_committed_past_the_recorded_end_is_kept() {
    let f = fixture("mem-mixed-writers");
    let mut store = import_all(&f);
    let end0 = store.jsonl_end().unwrap().unwrap();
    // The older writer: a committed record, and the recorded end left where it was.
    let mut w = store.writer().unwrap();
    w.create("older-writer-note", "memory", "", "", &draft("written by an older atlas\n"), None).unwrap();
    w.commit().unwrap();
    store.conn().execute("UPDATE mem_meta SET value = ?1 WHERE key = 'jsonl_end'", [end0.to_string()]).unwrap();
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok(), "the older writer's line is the store's: {:?}", rep.problems);
    // The next write keeps it.
    let mut w = store.writer().unwrap();
    w.create("newer-writer-note", "memory", "", "", &draft("written by this atlas\n"), None).unwrap();
    w.commit().unwrap();
    let copy = fs::read_to_string(store.jsonl()).unwrap();
    assert!(copy.contains("older-writer-note") && copy.contains("newer-writer-note"), "{copy}");
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
    // A batch that appended and never committed, plus a torn line, are cut.
    let last = copy.lines().last().unwrap().replace("newer-writer-note", "never-committed-note");
    fs::write(store.jsonl(), format!("{copy}{last}\n{{\"op\":\"crea")).unwrap();
    let mut w = store.writer().unwrap();
    w.create("after-the-crash", "memory", "", "", &draft("written after a crash\n"), None).unwrap();
    w.commit().unwrap();
    let copy = fs::read_to_string(store.jsonl()).unwrap();
    assert!(!copy.contains("never-committed-note") && !copy.contains("{\"op\":\"crea\n") && copy.contains("after-the-crash"), "{copy}");
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
}

/// A fresh store's first batch killed between its append and its commit
/// left its line in the copy with no recorded end at all, which read as "a
/// store written before the end was recorded": the line was never cut,
/// the next batch recorded its end past it, and `--memory-check` counted a
/// version the store never held (Windows CI, where the first write is the
/// one a 150 ms kill lands on).
#[test]
fn a_fresh_stores_first_batch_killed_before_its_commit_is_cut() {
    let f = fixture("mem-fresh-crash");
    // The crashed batch's line, as it would have been appended.
    let other = MemoryStore::open(&f.settings.store.with_extension("other")).unwrap();
    let line = {
        let mut o = other;
        let mut w = o.writer().unwrap();
        w.create("never-committed-note", "memory", "", "", &draft("appended, never committed\n"), None).unwrap();
        w.commit().unwrap();
        fs::read_to_string(o.jsonl()).unwrap()
    };
    let mut store = MemoryStore::open(&f.settings.store).unwrap();
    assert_eq!(store.jsonl_end().unwrap(), Some(0), "a fresh store records its copy's end when opened");
    fs::write(store.jsonl(), &line).unwrap();
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok(), "an uncommitted line is not the store's: {:?}", rep.problems);
    let mut w = store.writer().unwrap();
    w.create("after-the-crash", "memory", "", "", &draft("written after a crash\n"), None).unwrap();
    w.commit().unwrap();
    let copy = fs::read_to_string(store.jsonl()).unwrap();
    assert!(!copy.contains("never-committed-note") && copy.contains("after-the-crash"), "{copy}");
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
}

/// A line of `store`'s copy as another store would have appended it for
/// `key`: a batch that appended and never committed here.
fn foreign_line(f: &Fix, key: &str, refs: &[(String, String)]) -> String {
    let mut o = MemoryStore::open(&f.settings.store.with_extension(format!("{key}.other"))).unwrap();
    let mut w = o.writer().unwrap();
    w.create(key, "memory", "", "", &draft("appended, never committed\n"), None).unwrap();
    w.add_refs(key, refs).unwrap();
    w.commit().unwrap();
    fs::read_to_string(o.jsonl()).unwrap()
}

/// Commits `key` as an atlas older than 0.2 would: its line appended past
/// whatever the copy holds, and the recorded end left at `end`.
fn older_writer_commit(store: &mut MemoryStore, key: &str, end: u64) {
    let len = fs::metadata(store.jsonl()).unwrap().len();
    store.conn().execute("UPDATE mem_meta SET value = ?1 WHERE key = 'jsonl_end'", [len.to_string()]).unwrap();
    let mut w = store.writer().unwrap();
    w.create(key, "memory", "", "", &draft("written by an older atlas\n"), None).unwrap();
    w.commit().unwrap();
    store.conn().execute("UPDATE mem_meta SET value = ?1 WHERE key = 'jsonl_end'", [end.to_string()]).unwrap();
}

/// An older writer's committed line, then a batch killed before its commit:
/// the uncommitted lines are cut and the committed one kept. (Before, any
/// line the store held past the end kept everything past it, so the
/// uncommitted version was counted from then on.)
#[test]
fn uncommitted_lines_after_an_older_writers_line_are_cut_and_it_kept() {
    let f = fixture("mem-older-then-crash");
    let mut store = import_all(&f);
    let end0 = store.jsonl_end().unwrap().unwrap();
    older_writer_commit(&mut store, "older-writer-note", end0);
    let mut copy = fs::read_to_string(store.jsonl()).unwrap();
    copy.push_str(&foreign_line(&f, "never-committed-note", &[("lane".into(), "x".into())]));
    fs::write(store.jsonl(), &copy).unwrap();
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok() && rep.warnings.is_empty(), "{:?} {:?}", rep.problems, rep.warnings);
    let mut w = store.writer().unwrap();
    w.create("after-the-crash", "memory", "", "", &draft("written after a crash\n"), None).unwrap();
    w.commit().unwrap();
    let copy = fs::read_to_string(store.jsonl()).unwrap();
    assert!(copy.contains("older-writer-note") && !copy.contains("never-committed-note") && copy.contains("after-the-crash"), "{copy}");
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok() && rep.warnings.is_empty(), "{:?} {:?}", rep.problems, rep.warnings);
}

/// A batch killed before its commit, then an older writer's committed line
/// after it: the committed line is never cut, so the uncommitted ones
/// cannot be cut off the end either. `--memory-check` leaves them out and
/// warns (repairable), and the next write overwrites them in place with
/// `void` lines of the same length, so nothing committed moves.
#[test]
fn uncommitted_lines_before_an_older_writers_line_are_voided_in_place() {
    let f = fixture("mem-crash-then-older");
    let mut store = import_all(&f);
    let end0 = store.jsonl_end().unwrap().unwrap();
    let mut copy = fs::read_to_string(store.jsonl()).unwrap();
    copy.push_str(&foreign_line(&f, "never-committed-note", &[("lane".into(), "x".into())]));
    fs::write(store.jsonl(), &copy).unwrap();
    older_writer_commit(&mut store, "older-writer-note", end0);
    let before = fs::read(store.jsonl()).unwrap();
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok(), "{:?}", rep.problems);
    assert_eq!(rep.warnings.len(), 2, "the uncommitted create and its refs: {:?}", rep.warnings);
    assert!(rep.warnings[0].contains("never committed") && rep.warnings[0].contains("next write voids it"), "{:?}", rep.warnings);
    let mut w = store.writer().unwrap();
    w.create("after-the-crash", "memory", "", "", &draft("written after a crash\n"), None).unwrap();
    w.commit().unwrap();
    let after = fs::read(store.jsonl()).unwrap();
    let copy = String::from_utf8(after.clone()).unwrap();
    assert!(copy.contains("older-writer-note") && !copy.contains("never-committed-note") && copy.contains("after-the-crash"), "{copy}");
    assert!(after.len() > before.len(), "nothing was cut");
    let older = |b: &[u8]| String::from_utf8_lossy(b).find("older-writer-note").unwrap();
    assert_eq!(older(&before), older(&after), "the committed line did not move");
    assert_eq!(copy.lines().filter(|l| l.starts_with("{\"op\":\"void\"}")).count(), 2, "{copy}");
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok() && rep.warnings.is_empty(), "{:?} {:?}", rep.problems, rep.warnings);
}

/// An uncommitted revise of a record the store holds, with a refs line the
/// store holds a row like (from the record's earlier batch): the refs line
/// came in the same batch, so it is cut with the version.
#[test]
fn an_uncommitted_versions_refs_go_with_it_though_the_store_has_a_like_row() {
    let f = fixture("mem-crash-refs");
    let mut store = import_all(&f);
    let refs = [("lane".to_string(), "x".to_string())];
    let mut w = store.writer().unwrap();
    let a = w.create("refs-note", "memory", "", "", &draft("first\n"), None).unwrap();
    w.add_refs("refs-note", &refs).unwrap();
    w.commit().unwrap();
    let committed = fs::read_to_string(store.jsonl()).unwrap();
    // The same revise committed, its lines taken, and the store put back.
    let snap = f.settings.store.with_extension("snap");
    store.conn().execute("VACUUM INTO ?1", [snap.to_string_lossy()]).unwrap();
    let mut w = store.writer().unwrap();
    w.revise("refs-note", Some(&a.sha), &draft("second\n")).unwrap();
    w.add_refs("refs-note", &refs).unwrap();
    w.commit().unwrap();
    let lines = fs::read_to_string(store.jsonl()).unwrap()[committed.len()..].to_string();
    assert_eq!(lines.lines().count(), 2, "{lines}");
    drop(store);
    let jsonl = atlas::memory::jsonl_path(&f.settings.store);
    for ext in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{ext}", f.settings.store.display()));
    }
    fs::copy(&snap, &f.settings.store).unwrap();
    fs::write(&jsonl, format!("{committed}{lines}")).unwrap();
    let mut store = MemoryStore::open(&f.settings.store).unwrap();
    let rep = check(&store, &specs(), None, true).unwrap();
    assert!(rep.ok() && rep.warnings.is_empty(), "{:?} {:?}", rep.problems, rep.warnings);
    let w = store.writer().unwrap();
    w.commit().unwrap();
    assert_eq!(fs::read_to_string(&jsonl).unwrap(), committed);
}

/// A journal or handoff text whose first line is too long for a title
/// keeps that line whole in its body: the title clip loses nothing.
#[test]
fn a_first_line_longer_than_a_title_is_kept_in_the_body() {
    let f = fixture("mem-long-title");
    drop(import_all(&f));
    let (mut s, _fx) = server(&f);
    let long = format!("{} the tail that must survive", "word ".repeat(50));
    s.answer("journal", &json!({"lane": "L", "entry": "note", "text": long})).unwrap();
    s.answer("journal", &json!({"lane": "L", "entry": "note", "text": format!("{long}\nsecond line")})).unwrap();
    s.answer("handoff", &json!({"lane": "L", "text": long})).unwrap();
    s.answer("journal", &json!({"lane": "L", "entry": "note", "text": "short title\nthe body"})).unwrap();
    drop(s);
    let store = MemoryStore::open(&f.settings.store).unwrap();
    for key in ["journal:L:0001", "journal:L:0002", "handoff:L:001"] {
        let h = store.head(key).unwrap().unwrap();
        assert!(h.title.chars().count() <= atlas::memory::TITLE_CHARS && h.body.contains("the tail that must survive"), "{key}: {h:?}");
        // The title is cut between words, never inside one.
        assert!(h.title.ends_with("word"), "{key}: {}", h.title);
    }
    assert!(store.head("journal:L:0002").unwrap().unwrap().body.contains("second line"));
    let short = store.head("journal:L:0003").unwrap().unwrap();
    assert_eq!((short.title.as_str(), short.body.trim()), ("short title", "the body"));
    drop(store);
    // A long title given as its own argument: a handoff's and a decision's
    // stay whole in the body; a memory's or a rule's is refused, not cut.
    let (mut s, _fx) = server(&f);
    s.answer("handoff", &json!({"lane": "M", "title": long, "text": "the handoff"})).unwrap();
    s.answer("remember", &json!({"kind": "decision", "title": long, "reason": "the title clip test", "body": "the decision"})).unwrap();
    let memory = json!({"kind": "memory", "key": "a-memory", "sub": "project", "title": long, "reason": "the title clip test", "body": "- the text"});
    let rule = json!({"kind": "rule", "title": long, "reason": "the title clip test", "body": "- the text"});
    for (kind, a) in [("memory", memory), ("rule", rule)] {
        let e = s.answer("remember", &a).unwrap_err().to_string();
        assert!(e.contains("at most 120 characters"), "{kind}: {e}");
    }
    drop(s);
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let h = store.head("handoff:M:001").unwrap().unwrap();
    assert!(h.body.contains("the tail that must survive") && h.body.contains("the handoff"), "{h:?}");
    let decided = store.heads(Some("decision"), None).unwrap();
    assert!(decided.iter().any(|d| d.body.contains("the tail that must survive") && d.title.chars().count() <= atlas::memory::TITLE_CHARS), "{decided:?}");
}

/// `journal` with `amend: KEY` rewrites an entry's title and body as a new
/// version under the same key, at the head the caller read; kind, entry,
/// lane, meta and refs stay, so the queue still sees an amended item.
#[test]
fn a_journal_entry_is_amended_in_place_at_its_head() {
    let f = fixture("mem-amend");
    drop(import_all(&f));
    let (mut s, _fx) = server(&f);
    s.answer("journal", &json!({"lane": "PARSER", "entry": "queued", "text": "Fix the rea", "meta": "kind: builder\nuses: compile"})).unwrap();
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let v1 = store.head("journal:PARSER:0001").unwrap().unwrap();
    drop(store);
    let amend = |text: &str, sha: &str| json!({"lane": "PARSER", "entry": "queued", "amend": "journal:PARSER:0001", "expected_sha": sha, "text": text});
    // A stale sha, a wrong lane, a meta line or no sha is refused.
    assert!(s.answer("journal", &amend("Fix the reader\nThe scope.", "000000000000")).is_err());
    let wrong = json!({"lane": "DOCS", "entry": "queued", "amend": "journal:PARSER:0001", "expected_sha": v1.sha, "text": "x"});
    let e = s.answer("journal", &wrong).unwrap_err().to_string();
    assert!(e.contains("is lane PARSER entry queued"), "{e}");
    let meta = json!({"lane": "PARSER", "entry": "queued", "amend": "journal:PARSER:0001", "expected_sha": v1.sha, "text": "x", "meta": "kind: reviewer"});
    assert!(s.answer("journal", &meta).unwrap_err().to_string().contains("meta lines and refs are kept"));
    let nosha = json!({"lane": "PARSER", "entry": "queued", "amend": "journal:PARSER:0001", "text": "x"});
    assert!(s.answer("journal", &nosha).is_err());
    s.answer("journal", &amend("Fix the reader\nThe scope.", &v1.sha)).unwrap();
    drop(s);
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let v2 = store.head("journal:PARSER:0001").unwrap().unwrap();
    assert_eq!((v2.title.as_str(), v2.body.trim(), v2.seq), ("Fix the reader", "The scope.", v1.seq + 1));
    assert_eq!((v2.kind.as_str(), v2.sub.as_str(), v2.scope.as_str(), v2.status.as_str(), v2.meta.as_str()), (v1.kind.as_str(), v1.sub.as_str(), v1.scope.as_str(), v1.status.as_str(), v1.meta.as_str()));
    assert_eq!(store.history("journal:PARSER:0001").unwrap().len(), 2);
    let (_, items) = atlas::queue::items(&store, &atlas::memory::now()).unwrap();
    assert!(items.iter().any(|i| i.key == "journal:PARSER:0001"), "the amended item left the queue");
    drop(store);
    // A review entry is amended like any other: its meta is kept, not asked
    // again.
    let (mut s, _fx) = server(&f);
    s.answer("journal", &json!({"lane": "PARSER", "entry": "review", "text": "Reviewd", "meta": "role: cost\npr: 9\nhead: abcdef0\nverdict: pass"})).unwrap();
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let r = store.head("journal:PARSER:0002").unwrap().unwrap();
    drop(store);
    s.answer("journal", &json!({"lane": "PARSER", "entry": "review", "amend": "journal:PARSER:0002", "expected_sha": r.sha, "text": "Reviewed"})).unwrap();
    drop(s);
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let r2 = store.head("journal:PARSER:0002").unwrap().unwrap();
    assert_eq!((r2.title.as_str(), r2.meta.as_str()), ("Reviewed", r.meta.as_str()));
}

/// Amending a launch the hook queued moves its `brief_sha` to the new
/// brief, so the same launch made again is known as the amended one.
#[test]
fn an_amended_hook_launch_keeps_its_queue_identity() {
    let f = fixture("mem-amend-launch");
    drop(import_all(&f));
    let mut store = MemoryStore::open(&f.settings.store).unwrap();
    let launch = |prompt: &'static str| atlas::queue::Launch { kind: atlas::orchestrate::Kind::Builder, lane: Some("P"), description: "fix the reader", subagent_type: "", prompt, session: "s0", class_meta: "" };
    let (key, new) = atlas::queue::add(&mut store, &launch("Lane: P\nOld brief.")).unwrap();
    assert!(new);
    let sha = store.head(&key).unwrap().unwrap().sha;
    drop(store);
    let (mut s, _fx) = server(&f);
    s.answer("journal", &json!({"lane": "P", "entry": "queued", "amend": key, "expected_sha": sha, "text": "fix the reader\nLane: P\nNew brief."})).unwrap();
    drop(s);
    let mut store = MemoryStore::open(&f.settings.store).unwrap();
    let (again, new) = atlas::queue::add(&mut store, &launch("Lane: P\nNew brief.")).unwrap();
    assert_eq!((again.as_str(), new), (key.as_str(), false));

    // A description past a title: the title is cut by the library rule and
    // the whole line kept, so only the same whole description matches.
    let long = format!("{}the end", "fix the reader again, ".repeat(8));
    let long_launch = |description: &'static str, prompt: &'static str| atlas::queue::Launch { description, ..launch(prompt) };
    let long: &'static str = Box::leak(long.into_boxed_str());
    let (lkey, new) = atlas::queue::add(&mut store, &long_launch(long, "Lane: P\nLong brief.")).unwrap();
    assert!(new);
    let h = store.head(&lkey).unwrap().unwrap();
    assert!(h.title.chars().count() <= atlas::memory::TITLE_CHARS && h.meta.contains(&format!("description: {long}")), "{h:?}");
    assert_eq!(atlas::queue::add(&mut store, &long_launch(long, "Lane: P\nLong brief.")).unwrap(), (lkey.clone(), false));
    let other: &'static str = Box::leak(format!("{long} too").into_boxed_str());
    assert!(atlas::queue::add(&mut store, &long_launch(other, "Lane: P\nLong brief.")).unwrap().1, "a different long description is another launch");
    // Amended with a first line past a title: the brief alone is hashed,
    // so the amended launch made again is still known.
    let sha = store.head(&lkey).unwrap().unwrap().sha;
    drop(store);
    let (mut s, _fx) = server(&f);
    s.answer("journal", &json!({"lane": "P", "entry": "queued", "amend": lkey, "expected_sha": sha, "text": format!("{long}\nLane: P\nAmended brief.")})).unwrap();
    drop(s);
    let mut store = MemoryStore::open(&f.settings.store).unwrap();
    assert_eq!(atlas::queue::add(&mut store, &long_launch(long, "Lane: P\nAmended brief.")).unwrap(), (lkey, false));
}

/// The `journal` tool's `queued` entry is the MCP path for planned work: it
/// is checked and filed with the meta `atlas queue plan` writes.
#[test]
fn a_queued_journal_entry_is_filed_as_planned_work() {
    let f = fixture("mem-planned");
    drop(import_all(&f));
    let (mut s, _fx) = server(&f);
    let out = s.answer("journal", &json!({"lane": "PARSER", "entry": "queued", "text": "Fix the reader\nThe scope.", "meta": "kind: builder\nuses: compile, gpu\norder: 2\nreview: simplicity, cost"})).unwrap().into_text();
    assert!(out.contains("journal:PARSER:0001"), "{out}");
    let bad = [
        (json!({"lane": "P", "entry": "queued", "text": "t", "meta": "kind: blender"}), "unknown kind"),
        (json!({"lane": "P", "entry": "queued", "text": "t", "meta": "priority: soon"}), "priority"),
        (json!({"lane": "P", "entry": "queued", "text": "t", "meta": "order: first"}), "order needs a number"),
        (json!({"lane": "P", "entry": "queued", "text": "t", "meta": "after: journal:NOPE:0001"}), "no such queued item"),
        // A review entry's verdict is one of the four words, never a guess.
        (json!({"lane": "P", "entry": "review", "text": "t", "meta": "role: cost\npr: 9\nhead: abcdef0\nverdict: reject"}), "merge with follow-ups or request changes"),
        (json!({"lane": "P", "entry": "review", "text": "t", "meta": "role: cost\npr: 9\nhead: abcdef0\nverdict: do not merge"}), "merge with follow-ups or request changes"),
    ];
    for (args, want) in bad {
        let e = s.answer("journal", &args).unwrap_err().to_string();
        assert!(e.contains(want), "{e}");
    }
    s.answer("journal", &json!({"lane": "DOCS", "entry": "queued", "text": "Write the guide", "meta": "after: journal:PARSER:0001, the release"})).unwrap();
    s.answer("journal", &json!({"lane": "FRAC", "entry": "queued", "text": "A fractional place", "meta": "order: 9.5"})).unwrap();
    s.answer("journal", &json!({"lane": "P", "entry": "review", "text": "t", "meta": "role: cost\npr: 9\nhead: abcdef0\nverdict: Merge-With-Follow-Ups"})).unwrap();
    drop(s);
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let h = store.head("journal:PARSER:0001").unwrap().unwrap();
    assert_eq!((h.title.as_str(), h.body.trim()), ("Fix the reader", "The scope."));
    assert!(h.meta.contains("priority: normal (default)") && h.meta.contains("uses: compile, gpu") && h.meta.contains("review: simplicity, cost") && h.meta.contains("planned: true"), "{}", h.meta);
    let (_, items) = atlas::queue::items(&store, &atlas::memory::now()).unwrap();
    assert!(store.head("journal:FRAC:0001").unwrap().unwrap().meta.lines().any(|l| l == "order: 9.5"));
    assert_eq!(items.iter().find(|i| i.key == "journal:FRAC:0001").unwrap().order.as_ref().map(ToString::to_string).as_deref(), Some("9.5"));
    let docs = items.iter().find(|i| i.key == "journal:DOCS:0001").unwrap();
    assert_eq!(docs.unmet, ["journal:PARSER:0001", "the release"]);
}

/// Records another scope or kind holds must not use up the window the filter is
/// applied in: the list finds what matches, and says when it cut.
#[test]
fn a_filter_does_not_cut_a_list_silently() {
    let f = fixture("recall-filter-cut");
    let (mut s, _fx) = server(&f);
    // Short bodies all about the word rank ahead of the long ones about much else.
    for i in 0..40 {
        call(&mut s, "remember", json!({"key": format!("crowd-{i}"), "kind": "memory", "sub": "project", "scope": "CROWD", "title": format!("widget {i}"), "summary": "s", "body": "widget widget widget"}));
    }
    let long = "other words ".repeat(60);
    for i in 0..5 {
        call(&mut s, "remember", json!({"key": format!("rare-{i}"), "kind": "memory", "sub": "project", "scope": "RARE", "title": format!("item {i}"), "summary": "s", "body": format!("widget {long}")}));
    }
    // In a small store a common word scores near zero: such a hit is counted, not shown.
    let weak = |a: &str| a.lines().find(|l| l.contains("scoring near zero")).and_then(|l| l.strip_prefix('+')).and_then(|l| l.split(' ').next()).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
    let hits = |a: &str| (0..5).filter(|i| a.contains(&format!("rare-{i}"))).count() + weak(a);
    let a = text(call(&mut s, "recall", json!({"query": "widget", "scope": "RARE", "limit": 3, "budget_tokens": 5000})));
    assert_eq!(hits(&a), 3, "{a}");
    assert!(a.contains("+more (limit=3)"), "five match, three shown: {a}");
    let a = text(call(&mut s, "recall", json!({"query": "widget", "scope": "RARE", "limit": 5, "budget_tokens": 5000})));
    assert_eq!(hits(&a), 5, "{a}");
    assert!(!a.contains("+more ("), "all there: {a}");
    // Limit 0 keeps the search's own window of 8, and says when it cut.
    let a = text(call(&mut s, "recall", json!({"query": "widget", "scope": "CROWD", "limit": 0, "budget_tokens": 5000})));
    assert!(a.contains("+more (limit=0"), "{a}");
    // The kind filter shares the window.
    call(&mut s, "journal", json!({"lane": "RARE", "entry": "note", "text": format!("widget {long}")}));
    let a = text(call(&mut s, "recall", json!({"query": "widget", "kind": "journal", "limit": 3, "budget_tokens": 5000})));
    assert!(a.contains("journal:RARE:"), "{a}");
}

/// Words are picked by how rare they are in the whole store. A filter's only record
/// can hold just the commoner ones: over 12 known words must not lose it, and the
/// answer must not say nothing matches.
#[test]
fn a_filter_keeps_the_words_only_its_records_hold() {
    let f = fixture("recall-filter-terms");
    let (mut s, _fx) = server(&f);
    // Thirteen words known to the store, the first twelve each in one other scope's
    // record, and "gadget" commoner than any of them, in the one record of scope ONE.
    let rare: Vec<String> = (0..12).map(|i| format!("rareword{}", char::from(b'a' + i))).collect();
    for w in &rare {
        call(&mut s, "remember", json!({"key": format!("other-{w}"), "kind": "memory", "sub": "project", "scope": "OTHER", "title": w, "summary": "s", "body": "gadget"}));
    }
    call(&mut s, "remember", json!({"key": "only-one", "kind": "memory", "sub": "reference", "scope": "ONE", "title": "the gadget", "summary": "s", "body": "gadget"}));
    let long = format!("{} gadget", rare.join(" "));
    let a = text(call(&mut s, "recall", json!({"query": long.clone(), "scope": "ONE"})));
    assert!(a.contains("only-one") && !a.contains("no records match") && !a.contains("not searched"), "{a}");
    // The hook's case stays as it was: no filter, the 12 rarest, so "gadget" goes.
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let terms: Vec<String> = long.split(' ').map(str::to_string).collect();
    assert!(!store.rarest(&terms, atlas::memory::SEARCH_TERMS).unwrap().contains(&"gadget".to_string()));
    assert!(store.search(&terms, None, None, 50).unwrap().iter().all(|(h, _, _)| h.key != "only-one"));
    // Each filter alone keeps every word, at the store too.
    assert!(store.search(&terms, None, Some("ONE"), 50).unwrap().iter().any(|(h, _, _)| h.key == "only-one"));
    let kinds = store.search(&terms, Some("memory"), None, 50).unwrap();
    assert!(kinds.iter().any(|(h, _, _)| h.key == "only-one"), "{}", kinds.len());
}

/// The tokenizer folds diacritics, so the vocabulary does not hold the word as typed.
#[test]
fn a_word_with_diacritics_is_found() {
    let f = fixture("recall-diacritics");
    let (mut s, _fx) = server(&f);
    call(&mut s, "remember", json!({"key": "the-name", "kind": "memory", "sub": "project", "title": "a note on Müller", "summary": "s", "body": "the café"}));
    for q in ["Müller", "müller", "muller", "café"] {
        let a = text(call(&mut s, "recall", json!({"query": q})));
        assert!(a.contains("the-name") && !a.contains("no records match"), "{q}: {a}");
    }
    // The hook's search goes the same way.
    let store = MemoryStore::open(&f.settings.store).unwrap();
    assert_eq!(store.rarest(&["müller".to_string()], 12).unwrap(), ["müller"]);
    // A word nothing holds is still none, and takes no place from one that is held:
    // a long prompt in another script keeps its known words.
    let a = text(call(&mut s, "recall", json!({"query": "Größenwahn"})));
    assert!(a.contains("no records match"), "{a}");
    let unknown: Vec<String> = (0..14).map(|i| format!("слово{}", char::from(b'a' + i))).collect();
    let terms: Vec<String> = unknown.iter().cloned().chain(["müller".to_string(), "note".to_string()]).collect();
    assert_eq!(store.rarest(&terms, 2).unwrap().len(), 2);
    let a = text(call(&mut s, "recall", json!({"query": format!("{} Müller", unknown.join(" "))})));
    assert!(a.contains("the-name"), "{a}");
}

/// Past the terms' cap, words go unsearched by position: the answer says so, whether
/// it found something or not, so a "none" there is not taken for an answer.
#[test]
fn a_filtered_answer_says_when_words_went_unsearched() {
    let f = fixture("recall-filter-cap");
    let (mut s, _fx) = server(&f);
    call(&mut s, "remember", json!({"key": "held", "kind": "memory", "sub": "project", "scope": "ONE", "title": "firstword", "summary": "s", "body": "tailword"}));
    let filler: Vec<String> = (0..47).map(|i| format!("fill{}{}", char::from(b'a' + i / 26), char::from(b'a' + i % 26))).collect();
    let q = |tail: &str| format!("{} {tail}", filler.join(" "));
    // The 48th word is searched and the two after it are not.
    let a = text(call(&mut s, "recall", json!({"query": q("firstword extraone extratwo"), "scope": "ONE"})));
    assert!(a.contains("held") && a.contains("+2 more words were not searched"), "{a}");
    let a = text(call(&mut s, "recall", json!({"query": q("nosuchword tailword"), "scope": "ONE"})));
    assert!(a.contains("no records match") && a.contains("+1 more word was not searched"), "{a}");
    // Exactly at the cap, and with no filter, nothing is said.
    let a = text(call(&mut s, "recall", json!({"query": q("firstword"), "scope": "ONE"})));
    assert!(a.contains("held") && !a.contains("not searched"), "{a}");
    let a = text(call(&mut s, "recall", json!({"query": q("firstword extraone extratwo")})));
    assert!(!a.contains("not searched"), "{a}");
}

/// A filter's matches can rank past the first window of matches: they come back
/// by name, in rank order, not merely counted. Filler keeps the word's score real
/// (when most of the store holds a word, every score sits under the pointer floor
/// and the row is hidden, so a count would prove nothing).
#[test]
fn a_filter_finds_its_records_past_the_first_window() {
    let f = fixture("recall-filter-past");
    let (mut s, _fx) = server(&f);
    for i in 0..30 {
        call(&mut s, "remember", json!({"key": format!("crowd-{i}"), "kind": "memory", "sub": "project", "scope": "CROWD", "title": format!("widget {i}"), "summary": "s", "body": "widget widget widget"}));
    }
    for i in 0..80 {
        call(&mut s, "remember", json!({"key": format!("filler-{i}"), "kind": "memory", "sub": "project", "scope": "FILL", "title": format!("other {i}"), "summary": "s", "body": "nothing about it"}));
    }
    for i in 0..3 {
        // A little longer each time, so the ranking among them is known.
        let long = "other words ".repeat(20 + i * 5);
        call(&mut s, "remember", json!({"key": format!("rare-{i}"), "kind": "memory", "sub": "project", "scope": "RARE", "title": format!("item {i}"), "summary": "s", "body": format!("widget {long}")}));
    }
    // The tool asks for limit + 1, so its window at limit 3 is (3 + 1) * 2 + 8 rows.
    let window = (3 + 1) * 2 + 8;
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let terms = vec!["widget".to_string()];
    let ranked: Vec<String> = store.search(&terms, None, None, 500).unwrap().into_iter().map(|(h, _, _)| h.key).collect();
    let rank = |k: &str| ranked.iter().position(|x| x == k).unwrap();
    for k in ["rare-0", "rare-1", "rare-2"] {
        assert!(rank(k) >= window, "{k} at {} of {ranked:?}", rank(k));
    }
    let a = text(call(&mut s, "recall", json!({"query": "widget", "scope": "RARE", "limit": 3, "budget_tokens": 5000})));
    for k in ["rare-0", "rare-1", "rare-2"] {
        assert!(a.contains(k), "{k}: {a}");
    }
    assert!(!a.contains("scoring near zero"), "{a}");
    // Best first among them, as the unfiltered ranking has them.
    let keys: Vec<String> = store.search(&terms, None, Some("RARE"), 2).unwrap().into_iter().map(|(h, _, _)| h.key).collect();
    assert_eq!(keys, ["rare-0", "rare-1"]);
    // The same with both filters, and the kind alone finds what the scope holds.
    let keys: Vec<String> = store.search(&terms, Some("memory"), Some("RARE"), 5).unwrap().into_iter().map(|(h, _, _)| h.key).collect();
    assert_eq!(keys, ["rare-0", "rare-1", "rare-2"]);
    call(&mut s, "journal", json!({"lane": "RARE", "entry": "note", "text": format!("widget {}", "other words ".repeat(20))}));
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let keys: Vec<String> = store.search(&terms, Some("journal"), None, 3).unwrap().into_iter().map(|(h, _, _)| h.key).collect();
    assert!(keys.len() == 1 && keys[0].starts_with("journal:RARE:"), "{keys:?}");
    // A filter nothing holds, and a kind the scope lacks.
    assert!(store.search(&terms, None, Some("NO-SUCH"), 3).unwrap().is_empty());
    assert!(store.search(&terms, Some("rule"), Some("RARE"), 3).unwrap().is_empty());
}

/// The recall hook has no filter, but its status check drops superseded rows: more
/// of them ranked better than the hook's window of 24 * 2 + 8 must not hide a live
/// match. The memory eval never reaches this path.
#[test]
fn the_hook_search_reads_past_superseded_rows() {
    use atlas::memory::recall::{candidates, Gate};
    let f = fixture("recall-hook-past");
    let (mut s, _fx) = server(&f);
    for i in 0..60 {
        let old = format!("old-{i}");
        call(&mut s, "remember", json!({"key": old, "kind": "memory", "sub": "project", "scope": "OLD", "title": format!("widget {i}"), "summary": "s", "body": "widget widget widget"}));
        call(&mut s, "supersede", json!({"old": format!("old-{i}"), "key": format!("new-{i}"), "title": format!("replaced {i}"), "summary": "s", "body": "nothing about it now"}));
    }
    for i in 0..100 {
        call(&mut s, "remember", json!({"key": format!("filler-{i}"), "kind": "memory", "sub": "project", "scope": "FILL", "title": format!("other {i}"), "summary": "s", "body": "nothing about it"}));
    }
    let long = "other words ".repeat(20);
    call(&mut s, "remember", json!({"key": "live-one", "kind": "memory", "sub": "project", "scope": "LIVE", "title": "item", "summary": "s", "body": format!("widget {long}")}));
    let store = MemoryStore::open(&f.settings.store).unwrap();
    let terms = vec!["widget".to_string()];
    let ranked: Vec<(String, String)> = store.search_with(&terms, None, None, 500, atlas::memory::rules::Mode::Live).unwrap().into_iter().map(|(h, _, _)| (h.key, h.status)).collect();
    let at = ranked.iter().position(|(k, _)| k == "live-one").unwrap();
    assert!(at >= 24 * 2 + 8, "{at}");
    assert!(ranked[..at].iter().all(|(_, st)| st == "superseded"), "{ranked:?}");
    let g = Gate { rules: atlas::memory::rules::Mode::Today, fts_floor: 0.0, min_terms: 1, cos_floor: 0.0, cos_alone: 1.0 };
    let found = candidates(&store, "widget", &g, None).unwrap();
    assert!(found.iter().any(|c| c.head.key == "live-one"), "{:?}", found.iter().map(|c| &c.head.key).collect::<Vec<_>>());
}

/// Real session finding: in a small store every word scores next to nothing, so
/// of 4 records holding "gear note" a query showed 2 and said nothing of the rest.
#[test]
fn recall_says_what_it_left_out() {
    let f = fixture("recall-cut");
    let (mut s, _fx) = server(&f);
    for i in 0..4 {
        call(&mut s, "remember", json!({"key": format!("gear-{i}"), "kind": "memory", "sub": "project", "title": format!("item {i}"), "summary": "s", "body": format!("gear note {i}")}));
    }
    let shown = |a: &str| (0..4).filter(|i| a.contains(&format!("gear-{i}"))).count();
    let more = |a: &str, what: &str| a.lines().find(|l| l.contains(what)).and_then(|l| l.strip_prefix('+')).and_then(|l| l.split(' ').next()).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
    for args in [json!({"query": "gear note"}), json!({"query": "gear note", "limit": 20, "budget_tokens": 2000})] {
        let a = text(call(&mut s, "recall", args));
        assert_eq!(shown(&a) + more(&a, "scoring near zero"), 4, "each record is shown or counted: {a}");
    }
    // A limit that cuts says so.
    for i in 4..12 {
        call(&mut s, "remember", json!({"key": format!("gear-{i}"), "kind": "memory", "sub": "project", "title": format!("item {i}"), "summary": "s", "body": format!("gear note {i}")}));
    }
    let a = text(call(&mut s, "recall", json!({"query": "gear note", "limit": 3})));
    assert!(a.contains("+more (limit=3)"), "{a}");
    let a = text(call(&mut s, "recall", json!({"query": "gear note", "limit": 0})));
    assert!(shown(&a) + more(&a, "scoring near zero") > 1, "a limit of 0 keeps the search's own window: {a}");
    let a = text(call(&mut s, "recall", json!({"query": "gear note", "limit": 200})));
    assert!(!a.contains("(limit="), "{a}");
}
