//! The model files atlas downloads at setup: the semantic search and recall
//! embedders (see [`crate::embed`]).
//!
//! The list lives in the configuration (`models.files`): per file an `id`
//! (what `semantic.model_id` names), the `file` name it is stored under, an
//! `https` `url`, its `sha256` and its `size` in bytes. Changing the model is
//! a configuration change, not a code change.
//!
//! A download goes to `<file>.part` in the models folder (default
//! `models/` in [`crate::dirs::data_dir`]). An interrupted download is
//! resumed from the bytes already there with an HTTP range request; a server
//! that ignores the range is detected and the download restarts from zero.
//! When the part reaches the expected size it is hashed; a hash that does not
//! match is refused and the part deleted. Only a verified file is renamed to
//! its final name, so a file under its final name is always one that matched
//! its pin, and everything that loads a model only checks that the file
//! exists. Without the file, semantic search is unavailable and the recall
//! gate is lexical; both pick the model up when it appears.
//!
//! The transfer itself is done by the system `curl` (shipped with Windows
//! 10 and later, macOS and practically every Linux distribution), started
//! with an argument list, never through a shell. Atlas owns no TLS and links
//! no HTTP library: the dependency rule allows only bundled SQLite and the
//! ed25519 signature check (see `Cargo.toml`), and a hand-written TLS stack
//! would be the riskiest code in the crate. Atlas reaches the network only
//! here: model files at setup and first start, and the release check of
//! [`crate::update`] at session start; never at query time.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{ModelFileSpec, ModelsSpec};
use crate::sha256::Hasher;

/// A download lock is broken when neither it nor any `.part` file in the
/// folder has changed for this long: its holder died (a server is stopped
/// whenever its session ends, which can leave the lock behind while the
/// `curl` it started finishes on its own; the part file is then resumed or
/// verified by the next attempt).
pub const STALE_LOCK: Duration = Duration::from_secs(10 * 60);

/// The lock file in the models folder that keeps two processes from
/// downloading at once.
pub const LOCK_FILE: &str = ".download.lock";

/// Why a model file could not be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Another atlas process holds the download lock.
    Busy,
    /// `curl` could not be started, or failed.
    Curl(String),
    /// The transfer ended short of the expected size; the bytes so far are
    /// kept and the next attempt resumes from them.
    Incomplete {
        /// Bytes on disk.
        have: u64,
        /// Bytes expected.
        want: u64,
    },
    /// The file's SHA-256 does not match its pin; the file was deleted.
    HashMismatch {
        /// The pinned hash.
        expected: String,
        /// The hash of what was downloaded.
        actual: String,
    },
    /// The file system refused an operation.
    Io(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Busy => write!(f, "another atlas process is downloading models now"),
            FetchError::Curl(m) => write!(f, "download failed: {m}"),
            FetchError::Incomplete { have, want } => write!(f, "download incomplete: {have} of {want} bytes; run `atlas --setup` again to resume"),
            FetchError::HashMismatch { expected, actual } => {
                write!(f, "refused: sha256 {actual} does not match the pinned {expected}; the file was deleted")
            }
            FetchError::Io(m) => write!(f, "io: {m}"),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<std::io::Error> for FetchError {
    fn from(e: std::io::Error) -> Self {
        FetchError::Io(e.to_string())
    }
}

/// What a fetch did.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    /// The file was already there (and, when checked, matched its pin).
    Present,
    /// The file was downloaded and verified.
    Downloaded {
        /// Bytes that were already on disk from an earlier attempt.
        resumed_from: u64,
        /// Seconds the transfer and the hash took.
        secs: f64,
    },
}

/// The resolved model list.
#[derive(Debug, Clone)]
pub struct Models {
    /// The folder the files are stored in.
    pub dir: PathBuf,
    /// The `curl` binary.
    pub curl: PathBuf,
    /// Download missing files in the background when the server starts.
    pub auto_download: bool,
    /// The files.
    pub files: Vec<ModelFileSpec>,
}

fn default_curl() -> PathBuf {
    if cfg!(windows) {
        // The copy Windows ships, so a different curl earlier on PATH is not
        // picked up by accident.
        if let Some(root) = std::env::var_os("SystemRoot") {
            let p = PathBuf::from(root).join("System32").join("curl.exe");
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from("curl")
}

/// True for a URL atlas accepts in the model list: `https`, or plain `http`
/// to the loopback address (the tests' local server).
pub fn url_allowed(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest.is_empty();
    }
    let Some(rest) = url.strip_prefix("http://") else { return false };
    // Plain http only to loopback: the whole authority (up to the path)
    // must be a loopback host and an optional numeric port. A prefix check
    // let `http://127.0.0.1:@example.com/` through, which names the host
    // after the `@`.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|host| match authority.strip_prefix(host) {
        Some("") => rest.len() > authority.len(),
        Some(port) => port.strip_prefix(':').is_some_and(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())),
        None => false,
    })
}

/// Holds the download lock while alive.
struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// How long the lock and every `.part` file in `dir` have gone unchanged.
fn quiet_for(dir: &Path, lock: &Path) -> Duration {
    let age = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok()).unwrap_or_default();
    let mut quiet = age(lock);
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().ends_with(".part") {
                quiet = quiet.min(age(&e.path()));
            }
        }
    }
    quiet
}

fn take_lock(dir: &Path) -> Result<Lock, FetchError> {
    let path = dir.join(LOCK_FILE);
    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = std::io::Write::write_all(&mut f, format!("pid {}\n", std::process::id()).as_bytes());
                return Ok(Lock(path));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if quiet_for(dir, &path) > STALE_LOCK {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                return Err(FetchError::Busy);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(FetchError::Busy)
}

/// The SHA-256 of a file, read in pieces.
///
/// # Errors
///
/// [`FetchError::Io`] if the file cannot be read.
pub fn hash_file(path: &Path) -> Result<String, FetchError> {
    let mut f = File::open(path)?;
    let mut h = Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.hex())
}

fn len_of(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

type Fetches = Vec<(String, Result<Fetched, FetchError>)>;

impl Models {
    /// Resolves the configured list.
    ///
    /// # Arguments
    ///
    /// * `spec` - the configuration's `models`, if any.
    /// * `db` - the database file; the models folder falls back to `models/`
    ///   beside it when no data folder is known.
    pub fn new(spec: Option<&ModelsSpec>, db: &Path) -> Models {
        let dir = spec
            .and_then(|s| s.dir.clone())
            .or_else(|| crate::dirs::data_dir().map(|d| d.join("models")))
            .unwrap_or_else(|| db.parent().unwrap_or(Path::new(".")).join("models"));
        Models {
            dir,
            curl: spec.and_then(|s| s.curl.clone()).unwrap_or_else(default_curl),
            auto_download: spec.and_then(|s| s.auto_download).unwrap_or(true),
            files: spec.map(|s| s.files.clone()).unwrap_or_default(),
        }
    }

    /// The final path of the file with this id, if it is listed.
    pub fn path(&self, id: &str) -> Option<PathBuf> {
        self.files.iter().find(|f| f.id == id).map(|f| self.dir.join(&f.file))
    }

    /// The listed files not yet under their final name.
    pub fn missing(&self) -> Vec<&ModelFileSpec> {
        self.files.iter().filter(|f| !self.dir.join(&f.file).is_file()).collect()
    }

    /// Fetches every missing file, holding the download lock.
    ///
    /// # Arguments
    ///
    /// * `quiet` - no progress output (the server's background download).
    ///
    /// # Returns
    ///
    /// Each missing file's id and result, in list order.
    ///
    /// # Errors
    ///
    /// [`FetchError::Busy`] if another process holds the lock, or an I/O
    /// error creating the folder.
    pub fn fetch_missing(&self, quiet: bool) -> Result<Fetches, FetchError> {
        let missing = self.missing();
        if missing.is_empty() {
            return Ok(Vec::new());
        }
        fs::create_dir_all(&self.dir)?;
        let _lock = take_lock(&self.dir)?;
        Ok(missing.into_iter().map(|f| (f.id.clone(), self.fetch_locked(f, false, quiet))).collect())
    }

    /// Fetches one file, or with `check` re-verifies one already present
    /// (a mismatching file is deleted and downloaded again).
    ///
    /// # Arguments
    ///
    /// * `f` - the file.
    /// * `check` - hash a file that is already present.
    /// * `quiet` - no progress output.
    ///
    /// # Errors
    ///
    /// See [`FetchError`]; [`FetchError::Busy`] if another process holds the
    /// download lock.
    pub fn fetch(&self, f: &ModelFileSpec, check: bool, quiet: bool) -> Result<Fetched, FetchError> {
        fs::create_dir_all(&self.dir)?;
        let _lock = take_lock(&self.dir)?;
        self.fetch_locked(f, check, quiet)
    }

    fn fetch_locked(&self, f: &ModelFileSpec, check: bool, quiet: bool) -> Result<Fetched, FetchError> {
        let dest = self.dir.join(&f.file);
        if dest.is_file() {
            if !check {
                return Ok(Fetched::Present);
            }
            let actual = hash_file(&dest)?;
            if actual == f.sha256 && len_of(&dest) == f.size {
                return Ok(Fetched::Present);
            }
            // A file under the final name that no longer matches (edited, or
            // the pin changed): it is not used, and is fetched again.
            fs::remove_file(&dest)?;
        }
        let part = self.dir.join(format!("{}.part", f.file));
        let t0 = Instant::now();
        let mut have = len_of(&part);
        if have > f.size {
            fs::remove_file(&part)?;
            have = 0;
        }
        let resumed_from = have;
        if have < f.size {
            match self.curl(&f.url, &part, have > 0, f.size, quiet, None) {
                // 33: the server does not honour ranges. The bytes kept are
                // useless; start over once from zero.
                Err((33, _)) => {
                    fs::remove_file(&part)?;
                    self.curl(&f.url, &part, false, f.size, quiet, None).map_err(|(_, m)| FetchError::Curl(m))?;
                }
                Err((_, m)) => {
                    let now = len_of(&part);
                    if now > f.size {
                        let _ = fs::remove_file(&part);
                    }
                    return Err(FetchError::Curl(m));
                }
                Ok(()) => {}
            }
        }
        let got = len_of(&part);
        if got != f.size {
            if got > f.size {
                fs::remove_file(&part)?;
            }
            return Err(FetchError::Incomplete { have: got.min(f.size), want: f.size });
        }
        let actual = hash_file(&part)?;
        if actual != f.sha256 {
            fs::remove_file(&part)?;
            return Err(FetchError::HashMismatch { expected: f.sha256.clone(), actual });
        }
        fs::rename(&part, &dest)?;
        Ok(Fetched::Downloaded { resumed_from, secs: t0.elapsed().as_secs_f64() })
    }

    /// Fetches a small file of unknown size (a release manifest or its
    /// signature) in one transfer, no resume, no lock.
    ///
    /// # Arguments
    ///
    /// * `url` - an allowed URL (see [`url_allowed`]).
    /// * `out` - where the bytes go; replaced.
    /// * `cap` - the most bytes accepted (curl's `--max-filesize`).
    /// * `max_time` - seconds before the transfer is abandoned.
    ///
    /// # Errors
    ///
    /// [`FetchError::Curl`] if the URL is not allowed or the transfer fails.
    pub fn fetch_small(&self, url: &str, out: &Path, cap: u64, max_time: u64) -> Result<Vec<u8>, FetchError> {
        if !url_allowed(url) {
            return Err(FetchError::Curl(format!("URL not allowed: {url}")));
        }
        let _ = fs::remove_file(out);
        self.curl(url, out, false, cap, true, Some(max_time)).map_err(|(_, m)| FetchError::Curl(m))?;
        let b = fs::read(out)?;
        if b.len() as u64 > cap {
            return Err(FetchError::Curl(format!("{url}: larger than {cap} bytes")));
        }
        Ok(b)
    }

    /// Runs curl once. `Err((exit code, message))` on failure (-1 when it
    /// could not start). `max_time` bounds the whole transfer in seconds.
    fn curl(&self, url: &str, out: &Path, resume: bool, size: u64, quiet: bool, max_time: Option<u64>) -> Result<(), (i32, String)> {
        let proto = if url.starts_with("https://") { "=https" } else { "=http" };
        let mut cmd = Command::new(&self.curl);
        cmd.args(["--fail", "--location", "--show-error", "--retry", "3", "--retry-delay", "2", "--connect-timeout", "30"])
            .args(["--proto", proto, "--proto-redir", proto])
            .arg("--max-filesize")
            .arg(size.to_string());
        if quiet {
            cmd.arg("--silent");
        } else {
            cmd.arg("--progress-bar");
        }
        if resume {
            cmd.args(["--continue-at", "-"]);
        }
        if let Some(t) = max_time {
            cmd.arg("--max-time").arg(t.to_string());
        }
        cmd.arg("--output").arg(out).arg("--url").arg(url);
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
        cmd.stderr(if quiet { Stdio::piped() } else { Stdio::inherit() });
        let res = cmd.output().map_err(|e| (-1, format!("cannot run {}: {e}", self.curl.display())))?;
        if res.status.success() {
            return Ok(());
        }
        let code = res.status.code().unwrap_or(-1);
        let err = String::from_utf8_lossy(&res.stderr).trim().to_string();
        Err((code, if err.is_empty() { format!("curl exited {code}") } else { format!("curl exited {code}: {err}") }))
    }
}

/// The semantic model file a configuration names: `semantic.model` when
/// given, else the listed file whose id is `semantic.model_id` (default
/// [`crate::config::DEFAULT_MODEL_ID`]), else `models/embed.atm` beside the
/// database.
///
/// # Arguments
///
/// * `c` - the configuration, if any.
/// * `db` - the database file.
pub fn semantic_model(c: Option<&crate::config::Config>, db: &Path) -> PathBuf {
    let sem = c.and_then(|c| c.semantic.as_ref());
    if let Some(p) = sem.and_then(|s| s.model.clone()) {
        return p;
    }
    let id = sem.and_then(|s| s.model_id.as_deref()).unwrap_or(crate::config::DEFAULT_MODEL_ID);
    let listed = c.and_then(|c| c.models.as_ref()).and_then(|m| Models::new(Some(m), db).path(id));
    listed.unwrap_or_else(|| db.parent().unwrap_or(Path::new(".")).join("models").join("embed.atm"))
}
