//! Keeping the memory store in step with the files it was imported from.
//!
//! [`super::import`] filled the store once from a rules file (INTENT.md, an
//! `items` export) and a memory folder (a `memory` export). Those files keep
//! changing by hand. On every rescan that sees one of them change, the
//! store is brought back in line with it, without losing history:
//!
//! * an item or memory file new in the source becomes a new record, with the
//!   date and author the item states (`- 2026-11-02 Someone: ...`);
//! * an item whose text changed is superseded: the new text is a new record
//!   placed right after the old one, and the old one stays answerable,
//!   marked superseded. A memory file keeps its key (its file name), so a
//!   changed file is a new version of the same record (`recall {key,
//!   history}` shows every one);
//! * an item or memory file removed from the source retires its record: it
//!   keeps every version and leaves the export, and recall by query no
//!   longer offers it;
//! * a record with no source item (written through `remember` or
//!   `supersede`) is never touched, and neither is one a tool already
//!   superseded.
//!
//! Line endings are not text, and an index line that is a prefix of a
//! memory's stored title is the title cut for length (as an export's
//! `index_cap` cuts it), so neither is a change.
//!
//! An export maps to the configured source of the same name: an `items`
//! export to the file of its export path's name under that source's root,
//! a `memory` export to the root itself. An export that is written over its
//! own source is the source of truth there and is not synced. `headings`
//! exports (handoff files) are not synced.
//!
//! [`drift`] reports, without writing, what a sync would do: `--recall-check`
//! counts every line as a miss.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::error::{Error, Result};

use super::export::{ExportSpec, Shape};
use super::import::{front_value, piece_title, split_index, split_items, split_memory_file, Piece, PieceKind};
use super::settings::Settings;
use super::tree::{self, TreeSpec};
use super::{now, version_sha, Draft, MemoryStore, Place, Provenance};

/// The client recorded on every version a sync writes.
pub const CLIENT: &str = "atlas sync";

/// The least word overlap (Dice coefficient) for an unmatched source item to
/// count as an edit of an unmatched record rather than a new item.
pub const EDIT_SIMILARITY: f64 = 0.5;

/// One export and the file or folder it is synced from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The export.
    pub spec: ExportSpec,
    /// The source it maps to.
    pub source: String,
    /// The file (`items`) or folder (`memory`).
    pub path: PathBuf,
    /// Other memory folders whose memories marked `scope: user` this target
    /// also imports (the user layer's target only; see [`crate::project`]).
    pub user_from: Vec<PathBuf>,
    /// The memory folder's index is a tree (see [`super::tree`]): the sync
    /// reads its memories' lines in tree order, and a rescan rebuilds it.
    pub tree: Option<TreeSpec>,
}

/// Whether two paths name one file, compared canonical (a link, `\\?\` or
/// case aside), also when neither exists yet.
fn same_file(a: &Path, b: &Path) -> bool {
    crate::project::path_key(a) == crate::project::path_key(b)
}

/// The exports that are synced, given the configured sources as `(name,
/// root)`.
pub fn targets(settings: &Settings, sources: &[(String, PathBuf)]) -> Vec<Target> {
    let mut out = Vec::new();
    for spec in &settings.exports {
        let Some((name, root)) = sources.iter().find(|(n, _)| *n == spec.name) else { continue };
        let path = match spec.shape {
            Shape::Items => match spec.path.file_name() {
                Some(f) => root.join(f),
                None => continue,
            },
            Shape::MemoryDir => root.clone(),
            Shape::Headings => continue,
        };
        if settings.export_dir.as_ref().is_some_and(|d| same_file(&d.join(&spec.path), &path)) {
            continue;
        }
        let user_from = if spec.name == crate::project::USER_EXPORT && spec.shape == Shape::MemoryDir { settings.user_scope_from.clone() } else { Vec::new() };
        let tree = settings.tree.clone().filter(|t| t.export == spec.name && spec.shape == Shape::MemoryDir);
        out.push(Target { spec: spec.clone(), source: name.clone(), path, user_from, tree });
    }
    out
}

/// The files a target reads, relative to its source root: the one file of
/// an `items` export; the `.md` files directly in a memory folder (its index
/// among them).
pub fn target_files(t: &Target) -> Result<Vec<String>> {
    match t.spec.shape {
        Shape::MemoryDir => {
            let mut v: Vec<String> = std::fs::read_dir(&t.path)?
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|f| f.is_file()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|f| f.ends_with(".md"))
                .collect();
            v.sort();
            Ok(v)
        }
        _ => Ok(t.path.file_name().map(|f| vec![f.to_string_lossy().into_owned()]).unwrap_or_default()),
    }
}

/// The fingerprint of a set of files: SHA-256 over `path TAB sha256 LF`
/// lines in path order. The rescan gate compares it with the one stored at
/// the last sync, from the index's hashes, without reading a file.
pub fn fingerprint(files: &[(String, String)]) -> String {
    let mut v: Vec<&(String, String)> = files.iter().collect();
    v.sort();
    let mut h = crate::sha256::Hasher::new();
    for (p, s) in v {
        h.update(p.as_bytes());
        h.update(b"\t");
        h.update(s.as_bytes());
        h.update(b"\n");
    }
    h.hex()
}

/// The `mem_meta` key holding a target's fingerprint at its last sync.
pub fn meta_key(export: &str) -> String {
    format!("sync:{export}")
}

/// What one change does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// A new record.
    New,
    /// A changed item (superseded) or memory file (a new version).
    Changed,
    /// Its source item is gone: retired.
    Removed,
    /// A retired memory whose file is back: active again.
    Returned,
    /// Put into the export (a memory newly listed in the index).
    Placed,
    /// Taken out of the export (a memory no longer listed).
    Unplaced,
}

impl Change {
    /// The word drift lines use.
    pub fn word(self) -> &'static str {
        match self {
            Change::New => "new",
            Change::Changed => "changed",
            Change::Removed => "removed",
            Change::Returned => "returned",
            Change::Placed => "placed",
            Change::Unplaced => "unplaced",
        }
    }
}

/// A record's text as the source has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    /// Title.
    pub title: String,
    /// Metadata (a memory's frontmatter).
    pub meta: String,
    /// Body.
    pub body: String,
    /// The file, relative to the source root.
    pub path: String,
    /// The 1-based line it starts on.
    pub line: usize,
    /// The author the text states, if any.
    pub author: Option<String>,
    /// The date the text states (UTC, as [`now`] writes it), if any.
    pub date: Option<String>,
}

/// One change a sync makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    /// What it does.
    pub change: Change,
    /// The record it touches: the existing one, or a memory's new key.
    pub key: Option<String>,
    /// The record kind (`rule`, `block`, `memory`).
    pub kind: String,
    /// The sub-kind (a memory's type).
    pub sub: String,
    /// The text, for changes that write one.
    pub text: Option<Text>,
    /// The slot of the source sequence the record takes in the export.
    pub slot: Option<usize>,
}

/// What a sync would do to one export.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The export.
    pub export: String,
    /// The source.
    pub source: String,
    /// The changes, in source order.
    pub ops: Vec<Op>,
    /// The fingerprint of the files read.
    pub fingerprint: String,
    /// The export's sequence in source order: the key of the record already
    /// placed there, or `None` where an op places one.
    slots: Vec<Option<String>>,
    /// SHA-256 of each file read.
    shas: HashMap<String, String>,
}

impl Plan {
    /// One line per change.
    pub fn lines(&self) -> Vec<String> {
        self.ops
            .iter()
            .map(|o| {
                let at = o.text.as_ref().map(|t| format!(" from {}:{}:{}", self.source, t.path, t.line)).unwrap_or_default();
                let what = match (&o.key, &o.text) {
                    (Some(k), _) => k.clone(),
                    (None, Some(t)) => format!("{} {:?}", o.kind, t.title.chars().take(60).collect::<String>()),
                    _ => o.kind.clone(),
                };
                format!("{} {} {what}{at}", self.export, o.change.word())
            })
            .collect()
    }
}

/// What a sync did to one export.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The export.
    pub export: String,
    /// Records created.
    pub new: usize,
    /// Items superseded or memory files given a new version.
    pub changed: usize,
    /// Records retired.
    pub removed: usize,
    /// Records active again.
    pub returned: usize,
    /// Placement changes alone.
    pub placed: usize,
    /// Wall time in microseconds, reading the files included.
    pub elapsed_us: u128,
}

crate::impl_to_json!(Report { export, new, changed, removed, returned, placed, elapsed_us });

fn read_utf8(path: &Path) -> Result<String> {
    let b = std::fs::read(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    String::from_utf8(b).map_err(|_| Error::InvalidArgument(format!("{} is not UTF-8", path.display())))
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10 && b[..10].iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

/// A stated date as a version time: never in the future, and only a date
/// that parses.
fn stated_time(s: &str) -> Option<String> {
    if !is_date(s) {
        return None;
    }
    let t = if s.len() >= 20 && s.ends_with('Z') && s.as_bytes()[10] == b'T' { s.to_string() } else { format!("{}T00:00:00.000Z", &s[..10]) };
    (t[..10] <= now()[..10]).then_some(t)
}

/// The date and author an item states: `- YYYY-MM-DD Name: ...` (also
/// `Name, ...` and `Name on ...`). A name is one capitalised word, not all
/// capitals (`REVERSED` is a marker, not an author).
pub fn item_attribution(text: &str) -> (Option<String>, Option<String>) {
    let t = text.strip_prefix("- ").unwrap_or(text);
    if !is_date(t) {
        return (None, None);
    }
    let date = stated_time(&t[..10]);
    let rest = &t[10..];
    let author = rest.strip_prefix(' ').and_then(|r| {
        let end = r.find([':', ',', ' ']).unwrap_or(r.len());
        let w = &r[..end];
        let ok = w.len() > 1
            && w.chars().all(char::is_alphabetic)
            && w.chars().next().is_some_and(char::is_uppercase)
            && !w.chars().all(char::is_uppercase);
        ok.then(|| w.to_string())
    });
    (date, author)
}

fn words(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).map(str::to_lowercase).collect()
}

/// Word overlap of two texts (Dice coefficient over distinct words).
pub fn similarity(a: &str, b: &str) -> f64 {
    let (x, y) = (words(a), words(b));
    if x.is_empty() && y.is_empty() {
        return 1.0;
    }
    2.0 * x.intersection(&y).count() as f64 / (x.len() + y.len()) as f64
}

/// Row ids of records whose first version came from this export (imported
/// or synced): the only records a sync may supersede or retire.
fn from_source(store: &MemoryStore, export: &str) -> Result<HashSet<i64>> {
    let mut st = store.conn().prepare_cached("SELECT v.rid FROM versions v WHERE v.seq = 1 AND v.src = ?1")?;
    let rows = st.query_map([export], |r| r.get(0))?.collect::<rusqlite::Result<HashSet<i64>>>()?;
    Ok(rows)
}

fn piece_text(p: &Piece, path: &str) -> Text {
    Text { title: piece_title(p), meta: String::new(), body: p.text.clone(), path: path.to_string(), line: p.line, author: None, date: None }
}

/// The hash two texts share when they differ at most in line endings: a
/// file saved with CRLF instead of LF says nothing new.
fn same_text(title: &str, meta: &str, body: &str) -> String {
    let n = |s: &str| s.replace("\r\n", "\n");
    version_sha(n(title).trim_end_matches('\r'), &n(meta), &n(body))
}

/// Matches source pieces against the export's placed records of the same
/// kinds by exact text (title, meta and body), first come first served.
/// Returns, per piece, the matched record's index into `heads`.
fn match_exact(texts: &[Text], heads: &[super::Head]) -> Vec<Option<usize>> {
    let mut by_sha: HashMap<String, VecDeque<usize>> = HashMap::new();
    for (i, h) in heads.iter().enumerate() {
        by_sha.entry(same_text(&h.title, &h.meta, &h.body)).or_default().push_back(i);
    }
    texts.iter().map(|t| by_sha.get_mut(&same_text(&t.title, &t.meta, &t.body)).and_then(VecDeque::pop_front)).collect()
}

/// Plans the sync of one export from its files, read now.
///
/// # Errors
///
/// [`Error::Io`] or [`Error::InvalidArgument`] for an unreadable or
/// non-UTF-8 file; [`Error::Db`].
pub fn plan(store: &MemoryStore, t: &Target) -> Result<Plan> {
    let files = target_files(t)?;
    let mut texts: HashMap<String, String> = HashMap::new();
    let root = if t.spec.shape == Shape::MemoryDir { t.path.clone() } else { t.path.parent().map(Path::to_path_buf).unwrap_or_default() };
    for f in &files {
        texts.insert(f.clone(), read_utf8(&root.join(f))?);
    }
    if t.spec.shape == Shape::MemoryDir && t.spec.name == crate::project::USER_EXPORT {
        user_layer_texts(store, t, &mut texts)?;
    }
    let shas: HashMap<String, String> = texts.iter().map(|(p, s)| (p.clone(), crate::sha256::hex(s.as_bytes()))).collect();
    let fp = fingerprint(&shas.iter().map(|(a, b)| (a.clone(), b.clone())).collect::<Vec<_>>());
    // A generated tree root lists branches, not memories: the sync reads
    // every memory's line in tree order instead.
    if let (Some(tr), Shape::MemoryDir) = (&t.tree, t.spec.shape) {
        if let Some(flat) = tree::flat_index(&root, &t.spec.index, tr)? {
            texts.insert(t.spec.index.clone(), flat);
        }
    }
    let mut plan = Plan { export: t.spec.name.clone(), source: t.source.clone(), ops: Vec::new(), fingerprint: fp, slots: Vec::new(), shas };
    match t.spec.shape {
        Shape::Items => {
            let rel = files.first().cloned().unwrap_or_default();
            let text = texts.get(&rel).cloned().unwrap_or_default();
            plan_items(store, &t.spec, &rel, &text, &mut plan)?;
        }
        Shape::MemoryDir => plan_memory(store, &t.spec, &texts, &mut plan)?,
        Shape::Headings => {}
    }
    Ok(plan)
}

fn plan_items(store: &MemoryStore, spec: &ExportSpec, rel: &str, text: &str, plan: &mut Plan) -> Result<()> {
    let pieces = split_items(text);
    let heads = store.heads_by_rid(&store.placement(&spec.name)?)?;
    let eligible = from_source(store, &spec.name)?;
    let texts: Vec<Text> = pieces.iter().map(|p| piece_text(p, rel)).collect();
    let matched = match_exact(&texts, &heads);
    let mut used = vec![false; heads.len()];
    for m in matched.iter().flatten() {
        used[*m] = true;
    }
    plan.slots = matched.iter().map(|m| m.map(|i| heads[i].key.clone())).collect();
    // Unmatched records this sync owns, by kind.
    let open: Vec<usize> = (0..heads.len()).filter(|&i| !used[i] && heads[i].status == "active" && eligible.contains(&heads[i].rid)).collect();
    // Edits: an unmatched item and an unmatched rule close enough in words,
    // best pairs first.
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (pi, p) in pieces.iter().enumerate() {
        if matched[pi].is_some() || p.kind != PieceKind::Item {
            continue;
        }
        for &hi in &open {
            if heads[hi].kind != "rule" {
                continue;
            }
            let s = similarity(&p.text, &heads[hi].body);
            if s >= EDIT_SIMILARITY {
                pairs.push((s, pi, hi));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut edit_of: HashMap<usize, usize> = HashMap::new();
    let mut taken: HashSet<usize> = HashSet::new();
    for (_, pi, hi) in pairs {
        if !edit_of.contains_key(&pi) && !taken.contains(&hi) {
            edit_of.insert(pi, hi);
            taken.insert(hi);
        }
    }
    for (pi, p) in pieces.iter().enumerate() {
        if matched[pi].is_some() {
            continue;
        }
        let mut tx = texts[pi].clone();
        let kind = if p.kind == PieceKind::Item { "rule" } else { "block" };
        if p.kind == PieceKind::Item {
            let (date, author) = item_attribution(&p.text);
            tx.author = author;
            // A new item takes the date it states; an edit is dated now.
            tx.date = if edit_of.contains_key(&pi) { None } else { date };
        }
        match edit_of.get(&pi) {
            Some(&hi) => plan.ops.push(Op { change: Change::Changed, key: Some(heads[hi].key.clone()), kind: kind.into(), sub: String::new(), text: Some(tx), slot: Some(pi) }),
            None => plan.ops.push(Op { change: Change::New, key: None, kind: kind.into(), sub: String::new(), text: Some(tx), slot: Some(pi) }),
        }
    }
    for &hi in &open {
        if !taken.contains(&hi) {
            plan.ops.push(Op { change: Change::Removed, key: Some(heads[hi].key.clone()), kind: heads[hi].kind.clone(), sub: String::new(), text: None, slot: None });
        }
    }
    Ok(())
}

/// A synced memory's title. A listed memory's title is its index line, but
/// an index line that is a prefix of the stored title is the title cut for
/// length (as an export's `index_cap` cuts it), not a new title; a memory
/// no longer listed keeps the title it had.
pub fn memory_title(stored: &str, line: &str, listed: bool) -> String {
    let (s, l) = (stored.trim_end_matches('\r'), line.trim_end_matches('\r'));
    if !listed || s == l || (s.len() > l.len() && s.starts_with(l)) {
        stored.to_string()
    } else {
        line.to_string()
    }
}

fn plan_memory(store: &MemoryStore, spec: &ExportSpec, texts: &HashMap<String, String>, plan: &mut Plan) -> Result<()> {
    let index = texts.get(&spec.index).ok_or_else(|| Error::NotFound(format!("{}: no index {}", spec.name, spec.index)))?;
    let placed_rids = store.placement(&spec.name)?;
    let placed_heads = store.heads_by_rid(&placed_rids)?;
    let placed_keys: HashSet<String> = placed_heads.iter().map(|h| h.key.clone()).collect();
    let blocks: Vec<super::Head> = placed_heads.iter().filter(|h| h.kind == "block").cloned().collect();
    let eligible = from_source(store, &spec.name)?;
    // The index in order: memory entries (a listed file that exists, first
    // listing only) and blocks (every other line), as the import split it.
    let mut seen: HashSet<String> = HashSet::new();
    enum Slot {
        Memory(String, Text),
        Block(Text),
    }
    let mut seq: Vec<Slot> = Vec::new();
    for p in split_index(index) {
        let file = format!("{}.md", p.target);
        if p.kind == PieceKind::Entry && texts.contains_key(&file) && seen.insert(p.target.clone()) {
            let Some(title) = p.text.strip_suffix('\n') else {
                return Err(Error::InvalidArgument(format!("{}: line {} has no line break", spec.index, p.line)));
            };
            let body = &texts[&file];
            let (meta, rest) = split_memory_file(body);
            let tx = Text { title: title.to_string(), meta: meta.to_string(), body: rest.to_string(), path: file, line: 1, author: None, date: None };
            seq.push(Slot::Memory(p.target.clone(), tx));
        } else {
            seq.push(Slot::Block(piece_text(&p, &spec.index)));
        }
    }
    // Index blocks match placed blocks by text.
    let block_texts: Vec<Text> = seq.iter().filter_map(|s| if let Slot::Block(t) = s { Some(t.clone()) } else { None }).collect();
    let block_match = match_exact(&block_texts, &blocks);
    let mut used = vec![false; blocks.len()];
    for m in block_match.iter().flatten() {
        used[*m] = true;
    }
    // Memory files the index does not list: synced, not placed.
    let mut extra: Vec<(String, Text)> = Vec::new();
    for (f, body) in texts {
        let Some(stem) = f.strip_suffix(".md") else { continue };
        if *f == spec.index || seen.contains(stem) {
            continue;
        }
        let (meta, rest) = split_memory_file(body);
        let desc = front_value(meta, "description").unwrap_or("");
        let tx = Text { title: format!("- [{stem}]({stem}.md) - {desc}"), meta: meta.to_string(), body: rest.to_string(), path: f.clone(), line: 1, author: None, date: None };
        extra.push((stem.to_string(), tx));
    }
    extra.sort_by(|a, b| a.0.cmp(&b.0));
    let mut bi = 0usize;
    let memory_op = |plan: &mut Plan, stem: &str, mut tx: Text, slot: Option<usize>| -> Result<()> {
        let sub = front_value(&tx.meta, "type").unwrap_or("").to_string();
        let listed = slot.is_some();
        let head = store.head(stem)?;
        if let Some(h) = &head {
            // A `slug` tree line names no title: the stored one stands.
            let slug_shaped = tree::link(&tx.title).is_some_and(|(t, target, _)| target == format!("{stem}.md") && t == stem);
            tx.title = if slug_shaped && !h.title.is_empty() { h.title.clone() } else { memory_title(&h.title, &tx.title, listed) };
        }
        let sha = same_text(&tx.title, &tx.meta, &tx.body);
        match head {
            None => {
                let mut tx = tx;
                tx.date = front_value(&tx.meta, "modified").and_then(stated_time);
                plan.ops.push(Op { change: Change::New, key: Some(stem.to_string()), kind: "memory".into(), sub, text: Some(tx), slot });
            }
            Some(h) if h.kind != "memory" || h.status == "superseded" => {}
            Some(h) if h.status == "retired" => {
                plan.ops.push(Op { change: Change::Returned, key: Some(h.key.clone()), kind: "memory".into(), sub, text: (same_text(&h.title, &h.meta, &h.body) != sha).then_some(tx), slot });
            }
            Some(h) => {
                if same_text(&h.title, &h.meta, &h.body) != sha {
                    plan.ops.push(Op { change: Change::Changed, key: Some(h.key.clone()), kind: "memory".into(), sub: sub.clone(), text: Some(tx), slot: None });
                }
                let is_placed = placed_keys.contains(&h.key);
                if listed && !is_placed {
                    plan.ops.push(Op { change: Change::Placed, key: Some(h.key.clone()), kind: "memory".into(), sub, text: None, slot });
                } else if !listed && is_placed {
                    plan.ops.push(Op { change: Change::Unplaced, key: Some(h.key.clone()), kind: "memory".into(), sub, text: None, slot: None });
                }
            }
        }
        Ok(())
    };
    for (i, s) in seq.into_iter().enumerate() {
        match s {
            Slot::Block(tx) => {
                match block_match[bi] {
                    Some(m) => plan.slots.push(Some(blocks[m].key.clone())),
                    None => {
                        plan.slots.push(None);
                        plan.ops.push(Op { change: Change::New, key: None, kind: "block".into(), sub: String::new(), text: Some(tx), slot: Some(i) });
                    }
                }
                bi += 1;
            }
            Slot::Memory(stem, tx) => {
                plan.slots.push(placed_keys.contains(&stem).then(|| stem.clone()));
                memory_op(plan, &stem, tx, Some(i))?;
            }
        }
    }
    for (stem, tx) in extra {
        memory_op(plan, &stem, tx, None)?;
    }
    // Retired: index blocks gone from the index, memory files gone from the
    // folder (only records this export imported or synced).
    for (i, b) in blocks.iter().enumerate() {
        if !used[i] && b.status == "active" && eligible.contains(&b.rid) {
            plan.ops.push(Op { change: Change::Removed, key: Some(b.key.clone()), kind: "block".into(), sub: String::new(), text: None, slot: None });
        }
    }
    for h in store.heads(Some("memory"), None)? {
        if h.status == "active" && eligible.contains(&h.rid) && !texts.contains_key(&format!("{}.md", h.key)) {
            plan.ops.push(Op { change: Change::Removed, key: Some(h.key.clone()), kind: "memory".into(), sub: h.sub.clone(), text: None, slot: None });
        }
    }
    Ok(())
}

/// The user layer's extra texts and its guard: memories marked `scope: user`
/// in the other projects' folders join the folder's own, and a file whose
/// key the store already holds from another source (the project's own
/// memory of that name) is left out, so the user layer never overwrites a
/// project's record.
fn user_layer_texts(store: &MemoryStore, t: &Target, texts: &mut HashMap<String, String>) -> Result<()> {
    for dir in &t.user_from {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut names: Vec<String> = rd.flatten().filter(|e| e.file_type().is_ok_and(|f| f.is_file())).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for f in names {
            if !f.ends_with(".md") || f == t.spec.index || texts.contains_key(&f) {
                continue;
            }
            let Ok(text) = read_utf8(&dir.join(&f)) else { continue };
            let (meta, _) = split_memory_file(&text);
            if front_value(meta, "scope") == Some("user") {
                texts.insert(f, text);
            }
        }
    }
    let mine = from_source(store, &t.spec.name)?;
    let mut foreign = Vec::new();
    for f in texts.keys().filter(|f| **f != t.spec.index) {
        let Some(stem) = f.strip_suffix(".md") else { continue };
        if let Some(h) = store.head(stem)? {
            if !mine.contains(&h.rid) {
                foreign.push(f.clone());
            }
        }
    }
    for f in foreign {
        texts.remove(&f);
    }
    Ok(())
}

/// Applies a plan in one write batch and records its fingerprint.
///
/// # Errors
///
/// [`Error::Db`] (nothing is written), or as the writer's calls.
pub fn apply(store: &mut MemoryStore, plan: &Plan) -> Result<Report> {
    let mut rep = Report { export: plan.export.clone(), ..Report::default() };
    let export = plan.export.as_str();
    let mut placed: HashSet<String> = store.heads_by_rid(&store.placement(export)?)?.into_iter().map(|h| h.key).collect();
    let mut slots = plan.slots.clone();
    let stamp = now();
    let mut next: HashMap<&'static str, u64> = HashMap::new();
    let mut w = store.writer()?;
    let mut fresh_key = |w: &super::Writer<'_>, kind: &str| -> Result<String> {
        let (tag, prefix) = if kind == "rule" { ("r", "rule") } else { ("b", "block") };
        let n = match next.get(prefix) {
            Some(n) => *n,
            None => {
                let k = w.next_key(&format!("{export}:{tag}"), 5)?;
                k[export.len() + 1 + tag.len()..].parse::<u64>().unwrap_or(1)
            }
        };
        next.insert(prefix, n + 1);
        Ok(format!("{export}:{tag}{n:05}"))
    };
    let anchor = |slots: &[Option<String>], placed: &HashSet<String>, at: usize| -> Place {
        slots[..at.min(slots.len())].iter().rev().flatten().find(|k| placed.contains(*k)).map_or(Place::Start, |k| Place::After(k.clone()))
    };
    let draft = |tx: &Text, reason: String, dated: bool| Draft {
        title: tx.title.clone(),
        meta: tx.meta.clone(),
        body: tx.body.clone(),
        reason,
        author: tx.author.clone().unwrap_or_else(|| "sync".into()),
        client: CLIENT.into(),
        provenance: Some(Provenance { src: plan.export.clone(), path: tx.path.clone(), line: tx.line as i64, sha: plan.shas.get(&tx.path).cloned().unwrap_or_default() }),
        time: if dated { tx.date.clone() } else { None },
    };
    // A memory of the user layer is scope "user" in every project, as one
    // written there with `remember`; other memories have none.
    let scope = |kind: &str| match kind {
        "memory" if plan.export == crate::project::USER_EXPORT => crate::project::USER_EXPORT.to_string(),
        "memory" => String::new(),
        _ => plan.export.clone(),
    };
    // Retire and unplace first, so no placement anchors on a record leaving.
    for op in &plan.ops {
        let Some(key) = &op.key else { continue };
        match op.change {
            Change::Removed => {
                w.retire(key, &format!("synced {stamp}: no longer in {}", plan.source))?;
                placed.remove(key);
                rep.removed += 1;
            }
            Change::Unplaced => {
                w.unplace_record(export, key)?;
                placed.remove(key);
                rep.placed += 1;
            }
            _ => {}
        }
    }
    for op in &plan.ops {
        let from = |tx: &Text, why: &str| format!("synced {stamp} from {}:{}:{} ({why})", plan.source, tx.path, tx.line);
        match (op.change, &op.text) {
            (Change::New, Some(tx)) => {
                let key = match &op.key {
                    Some(k) => k.clone(),
                    None => fresh_key(&w, &op.kind)?,
                };
                let place = op.slot.map(|s| (export, anchor(&slots, &placed, s)));
                w.create(&key, &op.kind, &op.sub, &scope(&op.kind), &draft(tx, from(tx, "new in the source"), true), place)?;
                if let Some(s) = op.slot {
                    slots[s] = Some(key.clone());
                    placed.insert(key);
                }
                rep.new += 1;
            }
            (Change::Changed, Some(tx)) => {
                let old = op.key.clone().ok_or_else(|| Error::InvalidArgument("a change needs its record".into()))?;
                if op.kind == "memory" {
                    w.revise(&old, None, &draft(tx, from(tx, "the file changed"), false))?;
                } else {
                    let key = fresh_key(&w, &op.kind)?;
                    let d = draft(tx, from(tx, &format!("the item's text changed; supersedes {old}")), false);
                    w.supersede(std::slice::from_ref(&old), &key, &op.kind, &op.sub, &scope(&op.kind), &d, Some((export, Place::After(old.clone()))))?;
                    if let Some(s) = op.slot {
                        slots[s] = Some(key.clone());
                    }
                    placed.insert(key);
                }
                rep.changed += 1;
            }
            (Change::Returned, text) => {
                let key = op.key.clone().ok_or_else(|| Error::InvalidArgument("a return needs its record".into()))?;
                w.reactivate(&key, &format!("synced {stamp}: back in {}", plan.source))?;
                if let Some(tx) = text {
                    w.revise(&key, None, &draft(tx, from(tx, "back, with new text"), false))?;
                }
                if let Some(s) = op.slot {
                    w.place_record(export, &key, anchor(&slots, &placed, s))?;
                    slots[s] = Some(key.clone());
                    placed.insert(key);
                }
                rep.returned += 1;
            }
            (Change::Placed, _) => {
                let (Some(key), Some(s)) = (op.key.clone(), op.slot) else { continue };
                w.place_record(export, &key, anchor(&slots, &placed, s))?;
                slots[s] = Some(key.clone());
                placed.insert(key);
                rep.placed += 1;
            }
            _ => {}
        }
    }
    w.commit()?;
    store.meta_set(&meta_key(export), &plan.fingerprint)?;
    Ok(rep)
}

/// Plans and applies the sync of one target.
///
/// # Errors
///
/// As [`plan`] and [`apply`].
pub fn sync(store: &mut MemoryStore, t: &Target) -> Result<Report> {
    let t0 = Instant::now();
    let p = plan(store, t)?;
    let mut rep = if p.ops.is_empty() {
        store.meta_set(&meta_key(&p.export), &p.fingerprint)?;
        Report { export: p.export.clone(), ..Report::default() }
    } else {
        apply(store, &p)?
    };
    rep.elapsed_us = t0.elapsed().as_micros();
    Ok(rep)
}

/// The sync state an index store carries: the targets, the memory store
/// (opened on the first sync that needs it) and, per source, the
/// fingerprint this process last found in step, so an unchanged source
/// costs one map lookup.
pub struct Syncer {
    settings: Settings,
    targets: Vec<Target>,
    store: Option<MemoryStore>,
    checked: HashMap<String, String>,
    reports: Vec<Report>,
    /// The tree's embedder, loaded on its first use.
    embed: Option<Box<tree::Embed<'static>>>,
    /// Sources whose tree rebuild wrote files since the last call.
    rewritten: HashSet<String>,
}

impl Syncer {
    /// The syncer for these settings and sources, or `None` when no export
    /// maps to a source.
    pub fn new(settings: Settings, sources: &[(String, PathBuf)]) -> Option<Syncer> {
        let targets = targets(&settings, sources);
        (!targets.is_empty()).then(|| Syncer { settings, targets, store: None, checked: HashMap::new(), reports: Vec::new(), embed: None, rewritten: HashSet::new() })
    }

    /// The target synced from a source.
    pub fn target(&self, source: &str) -> Option<&Target> {
        self.targets.iter().find(|t| t.source == source)
    }

    /// Whether a source needs a look: it maps to an export, and it changed
    /// or this process has not checked it yet.
    pub fn needs(&self, source: &str, changed: bool) -> bool {
        self.target(source).is_some() && (changed || !self.checked.contains_key(source))
    }

    /// Syncs a source's export unless its fingerprint (from the index's
    /// hashes; `None` when the index lacks a file) equals the one stored at
    /// the last sync. Regenerates the export when anything changed.
    ///
    /// # Returns
    ///
    /// The report, when a sync ran.
    ///
    /// # Errors
    ///
    /// As [`sync`], and [`Error::Io`] when the export cannot be written.
    pub fn check(&mut self, source: &str, fp: Option<String>) -> Result<Option<Report>> {
        let Some(t) = self.target(source).cloned() else { return Ok(None) };
        // Memories marked `scope: user` in other folders are outside the
        // index's hashes: the user layer is read whole each time it is due.
        let fp = if t.user_from.is_empty() { fp } else { None };
        if fp.is_some() && self.checked.get(source) == fp.as_ref() {
            return Ok(None);
        }
        let mut fp = fp;
        if let Some(tr) = &t.tree {
            // The tree first, so the sync reads it as rebuilt; a rewritten
            // root changes the files, so the fingerprint is read again.
            let (model, threads) = (self.settings.model.clone(), self.settings.embed_threads);
            let slot = &mut self.embed;
            let mut lazy = |texts: &[String]| slot.get_or_insert_with(|| tree::embedder(&model, threads))(texts);
            match tree::rebuild(&t.path, &t.spec.index, tr, &mut lazy, true) {
                Ok((churn, _)) if churn.changed() => {
                    eprintln!("atlas: memory tree {}: {}", t.spec.name, churn.line());
                    self.rewritten.insert(source.to_string());
                    fp = None;
                }
                Ok(_) => {}
                Err(e) => eprintln!("atlas: memory tree {}: {e}", t.spec.name),
            }
        }
        if self.store.is_none() {
            self.store = Some(MemoryStore::open(&self.settings.store)?);
        }
        let store = self.store.as_mut().ok_or_else(|| Error::Db("memory store".into()))?;
        if let Some(fp) = &fp {
            if store.meta_get(&meta_key(&t.spec.name))?.as_ref() == Some(fp) {
                self.checked.insert(source.to_string(), fp.clone());
                return Ok(None);
            }
        }
        let rep = sync(store, &t)?;
        if rep.new + rep.changed + rep.removed + rep.returned + rep.placed > 0 {
            if let Some(dir) = &self.settings.export_dir {
                for (p, why) in super::export::write(store, &t.spec, dir, false)?.refused {
                    eprintln!("atlas: memory sync: export refused {}: {why}", p.display());
                }
            }
        }
        let stored = store.meta_get(&meta_key(&t.spec.name))?.unwrap_or_default();
        self.checked.insert(source.to_string(), fp.unwrap_or(stored));
        self.reports.push(rep.clone());
        Ok(Some(rep))
    }

    /// Whether a tree rebuild wrote into a source since the last call (its
    /// rows are then stale and the source is scanned again).
    pub fn take_rewritten(&mut self, source: &str) -> bool {
        self.rewritten.remove(source)
    }

    /// The reports of the syncs run since the last call.
    pub fn take_reports(&mut self) -> Vec<Report> {
        std::mem::take(&mut self.reports)
    }
}

/// Every difference between the targets' files and the store, one line
/// each, without writing anything. A target that cannot be read is one
/// line saying why.
///
/// # Errors
///
/// [`Error::Db`].
pub fn drift(store: &MemoryStore, targets: &[Target]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for t in targets {
        if let Some(tr) = &t.tree {
            out.extend(tree::pin_drift(&t.path, tr).into_iter().map(|l| format!("{} {l}", t.spec.name)));
        }
        match plan(store, t) {
            Ok(p) => out.extend(p.lines()),
            Err(Error::Db(e)) => return Err(Error::Db(e)),
            Err(e) => out.push(format!("{} unreadable: {e}", t.spec.name)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_reads_date_and_one_name() {
        assert_eq!(item_attribution("- 2026-01-02 Alex: rule"), (Some("2026-01-02T00:00:00.000Z".into()), Some("Alex".into())));
        assert_eq!(item_attribution("- 2026-01-02 Alex on THING: rule").1.as_deref(), Some("Alex"));
        assert_eq!(item_attribution("- 2026-01-02 Alex, standing rule: x").1.as_deref(), Some("Alex"));
        assert_eq!(item_attribution("- 2026-01-02: no author"), (Some("2026-01-02T00:00:00.000Z".into()), None));
        assert_eq!(item_attribution("- 2026-01-02 REVERSED: x").1, None);
        assert_eq!(item_attribution("- **Bold.** no date"), (None, None));
        // A date past today is not taken.
        assert_eq!(item_attribution("- 9999-01-01 Alex: x").0, None);
    }

    #[test]
    fn similarity_is_word_overlap() {
        assert!(similarity("the cap is 500k tokens", "the cap is 250k tokens") >= EDIT_SIMILARITY);
        assert!(similarity("the cap is 500k tokens", "an unrelated ruling about maps") < EDIT_SIMILARITY);
    }

    #[test]
    fn a_cut_index_line_keeps_the_full_title() {
        let full = "- [A](a.md) - the cap is 64 entries, least recently used first";
        assert_eq!(memory_title(full, "- [A](a.md) - the cap is 64 entries", true), full);
        assert_eq!(memory_title(full, "- [A](a.md) - the cap is 128 entries", true), "- [A](a.md) - the cap is 128 entries");
        assert_eq!(memory_title(full, "- [a](a.md) - synthesized", false), full);
        // A line ending is not a new title.
        assert_eq!(memory_title(&format!("{full}\r"), full, true), format!("{full}\r"));
        assert_eq!(same_text("t\r", "m\r\n", "b\r\n"), same_text("t", "m\n", "b\n"));
    }

    #[test]
    fn fingerprint_ignores_order() {
        let a = vec![("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string())];
        let b = vec![a[1].clone(), a[0].clone()];
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&a[..1]));
    }
}
