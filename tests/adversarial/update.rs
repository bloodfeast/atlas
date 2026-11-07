//! The release check refuses everything it should: a tampered binary or
//! manifest, a bad or truncated signature, a signature by another key, a
//! downgrade, a truncated download, a hash mismatch, an oversized
//! manifest, and URLs that only look like loopback. Test keys only.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::common::Temp;
use crate::gen::{each, HOSTILE};
use atlas::models::{url_allowed, Models};
use atlas::update::{parse_manifest, verify, Outcome, Updater};

const SEED: [u8; 32] = *b"adversarial-suite-test-key-00001";
const OTHER: [u8; 32] = *b"adversarial-suite-other-key-0002";
const TARGET: &str = "adv-target";
const OLD: &[u8] = b"installed binary 0.1.0";

fn kp(seed: [u8; 32]) -> ed25519_compact::KeyPair {
    ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new(seed))
}

type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A loopback server whose routes can be filled after it knows its port.
/// A route body starting with `TRUNC:` is announced at twice its length
/// and cut off.
fn server() -> (String, Files) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
    let r2 = routes.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { continue };
            let routes = r2.clone();
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(1) => head.push(b[0]),
                        _ => break,
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let body = routes.lock().unwrap().get(&path).cloned();
                match body {
                    Some(b) if b.starts_with(b"TRUNC:") => {
                        let b = &b[6..];
                        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len() * 2);
                        let _ = s.write_all(b);
                    }
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
    (base, routes)
}

fn manifest(base: &str, version: &str, sha: &str, size: usize) -> Vec<u8> {
    format!("{{\"version\":\"{version}\",\"files\":[{{\"target\":\"{TARGET}\",\"file\":\"atlas-{TARGET}\",\"url\":\"{base}/bin\",\"sha256\":\"{sha}\",\"size\":{size}}}]}}").into_bytes()
}

fn updater(t: &Temp, base: &str) -> Updater {
    let dir = t.path().join("bin");
    fs::create_dir_all(&dir).unwrap();
    let exe = dir.join(format!("atlas-{TARGET}"));
    fs::write(&exe, OLD).unwrap();
    Updater {
        url: format!("{base}/manifest.json"),
        exe,
        state_dir: t.path().join("state"),
        curl: Models::new(None, &t.path().join("db.sqlite")).curl,
        key: *kp(SEED).pk,
        current: "0.1.0".into(),
        target: TARGET.into(),
        pin: None,
        check_every: Duration::from_secs(3600),
        max_time: 5,
    }
}

/// Runs one check against a release built by `make` and asserts the
/// installed binary is untouched and the outcome is a refusal or failure.
fn refused(tag: &str, make: impl FnOnce(&str) -> HashMap<String, Vec<u8>>) -> Outcome {
    let t = Temp::new(tag);
    let (base, routes) = server();
    *routes.lock().unwrap() = make(&base);
    let u = updater(&t, &base);
    let o = u.check();
    assert_eq!(fs::read(&u.exe).unwrap(), OLD, "{tag}: the installed binary changed ({o:?})");
    assert!(!matches!(o, Outcome::Swapped { .. }), "{tag}: {o:?}");
    let leftovers: Vec<_> = fs::read_dir(u.exe.parent().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".new")).collect();
    assert!(leftovers.is_empty() || matches!(o, Outcome::Failed(_)), "{tag}: {leftovers:?}");
    o
}

fn signed(m: Vec<u8>, seed: [u8; 32]) -> (Vec<u8>, Vec<u8>) {
    let sig = kp(seed).sk.sign(&m, None).as_ref().to_vec();
    (m, sig)
}

fn routes(m: Vec<u8>, sig: Vec<u8>, bin: Vec<u8>) -> HashMap<String, Vec<u8>> {
    HashMap::from([("/manifest.json".to_string(), m), ("/manifest.json.sig".to_string(), sig), ("/bin".to_string(), bin)])
}

#[test]
fn every_tampered_release_is_refused() {
    let new = b"new binary 0.2.0".to_vec();
    let sha = atlas::sha256::hex(&new);
    // Signed by another key.
    let o = refused("adv-upd-key", |b| {
        let (m, s) = signed(manifest(b, "0.2.0", &sha, new.len()), OTHER);
        routes(m, s, new.clone())
    });
    assert!(matches!(o, Outcome::Refused(_)), "{o:?}");
    // Manifest changed after signing (one byte, and trailing white space).
    for edit in [0usize, 1] {
        let o = refused("adv-upd-tamper", |b| {
            let (mut m, s) = signed(manifest(b, "0.2.0", &sha, new.len()), SEED);
            if edit == 0 {
                let i = m.iter().position(|&c| c == b'2').unwrap();
                m[i] = b'9';
            } else {
                m.push(b' ');
            }
            routes(m, s, new.clone())
        });
        assert!(matches!(o, Outcome::Refused(_)), "{o:?}");
    }
    // Signatures: truncated, extended, zero, all ones, the manifest's own
    // hash, empty.
    for which in 0..6 {
        let o = refused("adv-upd-sig", |b| {
            let (m, mut s) = signed(manifest(b, "0.2.0", &sha, new.len()), SEED);
            match which {
                0 => s.truncate(63),
                1 => s.push(0),
                2 => s = vec![0; 64],
                3 => s = vec![0xff; 64],
                4 => s = atlas::sha256::hex(&m).into_bytes(),
                _ => s.clear(),
            }
            routes(m, s, new.clone())
        });
        assert!(matches!(o, Outcome::Refused(_)), "sig case {which}: {o:?}");
    }
    // The binary does not match the signed hash, or is cut short.
    let o = refused("adv-upd-hash", |b| {
        let (m, s) = signed(manifest(b, "0.2.0", &sha, new.len()), SEED);
        let mut evil = new.clone();
        evil[0] ^= 1;
        routes(m, s, evil)
    });
    assert!(matches!(o, Outcome::Failed(_) | Outcome::Refused(_)), "{o:?}");
    let o = refused("adv-upd-trunc", |b| {
        let (m, s) = signed(manifest(b, "0.2.0", &sha, new.len()), SEED);
        let mut t = b"TRUNC:".to_vec();
        t.extend_from_slice(&new[..new.len() / 2]);
        routes(m, s, t)
    });
    assert!(matches!(o, Outcome::Failed(_) | Outcome::Refused(_)), "{o:?}");
    // A truncated manifest (the signature then fails).
    let o = refused("adv-upd-mtrunc", |b| {
        let (m, s) = signed(manifest(b, "0.2.0", &sha, new.len()), SEED);
        let mut t = b"TRUNC:".to_vec();
        t.extend_from_slice(&m[..m.len() / 2]);
        let mut r = routes(m, s, new.clone());
        r.insert("/manifest.json".into(), t);
        r
    });
    assert!(!matches!(o, Outcome::Swapped { .. }));
    // A manifest past the size cap, even correctly signed.
    let o = refused("adv-upd-big", |b| {
        let mut m = manifest(b, "0.2.0", &sha, new.len());
        m.truncate(m.len() - 1);
        m.extend_from_slice(format!(",\"pad\":\"{}\"}}", "x".repeat(70 << 10)).as_bytes());
        let (m, s) = signed(m, SEED);
        routes(m, s, new.clone())
    });
    assert!(matches!(o, Outcome::Failed(_) | Outcome::Refused(_)), "{o:?}");
}

#[test]
fn a_signed_downgrade_or_same_version_is_not_installed() {
    let old = b"older binary 0.0.9".to_vec();
    let sha = atlas::sha256::hex(&old);
    for v in ["0.0.9", "0.1.0", "v0.1.0", "0.01.0"] {
        let o = refused("adv-upd-down", |b| {
            let (m, s) = signed(manifest(b, v, &sha, old.len()), SEED);
            routes(m, s, old.clone())
        });
        assert!(matches!(o, Outcome::UpToDate { .. } | Outcome::Refused(_)), "{v}: {o:?}");
    }
}

#[test]
fn signatures_verify_only_their_own_bytes_and_key() {
    let k = kp(SEED);
    let m = b"{\"version\":\"9.9.9\"}";
    let sig = k.sk.sign(m, None).as_ref().to_vec();
    assert!(verify(m, &sig, &k.pk));
    assert!(!verify(m, &sig, &kp(OTHER).pk));
    assert!(!verify(&m[..m.len() - 1], &sig, &k.pk));
    for i in 0..sig.len() {
        let mut s = sig.clone();
        s[i] ^= 0x01;
        assert!(!verify(m, &s, &k.pk), "a signature with byte {i} flipped verifies");
    }
    // A key that is not a curve point, all zeros, all ones.
    for key in [[0u8; 32], [0xffu8; 32], [1u8; 32]] {
        assert!(!verify(m, &sig, &key));
    }
    // The shipped key is not the test key of this suite.
    assert_ne!(atlas::update::trusted_key(), *k.pk);
}

#[test]
fn manifests_with_hostile_fields_are_refused() {
    let sha = "a".repeat(64);
    let ok = |file: &str, version: &str, sha: &str, size: &str| format!("{{\"version\":\"{version}\",\"files\":[{{\"target\":\"t\",\"file\":\"{file}\",\"url\":\"https://x/b\",\"sha256\":\"{sha}\",\"size\":{size}}}]}}");
    assert!(parse_manifest(ok("atlas", "1.2.3", &sha, "5").as_bytes()).is_ok());
    for (file, version, sh, size) in [
        ("../atlas", "1.2.3", sha.as_str(), "5"),
        ("..\\atlas", "1.2.3", sha.as_str(), "5"),
        ("C:atlas", "1.2.3", sha.as_str(), "5"),
        (".atlas", "1.2.3", sha.as_str(), "5"),
        ("", "1.2.3", sha.as_str(), "5"),
        ("dir/atlas", "1.2.3", sha.as_str(), "5"),
        ("atlas", "1.2.3-rc1", sha.as_str(), "5"),
        ("atlas", "1.2", sha.as_str(), "5"),
        ("atlas", "1.2.3.4", sha.as_str(), "5"),
        ("atlas", "1.2.9999999999", sha.as_str(), "5"),
        ("atlas", "-1.2.3", sha.as_str(), "5"),
        ("atlas", "1.2.3", "abc", "5"),
        ("atlas", "1.2.3", &"g".repeat(64), "5"),
        ("atlas", "1.2.3", sha.as_str(), "0"),
        ("atlas", "1.2.3", sha.as_str(), "-5"),
        ("atlas", "1.2.3", sha.as_str(), "\"5\""),
    ] {
        assert!(parse_manifest(ok(file, version, sh, size).as_bytes()).is_err(), "{file} {version} {sh} {size}");
    }
    each(70, 2000, |r| r.text_from(HOSTILE, 20).into_bytes(), |b| {
        let _ = parse_manifest(b);
    });
}

/// Plain http is allowed to loopback only; a URL whose authority merely
/// starts with a loopback name (`127.0.0.1:@host`, `localhost:80@host`)
/// names another host.
#[test]
fn only_https_or_real_loopback_urls_are_allowed() {
    for ok in ["https://github.com/x", "http://127.0.0.1:8080/x", "http://localhost/x", "http://[::1]:9/x", "http://127.0.0.1/"] {
        assert!(url_allowed(ok), "{ok}");
    }
    for bad in ["http://example.com/x", "ftp://x", "file:///etc/passwd", "https://", "http://127.0.0.2/x", "http://localhost.example.com/x", "HTTP://example.com", "", "http://0.0.0.0/x"] {
        assert!(!url_allowed(bad), "{bad}");
    }
    for (url, loopback) in [
        ("http://127.0.0.1:@example.com/bin", false),
        ("http://localhost:80@example.com/bin", false),
        ("http://[::1]:@example.com/bin", false),
        ("http://127.0.0.1:x@example.com/", false),
        ("http://localhost/@example.com", true),
    ] {
        assert_eq!(url_allowed(url), loopback, "{url}");
    }
}

/// The refusal branches after a valid signature: a manifest that does not
/// parse, a release with no build for this target, a URL that is not
/// https, and a running version that is not MAJOR.MINOR.PATCH.
#[test]
fn signed_but_unusable_releases_are_not_installed() {
    let new = b"new binary 0.3.0".to_vec();
    let sha = atlas::sha256::hex(&new);
    let o = refused("adv-upd-badjson", |_| {
        let (m, s) = signed(b"{\"version\":\"0.3.0\",\"files\":7}".to_vec(), SEED);
        routes(m, s, new.clone())
    });
    assert!(matches!(o, Outcome::Refused(_)), "{o:?}");
    let o = refused("adv-upd-notarget", |b| {
        let m = String::from_utf8(manifest(b, "0.3.0", &sha, new.len())).unwrap().replace(TARGET, "other-target");
        let (m, s) = signed(m.into_bytes(), SEED);
        routes(m, s, new.clone())
    });
    assert!(matches!(o, Outcome::Failed(_)), "{o:?}");
    let o = refused("adv-upd-url", |b| {
        let m = String::from_utf8(manifest(b, "0.3.0", &sha, new.len())).unwrap().replace(&format!("{b}/bin"), "http://example.com/bin");
        let (m, s) = signed(m.into_bytes(), SEED);
        routes(m, s, new.clone())
    });
    assert!(matches!(o, Outcome::Refused(_)), "{o:?}");
    let t = Temp::new("adv-upd-cur");
    let (base, _routes) = server();
    let u = Updater { current: "dev-build".into(), ..updater(&t, &base) };
    assert!(matches!(u.check(), Outcome::Failed(_)));
    assert_eq!(fs::read(&u.exe).unwrap(), OLD);
}
