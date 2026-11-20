//! Explaining a release binary's leak places by source content (the
//! release process, the binary leak check).
//!
//! A linker packs unrelated string literals side by side with nothing
//! between them, in an order that changes per build and per target, so a
//! denylist term can show up in a binary across two literals that each hold
//! only part of it. Whether a place is such a join is decided here, from
//! content that does not change between builds: the source the binary was
//! built from.
//!
//! # The corpus
//!
//! The known strings of three sources, each string with the file and line it
//! came from:
//!
//! - the tree: the exported source the binary is built from (a checkout's
//!   tracked files minus the export-ignored ones, or a folder as is);
//! - the dependencies: every package `cargo metadata --locked` names in the
//!   tree's Cargo.lock whose folder is not inside the tree (the registry's
//!   sources), by crate name, version and file;
//! - the toolchain: the string literals of the standard library's source
//!   (`rust-src`) under the tree's `rustc --print sysroot`, when it is
//!   installed: always read, so what explains a place never depends on
//!   whether another place was explained.
//!
//! The known strings of a file ([`scan`]) are of two classes. Literal values,
//! what a compiler lays into a binary whole: in Rust and C source each string
//! literal's value (escapes decoded; one that holds `{` or `}` also split
//! into the pieces a format string compiles to), each run of literals that
//! `concat!` (Rust) or adjacency (C) joins, as one value; in any other text
//! file, the whole file (`include_str!`). And the rest, kept only to report
//! a place one of them holds: identifiers, comment lines, a text file's
//! lines (none from the toolchain). A string shorter than [`MIN_PIECE`]
//! bytes is not kept.
//!
//! # The verdict
//!
//! A linker packs whole literal values end to end. So for a place at bytes
//! `start..end` of an executable ([`Corpus::explain`]):
//!
//! - [`Verdict::Contained`]: one occurrence of a known string holds the
//!   whole place, so the term is in that string. In the tree, the text gate
//!   refuses the same term in the source; in a dependency or the toolchain it
//!   is third-party text, reported by crate and file, and passes only by an
//!   accepted-hits entry keyed on that string ([`crate::accept`]), which can
//!   be made only for a literal value with [`MIN_CONTEXT`] bytes beyond the
//!   place.
//! - [`Verdict::Split`]: a chain of two or more whole occurrences of literal
//!   values, none overlapping the next, each starting where the one before
//!   it ends, covers the place, so the term crosses a boundary between two
//!   of them; and no two of them side by side come from the same file.
//!   Literals packed side by side: passes, and each place is listed.
//! - [`Verdict::Join`]: such a chain exists only with a piece that is not a
//!   literal value (an identifier, a comment, a text line), or with two
//!   pieces of one file side by side (a macro may join them in the source).
//!   It may be packing, it may not: stops, and only an entry keyed on its
//!   pieces passes it.
//! - [`Verdict::Unexplained`]: no such chain: what the build put there (a
//!   path, a user or host name, an environment value, generated code).
//!   Stops the release; never accepted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

type R<T = ()> = Result<T, String>;

/// The fewest bytes a known string must have.
pub const MIN_PIECE: usize = 3;

/// The fewest bytes an entry's known strings must hold beyond the place: an
/// entry for a string that is the term, or little more, would accept the
/// term wherever the build puts it.
pub const MIN_CONTEXT: usize = 4;

/// The prefix length the long strings are indexed by.
const KEY: usize = 8;

/// The largest file read.
const MAX_FILE: u64 = 16 << 20;

/// The largest file kept whole (a text file, for `include_str!`).
const MAX_WHOLE: usize = 1 << 20;

/// How far before a place occurrences are looked for, at most.
const MAX_BACK: usize = 1 << 20;

/// Where a known string came from, strictest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// The exported tree.
    Tree,
    /// A locked dependency.
    Dep,
    /// The toolchain's standard library.
    Toolchain,
}

/// A file of the corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// Its source.
    pub kind: Kind,
    /// The crate, `name version` (the tree's is `tree`, the toolchain's `rust`).
    pub krate: String,
    /// The crate name alone: what an accepted-hits entry is keyed on.
    pub name: String,
    /// The file, relative to the crate's folder.
    pub file: String,
}

/// The known strings.
#[derive(Default)]
pub struct Corpus {
    files: Vec<Origin>,
    strings: Vec<Known>,
    index: HashMap<Vec<u8>, u32>,
    long: HashMap<[u8; KEY], Vec<u32>>,
    max_len: usize,
    /// One line per source: what was read.
    pub summary: Vec<String>,
}

/// A known string: its bytes, and the strictest (file, line) it is found
/// at, and at as a literal value.
struct Known {
    bytes: Vec<u8>,
    any: (u32, u32),
    lit: Option<(u32, u32)>,
}

/// One known string of a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// Its bytes.
    pub string: Vec<u8>,
    /// Its file (as a literal value when it is one) and line.
    pub origin: Origin,
    pub line: usize,
    /// Whether it is a literal value.
    pub literal: bool,
}

/// What a place is (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// One known string holds it: that string, its file and line, whether it
    /// is a literal value, and the bytes it holds beyond the place.
    Contained { string: Vec<u8>, origin: Origin, line: usize, literal: bool, context: usize },
    /// Whole literal values of different files end to end make it.
    Split(Vec<Piece>),
    /// Whole known strings end to end make it, but not as [`Verdict::Split`]
    /// needs: why, and the pieces.
    Join(Vec<Piece>, &'static str),
    /// Not made of known strings.
    Unexplained,
}

impl Corpus {
    /// Adds a known string (`lit`: a literal value), keeping the strictest
    /// file for a repeat. The toolchain's other strings are not kept.
    pub fn add(&mut self, bytes: Vec<u8>, file: u32, line: usize, lit: bool) {
        let kind = self.files[file as usize].kind;
        if bytes.len() < MIN_PIECE || (kind == Kind::Toolchain && !lit) {
            return;
        }
        let at = (file, line as u32);
        let stricter = |old: (u32, u32), files: &[Origin]| kind < files[old.0 as usize].kind;
        if let Some(&id) = self.index.get(&bytes) {
            let s = &mut self.strings[id as usize];
            if stricter(s.any, &self.files) {
                s.any = at;
            }
            if lit && s.lit.is_none_or(|o| stricter(o, &self.files)) {
                s.lit = Some(at);
            }
            return;
        }
        let id = self.strings.len() as u32;
        if bytes.len() >= KEY {
            let k: [u8; KEY] = bytes[..KEY].try_into().expect("KEY bytes");
            self.long.entry(k).or_default().push(id);
        }
        self.max_len = self.max_len.max(bytes.len());
        self.index.insert(bytes.clone(), id);
        self.strings.push(Known { bytes, any: at, lit: lit.then_some(at) });
    }

    /// Registers a file; its strings are then added with [`Corpus::add`].
    pub fn file(&mut self, o: Origin) -> u32 {
        self.files.push(o);
        (self.files.len() - 1) as u32
    }

    /// The number of known strings.
    pub fn len(&self) -> usize {
        self.strings.len()
    }

    /// Whether it holds no string.
    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }

    /// Scans one file's text into the corpus (see [`scan`]).
    pub fn add_text(&mut self, o: Origin, text: &str) {
        let f = self.file(o.clone());
        scan(&o.file, text, &mut |b, l, lit| self.add(b, f, l, lit));
    }

    /// String `id` as a piece: its literal origin when it has one.
    fn piece(&self, id: u32) -> Piece {
        let s = &self.strings[id as usize];
        let (f, l) = s.lit.unwrap_or(s.any);
        Piece { string: s.bytes.clone(), origin: self.files[f as usize].clone(), line: l as usize, literal: s.lit.is_some() }
    }

    /// The file string `id` counts as, as a piece.
    fn piece_file(&self, id: u32) -> u32 {
        let s = &self.strings[id as usize];
        s.lit.unwrap_or(s.any).0
    }

    /// The ids of the known strings at byte `p` of `bin`.
    fn at(&self, bin: &[u8], p: usize, out: &mut Vec<u32>) {
        out.clear();
        for l in MIN_PIECE..KEY {
            if let Some(&id) = bin.get(p..p + l).and_then(|s| self.index.get(s)) {
                out.push(id);
            }
        }
        if let Some(ids) = bin.get(p..p + KEY).and_then(|k| self.long.get(k)) {
            out.extend(ids.iter().copied().filter(|&id| bin[p..].starts_with(&self.strings[id as usize].bytes)));
        }
    }

    /// The verdict on bytes `start..end` of `bin` (see the module docs).
    pub fn explain(&self, bin: &[u8], start: usize, end: usize) -> Verdict {
        if self.is_empty() || start >= end || end > bin.len() {
            return Verdict::Unexplained;
        }
        let lo = start.saturating_sub(self.max_len.min(MAX_BACK));
        // An occurrence never runs over a NUL the place does not hold: a
        // known string with a NUL is rare, and the bound keeps this fast.
        let lo = bin[lo..start].iter().rposition(|&b| b == 0).map_or(lo, |k| lo + k + 1);
        let kind = |f: u32| self.files[f as usize].kind;
        // The strictest string that holds the place, a literal value first,
        // then the longest (the literal is at least that long).
        type Rank = (Kind, bool, std::cmp::Reverse<usize>);
        let mut held: Option<(u32, Rank)> = None;
        // A chain's score, least best: (pieces that are not literal values,
        // pieces of one file side by side, ends of the place the chain does
        // not reach past, pieces). Per byte start+1..end, the best chain
        // whose last piece ends there; `done`, the best that covers the
        // place.
        type Chain = ((usize, usize, usize, usize), Vec<u32>);
        let mut reach: Vec<Option<Chain>> = vec![None; end - start];
        let mut done: Option<Chain> = None;
        let mut ids = Vec::new();
        for p in lo..end {
            // Past the place's first byte a piece only continues a chain.
            let from = if p > start {
                match &reach[p - start] {
                    Some(c) => Some(c.clone()),
                    None => continue,
                }
            } else {
                None
            };
            self.at(bin, p, &mut ids);
            for &id in &ids {
                let s = &self.strings[id as usize];
                let q = p + s.bytes.len();
                if q <= start {
                    continue;
                }
                if p <= start && q >= end {
                    let literal = s.lit.is_some_and(|o| kind(o.0) == kind(s.any.0));
                    let rank = (kind(s.any.0), !literal, std::cmp::Reverse(s.bytes.len()));
                    if held.is_none_or(|(_, r)| rank < r) {
                        held = Some((id, rank));
                    }
                    continue;
                }
                let ((mut a, mut b, mut e, mut n), mut chain) = from.clone().unwrap_or_default();
                a += usize::from(s.lit.is_none());
                b += usize::from(chain.last().is_some_and(|&last| self.piece_file(last) == self.piece_file(id)));
                // A first piece that starts with the place, or a last one
                // that ends with it: the chain may be a word of the build's
                // text that happens to be two known strings.
                e += usize::from(chain.is_empty() && p == start) + usize::from(q == end);
                n += 1;
                chain.push(id);
                let slot = if q >= end { &mut done } else { &mut reach[q - start] };
                if slot.as_ref().is_none_or(|(sc, _)| (a, b, e, n) < *sc) {
                    *slot = Some(((a, b, e, n), chain));
                }
            }
        }
        if let Some((id, (_, not_lit, _))) = held {
            let s = &self.strings[id as usize];
            let (f, l) = if not_lit { s.any } else { s.lit.unwrap_or(s.any) };
            return Verdict::Contained { string: s.bytes.clone(), origin: self.files[f as usize].clone(), line: l as usize, literal: !not_lit, context: s.bytes.len() - (end - start) };
        }
        match done {
            None => Verdict::Unexplained,
            Some(((a, b, e, _), chain)) => {
                let pieces = chain.iter().map(|&id| self.piece(id)).collect();
                match (a, b, e) {
                    (0, 0, 0) => Verdict::Split(pieces),
                    (0, 0, _) => Verdict::Join(pieces, "the pieces start or end where the term does: the build's text may hold it"),
                    (0, _, _) => Verdict::Join(pieces, "two pieces of one file side by side: a macro may join them in the source"),
                    _ => Verdict::Join(pieces, "a piece is not a literal value: an identifier, a comment or a text line"),
                }
            }
        }
    }

    /// The corpus for a release built from `tree` (see the module docs): the
    /// tree (a checkout's HEAD, not its working files), the dependencies its
    /// Cargo.lock locks, and the toolchain's library source when installed.
    pub fn release(tree: &Path) -> R<Corpus> {
        let mut c = Corpus::default();
        let files = tree_texts(tree)?;
        let n = files.len();
        for (f, t) in files {
            c.add_text(Origin { kind: Kind::Tree, krate: "tree".into(), name: "tree".into(), file: f }, &t);
        }
        c.summary.push(format!("the tree: {n} files"));
        if tree.join("Cargo.lock").is_file() {
            let deps = dependencies(tree)?;
            let mut nf = 0;
            for (name, version, dir, kind) in &deps {
                let mut fs = Vec::new();
                walk(dir, Path::new(""), &mut fs);
                nf += fs.len();
                for f in fs {
                    if let Some(t) = read_small(&dir.join(&f)) {
                        c.add_text(Origin { kind: *kind, krate: format!("{name} {version}"), name: name.clone(), file: f }, &t);
                    }
                }
            }
            c.summary.push(format!("the dependencies: {} crates, {nf} files", deps.len()));
        }
        c.add_toolchain(tree);
        if !tree.join("Cargo.toml").is_file() {
            c.summary.push(format!("{} known strings", c.len()));
        }
        Ok(c)
    }

    /// Adds the toolchain's library source's literal values, for a tree with
    /// a Cargo.toml.
    fn add_toolchain(&mut self, tree: &Path) {
        let c = self;
        if tree.join("Cargo.toml").is_file() {
            match toolchain_library(tree) {
                Some(lib) => {
                    let mut fs = Vec::new();
                    walk(&lib, Path::new(""), &mut fs);
                    fs.retain(|f| f.ends_with(".rs"));
                    let nf = fs.len();
                    for f in fs {
                        if let Some(t) = read_small(&lib.join(&f)) {
                            c.add_text(Origin { kind: Kind::Toolchain, krate: "rust".into(), name: "rust".into(), file: f }, &t);
                        }
                    }
                    c.summary.push(format!("the toolchain's library source: {nf} files"));
                }
                None => c.summary.push("the toolchain's library source: not installed (`rustup component add rust-src`), so a join of its strings is not explained".into()),
            }
            c.summary.push(format!("{} known strings", c.len()));
        }
    }
}

/// A file's text when it is small enough, UTF-8 and holds no NUL.
fn read_small(p: &Path) -> Option<String> {
    if std::fs::metadata(p).ok()?.len() > MAX_FILE {
        return None;
    }
    String::from_utf8(std::fs::read(p).ok()?).ok().filter(|t| !t.contains('\0'))
}

/// The text files of the tree, path and text: in a checkout, HEAD's files
/// that are not export-ignored, as HEAD holds them (what the release is
/// built from, whatever the working files hold); otherwise every file of the
/// folder (an exported tree).
fn tree_texts(tree: &Path) -> R<Vec<(String, String)>> {
    if !tree.join(".git").exists() {
        let mut fs = Vec::new();
        walk(tree, Path::new(""), &mut fs);
        return Ok(fs.into_iter().filter_map(|f| read_small(&tree.join(&f)).map(|t| (f, t))).collect());
    }
    let ls = git(tree, &["ls-tree", "-r", "-z", "--full-tree", "HEAD"], Vec::new())?;
    let ignored = crate::release_ci::export_ignored(tree)?;
    let (mut paths, mut ask) = (Vec::new(), Vec::new());
    for e in String::from_utf8_lossy(&ls).split('\0') {
        let Some((meta, path)) = e.split_once('\t') else { continue };
        let mut m = meta.split(' ');
        let (Some(_), Some("blob"), Some(obj)) = (m.next(), m.next(), m.next()) else { continue };
        if !ignored.contains(path) {
            paths.push(path.to_string());
            ask.extend_from_slice(obj.as_bytes());
            ask.push(b'\n');
        }
    }
    // `cat-file --batch`: per object `OBJECT TYPE SIZE` LF, the bytes, LF.
    let out = git(tree, &["cat-file", "--batch"], ask)?;
    let short = || "git cat-file --batch: short output".to_string();
    let (mut texts, mut i) = (Vec::new(), 0);
    for p in paths {
        let nl = i + out.get(i..).and_then(|o| o.iter().position(|&b| b == b'\n')).ok_or_else(short)?;
        let size: usize = std::str::from_utf8(&out[i..nl]).ok().and_then(|h| h.rsplit(' ').next()?.parse().ok()).ok_or_else(short)?;
        let body = out.get(nl + 1..nl + 1 + size).ok_or_else(short)?;
        i = nl + 2 + size;
        if size as u64 <= MAX_FILE {
            if let Some(t) = std::str::from_utf8(body).ok().filter(|t| !t.contains('\0')) {
                texts.push((p, t.to_string()));
            }
        }
    }
    Ok(texts)
}

/// `git -C dir args`'s standard output, with `input` on its standard input.
fn git(dir: &Path, args: &[&str], input: Vec<u8>) -> R<Vec<u8>> {
    use std::process::Stdio;
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("git {}: {e}", args[0]))?;
    let mut stdin = child.stdin.take().ok_or("git: no stdin")?;
    // Written from a thread: git answers as it reads.
    let feed = std::thread::spawn(move || std::io::Write::write_all(&mut stdin, &input));
    let o = child.wait_with_output().map_err(|e| format!("git {}: {e}", args[0]))?;
    let fed = feed.join().map_err(|_| "git: the input thread panicked".to_string())?;
    if !o.status.success() {
        return Err(format!("git {} in {} failed: {}", args[0], dir.display(), String::from_utf8_lossy(&o.stderr).trim()));
    }
    fed.map_err(|e| format!("git {}: {e}", args[0]))?;
    Ok(o.stdout)
}

/// Every file under `dir`, relative and with `/`, but `.git` and `target`.
fn walk(dir: &Path, rel: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir.join(rel)) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let r = rel.join(&name);
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                if name != ".git" && name != "target" {
                    walk(dir, &r, out);
                }
            }
            Ok(t) if t.is_file() => out.push(r.to_string_lossy().replace('\\', "/")),
            _ => {}
        }
    }
}

/// The packages `cargo metadata --locked` names for `tree` whose folder is
/// not inside it: name, version, folder.
fn dependencies(tree: &Path) -> R<Vec<(String, String, PathBuf, Kind)>> {
    let o = Command::new("cargo")
        .current_dir(tree)
        .args(["metadata", "--format-version", "1", "--locked"])
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !o.status.success() {
        return Err(format!("cargo metadata --locked in {} failed: {}", tree.display(), String::from_utf8_lossy(&o.stderr).trim()));
    }
    packages(&String::from_utf8_lossy(&o.stdout), tree)
}

/// [`dependencies`]' reading of the metadata JSON: name, version, folder and
/// kind. Only a registry's package is third-party ([`Kind::Dep`]); a path or
/// git package outside the tree is the owner's code, as strict as the tree.
pub fn packages(json: &str, tree: &Path) -> R<Vec<(String, String, PathBuf, Kind)>> {
    let v = atlas::json::parse(json).map_err(|e| format!("cargo metadata: {e:?}"))?;
    let root = std::fs::canonicalize(tree).unwrap_or_else(|_| tree.to_path_buf());
    let mut out = Vec::new();
    for p in v.pointer("/packages").and_then(|p| p.as_array()).ok_or("cargo metadata: no packages")? {
        let s = |k: &str| p.pointer(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let manifest = PathBuf::from(s("/manifest_path"));
        let Some(dir) = manifest.parent() else { continue };
        let dir_c = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if !dir_c.starts_with(&root) {
            let source = s("/source");
            let kind = if source.starts_with("registry+") || source.starts_with("sparse+") { Kind::Dep } else { Kind::Tree };
            out.push((s("/name"), s("/version"), dir.to_path_buf(), kind));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// The known strings of one file named `name` (its extension picks the
/// reading), each with its line: see the module docs.
/// The callback is given each string, its line, and whether it is a literal
/// value.
pub fn scan(name: &str, text: &str, add: &mut dyn FnMut(Vec<u8>, usize, bool)) {
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "rs" => lex(text, false, add),
        "c" | "h" | "cc" | "cpp" | "hpp" | "inc" => lex(text, true, add),
        _ => {
            if text.len() <= MAX_WHOLE {
                add(text.as_bytes().to_vec(), 1, true);
            }
            for (i, l) in text.lines().enumerate() {
                add(l.trim().as_bytes().to_vec(), i + 1, false);
            }
        }
    }
}

/// A run of literals a compiler joins into one value: the arguments of
/// Rust's `concat!` (at its own bracket depth; commas between them), and C's
/// adjacent string literals. Any other token ends the run.
#[derive(Default)]
struct Run {
    active: bool,
    depth: usize,
    parts: usize,
    value: Vec<u8>,
    line: usize,
}

impl Run {
    /// A literal's value, joined when it is in a run.
    fn lit(&mut self, v: &[u8], line: usize) {
        if self.active && self.depth == 1 {
            if self.parts == 0 {
                self.line = line;
            }
            self.value.extend_from_slice(v);
            self.parts += 1;
        }
    }

    /// Ends the run: the value of two or more literals is a literal value.
    fn end(&mut self, add: &mut dyn FnMut(Vec<u8>, usize, bool)) {
        if self.parts >= 2 {
            add(std::mem::take(&mut self.value), self.line, true);
        }
        self.value.clear();
        self.parts = 0;
    }

    /// Any other token: in C one ends the run; in `concat!` a comma does
    /// not, and brackets nest.
    fn token(&mut self, c: u8, c_like: bool, add: &mut dyn FnMut(Vec<u8>, usize, bool)) {
        if !self.active {
            return;
        }
        match c {
            _ if c_like => self.end(add),
            b',' if self.depth == 1 => {}
            b'(' | b'[' | b'{' => {
                if self.depth == 1 {
                    self.end(add);
                }
                self.depth += 1;
            }
            b')' | b']' | b'}' => {
                self.depth -= 1;
                if self.depth == 0 {
                    self.end(add);
                    self.active = false;
                }
            }
            _ if self.depth == 1 => self.end(add),
            _ => {}
        }
    }
}

/// A literal's value, and when it holds `{` or `}` the pieces a format
/// string with it compiles to (`{{` is `{`, `}}` is `}`, `{...}` a cut).
fn add_literal(v: Vec<u8>, line: usize, add: &mut dyn FnMut(Vec<u8>, usize, bool)) {
    if v.iter().any(|&b| b == b'{' || b == b'}') {
        let mut cur = Vec::new();
        let mut k = 0;
        while k < v.len() {
            match (v[k], v.get(k + 1)) {
                (b'{', Some(b'{')) | (b'}', Some(b'}')) => {
                    cur.push(v[k]);
                    k += 2;
                }
                (b'{', _) => {
                    add(std::mem::take(&mut cur), line, true);
                    while k < v.len() && v[k] != b'}' {
                        k += 1;
                    }
                    k += 1;
                }
                (b, _) => {
                    cur.push(b);
                    k += 1;
                }
            }
        }
        add(cur, line, true);
    }
    add(v, line, true);
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// The lexer for Rust (`c_like` false) and C source: string literals,
/// identifiers and comment lines, and the runs of literals joined into one
/// value ([`Run`]).
fn lex(text: &str, c_like: bool, add: &mut dyn FnMut(Vec<u8>, usize, bool)) {
    let b = text.as_bytes();
    let (mut i, mut line) = (0, 1);
    let nl = |s: &[u8]| s.iter().filter(|&&c| c == b'\n').count();
    let skip_ws = |mut k: usize| {
        while b.get(k).is_some_and(u8::is_ascii_whitespace) {
            k += 1;
        }
        k
    };
    // C joins adjacent literals anywhere; Rust only inside `concat!`.
    let mut run = Run { active: c_like, depth: 1, ..Run::default() };
    while i < b.len() {
        match b[i] {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |k| i + k);
                add(text[i + 2..end].trim_start_matches(['/', '!']).trim().as_bytes().to_vec(), line, false);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let (mut j, mut depth) = (i + 2, 1);
                while j < b.len() && depth > 0 {
                    if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                        depth -= 1;
                        j += 2;
                    } else if !c_like && b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                        depth += 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                let body = &text[i + 2..if depth == 0 { j - 2 } else { j }];
                for (k, l) in body.lines().enumerate() {
                    add(l.trim().trim_start_matches(['*', '!']).trim().as_bytes().to_vec(), line + k, false);
                }
                line += nl(&b[i..j]);
                i = j;
            }
            b'"' => {
                let (v, j) = cooked(b, i + 1, c_like);
                run.lit(&v, line);
                add_literal(v, line, add);
                line += nl(&b[i..j]);
                i = j;
            }
            b'\'' if c_like => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'\'' && b[j] != b'\n' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                run.token(b'\'', c_like, add);
                i = j + 1;
            }
            b'\'' => {
                // A char literal, or a lifetime or label.
                if b.get(i + 1) == Some(&b'\\') {
                    let mut j = i + 3;
                    while j < b.len() && b[j] != b'\'' && b[j] != b'\n' {
                        j += 1;
                    }
                    // An escaped char ends a run: its value is not decoded.
                    run.token(b'\'', c_like, add);
                    i = j + 1;
                } else {
                    let w = text[i + 1..].chars().next().map_or(1, char::len_utf8);
                    if b.get(i + 1 + w) == Some(&b'\'') {
                        run.lit(&b[i + 1..i + 1 + w], line);
                        i += w + 2;
                    } else {
                        run.token(b'\'', c_like, add);
                        i += 1;
                    }
                }
            }
            c if is_ident(c) => {
                let mut j = i;
                while j < b.len() && is_ident(b[j]) {
                    j += 1;
                }
                let id = &text[i..j];
                let raw_prefix = !c_like && matches!(id, "r" | "br" | "cr");
                if raw_prefix && matches!(b.get(j), Some(b'"' | b'#')) && !(b.get(j) == Some(&b'#') && b.get(j + 1).is_some_and(|&c| is_ident(c))) {
                    let hashes = b[j..].iter().take_while(|&&c| c == b'#').count();
                    let open = j + hashes;
                    if b.get(open) == Some(&b'"') {
                        let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
                        let end = b[open + 1..].windows(close.len()).position(|w| w == close.as_slice()).map_or(b.len(), |k| open + 1 + k);
                        run.lit(&b[open + 1..end], line);
                        add_literal(b[open + 1..end].to_vec(), line, add);
                        let stop = (end + close.len()).min(b.len());
                        line += nl(&b[i..stop]);
                        i = stop;
                        continue;
                    }
                }
                if matches!(id, "b" | "c" | "L" | "u8" | "u" | "U") && b.get(j) == Some(&b'"') {
                    let (v, k) = cooked(b, j + 1, c_like);
                    run.lit(&v, line);
                    add_literal(v, line, add);
                    line += nl(&b[i..k]);
                    i = k;
                    continue;
                }
                if !c_like && id == "concat" && !run.active {
                    let bang = skip_ws(j);
                    let open = skip_ws(bang + 1);
                    if b.get(bang) == Some(&b'!') && matches!(b.get(open), Some(b'(' | b'[' | b'{')) {
                        line += nl(&b[i..open]);
                        run = Run { active: true, depth: 1, ..Run::default() };
                        i = open + 1;
                        continue;
                    }
                }
                if !c_like && (matches!(id, "true" | "false") || id.bytes().all(|d| d.is_ascii_digit())) {
                    // `concat!` writes these as their text.
                    run.lit(id.as_bytes(), line);
                } else {
                    run.token(c, c_like, add);
                }
                if !c.is_ascii_digit() {
                    add(id.as_bytes().to_vec(), line, false);
                }
                i = j;
            }
            c => {
                if !c.is_ascii_whitespace() {
                    run.token(c, c_like, add);
                }
                i += 1;
            }
        }
    }
    run.end(add);
}

/// A quoted literal's value from just after its opening quote, and the
/// index just after its closing quote.
fn cooked(b: &[u8], mut j: usize, c_like: bool) -> (Vec<u8>, usize) {
    let mut v = Vec::new();
    let hex = |s: &[u8]| s.iter().fold(0u32, |a, &c| a * 16 + (c as char).to_digit(16).unwrap_or(0));
    while j < b.len() {
        match b[j] {
            b'"' => return (v, j + 1),
            b'\\' if j + 1 < b.len() => {
                let e = b[j + 1];
                j += 2;
                match e {
                    b'n' => v.push(b'\n'),
                    b'r' => v.push(b'\r'),
                    b't' => v.push(b'\t'),
                    b'a' if c_like => v.push(7),
                    b'b' if c_like => v.push(8),
                    b'f' if c_like => v.push(12),
                    b'v' if c_like => v.push(11),
                    b'0'..=b'7' if c_like => {
                        let mut n = (e - b'0') as u32;
                        for _ in 0..2 {
                            if let Some(d @ b'0'..=b'7') = b.get(j) {
                                n = n * 8 + (d - b'0') as u32;
                                j += 1;
                            }
                        }
                        v.push(n as u8);
                    }
                    b'0' => v.push(0),
                    b'x' => {
                        let k = b[j..].iter().take(2).take_while(|c| c.is_ascii_hexdigit()).count();
                        v.push(hex(&b[j..j + k]) as u8);
                        j += k;
                    }
                    b'u' if !c_like && b.get(j) == Some(&b'{') => {
                        let close = b[j..].iter().position(|&c| c == b'}').map_or(b.len(), |k| j + k);
                        let digits: Vec<u8> = b[j + 1..close].iter().copied().filter(|&c| c != b'_').collect();
                        let ch = char::from_u32(hex(&digits)).unwrap_or('\u{fffd}');
                        v.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                        j = close + 1;
                    }
                    b'\n' | b'\r' if !c_like => {
                        while j < b.len() && b[j].is_ascii_whitespace() {
                            j += 1;
                        }
                    }
                    b'\n' => {}
                    other => v.push(other),
                }
            }
            c => {
                v.push(c);
                j += 1;
            }
        }
    }
    (v, b.len())
}

/// The standard library's source for the toolchain `tree` builds with.
fn toolchain_library(tree: &Path) -> Option<PathBuf> {
    let o = Command::new("rustc").current_dir(tree).args(["--print", "sysroot"]).output().ok()?;
    let lib = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("lib/rustlib/src/rust/library");
    lib.is_dir().then_some(lib)
}

#[cfg(test)]
#[path = "explain_tests.rs"]
mod tests;
