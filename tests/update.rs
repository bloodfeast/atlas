//! The release check at session start, against a loopback HTTP server and a
//! throwaway signing key (the library's `Updater::key`; the binary only ever
//! uses the built-in key): a newer signed release is swapped in; the same
//! version is a no-op and is not asked again within the hour; a bad or
//! missing signature is refused; a server that stalls past the cap leaves
//! the installed binary to start; offline fails open; a pin stays put.
//!
//! Transfers run through the system `curl`, as in production.

mod common;

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use atlas::config::Config;
use atlas::models::Models;
use atlas::update::{self, run_capped, Outcome, Updater};
use common::*;

const SEED: [u8; 32] = *b"atlas-update-test-key-not-secret";
const TARGET: &str = "test-target";
const OLD: &[u8] = b"old binary, version 0.1.0";

/// Paths the server answered, in order.
type Seen = Arc<Mutex<Vec<String>>>;

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

/// Serves `routes` (path -> body; anything else 404) on a loopback port for
/// the rest of the test process. With `stall`, every connection is held
/// open without an answer.
fn serve(routes: HashMap<String, Vec<u8>>, stall: bool) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let routes = Arc::new(routes);
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { continue };
            let (log, routes) = (log.clone(), routes.clone());
            std::thread::spawn(move || {
                let head = read_head(&mut s);
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                if stall {
                    std::thread::sleep(Duration::from_secs(30));
                    return;
                }
                match routes.get(&path) {
                    Some(b) => {
                        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                        let _ = s.write_all(b);
                    }
                    None => {
                        let _ = write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
                let _ = s.flush();
            });
        }
    });
    (base, seen)
}

fn key() -> ed25519_compact::KeyPair {
    ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new(SEED))
}

fn manifest(base: &str, version: &str, bin: &[u8], extra: &str) -> Vec<u8> {
    format!(
        "{{\"version\":\"{version}\",\"files\":[{{\"target\":\"{TARGET}\",\"file\":\"atlas-{TARGET}\",\"url\":\"{base}/bin\",\"sha256\":\"{}\",\"size\":{}}}]{extra}}}",
        atlas::sha256::hex(bin),
        bin.len()
    )
    .into_bytes()
}

/// A release at `version` holding `bin`, signed by the test key.
fn release(version: &str, bin: &[u8]) -> (String, Seen) {
    release_with(version, bin, "")
}

/// As [`release`], with `extra` (`,"key":value...`) after the files.
fn release_with(version: &str, bin: &[u8], extra: &str) -> (String, Seen) {
    // The manifest names the server's own URL: serve first, then fill.
    let (base, seen) = serve_late();
    let mut routes = HashMap::new();
    let m = manifest(&base, version, bin, extra);
    let sig = key().sk.sign(&m, None);
    routes.insert("/manifest.json".into(), m);
    routes.insert("/manifest.json.sig".into(), sig.as_ref().to_vec());
    routes.insert("/bin".into(), bin.to_vec());
    install(&base, routes);
    (base, seen)
}

type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// Routes filled after the port is known (see [`release`]).
static LATE: Mutex<Vec<(String, Files)>> = Mutex::new(Vec::new());

fn serve_late() -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
    LATE.lock().unwrap().push((base.clone(), routes.clone()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { continue };
            let (log, routes) = (log.clone(), routes.clone());
            std::thread::spawn(move || {
                let head = read_head(&mut s);
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let body = routes.lock().unwrap().get(&path).cloned();
                match body {
                    Some(b) => {
                        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                        let _ = s.write_all(&b);
                    }
                    None => {
                        let _ = write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
                let _ = s.flush();
            });
        }
    });
    (base, seen)
}

fn install(base: &str, routes: HashMap<String, Vec<u8>>) {
    let late = LATE.lock().unwrap();
    let (_, r) = late.iter().find(|(b, _)| b == base).unwrap();
    *r.lock().unwrap() = routes;
}

fn routes_of(base: &str) -> Arc<Mutex<HashMap<String, Vec<u8>>>> {
    LATE.lock().unwrap().iter().find(|(b, _)| b == base).unwrap().1.clone()
}

/// An installed binary at version 0.1.0 and a check against `base`.
fn setup(t: &Temp, base: &str) -> Updater {
    let bin = t.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(format!("atlas-{TARGET}"));
    fs::write(&exe, OLD).unwrap();
    Updater {
        url: format!("{base}/manifest.json"),
        exe,
        state_dir: t.path().join("state"),
        curl: Models::new(None, &t.path().join("db.sqlite")).curl,
        key: *key().pk,
        current: "0.1.0".into(),
        target: TARGET.into(),
        pin: None,
        check_every: Duration::from_secs(3600),
        max_time: 5,
        install: true,
        notify: true,
        live: false,
        gate: update::SwapGate::default(),
    }
}

fn paths(seen: &Seen) -> Vec<String> {
    seen.lock().unwrap().clone()
}

fn aside(exe: &Path) -> Vec<PathBuf> {
    let name = exe.file_name().unwrap().to_string_lossy().into_owned();
    fs::read_dir(exe.parent().unwrap()).unwrap().flatten().map(|e| e.path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(&format!("{name}.old"))).collect()
}

#[test]
fn a_newer_signed_release_is_swapped_in() {
    let t = Temp::new("upd-swap");
    let new = b"new binary, version 0.2.0".to_vec();
    let (base, seen) = release("0.2.0", &new);
    let u = setup(&t, &base);
    let o = u.check();
    assert_eq!(o, Outcome::Swapped { from: "0.1.0".into(), to: "0.2.0".into() });
    assert_eq!(fs::read(&u.exe).unwrap(), new, "the new binary is in place");
    assert!(!u.exe.with_file_name(format!("atlas-{TARGET}.new")).exists(), "the download was renamed, not copied");
    assert_eq!(paths(&seen), vec!["/manifest.json", "/manifest.json.sig", "/bin"]);
    if cfg!(windows) {
        let a = aside(&u.exe);
        assert_eq!(a.len(), 1, "the running exe was renamed aside: {a:?}");
        assert_eq!(fs::read(&a[0]).unwrap(), OLD);
    }
    // A later start deletes what an earlier swap set aside.
    let u2 = Updater { current: "0.2.0".into(), ..u.clone() };
    assert!(matches!(u2.check(), Outcome::Skipped(_)), "0.2.0 is what the last check saw");
    assert!(aside(&u.exe).is_empty());
}

#[test]
fn the_same_version_is_a_no_op_and_is_not_asked_again_within_the_hour() {
    let t = Temp::new("upd-same");
    let (base, seen) = release("0.1.0", b"same version, other bytes");
    let u = setup(&t, &base);
    assert_eq!(u.check(), Outcome::UpToDate { latest: "0.1.0".into() });
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
    assert_eq!(paths(&seen), vec!["/manifest.json", "/manifest.json.sig"], "no binary was fetched");
    assert!(matches!(u.check(), Outcome::Skipped(_)));
    assert_eq!(paths(&seen).len(), 2, "the second start asked nothing");
    // An older signed manifest cannot roll the binary back.
    let (base, _) = release("0.0.9", b"older");
    let u = Updater { url: format!("{base}/manifest.json"), state_dir: t.path().join("state2"), ..setup(&t, &base) };
    assert_eq!(u.check(), Outcome::UpToDate { latest: "0.0.9".into() });
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
}

#[test]
fn a_bad_or_missing_signature_is_refused() {
    let t = Temp::new("upd-sig");
    let (base, seen) = release("0.2.0", b"would-be binary");
    let r = routes_of(&base);
    // Signed by the test key, but over other bytes.
    let other = key().sk.sign(b"another manifest", None);
    r.lock().unwrap().insert("/manifest.json.sig".into(), other.as_ref().to_vec());
    let u = setup(&t, &base);
    let o = u.check();
    assert!(matches!(&o, Outcome::Refused(m) if m.contains("does not verify")), "{o:?}");
    assert!(o.miss().is_some(), "a refusal is recorded");
    // A signature by another key over the right bytes.
    let m = r.lock().unwrap()["/manifest.json"].clone();
    let stranger = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new([7u8; 32]));
    r.lock().unwrap().insert("/manifest.json.sig".into(), stranger.sk.sign(&m, None).as_ref().to_vec());
    let u = Updater { state_dir: t.path().join("state-b"), ..u };
    assert!(matches!(u.check(), Outcome::Refused(_)));
    // No signature at all.
    r.lock().unwrap().remove("/manifest.json.sig");
    let u = Updater { state_dir: t.path().join("state-c"), ..u };
    let o = u.check();
    assert!(matches!(&o, Outcome::Refused(m) if m.contains("no signature")), "{o:?}");
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
    assert!(!paths(&seen).iter().any(|p| p == "/bin"), "nothing was downloaded");
}

#[test]
fn a_binary_that_does_not_match_the_signed_hash_is_not_installed() {
    let t = Temp::new("upd-hash");
    let (base, _) = release("0.2.0", b"the signed binary");
    routes_of(&base).lock().unwrap().insert("/bin".into(), b"the signed binarY".to_vec());
    let u = setup(&t, &base);
    let o = u.check();
    assert!(matches!(&o, Outcome::Failed(m) if m.contains("sha256") || m.contains("SHA-256") || m.contains("hash")), "{o:?}");
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
}

#[test]
fn a_check_past_the_cap_starts_the_installed_version() {
    let t = Temp::new("upd-stall");
    let (base, seen) = serve(HashMap::new(), true);
    let u = Updater { max_time: 2, ..setup(&t, &base) };
    let exe = u.exe.clone();
    let (tx, rx) = mpsc::channel();
    let t0 = Instant::now();
    let o = run_capped(u, Duration::from_millis(300), move |o| tx.send(o).unwrap());
    let waited = t0.elapsed();
    assert_eq!(o, None, "the cap returned first");
    assert!(waited < Duration::from_secs(2), "waited {waited:?}");
    assert_eq!(fs::read(&exe).unwrap(), OLD);
    // The check goes on in the background and its failure still arrives.
    let late = rx.recv_timeout(Duration::from_secs(20)).expect("the background check finished");
    assert!(late.miss().is_some(), "{late:?}");
    assert!(!paths(&seen).is_empty());
}

#[test]
fn offline_fails_open() {
    let t = Temp::new("upd-offline");
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    drop(l);
    let u = setup(&t, &base);
    let o = run_capped(u.clone(), Duration::from_secs(10), |_| {}).expect("a refused connection ends at once");
    assert!(matches!(&o, Outcome::Failed(_)), "{o:?}");
    assert!(o.miss().is_some());
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
    // Remembered: the next start within the hour does not wait again.
    assert!(matches!(u.check(), Outcome::Skipped(_)));
}

#[test]
fn a_pinned_version_stays_put() {
    let t = Temp::new("upd-pin");
    let (base, seen) = release("0.2.0", b"newer");
    // With the notice off, a pin asks nothing.
    let u = Updater { pin: Some("0.1.0".into()), notify: false, ..setup(&t, &base) };
    assert!(matches!(u.check(), Outcome::Skipped(m) if m.contains("pinned")));
    assert!(paths(&seen).is_empty(), "a pin with the notice off asks nothing");
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
    // With it on (the default), the signed manifest is read for the notice,
    // and the binary is still never fetched.
    let u = Updater { notify: true, ..u };
    assert_eq!(u.check(), Outcome::Available { latest: "0.2.0".into() });
    assert_eq!(paths(&seen), vec!["/manifest.json", "/manifest.json.sig"], "no binary was fetched");
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
}

// ------------------------------------------------- the update notice (#190)

/// A manifest's notice fields: two Highlights, and the plugin's files last
/// changed in 0.2.0.
const NOTES: &str = ",\"highlights\":[\"Faster recall\",\"A status line badge\"],\"plugin_changed_in\":\"0.2.0\"";

/// The notice settings of a release build at `current`, its plugin
/// installed at 0.1.0 (older than the 0.2.0 that changed the plugin).
fn policy(current: &str, install: bool, pin: Option<&str>) -> update::Policy {
    update::Policy { release: true, notify: true, install, pin: pin.map(str::to_string), current: current.into(), plugin: Some("0.1.0".into()) }
}

#[test]
fn a_pinned_or_off_check_reads_the_signed_manifest_and_never_downloads() {
    let t = Temp::new("upd-avail");
    let (base, seen) = release_with("0.2.0", b"newer", NOTES);
    let u = Updater { install: false, ..setup(&t, &base) };
    assert_eq!(u.check(), Outcome::Available { latest: "0.2.0".into() });
    assert_eq!(paths(&seen), vec!["/manifest.json", "/manifest.json.sig"]);
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
    let s = update::read_state(&u.state_dir).expect("the check wrote its state");
    assert_eq!(s.latest.as_deref(), Some("0.2.0"));
    assert_eq!(s.plugin_changed_in.as_deref(), Some("0.2.0"));
    assert_eq!(s.highlights, vec!["Faster recall", "A status line badge"]);
    // Within the hour the manifest is not asked for again.
    assert!(matches!(u.check(), Outcome::Skipped(_)));
    assert_eq!(paths(&seen).len(), 2);
}

#[test]
fn the_banner_is_shown_once_per_version_to_pinned_and_off_users() {
    let t = Temp::new("upd-banner");
    let (base, _) = release_with("0.2.0", b"newer", NOTES);
    // Pinned.
    let u = Updater { pin: Some("0.1.0".into()), ..setup(&t, &base) };
    assert_eq!(u.check(), Outcome::Available { latest: "0.2.0".into() });
    let p = policy("0.1.0", true, Some("0.1.0"));
    let b = update::banner_once(&u.state_dir, &p).expect("a pinned user is told");
    assert!(b.contains("atlas 0.2.0 is available") && b.contains("0.1.0") && b.contains("pinned"), "{b}");
    assert!(b.contains("/plugin"), "the installed plugin (0.1.0) is older than the 0.2.0 that changed it: {b}");
    assert_eq!(update::banner_once(&u.state_dir, &p), None, "once per version");
    // Off.
    let u = Updater { install: false, state_dir: t.path().join("state-off"), ..setup(&t, &base) };
    assert_eq!(u.check(), Outcome::Available { latest: "0.2.0".into() });
    let p = policy("0.1.0", false, None);
    let b = update::banner_once(&u.state_dir, &p).expect("an off user is told");
    assert!(b.contains("atlas 0.2.0 is available") && b.contains("atlas settings on update"), "{b}");
    assert_eq!(update::banner_once(&u.state_dir, &p), None);
    // The next version is told again.
    let (base3, _) = release("0.3.0", b"newer still");
    let u = Updater { url: format!("{base3}/manifest.json"), check_every: Duration::ZERO, ..u };
    assert_eq!(u.check(), Outcome::Available { latest: "0.3.0".into() });
    let b = update::banner_once(&u.state_dir, &p).expect("a new version is told");
    assert!(b.contains("0.3.0") && !b.contains("/plugin"), "0.3.0's manifest names no plugin change: {b}");
}

#[test]
fn the_plugin_note_compares_the_installed_plugin_with_the_release_that_changed_it() {
    let t = Temp::new("upd-plugin");
    let (base, _) = release_with("0.2.0", b"newer", NOTES);
    let u = Updater { pin: Some("0.1.0".into()), ..setup(&t, &base) };
    assert_eq!(u.check(), Outcome::Available { latest: "0.2.0".into() });
    let at = |plugin: Option<&str>| update::badge(&u.state_dir, &update::Policy { plugin: plugin.map(str::to_string), ..policy("0.1.0", true, Some("0.1.0")) }, None).unwrap();
    assert!(at(Some("0.1.0")).contains("/plugin"), "older than 0.2.0");
    assert!(!at(Some("0.2.0")).contains("/plugin"), "the plugin is already the one 0.2.0 needs");
    assert!(!at(Some("0.3.0")).contains("/plugin"));
    assert!(!at(None).contains("/plugin"), "no plugin installed: no /plugin to run");
    // The installed plugin's version is read beside the binary.
    let root = t.path().join("plugin");
    fs::create_dir_all(root.join(".claude-plugin")).unwrap();
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join(".claude-plugin").join("plugin.json"), "{\n  \"name\": \"atlas\",\n  \"version\": \"0.1.4\"\n}\n").unwrap();
    assert_eq!(update::installed_plugin_version(&root.join("bin").join("atlas.exe")).as_deref(), Some("0.1.4"));
    assert_eq!(update::installed_plugin_version(&t.path().join("elsewhere").join("atlas.exe")), None);
}

#[test]
fn an_auto_updated_user_is_told_once_with_the_signed_highlights() {
    let t = Temp::new("upd-told");
    let (base, _) = release_with("0.2.0", b"new binary", NOTES);
    let u = setup(&t, &base);
    assert_eq!(u.check(), Outcome::Swapped { from: "0.1.0".into(), to: "0.2.0".into() });
    let p = policy("0.2.0", true, None);
    let b = update::banner_once(&u.state_dir, &p).expect("told after the swap");
    assert!(b.contains("atlas updated 0.1.0 -> 0.2.0") && b.contains("/plugin"), "{b}");
    assert!(b.contains("Faster recall") && b.contains("A status line badge"), "{b}");
    assert_eq!(update::banner_once(&u.state_dir, &p), None, "once");
    assert_eq!(update::badge(&u.state_dir, &p, None), None, "nothing is pending");
}

#[test]
fn a_failed_check_keeps_the_updates_highlights_and_plugin_note() {
    let t = Temp::new("upd-keep");
    let (base, _) = release_with("0.2.0", b"new binary", NOTES);
    let u = setup(&t, &base);
    assert_eq!(u.check(), Outcome::Swapped { from: "0.1.0".into(), to: "0.2.0".into() });
    // Before any session starts on 0.2.0, a check fails (offline).
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    drop(l);
    let u = Updater { url: format!("{dead}/manifest.json"), current: "0.2.0".into(), check_every: Duration::ZERO, ..u };
    assert!(matches!(u.check(), Outcome::Failed(_)));
    let b = update::banner_once(&u.state_dir, &policy("0.2.0", true, None)).expect("still told");
    assert!(b.contains("Faster recall") && b.contains("A status line badge"), "the Highlights survive a failed check: {b}");
    assert!(b.contains("/plugin"), "so does the plugin note: {b}");
}

#[test]
fn highlights_are_bounded_and_old_manifests_still_parse() {
    let long = "x".repeat(500);
    let many: Vec<String> = (0..12).map(|i| format!("\"line {i} {long}\"")).collect();
    let m = format!("{{\"version\":\"1.2.3\",\"files\":[],\"highlights\":[{}],\"plugin_changed_in\":\"1.2.0\"}}", many.join(","));
    let m = update::parse_manifest(m.as_bytes()).unwrap();
    assert_eq!(m.highlights.len(), update::HIGHLIGHTS_MAX);
    assert!(m.highlights.iter().all(|h| h.chars().count() <= update::HIGHLIGHT_CHARS), "{:?}", m.highlights);
    assert_eq!(m.plugin_changed_in.as_deref(), Some("1.2.0"));
    // A control character never reaches the user's terminal.
    let m = update::parse_manifest(b"{\"version\":\"1.2.3\",\"files\":[],\"highlights\":[\"a\\u001b[31mb\\nc\"]}").unwrap();
    assert!(m.highlights[0].chars().all(|c| !c.is_control()), "{:?}", m.highlights);
    // Nor does a format character: a bidi override, a zero-width space or
    // joiner, a byte order mark, an isolate, a tag character.
    let m = update::parse_manifest("{\"version\":\"1.2.3\",\"files\":[],\"highlights\":[\"safe\u{202E}txt.exe\u{200B}\u{200D}\u{FEFF}\u{2066}x\u{2069}\u{E0041}\"]}".as_bytes()).unwrap();
    assert_eq!(m.highlights, vec!["safetxt.exex"]);
    assert_eq!(update::highlight_line("\u{200B}\u{202E}"), None, "nothing left");
    // A manifest from before the fields: no highlights, no plugin change.
    let m = update::parse_manifest(b"{\"version\":\"1.2.3\",\"files\":[]}").unwrap();
    assert!(m.highlights.is_empty() && m.plugin_changed_in.is_none());
    // The unreleased boolean, or a plugin release that is not a version.
    let m = update::parse_manifest(b"{\"version\":\"1.2.3\",\"files\":[],\"plugin_changed\":true}").unwrap();
    assert!(m.plugin_changed_in.is_none());
    let m = update::parse_manifest(b"{\"version\":\"1.2.3\",\"files\":[],\"plugin_changed_in\":\"soon\"}").unwrap();
    assert!(m.plugin_changed_in.is_none());
}

#[test]
fn source_builds_and_notify_off_are_told_nothing() {
    let t = Temp::new("upd-quiet");
    let (base, seen) = release_with("0.2.0", b"newer", NOTES);
    let u = Updater { install: false, ..setup(&t, &base) };
    assert_eq!(u.check(), Outcome::Available { latest: "0.2.0".into() });
    let source = update::Policy { release: false, ..policy("0.1.0", false, None) };
    assert_eq!(update::banner_once(&u.state_dir, &source), None, "a source build is told nothing");
    assert_eq!(update::badge(&u.state_dir, &source, None), None);
    let quiet = update::Policy { notify: false, ..policy("0.1.0", false, None) };
    assert_eq!(update::banner_once(&u.state_dir, &quiet), None, "notify off");
    assert_eq!(update::badge(&u.state_dir, &quiet, None), None);
    // Notify off with updates off: no network at all.
    let n = paths(&seen).len();
    let u = Updater { notify: false, state_dir: t.path().join("state-quiet"), ..u };
    assert!(matches!(u.check(), Outcome::Skipped(_)));
    assert_eq!(paths(&seen).len(), n, "nothing was asked");
}

#[test]
fn offline_or_a_bad_manifest_gives_no_notice() {
    let t = Temp::new("upd-nonotice");
    let p = policy("0.1.0", false, None);
    // Offline: for a check that only reads the notice, no error at all
    // (no miss is recorded, nothing is printed).
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    drop(l);
    let u = Updater { install: false, ..setup(&t, &dead) };
    let o = u.check();
    assert!(matches!(&o, Outcome::Skipped(_)) && o.miss().is_none(), "offline is no error: {o:?}");
    assert_eq!(update::banner_once(&u.state_dir, &p), None);
    assert_eq!(update::badge(&u.state_dir, &p, None), None);
    let pinned = Updater { install: true, pin: Some("0.1.0".into()), state_dir: t.path().join("state-pin"), ..u.clone() };
    let o = pinned.check();
    assert!(o.miss().is_none(), "pinned and offline is no error either: {o:?}");
    // Signed by another key.
    let (base, _) = release_with("0.2.0", b"newer", NOTES);
    let r = routes_of(&base);
    let m = r.lock().unwrap()["/manifest.json"].clone();
    let stranger = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new([7u8; 32]));
    r.lock().unwrap().insert("/manifest.json.sig".into(), stranger.sk.sign(&m, None).as_ref().to_vec());
    let u = Updater { install: false, state_dir: t.path().join("state-bad"), ..setup(&t, &base) };
    assert!(matches!(u.check(), Outcome::Refused(_)));
    assert_eq!(update::banner_once(&u.state_dir, &p), None);
    assert_eq!(update::badge(&u.state_dir, &p, None), None);
    // Unsigned.
    r.lock().unwrap().remove("/manifest.json.sig");
    let u = Updater { state_dir: t.path().join("state-unsigned"), ..u };
    assert!(matches!(u.check(), Outcome::Refused(_)));
    assert_eq!(update::banner_once(&u.state_dir, &p), None);
    assert_eq!(update::badge(&u.state_dir, &p, None), None);
}

#[test]
fn the_badge_appears_and_clears() {
    let t = Temp::new("upd-badge");
    let (base, _) = release_with("0.2.0", b"newer", NOTES);
    let u = Updater { pin: Some("0.1.0".into()), ..setup(&t, &base) };
    assert_eq!(u.check(), Outcome::Available { latest: "0.2.0".into() });
    let b = update::badge(&u.state_dir, &policy("0.1.0", true, Some("0.1.0")), None).expect("a pending update shows");
    assert!(b.contains("0.2.0"), "{b}");
    // The user moved the pin onto the newest version.
    assert_eq!(update::badge(&u.state_dir, &policy("0.2.0", true, Some("0.2.0")), None), None, "cleared");
}

#[test]
fn many_servers_on_the_old_version_install_the_release_once() {
    let t = Temp::new("upd-many");
    let new = b"new binary, version 0.2.0".to_vec();
    let (base, seen) = release_with("0.2.0", &new, NOTES);
    let u = Updater { live: true, check_every: Duration::ZERO, ..setup(&t, &base) };
    // Three servers started from 0.1.0 check at once.
    let outcomes: Vec<Outcome> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..3).map(|_| s.spawn(|| u.clone().check())).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let swapped = outcomes.iter().filter(|o| matches!(o, Outcome::Swapped { .. })).count();
    let installed = outcomes.iter().filter(|o| matches!(o, Outcome::Installed { latest } if latest == "0.2.0")).count();
    assert_eq!((swapped, installed), (1, 2), "{outcomes:?}");
    assert_eq!(paths(&seen).iter().filter(|p| *p == "/bin").count(), 1, "downloaded once");
    assert_eq!(fs::read(&u.exe).unwrap(), new);
    if cfg!(windows) {
        // One rename aside, not one per server (a later check may already
        // have deleted it: nothing runs it in this test).
        assert!(aside(&u.exe).len() <= 1, "{:?}", aside(&u.exe));
    }
    // A start check that finished past its cap and a server started later
    // find it on disk too.
    assert_eq!(Updater { live: false, ..u.clone() }.check(), Outcome::Installed { latest: "0.2.0".into() });
    assert_eq!(paths(&seen).iter().filter(|p| *p == "/bin").count(), 1);
}

#[test]
fn a_closed_gate_stops_a_swap() {
    let t = Temp::new("upd-gate");
    let (base, _) = release_with("0.2.0", b"new binary", NOTES);
    let u = setup(&t, &base);
    u.gate.close();
    let o = u.check();
    assert!(matches!(&o, Outcome::Skipped(m) if m.contains("stopping")), "{o:?}");
    assert_eq!(fs::read(&u.exe).unwrap(), OLD, "the binary on disk is untouched");
    assert!(aside(&u.exe).is_empty(), "nothing was renamed aside");
    // The next process's check installs it.
    let u = Updater { gate: update::SwapGate::default(), check_every: Duration::ZERO, ..u };
    assert!(matches!(u.check(), Outcome::Swapped { .. }));
}

/// Eight processes change one update state 25 times each, all at once: only
/// the file lock orders them (each process has its own turns), and not one
/// of the 200 changes is lost. The test binary runs itself as each child.
#[test]
fn state_changes_from_many_processes_are_never_lost() {
    const CHILD: &str = "ATLAS_TEST_STATE_CHILD";
    if let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) {
        // A child: wait for the start so all eight overlap, then change.
        let go = dir.with_extension("go");
        let start = Instant::now();
        while !go.exists() {
            assert!(start.elapsed() < Duration::from_secs(60), "no start signal");
            std::thread::sleep(Duration::from_millis(1));
        }
        for _ in 0..25 {
            // A long wait: a loaded machine is not a lost change.
            update::update_state_within(&dir, Duration::from_secs(60), |st| st.checked += 1).unwrap();
        }
        return;
    }
    let t = Temp::new("upd-lock");
    let dir = t.path().join("state");
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..8)
        .map(|_| {
            Command::new(&exe)
                .args(["--exact", "state_changes_from_many_processes_are_never_lost", "--nocapture", "--test-threads", "1"])
                .env(CHILD, &dir)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    fs::write(dir.with_extension("go"), "").unwrap();
    for mut c in children {
        assert!(c.wait().unwrap().success(), "a child failed");
    }
    assert_eq!(update::read_state(&dir).unwrap().checked, 200);
}

/// A change whose lock another process holds past the wait is an error,
/// never a silent drop: the state is untouched, and the same change lands
/// once the lock is free.
#[test]
fn a_state_change_past_its_lock_wait_is_an_error_not_lost() {
    let t = Temp::new("upd-held");
    let dir = t.path().join("state");
    update::update_state(&dir, |s| s.checked = 1).unwrap();
    // Another process's lock, held past the change's wait (a second lock in
    // one process waits on the first, as across processes).
    let held = atlas::fsio::FileLock::acquire(&dir.join(update::STATE_FILE)).unwrap();
    let wait = Duration::from_millis(300);
    let start = Instant::now();
    let r = update::update_state_within(&dir, wait, |s| s.checked = 2);
    let e = r.expect_err("a change that cannot take the lock is reported");
    assert!(e.contains("lock") && e.contains("nothing written"), "{e}");
    assert!(start.elapsed() < wait + Duration::from_secs(2), "waited {:?}", start.elapsed());
    assert_eq!(update::read_state(&dir).unwrap().checked, 1, "nothing half applied");
    drop(held);
    update::update_state_within(&dir, wait, |s| s.checked = 2).unwrap();
    assert_eq!(update::read_state(&dir).unwrap().checked, 2);
}

/// The threads of one process take the state in the order they came, and
/// one that gives up is skipped: the next in line is not stuck behind it.
#[test]
fn state_turns_are_served_in_order_and_skip_one_that_gave_up() {
    let turns = update::StateTurns::default();
    let first = turns.take(Instant::now()).expect("an empty queue serves at once");
    // While `first` holds its turn, a second gives up at its deadline.
    assert!(turns.take(Instant::now() + Duration::from_millis(50)).is_none());
    let order = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        let (turns, order) = (&turns, &order);
        let waiter = s.spawn(move || {
            let _t = turns.take(Instant::now() + Duration::from_secs(30)).expect("served after the one that gave up");
            order.lock().unwrap().push("waiter");
        });
        std::thread::sleep(Duration::from_millis(100));
        order.lock().unwrap().push("first");
        drop(first);
        waiter.join().unwrap();
    });
    assert_eq!(*order.lock().unwrap(), ["first", "waiter"]);
    assert!(turns.take(Instant::now()).is_some(), "the queue is free again");
}

#[test]
fn the_live_check_runs_when_the_last_check_expires() {
    let t = Temp::new("upd-every");
    let dir = t.path().join("state");
    let hour = Duration::from_secs(3600);
    assert_eq!(update::next_check_in(&dir, hour, hour), hour, "no state: a full interval");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    // Another server checked 40 minutes ago: 20 minutes (and a second), not
    // an hour plus the 40 minutes the check would skip.
    update::update_state(&dir, |s| s.checked = now - 40 * 60).unwrap();
    let w = update::next_check_in(&dir, hour, hour);
    assert!(w >= Duration::from_secs(20 * 60) && w <= Duration::from_secs(20 * 60 + 2), "{w:?}");
    // Just checked: an hour and a second.
    update::update_state(&dir, |s| s.checked = now).unwrap();
    assert!(update::next_check_in(&dir, hour, hour) <= hour + Duration::from_secs(1));
}

#[test]
fn a_live_server_stages_the_new_binary_and_is_told_to_reconnect() {
    let t = Temp::new("upd-live");
    let new = b"new binary, version 0.2.0".to_vec();
    let (base, _) = release_with("0.2.0", &new, NOTES);
    let u = Updater { live: true, check_every: Duration::ZERO, ..setup(&t, &base) };
    // A running process started from the installed binary (a real
    // executable, blocked reading its stdin) stands for the live server.
    fs::copy(env!("CARGO_BIN_EXE_atlas"), &u.exe).unwrap();
    let mut running = Command::new(&u.exe)
        .arg("--hook")
        .env("ATLAS_HOME", t.path().join("home"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let live = update::spawn_live(u.clone(), Duration::from_millis(50));
    let deadline = Instant::now() + Duration::from_secs(30);
    let swap = loop {
        if let Some(s) = update::read_state(&u.state_dir).and_then(|s| s.updated) {
            break s;
        }
        assert!(Instant::now() < deadline, "the live check staged nothing");
        std::thread::sleep(Duration::from_millis(20));
    };
    // The next checks find it on disk: no second download, no second notice.
    std::thread::sleep(Duration::from_millis(200));
    live.stop();
    assert_eq!((swap.from.as_str(), swap.to.as_str()), ("0.1.0", "0.2.0"));
    assert_eq!(fs::read(&u.exe).unwrap(), new, "the new binary is on disk");
    assert!(running.try_wait().unwrap().is_none(), "the running process was left alone");
    let told = update::take_live_notice().expect("the next hook answer tells the user");
    assert!(told.contains("atlas 0.2.0 ready") && told.contains("/mcp to reconnect"), "{told}");
    assert_eq!(update::take_live_notice(), None, "taken once");
    // Told once per version in this process, whoever staged it.
    update::tell_reconnect(&u.state_dir, "0.2.0", None);
    assert_eq!(update::take_live_notice(), None, "0.2.0 was told");
    // The hook and the status line run the binary on disk (0.2.0). Each
    // session's server recorded the version it runs: the reconnect notice
    // is per server.
    let p = policy("0.2.0", true, None);
    update::note_server(&u.state_dir, "s-old", "0.1.0");
    update::note_server(&u.state_dir, "s-new", "0.2.0");
    let badge = update::badge(&u.state_dir, &p, Some("s-old")).expect("pending until that session reconnects");
    assert!(badge.contains("/mcp") && badge.contains("/plugin"), "{badge}");
    assert_eq!(update::badge(&u.state_dir, &p, Some("s-new")), None, "a session on the new server has nothing to do");
    assert_eq!(update::badge(&u.state_dir, &p, Some("s-unknown")), None);
    assert_eq!(update::badge(&u.state_dir, &p, None), None);
    // A starting session starts its server from the binary on disk: its
    // banner says what changed, never to reconnect.
    let b = update::banner_once(&u.state_dir, &p).expect("the next session start says it");
    assert!(b.contains("atlas updated 0.1.0 -> 0.2.0") && b.contains("Faster recall") && !b.contains("/mcp"), "{b}");
    // The old session reconnects: its new server records 0.2.0.
    update::note_server(&u.state_dir, "s-old", "0.2.0");
    assert_eq!(update::badge(&u.state_dir, &p, Some("s-old")), None, "cleared by its reconnect");
    drop(running.stdin.take());
    let _ = running.wait();
}

#[test]
fn a_source_build_asks_nothing_and_shows_nothing() {
    // What a build from source does: a release build of the test binary
    // (ATLAS_RELEASE_BUILD at compile time) is not one.
    if update::release_build() {
        eprintln!("skipped: this binary is a release build");
        return;
    }
    let t = Temp::new("upd-source");
    let (base, seen) = release_with("9.9.9", b"newer", NOTES);
    let home = t.path().join("home");
    let state = home.join("update");
    fs::create_dir_all(&state).unwrap();
    // A state that would be news to a release build.
    fs::write(state.join("update-check.txt"), "checked 1\nlatest 9.9.9\nplugin-in 9.9.9\nupdated 0.0.1 9.9.9\n").unwrap();
    fs::create_dir_all(state.join("servers")).unwrap();
    fs::write(state.join("servers").join("s1.txt"), "0.0.1\n").unwrap();
    let cfg = t.path().join("atlas.json");
    fs::write(&cfg, format!("{{\"vaults\": [], \"update\": {{\"url\": \"{base}/manifest.json\", \"pin\": \"0.0.1\"}}}}")).unwrap();
    let run = |args: &[&str], input: &str| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_atlas"))
            .arg("--config")
            .arg(&cfg)
            .args(args)
            .env("ATLAS_HOME", &home)
            .current_dir(t.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let o = c.wait_with_output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let (ok, out) = run(&["--statusline"], "{\"session_id\":\"s1\"}");
    assert!(ok, "the status line command exists");
    assert_eq!(out.trim(), "", "a source build shows no badge");
    let ev = format!("{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"s1\",\"source\":\"startup\",\"cwd\":{:?}}}", t.path().to_string_lossy());
    let (_, out) = run(&["--hook"], &ev);
    assert!(!out.contains("systemMessage"), "a source build has no banner: {out}");
    // The server starts and stops at end of input without asking anything.
    let _ = run(&[], "");
    assert!(paths(&seen).is_empty(), "a source build makes no network call");
}

#[test]
fn the_update_settings_load_and_settings_off_writes_them() {
    let t = Temp::new("upd-cfg");
    let cfg = t.path().join("atlas.json");
    fs::write(&cfg, "{\"vaults\": [], \"update\": {\"pin\": \"0.1.0\"}}").unwrap();
    let c = Config::load(&cfg).unwrap();
    assert_eq!(c.update.as_ref().and_then(|u| u.pin.clone()).as_deref(), Some("0.1.0"));
    fs::write(&cfg, "{\"update\": {\"pin\": \"latest\"}}").unwrap();
    assert!(Config::load(&cfg).is_err(), "a pin is a version");
    fs::write(&cfg, "{\"update\": {\"url\": \"http://example.com/m.json\"}}").unwrap();
    assert!(Config::load(&cfg).is_err(), "plain http only to loopback");

    fs::write(&cfg, "{\"vaults\": []}").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_atlas")).arg("--config").arg(&cfg).args(["settings", "off", "update"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(Config::load(&cfg).unwrap().update.unwrap().enabled, Some(false));
    let out = Command::new(env!("CARGO_BIN_EXE_atlas")).arg("--config").arg(&cfg).args(["settings", "on", "update"]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(Config::load(&cfg).unwrap().update.unwrap().enabled, Some(true));
    let bad = Command::new(env!("CARGO_BIN_EXE_atlas")).arg("--config").arg(&cfg).args(["settings", "off", "everything"]).output().unwrap();
    assert!(!bad.status.success());
    // The bare-word form (the subcommand path; the configuration from
    // ATLAS_CONFIG) reaches the same switch.
    let out = Command::new(env!("CARGO_BIN_EXE_atlas")).env("ATLAS_CONFIG", &cfg).args(["settings", "off", "update"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(Config::load(&cfg).unwrap().update.unwrap().enabled, Some(false));
    // Any other `settings` form is the Claude Code settings command, not this switch.
    let other = Command::new(env!("CARGO_BIN_EXE_atlas")).env("ATLAS_CONFIG", &cfg).args(["settings", "list"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&other.stdout).contains("update: "), "routed to the update switch");
    assert_eq!(Config::load(&cfg).unwrap().update.unwrap().enabled, Some(false), "untouched by another settings form");
}

#[test]
fn notify_is_a_setting() {
    let t = Temp::new("upd-notify");
    let cfg = t.path().join("atlas.json");
    fs::write(&cfg, "{\"vaults\": [], \"update\": {\"notify\": false}}").unwrap();
    assert_eq!(Config::load(&cfg).unwrap().update.unwrap().notify, Some(false));
    fs::write(&cfg, "{\"vaults\": [], \"update\": {\"notify\": \"no\"}}").unwrap();
    assert!(Config::load(&cfg).is_err(), "notify is true or false");
}
