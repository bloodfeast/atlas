//! `code.roots[].exclude` (public #17): globs relative to a code root that
//! no code indexer reads. The walk never enters them, so no unit there is
//! indexed (no scip-* run) or scanned; rows a whole-workspace index gives
//! for them are dropped; the per-file re-index after an edit leaves them
//! out; and the configuration refuses a glob that is not relative.

mod common;

use std::path::Path;

use atlas::code::{self, CodeSetup, Indexers, Reindexed};
use atlas::code_index::{self, RustSource};
use common::*;

fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    std::fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

fn setup(root: &Path, db: &Path, exclude: &[&str]) -> CodeSetup {
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec {
            name: "plugins".into(),
            root: root.to_path_buf(),
            build_scripts: false,
            scip: Default::default(),
            cpp: Default::default(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
        }],
        debounce_s: Some(0),
        ..Default::default()
    };
    let mut code = CodeSetup::from_spec(&spec, db, false);
    code.defer_on_cargo = false;
    code.indexers = Indexers::Off("test".into());
    code
}

/// Two Python packages and a Rust file in a folder the globs name.
fn tree(root: &Path) {
    write(&root.join("good/pyproject.toml"), "[project]\nname = \"good\"\n");
    write(&root.join("good/good/core.py"), "def kept_function():\n    pass\n");
    write(&root.join("OctoPrint-Slow/setup.py"), "from setuptools import setup\nsetup()\n");
    write(&root.join("OctoPrint-Slow/slow/core.py"), "def dropped_function():\n    pass\n");
    write(&root.join("tools/gen/skip.rs"), "pub fn dropped_rust() {}\n");
    write(&root.join("tools/keep.rs"), "pub fn kept_rust() {}\n");
}

fn defs(conn: &atlas::db::Connection, name: &str) -> usize {
    conn.query_row("SELECT COUNT(*) FROM code_occ WHERE name = ?1", [name], |r| r.get::<_, i64>(0)).unwrap() as usize
}

#[test]
fn excluded_folders_are_never_walked_indexed_or_scanned() {
    let t = Temp::new("code-exclude");
    let root = t.path().join("plugins");
    tree(&root);
    let db = t.path().join("state/atlas.sqlite");
    // A glob without `/` names a folder anywhere; one with `/` is a path.
    let code = setup(&root, &db, &["OctoPrint-Slow", "tools/gen"]);
    let w = code.roots[0].walk().unwrap();
    let paths: Vec<&str> = w.source.iter().chain(&w.rust).map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["good/good/core.py", "tools/keep.rs"]);
    let units: Vec<String> = code::units(&w).iter().map(|u| u.dir.clone()).collect();
    assert_eq!(units, ["good"], "no unit, so no scip-python run, for the excluded package");
    drop(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    let mut conn = atlas::db::Connection::open(&db).unwrap();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    assert!(rep.units.iter().all(|u| !u.dir.contains("OctoPrint")), "{:?}", rep.units);
    assert_eq!((defs(&conn, "kept_function"), defs(&conn, "kept_rust")), (1, 1));
    assert_eq!((defs(&conn, "dropped_function"), defs(&conn, "dropped_rust")), (0, 0));
    // An edit in an excluded folder re-indexes nothing.
    let skipped = root.join("tools/gen/skip.rs");
    assert_eq!(code::reindex_file(&mut conn, &skipped.to_string_lossy()).unwrap(), Reindexed::NotCovered);
    assert_eq!(defs(&conn, "dropped_rust"), 0);
    // Without the globs the same tree indexes everything.
    let all = setup(&root, &db, &[]);
    code_index::index_root(&all, &mut conn, &all.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    assert_eq!((defs(&conn, "dropped_function"), defs(&conn, "dropped_rust")), (1, 1));
}

#[test]
fn exclude_matches_a_folder_and_everything_below_it() {
    let ex = |g: &[&str], p: &str| code::excluded(&g.iter().map(|s| s.to_string()).collect::<Vec<_>>(), p);
    assert!(ex(&["legacy"], "legacy") && ex(&["legacy"], "a/legacy/x.py") && !ex(&["legacy"], "a/legacy2/x.py"));
    assert!(ex(&["pkgs/slow"], "pkgs/slow/x/y.py") && !ex(&["pkgs/slow"], "other/pkgs/slow/x.py"));
    assert!(ex(&["**/generated"], "a/b/generated/x.rs") && ex(&["*.pb.go"], "api/x.pb.go"));
    assert!(!ex(&[], "anything"));
}

#[test]
fn exclude_globs_must_be_relative() {
    let t = Temp::new("code-exclude-cfg");
    let parsed = |glob: &str| {
        let text = format!(r#"{{"code": {{"roots": [{{"name": "c", "root": ".", "exclude": ["{glob}"]}}]}}}}"#);
        atlas::config::Config::parse(&text, t.path()).unwrap().code.unwrap().roots[0].exclude.clone()
    };
    // A trailing `/` and a leading `./` would match nothing: they are dropped.
    assert_eq!(parsed("./tools/gen/"), ["tools/gen"]);
    assert_eq!(parsed("OctoPrint-Slow/"), ["OctoPrint-Slow"]);
    for (glob, ok) in [
        ("pkgs/slow", true),
        ("slow-*", true),
        ("/abs", false),
        ("../up", false),
        ("a\\\\b", false),
        ("", false),
        ("a//b", false),
        (" a", false),
        ("/", false),
    ] {
        let text = format!(r#"{{"code": {{"roots": [{{"name": "c", "root": ".", "exclude": ["{glob}"]}}]}}}}"#);
        let r = atlas::config::Config::parse(&text, t.path());
        assert_eq!(r.is_ok(), ok, "{glob}: {r:?}");
        if !ok {
            assert!(r.unwrap_err().to_string().contains("exclude"), "{glob}");
        }
    }
}

/// Rows an index of the whole workspace gave for an excluded file go, and
/// the files after it move down with their rows and end lines.
#[test]
fn dropping_excluded_rows_renumbers_the_files_after_them() {
    let occ = |name: &str, file: u32| code::Occ {
        name: name.into(),
        sect: code::SECT_DEF,
        file,
        line: 1,
        col: 0,
        kind: "fn".into(),
        container: String::new(),
        within: String::new(),
    };
    let mut rows = code::Rows {
        files: vec![("a.rs".into(), "rust"), ("gen/b.rs".into(), "rust"), ("c.rs".into(), "rust")],
        occ: vec![occ("a", 0), occ("b", 1), occ("c", 2)],
        ends: [((0, 1), 3), ((1, 1), 4), ((2, 1), 5)].into_iter().collect(),
        ..Default::default()
    };
    rows.drop_excluded(&["gen".to_string()]);
    assert_eq!(rows.files, [("a.rs".to_string(), "rust"), ("c.rs".to_string(), "rust")]);
    assert_eq!(rows.occ.iter().map(|o| (o.name.as_str(), o.file)).collect::<Vec<_>>(), [("a", 0), ("c", 1)]);
    let mut ends: Vec<_> = rows.ends.into_iter().collect();
    ends.sort();
    assert_eq!(ends, [((0, 1), 3), ((1, 1), 5)]);
}

/// A copy of the `ts` SCIP fixture in `t` (its project and `index.scip`,
/// kept apart), so its whole-root index can be given with an exclude.
fn ts_fixture(t: &Temp) -> (std::path::PathBuf, std::path::PathBuf) {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/scip/ts");
    let root = t.path().join("ts");
    for f in ["package.json", "tsconfig.json", "src/main.ts", "src/shapes.ts"] {
        write(&root.join(f), &std::fs::read_to_string(from.join(f)).unwrap());
    }
    let scip = t.path().join("index.scip");
    std::fs::copy(from.join("index.scip"), &scip).unwrap();
    (root, scip)
}

/// #371: a whole-root SCIP index's excluded documents count nowhere: not in
/// the stats line, not as a package the root defines, not as a language
/// unit. Excluding one file lowers the counts; excluding every file leaves
/// none, no defined package and no prebuilt unit.
#[test]
fn excluded_documents_count_in_no_stats_packages_or_units() {
    let t = Temp::new("code-exclude-371");
    let (root, scip) = ts_fixture(&t);
    let db = t.path().join("state/atlas.sqlite");
    drop(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    let mut conn = atlas::db::Connection::open(&db).unwrap();
    let run = |conn: &mut atlas::db::Connection, exclude: &[&str]| {
        let code = setup(&root, &db, exclude);
        let rep = code_index::index_root(&code, conn, &code.roots[0], &RustSource::File(scip.clone())).unwrap();
        let packages = code::defined_packages(conn, "plugins", true).unwrap();
        (rep, packages)
    };
    let (full, full_pkgs) = run(&mut conn, &[]);
    assert!(full.rows.scip_defs > 0 && full.rows.scip_refs > 0 && !full_pkgs.is_empty(), "{:?} {full_pkgs:?}", full.rows);
    assert!(full.units.iter().any(|u| u.source.starts_with("prebuilt")), "{:?}", full.units);
    let (one, _) = run(&mut conn, &["src/shapes.ts"]);
    assert!(one.rows.scip_defs < full.rows.scip_defs && one.rows.scip_defs > 0, "{:?} vs {:?}", one.rows, full.rows);
    let (none, none_pkgs) = run(&mut conn, &["src"]);
    assert_eq!((none.rows.scip_defs, none.rows.scip_refs, none.rows.external_refs, none.rows.impls), (0, 0, 0, 0), "{:?}", none.rows);
    assert!(none_pkgs.is_empty(), "{none_pkgs:?}");
    assert!(none.units.iter().all(|u| !u.source.starts_with("prebuilt")), "{:?}", none.units);
}

/// #371: the watcher wakes a root's re-index for a changed file the code
/// index reads, unless an `exclude` glob names it.
#[test]
fn the_watcher_ignores_changes_in_excluded_folders() {
    let ex = |g: &[&str]| g.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let paths = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(code_index::change_wakes(&paths(&["tools/keep.rs"]), &ex(&["tools/gen"])));
    assert!(!code_index::change_wakes(&paths(&["tools/gen/skip.rs"]), &ex(&["tools/gen"])));
    assert!(!code_index::change_wakes(&paths(&["a/OctoPrint-Slow/x.py", "tools/gen/a/b.rs"]), &ex(&["OctoPrint-Slow", "tools/gen"])));
    assert!(code_index::change_wakes(&paths(&["tools/gen/skip.rs", "src/lib.rs"]), &ex(&["tools/gen"])));
    assert!(code_index::change_wakes(&paths(&["tools/gen/skip.rs"]), &[]));
    assert!(!code_index::change_wakes(&paths(&["notes.md"]), &[]));
}
