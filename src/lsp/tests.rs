//! The language-server driver's tests: the client over in-memory pipes
//! against the scripted server ([`super::fake`]), the symbol reading on
//! hand-written answers, and whole index runs of a small Python root with
//! the scripted server standing in for basedpyright.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::*;
use crate::code::{CodeSetup, Indexers};
use crate::code_index::{index_root, IndexReport, RustSource};

/// A client and session on a scripted server.
fn session(script: &str) -> Result<Session<Out>, Stop> {
    let name = fake::register(script);
    let (r, w) = fake::start(name.strip_prefix(fake::SCHEME).unwrap()).unwrap();
    let ask = Ask { root: Path::new("H:/x"), language_id: "python", init_options: "", deadline: Instant::now() + Duration::from_secs(20) };
    Session::start(Client::new(r, w), &ask)
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("atlas-lsp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn json_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[test]
fn frames_round_trip() {
    let mut buf = Vec::new();
    write_frame(&mut buf, r#"{"a":1}"#).unwrap();
    write_frame(&mut buf, "{\"b\":\"\u{e9}\"}").unwrap();
    assert!(buf.starts_with(b"Content-Length: 7\r\n\r\n{\"a\":1}"));
    let mut r = std::io::BufReader::new(&buf[..]);
    assert_eq!(read_frame(&mut r).unwrap().unwrap(), br#"{"a":1}"#);
    assert_eq!(read_frame(&mut r).unwrap().unwrap(), "{\"b\":\"\u{e9}\"}".as_bytes());
    assert_eq!(read_frame(&mut r).unwrap(), None);
    // Other headers are read past; the name's case does not matter.
    let mut r = std::io::BufReader::new(&b"content-type: x\r\ncontent-length: 2\r\n\r\n{}"[..]);
    assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"{}");
}

#[test]
fn a_bad_frame_scans_without_panic() {
    for bad in [&b"Content-Length: nope\r\n\r\n{}"[..], b"\r\n{}", b"no colon\r\n\r\n", b"Content-Length: 9\r\n"] {
        assert!(read_frame(&mut std::io::BufReader::new(bad)).is_err(), "{:?}", String::from_utf8_lossy(bad));
    }
    let mut s = session(r#"{"bad_frame": true}"#).ok().unwrap();
    let files = vec![(PathBuf::from("H:/x/a.py"), "def f():\n    pass\n".to_string())];
    let r = s.document_symbols(&files, Instant::now() + Duration::from_secs(20), |_, _| panic!("no answer"));
    assert!(matches!(r, Err(Stop::Bad(_))), "{r:?}");
    assert_eq!(
        stop_note("pyright-langserver", &Stop::Bad("x".into()), Duration::from_secs(9)),
        "scan only: pyright-langserver sent a bad frame"
    );
}

#[test]
fn server_requests_are_answered() {
    let d = temp("requests");
    let log = d.join("log.txt");
    let s = session(&format!(r#"{{"ask_config": true, "version": "v1.2.3", "log": "{}"}}"#, json_path(&log))).ok().unwrap();
    // Its version word, `v` dropped.
    assert_eq!(s.info, ("fake".to_string(), "1.2.3".to_string()));
    let got = std::fs::read_to_string(&log).unwrap();
    assert!(got.contains("reply c1 [null,null]"), "{got}");
    assert!(got.contains("reply c2 null"), "{got}");
    s.finish();
}

#[test]
fn windows_uris_round_trip() {
    assert_eq!(file_uri(Path::new("c:\\work\\a b.py")), "file:///C:/work/a%20b.py");
    assert_eq!(uri_path("file:///c%3A/work/a%20b.py").unwrap(), PathBuf::from("C:/work/a b.py"));
    assert_eq!(uri_path("file:///C:/work/a%20b.py").unwrap(), PathBuf::from("C:/work/a b.py"));
    assert_eq!(uri_path(&file_uri(Path::new("/usr/src/x#y.py"))).unwrap(), PathBuf::from("/usr/src/x#y.py"));
    assert_eq!(file_uri(Path::new("/usr/src/x#y.py")), "file:///usr/src/x%23y.py");
    assert_eq!(uri_path("untitled:1"), None);
}

#[test]
fn utf16_columns_become_bytes() {
    // `ü` is two bytes and one UTF-16 unit; `😀` four bytes and two units.
    let line = "s = '\u{fc}\u{1f600}'; y = 2";
    assert_eq!(byte_col(line, 11, true), 14);
    assert_eq!(byte_col(line, 14, false), 14);
    // A byte column inside a character is moved back to its start.
    assert_eq!(byte_col(line, 6, false), 5);
    let v = crate::json::parse(r#"[{"name": "y", "kind": 13, "range": {"start": {"line": 0, "character": 11}, "end": {"line": 0, "character": 16}}, "selectionRange": {"start": {"line": 0, "character": 11}, "end": {"line": 0, "character": 12}}}]"#).unwrap();
    let s = symbols(&v, line, true);
    assert_eq!((s[0].col, &line[s[0].col as usize..s[0].col as usize + 1]), (14, "y"));
}

fn sym(name: &str, kind: &str, line: u32, col: u32, end: u32, container: &str) -> Sym {
    Sym { name: name.into(), kind: kind.into(), line, col, end, container: container.into() }
}

const PY: &str = "class A:\n    def m(self):\n        v = 1\n        return v\n\nX = 3\n\ndef f():\n    pass\n";

#[test]
fn document_symbols_become_definitions() {
    let v = crate::json::parse(
        r#"[
        {"name": "A", "kind": 5, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 4, "character": 0}},
         "selectionRange": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 7}},
         "children": [{"name": "m", "kind": 6, "range": {"start": {"line": 1, "character": 4}, "end": {"line": 3, "character": 16}},
            "selectionRange": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 9}}}]},
        {"name": "(*Engine).Use", "kind": 6, "range": {"start": {"line": 7, "character": 0}, "end": {"line": 8, "character": 8}},
         "selectionRange": {"start": {"line": 7, "character": 4}, "end": {"line": 7, "character": 5}}},
        {"name": "F", "kind": 1, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
         "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}}
    ]"#,
    )
    .unwrap();
    assert_eq!(
        symbols(&v, PY, false),
        vec![sym("A", "class", 1, 6, 4, ""), sym("m", "method", 2, 8, 4, "A"), sym("Use", "method", 8, 4, 9, "Engine")]
    );
    for (k, w) in [
        (2, "mod"),
        (5, "class"),
        (6, "method"),
        (7, "property"),
        (8, "field"),
        (9, "ctor"),
        (10, "enum"),
        (11, "interface"),
        (12, "fn"),
        (13, "var"),
        (14, "const"),
        (22, "variant"),
        (23, "struct"),
        (24, "event"),
    ] {
        assert_eq!(kind_word(k), Some(w), "{k}");
    }
    assert_eq!((kind_word(1), kind_word(15), kind_word(26)), (None, None, None));
}

#[test]
fn locals_are_dropped() {
    let mut s = session(r#"{}"#).ok().unwrap();
    let mut got = Vec::new();
    s.document_symbols(&[(PathBuf::from("H:/x/a.py"), PY.to_string())], Instant::now() + Duration::from_secs(20), |_, v| got = v.unwrap())
        .unwrap();
    // `v` inside `m` goes; the module's `X` stays.
    assert_eq!(
        got,
        vec![sym("A", "class", 1, 6, 4, ""), sym("m", "method", 2, 8, 4, "A"), sym("X", "var", 6, 0, 6, ""), sym("f", "fn", 8, 4, 9, "")]
    );
    s.finish();
}

#[test]
fn flat_symbol_information_is_read() {
    let mut s = session(r#"{"flat": true}"#).ok().unwrap();
    let mut got = Vec::new();
    s.document_symbols(&[(PathBuf::from("H:/x/a.py"), PY.to_string())], Instant::now() + Duration::from_secs(20), |_, v| got = v.unwrap())
        .unwrap();
    // The name is found on the item's line; the local inside `m`'s range
    // is dropped there too.
    assert_eq!(
        got,
        vec![sym("A", "class", 1, 6, 4, ""), sym("m", "method", 2, 8, 4, "A"), sym("X", "var", 6, 0, 6, ""), sym("f", "fn", 8, 4, 9, "")]
    );
    s.finish();
}

#[test]
fn code_indexers_false_turns_a_server_off() {
    let go = crate::langs::by_name("go").unwrap().lsp;
    assert_eq!(choose(&Indexers::Chosen(vec![("gopls".into(), None)]), go).unwrap_err(), "gopls is off (code.indexers)");
    let py = crate::langs::by_name("python").unwrap().lsp;
    // A path given to the second server wins over finding the first.
    let (s, launch) = choose(&Indexers::Chosen(vec![("pyright-langserver".into(), Some(PathBuf::from("no/such/server")))]), py).unwrap();
    assert_eq!((s.tool, launch.program), ("pyright-langserver", PathBuf::from("no/such/server")));
    assert_eq!(choose(&Indexers::Off("all off".into()), py).unwrap_err(), "all off");
    assert_eq!(choose(&Indexers::Fixed(Vec::new()), py).unwrap_err(), "no basedpyright-langserver or pyright-langserver is configured");
}

/// A Python root: a project file and three modules.
fn py_root(name: &str) -> (PathBuf, PathBuf) {
    let d = temp(name);
    let root = d.join("root");
    std::fs::create_dir_all(root.join("pkg")).unwrap();
    std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"pkg\"\n").unwrap();
    std::fs::write(root.join("pkg/a.py"), PY).unwrap();
    std::fs::write(root.join("pkg/b.py"), "def b():\n    pass\n\ndef hidden_fn():\n    pass\n").unwrap();
    std::fs::write(root.join("pkg/c.py"), "class C:\n    pass\n").unwrap();
    (d, root)
}

/// The setup indexing `root` with the scripted server as basedpyright
/// (scip-python off).
fn setup(d: &Path, root: &Path, script: &str) -> CodeSetup {
    let spec = crate::config::CodeSpec {
        roots: vec![crate::config::CodeRootSpec {
            name: "py".into(),
            root: root.to_path_buf(),
            build_scripts: false,
            scip: Default::default(),
            exclude: Vec::new(),
        }],
        ..Default::default()
    };
    let mut s = CodeSetup::from_spec(&spec, &d.join("db.sqlite"), false);
    s.defer_on_cargo = false;
    s.indexers = Indexers::Chosen(vec![
        ("scip-python".into(), None),
        ("basedpyright-langserver".into(), Some(PathBuf::from(fake::register(script)))),
    ]);
    s
}

fn index(s: &CodeSetup) -> IndexReport {
    let mut c = crate::db::Connection::open_in_memory().unwrap();
    index_root(s, &mut c, &s.roots[0], &RustSource::ScanOnly("test".into())).unwrap()
}

fn py_note(r: &IndexReport) -> String {
    r.units.iter().find(|u| u.lang == "python").map(|u| u.source.clone()).unwrap_or_default()
}

fn asked(log: &Path) -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_to_string(log).unwrap_or_default().lines().filter_map(|l| l.strip_prefix("ask ")).map(str::to_string).collect();
    v.sort();
    v
}

#[test]
fn lsp_files_are_covered_and_filled() {
    let (d, root) = py_root("covered");
    let r = index(&setup(&d, &root, r#"{"version": "9.1", "hide": ["hidden_fn"]}"#));
    assert_eq!(py_note(&r), "basedpyright 9.1 (lsp)");
    // a.py: A, m, X, f; b.py: b; c.py: C. The hidden definition is the
    // scanner's fill, and no file is scanned whole.
    assert_eq!((r.rows.lsp_defs, r.rows.lang_defs), (6, 1), "{}", r.rows);
    assert_eq!(r.rows.scip_refs, 0);
}

#[test]
fn scip_failure_falls_to_the_server() {
    let (d, root) = py_root("fallback");
    let mut s = setup(&d, &root, r#"{"version": "9.1"}"#);
    if let Indexers::Chosen(list) = &mut s.indexers {
        list[0].1 = Some(d.join("no-such-scip-python"));
    }
    let r = index(&s);
    assert_eq!(py_note(&r), "basedpyright 9.1 (lsp)");
    assert_eq!(r.rows.lsp_defs, 7);
    // With the server off too, the scanner, and the note says why both.
    s.indexers =
        Indexers::Chosen(vec![("scip-python".into(), None), ("basedpyright-langserver".into(), None), ("pyright-langserver".into(), None)]);
    let r = index(&s);
    assert_eq!(
        py_note(&r),
        "scan only: scip-python is off (code.indexers); basedpyright-langserver is off (code.indexers); pyright-langserver is off (code.indexers)"
    );
    assert_eq!(r.rows.lsp_defs, 0);
}

#[test]
fn only_the_edited_file_is_re_asked() {
    let (d, root) = py_root("edited");
    let log = d.join("log.txt");
    let s = setup(&d, &root, &format!(r#"{{"version": "9.1", "log": "{}"}}"#, json_path(&log)));
    index(&s);
    assert_eq!(asked(&log), ["a.py", "b.py", "c.py"]);
    std::fs::write(&log, "").unwrap();
    std::fs::write(root.join("pkg/b.py"), "def b2():\n    pass\n").unwrap();
    std::fs::remove_file(root.join("pkg/c.py")).unwrap();
    let r = index(&s);
    assert_eq!(asked(&log), ["b.py"]);
    // a.py from the cache, b.py anew, c.py gone.
    assert_eq!(r.rows.lsp_defs, 5);
    let cache = Cache::load(&s.dir.join("py.basedpyright-langserver.lsp")).unwrap();
    let mut paths: Vec<&String> = cache.files.keys().collect();
    paths.sort();
    assert_eq!(paths, ["pkg/a.py", "pkg/b.py"]);
}

#[test]
fn a_stale_file_empties_the_cache() {
    let (d, root) = py_root("stale");
    let log = d.join("log.txt");
    let s = setup(&d, &root, &format!(r#"{{"version": "9.1", "log": "{}"}}"#, json_path(&log)));
    index(&s);
    std::fs::write(&log, "").unwrap();
    std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"pkg2\"\n").unwrap();
    index(&s);
    assert_eq!(asked(&log), ["a.py", "b.py", "c.py"]);
}

#[test]
fn a_new_server_version_empties_the_cache() {
    let (d, root) = py_root("version");
    let log = d.join("log.txt");
    index(&setup(&d, &root, &format!(r#"{{"version": "9.1", "log": "{}"}}"#, json_path(&log))));
    std::fs::write(&log, "").unwrap();
    // The same version: nothing is asked.
    index(&setup(&d, &root, &format!(r#"{{"version": "9.1", "log": "{}"}}"#, json_path(&log))));
    assert!(asked(&log).is_empty());
    let r = index(&setup(&d, &root, &format!(r#"{{"version": "9.2", "log": "{}"}}"#, json_path(&log))));
    assert_eq!(asked(&log), ["a.py", "b.py", "c.py"]);
    assert_eq!(py_note(&r), "basedpyright 9.2 (lsp)");
}

#[test]
fn the_server_is_killed_at_the_timeout() {
    let (d, root) = py_root("timeout");
    let mut s = setup(&d, &root, r#"{"version": "9.1", "hang_after": 0}"#);
    s.timeout = Duration::from_secs(1);
    let t = Instant::now();
    let r = index(&s);
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
    assert_eq!(py_note(&r), "scan only: scip-python is off (code.indexers); basedpyright-langserver passed the 1 s timeout");
    assert!(r.runs.iter().any(|(l, run)| l == "basedpyright-langserver" && run.timed_out), "{:?}", r.runs);
    // Every file scanned instead.
    assert_eq!(r.rows.lsp_defs, 0);
    assert!(r.rows.lang_defs > 0);
}

#[test]
fn answered_files_survive_a_timeout() {
    let (d, root) = py_root("survive");
    let log = d.join("log.txt");
    let mut s = setup(&d, &root, &format!(r#"{{"version": "9.1", "hang_after": 1, "log": "{}"}}"#, json_path(&log)));
    s.timeout = Duration::from_secs(1);
    let r = index(&s);
    assert!(py_note(&r).ends_with("passed the 1 s timeout"), "{}", py_note(&r));
    let first = asked(&log);
    assert_eq!(first.len(), 1);
    // The answered file's rows are used, and it stays cached.
    assert!(r.rows.lsp_defs > 0);
    std::fs::write(&log, "").unwrap();
    let mut s2 = setup(&d, &root, &format!(r#"{{"version": "9.1", "log": "{}"}}"#, json_path(&log)));
    s2.timeout = Duration::from_secs(20);
    let r = index(&s2);
    let second = asked(&log);
    assert_eq!(second.len(), 2);
    assert!(!second.contains(&first[0]));
    assert_eq!(r.rows.lsp_defs, 7);
}

#[test]
fn a_server_that_reads_no_file_scans() {
    let (d, root) = py_root("nofile");
    let r = index(&setup(&d, &root, r#"{"version": "9.1", "fail": true}"#));
    assert_eq!(py_note(&r), "scan only: scip-python is off (code.indexers); basedpyright-langserver answered no file (no view)");
    assert_eq!(r.rows.lsp_defs, 0);
    assert!(r.rows.lang_defs >= 7, "{}", r.rows);
}

#[test]
fn doctor_names_a_missing_server() {
    let (d, root) = py_root("doctor");
    let mut s = setup(&d, &root, r#"{"version": "9.1", "log": "never"}"#);
    // The scripted server configured: it is named, never started.
    let lines = doctor_lines(&s);
    assert_eq!(lines, [format!("code: python in {} is indexed by basedpyright (lsp)", root.display())]);
    // Neither found: why, and how to install one. Every server is turned
    // off, so the machine's own are not looked for.
    if let Indexers::Chosen(list) = &mut s.indexers {
        list[1].1 = None;
        list.push(("pyright-langserver".into(), None));
    }
    let scip = if cfg!(windows) { "no scip-python (not on Windows)" } else { "scip-python is off (code.indexers)" };
    assert_eq!(
        doctor_lines(&s),
        [format!(
            "code: python in {} is scanned: {scip}, basedpyright-langserver is off (code.indexers); pyright-langserver is off (code.indexers); install one (npm i -g basedpyright) or set code.indexers",
            root.display()
        )]
    );
    // No line for a language without files, and none with indexers off.
    s.indexers = Indexers::Off("indexers are off (code.indexers is false)".into());
    assert_eq!(doctor_lines(&s), [format!("code: python in {} is scanned: indexers are off (code.indexers is false)", root.display())]);
    assert!(!Path::new("never").exists());
}

#[test]
fn a_frame_past_the_cap_is_refused_and_one_at_it_is_read() {
    use std::io::Read as _;
    let past = format!("Content-Length: {}\r\n\r\n", MAX_FRAME + 1);
    let err = read_frame(&mut std::io::Cursor::new(past.into_bytes())).unwrap_err();
    assert_eq!(err, format!("a frame of {} bytes", MAX_FRAME + 1));
    let at = format!("Content-Length: {MAX_FRAME}\r\n\r\n");
    let mut r = std::io::BufReader::new(std::io::Cursor::new(at.into_bytes()).chain(std::io::repeat(b' ').take(MAX_FRAME as u64)));
    assert_eq!(read_frame(&mut r).unwrap().map(|b| b.len()), Some(MAX_FRAME));
}

#[test]
fn a_receiver_without_a_method_or_a_type_is_kept_whole() {
    assert_eq!(split_receiver("(*T).Use"), ("Use".to_string(), Some("T".to_string())));
    assert_eq!(split_receiver("(*T)"), ("(*T)".to_string(), None));
    assert_eq!(split_receiver("().M"), ("().M".to_string(), None));
}

#[test]
fn a_fixed_list_runs_the_server_it_names() {
    let py = crate::langs::by_name("python").unwrap().lsp;
    let (s, _) = choose(&Indexers::Fixed(vec![("pyright-langserver".into(), PathBuf::from("X"))]), py).unwrap();
    assert_eq!(s.tool, "pyright-langserver");
}

#[test]
fn a_cache_of_another_format_is_not_read() {
    let d = std::env::temp_dir().join(format!("atlas-lsp-head-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join("c.lsp");
    std::fs::write(&p, "atlas-lsp 0\tv\ts\n").unwrap();
    assert!(Cache::load(&p).is_none());
    std::fs::write(&p, format!("{CACHE_HEAD}\tv\ts\n")).unwrap();
    assert!(Cache::load(&p).is_some());
    let _ = std::fs::remove_dir_all(&d);
}

/// Two Go units side by side: the stale-making files of one.
fn go_walk(a: i64, b: i64, readme: i64) -> crate::code::Walk {
    let f = |path: &str, mtime: i64| crate::code::WalkedFile { path: path.into(), mtime, size: 1 };
    crate::code::Walk { markers: vec![f("a/go.mod", a), f("b/go.mod", b), f("a/README.md", readme)], ..Default::default() }
}

#[test]
fn stale_sig_needs_both_units() {
    let go = crate::langs::by_name("go").unwrap();
    let base = stale_sig(go, &go_walk(1, 1, 1), "a");
    // A sibling unit's manifest does not empty this unit's cache.
    assert_eq!(stale_sig(go, &go_walk(1, 2, 1), "a"), base);
    // A file of this unit that is no stale-maker does not either.
    assert_eq!(stale_sig(go, &go_walk(1, 1, 2), "a"), base);
    // Its own manifest does.
    assert_ne!(stale_sig(go, &go_walk(2, 1, 1), "a"), base);
    // The whole root (no folder) sees both.
    assert_ne!(stale_sig(go, &go_walk(1, 2, 1), ""), stale_sig(go, &go_walk(1, 1, 1), ""));
}

#[test]
fn a_header_line_over_the_cap_is_refused() {
    let long = format!("X-Pad: {}\r\nContent-Length: 2\r\n\r\n{{}}", "a".repeat(MAX_HEADER));
    let err = read_frame(&mut std::io::BufReader::new(long.as_bytes())).unwrap_err();
    assert!(err.contains("header line over"), "{err}");
    // A line with no end at all is cut at the cap too, not read to the end.
    let endless = vec![b'a'; 4 * MAX_HEADER];
    assert!(read_frame(&mut std::io::BufReader::new(&endless[..])).is_err());
    // A line of exactly the cap (its line end counted) is read.
    let exact = format!("X-Pad: {}\r\nContent-Length: 2\r\n\r\n{{}}", "a".repeat(MAX_HEADER - 9));
    assert_eq!(read_frame(&mut std::io::BufReader::new(exact.as_bytes())).unwrap().unwrap(), b"{}");
    // Just under the cap is read.
    let ok = format!("X-Pad: {}\r\nContent-Length: 2\r\n\r\n{{}}", "a".repeat(MAX_HEADER - 20));
    assert_eq!(read_frame(&mut std::io::BufReader::new(ok.as_bytes())).unwrap().unwrap(), b"{}");
}

#[test]
fn a_shutdown_write_that_blocks_is_killed() {
    // The pipe of a server that stopped reading: a write blocks until the
    // pipe is closed, which the kill does.
    struct Stuck(mpsc::Receiver<()>, bool);
    impl Write for Stuck {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            if !self.1 {
                let _ = self.0.recv();
                self.1 = true;
            }
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (closed, wait) = mpsc::channel();
    let (never, silent) = io::pipe().unwrap();
    let s = Session { client: Client::new(never, Stuck(wait, false)), utf16: false, info: Default::default(), language_id: "x".into() };
    let kill = closed.clone();
    let dog = Watchdog::start(Instant::now() + Duration::from_millis(300), move || {
        let _ = kill.send(());
    });
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        s.finish();
        let _ = done.send(());
    });
    assert!(finished.recv_timeout(Duration::from_secs(20)).is_ok(), "finish is stuck in a write the watchdog never unblocked");
    assert!(dog.stop());
    drop((silent, closed));
}

#[test]
fn a_watchdog_stopped_in_time_does_not_fire() {
    let (fired, rx) = mpsc::channel();
    let dog = Watchdog::start(Instant::now() + Duration::from_secs(30), move || {
        let _ = fired.send(());
    });
    assert!(!dog.stop());
    assert!(rx.try_recv().is_err());
}
