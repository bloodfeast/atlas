//! Generated files: the store rendered as the Markdown files people and
//! agents read.
//!
//! Three shapes, each named in the config:
//!
//! * [`Shape::MemoryDir`]: a folder with one file per memory record
//!   (frontmatter then body) and an index file whose lines link them.
//! * [`Shape::Items`]: one file of top-level list items (rules) between
//!   verbatim blocks.
//! * [`Shape::Headings`]: one file of heading sections; handoff records are
//!   added here as new sections when the export takes handoffs.
//!
//! Every piece is the exact text it was imported or written with, so an
//! export of a fresh import is byte-identical to its source. A superseded
//! record keeps its text and gains one marker line.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

use super::import::{split_headings, split_index, split_items, split_memory_file, PieceKind};
use super::{Head, MemoryStore};

/// How an export is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A folder: one file per memory record plus an index file.
    MemoryDir,
    /// One file of list items and blocks.
    Items,
    /// One file of heading sections.
    Headings,
}

impl Shape {
    /// Parses a config value.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for an unknown shape.
    pub fn parse(s: &str) -> Result<Shape> {
        match s {
            "memory" => Ok(Shape::MemoryDir),
            "items" => Ok(Shape::Items),
            "headings" => Ok(Shape::Headings),
            o => Err(Error::InvalidArgument(format!("export shape {o:?}; use memory, items or headings"))),
        }
    }
}

/// One configured export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSpec {
    /// Its name (also the key prefix of its imported records).
    pub name: String,
    /// Its layout.
    pub shape: Shape,
    /// The file (or, for [`Shape::MemoryDir`], the folder), relative to the
    /// export folder.
    pub path: PathBuf,
    /// The index file's name inside a memory folder.
    pub index: String,
    /// New handoff records are added to this export.
    pub handoffs: bool,
    /// Cut each index line to this many characters (off when `None`).
    pub index_cap: Option<usize>,
    /// Topic headers `(name, words)` of a clustered index (see
    /// [`clustered_index`]); `None` keeps the line-per-memory index.
    pub index_topics: Option<Vec<(String, Vec<String>)>>,
}

/// The line added under a superseded record.
pub fn superseded_line(h: &Head) -> String {
    let date = h.superseded_time.as_deref().and_then(|t| t.get(..10)).unwrap_or("");
    let by = h.superseded_by.as_deref().unwrap_or("?");
    let lead = if h.body.ends_with('\n') || h.body.is_empty() { "" } else { "\n" };
    format!("{lead}  (superseded {date} by {by})\n")
}

/// A handoff record as a section: a heading, then the body with any line
/// that would read as a heading escaped, so the section stays one section.
pub fn handoff_section(h: &Head) -> String {
    let mut s = format!("## {} handoff {}: {}\n\n", h.time.get(..16).unwrap_or(&h.time), h.scope, h.title);
    for l in h.body.split_inclusive('\n') {
        if l.starts_with('#') {
            s.push('\\');
        }
        s.push_str(l);
    }
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s.push('\n');
    s
}

/// A record's text as one piece of a one-file export.
pub fn piece_text(h: &Head) -> String {
    let mut s = if h.kind == "handoff" { handoff_section(h) } else { h.body.clone() };
    if h.status == "superseded" {
        s.push_str(&superseded_line(h));
    }
    s
}

/// A memory record's file text.
pub fn memory_file(h: &Head) -> String {
    let mut s = String::with_capacity(h.meta.len() + h.body.len() + 64);
    s.push_str(&h.meta);
    s.push_str(&h.body);
    if h.status == "superseded" {
        let mut t = h.clone();
        t.body = s.clone();
        s.push_str(&superseded_line(&t));
    }
    s
}

fn cap_line(line: &str, cap: Option<usize>) -> String {
    match cap {
        Some(n) if line.chars().count() > n => line.chars().take(n).collect(),
        _ => line.to_string(),
    }
}

/// Renders one export: each file's path (relative to the export folder) and
/// its exact text.
///
/// # Errors
///
/// [`Error::Db`].
pub fn render(store: &MemoryStore, spec: &ExportSpec) -> Result<Vec<(PathBuf, String)>> {
    let heads = store.heads_by_rid(&store.placement(&spec.name)?)?;
    match spec.shape {
        Shape::Items | Shape::Headings => {
            let mut text = String::new();
            for h in &heads {
                text.push_str(&piece_text(h));
            }
            Ok(vec![(spec.path.clone(), text)])
        }
        Shape::MemoryDir if spec.index_topics.is_some() => {
            let topics = spec.index_topics.as_deref().unwrap_or_default();
            let mut out = vec![(spec.path.join(&spec.index), clustered_index(&heads, topics))];
            for h in store.heads(Some("memory"), None)?.into_iter().filter(|h| h.status != "retired") {
                out.push((spec.path.join(format!("{}.md", h.key)), memory_file(&h)));
            }
            Ok(out)
        }
        Shape::MemoryDir => {
            let mut index = String::new();
            let mut superseded = Vec::new();
            for h in &heads {
                if h.kind == "memory" {
                    if h.status == "superseded" {
                        superseded.push(h);
                    } else {
                        index.push_str(&cap_line(&h.title, spec.index_cap));
                        index.push('\n');
                    }
                } else {
                    index.push_str(&h.body);
                }
            }
            if !superseded.is_empty() {
                index.push_str("\n## Superseded\n\n");
                for h in superseded {
                    index.push_str(&cap_line(&h.title, spec.index_cap));
                    index.push('\n');
                }
            }
            let mut out = vec![(spec.path.join(&spec.index), index)];
            for h in store.heads(Some("memory"), None)?.into_iter().filter(|h| h.status != "retired") {
                out.push((spec.path.join(format!("{}.md", h.key)), memory_file(&h)));
            }
            Ok(out)
        }
    }
}

/// A frontmatter value without its YAML quotes (`"..."` with `\"` and `\\`
/// escapes, or `'...'` with `''`).
pub fn unquote(v: &str) -> String {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let mut out = String::with_capacity(v.len());
        let mut it = v[1..v.len() - 1].chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                match it.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(o) => out.push(o),
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    } else if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        v[1..v.len() - 1].replace("''", "'")
    } else {
        v.to_string()
    }
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10 && b[..10].iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

/// A rule's statement without its leading attribution: words, an ISO date,
/// at most one more word, then `: `, ` - ` or ` -- ` (as in
/// `Someone 2026-01-02 - the rule`). Anything else is returned whole, so a
/// date that is part of the sentence stays, and so does a lead with a word
/// in capitals (a marker such as `REVERSED 2026-01-02 - ...`).
pub fn trim_rule(d: &str) -> &str {
    let Some(at) = d.char_indices().take_while(|(i, _)| *i <= 48).map(|(i, _)| i).find(|&i| is_date(&d[i..])) else {
        return d;
    };
    let lead = &d[..at];
    if !lead.chars().all(|c| c.is_alphabetic() || c == ' ' || c == ',' || c == '\'') {
        return d;
    }
    if lead.split(|c: char| !c.is_alphabetic()).any(|w| w.len() > 1 && w.chars().all(char::is_uppercase)) {
        return d;
    }
    let rest = &d[at + 10..];
    for sep in [": ", " -- ", " - "] {
        if let Some(i) = rest.find(sep) {
            let between = rest[..i].trim();
            if between.split_whitespace().count() <= 1 && between.chars().all(char::is_alphabetic) {
                let out = rest[i + sep.len()..].trim_start();
                if !out.is_empty() {
                    return out;
                }
            }
        }
    }
    d
}

fn is_slug_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.'
}

/// How many times `slug` appears in `text` as a whole slug (not inside a
/// longer one).
pub fn slug_count(text: &str, slug: &str) -> usize {
    if slug.is_empty() {
        return 0;
    }
    let b = text.as_bytes();
    let mut n = 0;
    let mut from = 0;
    while let Some(i) = text[from..].find(slug) {
        let s = from + i;
        let e = s + slug.len();
        // A trailing '.' ends a sentence, not a slug.
        let before = s == 0 || !is_slug_char(b[s - 1]);
        let after = e >= b.len() || !is_slug_char(b[e]) || (b[e] == b'.' && (e + 1 >= b.len() || !is_slug_char(b[e + 1])));
        if before && after {
            n += 1;
        }
        from = e;
    }
    n
}

fn push_cluster(out: &mut String, slugs: &[&str]) {
    const WIDTH: usize = 116;
    let mut line = String::new();
    for (i, s) in slugs.iter().enumerate() {
        let piece = if i + 1 < slugs.len() { format!("{s},") } else { (*s).to_string() };
        if !line.is_empty() && line.len() + 1 + piece.len() > WIDTH {
            out.push_str(&line);
            out.push('\n');
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&piece);
    }
    if !line.is_empty() {
        out.push_str(&line);
        out.push('\n');
    }
}

/// The clustered memory index: an index of the store rather than a copy of
/// every description.
///
/// * Standing rules (`metadata.type` feedback) and facts about the user
///   (`user`) one line each, `- slug: statement`, the statement being the
///   description with its leading attribution trimmed ([`trim_rule`]).
/// * Project facts (`project` and any other type) as slug clusters under
///   the first topic with a word in the slug, else the first with a word in
///   the description (`Other` when none matches), in topic order.
/// * References (`reference`) and superseded memories as clusters of their
///   own.
///
/// Every memory is named exactly once; the bodies are in the files and in
/// recall.
///
/// # Arguments
///
/// * `heads` - the export's records in placement order.
/// * `topics` - `(header, lowercase words)` in priority order.
pub fn clustered_index(heads: &[Head], topics: &[(String, Vec<String>)]) -> String {
    let mems: Vec<&Head> = heads.iter().filter(|h| h.kind == "memory").collect();
    let desc = |h: &Head| unquote(super::import::front_value(&h.meta, "description").unwrap_or(""));
    let mut out = String::from(
        "# Memory index\n\nGenerated from the memory store; do not edit. Rules and facts about the user are stated in full. Every other\nmemory is named by its slug: its file is `<slug>.md` beside this index, and recall with that key returns its body.\n",
    );
    let section = |out: &mut String, title: &str, list: Vec<&Head>| {
        if list.is_empty() {
            return;
        }
        out.push_str(&format!("\n## {title}\n\n"));
        for h in list {
            let d = desc(h);
            out.push_str(&format!("- {}: {}\n", h.key, trim_rule(d.trim()).replace('\n', " ")));
        }
    };
    let active = |sub: &str| -> Vec<&Head> { mems.iter().copied().filter(|h| h.status != "superseded" && h.sub == sub).collect() };
    section(&mut out, "Standing rules", active("feedback"));
    section(&mut out, "About the user", active("user"));
    let facts: Vec<&Head> = mems.iter().copied().filter(|h| h.status != "superseded" && !["feedback", "user", "reference"].contains(&h.sub.as_str())).collect();
    if !facts.is_empty() {
        out.push_str("\n## Project facts\n");
        let mut groups: Vec<Vec<&str>> = vec![Vec::new(); topics.len() + 1];
        for h in &facts {
            // The slug names the subject, so it decides first; the description
            // only places a slug no topic word appears in.
            let slug = h.key.replace(['-', '_'], " ").to_lowercase();
            let text = desc(h).to_lowercase();
            let hit = |hay: &str| topics.iter().position(|(_, words)| words.iter().any(|w| super::contains_word(hay, w)));
            let t = hit(&slug).or_else(|| hit(&text)).unwrap_or(topics.len());
            groups[t].push(&h.key);
        }
        for (i, g) in groups.iter().enumerate() {
            if g.is_empty() {
                continue;
            }
            let name = topics.get(i).map_or("Other", |t| t.0.as_str());
            out.push_str(&format!("\n### {name}\n\n"));
            push_cluster(&mut out, g);
        }
    }
    let refs: Vec<&str> = active("reference").into_iter().map(|h| h.key.as_str()).collect();
    if !refs.is_empty() {
        out.push_str("\n## References\n\n");
        push_cluster(&mut out, &refs);
    }
    let old: Vec<&str> = mems.iter().filter(|h| h.status == "superseded").map(|h| h.key.as_str()).collect();
    if !old.is_empty() {
        out.push_str("\n## Superseded\n\n");
        push_cluster(&mut out, &old);
    }
    out
}

/// What writing an export did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteReport {
    /// Files written.
    pub written: Vec<PathBuf>,
    /// Files already equal to their export.
    pub unchanged: usize,
    /// Files not written, with why (edited by hand since the last export,
    /// or present and never exported).
    pub refused: Vec<(PathBuf, String)>,
}

fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = target.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut tmp = target.as_os_str().to_owned();
    tmp.push(format!(".atlas-tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    let mut attempt = 0;
    loop {
        match std::fs::rename(&tmp, target) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
    }
}

/// Writes one export into a folder. A file is written only when its bytes
/// change, and never over a file that differs from what atlas last wrote
/// there (a hand edit), unless `adopt` (the first export into a folder that
/// already holds the files being replaced).
///
/// # Errors
///
/// [`Error::Io`], [`Error::Db`].
pub fn write(store: &MemoryStore, spec: &ExportSpec, dir: &Path, adopt: bool) -> Result<WriteReport> {
    let mut rep = WriteReport::default();
    let time = super::now();
    for (rel, text) in render(store, spec)? {
        let full = dir.join(&rel);
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        let last: Option<String> = rusqlite::OptionalExtension::optional(
            store.conn().prepare_cached("SELECT sha FROM export_files WHERE export = ?1 AND path = ?2")?.query_row([&spec.name, &rel_s], |r| r.get(0)),
        )?;
        let disk = std::fs::read(&full).ok();
        let new_sha = crate::sha256::hex(text.as_bytes());
        let record = |sha: &str| -> Result<()> {
            store
                .conn()
                .prepare_cached("INSERT INTO export_files(export, path, sha, time) VALUES(?1, ?2, ?3, ?4) ON CONFLICT(export, path) DO UPDATE SET sha = excluded.sha, time = excluded.time")?
                .execute([&spec.name, &rel_s, sha, &time])?;
            Ok(())
        };
        match &disk {
            Some(d) if d.as_slice() == text.as_bytes() => {
                rep.unchanged += 1;
                if last.as_deref() != Some(new_sha.as_str()) {
                    record(&new_sha)?;
                }
                continue;
            }
            Some(d) if !adopt => {
                let disk_sha = crate::sha256::hex(d);
                match &last {
                    None => {
                        rep.refused.push((full.clone(), "present and never exported by atlas".into()));
                        continue;
                    }
                    Some(l) if *l != disk_sha => {
                        rep.refused.push((full.clone(), format!("edited since the last export (first difference at line {})", first_diff_line(d, text.as_bytes()))));
                        continue;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        write_atomic(&full, text.as_bytes())?;
        record(&new_sha)?;
        rep.written.push(full);
    }
    Ok(rep)
}

/// The 1-based line where two texts first differ.
pub fn first_diff_line(a: &[u8], b: &[u8]) -> usize {
    let n = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    a[..n].iter().filter(|c| **c == b'\n').count() + 1
}

/// Splits a rendered export back into pieces and checks they line up with
/// the records it was rendered from: one piece per record, each equal to the
/// record's text. Returns one line per disagreement.
///
/// # Errors
///
/// [`Error::Db`].
pub fn round_trip(store: &MemoryStore, spec: &ExportSpec) -> Result<Vec<String>> {
    let heads = store.heads_by_rid(&store.placement(&spec.name)?)?;
    let mut errs = Vec::new();
    let files = render(store, spec)?;
    match spec.shape {
        Shape::Items | Shape::Headings => {
            let text = &files[0].1;
            let pieces = if spec.shape == Shape::Items { split_items(text) } else { split_headings(text) };
            // Adjacent blocks merge on a re-split; compare the merged runs.
            let mut expect: Vec<(bool, String, String)> = Vec::new();
            for h in &heads {
                let t = piece_text(h);
                let is_block = h.kind == "block";
                match expect.last_mut() {
                    Some((true, keys, acc)) if is_block => {
                        acc.push_str(&t);
                        keys.push(',');
                        keys.push_str(&h.key);
                    }
                    _ => expect.push((is_block, h.key.clone(), t)),
                }
            }
            if pieces.len() != expect.len() {
                errs.push(format!("{}: {} records render to {} pieces on a re-split", spec.name, expect.len(), pieces.len()));
            }
            for (p, (_, keys, t)) in pieces.iter().zip(&expect) {
                if p.text != *t {
                    errs.push(format!("{}: {keys} does not re-split to itself (line {})", spec.name, p.line));
                    break;
                }
            }
        }
        Shape::MemoryDir => {
            let index = &files[0].1;
            let listed: Vec<&Head> = heads.iter().filter(|h| h.kind == "memory").collect();
            if spec.index_topics.is_some() {
                // A clustered index names every memory once.
                for h in &listed {
                    let n = slug_count(index, &h.key);
                    if n != 1 {
                        errs.push(format!("{}: {} is named {n} times in the clustered index", spec.name, h.key));
                    }
                }
            } else {
                let entries: Vec<_> = split_index(index).into_iter().filter(|p| p.kind == PieceKind::Entry).collect();
                if entries.len() != listed.len() {
                    errs.push(format!("{}: {} memory records, {} index entries on a re-split", spec.name, listed.len(), entries.len()));
                }
            }
            for (file, text) in &files[1..] {
                let key = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let Some(h) = store.head(&key)? else {
                    errs.push(format!("{}: file {} has no record", spec.name, file.display()));
                    continue;
                };
                let (meta, body) = split_memory_file(text);
                let body_ok = if h.status == "superseded" { body.starts_with(h.body.as_str()) } else { body == h.body };
                if meta != h.meta || !body_ok {
                    errs.push(format!("{}: {key}.md does not split back into its record", spec.name));
                }
            }
        }
    }
    Ok(errs)
}
