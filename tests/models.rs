//! The setup-time model download against a local HTTP server: a transfer
//! cut short is resumed with a range request, a partial file left on disk
//! is resumed, a server that ignores ranges restarts from zero, a file whose
//! hash does not match its pin is refused and deleted, and until the model
//! is in place recall is lexical and hybrid search answers lexically.
//!
//! The transfers run through the system `curl`, as in production.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use atlas::config::{ModelFileSpec, ModelsSpec};
use atlas::hooks::{HookEvent, Hooks};
use atlas::json;
use atlas::memory::settings::Settings;
use atlas::memory::{Draft, MemoryStore};
use atlas::models::{FetchError, Fetched, Models};
use common::*;

/// What the test server does.
#[derive(Clone)]
struct Behaviour {
    body: Vec<u8>,
    honour_range: bool,
    /// Close the first response after this many body bytes (the header
    /// still promises the whole body).
    cut_first_after: Option<usize>,
}

/// The range start of each request the server saw (`None`: no range).
type Seen = Arc<Mutex<Vec<Option<u64>>>>;

fn read_head(s: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match s.read(&mut b) {
            Ok(1) => head.push(b[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

fn range_start(head: &str) -> Option<u64> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if !k.trim().eq_ignore_ascii_case("range") {
            return None;
        }
        v.trim().strip_prefix("bytes=")?.split('-').next()?.parse().ok()
    })
}

/// Serves `b` on a loopback port for the rest of the test process.
fn serve(b: Behaviour) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/model.atm", l.local_addr().unwrap().port());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for (n, conn) in l.incoming().enumerate() {
            let Ok(mut s) = conn else { continue };
            let head = read_head(&mut s);
            let range = range_start(&head);
            log.lock().unwrap().push(range);
            let len = b.body.len();
            let (status, extra, body) = match range {
                Some(r) if b.honour_range && (r as usize) < len => {
                    ("206 Partial Content", format!("Content-Range: bytes {r}-{}/{len}\r\n", len - 1), &b.body[r as usize..])
                }
                _ => ("200 OK", String::new(), &b.body[..]),
            };
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n", body.len());
            let send = match b.cut_first_after {
                Some(k) if n == 0 => &body[..k.min(body.len())],
                _ => body,
            };
            let _ = s.write_all(send);
            let _ = s.flush();
        }
    });
    (url, seen)
}

/// Bytes that hash differently at every offset.
fn body(n: usize) -> Vec<u8> {
    let mut x: u32 = 0x9e37_79b9;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn models(dir: &Path, url: &str, data: &[u8]) -> (Models, ModelFileSpec) {
    let f = ModelFileSpec { id: "embed".into(), file: "m.atm".into(), url: url.into(), sha256: atlas::sha256::hex(data), size: data.len() as u64 };
    let spec = ModelsSpec { dir: Some(dir.join("models")), files: vec![f.clone()], ..ModelsSpec::default() };
    (Models::new(Some(&spec), &dir.join("state").join("atlas.sqlite")), f)
}

fn part(m: &Models) -> PathBuf {
    m.dir.join("m.atm.part")
}

#[test]
fn an_interrupted_download_resumes_with_a_range_request() {
    let t = Temp::new("models-resume");
    let data = body(300_000);
    let (url, seen) = serve(Behaviour { body: data.clone(), honour_range: true, cut_first_after: Some(100_000) });
    let (m, f) = models(t.path(), &url, &data);
    let first = m.fetch(&f, false, true);
    assert!(matches!(first, Err(FetchError::Curl(_))), "{first:?}");
    assert_eq!(fs::metadata(part(&m)).unwrap().len(), 100_000, "the bytes received are kept");
    assert!(!m.dir.join("m.atm").exists(), "nothing under the final name before it is verified");
    assert_eq!(m.missing().len(), 1);
    match m.fetch(&f, false, true).unwrap() {
        Fetched::Downloaded { resumed_from, .. } => assert_eq!(resumed_from, 100_000),
        other => panic!("{other:?}"),
    }
    assert_eq!(fs::read(m.dir.join("m.atm")).unwrap(), data);
    assert!(!part(&m).exists());
    assert_eq!(*seen.lock().unwrap(), vec![None, Some(100_000)]);
    assert!(m.missing().is_empty());
    // Present now: no request, and a check re-hashes it.
    assert_eq!(m.fetch(&f, true, true).unwrap(), Fetched::Present);
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn a_partial_file_on_disk_is_resumed() {
    let t = Temp::new("models-partial");
    let data = body(50_000);
    let (url, seen) = serve(Behaviour { body: data.clone(), honour_range: true, cut_first_after: None });
    let (m, f) = models(t.path(), &url, &data);
    fs::create_dir_all(&m.dir).unwrap();
    fs::write(part(&m), &data[..1234]).unwrap();
    let got = m.fetch_missing(true).unwrap();
    assert_eq!(got.len(), 1);
    assert!(matches!(got[0].1, Ok(Fetched::Downloaded { resumed_from: 1234, .. })), "{got:?}");
    assert_eq!(fs::read(m.dir.join("m.atm")).unwrap(), data);
    assert_eq!(*seen.lock().unwrap(), vec![Some(1234)]);
    // The lock is given back.
    assert!(!m.dir.join(atlas::models::LOCK_FILE).exists());
    let _ = f;
}

#[test]
fn a_server_that_ignores_ranges_restarts_from_zero() {
    let t = Temp::new("models-norange");
    let data = body(40_000);
    let (url, seen) = serve(Behaviour { body: data.clone(), honour_range: false, cut_first_after: None });
    let (m, f) = models(t.path(), &url, &data);
    fs::create_dir_all(&m.dir).unwrap();
    fs::write(part(&m), &data[..500]).unwrap();
    assert!(matches!(m.fetch(&f, false, true).unwrap(), Fetched::Downloaded { .. }));
    assert_eq!(fs::read(m.dir.join("m.atm")).unwrap(), data);
    assert_eq!(*seen.lock().unwrap(), vec![Some(500), None]);
}

#[test]
fn a_hash_mismatch_is_refused_and_deleted() {
    let t = Temp::new("models-hash");
    let data = body(20_000);
    let mut bad = data.clone();
    bad[10_000] ^= 1;
    let (url, _) = serve(Behaviour { body: bad, honour_range: true, cut_first_after: None });
    let (m, f) = models(t.path(), &url, &data);
    match m.fetch(&f, false, true) {
        Err(FetchError::HashMismatch { expected, actual }) => {
            assert_eq!(expected, f.sha256);
            assert_ne!(actual, f.sha256);
        }
        other => panic!("{other:?}"),
    }
    assert!(!part(&m).exists(), "the refused bytes are deleted");
    assert!(!m.dir.join("m.atm").exists());
    // A file under the final name that no longer matches is not trusted by
    // a check: it is deleted and fetched again (and refused again here).
    fs::write(m.dir.join("m.atm"), b"edited").unwrap();
    assert!(matches!(m.fetch(&f, true, true), Err(FetchError::HashMismatch { .. })));
    assert!(!m.dir.join("m.atm").exists());
}

#[test]
fn a_body_longer_than_its_pin_is_refused() {
    let t = Temp::new("models-size");
    let data = body(10_000);
    let (url, _) = serve(Behaviour { body: body(30_000), honour_range: true, cut_first_after: None });
    let (m, f) = models(t.path(), &url, &data);
    assert!(m.fetch(&f, false, true).is_err());
    assert!(!m.dir.join("m.atm").exists());
    assert!(fs::metadata(part(&m)).map(|x| x.len() <= 10_000).unwrap_or(true), "nothing past the pinned size is kept");
}

/// A lock is busy while it or a part file changes; with neither changing
/// for [`atlas::models::STALE_LOCK`] it was left by a process that died.
#[test]
fn a_held_lock_means_busy_and_a_stale_one_is_broken() {
    let t = Temp::new("models-lock");
    let data = body(1000);
    let (url, _) = serve(Behaviour { body: data.clone(), honour_range: true, cut_first_after: None });
    let (m, f) = models(t.path(), &url, &data);
    fs::create_dir_all(&m.dir).unwrap();
    let lock = m.dir.join(atlas::models::LOCK_FILE);
    fs::write(&lock, "pid 1\n").unwrap();
    assert_eq!(m.fetch(&f, false, true), Err(FetchError::Busy));
    let old = std::time::SystemTime::now() - atlas::models::STALE_LOCK - std::time::Duration::from_secs(60);
    fs::File::options().write(true).open(&lock).unwrap().set_modified(old).unwrap();
    // A part file still growing keeps an old lock alive.
    fs::write(part(&m), &data[..10]).unwrap();
    assert_eq!(m.fetch(&f, false, true), Err(FetchError::Busy));
    fs::File::options().write(true).open(part(&m)).unwrap().set_modified(old).unwrap();
    match m.fetch(&f, false, true).unwrap() {
        Fetched::Downloaded { resumed_from, .. } => assert_eq!(resumed_from, 10),
        other => panic!("{other:?}"),
    }
}

fn settings(dir: &Path, model: PathBuf) -> Settings {
    let mut s = Settings::from_config(None, &dir.join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.recall.fts_floor = 0.0;
    s.recall.hybrid_fts_floor = 0.0;
    s.model = model;
    s
}

fn seed(s: &Settings) {
    let mut store = MemoryStore::open(&s.store).unwrap();
    let mut w = store.writer().unwrap();
    let d = Draft { title: "- [Widget cache](widget-cache.md) - bound".into(), body: "\nThe widget cache evicts entries past sixty four widgets.\n".into(), reason: "seeding".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
    w.create("widget-cache", "memory", "feedback", "", &d, None).unwrap();
    w.commit().unwrap();
}

/// Without the model the recall gate is lexical (a prompt sharing no words
/// recalls nothing, even with a cosine bar every record clears); once the
/// download lands, the same running hook state scores meaning.
#[test]
fn recall_is_lexical_until_the_model_arrives() {
    let t = Temp::new("models-recall");
    let tiny = atlas::embed::tiny_model_bytes(true, 7);
    let (url, _) = serve(Behaviour { body: tiny.clone(), honour_range: true, cut_first_after: None });
    let (m, f) = models(t.path(), &url, &tiny);
    let mut s = settings(t.path(), m.path("embed").unwrap());
    s.recall.cos_alone = -1.01;
    seed(&s);
    let mut h = Hooks::open(s.clone()).unwrap();
    let ev = |p: &str| HookEvent { event: "UserPromptSubmit".into(), session_id: "s1".into(), prompt: p.into(), ..HookEvent::default() };
    let a = h.handle(&ev("what is the weather like on the coast"));
    assert!(a.context.is_empty(), "lexical gate recalled an unrelated record: {}", a.context);
    let a = h.handle(&ev("why does the widget cache evict entries past sixty four widgets"));
    assert!(a.context.contains("widget-cache"), "the lexical gate still answers: {}", a.context);
    m.fetch(&f, false, true).unwrap();
    {
        // Embedded here, as the background thread would, so the test does not wait on it.
        let store = MemoryStore::open(&s.store).unwrap();
        let model = atlas::embed::Model::load(&s.model).unwrap();
        while atlas::memory::vectors::embed_pending(&store, &model, 2, 1).unwrap() > 0 {}
    }
    let a = h.handle(&HookEvent { session_id: "s2".into(), ..ev("what is the weather like on the coast") });
    assert!(a.context.contains("atlas recall"), "the model was not picked up: {}", a.context);
}

/// Hybrid search before the model is downloaded answers lexically with a
/// note; semantic search says how to get the model.
#[test]
fn hybrid_search_is_lexical_without_the_model() {
    let t = Temp::new("models-hybrid");
    let notes = t.path().join("Notes");
    write(&notes.join("Animals.md"), "# Animals\n\n## Cats\n\nThe cat sat on the mat.\n\n## Dogs\n\nThe dog ran fast.\n");
    let db = t.path().join("state/atlas.sqlite");
    let vaults = atlas::paths::Vaults::new(&[("notes".to_string(), notes.clone())]).unwrap();
    let mut store = atlas::store::Store::open(&db, vaults).unwrap();
    store.rescan().unwrap();
    let mut s = atlas::tools::Server::new(store);
    s.set_semantic(atlas::tools::SemState::new(t.path().join("models/absent.atm"), db, 1));
    let v = s.call("search", &json!({"query": "cat mat", "mode": "hybrid"})).unwrap();
    assert_eq!(v["results"][0]["heading_path"], "Animals > Cats", "{v}");
    assert!(v["vectors"].as_str().unwrap().contains("not downloaded"), "{v}");
    let e = s.call("search", &json!({"query": "cat mat", "mode": "semantic"})).unwrap_err();
    assert!(e.to_string().contains("atlas --setup"), "{e}");
}

fn atlas_home(home: &Path) -> Command {
    let mut c = Command::new(bin());
    c.env("ATLAS_HOME", home).env_remove("ATLAS_CONFIG");
    c
}

/// The first run with no configuration anywhere writes the template (and
/// its empty vault) into the configuration folder and keeps its state in
/// the data folder; `--setup` then downloads and verifies the listed models.
#[test]
fn first_run_writes_the_template_and_setup_fetches_the_models() {
    let t = Temp::new("models-first");
    let home = t.path().join("home");
    let out = atlas_home(&home).arg("--rescan").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("first run"));
    assert_eq!(fs::read_to_string(home.join("atlas.json")).unwrap(), atlas::config::TEMPLATE);
    assert!(home.join("notes").is_dir());
    assert!(home.join("state").join("atlas.sqlite").is_file(), "the database is in the data folder");
    // The user's own model list, pointing at the local server.
    let tiny = atlas::embed::tiny_model_bytes(false, 3);
    let (url, seen) = serve(Behaviour { body: tiny.clone(), honour_range: true, cut_first_after: None });
    let cfg = json!({"vaults": [{"name": "notes", "path": "notes"}],
        "models": {"files": [{"id": "embed", "file": "tiny.atm", "url": url, "sha256": atlas::sha256::hex(&tiny), "size": tiny.len()}]}});
    fs::write(home.join("atlas.json"), cfg.to_string()).unwrap();
    let out = atlas_home(&home).arg("--setup").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("downloaded and verified"), "{text}");
    assert_eq!(fs::read(home.join("models").join("tiny.atm")).unwrap(), tiny);
    assert!(text.contains(&format!("semantic model {}", home.join("models").join("tiny.atm").display())), "{text}");
    // Again: verified in place, nothing fetched.
    let out = atlas_home(&home).arg("--setup").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("present, sha256 verified"));
    assert_eq!(seen.lock().unwrap().len(), 1);
}
