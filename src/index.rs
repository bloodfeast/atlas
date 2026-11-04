//! Generated index regions.
//!
//! An index lives between two marker lines in a note:
//!
//! ```text
//! <!-- vault-mcp:index NAME start {compact JSON spec} -->
//! ...generated...
//! <!-- vault-mcp:index NAME end -->
//! ```
//!
//! Only the bytes from the start marker line to the end marker line are ever
//! rewritten. The spec rides in the start marker, so a refresh needs no
//! arguments, and rendering is deterministic, so an unchanged vault renders
//! byte-identical output.

use std::cmp::Ordering;

use crate::json::{FromJson, Map, ToJson, Value};
use crate::lex;

use crate::error::{Error, Result};
use crate::links;
use crate::store::NoteRow;

/// Where an index draws notes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// Vault short name.
    pub vault: String,
    /// Vault-relative folder (subfolders included); empty for the whole vault.
    pub folder: String,
    /// Optional glob over the path below `folder`.
    pub glob: Option<String>,
}

/// A `key = value` filter. A list value matches when any item equals
/// `value`. The pseudo-keys `folder` and `tag` test the note's folder and
/// tags. Matching ignores ASCII case.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    /// Frontmatter key, or `folder` / `tag`.
    pub key: String,
    /// The value to match.
    pub value: String,
    /// Match when the value starts with `value` instead of equalling it
    /// (`queued` matches `queued 139`).
    pub prefix: bool,
}

/// How rows are grouped.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupBy {
    /// A frontmatter key, or `folder`, or `tag` (a note appears under each of
    /// its tags).
    pub key: String,
    /// Group order. A value joins the first entry it starts with (ignoring
    /// ASCII case), so `queued` collects `queued 139` and `queued 79`; the
    /// group is labelled with the entry. Values matching no entry form their
    /// own groups after these, sorted.
    pub order: Vec<String>,
    /// Render group labels as wikilinks (`[[GPU passes]]`).
    pub link: bool,
    /// Heading level for group headings in `table` and `list` formats (2 to
    /// 6, default 3).
    pub heading_level: Option<usize>,
    /// How group labels are shown: `raw` (the value as written, the default)
    /// or `title_case` (each word capitalised).
    pub label: Option<LabelStyle>,
}

/// How group labels are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelStyle {
    /// The value as written.
    #[default]
    Raw,
    /// Each word capitalised.
    TitleCase,
}

/// One output column.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// Just the key.
    Key(String),
    /// A key with options.
    Full {
        /// Frontmatter key, or one of `link` (the note link), `title` (the
        /// frontmatter `title`, else the file name), `file` (the file name),
        /// `path`, `vault`, `folder`.
        key: String,
        /// Column header; defaults to the key.
        header: Option<String>,
        /// Render the value as a wikilink by name.
        link: bool,
    },
}

impl Column {
    fn key(&self) -> &str {
        match self {
            Column::Key(k) | Column::Full { key: k, .. } => k,
        }
    }
    fn header(&self) -> String {
        match self {
            Column::Full { header: Some(h), .. } => h.clone(),
            _ => match self.key() {
                "link" => "Note".to_string(),
                k => k.to_string(),
            },
        }
    }
    fn link(&self) -> bool {
        matches!(self, Column::Full { link: true, .. })
    }
}

/// What an index row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowKind {
    /// Every note in the sources.
    #[default]
    Notes,
    /// Discussion threads.
    Discussions,
    /// Unresolved threads waiting for a decision.
    DecisionQueue,
}

/// Output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// A Markdown table per group.
    #[default]
    Table,
    /// A bulleted list per group.
    List,
    /// One table of group sizes with a total row (needs `group_by`).
    Counts,
    /// Just the number of matching notes, inline: the markers and the number
    /// share one line, so prose like "243 ideas." can carry it.
    CountInline,
}

/// An index specification. Stored verbatim (as compact JSON) in the start
/// marker.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// One or more sources; they may be in other vaults.
    pub sources: Vec<Source>,
    /// All filters must match.
    pub filter: Vec<Filter>,
    /// Optional grouping.
    pub group_by: Option<GroupBy>,
    /// Sort key: a frontmatter key, or `title` (the default) or `path`.
    /// Digit runs compare by value, so `PI-9` sorts before `PI-10`.
    pub sort: Option<String>,
    /// Columns; defaults to `["link"]`.
    pub columns: Vec<Column>,
    /// Output format.
    pub format: Format,
    /// Frontmatter key whose value is the link text; defaults to the file stem.
    pub link_text: Option<String>,
    /// Header of the count column in `counts` format (default `Notes`).
    pub count_header: Option<String>,
    /// What a row is: `notes` (default), `discussions` (thread notes, with
    /// computed `thread`, `id`, `contested` and `last_post` keys) or
    /// `decision_queue` (unresolved threads, oldest first by `unresolved_at`).
    /// Thread rows with no sources cover every vault.
    pub rows: Option<RowKind>,
}

fn is_default_format(f: &Format) -> bool {
    *f == Format::Table
}

/// Checks an index name: 1 to 48 characters of `[A-Za-z0-9_-]`.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if it does not fit.
pub fn check_name(name: &str) -> Result<()> {
    if !name.is_empty() && name.len() <= 48 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!("index name {name:?} must be 1 to 48 characters of A-Z a-z 0-9 _ -")))
    }
}

impl Spec {
    /// Checks the spec's shape.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for no sources, more than 16 sources, more
    /// than 16 columns, `counts` without `group_by`, or a heading level out of
    /// range.
    pub fn validate(&self) -> Result<()> {
        let threads = self.rows.is_some_and(|r| r != RowKind::Notes);
        if (self.sources.is_empty() && !threads) || self.sources.len() > 16 {
            return Err(Error::InvalidArgument("an index needs 1 to 16 sources (thread rows may give none, meaning every vault)".into()));
        }
        if self.columns.len() > 16 {
            return Err(Error::InvalidArgument("at most 16 columns".into()));
        }
        if self.format == Format::Counts && self.group_by.is_none() {
            return Err(Error::InvalidArgument("format counts needs group_by".into()));
        }
        if let Some(g) = &self.group_by {
            if g.heading_level.is_some_and(|l| !(2..=6).contains(&l)) {
                return Err(Error::InvalidArgument("heading_level must be 2 to 6".into()));
            }
        }
        for s in &self.sources {
            if let Some(g) = &s.glob {
                check_glob(g)?;
            }
        }
        Ok(())
    }

    /// The compact JSON stored in the marker. `>` is written as `>`, so
    /// the JSON can never close the HTML comment around it.
    pub fn to_marker_json(&self) -> String {
        crate::json::to_string_ok(self).unwrap_or_default().replace('>', "\\u003e")
    }
}

/// Checks a glob is small enough to match quickly.
///
/// # Errors
///
/// [`Error::InvalidArgument`] over 128 bytes or 8 `*`.
pub fn check_glob(g: &str) -> Result<()> {
    if g.len() > 128 || g.matches('*').count() > 8 {
        return Err(Error::InvalidArgument("glob too long (128 bytes, 8 '*' at most)".into()));
    }
    Ok(())
}

/// Every index start marker, left to right (was the `captures_iter` of
/// `<!-- vault-mcp:index ([A-Za-z0-9_-]{1,48}) start (\{.*?\}) -->`):
/// groups 0, the name and the spec (to the first `} -->` on its line).
pub(crate) fn start_markers(s: &str) -> Vec<[lex::Span; 3]> {
    const OPEN: &str = "<!-- vault-mcp:index ";
    const NAME: lex::Ascii = lex::Ascii::new(b"_-", &[(b'a', b'z'), (b'A', b'Z'), (b'0', b'9')]);
    let mut out = Vec::new();
    let mut q = 0;
    while let Some(p) = lex::find_str(s, q, OPEN) {
        q = p + 1;
        let n0 = p + OPEN.len();
        let n1 = lex::run_ascii(s, n0, NAME);
        let Some(b) = (1..=48).contains(&(n1 - n0)).then(|| lex::lit(s, n1, " start {")).flatten().map(|e| e - 1) else { continue };
        let line_end = lex::line_end(s, b);
        match lex::find_str(&s[..line_end], b + 1, "} -->") {
            Some(close) => {
                out.push([Some((p, close + 5)), Some((n0, n1)), Some((b, close + 1))]);
                q = close + 5;
            }
            // Any later marker on this line searches less of it.
            None => q = q.max(line_end),
        }
    }
    out
}

#[cfg(test)]
pub(crate) fn lex_sites() -> Vec<lex::props::Site> {
    use lex::props::{v, Mode, Site};
    vec![Site {
        name: "index start marker",
        pattern: r"<!-- vault-mcp:index ([A-Za-z0-9_-]{1,48}) start (\{.*?\}) -->".into(),
        tokens: &[
            "<!-- vault-mcp:index ",
            "x",
            "name-1_",
            " start ",
            "{",
            "}",
            " -->",
            "} -->",
            "{\"a\":1}",
            "\n",
            " ",
            "<",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ],
        examples: &["<!-- vault-mcp:index open start {\"a\":{}} -->\nx\n<!-- vault-mcp:index open end -->"],
        mode: Mode::All(|s| start_markers(s).into_iter().map(v).collect()),
    }]
}

/// An index region found in a note.
#[derive(Debug, Clone)]
pub struct Region {
    /// The index name.
    pub name: String,
    /// The spec parsed from the start marker.
    pub spec: Spec,
    /// Byte offset of the start marker line.
    pub start: usize,
    /// Byte offset just past the end marker's text (its line ending is not
    /// part of the region).
    pub end: usize,
}

/// Finds every index region in a note.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if a start marker has no end marker, its spec
/// does not parse, or a name appears twice.
pub fn regions(text: &str) -> Result<Vec<Region>> {
    let mut out: Vec<Region> = Vec::new();
    for c in start_markers(text) {
        let m = c[0].unwrap_or((0, 0));
        let g = |n: usize| c[n].map_or("", |(a, b)| &text[a..b]);
        let name = g(1).to_string();
        let spec: Spec =
            crate::json::from_str(g(2)).map_err(|e| Error::InvalidArgument(format!("index {name}: spec does not parse: {e}")))?;
        let end_marker = format!("<!-- vault-mcp:index {name} end -->");
        let rel = text[m.1..].find(&end_marker).ok_or_else(|| Error::InvalidArgument(format!("index {name}: end marker missing")))?;
        let end = m.1 + rel + end_marker.len();
        if out.iter().any(|r| r.name == name) {
            return Err(Error::InvalidArgument(format!("index {name} appears twice in the note")));
        }
        out.push(Region { name, spec, start: m.0, end });
    }
    Ok(out)
}

/// Natural order: digit runs compare by value, other text case-insensitively.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    natural_fold(a, b).then_with(|| a.cmp(b))
}

fn natural_fold(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut nx = String::new();
                while let Some(c) = x.peek().copied().filter(char::is_ascii_digit) {
                    nx.push(c);
                    x.next();
                }
                let mut ny = String::new();
                while let Some(d) = y.peek().copied().filter(char::is_ascii_digit) {
                    ny.push(d);
                    y.next();
                }
                let tx = nx.trim_start_matches('0');
                let ty = ny.trim_start_matches('0');
                let o = tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty)).then_with(|| nx.len().cmp(&ny.len()));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let o = c.to_lowercase().cmp(d.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(a) => a.iter().map(value_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// The text of a key for a note, including the pseudo-keys: `title` is the
/// frontmatter `title` when present and non-empty, else the file name;
/// `file` is always the file name.
pub fn field(n: &NoteRow, key: &str) -> String {
    match key {
        "title" => match n.frontmatter.get("title").map(value_text) {
            Some(t) if !t.trim().is_empty() => t,
            _ => n.title.clone(),
        },
        "file" => n.title.clone(),
        "path" => n.path.clone(),
        "vault" => n.vault.clone(),
        "folder" => links::folder(&n.path).to_string(),
        k => n.frontmatter.get(k).map(value_text).unwrap_or_default(),
    }
}

fn tags_of(n: &NoteRow) -> Vec<String> {
    match n.frontmatter.get("tags") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|s| s.to_lowercase()).collect(),
        Some(Value::String(s)) => s.split([',', ' ']).filter(|s| !s.is_empty()).map(|s| s.to_lowercase()).collect(),
        _ => Vec::new(),
    }
}

/// True if a note passes a filter.
pub fn passes(n: &NoteRow, f: &Filter) -> bool {
    let test = |s: &str| {
        if f.prefix {
            s.to_lowercase().starts_with(&f.value.to_lowercase())
        } else {
            s.eq_ignore_ascii_case(&f.value)
        }
    };
    match f.key.as_str() {
        "tag" => tags_of(n).iter().any(|t| test(t)),
        "folder" => test(links::folder(&n.path)),
        k => match n.frontmatter.get(k) {
            Some(Value::Array(a)) => a.iter().any(|v| test(&value_text(v))),
            Some(v) => test(&value_text(v)),
            None => false,
        },
    }
}

fn cell(s: &str) -> String {
    s.replace(['\r', '\n'], " ").replace('|', "\\|")
}

fn title_case(s: &str) -> String {
    s.split(' ').map(upper_first).collect::<Vec<_>>().join(" ")
}

fn upper_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Renders an index body.
///
/// # Arguments
///
/// * `spec` - the spec.
/// * `from_vault` - the vault the index note is in; rows in other vaults get
///   `obsidian://` links, rows in this one get wikilinks.
/// * `rows` - candidate notes, already limited to the sources.
/// * `folder_of` - maps a vault short name to its folder name.
/// * `unique` - true when a note's file name is unique in its vault, so a
///   wikilink can drop the folder (Obsidian's shortest-path form).
///
/// # Returns
///
/// The generated lines, LF-terminated, without markers (for `count_inline`,
/// just the number).
pub fn render(
    spec: &Spec,
    from_vault: &str,
    rows: &[NoteRow],
    folder_of: &dyn Fn(&str) -> String,
    unique: &dyn Fn(&NoteRow) -> bool,
) -> String {
    let mut notes: Vec<&NoteRow> = rows.iter().filter(|n| spec.filter.iter().all(|f| passes(n, f))).collect();
    let sort_key = spec.sort.clone().unwrap_or_else(|| "file".to_string());
    notes.sort_by(|a, b| {
        natural_cmp(&field(a, &sort_key), &field(b, &sort_key)).then_with(|| a.vault.cmp(&b.vault)).then_with(|| a.path.cmp(&b.path))
    });
    notes.dedup_by(|a, b| a.vault == b.vault && a.path == b.path);
    if spec.format == Format::CountInline {
        return notes.len().to_string();
    }

    let link_for = |n: &NoteRow, in_table: bool| -> String {
        let text = spec.link_text.as_deref().map(|k| field(n, k)).filter(|t| !t.is_empty()).unwrap_or_else(|| n.title.clone());
        if n.vault == from_vault {
            let target = if unique(n) { n.title.as_str() } else { n.path.strip_suffix(".md").unwrap_or(&n.path) };
            if target == text {
                format!("[[{target}]]")
            } else if in_table {
                format!("[[{target}\\|{}]]", text.replace('|', "-"))
            } else {
                format!("[[{target}|{}]]", text.replace('|', "-"))
            }
        } else {
            let l = links::uri_link(&folder_of(&n.vault), &n.path, None, &text);
            if in_table {
                l.replace('|', "\\|")
            } else {
                l
            }
        }
    };

    // Group assignment: (group label, sort rank) per note.
    let mut groups: Vec<(String, usize, Vec<&NoteRow>)> = Vec::new();
    match &spec.group_by {
        None => groups.push((String::new(), 0, notes.clone())),
        Some(g) => {
            let mut assigned: Vec<(String, usize, &NoteRow)> = Vec::new();
            for n in &notes {
                let values: Vec<String> = match g.key.as_str() {
                    "tag" => tags_of(n),
                    k => vec![field(n, k)],
                };
                let values = if values.is_empty() { vec![String::new()] } else { values };
                for v in values {
                    let hit = g.order.iter().position(|o| v.to_lowercase().starts_with(&o.to_lowercase()));
                    let (label, rank) = match hit {
                        Some(i) => (g.order[i].clone(), i),
                        None if v.is_empty() => ("(none)".to_string(), usize::MAX),
                        None => (v, g.order.len()),
                    };
                    assigned.push((label, rank, n));
                }
            }
            for (label, rank, n) in assigned {
                match groups.iter_mut().find(|(l, _, _)| *l == label) {
                    Some(gr) => gr.2.push(n),
                    None => groups.push((label, rank, vec![n])),
                }
            }
            groups.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| natural_cmp(&a.0, &b.0)));
        }
    }

    let label_text = |label: &str| -> String {
        let style = spec.group_by.as_ref().and_then(|g| g.label).unwrap_or_default();
        let shown = if style == LabelStyle::TitleCase && !label.starts_with('(') { title_case(label) } else { label.to_string() };
        match &spec.group_by {
            Some(g) if g.link && !label.starts_with('(') => format!("[[{label}]]"),
            _ => shown,
        }
    };

    let mut out = String::new();
    if spec.format == Format::Counts {
        let g = spec.group_by.as_ref().map(|g| g.key.clone()).unwrap_or_default();
        let ch = spec.count_header.clone().unwrap_or_else(|| "Notes".to_string());
        out.push_str(&format!("| {} | {} |\n|---|---|\n", cell(&upper_first(&g)), cell(&ch)));
        for (label, _, ns) in &groups {
            out.push_str(&format!("| {} | {} |\n", cell(&label_text(label)), ns.len()));
        }
        out.push_str(&format!("| total | {} |\n", notes.len()));
        return out;
    }

    let columns: Vec<Column> = if spec.columns.is_empty() { vec![Column::Key("link".into())] } else { spec.columns.clone() };
    let level = spec.group_by.as_ref().and_then(|g| g.heading_level).unwrap_or(3);
    if notes.is_empty() {
        out.push_str("_No notes match._\n");
        return out;
    }
    for (gi, (label, _, ns)) in groups.iter().enumerate() {
        if spec.group_by.is_some() {
            if gi > 0 {
                out.push('\n');
            }
            out.push_str(&format!("{} {} ({})\n\n", "#".repeat(level), label_text(label), ns.len()));
        }
        let value = |n: &NoteRow, c: &Column, in_table: bool| -> String {
            if c.key() == "link" {
                return link_for(n, in_table);
            }
            let v = field(n, c.key());
            let v = if c.link() && !v.is_empty() { format!("[[{v}]]") } else { v };
            if in_table {
                cell(&v)
            } else {
                v.replace(['\r', '\n'], " ")
            }
        };
        match spec.format {
            Format::Table => {
                let headers: Vec<String> = columns.iter().map(|c| cell(&c.header())).collect();
                out.push_str(&format!("| {} |\n", headers.join(" | ")));
                out.push_str(&format!("|{}\n", "---|".repeat(columns.len())));
                for n in ns {
                    let cells: Vec<String> = columns.iter().map(|c| value(n, c, true)).collect();
                    out.push_str(&format!("| {} |\n", cells.join(" | ")));
                }
            }
            Format::List | Format::Counts | Format::CountInline => {
                for n in ns {
                    let link = link_for(n, false);
                    let rest: Vec<String> =
                        columns.iter().filter(|c| c.key() != "link").map(|c| format!("{}: {}", c.header(), value(n, c, false))).collect();
                    if rest.is_empty() {
                        out.push_str(&format!("- {link}\n"));
                    } else {
                        out.push_str(&format!("- {link} ({})\n", rest.join("; ")));
                    }
                }
            }
        }
    }
    out
}

/// Writes a region into a note.
///
/// # Arguments
///
/// * `text` - the note's current text (empty for a new note).
/// * `name` - the index name.
/// * `spec` - the spec to store in the marker.
/// * `body` - the rendered lines (LF), from [`render`].
///
/// # Returns
///
/// The new text. If the region exists, only the bytes from its start marker
/// to its end marker are replaced; otherwise the region is appended after a
/// blank line. Inserted lines use the note's own line ending.
///
/// # Errors
///
/// As [`regions`].
pub fn splice(text: &str, name: &str, spec: &Spec, body: &str) -> Result<String> {
    let nl = crate::markdown::eol(text);
    let block = if spec.format == Format::CountInline {
        format!("<!-- vault-mcp:index {name} start {} -->{}<!-- vault-mcp:index {name} end -->", spec.to_marker_json(), body.trim())
    } else {
        format!(
            "<!-- vault-mcp:index {name} start {} -->{nl}{}<!-- vault-mcp:index {name} end -->",
            spec.to_marker_json(),
            crate::markdown::to_eol(body, nl)
        )
    };
    let found = regions(text)?.into_iter().find(|r| r.name == name);
    let mut out = String::with_capacity(text.len() + block.len() + 4);
    match found {
        Some(r) => {
            out.push_str(&text[..r.start]);
            out.push_str(&block);
            out.push_str(&text[r.end..]);
        }
        None => {
            out.push_str(text);
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push_str(nl);
                }
                if !out.ends_with(&format!("{nl}{nl}")) {
                    out.push_str(nl);
                }
            }
            out.push_str(&block);
            out.push_str(nl);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- JSON
//
// A spec is written into the note's marker, so its JSON is stable: fields in
// declaration order, defaults left out. Reading refuses unknown fields.

fn obj_of<'a>(v: &'a Value, at: &str, known: &[&str]) -> std::result::Result<&'a Map, String> {
    let m = v.as_object().ok_or_else(|| format!("{at} must be an object"))?;
    if let Some(k) = m.keys().find(|k| !known.contains(&k.as_str())) {
        return Err(format!("unknown field `{k}` in {at}, expected one of {}", known.join(", ")));
    }
    Ok(m)
}

fn str_of(m: &Map, k: &str, at: &str) -> std::result::Result<Option<String>, String> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{at}.{k} must be a string")),
    }
}

fn req_str(m: &Map, k: &str, at: &str) -> std::result::Result<String, String> {
    str_of(m, k, at)?.ok_or_else(|| format!("missing field `{k}` in {at}"))
}

fn bool_of(m: &Map, k: &str, at: &str) -> std::result::Result<bool, String> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{at}.{k} must be true or false")),
    }
}

fn list_of<T: FromJson>(m: &Map, k: &str, at: &str) -> std::result::Result<Vec<T>, String> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a.iter().map(T::from_json).collect(),
        Some(_) => Err(format!("{at}.{k} must be a list")),
    }
}

impl ToJson for Source {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("vault".into(), self.vault.to_json());
        if !self.folder.is_empty() {
            m.insert("folder".into(), self.folder.to_json());
        }
        if let Some(g) = &self.glob {
            m.insert("glob".into(), g.to_json());
        }
        Value::Object(m)
    }
}

impl FromJson for Source {
    fn from_json(v: &Value) -> std::result::Result<Source, String> {
        let m = obj_of(v, "source", &["vault", "folder", "glob"])?;
        Ok(Source { vault: req_str(m, "vault", "source")?, folder: str_of(m, "folder", "source")?.unwrap_or_default(), glob: str_of(m, "glob", "source")? })
    }
}

impl ToJson for Filter {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("key".into(), self.key.to_json());
        m.insert("value".into(), self.value.to_json());
        if self.prefix {
            m.insert("prefix".into(), Value::Bool(true));
        }
        Value::Object(m)
    }
}

impl FromJson for Filter {
    fn from_json(v: &Value) -> std::result::Result<Filter, String> {
        let m = obj_of(v, "filter", &["key", "value", "prefix"])?;
        Ok(Filter { key: req_str(m, "key", "filter")?, value: req_str(m, "value", "filter")?, prefix: bool_of(m, "prefix", "filter")? })
    }
}

impl LabelStyle {
    fn name(self) -> &'static str {
        match self {
            LabelStyle::Raw => "raw",
            LabelStyle::TitleCase => "title_case",
        }
    }
}

impl ToJson for LabelStyle {
    fn to_json(&self) -> Value {
        Value::String(self.name().into())
    }
}

impl FromJson for LabelStyle {
    fn from_json(v: &Value) -> std::result::Result<LabelStyle, String> {
        match v.as_str() {
            Some("raw") => Ok(LabelStyle::Raw),
            Some("title_case") => Ok(LabelStyle::TitleCase),
            _ => Err(format!("unknown variant {v}, expected `raw` or `title_case`")),
        }
    }
}

impl ToJson for GroupBy {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("key".into(), self.key.to_json());
        if !self.order.is_empty() {
            m.insert("order".into(), self.order.to_json());
        }
        if self.link {
            m.insert("link".into(), Value::Bool(true));
        }
        if let Some(h) = self.heading_level {
            m.insert("heading_level".into(), h.to_json());
        }
        if let Some(l) = self.label {
            m.insert("label".into(), l.to_json());
        }
        Value::Object(m)
    }
}

impl FromJson for GroupBy {
    fn from_json(v: &Value) -> std::result::Result<GroupBy, String> {
        let at = "group_by";
        let m = obj_of(v, at, &["key", "order", "link", "heading_level", "label"])?;
        let heading_level = match m.get("heading_level") {
            None | Some(Value::Null) => None,
            Some(x) => Some(x.as_u64().ok_or_else(|| format!("{at}.heading_level must be a non-negative integer"))? as usize),
        };
        let label = match m.get("label") {
            None | Some(Value::Null) => None,
            Some(x) => Some(LabelStyle::from_json(x)?),
        };
        let order = match m.get("order") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| format!("{at}.order must hold strings"))).collect::<std::result::Result<_, _>>()?,
            Some(_) => return Err(format!("{at}.order must be a list")),
        };
        Ok(GroupBy { key: req_str(m, "key", at)?, order, link: bool_of(m, "link", at)?, heading_level, label })
    }
}

impl ToJson for Column {
    fn to_json(&self) -> Value {
        match self {
            Column::Key(k) => k.to_json(),
            Column::Full { key, header, link } => {
                let mut m = Map::new();
                m.insert("key".into(), key.to_json());
                if let Some(h) = header {
                    m.insert("header".into(), h.to_json());
                }
                if *link {
                    m.insert("link".into(), Value::Bool(true));
                }
                Value::Object(m)
            }
        }
    }
}

impl FromJson for Column {
    fn from_json(v: &Value) -> std::result::Result<Column, String> {
        if let Some(s) = v.as_str() {
            return Ok(Column::Key(s.to_string()));
        }
        let m = obj_of(v, "column", &["key", "header", "link"])
            .map_err(|e| format!("data did not match any variant of untagged enum Column: {e}"))?;
        Ok(Column::Full { key: req_str(m, "key", "column")?, header: str_of(m, "header", "column")?, link: bool_of(m, "link", "column")? })
    }
}

impl RowKind {
    fn name(self) -> &'static str {
        match self {
            RowKind::Notes => "notes",
            RowKind::Discussions => "discussions",
            RowKind::DecisionQueue => "decision_queue",
        }
    }
}

impl ToJson for RowKind {
    fn to_json(&self) -> Value {
        Value::String(self.name().into())
    }
}

impl FromJson for RowKind {
    fn from_json(v: &Value) -> std::result::Result<RowKind, String> {
        match v.as_str() {
            Some("notes") => Ok(RowKind::Notes),
            Some("discussions") => Ok(RowKind::Discussions),
            Some("decision_queue") => Ok(RowKind::DecisionQueue),
            _ => Err(format!("unknown variant {v}, expected `notes`, `discussions` or `decision_queue`")),
        }
    }
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Table => "table",
            Format::List => "list",
            Format::Counts => "counts",
            Format::CountInline => "count_inline",
        }
    }
}

impl ToJson for Format {
    fn to_json(&self) -> Value {
        Value::String(self.name().into())
    }
}

impl FromJson for Format {
    fn from_json(v: &Value) -> std::result::Result<Format, String> {
        match v.as_str() {
            Some("table") => Ok(Format::Table),
            Some("list") => Ok(Format::List),
            Some("counts") => Ok(Format::Counts),
            Some("count_inline") => Ok(Format::CountInline),
            _ => Err(format!("unknown variant {v}, expected `table`, `list`, `counts` or `count_inline`")),
        }
    }
}

impl ToJson for Spec {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("sources".into(), self.sources.to_json());
        if !self.filter.is_empty() {
            m.insert("filter".into(), self.filter.to_json());
        }
        if let Some(g) = &self.group_by {
            m.insert("group_by".into(), g.to_json());
        }
        if let Some(s) = &self.sort {
            m.insert("sort".into(), s.to_json());
        }
        if !self.columns.is_empty() {
            m.insert("columns".into(), self.columns.to_json());
        }
        if !is_default_format(&self.format) {
            m.insert("format".into(), self.format.to_json());
        }
        if let Some(s) = &self.link_text {
            m.insert("link_text".into(), s.to_json());
        }
        if let Some(s) = &self.count_header {
            m.insert("count_header".into(), s.to_json());
        }
        if let Some(r) = self.rows {
            m.insert("rows".into(), r.to_json());
        }
        Value::Object(m)
    }
}

impl FromJson for Spec {
    fn from_json(v: &Value) -> std::result::Result<Spec, String> {
        let at = "spec";
        let m = obj_of(v, at, &["sources", "filter", "group_by", "sort", "columns", "format", "link_text", "count_header", "rows"])?;
        let sources = match m.get("sources") {
            Some(Value::Array(a)) => a.iter().map(Source::from_json).collect::<std::result::Result<_, _>>()?,
            Some(_) => return Err(format!("{at}.sources must be a list")),
            None => return Err("missing field `sources`".into()),
        };
        Ok(Spec {
            sources,
            filter: list_of(m, "filter", at)?,
            group_by: match m.get("group_by") {
                None | Some(Value::Null) => None,
                Some(g) => Some(GroupBy::from_json(g)?),
            },
            sort: str_of(m, "sort", at)?,
            columns: list_of(m, "columns", at)?,
            format: match m.get("format") {
                None | Some(Value::Null) => Format::Table,
                Some(f) => Format::from_json(f)?,
            },
            link_text: str_of(m, "link_text", at)?,
            count_header: str_of(m, "count_header", at)?,
            rows: match m.get("rows") {
                None | Some(Value::Null) => None,
                Some(r) => Some(RowKind::from_json(r)?),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn row(vault: &str, path: &str, fm: Value) -> NoteRow {
        NoteRow {
            vault: vault.into(),
            path: path.into(),
            size: 0,
            sha256: String::new(),
            title: links::stem(path).into(),
            frontmatter: fm.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["PI-10", "PI-9", "PI-100", "pi-2"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["pi-2", "PI-9", "PI-10", "PI-100"]);
    }

    #[test]
    fn splice_keeps_outside_and_two_regions_are_independent() {
        let spec: Spec = crate::json::from_value(json!({"sources": [{"vault": "a"}]})).unwrap();
        let t = "# Hand\n\nkeep me\n";
        let one = splice(t, "x", &spec, "- 1\n").unwrap();
        let two = splice(&one, "y", &spec, "- 2\n").unwrap();
        let again = splice(&two, "x", &spec, "- 1b\n").unwrap();
        assert!(again.starts_with("# Hand\n\nkeep me\n\n"));
        assert!(again.contains("- 1b\n"));
        assert!(again.contains("- 2\n"));
        assert!(!again.contains("- 1\n"));
        assert_eq!(regions(&again).unwrap().len(), 2);
    }

    #[test]
    fn renders_groups_and_counts() {
        let rows = vec![
            row("p", "Ideas/PI-2 B.md", json!({"id": "PI-2", "status": "queued 139", "area": "GPU passes"})),
            row("p", "Ideas/PI-10 C.md", json!({"id": "PI-10", "status": "open", "area": "GPU passes"})),
            row("q", "X/PI-1 A.md", json!({"id": "PI-1", "status": "open", "area": "Tooling"})),
        ];
        let spec: Spec = crate::json::from_value(json!({
            "sources": [{"vault": "p"}],
            "group_by": {"key": "status", "order": ["open", "queued", "in flight", "done", "parked"]},
            "sort": "id",
            "columns": ["id", "link", {"key": "area", "link": true}]
        }))
        .unwrap();
        let out = render(&spec, "p", &rows, &|v| format!("Vault {v}"), &|_| false);
        assert!(out.starts_with("### open (2)\n\n| id | Note | area |\n|---|---|---|\n| PI-1 | [PI-1 A](obsidian://open?vault=Vault%20q&file=X%2FPI-1%20A) | [[Tooling]] |\n| PI-10 | [[Ideas/PI-10 C\\|PI-10 C]] | [[GPU passes]] |\n"), "{out}");
        assert!(out.contains("### queued (1)"));
        let counts: Spec = crate::json::from_value(json!({
            "sources": [{"vault": "p"}], "format": "counts", "count_header": "Ideas",
            "group_by": {"key": "status", "order": ["open", "queued"]}
        }))
        .unwrap();
        let out = render(&counts, "p", &rows, &|v| v.to_string(), &|_| false);
        assert_eq!(out, "| Status | Ideas |\n|---|---|\n| open | 2 |\n| queued | 1 |\n| total | 3 |\n");
        // Title case is opt-in.
        let tc: Spec = crate::json::from_value(json!({
            "sources": [{"vault": "p"}], "format": "counts",
            "group_by": {"key": "status", "order": ["open", "in flight"], "label": "title_case"}
        }))
        .unwrap();
        let rows2 = vec![row("p", "a.md", json!({"status": "in flight X"}))];
        assert!(render(&tc, "p", &rows2, &|v| v.to_string(), &|_| false).contains("| In Flight | 1 |"));
    }
}
