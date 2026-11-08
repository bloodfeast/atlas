//! Splitting exported files into records, and importing them with
//! provenance.
//!
//! The same split functions serve the migration (files to records) and the
//! round-trip check (rendered exports back to records), so the two cannot
//! drift apart. Splits are byte-exact: concatenating the pieces gives the
//! input back, line endings included.

use std::path::Path;

use crate::error::{Error, Result};

use super::export::{ExportSpec, Shape};
use super::{Draft, Place, Provenance, Writer};

/// What a piece of a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceKind {
    /// A top-level list item and its continuation lines (a rule).
    Item,
    /// A heading line and everything up to the next heading.
    Section,
    /// Anything else, kept verbatim.
    Block,
    /// An index line naming a memory file (`- [Title](file.md) ...`).
    Entry,
}

/// One piece of a split file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// Its kind.
    pub kind: PieceKind,
    /// Its exact text.
    pub text: String,
    /// The 1-based line it starts on.
    pub line: usize,
    /// For an [`PieceKind::Entry`], the file stem it links.
    pub target: String,
}

/// Lines with their terminators (`\n` or `\r\n`) kept.
pub fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
}

fn bare(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

fn is_heading(line: &str) -> bool {
    let l = bare(line);
    let hashes = l.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&hashes) && (l.len() == hashes || l.as_bytes()[hashes] == b' ')
}

fn is_item(line: &str) -> bool {
    line.starts_with("- ") || bare(line) == "-"
}

fn is_continuation(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t')
}

fn push(out: &mut Vec<Piece>, kind: PieceKind, line_no: usize, text: &str) {
    match out.last_mut() {
        Some(p) if p.kind == kind && kind == PieceKind::Block => p.text.push_str(text),
        _ => out.push(Piece { kind, text: text.to_string(), line: line_no, target: String::new() }),
    }
}

/// Splits a rules-shaped file: every top-level item (`- ` at column 0) with
/// its indented continuation lines is one [`PieceKind::Item`]; runs of other
/// lines (headings, paragraphs, blank lines) are [`PieceKind::Block`]s.
pub fn split_items(text: &str) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    for (i, l) in lines(text).enumerate() {
        if is_item(l) {
            out.push(Piece { kind: PieceKind::Item, text: l.to_string(), line: i + 1, target: String::new() });
        } else if is_continuation(l) && out.last().is_some() {
            if let Some(p) = out.last_mut() {
                p.text.push_str(l);
            }
        } else {
            push(&mut out, PieceKind::Block, i + 1, l);
        }
    }
    out
}

/// Splits a sections-shaped file: every heading line starts a
/// [`PieceKind::Section`] that runs to the next heading; text before the
/// first heading is a [`PieceKind::Block`]. Lines inside fenced code blocks
/// never start a section.
pub fn split_headings(text: &str) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut fence = false;
    for (i, l) in lines(text).enumerate() {
        if bare(l).trim_start().starts_with("```") {
            fence = !fence;
        }
        if !fence && is_heading(l) {
            out.push(Piece { kind: PieceKind::Section, text: l.to_string(), line: i + 1, target: String::new() });
        } else if let Some(p) = out.last_mut() {
            p.text.push_str(l);
        } else {
            push(&mut out, PieceKind::Block, i + 1, l);
        }
    }
    out
}

/// The file stem an index line links (`- [Title](stem.md) ...`), if any.
pub fn index_target(line: &str) -> Option<&str> {
    let l = line.strip_prefix("- [")?;
    let close = l.find("](")?;
    let rest = &l[close + 2..];
    let end = rest.find(')')?;
    let target = &rest[..end];
    let stem = target.strip_suffix(".md")?;
    (!stem.is_empty() && !stem.contains(['/', '\\'])).then_some(stem)
}

/// Splits a memory index: lines linking a memory file are
/// [`PieceKind::Entry`] pieces (one line each); everything else is a block.
pub fn split_index(text: &str) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    for (i, l) in lines(text).enumerate() {
        match index_target(l) {
            Some(stem) => out.push(Piece { kind: PieceKind::Entry, text: l.to_string(), line: i + 1, target: stem.to_string() }),
            None => push(&mut out, PieceKind::Block, i + 1, l),
        }
    }
    out
}

/// A memory file split into its frontmatter (both `---` lines included,
/// verbatim) and the body after it. A file without frontmatter has an empty
/// meta.
pub fn split_memory_file(text: &str) -> (&str, &str) {
    let mut it = lines(text);
    let Some(first) = it.next() else { return ("", text) };
    if bare(first) != "---" {
        return ("", text);
    }
    let mut at = first.len();
    for l in it {
        at += l.len();
        if bare(l) == "---" {
            return (&text[..at], &text[at..]);
        }
    }
    ("", text)
}

/// A frontmatter key's value (`key: value` at column 0, or indented one
/// level under `metadata:`), quotes not removed.
pub fn front_value<'a>(meta: &'a str, key: &str) -> Option<&'a str> {
    for l in lines(meta) {
        let t = bare(l).trim_start();
        if let Some(v) = t.strip_prefix(key).and_then(|r| r.strip_prefix(':')) {
            return Some(v.trim());
        }
    }
    None
}

/// The title of a piece: the item text without its marker, or the heading
/// text, cut to 160 characters.
pub fn piece_title(p: &Piece) -> String {
    let first = bare(lines(&p.text).next().unwrap_or(""));
    let t = match p.kind {
        PieceKind::Item => first.trim_start_matches('-').trim(),
        PieceKind::Section => first.trim_start_matches('#').trim(),
        _ => first.trim(),
    };
    t.chars().take(160).collect()
}

/// What an import wrote, per kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportCounts {
    /// The export.
    pub export: String,
    /// Records of each kind, in first-seen order.
    pub kinds: Vec<(String, usize)>,
    /// Bytes read.
    pub bytes: usize,
    /// Lines read.
    pub lines: usize,
}

impl ImportCounts {
    fn add(&mut self, kind: &str) {
        match self.kinds.iter_mut().find(|(k, _)| k == kind) {
            Some((_, n)) => *n += 1,
            None => self.kinds.push((kind.to_string(), 1)),
        }
    }
    /// The count for one kind.
    pub fn of(&self, kind: &str) -> usize {
        self.kinds.iter().find(|(k, _)| k == kind).map_or(0, |(_, n)| *n)
    }
}

fn read_utf8(path: &Path) -> Result<String> {
    let b = std::fs::read(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    String::from_utf8(b).map_err(|_| Error::InvalidArgument(format!("{} is not UTF-8", path.display())))
}

fn draft(title: String, meta: String, body: String, client: &str, p: Provenance) -> Draft {
    Draft { title, meta, body, reason: format!("imported from {}:{}", p.src, p.path), author: "import".into(), client: client.to_string(), provenance: Some(p), time: None }
}

/// Imports one export's source file (or folder) into a write batch.
///
/// # Arguments
///
/// * `w` - the batch.
/// * `spec` - the export: its shape says how to split.
/// * `source` - the file (rules and sections shapes) or the folder (memory
///   shape) to read.
/// * `client` - recorded on every version.
///
/// # Errors
///
/// [`Error::Io`] or [`Error::InvalidArgument`] for unreadable or non-UTF-8
/// files, [`Error::AlreadyExists`] when a key is taken (importing twice).
pub fn import(w: &mut Writer<'_>, spec: &ExportSpec, source: &Path, client: &str) -> Result<ImportCounts> {
    let mut c = ImportCounts { export: spec.name.clone(), ..ImportCounts::default() };
    let name = spec.name.as_str();
    match spec.shape {
        Shape::Items | Shape::Headings => {
            let text = read_utf8(source)?;
            let sha = crate::sha256::hex(text.as_bytes());
            c.bytes = text.len();
            c.lines = lines(&text).count();
            let rel = source.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let pieces = if spec.shape == Shape::Items { split_items(&text) } else { split_headings(&text) };
            let (mut r, mut s, mut b) = (0usize, 0usize, 0usize);
            for p in pieces {
                let prov = Provenance { src: name.to_string(), path: rel.clone(), line: p.line as i64, sha: sha.clone() };
                let (key, kind) = match p.kind {
                    PieceKind::Item => {
                        r += 1;
                        (format!("{name}:r{r:05}"), "rule")
                    }
                    PieceKind::Section => {
                        s += 1;
                        (format!("{name}:s{s:05}"), "section")
                    }
                    _ => {
                        b += 1;
                        (format!("{name}:b{b:05}"), "block")
                    }
                };
                let d = draft(piece_title(&p), String::new(), p.text, client, prov);
                w.create(&key, kind, "", name, &d, Some((name, Place::End)))?;
                c.add(kind);
            }
        }
        Shape::MemoryDir => {
            let index_path = source.join(&spec.index);
            let index = read_utf8(&index_path)?;
            let index_sha = crate::sha256::hex(index.as_bytes());
            c.bytes += index.len();
            c.lines += lines(&index).count();
            let mut b = 0usize;
            let mut seen = std::collections::HashSet::new();
            for p in split_index(&index) {
                let prov = Provenance { src: name.to_string(), path: spec.index.clone(), line: p.line as i64, sha: index_sha.clone() };
                if p.kind == PieceKind::Entry && source.join(format!("{}.md", p.target)).is_file() && seen.insert(p.target.clone()) {
                    let file = source.join(format!("{}.md", p.target));
                    let text = read_utf8(&file)?;
                    c.bytes += text.len();
                    let (meta, body) = split_memory_file(&text);
                    let sub = front_value(meta, "type").unwrap_or("").to_string();
                    let title = p.text.strip_suffix('\n').ok_or_else(|| Error::InvalidArgument(format!("{}: line {} has no line break", spec.index, p.line)))?;
                    let fprov = Provenance { src: name.to_string(), path: format!("{}.md", p.target), line: 1, sha: crate::sha256::hex(text.as_bytes()) };
                    let d = draft(title.to_string(), meta.to_string(), body.to_string(), client, fprov);
                    w.create(&p.target, "memory", &sub, "", &d, Some((name, Place::End)))?;
                    c.add("memory");
                } else {
                    b += 1;
                    let d = draft(piece_title(&p), String::new(), p.text, client, prov);
                    w.create(&format!("{name}:b{b:05}"), "block", "", name, &d, Some((name, Place::End)))?;
                    c.add("block");
                }
            }
            // Memory files the index does not list: imported, not placed.
            let mut extra: Vec<String> = std::fs::read_dir(source)?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|f| f.ends_with(".md") && *f != spec.index)
                .map(|f| f.trim_end_matches(".md").to_string())
                .filter(|s| !seen.contains(s))
                .collect();
            extra.sort();
            for stem in extra {
                let file = source.join(format!("{stem}.md"));
                let text = read_utf8(&file)?;
                c.bytes += text.len();
                let (meta, body) = split_memory_file(&text);
                let sub = front_value(meta, "type").unwrap_or("").to_string();
                let desc = front_value(meta, "description").unwrap_or("").to_string();
                let prov = Provenance { src: name.to_string(), path: format!("{stem}.md"), line: 1, sha: crate::sha256::hex(text.as_bytes()) };
                let d = draft(format!("- [{stem}]({stem}.md) - {desc}"), meta.to_string(), body.to_string(), client, prov);
                w.create(&stem, "memory", &sub, "", &d, None)?;
                c.add("memory (not in index)");
            }
        }
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(p: &[Piece]) -> String {
        p.iter().map(|x| x.text.as_str()).collect()
    }

    #[test]
    fn items_split_exactly() {
        let t = "# Title\n\nIntro line.\n- 2026-01-01 first rule\n  continued\n- second\r\n\n## Sub\n- third";
        let p = split_items(t);
        assert_eq!(join(&p), t);
        let kinds: Vec<PieceKind> = p.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, vec![PieceKind::Block, PieceKind::Item, PieceKind::Item, PieceKind::Block, PieceKind::Item]);
        assert_eq!(p[1].text, "- 2026-01-01 first rule\n  continued\n");
        assert_eq!(piece_title(&p[1]), "2026-01-01 first rule");
    }

    #[test]
    fn headings_split_exactly_and_skip_fences() {
        let t = "pre\r\n# A\r\nx\r\n```\n# not a heading\n```\n## B\nend";
        let p = split_headings(t);
        assert_eq!(join(&p), t);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].kind, PieceKind::Block);
        assert_eq!(piece_title(&p[2]), "B");
    }

    #[test]
    fn index_and_memory_files_split() {
        let t = "# Index\n- [Alpha](alpha.md) - first\n- [Beta](beta.md) - second\nnote\n";
        let p = split_index(t);
        assert_eq!(join(&p), t);
        assert_eq!(p.iter().filter(|x| x.kind == PieceKind::Entry).count(), 2);
        assert_eq!(p[1].target, "alpha");
        let f = "---\nname: alpha\ndescription: d\nmetadata:\n  type: feedback\n---\n\nBody\n";
        let (m, b) = split_memory_file(f);
        assert_eq!(format!("{m}{b}"), f);
        assert_eq!(b, "\nBody\n");
        assert_eq!(front_value(m, "type"), Some("feedback"));
        assert_eq!(split_memory_file("no frontmatter\n"), ("", "no frontmatter\n"));
    }
}
