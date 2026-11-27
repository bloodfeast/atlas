//! The language-server driver's kill paths with a real server process: the
//! `atlas` test binary run as `atlas --stdio` with a script in
//! `ATLAS_TEST_LSP_FAKE` (the `test-seams` feature) stands in for the
//! server. One test sets the variable for this binary.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use atlas::code::{CodeSetup, Indexers};
use atlas::code_index::{self, RustSource};
use common::*;

fn setup(root: &Path, db: &Path, exe: PathBuf, timeout: Duration) -> CodeSetup {
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
    code.timeout = timeout;
    code.indexers = Indexers::Chosen(vec![("scip-python".into(), None), ("basedpyright-langserver".into(), Some(exe))]);
    code
}

/// The script goes through the environment, which the tests share.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn beats(log: &Path) -> usize {
    fs::read_to_string(log).unwrap_or_default().lines().filter(|l| *l == "beat").count()
}

#[test]
fn a_server_that_stops_reading_stdin_is_killed_at_the_deadline() {
    let _env = ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = Temp::new("code-lsp-kill");
    let root = t.path().join("svc");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"svc\"\n").unwrap();
    // A didOpen far past any pipe buffer: the write blocks once the server
    // stops reading.
    fs::write(root.join("big.py"), "x = 1\n".repeat(400_000)).unwrap();
    let log = t.path().join("server.log");
    std::env::set_var(
        "ATLAS_TEST_LSP_FAKE",
        format!(r#"{{"version": "7.0", "deaf": true, "log": "{}"}}"#, log.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")),
    );
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_atlas"));
    let db = t.path().join("state/atlas.sqlite");
    let code = setup(&root, &db, exe.clone(), Duration::from_secs(1));
    let mut conn = atlas::db::Connection::open_in_memory().unwrap();
    assert!(code_index::take_lease(&mut conn, "a", Duration::from_secs(600)).unwrap());
    // Indexed on a thread: a run that never returns fails here, not hangs.
    let (done, rx) = std::sync::mpsc::channel();
    let c2 = code.clone();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let rep = code_index::index_root(&c2, &mut conn, &c2.roots[0], &RustSource::ScanOnly("test".into()));
        let _ = done.send((rep.map(|r| (r.units, r.runs)), t0.elapsed(), conn));
    });
    let (rep, took, conn) = rx.recv_timeout(Duration::from_secs(60)).expect("index_root never returned: the server was not killed");
    let (units, runs) = rep.unwrap();
    // The timeout, the five seconds of grace, and slack.
    assert!(took < Duration::from_secs(25), "{took:?}");
    let unit = units.iter().find(|u| u.lang == "python").unwrap();
    assert!(unit.source.ends_with("passed the 1 s timeout"), "{}", unit.source);
    assert!(runs.iter().any(|(l, r)| l == "basedpyright-langserver" && r.timed_out), "{runs:?}");
    // The child is gone: it logs no more beats.
    let n = beats(&log);
    assert!(n > 0);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(beats(&log), n);
    // The lease is free for the next holder once its holder lets go.
    code_index::release_lease(&conn, "a").unwrap();
    let mut conn = conn;
    assert!(code_index::take_lease(&mut conn, "b", Duration::from_secs(600)).unwrap());
    // A version probe that never answers is cut at the same limit.
    let launch = atlas::langs::Launch::configured(&exe);
    assert_eq!(code_index::answer_text(&launch, "--say", Duration::from_secs(30)).as_deref().map(str::trim), Some("fake v1.2.3"));
    let t1 = Instant::now();
    assert_eq!(code_index::answer_text(&launch, "--stall", Duration::from_secs(1)), None);
    assert!(t1.elapsed() < Duration::from_secs(15), "{:?}", t1.elapsed());
}

/// A server that reads everything and never answers a symbol request is
/// killed at the timeout (a real process, killed with no grace).
#[test]
fn the_server_is_killed_at_the_timeout() {
    let _env = ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let t = Temp::new("code-lsp-hang");
    let root = t.path().join("svc");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname = \"svc\"\n").unwrap();
    fs::write(root.join("a.py"), "def f():\n    pass\n").unwrap();
    std::env::set_var("ATLAS_TEST_LSP_FAKE", r#"{"version": "7.0", "hang_after": 0}"#);
    let code = setup(&root, &t.path().join("state/atlas.sqlite"), PathBuf::from(env!("CARGO_BIN_EXE_atlas")), Duration::from_secs(1));
    let mut conn = atlas::db::Connection::open_in_memory().unwrap();
    let t0 = Instant::now();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
    let unit = rep.units.iter().find(|u| u.lang == "python").unwrap();
    assert!(unit.source.ends_with("passed the 1 s timeout"), "{}", unit.source);
    assert!(rep.runs.iter().any(|(l, r)| l == "basedpyright-langserver" && r.timed_out), "{:?}", rep.runs);
}
