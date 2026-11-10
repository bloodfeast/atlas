//! The code index behind the `symbol` tool: definitions, references,
//! impl edges and callers for Rust (from `rust-analyzer scip`, decoded by
//! [`crate::scip`]), WGSL (scanned by [`crate::wgsl`]), and any language a
//! SCIP indexer wrote a file for ([`scip_rows`]).
//!
//! # Storage
//!
//! One pre-sorted occurrence table, `code_occ`, keyed by
//! `(name, sect, file, line, col, ord)`: every row a symbol answer needs is
//! one index range per section, so the answer is one statement (three
//! `LIMIT`ed ranges and a count row, `UNION ALL`). `sect` is 0 for a
//! definition, 1 for a reference, 2 for an impl edge. Source lines are kept
//! once each in `code_lines` and joined, so a snippet costs no per-row copy.
//! `code_counts` holds per-name totals; `code_roots` each root's last
//! indexing (fingerprint, time, counts, error); `code_units` the parts of a
//! root indexed on their own (its Rust side, a `--scip` file's languages).
//!
//! # What a row is
//!
//! * Rust definitions: every definition occurrence of a non-local symbol
//!   in the SCIP index. Kind from `SymbolInformation.kind`, else from the
//!   descriptor suffix. Container: the parent descriptor (`Store` for
//!   `Store::open`, the self type for a method in an impl block).
//! * Rust references: every non-definition occurrence of a symbol that is
//!   defined in the same index (references into dependencies and `std` are
//!   not stored, except into a package another code root defines: see
//!   [`Rows::linked`]). `within` is the innermost enclosing definition that has an
//!   `enclosing_range` (rust-analyzer gives one for functions, methods,
//!   impls and types), so references double as call sites: `within` names
//!   the caller.
//! * Rust impl edges: rust-analyzer encodes an impl block as the symbol
//!   `impl#[SelfType][Trait]` (it emits no `relationships`). Each block is
//!   an edge row under both the trait's and the self type's name. Its
//!   `end_line` is the block's closing line, so an impl is an item with a
//!   range: annotations name the innermost fn inside it, else the impl.
//! * Rust definitions and impl blocks the SCIP index lacks (items under an
//!   inactive `cfg`, files outside every crate's module tree, items written
//!   inside a `macro_rules!` body, or every item when rust-analyzer did not
//!   run) come from an owned line scan ([`scan_rust_defs`],
//!   [`scan_rust_impls`]) and are marked `(scan)`. A scanned item inside an
//!   impl or trait block takes the block's type or trait as its container.
//! * WGSL: declarations, and identifier references that name a symbol
//!   declared in the same root (`#import` items included).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Error, Result};
use crate::scip::{self, Index};
use crate::text;
use crate::wgsl;

/// Default rust-analyzer timeout, seconds.
pub const DEFAULT_TIMEOUT_S: usize = 1800;
/// Default quiet time before a changed root is re-indexed, seconds.
pub const DEFAULT_DEBOUNCE_S: usize = 20;
/// Bumped whenever the code tables change shape or meaning.
pub const CODE_SCHEMA: &str = "code-7";
/// The schemas [`ensure_schema`] migrates in place (their rows keep their
/// meaning in [`CODE_SCHEMA`]), oldest first, each with the statements that
/// bring it to the next; any other schema is dropped and rebuilt.
const MIGRATIONS: &[(&str, &str)] = &[("code-5", MIGRATE_5), ("code-6", MIGRATE_6)];
/// The longest source line stored for snippets, in bytes.
pub const MAX_LINE: usize = 400;
/// The largest source file read, in bytes.
pub const MAX_FILE: u64 = 4 << 20;

/// Section numbers in `code_occ`.
pub const SECT_DEF: i64 = 0;
/// See [`SECT_DEF`].
pub const SECT_REF: i64 = 1;
/// See [`SECT_DEF`].
pub const SECT_IMPL: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS code_files(
    id INTEGER PRIMARY KEY,
    root TEXT NOT NULL,
    path TEXT NOT NULL,
    lang TEXT NOT NULL,
    indexed TEXT NOT NULL DEFAULT '',
    UNIQUE(root, path)
);
CREATE TABLE IF NOT EXISTS code_occ(
    name TEXT NOT NULL,
    sect INTEGER NOT NULL,
    file INTEGER NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    ord INTEGER NOT NULL,
    kind TEXT NOT NULL,
    container TEXT NOT NULL,
    within TEXT NOT NULL,
    end_line INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(name, sect, file, line, col, ord)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS code_occ_file ON code_occ(file);
CREATE TABLE IF NOT EXISTS code_lines(
    file INTEGER NOT NULL,
    line INTEGER NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY(file, line)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS code_counts(
    name TEXT NOT NULL,
    root TEXT NOT NULL,
    defs INTEGER NOT NULL,
    refs INTEGER NOT NULL,
    impls INTEGER NOT NULL,
    PRIMARY KEY(name, root)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS code_roots(
    root TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    indexed TEXT NOT NULL,
    files INTEGER NOT NULL,
    defs INTEGER NOT NULL,
    refs INTEGER NOT NULL,
    impls INTEGER NOT NULL,
    rust TEXT NOT NULL,
    elapsed_ms INTEGER NOT NULL,
    dir TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS code_units(
    root TEXT NOT NULL,
    dir TEXT NOT NULL,
    lang TEXT NOT NULL,
    source TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    indexed TEXT NOT NULL,
    elapsed_ms INTEGER NOT NULL,
    PRIMARY KEY(root, dir, lang)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS code_packages(
    root TEXT NOT NULL,
    package TEXT NOT NULL,
    defines INTEGER NOT NULL,
    PRIMARY KEY(root, package, defines)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS code_packages_package ON code_packages(package, defines);
CREATE TABLE IF NOT EXISTS code_lease(
    id INTEGER PRIMARY KEY CHECK (id = 1),
    holder TEXT NOT NULL,
    until INTEGER NOT NULL
);
";

const DROP: &str = "DROP TABLE IF EXISTS code_files; DROP TABLE IF EXISTS code_occ; DROP TABLE IF EXISTS code_lines;
DROP TABLE IF EXISTS code_counts; DROP TABLE IF EXISTS code_roots; DROP TABLE IF EXISTS code_units; DROP TABLE IF EXISTS code_packages;";

/// From `code-5`: the tables keep their rows (every `code-5` row means the
/// same in `code-6`), `code_units` is added, and each indexed root gets its
/// Rust unit from its `code_roots` note.
const MIGRATE_5: &str = "INSERT OR IGNORE INTO code_units(root, dir, lang, source, fingerprint, indexed, elapsed_ms)
SELECT root, '', 'rust', rust, fingerprint, indexed, elapsed_ms FROM code_roots WHERE rust <> 'no Rust files';";

/// From `code-6`: `code_packages` is added (by the schema), and every
/// root's fingerprint is cleared so the next walk re-indexes it and records
/// its packages (its cross-root references come with that run).
const MIGRATE_6: &str = "UPDATE code_roots SET fingerprint = '';";

/// One unit of a code root: a part indexed and timed on its own (in this
/// release the root's Rust side, and a `--scip` file's language).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// The unit's folder below the root (empty for the root itself).
    pub dir: String,
    /// Its language (`rust`, `go`, `typescript`, ...).
    pub lang: String,
    /// Where its rows came from: `scip` (rust-analyzer), `prebuilt TOOL
    /// VERSION` (a `--scip` file from another indexer), or `scan only:
    /// why`.
    pub source: String,
}

/// Folder names that hold test inputs rather than a project's code: a unit
/// at or below one (`fixtures/scip/ts`, `testdata/mod`) is indexed like any
/// other but left out of the root's note ([`units_note`]), which every
/// `symbol` answer's freshness lines read.
pub const NOTE_SKIPPED_DIRS: &[&str] = &["fixtures", "fixture", "__fixtures__", "testdata", "test-data", "test_data"];

/// True when a unit folder is at or below a [`NOTE_SKIPPED_DIRS`] folder.
pub fn fixture_unit(dir: &str) -> bool {
    dir.split('/').any(|d| NOTE_SKIPPED_DIRS.contains(&d))
}

/// The one-line note `code_roots.rust` keeps: a lone Rust unit's source as
/// it always was (`scip`, `scan only: why`), else `lang: source` per unit,
/// joined by `; `. Units in fixture folders ([`fixture_unit`]) are left
/// out, unless every unit is in one.
pub fn units_note(units: &[Unit]) -> String {
    let real: Vec<Unit> = units.iter().filter(|u| !fixture_unit(&u.dir)).cloned().collect();
    let units = if real.is_empty() { units } else { &real[..] };
    match units {
        [] => "no Rust files".into(),
        [u] if u.lang == "rust" => u.source.clone(),
        _ => units
            .iter()
            .map(|u| if u.dir.is_empty() { format!("{}: {}", u.lang, u.source) } else { format!("{} {}: {}", u.lang, u.dir, u.source) })
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// One configured code root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRoot {
    /// Short name.
    pub name: String,
    /// The folder.
    pub root: PathBuf,
    /// See [`crate::config::CodeRootSpec::build_scripts`].
    pub build_scripts: bool,
    /// Its prebuilt SCIP files ([`Prebuilt`]).
    pub scip: Prebuilt,
}

/// Where a root's prebuilt SCIP files are (`code.roots[i].scip`): files an
/// indexer wrote elsewhere (CI, WSL, Docker) that the code index reads as a
/// unit's index. A source file newer than the file it is in is scanned
/// instead, so a stale prebuilt index never gives wrong lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Prebuilt {
    /// [`PREBUILT_FILE`] in a unit's folder, when there is one (the
    /// default).
    #[default]
    Convention,
    /// None is read.
    Off,
    /// Files by unit folder (below the root, `/`-separated; empty for the
    /// root): a path is that folder's file, `None` reads none for it, and a
    /// folder not named follows the convention.
    Files(Vec<(String, Option<PathBuf>)>),
}

/// The file name a prebuilt SCIP index has by convention, in its unit's
/// folder.
pub const PREBUILT_FILE: &str = "index.scip";

impl Prebuilt {
    /// The prebuilt file of a unit folder, if one is configured or found by
    /// the walk.
    pub fn for_dir(&self, root: &Path, walk: &Walk, dir: &str) -> Option<PathBuf> {
        let found = || {
            let want = if dir.is_empty() { PREBUILT_FILE.to_string() } else { format!("{dir}/{PREBUILT_FILE}") };
            walk.prebuilt.iter().any(|f| f.path == want).then(|| root.join(&want))
        };
        match self {
            Prebuilt::Off => None,
            Prebuilt::Convention => found(),
            Prebuilt::Files(list) => match list.iter().find(|(d, _)| d == dir) {
                Some((_, p)) => p.clone().filter(|p| p.is_file()),
                None => found(),
            },
        }
    }

    /// The configured files, as the walk lists files (their path in full),
    /// for a root's fingerprint: a file outside the root changes the
    /// fingerprint too.
    pub fn configured(&self) -> Vec<WalkedFile> {
        let Prebuilt::Files(list) = self else { return Vec::new() };
        list.iter()
            .filter_map(|(_, p)| p.as_ref())
            .filter_map(|p| {
                let m = fs::metadata(p).ok()?;
                let mtime = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64);
                Some(WalkedFile { path: p.to_string_lossy().into_owned(), mtime, size: m.len() })
            })
            .collect()
    }
}

/// A root's fingerprint: its walk's ([`Walk::fingerprint`]) and its
/// configured prebuilt files'.
///
/// # Errors
///
/// [`Error::Io`] when the root cannot be walked.
pub fn root_fingerprint(root: &CodeRoot) -> Result<String> {
    Ok(walk(&root.root)?.fingerprint_with(&root.scip.configured()))
}

/// Everything the code index is configured with.
#[derive(Debug, Clone)]
pub struct CodeSetup {
    /// The roots.
    pub roots: Vec<CodeRoot>,
    /// An explicit rust-analyzer binary.
    pub rust_analyzer: Option<PathBuf>,
    /// The rust-analyzer timeout.
    pub timeout: Duration,
    /// Quiet time before a changed root is re-indexed.
    pub debounce: Duration,
    /// See [`crate::config::CodeSpec::defer_lock`].
    pub defer_lock: Option<PathBuf>,
    /// Background re-indexing waits while a `cargo` process (not an
    /// ancestor of this one) runs. On by default; tests turn it off so
    /// another build on the machine cannot stall them.
    pub defer_on_cargo: bool,
    /// Where `.scip` files and logs go.
    pub dir: PathBuf,
    /// Watch the roots for changes (else only startup and explicit checks).
    pub watch: bool,
    /// Seconds between safety-net fingerprint walks of a watched root.
    pub rescan_interval: Duration,
    /// Which SCIP indexers of [`crate::langs`] run (rust-analyzer is
    /// chosen apart, see [`crate::code_index::RustSource`]).
    pub indexers: Indexers,
}

/// Which SCIP indexers of the language table ([`crate::langs::Driver`])
/// the code index runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Indexers {
    /// Each one that is found ([`crate::langs::Driver::find`]); the default.
    Search,
    /// None: every unit is scanned, and its note says why.
    Off(String),
    /// These binaries, by tool name (`scip-go`); a tool not listed does not
    /// run.
    Fixed(Vec<(String, PathBuf)>),
    /// The configuration's `code.indexers` object: a named tool runs from
    /// its path (`None`: it does not run), a tool not named is found as
    /// for [`Indexers::Search`].
    Chosen(Vec<(String, Option<PathBuf>)>),
}

impl CodeSetup {
    /// Builds the setup from the configuration's `code` section.
    ///
    /// # Arguments
    ///
    /// * `spec` - the parsed section.
    /// * `db` - the database path (the default `dir` is `code` beside it).
    /// * `watch` - whether to watch the roots.
    pub fn from_spec(spec: &crate::config::CodeSpec, db: &Path, watch: bool) -> CodeSetup {
        let dir = spec.dir.clone().unwrap_or_else(|| db.parent().unwrap_or(Path::new(".")).join("code"));
        CodeSetup {
            roots: spec
                .roots
                .iter()
                .map(|r| CodeRoot { name: r.name.clone(), root: r.root.clone(), build_scripts: r.build_scripts, scip: r.scip.clone() })
                .collect(),
            rust_analyzer: spec.rust_analyzer.clone(),
            timeout: Duration::from_secs(spec.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S) as u64),
            debounce: Duration::from_secs(spec.debounce_s.unwrap_or(DEFAULT_DEBOUNCE_S) as u64),
            defer_lock: spec.defer_lock.clone(),
            defer_on_cargo: true,
            dir,
            watch,
            rescan_interval: Duration::from_secs(crate::config::DEFAULT_RESCAN_INTERVAL_S),
            indexers: match &spec.indexers {
                None => Indexers::Search,
                Some(None) => Indexers::Off("indexers are off (code.indexers is false)".into()),
                Some(Some(list)) => Indexers::Chosen(list.clone()),
            },
        }
    }
}

/// The stored code schema, or `None` (no `meta` table yet, or no row).
fn stored_code_schema(conn: &Connection) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key = 'code_schema'", [], |r| r.get(0)).optional().unwrap_or(None)
}

/// The number of a `code-N` schema; `None` for any other shape (older
/// schemas than the numbered ones, or garbage: both count as older).
fn code_schema_number(s: &str) -> Option<u64> {
    s.strip_prefix("code-")?.parse().ok()
}

/// The refusal for a code index at schema `have` when it is newer than
/// [`CODE_SCHEMA`].
pub(crate) fn code_schema_too_new(have: Option<&str>, path: &str) -> Option<Error> {
    let have = have?;
    let ours = code_schema_number(CODE_SCHEMA)?;
    (code_schema_number(have)? > ours).then(|| Error::StoreTooNew {
        store: "code index".into(),
        path: path.to_string(),
        found: have.to_string(),
        supported: CODE_SCHEMA.to_string(),
    })
}

/// Whether the code index in `conn` is newer than this binary reads (for
/// `atlas doctor`; reads only). `db` names the file in the message.
pub fn code_too_new(conn: &Connection, db: &Path) -> Option<Error> {
    code_schema_too_new(stored_code_schema(conn).as_deref(), &db.display().to_string())
}

/// Creates the code tables: migrates them in place from `code-5` (adding
/// `code_units`) or `code-6` (adding `code_packages`), else drops and
/// rebuilds them when their schema changed. A newer schema than
/// [`CODE_SCHEMA`] is refused and left as it is.
///
/// # Errors
///
/// [`Error::StoreTooNew`] for a newer code index (nothing written), else
/// [`Error::Db`] from SQLite.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    let have = stored_code_schema(conn);
    if have.as_deref() == Some(CODE_SCHEMA) {
        return Ok(());
    }
    // A newer binary's index is refused before anything is written: dropping
    // it here would cost that binary a full reindex, and each would drop the
    // other's on every open.
    if let Some(e) = code_schema_too_new(have.as_deref(), conn.path().unwrap_or("")) {
        return Err(e);
    }
    conn.execute_batch("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
    // One write transaction, taken at its start (a deferred one that reads
    // first cannot wait for another writer).
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    make_schema(&tx)?;
    tx.commit()?;
    Ok(())
}

/// The schema step of [`ensure_schema`], under the write lock: reads the
/// schema again (another binary may have written it since the first read),
/// refuses a newer one with nothing written, and migrates or rebuilds an
/// older one. The caller commits; on an error dropping `tx` rolls back.
///
/// # Errors
///
/// [`Error::StoreTooNew`], else [`Error::Db`] from SQLite.
fn make_schema(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let have = stored_code_schema(tx);
    if have.as_deref() == Some(CODE_SCHEMA) {
        return Ok(());
    }
    if let Some(e) = code_schema_too_new(have.as_deref(), tx.path().unwrap_or("")) {
        return Err(e);
    }
    if let Some(from) = MIGRATIONS.iter().position(|(s, _)| have.as_deref() == Some(*s)) {
        tx.execute_batch(SCHEMA)?;
        for (_, sql) in &MIGRATIONS[from..] {
            tx.execute_batch(sql)?;
        }
    } else {
        tx.execute_batch(DROP)?;
        tx.execute_batch(SCHEMA)?;
    }
    tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('code_schema', ?1)", [CODE_SCHEMA])?;
    Ok(())
}

/// A file of a code root, as walked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    /// Path below the root, `/`-separated.
    pub path: String,
    /// Modification time, nanoseconds since the epoch.
    pub mtime: i64,
    /// Size in bytes.
    pub size: u64,
}

/// The Rust, manifest and WGSL files of a root.
#[derive(Debug, Clone, Default)]
pub struct Walk {
    /// `.rs` files.
    pub rust: Vec<WalkedFile>,
    /// `.wgsl` files.
    pub wgsl: Vec<WalkedFile>,
    /// `Cargo.toml` and `Cargo.lock` files (they change what rust-analyzer
    /// sees without a `.rs` change).
    pub manifests: Vec<WalkedFile>,
    /// `.proto` files (see [`crate::proto`]).
    pub proto: Vec<WalkedFile>,
    /// Other source files searched for rpc call sites
    /// ([`crate::proto::MENTION_EXTS`], Rust and the languages of
    /// [`crate::langs`] aside).
    pub mention: Vec<WalkedFile>,
    /// Source files of the languages of [`crate::langs`] (Go, TypeScript, JavaScript), outside the
    /// folders their row skips.
    pub source: Vec<WalkedFile>,
    /// Their unit markers and the other files whose change makes the root
    /// stale (`go.mod`, `go.sum`, `package.json`, `tsconfig.json`, lock files).
    pub markers: Vec<WalkedFile>,
    /// Prebuilt SCIP indexes found by convention ([`PREBUILT_FILE`]).
    pub prebuilt: Vec<WalkedFile>,
}

/// One unit of a language of [`crate::langs`] in a root: a folder holding
/// one of its markers (a Go module).
#[derive(Debug, Clone)]
pub struct LangUnit {
    /// The language's row.
    pub lang: &'static crate::langs::Lang,
    /// The folder below the root, `/`-separated (empty: the root).
    pub dir: String,
}

impl PartialEq for LangUnit {
    fn eq(&self, other: &LangUnit) -> bool {
        self.lang.name == other.lang.name && self.dir == other.dir
    }
}

impl Eq for LangUnit {}

/// The units of a walked root: every folder with a marker of a language of
/// [`crate::langs`], sorted by language then folder. Each Go module is its
/// own unit, nested ones included (the Go tool treats them so); for a row
/// without [`crate::langs::Lang::nested_units`] (TypeScript) the shallowest
/// marker claims its subtree.
pub fn units(walk: &Walk) -> Vec<LangUnit> {
    let mut out: Vec<LangUnit> = Vec::new();
    // A marker may be a source file (Python's `setup.py`).
    for f in walk.markers.iter().chain(&walk.source) {
        let (dir, name) = f.path.rsplit_once('/').unwrap_or(("", f.path.as_str()));
        for lang in crate::langs::LANGS.iter().filter(|l| l.marker_rank(name).is_some()) {
            out.push(LangUnit { lang, dir: dir.to_string() });
        }
    }
    out.sort_by(|a, b| (a.lang.name, &a.dir).cmp(&(b.lang.name, &b.dir)));
    out.dedup();
    let below = |d: &str, top: &str| top.is_empty() || (d.len() > top.len() && d.starts_with(top) && d.as_bytes()[top.len()] == b'/');
    let all = out.clone();
    out.retain(|u| u.lang.nested_units || !all.iter().any(|o| o.lang.name == u.lang.name && o.dir != u.dir && below(&u.dir, &o.dir)));
    out
}

/// The unit holding a source file: the deepest unit folder of its
/// language above it (for JavaScript, of the row it takes its units from:
/// [`crate::langs::Lang::units_of`]).
pub fn unit_of<'a>(units: &'a [LangUnit], lang: &str, path: &str) -> Option<&'a LangUnit> {
    let lang = crate::langs::by_name(lang).and_then(|l| l.units_of).unwrap_or(lang);
    units
        .iter()
        .filter(|u| u.lang.name == lang && (u.dir.is_empty() || (path.len() > u.dir.len() && path.starts_with(u.dir.as_str()) && path.as_bytes()[u.dir.len()] == b'/')))
        .max_by_key(|u| u.dir.len())
}

/// True for a path component the walk never enters: build output,
/// dot folders, vendored JavaScript.
fn skipped_dir(name: &str) -> bool {
    name.starts_with('.') || name == "target" || name == "node_modules"
}

/// True when a changed path (relative, `/`-separated) can change the code
/// index.
pub fn relevant(rel: &str) -> bool {
    if rel.split('/').any(skipped_dir) {
        return false;
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    rel.ends_with(".rs")
        || rel.ends_with(".wgsl")
        || rel.ends_with("Cargo.toml")
        || rel.ends_with("Cargo.lock")
        || rel.ends_with(".proto")
        || crate::proto::is_mention_file(rel)
        || crate::langs::of_path(rel).is_some_and(|l| !l.skips_path(rel))
        || crate::langs::of_stale_file(name).is_some_and(|l| !l.skips_path(rel))
        || name == PREBUILT_FILE
}

/// The language row of a file the walk lists in [`Walk::source`]: its
/// extension is a row's and no folder on its path is one the row skips.
pub fn source_lang(rel: &str) -> Option<&'static crate::langs::Lang> {
    crate::langs::of_path(rel).filter(|l| !l.skips_path(rel))
}

/// Walks a code root for `.rs`, `.wgsl` and Cargo manifest files, skipping
/// `target`, `node_modules` and dot folders, never following links.
///
/// # Errors
///
/// [`Error::Io`] when the root cannot be read.
pub fn walk(root: &Path) -> Result<Walk> {
    let mut w = Walk::default();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    let mut first = true;
    while let Some((dir, rel)) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if first => return Err(Error::Io(format!("{}: {e}", dir.display()))),
            Err(_) => continue,
        };
        first = false;
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if ft.is_dir() {
                if !skipped_dir(&name) {
                    stack.push((e.path(), child));
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let list = if name.ends_with(".rs") {
                &mut w.rust
            } else if name.ends_with(".wgsl") {
                &mut w.wgsl
            } else if name == "Cargo.toml" || name == "Cargo.lock" {
                &mut w.manifests
            } else if name.ends_with(".proto") {
                &mut w.proto
            } else if source_lang(&child).is_some() {
                &mut w.source
            } else if crate::langs::of_stale_file(&name).is_some_and(|l| !l.skips_path(&child)) {
                &mut w.markers
            } else if name == PREBUILT_FILE {
                &mut w.prebuilt
            } else if crate::proto::is_mention_file(&name) {
                &mut w.mention
            } else {
                continue;
            };
            let Ok(m) = e.metadata() else { continue };
            let mtime = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64);
            list.push(WalkedFile { path: child, mtime, size: m.len() });
        }
    }
    for l in [&mut w.rust, &mut w.wgsl, &mut w.manifests, &mut w.proto, &mut w.mention, &mut w.source, &mut w.markers, &mut w.prebuilt] {
        l.sort_by(|a, b| a.path.cmp(&b.path));
    }
    Ok(w)
}

impl Walk {
    /// A digest of every walked file's path, time and size: equal when
    /// nothing the index depends on changed. (A root with none of the
    /// [`crate::langs`] files digests as it did before they existed.)
    pub fn fingerprint(&self) -> String {
        self.fingerprint_with(&[])
    }

    /// [`Walk::fingerprint`] with files from outside the walk (configured
    /// prebuilt indexes); with none, the same digest.
    pub fn fingerprint_with(&self, extra: &[WalkedFile]) -> String {
        let mut s = String::new();
        let extra = extra.to_vec();
        for (tag, l) in [
            ("r", &self.rust),
            ("w", &self.wgsl),
            ("m", &self.manifests),
            ("p", &self.proto),
            ("o", &self.mention),
            ("s", &self.source),
            ("k", &self.markers),
            ("x", &self.prebuilt),
            ("c", &extra),
        ] {
            for f in l {
                s.push_str(&format!("{tag}\t{}\t{}\t{}\n", f.path, f.mtime, f.size));
            }
        }
        // A root with `.proto` files or generated gRPC code is indexed once
        // more when rpc linking ([`crate::proto::link_rows`]) first applies.
        if !self.proto.is_empty() || self.source.iter().any(|f| crate::langs::is_generated(&f.path)) {
            s.push_str("rpc links 1\n");
        }
        crate::sha256::hex(s.as_bytes())
    }
}

/// One occurrence row before it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occ {
    /// The short name looked up.
    pub name: String,
    /// [`SECT_DEF`], [`SECT_REF`] or [`SECT_IMPL`].
    pub sect: i64,
    /// Index into [`Rows::files`].
    pub file: u32,
    /// Line, 1-based.
    pub line: u32,
    /// Column, 0-based, in the indexer's units (bytes for ASCII lines).
    pub col: u32,
    /// What it is (`fn`, `struct`, `impl`, `use`, ...).
    pub kind: String,
    /// The parent (a definition's type or module; an impl edge's
    /// `Trait for Type`).
    pub container: String,
    /// The enclosing definition of a reference: the caller.
    pub within: String,
}

/// Everything one root contributes.
#[derive(Debug, Default)]
pub struct Rows {
    /// Files: path below the root and language.
    pub files: Vec<(String, &'static str)>,
    /// Occurrences.
    pub occ: Vec<Occ>,
    /// Counts of what came from where, for the stats line.
    pub stats: RowStats,
    /// End lines decided while building rows, by (file, line): those of
    /// [`scip_rows`]. A definition without one gets the brace matcher's end
    /// when written.
    pub ends: HashMap<(u32, u32), u32>,
    /// The packages other code roots define ([`crate::langs::package_key`]),
    /// set before the SCIP rows are built: a reference into one of them is
    /// stored (a cross-root reference) where any other reference into a
    /// dependency is dropped.
    pub linked: HashSet<String>,
    /// The packages this root's SCIP indexes define symbols of.
    pub pkg_defs: HashSet<String>,
    /// The packages this root's references go into without defining them
    /// (its dependencies, linked or not).
    pub pkg_uses: HashSet<String>,
}

/// Counts of what [`Rows`] holds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowStats {
    /// Definitions from SCIP.
    pub scip_defs: usize,
    /// Definitions from the line scan, absent from SCIP.
    pub scan_defs: usize,
    /// References from SCIP.
    pub scip_refs: usize,
    /// References into dependencies, not stored.
    pub external_refs: usize,
    /// References into a package another code root defines, stored
    /// ([`Rows::linked`]).
    pub xroot_refs: usize,
    /// Impl edge rows.
    pub impls: usize,
    /// WGSL declarations.
    pub wgsl_defs: usize,
    /// WGSL references.
    pub wgsl_refs: usize,
    /// `.proto` definitions.
    pub proto_defs: usize,
    /// `.proto` type references.
    pub proto_refs: usize,
    /// rpc call sites in other source files.
    pub mentions: usize,
    /// Definitions from the scanner of a language of [`crate::langs`]
    /// (files no SCIP index covers).
    pub lang_defs: usize,
    /// rpc stub links: a generated stub method of an rpc
    /// ([`crate::proto::link_rows`]).
    pub rpc_stubs: usize,
    /// References to an rpc's stub outside generated code: exact call
    /// sites.
    pub rpc_calls: usize,
    /// rpc implementation links: a method of a type that extends,
    /// implements or embeds a generated server type.
    pub rpc_servers: usize,
}

impl Rows {
    pub(crate) fn file(&mut self, map: &mut HashMap<String, u32>, path: &str, lang: &'static str) -> u32 {
        if let Some(&i) = map.get(path) {
            return i;
        }
        let i = self.files.len() as u32;
        self.files.push((path.to_string(), lang));
        map.insert(path.to_string(), i);
        i
    }
}

/// The type named in an impl descriptor: `Result<T, E>` is `Result`,
/// `&mut Foo` is `Foo`, `[T]` stays `[T]`.
fn type_name(t: &str) -> &str {
    let t = t.trim_start_matches('&').trim_start_matches("mut ").trim_start_matches("dyn ").trim();
    let end = t.find(['<', ' ']).unwrap_or(t.len());
    let head = &t[..end];
    head.rsplit("::").next().unwrap_or(head)
}

/// Name, container and impl parts of a symbol, from its descriptors.
struct Parsed<'a> {
    name: &'a str,
    suffix: char,
    container: String,
    /// For a symbol inside an impl block: (self type, trait).
    imp: Option<(&'a str, Option<&'a str>)>,
    /// True for the impl block itself.
    is_impl_block: bool,
}

fn parse_symbol(sym: &str) -> Option<Parsed<'_>> {
    let d = scip::descriptors(sym);
    let last = *d.last()?;
    // impl#[Self][Trait]...: find the impl descriptor and its type params.
    let mut imp = None;
    let mut impl_at = None;
    for (i, x) in d.iter().enumerate() {
        if x.name == "impl" && x.suffix == '#' {
            let ty = d.get(i + 1).filter(|p| p.suffix == '[').map(|p| type_name(p.name));
            let tr = d.get(i + 2).filter(|p| p.suffix == '[').map(|p| type_name(p.name));
            if let Some(ty) = ty {
                imp = Some((ty, tr));
                impl_at = Some(i);
            }
        }
    }
    let n = d.len();
    let is_impl_block = impl_at.is_some_and(|i| d[i + 1..].iter().all(|x| x.suffix == '['));
    if is_impl_block {
        return Some(Parsed { name: "impl", suffix: '#', container: String::new(), imp, is_impl_block });
    }
    if last.suffix == '[' || last.suffix == ')' {
        // Type and value parameters are not indexed.
        return None;
    }
    let container = match (impl_at, imp) {
        (Some(i), Some((ty, _))) if i + 1 < n => ty.to_string(),
        _ => d[..n - 1].iter().rev().find(|x| x.suffix != '[').map(|x| x.name.to_string()).unwrap_or_default(),
    };
    Some(Parsed { name: last.name, suffix: last.suffix, container, imp, is_impl_block })
}

/// Each symbol's package ([`crate::langs::package_key`]) as an index into
/// the distinct keys, which are returned beside: `None` for a local symbol
/// or a package with no name.
fn package_ids(ix: &Index<'_>) -> (Vec<Option<u32>>, Vec<String>) {
    let mut seen: HashMap<(&str, &str), Option<u32>> = HashMap::new();
    let mut keys: Vec<String> = Vec::new();
    let ids = ix
        .symbols
        .iter()
        .map(|s| {
            let (m, n) = scip::package(s)?;
            *seen.entry((m, n)).or_insert_with(|| {
                crate::langs::package_key(m, n).map(|k| {
                    keys.push(k);
                    keys.len() as u32 - 1
                })
            })
        })
        .collect();
    (ids, keys)
}

/// Records the packages an index defines symbols of in `rows.pkg_defs`, and
/// says per package whether a reference into it is stored although its
/// symbol has no definition here (another root defines the package:
/// [`Rows::linked`]).
fn package_links(ix: &Index<'_>, defined: &[bool], rows: &mut Rows) -> (Vec<Option<u32>>, Vec<String>, Vec<bool>) {
    let (ids, keys) = package_ids(ix);
    for (s, id) in ids.iter().enumerate() {
        if let (true, Some(k)) = (defined[s], id) {
            if !rows.pkg_defs.contains(&keys[*k as usize]) {
                rows.pkg_defs.insert(keys[*k as usize].clone());
            }
        }
    }
    // A package this index defines is its own, whoever else defines it
    // too (a second checkout of the same repository).
    let link = keys.iter().map(|k| rows.linked.contains(k) && !rows.pkg_defs.contains(k)).collect();
    (ids, keys, link)
}

/// For a reference to a symbol with no definition in its index: records its
/// package as used, and says whether the reference is stored (its package
/// is another root's), counting it either way.
fn external_ref(rows: &mut Rows, id: Option<u32>, keys: &[String], link: &[bool], used: &mut [bool]) -> bool {
    let Some(k) = id.map(|k| k as usize) else {
        rows.stats.external_refs += 1;
        return false;
    };
    if !used[k] {
        used[k] = true;
        rows.pkg_uses.insert(keys[k].clone());
    }
    if link[k] {
        rows.stats.xroot_refs += 1;
    } else {
        rows.stats.external_refs += 1;
    }
    link[k]
}

/// Builds rows from a decoded SCIP index.
///
/// # Arguments
///
/// * `ix` - the decoded index.
/// * `rows` - where rows go.
/// * `files` - the file map, shared with the WGSL and scan rows of the same
///   root.
pub fn rust_rows(ix: &Index<'_>, rows: &mut Rows, files: &mut HashMap<String, u32>) {
    let parsed: Vec<Option<Parsed<'_>>> = ix.symbols.iter().map(|s| parse_symbol(s)).collect();
    // A symbol is the project's when it has a definition here (or is an
    // impl block, whose occurrence is not marked as one).
    let mut defined = vec![false; ix.symbols.len()];
    for i in 0..ix.occurrences() {
        if ix.occ_roles[i] & scip::ROLE_DEFINITION != 0 {
            defined[ix.occ_sym[i] as usize] = true;
        }
    }
    let (pkg, pkg_keys, pkg_link) = package_links(ix, &defined, rows);
    let mut pkg_used = vec![false; pkg_keys.len()];
    let doc_file: Vec<u32> = ix.doc_path.iter().map(|p| rows.file(files, &p.replace('\\', "/"), "rust")).collect();
    // Per document, the definitions with an enclosing range, for `within`.
    let mut encl: Vec<Vec<(i32, i32, u32)>> = vec![Vec::new(); ix.doc_path.len()];
    for i in 0..ix.occurrences() {
        if ix.occ_encl_start[i] != scip::NONE {
            let s = ix.occ_sym[i];
            if parsed[s as usize].as_ref().is_some_and(|p| matches!(p.suffix, '(' | '#') && !p.is_impl_block) {
                encl[ix.occ_doc[i] as usize].push((ix.occ_encl_start[i], ix.occ_encl_end[i], s));
            }
        }
    }
    let mut seen_impl: HashSet<(u32, u32, String)> = HashSet::new();
    for i in 0..ix.occurrences() {
        let s = ix.occ_sym[i] as usize;
        let Some(p) = &parsed[s] else { continue };
        let doc = ix.occ_doc[i] as usize;
        let file = doc_file[doc];
        let line = ix.occ_line[i].max(0) as u32 + 1;
        let col = ix.occ_col[i].max(0) as u32;
        if p.is_impl_block {
            let Some((ty, tr)) = p.imp else { continue };
            let edge = match tr {
                Some(t) => format!("{t} for {ty}"),
                None => ty.to_string(),
            };
            if !seen_impl.insert((file, line, edge.clone())) {
                continue;
            }
            for name in [Some(ty), tr].into_iter().flatten() {
                rows.occ.push(Occ { name: name.to_string(), sect: SECT_IMPL, file, line, col, kind: "impl".into(), container: edge.clone(), within: String::new() });
                rows.stats.impls += 1;
            }
            continue;
        }
        let roles = ix.occ_roles[i];
        if roles & scip::ROLE_DEFINITION != 0 {
            let info = ix.info[s];
            let mut kind = scip::kind_word(info.kind).unwrap_or_else(|| suffix_kind(p.suffix)).to_string();
            if let Some((_, Some(tr))) = p.imp {
                kind = format!("{kind} ({tr})");
            }
            rows.occ.push(Occ { name: p.name.to_string(), sect: SECT_DEF, file, line, col, kind, container: p.container.clone(), within: String::new() });
            rows.stats.scip_defs += 1;
            continue;
        }
        if !defined[s] && !external_ref(rows, pkg[s], &pkg_keys, &pkg_link, &mut pkg_used) {
            continue;
        }
        let l0 = ix.occ_line[i];
        let within = encl[doc]
            .iter()
            .filter(|(a, b, sym)| *a <= l0 && l0 <= *b && *sym as usize != s)
            .max_by_key(|(a, b, _)| (*a, -*b))
            .and_then(|(_, _, sym)| parsed[*sym as usize].as_ref())
            .map(|q| if q.container.is_empty() { q.name.to_string() } else { format!("{}::{}", q.container, q.name) })
            .unwrap_or_default();
        let kind = if roles & scip::ROLE_IMPORT != 0 {
            "use"
        } else if roles & scip::ROLE_WRITE != 0 {
            "write"
        } else {
            ""
        };
        rows.occ.push(Occ { name: p.name.to_string(), sect: SECT_REF, file, line, col, kind: kind.into(), container: p.container.clone(), within });
        rows.stats.scip_refs += 1;
    }
}

fn suffix_kind(c: char) -> &'static str {
    scip::suffix_word(&scip::Descriptor { name: "", suffix: c })
}

/// The language word `code_files.lang` stores for a SCIP document: from
/// its `language` field, else from its extension; `other` when neither is
/// known.
///
/// # Arguments
///
/// * `doc_lang` - the document's `language` (any case; often empty).
/// * `path` - its path.
pub fn lang_of(doc_lang: &str, path: &str) -> &'static str {
    let named = match doc_lang.to_ascii_lowercase().as_str() {
        "go" => "go",
        "typescript" | "typescriptreact" => "typescript",
        "javascript" | "javascriptreact" => "javascript",
        "python" => "python",
        "java" => "java",
        "kotlin" => "kotlin",
        "scala" => "scala",
        "csharp" | "c#" => "csharp",
        "rust" => "rust",
        "c" => "c",
        "cpp" | "c++" => "cpp",
        "ruby" => "ruby",
        "php" => "php",
        "dart" => "dart",
        "swift" => "swift",
        _ => "",
    };
    if !named.is_empty() {
        return named;
    }
    let ext = path.rsplit_once('.').map_or("", |(_, e)| e).to_ascii_lowercase();
    match ext.as_str() {
        "go" => "go",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" | "pyi" => "python",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "scala" | "sc" => "scala",
        "cs" => "csharp",
        "rs" => "rust",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "rb" => "ruby",
        "php" => "php",
        "dart" => "dart",
        "swift" => "swift",
        _ => "other",
    }
}

/// Languages whose blocks are indentation (a row with
/// [`crate::langs::ScanRow::indent`]): end lines come from the scanner's
/// indentation blocks, else SCIP's enclosing ranges, never from the brace
/// matcher.
fn indented(lang: &str) -> bool {
    crate::langs::by_name(lang).is_some_and(|l| l.scanner.indent)
}

/// The kind of an impl edge from a SCIP relationship (Rust's impl blocks
/// are `impl`): it sits on its definition's line, so it is no item of its
/// own in [`file_items`].
pub const IMPLEMENTS: &str = "implements";

/// Constructor spellings: TypeScript, Java, C# (and its static
/// constructor), Python.
pub const CTORS: &[&str] = &["<constructor>", "<init>", ".ctor", ".cctor", "__init__"];

/// Name, container and shape of a symbol from any SCIP indexer.
struct Generic<'a> {
    /// The name stored: the last descriptor, or the type's name for a
    /// constructor.
    name: &'a str,
    /// The last descriptor's suffix.
    suffix: char,
    /// The nearest enclosing type, else the nearest callable or term, else
    /// the last namespace segment.
    container: String,
    /// True when the parent descriptor is a type (a member).
    member: bool,
    /// True for a constructor.
    ctor: bool,
}

impl Generic<'_> {
    /// `Container::name`, or the name alone.
    fn qualified(&self) -> String {
        if self.container.is_empty() {
            self.name.to_string()
        } else {
            format!("{}::{}", self.container, self.name)
        }
    }
}

/// The last segment of a namespace descriptor: `gin` for Go's
/// `github.com/gin-gonic/gin`, `conftest` for Python's
/// `examples.tests.conftest`, `standard-schema` for TypeScript's
/// `standard-schema.ts`.
fn namespace_name<'a>(name: &'a str, scheme: &str) -> &'a str {
    let last = name.rsplit('/').next().unwrap_or(name);
    if scheme == "scip-python" {
        return last.rsplit('.').next().unwrap_or(last);
    }
    for ext in [".d.ts", ".tsx", ".ts", ".mts", ".cts", ".jsx", ".js", ".mjs", ".cjs", ".py", ".go", ".java", ".kt", ".cs", ".scala"] {
        if let Some(stem) = last.strip_suffix(ext).filter(|s| !s.is_empty()) {
            return stem;
        }
    }
    last
}

/// The member name of a C# explicit interface implementation's
/// descriptor (`IEnumerable<Serilog.Core.ILogEventEnricher>.GetEnumerator`:
/// `GetEnumerator`); any other name as it is.
fn explicit_member(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((iface, member))
            if !iface.is_empty()
                && !member.is_empty()
                && member.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !iface.starts_with('.') =>
        {
            member
        }
        _ => name,
    }
}

/// Parses a symbol of any indexer: `None` for what is not stored as a
/// definition or a reference target (parameters, type parameters, `:` meta
/// such as TypeScript's object-literal members, namespaces and file
/// modules).
fn parse_generic(sym: &str) -> Option<Generic<'_>> {
    let d = scip::descriptors(sym);
    let last = *d.last()?;
    if matches!(last.suffix, '[' | ')' | ':' | '/') || last.name.is_empty() {
        return None;
    }
    let scheme = sym.split(' ').next().unwrap_or("");
    let parents = &d[..d.len() - 1];
    let is_ctor = last.suffix == '(' && CTORS.contains(&last.name);
    let owner = parents.last().filter(|x| x.suffix == '#');
    // A constructor is stored under its type's name, in its type.
    // scip-typescript names an accessor `<get>name` or `<set>name`: it is
    // stored under the name it is used by.
    // A C# explicit interface member is named with its interface
    // (`System.Collections.IEnumerable.GetEnumerator`): it is stored under
    // the name it is called by.
    let (name, ctor) = match owner {
        Some(t) if is_ctor => (t.name, true),
        _ => {
            let n =
                last.name.strip_prefix("<get>").or_else(|| last.name.strip_prefix("<set>")).filter(|n| !n.is_empty()).unwrap_or(last.name);
            (explicit_member(n), false)
        }
    };
    let container = if let Some(t) = parents.iter().rev().find(|x| x.suffix == '#') {
        t.name.to_string()
    } else if let Some(c) = parents.iter().rev().find(|x| matches!(x.suffix, '(' | '.')) {
        c.name.to_string()
    } else {
        parents.iter().rev().find(|x| x.suffix == '/').map(|x| namespace_name(x.name, scheme).to_string()).unwrap_or_default()
    };
    Some(Generic { name, suffix: last.suffix, container, member: owner.is_some(), ctor })
}

/// The kind word of a definition when the indexer gave none: the nearest
/// keyword before the name on its line, else its shape (a member term
/// followed by an accessor block, C#'s `{ get; }`, is a property).
///
/// # Arguments
///
/// * `g` - the parsed symbol.
/// * `before` - the definition's line up to the name.
/// * `after` - the rest of the line from the name.
fn fallback_kind(g: &Generic<'_>, before: &str, after: &str) -> &'static str {
    if g.ctor {
        return "ctor";
    }
    for w in before.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).rev() {
        return match w {
            "class" | "record" => "class",
            "interface" | "protocol" => "interface",
            "struct" => "struct",
            "enum" => "enum",
            "trait" => "trait",
            "def" | "func" | "function" | "fn" | "fun" if g.member => "method",
            "def" | "func" | "function" | "fn" | "fun" => "fn",
            "const" => "const",
            "var" | "let" | "val" => "var",
            "type" => "type",
            "event" => "event",
            _ => continue,
        };
    }
    match g.suffix {
        '#' => "type",
        '(' if g.member => "method",
        '(' => "fn",
        '!' => "macro",
        '.' if g.member && ["{ get", "{get", "{ set", "{set", "{ init"].iter().any(|a| after.contains(a)) => "property",
        // An expression-bodied property (`int Count => n;`).
        '.' if g.member
            && after.split_once(|c: char| !(c.is_alphanumeric() || c == '_')).is_some_and(|(_, r)| r.trim_start().starts_with("=>")) =>
        {
            "property"
        }
        _ if g.member => "field",
        _ => "var",
    }
}

/// Builds rows from a SCIP index of any indexer but rust-analyzer (whose
/// symbols [`rust_rows`] reads): scip-go, scip-typescript, scip-python,
/// scip-java, scip-dotnet and the rest share one symbol grammar.
///
/// * Definitions: every definition occurrence of a symbol
///   [`parse_generic`] keeps. Kind from `SymbolInformation.kind`
///   ([`scip::generic_kind_word`]), else from the line's keyword and the
///   symbol's shape ([`fallback_kind`]); a constructor is stored under its
///   type's name with kind `ctor`.
/// * End lines: the brace matcher where it finds a body that closes (brace
///   languages), else SCIP's enclosing range, else the line itself; kept in
///   [`Rows::ends`].
/// * References: every other occurrence of a symbol defined in the index;
///   `within` is the innermost type or callable whose range (the end lines
///   above) holds it, so callers do not depend on the indexer writing
///   enclosing ranges.
/// * Impl edges: each `Relationship.is_implementation` from a symbol
///   defined here, at its definition, under both names, of kind
///   [`IMPLEMENTS`]; the container is `Base for Impl` (members qualified:
///   `Shape::Area for Square::Area`).
///
/// # Arguments
///
/// * `ix` - the decoded index.
/// * `rows` - where rows go.
/// * `files` - the file map, shared with the root's other rows.
/// * `text_of` - a document's text by its path below the root (`None` when
///   unreadable: its definitions keep SCIP's ranges only).
pub fn scip_rows(ix: &Index<'_>, rows: &mut Rows, files: &mut HashMap<String, u32>, text_of: &dyn Fn(&str) -> Option<String>) {
    scip_rows_in(ix, "", &HashSet::new(), rows, files, text_of);
}

/// [`scip_rows`] for an index written in a unit's folder: its document
/// paths are relative to `dir` (a folder below the root, `/`-separated;
/// empty for the root) and are stored, and read through `text_of`,
/// relative to the root. Documents in `skip` (already read from another
/// unit's index: a C# project another unit's project references) and
/// documents outside the root give no rows.
pub fn scip_rows_in(
    ix: &Index<'_>,
    dir: &str,
    skip: &HashSet<String>,
    rows: &mut Rows,
    files: &mut HashMap<String, u32>,
    text_of: &dyn Fn(&str) -> Option<String>,
) {
    let parsed: Vec<Option<Generic<'_>>> = ix.symbols.iter().map(|s| parse_generic(s)).collect();
    let mut defined = vec![false; ix.symbols.len()];
    // The first definition occurrence of each symbol, for impl edges.
    let mut first_def: Vec<Option<usize>> = vec![None; ix.symbols.len()];
    let mut has_def = vec![false; ix.doc_path.len()];
    for i in 0..ix.occurrences() {
        if ix.occ_roles[i] & scip::ROLE_DEFINITION != 0 {
            let s = ix.occ_sym[i] as usize;
            defined[s] = true;
            first_def[s].get_or_insert(i);
            has_def[ix.occ_doc[i] as usize] |= parsed[s].is_some();
        }
    }
    let (pkg, pkg_keys, pkg_link) = package_links(ix, &defined, rows);
    let mut pkg_used = vec![false; pkg_keys.len()];
    let paths: Vec<String> = ix.doc_path.iter().map(|p| rebase(dir, p)).collect();
    let keep: Vec<bool> = paths.iter().map(|p| !skip.contains(p) && !p.starts_with("../")).collect();
    let langs: Vec<&'static str> = paths.iter().zip(&ix.doc_lang).map(|(p, l)| lang_of(l, p)).collect();
    let doc_file: Vec<u32> =
        paths.iter().zip(&langs).zip(&keep).map(|((p, l), &k)| if k { rows.file(files, p, l) } else { u32::MAX }).collect();
    let texts: Vec<Option<String>> =
        paths.iter().zip(&has_def).zip(&keep).map(|((p, &h), &k)| if h && k { text_of(p) } else { None }).collect();
    let lines: Vec<Vec<&str>> = texts.iter().map(|t| t.as_deref().map(|t| t.lines().collect()).unwrap_or_default()).collect();
    // Per document, each type or callable with its range: (start, end,
    // symbol), 1-based.
    let mut scopes: Vec<Vec<(u32, u32, u32)>> = vec![Vec::new(); ix.doc_path.len()];
    // Per document, the scanner's end line by definition line (scanned
    // when a definition first needs it).
    let mut scan_ends: Vec<Option<HashMap<u32, u32>>> = vec![None; ix.doc_path.len()];
    for i in 0..ix.occurrences() {
        if ix.occ_roles[i] & scip::ROLE_DEFINITION == 0 {
            continue;
        }
        let s = ix.occ_sym[i] as usize;
        let Some(g) = &parsed[s] else { continue };
        let doc = ix.occ_doc[i] as usize;
        if !keep[doc] {
            continue;
        }
        let file = doc_file[doc];
        let line = ix.occ_line[i].max(0) as u32 + 1;
        let col = ix.occ_col[i].max(0) as u32;
        let kind = match scip::generic_kind_word(ix.info[s].kind) {
            Some(_) if g.ctor => "ctor",
            Some(k) => k,
            None => {
                let text_line = lines[doc].get(line as usize - 1).copied().unwrap_or("");
                let mut at = (col as usize).min(text_line.len());
                while !text_line.is_char_boundary(at) {
                    at -= 1;
                }
                fallback_kind(g, &text_line[..at], &text_line[at..])
            }
        };
        let brace = if indented(langs[doc]) || lines[doc].len() < line as usize { None } else { crate::digest::brace_body_end(&lines[doc], line as usize - 1).map(|e| e as u32) };
        let scip_end = (ix.occ_encl_end[i] != scip::NONE).then(|| ix.occ_encl_end[i].max(0) as u32 + 1);
        // The scanner's end for a definition on the line: an indentation
        // language's first choice, and a brace language's when it has
        // neither a brace body nor an enclosing range (scip-dotnet writes
        // none; the brace matcher stops at a base list's or a `where`
        // clause's comma).
        let mut scanned = || {
            let lang = crate::langs::of_path(&paths[doc])?;
            let ends = scan_ends[doc].get_or_insert_with(|| {
                let mut m: HashMap<u32, u32> = HashMap::new();
                for d in crate::scanner::of(lang).scan_file(&paths[doc], texts[doc].as_deref().unwrap_or("")) {
                    let e = m.entry(d.line).or_insert(d.end);
                    *e = (*e).max(d.end);
                }
                m
            });
            ends.get(&line).copied()
        };
        let end = if indented(langs[doc]) { scanned().or(scip_end) } else { brace.or(scip_end).or_else(scanned) };
        let end = end.unwrap_or(line).max(line);
        rows.ends.insert((file, line), end);
        if matches!(g.suffix, '#' | '(') {
            scopes[doc].push((line, end, s as u32));
        }
        rows.occ.push(Occ { name: g.name.to_string(), sect: SECT_DEF, file, line, col, kind: kind.into(), container: g.container.clone(), within: String::new() });
        rows.stats.scip_defs += 1;
    }
    for i in 0..ix.occurrences() {
        let roles = ix.occ_roles[i];
        if roles & scip::ROLE_DEFINITION != 0 {
            continue;
        }
        let s = ix.occ_sym[i] as usize;
        let Some(g) = &parsed[s] else { continue };
        if !defined[s] && !external_ref(rows, pkg[s], &pkg_keys, &pkg_link, &mut pkg_used) {
            continue;
        }
        let doc = ix.occ_doc[i] as usize;
        if !keep[doc] {
            continue;
        }
        let line = ix.occ_line[i].max(0) as u32 + 1;
        let within = scopes[doc]
            .iter()
            .filter(|(a, b, sym)| *a <= line && line <= *b && *sym as usize != s)
            .max_by_key(|(a, b, _)| (*a, u32::MAX - *b))
            .and_then(|(_, _, sym)| parsed[*sym as usize].as_ref())
            .map(Generic::qualified)
            .unwrap_or_default();
        let kind = if roles & scip::ROLE_IMPORT != 0 {
            "use"
        } else if roles & scip::ROLE_WRITE != 0 {
            "write"
        } else {
            ""
        };
        rows.occ.push(Occ { name: g.name.to_string(), sect: SECT_REF, file: doc_file[doc], line, col: ix.occ_col[i].max(0) as u32, kind: kind.into(), container: g.container.clone(), within });
        rows.stats.scip_refs += 1;
    }
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    for r in 0..ix.rel_from.len() {
        if ix.rel_flags[r] & scip::REL_IMPLEMENTATION == 0 {
            continue;
        }
        let (a, b) = (ix.rel_from[r], ix.rel_to[r]);
        let (Some(at), Some(ga), Some(gb)) = (first_def[a as usize], &parsed[a as usize], &parsed[b as usize]) else { continue };
        if !keep[ix.occ_doc[at] as usize] || !seen.insert((a, b)) {
            continue;
        }
        let q = |g: &Generic<'_>| if g.suffix == '#' { g.name.to_string() } else { g.qualified() };
        let edge = format!("{} for {}", q(gb), q(ga));
        let file = doc_file[ix.occ_doc[at] as usize];
        let line = ix.occ_line[at].max(0) as u32 + 1;
        let col = ix.occ_col[at].max(0) as u32;
        // A member implementing its namesake (`Area` for `Shape::Area`) is
        // one row.
        let names = if ga.name == gb.name { vec![ga.name] } else { vec![ga.name, gb.name] };
        for name in names {
            rows.occ.push(Occ { name: name.to_string(), sect: SECT_IMPL, file, line, col, kind: IMPLEMENTS.into(), container: edge.clone(), within: String::new() });
            rows.stats.impls += 1;
        }
    }
}

/// A SCIP document path relative to the root: `/`-separated, below the
/// unit folder `dir` (empty: the root).
pub fn rebase(dir: &str, path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.trim_start_matches("./");
    let joined = if dir.is_empty() { p.to_string() } else { format!("{dir}/{p}") };
    if !joined.split('/').any(|c| c == ".." || c == ".") {
        return joined;
    }
    // A document beside the unit (`../lib/a.cs`): the path below the root,
    // or still starting with `../` when it is outside the root.
    let mut out: Vec<&str> = Vec::new();
    for c in joined.split('/') {
        match c {
            "." | "" => {}
            ".." if out.last().is_some_and(|l| *l != "..") => {
                out.pop();
            }
            _ => out.push(c),
        }
    }
    out.join("/")
}

/// True when an index came from rust-analyzer (or names no tool and holds
/// only `.rs` documents, as hand-built test indexes do): its symbols are
/// read by [`rust_rows`], every other index's by [`scip_rows`].
pub fn is_rust_index(ix: &Index<'_>) -> bool {
    match ix.tool.0 {
        "rust-analyzer" => true,
        "" => ix.doc_path.iter().all(|p| p.ends_with(".rs")),
        _ => false,
    }
}

/// Rust item keywords the line scan recognises.
const RUST_ITEMS: &[&str] = &["fn", "struct", "enum", "trait", "type", "union", "mod", "const", "static", "macro_rules!"];

/// For each line of Rust source, whether it starts in code (not inside a
/// block comment or a string literal).
///
/// # Arguments
///
/// * `text` - the file's text.
pub fn rust_code_lines(text: &str) -> Vec<bool> {
    let b = text.as_bytes();
    let mut out = vec![true];
    let mut i = 0;
    // 0 code, >0 block comment depth, -1 string, -2 raw string (with hashes).
    let mut comment = 0usize;
    let mut in_str = false;
    let mut raw_hashes: Option<usize> = None;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            out.push(comment == 0 && !in_str && raw_hashes.is_none());
            i += 1;
            continue;
        }
        if comment > 0 {
            if c == b'/' && b.get(i + 1) == Some(&b'*') {
                comment += 1;
                i += 2;
            } else if c == b'*' && b.get(i + 1) == Some(&b'/') {
                comment -= 1;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(h) = raw_hashes {
            if c == b'"' && b.len() >= i + 1 + h && b[i + 1..i + 1 + h].iter().all(|&x| x == b'#') {
                raw_hashes = None;
                i += 1 + h;
            } else {
                i += 1;
            }
            continue;
        }
        if in_str {
            if c == b'\\' {
                i += 2;
                if b.get(i - 1) == Some(&b'\n') {
                    out.push(false);
                }
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                comment = 1;
                i += 2;
            }
            b'"' => {
                in_str = true;
                i += 1;
            }
            b'r' | b'b' if i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') => {
                // r"..", r#".."#, b"..", br#".."#, b'.'
                let mut j = i + 1;
                if c == b'b' && b.get(j) == Some(&b'r') {
                    j += 1;
                }
                let raw = c == b'r' || b.get(i + 1) == Some(&b'r');
                let mut h = 0;
                while raw && b.get(j) == Some(&b'#') {
                    h += 1;
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    if raw {
                        raw_hashes = Some(h);
                    } else {
                        in_str = true;
                    }
                    i = j + 1;
                } else {
                    i += 1;
                }
            }
            b'\'' => {
                // A char literal ('x', '\n', '\u{..}') or a lifetime ('a).
                if b.get(i + 1) == Some(&b'\\') {
                    let end = text[i + 2..].find('\'').map_or(b.len(), |p| i + 2 + p + 1);
                    i = end;
                } else {
                    let ch = text[i + 1..].chars().next();
                    let w = ch.map_or(1, char::len_utf8);
                    if b.get(i + 1 + w) == Some(&b'\'') {
                        i += 2 + w;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// The Rust item definition a line starts with, if any: `(keyword, name,
/// column of the name)`. Visibility and qualifiers (`pub(crate)`, `const`,
/// `async`, `unsafe`, `extern "C"`, `default`) are skipped.
///
/// # Arguments
///
/// * `line` - one source line.
pub fn rust_item(line: &str) -> Option<(&'static str, &str, usize)> {
    let mut rest = line.trim_start();
    let mut at = line.len() - rest.len();
    let skip = |rest: &mut &str, at: &mut usize, n: usize| {
        let before = rest.len();
        *rest = rest[n..].trim_start();
        *at += before - rest.len();
    };
    if rest.starts_with("pub") {
        let after = &rest[3..];
        if after.starts_with('(') {
            let close = after.find(')')?;
            skip(&mut rest, &mut at, 3 + close + 1);
        } else if after.starts_with(char::is_whitespace) {
            skip(&mut rest, &mut at, 3);
        }
    }
    loop {
        let w = rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '!')).next().unwrap_or("");
        if let Some(&kw) = RUST_ITEMS.iter().find(|k| **k == w) {
            // `const fn`, `const unsafe fn`: const is a qualifier when an
            // item keyword follows it.
            if kw == "const" || kw == "static" {
                let after = rest[w.len()..].trim_start();
                let next = after.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).next().unwrap_or("");
                if kw == "const" && matches!(next, "fn" | "unsafe" | "async" | "extern") {
                    skip(&mut rest, &mut at, w.len());
                    continue;
                }
            }
            skip(&mut rest, &mut at, w.len());
            if kw == "static" && rest.starts_with("mut ") {
                skip(&mut rest, &mut at, 3);
            }
            if let Some(r) = rest.strip_prefix("r#") {
                at += 2;
                rest = r;
            }
            let name_len = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
            let name = &rest[..name_len];
            if name.is_empty() || name == "_" || name.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            return Some((kw, name, at));
        }
        match w {
            "async" | "unsafe" | "safe" | "default" => skip(&mut rest, &mut at, w.len()),
            "extern" => {
                skip(&mut rest, &mut at, w.len());
                if rest.starts_with('"') {
                    let close = rest[1..].find('"')?;
                    skip(&mut rest, &mut at, close + 2);
                }
            }
            _ => return None,
        }
    }
}

/// Every item definition a line scan finds in Rust source: lines that
/// start in code and begin with an item keyword.
///
/// # Arguments
///
/// * `text` - the file's text.
///
/// # Returns
///
/// `(keyword, name, line 1-based, column)` for each.
pub fn scan_rust_defs(text: &str) -> Vec<(&'static str, String, u32, u32)> {
    let code = rust_code_lines(text);
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if !code.get(i).copied().unwrap_or(true) {
            continue;
        }
        if let Some((kw, name, col)) = rust_item(line) {
            out.push((kw, name.to_string(), i as u32 + 1, col as u32));
        }
    }
    out
}

/// The impl block a line starts, if any: `(self type, trait, column of
/// `impl`)`, each type shortened as SCIP's impl rows are (`Result<T, E>` is
/// `Result`). `unsafe impl`, `default impl` and generic parameters are
/// handled; a header whose generics do not close on the line is not.
///
/// # Arguments
///
/// * `line` - one source line.
pub fn rust_impl(line: &str) -> Option<(String, Option<String>, usize)> {
    let mut rest = line.trim_start();
    let at = line.len() - rest.len();
    let mut col = at;
    for q in ["unsafe", "default"] {
        if let Some(r) = rest.strip_prefix(q).filter(|r| r.starts_with(char::is_whitespace)) {
            let r2 = r.trim_start();
            col += rest.len() - r2.len();
            rest = r2;
        }
    }
    let after = rest.strip_prefix("impl")?;
    if !(after.starts_with('<') || after.starts_with(char::is_whitespace)) {
        return None;
    }
    let mut head = after.trim_start();
    if head.starts_with('<') {
        let close = angle_close(head)?;
        head = head[close + 1..].trim_start();
    }
    // The header runs to the body's brace or a `where` clause.
    let end = [head.find('{'), find_word(head, "where")].into_iter().flatten().min().unwrap_or(head.len());
    let head = head[..end].trim().trim_start_matches('!');
    if head.is_empty() {
        return None;
    }
    let short = |t: &str| -> Option<String> {
        let mut t = t.trim();
        while let Some(r) = t.strip_prefix('&') {
            t = r.trim_start();
            if t.starts_with('\'') {
                t = t.split_once(char::is_whitespace).map_or("", |x| x.1).trim_start();
            }
        }
        let n = type_name(t);
        (!n.is_empty()).then(|| n.to_string())
    };
    match top_level_for(head) {
        Some(k) => Some((short(&head[k + 5..])?, Some(short(&head[..k])?), col)),
        None => Some((short(head)?, None, col)),
    }
}

/// The byte index of the `>` closing the `<` at the start of `s`.
fn angle_close(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' if i > 0 && s.as_bytes()[i - 1] == b'-' => {}
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The byte index of `word` standing alone in `s`.
fn find_word(s: &str, word: &str) -> Option<usize> {
    let b = s.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    s.match_indices(word).map(|(i, _)| i).find(|&i| (i == 0 || !ident(b[i - 1])) && b.get(i + word.len()).is_none_or(|&c| !ident(c)))
}

/// The byte index of the ` for ` separating trait and self type, outside
/// generic arguments (so `Fn(&'a T) for X` and `Foo<for<'a> ..>` are safe).
fn top_level_for(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    for i in 0..b.len() {
        match b[i] {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if i > 0 && b[i - 1] == b'-' => {}
            b'>' | b')' | b']' => depth -= 1,
            b' ' if depth == 0 && s[i..].starts_with(" for ") => return Some(i),
            _ => {}
        }
    }
    None
}

/// Every impl block a line scan finds in Rust source.
///
/// # Returns
///
/// `(self type, trait, line 1-based, column)` for each.
pub fn scan_rust_impls(text: &str) -> Vec<(String, Option<String>, u32, u32)> {
    let code = rust_code_lines(text);
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if !code.get(i).copied().unwrap_or(true) {
            continue;
        }
        if let Some((ty, tr, col)) = rust_impl(line) {
            out.push((ty, tr, i as u32 + 1, col as u32));
        }
    }
    out
}

/// Adds what the line scan finds that SCIP does not have at the same line
/// (or all of it when there was no SCIP index): item definitions, and impl
/// blocks as impl edge rows (`Trait for Type`, under both names). A scanned
/// definition inside an impl or trait block gets that block's type or trait
/// as its container, so `Store::open` finds a method without SCIP.
///
/// # Arguments
///
/// * `texts` - each Rust file's path and text.
/// * `rows` - the rows so far (SCIP ones included).
/// * `files` - the shared file map.
pub fn scan_rows(texts: &[(String, String)], rows: &mut Rows, files: &mut HashMap<String, u32>) {
    let have: HashSet<(u32, u32, String)> = rows.occ.iter().filter(|o| o.sect == SECT_DEF).map(|o| (o.file, o.line, o.name.clone())).collect();
    let have_impl: HashSet<(u32, u32)> = rows.occ.iter().filter(|o| o.sect == SECT_IMPL).map(|o| (o.file, o.line)).collect();
    for (path, text) in texts {
        let file = rows.file(files, path, "rust");
        let defs = scan_rust_defs(text);
        let impls = scan_rust_impls(text);
        // Blocks that name the items inside them: (start, end, container).
        let lines: Vec<&str> = if impls.is_empty() && !defs.iter().any(|d| d.0 == "trait") { Vec::new() } else { text.lines().collect() };
        let mut blocks: Vec<(u32, u32, String)> = Vec::new();
        for (ty, tr, line, col) in &impls {
            blocks.push((*line, crate::digest::item_end(&lines, *line as usize - 1) as u32, ty.clone()));
            if have_impl.contains(&(file, *line)) {
                continue;
            }
            let edge = match tr {
                Some(t) => format!("{t} for {ty}"),
                None => ty.clone(),
            };
            for name in [Some(ty), tr.as_ref()].into_iter().flatten() {
                rows.occ.push(Occ { name: name.clone(), sect: SECT_IMPL, file, line: *line, col: *col, kind: "impl (scan)".into(), container: edge.clone(), within: String::new() });
                rows.stats.impls += 1;
            }
        }
        for (_, name, line, _) in defs.iter().filter(|d| d.0 == "trait") {
            blocks.push((*line, crate::digest::item_end(&lines, *line as usize - 1) as u32, name.clone()));
        }
        for (kw, name, line, col) in defs {
            if have.contains(&(file, line, name.clone())) {
                continue;
            }
            let container = blocks
                .iter()
                .filter(|(a, b, _)| *a < line && line <= *b)
                .min_by_key(|(a, b, _)| (b - a, u32::MAX - a))
                .map(|x| x.2.clone())
                .unwrap_or_default();
            let kind = if kw == "macro_rules!" { "macro (scan)".to_string() } else { format!("{kw} (scan)") };
            rows.occ.push(Occ { name, sect: SECT_DEF, file, line, col, kind, container, within: String::new() });
            rows.stats.scan_defs += 1;
        }
    }
}

/// Builds WGSL rows: declarations, and references to names declared in
/// these files.
///
/// # Arguments
///
/// * `texts` - each WGSL file's path and text.
/// * `rows` - where rows go.
/// * `files` - the shared file map.
pub fn wgsl_rows(texts: &[(String, String)], rows: &mut Rows, files: &mut HashMap<String, u32>) {
    let scans: Vec<(u32, wgsl::Scan)> = texts.iter().map(|(p, t)| (rows.file(files, p, "wgsl"), wgsl::scan(t))).collect();
    let declared: HashSet<&str> = scans.iter().flat_map(|(_, s)| s.decls.iter().map(|d| d.name.as_str())).collect();
    for (file, s) in &scans {
        let module = s.module.clone().unwrap_or_default();
        for d in &s.decls {
            let kind = if d.detail.is_empty() { d.kind.to_string() } else { format!("{} {}", d.kind, d.detail) };
            rows.occ.push(Occ { name: d.name.clone(), sect: SECT_DEF, file: *file, line: d.line, col: d.col, kind, container: module.clone(), within: String::new() });
            rows.stats.wgsl_defs += 1;
        }
        for r in &s.refs {
            if !declared.contains(r.name.as_str()) {
                continue;
            }
            let kind = if r.import { "import" } else { "" };
            rows.occ.push(Occ {
                name: r.name.clone(),
                sect: SECT_REF,
                file: *file,
                line: r.line,
                col: r.col,
                kind: kind.into(),
                container: String::new(),
                within: r.within.clone().unwrap_or_default(),
            });
            rows.stats.wgsl_refs += 1;
        }
    }
}

/// Builds scanner rows ([`crate::scanner`]) for files of the languages of
/// [`crate::langs`]: definitions with their kinds marked `(scan)`,
/// containers and end lines (kept in [`Rows::ends`]), no references.
/// Every file is registered, so later rows (rpc call sites) find it under
/// its language.
///
/// # Arguments
///
/// * `texts` - each file's path below the root and text; a file of no
///   row's language is skipped.
/// * `rows` - where rows go.
/// * `files` - the shared file map.
pub fn lang_rows(texts: &[(String, String)], rows: &mut Rows, files: &mut HashMap<String, u32>) {
    for (path, text) in texts {
        let Some(lang) = crate::langs::of_path(path) else { continue };
        let file = rows.file(files, path, lang.name);
        for d in crate::scanner::of(lang).scan_file(path, text) {
            // Several definitions on a line share its end: the longest.
            let e = rows.ends.entry((file, d.line)).or_insert(d.end);
            *e = (*e).max(d.end);
            rows.occ.push(Occ {
                name: d.name,
                sect: SECT_DEF,
                file,
                line: d.line,
                col: d.col,
                kind: format!("{} (scan)", d.kind),
                container: d.container,
                within: String::new(),
            });
            rows.stats.lang_defs += 1;
        }
    }
}

/// Adds, for files of a row that fills ([`crate::langs::Lang::fills`]) and
/// that a SCIP index covers, the scanner's definitions on lines where the
/// index has no definition, marked `(scan)` as [`lang_rows`] marks them.
///
/// # Arguments
///
/// * `texts` - the root's source files of the language table, by path.
/// * `covered` - the paths an index has a document for.
pub fn fill_rows(texts: &[(String, String)], covered: &HashSet<String>, rows: &mut Rows, files: &mut HashMap<String, u32>) {
    let mut have: Option<HashSet<(u32, u32)>> = None;
    for (path, text) in texts {
        let Some(lang) = crate::langs::of_path(path).filter(|l| l.fills && covered.contains(path)) else { continue };
        let have = have.get_or_insert_with(|| rows.occ.iter().filter(|o| o.sect == SECT_DEF).map(|o| (o.file, o.line)).collect());
        let file = rows.file(files, path, lang.name);
        for d in crate::scanner::of(lang).scan_file(path, text) {
            if have.contains(&(file, d.line)) {
                continue;
            }
            let e = rows.ends.entry((file, d.line)).or_insert(d.end);
            *e = (*e).max(d.end);
            rows.occ.push(Occ {
                name: d.name,
                sect: SECT_DEF,
                file,
                line: d.line,
                col: d.col,
                kind: format!("{} (scan)", d.kind),
                container: d.container,
                within: String::new(),
            });
            rows.stats.lang_defs += 1;
        }
    }
}

/// Reads a source file as text (lossy), or `None` when it is missing or
/// over [`MAX_FILE`].
pub fn read_source(root: &Path, rel: &str) -> Option<String> {
    let p = root.join(rel);
    let m = fs::metadata(&p).ok()?;
    if m.len() > MAX_FILE {
        return None;
    }
    let b = fs::read(&p).ok()?;
    Some(String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// What one root's write produced.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WriteStats {
    /// Files.
    pub files: usize,
    /// Definition rows.
    pub defs: usize,
    /// Reference rows.
    pub refs: usize,
    /// Impl rows.
    pub impls: usize,
    /// Source lines stored for snippets.
    pub lines: usize,
}

/// Replaces one root's rows in one transaction.
///
/// # Arguments
///
/// * `conn` - a connection (the indexer's own).
/// * `root` - the root's name.
/// * `dir` - the root's folder (kept so a file path maps back to its root).
/// * `rows` - its rows.
/// * `text_of` - each file's text, by index into `rows.files` (`None` for
///   an unreadable file: its rows keep no snippet).
/// * `fingerprint` - the walk fingerprint the rows were built from.
/// * `units` - the root's units (`code_units`); `code_roots.rust` gets
///   their [`units_note`].
/// * `elapsed_ms` - how long the indexing took.
///
/// # Errors
///
/// [`Error::Db`] from SQLite; nothing is changed then.
#[allow(clippy::too_many_arguments)] // one row of the index: a struct would only name the same fields
pub fn write_root(
    conn: &mut Connection,
    root: &str,
    dir: &Path,
    rows: &Rows,
    text_of: &dyn Fn(usize) -> Option<String>,
    fingerprint: &str,
    units: &[Unit],
    elapsed_ms: u64,
) -> Result<WriteStats> {
    ensure_schema(conn)?;
    // `begin` refuses a store a newer atlas upgraded since this one opened it.
    let tx = crate::store::begin(conn)?;
    tx.execute("DELETE FROM code_occ WHERE file IN (SELECT id FROM code_files WHERE root = ?1)", [root])?;
    tx.execute("DELETE FROM code_lines WHERE file IN (SELECT id FROM code_files WHERE root = ?1)", [root])?;
    tx.execute("DELETE FROM code_files WHERE root = ?1", [root])?;
    tx.execute("DELETE FROM code_counts WHERE root = ?1", [root])?;
    tx.execute("DELETE FROM code_units WHERE root = ?1", [root])?;
    tx.execute("DELETE FROM code_packages WHERE root = ?1", [root])?;
    {
        let mut st = tx.prepare("INSERT OR IGNORE INTO code_packages(root, package, defines) VALUES(?1, ?2, ?3)")?;
        for p in &rows.pkg_defs {
            st.execute(params![root, p, 1])?;
        }
        for p in rows.pkg_uses.iter().filter(|p| !rows.pkg_defs.contains(*p)) {
            st.execute(params![root, p, 0])?;
        }
    }
    // File ids order a name's rows within each section: `.proto` files
    // first (an rpc's definition), generated code last (its stubs), the
    // rest as walked.
    let mut ids = vec![0; rows.files.len()];
    {
        let rank = |p: &str| if p.ends_with(".proto") { 0 } else if crate::langs::is_generated(p) { 2 } else { 1 };
        let mut by_rank: Vec<usize> = (0..rows.files.len()).collect();
        by_rank.sort_by_key(|&i| rank(&rows.files[i].0));
        let mut st = tx.prepare("INSERT INTO code_files(root, path, lang) VALUES(?1, ?2, ?3)")?;
        for i in by_rank {
            let (p, lang) = &rows.files[i];
            st.execute(params![root, p, lang])?;
            ids[i] = tx.last_insert_rowid();
        }
    }
    // Rows sorted by key so the b-tree is written in order.
    let mut order: Vec<usize> = (0..rows.occ.len()).collect();
    order.sort_by(|&a, &b| {
        let (x, y) = (&rows.occ[a], &rows.occ[b]);
        (&x.name, x.sect, ids[x.file as usize], x.line, x.col).cmp(&(&y.name, y.sect, ids[y.file as usize], y.line, y.col))
    });
    let mut ws = WriteStats { files: rows.files.len(), ..WriteStats::default() };
    // A definition's or impl block's last line, by brace matching over its
    // file (the same matcher as the outlines), so `symbol` answers
    // `path:line-end`.
    let mut ends: HashMap<(u32, u32), u32> = HashMap::new();
    {
        let mut by_file: HashMap<u32, Vec<u32>> = HashMap::new();
        for o in rows.occ.iter().filter(|o| o.sect != SECT_REF && !rows.ends.contains_key(&(o.file, o.line))) {
            by_file.entry(o.file).or_default().push(o.line);
        }
        for (f, defs) in by_file {
            let Some(text) = text_of(f as usize) else { continue };
            let lines: Vec<&str> = text.lines().collect();
            for l in defs {
                if l >= 1 && (l as usize) <= lines.len() {
                    ends.insert((f, l), crate::digest::item_end(&lines, l as usize - 1) as u32);
                }
            }
        }
    }
    let mut counts: HashMap<&str, [i64; 3]> = HashMap::new();
    let mut need: Vec<HashSet<u32>> = vec![HashSet::new(); rows.files.len()];
    {
        let mut st = tx.prepare(
            "INSERT OR IGNORE INTO code_occ(name, sect, file, line, col, ord, kind, container, within, end_line) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        let mut prev: Option<(&str, i64, u32, u32, u32)> = None;
        let mut ord = 0;
        for &k in &order {
            let o = &rows.occ[k];
            let key = (o.name.as_str(), o.sect, o.file, o.line, o.col);
            ord = if prev == Some(key) { ord + 1 } else { 0 };
            prev = Some(key);
            let end = if o.sect != SECT_REF { rows.ends.get(&(o.file, o.line)).or_else(|| ends.get(&(o.file, o.line))).copied().unwrap_or(0) } else { 0 };
            st.execute(params![o.name, o.sect, ids[o.file as usize], o.line, o.col, ord, o.kind, o.container, o.within, end])?;
            counts.entry(o.name.as_str()).or_insert([0; 3])[o.sect as usize] += 1;
            need[o.file as usize].insert(o.line);
            match o.sect {
                SECT_DEF => ws.defs += 1,
                SECT_REF => ws.refs += 1,
                _ => ws.impls += 1,
            }
        }
    }
    {
        let mut st = tx.prepare("INSERT INTO code_counts(name, root, defs, refs, impls) VALUES(?1, ?2, ?3, ?4, ?5)")?;
        for (n, c) in &counts {
            st.execute(params![n, root, c[0], c[1], c[2]])?;
        }
    }
    {
        let mut st = tx.prepare("INSERT INTO code_lines(file, line, text) VALUES(?1, ?2, ?3)")?;
        for (fi, lines) in need.iter().enumerate() {
            if lines.is_empty() {
                continue;
            }
            let Some(text) = text_of(fi) else { continue };
            for (i, l) in text.lines().enumerate() {
                let n = i as u32 + 1;
                if !lines.contains(&n) {
                    continue;
                }
                let mut end = l.len().min(MAX_LINE);
                while !l.is_char_boundary(end) {
                    end -= 1;
                }
                st.execute(params![ids[fi], n, &l[..end]])?;
                ws.lines += 1;
            }
        }
    }
    let now = crate::store::utc(SystemTime::now());
    {
        let mut st = tx.prepare("INSERT OR REPLACE INTO code_units(root, dir, lang, source, fingerprint, indexed, elapsed_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
        for u in units {
            st.execute(params![root, u.dir, u.lang, u.source, fingerprint, now, elapsed_ms as i64])?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO code_roots(root, fingerprint, indexed, files, defs, refs, impls, rust, elapsed_ms, dir) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            root,
            fingerprint,
            now,
            ws.files as i64,
            ws.defs as i64,
            ws.refs as i64,
            ws.impls as i64,
            units_note(units),
            elapsed_ms as i64,
            dir.to_string_lossy().replace('\\', "/")
        ],
    )?;
    tx.commit()?;
    Ok(ws)
}

/// One row of a `symbol` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolHit {
    /// [`SECT_DEF`], [`SECT_REF`] or [`SECT_IMPL`].
    pub sect: i64,
    /// The code root.
    pub root: String,
    /// Path below the root.
    pub path: String,
    /// Line, 1-based.
    pub line: u32,
    /// Column, 0-based.
    pub col: u32,
    /// See [`Occ::kind`].
    pub kind: String,
    /// See [`Occ::container`].
    pub container: String,
    /// See [`Occ::within`].
    pub within: String,
    /// The source line, when stored.
    pub text: String,
    /// A definition's last line (0: unknown).
    pub end_line: u32,
}

/// A `symbol` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolAnswer {
    /// The name looked up (the last `::` segment of the query).
    pub name: String,
    /// Definitions, then references, then impl edges, each capped.
    pub hits: Vec<SymbolHit>,
    /// Totals: definitions, references, impl edges.
    pub totals: [i64; 3],
}

/// The limits of one `symbol` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Definitions.
    pub defs: usize,
    /// References.
    pub refs: usize,
    /// Impl edges.
    pub impls: usize,
}

/// The `symbol` statement for one set of limits. The limits are written in
/// as integer literals, not bound: with `LIMIT ?` in the arms SQLite 3.49
/// ran the statement about 5x slower (150 us against 28 us median on the
/// real index, 50 names), so each limit triple is its own cached statement.
fn symbol_sql(lim: Limits) -> String {
    format!(
        "
SELECT o.sect, f.root, f.path, o.line, o.col, o.kind, o.container, o.within, l.text, o.end_line
FROM (SELECT * FROM (SELECT * FROM code_occ WHERE name = ?1 AND sect = 0 AND (?2 IS NULL OR container = ?2) LIMIT {})
      UNION ALL SELECT * FROM (SELECT * FROM code_occ WHERE name = ?1 AND sect = 1 AND (?2 IS NULL OR container = ?2) LIMIT {})
      UNION ALL SELECT * FROM (SELECT * FROM code_occ WHERE name = ?1 AND sect = 2 LIMIT {})) o
JOIN code_files f ON f.id = o.file
LEFT JOIN code_lines l ON l.file = o.file AND l.line = o.line
UNION ALL
SELECT 9, '', '', SUM(defs), SUM(refs), '', '', '', SUM(impls), 0 FROM code_counts WHERE name = ?1",
        lim.defs, lim.refs, lim.impls
    )
}

/// Looks a symbol up: definitions, then references, then impl edges, in
/// one statement.
///
/// # Arguments
///
/// * `conn` - the store's connection.
/// * `query` - a name, or `Container::name` (only the last two segments
///   are used; the container filters definitions and references).
/// * `lim` - how many of each section.
///
/// # Errors
///
/// [`Error::Db`] from SQLite. A database the code index never wrote to
/// answers empty.
pub fn symbol(conn: &Connection, query: &str, lim: Limits) -> Result<SymbolAnswer> {
    let mut parts = query.trim().rsplit("::");
    let name = parts.next().unwrap_or("").trim().to_string();
    let container = parts.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let mut ans = SymbolAnswer { name: name.clone(), ..SymbolAnswer::default() };
    let mut st = match conn.prepare_cached(&symbol_sql(lim)) {
        Ok(st) => st,
        Err(e) if e.to_string().contains("no such table") => return Ok(ans),
        Err(e) => return Err(e.into()),
    };
    let mut rows = st.query(params![name, container])?;
    while let Some(r) = rows.next()? {
        let sect: i64 = r.get(0)?;
        if sect == 9 {
            // Counts row: SUMs, NULL when the name is unknown. With a
            // container filter the totals are the shown rows'.
            let d: Option<i64> = r.get(3)?;
            let rf: Option<i64> = r.get(4)?;
            let im: Option<i64> = r.get(8)?;
            ans.totals = [d.unwrap_or(0), rf.unwrap_or(0), im.unwrap_or(0)];
            continue;
        }
        ans.hits.push(SymbolHit {
            sect,
            root: r.get(1)?,
            path: r.get(2)?,
            line: r.get(3)?,
            col: r.get(4)?,
            kind: r.get(5)?,
            container: r.get(6)?,
            within: r.get(7)?,
            text: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            end_line: r.get::<_, Option<u32>>(9)?.unwrap_or(0),
        });
    }
    if container.is_some() {
        for s in 0..2 {
            let n = ans.hits.iter().filter(|h| h.sect == s as i64).count() as i64;
            let cap = if s == 0 { lim.defs } else { lim.refs } as i64;
            if n < cap {
                ans.totals[s] = n;
            }
        }
    }
    Ok(ans)
}

/// Names that match a query case-insensitively or by prefix, for a miss.
///
/// # Arguments
///
/// * `conn` - the store's connection.
/// * `name` - the name that found nothing.
/// * `limit` - how many to return.
///
/// # Errors
///
/// [`Error::Db`] from SQLite.
pub fn near_names(conn: &Connection, name: &str, limit: usize) -> Result<Vec<(String, i64)>> {
    let mut st = match conn.prepare_cached(
        "SELECT name, SUM(defs) FROM code_counts WHERE name LIKE ?1 ESCAPE '\\' GROUP BY name ORDER BY name = ?2 COLLATE NOCASE DESC, length(name), name LIMIT ?3",
    ) {
        Ok(st) => st,
        Err(e) if e.to_string().contains("no such table") => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let pat = format!("{}%", name.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let rows = st.query_map(params![pat, name, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// A root's last indexing, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRecord {
    /// The walk fingerprint indexed.
    pub fingerprint: String,
    /// When (UTC).
    pub indexed: String,
    /// The Rust note (`scip` or why not).
    pub rust: String,
}

/// The packages code roots have recorded ([`write_root`]): those `root`
/// defines when `own`, else those every other root defines (the packages a
/// reference of `root` is stored into: [`Rows::linked`]).
///
/// # Errors
///
/// [`Error::Db`] from SQLite (a missing table is an empty set).
pub fn defined_packages(conn: &Connection, root: &str, own: bool) -> Result<HashSet<String>> {
    let sql = if own {
        "SELECT package FROM code_packages WHERE defines = 1 AND root = ?1"
    } else {
        "SELECT DISTINCT package FROM code_packages WHERE defines = 1 AND root <> ?1"
    };
    let mut st = match conn.prepare_cached(sql) {
        Ok(st) => st,
        Err(e) if e.to_string().contains("no such table") => return Ok(HashSet::new()),
        Err(e) => return Err(e.into()),
    };
    let rows = st.query_map([root], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// After `root` was written: when the packages it defines changed (`before`
/// to `after`), every other root whose references go into a changed one has
/// its fingerprint cleared, so the next walk re-indexes it and its
/// cross-root references follow. Nothing else is re-indexed: a root's rows
/// never need another root's run, only its recorded packages.
///
/// # Returns
///
/// How many roots were marked.
///
/// # Errors
///
/// [`Error::StoreTooNew`] (nothing marked), or [`Error::Db`] from SQLite.
pub fn mark_package_users(conn: &Connection, root: &str, before: &HashSet<String>, after: &HashSet<String>) -> Result<usize> {
    let mut changed: Vec<&String> = before.symmetric_difference(after).collect();
    if changed.is_empty() {
        return Ok(0);
    }
    changed.sort();
    crate::store::guarded(conn, || {
        let mut st = conn.prepare_cached(
            "UPDATE code_roots SET fingerprint = '' WHERE root <> ?1 AND fingerprint <> ''
             AND root IN (SELECT root FROM code_packages WHERE package = ?2 AND defines = 0)",
        )?;
        let mut n = 0;
        for p in changed {
            n += st.execute(params![root, p])?;
        }
        Ok(n)
    })
}

/// Every root's last indexing, by name.
///
/// # Errors
///
/// [`Error::Db`] from SQLite (a missing table is an empty map).
pub fn root_records(conn: &Connection) -> Result<HashMap<String, RootRecord>> {
    let mut st = match conn.prepare_cached("SELECT root, fingerprint, indexed, rust FROM code_roots") {
        Ok(st) => st,
        Err(e) if e.to_string().contains("no such table") => return Ok(HashMap::new()),
        Err(e) => return Err(e.into()),
    };
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, RootRecord { fingerprint: r.get(1)?, indexed: r.get(2)?, rust: r.get(3)? })))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// One definition of a file, with its line range (for annotations).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The name, with its container when it has one (`Store::open`).
    pub name: String,
    /// A short kind: `fn`, `struct`, `mod`, ...
    pub kind: String,
    /// First line, 1-based (the line that names it).
    pub line: u32,
    /// Last line, 1-based (at least `line`).
    pub end: u32,
}

impl Item {
    /// `fn name 10-20`, the annotation text without brackets.
    pub fn label(&self) -> String {
        format!("{} {} {}-{}", self.kind, self.name, self.line, self.end)
    }
}

/// The short kind of a stored definition kind: `fn (scan)` and SCIP's
/// `Method` both become `fn`.
fn short_kind(kind: &str) -> String {
    let k = kind.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    match k.as_str() {
        "function" | "method" | "staticmethod" | "constructor" | "ctor" => "fn".into(),
        "module" | "namespace" => "mod".into(),
        "constant" => "const".into(),
        "typealias" | "typeparameter" => "type".into(),
        "macro_rules!" => "macro".into(),
        _ => k,
    }
}

/// Folder paths compare with `/` separators, ignoring ASCII case on Windows.
fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// The code root holding a file: `(root name, root's last indexing, folder,
/// path below it)`. The deepest root wins (nested roots).
fn root_of(conn: &Connection, path: &str) -> Result<Option<(String, String, String, String)>> {
    let p = path.replace('\\', "/");
    let mut st = match conn.prepare_cached("SELECT root, dir, indexed FROM code_roots WHERE dir != ''") {
        Ok(st) => st,
        Err(e) if e.to_string().contains("no such") => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let roots: Vec<(String, String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
    let mut best: Option<(String, String, String)> = None;
    for (root, dir, indexed) in roots {
        let d = dir.trim_end_matches('/').to_string();
        if p.len() > d.len() + 1 && p.is_char_boundary(d.len()) && same_path(&p[..d.len()], &d) && p.as_bytes()[d.len()] == b'/' && best.as_ref().is_none_or(|b| d.len() > b.2.len()) {
            best = Some((root, indexed, d));
        }
    }
    Ok(best.map(|(root, indexed, d)| {
        let rel = p[d.len() + 1..].to_string();
        (root, indexed, d, rel)
    }))
}

/// A file's row in `code_files`: its id and when it alone was last
/// indexed ([`reindex_file`]; empty when only its root was).
fn file_row(conn: &Connection, root: &str, rel: &str) -> Result<Option<(i64, String)>> {
    let sql = if cfg!(windows) {
        "SELECT id, indexed FROM code_files WHERE root = ?1 AND path = ?2 COLLATE NOCASE"
    } else {
        "SELECT id, indexed FROM code_files WHERE root = ?1 AND path = ?2"
    };
    Ok(conn.query_row(sql, params![root, rel], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
}

/// The definitions of an indexed file, sorted by first line, when the code
/// index covers it and is current for it.
///
/// `None` when no root holds the file, the file is not indexed, or it
/// changed after it was last indexed (with its root, or alone by
/// [`reindex_file`]): its line numbers may have moved.
///
/// # Arguments
///
/// * `conn` - a connection to the index database.
/// * `path` - the file's absolute path (either separator).
///
/// # Errors
///
/// [`Error::Db`] from SQLite (a database without the code tables answers
/// `None`).
pub fn file_items(conn: &Connection, path: &str) -> Result<Option<Vec<Item>>> {
    let Some((root, root_indexed, _, rel)) = root_of(conn, path)? else { return Ok(None) };
    let Some((file, file_indexed)) = file_row(conn, &root, &rel)? else { return Ok(None) };
    // A file written after the indexing may have moved its lines.
    let indexed = root_indexed.max(file_indexed);
    let modified = fs::metadata(path).and_then(|m| m.modified()).map(crate::store::utc).unwrap_or_default();
    if modified.is_empty() || modified > indexed {
        return Ok(None);
    }
    // Definitions, and impl blocks (an impl edge row names its block
    // `Trait for Type`; the two rows of one block become one item).
    // An impl edge from a SCIP relationship (kind `implements`) sits on its
    // definition's own line and range: the definition is the item.
    let mut st = conn.prepare_cached("SELECT name, kind, container, line, end_line, sect FROM code_occ WHERE file = ?1 AND sect IN (0, 2) AND kind <> 'implements'")?;
    let mut items: Vec<Item> = st
        .query_map([file], |r| {
            let name: String = r.get(0)?;
            let kind: String = r.get(1)?;
            let container: String = r.get(2)?;
            let line: u32 = r.get(3)?;
            let end: u32 = r.get(4)?;
            let sect: i64 = r.get(5)?;
            if sect == SECT_IMPL {
                return Ok(Item { name: container, kind: "impl".into(), line, end: end.max(line) });
            }
            Ok(Item { name: if container.is_empty() { name } else { format!("{container}::{name}") }, kind: short_kind(&kind), line, end: end.max(line) })
        })?
        .collect::<std::result::Result<_, _>>()?;
    items.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.name.cmp(&b.name)));
    items.dedup_by(|a, b| a.line == b.line && a.name == b.name);
    Ok(Some(items))
}

/// What [`reindex_file`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reindexed {
    /// No code root holds the file, it is not a `.rs` or `.wgsl` file the
    /// walk would index, or the code tables are from another schema.
    NotCovered,
    /// The file is gone (or over [`MAX_FILE`]): its rows were removed.
    Removed,
    /// Its rows were rewritten: definitions, impl edges, lines.
    Rows(usize),
}

/// Re-indexes one file of a code root at once, from the line scan (Rust)
/// or the WGSL scanner, without waiting for the root's next indexing.
///
/// The file's rows are replaced in one transaction and it is stamped
/// indexed now, so annotations cover it again. Its SCIP rows (references,
/// SCIP kinds) are replaced by scan rows until the root's next full
/// indexing, which the change itself schedules; WGSL references are kept
/// only to names the file itself declares until then.
///
/// # Arguments
///
/// * `conn` - a connection to the index database.
/// * `path` - the file's absolute path (either separator).
///
/// # Errors
///
/// [`Error::Db`] from SQLite; nothing is changed then.
pub fn reindex_file(conn: &mut Connection, path: &str) -> Result<Reindexed> {
    let have: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = 'code_schema'", [], |r| r.get(0)).optional().unwrap_or(None);
    if have.as_deref() != Some(CODE_SCHEMA) {
        return Ok(Reindexed::NotCovered);
    }
    let Some((root, _, dir, rel)) = root_of(conn, path)? else { return Ok(Reindexed::NotCovered) };
    let wgsl = rel.ends_with(".wgsl");
    let lang = source_lang(&rel);
    if !relevant(&rel) || !(wgsl || rel.ends_with(".rs") || lang.is_some()) {
        return Ok(Reindexed::NotCovered);
    }
    let text = read_source(Path::new(&dir), &rel);
    let mut rows = Rows::default();
    let mut files: HashMap<String, u32> = HashMap::new();
    if let Some(t) = &text {
        let one = [(rel.clone(), t.clone())];
        if wgsl {
            wgsl_rows(&one, &mut rows, &mut files);
        } else if lang.is_some() {
            lang_rows(&one, &mut rows, &mut files);
        } else {
            scan_rows(&one, &mut rows, &mut files);
        }
    }
    // `begin` refuses a store a newer atlas upgraded since this one opened it.
    let tx = crate::store::begin(conn)?;
    let existing: Option<i64> = file_row(&tx, &root, &rel)?.map(|x| x.0);
    let mut names: HashSet<String> = HashSet::new();
    if let Some(id) = existing {
        let mut st = tx.prepare("SELECT DISTINCT name FROM code_occ WHERE file = ?1")?;
        for n in st.query_map([id], |r| r.get::<_, String>(0))? {
            names.insert(n?);
        }
        drop(st);
        tx.execute("DELETE FROM code_occ WHERE file = ?1", [id])?;
        tx.execute("DELETE FROM code_lines WHERE file = ?1", [id])?;
    }
    let Some(text) = text else {
        if let Some(id) = existing {
            tx.execute("DELETE FROM code_files WHERE id = ?1", [id])?;
        }
        recount(&tx, &root, &names)?;
        tx.commit()?;
        return Ok(Reindexed::Removed);
    };
    let now = crate::store::utc(SystemTime::now());
    let id = match existing {
        Some(id) => {
            tx.execute("UPDATE code_files SET indexed = ?2 WHERE id = ?1", params![id, now])?;
            id
        }
        None => {
            tx.execute("INSERT INTO code_files(root, path, lang, indexed) VALUES(?1, ?2, ?3, ?4)", params![root, rel, if wgsl { "wgsl" } else { lang.map_or("rust", |l| l.name) }, now])?;
            tx.last_insert_rowid()
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut need: HashSet<u32> = HashSet::new();
    {
        let mut st = tx.prepare(
            "INSERT OR IGNORE INTO code_occ(name, sect, file, line, col, ord, kind, container, within, end_line) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        let mut order: Vec<&Occ> = rows.occ.iter().collect();
        order.sort_by(|x, y| (&x.name, x.sect, x.line, x.col).cmp(&(&y.name, y.sect, y.line, y.col)));
        let mut prev: Option<(&str, i64, u32, u32)> = None;
        let mut ord = 0;
        for o in order {
            let key = (o.name.as_str(), o.sect, o.line, o.col);
            ord = if prev == Some(key) { ord + 1 } else { 0 };
            prev = Some(key);
            let end = match rows.ends.get(&(o.file, o.line)) {
                Some(&e) if o.sect != SECT_REF => e,
                _ if o.sect != SECT_REF && o.line >= 1 && (o.line as usize) <= lines.len() => crate::digest::item_end(&lines, o.line as usize - 1) as u32,
                _ => 0,
            };
            st.execute(params![o.name, o.sect, id, o.line, o.col, ord, o.kind, o.container, o.within, end])?;
            names.insert(o.name.clone());
            need.insert(o.line);
        }
    }
    {
        let mut st = tx.prepare("INSERT INTO code_lines(file, line, text) VALUES(?1, ?2, ?3)")?;
        for (i, l) in lines.iter().enumerate() {
            let n = i as u32 + 1;
            if !need.contains(&n) {
                continue;
            }
            let mut end = l.len().min(MAX_LINE);
            while !l.is_char_boundary(end) {
                end -= 1;
            }
            st.execute(params![id, n, &l[..end]])?;
        }
    }
    recount(&tx, &root, &names)?;
    tx.commit()?;
    Ok(Reindexed::Rows(rows.occ.len()))
}

/// Recomputes `code_counts` for these names in one root.
fn recount(tx: &rusqlite::Transaction<'_>, root: &str, names: &HashSet<String>) -> Result<()> {
    let mut del = tx.prepare("DELETE FROM code_counts WHERE name = ?1 AND root = ?2")?;
    let mut ins = tx.prepare(
        "INSERT INTO code_counts(name, root, defs, refs, impls)
         SELECT ?1, ?2, SUM(o.sect = 0), SUM(o.sect = 1), SUM(o.sect = 2) FROM code_occ o JOIN code_files f ON f.id = o.file
         WHERE o.name = ?1 AND f.root = ?2 HAVING COUNT(*) > 0",
    )?;
    for n in names {
        del.execute(params![n, root])?;
        ins.execute(params![n, root])?;
    }
    Ok(())
}

/// The innermost item whose range holds `line`: the shortest span, then
/// the latest start, then the name (so the answer is one fixed item).
pub fn enclosing(items: &[Item], line: u32) -> Option<&Item> {
    items
        .iter()
        .filter(|i| i.line <= line && line <= i.end && i.end > i.line)
        .min_by(|a, b| (a.end - a.line).cmp(&(b.end - b.line)).then(b.line.cmp(&a.line)).then(a.name.cmp(&b.name)))
}

/// The text answer for `symbol`: `defs`, `refs` (with the caller in
/// `in`), `impls`, each a header row and tab-separated rows (see
/// [`crate::text`]), with `+N more` lines when cut.
///
/// # Arguments
///
/// * `a` - the answer.
/// * `lim` - the limits it was asked with.
pub fn render(a: &SymbolAnswer, lim: Limits) -> String {
    let mut out = String::new();
    let loc = |h: &SymbolHit| {
        if h.end_line > h.line {
            format!("{}:{}:{}-{}", h.root, h.path, h.line, h.end_line)
        } else {
            format!("{}:{}:{}", h.root, h.path, h.line)
        }
    };
    let snip = |h: &SymbolHit| text::window(&h.text, h.col as usize, a.name.len(), text::SNIPPET_CHARS);
    let sect = |s: i64| a.hits.iter().filter(move |h| h.sect == s);
    if a.totals[0] > 0 {
        out.push_str(&format!("defs {}\n", a.totals[0]));
        {
            let mut t = text::Table::new(&mut out, &["loc", "kind", "in", "snippet"]);
            for h in sect(SECT_DEF) {
                t.field(&loc(h)).field(&h.kind).field(&h.container).field(&snip(h));
            }
        }
        text::more(&mut out, sect(SECT_DEF).count(), a.totals[0] as usize, lim.defs);
    }
    if a.totals[1] > 0 {
        out.push_str(&format!("refs {}\n", a.totals[1]));
        {
            let mut t = text::Table::new(&mut out, &["loc", "in", "snippet"]);
            for h in sect(SECT_REF) {
                let within = if h.kind.is_empty() { h.within.clone() } else if h.within.is_empty() { h.kind.clone() } else { format!("{} ({})", h.within, h.kind) };
                t.field(&loc(h)).field(&within).field(&snip(h));
            }
        }
        text::more(&mut out, sect(SECT_REF).count(), a.totals[1] as usize, lim.refs);
    }
    if a.totals[2] > 0 {
        out.push_str(&format!("impls {}\n", a.totals[2]));
        {
            let mut t = text::Table::new(&mut out, &["loc", "impl"]);
            for h in sect(SECT_IMPL) {
                t.field(&loc(h)).field(&h.container);
            }
        }
        text::more(&mut out, sect(SECT_IMPL).count(), a.totals[2] as usize, lim.impls);
    }
    out
}

impl std::fmt::Display for RowStats {
    /// `name N` pairs for the nonzero counts, as `--code-index` prints them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let all = [
            ("scip defs", self.scip_defs),
            ("scan defs", self.scan_defs),
            ("scip refs", self.scip_refs),
            ("external refs", self.external_refs),
            ("cross-root refs", self.xroot_refs),
            ("impls", self.impls),
            ("wgsl defs", self.wgsl_defs),
            ("wgsl refs", self.wgsl_refs),
            ("proto defs", self.proto_defs),
            ("proto refs", self.proto_refs),
            ("mentions", self.mentions),
            ("other-language defs", self.lang_defs),
            ("rpc stubs", self.rpc_stubs),
            ("rpc calls", self.rpc_calls),
            ("rpc servers", self.rpc_servers),
        ];
        let shown: Vec<String> = all.iter().filter(|(_, n)| *n > 0).map(|(k, n)| format!("{n} {k}")).collect();
        if shown.is_empty() {
            f.write_str("none")
        } else {
            f.write_str(&shown.join(", "))
        }
    }
}

impl std::fmt::Display for WriteStats {
    /// `N files, N defs, N refs, N impls, N lines`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} files, {} defs, {} refs, {} impls, {} lines", self.files, self.defs, self.refs, self.impls, self.lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_lines() {
        type Case = (&'static str, Option<(&'static str, &'static str, usize)>);
        let cases: &[Case] = &[
            ("pub fn open(p: &Path) {", Some(("fn", "open", 7))),
            ("    pub(crate) const unsafe fn raw() {}", Some(("fn", "raw", 31))),
            ("pub(in crate::a) struct S;", Some(("struct", "S", 24))),
            ("const MAX: u32 = 3;", Some(("const", "MAX", 6))),
            ("const _: () = ();", None),
            ("static mut COUNT: u32 = 0;", Some(("static", "COUNT", 11))),
            ("extern \"C\" fn cb() {}", Some(("fn", "cb", 14))),
            ("async fn go() {}", Some(("fn", "go", 9))),
            ("macro_rules! twice {", Some(("macro_rules!", "twice", 13))),
            ("fn r#type() {}", Some(("fn", "type", 5))),
            ("    type Output = Self;", Some(("type", "Output", 9))),
            ("let fn_ptr = 3;", None),
            ("// fn hidden() {}", None),
            ("impl Foo for Bar {", None),
            ("mod tests {", Some(("mod", "tests", 4))),
            ("union U { a: u8 }", Some(("union", "U", 6))),
        ];
        for (line, want) in cases {
            assert_eq!(rust_item(line), *want, "{line}");
        }
    }

    #[test]
    fn impl_lines() {
        let s = |t: &str| Some(t.to_string());
        type Case = (&'static str, Option<(String, Option<String>, usize)>);
        let cases: &[Case] = &[
            ("impl Store {", Some(("Store".into(), None, 0))),
            ("    impl std::fmt::Debug for Router {", Some(("Router".into(), s("Debug"), 4))),
            ("impl<T: Clone, const N: usize> From<[T; N]> for Ring<T> {", Some(("Ring".into(), s("From"), 0))),
            ("unsafe impl Send for Raw {}", Some(("Raw".into(), s("Send"), 7))),
            ("impl<'a> Iterator for &'a mut Walk<'a>", Some(("Walk".into(), s("Iterator"), 0))),
            ("impl<F> Visit for F where F: Fn(u32) -> u32 {", Some(("F".into(), s("Visit"), 0))),
            ("impl<T> Tr for Box<dyn Fn(&T) -> bool> {", Some(("Box".into(), s("Tr"), 0))),
            ("impl !Sync for Cell {}", Some(("Cell".into(), s("Sync"), 0))),
            ("impl<T> Foo<T> {", Some(("Foo".into(), None, 0))),
            ("fn implement() {}", None),
            ("let imp = impl_of(x);", None),
            ("impl<", None),
            ("// impl Store {", None),
        ];
        for (line, want) in cases {
            assert_eq!(rust_impl(line), *want, "{line}");
        }
    }

    #[test]
    fn code_lines_skip_comments_and_strings() {
        let src = "fn a() {}\n/* fn b() {}\nfn c() {} */\nfn d() {}\nconst S: &str = r#\"\nfn e() {}\n\"#;\nlet x = \"a\\\nfn f\";\nfn g<'a>(c: char) { let q = '\"'; }\nfn h() {}\n";
        let names: Vec<String> = scan_rust_defs(src).into_iter().map(|d| d.1).collect();
        assert_eq!(names, ["a", "d", "S", "g", "h"]);
    }

    #[test]
    fn symbol_parts() {
        let p = parse_symbol("rust-analyzer cargo demo 0.1.0 store/impl#[Store][Open]open().").unwrap();
        assert_eq!((p.name, p.container.as_str(), p.imp), ("open", "Store", Some(("Store", Some("Open")))));
        let p = parse_symbol("rust-analyzer cargo demo 0.1.0 store/impl#[`Result<T, E>`][Open]").unwrap();
        assert!(p.is_impl_block);
        assert_eq!(p.imp, Some(("Result", Some("Open"))));
        let p = parse_symbol("rust-analyzer cargo demo 0.1.0 store/Store#field.").unwrap();
        assert_eq!((p.name, p.container.as_str()), ("field", "Store"));
        assert!(parse_symbol("rust-analyzer cargo demo 0.1.0 f().[T]").is_none());
        assert_eq!(type_name("&mut a::Foo<T>"), "Foo");
    }

    #[test]
    fn generic_symbol_parts() {
        fn part(sym: &str) -> Option<(&str, char, String, bool, bool)> {
            parse_generic(sym).map(|g| (g.name, g.suffix, g.container, g.member, g.ctor))
        }
        // The samples of each indexer's scheme in docs/design/langs.md.
        assert_eq!(
            part("scip-go gomod github.com/gin-gonic/gin 0f09c3a9b462 `github.com/gin-gonic/gin`/authPairs#searchCredential()."),
            Some(("searchCredential", '(', "authPairs".into(), true, false))
        );
        assert_eq!(part("scip-go gomod github.com/gin-gonic/gin 0f09c3a9b462 `github.com/gin-gonic/gin`/New()."), Some(("New", '(', "gin".into(), false, false)));
        assert_eq!(
            part("scip-typescript npm got 16.2.0 source/core/`standard-schema.ts`/StandardSchemaV1Issue#"),
            Some(("StandardSchemaV1Issue", '#', "standard-schema".into(), false, false))
        );
        assert_eq!(part("scip-python python flask 49943b5 `examples.tutorial.tests.conftest`/AuthActions#"), Some(("AuthActions", '#', "conftest".into(), false, false)));
        assert_eq!(
            part("semanticdb maven maven/com.google.code.gson/gson 2.15.1-SNAPSHOT com/google/gson/JsonNull#deepCopy()."),
            Some(("deepCopy", '(', "JsonNull".into(), true, false))
        );
        // Constructors take their type's name.
        assert_eq!(
            part("scip-dotnet nuget . . Capturing/PropertyValueConverter#DepthLimiter#`.ctor`()."),
            Some(("DepthLimiter", '(', "DepthLimiter".into(), true, true))
        );
        assert_eq!(part("scip-typescript npm shapes 1.0.0 src/`shapes.ts`/Square#`<constructor>`()."), Some(("Square", '(', "Square".into(), true, true)));
        assert_eq!(part("semanticdb maven . . a/B#`<init>`()."), Some(("B", '(', "B".into(), true, true)));
        // A C# static constructor too; an explicit interface member by the
        // name it is called by.
        assert_eq!(
            part("scip-dotnet nuget . . Core/LoggerTests#`.cctor`()."),
            Some(("LoggerTests", '(', "LoggerTests".into(), true, true))
        );
        assert_eq!(
            part("scip-dotnet nuget . . Context/EnricherStack#`System.Collections.Generic.IEnumerable<Serilog.Core.ILogEventEnricher>.GetEnumerator`()."),
            Some(("GetEnumerator", '(', "EnricherStack".into(), true, false))
        );
        assert_eq!(explicit_member("System.Collections.IEnumerator.Current"), "Current");
        assert_eq!(explicit_member("plain"), "plain");
        assert_eq!(explicit_member("a.b-c"), "a.b-c", "only an identifier after the dot");
        // A Python class inside a function: the function is the container.
        assert_eq!(part("scip-python python p 1 `m`/make().Inner#"), Some(("Inner", '#', "make".into(), false, false)));
        // Not stored: parameters, type parameters, `:` members, namespaces.
        assert!(parse_generic("scip-typescript npm shapes 1.0.0 src/`shapes.ts`/total().(shapes)").is_none());
        assert!(parse_generic("semanticdb maven . . a/B#[T]").is_none());
        assert!(parse_generic("scip-typescript npm got 16.2.0 source/`index.ts`/options0:").is_none());
        assert!(parse_generic("scip-typescript npm shapes 1.0.0 src/`shapes.ts`/").is_none());
        assert!(parse_generic("local 3").is_none());
    }

    #[test]
    fn kinds_without_an_indexer_kind() {
        let k = |sym: &str, line: &str, name: &str| {
            let g = parse_generic(sym).unwrap();
            let at = line.find(name).unwrap();
            fallback_kind(&g, &line[..at], &line[at..])
        };
        let cs = "scip-dotnet nuget . . Shapes/";
        assert_eq!(k(&format!("{cs}IShape#"), "    public interface IShape", "IShape"), "interface");
        assert_eq!(k(&format!("{cs}Square#Name."), "    public string Name => $\"{Side}\";", "Name"), "property");
        assert_eq!(k(&format!("{cs}Square#_side."), "    readonly double _side = 1;", "_side"), "field");
        assert_eq!(k(&format!("{cs}Square#"), "    public sealed class Square : IShape", "Square"), "class");
        assert_eq!(k(&format!("{cs}Point#"), "    public readonly record struct Point(int X);", "Point"), "struct");
        assert_eq!(k(&format!("{cs}Square#`.ctor`()."), "        public Square(double side)", "Square"), "ctor");
        assert_eq!(k(&format!("{cs}Square#Area()."), "        public double Area()", "Area"), "method");
        assert_eq!(k(&format!("{cs}Square#Side."), "        public double Side { get; }", "Side"), "property");
        assert_eq!(k(&format!("{cs}Square#side."), "        private double side;", "side"), "field");
        let py = "scip-python python p 1 `pkg.shapes`/";
        assert_eq!(k(&format!("{py}Square#"), "class Square(Shape):", "Square"), "class");
        assert_eq!(k(&format!("{py}Square#area()."), "    def area(self):", "area"), "method");
        assert_eq!(k(&format!("{py}total()."), "def total(shapes):", "total"), "fn");
        assert_eq!(k(&format!("{py}UNIT."), "UNIT = 1.0", "UNIT"), "var");
    }

    #[test]
    fn languages_of_documents() {
        assert_eq!(lang_of("TypeScript", "src/a.ts"), "typescript");
        assert_eq!(lang_of("", "src\\a.tsx"), "typescript");
        assert_eq!(lang_of("", "shapes.go"), "go");
        assert_eq!(lang_of("", "Shapes.cs"), "csharp");
        assert_eq!(lang_of("python", "x"), "python");
        assert_eq!(lang_of("", "README"), "other");
    }

    #[test]
    fn units_note_keeps_the_rust_note() {
        let u = |lang: &str, source: &str| Unit { dir: String::new(), lang: lang.into(), source: source.into() };
        assert_eq!(units_note(&[u("rust", "scip")]), "scip");
        assert_eq!(units_note(&[]), "no Rust files");
        assert_eq!(units_note(&[u("go", "prebuilt scip-go 0.2.9")]), "go: prebuilt scip-go 0.2.9");
        assert_eq!(units_note(&[u("rust", "scan only: x"), u("go", "prebuilt scip-go 0.2.9")]), "rust: scan only: x; go: prebuilt scip-go 0.2.9");
        // Fixture units are indexed but not named, unless nothing else is.
        let at = |dir: &str, lang: &str, source: &str| Unit { dir: dir.into(), lang: lang.into(), source: source.into() };
        let units = [u("rust", "scip"), at("fixtures/scip/ts", "typescript", "scan only: x"), at("svc/testdata/m", "go", "scan only: y")];
        assert_eq!(units_note(&units), "scip");
        assert_eq!(units_note(&units[1..2]), "typescript fixtures/scip/ts: scan only: x");
        assert_eq!(
            units_note(&[at("web", "typescript", "scip-typescript 0.5.2"), units[1].clone()]),
            "typescript web: scip-typescript 0.5.2"
        );
        assert!(!fixture_unit("fixturesque") && fixture_unit("fixtures") && !fixture_unit(""));
    }

    #[test]
    fn a_newer_code_index_is_refused_and_left_as_it_is() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let newer = format!("code-{}", code_schema_number(CODE_SCHEMA).unwrap() + 1);
        conn.execute("UPDATE meta SET value = ?1 WHERE key = 'code_schema'", [&newer]).unwrap();
        conn.execute_batch("INSERT INTO code_files(id, root, path, lang) VALUES(1, 'app', 'src/lib.rs', 'rust'); DROP TABLE code_packages;").unwrap();
        let e = ensure_schema(&conn).unwrap_err();
        assert!(matches!(&e, Error::StoreTooNew { store, found, supported, .. } if store == "code index" && *found == newer && supported == CODE_SCHEMA), "{e}");
        let schema: String = conn.query_row("SELECT value FROM meta WHERE key = 'code_schema'", [], |r| r.get(0)).unwrap();
        assert_eq!(schema, newer, "the version is not written back");
        let files: i64 = conn.query_row("SELECT COUNT(*) FROM code_files", [], |r| r.get(0)).unwrap();
        assert_eq!(files, 1, "nothing is dropped");
        let made: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'code_packages'", [], |r| r.get(0)).unwrap();
        assert_eq!(made, 0, "no schema is run");
        assert!(code_too_new(&conn, Path::new("x.sqlite")).is_some());
        // Unnumbered and older schemas are not refused.
        assert!(code_schema_too_new(Some("code-6"), "").is_none() && code_schema_too_new(Some("garbage"), "").is_none() && code_schema_too_new(None, "").is_none());
    }

    #[test]
    fn the_schema_step_under_the_lock_refuses_an_index_upgraded_since_the_first_read() {
        // The race: `ensure_schema` read an older schema, a newer binary then
        // wrote its own, and this one takes the write lock. The step under
        // the lock reads it again and refuses, with nothing written.
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let newer = format!("code-{}", code_schema_number(CODE_SCHEMA).unwrap() + 1);
        conn.execute("UPDATE meta SET value = ?1 WHERE key = 'code_schema'", [&newer]).unwrap();
        conn.execute_batch("DROP TABLE code_packages;").unwrap();
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        let e = make_schema(&tx).unwrap_err();
        assert!(e.is_store_too_new(), "{e}");
        drop(tx);
        assert!(conn.is_autocommit(), "rolled back");
        let made: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'code_packages'", [], |r| r.get(0)).unwrap();
        assert_eq!(made, 0, "no schema is run");
        // An older one is migrated by the same step.
        conn.execute("UPDATE meta SET value = 'code-6' WHERE key = 'code_schema'", []).unwrap();
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        make_schema(&tx).unwrap();
        tx.commit().unwrap();
        let schema: String = conn.query_row("SELECT value FROM meta WHERE key = 'code_schema'", [], |r| r.get(0)).unwrap();
        assert_eq!(schema, CODE_SCHEMA);
    }

    #[test]
    fn schema_migrates_from_code_5_and_6_and_rebuilds_from_older() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        // A code-5 database: the same tables without code_units, and rows.
        conn.execute_batch(
            "DROP TABLE code_units;
             UPDATE meta SET value = 'code-5' WHERE key = 'code_schema';
             INSERT INTO code_files(id, root, path, lang) VALUES(1, 'app', 'src/lib.rs', 'rust');
             INSERT INTO code_occ(name, sect, file, line, col, ord, kind, container, within, end_line) VALUES('Store', 0, 1, 3, 11, 0, 'struct', '', '', 3);
             INSERT INTO code_counts(name, root, defs, refs, impls) VALUES('Store', 'app', 1, 0, 0);
             INSERT INTO code_roots(root, fingerprint, indexed, files, defs, refs, impls, rust, elapsed_ms, dir) VALUES('app', 'fp', '2026-11-01T00:00:00.000Z', 1, 1, 0, 0, 'scip', 42, 'C:/app');
             INSERT INTO code_roots(root, fingerprint, indexed, files, defs, refs, impls, rust, elapsed_ms, dir) VALUES('shaders', 'fp2', '2026-11-01T00:00:00.000Z', 0, 0, 0, 0, 'no Rust files', 1, 'C:/shaders');",
        )
        .unwrap();
        ensure_schema(&conn).unwrap();
        let schema: String = conn.query_row("SELECT value FROM meta WHERE key = 'code_schema'", [], |r| r.get(0)).unwrap();
        assert_eq!(schema, CODE_SCHEMA);
        let occ: i64 = conn.query_row("SELECT COUNT(*) FROM code_occ", [], |r| r.get(0)).unwrap();
        assert_eq!(occ, 1, "a code-5 index keeps its rows");
        let units: Vec<(String, String, String, String, String, i64)> = conn
            .prepare("SELECT root, dir, lang, source, fingerprint, elapsed_ms FROM code_units")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(units, [("app".into(), "".into(), "rust".into(), "scip".into(), "fp".into(), 42)], "one Rust unit per Rust root");
        assert_eq!(root_records(&conn).unwrap()["app"].rust, "scip");
        // Then on to code-7: code_packages, and every root re-indexed to
        // fill it.
        assert_eq!(root_records(&conn).unwrap()["app"].fingerprint, "");
        // From code-6: the rows stay, the roots are stale.
        conn.execute_batch(
            "DROP TABLE code_packages;
             UPDATE meta SET value = 'code-6' WHERE key = 'code_schema';
             UPDATE code_roots SET fingerprint = 'fp';",
        )
        .unwrap();
        ensure_schema(&conn).unwrap();
        let occ: i64 = conn.query_row("SELECT COUNT(*) FROM code_occ", [], |r| r.get(0)).unwrap();
        let pkgs: i64 = conn.query_row("SELECT COUNT(*) FROM code_packages", [], |r| r.get(0)).unwrap();
        assert_eq!((occ, pkgs), (1, 0), "a code-6 index keeps its rows");
        assert!(root_records(&conn).unwrap().values().all(|r| r.fingerprint.is_empty()));
        // Current: nothing to do.
        ensure_schema(&conn).unwrap();
        let occ: i64 = conn.query_row("SELECT COUNT(*) FROM code_occ", [], |r| r.get(0)).unwrap();
        assert_eq!(occ, 1);
        // Any older schema is rebuilt (its rows dropped).
        conn.execute("UPDATE meta SET value = 'code-4' WHERE key = 'code_schema'", []).unwrap();
        ensure_schema(&conn).unwrap();
        let occ: i64 = conn.query_row("SELECT COUNT(*) FROM code_occ", [], |r| r.get(0)).unwrap();
        let units: i64 = conn.query_row("SELECT COUNT(*) FROM code_units", [], |r| r.get(0)).unwrap();
        assert_eq!((occ, units), (0, 0));
    }

    #[test]
    fn relevant_paths() {
        assert!(relevant("src/a.rs"));
        assert!(relevant("crates/x/Cargo.toml"));
        assert!(relevant("shaders/a.wgsl"));
        assert!(!relevant("target/debug/build/x.rs"));
        assert!(!relevant(".git/HEAD"));
        assert!(!relevant("README.md"));
    }
}

#[cfg(test)]
mod storeguard_tests {
    use super::*;
    use crate::store::skew_support::{dump, make_index_newer};

    /// Marking the users of a changed package on a store a newer atlas
    /// upgraded since this one opened it clears no fingerprint; at the
    /// current version it does.
    #[test]
    fn marking_package_users_checks_the_version_under_the_lock() {
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c).unwrap();
        c.execute_batch(
            "INSERT INTO code_roots(root, fingerprint, indexed, files, defs, refs, impls, rust, elapsed_ms) VALUES('b', 'fp', 't', 0, 0, 0, 0, '', 0);
             INSERT INTO code_packages(root, package, defines) VALUES('b', 'p', 0);",
        )
        .unwrap();
        let (was, now): (HashSet<String>, HashSet<String>) = (HashSet::new(), ["p".to_string()].into());
        make_index_newer(&c);
        let before = dump(&c);
        assert!(mark_package_users(&c, "a", &was, &now).expect_err("refused").is_store_too_new());
        assert!(c.is_autocommit(), "rolled back");
        assert_eq!(dump(&c), before, "no fingerprint cleared");
        c.execute("DELETE FROM meta WHERE key = 'schema'", []).unwrap();
        assert_eq!(mark_package_users(&c, "a", &was, &now).unwrap(), 1);
    }
}
