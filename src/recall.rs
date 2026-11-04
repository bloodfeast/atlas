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
use crate::lex;
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
    /// The file's language (`rust`, `wgsl`, `go`).
    pub lang: String,
}

/// The recall check of one language of [`crate::langs`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LangRecall {
    /// The language.
    pub lang: String,
    /// Its files read.
    pub files: usize,
    /// Definitions its truth regex found (lines inside block comments and
    /// multi-line strings excluded).
    pub truth: usize,
    /// Lines the regex matched inside a block comment or multi-line string.
    pub excluded: usize,
    /// Of `truth`, how many SCIP rows have.
    pub indexed: usize,
    /// Of `truth`, how many only scanner rows have.
    pub scan_only: usize,
    /// Of `truth`, how many the index lacks (listed in
    /// [`CodeRecall::misses`]).
    pub missing: usize,
}

/// What the code recall check found.
#[derive(Debug, Clone, Default)]
pub struct CodeRecall {
    /// Rust files and WGSL files read.
    pub files: (usize, usize),
    /// Each language of [`crate::langs`] a root has files of; the counts
    /// above are Rust's and WGSL's alone.
    pub langs: Vec<LangRecall>,
    /// Definitions grep found (lines in comments and strings excluded).
    pub truth: usize,
    /// Lines grep matched that sit inside a block comment or string.
    pub excluded: usize,
    /// Of `truth`, how many the SCIP or WGSL rows have.
    pub indexed: usize,
    /// Of `truth`, how many only the line-scan rows have.
    pub scan_only: usize,
    /// Definitions grep found and the index lacks entirely (every
    /// language's).
    pub misses: Vec<CodeMiss>,
    /// Roots never indexed (`atlas --code-index` builds them): not
    /// checked, as the `symbol` tool reports them "not indexed yet".
    pub unindexed: Vec<String>,
}

// The ground truth: grep-style line scans. Rust items at a line start
// after visibility and qualifiers; WGSL functions and structs anywhere at a
// line start, module-scope `var`, `const`, `override` and `alias` at
// column 0.

#[cfg(test)]
pub(crate) fn lex_sites() -> Vec<lex::props::Site> {
    use lex::props::{v, Mode, Site};
    vec![
        Site {
            name: "recall rust definition",
            pattern: r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|async|unsafe|safe|default|extern(?:\s+"[^"]*")?)\s+)*(?:(fn|struct|enum|trait|type|union|mod|const|static(?:\s+mut)?)\s+|(macro_rules!)\s*)(?:r#)?([A-Za-z_][A-Za-z0-9_]*)"#.into(),
            tokens: &[
                "pub", "pub(crate)", "(", ")", " ", "\t", "const", "async", "unsafe", "safe", "default", "extern", "\"C\"", "\"", "fn", "struct", "enum", "trait", "type",
                "union", "mod", "static", "mut", "macro_rules!", "r#", "x", "X1", "_", "1", "{", "<T>",
            ],
            examples: &[
                "pub(crate) const unsafe fn x() {}",
                "  pub extern \"C\" fn y_1()",
                "const MAX: u32 = 1;",
                "static mut COUNT: usize = 0;",
                "static mutable: u8 = 0;",
                "macro_rules! m {",
                "pub fn r#type()",
                "unsafe extern \"system\" fn z",
                "pub(in a::b) struct S;",
            ],
            mode: Mode::One(|s| rust_def(s).map(v)),
        },
        Site {
            name: "recall wgsl definition",
            pattern: r"^(?:\s*(?:@[A-Za-z_]+(?:\([^)]*\))?\s*)*(fn|struct)\s+|(?:@[A-Za-z_]+(?:\([^)]*\))?\s*)*(var)(?:<[^>]*>)?\s+|(const|override|alias)\s+)([A-Za-z_][A-Za-z0-9_]*)".into(),
            tokens: &["@", "@vertex", "@group(0)", "(", ")", " ", "\t", "fn", "struct", "var", "<uniform>", "<", ">", "const", "override", "alias", "x", "_y", "1", "a"],
            examples: &[
                "@vertex fn vs_main(",
                "  @compute @workgroup_size(64) fn cs(",
                "@group(0) @binding(1) var<storage, read> buf: B;",
                "var<private> p: f32;",
                "const N = 4u;",
                "override K: f32;",
                "alias V = vec3f;",
                "struct S {",
                "@vertexfn x",
                "@foovar<f32> y",
            ],
            mode: Mode::One(|s| wgsl_def(s).map(v)),
        },
    ]
}

/// `[A-Za-z_][A-Za-z0-9_]*` at byte `i`: where it ends.
fn ident_at(s: &str, i: usize) -> Option<usize> {
    lex::at(s, i).filter(|&c| lex::ident_start(c)).map(|_| lex::run(s, i + 1, lex::ident))
}

/// A Rust definition line (was
/// `^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|async|unsafe|safe|default|extern(?:\s+"[^"]*")?)\s+)*(?:(fn|struct|enum|trait|type|union|mod|const|static(?:\s+mut)?)\s+|(macro_rules!)\s*)(?:r#)?([A-Za-z_][A-Za-z0-9_]*)`):
/// groups 0, the keyword, `macro_rules!` and the name.
pub(crate) fn rust_def(s: &str) -> Option<[lex::Span; 4]> {
    let mut i = lex::spaces(s, 0);
    // `pub` or `pub(...)`, then space. Without it nothing else can match a
    // `pub` line, so taking it when it is there is the whole choice.
    if let Some(p) = lex::lit(s, i, "pub") {
        let parens = (s.as_bytes().get(p) == Some(&b'(')).then(|| s[p + 1..].find(')').map(|k| p + 2 + k)).flatten();
        if let Some(a) = parens.and_then(|q| lex::spaces1(s, q)).or_else(|| lex::spaces1(s, p)) {
            i = a;
        }
    }
    // The qualifiers, each then space; where each one starts.
    let mut starts = vec![i];
    loop {
        let x = *starts.last().expect("one start");
        // A prefix compare per word: most fail on the first byte, which
        // measured faster than a binary search of six words.
        let Some((e, w)) = lex::one_of(s, x, &["const", "async", "unsafe", "safe", "default", "extern"]) else { break };
        let abi = (w == "extern")
            .then(|| lex::spaces1(s, e).filter(|&q| s.as_bytes().get(q) == Some(&b'"')).and_then(|q| s[q + 1..].find('"').map(|k| q + 2 + k)))
            .flatten()
            .and_then(|q| lex::spaces1(s, q));
        match abi.or_else(|| lex::spaces1(s, e)) {
            Some(n) => starts.push(n),
            None => break,
        }
    }
    // The most qualifiers first: `const` is a qualifier and a keyword.
    starts.iter().rev().find_map(|&x| {
        let name = |n: usize| {
            let r = lex::lit(s, n, "r#").filter(|&r| ident_at(s, r).is_some());
            let n = r.unwrap_or(n);
            ident_at(s, n).map(|e| (n, e))
        };
        if let Some((e, w)) = lex::one_of(s, x, &["fn", "struct", "enum", "trait", "type", "union", "mod", "const", "static"]) {
            let with_mut = (w == "static").then(|| lex::spaces1(s, e).and_then(|m| lex::lit(s, m, "mut"))).flatten();
            for k in [with_mut, Some(e)].into_iter().flatten() {
                if let Some(n) = lex::spaces1(s, k).and_then(name) {
                    return Some([Some((0, n.1)), Some((x, k)), None, Some(n)]);
                }
            }
            return None;
        }
        let e = lex::lit(s, x, "macro_rules!")?;
        let n = name(lex::spaces(s, e))?;
        Some([Some((0, n.1)), None, Some((x, e)), Some(n)])
    })
}

/// A WGSL definition line (was
/// `^(?:\s*(?:@[A-Za-z_]+(?:\([^)]*\))?\s*)*(fn|struct)\s+|(?:@[A-Za-z_]+(?:\([^)]*\))?\s*)*(var)(?:<[^>]*>)?\s+|(const|override|alias)\s+)([A-Za-z_][A-Za-z0-9_]*)`):
/// groups 0, `fn` or `struct`, `var`, `const` `override` or `alias`, and
/// the name.
pub(crate) fn wgsl_def(s: &str) -> Option<[lex::Span; 5]> {
    let fn_or_struct = |x: usize| {
        let (e, _) = lex::one_of(s, x, &["fn", "struct"])?;
        let n = lex::spaces1(s, e)?;
        Some([Some((0, ident_at(s, n)?)), Some((x, e)), None, None, Some((n, ident_at(s, n)?))])
    };
    let var = |x: usize| {
        let e = lex::lit(s, x, "var")?;
        let g = (s.as_bytes().get(e) == Some(&b'<')).then(|| s[e + 1..].find('>').map(|k| e + 2 + k)).flatten().unwrap_or(e);
        let n = lex::spaces1(s, g)?;
        Some([Some((0, ident_at(s, n)?)), None, Some((x, e)), None, Some((n, ident_at(s, n)?))])
    };
    wgsl_attrs_then(s, lex::spaces(s, 0), fn_or_struct).or_else(|| wgsl_attrs_then(s, 0, var)).or_else(|| {
        let (e, _) = lex::one_of(s, 0, &["const", "override", "alias"])?;
        let n = lex::spaces1(s, e)?;
        Some([Some((0, ident_at(s, n)?)), None, None, Some((0, e)), Some((n, ident_at(s, n)?))])
    })
}

/// `(?:@[A-Za-z_]+(?:\([^)]*\))?\s*)*` from byte `x`, then `k`, tried in
/// the order the regex engine backtracks: every attribute whole first,
/// then the last attribute's name cut shorter (the keyword can start
/// inside it: `@vertexfn`), then the one before it, and so on.
fn wgsl_attrs_then<R>(s: &str, x: usize, k: impl Fn(usize) -> Option<R>) -> Option<R> {
    let b = s.as_bytes();
    // (name start, name end) of each attribute, and where the run ends.
    let mut names: Vec<(usize, usize)> = Vec::new();
    let mut y = x;
    while b.get(y) == Some(&b'@') {
        let ne = lex::run(s, y + 1, lex::ident_start);
        if ne == y + 1 {
            break;
        }
        let pe = (b.get(ne) == Some(&b'(')).then(|| s[ne + 1..].find(')').map(|k| ne + 2 + k)).flatten().unwrap_or(ne);
        names.push((y + 1, ne));
        y = lex::spaces(s, pe);
    }
    if let Some(r) = k(y) {
        return Some(r);
    }
    names.iter().rev().find_map(|&(ns, ne)| (ns + 1..ne).rev().find_map(&k))
}

/// Compares the code index with a grep of every `.rs` and `.wgsl` file of
/// the configured roots, and of every file of a language of
/// [`crate::langs`] by its row's truth regex: every definition grep finds
/// must be a definition row at the same line with the same name. A root with no index yet
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
            // Files are read and scanned on several threads; the results
            // come back in file order, so the report is the same.
            let defs = lex::par_map(files, |f| {
                let Some(text) = crate::code::read_source(&root.root, &f.path) else { return Vec::new() };
                let code = if rust { crate::code::rust_code_lines(&text) } else { wgsl_code_lines(&text) };
                let mut out: Vec<(usize, String, String, bool)> = Vec::new();
                for (i, line) in text.lines().enumerate() {
                    let g = |m: lex::Span| m.map(|(a, b)| &line[a..b]);
                    let (kw, name) = if rust {
                        let Some(c) = rust_def(line) else { continue };
                        let kw = g(c[1]).or_else(|| g(c[2])).unwrap_or("");
                        (kw.split_whitespace().next().unwrap_or("").to_string(), g(c[3]).unwrap_or(""))
                    } else {
                        let Some(c) = wgsl_def(line) else { continue };
                        let kw = (1..=3).find_map(|k| g(c[k])).unwrap_or("").to_string();
                        (kw, g(c[4]).unwrap_or(""))
                    };
                    if name != "_" {
                        out.push((i, kw, name.to_string(), code.get(i).copied().unwrap_or(true)));
                    }
                }
                out
            });
            for (f, defs) in files.iter().zip(defs) {
                for (i, kw, name, is_code) in defs {
                    if !is_code {
                        rep.excluded += 1;
                        continue;
                    }
                    rep.truth += 1;
                    match have.get(&(f.path.clone(), i as u32 + 1, name.clone())) {
                        Some(false) => rep.indexed += 1,
                        Some(true) => rep.scan_only += 1,
                        None => rep.misses.push(CodeMiss { root: root.name.clone(), path: f.path.clone(), line: i as u32 + 1, name, keyword: kw, lang: if rust { "rust" } else { "wgsl" }.into() }),
                    }
                }
            }
        }
        for lang in crate::langs::LANGS {
            let files: Vec<&crate::code::WalkedFile> = walk.source.iter().filter(|f| crate::langs::of_path(&f.path).is_some_and(|l| l.name == lang.name)).collect();
            if files.is_empty() {
                continue;
            }
            let re = crate::regex::Regex::new(lang.truth).map_err(|e| crate::error::Error::InvalidArgument(e.to_string()))?;
            let scanner = crate::scanner::of(lang);
            // A constructor's own spelling (Python's `__init__`) is stored
            // under its type's name: such a line is held by a `ctor` row.
            let ctors = ctor_rows(conn, &root.name)?;
            let at = match rep.langs.iter().position(|l| l.lang == lang.name) {
                Some(i) => i,
                None => {
                    rep.langs.push(LangRecall { lang: lang.name.into(), ..LangRecall::default() });
                    rep.langs.len() - 1
                }
            };
            for f in files {
                rep.langs[at].files += 1;
                let Some(text) = crate::code::read_source(&root.root, &f.path) else { continue };
                let code = scanner.code_lines(&text);
                for (i, line) in text.lines().enumerate() {
                    let Some(c) = re.captures(line) else { continue };
                    let (kw, name) = (c.name("kw").map_or("", |m| m.as_str()), c.name("name").map_or("", |m| m.as_str()));
                    if name == "_" {
                        continue;
                    }
                    let l = &mut rep.langs[at];
                    if !code.get(i).copied().unwrap_or(true) {
                        l.excluded += 1;
                        continue;
                    }
                    l.truth += 1;
                    let held = have
                        .get(&(f.path.clone(), i as u32 + 1, name.to_string()))
                        .or_else(|| crate::code::CTORS.contains(&name).then(|| ctors.get(&(f.path.clone(), i as u32 + 1))).flatten());
                    match held {
                        Some(false) => l.indexed += 1,
                        Some(true) => l.scan_only += 1,
                        None => {
                            l.missing += 1;
                            rep.misses.push(CodeMiss { root: root.name.clone(), path: f.path.clone(), line: i as u32 + 1, name: name.to_string(), keyword: kw.to_string(), lang: lang.name.into() });
                        }
                    }
                }
            }
        }
    }
    Ok(rep)
}

/// The constructor definition rows of a root, by (path, line): whether
/// only scanner rows are there.
///
/// # Errors
///
/// [`crate::error::Error::Db`] from SQLite.
pub fn ctor_rows(conn: &rusqlite::Connection, root: &str) -> Result<std::collections::HashMap<(String, u32), bool>> {
    let mut out: std::collections::HashMap<(String, u32), bool> = std::collections::HashMap::new();
    let mut st = conn.prepare(
        "SELECT f.path, o.line, o.kind FROM code_occ o JOIN code_files f ON f.id = o.file WHERE f.root = ?1 AND o.sect = 0 AND o.kind LIKE 'ctor%'",
    )?;
    let rows = st.query_map([root], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?)))?;
    for r in rows {
        let (p, l, k) = r?;
        let scan = k.ends_with("(scan)");
        let e = out.entry((p, l)).or_insert(scan);
        *e = *e && scan;
    }
    Ok(out)
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
