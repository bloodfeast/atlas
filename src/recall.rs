//! The recall check: atlas must find at least what a plain scan of the same
//! files finds.
//!
//! For every indexed document the check reads the file again from disk and
//! scans it independently of the index:
//!
//! * every configured pattern, line by line (as grep runs it);
//! * every name, as a whole word (`[A-Za-z0-9_-]` boundaries), by plain
//!   substring search;
//! * every `[[wikilink]]`, by plain substring search for `[[` and `]]`;
//! * every heading's text as a phrase (and any extra phrases given), by a
//!   case-insensitive scan for its words in order on one line.
//!
//! An identifier occurrence the scan finds but `lookup` does not return, or
//! a document containing a phrase that section search does not return, is a
//! miss. The suite runs this over a fixture corpus; `atlas --recall-check`
//! runs it over the configured sources.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::error::Result;
use crate::markdown;
use crate::registry;
use crate::store::Store;

/// An identifier occurrence the scan found and atlas did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdMiss {
    /// The identifier.
    pub id: String,
    /// Source or vault.
    pub source: String,
    /// Path below its root.
    pub path: String,
    /// 1-based line.
    pub line: usize,
    /// `index` when the occurrence is not in the index at all, `lookup` when
    /// it is indexed but `lookup` does not return it.
    pub stage: &'static str,
}

crate::impl_to_json!(IdMiss { id, source, path, line, stage });

/// A document holding a phrase that section search did not return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhraseMiss {
    /// The phrase.
    pub phrase: String,
    /// Source or vault.
    pub source: String,
    /// Path below its root.
    pub path: String,
}

crate::impl_to_json!(PhraseMiss { phrase, source, path });

/// What the check compared and what it missed.
#[derive(Debug, Clone, Default)]
pub struct RecallReport {
    /// Documents scanned.
    pub docs: usize,
    /// Identifier occurrences the plain scan found.
    pub id_occurrences: usize,
    /// Distinct identifiers the plain scan found.
    pub ids: usize,
    /// Phrases checked.
    pub phrases: usize,
    /// (phrase, document) pairs the plain scan found.
    pub phrase_hits: usize,
    /// Identifier misses.
    pub id_misses: Vec<IdMiss>,
    /// Phrase misses.
    pub phrase_misses: Vec<PhraseMiss>,
}

crate::impl_to_json!(RecallReport { docs, id_occurrences, ids, phrases, phrase_hits, id_misses, phrase_misses });

impl RecallReport {
    /// True when atlas found everything the scan found.
    pub fn complete(&self) -> bool {
        self.id_misses.is_empty() && self.phrase_misses.is_empty()
    }
}

/// The most heading phrases checked in one run.
pub const MAX_PHRASES: usize = 5_000;

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// The words of a phrase as FTS5's `unicode61` tokenizer splits them:
/// runs of letters and digits.
pub fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// An FTS5 phrase query for a list of words.
pub fn phrase_query(words: &[String]) -> String {
    format!("\"{}\"", words.join(" "))
}

/// True when the lowercased `hay` holds `words` in order, each a whole run
/// of alphanumerics, separated only by non-alphanumerics.
fn holds_phrase(hay: &str, words: &[String]) -> bool {
    let Some(first) = words.first() else { return false };
    let mut from = 0;
    while let Some(i) = hay[from..].find(first.as_str()) {
        let start = from + i;
        from = start + first.len();
        let before_ok = hay[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        if !before_ok {
            continue;
        }
        let mut pos = start;
        let mut ok = true;
        for (k, w) in words.iter().enumerate() {
            if k > 0 {
                let rest = &hay[pos..];
                let skip = rest.find(|c: char| c.is_alphanumeric()).unwrap_or(rest.len());
                // Like grep, a phrase does not run across a line break (a
                // section can end at one).
                if skip == 0 || rest[..skip].contains('\n') {
                    ok = false;
                    break;
                }
                pos += skip;
            }
            if !hay[pos..].starts_with(w.as_str()) {
                ok = false;
                break;
            }
            pos += w.len();
            if hay[pos..].chars().next().is_some_and(char::is_alphanumeric) {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    false
}

/// Runs the recall check over every indexed document.
///
/// # Arguments
///
/// * `store` - an up-to-date store.
/// * `extra` - phrases to check besides every heading's text.
///
/// # Errors
///
/// [`crate::Error::Db`] on SQLite failure. Files that can no longer be read
/// are skipped (a freshness check reports them).
pub fn check(store: &Store, extra: &[String]) -> Result<RecallReport> {
    let mut report = RecallReport::default();
    let docs = store.docs()?;
    let rules = store.rules();
    let mut texts: Vec<(String, String, String)> = Vec::with_capacity(docs.len());
    // id lowercased -> occurrences (source, path, line)
    let mut expected: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    let mut phrases: Vec<String> = extra.to_vec();
    let names = store.names();
    for (source, path) in &docs {
        let Ok(bytes) = store.read_document(source, path) else { continue };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let d = texts.len();
        for (li, line) in text.lines().enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let mut m = Vec::new();
            rules.matches(line, &mut m);
            for (_, _, id, _) in m {
                expected.entry(id.to_lowercase()).or_default().push((d, li + 1));
            }
            if rules.wikilinks {
                let mut from = 0;
                while let Some(i) = line[from..].find("[[") {
                    let open = from + i + 2;
                    let Some(j) = line[open..].find("]]") else { break };
                    let inner = &line[open..open + j];
                    from = open;
                    if inner.contains('[') || inner.contains(']') {
                        continue;
                    }
                    let target = registry::wikilink_target(inner);
                    if !target.is_empty() {
                        expected.entry(target.to_lowercase()).or_default().push((d, li + 1));
                    }
                }
            }
            if !names.is_empty() {
                let lower = line.to_ascii_lowercase();
                for name in names.sorted() {
                    let b = lower.as_bytes();
                    let mut from = 0;
                    while let Some(i) = lower[from..].find(name) {
                        let s = from + i;
                        let e = s + name.len();
                        from = s + 1;
                        if (s == 0 || !is_word(b[s - 1])) && (e == b.len() || !is_word(b[e])) {
                            expected.entry(name.to_string()).or_default().push((d, li + 1));
                        }
                    }
                }
            }
        }
        for h in markdown::headings(&text) {
            if phrases.len() < MAX_PHRASES {
                phrases.push(h.text);
            }
        }
        texts.push((source.clone(), path.clone(), text.to_lowercase()));
    }
    report.docs = texts.len();
    // The index side, one document at a time.
    let mut indexed: HashMap<usize, HashSet<(String, i64)>> = HashMap::new();
    for (id, occ) in &expected {
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        let l = store.lookup(id, 1_000_000, crate::store::DefText::None)?;
        let returned: HashSet<(&str, &str, i64)> =
            l.definitions.iter().chain(&l.mentions).map(|h| (h.source.as_str(), h.path.as_str(), h.line)).collect();
        for &(d, line) in occ {
            if !seen.insert((d, line)) {
                continue;
            }
            report.id_occurrences += 1;
            let (source, path, _) = &texts[d];
            if returned.contains(&(source.as_str(), path.as_str(), line as i64)) {
                continue;
            }
            // Repeat mentions in one section come back as one line with a
            // count; the section's range covers the rest.
            let merged = l.mentions.iter().any(|h| {
                h.count > 1 && h.source == *source && h.path == *path && (h.section_start..=h.section_end).contains(&(line as i64))
            });
            if merged {
                continue;
            }
            if !indexed.contains_key(&d) {
                indexed.insert(d, store.doc_idents(source, path)?);
            }
            let in_index = indexed[&d].contains(&(id.clone(), line as i64));
            report.id_misses.push(IdMiss {
                id: id.clone(),
                source: source.clone(),
                path: path.clone(),
                line,
                stage: if in_index { "lookup" } else { "index" },
            });
        }
    }
    report.ids = expected.len();
    let mut done: HashSet<Vec<String>> = HashSet::new();
    for p in &phrases {
        let w = words(p);
        if w.is_empty() || !done.insert(w.clone()) {
            continue;
        }
        report.phrases += 1;
        let found = store.docs_matching(&phrase_query(&w))?;
        for (source, path, lower) in &texts {
            if !lower.contains(w[0].as_str()) || !holds_phrase(lower, &w) {
                continue;
            }
            report.phrase_hits += 1;
            if !found.contains(&(source.clone(), path.clone())) {
                report.phrase_misses.push(PhraseMiss { phrase: p.clone(), source: source.clone(), path: path.clone() });
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrase_scan_respects_word_runs() {
        let w = words("Ground look: rulings");
        assert!(holds_phrase("the ground look - rulings here", &w));
        assert!(!holds_phrase("background look rulings", &w));
        assert!(!holds_phrase("ground lookrulings", &w));
        assert!(holds_phrase("x ground look rulings", &w));
        assert!(!holds_phrase("ground look\n## rulings", &w), "a phrase stays on one line");
    }
}

/// One definition a grep finds that the code index does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeMiss {
    /// The code root.
    pub root: String,
    /// Path below it.
    pub path: String,
    /// Line, 1-based.
    pub line: u32,
    /// The defined name.
    pub name: String,
    /// The keyword grep matched.
    pub keyword: String,
}

/// What the code recall check found.
#[derive(Debug, Clone, Default)]
pub struct CodeRecall {
    /// Rust files and WGSL files read.
    pub files: (usize, usize),
    /// Definitions grep found (lines in comments and strings excluded).
    pub truth: usize,
    /// Lines grep matched that sit inside a block comment or string.
    pub excluded: usize,
    /// Of `truth`, how many the SCIP or WGSL rows have.
    pub indexed: usize,
    /// Of `truth`, how many only the line-scan rows have.
    pub scan_only: usize,
    /// Definitions grep found and the index lacks entirely.
    pub misses: Vec<CodeMiss>,
    /// Roots never indexed (`atlas --code-index` builds them): not
    /// checked, as the `symbol` tool reports them "not indexed yet".
    pub unindexed: Vec<String>,
}

/// The ground-truth patterns: grep-style regexes over one line. Rust items
/// at a line start after visibility and qualifiers; WGSL functions and
/// structs anywhere at a line start, module-scope `var`, `const`,
/// `override` and `alias` at column 0.
const RUST_DEF_RE: &str = r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|async|unsafe|safe|default|extern(?:\s+"[^"]*")?)\s+)*(?:(fn|struct|enum|trait|type|union|mod|const|static(?:\s+mut)?)\s+|(macro_rules!)\s*)(?:r#)?([A-Za-z_][A-Za-z0-9_]*)"#;
const WGSL_DEF_RE: &str = r"^(?:\s*(?:@[A-Za-z_]+(?:\([^)]*\))?\s*)*(fn|struct)\s+|(?:@[A-Za-z_]+(?:\([^)]*\))?\s*)*(var)(?:<[^>]*>)?\s+|(const|override|alias)\s+)([A-Za-z_][A-Za-z0-9_]*)";

/// Compares the code index with a grep of every `.rs` and `.wgsl` file of
/// the configured roots: every definition grep finds must be a definition
/// row at the same line with the same name. A root with no index yet
/// is listed in [`CodeRecall::unindexed`] and not checked.
///
/// # Arguments
///
/// * `conn` - the database the code index wrote.
/// * `roots` - the configured code roots.
///
/// # Errors
///
/// [`crate::error::Error::Io`] when a root cannot be walked,
/// [`crate::error::Error::Db`] from SQLite.
pub fn code_check(conn: &rusqlite::Connection, roots: &[crate::code::CodeRoot]) -> Result<CodeRecall> {
    use std::collections::HashMap;
    let rust_re = crate::regex::Regex::new(RUST_DEF_RE).map_err(|e| crate::error::Error::InvalidArgument(e.to_string()))?;
    let wgsl_re = crate::regex::Regex::new(WGSL_DEF_RE).map_err(|e| crate::error::Error::InvalidArgument(e.to_string()))?;
    let mut rep = CodeRecall::default();
    let built = crate::code::root_records(conn)?;
    for root in roots {
        if !built.contains_key(&root.name) {
            rep.unindexed.push(root.name.clone());
            continue;
        }
        // (path, line, name) -> scan-only?
        let mut have: HashMap<(String, u32, String), bool> = HashMap::new();
        {
            let mut st = conn.prepare(
                "SELECT f.path, o.line, o.name, o.kind FROM code_occ o JOIN code_files f ON f.id = o.file WHERE f.root = ?1 AND o.sect = 0",
            )?;
            let rows = st.query_map([&root.name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
            for r in rows {
                let (p, l, n, k) = r?;
                let scan = k.ends_with("(scan)");
                let e = have.entry((p, l, n)).or_insert(scan);
                *e = *e && scan;
            }
        }
        let walk = crate::code::walk(&root.root)?;
        rep.files.0 += walk.rust.len();
        rep.files.1 += walk.wgsl.len();
        for (files, rust) in [(&walk.rust, true), (&walk.wgsl, false)] {
            for f in files {
                let Some(text) = crate::code::read_source(&root.root, &f.path) else { continue };
                let code = if rust { crate::code::rust_code_lines(&text) } else { wgsl_code_lines(&text) };
                for (i, line) in text.lines().enumerate() {
                    let (kw, name) = if rust {
                        let Some(c) = rust_re.captures(line) else { continue };
                        let kw = c.get(1).or_else(|| c.get(2)).map_or("", |m| m.as_str());
                        (kw.split_whitespace().next().unwrap_or("").to_string(), c.get(3).map_or("", |m| m.as_str()))
                    } else {
                        let Some(c) = wgsl_re.captures(line) else { continue };
                        let kw = (1..=3).find_map(|g| c.get(g)).map_or("", |m| m.as_str()).to_string();
                        (kw, c.get(4).map_or("", |m| m.as_str()))
                    };
                    if name == "_" {
                        continue;
                    }
                    if !code.get(i).copied().unwrap_or(true) {
                        rep.excluded += 1;
                        continue;
                    }
                    rep.truth += 1;
                    match have.get(&(f.path.clone(), i as u32 + 1, name.to_string())) {
                        Some(false) => rep.indexed += 1,
                        Some(true) => rep.scan_only += 1,
                        None => rep.misses.push(CodeMiss { root: root.name.clone(), path: f.path.clone(), line: i as u32 + 1, name: name.to_string(), keyword: kw }),
                    }
                }
            }
        }
    }
    Ok(rep)
}

/// For each WGSL line, whether it starts outside a block comment.
fn wgsl_code_lines(text: &str) -> Vec<bool> {
    let b = text.as_bytes();
    let mut out = vec![true];
    let (mut i, mut depth) = (0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'\n' => {
                out.push(depth == 0);
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                depth += 1;
                i += 2;
            }
            b'*' if depth > 0 && b.get(i + 1) == Some(&b'/') => {
                depth -= 1;
                i += 2;
            }
            b'/' if depth == 0 && b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    out
}
