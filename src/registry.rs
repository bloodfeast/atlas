//! The identifier registry: which identifiers a document defines and which
//! it only mentions.
//!
//! Identifiers come from three places, none of them hard-coded:
//!
//! * **Patterns** from the configuration ([`PatternSpec`]), each a regex
//!   with a kind name (`decision`, `pr`, ...). A pattern with a capture group
//!   named `id` takes the identifier from that group, so context can be
//!   matched without being part of the identifier.
//! * **Names**: every file stem of a source with `names_kind` set (memory
//!   slugs, say) is an identifier, mentioned wherever the stem appears as a
//!   whole word (`[A-Za-z0-9_-]` runs).
//! * **Wikilinks**: the target of every `[[target#heading|alias]]` (its last
//!   path segment, `.md` dropped) is a mention of the note of that name, and
//!   every document's file stem is a `note` identifier it defines.
//!
//! Every line is scanned, frontmatter and fenced code included, so the
//! registry never finds less than a plain regex over the same file.
//!
//! # The definition heuristic
//!
//! An occurrence is a *definition* when one of these holds, strongest first
//! ([`Why::rank`]); everything else is a *mention*:
//!
//! 0. `field`: the line ends with atlas's marker `<!-- atlas defines: ID ... -->`
//!    and the identifier is listed in it. Written by `atlas ruling` (see
//!    [`crate::records`]): the definition is an explicit field, never read
//!    from the prose. A listed identifier no pattern matches is still
//!    defined, with kind `field`.
//! 1. `frontmatter`: the line is a frontmatter `key: value` whose key is one
//!    of the source's `id_keys` (default `id`) and whose value is the
//!    identifier.
//! 2. `file`: the document's file stem is the identifier, or starts with it
//!    followed by a space (`D-004 Topic.md`, `PI-002 Name.md`).
//! 3. `heading`: the identifier is the first identifier on a heading line
//!    (the first identifier is what a heading is about; later ones are
//!    cross-references).
//! 4. `table_row`: the identifier is the first identifier on a table row and
//!    sits in its first cell.
//! 5. `list_item`: the identifier is the first identifier on a list item,
//!    only markup (`*`, `_`, `` ` ``, `~`, spaces) stands between the marker
//!    and it, and it is followed (after closing markup) by the end of the
//!    line, `:`, ` - `, ` (`, ` =` or a dash: the item introduces it rather
//!    than citing it. `- [[X]]` is a citation; `- **X** (date): ...` is a
//!    definition. A leading ISO date, with an optional time, counts as markup
//!    (`- 2026-10-30 X: ...`).
//!
//! Headings, table rows and list items inside fenced code never define.

use std::collections::{HashMap, HashSet};

use crate::regex::{Regex, RegexBuilder};

use crate::chunk::{self, Chunk};
use crate::error::{Error, Result};
use crate::markdown;

/// One identifier pattern as written in the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternSpec {
    /// The kind name results carry (`decision`, `pr`, `work`).
    pub name: String,
    /// The regex. A capture group named `id` selects the identifier.
    pub regex: String,
    /// Match case-insensitively (default false).
    pub case_insensitive: bool,
    /// Matches to ignore, compared case-insensitively (`UTF-8`, say).
    pub exclude: Vec<String>,
}

/// One compiled pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    /// The kind name.
    pub name: String,
    /// The compiled regex.
    pub re: Regex,
    /// True when the regex has a group named `id`.
    pub grouped: bool,
    /// Lowercased matches to ignore.
    pub exclude: HashSet<String>,
}

/// The compiled identifier rules.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    /// Patterns, in configuration order; when two match the same text on a
    /// line, the earlier pattern's kind is kept.
    pub patterns: Vec<Pattern>,
    /// Record wikilink targets and file stems as `note` identifiers.
    pub wikilinks: bool,
}

/// Kind name of file-stem identifiers and wikilink mentions.
pub const NOTE_KIND: &str = "note";
/// Kind name of a frontmatter id that no pattern matches.
pub const FRONTMATTER_KIND: &str = "frontmatter";
/// Kind name of a marker-defined id that no pattern matches.
pub const FIELD_KIND: &str = "field";
/// The opening of atlas's definition marker.
pub const DEFINES_MARK: &str = "<!-- atlas defines:";

/// The identifiers a line's definition marker lists, if it has one.
pub fn marker_ids(line: &str) -> Option<Vec<&str>> {
    let at = line.rfind(DEFINES_MARK)?;
    let rest = &line[at + DEFINES_MARK.len()..];
    let end = rest.find("-->")?;
    if !line[at + DEFINES_MARK.len() + end + 3..].trim().is_empty() {
        return None;
    }
    Some(rest[..end].split_whitespace().collect())
}

impl Rules {
    /// Compiles the configured patterns.
    ///
    /// # Arguments
    ///
    /// * `specs` - the patterns, in priority order.
    /// * `wikilinks` - see [`Rules::wikilinks`].
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for an empty or duplicate kind name, a kind
    /// name that is reserved (`note`, `frontmatter`), a regex that does not
    /// compile within 1 MiB, or one that matches the empty string.
    pub fn new(specs: &[PatternSpec], wikilinks: bool) -> Result<Rules> {
        let mut patterns = Vec::with_capacity(specs.len());
        for s in specs {
            if s.name.is_empty() || s.name.len() > 32 || !s.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                return Err(Error::InvalidArgument(format!("id pattern name {:?} must be 1 to 32 characters of A-Z a-z 0-9 _ -", s.name)));
            }
            if s.name == NOTE_KIND || s.name == FRONTMATTER_KIND || s.name == FIELD_KIND {
                return Err(Error::InvalidArgument(format!("id pattern name {:?} is reserved", s.name)));
            }
            if patterns.iter().any(|p: &Pattern| p.name == s.name) {
                return Err(Error::InvalidArgument(format!("id pattern name {:?} given twice", s.name)));
            }
            if s.regex.chars().any(char::is_control) {
                // JSON turns a single-backslash b into a backspace; the pattern almost certainly meant a word boundary.
                return Err(Error::InvalidArgument(format!(
                    "id pattern {} holds a control character; in JSON every regex backslash is written twice",
                    s.name
                )));
            }
            let re = RegexBuilder::new(&s.regex)
                .case_insensitive(s.case_insensitive)
                .build()
                .map_err(|e| Error::InvalidArgument(format!("id pattern {}: {e}", s.name)))?;
            if re.is_match("") {
                return Err(Error::InvalidArgument(format!("id pattern {} matches the empty string", s.name)));
            }
            let grouped = re.capture_names().any(|n| n == Some("id"));
            patterns.push(Pattern {
                name: s.name.clone(),
                re,
                grouped,
                exclude: s.exclude.iter().map(|x| x.to_lowercase()).collect(),
            });
        }
        Ok(Rules { patterns, wikilinks })
    }

    /// Every match of every pattern in `text`: `(start, end, identifier,
    /// pattern index)`, in pattern order then text order.
    pub fn matches<'t>(&self, text: &'t str, out: &mut Vec<(usize, usize, &'t str, usize)>) {
        for (k, p) in self.patterns.iter().enumerate() {
            if p.grouped {
                for c in p.re.captures_iter(text) {
                    if let Some(m) = c.name("id") {
                        if !m.as_str().is_empty() && !p.excluded(m.as_str()) {
                            out.push((m.start(), m.end(), m.as_str(), k));
                        }
                    }
                }
            } else {
                for m in p.re.find_iter(text) {
                    if !m.as_str().is_empty() && !p.excluded(m.as_str()) {
                        out.push((m.start(), m.end(), m.as_str(), k));
                    }
                }
            }
        }
    }
}

impl Pattern {
    fn excluded(&self, s: &str) -> bool {
        !self.exclude.is_empty() && self.exclude.contains(&s.to_lowercase())
    }
}

/// The names identifiers: lowercased name to `(name as written, kind)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    map: HashMap<String, (String, String)>,
}

impl Names {
    /// True for a string that can be a name: `[A-Za-z0-9_-]+`, 2 to 128
    /// bytes, with at least one letter.
    pub fn valid(s: &str) -> bool {
        (2..=128).contains(&s.len())
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            && s.bytes().any(|b| b.is_ascii_alphabetic())
    }

    /// Adds a name; invalid names are ignored.
    pub fn insert(&mut self, name: &str, kind: &str) {
        if Names::valid(name) {
            self.map.entry(name.to_ascii_lowercase()).or_insert_with(|| (name.to_string(), kind.to_string()));
        }
    }

    /// The name and kind for a lowercased token.
    pub fn get(&self, lower: &str) -> Option<&(String, String)> {
        self.map.get(lower)
    }

    /// True when there are no names.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The names as text, one `kind<TAB>name` line each, sorted, for
    /// storing between runs.
    pub fn encode(&self) -> String {
        let mut v: Vec<(&String, &(String, String))> = self.map.iter().collect();
        v.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut out = String::new();
        for (_, (name, kind)) in v {
            out.push_str(kind);
            out.push('\t');
            out.push_str(name);
            out.push('\n');
        }
        out
    }

    /// Reads what [`Names::encode`] wrote.
    pub fn decode(s: &str) -> Names {
        let mut n = Names::default();
        for line in s.lines() {
            if let Some((kind, name)) = line.split_once('\t') {
                n.insert(name, kind);
            }
        }
        n
    }

    /// Every name, lowercased, sorted.
    pub fn sorted(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.map.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }
}

/// Why an occurrence is a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Why {
    /// Listed in atlas's definition marker.
    Field,
    /// A frontmatter id key.
    Frontmatter,
    /// The file is named by it.
    File,
    /// The first identifier on a heading.
    Heading,
    /// The first identifier in a table row's first cell.
    TableRow,
    /// A list item that introduces it.
    ListItem,
}

impl Why {
    /// The name stored and returned.
    pub fn as_str(self) -> &'static str {
        match self {
            Why::Field => "field",
            Why::Frontmatter => "frontmatter",
            Why::File => "file",
            Why::Heading => "heading",
            Why::TableRow => "table_row",
            Why::ListItem => "list_item",
        }
    }

    /// Strength, 6 (field) down to 1 (list item); mentions are 0.
    pub fn rank(self) -> i64 {
        match self {
            Why::Field => 6,
            Why::Frontmatter => 5,
            Why::File => 4,
            Why::Heading => 3,
            Why::TableRow => 2,
            Why::ListItem => 1,
        }
    }
}

/// One identifier occurrence in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// The identifier as written.
    pub ident: String,
    /// The kind (a pattern name, a names kind, `note` or `frontmatter`).
    pub kind: String,
    /// 1-based line.
    pub line: usize,
    /// Set when the occurrence defines the identifier.
    pub def: Option<Why>,
    /// Index of the section (from [`chunk::chunks`]) holding the line.
    pub chunk: usize,
    /// The first `YYYY-MM-DD` on the line, if any.
    pub date: Option<String>,
}

/// What [`extract`] needs to know about a document besides its text.
#[derive(Debug, Clone, Copy)]
pub struct DocInfo<'a> {
    /// The file stem (name without folders and extension).
    pub stem: &'a str,
    /// Frontmatter keys that name the identifier the document defines.
    pub id_keys: &'a [String],
    /// The kind of the document's own stem when its source lists names.
    pub names_kind: Option<&'a str>,
}

/// The inner text of every wikilink, left to right (was the
/// `captures_iter` of `\[\[([^\[\]\n]+?)\]\]`, group 1).
pub(crate) fn wikilink_inners(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut q = 0;
    while let Some(p) = crate::lex::find_str(s, q, "[[") {
        match crate::markdown::wiki_inner(s, p + 2) {
            Some(e) => {
                out.push((p + 2, e));
                q = e + 2;
            }
            None => q = p + 1,
        }
    }
    out
}

/// Every run of `[A-Za-z0-9_-]`, left to right (was the `find_iter` of
/// `[A-Za-z0-9_-]+`).
pub(crate) fn tokens(s: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    const TOKEN: crate::lex::Ascii = crate::lex::Ascii::new(b"_-", &[(b'a', b'z'), (b'A', b'Z'), (b'0', b'9')]);
    let b = s.as_bytes();
    let tok = |c: u8| TOKEN.has(c);
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < b.len() && !tok(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        let a = i;
        while i < b.len() && tok(b[i]) {
            i += 1;
        }
        Some((a, i))
    })
}

#[cfg(test)]
pub(crate) fn lex_sites() -> Vec<crate::lex::props::Site> {
    use crate::lex::props::{Mode, Site};
    vec![
        Site {
            name: "registry wikilink",
            pattern: r"\[\[([^\[\]\n]+?)\]\]".into(),
            tokens: &["[[", "]]", "[", "]", "a", "|", "#", "\n", "[[x]]"],
            examples: &["see [[a/b.md#h|alias]] and [[c]]"],
            mode: Mode::All(|s| wikilink_inners(s).into_iter().map(|(a, b)| vec![Some((a - 2, b + 2)), Some((a, b))]).collect()),
        },
        Site {
            name: "registry token",
            pattern: r"[A-Za-z0-9_-]+".into(),
            tokens: &["a", "Z", "0", "_", "-", " ", "\u{e9}", "TK-1"],
            examples: &["TK-001 and PI-2"],
            mode: Mode::All(|s| tokens(s).map(|(a, b)| vec![Some((a, b))]).collect()),
        },
    ]
}

/// The note a wikilink's inner text points at: the part before `|` and `#`,
/// its last path segment, without `.md`.
pub fn wikilink_target(inner: &str) -> &str {
    let t = inner.split('|').next().unwrap_or(inner);
    let t = t.split('#').next().unwrap_or(t).trim();
    let t = t.rsplit('/').next().unwrap_or(t);
    t.strip_suffix(".md").unwrap_or(t).trim()
}

const MARKUP: &[char] = &['*', '_', '`', '~', ' ', '\t'];

/// The bytes a list body's leading date takes: markup, an ISO date
/// (`YYYY-MM-DD`), an optional time (`T` or a space, `HH:MM`, optional
/// `:SS` and `Z`), then the markup after it, which must not be empty. 0 when
/// the body does not start with a date.
fn date_lead(body: &str) -> usize {
    let b = body.as_bytes();
    let digits = |at: usize, n: usize| b.len() >= at + n && b[at..at + n].iter().all(u8::is_ascii_digit);
    let mut i = body.len() - body.trim_start_matches(MARKUP).len();
    if !(digits(i, 4) && b.get(i + 4) == Some(&b'-') && digits(i + 5, 2) && b.get(i + 7) == Some(&b'-') && digits(i + 8, 2)) {
        return 0;
    }
    i += 10;
    if matches!(b.get(i), Some(b'T' | b' ')) && digits(i + 1, 2) && b.get(i + 3) == Some(&b':') && digits(i + 4, 2) {
        i += 6;
        if b.get(i) == Some(&b':') && digits(i + 1, 2) {
            i += 3;
        }
        if b.get(i) == Some(&b'Z') {
            i += 1;
        }
    }
    // Closing markup of a bold or code date, then at least one space.
    let close = body[i..].len() - body[i..].trim_start_matches(['*', '_', '`', '~']).len();
    let rest = &body[i + close..];
    let space = rest.len() - rest.trim_start_matches(MARKUP).len();
    if space == 0 || !rest.starts_with([' ', '\t']) {
        return 0;
    }
    i + close + space
}

fn list_body(line: &str) -> Option<&str> {
    let t = line.trim_start_matches([' ', '\t']);
    let b = t.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'-' | b'*' | b'+') && (b[1] == b' ' || b[1] == b'\t') {
        return Some(&t[2..]);
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits) && b.len() > digits + 1 && matches!(b[digits], b'.' | b')') && b[digits + 1] == b' ' {
        return Some(&t[digits + 2..]);
    }
    None
}

/// True when the text after an identifier marks a list item as introducing
/// it.
fn introduces(after: &str) -> bool {
    let a = after.trim_start_matches(['*', '_', '`', '~', ']']);
    a.is_empty()
        || a.starts_with(':')
        || a.starts_with(" -")
        || a.starts_with(" (")
        || a.starts_with(" =")
        || a.starts_with(" \u{2014}")
        || a.starts_with(" \u{2013}")
        || a.starts_with('\u{2014}')
        || a.trim().is_empty()
}

/// Finds every identifier occurrence in a document and classifies it.
///
/// # Arguments
///
/// * `rules` - the compiled patterns and the wikilink switch.
/// * `names` - the names identifiers.
/// * `text` - the document.
/// * `chunks` - its sections, from [`chunk::chunks`].
/// * `info` - stem, id keys and names kind.
///
/// # Returns
///
/// The occurrences, in line order, at most one per identifier per line (the
/// strongest), plus the file-level definitions (line 1).
pub fn extract(rules: &Rules, names: &Names, text: &str, chunks: &[Chunk], info: &DocInfo<'_>) -> Vec<Occurrence> {
    let spans = markdown::line_spans(text);
    let code = markdown::code_lines(text, &spans);
    let fm_end = markdown::frontmatter_span(text).map(|f| f.end).unwrap_or(0);
    // (start, end, ident, kind)
    let mut raw: Vec<(usize, usize, &str, &str)> = Vec::new();
    if !rules.patterns.is_empty() {
        // Line by line, as grep sees the file, so `^` and `$` mean the line.
        let mut m: Vec<(usize, usize, &str, usize)> = Vec::new();
        for &(ls, le, _) in &spans {
            m.clear();
            rules.matches(&text[ls..le], &mut m);
            raw.extend(m.iter().map(|&(s, e, id, k)| (ls + s, ls + e, id, rules.patterns[k].name.as_str())));
        }
    }
    if !names.is_empty() {
        let mut buf = String::new();
        for (a, b) in tokens(text) {
            let t = &text[a..b];
            if t.len() < 2 || t.len() > 128 {
                continue;
            }
            buf.clear();
            buf.extend(t.chars().map(|c| c.to_ascii_lowercase()));
            if let Some((_, kind)) = names.get(&buf) {
                raw.push((a, b, t, kind.as_str()));
            }
        }
    }
    if rules.wikilinks {
        for (a, b) in wikilink_inners(text) {
            let inner = &text[a..b];
            let target = wikilink_target(inner);
            if !target.is_empty() {
                let off = inner.find(target).unwrap_or(0) + a;
                raw.push((off, off + target.len(), target, NOTE_KIND));
            }
        }
    }
    // Text order; at equal starts the longer match first, then config order
    // (the sort is stable).
    raw.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let line_of = |off: usize| -> usize { spans.partition_point(|s| s.0 <= off).max(1) - 1 };
    let chunk_of = |line1: usize| -> usize { chunks.partition_point(|c| c.line_start <= line1).max(1) - 1 };
    let fm = markdown::parse_frontmatter(text);
    let fm_ids: Vec<String> =
        info.id_keys.iter().filter_map(|k| fm.get(k).and_then(|v| v.as_str()).map(|s| s.trim().to_lowercase())).collect();

    let mut out: Vec<Occurrence> = Vec::with_capacity(raw.len() + 2);
    let mut seen: HashMap<(String, usize), usize> = HashMap::new();
    let mut first_on_line: Option<(usize, usize)> = None; // (line index, start)
    for &(s, e, ident, kind) in &raw {
        let li = line_of(s);
        let (ls, le, _) = spans[li];
        if first_on_line.is_none_or(|(l, _)| l != li) {
            first_on_line = Some((li, s));
        }
        let is_first = first_on_line.is_some_and(|(_, fs)| fs == s);
        let line = &text[ls..le];
        let def = if s < fm_end {
            let key = line.split(':').next().unwrap_or("").trim();
            let val = line.split_once(':').map(|(_, v)| v.trim().trim_matches(['"', '\''])).unwrap_or("");
            (info.id_keys.iter().any(|k| k == key) && val.eq_ignore_ascii_case(ident)).then_some(Why::Frontmatter)
        } else if code[li] {
            None
        } else if chunk::heading(line).is_some() {
            is_first.then_some(Why::Heading)
        } else if line.trim_start().starts_with('|') {
            let row = line.trim_start();
            let row_off = le - row.len();
            let first_cell_end = row[1..].find('|').map(|i| row_off + 1 + i).unwrap_or(le);
            (is_first && e <= first_cell_end).then_some(Why::TableRow)
        } else if let Some(body) = list_body(line) {
            let body_off = le - body.len();
            // A leading date (`- 2026-10-30 X: ...`, a dated ruling) is markup
            // too: the item still introduces the identifier after it.
            let markup_from = |from: usize| s >= from && text[from..s].chars().all(|c| MARKUP.contains(&c));
            let prefix_ok = markup_from(body_off) || markup_from(body_off + date_lead(body));
            (is_first && prefix_ok && introduces(&text[e..le])).then_some(Why::ListItem)
        } else {
            None
        };
        let key = (ident.to_lowercase(), li);
        if let Some(&k) = seen.get(&key) {
            let prev: &mut Occurrence = &mut out[k];
            if def.is_some_and(|d| prev.def.is_none_or(|p| d.rank() > p.rank())) {
                prev.def = def;
            }
            continue;
        }
        seen.insert(key, out.len());
        out.push(Occurrence {
            ident: ident.to_string(),
            kind: kind.to_string(),
            line: li + 1,
            def,
            chunk: chunk_of(li + 1),
            date: chunk::find_date(line).map(str::to_string),
        });
    }
    // Frontmatter ids that no pattern or name matched.
    if fm_end > 0 && !fm_ids.is_empty() {
        for (li, &(ls, le, _)) in spans.iter().enumerate() {
            if ls >= fm_end {
                break;
            }
            let line = &text[ls..le];
            let Some((key, val)) = line.split_once(':') else { continue };
            let val = val.trim().trim_matches(['"', '\'']);
            if !info.id_keys.iter().any(|k| k == key.trim()) || val.is_empty() || val.len() > 128 {
                continue;
            }
            if seen.contains_key(&(val.to_lowercase(), li)) {
                continue;
            }
            seen.insert((val.to_lowercase(), li), out.len());
            out.push(Occurrence {
                ident: val.to_string(),
                kind: FRONTMATTER_KIND.to_string(),
                line: li + 1,
                def: Some(Why::Frontmatter),
                chunk: chunk_of(li + 1),
                date: None,
            });
        }
    }
    // File-level definitions: the stem itself, and any pattern match that
    // starts the stem.
    let mut files: Vec<(String, String)> = Vec::new();
    if let Some(k) = info.names_kind.filter(|_| Names::valid(info.stem)) {
        files.push((info.stem.to_string(), k.to_string()));
    } else if rules.wikilinks && !info.stem.is_empty() {
        files.push((info.stem.to_string(), NOTE_KIND.to_string()));
    }
    {
        let mut m = Vec::new();
        rules.matches(info.stem, &mut m);
        for (s, e, id, k) in m {
            let rest = &info.stem[e..];
            if s == 0 && (rest.is_empty() || rest.starts_with(' ')) && !files.iter().any(|f| f.0.eq_ignore_ascii_case(id)) {
                files.push((id.to_string(), rules.patterns[k].name.clone()));
            }
        }
    }
    // Atlas's definition marker: explicit fields, outside fenced code.
    for (li, &(ls, le, _)) in spans.iter().enumerate() {
        if code[li] || ls < fm_end {
            continue;
        }
        let line = &text[ls..le];
        let Some(ids) = marker_ids(line) else { continue };
        for id in ids {
            if id.len() > 128 {
                continue;
            }
            match seen.get(&(id.to_lowercase(), li)) {
                Some(&k) => out[k].def = Some(Why::Field),
                None => {
                    let mut m = Vec::new();
                    rules.matches(id, &mut m);
                    let kind = m.iter().find(|x| x.2 == id).map_or(FIELD_KIND.to_string(), |x| rules.patterns[x.3].name.clone());
                    seen.insert((id.to_lowercase(), li), out.len());
                    out.push(Occurrence { ident: id.to_string(), kind, line: li + 1, def: Some(Why::Field), chunk: chunk_of(li + 1), date: chunk::find_date(line).map(str::to_string) });
                }
            }
        }
    }
    for (ident, kind) in files {
        let key = (ident.to_lowercase(), usize::MAX);
        if seen.contains_key(&key) {
            continue;
        }
        seen.insert(key, out.len());
        out.push(Occurrence { ident, kind, line: 1, def: Some(Why::File), chunk: 0, date: None });
    }
    out.sort_by_key(|o| o.line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{chunks, ChunkRule};

    fn rules() -> Rules {
        Rules::new(
            &[
                PatternSpec { name: "ticket".into(), regex: r"\bTK-[0-9]{3}\b".into(), case_insensitive: false, exclude: vec![] },
                PatternSpec {
                    name: "pr".into(),
                    regex: r"(?:^|[^A-Za-z0-9_&/])(?P<id>#[0-9]{1,5})\b".into(),
                    case_insensitive: false,
                    exclude: vec![],
                },
                PatternSpec {
                    name: "work".into(),
                    regex: r"\b[A-Z][A-Z0-9]+(?:-[A-Z][A-Z0-9]+)+\b".into(),
                    case_insensitive: false,
                    exclude: vec!["NOT-THIS".into()],
                },
            ],
            true,
        )
        .unwrap()
    }

    fn run(text: &str, stem: &str) -> Vec<(String, String, usize, Option<&'static str>)> {
        let mut names = Names::default();
        names.insert("slug-one", "memo");
        let keys = vec!["id".to_string()];
        let c = chunks(text, &ChunkRule::default());
        extract(&rules(), &names, text, &c, &DocInfo { stem, id_keys: &keys, names_kind: None })
            .into_iter()
            .map(|o| (o.ident, o.kind, o.line, o.def.map(Why::as_str)))
            .collect()
    }

    #[test]
    fn definitions_follow_the_heuristic() {
        let text = "---\nid: TK-001\n---\n# TK-002 and TK-003\n\
- **TK-004** (2026-01-01): introduced here\n\
- see TK-005 for detail\n\
- [[TK-006 Name]]\n\
| TK-007 | uses TK-008 |\n\
Prose cites TK-009, BIG-WORK and #12 and NOT-THIS and slug-one.\n\
```\n# TK-010\n```\n";
        let got = run(text, "TK-011 Name");
        let find = |id: &str| got.iter().find(|g| g.0 == id).unwrap_or_else(|| panic!("{id} missing from {got:?}")).clone();
        assert_eq!(find("TK-001").3, Some("frontmatter"));
        assert_eq!(find("TK-002").3, Some("heading"));
        assert_eq!(find("TK-003").3, None, "only the first id on a heading defines");
        assert_eq!(find("TK-004").3, Some("list_item"));
        assert_eq!(find("TK-005").3, None, "an item that cites is a mention");
        assert_eq!(find("TK-006").3, None, "a link item is a citation");
        assert_eq!(find("TK-006 Name").1, "note");
        assert_eq!(find("TK-007").3, Some("table_row"));
        assert_eq!(find("TK-008").3, None);
        assert_eq!(find("TK-009").3, None);
        assert_eq!(find("BIG-WORK").1, "work");
        assert_eq!(find("#12").1, "pr");
        assert_eq!(find("slug-one").1, "memo");
        assert_eq!(find("TK-010").3, None, "headings in code never define");
        assert_eq!(find("TK-011").3, Some("file"));
        assert_eq!(find("TK-011 Name").3, Some("file"));
        assert!(!got.iter().any(|g| g.0 == "NOT-THIS"), "excluded");
    }

    /// Dated rulings (`- 2026-10-30 X: ...`): the leading date, with or
    /// without a time, is markup, so the item still introduces its id.
    #[test]
    fn a_leading_date_does_not_stop_a_list_item_defining() {
        let text = "# Rulings\n\
- 2026-10-30 TK-020: a dated ruling\n\
- 2026-10-30T14:05Z **TK-021** (scope): a timed one\n\
- **2026-10-30** TK-022 - a bold date\n\
- 2026-10-30 14:05:09 CACHE-RACE: a work name after a spaced time\n\
- 2026-10-30 see TK-023 for detail\n\
- 2026-1-30 TK-025: not an ISO date\n";
        let got = run(text, "Rulings");
        let def = |id: &str| got.iter().find(|g| g.0 == id).unwrap_or_else(|| panic!("{id} missing from {got:?}")).3;
        assert_eq!(def("TK-020"), Some("list_item"));
        assert_eq!(def("TK-021"), Some("list_item"));
        assert_eq!(def("TK-022"), Some("list_item"));
        assert_eq!(def("CACHE-RACE"), Some("list_item"));
        assert_eq!(def("TK-023"), None, "a dated item that cites is still a mention");
        assert_eq!(def("TK-025"), None, "only an ISO date is markup");
        assert_eq!(date_lead("2026-10-30 X"), 11);
        assert_eq!(date_lead("**2026-10-30T14:05:09Z** X"), 25);
        assert_eq!(date_lead("2026-10-30: X"), 0, "a date followed by a colon is the item's subject");
        assert_eq!(date_lead("X 2026-10-30"), 0);
    }

    #[test]
    fn patterns_with_control_characters_are_refused() {
        // What a JSON config holding a single-backslash b parses to.
        let spec = PatternSpec { name: "t".into(), regex: "\u{8}TK-[0-9]+".into(), case_insensitive: false, exclude: vec![] };
        assert!(Rules::new(&[spec], true).is_err());
    }

    #[test]
    fn wikilink_targets() {
        assert_eq!(wikilink_target("Folder/Note.md#Head|alias"), "Note");
        assert_eq!(wikilink_target(" X "), "X");
    }
}
