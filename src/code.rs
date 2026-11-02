//! The code index behind the `symbol` tool: definitions, references,
//! impl edges and callers for Rust (from `rust-analyzer scip`, decoded by
//! [`crate::scip`]) and WGSL (scanned by [`crate::wgsl`]).
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
//! indexing (fingerprint, time, counts, error).
//!
//! # What a row is
//!
//! * Rust definitions: every definition occurrence of a non-local symbol
//!   in the SCIP index. Kind from `SymbolInformation.kind`, else from the
//!   descriptor suffix. Container: the parent descriptor (`Store` for
//!   `Store::open`, the self type for a method in an impl block).
//! * Rust references: every non-definition occurrence of a symbol that is
//!   defined in the same index (references into dependencies and `std` are
//!   not stored). `within` is the innermost enclosing definition that has an
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
pub const CODE_SCHEMA: &str = "code-5";
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
CREATE TABLE IF NOT EXISTS code_lease(
    id INTEGER PRIMARY KEY CHECK (id = 1),
    holder TEXT NOT NULL,
    until INTEGER NOT NULL
);
";

const DROP: &str = "DROP TABLE IF EXISTS code_files; DROP TABLE IF EXISTS code_occ; DROP TABLE IF EXISTS code_lines;
DROP TABLE IF EXISTS code_counts; DROP TABLE IF EXISTS code_roots;";

/// One configured code root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRoot {
    /// Short name.
    pub name: String,
    /// The folder.
    pub root: PathBuf,
    /// See [`crate::config::CodeRootSpec::build_scripts`].
    pub build_scripts: bool,
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
            roots: spec.roots.iter().map(|r| CodeRoot { name: r.name.clone(), root: r.root.clone(), build_scripts: r.build_scripts }).collect(),
            rust_analyzer: spec.rust_analyzer.clone(),
            timeout: Duration::from_secs(spec.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S) as u64),
            debounce: Duration::from_secs(spec.debounce_s.unwrap_or(DEFAULT_DEBOUNCE_S) as u64),
            defer_lock: spec.defer_lock.clone(),
            defer_on_cargo: true,
            dir,
            watch,
            rescan_interval: Duration::from_secs(crate::config::DEFAULT_RESCAN_INTERVAL_S),
        }
    }
}

/// Creates the code tables, dropping them first when their schema changed.
///
/// # Errors
///
/// [`Error::Db`] from SQLite.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    let have: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'code_schema'", [], |r| r.get(0))
        .optional()
        .unwrap_or(None);
    if have.as_deref() == Some(CODE_SCHEMA) {
        return Ok(());
    }
    conn.execute_batch("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
    conn.execute_batch(DROP)?;
    conn.execute_batch(SCHEMA)?;
    conn.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('code_schema', ?1)", [CODE_SCHEMA])?;
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
    /// ([`crate::proto::MENTION_EXTS`], Rust aside).
    pub mention: Vec<WalkedFile>,
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
    rel.ends_with(".rs") || rel.ends_with(".wgsl") || rel.ends_with("Cargo.toml") || rel.ends_with("Cargo.lock") || rel.ends_with(".proto") || crate::proto::is_mention_file(rel)
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
    for l in [&mut w.rust, &mut w.wgsl, &mut w.manifests, &mut w.proto, &mut w.mention] {
        l.sort_by(|a, b| a.path.cmp(&b.path));
    }
    Ok(w)
}

impl Walk {
    /// A digest of every walked file's path, time and size: equal when
    /// nothing the index depends on changed.
    pub fn fingerprint(&self) -> String {
        let mut s = String::new();
        for (tag, l) in [("r", &self.rust), ("w", &self.wgsl), ("m", &self.manifests), ("p", &self.proto), ("o", &self.mention)] {
            for f in l {
                s.push_str(&format!("{tag}\t{}\t{}\t{}\n", f.path, f.mtime, f.size));
            }
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
        if !defined[s] {
            rows.stats.external_refs += 1;
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
/// * `rust` - a one-line note on the Rust side (`scip`, `scan only: why`).
/// * `elapsed_ms` - how long the indexing took.
///
/// # Errors
///
/// [`Error::Db`] from SQLite; nothing is changed then.
pub fn write_root(
    conn: &mut Connection,
    root: &str,
    dir: &Path,
    rows: &Rows,
    text_of: &dyn Fn(usize) -> Option<String>,
    fingerprint: &str,
    rust: &str,
    elapsed_ms: u64,
) -> Result<WriteStats> {
    ensure_schema(conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM code_occ WHERE file IN (SELECT id FROM code_files WHERE root = ?1)", [root])?;
    tx.execute("DELETE FROM code_lines WHERE file IN (SELECT id FROM code_files WHERE root = ?1)", [root])?;
    tx.execute("DELETE FROM code_files WHERE root = ?1", [root])?;
    tx.execute("DELETE FROM code_counts WHERE root = ?1", [root])?;
    let mut ids = Vec::with_capacity(rows.files.len());
    {
        let mut st = tx.prepare("INSERT INTO code_files(root, path, lang) VALUES(?1, ?2, ?3)")?;
        for (p, lang) in &rows.files {
            st.execute(params![root, p, lang])?;
            ids.push(tx.last_insert_rowid());
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
        for o in rows.occ.iter().filter(|o| o.sect != SECT_REF) {
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
            let end = if o.sect != SECT_REF { ends.get(&(o.file, o.line)).copied().unwrap_or(0) } else { 0 };
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
    tx.execute(
        "INSERT OR REPLACE INTO code_roots(root, fingerprint, indexed, files, defs, refs, impls, rust, elapsed_ms, dir) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            root,
            fingerprint,
            crate::store::utc(SystemTime::now()),
            ws.files as i64,
            ws.defs as i64,
            ws.refs as i64,
            ws.impls as i64,
            rust,
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
        "function" | "method" | "staticmethod" | "constructor" => "fn".into(),
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
        if p.len() > d.len() + 1 && p.is_char_boundary(d.len()) && same_path(&p[..d.len()], &d) && p.as_bytes()[d.len()] == b'/' && best.as_ref().map_or(true, |b| d.len() > b.2.len()) {
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
    let mut st = conn.prepare_cached("SELECT name, kind, container, line, end_line, sect FROM code_occ WHERE file = ?1 AND sect IN (0, 2)")?;
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
    if !relevant(&rel) || !(wgsl || rel.ends_with(".rs")) {
        return Ok(Reindexed::NotCovered);
    }
    let text = read_source(Path::new(&dir), &rel);
    let mut rows = Rows::default();
    let mut files: HashMap<String, u32> = HashMap::new();
    if let Some(t) = &text {
        let one = [(rel.clone(), t.clone())];
        if wgsl {
            wgsl_rows(&one, &mut rows, &mut files);
        } else {
            scan_rows(&one, &mut rows, &mut files);
        }
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
            tx.execute("INSERT INTO code_files(root, path, lang, indexed) VALUES(?1, ?2, ?3, ?4)", params![root, rel, if wgsl { "wgsl" } else { "rust" }, now])?;
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
            let end = if o.sect != SECT_REF && o.line >= 1 && (o.line as usize) <= lines.len() { crate::digest::item_end(&lines, o.line as usize - 1) as u32 } else { 0 };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_lines() {
        let cases: &[(&str, Option<(&str, &str, usize)>)] = &[
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
        let cases: &[(&str, Option<(String, Option<String>, usize)>)] = &[
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
    fn relevant_paths() {
        assert!(relevant("src/a.rs"));
        assert!(relevant("crates/x/Cargo.toml"));
        assert!(relevant("shaders/a.wgsl"));
        assert!(!relevant("target/debug/build/x.rs"));
        assert!(!relevant(".git/HEAD"));
        assert!(!relevant("README.md"));
    }
}
