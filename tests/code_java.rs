//! Java and Kotlin in the code index through the language table
//! (`src/langs.rs`): Maven and Gradle units and their skipped build
//! folders, the scip-java driver for a unit with Java files, a unit of
//! Kotlin alone left to the scanner (scip-java reads Kotlin only through a
//! plugin that does not load in Kotlin 2), and the recall check.
//!
//! No real scip-java runs here: the driver is pointed at a stand-in script
//! that records its arguments and copies the checked-in
//! `fixtures/scip/java/index.scip` (written by scip-java, see
//! `tests/code_scip.rs`) to the output path it is given.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use atlas::code::{self, CodeSetup, Indexers, Unit};
use atlas::code_index::{self, IndexReport, RustSource};
use atlas::json;
use atlas::tools::{CodeState, Server};
use common::*;

fn java_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("scip").join("java")
}

const SOURCES: &[&str] = &["pom.xml", "src/main/java/shapes/Shape.java", "src/main/java/shapes/Square.java", "src/main/java/shapes/Main.java"];

/// Copies the Java fixture's sources (not its index) into `to`.
fn copy_sources(to: &Path) {
    for f in SOURCES {
        let p = to.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::copy(java_fixture().join(f), &p).unwrap();
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

/// A stand-in scip-java in `dir`: appends its arguments to `args.txt`
/// beside it (one line per run), then copies the fixture index to the
/// argument after `--output` (the third).
fn stand_in(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    fs::copy(java_fixture().join("index.scip"), dir.join("index.scip")).unwrap();
    if cfg!(windows) {
        let p = dir.join("scip-java.cmd");
        fs::write(&p, "@echo %*>> \"%~dp0args.txt\"\r\n@copy /Y \"%~dp0index.scip\" %3 >NUL\r\n@exit /b 0\r\n").unwrap();
        p
    } else {
        let p = dir.join("scip-java");
        fs::write(&p, "#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/args.txt\"\ncp \"$(dirname \"$0\")/index.scip\" \"$3\"\n").unwrap();
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

/// `path:line kind container` for each definition of a name.
fn defs(s: &mut Server, name: &str) -> Vec<String> {
    let v = s.call("symbol", &json!({ "name": name })).unwrap();
    v["defs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| format!("{}:{} {} {}", h["path"].as_str().unwrap(), h["line"], h["kind"].as_str().unwrap(), h["container"].as_str().unwrap()))
        .collect()
}

fn units(w: &code::Walk) -> Vec<(String, String)> {
    code::units(w).iter().map(|u| (u.lang.name.to_string(), u.dir.clone())).collect()
}

#[test]
fn maven_and_gradle_files_make_units_and_build_folders_are_skipped() {
    let t = Temp::new("java-walk");
    let root = t.path().join("repo");
    write(&root.join("server/pom.xml"), "<project><modules><module>core</module></modules></project>\n");
    write(&root.join("server/core/pom.xml"), "<project/>\n");
    write(&root.join("server/core/src/main/java/a/Core.java"), "package a;\npublic class Core {}\n");
    write(&root.join("server/core/target/classes/a/Gen.java"), "class Gen {}\n");
    write(&root.join("app/settings.gradle.kts"), "include(\":lib\")\n");
    write(&root.join("app/lib/build.gradle.kts"), "plugins { kotlin(\"jvm\") }\n");
    write(&root.join("app/lib/src/main/kotlin/a/Lib.kt"), "package a\nclass Lib\n");
    write(&root.join("app/lib/build/generated/a/Gen.kt"), "class Gen\n");
    write(&root.join("app/.gradle/x/Cache.java"), "class Cache {}\n");
    let w = code::walk(&root).unwrap();
    let paths = |l: &[code::WalkedFile]| l.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
    // Kotlin build scripts are Kotlin source and markers both.
    assert_eq!(
        paths(&w.source),
        ["app/lib/build.gradle.kts", "app/lib/src/main/kotlin/a/Lib.kt", "app/settings.gradle.kts", "server/core/src/main/java/a/Core.java"]
    );
    assert_eq!(paths(&w.markers), ["server/core/pom.xml", "server/pom.xml"]);
    // A settings file claims its builds; a parent POM its modules.
    assert_eq!(units(&w), [("java".to_string(), "app".to_string()), ("java".to_string(), "server".to_string())]);
    assert!(code::relevant("server/pom.xml") && code::relevant("app/gradle.properties") && code::relevant("app/lib/src/main/kotlin/a/Lib.kt"));
}

#[test]
fn a_kotlin_unit_is_scanned_while_a_java_unit_runs_scip_java() {
    let t = Temp::new("java-driver");
    let root = t.path().join("svc");
    copy_sources(&root.join("server"));
    write(&root.join("tool/settings.gradle.kts"), "rootProject.name = \"tool\"\n");
    write(&root.join("tool/src/main/kotlin/tool/Cli.kt"), "package tool\n\nclass Cli(val name: String) {\n  fun run(): Int {\n    return 0\n  }\n}\n");
    let bin = stand_in(&t.path().join("bin"));
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Fixed(vec![("scip-java".into(), bin.clone())]));
    let (rep, mut s) = indexed(&code, &db);
    assert_eq!(
        rep.units,
        [
            Unit { dir: "server".into(), lang: "java".into(), source: "scip-java HEAD-20260402-1750".into() },
            Unit {
                dir: "tool".into(),
                lang: "kotlin".into(),
                source: "scan only: scip-java reads Kotlin through semanticdb-kotlinc, which supports Kotlin 1.8 and 1.9 only".into()
            },
        ]
    );
    assert_eq!(recorded(bin.parent().unwrap()), [["index", "--output"]], "run once, for the Java unit");
    assert_eq!(defs(&mut s, "unit"), ["server/src/main/java/shapes/Square.java:42 method Square"]);
    assert_eq!(defs(&mut s, "run"), ["tool/src/main/kotlin/tool/Cli.kt:4 method (scan) Cli"]);
    assert_eq!(defs(&mut s, "name"), ["tool/src/main/kotlin/tool/Cli.kt:3 property (scan) Cli"]);
}

#[test]
fn the_scanner_alone_holds_the_recall_truth() {
    let t = Temp::new("java-scan");
    let root = t.path().join("svc");
    copy_sources(&root);
    write(&root.join("src/main/kotlin/shapes/Extra.kt"), "package shapes\n\nconst val SIDES = 4\n\nfun Square.double(): Square = Square(2.0)\n");
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Off("test".into()));
    let (rep, mut s) = indexed(&code, &db);
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "java".into(), source: "scan only: test".into() }]);
    assert_eq!(defs(&mut s, "Square"), ["src/main/java/shapes/Square.java:6 class (scan) shapes", "src/main/java/shapes/Square.java:19 ctor (scan) Square"]);
    assert_eq!(defs(&mut s, "double"), ["src/main/kotlin/shapes/Extra.kt:5 fn (scan) shapes"]);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let r = atlas::recall::code_check(&conn, &code.roots).unwrap();
    let got: Vec<(String, usize, usize, usize)> = r.langs.iter().map(|l| (l.lang.clone(), l.truth, l.scan_only, l.missing)).collect();
    assert_eq!(got, [("java".to_string(), 14, 14, 0), ("kotlin".to_string(), 2, 2, 0)]);
}
