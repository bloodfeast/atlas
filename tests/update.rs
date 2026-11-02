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
use atlas::update::{run_capped, Outcome, Updater};
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

fn manifest(base: &str, version: &str, bin: &[u8]) -> Vec<u8> {
    format!(
        "{{\"version\":\"{version}\",\"files\":[{{\"target\":\"{TARGET}\",\"file\":\"atlas-{TARGET}\",\"url\":\"{base}/bin\",\"sha256\":\"{}\",\"size\":{}}}]}}",
        atlas::sha256::hex(bin),
        bin.len()
    )
    .into_bytes()
}

/// A release at `version` holding `bin`, signed by the test key.
fn release(version: &str, bin: &[u8]) -> (String, Seen) {
    // The manifest names the server's own URL: serve first, then fill.
    let (base, seen) = serve_late();
    let mut routes = HashMap::new();
    let m = manifest(&base, version, bin);
    let sig = key().sk.sign(&m, None);
    routes.insert("/manifest.json".into(), m);
    routes.insert("/manifest.json.sig".into(), sig.as_ref().to_vec());
    routes.insert("/bin".into(), bin.to_vec());
    install(&base, routes);
    (base, seen)
}

/// Routes filled after the port is known (see [`release`]).
static LATE: Mutex<Vec<(String, Arc<Mutex<HashMap<String, Vec<u8>>>>)>> = Mutex::new(Vec::new());

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
    let u = Updater { pin: Some("0.1.0".into()), ..setup(&t, &base) };
    assert!(matches!(u.check(), Outcome::Skipped(m) if m.contains("pinned")));
    assert!(paths(&seen).is_empty(), "a pin asks nothing");
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
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
