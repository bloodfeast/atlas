//! The MCP server's own log, `atlas-server.log` in the folder of the
//! database the server opened: one line when `atlas` starts serving, one per
//! panic, and one when it stops, with the reason.
//!
//! The log is purely diagnostic, so that a user can attach it to a public
//! bug report. Its first line says so. Each event line is
//! `TIME pid=N v=VERSION EVENT ...`, the time in UTC:
//!
//! * `start mode=serve os=OS arch=ARCH`
//! * `panic thread=NAME at=src/FILE.rs:LINE|external|unknown msg="..."`
//!   (or `payload omitted` in place of `msg=`)
//! * `exit reason=eof|reexec|refused uptime=N.Ns responses=N`
//! * `exit reason=io site=SITE kind=KIND at=src/FILE.rs:LINE uptime=N.Ns responses=N`
//! * `exit reason=failed site=SITE at=src/FILE.rs:LINE uptime=N.Ns responses=N`
//!
//! A start line with no exit line after it (for the same pid) means the
//! process was killed without unwinding: TerminateProcess, SIGKILL, an abort,
//! a stack overflow or a power loss.
//!
//! An allowlist, enforced by the types: no free text is ever written. The
//! log's API takes only enums, integers, an [`io::ErrorKind`] (logged by its
//! name), a `&'static Location` (logged as `src/FILE.rs:LINE` when it is in
//! atlas's own source, else `external`) and the `&'static str` constants in
//! this file. No `String`, error value, path, name or argument reaches it, so
//! no Display or Debug text of one can be written. An error's text still
//! goes to stderr, where the user sees it locally; the log gets its code.
//!
//! A panic's message is written only when its payload is a `&'static str`
//! and the panic is in atlas's own `src/`, which makes it a literal in
//! atlas's source (atlas never leaks a runtime string to `'static` or panics
//! with a custom payload: a test scans `src/` for both). Any other payload,
//! and any panic outside atlas's source, logs `payload omitted`. The thread
//! is named only when it is one of atlas's own threads ([`THREADS`]), else
//! `other`.
//!
//! Only the serve path writes here; `atlas --hook` and the CLI modes do not.
//! Every function takes the log's path, so a test needs no globals.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// The log's file name.
pub const FILE: &str = "atlas-server.log";

/// The size at which the log is renamed to `atlas-server.log.1` (replacing
/// an older one) before the next line is written.
pub const ROTATE_AT: u64 = 1 << 20;

/// A rotation lock older than this was left by a process that died while
/// rotating; the next appender takes it over.
pub const STALE_LOCK: Duration = Duration::from_secs(10);

/// The first line of every log file.
pub const HEADER: &str = "# atlas-server.log: diagnostic only, safe to attach to a GitHub issue. \
Each line holds the UTC time, pid, atlas version and one event: start (mode, OS, arch), \
panic (atlas thread name, location in atlas's source, the message only when it is a literal in atlas's source), \
exit (reason, site code, io error kind, location in atlas's source, uptime, responses written). \
Allowlist only: no free text is written, so no request contents, tool arguments, record text, \
error text, names or paths.";

/// Atlas's own thread names, the only ones the log writes (besides
/// `atlas-watch-N`); any other thread is `other`.
pub const THREADS: &[&str] = &[
    "main",
    "atlas-hooks",
    "atlas-reader",
    "atlas-hook-log",
    "atlas-code-index",
    "atlas-embed",
    "atlas-rec-embed",
    "atlas-grammar",
    "atlas-calibrate",
    "atlas-update",
    "atlas-update-live",
    "atlas-tokens",
    "atlas-catch",
    "atlas-endsession",
    "atlas-restart-point",
    "atlas-restart-warn",
];

/// The longest panic literal written, in characters.
const MAX_MSG: usize = 200;

/// Response lines written to the client by this process.
static RESPONSES: AtomicU64 = AtomicU64::new(0);

/// Counts one response line written to the client.
pub fn responded() {
    RESPONSES.fetch_add(1, Ordering::Relaxed);
}

/// The response lines written so far.
pub fn responses() -> u64 {
    RESPONSES.load(Ordering::Relaxed)
}

/// The log beside a database: `atlas-server.log` in its folder.
pub fn path_for(db: &Path) -> PathBuf {
    db.parent().map(|d| d.join(FILE)).unwrap_or_else(|| PathBuf::from(FILE))
}

/// Where in the serve path an error ended the server: a fixed code each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    /// The vault list could not be opened (`Vaults::new`).
    VaultsNew,
    /// The index store could not be opened.
    StoreOpen,
    /// Reading or writing the client while refusing (a newer atlas's store).
    ServeRefused,
    /// Reading or writing the client while serving.
    Serve,
}

impl Site {
    /// The code written to the log.
    pub const fn code(self) -> &'static str {
        match self {
            Site::VaultsNew => "vaults-new",
            Site::StoreOpen => "store-open",
            Site::ServeRefused => "serve-refused",
            Site::Serve => "serve",
        }
    }
}

/// Why the server stopped. Holds no text: an error is its [`Site`], its
/// [`io::ErrorKind`] when it is an io error, and the line in atlas's source
/// that returned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The client closed stdin.
    Eof,
    /// An updated binary was started in this one's place and has exited.
    Reexec,
    /// The store was too new for this binary: every call was refused until
    /// the client closed stdin.
    Refused,
    /// An io error.
    Io {
        /// Where in the serve path.
        site: Site,
        /// The error's kind, logged by its name.
        kind: io::ErrorKind,
        /// The line in atlas's source that returned it.
        at: &'static Location<'static>,
    },
    /// Any other error, by its site.
    Failed {
        /// Where in the serve path.
        site: Site,
        /// The line in atlas's source that returned it.
        at: &'static Location<'static>,
    },
}

impl Exit {
    /// An io error of `kind` at `site`, located at the caller.
    #[track_caller]
    pub fn io(site: Site, kind: io::ErrorKind) -> Exit {
        Exit::Io { site, kind, at: Location::caller() }
    }

    /// A non-io error at `site`, located at the caller.
    #[track_caller]
    pub fn failed(site: Site) -> Exit {
        Exit::Failed { site, at: Location::caller() }
    }
}

/// An atlas thread's name, or `other` for one atlas did not name. The name
/// written is the constant from [`THREADS`] (or the parsed number of an
/// `atlas-watch-N`), never the text the thread carries.
fn thread_name(name: Option<&str>) -> String {
    let Some(n) = name else { return "other".into() };
    if let Some(t) = THREADS.iter().find(|t| **t == n) {
        return (*t).into();
    }
    match n.strip_prefix("atlas-watch-") {
        Some(d) if !d.is_empty() && d.len() <= 6 && d.bytes().all(|b| b.is_ascii_digit()) => match d.parse::<u32>() {
            Ok(i) => format!("atlas-watch-{i}"),
            Err(_) => "other".into(),
        },
        _ => "other".into(),
    }
}

/// A source location: `src/...rs:LINE` when the file is in atlas's own
/// source, otherwise `external`, so no path outside atlas's tree is
/// written. Cargo hands a workspace member's files to rustc as relative
/// paths (`src/...`), so atlas's own locations are relative already. The
/// folder atlas was built in is never compiled in: an `env!` of
/// `CARGO_MANIFEST_DIR` would put that absolute path into every binary
/// (`tests/serve_log.rs` checks no shipped code does).
fn location(l: &Location<'_>) -> String {
    match source_file(l.file(), "") {
        Some(f) => format!("{f}:{}", l.line()),
        None => "external".into(),
    }
}

/// `file` relative to atlas's source with `/` separators, when it is a
/// `src/...rs` file of atlas; `None` otherwise. A `file` under `root` (a
/// folder a test names; empty for none) has that prefix stripped first.
/// No indexing: this runs inside the panic hook.
fn source_file(file: &str, root: &str) -> Option<String> {
    let f = file.replace('\\', "/");
    let dir = root.replace('\\', "/");
    let dir = dir.trim_end_matches('/');
    let under = match (f.get(..dir.len()), f.get(dir.len()..)) {
        (Some(head), Some(tail)) if !dir.is_empty() && head.eq_ignore_ascii_case(dir) => tail.strip_prefix('/'),
        _ => None,
    };
    let rel = under.unwrap_or(f.as_str());
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    let atlas = rel.strip_prefix("src/").and_then(|r| r.strip_suffix(".rs")).is_some_and(|r| r.split('/').all(ok));
    atlas.then(|| rel.to_string())
}

/// A literal panic message as one quoted value: printable ASCII but `"`
/// and `\`, cut at [`MAX_MSG`] characters.
fn quoted(msg: &'static str) -> String {
    let body: String = msg.chars().take(MAX_MSG).map(|c| if (' '..='~').contains(&c) && c != '"' && c != '\\' { c } else { '?' }).collect();
    format!("msg=\"{body}\"")
}

/// One serving process's log: its path and when it started.
#[derive(Debug, Clone)]
pub struct Log {
    path: PathBuf,
    started: Instant,
}

impl Log {
    /// A log at `path`; uptime counts from now.
    pub fn new(path: PathBuf) -> Log {
        Log { path, started: Instant::now() }
    }

    /// The log's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes the start line.
    pub fn start(&self) {
        sweep_temps(&self.path);
        append(&self.path, &format!("start mode=serve os={} arch={}", std::env::consts::OS, std::env::consts::ARCH));
    }

    /// Writes the exit line, with the uptime and the responses written.
    pub fn exit(&self, why: Exit) {
        let reason = match why {
            Exit::Eof => "reason=eof".to_string(),
            Exit::Reexec => "reason=reexec".to_string(),
            Exit::Refused => "reason=refused".to_string(),
            Exit::Io { site, kind, at } => format!("reason=io site={} kind={kind:?} at={}", site.code(), location(at)),
            Exit::Failed { site, at } => format!("reason=failed site={} at={}", site.code(), location(at)),
        };
        let up = self.started.elapsed().as_secs_f64();
        append(&self.path, &format!("exit {reason} uptime={up:.1}s responses={}", responses()));
    }

    /// Installs a panic hook that writes a panic line here, then runs the
    /// hook it replaced (the default prints the panic to stderr).
    pub fn install_panic_hook(&self) {
        let path = self.path.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            append(&path, &panic_line(info.location(), info.payload().downcast_ref::<&'static str>().copied()));
            previous(info);
        }));
    }
}

/// The panic event: thread, location, and the message only when it is a
/// `&'static str` and the location is in atlas's own source. No unwrap,
/// expect, indexing or lock: this runs inside the panic hook.
fn panic_line(at: Option<&Location<'_>>, literal: Option<&'static str>) -> String {
    let thread = std::thread::current();
    let name = thread_name(thread.name());
    let at = at.map(location).unwrap_or_else(|| "unknown".into());
    let msg = match literal {
        Some(s) if at.starts_with("src/") => quoted(s),
        _ => "payload omitted".into(),
    };
    format!("panic thread={name} at={at} {msg}")
}

/// Appends one event line to the log at `path`, rotating it first when it
/// has reached [`ROTATE_AT`]; a new file gets [`HEADER`] as its first line.
/// The line goes in one `write_all` to a file opened in append mode, so
/// lines from concurrent servers sharing the folder do not interleave. A
/// failure is reported on stderr (ignoring a failure to write that) and
/// otherwise ignored: the log never stops the server. Private: every caller
/// is a `Log` method, which takes no text.
fn append(path: &Path, event: &str) {
    if let Err(e) = try_append(path, event) {
        let _ = writeln!(io::stderr(), "atlas: server log {FILE}: {e}");
    }
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Renames a full log to `.1` under the rotation lock (`.lock`, created
/// with `create_new`), so that of several appenders sharing the folder only
/// one rotates, and only a log that is still full when it holds the lock.
/// Another holder's lock means another appender is rotating: skip. A lock
/// older than [`STALE_LOCK`], or dated more than that in the future, is
/// taken over. Every failure is ignored. Known limit: two appenders that
/// take over the same stale lock at once can both hold it; with a third
/// appender creating a fresh log between their renames, the second rename
/// can replace `.1` with that small file. It needs a crash mid-rotation and
/// three appenders within milliseconds, and loses at most one rotated file.
/// The holder also sweeps stale header temporaries.
fn rotate_if_full(path: &Path) {
    if fs::metadata(path).map(|m| m.len()).unwrap_or(0) < ROTATE_AT {
        return;
    }
    let lock = sibling(path, ".lock");
    let take = || OpenOptions::new().write(true).create_new(true).open(&lock);
    let held = match take() {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let stale = fs::metadata(&lock).and_then(|m| m.modified()).is_ok_and(|t| is_stale(t, SystemTime::now()));
            stale && fs::remove_file(&lock).is_ok() && take().is_ok()
        }
        Err(_) => false,
    };
    if !held {
        return;
    }
    // Re-stat under the lock: another appender may have rotated it already.
    if fs::metadata(path).map(|m| m.len()).unwrap_or(0) >= ROTATE_AT {
        let _ = fs::rename(path, sibling(path, ".1"));
    }
    sweep_temps(path);
    let _ = fs::remove_file(&lock);
}

/// Whether a file dated `mtime` is older than [`STALE_LOCK`] at `now`, or
/// dated more than [`STALE_LOCK`] after it (the clock stepped back after a
/// crash). A date only a little ahead, as file-system time granularity can
/// give a live lock, is not stale.
fn is_stale(mtime: SystemTime, now: SystemTime) -> bool {
    let apart = now.duration_since(mtime).unwrap_or_else(|e| e.duration());
    apart > STALE_LOCK
}

/// Removes header temporaries (`atlas-server.log.PID.NANOS.new`, from
/// [`create_with_header`]) that a process killed mid-create, or a refused
/// remove, left in the log's folder. Only stale ones go, so another
/// appender's temporary in use is kept. Every failure is ignored.
fn sweep_temps(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else { return };
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let Ok(entries) = fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let file = e.file_name();
        let Some(mid) =
            file.to_str().and_then(|f| f.strip_prefix(name)).and_then(|f| f.strip_prefix('.')).and_then(|f| f.strip_suffix(".new"))
        else {
            continue;
        };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let temp = mid.split_once('.').is_some_and(|(pid, nanos)| digits(pid) && digits(nanos));
        if temp && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| is_stale(t, now)) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Creates the log holding only [`HEADER`], atomically when the file system
/// allows: the header is written to a file of this process's own and hard
/// linked into place, which fails when another appender created it first.
/// So a log never holds a line before its header, nor two headers.
fn create_with_header(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = sibling(path, &format!(".{}.{nanos}.new", std::process::id()));
    let head = format!("{HEADER}\n");
    let linked = fs::write(&tmp, &head).and_then(|()| fs::hard_link(&tmp, path));
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        // No hard links here (FAT, some network shares): create it and
        // write the header, unless another appender created it first.
        Err(_) => match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => f.write_all(head.as_bytes()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        },
    }
}

fn try_append(path: &Path, event: &str) -> io::Result<()> {
    rotate_if_full(path);
    let line = format!("{} pid={} v={} {event}\n", crate::store::utc(SystemTime::now()), std::process::id(), env!("CARGO_PKG_VERSION"));
    let open = || OpenOptions::new().append(true).open(path);
    let mut file = match open() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            create_with_header(path)?;
            open()?
        }
        r => r?,
    };
    // An empty file (made by something other than this module) gets the
    // header, decided from the open handle.
    let text = if file.metadata()?.len() == 0 { format!("{HEADER}\n{line}") } else { line };
    file.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn path_is_beside_the_database() {
        assert_eq!(path_for(Path::new("state/atlas.sqlite")), Path::new("state").join(FILE));
    }

    #[test]
    fn locations_and_thread_names_are_atlas_own_or_not_written() {
        let built = r"C:\Users\Jo Smith\.cargo\git\checkouts\atlas-1a2b\0c1d2e3";
        let src = |f: &str| source_file(f, built);
        assert_eq!(src("src/protocol.rs").as_deref(), Some("src/protocol.rs"));
        assert_eq!(src(r"src\calibrate\mod.rs").as_deref(), Some("src/calibrate/mod.rs"));
        assert_eq!(src(&format!(r"{built}\src\protocol.rs")).as_deref(), Some("src/protocol.rs"));
        assert_eq!(src("c:/users/jo smith/.cargo/git/checkouts/atlas-1a2b/0c1d2e3/src/main.rs").as_deref(), Some("src/main.rs"));
        assert_eq!(src(&format!(r"{built}x\src\protocol.rs")), None);
        assert_eq!(src("/home/jo/.cargo/registry/src/x/lib.rs"), None);
        assert_eq!(src("/home/jo/src/lib.rs"), None);
        assert_eq!(src("tests/serve_log.rs"), None);
        assert_eq!(src("src/Jo Smith.rs"), None);
        assert_eq!(src("src/../../jo/x.rs"), None);
        assert_eq!(src("C:"), None);
        assert_eq!(src("\u{e9}"), None);
        assert_eq!(location(Location::caller()), format!("src/serve_log.rs:{}", line!()));
        for n in THREADS {
            assert_eq!(thread_name(Some(n)), *n);
        }
        assert_eq!(thread_name(Some("atlas-watch-3")), "atlas-watch-3");
        for n in ["atlas-watch-", "atlas-watch-x", "atlas-watch-1234567", "atlas-worker", "atlas-falcon-merger", "jo's thread", "Main"] {
            assert_eq!(thread_name(Some(n)), "other", "{n}");
        }
        assert_eq!(thread_name(None), "other");
        assert_eq!(quoted("a \"b\"\n\\c"), "msg=\"a ?b???c\"");
    }

    /// A literal panic in atlas's own source keeps its location and message,
    /// and a failing append (the log path is a folder) does not break the
    /// hook: the panic still unwinds to `join` instead of aborting. One test,
    /// so the process-wide hook is installed once; other tests' panics may
    /// land in the file too, so lines are found by their location.
    #[test]
    fn a_literal_panic_in_src_is_logged_and_a_failing_append_is_survived() {
        let dir = std::env::temp_dir().join(format!("atlas-serve-log-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("as-folder").join(FILE)).unwrap();
        let good = dir.join(FILE);
        let seen = Arc::new(Mutex::new(0));
        let quiet = std::panic::take_hook();
        let s = seen.clone();
        std::panic::set_hook(Box::new(move |_| {
            if let Ok(mut n) = s.lock() {
                *n += 1;
            }
        }));
        Log::new(dir.join("as-folder").join(FILE)).install_panic_hook();
        Log::new(good.clone()).install_panic_hook();
        let line = line!() + 1;
        let h = std::thread::Builder::new().name("atlas-hooks".into()).spawn(|| panic!("a literal in src")).unwrap();
        let joined = h.join();
        // Formatted (String) payloads raised in atlas's own source: the
        // location is written, the text never is. Built at run time, so
        // format_args cannot fold it into a literal.
        let secret = format!("jo-smith-{}-falcon", std::process::id());
        let named = || std::thread::Builder::new().name("atlas-hooks".into());
        let s = secret.clone();
        let fmt_line = line!() + 1;
        let fmt = named().spawn(move || panic!("fmt {}", s)).unwrap().join();
        let s = secret.clone();
        let none = std::hint::black_box(None::<u8>);
        let expect_line = line!() + 1;
        let expect = named().spawn(move || none.expect(&s)).unwrap().join();
        std::panic::set_hook(quiet);
        assert!(joined.is_err() && fmt.is_err() && expect.is_err(), "the panics unwound");
        assert!(*seen.lock().unwrap() >= 3, "the first hook still ran");
        let text = fs::read_to_string(&good).unwrap();
        let want = format!(" panic thread=atlas-hooks at=src/serve_log.rs:{line} msg=\"a literal in src\"");
        assert!(text.lines().any(|l| l.ends_with(&want)), "{want} in\n{text}");
        for n in [fmt_line, expect_line] {
            let want = format!(" panic thread=atlas-hooks at=src/serve_log.rs:{n} payload omitted");
            assert!(text.lines().any(|l| l.ends_with(&want)), "{want} in\n{text}");
        }
        assert!(!text.contains(&secret), "a formatted payload reached the log:\n{text}");
        let _ = fs::remove_dir_all(&dir);
    }
}
