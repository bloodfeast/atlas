//! References across code roots: a Go service whose module uses a sibling
//! root's module (a shared library with generated gRPC stubs), each root
//! indexed from a synthetic prebuilt SCIP file. The service's references
//! into the library's package are stored with the service's rows, so
//! `symbol` answers the library's definition with the service's callers,
//! and a call through the library's stub is an rpc call site. Each root is
//! indexed on its own; indexing the library after the service marks the
//! service for re-indexing. No indexer runs.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use atlas::code::{CodeSetup, Indexers};
use atlas::code_index::{self, IndexReport, RustSource};
use atlas::json;
use atlas::json::Value;
use atlas::protobuf::Writer;
use atlas::tools::{CodeState, Server};
use common::*;

const PROTO: &str = "syntax = \"proto3\";
package shop;

service CartService {
  rpc GetCart(GetCartRequest) returns (Cart);
}

message GetCartRequest {}
message Cart {}
";

const GEN_GO: &str = "package genproto

type CartServiceClient interface {
\tGetCart(ctx context.Context, in *GetCartRequest) (*Cart, error)
}
";

const STATS_GO: &str = "package stats

func Mean(xs []float64) float64 { return 0 }
";

const MAIN_GO: &str = "package main

import (
\t\"example.com/lib/stats\"
\tpb \"example.com/lib/genproto\"
\t\"github.com/other/dep\"
)

func main() {
\t_ = stats.Mean(nil)
\tvar c pb.CartServiceClient
\tc.GetCart(nil, nil)
\tdep.Do()
}
";

fn occ(w: &mut Writer, range: &[i32], sym: &str, roles: i64) {
    let mut o = Writer::default();
    o.packed(1, range).string(2, sym);
    if roles != 0 {
        o.int(3, roles);
    }
    w.message(2, &o);
}

fn index(docs: &[&Writer]) -> Vec<u8> {
    let mut tool = Writer::default();
    tool.string(1, "scip-go").string(2, "0.2.9");
    let mut meta = Writer::default();
    meta.message(2, &tool);
    let mut ix = Writer::default();
    ix.message(1, &meta);
    for d in docs {
        ix.message(2, d);
    }
    ix.buf
}

/// The library's index: its module is `example.com/lib`, at a commit.
fn lib_index() -> Vec<u8> {
    let lib = |x: &str| format!("scip-go gomod example.com/lib 0f09c3a9 {x}");
    let mut s = Writer::default();
    s.string(1, "stats/stats.go").string(4, "go");
    occ(&mut s, &[2, 5, 9], &lib("`example.com/lib/stats`/Mean()."), 1);
    let mut g = Writer::default();
    g.string(1, "genproto/demo_grpc.pb.go").string(4, "go");
    occ(&mut g, &[2, 5, 22], &lib("`example.com/lib/genproto`/CartServiceClient#"), 1);
    occ(&mut g, &[3, 1, 8], &lib("`example.com/lib/genproto`/CartServiceClient#GetCart()."), 1);
    index(&[&s, &g])
}

/// The service's index: the library is a replaced module (version `.`),
/// and a third-party module no root defines.
fn svc_index() -> Vec<u8> {
    let lib = |x: &str| format!("scip-go gomod example.com/lib . {x}");
    let mut m = Writer::default();
    m.string(1, "main.go").string(4, "go");
    occ(&mut m, &[8, 5, 9], "scip-go gomod example.com/svc v0 `example.com/svc`/main().", 1);
    occ(&mut m, &[9, 11, 15], &lib("`example.com/lib/stats`/Mean()."), 0);
    occ(&mut m, &[10, 10, 27], &lib("`example.com/lib/genproto`/CartServiceClient#"), 0);
    occ(&mut m, &[11, 3, 10], &lib("`example.com/lib/genproto`/CartServiceClient#GetCart()."), 0);
    occ(&mut m, &[12, 5, 7], "scip-go gomod github.com/other/dep v1.0.0 `github.com/other/dep`/Do().", 0);
    index(&[&m])
}

fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

/// Writes both roots and returns the setup (roots: `svc`, `lib`) and the
/// database path.
fn setup(t: &Temp) -> (CodeSetup, PathBuf) {
    let lib = t.path().join("lib");
    write(&lib.join("go.mod"), "module example.com/lib\n\ngo 1.22\n");
    write(&lib.join("protos/cart.proto"), PROTO);
    write(&lib.join("stats/stats.go"), STATS_GO);
    write(&lib.join("genproto/demo_grpc.pb.go"), GEN_GO);
    fs::write(lib.join("index.scip"), lib_index()).unwrap();
    let svc = t.path().join("svc");
    write(&svc.join("go.mod"), "module example.com/svc\n\ngo 1.22\n\nrequire example.com/lib v0.0.0\n\nreplace example.com/lib => ../lib\n");
    write(&svc.join("main.go"), MAIN_GO);
    fs::write(svc.join("index.scip"), svc_index()).unwrap();
    let db = t.path().join("state/atlas.sqlite");
    let root = |name: &str, root: PathBuf| atlas::config::CodeRootSpec { name: name.into(), root, build_scripts: false, scip: Default::default() };
    let spec = atlas::config::CodeSpec { roots: vec![root("svc", svc), root("lib", lib)], debounce_s: Some(0), ..Default::default() };
    let mut code = CodeSetup::from_spec(&spec, &db, false);
    code.defer_on_cargo = false;
    code.indexers = Indexers::Off("off in tests".into());
    drop(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    (code, db)
}

fn index_one(code: &CodeSetup, db: &Path, i: usize) -> IndexReport {
    let mut conn = rusqlite::Connection::open(db).unwrap();
    code_index::index_root(code, &mut conn, &code.roots[i], &RustSource::ScanOnly("test".into())).unwrap()
}

fn fingerprint(db: &Path, root: &str) -> String {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row("SELECT fingerprint FROM code_roots WHERE root = ?1", [root], |r| r.get(0)).unwrap()
}

/// `root:path:line kind within` of a section's rows (`container` for defs).
fn rows(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let what = if key == "refs" { &h["within"] } else { &h["container"] };
            format!("{}:{}:{} {} {}", h["root"].as_str().unwrap(), h["path"].as_str().unwrap(), h["line"], h["kind"].as_str().unwrap(), what.as_str().unwrap())
        })
        .collect()
}

#[test]
fn a_reference_into_a_sibling_roots_package_is_stored_with_its_caller() {
    let t = Temp::new("code-xroot");
    let (code, db) = setup(&t);
    // The service first: the library's package is not known yet, so its
    // references into it are dropped like any dependency's, and the
    // packages it uses are recorded.
    let rep = index_one(&code, &db, 0);
    assert_eq!((rep.rows.xroot_refs, rep.rows.external_refs), (0, 4), "{:?}", rep.rows);
    // The library: it defines a package the service uses, so the service
    // is marked for re-indexing; the library needed nothing of it.
    let rep = index_one(&code, &db, 1);
    assert_eq!(rep.rows.xroot_refs, 0, "{:?}", rep.rows);
    assert_eq!(fingerprint(&db, "svc"), "", "the service is stale");
    assert_ne!(fingerprint(&db, "lib"), "");
    // The service again, alone: its references into the library are
    // stored (the version differs: a replaced module is `.`); the
    // third-party module's reference is still dropped.
    let rep = index_one(&code, &db, 0);
    assert_eq!((rep.rows.xroot_refs, rep.rows.external_refs), (3, 1), "{:?}", rep.rows);
    assert_eq!(rep.rows.rpc_calls, 1, "the call through the library's stub is a call site: {:?}", rep.rows);
    assert_ne!(fingerprint(&db, "svc"), "");
    let lib_fp = fingerprint(&db, "lib");
    // The library again: its packages did not change, so nothing else is
    // marked.
    index_one(&code, &db, 1);
    assert_ne!(fingerprint(&db, "svc"), "", "an unchanged package marks no user");
    assert_eq!(fingerprint(&db, "lib"), lib_fp);

    let mut s = Server::new(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    s.set_code(CodeState { setup: code, indexer: None });
    let v = s.call("symbol", &json!({"name": "Mean"})).unwrap();
    assert_eq!(rows(&v, "defs"), ["lib:stats/stats.go:3 fn stats"], "{v}");
    assert_eq!(rows(&v, "refs"), ["svc:main.go:10  svc::main"], "{v}");
    let v = s.call("symbol", &json!({"name": "GetCart"})).unwrap();
    assert!(rows(&v, "refs").contains(&"svc:main.go:12 rpc svc::main".to_string()), "{v}");
    let v = s.call("symbol", &json!({"name": "Do"})).unwrap();
    assert_eq!(v["refs"].as_array().map_or(0, |a| a.len()), 0, "{v}");
}

#[test]
fn a_package_another_root_stops_defining_marks_its_users() {
    let t = Temp::new("code-xroot-gone");
    let (code, db) = setup(&t);
    index_one(&code, &db, 1);
    let rep = index_one(&code, &db, 0);
    assert_eq!(rep.rows.xroot_refs, 3, "{:?}", rep.rows);
    // The library's index goes away and its files are scanned: it defines
    // no package now, so the service's stored references are stale.
    fs::remove_file(code.roots[1].root.join("index.scip")).unwrap();
    index_one(&code, &db, 1);
    assert_eq!(fingerprint(&db, "svc"), "");
    let rep = index_one(&code, &db, 0);
    assert_eq!((rep.rows.xroot_refs, rep.rows.external_refs), (0, 4), "{:?}", rep.rows);
}

/// rust-analyzer's index of a crate root: definitions with enclosing
/// ranges, and references.
fn rust_index(defs: &[(&[i32], &str, &[i32])], refs: &[(&[i32], &str)]) -> Vec<u8> {
    let mut d = Writer::default();
    d.string(1, "src/lib.rs");
    for (range, sym, encl) in defs {
        let mut o = Writer::default();
        o.packed(1, range).string(2, sym).int(3, 1);
        if !encl.is_empty() {
            o.packed(7, encl);
        }
        d.message(2, &o);
    }
    for (range, sym) in refs {
        occ(&mut d, range, sym, 0);
    }
    let mut tool = Writer::default();
    tool.string(1, "rust-analyzer").string(2, "1.0.0");
    let mut meta = Writer::default();
    meta.message(2, &tool);
    let mut ix = Writer::default();
    ix.message(1, &meta).message(2, &d);
    ix.buf
}

#[test]
fn a_rust_crate_using_a_sibling_roots_crate_gets_its_references() {
    let t = Temp::new("code-xroot-rust");
    let geo = t.path().join("geo");
    write(&geo.join("Cargo.toml"), "[package]\nname = \"geo\"\nversion = \"0.3.0\"\n");
    write(&geo.join("src/lib.rs"), "pub struct Point;\n\nimpl Point {\n    pub fn dist(&self) -> f64 {\n        0.0\n    }\n}\n");
    let app = t.path().join("app");
    write(&app.join("Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\n");
    write(&app.join("src/lib.rs"), "pub fn run(p: geo::Point) -> f64 {\n    p.dist()\n}\n");
    let geo_scip = t.path().join("geo.scip");
    fs::write(
        &geo_scip,
        rust_index(
            &[(&[0, 11, 16], "rust-analyzer cargo geo 0.3.0 Point#", &[]), (&[3, 11, 15], "rust-analyzer cargo geo 0.3.0 Point#dist().", &[3, 4, 5, 5])],
            &[],
        ),
    )
    .unwrap();
    let app_scip = t.path().join("app.scip");
    fs::write(
        &app_scip,
        rust_index(
            &[(&[0, 7, 10], "rust-analyzer cargo app 0.1.0 run().", &[0, 0, 2, 1])],
            &[(&[0, 19, 24], "rust-analyzer cargo geo 0.3.0 Point#"), (&[1, 6, 10], "rust-analyzer cargo geo 0.3.0 Point#dist()."), (&[1, 4, 5], "rust-analyzer cargo std 1.0.0 f64#")],
        ),
    )
    .unwrap();
    let db = t.path().join("state/atlas.sqlite");
    let root = |name: &str, root: PathBuf| atlas::config::CodeRootSpec { name: name.into(), root, build_scripts: false, scip: Default::default() };
    let spec = atlas::config::CodeSpec { roots: vec![root("geo", geo), root("app", app)], debounce_s: Some(0), ..Default::default() };
    let mut code = CodeSetup::from_spec(&spec, &db, false);
    code.defer_on_cargo = false;
    code.indexers = Indexers::Off("off in tests".into());
    drop(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::File(geo_scip)).unwrap();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[1], &RustSource::File(app_scip)).unwrap();
    assert_eq!((rep.rows.xroot_refs, rep.rows.external_refs), (2, 1), "{:?}", rep.rows);
    drop(conn);
    let mut s = Server::new(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    s.set_code(CodeState { setup: code, indexer: None });
    let v = s.call("symbol", &json!({"name": "Point::dist"})).unwrap();
    assert_eq!(rows(&v, "defs"), ["geo:src/lib.rs:4 fn Point"], "{v}");
    assert_eq!(rows(&v, "refs"), ["app:src/lib.rs:2  run"], "{v}");
}
