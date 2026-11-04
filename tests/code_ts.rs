//! TypeScript and JavaScript in the code index through the language table
//! (`src/langs.rs`): the unit walk (`tsconfig.json`, `package.json`, the
//! shallowest claiming its subtree), the scip-typescript driver (its
//! `node_modules` prerequisite, its projects: a `tsconfig.json` folder as it
//! is, workspace members, and the configuration atlas infers into its own
//! folder for a package without one), the scanner when no indexer runs or
//! for files the index lacks, the per-file re-scan and the recall check.
//!
//! No real scip-typescript runs here: the driver is pointed at a stand-in
//! script that records its arguments and copies the checked-in
//! `fixtures/scip/ts/index.scip` (written by scip-typescript 0.5.2, see
//! `tests/code_scip.rs`) to the output path it is given, or exits with
//! failure.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use atlas::code::{self, CodeSetup, Indexers, Reindexed, Unit};
use atlas::code_index::{self, IndexReport, RustSource};
use atlas::json;
use atlas::tools::{CodeState, Server};
use common::*;

fn ts_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("scip").join("ts")
}

/// Copies the TypeScript fixture's sources (not its index) into `to`.
fn copy_sources(to: &Path) {
    for f in ["package.json", "tsconfig.json", "src/main.ts", "src/shapes.ts"] {
        let p = to.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::copy(ts_fixture().join(f), p).unwrap();
    }
}

fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

fn setup(root: &Path, db: &Path, indexers: Indexers) -> CodeSetup {
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec { name: "web".into(), root: root.to_path_buf(), build_scripts: false, scip: Default::default() }],
        debounce_s: Some(0),
        ..Default::default()
    };
    let mut code = CodeSetup::from_spec(&spec, db, false);
    code.defer_on_cargo = false;
    code.indexers = indexers;
    code
}

/// A stand-in scip-typescript in `dir`: writes its arguments to
/// `args.txt` beside it (one run's, appended), then copies the fixture
/// index to its fourth argument (`index --no-progress-bar --output FILE`),
/// or exits 1 when `fail`.
fn stand_in(dir: &Path, fail: bool) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    fs::copy(ts_fixture().join("index.scip"), dir.join("index.scip")).unwrap();
    if cfg!(windows) {
        let p = dir.join("scip-typescript.cmd");
        let run = if fail { "@exit /b 1\r\n" } else { "@copy /Y \"%~dp0index.scip\" %4 >NUL\r\n@exit /b 0\r\n" };
        fs::write(&p, format!("@echo %*>> \"%~dp0args.txt\"\r\n{run}")).unwrap();
        p
    } else {
        let p = dir.join("scip-typescript");
        let run = if fail { "exit 1\n" } else { "cp \"$(dirname \"$0\")/index.scip\" \"$4\"\n" };
        fs::write(&p, format!("#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/args.txt\"\n{run}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

/// Each recorded run's arguments after the output file, `/`-separated.
fn recorded(dir: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(dir.join("args.txt"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.split_whitespace().skip(4).map(|a| a.trim_matches('"').replace('\\', "/")).collect())
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
fn the_walk_lists_scripts_and_the_shallowest_package_claims_its_subtree() {
    let t = Temp::new("ts-walk");
    let root = t.path().join("repo");
    copy_sources(&root.join("web"));
    write(&root.join("web/packages/ui/package.json"), "{\"name\": \"ui\"}\n");
    write(&root.join("web/packages/ui/button.tsx"), "export const Button = 1;\n");
    write(&root.join("web/node_modules/dep/index.js"), "module.exports = 1;\n");
    write(&root.join("web/dist/main.js"), "var x = 1;\n");
    write(&root.join("tools/package.json"), "{\"name\": \"tools\"}\n");
    write(&root.join("tools/run.mjs"), "export function run() {}\n");
    write(&root.join("site/package.json"), "{\"name\": \"site\"}\n");
    write(&root.join("site/README.md"), "# Site\n");
    let w = code::walk(&root).unwrap();
    let paths = |l: &[code::WalkedFile]| l.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
    assert_eq!(paths(&w.source), ["tools/run.mjs", "web/packages/ui/button.tsx", "web/src/main.ts", "web/src/shapes.ts"]);
    assert_eq!(paths(&w.mention), ["web/dist/main.js"], "build output is searched for rpc call sites only");
    let units: Vec<(String, String)> = code::units(&w).iter().map(|u| (u.lang.name.to_string(), u.dir.clone())).collect();
    assert_eq!(
        units,
        [
            ("typescript".to_string(), "site".to_string()),
            ("typescript".to_string(), "tools".to_string()),
            ("typescript".to_string(), "web".to_string())
        ],
        "web/packages/ui is claimed by web"
    );
    assert!(code::relevant("web/tsconfig.json") && code::relevant("web/pnpm-lock.yaml") && code::relevant("web/a.cts"));
    // A package of JavaScript alone is a javascript unit; one without any
    // source is not run at all.
    let bin = stand_in(&t.path().join("bin"), true);
    let (rep, _) = indexed(&root, &t.path().join("state/atlas.sqlite"), Indexers::Fixed(vec![("scip-typescript".into(), bin.clone())]));
    assert_eq!(
        rep.units,
        [
            Unit { dir: "tools".into(), lang: "javascript".into(), source: "scan only: scip-typescript exited with Some(1)".into() },
            Unit { dir: "web".into(), lang: "typescript".into(), source: "scan only: scip-typescript exited with Some(1)".into() },
        ]
    );
    assert_eq!(recorded(bin.parent().unwrap()).len(), 2, "site holds no source and is not run");
}

#[test]
fn the_scanner_alone_gives_definitions_and_ranges() {
    let t = Temp::new("ts-scan");
    let root = t.path().join("web");
    copy_sources(&root);
    let db = t.path().join("state/atlas.sqlite");
    let (rep, mut s) = indexed(&root, &db, Indexers::Off("test".into()));
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "typescript".into(), source: "scan only: test".into() }]);
    assert!(rep.runs.is_empty());
    let (defs, refs) = answer(&mut s, "Square");
    assert_eq!(defs, ["src/shapes.ts:7 class (scan) shapes", "src/shapes.ts:8 ctor (scan) Square"]);
    assert!(refs.is_empty(), "the scanner gives definitions only");
    assert_eq!(
        labels(&db, &root.join("src/shapes.ts")),
        [
            "interface shapes::Shape 2-4",
            "fn Shape::area 3-3",
            "class shapes::Square 7-13",
            "fn Square::Square 8-8",
            "fn Square::area 10-12",
            "fn shapes::total 16-22"
        ]
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    let r = atlas::recall::code_check(&conn, &setup(&root, &db, Indexers::Search).roots).unwrap();
    assert_eq!(
        r.langs,
        [atlas::recall::LangRecall { lang: "typescript".into(), files: 2, truth: 5, excluded: 0, indexed: 0, scan_only: 5, missing: 0 }]
    );
}

#[test]
fn a_found_indexer_runs_once_per_package_and_the_scanner_covers_the_rest() {
    let t = Temp::new("ts-driver");
    let root = t.path().join("web");
    copy_sources(&root);
    // A test file outside the tsconfig's `include`, which the index lacks.
    write(
        &root.join("test/shapes.test.ts"),
        "import {total} from '../src/shapes';\n\nexport function check(): boolean {\n  return total([]) === 0;\n}\n",
    );
    let bin = stand_in(&t.path().join("bin"), false);
    let db = t.path().join("state/atlas.sqlite");
    let (rep, mut s) = indexed(&root, &db, Indexers::Fixed(vec![("scip-typescript".into(), bin.clone())]));
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "typescript".into(), source: "scip-typescript 0.5.2".into() }]);
    assert_eq!(rep.rust, "typescript: scip-typescript 0.5.2");
    assert_eq!(recorded(bin.parent().unwrap()), [Vec::<String>::new()], "a folder with a tsconfig.json is read as it is");
    let (defs, refs) = answer(&mut s, "Square");
    assert_eq!(defs, ["src/shapes.ts:7 class shapes", "src/shapes.ts:8 ctor Square"]);
    assert!(refs.contains(&"src/main.ts:6 main::report".to_string()), "{refs:?}");
    let (defs, _) = answer(&mut s, "check");
    assert_eq!(defs, ["test/shapes.test.ts:3 fn (scan) shapes.test"]);
    assert!(t.path().join("state/code/web.scip-typescript.scip").is_file());
    let conn = rusqlite::Connection::open(&db).unwrap();
    let r = atlas::recall::code_check(&conn, &setup(&root, &db, Indexers::Search).roots).unwrap();
    assert_eq!((r.langs[0].truth, r.langs[0].indexed, r.langs[0].scan_only, r.langs[0].missing), (6, 5, 1, 0));
}

#[test]
fn a_package_without_tsconfig_gets_one_inferred_outside_the_tree() {
    let t = Temp::new("ts-infer");
    let root = t.path().join("cart");
    write(&root.join("package.json"), "{\"name\": \"cart\", \"dependencies\": {}}\n");
    write(&root.join("server.js"), "function main() {}\nmodule.exports = { main };\n");
    let bin = stand_in(&t.path().join("bin"), true);
    let db = t.path().join("state/atlas.sqlite");
    let (rep, mut s) = indexed(&root, &db, Indexers::Fixed(vec![("scip-typescript".into(), bin.clone())]));
    assert_eq!(rep.units[0].lang, "javascript");
    let runs = recorded(bin.parent().unwrap());
    assert_eq!(runs.len(), 1);
    let inferred = PathBuf::from(&runs[0][0]);
    assert_eq!(runs[0].len(), 1);
    assert!(inferred.starts_with(t.path().join("state/code").to_string_lossy().replace('\\', "/")), "{inferred:?}");
    let text = fs::read_to_string(&inferred).unwrap();
    let v = json::parse(&text).unwrap();
    assert_eq!(v["compilerOptions"]["allowJs"].as_bool(), Some(true));
    assert_eq!(v["include"][0].as_str().unwrap(), format!("{}/**/*", root.to_string_lossy().replace('\\', "/")));
    assert!(!root.join("tsconfig.json").exists(), "nothing is written into the project");
    let (defs, _) = answer(&mut s, "main");
    assert_eq!(defs, ["server.js:1 fn (scan) server"]);
}

#[test]
fn missing_node_modules_and_workspaces() {
    let t = Temp::new("ts-prereq");
    let root = t.path().join("mono");
    write(
        &root.join("package.json"),
        "{\"name\": \"mono\", \"workspaces\": [\"packages/*\", \"!packages/old\"], \"devDependencies\": {\"typescript\": \"5\"}}\n",
    );
    write(&root.join("tsconfig.json"), "{}\n");
    for p in ["a", "b", "old"] {
        write(&root.join(format!("packages/{p}/package.json")), &format!("{{\"name\": \"{p}\"}}\n"));
        write(&root.join(format!("packages/{p}/index.ts")), "export const x = 1;\n");
    }
    // A member's configuration extends a package: without node_modules it
    // cannot load, so the unit is not run.
    write(&root.join("packages/a/tsconfig.json"), "{\n  // shared settings\n  \"extends\": \"@acme/tsconfig\",\n}\n");
    let bin = stand_in(&t.path().join("bin"), true);
    let fixed = || Indexers::Fixed(vec![("scip-typescript".into(), bin.clone())]);
    let (rep, _) = indexed(&root, &t.path().join("state/atlas.sqlite"), fixed());
    assert_eq!(
        rep.units,
        [Unit { dir: String::new(), lang: "typescript".into(), source: "scan only: no node_modules: run npm install".into() }]
    );
    assert!(recorded(bin.parent().unwrap()).is_empty(), "not run: a tsconfig.json extends a package");
    // Without that, it runs anyway; the fix is named only because it failed.
    write(&root.join("packages/a/tsconfig.json"), "{\"extends\": \"../../tsconfig.json\"}\n");
    let (rep, _) = indexed(&root, &t.path().join("state1/atlas.sqlite"), fixed());
    assert_eq!(rep.units[0].source, "scan only: scip-typescript exited with Some(1); no node_modules: run npm install");
    assert_eq!(recorded(bin.parent().unwrap()).len(), 1);
    let ok = stand_in(&t.path().join("ok"), false);
    let (rep, _) = indexed(&root, &t.path().join("state3/atlas.sqlite"), Indexers::Fixed(vec![("scip-typescript".into(), ok)]));
    assert_eq!(rep.units[0].source, "scip-typescript 0.5.2", "a run that succeeds says nothing of node_modules");
    fs::remove_file(bin.parent().unwrap().join("args.txt")).unwrap();
    fs::create_dir_all(root.join("node_modules")).unwrap();
    let (rep, _) = indexed(&root, &t.path().join("state2/atlas.sqlite"), fixed());
    assert_eq!(rep.units[0].source, "scan only: scip-typescript exited with Some(1)");
    let runs = recorded(bin.parent().unwrap());
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0][..2], [".".to_string(), "packages/a".to_string()], "the root, then each member with its own tsconfig");
    assert_eq!(runs[0].len(), 3, "{runs:?}");
    assert!(runs[0][2].ends_with(".tsconfig.json") && fs::read_to_string(&runs[0][2]).unwrap().contains("/packages/b/**/*"), "{runs:?}");
}

#[test]
fn an_edited_typescript_file_is_rescanned_at_once() {
    let t = Temp::new("ts-reindex");
    let root = t.path().join("web");
    copy_sources(&root);
    let bin = stand_in(&t.path().join("bin"), false);
    let db = t.path().join("state/atlas.sqlite");
    let (_, mut s) = indexed(&root, &db, Indexers::Fixed(vec![("scip-typescript".into(), bin)]));
    let (_, refs) = answer(&mut s, "area");
    assert_eq!(refs, ["src/shapes.ts:19 shapes::total"]);
    let p = root.join("src/shapes.ts");
    let text = format!("export const VERSION = \"1\";\n\n{}", fs::read_to_string(&p).unwrap());
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&p, text).unwrap();
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    assert!(matches!(code::reindex_file(&mut conn, &p.to_string_lossy()).unwrap(), Reindexed::Rows(_)));
    let labels = labels(&db, &p);
    assert_eq!(labels[..2], ["var shapes::VERSION 1-1", "interface shapes::Shape 4-6"]);
    let (defs, refs) = answer(&mut s, "area");
    assert_eq!(defs, ["src/shapes.ts:5 method (scan) Shape", "src/shapes.ts:12 method (scan) Square"]);
    assert!(refs.is_empty(), "references come back with the next full run");
    let lang: String = conn.query_row("SELECT lang FROM code_files WHERE path = 'src/shapes.ts'", [], |r| r.get(0)).unwrap();
    assert_eq!(lang, "typescript");
}
