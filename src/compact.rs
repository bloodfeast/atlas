//! Compaction: content-addressed refs, their validation, and the
//! compaction metrics (`docs/design/compaction.md`).
//!
//! A ref names exact text, so a stale one is detected instead of trusted:
//!
//! * `key@hash` - a memory record's version. `hash` is the first
//!   [`HASH_LEN`] hex digits of the version's SHA-256. Versions are
//!   append-only, so every hash ever handed out stays resolvable: a ref to an
//!   older version is *changed* (its text is still returned verbatim), one
//!   that matches no version is *dangling*.
//! * `src:path:a-b@hash` - lines `a` to `b` of a vault or source file, hashed
//!   over those lines joined with `\n` (a CRLF file reads as LF).
//!
//! The replay ([`replay`]) reads a recorded Claude Code transcript and
//! measures each compaction in it: Claude Code's own `preTokens` and
//! `postTokens`, the summary's size and refs, the lookups made again after
//! the boundary, and the tool-result tokens spent before the first edit. It
//! keeps transcript text in memory only and returns numbers.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::error::Result;
use crate::json::Value;
use crate::memory::{MemoryStore, VersionRow};

/// Hex digits of a ref's hash (48 bits).
pub const HASH_LEN: usize = 12;

/// The fewest hex digits a ref's hash may be written with.
pub const MIN_HASH: usize = 8;

/// Tool calls after a boundary in which a lookup made before it counts as a
/// post-compaction miss.
pub const MISS_WINDOW: usize = 40;

/// The ref of a record version: `key@` and the first [`HASH_LEN`] digits of
/// its SHA-256.
pub fn record_ref(key: &str, sha: &str) -> String {
    format!("{key}@{}", sha.get(..HASH_LEN).unwrap_or(sha))
}

/// The hash of lines `a` to `b` (1-based, inclusive) of a text: SHA-256 of
/// the lines joined with `\n`, CR before LF dropped, first [`HASH_LEN`]
/// digits. `None` when the range is empty or past the end.
pub fn lines_hash(text: &str, a: usize, b: usize) -> Option<String> {
    if a == 0 || b < a {
        return None;
    }
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let lines = if text.ends_with('\n') { &lines[..lines.len() - 1] } else { &lines[..] };
    if b > lines.len() {
        return None;
    }
    let h = crate::sha256::hex(lines[a - 1..b].join("\n").as_bytes());
    Some(h[..HASH_LEN].to_string())
}

/// What a ref names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A transcript turn: `turn:<session8>:<line>` (the opt-in transcript
    /// source, [`crate::transcripts`]) or `turn:<session8>:b<offset>` (a
    /// checkpoint's result ref, [`crate::checkpoint::turn_ref`]).
    Turn {
        /// The session id's first 8 characters.
        session8: String,
        /// Where its record starts in the transcript.
        at: TurnAt,
    },
    /// A memory record by key.
    Record(String),
    /// Lines `a` to `b` of `path` in vault or source `src`.
    Lines {
        /// The vault or source.
        src: String,
        /// The file below its root.
        path: String,
        /// First line, 1-based.
        a: usize,
        /// Last line, inclusive.
        b: usize,
    },
}

/// Where a turn ref's record starts in its transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnAt {
    /// Its 1-based line.
    Line(usize),
    /// Its byte offset: what a checkpoint writes, since finding a line
    /// number means counting the whole history before the boundary.
    Byte(u64),
}

/// One ref found in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// The ref as written.
    pub text: String,
    /// What it names.
    pub target: Target,
    /// The hash as written (lowercase hex, [`MIN_HASH`] to 64 digits).
    pub hash: String,
}

fn ref_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/')
}

/// Splits `src:path:a-b` (or `src:path:a`) into its parts. The path must
/// hold a `/` or a `.`, so a record key such as `journal:lane:0007` is not
/// read as lines.
fn lines_target(s: &str) -> Option<Target> {
    let (rest, range) = s.rsplit_once(':')?;
    let (a, b) = match range.split_once('-') {
        Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
        None => {
            let a: usize = range.parse().ok()?;
            (a, a)
        }
    };
    let (src, path) = rest.split_once(':')?;
    if src.is_empty() || path.is_empty() || !(path.contains('/') || path.contains('.')) || a == 0 || b < a {
        return None;
    }
    Some(Target::Lines { src: src.to_string(), path: path.to_string(), a, b })
}

/// `turn:<session8>:<line>` or `turn:<session8>:b<offset>`.
fn turn_target(s: &str) -> Option<Target> {
    let rest = s.strip_prefix("turn:")?;
    let (s8, line) = rest.split_once(':')?;
    if s8.is_empty() || s8.len() > 8 || !s8.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    let at = match line.strip_prefix('b') {
        Some(b) if !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit()) => TurnAt::Byte(b.parse().ok()?),
        Some(_) => return None,
        None => TurnAt::Line(line.parse().ok().filter(|n| *n > 0)?),
    };
    Some(Target::Turn { session8: s8.to_string(), at })
}

/// Parses one ref, `key@hash` or `src:path:a-b@hash`.
pub fn parse_ref(s: &str) -> Option<Ref> {
    let (name, hash) = s.rsplit_once('@')?;
    if name.is_empty() || hash.len() < MIN_HASH || hash.len() > 64 || !hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    if !name.chars().all(ref_char) {
        return None;
    }
    let target = turn_target(name).or_else(|| lines_target(name)).unwrap_or_else(|| Target::Record(name.to_string()));
    Some(Ref { text: s.to_string(), target, hash: hash.to_string() })
}

/// Every ref in a text, in order, each once. A ref is a run of key
/// characters, `@`, and [`MIN_HASH`] or more lowercase hex digits ending at
/// a non-alphanumeric; an e-mail address does not parse (its domain is not
/// hex).
pub fn refs_in(text: &str) -> Vec<Ref> {
    let mut out: Vec<Ref> = Vec::new();
    let bytes = text.as_bytes();
    for (at, _) in text.match_indices('@') {
        let mut s = at;
        while s > 0 && ref_char(bytes[s - 1] as char) {
            s -= 1;
        }
        let mut e = at + 1;
        while e < bytes.len() && bytes[e].is_ascii_alphanumeric() {
            e += 1;
        }
        // Trailing punctuation is not part of the name.
        let name = text[s..at].trim_start_matches(['.', ':', '/', '-']);
        if name.is_empty() {
            continue;
        }
        if let Some(r) = parse_ref(&format!("{name}@{}", &text[at + 1..e])) {
            if !out.iter().any(|x| x.text == r.text) {
                out.push(r);
            }
        }
    }
    out
}

/// How a ref resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// It names the current text.
    Current,
    /// It names an older version; the head's ref is given.
    Changed(String),
    /// Nothing has that hash.
    Dangling,
    /// It could not be checked here (an unknown vault or source).
    Unchecked,
}

/// Resolves a record ref: the status, and the version it names when one
/// does (the head when current, the old version when changed).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn resolve_record(store: &MemoryStore, key: &str, hash: &str) -> Result<(Status, Option<VersionRow>)> {
    let Some(head) = store.head(key)? else { return Ok((Status::Dangling, None)) };
    let versions = store.history(key)?;
    let found = versions.into_iter().rev().find(|v| v.sha.starts_with(hash));
    Ok(match found {
        Some(v) if head.sha.starts_with(hash) => (Status::Current, Some(v)),
        Some(v) => (Status::Changed(record_ref(key, &head.sha)), Some(v)),
        None => (Status::Dangling, None),
    })
}

/// Reads a file below a root, confined as the vault and source reads are:
/// no absolute path, no `..`, no dot file or dot folder and no denied name,
/// neither as given nor as the canonical path it resolves to (a link
/// `notes.txt -> .env` is refused), and the canonical path must stay under
/// the root.
pub fn read_below(root: &Path, rel: &str) -> Option<String> {
    let p = Path::new(rel);
    let clean = |p: &Path| {
        p.components().all(|c| match c {
            std::path::Component::Normal(n) => n.to_str().is_some_and(|n| !n.starts_with('.')),
            _ => false,
        })
    };
    if p.is_absolute() || !clean(p) || crate::sources::is_denied(p.file_name()?.to_str()?) {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let full = std::fs::canonicalize(root.join(p)).ok()?;
    let under = full.strip_prefix(&root).ok()?;
    if !clean(under) || crate::sources::is_denied(under.file_name()?.to_str()?) {
        return None;
    }
    std::fs::read_to_string(full).ok()
}

/// How lines `a` to `b` of `text` (the file a lines ref names) resolve
/// against the ref's hash.
pub fn lines_status(text: &str, src: &str, path: &str, a: usize, b: usize, hash: &str) -> Status {
    match lines_hash(text, a, b) {
        Some(h) if h.starts_with(hash) || hash.starts_with(&h) => Status::Current,
        Some(h) => Status::Changed(format!("{src}:{path}:{a}-{b}@{h}")),
        None => Status::Dangling,
    }
}

/// A root a lines ref may name: a vault, whose `.md` notes it reads, or a
/// read-only source, whose files it reads through the source's own checks
/// (its globs, dot names, the deny-list on the canonical path).
#[derive(Debug, Clone)]
pub enum RefRoot {
    /// A vault: its name and root folder.
    Vault(String, PathBuf),
    /// A read-only source.
    Source(crate::sources::Source),
}

impl RefRoot {
    /// The vault's or source's name.
    pub fn name(&self) -> &str {
        match self {
            RefRoot::Vault(n, _) => n,
            RefRoot::Source(s) => &s.name,
        }
    }

    /// The text of `rel`, when this root serves it.
    pub fn read(&self, rel: &str) -> Option<String> {
        match self {
            // As a vault serves notes: a `.md` file, by the name given and
            // by the name it resolves to.
            RefRoot::Vault(_, root) => {
                let md = |p: &Path| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md"));
                let full = std::fs::canonicalize(root.join(rel)).ok()?;
                (md(Path::new(rel)) && md(&full)).then(|| read_below(root, rel)).flatten()
            }
            RefRoot::Source(s) => s.read(rel).ok().and_then(|(b, _)| String::from_utf8(b).ok()),
        }
    }
}

/// Resolves a lines ref against the named roots (vaults and sources).
pub fn resolve_lines(roots: &[RefRoot], src: &str, path: &str, a: usize, b: usize, hash: &str) -> Status {
    let Some(root) = roots.iter().find(|r| r.name() == src) else { return Status::Unchecked };
    match root.read(path) {
        Some(t) => lines_status(&t, src, path, a, b, hash),
        None => Status::Dangling,
    }
}

/// Resolves any ref.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn resolve(store: &MemoryStore, roots: &[RefRoot], r: &Ref) -> Result<Status> {
    Ok(match &r.target {
        // A chain key is checked by its own resolver against the transcript
        // ([`crate::chain::resolve`]); no record has it.
        Target::Record(_) if crate::chain::parse_key(&r.text).is_some() => Status::Unchecked,
        Target::Record(k) => resolve_record(store, k, &r.hash)?.0,
        Target::Lines { src, path, a, b } => resolve_lines(roots, src, path, *a, *b, &r.hash),
        // A transcript turn is checked against the transcript itself (see
        // `crate::transcripts::check_line`), not here.
        Target::Turn { .. } => Status::Unchecked,
    })
}

/// What a text's refs resolved to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefCheck {
    /// Refs found.
    pub refs: usize,
    /// Refs to older text (`(ref, head ref)`).
    pub changed: Vec<(String, String)>,
    /// Refs that resolve to nothing.
    pub dangling: Vec<String>,
    /// Refs that could not be checked.
    pub unchecked: usize,
    /// Record keys named without a hash (`prefix:rest` whose prefix some
    /// stored key has).
    pub bare: usize,
    /// Of those, keys no record has.
    pub bare_missing: usize,
}

/// Checks every ref in a text, and counts the bare record keys it names.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn check(store: &MemoryStore, roots: &[RefRoot], text: &str) -> Result<RefCheck> {
    let mut c = RefCheck::default();
    let prefixes = key_prefixes(store)?;
    let refs = refs_in(text);
    for r in &refs {
        // `name@hex` that names no record and no key family of the store
        // is someone else's notation (a package version, a commit), not an
        // atlas ref.
        if let Target::Record(k) = &r.target {
            let family = k.split_once(':').is_some_and(|(p, _)| prefixes.contains(p));
            if !family && store.head(k)?.is_none() && crate::chain::parse_key(&r.text).is_none() {
                continue;
            }
        }
        c.refs += 1;
        match resolve(store, roots, r)? {
            Status::Current => {}
            Status::Changed(now) => c.changed.push((r.text.clone(), now)),
            Status::Dangling => c.dangling.push(r.text.clone()),
            Status::Unchecked => c.unchecked += 1,
        }
    }
    let mut seen = HashSet::new();
    for word in text.split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':' | '@'))) {
        if word.contains('@') {
            continue;
        }
        let w = word.trim_end_matches([':', '-']);
        let Some((p, rest)) = w.split_once(':') else { continue };
        if rest.is_empty() || !prefixes.contains(p) || !seen.insert(w.to_string()) {
            continue;
        }
        c.bare += 1;
        if store.head(w)?.is_none() {
            c.bare_missing += 1;
        }
    }
    Ok(c)
}

/// The part before the first `:` of every stored key that has one.
fn key_prefixes(store: &MemoryStore) -> Result<HashSet<String>> {
    let mut st = store.conn().prepare_cached("SELECT DISTINCT substr(key, 1, instr(key, ':') - 1) FROM records WHERE instr(key, ':') > 1")?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(rows)
}

/// The normalized target of a lookup tool call, or `None` for a call that
/// is not a lookup (an edit, a shell command, an agent).
///
/// A Read is its file; a Grep its pattern, path and glob; a Glob its
/// pattern and path; an atlas tool (or any MCP tool) its name and its key,
/// query, path, id, heading, name or pattern. Lowercase, `\` read as `/`.
pub fn lookup_sig(tool: &str, input: &Value) -> Option<String> {
    let f = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let sig = match tool {
        "Read" => format!("read {}", f("file_path")),
        "Grep" => format!("grep {} {} {}", f("pattern"), f("path"), f("glob")),
        "Glob" => format!("glob {} {}", f("pattern"), f("path")),
        t if t.starts_with("mcp__") => {
            let name = t.rsplit("__").next().unwrap_or(t);
            let mut parts = vec![name.to_string()];
            for k in ["vault", "source", "key", "query", "path", "id", "heading", "name", "pattern", "lane"] {
                let v = f(k);
                if !v.is_empty() {
                    parts.push(v);
                }
            }
            if parts.len() == 1 {
                return None;
            }
            parts.join(" ")
        }
        _ => return None,
    };
    let sig = sig.trim().replace('\\', "/").to_lowercase();
    (sig.split_once(' ').is_some_and(|(_, r)| !r.trim().is_empty())).then_some(sig)
}

/// One compaction found in a transcript.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Compaction {
    /// The boundary's time.
    pub time: String,
    /// `manual` or `auto`.
    pub trigger: String,
    /// Claude Code's context size before compaction.
    pub pre_tokens: u64,
    /// Claude Code's context size right after it (0 when not recorded).
    pub post_tokens: u64,
    /// Claude Code's own compaction time.
    pub duration_ms: u64,
    /// The summary's text (kept in memory only).
    pub summary: String,
    /// Lookups made since the previous boundary.
    pub lookups_before: usize,
    /// Lookups in the [`MISS_WINDOW`] calls after the boundary.
    pub lookups_after: usize,
    /// Of those, ones made before the boundary too.
    pub misses: usize,
    /// Of the misses, ones whose target the summary names.
    pub misses_named: usize,
    /// Tool-result bytes from the boundary to the first edit, write or
    /// commit (or the next boundary, or the end).
    pub resume_bytes: usize,
    /// Whether an edit, write or commit was reached.
    pub resumed: bool,
    /// Facts: identifier-like tokens used in the window's tool inputs (not
    /// the file text an edit or write carries) that the compacted segment's
    /// text held.
    pub facts: usize,
    /// Of those, ones the summary holds.
    pub facts_in_summary: usize,
    /// The facts themselves (kept in memory only), for coverage measures.
    pub fact_list: Vec<String>,
    /// Each miss's target as the summary would name it (its last path
    /// component, lowercase).
    pub miss_names: Vec<String>,
    /// Tool-result bytes the misses read again.
    pub miss_bytes: usize,
}

/// A text's identifier-like tokens: runs of at least 6 characters of
/// letters, digits and `_ - . / :`, holding a digit, `/`, `.`, `_` or `:`.
fn fact_tokens(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':')))
        .map(|w| w.trim_matches(['.', ':', '-', '/']))
        .filter(|w| w.len() >= 6 && w.len() <= 120 && w.bytes().any(|b| b.is_ascii_digit() || matches!(b, b'/' | b'.' | b'_' | b':')))
        .collect()
}

/// Tool-input fields that hold text the call writes into a file, or must
/// match a file's current text exactly: code the session authors or copies
/// from a fresh read, not something it has to remember across a compaction.
const AUTHORED: &[&str] = &["content", "old_string", "new_string", "edits", "new_source"];

/// The text of a tool call's input that a fact can come from: its string
/// and number values, one per line, except the [`AUTHORED`] fields. Values
/// are read raw, so JSON escapes (`\\`, `\n`, `\"`) and the field names
/// never make or split a fact.
fn fact_input(input: &Value) -> String {
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Number(n) => {
                out.push_str(&n.to_string());
                out.push('\n');
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(m) => m.iter().filter(|(k, _)| !AUTHORED.contains(&k.as_str())).for_each(|(_, x)| walk(x, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    walk(input, &mut out);
    out
}

/// Which text of a tool call's input a fact may come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactRule {
    /// What a call names: its values except the [`AUTHORED`] fields (the
    /// scored rule).
    Named,
    /// The input serialized as JSON, field names and escapes included (the
    /// rule before the [`AUTHORED`] exclusion; scored beside it only).
    Serialized,
    /// Only the [`AUTHORED`] fields' values: file text an edit or write
    /// carries, and TodoWrite's `content` (the same field name).
    Authored,
}

/// The text of a tool call's input under `rule`.
fn rule_input(input: &Value, rule: FactRule) -> String {
    fn authored(v: &Value, inside: bool, out: &mut String) {
        match v {
            Value::String(s) if inside => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Number(n) if inside => {
                out.push_str(&n.to_string());
                out.push('\n');
            }
            Value::Array(a) => a.iter().for_each(|x| authored(x, inside, out)),
            Value::Object(m) => m.iter().for_each(|(k, x)| authored(x, inside || AUTHORED.contains(&k.as_str()), out)),
            _ => {}
        }
    }
    match rule {
        FactRule::Named => fact_input(input),
        FactRule::Serialized => input.to_string(),
        FactRule::Authored => {
            let mut out = String::new();
            authored(input, false, &mut out);
            out
        }
    }
}

fn content_texts(v: &Value, out: &mut Vec<String>) {
    match v.pointer("/message/content") {
        Some(Value::String(s)) => out.push(s.clone()),
        Some(Value::Array(a)) => {
            for x in a {
                match x.get("type").and_then(Value::as_str) {
                    Some("text") => out.extend(x.get("text").and_then(Value::as_str).map(str::to_string)),
                    Some("tool_result") => match x.get("content") {
                        Some(Value::String(s)) => out.push(s.clone()),
                        Some(Value::Array(c)) => out.extend(c.iter().filter_map(|y| y.get("text").and_then(Value::as_str)).map(str::to_string)),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn is_work(tool: &str, input: &Value) -> bool {
    matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
        || (matches!(tool, "Bash" | "PowerShell") && input.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("git commit") || c.contains("commit -m")))
}

/// The facts of a cut at line `cut` (0-based, the first line not kept) of a
/// transcript whose segment starts at line `start`: identifier-like tokens
/// used in the [`MISS_WINDOW`] tool calls after the cut that the kept
/// segment's visible text held, each once. The same rule as [`replay`]'s
/// facts at a recorded boundary, for any cut.
pub fn facts_at(path: &Path, start: usize, cut: usize) -> Vec<String> {
    facts_at_by(path, start, cut, FactRule::Named).into_iter().map(|(f, _)| f).collect()
}

/// [`facts_at`] under any [`FactRule`], each fact with the tool whose input
/// it first came from.
pub fn facts_at_by(path: &Path, start: usize, cut: usize, rule: FactRule) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    facts_in_by(&text, start, cut, rule)
}

/// [`facts_at_by`] over a transcript's text already read.
pub fn facts_in_by(text: &str, start: usize, cut: usize, rule: FactRule) -> Vec<(String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut segment = String::new();
    for l in lines.iter().take(cut).skip(start) {
        if l.contains("\"isSidechain\":true") || !(l.contains("\"text\"") || l.contains("\"tool_result\"")) {
            continue;
        }
        if let Ok(v) = crate::json::parse(l) {
            let mut t = Vec::new();
            content_texts(&v, &mut t);
            for x in t {
                segment.push_str(&x);
                segment.push('\n');
            }
        }
    }
    let mut used: Vec<(String, String)> = Vec::new();
    for l in lines.iter().skip(cut) {
        if used.len() >= MISS_WINDOW {
            break;
        }
        if l.contains("\"isSidechain\":true") || !l.contains("\"tool_use\"") {
            continue;
        }
        let Ok(v) = crate::json::parse(l) else { continue };
        for b in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") && used.len() < MISS_WINDOW {
                let tool = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                used.push((rule_input(b.get("input").unwrap_or(&Value::Null), rule), tool));
            }
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (u, tool) in &used {
        for t in fact_tokens(u) {
            if seen.insert(t.to_string()) && segment.contains(t) {
                out.push((t.to_string(), tool.clone()));
            }
        }
    }
    out
}

/// Replays one transcript: every compaction in it, oldest first. Lines that
/// are not JSON are skipped; subagent (sidechain) records are ignored.
pub fn replay(path: &Path) -> Vec<Compaction> {
    let Ok(f) = std::fs::File::open(path) else { return Vec::new() };
    let mut out: Vec<Compaction> = Vec::new();
    // Since the previous boundary: lookups and the text the context held.
    let mut seen: HashSet<String> = HashSet::new();
    let mut before: HashSet<String> = HashSet::new();
    let mut segment = String::new();
    let mut prev_segment = String::new();
    // After the current boundary.
    let mut window = 0usize;
    let mut resuming = false;
    let mut awaiting_summary = false;
    let mut used: Vec<String> = Vec::new();
    let mut miss_ids: HashSet<String> = HashSet::new();
    let finish = |c: &mut Compaction, used: &mut Vec<String>, prev: &str| {
        let mut uniq: HashSet<&str> = HashSet::new();
        for u in used.iter() {
            for t in fact_tokens(u) {
                if uniq.insert(t) && prev.contains(t) {
                    c.facts += 1;
                    c.fact_list.push(t.to_string());
                    if c.summary.contains(t) {
                        c.facts_in_summary += 1;
                    }
                }
            }
        }
        used.clear();
    };
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        let boundary = line.contains("\"compact_boundary\"");
        let summary = line.contains("\"isCompactSummary\":true");
        let tool_use = line.contains("\"tool_use\"");
        let text = line.contains("\"text\"") || line.contains("\"tool_result\"");
        if !(boundary || summary || tool_use || text) || line.contains("\"isSidechain\":true") {
            continue;
        }
        let Ok(v) = crate::json::parse(&line) else { continue };
        if boundary && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            if let Some(c) = out.last_mut() {
                finish(c, &mut used, &prev_segment);
            }
            let m = v.get("compactMetadata").cloned().unwrap_or(Value::Null);
            let n = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
            out.push(Compaction {
                time: v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string(),
                trigger: m.get("trigger").and_then(Value::as_str).unwrap_or("").to_string(),
                pre_tokens: n("preTokens"),
                post_tokens: n("postTokens"),
                duration_ms: n("durationMs"),
                lookups_before: seen.len(),
                ..Compaction::default()
            });
            before = std::mem::take(&mut seen);
            prev_segment = std::mem::take(&mut segment);
            window = MISS_WINDOW;
            resuming = true;
            awaiting_summary = true;
            continue;
        }
        if awaiting_summary && v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
            if let Some(c) = out.last_mut() {
                let mut t = Vec::new();
                content_texts(&v, &mut t);
                c.summary = t.join("\n");
            }
            awaiting_summary = false;
            continue;
        }
        let mut texts = Vec::new();
        content_texts(&v, &mut texts);
        let result_bytes: usize = if v.get("type").and_then(Value::as_str) == Some("user") { texts.iter().map(String::len).sum() } else { 0 };
        for t in &texts {
            segment.push_str(t);
            segment.push('\n');
        }
        if resuming {
            if let Some(c) = out.last_mut() {
                c.resume_bytes += result_bytes;
            }
        }
        let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else { continue };
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                if b.get("tool_use_id").and_then(Value::as_str).is_some_and(|id| miss_ids.remove(id)) {
                    let mut t = Vec::new();
                    content_texts(&crate::json!({"message": {"content": [b.clone()]}}), &mut t);
                    if let Some(c) = out.last_mut() {
                        c.miss_bytes += t.iter().map(String::len).sum::<usize>();
                    }
                }
                continue;
            }
            if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let tool = b.get("name").and_then(Value::as_str).unwrap_or("");
            let input = b.get("input").cloned().unwrap_or(Value::Null);
            if resuming && is_work(tool, &input) {
                resuming = false;
                if let Some(c) = out.last_mut() {
                    c.resumed = true;
                }
            }
            let in_window = window > 0;
            if in_window {
                window -= 1;
                used.push(fact_input(&input));
            }
            let Some(sig) = lookup_sig(tool, &input) else { continue };
            if in_window {
                if let Some(c) = out.last_mut() {
                    c.lookups_after += 1;
                    if before.contains(&sig) {
                        c.misses += 1;
                        let target = sig.split_once(' ').map_or("", |x| x.1).trim();
                        let last = target.rsplit('/').next().unwrap_or(target);
                        c.miss_names.push(last.to_string());
                        if let Some(id) = b.get("id").and_then(Value::as_str) {
                            miss_ids.insert(id.to_string());
                        }
                        if !last.is_empty() && c.summary.to_lowercase().contains(last) {
                            c.misses_named += 1;
                        }
                    }
                }
            }
            seen.insert(sig);
        }
    }
    if let Some(c) = out.last_mut() {
        finish(c, &mut used, &prev_segment);
    }
    out
}

/// The compaction log table, in the index database beside the token log.
pub const COMPACT_TABLE: &str = "
CREATE TABLE IF NOT EXISTS compact_log(
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    session TEXT NOT NULL,
    trigger TEXT NOT NULL,
    pre_tokens INTEGER NOT NULL,
    post_tokens INTEGER NOT NULL,
    summary_bytes INTEGER NOT NULL,
    refs INTEGER NOT NULL,
    changed INTEGER NOT NULL,
    dangling INTEGER NOT NULL,
    bare INTEGER NOT NULL,
    bare_missing INTEGER NOT NULL,
    hook_ms REAL NOT NULL
);
";

/// One logged compaction, as SessionStart(compact) saw it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Logged {
    /// The session.
    pub session: String,
    /// `manual` or `auto`.
    pub trigger: String,
    /// Context tokens before.
    pub pre_tokens: u64,
    /// Context tokens right after.
    pub post_tokens: u64,
    /// The summary's bytes.
    pub summary_bytes: usize,
    /// Its refs, checked.
    pub check: RefCheck,
    /// The hook's own time.
    pub hook_ms: f64,
}

/// Logs one compaction.
///
/// # Errors
///
/// [`crate::Error::StoreTooNew`] (nothing written, no table made), or
/// [`crate::Error::Db`].
pub fn log(conn: &Connection, l: &Logged) -> Result<()> {
    // The table is made inside the checked transaction too.
    crate::store::guarded(conn, || {
        conn.execute_batch(COMPACT_TABLE)?;
        conn.execute(
            "INSERT INTO compact_log(time, session, trigger, pre_tokens, post_tokens, summary_bytes, refs, changed, dangling, bare, bare_missing, hook_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                crate::memory::now(),
                l.session,
                l.trigger,
                l.pre_tokens as i64,
                l.post_tokens as i64,
                l.summary_bytes as i64,
                l.check.refs as i64,
                l.check.changed.len() as i64,
                l.check.dangling.len() as i64,
                l.check.bare as i64,
                l.check.bare_missing as i64,
                l.hook_ms
            ],
        )?;
        Ok(())
    })
}

/// The compaction totals for `--token-report`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    /// Compactions.
    pub n: i64,
    /// Sum of pre-compaction tokens.
    pub pre: f64,
    /// Sum of post-compaction tokens, over compactions that record it.
    pub post: f64,
    /// Sum of pre-compaction tokens over those same compactions.
    pub pre_with_post: f64,
    /// Sum of summary tokens.
    pub summary: f64,
    /// Refs, changed, dangling.
    pub refs: i64,
    /// Changed refs.
    pub changed: i64,
    /// Dangling refs.
    pub dangling: i64,
    /// Bare keys and missing bare keys.
    pub bare: i64,
    /// Bare keys no record has.
    pub bare_missing: i64,
    /// Post-compaction misses (`compact:miss` token-log rows).
    pub misses: i64,
    /// The slowest hook time.
    pub max_ms: f64,
}

/// The compaction totals, or `None` when none are logged.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn totals(conn: &Connection, bytes_per_token: f64) -> Result<Option<Totals>> {
    conn.execute_batch(COMPACT_TABLE)?;
    conn.execute_batch(crate::tokens::TOKEN_TABLE)?;
    let mut t = conn.query_row(
        "SELECT COUNT(*), ifnull(SUM(pre_tokens), 0), ifnull(SUM(post_tokens), 0), ifnull(SUM(CASE WHEN post_tokens > 0 THEN pre_tokens ELSE 0 END), 0),
                ifnull(SUM(summary_bytes), 0), ifnull(SUM(refs), 0), ifnull(SUM(changed), 0), ifnull(SUM(dangling), 0), ifnull(SUM(bare), 0),
                ifnull(SUM(bare_missing), 0), ifnull(MAX(hook_ms), 0)
         FROM compact_log",
        [],
        |r| {
            Ok(Totals {
                n: r.get(0)?,
                pre: r.get::<_, i64>(1)? as f64,
                post: r.get::<_, i64>(2)? as f64,
                pre_with_post: r.get::<_, i64>(3)? as f64,
                summary: r.get::<_, i64>(4)? as f64 / bytes_per_token,
                refs: r.get(5)?,
                changed: r.get(6)?,
                dangling: r.get(7)?,
                bare: r.get(8)?,
                bare_missing: r.get(9)?,
                misses: 0,
                max_ms: r.get(10)?,
            })
        },
    )?;
    t.misses = conn.query_row("SELECT COUNT(*) FROM token_log WHERE tool = 'compact:miss'", [], |r| r.get(0))?;
    Ok((t.n > 0 || t.misses > 0).then_some(t))
}

/// The newest compaction recorded in a transcript: its metadata and
/// summary, read from the file's end (at most the last 32 MiB): only the
/// lines from the newest boundary on are read and parsed.
pub fn newest(path: &Path) -> Option<Compaction> {
    let is_boundary = |l: &str| crate::json::parse(l).is_ok_and(|v| v.get("subtype").and_then(Value::as_str) == Some("compact_boundary"));
    let (_, buf) = crate::checkpoint::last_line_where(path, 32 << 20, "\"compact_boundary\"", is_boundary)?;
    let text = String::from_utf8_lossy(&buf);
    let mut found: Option<Compaction> = None;
    for line in text.lines() {
        if line.contains("\"compact_boundary\"") {
            let Ok(v) = crate::json::parse(line) else { continue };
            if v.get("subtype").and_then(Value::as_str) != Some("compact_boundary") {
                continue;
            }
            let m = v.get("compactMetadata").cloned().unwrap_or(Value::Null);
            let n = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
            found = Some(Compaction {
                time: v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string(),
                trigger: m.get("trigger").and_then(Value::as_str).unwrap_or("").to_string(),
                pre_tokens: n("preTokens"),
                post_tokens: n("postTokens"),
                duration_ms: n("durationMs"),
                ..Compaction::default()
            });
        } else if line.contains("\"isCompactSummary\":true") {
            let Ok(v) = crate::json::parse(line) else { continue };
            if let Some(c) = found.as_mut() {
                let mut t = Vec::new();
                content_texts(&v, &mut t);
                c.summary = t.join("\n");
            }
        }
    }
    found
}

/// The ref report injected after a compaction: one line per changed or
/// dangling ref, empty when every ref resolves.
pub fn report_lines(c: &RefCheck) -> String {
    let mut out = String::new();
    for (old, now) in &c.changed {
        out.push_str(&format!("atlas ref changed: {old} -> {now} (recall the old key@hash for its exact text)\n"));
    }
    for d in &c.dangling {
        out.push_str(&format!("atlas ref dangling: {d} (no such version; recall the key without @ for the current text)\n"));
    }
    out
}

/// Lookup targets per session, for counting post-compaction misses live:
/// the PreToolUse hook notes each lookup, PreCompact moves the set aside,
/// and a lookup after it that the moved set holds is a miss.
#[derive(Debug, Default)]
pub struct MissTracker {
    seen: HashMap<String, HashSet<String>>,
    before: HashMap<String, HashSet<String>>,
}

/// The most lookups remembered per session.
const MAX_SEEN: usize = 8192;

impl MissTracker {
    /// Notes a lookup; true when it is a post-compaction miss.
    pub fn lookup(&mut self, session: &str, sig: String) -> bool {
        let miss = self.before.get(session).is_some_and(|b| b.contains(&sig));
        let s = self.seen.entry(session.to_string()).or_default();
        if s.len() < MAX_SEEN {
            s.insert(sig);
        }
        miss
    }

    /// A compaction: what was looked up so far is what the session had.
    pub fn compact(&mut self, session: &str) {
        let s = self.seen.remove(session).unwrap_or_default();
        self.before.insert(session.to_string(), s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_come_from_what_a_call_names_not_the_text_it_writes() {
        let edit = crate::json::parse(r#"{"file_path":"Z:\\dev\\ui\\app.js","old_string":"state.route = r;","new_string":"state.params = p;"}"#).unwrap();
        let bash = crate::json::parse(r#"{"command":"cd ui\ncargo test -p lpt_core","timeout":120000}"#).unwrap();
        let write = crate::json::parse(r#"{"file_path":"notes/css_patch.py","content":"body.in-ws { color: #7b8689 }"}"#).unwrap();
        let toks = |v: &Value| fact_tokens(&fact_input(v)).into_iter().map(str::to_string).collect::<Vec<_>>();
        // The path is a fact; the edit's old and new text are not.
        assert_eq!(toks(&edit), ["app.js"]);
        // A command's lines are read raw: no `ncargo` from an escaped newline, no field names.
        assert_eq!(toks(&bash), ["lpt_core", "120000"]);
        assert_eq!(toks(&write), ["notes/css_patch.py"]);
        // The authored rule reads only those fields, TodoWrite's content too;
        // the serialized rule reads the JSON text, field names and escapes.
        let todo = crate::json::parse(r#"{"todos":[{"content":"run lpt_core tests","status":"pending"}]}"#).unwrap();
        let by = |v: &Value, r: FactRule| fact_tokens(&rule_input(v, r)).into_iter().map(str::to_string).collect::<Vec<_>>();
        let mut authored = by(&edit, FactRule::Authored);
        authored.sort();
        assert_eq!(authored, ["state.params", "state.route"]);
        assert_eq!(by(&todo, FactRule::Authored), ["lpt_core"]);
        assert_eq!(by(&bash, FactRule::Named), toks(&bash));
        assert!(by(&edit, FactRule::Serialized).contains(&"file_path".to_string()));
    }

    #[test]
    fn refs_parse_and_skip_emails() {
        let r = refs_in("see rules:r00412@3f9c2a1b7e40, and vault:notes/a.md:3-5@0123abcd. mail me@example.com or x@deadbeef");
        let t: Vec<&str> = r.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(t, ["rules:r00412@3f9c2a1b7e40", "vault:notes/a.md:3-5@0123abcd", "x@deadbeef"]);
        assert_eq!(r[0].target, Target::Record("rules:r00412".into()));
        assert_eq!(r[1].target, Target::Lines { src: "vault".into(), path: "notes/a.md".into(), a: 3, b: 5 });
        // A journal key's numeric tail is not a line range.
        assert_eq!(parse_ref("journal:lane:0007@0123abcd").unwrap().target, Target::Record("journal:lane:0007".into()));
        assert!(parse_ref("k@0123abc").is_none());
        assert!(parse_ref("k@0123ABCD").is_none());
    }

    #[test]
    fn a_chain_key_a_summary_cites_is_unchecked_not_dangling() {
        let d = std::env::temp_dir().join(format!("atlas-compact-chain-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let store = MemoryStore::open(&d.join("memory.sqlite")).unwrap();
        for k in ["turn:43f55b2e:b10-b20@0123456789ab", "compact:43f55b2e:b10@0123456789ab"] {
            let r = parse_ref(k).unwrap();
            assert_eq!(resolve(&store, &[], &r).unwrap(), Status::Unchecked, "{k}");
        }
        let c = check(&store, &[], "state: turn:43f55b2e:b10-b20@0123456789ab, compact:43f55b2e:b10@0123456789ab").unwrap();
        assert_eq!((c.refs, c.unchecked), (2, 2), "{c:?}");
        assert!(c.dangling.is_empty(), "{c:?}");
        // Still a record to the check: a key of no family is someone else's notation.
        assert_eq!(check(&store, &[], "pkg:left-pad@0123456789ab").unwrap().refs, 0);
        // The short form the card prints is no plain ref: the check does not see it.
        assert_eq!(check(&store, &[], "turn:43f5:b10-b20@012345").unwrap().refs, 0);
        drop(store);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lines_hash_reads_crlf_as_lf() {
        assert_eq!(lines_hash("a\r\nb\r\nc\r\n", 1, 2), lines_hash("a\nb\nc\n", 1, 2));
        assert_ne!(lines_hash("a\nb\nc\n", 1, 2), lines_hash("a\nb\nc\n", 2, 3));
        assert_eq!(lines_hash("a\nb\n", 2, 3), None);
        assert_eq!(lines_hash("a\nb\n", 0, 1), None);
    }

    #[test]
    fn lookups_normalize() {
        let v = crate::json::parse(r#"{"file_path": "C:\\Src\\Main.rs"}"#).unwrap();
        assert_eq!(lookup_sig("Read", &v).as_deref(), Some("read c:/src/main.rs"));
        assert_eq!(lookup_sig("Edit", &v), None);
        let q = crate::json::parse(r#"{"key": "rules:r1"}"#).unwrap();
        assert_eq!(lookup_sig("mcp__plugin_atlas_atlas__recall", &q).as_deref(), Some("recall rules:r1"));
        assert_eq!(lookup_sig("Read", &Value::Null), None);
    }

    #[test]
    fn misses_count_only_after_a_compaction() {
        let mut m = MissTracker::default();
        assert!(!m.lookup("s", "read a".into()));
        assert!(!m.lookup("s", "read a".into()));
        m.compact("s");
        assert!(m.lookup("s", "read a".into()));
        assert!(!m.lookup("s", "read b".into()));
        assert!(!m.lookup("t", "read a".into()));
    }

    /// Review finding 3: the canonical target of a lines ref is checked, as
    /// `path_denied` checks it, not only the name given.
    #[test]
    fn read_below_refuses_dot_names_and_what_a_link_resolves_to() {
        let root = std::env::temp_dir().join(format!("atlas-read-below-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("config"), "secret
").unwrap();
        std::fs::write(root.join("plain.md"), "plain
").unwrap();
        assert_eq!(read_below(&root, "plain.md").as_deref(), Some("plain
"));
        assert_eq!(read_below(&root, ".git/config"), None);
        assert_eq!(read_below(&root, "../x"), None);
        // A folder link to the dot folder: a junction on Windows needs no privilege.
        #[cfg(windows)]
        let linked = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(root.join("pub")).arg(root.join(".git")).output().is_ok_and(|o| o.status.success());
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(root.join(".git"), root.join("pub")).is_ok();
        if linked {
            assert_eq!(read_below(&root, "pub/config"), None, "the link resolves into a dot folder");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod storeguard_tests {
    use super::*;
    use crate::store::skew_support::{dump, make_index_newer};

    /// A compaction logged on a store a newer atlas upgraded since this one
    /// opened it writes no row and makes no table.
    #[test]
    fn the_compaction_log_checks_the_version_under_the_lock() {
        let c = Connection::open_in_memory().unwrap();
        make_index_newer(&c);
        let before = dump(&c);
        let e = log(&c, &Logged::default()).expect_err("refused");
        assert!(e.is_store_too_new(), "{e}");
        assert!(c.is_autocommit(), "rolled back");
        assert_eq!(dump(&c), before, "no table made, no row");
    }
}
