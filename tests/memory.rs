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
    let a = snapshot(&store, &dir, u64::MAX).unwrap();
    assert!(a.bytes > 0);
    assert_eq!(a.records, 10);
    // Same second: the second name takes a counter and still sorts after.
    let b = snapshot(&store, &dir, a.bytes + a.bytes / 2).unwrap();
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
    let mut snaps = vec![snapshot(&store, &dir, u64::MAX).unwrap().path];
    revise(&mut store, "widget-cache", "\nThe widget cache holds 128 entries.\n");
    snaps.push(snapshot(&store, &dir, u64::MAX).unwrap().path);
    {
        let mut w = store.writer().unwrap();
        w.supersede(&["rules:r00001".into()], "rules:r00004", "rule", "", "rules", &draft("- 2026-02-01 alpha2: bounded at 128.\n"), Some(("rules", Place::After("rules:r00001".into()))))
            .unwrap();
        w.create("journal:track-x:0001", "journal", "note", "track-x", &draft("first entry\n"), None).unwrap();
        w.add_refs("journal:track-x:0001", &[("lane".into(), "track-x".into())]).unwrap();
        w.commit().unwrap();
    }
    snaps.push(snapshot(&store, &dir, u64::MAX).unwrap().path);
    revise(&mut store, "widget-cache", "\nThe widget cache holds 256 entries.\n");
    revise(&mut store, "journal:track-x:0001", "first entry, corrected\n");
    let last = snapshot(&store, &dir, u64::MAX).unwrap();
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
    let a = snapshot(&store, &dir, u64::MAX).unwrap();
    let b = snapshot(&store, &dir, u64::MAX).unwrap();
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
