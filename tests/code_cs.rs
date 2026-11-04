//! C# in the code index through the language table (`src/langs.rs`): the
//! unit walk (a solution claims its folder's projects, a project without
//! one is its own unit), the scip-dotnet driver (the solution passed as its
//! argument, its environment, the `global.json` SDK named when a run
//! fails), the scanner when no indexer runs, its definitions filled in on
//! lines the index has none (an inactive `#if` branch), the per-file
//! re-scan, the recall check, and documents another unit's index already
//! gave.
//!
//! No real scip-dotnet runs here: the driver is pointed at a stand-in
//! script that records its arguments and copies the checked-in
//! `fixtures/scip/cs/index.scip` (written by scip-dotnet 0.2.16, see
//! `tests/code_scip.rs`) to the output path it is given, or exits with
//! failure.

mod common;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use atlas::code::{self, CodeSetup, Indexers, Reindexed, Rows, Unit};
use atlas::code_index::{self, IndexReport, RustSource};
use atlas::json;
use atlas::tools::{CodeState, Server};
use common::*;

fn cs_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("scip").join("cs")
}

/// Copies the C# fixture's sources (not its index) into `to`.
fn copy_sources(to: &Path) {
    fs::create_dir_all(to).unwrap();
    for f in ["Shapes.cs", "Shapes.csproj"] {
        fs::copy(cs_fixture().join(f), to.join(f)).unwrap();
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

/// A stand-in scip-dotnet in `dir`: writes its arguments, and the value of
/// `MSBUILDDISABLENODEREUSE` it was given, to `args.txt` beside it (one
/// line per run), then copies the fixture index to its third argument
/// (`index --output FILE ...`), or exits 1 when `fail`.
fn stand_in(dir: &Path, fail: bool) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    fs::copy(cs_fixture().join("index.scip"), dir.join("index.scip")).unwrap();
    if cfg!(windows) {
        let p = dir.join("scip-dotnet.cmd");
        let run = if fail { "@exit /b 1\r\n" } else { "@copy /Y \"%~dp0index.scip\" %3 >NUL\r\n@exit /b 0\r\n" };
        fs::write(&p, format!("@echo reuse=%MSBUILDDISABLENODEREUSE% %*>> \"%~dp0args.txt\"\r\n{run}")).unwrap();
        p
    } else {
        let p = dir.join("scip-dotnet");
        let run = if fail { "exit 1\n" } else { "cp \"$(dirname \"$0\")/index.scip\" \"$3\"\n" };
        fs::write(&p, format!("#!/bin/sh\necho \"reuse=$MSBUILDDISABLENODEREUSE $@\" >> \"$(dirname \"$0\")/args.txt\"\n{run}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

/// Each recorded run: the environment value, then the arguments after the
/// output file.
fn recorded(dir: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(dir.join("args.txt"))
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let mut w = l.split_whitespace();
            let env = w.next().unwrap_or("").to_string();
            std::iter::once(env).chain(w.skip(3).map(|a| a.trim_matches('"').to_string())).collect()
        })
        .collect()
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
fn a_solution_claims_its_projects_and_build_output_is_skipped() {
    let t = Temp::new("cs-walk");
    let root = t.path().join("repo");
    write(&root.join("App.sln"), "\n");
    write(&root.join("src/App/App.csproj"), "<Project Sdk=\"Microsoft.NET.Sdk\" />\n");
    write(&root.join("src/App/Program.cs"), "class Program { static void Main() { } }\n");
    write(&root.join("src/App/obj/Debug/net8.0/App.AssemblyInfo.cs"), "class Generated { }\n");
    write(&root.join("src/App/bin/Debug/Stale.cs"), "class Stale { }\n");
    write(&root.join("tests/App.Tests/App.Tests.csproj"), "<Project Sdk=\"Microsoft.NET.Sdk\" />\n");
    write(&root.join("tests/App.Tests/ProgramTests.cs"), "public class ProgramTests { }\n");
    write(&root.join("Directory.Build.props"), "<Project />\n");
    let w = code::walk(&root).unwrap();
    let paths = |l: &[code::WalkedFile]| l.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
    assert_eq!(paths(&w.source), ["src/App/Program.cs", "tests/App.Tests/ProgramTests.cs"]);
    assert_eq!(paths(&w.markers), ["App.sln", "Directory.Build.props", "src/App/App.csproj", "tests/App.Tests/App.Tests.csproj"]);
    let units = |w: &code::Walk| code::units(w).iter().map(|u| (u.lang.name.to_string(), u.dir.clone())).collect::<Vec<_>>();
    assert_eq!(units(&w), [("csharp".to_string(), String::new())], "the solution claims both projects");
    assert!(code::relevant("src/App/App.csproj") && code::relevant("global.json") && code::relevant("Directory.Build.props"));
    // Without the solution each project is its own unit.
    fs::remove_file(root.join("App.sln")).unwrap();
    let w = code::walk(&root).unwrap();
    assert_eq!(units(&w), [("csharp".to_string(), "src/App".to_string()), ("csharp".to_string(), "tests/App.Tests".to_string())]);
}

#[test]
fn the_scanner_alone_gives_definitions_and_ranges() {
    let t = Temp::new("cs-scan");
    let root = t.path().join("svc");
    copy_sources(&root);
    let db = t.path().join("state/atlas.sqlite");
    let (rep, mut s) = indexed(&root, &db, Indexers::Off("test".into()));
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "csharp".into(), source: "scan only: test".into() }]);
    let (defs, refs) = answer(&mut s, "Square");
    assert_eq!(defs, ["Shapes.cs:10 class (scan) Shapes", "Shapes.cs:12 ctor (scan) Square"]);
    assert!(refs.is_empty(), "the scanner gives definitions only");
    assert_eq!(
        labels(&db, &root.join("Shapes.cs")),
        [
            "interface Shapes::IShape 4-7",
            "fn IShape::Area 6-6",
            "class Shapes::Square 10-23",
            "fn Square::Square 12-15",
            "property Square::Side 17-17",
            "fn Square::Area 19-22",
            "class Shapes::Sums 25-36",
            "fn Sums::Total 27-35"
        ]
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    let r = atlas::recall::code_check(&conn, &setup(&root, &db, Indexers::Search).roots).unwrap();
    assert_eq!(
        r.langs,
        [atlas::recall::LangRecall { lang: "csharp".into(), files: 1, truth: 7, excluded: 0, indexed: 0, scan_only: 7, missing: 0 }]
    );
}

#[test]
fn a_found_indexer_runs_on_the_solution_and_the_scanner_fills_inactive_branches() {
    let t = Temp::new("cs-driver");
    let root = t.path().join("svc");
    copy_sources(&root);
    write(&root.join("Shapes.sln"), "\n");
    write(&root.join("Other.slnx"), "<Solution />\n");
    // A branch the indexer's build left out, after the indexed lines.
    let p = root.join("Shapes.cs");
    let text = fs::read_to_string(&p).unwrap().replace("\r\n", "\n");
    let at = text.trim_end().rfind('}').unwrap();
    let extra = "#if NEVER_DEFINED\n    public static class Extra\n    {\n        public static int Count() => 0;\n    }\n#endif\n";
    fs::write(&p, format!("{}{extra}{}", &text[..at], &text[at..])).unwrap();
    let bin = stand_in(&t.path().join("bin"), false);
    let db = t.path().join("state/atlas.sqlite");
    let (rep, mut s) = indexed(&root, &db, Indexers::Fixed(vec![("scip-dotnet".into(), bin.clone())]));
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "csharp".into(), source: "scip-dotnet 0.1.0-SNAPSHOT".into() }]);
    assert_eq!(recorded(bin.parent().unwrap()), [vec!["reuse=1".to_string(), "Shapes.sln".to_string()]], "the solution, not the slnx");
    let (defs, refs) = answer(&mut s, "Square");
    assert_eq!(defs, ["Shapes.cs:10 class Shapes", "Shapes.cs:12 ctor Square"]);
    assert!(refs.is_empty() || refs.iter().all(|r| r.starts_with("Shapes.cs:")), "{refs:?}");
    let (defs, _) = answer(&mut s, "Area");
    assert_eq!(defs, ["Shapes.cs:6 method IShape", "Shapes.cs:19 method Square"]);
    // The inactive branch: scanner rows on lines the index has none.
    let (defs, _) = answer(&mut s, "Extra");
    assert_eq!(defs, ["Shapes.cs:38 class (scan) Shapes"]);
    let (defs, _) = answer(&mut s, "Count");
    assert_eq!(defs, ["Shapes.cs:40 method (scan) Extra"]);
    // Ranges: scip-dotnet writes no enclosing ranges; the rows take the
    // brace matcher's, else the scanner's.
    let l = labels(&db, &p);
    assert!(l.contains(&"class Shapes::Square 10-23".to_string()) && l.contains(&"class Shapes::Extra 38-41".to_string()), "{l:?}");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let r = atlas::recall::code_check(&conn, &setup(&root, &db, Indexers::Search).roots).unwrap();
    assert_eq!(
        (r.langs[0].truth, r.langs[0].indexed, r.langs[0].scan_only, r.langs[0].missing),
        (9, 7, 2, 0),
        "interface members carry no access modifier"
    );
}

#[test]
fn a_failed_run_names_the_sdk_global_json_asks_for() {
    let t = Temp::new("cs-fail");
    let root = t.path().join("repo");
    write(&root.join("global.json"), "{\"sdk\": {\"version\": \"10.0.100\", \"rollForward\": \"latestFeature\"}}\n");
    copy_sources(&root.join("src/Shapes"));
    let bin = stand_in(&t.path().join("bin"), true);
    let (rep, mut s) = indexed(&root, &t.path().join("state/atlas.sqlite"), Indexers::Fixed(vec![("scip-dotnet".into(), bin.clone())]));
    assert_eq!(
        rep.units,
        [Unit {
            dir: "src/Shapes".into(),
            lang: "csharp".into(),
            source: "scan only: scip-dotnet exited with Some(1); global.json asks for .NET SDK 10.0.100".into()
        }]
    );
    assert_eq!(recorded(bin.parent().unwrap())[0][1], "Shapes.csproj", "a project without a solution is passed itself");
    let (defs, _) = answer(&mut s, "Total");
    assert_eq!(defs, ["src/Shapes/Shapes.cs:27 method (scan) Sums"]);
    // Off, or not found: the note says so.
    let (rep, _) = indexed(&root, &t.path().join("state2/atlas.sqlite"), Indexers::Chosen(vec![("scip-dotnet".into(), None)]));
    assert_eq!(rep.units[0].source, "scan only: scip-dotnet is off (code.indexers)");
}

#[test]
fn an_edited_csharp_file_is_rescanned_at_once() {
    let t = Temp::new("cs-reindex");
    let root = t.path().join("svc");
    copy_sources(&root);
    let bin = stand_in(&t.path().join("bin"), false);
    let db = t.path().join("state/atlas.sqlite");
    let (_, mut s) = indexed(&root, &db, Indexers::Fixed(vec![("scip-dotnet".into(), bin)]));
    let (_, refs) = answer(&mut s, "Area");
    assert_eq!(refs, ["Shapes.cs:32 Sums::Total"]);
    let p = root.join("Shapes.cs");
    let text = format!("// A header line.\n{}", fs::read_to_string(&p).unwrap());
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&p, text).unwrap();
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    assert!(matches!(code::reindex_file(&mut conn, &p.to_string_lossy()).unwrap(), Reindexed::Rows(_)));
    assert_eq!(labels(&db, &p)[..2], ["interface Shapes::IShape 5-8", "fn IShape::Area 7-7"]);
    let (defs, refs) = answer(&mut s, "Area");
    assert_eq!(defs, ["Shapes.cs:7 method (scan) IShape", "Shapes.cs:20 method (scan) Square"]);
    assert!(refs.is_empty(), "references come back with the next full run");
    let lang: String = conn.query_row("SELECT lang FROM code_files WHERE path = 'Shapes.cs'", [], |r| r.get(0)).unwrap();
    assert_eq!(lang, "csharp");
}

#[test]
fn documents_another_unit_already_gave_are_not_read_twice() {
    let buf = fs::read(cs_fixture().join("index.scip")).unwrap();
    let ix = atlas::scip::decode(&buf, atlas::scip::Options::default()).unwrap();
    let text = fs::read_to_string(cs_fixture().join("Shapes.cs")).unwrap();
    let read = |p: &str| (p.ends_with("Shapes.cs")).then(|| text.clone());
    let mut rows = Rows::default();
    let mut files: HashMap<String, u32> = HashMap::new();
    code::scip_rows_in(&ix, "a", &HashSet::new(), &mut rows, &mut files, &read);
    let first = rows.occ.len();
    assert!(first > 0);
    // A second unit whose index holds the same document (a referenced
    // project) adds nothing for it.
    let skip: HashSet<String> = ["a/Shapes.cs".to_string()].into();
    code::scip_rows_in(&ix, "a", &skip, &mut rows, &mut files, &read);
    assert_eq!(rows.occ.len(), first);
    // Paths beside the unit are taken below the root; above it, left out.
    assert_eq!(code::rebase("tests/App.Tests", "../../src/App/Program.cs"), "src/App/Program.cs");
    assert_eq!(code::rebase("tests", "./x/../y.cs"), "tests/y.cs");
    assert_eq!(code::rebase("", "../outside.cs"), "../outside.cs");
    assert_eq!(code::rebase("a", "b\\c.cs"), "a/b/c.cs");
}
