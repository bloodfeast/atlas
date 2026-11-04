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
    let spec = atlas::config::CodeSpec { roots: vec![atlas::config::CodeRootSpec { name: "app".into(), root: f.root.clone(), build_scripts: false, scip: Default::default() }], debounce_s: Some(0), ..Default::default() };
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
    let mut conn = rusqlite::Connection::open(&f.db).unwrap();
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
    let conn = rusqlite::Connection::open(&f.db).unwrap();
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
    let mut conn = rusqlite::Connection::open(&f.db).unwrap();
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
    let mut conn = rusqlite::Connection::open(&f.db).unwrap();
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
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let a = code::symbol(&conn, "Store", Limits { defs: 1, refs: 1, impls: 1 }).unwrap();
    assert_eq!(a.totals, [1, 2, 2]);
    assert_eq!(a.hits.iter().map(|h| h.sect).collect::<Vec<_>>(), [0, 1, 2]);
    let t = code::render(&a, Limits { defs: 1, refs: 1, impls: 1 });
    assert!(t.contains("+1 more (limit=1)\nimpls 2"), "{t}");
    let none = code::symbol(&conn, "Nothing", Limits { defs: 5, refs: 5, impls: 5 }).unwrap();
    assert_eq!(none.totals, [0, 0, 0]);
    // A database the code index never touched answers empty.
    let t = Temp::new("code-empty");
    let empty = rusqlite::Connection::open(t.path().join("e.sqlite")).unwrap();
    assert!(code::symbol(&empty, "Store", Limits { defs: 1, refs: 1, impls: 1 }).unwrap().hits.is_empty());
}

#[test]
fn code_config_is_read_and_unconfigured_symbol_is_refused() {
    let t = Temp::new("code-config");
    let p = t.path().join("atlas.json");
    fs::write(&p, r#"{"db": "s/a.sqlite", "code": {"roots": [{"name": "app", "root": "app"}], "debounce_s": 5}}"#).unwrap();
    let c = atlas::config::Config::load(&p).unwrap();
    let setup = CodeSetup::from_spec(c.code.as_ref().unwrap(), c.db.as_ref().unwrap(), true);
    assert_eq!(setup.roots, [CodeRoot { name: "app".into(), root: t.path().join("app"), build_scripts: false, scip: Default::default() }]);
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
    let mut conn = rusqlite::Connection::open(&f.db).unwrap();
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
    let grep = format!("src/multi.rs:7:        let n = self.n;\nsrc/multi.rs-8-        n + 1\nsrc/nothere.rs:1:x");
    let out = atlas::hook_rewrite::annotate_grep(&grep, &f.root.to_string_lossy(), None, &mut |p| code::file_items(&conn, p)).unwrap();
    assert_eq!(out, "src/multi.rs:7:        let n = self.n; [fn Cache::get 6-9]\nsrc/multi.rs-8-        n + 1\nsrc/nothere.rs:1:x");
    // A file outside every root, and a file written after the indexing.
    assert!(code::file_items(&conn, &f.db.to_string_lossy()).unwrap().is_none());
    let later = std::time::SystemTime::now() + Duration::from_secs(60);
    fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
    assert!(code::file_items(&conn, &path.to_string_lossy()).unwrap().is_none());
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
    let mut conn = rusqlite::Connection::open(&f.db).unwrap();
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
    let mut conn = rusqlite::Connection::open(&f.db).unwrap();
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
