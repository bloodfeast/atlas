//! The code index's language-server driver with a real server process:
//! the `atlas` test binary run as `atlas --stdio` with a script in
//! `ATLAS_TEST_LSP_FAKE` (the `test-seams` feature) stands in for
//! basedpyright. No real server runs here; one live test per installed
//! server runs only with `ATLAS_LSP_LIVE=1`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use atlas::code::{self, CodeSetup, Indexers, Reindexed};
use atlas::code_index::{self, RustSource};
use atlas::json;
use atlas::tools::{CodeState, Server};
use common::*;

fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

fn setup(root: &Path, db: &Path, indexers: Indexers) -> CodeSetup {
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec {
            name: "py".into(),
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
    code.indexers = indexers;
    code
}

fn asked(log: &Path) -> Vec<String> {
    fs::read_to_string(log).unwrap_or_default().lines().filter_map(|l| l.strip_prefix("ask ")).map(str::to_string).collect()
}

/// The definitions of `name`: `path:line kind container`.
fn defs(s: &mut Server, name: &str) -> Vec<String> {
    let v = s.call("symbol", &json!({ "name": name })).unwrap();
    v["defs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h: &json::Value| {
            format!("{}:{} {} {}", h["path"].as_str().unwrap(), h["line"], h["kind"].as_str().unwrap(), h["container"].as_str().unwrap())
        })
        .collect()
}

/// One test: the script is passed through the environment, which every
/// test of this binary would share.
#[test]
fn a_server_process_is_driven_killed_after_exit_and_never_started_on_edit() {
    let t = Temp::new("code-lsp");
    let root = t.path().join("svc");
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"svc\"\n").unwrap();
    fs::write(
        root.join("pkg/shapes.py"),
        "class Shape:\n    def area(self):\n        side = 2\n        return side\n\ndef largest():\n    pass\n",
    )
    .unwrap();
    let log = t.path().join("server.log");
    // It lingers a minute after `exit`: atlas kills it after five seconds.
    let script = format!(r#"{{"version": "7.0", "linger": true, "log": "{}"}}"#, log.to_string_lossy().replace('\\', "/"));
    std::env::set_var("ATLAS_TEST_LSP_FAKE", &script);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_atlas"));
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Chosen(vec![("scip-python".into(), None), ("basedpyright-langserver".into(), Some(exe))]));
    drop(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    let mut conn = atlas::db::Connection::open(&db).unwrap();
    let t0 = Instant::now();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    let took = t0.elapsed().as_secs_f64();
    let unit = rep.units.iter().find(|u| u.lang == "python").unwrap();
    assert_eq!(unit.source, "basedpyright 7.0 (lsp)");
    let (_, run) = rep.runs.iter().find(|(l, _)| l == "basedpyright-langserver").unwrap();
    assert!(run.seconds >= 4.5 && took < 40.0, "run {} s, index {took} s", run.seconds);
    assert_eq!(asked(&log), ["shapes.py"]);
    let store = atlas::store::Store::open(&db, vaults(&db)).unwrap();
    let mut s = Server::new(store);
    s.set_code(CodeState { setup: code.clone(), indexer: None });
    assert_eq!(defs(&mut s, "area"), ["pkg/shapes.py:2 method Shape"]);
    assert_eq!(defs(&mut s, "largest"), ["pkg/shapes.py:6 fn "]);
    // The local is no definition.
    assert!(defs(&mut s, "side").is_empty());
    // The recall check counts the server's share.
    let r = atlas::recall::code_check(&conn, &code.roots).unwrap();
    let py = r.langs.iter().find(|l| l.lang == "python").unwrap();
    assert_eq!((py.truth, py.indexed, py.lsp, py.scan_only, py.missing), (3, 3, 3, 0, 0));
    // An edit between full runs is re-scanned: no server starts.
    let p = root.join("pkg/shapes.py");
    fs::write(&p, "# moved\nclass Shape:\n    def area(self):\n        return 2\n\ndef largest():\n    pass\n").unwrap();
    assert!(matches!(code::reindex_file(&mut conn, &p.to_string_lossy()).unwrap(), Reindexed::Rows(_)));
    assert_eq!(asked(&log), ["shapes.py"]);
    // The scanner's row now (its kind says so).
    assert_eq!(defs(&mut s, "area"), ["pkg/shapes.py:3 method (scan) Shape"]);
}

/// A real server on a real file, when `ATLAS_LSP_LIVE=1` and the server is
/// found (`ATLAS_LSP_LIVE_BASEDPYRIGHT` may name its package folder).
#[test]
fn live_servers_answer() {
    if std::env::var("ATLAS_LSP_LIVE").as_deref() != Ok("1") {
        return;
    }
    let t = Temp::new("code-lsp-live");
    let root = t.path().join("svc");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"svc\"\n").unwrap();
    fs::write(root.join("shapes.py"), "class Shape:\n    def area(self):\n        return 2\n").unwrap();
    fs::write(root.join("go.mod"), "module example.com/svc\n\ngo 1.21\n").unwrap();
    fs::write(root.join("main.go"), "package main\n\ntype Engine struct{ n int }\n\nfunc (e *Engine) Use() {}\n\nfunc main() {}\n")
        .unwrap();
    let mut list: Vec<(String, Option<PathBuf>)> = vec![("scip-python".into(), None), ("scip-go".into(), None)];
    if let Some(p) = std::env::var_os("ATLAS_LSP_LIVE_BASEDPYRIGHT") {
        list.push(("basedpyright-langserver".into(), Some(PathBuf::from(p))));
    }
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, Indexers::Chosen(list));
    drop(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    let mut conn = atlas::db::Connection::open(&db).unwrap();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    for u in &rep.units {
        eprintln!("{} {}: {}", u.lang, u.dir, u.source);
    }
    assert!(rep.units.iter().any(|u| u.source.ends_with("(lsp)")), "{:?}", rep.units);
    let mut s = Server::new(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    s.set_code(CodeState { setup: code.clone(), indexer: None });
    let go = rep.units.iter().find(|u| u.lang == "go").unwrap();
    if go.source.ends_with("(lsp)") {
        assert_eq!(defs(&mut s, "Use"), ["main.go:5 method Engine"]);
        assert_eq!(defs(&mut s, "Engine"), ["main.go:3 struct "]);
    }
    let py = rep.units.iter().find(|u| u.lang == "python").unwrap();
    if py.source.ends_with("(lsp)") {
        assert_eq!(defs(&mut s, "area"), ["shapes.py:2 method Shape"]);
    }
}
