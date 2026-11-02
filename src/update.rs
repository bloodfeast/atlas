//! Self-update at session start: atlas starts on the newest signed release.
//!
//! Before the server answers its first message, a release build checks the
//! public release's manifest (at most once an hour, see [`CHECK_EVERY`]):
//!
//! 1. `manifest.json` and `manifest.json.sig` are fetched (the system curl,
//!    as for the model files: see [`crate::models`]). The signature is the
//!    raw 64-byte ed25519 signature over the manifest's exact bytes, checked
//!    against the public key built into atlas ([`TRUSTED_KEY_HEX`]) before
//!    the manifest is even parsed. A bad or missing signature is refused.
//! 2. A manifest whose version is not newer than the running one is a no-op
//!    (an old signed manifest cannot roll a binary back).
//! 3. The binary for this target is downloaded beside the running one as
//!    `<exe>.new` with the model downloader: resumed after an interruption,
//!    capped at its listed size, and renamed into place only when its
//!    SHA-256 matches the signed manifest.
//! 4. The swap: on Windows the running exe is renamed aside (`<exe>.old`,
//!    deleted at a later start) and the new one renamed into its place; on
//!    other systems one rename replaces it. A failed second rename puts the
//!    old binary back.
//!
//! The whole step is capped ([`DEFAULT_CAP`]); past the cap the installed
//! version starts and the check finishes in the background for the next
//! start. Any failure, offline included, starts what is installed and is
//! recorded as a miss: the check fails open, never closed.
//!
//! Only binaries built by `cargo xtask release` check (it sets
//! `ATLAS_RELEASE_BUILD` at compile time): a build from source never
//! replaces itself. Config `update.enabled: false`
//! (`atlas settings off update`) or a pinned version (`update.pin`) turn the
//! check off.
//!
//! Signature verification uses the `ed25519-compact` crate (default features
//! off, no dependencies of its own), the second named exception to the
//! no-dependency rule beside bundled SQLite: curve arithmetic is not
//! hand-written.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::ModelFileSpec;
use crate::json::Value;
use crate::models::{url_allowed, Models};

/// The public key releases are signed with: raw ed25519, 32 bytes, hex.
/// The private half is kept offline; `cargo xtask release` signs with it and
/// refuses to publish if this constant is ever [`TEST_KEY_HEX`]. Nothing
/// in configuration or the environment replaces it; only the library's
/// [`Updater::key`] field takes another key, which the binary never sets
/// from outside.
pub const TRUSTED_KEY_HEX: &str = "621dcb943436497a5028ddae8961dfcf306737eeaf8fba97ce155e9ed9e855c7";

/// The public half of the throwaway test key (seed
/// `b"atlas-update-test-key-not-secret"`, public on purpose). Never trusted
/// by a published release.
pub const TEST_KEY_HEX: &str = "a74f943d8f0741245e998be68e6ee423591d4cb97cc7a390fbd2e39cf14a7864";

/// Where the newest release's manifest is read from; its signature is at
/// the same URL with `.sig` appended.
pub const DEFAULT_URL: &str = "https://github.com/bloodfeast/atlas/releases/latest/download/manifest.json";

/// How long a check's result is trusted before the next start asks again.
pub const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

/// How long a start waits for the check before it starts the installed
/// version.
pub const DEFAULT_CAP: Duration = Duration::from_secs(10);

/// Set in the environment of a binary started right after a swap, so it
/// does not check again.
pub const UPDATED_ENV: &str = "ATLAS_UPDATED";

/// The most bytes a manifest may have.
pub const MANIFEST_CAP: u64 = 64 << 10;

/// The file in the state folder that remembers the last check.
pub const STATE_FILE: &str = "update-check.txt";

/// True for a binary built by `cargo xtask release`.
pub fn release_build() -> bool {
    option_env!("ATLAS_RELEASE_BUILD").is_some()
}

/// The running version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The release target this binary was built for, as the manifest names it,
/// or `None` for a platform releases do not cover.
pub fn target() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu")) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else {
        None
    }
}

/// Decodes 64 hex characters into a 32-byte key.
///
/// # Errors
///
/// A message if `hex` is not exactly 64 hex characters.
pub fn key_from_hex(hex: &str) -> Result<[u8; 32], String> {
    let b = hex.as_bytes();
    if b.len() != 64 {
        return Err(format!("a key is 64 hex characters, not {}", b.len()));
    }
    let nib = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("not a hex character: {:?}", c as char)),
        }
    };
    let mut k = [0u8; 32];
    for (i, k) in k.iter_mut().enumerate() {
        *k = nib(b[2 * i])? << 4 | nib(b[2 * i + 1])?;
    }
    Ok(k)
}

/// The built-in public key.
pub fn trusted_key() -> [u8; 32] {
    key_from_hex(TRUSTED_KEY_HEX).expect("TRUSTED_KEY_HEX is 64 hex characters")
}

/// True when `sig` is a valid ed25519 signature by `key` over `message`.
/// Anything malformed (a signature that is not 64 bytes, a key that is not
/// a curve point) is false.
pub fn verify(message: &[u8], sig: &[u8], key: &[u8; 32]) -> bool {
    let Ok(sig) = ed25519_compact::Signature::from_slice(sig) else { return false };
    ed25519_compact::PublicKey::new(*key).verify(message, &sig).is_ok()
}

/// A release version, `MAJOR.MINOR.PATCH` (an optional leading `v`).
/// Anything else (a pre-release suffix included) does not parse, so it is
/// never installed.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut it = s.split('.');
    let mut n = || -> Option<u64> {
        let p = it.next()?;
        if p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse().ok()
    };
    let v = (n()?, n()?, n()?);
    it.next().is_none().then_some(v)
}

/// One binary a manifest lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    /// The target triple it was built for.
    pub target: String,
    /// Its file name as published.
    pub file: String,
    /// Where it is downloaded from.
    pub url: String,
    /// Its SHA-256, lowercase hex.
    pub sha256: String,
    /// Its size in bytes.
    pub size: u64,
}

/// A release manifest: what `cargo xtask release` signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The release version.
    pub version: String,
    /// One entry per target built.
    pub files: Vec<ManifestFile>,
}

/// Parses a manifest's bytes. Call only after [`verify`] accepted them.
///
/// # Errors
///
/// A message naming the first field that is missing or malformed.
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    let v = crate::json::parse_bytes(bytes).map_err(|e| format!("manifest: {e}"))?;
    let s = |o: &Value, k: &str| -> Result<String, String> { o.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("manifest: {k} missing")) };
    let version = s(&v, "version")?;
    parse_version(&version).ok_or_else(|| format!("manifest: version {version:?} is not MAJOR.MINOR.PATCH"))?;
    let mut files = Vec::new();
    for f in v.get("files").and_then(Value::as_array).ok_or("manifest: files missing")? {
        let sha256 = s(f, "sha256")?.to_ascii_lowercase();
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("manifest: a sha256 is not 64 hex characters".into());
        }
        let file = s(f, "file")?;
        if file.is_empty() || file.starts_with('.') || file.contains(['/', '\\', ':']) {
            return Err(format!("manifest: file {file:?} is not a plain file name"));
        }
        let size = f.get("size").and_then(Value::as_u64).filter(|n| *n > 0).ok_or("manifest: a size is missing")?;
        files.push(ManifestFile { target: s(f, "target")?, file, url: s(f, "url")?, sha256, size });
    }
    Ok(Manifest { version, files })
}

/// What a check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not checked: pinned, or checked recently with nothing newer.
    Skipped(String),
    /// The newest signed release is not newer than this one.
    UpToDate {
        /// Its version.
        latest: String,
    },
    /// A newer release was installed over the running binary.
    Swapped {
        /// The version that was running.
        from: String,
        /// The version now in place.
        to: String,
    },
    /// The manifest or its signature was refused.
    Refused(String),
    /// The check could not finish (offline, a download or a rename failed).
    Failed(String),
}

impl Outcome {
    /// What to record as a miss: refusals and failures.
    pub fn miss(&self) -> Option<&str> {
        match self {
            Outcome::Refused(m) | Outcome::Failed(m) => Some(m),
            _ => None,
        }
    }
}

/// One check, fully described (the binary fills it from its build and
/// configuration; tests point it at a loopback server and a test key).
#[derive(Debug, Clone)]
pub struct Updater {
    /// The manifest URL; the signature is at the same URL plus `.sig`.
    pub url: String,
    /// The binary to replace.
    pub exe: PathBuf,
    /// Where the last check is remembered and the manifest is kept.
    pub state_dir: PathBuf,
    /// The `curl` binary.
    pub curl: PathBuf,
    /// The trusted public key.
    pub key: [u8; 32],
    /// The running version.
    pub current: String,
    /// This binary's target triple.
    pub target: String,
    /// A pinned version: when set, nothing is checked or installed.
    pub pin: Option<String>,
    /// How long a check's result is trusted.
    pub check_every: Duration,
    /// Seconds a manifest or signature transfer may take.
    pub max_time: u64,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Updater {
    fn models(&self, dir: &Path) -> Models {
        Models { dir: dir.to_path_buf(), curl: self.curl.clone(), auto_download: false, files: Vec::new() }
    }

    /// The last check: (unix seconds, newest version seen, if any).
    fn read_state(&self) -> Option<(u64, Option<String>)> {
        let t = fs::read_to_string(self.state_dir.join(STATE_FILE)).ok()?;
        let mut checked = None;
        let mut latest = None;
        for l in t.lines() {
            match l.split_once(' ') {
                Some(("checked", v)) => checked = v.trim().parse().ok(),
                Some(("latest", v)) => latest = Some(v.trim().to_string()),
                _ => {}
            }
        }
        Some((checked?, latest))
    }

    fn write_state(&self, latest: Option<&str>) {
        let mut t = format!("checked {}\n", unix_now());
        if let Some(l) = latest {
            t.push_str(&format!("latest {l}\n"));
        }
        let _ = fs::create_dir_all(&self.state_dir);
        let _ = crate::fsio::replace_atomic(&self.state_dir.join(STATE_FILE), t.as_bytes());
    }

    /// Runs the check to the end: see the module documentation.
    pub fn check(&self) -> Outcome {
        if let Some(p) = &self.pin {
            return Outcome::Skipped(format!("pinned to {p}"));
        }
        let Some(cur) = parse_version(&self.current) else {
            return Outcome::Failed(format!("the running version {:?} is not MAJOR.MINOR.PATCH", self.current));
        };
        remove_aside(&self.exe);
        if let Some((checked, latest)) = self.read_state() {
            let fresh = unix_now().saturating_sub(checked) < self.check_every.as_secs();
            let newer = latest.as_deref().and_then(parse_version).is_some_and(|l| l > cur);
            if fresh && !newer {
                return Outcome::Skipped("checked within the hour".into());
            }
        }
        if let Err(e) = fs::create_dir_all(&self.state_dir) {
            return Outcome::Failed(format!("{}: {e}", self.state_dir.display()));
        }
        let m = self.models(&self.state_dir);
        let manifest = match m.fetch_small(&self.url, &self.state_dir.join("manifest.json"), MANIFEST_CAP, self.max_time) {
            Ok(b) => b,
            Err(e) => {
                // Offline: remembered, so the next start within the hour
                // does not wait again.
                self.write_state(None);
                return Outcome::Failed(format!("release check: {e}"));
            }
        };
        let sig_url = format!("{}.sig", self.url);
        let sig = match m.fetch_small(&sig_url, &self.state_dir.join("manifest.json.sig"), 1024, self.max_time) {
            Ok(b) => b,
            Err(e) => {
                self.write_state(None);
                return Outcome::Refused(format!("no signature for the release manifest: {e}"));
            }
        };
        if !verify(&manifest, &sig, &self.key) {
            self.write_state(None);
            return Outcome::Refused("the release manifest's signature does not verify".into());
        }
        let man = match parse_manifest(&manifest) {
            Ok(m) => m,
            Err(e) => return Outcome::Refused(e),
        };
        let latest = parse_version(&man.version).expect("parse_manifest checked the version");
        self.write_state(Some(&man.version));
        if latest <= cur {
            return Outcome::UpToDate { latest: man.version };
        }
        let Some(f) = man.files.iter().find(|f| f.target == self.target) else {
            return Outcome::Failed(format!("release {} has no build for {}", man.version, self.target));
        };
        if !url_allowed(&f.url) {
            return Outcome::Refused(format!("release {}: URL not allowed: {}", man.version, f.url));
        }
        let (Some(dir), Some(name)) = (self.exe.parent(), self.exe.file_name()) else {
            return Outcome::Failed(format!("{}: no folder", self.exe.display()));
        };
        let new_name = format!("{}.new", name.to_string_lossy());
        let spec = ModelFileSpec { id: "atlas".into(), file: new_name.clone(), url: f.url.clone(), sha256: f.sha256.clone(), size: f.size };
        if let Err(e) = self.models(dir).fetch(&spec, true, true) {
            return Outcome::Failed(format!("release {}: {e}", man.version));
        }
        match swap(&self.exe, &dir.join(&new_name)) {
            Ok(()) => Outcome::Swapped { from: self.current.clone(), to: man.version },
            Err(e) => Outcome::Failed(format!("release {}: swap: {e}", man.version)),
        }
    }
}

/// `<exe>.old` and `<exe>.old-*`: binaries renamed aside by earlier swaps.
fn aside_files(exe: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else { return Vec::new() };
    let base = format!("{}.old", name.to_string_lossy());
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n == base || n.starts_with(&format!("{base}-"))
        })
        .map(|e| e.path())
        .collect()
}

/// Deletes binaries renamed aside by earlier swaps; one still running is
/// left for a later start.
fn remove_aside(exe: &Path) {
    for p in aside_files(exe) {
        let _ = fs::remove_file(p);
    }
}

/// Replaces `exe` with `new`. On Windows a running exe cannot be replaced
/// but can be renamed, so it is renamed aside first; if the second rename
/// fails the old one is put back.
fn swap(exe: &Path, new: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(new, fs::Permissions::from_mode(0o755))?;
        fs::rename(new, exe)
    }
    #[cfg(not(unix))]
    {
        let mut aside = PathBuf::from(format!("{}.old", exe.display()));
        if aside.exists() && fs::remove_file(&aside).is_err() {
            // An earlier one still running: a fresh name.
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
            aside = PathBuf::from(format!("{}.old-{}-{nanos}", exe.display(), std::process::id()));
        }
        fs::rename(exe, &aside)?;
        if let Err(e) = fs::rename(new, exe) {
            let _ = fs::rename(&aside, exe);
            return Err(e);
        }
        Ok(())
    }
}

enum Slot {
    Waiting,
    Done(Outcome),
    Abandoned,
}

/// Runs [`Updater::check`] on its own thread and waits at most `cap`.
///
/// # Arguments
///
/// * `u` - the check.
/// * `cap` - how long to wait.
/// * `late` - called on the check's thread with the outcome when it
///   finishes after the cap (so a late failure is still recorded).
///
/// # Returns
///
/// The outcome when the check finished within `cap`, else `None` (it goes
/// on in the background; a swap it makes takes effect at the next start).
pub fn run_capped(u: Updater, cap: Duration, late: impl FnOnce(Outcome) + Send + 'static) -> Option<Outcome> {
    let shared = Arc::new((Mutex::new(Slot::Waiting), Condvar::new()));
    let s2 = shared.clone();
    let spawned = std::thread::Builder::new().name("atlas-update".into()).spawn(move || {
        let o = u.check();
        let (m, cv) = &*s2;
        let mut slot = m.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*slot, Slot::Abandoned) {
            drop(slot);
            late(o);
        } else {
            *slot = Slot::Done(o);
            cv.notify_all();
        }
    });
    if let Err(e) = spawned {
        return Some(Outcome::Failed(format!("cannot start the update thread: {e}")));
    }
    let (m, cv) = &*shared;
    let slot = m.lock().unwrap_or_else(|p| p.into_inner());
    let (mut slot, _) = cv.wait_timeout_while(slot, cap, |s| matches!(s, Slot::Waiting)).unwrap_or_else(|p| p.into_inner());
    match std::mem::replace(&mut *slot, Slot::Abandoned) {
        Slot::Done(o) => Some(o),
        _ => None,
    }
}

/// Starts `exe` (the binary just swapped in) with this process's arguments
/// and standard streams, and waits for it. Nothing has been read from
/// standard input yet, so the new binary serves the whole session.
///
/// # Errors
///
/// An I/O error if it cannot be started.
pub fn reexec(exe: &Path) -> io::Result<ExitStatus> {
    Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(UPDATED_ENV, "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_key_constant_is_the_seeds_public_half() {
        let kp = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new(*b"atlas-update-test-key-not-secret"));
        assert_eq!(*kp.pk, key_from_hex(TEST_KEY_HEX).unwrap());
    }

    #[test]
    fn the_release_key_is_not_the_test_key() {
        assert_ne!(TRUSTED_KEY_HEX, TEST_KEY_HEX);
        assert_eq!(trusted_key().len(), 32);
    }

    #[test]
    fn versions_parse_strictly() {
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("v10.20.3"), Some((10, 20, 3)));
        for bad in ["", "1", "1.2", "1.2.3.4", "1.2.x", "1.2.3-rc1", "1..3", "+1.2.3"] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
    }

    #[test]
    fn a_signature_verifies_only_its_own_bytes() {
        let kp = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new(*b"atlas-update-test-key-not-secret"));
        let sig = kp.sk.sign(b"manifest", None);
        assert!(verify(b"manifest", sig.as_ref(), &kp.pk));
        assert!(!verify(b"manifesT", sig.as_ref(), &kp.pk));
        assert!(!verify(b"manifest", &sig.as_ref()[..63], &kp.pk));
        assert!(!verify(b"manifest", &[0u8; 64], &kp.pk));
    }

    /// The release format: `openssl pkeyutl -sign -rawin` (OpenSSL 3.2)
    /// with the test key's PEM over these exact bytes wrote this raw
    /// 64-byte signature. Ed25519 is deterministic, so it must verify here
    /// and be exactly what this crate signs.
    #[test]
    fn an_openssl_rawin_signature_verifies() {
        let msg = br#"{"version":"0.2.0","files":[]}"#;
        let hex = "19d18ad66b5278aef1962f74f927fbeb45268717a41ef5db58c1856e86ef6862efa64ae68b3447752332d0ebf1a917d8e2b16ce9cf90c43da42a0f47444d2e06";
        let sig: Vec<u8> = [&hex[..64], &hex[64..]].iter().flat_map(|h| key_from_hex(h).unwrap()).collect();
        let test_key = key_from_hex(TEST_KEY_HEX).unwrap();
        assert!(verify(msg, &sig, &test_key));
        assert!(!verify(msg, &sig, &trusted_key()), "the release key does not accept a test signature");
        let kp = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new(*b"atlas-update-test-key-not-secret"));
        assert_eq!(kp.sk.sign(msg, None).as_ref(), &sig[..]);
    }

    #[test]
    fn hex_keys_are_exactly_32_bytes() {
        assert!(key_from_hex(&"ab".repeat(31)).is_err());
        assert!(key_from_hex(&"zz".repeat(32)).is_err());
        assert_eq!(key_from_hex(&"0f".repeat(32)).unwrap(), [0x0f; 32]);
    }
}
