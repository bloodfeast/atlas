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
//! replaces itself, and makes no network call. Config `update.enabled:
//! false` (`atlas settings off update`) or a pinned version (`update.pin`)
//! turn the install off; the check still reads the signed manifest (same
//! rules, never the binary) so the user can be told a newer release exists,
//! unless `update.notify: false` turns that off too.
//!
//! Long sessions: a running server repeats the check hourly
//! ([`spawn_live`]) and stages a newer release on disk without touching
//! itself; command hooks start the new binary at once, and the server is
//! told to reconnect (`/mcp`). One install runs at a time (a lock in the
//! state folder), and a binary on disk that already is the release (its
//! SHA-256) is never downloaded again, so many servers on the old version
//! stage it once.
//!
//! The notice (issue #190) is read from local files only, never the
//! network: a session start banner (`systemMessage`, shown to the user and
//! not added to the model's context, once per version: [`banner_once`]) and
//! a status line badge while an update is pending ([`badge`]). The
//! reconnect notice is per server: each server records the version it runs
//! for the sessions it answers ([`note_session`]), and tells its own user
//! once ([`tell_reconnect`]). The Highlights come only from the signed
//! manifest; the `/plugin` note compares the manifest's
//! `plugin_changed_in` with the installed plugin's version.
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
    /// The release's Highlights lines (at most [`HIGHLIGHTS_MAX`], each at
    /// most [`HIGHLIGHT_CHARS`] characters); empty in a manifest without
    /// the field.
    pub highlights: Vec<String>,
    /// The release that last changed the plugin's own files (hooks,
    /// `.mcp.json`, skills), which only Claude Code's `/plugin` update
    /// installs: an installed plugin older than it needs `/plugin`. `None`
    /// in a manifest without the field (or with a value that is not
    /// `MAJOR.MINOR.PATCH`).
    pub plugin_changed_in: Option<String>,
}

/// The most Highlights lines a notice carries.
pub const HIGHLIGHTS_MAX: usize = 5;

/// The most characters of one Highlights line.
pub const HIGHLIGHT_CHARS: usize = 200;

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
    // The notice fields are optional: a manifest without them (or with a
    // shape this version does not know, such as the unreleased boolean
    // `plugin_changed`) has no highlights and no plugin change, never a
    // refusal.
    let highlights = v.get("highlights").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).filter_map(highlight_line).take(HIGHLIGHTS_MAX).collect()).unwrap_or_default();
    let plugin_changed_in = v.get("plugin_changed_in").and_then(Value::as_str).filter(|p| parse_version(p).is_some()).map(str::to_string);
    Ok(Manifest { version, files, highlights, plugin_changed_in })
}

/// True for a Unicode format character (general category Cf): bidi
/// overrides and isolates, zero-width spaces and joiners, the byte order
/// mark, soft hyphen, tag characters and the like. They draw nothing, and
/// some reorder or hide the text around them.
fn is_format_char(c: char) -> bool {
    matches!(
        c as u32,
        0xAD | 0x600..=0x605
            | 0x61C
            | 0x6DD
            | 0x70F
            | 0x890..=0x891
            | 0x8E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// One Highlights line as a notice shows it: control characters (a
/// terminal escape, a newline) made spaces, format characters (a bidi
/// override, a zero-width space) removed, runs of space made one, and at
/// most [`HIGHLIGHT_CHARS`] characters (a longer one is cut and ends in
/// `...`). `None` when nothing is left. The release step bounds the lines
/// it writes with this, and the reader bounds them again.
pub fn highlight_line(s: &str) -> Option<String> {
    let spaced: String = s.chars().filter(|c| !is_format_char(*c)).map(|c| if c.is_control() { ' ' } else { c }).collect();
    let t = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return None;
    }
    if t.chars().count() <= HIGHLIGHT_CHARS {
        return Some(t);
    }
    let cut: String = t.chars().take(HIGHLIGHT_CHARS - 3).collect();
    Some(format!("{}...", cut.trim_end()))
}

/// What a check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not checked: pinned, checked recently with nothing newer, the
    /// server stopping, or (a check that installs nothing) the release
    /// unreachable, which is no error.
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
    /// The newest release is already the binary on disk (another server,
    /// or an earlier check, put it there): nothing was downloaded.
    Installed {
        /// Its version.
        latest: String,
    },
    /// The manifest or its signature was refused.
    Refused(String),
    /// The check could not finish (offline, a download or a rename failed).
    Failed(String),
    /// A newer signed release exists and this check does not install it
    /// (pinned, or updates off): the notice tells the user.
    Available {
        /// Its version.
        latest: String,
    },
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

/// Closes the swap for a stopping server: [`SwapGate::close`] waits for a
/// swap in progress (two renames) and stops any later one, so a process
/// that exits never leaves the binary renamed aside with nothing in its
/// place. A download in progress is not waited for. Clones share the gate.
#[derive(Debug, Clone, Default)]
pub struct SwapGate(Arc<Mutex<bool>>);

impl SwapGate {
    /// Waits for a swap in progress, then refuses every later one.
    pub fn close(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
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
    /// Where the last check is remembered (the manifest is read into a
    /// file of its own there, removed after the check).
    pub state_dir: PathBuf,
    /// The `curl` binary.
    pub curl: PathBuf,
    /// The trusted public key.
    pub key: [u8; 32],
    /// The running version (in a live check: the version on disk, which
    /// moves forward as releases are staged).
    pub current: String,
    /// This binary's target triple.
    pub target: String,
    /// A pinned version: when set, nothing is checked or installed.
    pub pin: Option<String>,
    /// How long a check's result is trusted.
    pub check_every: Duration,
    /// Seconds a manifest or signature transfer may take.
    pub max_time: u64,
    /// Install a newer release (config `update.enabled`, default true).
    pub install: bool,
    /// Read the manifest for the notice even when not installing (config
    /// `update.notify`, default true).
    pub notify: bool,
    /// A check in a running server: a swap is staged on disk for the next
    /// start (the notice says `/mcp to reconnect`), never started now.
    pub live: bool,
    /// The swap's gate (see [`SwapGate`]).
    pub gate: SwapGate,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// How long one install may hold the install lock's waiters: a download of
/// a release binary.
const INSTALL_WAIT: Duration = Duration::from_secs(15 * 60);

/// The SHA-256 of a file, lowercase hex.
fn file_sha256(p: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = fs::File::open(p).ok()?;
    let mut h = crate::sha256::Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            return Some(h.hex());
        }
        h.update(&buf[..n]);
    }
}

impl Updater {
    fn models(&self, dir: &Path) -> Models {
        Models { dir: dir.to_path_buf(), curl: self.curl.clone(), auto_download: false, files: Vec::new() }
    }

    /// Records this check: the newest signed manifest seen (`None` when the
    /// check failed or was refused, which clears what an earlier check
    /// saw). A finished swap, with its own Highlights and plugin release,
    /// is kept.
    fn write_state(&self, latest: Option<&Manifest>) {
        let r = update_state(&self.state_dir, |s| {
            s.checked = unix_now();
            s.latest = latest.map(|m| m.version.clone());
            s.plugin_changed_in = latest.and_then(|m| m.plugin_changed_in.clone());
            s.highlights = latest.map(|m| m.highlights.clone()).unwrap_or_default();
        });
        if let Err(e) = r {
            eprintln!("atlas: update check not recorded (the next check runs again): {e}");
        }
    }

    /// Records a swap onto `m`, with what its notice shows.
    fn record_swap(&self, m: &Manifest) {
        let swap = Swap { from: self.current.clone(), to: m.version.clone(), highlights: m.highlights.clone(), plugin_changed_in: m.plugin_changed_in.clone() };
        if let Err(e) = update_state(&self.state_dir, |s| s.updated = Some(swap)) {
            eprintln!("atlas: the update to {} is installed, but its notice was not recorded: {e}", m.version);
        }
    }

    /// Runs the check to the end: see the module documentation.
    pub fn check(&self) -> Outcome {
        let installing = self.install && self.pin.is_none();
        if !installing && !self.notify {
            return Outcome::Skipped(match &self.pin {
                Some(p) => format!("pinned to {p}"),
                None => "updates are off".into(),
            });
        }
        let Some(cur) = parse_version(&self.current) else {
            return Outcome::Failed(format!("the running version {:?} is not MAJOR.MINOR.PATCH", self.current));
        };
        if installing {
            remove_aside(&self.exe);
        }
        if let Some(s) = read_state(&self.state_dir) {
            let fresh = unix_now().saturating_sub(s.checked) < self.check_every.as_secs();
            let newer = s.latest.as_deref().and_then(parse_version).is_some_and(|l| l > cur);
            // A newer release seen within the hour is fetched again only to
            // install it; the notice already has it.
            if fresh && !(newer && installing) {
                return Outcome::Skipped("checked within the hour".into());
            }
        }
        if let Err(e) = fs::create_dir_all(&self.state_dir) {
            return Outcome::Failed(format!("{}: {e}", self.state_dir.display()));
        }
        let m = self.models(&self.state_dir);
        // Each check reads into files of its own: servers checking at once
        // must not read each other's half-written manifest.
        let (man_path, sig_path) = check_files(&self.state_dir);
        let _cleanup = Remove(vec![man_path.clone(), sig_path.clone()]);
        let manifest = match m.fetch_small(&self.url, &man_path, MANIFEST_CAP, self.max_time) {
            Ok(b) => b,
            Err(e) => {
                // Offline: remembered, so the next start within the hour
                // does not wait again.
                self.write_state(None);
                // A check for the notice only fails open and quietly:
                // offline is no error (issue #190).
                if !installing {
                    return Outcome::Skipped(format!("release check unreachable: {e}"));
                }
                return Outcome::Failed(format!("release check: {e}"));
            }
        };
        let sig_url = format!("{}.sig", self.url);
        let sig = match m.fetch_small(&sig_url, &sig_path, 1024, self.max_time) {
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
        self.write_state(Some(&man));
        if latest <= cur {
            return Outcome::UpToDate { latest: man.version };
        }
        if !installing {
            return Outcome::Available { latest: man.version };
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
        // One install at a time across every server and start of this
        // user: the next one finds the release already on disk.
        let _install = match crate::fsio::FileLock::acquire_within(&self.state_dir.join("install"), INSTALL_WAIT) {
            Ok(l) => l,
            Err(e) => return Outcome::Failed(format!("release {}: {e}", man.version)),
        };
        if file_sha256(&self.exe).as_deref() == Some(f.sha256.as_str()) {
            return Outcome::Installed { latest: man.version };
        }
        let new_name = format!("{}.new", name.to_string_lossy());
        let spec = ModelFileSpec { id: "atlas".into(), file: new_name.clone(), url: f.url.clone(), sha256: f.sha256.clone(), size: f.size };
        if let Err(e) = self.models(dir).fetch(&spec, true, true) {
            return Outcome::Failed(format!("release {}: {e}", man.version));
        }
        let closed = self.gate.0.lock().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Outcome::Skipped(format!("release {}: the server is stopping; it is installed at the next check", man.version));
        }
        match swap(&self.exe, &dir.join(&new_name)) {
            Ok(()) => {
                self.record_swap(&man);
                drop(closed);
                Outcome::Swapped { from: self.current.clone(), to: man.version }
            }
            Err(e) => Outcome::Failed(format!("release {}: swap: {e}", man.version)),
        }
    }
}

/// The files one check reads the manifest and its signature into: named
/// for this process and this check, so concurrent checks never share one.
fn check_files(dir: &Path) -> (PathBuf, PathBuf) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stem = format!("manifest-{}-{n}", std::process::id());
    (dir.join(format!("{stem}.json")), dir.join(format!("{stem}.json.sig")))
}

/// Removes its files when dropped.
struct Remove(Vec<PathBuf>);

impl Drop for Remove {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = fs::remove_file(p);
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

/// A finished swap and what its notice shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Swap {
    /// The version replaced.
    pub from: String,
    /// The version installed.
    pub to: String,
    /// Its release's Highlights.
    pub highlights: Vec<String>,
    /// Its manifest's `plugin_changed_in`.
    pub plugin_changed_in: Option<String>,
}

/// The state file's contents (see [`STATE_FILE`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// When the manifest was last asked for (unix seconds).
    pub checked: u64,
    /// The newest version a signed manifest named at that check.
    pub latest: Option<String>,
    /// That manifest's `plugin_changed_in`.
    pub plugin_changed_in: Option<String>,
    /// That manifest's Highlights.
    pub highlights: Vec<String>,
    /// The last swap of the binary on disk, live or at a start. A failed
    /// check leaves it whole.
    pub updated: Option<Swap>,
}

impl State {
    /// Reads the file's text: one `key value` line each (`highlight` and
    /// `updated-highlight` once per line); unknown lines are skipped, so an
    /// older or newer atlas reads what it knows. `None` without a `checked`
    /// line.
    pub fn parse(text: &str) -> Option<State> {
        let mut s = State::default();
        let mut checked = None;
        let version = |v: &str| Some(v.trim().to_string()).filter(|v| parse_version(v).is_some());
        let mut swap: Option<Swap> = None;
        let mut swap_highlights = Vec::new();
        let mut swap_plugin = None;
        for l in text.lines() {
            match l.split_once(' ') {
                Some(("checked", v)) => checked = v.trim().parse().ok(),
                Some(("latest", v)) => s.latest = Some(v.trim().to_string()),
                Some(("plugin-in", v)) => s.plugin_changed_in = version(v),
                Some(("highlight", v)) => s.highlights.extend(highlight_line(v)),
                Some(("updated", v)) => swap = v.split_once(' ').map(|(a, b)| Swap { from: a.trim().to_string(), to: b.trim().to_string(), ..Swap::default() }),
                Some(("updated-plugin-in", v)) => swap_plugin = version(v),
                Some(("updated-highlight", v)) => swap_highlights.extend(highlight_line(v)),
                _ => {}
            }
        }
        s.checked = checked?;
        s.highlights.truncate(HIGHLIGHTS_MAX);
        swap_highlights.truncate(HIGHLIGHTS_MAX);
        s.updated = swap.map(|w| Swap { highlights: swap_highlights, plugin_changed_in: swap_plugin, ..w });
        Some(s)
    }

    /// The file's text.
    pub fn to_text(&self) -> String {
        let mut t = format!("checked {}\n", self.checked);
        if let Some(l) = &self.latest {
            t.push_str(&format!("latest {l}\n"));
        }
        if let Some(p) = &self.plugin_changed_in {
            t.push_str(&format!("plugin-in {p}\n"));
        }
        for h in &self.highlights {
            t.push_str(&format!("highlight {h}\n"));
        }
        if let Some(w) = &self.updated {
            t.push_str(&format!("updated {} {}\n", w.from, w.to));
            if let Some(p) = &w.plugin_changed_in {
                t.push_str(&format!("updated-plugin-in {p}\n"));
            }
            for h in &w.highlights {
                t.push_str(&format!("updated-highlight {h}\n"));
            }
        }
        t
    }
}

/// The state in `dir`, if a check ever wrote one.
pub fn read_state(dir: &Path) -> Option<State> {
    State::parse(&fs::read_to_string(dir.join(STATE_FILE)).ok()?)
}

/// How long a state change waits for another process changing the file.
pub const STATE_WAIT: Duration = Duration::from_secs(5);

/// Changes the state in `dir` under its lock (every server, start check
/// and hook of this user shares the file), starting from an empty state
/// when there is none, waiting at most [`STATE_WAIT`] in all.
///
/// # Errors
///
/// A message when the change did not land: the wait passed (another
/// process held the file all along), or the folder or file could not be
/// written. The change is then not applied at all, never half applied.
pub fn update_state(dir: &Path, f: impl FnOnce(&mut State)) -> Result<(), String> {
    update_state_within(dir, STATE_WAIT, f)
}

/// [`update_state`], waiting at most `wait` in all.
///
/// The threads of one process take turns first, in the order they came
/// (see [`StateTurns`]), so they never race each other for the file lock:
/// that lock is polled, and a thread polling it among many could lose
/// every round until its wait ran out. The file lock is then only
/// contended by other processes, for what is left of `wait`.
///
/// # Errors
///
/// As [`update_state`].
pub fn update_state_within(dir: &Path, wait: Duration, f: impl FnOnce(&mut State)) -> Result<(), String> {
    let deadline = std::time::Instant::now() + wait;
    let path = dir.join(STATE_FILE);
    let _turn = StateTurns::global().take(deadline).ok_or_else(|| {
        format!("{}: other threads of this process held the update state for {} ms; not saved", path.display(), wait.as_millis())
    })?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    let _held = crate::fsio::FileLock::acquire_or_time_out(&path, left)?.ok_or_else(|| {
        format!("{}: another atlas process held its lock for the whole {} ms wait; nothing written", path.display(), wait.as_millis())
    })?;
    let mut s = read_state(dir).unwrap_or_default();
    f(&mut s);
    crate::fsio::replace_atomic(&path, s.to_text().as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// The turns of one process's threads at the update state: a ticket queue,
/// served in the order taken. A thread whose deadline passes leaves the
/// queue (its ticket is skipped when its turn comes), so no thread waits
/// past its own deadline and none waits on one that gave up.
#[derive(Debug, Default)]
pub struct StateTurns {
    q: Mutex<TurnQueue>,
    turned: Condvar,
}

#[derive(Debug, Default)]
struct TurnQueue {
    /// The next ticket handed out.
    next: u64,
    /// The ticket whose turn it is.
    serving: u64,
    /// Tickets whose holders gave up before their turn.
    left: std::collections::BTreeSet<u64>,
}

/// A thread's turn at the update state, until dropped.
#[derive(Debug)]
pub struct Turn<'a> {
    turns: &'a StateTurns,
}

impl StateTurns {
    fn global() -> &'static StateTurns {
        static TURNS: std::sync::OnceLock<StateTurns> = std::sync::OnceLock::new();
        TURNS.get_or_init(StateTurns::default)
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, TurnQueue> {
        self.q.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits for this thread's turn until `deadline`; `None` when it did
    /// not come by then.
    pub fn take(&self, deadline: std::time::Instant) -> Option<Turn<'_>> {
        let mut q = self.queue();
        let mine = q.next;
        q.next += 1;
        while q.serving != mine {
            let now = std::time::Instant::now();
            if now >= deadline {
                q.left.insert(mine);
                return None;
            }
            q = self.turned.wait_timeout(q, deadline - now).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
        Some(Turn { turns: self })
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut q = self.turns.queue();
        // The next ticket still waiting: those that gave up are skipped.
        loop {
            q.serving += 1;
            let s = q.serving;
            if !q.left.remove(&s) {
                break;
            }
        }
        drop(q);
        self.turns.turned.notify_all();
    }
}

/// The file in the state folder that remembers which notices the session
/// start banner already showed.
pub const ANNOUNCED_FILE: &str = "update-announced.txt";

/// The version of the Claude Code plugin `exe` was installed with: the
/// `version` of `.claude-plugin/plugin.json` in the plugin folder that holds
/// `bin/<exe>`. `None` outside a plugin (a binary installed by hand).
pub fn installed_plugin_version(exe: &Path) -> Option<String> {
    let root = exe.parent()?.parent()?;
    let text = fs::read_to_string(root.join(".claude-plugin").join("plugin.json")).ok()?;
    let v = crate::json::parse(&text).ok()?;
    v.get("version").and_then(Value::as_str).filter(|v| parse_version(v).is_some()).map(str::to_string)
}

/// The notice settings of `spec` for this binary.
pub fn policy(spec: Option<&crate::config::UpdateSpec>) -> Policy {
    Policy {
        release: release_build(),
        notify: spec.and_then(|s| s.notify) != Some(false),
        install: spec.and_then(|s| s.enabled) != Some(false),
        pin: spec.and_then(|s| s.pin.clone()),
        current: version().into(),
        plugin: std::env::current_exe().ok().as_deref().and_then(installed_plugin_version),
    }
}

/// What decides a notice: the build, the settings and the version of the
/// binary asking (the hook and the status line run the binary on disk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// A release build ([`release_build`]); a source build is told nothing.
    pub release: bool,
    /// Config `update.notify` (default true).
    pub notify: bool,
    /// Config `update.enabled` (default true).
    pub install: bool,
    /// Config `update.pin`.
    pub pin: Option<String>,
    /// The asking binary's version.
    pub current: String,
    /// The installed plugin's version ([`installed_plugin_version`]):
    /// older than a release's `plugin_changed_in`, the notice says to run
    /// `/plugin`. `None` outside a plugin: no `/plugin` to run.
    pub plugin: Option<String>,
}

/// What the user is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A newer release this setup does not install (pinned or off).
    Available {
        /// The newer version.
        latest: String,
        /// The running one.
        current: String,
        /// The pin, when that is why.
        pin: Option<String>,
        /// The installed plugin needs `/plugin` for this release.
        plugin: bool,
    },
    /// The binary on disk is newer than the server of this session:
    /// reconnect to use it.
    Ready {
        /// The version on disk.
        to: String,
        /// The installed plugin needs `/plugin` for this release.
        plugin: bool,
    },
    /// The user was moved onto a newer release.
    Updated {
        /// The version before.
        from: String,
        /// The version now.
        to: String,
        /// That release's Highlights.
        highlights: Vec<String>,
        /// The installed plugin needs `/plugin` for this release.
        plugin: bool,
    },
}

/// The suffix a notice gets when the installed plugin is older than the
/// release that last changed the plugin's files.
const PLUGIN_NOTE: &str = "; this release also changes the plugin's files, so run /plugin to update it";

impl Notice {
    /// What the banner remembers it showed: the kind and the version.
    fn key(&self) -> String {
        match self {
            Notice::Available { latest, .. } => format!("available {latest}"),
            Notice::Ready { to, .. } => format!("ready {to}"),
            Notice::Updated { to, .. } => format!("updated {to}"),
        }
    }

    /// The session start banner's text.
    pub fn banner(&self) -> String {
        let plugin = |p: bool| if p { PLUGIN_NOTE } else { "" };
        match self {
            Notice::Available { latest, current, pin: Some(_), plugin: p } => {
                format!("atlas {latest} is available (you're on {current}, pinned): change update.pin in atlas's configuration to move{}", plugin(*p))
            }
            Notice::Available { latest, current, pin: None, plugin: p } => {
                format!("atlas {latest} is available (you're on {current}, updates off): atlas settings on update{}", plugin(*p))
            }
            Notice::Ready { to, plugin: p } => format!("atlas {to} ready: /mcp to reconnect{}", plugin(*p)),
            Notice::Updated { from, to, highlights, plugin: p } => {
                let mut t = format!("atlas updated {from} -> {to}{}", plugin(*p));
                for h in highlights {
                    t.push_str("\n- ");
                    t.push_str(h);
                }
                t
            }
        }
    }

    /// The status line badge's text: `None` for a finished update.
    pub fn badge(&self) -> Option<String> {
        let plugin = |p: bool| if p { " (+ /plugin)" } else { "" };
        match self {
            Notice::Available { latest, plugin: p, .. } => Some(format!("atlas {latest} available{}", plugin(*p))),
            Notice::Ready { to, plugin: p } => Some(format!("atlas {to}: /mcp to reconnect{}", plugin(*p))),
            Notice::Updated { .. } => None,
        }
    }
}

/// Whether an installed plugin at `installed` needs `/plugin` for a release
/// whose plugin files last changed in `changed_in`: both known, and the
/// plugin older.
pub fn plugin_needs_update(changed_in: Option<&str>, installed: Option<&str>) -> bool {
    match (changed_in.and_then(parse_version), installed.and_then(parse_version)) {
        (Some(c), Some(i)) => i < c,
        _ => false,
    }
}

/// The `plugin_changed_in` the state knows for release `v`: its swap
/// record's, else the newest manifest's when that is `v`.
fn plugin_changed_in_of<'a>(state: &'a State, v: &str) -> Option<&'a str> {
    match &state.updated {
        Some(w) if w.to == v => w.plugin_changed_in.as_deref(),
        _ if state.latest.as_deref() == Some(v) => state.plugin_changed_in.as_deref(),
        _ => None,
    }
}

/// The notice for `state` under `p`, if any. A source build, or
/// `update.notify: false`, has none. `server` is the version the asking
/// session's server runs, when known: older than the binary on disk, the
/// session is told to reconnect (tracked per server, so one session's
/// reconnect does not clear another's notice).
pub fn notice(state: &State, p: &Policy, server: Option<&str>) -> Option<Notice> {
    if !p.release || !p.notify {
        return None;
    }
    let cur = parse_version(&p.current)?;
    let plugin = |v: &str| plugin_needs_update(plugin_changed_in_of(state, v), p.plugin.as_deref());
    if server.and_then(parse_version).is_some_and(|s| s < cur) {
        return Some(Notice::Ready { to: p.current.clone(), plugin: plugin(&p.current) });
    }
    let auto = p.install && p.pin.is_none();
    if !auto {
        if let Some(l) = state.latest.as_deref().filter(|l| parse_version(l).is_some_and(|l| l > cur)) {
            return Some(Notice::Available { latest: l.to_string(), current: p.current.clone(), pin: p.pin.clone(), plugin: plugin(l) });
        }
    }
    if let Some(w) = state.updated.as_ref().filter(|w| w.to == p.current) {
        return Some(Notice::Updated { from: w.from.clone(), to: w.to.clone(), highlights: w.highlights.clone(), plugin: plugin(&w.to) });
    }
    None
}

/// The session start banner: the notice's text, at most once per kind and
/// version (remembered in [`ANNOUNCED_FILE`]). Reads and writes local files
/// only. Never the reconnect notice: a session that is starting starts its
/// server from the binary on disk, so it has nothing to reconnect to (that
/// notice comes from the running server itself, and from the badge).
pub fn banner_once(dir: &Path, p: &Policy) -> Option<String> {
    let n = notice(&read_state(dir)?, p, None)?;
    let key = n.key();
    let path = dir.join(ANNOUNCED_FILE);
    let _held = crate::fsio::FileLock::acquire_within(&path, STATE_WAIT).ok()?;
    let seen = fs::read_to_string(&path).unwrap_or_default();
    if seen.lines().any(|l| l == key) {
        return None;
    }
    // The last few keys are enough: versions only move forward.
    let mut keep: Vec<&str> = seen.lines().filter(|l| !l.is_empty()).collect();
    let skip = keep.len().saturating_sub(19);
    keep.drain(..skip);
    keep.push(&key);
    let t = keep.join("\n") + "\n";
    // Not remembered (a read-only folder): not shown, rather than shown at
    // every start.
    crate::fsio::replace_atomic(&path, t.as_bytes()).ok()?;
    Some(n.banner())
}

/// The status line badge for the session `session` (the status line's
/// input names it): only while an update is pending (available to a pinned
/// or off setup, or on disk and newer than that session's server). Reads
/// local files only.
pub fn badge(dir: &Path, p: &Policy, session: Option<&str>) -> Option<String> {
    let server = session.and_then(|s| server_version(dir, s));
    notice(&read_state(dir)?, p, server.as_deref())?.badge()
}

// ------------------------------------------- which server each session runs

/// The folder in the state folder where each session's server writes the
/// version it runs (one file per session id).
pub const SERVERS_DIR: &str = "servers";

/// How long a session's record is kept without its server writing it.
const SERVER_RECORD_DAYS: u64 = 14;

fn session_file(dir: &Path, session: &str) -> Option<PathBuf> {
    let ok = !session.is_empty() && session.len() <= 128 && session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then(|| dir.join(SERVERS_DIR).join(format!("{session}.txt")))
}

/// Records that the server of `session` runs `version`.
pub fn note_server(dir: &Path, session: &str, version: &str) {
    let Some(p) = session_file(dir, session) else { return };
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let _ = crate::fsio::replace_atomic(&p, format!("{version}\n").as_bytes());
}

/// The version the server of `session` runs, as it last recorded.
pub fn server_version(dir: &Path, session: &str) -> Option<String> {
    let t = fs::read_to_string(session_file(dir, session)?).ok()?;
    let v = t.trim();
    parse_version(v).map(|_| v.to_string())
}

/// This process's server: the state folder, its version, the sessions it
/// already recorded.
static SERVER: Mutex<Option<(PathBuf, String, std::collections::HashSet<String>)>> = Mutex::new(None);

/// Called when a server starts serving `version`: each session it answers
/// a hook for is recorded as running it ([`note_session`]), and records no
/// server wrote for [`SERVER_RECORD_DAYS`] are removed.
pub fn register_server(dir: &Path, version: &str) {
    if let Ok(rd) = fs::read_dir(dir.join(SERVERS_DIR)) {
        let old = Duration::from_secs(SERVER_RECORD_DAYS * 24 * 3600);
        for e in rd.flatten() {
            let stale = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > old);
            if stale {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    *SERVER.lock().unwrap_or_else(|p| p.into_inner()) = Some((dir.to_path_buf(), version.to_string(), std::collections::HashSet::new()));
}

/// Records, once per session, that this process's server (see
/// [`register_server`]) answers `session`. A no-op in any other process.
pub fn note_session(session: &str) {
    let mut g = SERVER.lock().unwrap_or_else(|p| p.into_inner());
    let Some((dir, version, seen)) = g.as_mut() else { return };
    if session.is_empty() || seen.contains(session) {
        return;
    }
    seen.insert(session.to_string());
    note_server(dir, session, version);
}

// ------------------------------------------------- the running server's check

/// The running server's hourly check (see [`spawn_live`]). Dropping it
/// stops the loop after its current check without waiting for it;
/// [`LiveCheck::stop`] waits.
#[derive(Debug)]
pub struct LiveCheck {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LiveCheck {
    /// Stops the loop and waits for a check in progress to finish.
    pub fn stop(mut self) {
        self.signal();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn signal(&self) {
        let (m, cv) = &*self.stop;
        *m.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cv.notify_all();
    }
}

impl Drop for LiveCheck {
    fn drop(&mut self) {
        self.signal();
    }
}

/// The reconnect notice this server left for the next hook answer the user
/// sees, and the version it last told (each version is told once per
/// server process).
static LIVE_NOTICE: Mutex<(Option<String>, Option<String>)> = Mutex::new((None, None));

/// Leaves this server's reconnect notice for release `to` (now on disk):
/// told once per version in this process, on the next hook answer the user
/// sees ([`take_live_notice`]). `dir` is the state folder (for the
/// release's plugin files), `plugin` the installed plugin's version.
pub fn tell_reconnect(dir: &Path, to: &str, plugin: Option<&str>) {
    let mut g = LIVE_NOTICE.lock().unwrap_or_else(|p| p.into_inner());
    if g.1.as_deref() == Some(to) {
        return;
    }
    let changed_in = read_state(dir).and_then(|s| plugin_changed_in_of(&s, to).map(str::to_string));
    let n = Notice::Ready { to: to.to_string(), plugin: plugin_needs_update(changed_in.as_deref(), plugin) };
    *g = (Some(n.banner()), Some(to.to_string()));
}

/// How long the live loop waits before its next check: until the hour the
/// last check (by any server, in the state file) is trusted for has passed,
/// and a second more, at most `every`. With no state, or a check already
/// due, `every`.
pub fn next_check_in(dir: &Path, check_every: Duration, every: Duration) -> Duration {
    let Some(s) = read_state(dir) else { return every };
    let due = s.checked.saturating_add(check_every.as_secs());
    let now = unix_now();
    if due >= now && !check_every.is_zero() {
        Duration::from_secs(due - now + 1).min(every)
    } else {
        every
    }
}

/// Repeats `u.check()` in a running server whose session may outlive many
/// releases, on its own thread. Each wait ends when the hour the last check
/// (by any server) is trusted for has passed ([`next_check_in`]), so one
/// check runs about every `every`. A newer release is staged on disk (never
/// started: the running server and its standard streams are not touched),
/// or found already there (another server staged it), and from then on the
/// loop compares against that version. Either leaves this server's user a
/// reconnect notice (with `u.notify`) for the next hook answer
/// ([`take_live_notice`]); a failure is told on stderr and the loop goes on.
pub fn spawn_live(mut u: Updater, every: Duration) -> LiveCheck {
    u.live = true;
    let plugin = installed_plugin_version(&u.exe);
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let s2 = stop.clone();
    let thread = std::thread::Builder::new()
        .name("atlas-update-live".into())
        .spawn(move || loop {
            {
                let wait = next_check_in(&u.state_dir, u.check_every, every);
                let (m, cv) = &*s2;
                let g = m.lock().unwrap_or_else(|p| p.into_inner());
                let (g, _) = cv.wait_timeout_while(g, wait, |stopped| !*stopped).unwrap_or_else(|p| p.into_inner());
                if *g {
                    return;
                }
            }
            match u.check() {
                Outcome::Swapped { to, .. } | Outcome::Installed { latest: to } => {
                    if u.notify {
                        tell_reconnect(&u.state_dir, &to, plugin.as_deref());
                    }
                    u.current = to;
                }
                o => {
                    if let Some(m) = o.miss() {
                        eprintln!("atlas: update (hourly check): {m}");
                    }
                }
            }
        })
        .ok();
    LiveCheck { stop, thread }
}

/// The notice the live check left for the next hook answer the user sees,
/// taken once.
pub fn take_live_notice() -> Option<String> {
    LIVE_NOTICE.lock().unwrap_or_else(|p| p.into_inner()).0.take()
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
///   finishes after the cap (so a late failure is still recorded, and a
///   late swap told as a reconnect).
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
