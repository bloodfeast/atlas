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

use std::collections::{HashMap, HashSet};
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
    answers(&crate::langs::Launch::program(p.to_path_buf()), "--version")
}

/// True when a program runs and exits with success given one argument
/// (how an indexer is told from a stale file or a proxy without the tool).
pub fn answers(launch: &crate::langs::Launch, arg: &str) -> bool {
    let mut c = Command::new(&launch.program);
    c.args(&launch.pre).arg(arg).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    no_window(&mut c);
    c.status().is_ok_and(|s| s.success())
}

/// npm's global package folder (`npm root -g`), asked once per process.
pub(crate) fn npm_root() -> Option<PathBuf> {
    static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let npm = crate::langs::on_path("npm")?;
        let mut c = std::process::Command::new(npm);
        c.args(["root", "-g"]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        no_window(&mut c);
        let out = c.output().ok().filter(|o| o.status.success())?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then(|| PathBuf::from(s))
    })
    .clone()
}

#[cfg(windows)]
pub(crate) fn no_window(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    c.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn no_window(_c: &mut Command) {}

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
    run_bounded_env(program, args, &[], cwd, log, timeout)
}

/// [`run_bounded`] with environment variables set for the program.
///
/// # Errors
///
/// [`Error::Io`] when it cannot be started.
pub fn run_bounded_env(
    program: &Path,
    args: &[std::ffi::OsString],
    env: &[(&str, &str)],
    cwd: &Path,
    log: &Path,
    timeout: Duration,
) -> Result<RunReport> {
    let t0 = Instant::now();
    let err = fs::File::create(log)?;
    let mut c = Command::new(program);
    c.args(args).envs(env.iter().copied()).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::from(err));
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
    /// A SCIP file already written, by rust-analyzer or by any other SCIP
    /// indexer (`--code-index --scip FILE`); its paths are taken relative
    /// to the root.
    File(PathBuf),
}

/// What indexing one root did.
#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    /// The root's name.
    pub root: String,
    /// The root's note: `scip` or why the line scan stands alone for a
    /// Rust root (see [`code::units_note`]).
    pub rust: String,
    /// The root's units as written to `code_units`.
    pub units: Vec<code::Unit>,
    /// The rust-analyzer run, if one ran.
    pub ra: Option<RunReport>,
    /// The other indexers' runs: `lang dir` (the unit) and how each ended.
    pub runs: Vec<(String, RunReport)>,
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

/// Indexes one root: rust-analyzer (or the given SCIP file, from any
/// indexer), the line scan, WGSL, then one write transaction.
///
/// A SCIP file from rust-analyzer goes through [`code::rust_rows`]; one
/// from any other indexer through [`code::scip_rows`], as a unit of each
/// language its documents hold (`prebuilt TOOL VERSION`), and the root's
/// Rust files then have the line scan only.
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
    let fingerprint = walk.fingerprint_with(&root.scip.configured());
    let mut rep = IndexReport { root: root.name.clone(), ..IndexReport::default() };
    let mut scip_bytes: Option<Vec<u8>> = None;
    let has_manifest = root.root.join("Cargo.toml").is_file();
    // A root of another language's code alone (a Go service) has no Rust
    // side at all.
    let other_only = !has_manifest && walk.rust.is_empty() && !walk.source.is_empty();
    match rust {
        RustSource::File(p) => {
            scip_bytes = Some(fs::read(p)?);
            rep.rust = "scip".into();
        }
        _ if other_only => rep.rust = "no Rust files".into(),
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
    // References into a package another root defines are stored: that
    // root's recorded packages, read here (its own run is not needed).
    code::ensure_schema(conn)?;
    rows.linked = code::defined_packages(conn, &root.name, false)?;
    let packages_before = code::defined_packages(conn, &root.name, true)?;
    let mut files: HashMap<String, u32> = HashMap::new();
    let mut other_units: Vec<code::Unit> = Vec::new();
    // Files a SCIP index has a document for: the scanner leaves them alone.
    let mut covered: HashSet<String> = HashSet::new();
    if let Some(buf) = &scip_bytes {
        rep.scip_bytes = buf.len();
        let t = Instant::now();
        match scip::decode(buf, scip::Options::default()) {
            Ok(ix) => {
                rep.decode_ms = t.elapsed().as_secs_f64() * 1e3;
                rep.decode_mem = decode_mem(&ix, buf.len());
                rep.scip_counts = (ix.doc_path.len(), ix.occurrences(), ix.symbols.len());
                if code::is_rust_index(&ix) {
                    code::rust_rows(&ix, &mut rows, &mut files);
                } else {
                    let read = |p: &str| code::read_source(&root.root, p);
                    code::scip_rows(&ix, &mut rows, &mut files, &read);
                    covered.extend(ix.doc_path.iter().map(|p| code::rebase("", p)));
                    let source = format!("prebuilt {} {}", ix.tool.0, ix.tool.1).trim_end().to_string();
                    let mut langs: Vec<&str> = ix.doc_path.iter().zip(&ix.doc_lang).map(|(p, l)| code::lang_of(l, p)).collect();
                    langs.sort_unstable();
                    langs.dedup();
                    other_units = langs.into_iter().map(|l| code::Unit { dir: String::new(), lang: l.into(), source: source.clone() }).collect();
                    rep.rust = if has_manifest || !walk.rust.is_empty() { format!("scan only: the SCIP file is from {}", ix.tool.0) } else { "no Rust files".into() };
                }
            }
            Err(e) => rep.rust = format!("scan only: {e}"),
        }
    }
    // The languages of the language table: each unit through its indexer
    // when one runs (not for a language the given SCIP file already holds),
    // then the scanner for every file no index covers.
    let prebuilt: Vec<String> = other_units.iter().map(|u| u.lang.clone()).collect();
    let lang_units = index_units(setup, root, &walk, &prebuilt, &mut rows, &mut files, &mut covered, &mut rep.runs);
    let t1 = Instant::now();
    let lang_texts: Vec<(String, String)> = walk.source.iter().filter_map(|f| code::read_source(&root.root, &f.path).map(|t| (f.path.clone(), t))).collect();
    let scanned: Vec<(String, String)> = lang_texts.iter().filter(|(p, _)| !covered.contains(p)).cloned().collect();
    code::lang_rows(&scanned, &mut rows, &mut files);
    code::fill_rows(&lang_texts, &covered, &mut rows, &mut files);
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
        // Generated stubs, the calls made through them and the server
        // types implementing them, linked to their rpcs.
        let mut services: Vec<(String, String)> = rows
            .occ
            .iter()
            .filter(|o| o.sect == code::SECT_DEF && o.kind.starts_with("rpc ("))
            .map(|o| (o.name.clone(), crate::proto::service_of(&o.container)))
            .collect();
        services.extend(crate::proto::indexed_rpc_services(conn, &root.name)?);
        let texts: HashMap<&str, &str> = lang_texts.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
        // Stubs another root generated, when this root refers into another
        // root's packages at all.
        let foreign = if rows.stats.xroot_refs > 0 { crate::proto::foreign_stubs(conn, &root.name, &services)? } else { Vec::new() };
        crate::proto::link_rows(&mut rows, &services, &texts, &foreign);
        // A definition's name is no mention, nor, in a file a SCIP index
        // covers, a word the index has a reference at: the index says what
        // it is.
        let in_index: Vec<bool> = rows.files.iter().map(|(p, _)| covered.contains(p)).collect();
        let taken: HashSet<(u32, u32, u32)> = rows
            .occ
            .iter()
            .filter(|o| o.sect == code::SECT_DEF || (o.sect == code::SECT_REF && in_index[o.file as usize]))
            .map(|o| (o.file, o.line, o.col))
            .collect();
        crate::proto::mention_rows(&rust_texts, &rpcs, &taken, &mut rows, &mut files);
        crate::proto::mention_rows(&mention_texts, &rpcs, &taken, &mut rows, &mut files);
        crate::proto::mention_rows(&lang_texts, &rpcs, &taken, &mut rows, &mut files);
    }
    let by_path: HashMap<&str, &str> =
        rust_texts.iter().chain(&wgsl_texts).chain(&proto_texts).chain(&mention_texts).chain(&lang_texts).map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let text_of = |i: usize| -> Option<String> {
        let p = &rows.files[i].0;
        by_path.get(p.as_str()).map(|t| t.to_string()).or_else(|| code::read_source(&root.root, p))
    };
    rep.rows = rows.stats;
    // The Rust unit carries the note the root always had; a root without
    // Rust has none.
    rep.units = other_units;
    rep.units.extend(lang_units);
    if rep.rust != "no Rust files" {
        rep.units.insert(0, code::Unit { dir: String::new(), lang: "rust".into(), source: rep.rust.clone() });
    }
    rep.rust = code::units_note(&rep.units);
    let elapsed = t0.elapsed().as_millis() as u64;
    rep.write = code::write_root(conn, &root.name, &root.root, &rows, &text_of, &fingerprint, &rep.units, elapsed)?;
    if own_rpcs != own_before {
        // The other roots' call sites of this root's rpcs are stale: their
        // fingerprints are cleared, so the next walk re-indexes them.
        conn.execute("UPDATE code_roots SET fingerprint = '' WHERE root <> ?1", [&root.name])?;
    }
    // The roots referring to a package this root started or stopped
    // defining: their stored cross-root references are stale.
    code::mark_package_users(conn, &root.name, &packages_before, &rows.pkg_defs)?;
    rep.write_ms = t1.elapsed().as_secs_f64() * 1e3;
    rep.total_ms = t0.elapsed().as_secs_f64() * 1e3;
    Ok(rep)
}

/// Indexes the units of the language table's languages ([`code::units`]):
/// each through its driver when one is found (or as `setup.indexers`
/// says), its rows added and its documents marked covered; a unit whose
/// indexer is missing or fails, or whose prerequisite is missing where a
/// configuration needs it (a TypeScript package without `node_modules`
/// whose `tsconfig.json` extends a package), is left to the scanner, and
/// its note says why. A TypeScript unit holding no file of its languages is not
/// run.
/// Files of a language outside every unit get one `scan only` unit.
///
/// # Arguments
///
/// * `prebuilt` - languages the given SCIP file already holds: not run.
/// * `covered` - paths with a SCIP document, extended here.
/// * `runs` - each indexer run, appended.
#[allow(clippy::too_many_arguments)]
fn index_units(
    setup: &CodeSetup,
    root: &CodeRoot,
    walk: &code::Walk,
    prebuilt: &[String],
    rows: &mut Rows,
    files: &mut HashMap<String, u32>,
    covered: &mut HashSet<String>,
    runs: &mut Vec<(String, RunReport)>,
) -> Vec<code::Unit> {
    let units = code::units(walk);
    // Each source file's language row and unit.
    let placed: Vec<(&'static crate::langs::Lang, Option<&code::LangUnit>)> =
        walk.source.iter().filter_map(|f| crate::langs::of_path(&f.path).map(|l| (l, code::unit_of(&units, l.name, &f.path)))).collect();
    let mut out = Vec::new();
    for lang in crate::langs::LANGS {
        // A language whose files are another row's units' runs only adds
        // the note for its files outside them.
        let owner = lang.units_of.unwrap_or(lang.name);
        // A SCIP file given for any language of the family stands for its run.
        let family = |n: &str| crate::langs::by_name(n).is_some_and(|l| l.units_of.unwrap_or(l.name) == owner);
        if prebuilt.iter().any(|p| family(p)) {
            continue;
        }
        let loose = placed.iter().any(|(l, u)| l.name == lang.name && u.is_none());
        let mine: Vec<&code::LangUnit> = if lang.units_of.is_some() {
            Vec::new()
        } else {
            // A unit that claims its subtree (a `package.json` beside docs or
            // tooling) runs only when it holds a source file; every Go module
            // runs, as before.
            units
                .iter()
                .filter(|u| u.lang.name == lang.name && (lang.nested_units || placed.iter().any(|(_, p)| p.is_some_and(|p| p == *u))))
                .collect()
        };
        if mine.is_empty() && !loose {
            continue;
        }
        let driver: std::result::Result<(&crate::langs::Driver, crate::langs::Launch), String> = match (&setup.indexers, &lang.driver) {
            _ if mine.is_empty() => Err(String::new()),
            (_, None) => Err(format!("no SCIP indexer for {}", lang.name)),
            (code::Indexers::Off(why), _) => Err(why.clone()),
            (code::Indexers::Search, Some(d)) => d.find().map(|p| (d, p)),
            (code::Indexers::Fixed(list), Some(d)) => list
                .iter()
                .find(|(t, _)| t == d.tool)
                .map(|(_, p)| (d, crate::langs::Launch::configured(p)))
                .ok_or_else(|| format!("{} is not configured", d.tool)),
            (code::Indexers::Chosen(list), Some(d)) => match list.iter().find(|(t, _)| t == d.tool) {
                Some((_, Some(p))) => Ok((d, crate::langs::Launch::configured(p))),
                Some((_, None)) => Err(format!("{} is off (code.indexers)", d.tool)),
                None => d.find().map(|p| (d, p)),
            },
        };
        for (k, u) in mine.iter().enumerate() {
            // A unit is named after the languages it holds: a package of
            // JavaScript alone is a `javascript` unit.
            let own = placed.iter().any(|(l, p)| l.name == lang.name && p.is_some_and(|p| p == *u));
            let word =
                if own { lang.name } else { placed.iter().find(|(_, p)| p.is_some_and(|p| p == *u)).map_or(lang.name, |(l, _)| l.name) };
            let pre = root.scip.for_dir(&root.root, walk, &u.dir);
            // The unit's source files: (path, modification time).
            let unit_files: Vec<(&str, i64)> = walk
                .source
                .iter()
                .filter(|f| crate::langs::of_path(&f.path).and_then(|l| code::unit_of(&units, l.name, &f.path)).is_some_and(|p| p == *u))
                .map(|f| (f.path.as_str(), f.mtime))
                .collect();
            // A unit holding only files its indexer cannot read (Kotlin
            // alone, for scip-java) is not run.
            let unread = {
                let mut kinds = placed.iter().filter(|(_, p)| p.is_some_and(|p| p == *u)).map(|(l, _)| l.unindexed);
                match kinds.next() {
                    Some(Some(why)) if kinds.all(|w| w.is_some()) => Some(why),
                    _ => None,
                }
            };
            let unit_driver = match unread {
                Some(why) => Err(why.to_string()),
                None => driver.clone(),
            };
            let source = match (&unit_driver, pre) {
                (Err(why), None) => format!("scan only: {why}"),
                (Err(_), Some(p)) => read_prebuilt(root, walk, owner, &u.dir, &p, rows, files, covered),
                (Ok((d, _)), Some(p)) if prebuilt_wins(&p, &setup.dir.join(format!("{}.scip", run_stem(root, d, k))), &unit_files) => {
                    read_prebuilt(root, walk, owner, &u.dir, &p, rows, files, covered)
                }
                (Ok((d, launch)), pre) => {
                    let ran = run_driver(setup, root, walk, u, k, d, launch, rows, files, covered, runs);
                    match pre {
                        // A failed run falls back on the prebuilt file.
                        Some(p) if ran.starts_with("scan only: ") => {
                            let why = ran.trim_start_matches("scan only: ");
                            let read = read_prebuilt(root, walk, owner, &u.dir, &p, rows, files, covered);
                            if read.starts_with("scan only: ") {
                                ran
                            } else {
                                format!("{read} ({why})")
                            }
                        }
                        _ => ran,
                    }
                }
            };
            out.push(code::Unit { dir: u.dir.clone(), lang: word.into(), source });
        }
        if loose && !mine.iter().any(|u| u.dir.is_empty()) {
            // Files outside every unit: a prebuilt file in the root's folder
            // covers them, else the scanner.
            let source = match root.scip.for_dir(&root.root, walk, "") {
                Some(p) => read_prebuilt(root, walk, owner, "", &p, rows, files, covered),
                None => {
                    let markers = crate::langs::by_name(owner).map_or(lang.markers, |o| o.markers);
                    format!("scan only: no {}", markers.join(" or "))
                }
            };
            out.push(code::Unit { dir: String::new(), lang: lang.name.into(), source });
        }
    }
    out
}

/// Runs one unit's indexer and adds its rows.
///
/// # Returns
///
/// The unit's note: `TOOL VERSION` from the index, or `scan only: why`
/// when its prerequisite is missing and a configuration needs it, or the
/// run fails, times out or writes no readable index (a failed run of a unit
/// missing its prerequisite names the fix).
#[allow(clippy::too_many_arguments)]
fn run_driver(
    setup: &CodeSetup,
    root: &CodeRoot,
    walk: &code::Walk,
    u: &code::LangUnit,
    k: usize,
    d: &crate::langs::Driver,
    launch: &crate::langs::Launch,
    rows: &mut Rows,
    files: &mut HashMap<String, u32>,
    covered: &mut HashSet<String>,
    runs: &mut Vec<(String, RunReport)>,
) -> String {
    let cwd = if u.dir.is_empty() { root.root.clone() } else { root.root.join(&u.dir) };
    // A missing prerequisite stops the run only when a configuration file
    // needs it; otherwise the tool runs, and the note names the fix if the
    // run fails.
    let unmet = d.prereq.as_ref().and_then(|p| p.unmet(&cwd).map(|why| (p, why)));
    if let Some((p, why)) = &unmet {
        let configs: Vec<PathBuf> =
            p.required_by.map_or_else(Vec::new, |(file, _)| unit_files(walk, &u.dir, file).map(|f| root.root.join(f)).collect());
        if p.blocks(&configs) {
            return format!("scan only: {why}");
        }
    }
    // What a failed run's note adds: the missing prerequisite's fix, and
    // the driver's hint (the SDK a `global.json` asks for).
    let mut fix = unmet.map(|(_, why)| format!("; {why}")).unwrap_or_default();
    if let Some(h) = d.fail_hint.as_ref().and_then(|h| h.for_unit(&root.root, &cwd)) {
        fix.push_str(&format!("; {h}"));
    }
    if let Err(e) = fs::create_dir_all(&setup.dir) {
        return format!("scan only: {e}");
    }
    let stem = run_stem(root, d, k);
    let out = setup.dir.join(format!("{stem}.scip"));
    let tmp = setup.dir.join(format!("{stem}.scip.part"));
    let _ = fs::remove_file(&tmp);
    // The unit's marker file: a solution before a project, then by name.
    let marker = walk
        .markers
        .iter()
        .filter_map(|f| {
            let (dir, name) = f.path.rsplit_once('/').unwrap_or(("", f.path.as_str()));
            (dir == u.dir).then_some(name).and_then(|n| u.lang.marker_rank(n).map(|r| (r, n)))
        })
        .min()
        .map(|(_, n)| n);
    let name = cwd.file_name().map_or_else(|| root.name.clone(), |n| n.to_string_lossy().into_owned());
    let mut args = d.args_for(&tmp, marker, &name);
    if let Some(p) = &d.projects {
        match unit_projects(setup, walk, u, &cwd, &stem, p) {
            Ok(list) => args.extend(list),
            Err(e) => return format!("scan only: {e}"),
        }
    }
    let r = match run_bounded_env(&launch.program, &launch.args(args), d.env, &cwd, &setup.dir.join(format!("{stem}.log")), setup.timeout) {
        Ok(r) => r,
        Err(e) => return format!("scan only: {e}"),
    };
    let ok = !r.timed_out && r.code == Some(0) && tmp.is_file();
    let label = if u.dir.is_empty() { u.lang.name.to_string() } else { format!("{} {}", u.lang.name, u.dir) };
    let note = if r.timed_out {
        Some(format!("scan only: {} passed the {} s timeout{fix}", d.tool, setup.timeout.as_secs()))
    } else if !ok {
        Some(format!("scan only: {} exited with {:?}{fix}", d.tool, r.code))
    } else {
        None
    };
    runs.push((label, r));
    if let Some(n) = note {
        return n;
    }
    let buf = match fs::rename(&tmp, &out).and_then(|_| fs::read(&out)) {
        Ok(b) => b,
        Err(e) => return format!("scan only: {e}"),
    };
    match scip::decode(&buf, scip::Options::default()) {
        Ok(ix) => {
            let read = |p: &str| code::read_source(&root.root, p);
            code::scip_rows_in(&ix, &u.dir, covered, rows, files, &read);
            covered.extend(ix.doc_path.iter().map(|p| code::rebase(&u.dir, p)));
            format!("{} {}", ix.tool.0, ix.tool.1).trim_end().to_string()
        }
        Err(e) => format!("scan only: {e}"),
    }
}

/// The name of a unit's indexer run in the code folder (`.scip`, `.log`):
/// the root's, the tool's, and the unit's number when it is not the first.
fn run_stem(root: &CodeRoot, d: &crate::langs::Driver, k: usize) -> String {
    if k == 0 {
        format!("{}.{}", root.name, d.tool)
    } else {
        format!("{}.{}.{k}", root.name, d.tool)
    }
}

/// A file's modification time, nanoseconds since the epoch (0 when
/// unknown).
fn mtime_ns(p: &Path) -> Option<i64> {
    let t = fs::metadata(p).ok()?.modified().ok()?;
    Some(t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64))
}

/// Whether a unit with both an indexer and a prebuilt file reads the
/// prebuilt file instead of running the indexer: when the file is newer
/// than the indexer's last output (`last`, absent when it never ran here)
/// and no source file of the unit (`files`, path and time) is newer than
/// it. A stale prebuilt file leaves the unit to its indexer.
pub fn prebuilt_wins(prebuilt: &Path, last: &Path, files: &[(&str, i64)]) -> bool {
    let Some(at) = mtime_ns(prebuilt) else { return false };
    mtime_ns(last).is_none_or(|run| at > run) && files.iter().all(|(_, t)| *t <= at)
}

/// Reads a prebuilt SCIP file as the index of the unit in folder `dir`
/// (its document paths are relative to that folder): rows for its
/// documents of the unit's languages (`owner`, a row's name, and the rows
/// whose units are its), except those another index already gave
/// (`covered`) and those whose source file is newer than the prebuilt file,
/// which are left to the scanner. A file shared by the units of several
/// languages in one folder serves each its own documents.
///
/// # Returns
///
/// The unit's note: `prebuilt TOOL VERSION`, with how many files were newer
/// (`(2 files newer, scanned)`), or `scan only: why` when the file cannot be
/// read or decoded, or is rust-analyzer's.
#[allow(clippy::too_many_arguments)]
fn read_prebuilt(
    root: &CodeRoot,
    walk: &code::Walk,
    owner: &str,
    dir: &str,
    file: &Path,
    rows: &mut Rows,
    files: &mut HashMap<String, u32>,
    covered: &mut HashSet<String>,
) -> String {
    let name = file.file_name().map_or_else(|| code::PREBUILT_FILE.to_string(), |n| n.to_string_lossy().into_owned());
    let Some(at) = mtime_ns(file) else { return format!("scan only: {name} is unreadable") };
    let buf = match fs::read(file) {
        Ok(b) => b,
        Err(e) => return format!("scan only: {name}: {e}"),
    };
    let ix = match scip::decode(&buf, scip::Options::default()) {
        Ok(ix) => ix,
        Err(e) => return format!("scan only: {name}: {e}"),
    };
    if code::is_rust_index(&ix) {
        return format!("scan only: {name} is rust-analyzer's");
    }
    let family = |doc: usize| {
        let lang = code::lang_of(ix.doc_lang[doc], ix.doc_path[doc]);
        crate::langs::by_name(lang).is_some_and(|l| l.units_of.unwrap_or(l.name) == owner)
    };
    let times: HashMap<&str, i64> = walk.source.iter().map(|f| (f.path.as_str(), f.mtime)).collect();
    let paths: Vec<String> = ix.doc_path.iter().map(|p| code::rebase(dir, p)).collect();
    let (docs, others): (Vec<(usize, String)>, Vec<(usize, String)>) = paths.into_iter().enumerate().partition(|(i, _)| family(*i));
    let docs: Vec<String> = docs.into_iter().map(|(_, p)| p).collect();
    if docs.is_empty() {
        return format!("scan only: {name} holds no {owner} files");
    }
    let newer: HashSet<String> = docs.iter().filter(|p| times.get(p.as_str()).is_some_and(|t| *t > at)).cloned().collect();
    let mut skip: HashSet<String> = covered.union(&newer).cloned().collect();
    skip.extend(others.into_iter().map(|(_, p)| p));
    let read = |p: &str| code::read_source(&root.root, p);
    code::scip_rows_in(&ix, dir, &skip, rows, files, &read);
    covered.extend(docs.into_iter().filter(|p| !newer.contains(p)));
    let note = format!("prebuilt {} {}", ix.tool.0, ix.tool.1).trim_end().to_string();
    match newer.len() {
        0 => note,
        1 => format!("{note} (1 file newer, scanned)"),
        n => format!("{note} ({n} files newer, scanned)"),
    }
}

/// The walked marker and manifest files named `name` in a unit's folder or
/// below it, as paths below the root.
fn unit_files<'a>(walk: &'a code::Walk, dir: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    walk.markers.iter().map(|f| f.path.as_str()).filter(move |p| {
        let (d, n) = p.rsplit_once('/').unwrap_or(("", p));
        n == name && (dir.is_empty() || d == dir || d.strip_prefix(dir).is_some_and(|r| r.starts_with('/')))
    })
}

/// The project arguments of one unit's run ([`crate::langs::Projects`]):
/// each project folder that has its own configuration as a path relative to
/// the unit, and for one without, the path of the configuration atlas
/// writes for it into `setup.dir` (never into the user's folder).
///
/// # Errors
///
/// [`Error::Io`] when an inferred configuration cannot be written.
fn unit_projects(
    setup: &CodeSetup,
    walk: &code::Walk,
    u: &code::LangUnit,
    cwd: &Path,
    stem: &str,
    p: &crate::langs::Projects,
) -> Result<Vec<std::ffi::OsString>> {
    // The folders below the unit holding a `package.json`, relative to it.
    let prefix = if u.dir.is_empty() { String::new() } else { format!("{}/", u.dir) };
    let manifests: Vec<String> = walk
        .markers
        .iter()
        .filter_map(|f| f.path.strip_prefix(&prefix))
        .filter_map(|rel| rel.strip_suffix("package.json").map(|d| d.trim_end_matches('/').to_string()))
        .collect();
    let projects = p.of_unit(cwd, &manifests);
    let only_self = projects.len() == 1 && projects[0] == ".";
    let mut out = Vec::new();
    for (i, proj) in projects.iter().enumerate() {
        let dir = if proj == "." { cwd.to_path_buf() } else { cwd.join(proj) };
        if dir.join(p.config).is_file() {
            // The tool reads the unit's own folder unasked.
            if !only_self {
                out.push(std::ffi::OsString::from(proj));
            }
            continue;
        }
        let file = setup.dir.join(format!("{stem}.{i}.{}", p.config));
        fs::write(&file, p.inferred_for(&dir))?;
        out.push(file.into_os_string());
    }
    Ok(out)
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
    /// The first walk since start has not finished for this root (it is
    /// being checked, or indexed for the first time).
    Starting,
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
        let status: Status = setup.roots.iter().map(|r| (r.name.clone(), RootState::Starting)).collect();
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
        let busy = |s: &mut Status| s.values().any(|x| matches!(x, RootState::Starting | RootState::Pending | RootState::Running));
        let (st, _) = self.shared.moved.wait_timeout_while(st, timeout, busy).unwrap_or_else(|p| p.into_inner());
        !st.values().any(|x| matches!(x, RootState::Starting | RootState::Pending | RootState::Running))
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
    // Whether a root's first walk since start has finished: until then its
    // state reads `Starting`, never "stale".
    let mut settled: Vec<bool> = vec![false; n];
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
                let fp = code::root_fingerprint(r).unwrap_or_default();
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
                (Some(_), _) if !settled[i] && deferred[i].is_none() => RootState::Starting,
                (Some(_), _) => deferred[i].clone().map_or(RootState::Pending, RootState::Deferred),
                (None, Some((_, e))) => RootState::Failed(e.clone()),
                (None, None) => RootState::Idle,
            };
            if changed[i].is_none() {
                settled[i] = true;
            }
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
            set(shared, &r.name, if settled[i] { RootState::Running } else { RootState::Starting });
            log_line(&setup.dir, &format!("{} indexing", r.name));
            let res = index_root(setup, &mut conn, r, &rust);
            let _ = release_lease(&conn, &holder);
            changed[i] = None;
            match res {
                Ok(rep) => {
                    log_line(&setup.dir, &format!("{} done: {} in {:.0} ms ({} defs, {} refs, {} impls)", r.name, rep.rust, rep.total_ms, rep.write.defs, rep.write.refs, rep.write.impls));
                    failed[i] = None;
                    seen_fp[i] = code::root_fingerprint(r).unwrap_or_default();
                    shared.reports.lock().unwrap_or_else(|p| p.into_inner()).push(rep);
                    set(shared, &r.name, RootState::Idle);
                    settled[i] = true;
                }
                Err(e) => {
                    log_line(&setup.dir, &format!("{} failed: {e}", r.name));
                    failed[i] = Some((seen_fp[i].clone(), e.to_string()));
                    set(shared, &r.name, RootState::Failed(e.to_string()));
                    settled[i] = true;
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
