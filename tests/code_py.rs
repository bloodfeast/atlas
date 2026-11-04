//! Python in the code index through the language table (`src/langs.rs`):
//! the unit walk (project files, requirements files as the weakest marker,
//! skipped folders), the scanner's indentation blocks when no indexer runs,
//! prebuilt SCIP files (`index.scip` in a unit's folder) and their
//! file-by-file degradation when a source file is newer, the scip-python
//! driver beside a prebuilt file, the per-file re-scan and the recall
//! check.
//!
//! No real scip-python runs here: the driver is pointed at a stand-in
//! script that records its arguments and copies the checked-in
//! `fixtures/scip/py/index.scip` (written by scip-python 0.6.8, see
//! `tests/code_scip.rs`) to the output path it is given, or exits with
//! failure. File times are set explicitly, so no test waits on the clock.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use atlas::code::{self, CodeSetup, Indexers, Reindexed, Unit};
use atlas::code_index::{self, IndexReport, RustSource};
use atlas::json;
use atlas::tools::{CodeState, Server};
use common::*;

fn py_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("scip").join("py")
}

const SOURCES: &[&str] = &["pyproject.toml", "shapes/__init__.py", "shapes/geometry.py", "shapes/main.py"];

/// Copies the Python fixture's sources (not its index) into `to`, all
/// stamped `at`.
fn copy_sources(to: &Path, at: SystemTime) {
    for f in SOURCES {
        let p = to.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::copy(py_fixture().join(f), &p).unwrap();
        stamp(&p, at);
    }
}

/// Sets a file's modification time.
fn stamp(p: &Path, at: SystemTime) {
    fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(at).unwrap();
}

/// Copies the fixture's index into a unit folder as its prebuilt file,
/// stamped `at`.
fn prebuilt(dir: &Path, at: SystemTime) -> PathBuf {
    let p = dir.join("index.scip");
    fs::copy(py_fixture().join("index.scip"), &p).unwrap();
    stamp(&p, at);
    p
}

fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

fn setup(root: &Path, db: &Path, indexers: Indexers, scip: code::Prebuilt) -> CodeSetup {
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec { name: "svc".into(), root: root.to_path_buf(), build_scripts: false, scip }],
        debounce_s: Some(0),
        ..Default::default()
    };
    let mut code = CodeSetup::from_spec(&spec, db, false);
    code.defer_on_cargo = false;
    code.indexers = indexers;
    code
}

/// A stand-in scip-python in `dir`: appends its arguments to `args.txt`
/// beside it (one line per run), then copies the fixture index to the
/// argument after `--output` (the eighth), or exits 1 when `fail`.
fn stand_in(dir: &Path, fail: bool) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    fs::copy(py_fixture().join("index.scip"), dir.join("index.scip")).unwrap();
    if cfg!(windows) {
        let p = dir.join("scip-python.cmd");
        let run = if fail { "@exit /b 1\r\n" } else { "@copy /Y \"%~dp0index.scip\" %8 >NUL\r\n@exit /b 0\r\n" };
        fs::write(&p, format!("@echo %*>> \"%~dp0args.txt\"\r\n{run}")).unwrap();
        p
    } else {
        let p = dir.join("scip-python");
        let run = if fail { "exit 1\n" } else { "cp \"$(dirname \"$0\")/index.scip\" \"$8\"\n" };
        fs::write(&p, format!("#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/args.txt\"\n{run}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

/// Each recorded run's arguments, the output file left out.
fn recorded(dir: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(dir.join("args.txt"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.split_whitespace().map(|a| a.trim_matches('"')).filter(|a| !a.ends_with(".part")).map(str::to_string).collect())
        .collect()
}

/// Indexes the root and opens a server over it.
fn indexed(code: &CodeSetup, db: &Path) -> (IndexReport, Server) {
    drop(atlas::store::Store::open(db, vaults(db)).unwrap());
    let mut conn = rusqlite::Connection::open(db).unwrap();
    let rep = code_index::index_root(code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    let store = atlas::store::Store::open(db, vaults(db)).unwrap();
    let mut s = Server::new(store);
    s.set_code(CodeState { setup: code.clone(), indexer: None });
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

fn units(w: &code::Walk) -> Vec<(String, String)> {
    code::units(w).iter().map(|u| (u.lang.name.to_string(), u.dir.clone())).collect()
}

#[test]
fn project_files_make_units_and_environments_are_skipped() {
    let t = Temp::new("py-walk");
    let root = t.path().join("repo");
    write(&root.join("api/pyproject.toml"), "[project]\nname = \"api\"\n");
    write(&root.join("api/src/api/app.py"), "def create(): pass\n");
    write(&root.join("api/build/lib/api/app.py"), "def stale(): pass\n");
    write(&root.join("api/src/api/__pycache__/x.py"), "X = 1\n");
    write(&root.join("api/src/api.egg-info/x.py"), "X = 1\n");
    write(&root.join("api/venv/lib/site.py"), "X = 1\n");
    write(&root.join("legacy/setup.py"), "from setuptools import setup\nsetup()\n");
    write(&root.join("legacy/legacy/core.py"), "class Core: pass\n");
    write(&root.join("scripts/requirements-dev.txt"), "pytest\n");
    write(&root.join("scripts/run.py"), "def main(): pass\n");
    write(&root.join("api/index.scip"), "");
    let w = code::walk(&root).unwrap();
    let paths = |l: &[code::WalkedFile]| l.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
    assert_eq!(paths(&w.source), ["api/src/api/app.py", "legacy/legacy/core.py", "legacy/setup.py", "scripts/run.py"]);
    assert_eq!(paths(&w.markers), ["api/pyproject.toml", "scripts/requirements-dev.txt"]);
    assert_eq!(paths(&w.prebuilt), ["api/index.scip"]);
    // `setup.py` is a source file and a marker.
    assert_eq!(
        units(&w),
        [("python".to_string(), "api".to_string()), ("python".to_string(), "legacy".to_string()), ("python".to_string(), "scripts".to_string())]
    );
    assert!(code::relevant("api/pyproject.toml") && code::relevant("uv.lock") && code::relevant("api/index.scip"));
    assert!(code::relevant("api/src/api/app.py"));
    // A project file above claims the folders below it.
    write(&root.join("pyproject.toml"), "[project]\nname = \"all\"\n");
    assert_eq!(units(&code::walk(&root).unwrap()), [("python".to_string(), String::new())]);
}

#[test]
fn the_scanner_alone_gives_definitions_and_ranges() {
    let t = Temp::new("py-scan");
    let root = t.path().join("svc");
    copy_sources(&root, SystemTime::now());
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Off("test".into()), code::Prebuilt::Convention);
    let (rep, mut s) = indexed(&code, &db);
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "python".into(), source: "scan only: test".into() }]);
    let (defs, refs) = answer(&mut s, "Square");
    assert_eq!(defs, ["shapes/geometry.py:37 class (scan) geometry", "shapes/geometry.py:40 ctor (scan) Square"]);
    assert!(refs.is_empty(), "the scanner gives definitions only");
    let (defs, _) = answer(&mut s, "squared");
    assert_eq!(defs, ["shapes/geometry.py:45 method (scan) Square"], "a function in a method is the class's member");
    let l = labels(&db, &root.join("shapes/geometry.py"));
    for want in ["class geometry::Shape 15-34", "fn Shape::Shape 21-24", "field Shape::tagged 24-24", "fn Circle::area 59-64", "fn geometry::area_of 70-75"] {
        assert!(l.contains(&want.to_string()), "{want}: {l:?}");
    }
    let conn = rusqlite::Connection::open(&db).unwrap();
    let r = atlas::recall::code_check(&conn, &code.roots).unwrap();
    assert_eq!(
        r.langs,
        [atlas::recall::LangRecall { lang: "python".into(), files: 3, truth: 16, excluded: 1, indexed: 0, scan_only: 16, missing: 0 }],
        "`def __init__` lines are held by constructor rows; the `def` in a string is no code"
    );
}

#[test]
fn a_prebuilt_index_is_read_and_a_newer_file_falls_back_to_the_scanner() {
    let t = Temp::new("py-prebuilt");
    let root = t.path().join("svc");
    let t0 = SystemTime::now() - Duration::from_secs(3600);
    copy_sources(&root, t0);
    prebuilt(&root, t0 + Duration::from_secs(60));
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Off("test".into()), code::Prebuilt::Convention);
    let (rep, mut s) = indexed(&code, &db);
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "python".into(), source: "prebuilt scip-python 0.6.8".into() }]);
    let (defs, refs) = answer(&mut s, "area_of");
    assert_eq!(defs, ["shapes/geometry.py:70 fn geometry"]);
    assert_eq!(refs, ["shapes/__init__.py:3 ", "shapes/main.py:1 ", "shapes/main.py:6 main::main"]);
    // Ends from the indentation blocks; callers from them.
    let l = labels(&db, &root.join("shapes/geometry.py"));
    assert!(l.contains(&"fn geometry::area_of 70-75".to_string()), "{l:?}");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let r = atlas::recall::code_check(&conn, &code.roots).unwrap();
    assert_eq!((r.langs[0].truth, r.langs[0].indexed, r.langs[0].missing), (16, 16, 0), "SCIP rows hold every definition grep finds");
    drop(conn);
    // The geometry file is edited after the index was written: its rows
    // come from the scanner at its new lines, the other files' from the
    // index.
    let g = root.join("shapes/geometry.py");
    let text = fs::read_to_string(&g).unwrap().replace("\r\n", "\n");
    fs::write(&g, format!("# Edited.\n# Twice.\n{text}")).unwrap();
    stamp(&g, t0 + Duration::from_secs(120));
    let (rep, mut s) = indexed(&code, &db);
    assert_eq!(rep.units[0].source, "prebuilt scip-python 0.6.8 (1 file newer, scanned)");
    // The coverage note comes once per session: on the first answer.
    let text = s.answer("symbol", &json!({"name": "largest"})).unwrap().into_text();
    assert!(text.contains("svc: python: prebuilt scip-python 0.6.8 (1 file newer, scanned)"), "{text}");
    let (defs, refs) = answer(&mut s, "area_of");
    assert_eq!(defs, ["shapes/geometry.py:72 fn (scan) geometry"], "the edited file's lines, not the index's");
    assert_eq!(refs, ["shapes/__init__.py:3 ", "shapes/main.py:1 ", "shapes/main.py:6 main::main"]);
    let l = labels(&db, &g);
    assert!(l.contains(&"fn geometry::area_of 72-77".to_string()), "{l:?}");
    // Turned off, the file is not read.
    let off = setup(&root, &t.path().join("state2/atlas.sqlite"), Indexers::Off("test".into()), code::Prebuilt::Off);
    let (rep, _) = indexed(&off, &t.path().join("state2/atlas.sqlite"));
    assert_eq!(rep.units[0].source, "scan only: test");
}

#[test]
fn a_configured_prebuilt_file_may_live_outside_the_root() {
    let t = Temp::new("py-configured");
    let root = t.path().join("svc");
    let t0 = SystemTime::now() - Duration::from_secs(3600);
    copy_sources(&root, t0);
    fs::create_dir_all(t.path().join("ci")).unwrap();
    let file = prebuilt(&t.path().join("ci"), t0 + Duration::from_secs(60));
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Off("test".into()), code::Prebuilt::Files(vec![(String::new(), Some(file.clone()))]));
    let before = code::root_fingerprint(&code.roots[0]).unwrap();
    let (rep, _) = indexed(&code, &db);
    assert_eq!(rep.units[0].source, "prebuilt scip-python 0.6.8");
    // A new file outside the root changes the root's fingerprint.
    stamp(&file, t0 + Duration::from_secs(90));
    assert_ne!(code::root_fingerprint(&code.roots[0]).unwrap(), before);
    assert_eq!(code::walk(&root).unwrap().fingerprint(), code::walk(&root).unwrap().fingerprint_with(&[]));
}

#[test]
fn the_indexer_runs_unless_a_fresh_prebuilt_file_is_newer_and_a_failed_run_falls_back() {
    let t = Temp::new("py-driver");
    let root = t.path().join("shop");
    let t0 = SystemTime::now() - Duration::from_secs(3600);
    copy_sources(&root, t0);
    let bin = stand_in(&t.path().join("bin"), false);
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Fixed(vec![("scip-python".into(), bin.clone())]), code::Prebuilt::Convention);
    // No prebuilt file: the indexer runs, named after the unit's folder.
    let (rep, mut s) = indexed(&code, &db);
    assert_eq!(rep.units[0].source, "scip-python 0.6.8");
    let runs = recorded(bin.parent().unwrap());
    assert_eq!(runs, [["index", ".", "--project-name", "shop", "--project-version", "0", "--output"]]);
    let (defs, _) = answer(&mut s, "Square");
    assert_eq!(defs, ["shapes/geometry.py:37 class geometry", "shapes/geometry.py:40 ctor Square"]);
    // A prebuilt file newer than the last run and than every source file
    // stands for the run.
    let later = SystemTime::now() + Duration::from_secs(600);
    let p = prebuilt(&root, later);
    let (rep, _) = indexed(&code, &db);
    assert_eq!(rep.units[0].source, "prebuilt scip-python 0.6.8");
    assert_eq!(recorded(bin.parent().unwrap()).len(), 1, "not run");
    // A source file newer than the prebuilt file: the indexer runs again.
    stamp(&root.join("shapes/main.py"), later + Duration::from_secs(60));
    let (rep, _) = indexed(&code, &db);
    assert_eq!(rep.units[0].source, "scip-python 0.6.8");
    assert_eq!(recorded(bin.parent().unwrap()).len(), 2);
    // The run fails: the prebuilt file is read, its newer file scanned.
    let bad = stand_in(&t.path().join("bad"), true);
    let failing = setup(&root, &db, Indexers::Fixed(vec![("scip-python".into(), bad)]), code::Prebuilt::Convention);
    let (rep, mut s) = indexed(&failing, &db);
    assert_eq!(rep.units[0].source, "prebuilt scip-python 0.6.8 (1 file newer, scanned) (scip-python exited with Some(1))");
    let (_, refs) = answer(&mut s, "area_of");
    assert_eq!(refs, ["shapes/__init__.py:3 "], "main.py is newer than the file: definitions only");
    assert!(code_index::prebuilt_wins(&p, &t.path().join("never-ran.scip"), &[("a.py", 0)]));
    assert!(!code_index::prebuilt_wins(&t.path().join("missing.scip"), &p, &[]));
}

#[test]
fn an_edited_python_file_is_rescanned_at_once() {
    let t = Temp::new("py-reindex");
    let root = t.path().join("svc");
    let t0 = SystemTime::now() - Duration::from_secs(3600);
    copy_sources(&root, t0);
    prebuilt(&root, t0 + Duration::from_secs(60));
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Off("test".into()), code::Prebuilt::Convention);
    let (_, mut s) = indexed(&code, &db);
    let (_, refs) = answer(&mut s, "largest");
    assert_eq!(refs, ["shapes/main.py:1 ", "shapes/main.py:7 main::main"]);
    let p = root.join("shapes/geometry.py");
    let text = format!("# A header line.\n{}", fs::read_to_string(&p).unwrap());
    fs::write(&p, text).unwrap();
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    assert!(matches!(code::reindex_file(&mut conn, &p.to_string_lossy()).unwrap(), Reindexed::Rows(_)));
    let (defs, _) = answer(&mut s, "largest");
    assert_eq!(defs, ["shapes/geometry.py:79 fn (scan) geometry"]);
    assert!(labels(&db, &p).contains(&"class geometry::Shape 16-35".to_string()));
    let lang: String = conn.query_row("SELECT lang FROM code_files WHERE path = 'shapes/geometry.py'", [], |r| r.get(0)).unwrap();
    assert_eq!(lang, "python");
}

#[test]
fn a_prebuilt_file_serves_only_its_own_language() {
    let t = Temp::new("py-mixed");
    let root = t.path().join("svc");
    let t0 = SystemTime::now() - Duration::from_secs(3600);
    copy_sources(&root, t0);
    // Another language's index in the Python unit's folder.
    let go = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/scip/go/index.scip");
    fs::copy(go, root.join("index.scip")).unwrap();
    stamp(&root.join("index.scip"), t0 + Duration::from_secs(60));
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Off("test".into()), code::Prebuilt::Convention);
    let (rep, mut s) = indexed(&code, &db);
    assert_eq!(rep.units[0].source, "scan only: index.scip holds no python files");
    let (defs, _) = answer(&mut s, "Square");
    assert_eq!(defs, ["shapes/geometry.py:37 class (scan) geometry", "shapes/geometry.py:40 ctor (scan) Square"]);
}
