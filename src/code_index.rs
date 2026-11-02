//! Running the code index: finding and spawning rust-analyzer, building a
//! root's rows ([`index_root`]) and the background indexer thread
//! ([`Indexer`]) that keeps them current.
//!
//! rust-analyzer is the one program atlas starts, and only here, never on
//! the query path:
//!
//! * found as configured, else as `rust-analyzer` on `PATH`, else in a
//!   rustup toolchain's `bin` folder (default toolchain first); each
//!   candidate must answer `--version` (a rustup proxy without the
//!   component does not);
//! * run as `rust-analyzer scip ROOT --output FILE --config-path JSON`, at
//!   below-normal priority, its stderr to `<dir>/<root>.ra.log`;
//! * killed when it passes the timeout;
//! * one at a time across every atlas process on the database (a lease row
//!   in `code_lease`);
//! * debounced: a root is re-indexed once it has been quiet for the
//!   debounce time after a change to a `.rs`, `.wgsl` or Cargo manifest.
//!
//! Queries read the last good index; while a root is pending or running the
//! `symbol` answer says so (see [`Indexer::status`]).
//!
//! The rust-analyzer configuration enables every Cargo feature (so feature-gated
//! modules are indexed) and turns build scripts and proc macros off
//! by default (`cargo.buildScripts.overrideCommand` is `cargo --version`,
//! which builds nothing), so no compiler runs; `build_scripts: true` lets
//! them run into the workspace target's `rust-analyzer` folder instead of
//! the folder the project's own builds use.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::code::{self, CodeRoot, CodeSetup, Rows, RowStats, WriteStats};
use crate::error::{Error, Result};
use crate::scip;
use crate::watch::{Poker, Watcher};

/// How a rust-analyzer run ended.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    /// Wall time.
    pub seconds: f64,
    /// The exit code, if it exited.
    pub code: Option<i32>,
    /// True when it was killed at the timeout.
    pub timed_out: bool,
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn answers_version(p: &Path) -> bool {
    let mut c = Command::new(p);
    c.arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    no_window(&mut c);
    c.status().is_ok_and(|s| s.success())
}

#[cfg(windows)]
fn no_window(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    c.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_c: &mut Command) {}

#[cfg(windows)]
fn below_normal(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    c.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
}

#[cfg(not(windows))]
fn below_normal(_c: &mut Command) {}

/// Finds a working rust-analyzer.
///
/// # Arguments
///
/// * `explicit` - the configured binary, tried alone when given.
///
/// # Returns
///
/// The binary, or a note saying where it looked.
pub fn find_rust_analyzer(explicit: Option<&Path>) -> std::result::Result<PathBuf, String> {
    if let Some(p) = explicit {
        return if answers_version(p) { Ok(p.to_path_buf()) } else { Err(format!("rust-analyzer {} does not run", p.display())) };
    }
    let name = exe("rust-analyzer");
    let mut tried = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(&name);
            if p.is_file() {
                if answers_version(&p) {
                    return Ok(p);
                }
                tried.push(p);
            }
        }
    }
    let home = std::env::var_os("RUSTUP_HOME").map(PathBuf::from).or_else(|| {
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| PathBuf::from(h).join(".rustup"))
    });
    if let Some(home) = home {
        let default = fs::read_to_string(home.join("settings.toml"))
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("default_toolchain").map(|v| v.trim_start_matches([' ', '=']).trim().trim_matches('"').to_string())));
        let mut chains: Vec<PathBuf> = fs::read_dir(home.join("toolchains")).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        chains.sort();
        chains.sort_by_key(|c| default.as_deref().is_none_or(|d| !c.file_name().is_some_and(|f| f.to_string_lossy().starts_with(d))));
        for c in chains {
            let p = c.join("bin").join(&name);
            if p.is_file() && answers_version(&p) {
                return Ok(p);
            }
        }
    }
    Err(if tried.is_empty() { "rust-analyzer not found on PATH or in a rustup toolchain".into() } else { "rust-analyzer on PATH does not run (a rustup proxy without the component?) and none is in a rustup toolchain".into() })
}

/// Writes the rust-analyzer configuration file for a run and returns its
/// path.
fn ra_config(setup: &CodeSetup, root: &CodeRoot) -> Result<PathBuf> {
    fs::create_dir_all(&setup.dir)?;
    let p = setup.dir.join(format!("{}.rust-analyzer.json", root.name));
    let body = if root.build_scripts {
        r#"{"cargo": {"targetDir": true, "features": "all"}}"#
    } else {
        r#"{"cargo": {"features": "all", "buildScripts": {"overrideCommand": ["cargo", "--version"]}}, "procMacro": {"enable": false}}"#
    };
    fs::write(&p, body)?;
    Ok(p)
}

/// Runs a program to completion or until the timeout, at below-normal
/// priority, with stdout discarded and stderr to `log`.
///
/// # Arguments
///
/// * `program` - the binary.
/// * `args` - its arguments.
/// * `cwd` - its working folder.
/// * `log` - the file stderr is written to (truncated).
/// * `timeout` - when it is killed.
///
/// # Errors
///
/// [`Error::Io`] when it cannot be started.
pub fn run_bounded(program: &Path, args: &[std::ffi::OsString], cwd: &Path, log: &Path, timeout: Duration) -> Result<RunReport> {
    let t0 = Instant::now();
    let err = fs::File::create(log)?;
    let mut c = Command::new(program);
    c.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::from(err));
    below_normal(&mut c);
    let mut child = c.spawn().map_err(|e| Error::Io(format!("{}: {e}", program.display())))?;
    let pid = child.id();
    crate::platform::process::lower_priority(pid);
    // The waiter owns the child (so its pid is not reaped and reused while
    // the timer might still kill it); the timer is a channel receive with a
    // deadline, not a poll.
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let s = child.wait();
        let _ = tx.send(());
        s
    });
    let timed_out = match rx.recv_timeout(timeout) {
        Ok(()) => false,
        Err(_) => {
            crate::platform::process::terminate(pid);
            true
        }
    };
    let status = waiter.join().map_err(|_| Error::Io("waiter thread panicked".into()))??;
    Ok(RunReport { seconds: t0.elapsed().as_secs_f64(), code: status.code(), timed_out })
}

/// Where the Rust side of a root comes from.
#[derive(Debug, Clone)]
pub enum RustSource {
    /// Run this rust-analyzer.
    Analyzer(PathBuf),
    /// No rust-analyzer: definitions from the line scan only. The note says
    /// why.
    ScanOnly(String),
    /// A SCIP file already written (tests and re-decoding).
    File(PathBuf),
}

/// What indexing one root did.
#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    /// The root's name.
    pub root: String,
    /// `scip` or why the line scan stands alone.
    pub rust: String,
    /// The rust-analyzer run, if one ran.
    pub ra: Option<RunReport>,
    /// SCIP file size, bytes.
    pub scip_bytes: usize,
    /// Decode time, milliseconds.
    pub decode_ms: f64,
    /// Bytes the decoded index holds (the file plus its columns and
    /// intern table).
    pub decode_mem: usize,
    /// SCIP documents, occurrences (non-local) and symbols.
    pub scip_counts: (usize, usize, usize),
    /// Where the rows came from.
    pub rows: RowStats,
    /// What was written.
    pub write: WriteStats,
    /// Row building plus the write, milliseconds.
    pub write_ms: f64,
    /// Everything, milliseconds.
    pub total_ms: f64,
}

fn decode_mem(ix: &scip::Index<'_>, file: usize) -> usize {
    let u32s = ix.occ_doc.capacity() + ix.occ_sym.capacity() + ix.occ_roles.capacity() + ix.rel_from.capacity() + ix.rel_to.capacity();
    let i32s = ix.occ_line.capacity() + ix.occ_col.capacity() + ix.occ_end_line.capacity() + ix.occ_end_col.capacity() + ix.occ_encl_start.capacity() + ix.occ_encl_end.capacity();
    // The intern table: a (&str, u32) entry plus a control byte per slot.
    let intern = ix.symbols.capacity() * (16 + 8 + 1) * 8 / 7;
    file + 4 * (u32s + i32s) + ix.rel_flags.capacity() + ix.symbols.capacity() * 16 + ix.info.capacity() * std::mem::size_of::<scip::SymInfo<'_>>() + intern + ix.doc_path.capacity() * 32
}

/// Indexes one root: rust-analyzer (or the given SCIP file), the line
/// scan, WGSL, then one write transaction.
///
/// # Arguments
///
/// * `setup` - the code setup (for the folder, timeout and build-script
///   choice).
/// * `conn` - the connection written through.
/// * `root` - the root.
/// * `rust` - where the Rust side comes from.
///
/// # Errors
///
/// [`Error::Io`] when the root cannot be walked, [`Error::Db`] from the
/// write. A failed or timed-out rust-analyzer run is not an error: the root
/// is written from the line scan and the report says why.
pub fn index_root(setup: &CodeSetup, conn: &mut Connection, root: &CodeRoot, rust: &RustSource) -> Result<IndexReport> {
    let t0 = Instant::now();
    let walk = code::walk(&root.root)?;
    let fingerprint = walk.fingerprint();
    let mut rep = IndexReport { root: root.name.clone(), ..IndexReport::default() };
    let mut scip_bytes: Option<Vec<u8>> = None;
    let has_manifest = root.root.join("Cargo.toml").is_file();
    match rust {
        RustSource::File(p) => {
            scip_bytes = Some(fs::read(p)?);
            rep.rust = "scip".into();
        }
        RustSource::ScanOnly(why) => rep.rust = format!("scan only: {why}"),
        RustSource::Analyzer(_) if !has_manifest => rep.rust = "scan only: no Cargo.toml".into(),
        RustSource::Analyzer(_) if walk.rust.is_empty() => rep.rust = "no Rust files".into(),
        RustSource::Analyzer(ra) => {
            fs::create_dir_all(&setup.dir)?;
            let out = setup.dir.join(format!("{}.scip", root.name));
            let tmp = setup.dir.join(format!("{}.scip.part", root.name));
            let _ = fs::remove_file(&tmp);
            let cfg = ra_config(setup, root)?;
            let args: Vec<std::ffi::OsString> = vec!["scip".into(), root.root.clone().into(), "--output".into(), tmp.clone().into(), "--config-path".into(), cfg.into()];
            let r = run_bounded(ra, &args, &root.root, &setup.dir.join(format!("{}.ra.log", root.name)), setup.timeout)?;
            let ok = !r.timed_out && r.code == Some(0) && tmp.is_file();
            rep.rust = if r.timed_out {
                format!("scan only: rust-analyzer passed the {} s timeout", setup.timeout.as_secs())
            } else if ok {
                "scip".into()
            } else {
                format!("scan only: rust-analyzer exited with {:?}", r.code)
            };
            rep.ra = Some(r);
            if ok {
                fs::rename(&tmp, &out)?;
                scip_bytes = Some(fs::read(&out)?);
            }
        }
    }
    let mut rows = Rows::default();
    let mut files: HashMap<String, u32> = HashMap::new();
    if let Some(buf) = &scip_bytes {
        rep.scip_bytes = buf.len();
        let t = Instant::now();
        match scip::decode(buf, scip::Options::default()) {
            Ok(ix) => {
                rep.decode_ms = t.elapsed().as_secs_f64() * 1e3;
                rep.decode_mem = decode_mem(&ix, buf.len());
                rep.scip_counts = (ix.doc_path.len(), ix.occurrences(), ix.symbols.len());
                code::rust_rows(&ix, &mut rows, &mut files);
            }
            Err(e) => rep.rust = format!("scan only: {e}"),
        }
    }
    let t1 = Instant::now();
    let rust_texts: Vec<(String, String)> = walk.rust.iter().filter_map(|f| code::read_source(&root.root, &f.path).map(|t| (f.path.clone(), t))).collect();
    code::scan_rows(&rust_texts, &mut rows, &mut files);
    let wgsl_texts: Vec<(String, String)> = walk.wgsl.iter().filter_map(|f| code::read_source(&root.root, &f.path).map(|t| (f.path.clone(), t))).collect();
    code::wgsl_rows(&wgsl_texts, &mut rows, &mut files);
    let read = |l: &[code::WalkedFile]| -> Vec<(String, String)> { l.iter().filter_map(|f| code::read_source(&root.root, &f.path).map(|t| (f.path.clone(), t))).collect() };
    let proto_texts = read(&walk.proto);
    crate::proto::proto_rows(&proto_texts, &mut rows, &mut files);
    // rpc call sites: the names this root defines and every other root's.
    let own_rpcs: std::collections::HashSet<String> = rows.occ.iter().filter(|o| o.sect == code::SECT_DEF && o.kind.starts_with("rpc (")).map(|o| o.name.clone()).collect();
    code::ensure_schema(conn)?;
    let own_before = crate::proto::indexed_rpcs(conn, &root.name, true)?;
    let mut rpcs = crate::proto::indexed_rpcs(conn, &root.name, false)?;
    rpcs.extend(own_rpcs.iter().cloned());
    let mention_texts = if rpcs.is_empty() { Vec::new() } else { read(&walk.mention) };
    if !rpcs.is_empty() {
        crate::proto::mention_rows(&rust_texts, &rpcs, &mut rows, &mut files);
        crate::proto::mention_rows(&mention_texts, &rpcs, &mut rows, &mut files);
    }
    let by_path: HashMap<&str, &str> = rust_texts.iter().chain(&wgsl_texts).chain(&proto_texts).chain(&mention_texts).map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let text_of = |i: usize| -> Option<String> {
        let p = &rows.files[i].0;
        by_path.get(p.as_str()).map(|t| t.to_string()).or_else(|| code::read_source(&root.root, p))
    };
    rep.rows = rows.stats;
    let elapsed = t0.elapsed().as_millis() as u64;
    rep.write = code::write_root(conn, &root.name, &root.root, &rows, &text_of, &fingerprint, &rep.rust, elapsed)?;
    if own_rpcs != own_before {
        // The other roots' call sites of this root's rpcs are stale: their
        // fingerprints are cleared, so the next walk re-indexes them.
        conn.execute("UPDATE code_roots SET fingerprint = '' WHERE root <> ?1", [&root.name])?;
    }
    rep.write_ms = t1.elapsed().as_secs_f64() * 1e3;
    rep.total_ms = t0.elapsed().as_secs_f64() * 1e3;
    Ok(rep)
}

fn now_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Takes the indexing lease (one rust-analyzer at a time across every
/// process on the database).
///
/// # Arguments
///
/// * `conn` - a connection to the database.
/// * `holder` - this process's name for itself.
/// * `ttl` - how long the lease lasts unless released (longer than the
///   run's timeout, so a crashed holder frees it).
///
/// # Returns
///
/// True when this holder has the lease.
///
/// # Errors
///
/// [`Error::Db`] from SQLite.
pub fn take_lease(conn: &mut Connection, holder: &str, ttl: Duration) -> Result<bool> {
    code::ensure_schema(conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let cur: Option<(String, i64)> = tx.query_row("SELECT holder, until FROM code_lease WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let now = now_s();
    let free = match &cur {
        None => true,
        Some((h, until)) => h == holder || *until < now,
    };
    if free {
        tx.execute("INSERT OR REPLACE INTO code_lease(id, holder, until) VALUES(1, ?1, ?2)", params![holder, now + ttl.as_secs() as i64])?;
    }
    tx.commit()?;
    Ok(free)
}

/// Gives the lease back.
///
/// # Errors
///
/// [`Error::Db`] from SQLite.
pub fn release_lease(conn: &Connection, holder: &str) -> Result<()> {
    conn.execute("DELETE FROM code_lease WHERE id = 1 AND holder = ?1", [holder])?;
    Ok(())
}

/// What the indexer is doing with a root.
#[derive(Debug, Clone, PartialEq)]
pub enum RootState {
    /// Up to date as far as the indexer knows.
    Idle,
    /// Changed; re-indexing once quiet.
    Pending,
    /// Re-indexing now.
    Running,
    /// The last attempt failed (the root keeps its previous rows).
    Failed(String),
    /// Changed and quiet, but waiting for the reason given (the defer lock
    /// file exists, or a cargo process runs).
    Deferred(String),
}

/// Why background re-indexing should wait now, if it should: the
/// configured lock file exists, or a `cargo` process that is not an
/// ancestor of this one runs (a build would compete with rust-analyzer for
/// memory and cores).
pub fn defer_reason(setup: &CodeSetup) -> Option<String> {
    if let Some(lock) = &setup.defer_lock {
        if lock.exists() {
            return Some(format!("lock file {} exists", lock.display()));
        }
    }
    if setup.defer_on_cargo && crate::platform::process::foreign_cargo_running() {
        return Some("a cargo process is running".into());
    }
    None
}

/// The indexer's view of every root, by name.
pub type Status = HashMap<String, RootState>;

struct Shared {
    status: Mutex<Status>,
    /// Signalled whenever a root's state changes.
    moved: Condvar,
    stop: AtomicBool,
    reports: Mutex<Vec<IndexReport>>,
}

/// The background indexer: one thread that watches the code roots and
/// re-indexes a root once it has been quiet for the debounce time.
pub struct Indexer {
    shared: Arc<Shared>,
    poker: Poker,
}

impl std::fmt::Debug for Indexer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Indexer").field("status", &self.status()).finish()
    }
}

impl Indexer {
    /// Starts the thread. It opens its own connection, compares each root
    /// with its stored fingerprint and indexes what is stale, then waits
    /// for changes.
    ///
    /// # Arguments
    ///
    /// * `setup` - the code setup.
    /// * `db` - the database file.
    /// * `rust` - how to find rust-analyzer: `None` searches (see
    ///   [`find_rust_analyzer`]); `Some` fixes the source (tests).
    pub fn start(setup: CodeSetup, db: PathBuf, rust: Option<RustSource>) -> Indexer {
        let roots: Vec<PathBuf> = setup.roots.iter().map(|r| r.root.clone()).collect();
        // The watcher lives on the indexer thread; this handle wakes it.
        let watcher = if setup.watch { Watcher::start(&roots) } else { Watcher::disabled(&roots) };
        let poker = watcher.poker();
        let status: Status = setup.roots.iter().map(|r| (r.name.clone(), RootState::Pending)).collect();
        let shared = Arc::new(Shared { status: Mutex::new(status), moved: Condvar::new(), stop: AtomicBool::new(false), reports: Mutex::new(Vec::new()) });
        let s2 = shared.clone();
        std::thread::Builder::new()
            .name("atlas-code-index".into())
            .spawn(move || {
                if let Err(e) = run(&setup, &db, rust, &s2, &watcher) {
                    let mut st = s2.status.lock().unwrap_or_else(|p| p.into_inner());
                    for v in st.values_mut() {
                        *v = RootState::Failed(e.to_string());
                    }
                    s2.moved.notify_all();
                }
            })
            .ok();
        Indexer { shared, poker }
    }

    /// Every root's state.
    pub fn status(&self) -> Status {
        self.shared.status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Asks the thread to compare every root with its fingerprint now.
    pub fn check(&self) {
        for i in 0..self.poker.roots() {
            self.poker.poke(i);
        }
    }

    /// The reports of every indexing this indexer ran, oldest first.
    pub fn reports(&self) -> Vec<IndexReport> {
        self.shared.reports.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Blocks until at least `n` indexings have finished, or the timeout
    /// passes (a condition-variable wait).
    ///
    /// # Returns
    ///
    /// True when `n` were reached.
    pub fn wait_reports(&self, n: usize, timeout: Duration) -> bool {
        let st = self.shared.status.lock().unwrap_or_else(|p| p.into_inner());
        let reports = &self.shared.reports;
        let count = || reports.lock().map(|r| r.len()).unwrap_or(0);
        let (_st, _) = self.shared.moved.wait_timeout_while(st, timeout, |_| count() < n).unwrap_or_else(|p| p.into_inner());
        count() >= n
    }

    /// Blocks until no root is pending or running, or the timeout passes
    /// (tests and the command line): a condition-variable wait on the state.
    ///
    /// # Returns
    ///
    /// True when every root settled.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let st = self.shared.status.lock().unwrap_or_else(|p| p.into_inner());
        let busy = |s: &mut Status| s.values().any(|x| matches!(x, RootState::Pending | RootState::Running));
        let (st, _) = self.shared.moved.wait_timeout_while(st, timeout, busy).unwrap_or_else(|p| p.into_inner());
        !st.values().any(|x| matches!(x, RootState::Pending | RootState::Running))
    }
}

impl Drop for Indexer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the thread so it sees the flag; a run in progress finishes
        // (or times out) on its own.
        self.poker.poke(0);
    }
}

/// How long a deferred root waits before the reason is looked at again.
const DEFER_RECHECK: Duration = Duration::from_secs(15);

fn log_line(dir: &Path, line: &str) {
    if fs::create_dir_all(dir).is_ok() {
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(dir.join("index.log")) {
            let _ = writeln!(f, "{} {line}", crate::store::utc(SystemTime::now()));
        }
    }
}

fn set(shared: &Shared, name: &str, s: RootState) {
    let mut st = shared.status.lock().unwrap_or_else(|p| p.into_inner());
    if st.get(name) != Some(&s) {
        st.insert(name.to_string(), s);
        shared.moved.notify_all();
    }
}

fn run(setup: &CodeSetup, db: &Path, rust: Option<RustSource>, shared: &Shared, watcher: &Watcher) -> Result<()> {
    let mut conn = Connection::open(db)?;
    conn.busy_timeout(Duration::from_secs(30))?;
    code::ensure_schema(&conn)?;
    let rust = rust.unwrap_or_else(|| match find_rust_analyzer(setup.rust_analyzer.as_deref()) {
        Ok(p) => RustSource::Analyzer(p),
        Err(why) => RustSource::ScanOnly(why),
    });
    let holder = format!("pid {}", std::process::id());
    let n = setup.roots.len();
    // Per root: when it last changed, and the fingerprint last walked.
    let mut changed: Vec<Option<Instant>> = vec![None; n];
    let mut seen_fp: Vec<String> = vec![String::new(); n];
    // The fingerprint and error of a failed attempt: not retried until the
    // root changes again.
    let mut failed: Vec<Option<(String, String)>> = vec![None; n];
    // Why a due root is waiting (see [`defer_reason`]).
    let mut deferred: Vec<Option<String>> = vec![None; n];
    let mut walk_all = true;
    let mut last_walk = Instant::now();
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let stored = code::root_records(&conn)?;
        for (i, r) in setup.roots.iter().enumerate() {
            let snap = watcher.snapshot(i);
            let must_walk = walk_all || !snap.healthy || snap.full;
            if must_walk {
                let fp = code::walk(&r.root).map(|w| w.fingerprint()).unwrap_or_default();
                let indexed = stored.get(&r.name).map(|x| x.fingerprint.as_str());
                if indexed == Some(fp.as_str()) || failed[i].as_ref().is_some_and(|f| f.0 == fp) {
                    changed[i] = None;
                } else if fp != seen_fp[i] {
                    // Still changing (or first sight): restart the quiet time,
                    // except at startup, where a stale root goes at once.
                    changed[i] = Some(if walk_all && seen_fp[i].is_empty() { Instant::now() - setup.debounce } else { Instant::now() });
                }
                seen_fp[i] = fp;
            } else if snap.paths.iter().any(|p| code::relevant(p)) {
                changed[i] = Some(Instant::now());
            }
            watcher.clear(i, snap.seq);
            let state = match (&changed[i], &failed[i]) {
                (Some(_), _) => deferred[i].clone().map_or(RootState::Pending, RootState::Deferred),
                (None, Some((_, e))) => RootState::Failed(e.clone()),
                (None, None) => RootState::Idle,
            };
            set(shared, &r.name, state);
        }
        if walk_all {
            last_walk = Instant::now();
        }
        walk_all = false;
        // Index every root that has been quiet long enough.
        for (i, r) in setup.roots.iter().enumerate() {
            let Some(at) = changed[i] else { continue };
            if at.elapsed() < setup.debounce || shared.stop.load(Ordering::SeqCst) {
                continue;
            }
            if let Some(why) = defer_reason(setup) {
                // Look again after the quiet time (at least the floor, so a
                // zero debounce does not spin).
                changed[i] = Some(Instant::now() + DEFER_RECHECK.saturating_sub(setup.debounce));
                set(shared, &r.name, RootState::Deferred(why.clone()));
                deferred[i] = Some(why);
                continue;
            }
            deferred[i] = None;
            if !take_lease(&mut conn, &holder, setup.timeout + Duration::from_secs(120))? {
                // Another process is indexing: try again after the quiet time.
                changed[i] = Some(Instant::now());
                continue;
            }
            set(shared, &r.name, RootState::Running);
            log_line(&setup.dir, &format!("{} indexing", r.name));
            let res = index_root(setup, &mut conn, r, &rust);
            let _ = release_lease(&conn, &holder);
            changed[i] = None;
            match res {
                Ok(rep) => {
                    log_line(&setup.dir, &format!("{} done: {} in {:.0} ms ({} defs, {} refs, {} impls)", r.name, rep.rust, rep.total_ms, rep.write.defs, rep.write.refs, rep.write.impls));
                    failed[i] = None;
                    seen_fp[i] = code::walk(&r.root).map(|w| w.fingerprint()).unwrap_or_default();
                    shared.reports.lock().unwrap_or_else(|p| p.into_inner()).push(rep);
                    set(shared, &r.name, RootState::Idle);
                }
                Err(e) => {
                    log_line(&setup.dir, &format!("{} failed: {e}", r.name));
                    failed[i] = Some((seen_fp[i].clone(), e.to_string()));
                    set(shared, &r.name, RootState::Failed(e.to_string()));
                }
            }
        }
        // Sleep until the next root is due, a change arrives, or the safety
        // net walk is due.
        let next_due = changed.iter().flatten().map(|at| (*at + setup.debounce).saturating_duration_since(Instant::now())).min();
        let net = setup.rescan_interval.saturating_sub(last_walk.elapsed());
        let wait = next_due.map_or(net, |d| d.min(net)).max(Duration::from_millis(10));
        let seen = watcher.events();
        if !watcher.wait_past(seen, wait) && last_walk.elapsed() >= setup.rescan_interval {
            walk_all = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a test: the child the timeout test starts and kills.
    #[test]
    #[ignore]
    fn sleeper_child() {
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn a_run_past_its_timeout_is_killed() {
        let exe = std::env::current_exe().unwrap();
        let dir = std::env::temp_dir().join(format!("atlas-run-bounded-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let args: Vec<std::ffi::OsString> = ["--ignored", "--exact", "code_index::tests::sleeper_child", "--nocapture"].iter().map(Into::into).collect();
        let r = run_bounded(&exe, &args, &dir, &dir.join("log.txt"), Duration::from_millis(1500)).unwrap();
        assert!(r.timed_out, "{r:?}");
        assert!(r.seconds < 30.0, "{r:?}");
        let ok: Vec<std::ffi::OsString> = ["--list"].iter().map(Into::into).collect();
        let r = run_bounded(&exe, &ok, &dir, &dir.join("log.txt"), Duration::from_secs(60)).unwrap();
        assert_eq!((r.timed_out, r.code), (false, Some(0)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_binary_is_reported() {
        assert!(find_rust_analyzer(Some(Path::new("no-such-dir/rust-analyzer-missing"))).is_err());
    }
}
