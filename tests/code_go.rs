//! Go in the code index through the language table (`src/langs.rs`): the
//! unit walk (`go.mod`), the scip-go driver, the scanner when no indexer
//! runs or for files the index lacks, the per-file re-scan, the recall
//! check and `add-project`-style detection.
//!
//! No real scip-go runs here: the driver is pointed at a stand-in script
//! that copies the checked-in `fixtures/scip/go/index.scip` (written by
//! scip-go 0.2.9, see `tests/code_scip.rs`) to the output path it is given,
//! or exits with failure.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use atlas::code::{self, CodeSetup, Indexers, Reindexed, Unit};
use atlas::code_index::{self, IndexReport, RustSource};
use atlas::json;
use atlas::tools::{CodeState, Server};
use common::*;

fn go_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("scip").join("go")
}

/// Copies the Go fixture's sources (not its index) into `to`.
fn copy_sources(to: &Path) {
    fs::create_dir_all(to).unwrap();
    for f in ["go.mod", "shapes.go", "scale.go"] {
        fs::copy(go_fixture().join(f), to.join(f)).unwrap();
    }
}

fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

fn setup(root: &Path, db: &Path, indexers: Indexers) -> CodeSetup {
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec { name: "svc".into(), root: root.to_path_buf(), build_scripts: false, scip: Default::default() }],
        debounce_s: Some(0),
        ..Default::default()
    };
    let mut code = CodeSetup::from_spec(&spec, db, false);
    code.defer_on_cargo = false;
    code.indexers = indexers;
    code
}

/// A stand-in indexer in `dir`: copies the fixture index to its second
/// argument (`--output FILE`), or exits 1 when `fail`.
fn stand_in(dir: &Path, fail: bool) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    fs::copy(go_fixture().join("index.scip"), dir.join("index.scip")).unwrap();
    if cfg!(windows) {
        let p = dir.join("scip-go.cmd");
        let body = if fail { "@exit /b 1\r\n".to_string() } else { "@copy /Y \"%~dp0index.scip\" %2 >NUL\r\n@exit /b 0\r\n".to_string() };
        fs::write(&p, body).unwrap();
        p
    } else {
        let p = dir.join("scip-go");
        let body =
            if fail { "#!/bin/sh\nexit 1\n".to_string() } else { "#!/bin/sh\ncp \"$(dirname \"$0\")/index.scip\" \"$2\"\n".to_string() };
        fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

/// Indexes the root and opens a server over it.
fn indexed(root: &Path, db: &Path, indexers: Indexers) -> (IndexReport, Server) {
    let code = setup(root, db, indexers);
    drop(atlas::store::Store::open(db, vaults(db)).unwrap());
    let mut conn = rusqlite::Connection::open(db).unwrap();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    let store = atlas::store::Store::open(db, vaults(db)).unwrap();
    let mut s = Server::new(store);
    s.set_code(CodeState { setup: code, indexer: None });
    (rep, s)
}

/// `path:line kind container` for definitions, `path:line within` for
/// references.
fn answer(s: &mut Server, name: &str) -> (Vec<String>, Vec<String>) {
    let v = s.call("symbol", &json!({ "name": name })).unwrap();
    let list = |key: &str, f: &dyn Fn(&json::Value) -> String| -> Vec<String> { v[key].as_array().unwrap().iter().map(f).collect() };
    (
        list("defs", &|h| {
            format!("{}:{} {} {}", h["path"].as_str().unwrap(), h["line"], h["kind"].as_str().unwrap(), h["container"].as_str().unwrap())
        }),
        list("refs", &|h| format!("{}:{} {}", h["path"].as_str().unwrap(), h["line"], h["within"].as_str().unwrap())),
    )
}

fn labels(db: &Path, file: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    code::file_items(&conn, &file.to_string_lossy()).unwrap().expect("covered").iter().map(|i| i.label()).collect()
}

#[test]
fn the_walk_lists_go_files_and_modules_outside_skipped_folders() {
    let t = Temp::new("go-walk");
    let root = t.path().join("repo");
    copy_sources(&root.join("svc"));
    write(&root.join("svc/go.sum"), "");
    write(&root.join("svc/vendor/x/dep.go"), "package x\n");
    write(&root.join("svc/testdata/case.go"), "package bad\n");
    write(&root.join("tools/gen/go.mod"), "module example.com/gen\n");
    write(&root.join("tools/gen/main.go"), "package main\n\nfunc main() {}\n");
    write(&root.join("lib/src/lib.rs"), "pub fn f() {}\n");
    let w = code::walk(&root).unwrap();
    let paths = |l: &[code::WalkedFile]| l.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
    assert_eq!(paths(&w.source), ["svc/scale.go", "svc/shapes.go", "tools/gen/main.go"]);
    assert_eq!(paths(&w.markers), ["svc/go.mod", "svc/go.sum", "tools/gen/go.mod"]);
    assert_eq!(
        paths(&w.mention),
        ["svc/testdata/case.go", "svc/vendor/x/dep.go"],
        "skipped Go files are still searched for rpc call sites"
    );
    assert_eq!(paths(&w.rust), ["lib/src/lib.rs"]);
    let units: Vec<(String, String)> = code::units(&w).iter().map(|u| (u.lang.name.to_string(), u.dir.clone())).collect();
    assert_eq!(units, [("go".to_string(), "svc".to_string()), ("go".to_string(), "tools/gen".to_string())]);
    assert!(code::relevant("svc/go.sum") && code::relevant("svc/a.go") && !code::relevant("svc/README.md"));
    // A root without Go digests as it always did: the new lists add nothing.
    let rust_only = code::walk(&root.join("lib")).unwrap();
    assert!(rust_only.source.is_empty() && rust_only.markers.is_empty());
}

#[test]
fn the_scanner_alone_matches_the_indexer_on_definitions_and_ranges() {
    let t = Temp::new("go-scan");
    let root = t.path().join("svc");
    copy_sources(&root);
    let db = t.path().join("state/atlas.sqlite");
    let (rep, mut s) = indexed(&root, &db, Indexers::Off("test".into()));
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "go".into(), source: "scan only: test".into() }]);
    assert_eq!(rep.rust, "go: scan only: test", "a Go root has no Rust unit");
    assert!(rep.runs.is_empty());
    // The coverage note comes once per session: on the first answer.
    let t2 = s.answer("symbol", &json!({"name": "Total"})).unwrap().into_text();
    assert!(t2.contains("svc:shapes.go:20-26\tfn (scan)\tshapes\t"), "{t2}");
    assert!(t2.ends_with("svc: go definitions only (scan only: test)\n"), "{t2}");
    let (defs, refs) = answer(&mut s, "Square");
    assert_eq!(defs, ["shapes.go:10 struct (scan) shapes"]);
    assert!(refs.is_empty(), "the scanner gives definitions only");
    let (defs, _) = answer(&mut s, "Area");
    assert_eq!(defs, ["shapes.go:6 method (scan) Shape", "shapes.go:15 method (scan) Square"]);
    let (defs, _) = answer(&mut s, "Square::Area");
    assert_eq!(defs, ["shapes.go:15 method (scan) Square"]);
    // The same items and ranges the scip-go rows give (tests/code_scip.rs).
    assert_eq!(
        labels(&db, &root.join("shapes.go")),
        [
            "interface shapes::Shape 5-7",
            "fn Shape::Area 6-6",
            "struct shapes::Square 10-12",
            "field Square::Side 11-11",
            "fn Square::Area 15-17",
            "fn shapes::Total 20-26",
            "const shapes::Unit 29-29"
        ]
    );
    // The recall check: every grep definition is a scanner row.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let r = atlas::recall::code_check(&conn, &setup(&root, &db, Indexers::Search).roots).unwrap();
    assert_eq!(
        r.langs,
        [atlas::recall::LangRecall { lang: "go".into(), files: 2, truth: 6, excluded: 0, indexed: 0, scan_only: 6, missing: 0 }]
    );
    assert!(r.misses.is_empty() && r.truth == 0, "no Rust or WGSL here");
}

#[test]
fn a_found_indexer_runs_per_module_and_the_scanner_covers_the_rest() {
    let t = Temp::new("go-driver");
    let root = t.path().join("repo");
    copy_sources(&root.join("svc"));
    // A file behind a build tag the index lacks, and one outside any module.
    write(
        &root.join("svc/extra_appengine.go"),
        "//go:build appengine\n\npackage shapes\n\n// Cloud is only built on App Engine.\nfunc Cloud() Square {\n\treturn Square{}\n}\n",
    );
    write(&root.join("scripts/gen.go"), "package main\n\nfunc main() {}\n");
    let bin = stand_in(&t.path().join("bin"), false);
    let db = t.path().join("state/atlas.sqlite");
    let (rep, mut s) = indexed(&root, &db, Indexers::Fixed(vec![("scip-go".into(), bin)]));
    assert_eq!(
        rep.units,
        [
            Unit { dir: "svc".into(), lang: "go".into(), source: "scip-go 0.2.9".into() },
            Unit { dir: String::new(), lang: "go".into(), source: "scan only: no go.mod".into() }
        ]
    );
    assert_eq!(rep.rust, "go svc: scip-go 0.2.9; go: scan only: no go.mod");
    assert_eq!(rep.runs.len(), 1);
    assert_eq!(rep.runs[0].0, "go svc");
    // The coverage note comes once per session: on the first answer.
    let t2 = s.answer("symbol", &json!({"name": "main"})).unwrap().into_text();
    assert!(t2.ends_with("svc: go definitions only (scan only: no go.mod)\n"), "{t2}");
    // SCIP rows, their paths below the module folder made the root's.
    let (defs, refs) = answer(&mut s, "Square");
    assert_eq!(defs, ["svc/shapes.go:10 struct shapes"]);
    assert!(refs.contains(&"svc/scale.go:4 shapes::Double".to_string()), "{refs:?}");
    let (defs, _) = answer(&mut s, "Cloud");
    assert_eq!(defs, ["svc/extra_appengine.go:6 fn (scan) shapes"]);
    let (defs, _) = answer(&mut s, "main");
    assert_eq!(defs, ["scripts/gen.go:3 fn (scan) main"]);
    assert!(t.path().join("state/code/svc.scip-go.scip").is_file(), "the index is kept beside the database");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let r = atlas::recall::code_check(&conn, &setup(&root, &db, Indexers::Search).roots).unwrap();
    assert_eq!(r.langs.len(), 1);
    assert_eq!((r.langs[0].truth, r.langs[0].indexed, r.langs[0].scan_only, r.langs[0].missing), (8, 6, 2, 0));
}

#[test]
fn a_failing_indexer_leaves_the_unit_to_the_scanner() {
    let t = Temp::new("go-fail");
    let root = t.path().join("svc");
    copy_sources(&root);
    let bin = stand_in(&t.path().join("bin"), true);
    let db = t.path().join("state/atlas.sqlite");
    let (rep, mut s) = indexed(&root, &db, Indexers::Fixed(vec![("scip-go".into(), bin)]));
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "go".into(), source: "scan only: scip-go exited with Some(1)".into() }]);
    let (defs, _) = answer(&mut s, "Total");
    assert_eq!(defs, ["shapes.go:20 fn (scan) shapes"]);
    // A tool the setup does not list never runs.
    let (rep, _) = indexed(&root, &t.path().join("state2/atlas.sqlite"), Indexers::Fixed(Vec::new()));
    assert_eq!(rep.units[0].source, "scan only: scip-go is not configured");
}

#[test]
fn an_edited_go_file_is_rescanned_at_once() {
    let t = Temp::new("go-reindex");
    let root = t.path().join("svc");
    copy_sources(&root);
    let bin = stand_in(&t.path().join("bin"), false);
    let db = t.path().join("state/atlas.sqlite");
    let (_, mut s) = indexed(&root, &db, Indexers::Fixed(vec![("scip-go".into(), bin)]));
    let (_, refs) = answer(&mut s, "Area");
    assert_eq!(refs, ["shapes.go:23 shapes::Total"]);
    // Three lines added above everything: the SCIP rows of the file are
    // replaced by scanner rows with the new lines, until the next full run.
    let p = root.join("shapes.go");
    let text = fs::read_to_string(&p).unwrap().replace("package shapes\n", "package shapes\n\n// Added.\nvar Version = \"1\"\n");
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&p, text).unwrap();
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    assert!(matches!(code::reindex_file(&mut conn, &p.to_string_lossy()).unwrap(), Reindexed::Rows(8)));
    let labels = labels(&db, &p);
    assert_eq!(labels[..3], ["var shapes::Version 5-5", "interface shapes::Shape 8-10", "fn Shape::Area 9-9"]);
    assert!(labels.contains(&"fn shapes::Total 23-29".to_string()), "{labels:?}");
    let (defs, refs) = answer(&mut s, "Area");
    assert_eq!(defs, ["shapes.go:9 method (scan) Shape", "shapes.go:18 method (scan) Square"]);
    assert!(refs.is_empty(), "references come back with the next full run");
    let lang: String = conn.query_row("SELECT lang FROM code_files WHERE path = 'shapes.go'", [], |r| r.get(0)).unwrap();
    assert_eq!(lang, "go");
    // A Go file in a skipped folder is not covered.
    write(&root.join("vendor/x/dep.go"), "package x\n");
    assert_eq!(code::reindex_file(&mut conn, &root.join("vendor/x/dep.go").to_string_lossy()).unwrap(), Reindexed::NotCovered);
}
