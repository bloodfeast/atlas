//! A run that cannot mark the other roots its change made stale must not store
//! its own fingerprint: stored, the root reads as current, gets no further
//! run, and the other roots' marks are never written.

mod common;

use std::path::Path;

use atlas::code::CodeSetup;
use atlas::code_index::{index_root, RustSource};
use common::*;

fn setup(api: &Path, app: &Path, db: &Path) -> CodeSetup {
    let root = |name: &str, dir: &Path| atlas::config::CodeRootSpec {
        name: name.into(),
        root: dir.to_path_buf(),
        build_scripts: false,
        scip: Default::default(),
        exclude: Vec::new(),
    };
    let spec = atlas::config::CodeSpec { roots: vec![root("api", api), root("app", app)], debounce_s: Some(0), ..Default::default() };
    let mut code = CodeSetup::from_spec(&spec, db, false);
    code.defer_on_cargo = false;
    code
}

/// `api` gains an rpc, so its run marks `app`. The mark fails (a trigger
/// stands in for a busy store) and the run fails. Once the store is fine,
/// `api` runs again and writes the mark.
#[test]
fn a_run_whose_marks_fail_keeps_its_old_fingerprint_and_marks_on_the_next_run() {
    let t = Temp::new("ix-marks-error");
    let api = t.path().join("api");
    let app = t.path().join("app");
    write(
        &api.join("proto/api.proto"),
        "syntax = \"proto3\";\npackage api;\nmessage Req {}\nmessage Resp {}\nservice Api {\n  rpc Get(Req) returns (Resp);\n}\n",
    );
    write(&app.join("src/lib.rs"), "pub fn a() {}\n");
    let db = t.path().join("state/atlas.sqlite");
    let notes = t.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    drop(atlas::store::Store::open(&db, atlas::paths::Vaults::new(&[("notes".to_string(), notes)]).unwrap()).unwrap());
    let code = setup(&api, &app, &db);
    let scan = RustSource::ScanOnly("test".into());
    let mut conn = atlas::db::Connection::open(&db).unwrap();
    index_root(&code, &mut conn, &code.roots[1], &scan).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER no_marks BEFORE UPDATE OF fingerprint ON code_roots WHEN NEW.fingerprint = ''
         BEGIN SELECT RAISE(ABORT, 'injected mark failure'); END;",
    )
    .unwrap();
    assert!(index_root(&code, &mut conn, &code.roots[0], &scan).is_err(), "the mark did not fail");
    let walk = atlas::code::root_fingerprint(&code.roots[0]).unwrap();
    let stored: Option<String> = conn.query_row("SELECT fingerprint FROM code_roots WHERE root = 'api'", [], |r| r.get(0)).ok();
    assert_ne!(stored.as_deref(), Some(walk.as_str()), "the failed run stored its fingerprint: the root reads as current");
    conn.execute_batch("DROP TRIGGER no_marks;").unwrap();
    index_root(&code, &mut conn, &code.roots[0], &scan).unwrap();
    let fp: String = conn.query_row("SELECT fingerprint FROM code_roots WHERE root = 'app'", [], |r| r.get(0)).unwrap();
    assert!(fp.is_empty(), "the run after the failure did not mark the root its rpc made stale");
}
