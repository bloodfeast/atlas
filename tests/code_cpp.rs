//! C++ in the code index through the language table (`src/langs.rs`): the
//! unit walk (a folder holding `compile_commands.json` is a unit; build
//! trees are skipped), the scip-clang driver (run in the unit's folder, the
//! database passed by name, every output in atlas's state), and the scanner
//! when no indexer runs.
//!
//! No real scip-clang runs here: the driver is pointed at a stand-in script
//! that records its arguments and working folder.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use atlas::code::{self, CodeSetup, Indexers};
use atlas::code_index::{self, IndexReport, RustSource};
use atlas::config::CppTrust;
use atlas::json;
use atlas::tools::{CodeState, Server};
use common::*;

fn cpp_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("scip").join("cpp")
}

/// Copies the C++ fixture's sources (not its database) into `to`.
fn copy_sources(to: &Path) {
    fs::create_dir_all(to).unwrap();
    for f in ["shapes.hpp", "shapes.cpp"] {
        fs::copy(cpp_fixture().join(f), to.join(f)).unwrap();
    }
}

fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

fn setup(root: &Path, db: &Path, indexers: Indexers) -> CodeSetup {
    setup_trusted(root, db, indexers, CppTrust::Off)
}

fn setup_trusted(root: &Path, db: &Path, indexers: Indexers, trust: CppTrust) -> CodeSetup {
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec {
            name: "svc".into(),
            root: root.to_path_buf(),
            build_scripts: false,
            scip: Default::default(),
            cpp: trust,
            exclude: Vec::new(),
        }],
        debounce_s: Some(0),
        ..Default::default()
    };
    let mut code = CodeSetup::from_spec(&spec, db, false);
    code.defer_on_cargo = false;
    code.indexers = indexers;
    code
}

/// A stand-in scip-clang in `dir`: appends its working folder and its
/// arguments to `args.txt` beside it (one line per run), then exits 1, so
/// the unit falls back to the scanner. `--version` answers 0.
fn stand_in(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join("scip-clang");
    fs::write(&p, "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\necho \"$(pwd) $@\" >> \"$(dirname \"$0\")/args.txt\"\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    p
}

/// Each recorded run: its working folder, then its arguments.
fn recorded(dir: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(dir.join("args.txt"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.split_whitespace().map(str::to_string).collect())
        .collect()
}

fn indexed(root: &Path, db: &Path, indexers: Indexers) -> (IndexReport, Server) {
    indexed_trusted(root, db, indexers, CppTrust::Off)
}

fn indexed_trusted(root: &Path, db: &Path, indexers: Indexers, trust: CppTrust) -> (IndexReport, Server) {
    let code = setup_trusted(root, db, indexers, trust);
    drop(atlas::store::Store::open(db, vaults(db)).unwrap());
    let mut conn = atlas::db::Connection::open(db).unwrap();
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
    let conn = atlas::db::Connection::open(db).unwrap();
    code::file_items(&conn, &file.to_string_lossy()).unwrap().expect("covered").iter().map(|i| i.label()).collect()
}

#[test]
fn a_folder_with_a_compilation_database_is_a_unit_and_build_trees_are_skipped() {
    let t = Temp::new("cpp-walk");
    let root = t.path().join("repo");
    copy_sources(&root.join("src"));
    write(&root.join("compile_commands.json"), "[]\n");
    write(&root.join("CMakeLists.txt"), "project(geo CXX)\n");
    write(&root.join("build/gen/version.hpp"), "struct Generated {};\n");
    write(&root.join("build/compile_commands.json"), "[]\n");
    write(&root.join("third_party/lib/lib.cc"), "int vendored() { return 0; }\n");
    let w = code::walk(&root).unwrap();
    let paths = |l: &[code::WalkedFile]| l.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
    assert_eq!(paths(&w.source), ["src/shapes.cpp", "src/shapes.hpp"]);
    let units = |w: &code::Walk| code::units(w).iter().map(|u| (u.lang.name.to_string(), u.dir.clone())).collect::<Vec<_>>();
    assert_eq!(units(&w), [("cpp".to_string(), String::new())], "the root's database; build/'s is no unit");
    assert!(code::relevant("compile_commands.json") && code::relevant("CMakeLists.txt") && code::relevant("src/geo.h"));
}

#[test]
fn the_scanner_alone_gives_definitions_and_ranges() {
    let t = Temp::new("cpp-scan");
    let root = t.path().join("svc");
    copy_sources(&root);
    let db = t.path().join("state/atlas.sqlite");
    let (_rep, mut s) = indexed(&root, &db, Indexers::Off("test".into()));
    assert_eq!(
        labels(&db, &root.join("shapes.hpp")),
        [
            "variable detail::kPi 9-9",
            "enum geo::Color 12-12",
            "enum geo::Shade 14-17",
            "field Shade::Light 15-15",
            "field Shade::Dark 16-16",
            "class geo::Point 19-22",
            "field Point::x 20-20",
            "field Point::y 21-21",
            "class geo::Shape 27-37",
            "fn Shape::Shape 29-29",
            "fn Shape::~Shape 30-30",
            "fn Shape::area 31-31",
            "fn Shape::name 32-32",
            "fn Shape::set_sides 33-33",
            "field Shape::sides_ 36-36",
            "class geo::Circle 39-47",
            "fn Circle::Circle 42-42",
            "fn Circle::area 43-43",
            "field Circle::r_ 46-46",
            "type geo::Radius 49-49",
            "type geo::Count 50-50",
            "concept geo::Areal 53-53",
            "fn geo::total 55-55"
        ],
        "a forward declaration, a macro and a string's braces define nothing"
    );
    assert_eq!(
        labels(&db, &root.join("shapes.cpp")),
        ["variable geo::kRaw 4-4", "fn Circle::Circle 6-6", "fn Circle::area 8-11", "fn geo::total 13-19", "fn main 22-24"],
        "a raw string's class defines nothing; out-of-line members take their type as container"
    );
    let (defs, refs) = answer(&mut s, "area");
    assert_eq!(defs.len(), 3, "{defs:?}");
    assert!(refs.is_empty(), "the scanner gives definitions only");
}

#[cfg(unix)]
#[test]
fn a_found_scip_clang_runs_in_the_units_folder_with_every_output_in_atlas_state() {
    let t = Temp::new("cpp-driver");
    let root = t.path().join("repo");
    copy_sources(&root);
    write(&root.join("compile_commands.json"), "[]\n");
    let bin = t.path().join("bin");
    let tool = stand_in(&bin);
    let db = t.path().join("state/atlas.sqlite");
    let (rep, _s) = indexed_trusted(&root, &db, Indexers::Fixed(vec![("scip-clang".into(), tool)]), CppTrust::Always);
    let runs = recorded(&bin);
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_eq!(Path::new(&run[0]).canonicalize().unwrap(), root.canonicalize().unwrap(), "run in the unit's folder");
    assert_eq!(&run[1..3], ["--compdb-path", "compile_commands.json"]);
    assert_eq!(run[3], "--index-output-path");
    assert_eq!(run[5], "--supplementary-output-dir");
    let state = db.parent().unwrap().canonicalize().unwrap();
    for out in [&run[4], &run[6]] {
        assert!(
            Path::new(out).canonicalize().unwrap_or_else(|_| PathBuf::from(out)).starts_with(&state),
            "{out} is not in {}",
            state.display()
        );
    }
    assert!(!root.join("index.scip").exists(), "nothing is written to the source tree");
    // The run failed, so the unit is scanned.
    assert_eq!(rep.units.len(), 1, "{:?}", rep.units);
    assert_eq!(rep.units[0].lang, "cpp");
    assert!(rep.units[0].source.starts_with("scan only"), "{:?}", rep.units[0]);
    assert!(!labels(&db, &root.join("shapes.hpp")).is_empty());
}

/// scip-clang reads the repository's own compilation database, which names
/// the compiler it may run: it runs only on a root whose `cpp.trust` allows
/// it, with `ask` only on a database approved since it last changed, and
/// never with a compiler inside the repository.
#[cfg(unix)]
#[test]
fn scip_clang_runs_only_on_a_trusted_root_and_an_approved_database() {
    let t = Temp::new("cpp-trust");
    let root = t.path().join("repo");
    copy_sources(&root);
    // A compiler outside the repository, as /usr/bin/c++ would be.
    let cxx = t.path().join("sys/c++");
    write(&cxx, "");
    let system_db = format!(r#"[{{"directory": ".", "arguments": ["{}", "-c", "shapes.cpp"], "file": "shapes.cpp"}}]"#, cxx.display());
    let system_db = system_db.as_str();
    write(&root.join("compile_commands.json"), system_db);
    let bin = t.path().join("bin");
    let tool = stand_in(&bin);
    let db = t.path().join("state/atlas.sqlite");
    let fixed = || Indexers::Fixed(vec![("scip-clang".into(), tool.clone())]);
    let note = |rep: &IndexReport| rep.units[0].source.clone();
    // Off by default: scanned, and the note says how to allow it.
    let (rep, _) = indexed(&root, &db, fixed());
    assert!(recorded(&bin).is_empty());
    assert!(note(&rep).contains("scip-clang is off for this root"), "{}", note(&rep));
    // Ask: not before the database is approved.
    let (rep, _) = indexed_trusted(&root, &db, fixed(), CppTrust::Ask);
    assert!(recorded(&bin).is_empty());
    assert!(note(&rep).contains("not approved"), "{}", note(&rep));
    let state = setup(&root, &db, fixed()).dir;
    atlas::cpp_trust::approve("svc", &root, std::slice::from_ref(&root), &state).unwrap();
    indexed_trusted(&root, &db, fixed(), CppTrust::Ask);
    assert_eq!(recorded(&bin).len(), 1, "an approved database runs");
    // A changed database (a pull) needs a new approval.
    write(&root.join("compile_commands.json"), &system_db.replace(r#""-c""#, r#""-O2", "-c""#));
    let (rep, _) = indexed_trusted(&root, &db, fixed(), CppTrust::Ask);
    assert_eq!(recorded(&bin).len(), 1, "not run again");
    assert!(note(&rep).contains("not approved"), "{}", note(&rep));
    // A compiler inside the repository is never run, even trusted.
    write(&root.join("tools/cc"), "#!/bin/sh\n");
    write(
        &root.join("compile_commands.json"),
        &system_db.replace(&cxx.to_string_lossy().into_owned(), &root.join("tools/cc").to_string_lossy()),
    );
    let (rep, _) = indexed_trusted(&root, &db, fixed(), CppTrust::Always);
    assert_eq!(recorded(&bin).len(), 1, "not run");
    assert!(note(&rep).contains("inside the repository"), "{}", note(&rep));
}

/// `atlas trust cpp ROOT` approves the root's databases as they are now.
#[test]
fn the_trust_command_approves_a_roots_databases() {
    let t = Temp::new("cpp-trust-cli");
    let root = t.path().join("repo");
    copy_sources(&root);
    write(&root.join("compile_commands.json"), "[]\n");
    let cfg = t.path().join("atlas.json");
    let state = t.path().join("state");
    write(
        &cfg,
        &format!(
            r#"{{"db": "{}", "vaults": [], "models": {{"auto_download": false, "files": []}}, "code": {{"roots": [{{"name": "svc", "root": "{}", "cpp": {{"trust": "ask"}}}}]}}}}"#,
            state.join("atlas.sqlite").display(),
            root.display()
        ),
    );
    let run = |args: &[&str]| {
        std::process::Command::new(bin())
            .arg("--config")
            .arg(&cfg)
            .args(args)
            .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
            .output()
            .unwrap()
    };
    let o = run(&["trust", "cpp", "svc"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{out} {}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("compile_commands.json"), "{out}");
    let trust = fs::read_to_string(state.join("code").join(atlas::cpp_trust::TRUST_FILE)).unwrap();
    assert!(trust.contains("svc:"), "{trust}");
    let o = run(&["trust", "cpp", "nope"]);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("no code root named nope"));
}

/// A stand-in scip-clang that copies `fixtures/scip/cpp/index.scip`, written
/// by scip-clang 0.4.1 from this fixture, to its output path.
#[cfg(unix)]
fn indexing_stand_in(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    fs::copy(cpp_fixture().join("index.scip"), dir.join("index.scip")).unwrap();
    let p = dir.join("scip-clang");
    fs::write(&p, "#!/bin/sh\n[ \"$1\" = --version ] && echo 'scip-clang 0.4.1' && exit 0\ncp \"$(dirname \"$0\")/index.scip\" \"$4\"\n")
        .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[cfg(unix)]
#[test]
fn a_scip_clang_index_gives_definitions_and_references() {
    let t = Temp::new("cpp-index");
    let root = t.path().join("repo");
    copy_sources(&root);
    write(&root.join("compile_commands.json"), "[]\n");
    let tool = indexing_stand_in(&t.path().join("bin"));
    let db = t.path().join("state/atlas.sqlite");
    let (rep, mut s) = indexed_trusted(&root, &db, Indexers::Fixed(vec![("scip-clang".into(), tool)]), CppTrust::Always);
    assert_eq!(rep.units.len(), 1, "{:?}", rep.units);
    assert_eq!(rep.units[0].source, "scip-clang 0.4.1");
    // The definitions and references are the index's, not the scanner's: a
    // call in the inactive `#if defined(FAST)` branch is no reference.
    let (defs, refs) = answer(&mut s, "area");
    assert_eq!(defs, ["shapes.cpp:8 method Circle", "shapes.hpp:31 method Shape"]);
    assert_eq!(refs, ["shapes.cpp:17 geo::total", "shapes.cpp:17 geo::total", "shapes.hpp:43 geo::Circle"]);
}
