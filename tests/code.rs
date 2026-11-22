//! The code index: a generic Rust crate and WGSL shaders, a synthetic SCIP
//! index standing in for rust-analyzer's, the `symbol` tool, the line-scan
//! fallback, the recall check and the background indexer.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use atlas::code::{self, CodeRoot, CodeSetup, Limits};
use atlas::code_index::{self, Indexer, RustSource};
use atlas::json;
use atlas::json::Value;
use atlas::protobuf::Writer;
use atlas::tools::{CodeState, Server};
use common::*;

const LIB: &str = "pub mod shapes;
/// A store.
pub struct Store { pub items: Vec<u32> }
pub trait Open { fn open(&self) -> u32; }
impl Store {
    pub fn new() -> Store { Store { items: Vec::new() } }
}
impl Open for Store {
    fn open(&self) -> u32 { helper(self.items.len()) }
}
fn helper(n: usize) -> u32 { n as u32 }
#[cfg(target_os = \"none\")]
pub fn only_elsewhere() {}
const DOC: &str = \"
fn not_a_definition() {}
\";
";

const SHAPES: &str = "pub fn area() -> u32 { let s = crate::Store::new(); s.open() }\n";

const COMMON_WGSL: &str = "#define_import_path demo::common
struct Light { color: vec3<f32> };
fn shade(l: Light) -> vec3<f32> { return l.color; }
";

const MAIN_WGSL: &str = "#import demo::common::{Light, shade}
@group(0) @binding(0) var<uniform> light: Light;
@fragment
fn main() -> @location(0) vec4<f32> { return vec4<f32>(shade(light), 1.0); }
";

const P: &str = "rust-analyzer cargo app 0.1.0 ";

fn occ(w: &mut Writer, range: &[i32], sym: &str, roles: i64, encl: &[i32]) {
    let mut o = Writer::default();
    o.packed(1, range).string(2, sym);
    if roles != 0 {
        o.int(3, roles);
    }
    if !encl.is_empty() {
        o.packed(7, encl);
    }
    w.message(2, &o);
}

fn info(w: &mut Writer, sym: &str, kind: i64, name: &str) {
    let mut i = Writer::default();
    i.string(1, sym).int(5, kind).string(6, name);
    w.message(3, &i);
}

/// The SCIP index rust-analyzer would write for the crate, minus the
/// `cfg`-disabled function, the module declaration and the constant (so the
/// line scan has something to add).
fn scip_bytes() -> Vec<u8> {
    let s = |x: &str| format!("{P}{x}");
    let mut lib = Writer::default();
    lib.string(1, "src/lib.rs").string(4, "rust");
    occ(&mut lib, &[2, 11, 16], &s("Store#"), 1, &[2, 0, 2, 40]);
    occ(&mut lib, &[2, 32, 35], "rust-analyzer cargo std 1.0.0 vec/Vec#", 0, &[]);
    occ(&mut lib, &[3, 10, 14], &s("Open#"), 1, &[3, 0, 3, 41]);
    occ(&mut lib, &[3, 20, 24], &s("Open#open()."), 1, &[3, 17, 3, 39]);
    occ(&mut lib, &[4, 5, 10], &s("impl#[Store]"), 0, &[4, 0, 6, 1]);
    occ(&mut lib, &[5, 11, 14], &s("impl#[Store]new()."), 1, &[5, 4, 5, 55]);
    occ(&mut lib, &[5, 20, 25], &s("Store#"), 0, &[]);
    occ(&mut lib, &[7, 5, 9], &s("impl#[Store][Open]"), 0, &[7, 0, 9, 1]);
    occ(&mut lib, &[8, 7, 11], &s("impl#[Store][Open]open()."), 1, &[8, 4, 8, 55]);
    occ(&mut lib, &[8, 28, 34], &s("helper()."), 0, &[]);
    occ(&mut lib, &[10, 3, 9], &s("helper()."), 1, &[10, 0, 10, 40]);
    occ(&mut lib, &[10, 10, 11], "local 0", 1, &[]);
    info(&mut lib, &s("Store#"), 49, "Store");
    info(&mut lib, &s("Open#"), 53, "Open");
    info(&mut lib, &s("Open#open()."), 70, "open");
    info(&mut lib, &s("impl#[Store]new()."), 80, "new");
    info(&mut lib, &s("impl#[Store][Open]open()."), 26, "open");
    info(&mut lib, &s("helper()."), 17, "helper");
    let mut shapes = Writer::default();
    shapes.string(1, "src\\shapes.rs").string(4, "rust");
    occ(&mut shapes, &[0, 7, 11], &s("shapes/area()."), 1, &[0, 0, 0, 63]);
    occ(&mut shapes, &[0, 38, 43], &s("Store#"), 0, &[]);
    occ(&mut shapes, &[0, 45, 48], &s("impl#[Store]new()."), 0, &[]);
    occ(&mut shapes, &[0, 55, 59], &s("impl#[Store][Open]open()."), 0, &[]);
    info(&mut shapes, &s("shapes/area()."), 17, "area");
    let mut ix = Writer::default();
    ix.message(2, &lib).message(2, &shapes);
    ix.buf
}

struct Fixture {
    _t: Temp,
    root: PathBuf,
    db: PathBuf,
    scip: PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    let t = Temp::new(tag);
    let root = t.path().join("app");
    write(&root.join("Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\n");
    write(&root.join("src/lib.rs"), LIB);
    write(&root.join("src/shapes.rs"), SHAPES);
    write(&root.join("shaders/common.wgsl"), COMMON_WGSL);
    write(&root.join("shaders/main.wgsl"), MAIN_WGSL);
    write(&root.join("target/debug/build/gen.rs"), "pub fn generated() {}\n");
    let scip = t.path().join("app.scip");
    fs::write(&scip, scip_bytes()).unwrap();
    let db = t.path().join("state/atlas.sqlite");
    Fixture { root, db, scip, _t: t }
}

/// One empty vault beside the database (a store needs at least one).
fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

fn setup(f: &Fixture) -> CodeSetup {
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec {
            name: "app".into(),
            root: f.root.clone(),
            build_scripts: false,
            scip: Default::default(),
            exclude: Vec::new(),
        }],
        debounce_s: Some(0),
        ..Default::default()
    };
    let mut code = CodeSetup::from_spec(&spec, &f.db, false);
    // Another build on the machine must not stall the tests.
    code.defer_on_cargo = false;
    code
}

fn server(f: &Fixture, code: CodeSetup, indexer: Option<Indexer>) -> Server {
    let store = atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap();
    let mut s = Server::new(store);
    s.set_code(CodeState { setup: code, indexer });
    s
}

fn index(f: &Fixture, code: &CodeSetup) -> code_index::IndexReport {
    // The store's open makes the meta table first.
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let mut conn = atlas::db::Connection::open(&f.db).unwrap();
    code_index::index_root(code, &mut conn, &code.roots[0], &RustSource::File(f.scip.clone())).unwrap()
}

fn rows(v: &Value, key: &str) -> Vec<(String, u64, String, String)> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["path"].as_str().unwrap().to_string(), h["line"].as_u64().unwrap(), h["kind"].as_str().unwrap().to_string(), h[if key == "refs" { "within" } else { "container" }].as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn symbol_gives_definitions_references_with_callers_and_impls() {
    let f = fixture("code-symbol");
    let code = setup(&f);
    let rep = index(&f, &code);
    assert_eq!(rep.rust, "scip");
    assert_eq!(rep.scip_counts, (2, 15, 10), "{rep:?}");
    assert_eq!(rep.rows.external_refs, 1);
    let mut s = server(&f, code, None);
    let v = s.call("symbol", &json!({"name": "Store"})).unwrap();
    assert_eq!(rows(&v, "defs"), [("src/lib.rs".into(), 3, "struct".into(), "".into())]);
    assert_eq!(rows(&v, "refs"), [("src/lib.rs".into(), 6, "".into(), "Store::new".into()), ("src/shapes.rs".into(), 1, "".into(), "shapes::area".into())]);
    let impls: Vec<String> = v["impls"].as_array().unwrap().iter().map(|h| format!("{}:{}", h["line"], h["container"].as_str().unwrap())).collect();
    assert_eq!(impls, ["5:Store", "8:Open for Store"]);
    let v = s.call("symbol", &json!({"name": "open"})).unwrap();
    assert_eq!(rows(&v, "defs"), [("src/lib.rs".into(), 4, "trait method".into(), "Open".into()), ("src/lib.rs".into(), 9, "method (Open)".into(), "Store".into())]);
    assert_eq!(rows(&v, "refs"), [("src/shapes.rs".into(), 1, "".into(), "shapes::area".into())]);
    let v = s.call("symbol", &json!({"name": "Open"})).unwrap();
    assert_eq!(v["totals"]["impls"].as_i64(), Some(1));
    let v = s.call("symbol", &json!({"name": "helper"})).unwrap();
    assert_eq!(rows(&v, "refs"), [("src/lib.rs".into(), 9, "".into(), "Store::open".into())]);
    // Container filter.
    let v = s.call("symbol", &json!({"name": "Store::open"})).unwrap();
    assert_eq!(rows(&v, "defs").len(), 1);
    // The line scan adds what SCIP lacks; a string's contents are not code.
    let v = s.call("symbol", &json!({"name": "only_elsewhere"})).unwrap();
    assert_eq!(rows(&v, "defs"), [("src/lib.rs".into(), 13, "fn (scan)".into(), "".into())]);
    assert_eq!(s.call("symbol", &json!({"name": "not_a_definition"})).unwrap()["totals"]["defs"].as_i64(), Some(0));
    assert_eq!(s.call("symbol", &json!({"name": "generated"})).unwrap()["totals"]["defs"].as_i64(), Some(0), "target/ is not walked");
    // Text answer: header rows, caller column, snippet on the name.
    let t = s.answer("symbol", &json!({"name": "Store"})).unwrap().into_text();
    assert!(t.starts_with("defs 1\nloc\tkind\tin\tsnippet\napp:src/lib.rs:3\tstruct\t\tpub struct Store"), "{t}");
    assert!(t.contains("refs 2\nloc\tin\tsnippet\napp:src/lib.rs:6\tStore::new\t"), "{t}");
    assert!(t.contains("impls 2\nloc\timpl\napp:src/lib.rs:5-7\tStore\napp:src/lib.rs:8-10\tOpen for Store\n"), "{t}");
    let miss = s.answer("symbol", &json!({"name": "Stor"})).unwrap().into_text();
    assert!(miss.starts_with("not found; names starting so: Store"), "{miss}");
}

#[test]
fn wgsl_declarations_imports_and_references() {
    let f = fixture("code-wgsl");
    let code = setup(&f);
    index(&f, &code);
    let mut s = server(&f, code, None);
    let v = s.call("symbol", &json!({"name": "shade"})).unwrap();
    assert_eq!(rows(&v, "defs"), [("shaders/common.wgsl".into(), 3, "fn".into(), "demo::common".into())]);
    assert_eq!(rows(&v, "refs"), [("shaders/main.wgsl".into(), 1, "import".into(), "".into()), ("shaders/main.wgsl".into(), 4, "".into(), "main".into())]);
    let v = s.call("symbol", &json!({"name": "light"})).unwrap();
    assert_eq!(rows(&v, "defs")[0].2, "var <uniform> group 0 binding 0");
    let v = s.call("symbol", &json!({"name": "main"})).unwrap();
    assert_eq!(rows(&v, "defs")[0].2, "fn fragment");
}

#[test]
fn recall_against_grep_has_no_misses_and_counts_the_scan() {
    let f = fixture("code-recall");
    let code = setup(&f);
    index(&f, &code);
    let conn = atlas::db::Connection::open(&f.db).unwrap();
    let r = atlas::recall::code_check(&conn, &code.roots).unwrap();
    assert!(r.misses.is_empty(), "{:?}", r.misses);
    assert_eq!(r.files, (2, 2));
    // lib.rs: mod, struct, trait, fn new, fn open, fn helper, fn only_elsewhere,
    // const DOC; shapes.rs: fn area; WGSL: struct, fn shade, var, fn main.
    assert_eq!(r.truth, 13, "{r:?}");
    assert_eq!(r.excluded, 1, "the fn inside a string literal");
    assert_eq!(r.scan_only, 3, "mod shapes, only_elsewhere, DOC");
    // Without the SCIP rows of one definition, recall reports it.
    conn.execute("DELETE FROM code_occ WHERE name = 'helper' AND sect = 0", []).unwrap();
    let r = atlas::recall::code_check(&conn, &code.roots).unwrap();
    assert_eq!(r.misses.iter().map(|m| (m.name.as_str(), m.line)).collect::<Vec<_>>(), [("helper", 11)]);
}

#[test]
fn reindex_replaces_one_root_and_scan_only_without_rust_analyzer() {
    let f = fixture("code-reindex");
    let code = setup(&f);
    index(&f, &code);
    // No rust-analyzer: the line scan alone gives the definitions.
    let mut conn = atlas::db::Connection::open(&f.db).unwrap();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("not installed".into())).unwrap();
    assert_eq!(rep.rust, "scan only: not installed");
    let mut s = server(&f, code.clone(), None);
    let v = s.call("symbol", &json!({"name": "Store"})).unwrap();
    assert_eq!(rows(&v, "defs"), [("src/lib.rs".into(), 3, "struct (scan)".into(), "".into())]);
    assert_eq!(v["totals"]["refs"].as_i64(), Some(0));
    // The coverage note comes with the session's first symbol answer only.
    assert_eq!(v["freshness"], json!(["app: rust definitions only (scan only: not installed)"]), "{v}");
    let t = s.answer("symbol", &json!({"name": "Store"})).unwrap().into_text();
    assert!(!t.contains("definitions only"), "said once per session: {t}");
    let mut s2 = server(&f, code.clone(), None);
    let t = s2.answer("symbol", &json!({"name": "Store"})).unwrap().into_text();
    assert!(t.ends_with("app: rust definitions only (scan only: not installed)\n"), "a new session hears it again: {t}");
    // One root replaced: no duplicates after a second write.
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::File(f.scip.clone())).unwrap();
    assert_eq!(rep.write.defs, 14);
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM code_occ WHERE name = 'Store' AND sect = 0", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn background_indexer_indexes_at_start_and_after_a_change() {
    let f = fixture("code-indexer");
    let code = setup(&f);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let ix = Indexer::start(code.clone(), f.db.clone(), Some(RustSource::File(f.scip.clone())));
    assert!(ix.wait_idle(Duration::from_secs(60)), "{:?}", ix.status());
    let mut s = server(&f, code, Some(ix));
    assert_eq!(s.call("symbol", &json!({"name": "helper"})).unwrap()["totals"]["defs"].as_i64(), Some(1));
    // A change: the indexer re-indexes after the (zero) debounce.
    fs::write(f.root.join("src/shapes.rs"), format!("{SHAPES}pub fn added_later() {{}}\n")).unwrap();
    let ix = s.code().unwrap().indexer.as_ref().unwrap();
    ix.check();
    assert!(ix.wait_reports(2, Duration::from_secs(60)), "{:?}", ix.status());
    assert!(ix.wait_idle(Duration::from_secs(60)));
    assert_eq!(s.call("symbol", &json!({"name": "added_later"})).unwrap()["totals"]["defs"].as_i64(), Some(1));
    let reports = s.code().unwrap().indexer.as_ref().unwrap().reports();
    assert!(reports.len() >= 2, "{reports:?}");
    // The lease is free again.
    let mut conn = atlas::db::Connection::open(&f.db).unwrap();
    assert!(code_index::take_lease(&mut conn, "test", Duration::from_secs(5)).unwrap());
    assert!(!code_index::take_lease(&mut conn, "other", Duration::from_secs(5)).unwrap(), "one holder at a time");
}

#[test]
fn background_indexer_waits_while_the_defer_lock_exists() {
    let f = fixture("code-defer");
    let mut code = setup(&f);
    let lock = f.db.parent().unwrap().join("gpu.lock");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(&lock, "held").unwrap();
    code.defer_lock = Some(lock.clone());
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let ix = Indexer::start(code.clone(), f.db.clone(), Some(RustSource::File(f.scip.clone())));
    // Deferred is settled for wait_idle: the root is due but waiting.
    assert!(ix.wait_idle(Duration::from_secs(60)), "{:?}", ix.status());
    assert!(matches!(ix.status().get("app"), Some(code_index::RootState::Deferred(why)) if why.contains("lock file")), "{:?}", ix.status());
    assert!(ix.reports().is_empty());
    let mut s = server(&f, code, Some(ix));
    let v = s.call("symbol", &json!({"name": "helper"})).unwrap();
    assert!(v["freshness"].to_string().contains("reindex waiting: lock file"), "{v}");
    // The lock goes: a wake (or the recheck timer) indexes the root.
    fs::remove_file(&lock).unwrap();
    let ix = s.code().unwrap().indexer.as_ref().unwrap();
    ix.check();
    assert!(ix.wait_reports(1, Duration::from_secs(60)), "{:?}", ix.status());
    assert!(ix.wait_idle(Duration::from_secs(60)));
    assert_eq!(s.call("symbol", &json!({"name": "helper"})).unwrap()["totals"]["defs"].as_i64(), Some(1));
}

#[test]
fn symbol_is_one_statement_and_limits_each_section() {
    let f = fixture("code-limits");
    let code = setup(&f);
    index(&f, &code);
    let conn = atlas::db::Connection::open(&f.db).unwrap();
    let a = code::symbol(&conn, "Store", Limits { defs: 1, refs: 1, impls: 1 }).unwrap();
    assert_eq!(a.totals, [1, 2, 2]);
    assert_eq!(a.hits.iter().map(|h| h.sect).collect::<Vec<_>>(), [0, 1, 2]);
    let t = code::render(&a, Limits { defs: 1, refs: 1, impls: 1 });
    assert!(t.contains("+1 more (limit=1)\nimpls 2"), "{t}");
    let none = code::symbol(&conn, "Nothing", Limits { defs: 5, refs: 5, impls: 5 }).unwrap();
    assert_eq!(none.totals, [0, 0, 0]);
    // A database the code index never touched answers empty.
    let t = Temp::new("code-empty");
    let empty = atlas::db::Connection::open(t.path().join("e.sqlite")).unwrap();
    assert!(code::symbol(&empty, "Store", Limits { defs: 1, refs: 1, impls: 1 }).unwrap().hits.is_empty());
}

#[test]
fn code_config_is_read_and_unconfigured_symbol_is_refused() {
    let t = Temp::new("code-config");
    let p = t.path().join("atlas.json");
    fs::write(&p, r#"{"db": "s/a.sqlite", "code": {"roots": [{"name": "app", "root": "app"}], "debounce_s": 5}}"#).unwrap();
    let c = atlas::config::Config::load(&p).unwrap();
    let setup = CodeSetup::from_spec(c.code.as_ref().unwrap(), c.db.as_ref().unwrap(), true);
    assert_eq!(
        setup.roots,
        [CodeRoot { name: "app".into(), root: t.path().join("app"), build_scripts: false, scip: Default::default(), exclude: Vec::new() }]
    );
    assert!(setup.defer_on_cargo && setup.defer_lock.is_none(), "defaults: wait for cargo, no lock file");
    assert_eq!(setup.debounce, Duration::from_secs(5));
    assert_eq!(setup.dir, t.path().join("s").join("code"));
    let store = atlas::store::Store::open(&t.path().join("x.sqlite"), vaults(&t.path().join("x.sqlite"))).unwrap();
    let mut s = Server::new(store);
    assert!(s.call("symbol", &json!({"name": "x"})).is_err());
}


#[test]
fn file_items_map_a_path_to_its_items_and_skip_stale_or_foreign_files() {
    let f = fixture("code-items");
    let src = "pub struct Cache {\n    n: u32,\n}\n\nimpl Cache {\n    pub fn get(&self) -> u32 {\n        let n = self.n;\n        n + 1\n    }\n}\n\nmod tests {\n    fn t() {\n        let _ = 1;\n    }\n}\n";
    write(&f.root.join("src/multi.rs"), src);
    let code = setup(&f);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let mut conn = atlas::db::Connection::open(&f.db).unwrap();
    code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    let path = f.root.join("src/multi.rs");
    let items = code::file_items(&conn, &path.to_string_lossy()).unwrap().expect("covered");
    let get = code::enclosing(&items, 7).expect("line 7 is in get");
    assert_eq!(get.label(), "fn Cache::get 6-9");
    assert_eq!(code::enclosing(&items, 14).unwrap().label(), "fn t 13-15");
    assert_eq!(code::enclosing(&items, 12).unwrap().label(), "mod tests 12-16");
    assert!(code::enclosing(&items, 4).is_none());
    // Either separator names the same file.
    let back = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        assert!(code::file_items(&conn, &back).unwrap().is_some());
    }
    // The router's Grep annotation over the real index.
    let grep = "src/multi.rs:7:        let n = self.n;\nsrc/multi.rs-8-        n + 1\nsrc/nothere.rs:1:x".to_string();
    let out = atlas::hook_rewrite::annotate_grep(&grep, &f.root.to_string_lossy(), None, &mut |p| code::file_items(&conn, p)).unwrap();
    assert_eq!(out, "src/multi.rs:7:        let n = self.n; [fn Cache::get 6-9]\nsrc/multi.rs-8-        n + 1\nsrc/nothere.rs:1:x");
    // A covered file whose rows hold no item is covered with no items, not
    // unknown (`file_items` reads the file and its rows in one statement).
    let shapes = f.root.join("src/shapes.rs");
    let rows = "DELETE FROM code_occ WHERE file = (SELECT id FROM code_files WHERE root = 'app' AND path = 'src/shapes.rs')";
    conn.execute(rows, []).unwrap();
    assert_eq!(code::file_items(&conn, &shapes.to_string_lossy()).unwrap().map(|v| v.len()), Some(0));
    // A file outside every root, and a file written after the indexing.
    assert!(code::file_items(&conn, &f.db.to_string_lossy()).unwrap().is_none());
    let later = std::time::SystemTime::now() + Duration::from_secs(60);
    fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
    assert!(code::file_items(&conn, &path.to_string_lossy()).unwrap().is_none());
}

/// The rule: a file modified after a run's walk began is not served from that
/// run's rows. A stand-in rust-analyzer edits a file while the run is under
/// way. The run's stamp used to be taken at its write, after the edit, so the
/// file passed as indexed (and when the edit lands after atlas read the file,
/// its old lines would be served).
#[cfg(unix)]
#[test]
fn a_file_modified_after_the_walk_began_is_not_served_from_that_run() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("code-mid-run");
    write(&f.root.join("src/mid.rs"), "pub fn before() {}\n");
    // Its arguments: scip ROOT --output FILE --config-path JSON. It sleeps
    // well past the stamps' millisecond, edits, and fails, so the run goes on
    // from the line scan.
    let ra = f.root.parent().unwrap().join("stand-in-ra.sh");
    fs::write(&ra, "#!/bin/sh\nsleep 0.05\nprintf 'pub fn during() {}\\n' >> \"$2/src/mid.rs\"\nexit 1\n").unwrap();
    fs::set_permissions(&ra, fs::Permissions::from_mode(0o755)).unwrap();
    let code = setup(&f);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let mut conn = atlas::db::Connection::open(&f.db).unwrap();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::Analyzer(ra)).unwrap();
    assert!(rep.rust.contains("exited with"), "the stand-in ran: {}", rep.rust);
    assert!(fs::read_to_string(f.root.join("src/mid.rs")).unwrap().contains("during"), "the stand-in edited the file");
    assert!(code::file_items(&conn, &f.root.join("src/mid.rs").to_string_lossy()).unwrap().is_none(), "edited after the walk began");
    assert!(code::file_items(&conn, &f.root.join("src/lib.rs").to_string_lossy()).unwrap().is_some(), "an untouched file is served");
}

/// Asks the indexer to compare every root now, and waits until it has: the
/// comparison, and any run it started, have ended
/// ([`Indexer::wait_checked`]).
fn checked(ix: &Indexer) {
    let t = ix.check();
    assert!(ix.wait_checked(t, Duration::from_secs(30)), "the check was never served: {:?}", ix.status());
}

/// Every server shares one index database, and two servers can compute
/// different fingerprints for one root: one started before a configuration
/// change (here a configured prebuilt SCIP file) and one after. Each settles on
/// the other's write. A stale root goes at once only for a mark (a stored
/// fingerprint of ''), so the two never take turns re-indexing it. This test
/// guards that mark-only rule; it does not fail on the code before the rule.
#[test]
fn two_indexers_that_disagree_on_a_fingerprint_settle() {
    let f = fixture("code-two-fp");
    let mut a = setup(&f);
    a.watch = false;
    a.rescan_interval = Duration::from_millis(20);
    let mut b = a.clone();
    b.roots[0].scip = code::Prebuilt::Files(vec![(String::new(), Some(f.scip.clone()))]);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let ia = Indexer::start(a, f.db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(ia.wait_reports(1, Duration::from_secs(30)), "{:?}", ia.status());
    let ib = Indexer::start(b, f.db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(ib.wait_reports(1, Duration::from_secs(30)), "{:?}", ib.status());
    // Each compares the root twice more, with the other's write stored.
    for _ in 0..2 {
        checked(&ia);
        checked(&ib);
    }
    assert_eq!((ia.reports().len(), ib.reports().len()), (1, 1), "the two indexers took turns");
}

/// The same two servers, watching as servers do: each walks only when asked
/// (here a poke, in a server the safety-net walk), so each one's walk comes
/// after the other's write. An indexer that forgot the fingerprint its own
/// run stored would read the other's write as a change, and the two would
/// take turns again at every walk. The test above walks every 20 ms, so each
/// indexer's own next walk comes first and hides that.
#[test]
fn two_watching_indexers_that_disagree_on_a_fingerprint_settle() {
    let f = fixture("code-two-fp-watch");
    let mut a = setup(&f);
    a.watch = true;
    a.rescan_interval = Duration::from_secs(60);
    let mut b = a.clone();
    b.roots[0].scip = code::Prebuilt::Files(vec![(String::new(), Some(f.scip.clone()))]);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let ia = Indexer::start(a, f.db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(ia.wait_reports(1, Duration::from_secs(30)), "{:?}", ia.status());
    let ib = Indexer::start(b, f.db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(ib.wait_reports(1, Duration::from_secs(30)), "{:?}", ib.status());
    for _ in 0..3 {
        checked(&ia);
        checked(&ib);
    }
    assert_eq!((ia.reports().len(), ib.reports().len()), (1, 1), "the two indexers took turns");
}

/// A lease left by a holder that is gone (a server that crashed in a run) is
/// free once its time is up, and a holder takes its own lease again before
/// then. Without the first, one crash would stop every server's indexing for
/// good.
#[test]
fn an_expired_lease_is_free_and_a_holder_takes_its_own_again() {
    let f = fixture("code-lease-expiry");
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let mut conn = atlas::db::Connection::open(&f.db).unwrap();
    let ttl = Duration::from_secs(600);
    assert!(code_index::take_lease(&mut conn, "crashed", ttl).unwrap());
    assert!(!code_index::take_lease(&mut conn, "next", ttl).unwrap(), "held");
    assert!(code_index::take_lease(&mut conn, "crashed", ttl).unwrap(), "its own lease, again");
    conn.execute("UPDATE code_lease SET until = until - 1200 WHERE id = 1", []).unwrap();
    assert!(code_index::take_lease(&mut conn, "next", ttl).unwrap(), "an expired lease is free");
    assert!(!code_index::take_lease(&mut conn, "crashed", ttl).unwrap(), "and the next holder has it");
}

/// A watching indexer still walks at its safety-net interval when nothing in
/// the root moves. That walk is how it finds a mark another root's run left
/// (a stored fingerprint of ''), which no file event announces.
#[test]
fn a_watching_indexer_finds_a_mark_at_its_safety_net_walk() {
    let f = fixture("code-net-walk");
    let mut code = setup(&f);
    code.watch = true;
    code.rescan_interval = Duration::from_millis(300);
    index(&f, &code);
    let ix = Indexer::start(code, f.db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(ix.wait_idle(Duration::from_secs(10)), "the first walk never ended: {:?}", ix.status());
    assert_eq!(ix.reports().len(), 0, "the stored index is current");
    let conn = atlas::db::Connection::open(&f.db).unwrap();
    conn.execute("UPDATE code_roots SET fingerprint = ''", []).unwrap();
    assert!(ix.wait_reports(1, Duration::from_secs(10)), "the mark was never found: {:?}", ix.status());
}

/// A poke makes a watching indexer walk at once, as a watcher that lost
/// events (a queue overflow) asks it to: here it finds a mark long before its
/// safety-net walk.
#[test]
fn a_poke_makes_a_watching_indexer_walk_at_once() {
    let f = fixture("code-poke");
    let mut code = setup(&f);
    code.watch = true;
    code.rescan_interval = Duration::from_secs(60);
    index(&f, &code);
    let ix = Indexer::start(code, f.db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(ix.wait_idle(Duration::from_secs(10)), "the first walk never ended: {:?}", ix.status());
    let conn = atlas::db::Connection::open(&f.db).unwrap();
    conn.execute("UPDATE code_roots SET fingerprint = ''", []).unwrap();
    assert_eq!(ix.reports().len(), 0, "nothing ran before the poke");
    checked(&ix);
    assert_eq!(ix.reports().len(), 1, "the poke's walk did not run the marked root: {:?}", ix.status());
}

/// Two servers that see one change run it once: the second, given the lease
/// after the first's run, finds the stored fingerprint current and stands
/// down. The test holds the lease while both see the change, so both wait
/// for it. Neither walks again after that (both watch the root, as servers
/// do, the safety-net walk is far off, and nothing pokes them), so nothing
/// but that recheck stops the second run. A stand-in rust-analyzer keeps the
/// first run long enough for two holders that shared a name to overlap.
#[cfg(unix)]
#[test]
fn two_indexers_index_one_change_once() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("code-two-once");
    let ra = f.root.parent().unwrap().join("slow-ra.sh");
    fs::write(&ra, "#!/bin/sh\nsleep 0.3\nexit 1\n").unwrap();
    fs::set_permissions(&ra, fs::Permissions::from_mode(0o755)).unwrap();
    let mut code = setup(&f);
    code.watch = true;
    code.rescan_interval = Duration::from_secs(60);
    index(&f, &code);
    let ia = Indexer::start(code.clone(), f.db.clone(), Some(RustSource::Analyzer(ra.clone())));
    let ib = Indexer::start(code.clone(), f.db.clone(), Some(RustSource::Analyzer(ra)));
    let first_walks = ia.wait_idle(Duration::from_secs(10)) && ib.wait_idle(Duration::from_secs(10));
    assert!(first_walks, "a first walk never ended: {:?} {:?}", ia.status(), ib.status());
    assert_eq!(ia.reports().len() + ib.reports().len(), 0, "the stored index is current");
    // One change, renamed into place: with a debounce of 0, a walk between
    // `fs::write`'s create and its write stored the empty file, and the full
    // file was a second change (2 runs in 4 of 40 tries with every core busy).
    let half = f.root.join("src/.later.rs.part");
    fs::write(&half, "pub fn later() {}\n").unwrap();
    let mut conn = atlas::db::Connection::open(&f.db).unwrap();
    assert!(code_index::take_lease(&mut conn, "the test", Duration::from_secs(600)).unwrap());
    fs::rename(&half, f.root.join("src/later.rs")).unwrap();
    // Both compare the root after the change, and wait for the lease.
    checked(&ia);
    checked(&ib);
    let pending = |ix: &Indexer| matches!(ix.status().get("app"), Some(code_index::RootState::Pending));
    assert!(pending(&ia) && pending(&ib), "{:?} {:?}", ia.status(), ib.status());
    // Given back: one runs the change, and the other, once it has the lease,
    // has stood down, or run, by the time both are idle.
    code_index::release_lease(&conn, "the test").unwrap();
    let settled = ia.wait_idle(Duration::from_secs(30)) && ib.wait_idle(Duration::from_secs(30));
    assert!(settled, "{:?} {:?}", ia.status(), ib.status());
    assert_eq!(ia.reports().len() + ib.reports().len(), 1, "one change was indexed more than once");
}

/// A running indexer indexes a file edited during its run again, once. The
/// run stores its walk's fingerprint, and the indexer remembers that one, so
/// the edit reads as a change at the next walk. A stand-in rust-analyzer makes
/// the edit during the first run only.
#[cfg(unix)]
#[test]
fn a_running_indexer_indexes_a_mid_run_edit_again_once() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("code-mid-run-ix");
    write(&f.root.join("src/mid.rs"), "pub fn before() {}\n");
    let ra = f.root.parent().unwrap().join("edit-once-ra.sh");
    let script = "#!/bin/sh\nif [ ! -e \"$2/../edited\" ]; then\n  touch \"$2/../edited\"\n  sleep 0.05\n  printf 'pub fn during() {}\\n' >> \"$2/src/mid.rs\"\nfi\nexit 1\n";
    fs::write(&ra, script).unwrap();
    fs::set_permissions(&ra, fs::Permissions::from_mode(0o755)).unwrap();
    let mut code = setup(&f);
    code.watch = false;
    code.rescan_interval = Duration::from_millis(20);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let ix = Indexer::start(code, f.db.clone(), Some(RustSource::Analyzer(ra)));
    assert!(ix.wait_reports(2, Duration::from_secs(30)), "{:?}", ix.status());
    checked(&ix);
    checked(&ix);
    assert_eq!(ix.reports().len(), 2, "{:?}", ix.status());
    let conn = atlas::db::Connection::open(&f.db).unwrap();
    let items = code::file_items(&conn, &f.root.join("src/mid.rs").to_string_lossy()).unwrap().expect("current");
    assert!(items.iter().any(|i| i.name == "during"), "{items:?}");
}

/// An edit made during a watching indexer's run is indexed as soon as the
/// run ends: its event, counted while the run was under way, wakes the next
/// pass. The safety-net walk is ten minutes off, so only that wake-up finds
/// the edit in time. A stand-in rust-analyzer makes the edit early in the
/// first run, then keeps the run going.
#[cfg(unix)]
#[test]
fn a_watching_indexer_indexes_an_edit_made_during_its_run_at_once() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("code-edit-in-run");
    write(&f.root.join("src/mid.rs"), "pub fn before() {}\n");
    let ra = f.root.parent().unwrap().join("edit-early-ra.sh");
    let script = "#!/bin/sh\nif [ ! -e \"$2/../edited\" ]; then\n  touch \"$2/../edited\"\n  printf 'pub fn during() {}\\n' >> \"$2/src/mid.rs\"\n  sleep 0.5\nfi\nexit 1\n";
    fs::write(&ra, script).unwrap();
    fs::set_permissions(&ra, fs::Permissions::from_mode(0o755)).unwrap();
    let mut code = setup(&f);
    code.watch = true;
    code.rescan_interval = Duration::from_secs(600);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let ix = Indexer::start(code, f.db.clone(), Some(RustSource::Analyzer(ra)));
    assert!(ix.wait_reports(2, Duration::from_secs(30)), "the edit waited for the safety-net walk: {:?}", ix.status());
    let conn = atlas::db::Connection::open(&f.db).unwrap();
    let items = code::file_items(&conn, &f.root.join("src/mid.rs").to_string_lossy()).unwrap().expect("current");
    assert!(items.iter().any(|i| i.name == "during"), "{items:?}");
}

/// A root whose stored fingerprint no longer matches its files, while the
/// files stay as they are, is indexed again by the running indexer. A file
/// edited during a run leaves this state (the stored fingerprint is the
/// walk's, the last one seen is the run's end), and so does another root's rpc
/// change, which clears the stored fingerprint. A full walk used to see the
/// files unchanged since its last look and schedule nothing.
#[test]
fn a_stale_but_stable_root_is_indexed_again() {
    let f = fixture("code-stale-stable");
    let mut code = setup(&f);
    code.watch = false;
    code.rescan_interval = Duration::from_millis(20);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let ix = Indexer::start(code.clone(), f.db.clone(), Some(RustSource::File(f.scip.clone())));
    assert!(ix.wait_reports(1, Duration::from_secs(60)), "{:?}", ix.status());
    let conn = atlas::db::Connection::open(&f.db).unwrap();
    conn.execute("UPDATE code_roots SET fingerprint = '' WHERE root = 'app'", []).unwrap();
    ix.check();
    assert!(ix.wait_reports(2, Duration::from_secs(20)), "the stale root was not indexed again: {:?}", ix.status());
}

/// Files in linked git worktrees of a root's repository. The index holds no row
/// for them: the walk never enters a dot folder such as Claude Code's
/// `.claude/worktrees/`, and a worktree beside the root is outside it. A read
/// there is annotated from a scan of the file's own bytes, and nothing is
/// written. The git layouts are the files `git worktree add` writes, made by
/// hand so that no git runs.
mod worktrees {
    use super::*;

    /// The index database's connection.
    type Db = atlas::db::Connection;

    fn db(f: &Fixture) -> Db {
        Db::open(&f.db).unwrap()
    }

    /// Indexes the fixture's root by the line scan.
    fn scan_index(f: &Fixture, code: &CodeSetup) {
        drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
        code_index::index_root(code, &mut db(f), &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    }

    /// The fixture's root as a repository's main checkout (`app/.git` a
    /// folder), indexed by the line scan.
    fn repo(tag: &str) -> (Fixture, Db) {
        let f = fixture(tag);
        fs::create_dir_all(f.root.join(".git")).unwrap();
        scan_index(&f, &setup(&f));
        let conn = db(&f);
        (f, conn)
    }

    /// Makes `top` a linked worktree, named `name`, of the repository whose
    /// git folder is `git`: `top/.git` names the admin folder
    /// `git/worktrees/NAME`, which holds `commondir` (`../..`) and `gitdir`,
    /// naming `top/.git` back.
    fn link_worktree(git: &Path, top: &Path, name: &str) {
        let admin = git.join("worktrees").join(name);
        fs::create_dir_all(&admin).unwrap();
        write(&top.join(".git"), &format!("gitdir: {}\n", admin.display()));
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(admin.join("gitdir"), format!("{}\n", top.join(".git").display())).unwrap();
    }

    /// A file's item labels, or `None` when it is not covered.
    fn labels(conn: &Db, path: &Path) -> Option<Vec<String>> {
        code::file_items(conn, &path.to_string_lossy()).unwrap().map(|v| v.iter().map(code::Item::label).collect())
    }

    fn some(labels: &[&str]) -> Option<Vec<String>> {
        Some(labels.iter().map(|l| l.to_string()).collect())
    }

    /// Every row the per-edit re-index can change, for comparing before and
    /// after.
    fn rows(conn: &Db) -> Vec<String> {
        let mut out = Vec::new();
        for sql in [
            "SELECT printf('%s|%s|%s|%s|%s', id, root, path, lang, indexed) FROM code_files ORDER BY 1",
            "SELECT printf('%s|%s|%s|%s|%s|%s|%s|%s|%s|%s', name, sect, file, line, col, ord, kind, container, within, end_line) FROM code_occ ORDER BY 1",
            "SELECT printf('%s|%s|%s', file, line, text) FROM code_lines ORDER BY 1",
            "SELECT printf('%s|%s|%s|%s|%s', name, root, defs, refs, impls) FROM code_counts ORDER BY 1",
        ] {
            let mut st = conn.prepare(sql).unwrap();
            out.extend(st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap));
        }
        out
    }

    /// A worktree under the root's dot folder, as Claude Code keeps them. A
    /// file whose item moved down on the worktree's branch is annotated at the
    /// worktree's lines. An edit there writes nothing: the checkout's rows stay
    /// as they were. `symbol` still answers the checkout's lines.
    #[test]
    fn a_worktree_file_gets_the_items_of_its_own_bytes() {
        let f = fixture("code-worktree");
        fs::create_dir_all(f.root.join(".git")).unwrap();
        write(&f.root.join("src/wt_probe.rs"), "pub fn wt_probe() -> u32 {\n    1\n}\n");
        let wt = f.root.join(".claude/worktrees/feature");
        link_worktree(&f.root.join(".git"), &wt, "feature");
        write(&wt.join("src/wt_probe.rs"), "// moved down on the worktree's branch\n\n\n\npub fn wt_probe() -> u32 {\n    2\n}\n");
        scan_index(&f, &setup(&f));
        let mut conn = db(&f);
        let main = f.root.join("src/wt_probe.rs");
        assert_eq!(labels(&conn, &main), some(&["fn wt_probe 1-3"]));
        let in_wt = wt.join("src/wt_probe.rs");
        assert_eq!(labels(&conn, &in_wt), some(&["fn wt_probe 5-7"]));
        let before = rows(&conn);
        assert_eq!(code::reindex_file(&mut conn, &in_wt.to_string_lossy()).unwrap(), code::Reindexed::NotCovered);
        assert_eq!(rows(&conn), before, "an edit in a worktree writes nothing");
        assert_eq!(labels(&conn, &main), some(&["fn wt_probe 1-3"]));
        let a = code::symbol(&conn, "wt_probe", Limits { defs: 5, refs: 5, impls: 5 }).unwrap();
        let defs: Vec<(String, u32)> = a.hits.iter().filter(|h| h.sect == code::SECT_DEF).map(|h| (h.path.clone(), h.line)).collect();
        assert_eq!(defs, [("src/wt_probe.rs".to_string(), 1)]);
    }

    /// An unchanged copy in a worktree gets the checkout's items, and a read's
    /// scan gives what the per-edit re-index writes for the same bytes: Rust,
    /// WGSL and a scanner language alike.
    #[test]
    fn a_worktree_copy_gets_the_items_the_checkout_has() {
        let f = fixture("code-wt-copy");
        fs::create_dir_all(f.root.join(".git")).unwrap();
        let wt = f.root.join(".claude/worktrees/w");
        link_worktree(&f.root.join(".git"), &wt, "w");
        let files = ["src/lib.rs", "src/shapes.rs", "shaders/common.wgsl", "shaders/main.wgsl"];
        for rel in files {
            write(&wt.join(rel), &fs::read_to_string(f.root.join(rel)).unwrap());
        }
        scan_index(&f, &setup(&f));
        let mut conn = db(&f);
        for rel in files {
            let main = labels(&conn, &f.root.join(rel));
            assert!(main.as_ref().is_some_and(|v| !v.is_empty()), "{rel}: {main:?}");
            assert_eq!(labels(&conn, &wt.join(rel)), main, "{rel}, against the full run's rows");
        }
        // A scanner language, written after the run: the re-index writes the
        // checkout's rows, and the worktree's copy is scanned on read.
        let py = "class Tool:\n    def run(self):\n        return 1\n\n\ndef helper():\n    return 2\n";
        write(&f.root.join("src/tool.py"), py);
        write(&wt.join("src/tool.py"), py);
        for rel in ["src/lib.rs", "shaders/common.wgsl", "src/tool.py"] {
            assert!(matches!(code::reindex_file(&mut conn, &f.root.join(rel).to_string_lossy()).unwrap(), code::Reindexed::Rows(_)), "{rel}");
            let main = labels(&conn, &f.root.join(rel));
            assert!(main.as_ref().is_some_and(|v| !v.is_empty()), "{rel}: {main:?}");
            assert_eq!(labels(&conn, &wt.join(rel)), main, "{rel}, against the re-index's rows");
        }
    }

    /// A worktree moved with a plain `mv` keeps an admin folder that names its
    /// old place: it is not mapped until `git worktree repair` writes the
    /// back-link again.
    #[test]
    fn a_worktree_whose_admin_folder_does_not_name_it_back_is_not_mapped() {
        let (f, conn) = repo("code-wt-moved");
        let t = f.root.parent().unwrap();
        let wt = t.join("moved");
        link_worktree(&f.root.join(".git"), &wt, "w");
        write(&wt.join("src/lib.rs"), LIB);
        let back = f.root.join(".git/worktrees/w/gitdir");
        fs::write(&back, format!("{}\n", t.join("before-the-move/.git").display())).unwrap();
        assert_eq!(labels(&conn, &wt.join("src/lib.rs")), None);
        fs::write(&back, format!("{}\n", wt.join(".git").display())).unwrap();
        assert!(labels(&conn, &wt.join("src/lib.rs")).is_some(), "repaired");
    }

    /// A submodule's `.git` file names a folder of the superproject's git
    /// folder that holds no `commondir`: another repository, not mapped.
    #[test]
    fn a_submodule_style_git_file_is_not_mapped() {
        let (f, conn) = repo("code-wt-submodule");
        let sub = f.root.parent().unwrap().join("vendored");
        let modules = f.root.join(".git/modules/vendored");
        fs::create_dir_all(&modules).unwrap();
        write(&sub.join(".git"), &format!("gitdir: {}\n", modules.display()));
        write(&sub.join("src/lib.rs"), LIB);
        assert_eq!(labels(&conn, &sub.join("src/lib.rs")), None);
    }

    /// A worktree beside the root, outside its folder, is mapped, and so is a
    /// file added on its branch. A file under the worktree's `target/` is not,
    /// as in the checkout.
    #[test]
    fn a_worktree_beside_the_root_is_mapped_with_its_added_files() {
        let (f, conn) = repo("code-wt-beside");
        let wt = f.root.parent().unwrap().join("app-feature");
        link_worktree(&f.root.join(".git"), &wt, "app-feature");
        write(&wt.join("src/lib.rs"), LIB);
        let main = labels(&conn, &f.root.join("src/lib.rs")).expect("indexed");
        assert_eq!(labels(&conn, &wt.join("src/lib.rs")), Some(main));
        write(&wt.join("src/added.rs"), "pub struct Added;\n\nimpl Added {\n    pub fn new() -> Added {\n        Added\n    }\n}\n");
        assert_eq!(labels(&conn, &f.root.join("src/added.rs")), None, "not in the checkout");
        assert_eq!(labels(&conn, &wt.join("src/added.rs")), some(&["struct Added 1-1", "impl Added 3-7", "fn Added::new 4-6"]));
        write(&wt.join("target/debug/build/gen.rs"), "pub fn generated() {}\n");
        assert_eq!(labels(&conn, &wt.join("target/debug/build/gen.rs")), None, "the walk skips target/");
    }

    /// A root below its repository's top maps with its folder as the prefix:
    /// the worktree's `app/src/lib.rs` is the root's `src/lib.rs`, and a
    /// worktree file outside `app/` belongs to no root.
    #[test]
    fn a_root_below_its_repository_top_maps_with_its_prefix() {
        let f = fixture("code-wt-prefix");
        let top = f.root.parent().unwrap().to_path_buf();
        fs::create_dir_all(top.join(".git")).unwrap();
        let wt = top.join(".claude/worktrees/w");
        link_worktree(&top.join(".git"), &wt, "w");
        write(&wt.join("app/src/lib.rs"), LIB);
        write(&wt.join("src/lib.rs"), LIB);
        scan_index(&f, &setup(&f));
        let conn = db(&f);
        let main = labels(&conn, &f.root.join("src/lib.rs")).expect("indexed");
        assert_eq!(labels(&conn, &wt.join("app/src/lib.rs")), Some(main));
        assert_eq!(labels(&conn, &wt.join("src/lib.rs")), None, "outside the root's folder");
    }

    /// A bare repository with worktrees: the root is itself a linked worktree
    /// of `.bare` and keeps its annotations from its rows, and another
    /// worktree of `.bare` maps to it by the common git folder.
    #[test]
    fn worktrees_of_a_bare_repository_map_to_a_root_that_is_one() {
        let f = fixture("code-wt-bare");
        let t = f.root.parent().unwrap();
        let bare = t.join(".bare");
        link_worktree(&bare, &f.root, "app");
        let feat = t.join("feat");
        link_worktree(&bare, &feat, "feat");
        write(&feat.join("src/shapes.rs"), SHAPES);
        scan_index(&f, &setup(&f));
        let conn = db(&f);
        // A mark only the rows carry shows that the rows are served.
        conn.execute("UPDATE code_occ SET container = 'from_rows' WHERE sect = 0 AND file = (SELECT id FROM code_files WHERE path = 'src/shapes.rs')", []).unwrap();
        assert_eq!(labels(&conn, &f.root.join("src/shapes.rs")), some(&["fn from_rows::area 1-1"]));
        assert_eq!(labels(&conn, &feat.join("src/shapes.rs")), some(&["fn area 1-1"]));
    }

    /// `git init --separate-git-dir`: the root's own `.git` is a file naming a
    /// git folder elsewhere, which holds no `commondir`. A worktree of that
    /// repository maps to the root.
    #[test]
    fn a_worktree_of_a_separate_git_dir_repository_is_mapped() {
        let f = fixture("code-wt-separate");
        let t = f.root.parent().unwrap();
        let gd = t.join("app-gitdir");
        fs::create_dir_all(&gd).unwrap();
        write(&f.root.join(".git"), &format!("gitdir: {}\n", gd.display()));
        let wt = t.join("app-wt");
        link_worktree(&gd, &wt, "app-wt");
        write(&wt.join("src/lib.rs"), LIB);
        scan_index(&f, &setup(&f));
        let conn = db(&f);
        let main = labels(&conn, &f.root.join("src/lib.rs")).expect("indexed");
        assert_eq!(labels(&conn, &wt.join("src/lib.rs")), Some(main));
    }

    /// `git worktree add --relative-paths`: both links are relative, the
    /// admin's back-link to the admin folder, so they are resolved and compared
    /// canonically, as is a path read through a link.
    #[test]
    fn relative_worktree_links_map() {
        let (f, conn) = repo("code-wt-relative");
        let wt = f.root.join(".claude/worktrees/rel");
        let admin = f.root.join(".git/worktrees/rel");
        write(&wt.join(".git"), "gitdir: ../../../.git/worktrees/rel\n");
        write(&admin.join("commondir"), "../..\n");
        write(&admin.join("gitdir"), "../../../.claude/worktrees/rel/.git\n");
        write(&wt.join("src/lib.rs"), LIB);
        let main = labels(&conn, &f.root.join("src/lib.rs")).expect("indexed");
        assert_eq!(labels(&conn, &wt.join("src/lib.rs")), Some(main.clone()));
        #[cfg(unix)]
        {
            let link = f.root.parent().unwrap().join("rel-link");
            std::os::unix::fs::symlink(&wt, &link).unwrap();
            assert_eq!(labels(&conn, &link.join("src/lib.rs")), Some(main));
        }
    }

    /// A worktree file over the index's size limit gets no items, as in the
    /// checkout: it is read through the same check.
    #[test]
    fn a_worktree_file_over_the_size_limit_gets_no_items() {
        let (f, conn) = repo("code-wt-big");
        let wt = f.root.join(".claude/worktrees/w");
        link_worktree(&f.root.join(".git"), &wt, "w");
        let filler = "// filler\n".repeat(code::MAX_FILE as usize / 10 + 1);
        write(&wt.join("src/big.rs"), &format!("pub fn big() {{}}\n{filler}"));
        assert_eq!(labels(&conn, &wt.join("src/big.rs")), None);
        write(&wt.join("src/small.rs"), "pub fn small() {}\n");
        assert_eq!(labels(&conn, &wt.join("src/small.rs")), some(&["fn small 1-1"]));
    }

    /// A root's `exclude` globs hold in its worktrees too: a worktree file they
    /// cover gets no items, as the checkout's own copy gets none.
    #[test]
    fn a_worktree_file_the_roots_excludes_cover_gets_no_items() {
        let f = fixture("code-wt-exclude");
        fs::create_dir_all(f.root.join(".git")).unwrap();
        write(&f.root.join("src/gen/made.rs"), "pub fn made() {}\n");
        let mut code = setup(&f);
        code.roots[0].exclude = vec!["gen".into()];
        scan_index(&f, &code);
        let conn = db(&f);
        assert_eq!(labels(&conn, &f.root.join("src/gen/made.rs")), None, "excluded in the checkout");
        let wt = f.root.join(".claude/worktrees/w");
        link_worktree(&f.root.join(".git"), &wt, "w");
        write(&wt.join("src/gen/made.rs"), "pub fn made() {}\n");
        assert_eq!(labels(&conn, &wt.join("src/gen/made.rs")), None, "excluded in the worktree");
        write(&wt.join("src/kept.rs"), "pub fn kept() {}\n");
        assert_eq!(labels(&conn, &wt.join("src/kept.rs")), some(&["fn kept 1-1"]));
    }

    /// The checkout's own files are never mapped: a file the index holds no
    /// row for stays unannotated, as before worktrees were covered. A database
    /// without the code tables answers nothing.
    #[test]
    fn the_checkouts_own_unindexed_files_stay_unannotated() {
        let (f, conn) = repo("code-wt-own");
        write(&f.root.join("src/new.rs"), "pub fn new_here() {}\n");
        assert_eq!(labels(&conn, &f.root.join("src/new.rs")), None);
        let wt = f.root.join(".claude/worktrees/w");
        link_worktree(&f.root.join(".git"), &wt, "w");
        write(&wt.join("src/lib.rs"), LIB);
        let empty = Db::open(f.root.parent().unwrap().join("empty.sqlite")).unwrap();
        assert_eq!(labels(&empty, &wt.join("src/lib.rs")), None);
    }

    /// Two roots that are checkouts of one repository, here two worktrees of
    /// a bare one. A file with no row in either root's own checkout stays
    /// unannotated, though the other root would map it. A third worktree's
    /// file maps.
    #[test]
    fn a_file_in_any_roots_own_checkout_is_never_mapped() {
        let t = Temp::new("code-wt-two-roots");
        let bare = t.path().join(".bare");
        let (a, b, c) = (t.path().join("a"), t.path().join("b"), t.path().join("c"));
        for (top, name) in [(&a, "a"), (&b, "b"), (&c, "c")] {
            link_worktree(&bare, top, name);
            write(&top.join("src/lib.rs"), LIB);
        }
        let db = t.path().join("state/atlas.sqlite");
        let root = |name: &str, dir: &Path| atlas::config::CodeRootSpec { name: name.into(), root: dir.to_path_buf(), build_scripts: false, scip: Default::default(), exclude: Vec::new() };
        let spec = atlas::config::CodeSpec { roots: vec![root("a", &a), root("b", &b)], debounce_s: Some(0), ..Default::default() };
        let code = CodeSetup::from_spec(&spec, &db, false);
        drop(atlas::store::Store::open(&db, vaults(&db)).unwrap());
        let mut conn = Db::open(&db).unwrap();
        for r in &code.roots {
            code_index::index_root(&code, &mut conn, r, &RustSource::ScanOnly("test".into())).unwrap();
        }
        write(&b.join("src/new.rs"), "pub fn new_here() {}\n");
        assert_eq!(labels(&conn, &b.join("src/new.rs")), None, "b's own checkout");
        assert!(labels(&conn, &c.join("src/lib.rs")).is_some(), "a third worktree maps");
    }

    /// The bytes scanned are the given path's, the file the tool read, even
    /// when the canonical-path cache still names a link's old target.
    #[cfg(unix)]
    #[test]
    fn a_link_moved_since_it_was_cached_scans_the_file_it_names_now() {
        let (f, conn) = repo("code-wt-link-moved");
        let t = f.root.parent().unwrap();
        let (one, two) = (t.join("one"), t.join("two"));
        link_worktree(&f.root.join(".git"), &one, "one");
        link_worktree(&f.root.join(".git"), &two, "two");
        write(&one.join("src/moved.rs"), "pub fn moved() {}\n");
        write(&two.join("src/moved.rs"), "\n\npub fn moved() {}\n");
        let link = t.join("link");
        std::os::unix::fs::symlink(&one, &link).unwrap();
        assert_eq!(labels(&conn, &link.join("src/moved.rs")), some(&["fn moved 1-1"]));
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&two, &link).unwrap();
        assert_eq!(labels(&conn, &link.join("src/moved.rs")), some(&["fn moved 3-3"]));
    }

    /// Which repository a file is in comes from the path as it resolves now,
    /// not from the canonical-path cache: a link moved to another
    /// repository's checkout maps to no root, rather than annotating that
    /// repository's bytes under the root its old target mapped to.
    #[cfg(unix)]
    #[test]
    fn a_link_moved_to_another_repository_maps_to_no_root() {
        let (f, conn) = repo("code-wt-link-repo");
        let t = f.root.parent().unwrap();
        let (one, other) = (t.join("one"), t.join("other"));
        link_worktree(&f.root.join(".git"), &one, "one");
        fs::create_dir_all(other.join(".git")).unwrap();
        write(&one.join("src/moved.rs"), "pub fn moved() {}\n");
        write(&other.join("src/moved.rs"), "pub fn elsewhere() {}\n");
        let link = t.join("link");
        std::os::unix::fs::symlink(&one, &link).unwrap();
        assert_eq!(labels(&conn, &link.join("src/moved.rs")), some(&["fn moved 1-1"]));
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_eq!(labels(&conn, &link.join("src/moved.rs")), None, "another repository's file");
    }
}

/// The line of `text` (1-based) that is exactly `want` after trimming its
/// end, at or after `from`.
fn line_of(text: &str, want: &str, from: usize) -> u32 {
    text.lines().enumerate().skip(from.saturating_sub(1)).find(|(_, l)| l.trim_end() == want).map(|(i, _)| i as u32 + 1).unwrap_or_else(|| panic!("no line {want:?}"))
}

#[test]
fn impl_blocks_are_items_so_methods_and_gaps_between_them_annotate() {
    // A real file shape: this crate's own router, with an inherent impl, a
    // trait impl, doc comments between methods and a test module.
    let src = include_str!("../src/hook_rewrite.rs");
    let f = fixture("code-impls");
    write(&f.root.join("src/router.rs"), src);
    let code = setup(&f);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let mut conn = atlas::db::Connection::open(&f.db).unwrap();
    code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    let path = f.root.join("src/router.rs");
    let items = code::file_items(&conn, &path.to_string_lossy()).unwrap().expect("covered");
    let imp = line_of(src, "impl Router {", 1);
    let imp_end = line_of(src, "}", imp as usize);
    let dbg = line_of(src, "impl std::fmt::Debug for Router {", 1);
    let post = line_of(src, "    pub fn post_tool_use(&mut self, ev: &HookEvent) -> Result<Option<Value>> {", imp as usize);
    let post_end = line_of(src, "    }", post as usize);
    let label = |l: u32| code::enclosing(&items, l).map(|i| i.label()).unwrap_or_default();
    // The impl line itself, and a doc comment between two methods.
    assert_eq!(label(imp), format!("impl Router {imp}-{imp_end}"));
    assert_eq!(label(post - 2), format!("impl Router {imp}-{imp_end}"));
    // Inside a method: the method, named with its type.
    assert_eq!(label(post + 3), format!("fn Router::post_tool_use {post}-{post_end}"));
    assert_eq!(label(dbg + 1), format!("fn Router::fmt {}-{}", dbg + 1, dbg + 3));
    assert!(label(dbg).starts_with("impl Debug for Router "), "{}", label(dbg));
    // Free functions keep no container.
    let rel = line_of(src, "pub fn relative(path: &str, cwd: &str) -> String {", 1);
    assert!(label(rel + 1).starts_with("fn relative "), "{}", label(rel + 1));
    // `symbol` finds the method by Type::name, and the impls with ranges.
    let mut s = server(&f, code, None);
    let v = s.call("symbol", &json!({"name": "Router::post_tool_use"})).unwrap();
    assert_eq!(rows(&v, "defs"), [("src/router.rs".into(), post as u64, "fn (scan)".into(), "Router".into())]);
    assert_eq!(v["defs"][0]["end_line"].as_u64(), Some(post_end as u64));
    let v = s.call("symbol", &json!({"name": "Router"})).unwrap();
    let impls: Vec<String> = v["impls"].as_array().unwrap().iter().map(|h| format!("{}-{} {}", h["line"], h["end_line"], h["container"].as_str().unwrap())).collect();
    assert_eq!(impls, [format!("{dbg}-{} Debug for Router", dbg + 4), format!("{imp}-{imp_end} Router")]);
}
#[test]
fn shell_reads_are_annotated_like_grep_and_read() {
    let f = fixture("code-shell");
    let src = include_str!("../src/hook_rewrite.rs");
    write(&f.root.join("src/router.rs"), src);
    let code = setup(&f);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let mut conn = atlas::db::Connection::open(&f.db).unwrap();
    code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    let post = line_of(src, "    pub fn post_tool_use(&mut self, ev: &HookEvent) -> Result<Option<Value>> {", 1);
    let post_end = line_of(src, "    }", post as usize);
    let imp = line_of(src, "impl Router {", 1);
    let imp_end = line_of(src, "}", imp as usize);
    let cwd = f.root.to_string_lossy().replace('\\', "/");
    let mut router = atlas::hook_rewrite::Router::with_db(&f.db, None);
    let bash = |cmd: &str, stdout: &str| {
        atlas::hooks::HookEvent::from_value(&json!({
            "hook_event_name": "PostToolUse", "session_id": "s", "tool_name": "Bash", "cwd": cwd,
            "tool_input": {"command": cmd},
            "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}
        }))
    };
    let lines: Vec<&str> = src.lines().collect();
    // grep -n over one file: `line:text`, each match gets its item; the
    // exit tail the PreToolUse hook added is read through.
    let hit = post + 3;
    let out = format!("{hit}:{}\n{}:{}", lines[hit as usize - 1], post - 2, lines[post as usize - 3]);
    let (u, note) = router.annotate(&bash("grep -n self src/router.rs || echo atlas-exit-nonzero", &out)).unwrap();
    let u = u.expect("annotated");
    let got = u["stdout"].as_str().unwrap();
    assert_eq!(got, format!("{hit}:{} [fn Router::post_tool_use {post}-{post_end}]\n{}:{} [impl Router {imp}-{imp_end}]", lines[hit as usize - 1], post - 2, lines[post as usize - 3]));
    assert!(note.is_empty());
    // The rest of the Bash shape is kept.
    assert_eq!(u["interrupted"].as_bool(), Some(false));
    assert_eq!(u["isImage"].as_bool(), Some(false));
    // grep -rn over a folder: `path:line:text`.
    let out = format!("src/router.rs:{hit}:{}", lines[hit as usize - 1]);
    let (u, _) = router.annotate(&bash("grep -rn self src", &out)).unwrap();
    assert!(u.unwrap()["stdout"].as_str().unwrap().ends_with(&format!(" [fn Router::post_tool_use {post}-{post_end}]")));
    // A sed window that cuts post_tool_use: the Read note, stdout untouched.
    let (a, b) = (post + 2, post + 6);
    let window = lines[a as usize - 1..b as usize].join("\n");
    let (u, note) = router.annotate(&bash(&format!("sed -n '{a},{b}p' src/router.rs"), &window)).unwrap();
    assert!(u.is_none());
    assert_eq!(note, format!("atlas: src/router.rs lines {a}-{b} cut [impl Router {imp}-{imp_end}] [fn Router::post_tool_use {post}-{post_end}]"));
    // A window past the end is cut to what was printed.
    let (_, note) = router.annotate(&bash(&format!("sed -n '{a},99999p' src/router.rs"), &window)).unwrap();
    assert!(note.starts_with(&format!("atlas: src/router.rs lines {a}-{b} cut")), "{note}");
    // A batch of reads split by echo lines: each part is annotated as its own.
    let batch = format!("{window}\n----\n{hit}:{}", lines[hit as usize - 1]);
    let (u, note) = router.annotate(&bash(&format!("sed -n '{a},{b}p' src/router.rs; echo ----; grep -n self src/router.rs"), &batch)).unwrap();
    assert!(note.starts_with(&format!("atlas: src/router.rs lines {a}-{b} cut")), "{note}");
    let got = u.expect("grep part annotated")["stdout"].as_str().unwrap().to_string();
    assert_eq!(got, format!("{window}\n----\n{hit}:{} [fn Router::post_tool_use {post}-{post_end}]", lines[hit as usize - 1]));
    // Anything else passes: a plain cat, a chained command, no output.
    for (cmd, out) in [("cat src/router.rs", window.as_str()), ("grep -n self src/router.rs; ls", "1:x"), ("grep -n nothing src/router.rs", "")] {
        let (u, note) = router.annotate(&bash(cmd, out)).unwrap();
        assert!(u.is_none() && note.is_empty(), "{cmd}");
    }
}
/// A routed grep that printed nothing used to panic the hook (and, once the
/// hooks thread had died, the server's main thread): its segment holds no
/// lines, but `annotate_grep("")` returns one empty line, written back one
/// past the end of the output. Every shape of output keeps its line count and
/// its separators, and nothing panics.
#[test]
fn an_empty_grep_part_of_a_batch_never_panics_or_eats_a_line() {
    let f = fixture("code-shell-empty");
    write(&f.root.join("src/a.rs"), "fn a() {\n    let x = 1;\n}\n");
    let _code = setup(&f);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let cwd = f.root.to_string_lossy().replace('\\', "/");
    let mut router = atlas::hook_rewrite::Router::with_db(&f.db, None);
    let bash = |cmd: &str, stdout: &str| {
        atlas::hooks::HookEvent::from_value(&json!({
            "hook_event_name": "PostToolUse", "session_id": "s", "tool_name": "Bash", "cwd": cwd,
            "tool_input": {"command": cmd},
            "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}
        }))
    };
    let cases: &[(&str, &str)] = &[
        // The last grep matched nothing and Claude Code trimmed the newline:
        // the output ends on the echo line.
        ("grep -n x src/a.rs; echo ----; grep -n nothing src/a.rs", "2:    let x = 1;\n----"),
        ("grep -n x src/a.rs; echo ----; grep -rn nothing src | head -5", "2:    let x = 1;\n----"),
        ("grep -n x src/a.rs; echo ----; grep -rn nothing src | cut -c1-200", "src/a.rs:2:    let x = 1;\r\n----"),
        // CRLF output, a trailing newline, an empty last line.
        ("grep -n x src/a.rs; echo ----; grep -n nothing src/a.rs", "2:    let x = 1;\r\n----\r\n"),
        ("grep -rn x src; echo ----; grep -rn nothing src", "src/a.rs:2:    let x = 1;\n----\n"),
        ("grep -rn x src; echo ----; grep -rn y src", "src/a.rs:2:    let x = 1;\r\n----\r\nsrc/a.rs:1:fn a() {\r\n\r\n"),
        // An empty grep between two separators keeps the second separator.
        ("grep -n x src/a.rs; echo ----; grep -n nothing src/a.rs; echo ====; grep -rn x src", "2:    let x = 1;\n----\n====\nsrc/a.rs:2:    let x = 1;"),
    ];
    for (cmd, out) in cases {
        let (u, _) = router.annotate(&bash(cmd, out)).unwrap();
        let got = u.map_or_else(|| out.to_string(), |u| u["stdout"].as_str().unwrap().to_string());
        assert_eq!(got.split('\n').count(), out.split('\n').count(), "{cmd}: {got:?}");
        for sep in ["----", "===="] {
            assert_eq!(got.matches(sep).count(), out.matches(sep).count(), "{cmd}: {got:?}");
        }
    }
}
/// Until a root's first walk since start finishes, its state is Starting
/// (symbol says "indexing"), never the "stale ... reindex pending" of a root
/// that changed after it was indexed.
#[test]
fn a_root_reads_as_indexing_until_its_first_walk_finishes() {
    let f = fixture("code-starting");
    let code = setup(&f);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let ix = Indexer::start(code.clone(), f.db.clone(), Some(RustSource::File(f.scip.clone())));
    let mut seen = Vec::new();
    let t0 = std::time::Instant::now();
    loop {
        let st = ix.status().get("app").cloned();
        if seen.last() != Some(&st) {
            seen.push(st.clone());
        }
        if st == Some(code_index::RootState::Idle) || t0.elapsed() > Duration::from_secs(60) {
            break;
        }
    }
    assert_eq!(seen.first(), Some(&Some(code_index::RootState::Starting)), "{seen:?}");
    assert!(seen.iter().all(|s| matches!(s, Some(code_index::RootState::Starting | code_index::RootState::Idle))), "{seen:?}");
    let mut s = server(&f, code, Some(ix));
    let t = s.answer("symbol", &json!({"name": "helper"})).unwrap().into_text();
    assert!(!t.contains("stale app"), "{t}");
}

#[test]
fn code_index_stats_print_as_words_not_debug_structs() {
    let rows = atlas::code::RowStats { scip_defs: 3, impls: 1, ..Default::default() };
    assert_eq!(rows.to_string(), "3 scip defs, 1 impls");
    assert_eq!(atlas::code::RowStats::default().to_string(), "none");
    let w = atlas::code::WriteStats { files: 2, defs: 3, refs: 4, impls: 1, lines: 9 };
    assert_eq!(w.to_string(), "2 files, 3 defs, 4 refs, 1 impls, 9 lines");
}
