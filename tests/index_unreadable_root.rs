//! A root the indexer cannot walk shows `Failed`, not `Idle`: its folder
//! missing from the start, or removed after another root's run marked it. A
//! walk error used to read as the fingerprint '', which equals a mark's, so
//! the root looked current.

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use atlas::code::CodeSetup;
use atlas::code_index::{Indexer, RootState, RustSource};
use common::*;

fn setup(root: &Path, db: &Path) -> CodeSetup {
    let spec = atlas::config::CodeSpec { roots: vec![atlas::config::CodeRootSpec { name: "app".into(), root: root.to_path_buf(), build_scripts: false, scip: Default::default(), exclude: Vec::new() }], debounce_s: Some(0), ..Default::default() };
    let mut code = CodeSetup::from_spec(&spec, db, false);
    code.defer_on_cargo = false;
    code.watch = false;
    code.rescan_interval = Duration::from_millis(20);
    code
}

fn open_store(db: &Path) {
    let notes = db.parent().unwrap().join("notes");
    fs::create_dir_all(&notes).unwrap();
    drop(atlas::store::Store::open(db, atlas::paths::Vaults::new(&[("notes".to_string(), notes)]).unwrap()).unwrap());
}

/// Whether the root's first walk ends with it `Failed`. A root stays
/// `Starting` until that walk ends, so this waits on the indexer's state.
fn first_walk_fails(ix: &Indexer) -> bool {
    ix.wait_idle(Duration::from_secs(10)) && matches!(ix.status().get("app"), Some(RootState::Failed(_)))
}


#[test]
fn a_root_whose_folder_is_missing_shows_failed() {
    let t = Temp::new("ix-missing-root");
    let db = t.path().join("state/atlas.sqlite");
    open_store(&db);
    let ix = Indexer::start(setup(&t.path().join("gone"), &db), db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(first_walk_fails(&ix), "{:?}", ix.status());
}

#[test]
fn a_marked_root_whose_folder_is_removed_shows_failed() {
    let t = Temp::new("ix-removed-root");
    let root = t.path().join("app");
    write(&root.join("src/lib.rs"), "pub fn a() {}\n");
    let db = t.path().join("state/atlas.sqlite");
    open_store(&db);
    let ix = Indexer::start(setup(&root, &db), db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(ix.wait_reports(1, Duration::from_secs(30)), "{:?}", ix.status());
    fs::remove_dir_all(&root).unwrap();
    let conn = atlas::db::Connection::open(&db).unwrap();
    conn.execute("UPDATE code_roots SET fingerprint = '' WHERE root = 'app'", []).unwrap();
    let t = ix.check();
    assert!(ix.wait_checked(t, Duration::from_secs(10)), "the check was never served: {:?}", ix.status());
    assert!(matches!(ix.status().get("app"), Some(RootState::Failed(_))), "{:?}", ix.status());
}

/// A root whose folder comes back after a failed walk is indexed again: a
/// failure holds only while the root stays as it was when it failed.
#[test]
fn a_root_whose_folder_comes_back_is_indexed_again() {
    let t = Temp::new("ix-back-root");
    let root = t.path().join("app");
    let db = t.path().join("state/atlas.sqlite");
    open_store(&db);
    let ix = Indexer::start(setup(&root, &db), db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(first_walk_fails(&ix), "{:?}", ix.status());
    write(&root.join("src/lib.rs"), "pub fn back() {}\n");
    assert!(ix.wait_reports(1, Duration::from_secs(30)), "the root was not indexed again: {:?}", ix.status());
}
