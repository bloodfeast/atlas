//! A watching indexer that finds a failed root current when it rechecks under
//! the lease (another server indexed it) ends the failure there. A watcher in
//! good health walks nothing more, so nothing else would end it: the root
//! would read `Failed` again at the next pass.

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use atlas::code::CodeSetup;
use atlas::code_index::{index_root, Indexer, RootState, RustSource};
use common::*;

fn setup(app: &Path, other: &Path, db: &Path) -> CodeSetup {
    let root = |name: &str, dir: &Path| atlas::config::CodeRootSpec {
        name: name.into(),
        root: dir.to_path_buf(),
        build_scripts: false,
        scip: Default::default(),
        exclude: Vec::new(),
    };
    let spec = atlas::config::CodeSpec { roots: vec![root("app", app), root("other", other)], debounce_s: Some(0), ..Default::default() };
    let mut code = CodeSetup::from_spec(&spec, db, false);
    code.defer_on_cargo = false;
    code.watch = true;
    code.rescan_interval = Duration::from_secs(600);
    code
}

/// `app`'s first run fails (a trigger stands in for a refused write), then
/// another server indexes it. An event that leaves its fingerprint as it was
/// (the same file, size and mtime, moved into place) schedules it, and the
/// recheck under the lease finds it current. A change in `other` then wakes a
/// pass that walks nothing: `app` must still read `Idle`.
#[test]
fn a_failed_root_found_current_at_the_recheck_stays_idle() {
    let t = Temp::new("ix-recheck-failed");
    let app = t.path().join("app");
    let other = t.path().join("other");
    let lib = app.join("src/lib.rs");
    write(&lib, "pub fn a() {}\n");
    write(&other.join("src/lib.rs"), "pub fn b() {}\n");
    let db = t.path().join("state/atlas.sqlite");
    let notes = t.path().join("notes");
    fs::create_dir_all(&notes).unwrap();
    drop(atlas::store::Store::open(&db, atlas::paths::Vaults::new(&[("notes".to_string(), notes)]).unwrap()).unwrap());
    let mut conn = atlas::db::Connection::open(&db).unwrap();
    atlas::code::ensure_schema(&conn).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_app BEFORE INSERT ON code_roots WHEN NEW.root = 'app'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .unwrap();
    let code = setup(&app, &other, &db);
    let ix = Indexer::start(code.clone(), db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(ix.wait_reports(1, Duration::from_secs(30)), "{:?}", ix.status());
    assert!(ix.wait_idle(Duration::from_secs(10)), "{:?}", ix.status());
    assert!(matches!(ix.status().get("app"), Some(RootState::Failed(_))), "{:?}", ix.status());
    // Another server indexes `app`.
    conn.execute_batch("DROP TRIGGER refuse_app;").unwrap();
    index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    // The same file moved into place: an event, the fingerprint unchanged.
    let copy = t.path().join("lib.rs");
    fs::copy(&lib, &copy).unwrap();
    let mtime = fs::metadata(&lib).unwrap().modified().unwrap();
    fs::File::options().write(true).open(&copy).unwrap().set_modified(mtime).unwrap();
    fs::rename(&copy, &lib).unwrap();
    let idle = |s: &RootState| matches!(s, RootState::Idle);
    assert!(ix.wait_state("app", idle, Duration::from_secs(10)), "the recheck did not find app current: {:?}", ix.status());
    assert_eq!(ix.reports().len(), 1, "app was run, not found current at the recheck");
    write(&other.join("src/lib.rs"), "pub fn b2() {}\n");
    assert!(ix.wait_reports(2, Duration::from_secs(30)), "{:?}", ix.status());
    assert!(matches!(ix.status().get("app"), Some(RootState::Idle)), "the failure came back: {:?}", ix.status());
}
