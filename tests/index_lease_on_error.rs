//! A database error while the indexer holds the lease must not leave the lease
//! held: other servers would be blocked until it expired.

mod common;

use std::path::Path;
use std::time::Duration;

use atlas::code::CodeSetup;
use atlas::code_index::{Indexer, RootState, RustSource};
use common::*;

fn setup(root: &Path, db: &Path) -> CodeSetup {
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec {
            name: "app".into(),
            root: root.to_path_buf(),
            build_scripts: false,
            scip: Default::default(),
            exclude: Vec::new(),
        }],
        debounce_s: Some(0),
        ..Default::default()
    };
    let mut code = CodeSetup::from_spec(&spec, db, false);
    code.defer_on_cargo = false;
    code.watch = false;
    code.rescan_interval = Duration::from_millis(20);
    code
}

/// `root_records` reads fine until a lease row exists, then fails: the error
/// lands in the recheck made while this indexer holds the lease.
#[test]
fn a_database_error_under_the_lease_leaves_the_lease_free() {
    let t = Temp::new("ix-lease-error");
    let root = t.path().join("app");
    write(&root.join("src/lib.rs"), "pub fn a() {}\n");
    let db = t.path().join("state/atlas.sqlite");
    let notes = t.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    drop(atlas::store::Store::open(&db, atlas::paths::Vaults::new(&[("notes".to_string(), notes)]).unwrap()).unwrap());
    let conn = atlas::store::open_raw(&db).unwrap();
    atlas::code::ensure_schema(&conn).unwrap();
    conn.execute_batch(
        "DROP TABLE code_roots;
         CREATE VIEW code_roots AS SELECT 'app' AS root, 'x' AS fingerprint, '' AS indexed, 0 AS files, 0 AS defs, 0 AS refs, 0 AS impls,
             CASE WHEN EXISTS (SELECT 1 FROM code_lease) THEN abs(-9223372036854775808) ELSE 'ok' END AS rust;",
    )
    .unwrap();
    let ix = Indexer::start(setup(&root, &db), db.clone(), Some(RustSource::ScanOnly("test".into())));
    assert!(ix.wait_idle(Duration::from_secs(10)));
    assert!(matches!(ix.status().get("app"), Some(RootState::Failed(_))), "{:?}", ix.status());
    let held: i64 = conn.query_row("SELECT count(*) FROM code_lease", [], |r| r.get(0)).unwrap();
    assert_eq!(held, 0, "the lease is still held after the error");
}
