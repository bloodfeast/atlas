//! Compact plain-text answers: snippets centred on a match, and tables of
//! tab-separated rows under one header row.
//!
//! The format, which every list-shaped answer follows:
//!
//! * A table is a header row of field names, then one row per record, the
//!   fields separated by a tab. Field names appear once, not per record.
//! * Inside a value a tab is written `\t`, a newline `\n` and a carriage
//!   return `\r`, so one record is always one line. Nothing else is escaped.
//! * A snippet is a window of about [`SNIPPET_CHARS`] characters centred on
//!   the matched text, cut on character boundaries, with `…` where it was
//!   cut.
//! * A truncated list ends with a `+N more (limit=L)` line.

/// The width of a snippet, in characters.
pub const SNIPPET_CHARS: usize = 120;

/// The ellipsis marking a cut.
pub const ELLIPSIS: &str = "…";

/// Appends `s` to `out` with tab, newline and carriage return escaped.
///
/// # Arguments
///
/// * `out` - the answer being written.
/// * `s` - one field value.
pub fn push_field(out: &mut String, s: &str) {
    if !s.bytes().any(|b| matches!(b, b'\t' | b'\n' | b'\r')) {
        out.push_str(s);
        return;
    }
    for c in s.chars() {
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

/// Byte offset of the first case-insensitive (ASCII folding) occurrence of
/// `needle` in `hay`.
///
/// # Arguments
///
/// * `hay` - the text searched.
/// * `needle` - the text sought; empty finds nothing.
pub fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    let first = n[0].to_ascii_lowercase();
    let last = h.len() - n.len();
    let mut i = 0;
    while i <= last {
        if h[i].to_ascii_lowercase() == first && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// A window of about `chars` characters of `text` centred on the byte
/// range `at..at + len`, whitespace runs folded to one space, with an
/// ellipsis on each cut side.
///
/// # Arguments
///
/// * `text` - the source text (one line or a whole section).
/// * `at` - byte offset of the match (clamped; snapped to a character
///   boundary).
/// * `len` - byte length of the match.
/// * `chars` - the window width in characters.
pub fn window(text: &str, at: usize, len: usize, chars: usize) -> String {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let mut end = (at + len).min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    let match_chars = text[at..end].chars().count();
    let budget = chars.saturating_sub(match_chars);
    // Count at most `budget` characters each way; a side that runs short
    // gives its unused share to the other.
    let left_avail = text[..at].chars().rev().take(budget).count();
    let right_avail = text[end..].chars().take(budget).count();
    let right = right_avail.min(budget - left_avail.min(budget / 2));
    let left = left_avail.min(budget - right);
    let start = if left == 0 { at } else { text[..at].char_indices().rev().nth(left - 1).map(|(i, _)| i).unwrap_or(0) };
    let stop = text[end..].char_indices().nth(right).map(|(i, _)| end + i).unwrap_or(text.len());
    let mut out = String::with_capacity(stop - start + 8);
    if start > 0 {
        out.push_str(ELLIPSIS);
    }
    let mut space = false;
    for c in text[start..stop].chars() {
        if c.is_whitespace() {
            if !space && !out.is_empty() && !out.ends_with(ELLIPSIS) {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    if stop < text.len() {
        out.push_str(ELLIPSIS);
    }
    out
}

/// A snippet of `text` centred on the first case-insensitive occurrence of
/// any of `needles` (the earliest in the text wins), or the head of the
/// text when none occurs.
///
/// # Arguments
///
/// * `text` - the source text.
/// * `needles` - the terms to centre on.
pub fn snippet(text: &str, needles: &[&str]) -> String {
    let hit = needles.iter().filter_map(|n| find_ci(text, n).map(|i| (i, n.len()))).min();
    match hit {
        Some((at, len)) => window(text, at, len, SNIPPET_CHARS),
        None => window(text, 0, 0, SNIPPET_CHARS),
    }
}

/// A table being written: a header row, then rows of fields.
pub struct Table<'a> {
    out: &'a mut String,
    width: usize,
    col: usize,
}

impl<'a> Table<'a> {
    /// Writes the header row and starts a table.
    ///
    /// # Arguments
    ///
    /// * `out` - the answer being written.
    /// * `header` - the field names.
    pub fn new(out: &'a mut String, header: &[&str]) -> Table<'a> {
        out.push_str(&header.join("\t"));
        out.push('\n');
        Table { out, width: header.len(), col: 0 }
    }

    /// Appends one field to the current row, escaped.
    pub fn field(&mut self, s: &str) -> &mut Self {
        if self.col > 0 {
            self.out.push('\t');
        }
        push_field(self.out, s);
        self.col += 1;
        if self.col == self.width {
            self.out.push('\n');
            self.col = 0;
        }
        self
    }

    /// Appends one numeric field.
    pub fn num(&mut self, n: impl std::fmt::Display) -> &mut Self {
        let s = n.to_string();
        self.field(&s)
    }
}

/// Appends the `+N more (limit=L)` line when `shown < total`.
///
/// # Arguments
///
/// * `out` - the answer being written.
/// * `shown` - records shown.
/// * `total` - records that matched.
/// * `limit` - the limit in force.
pub fn more(out: &mut String, shown: usize, total: usize, limit: usize) {
    if total > shown {
        out.push_str(&format!("+{} more (limit={limit})\n", total - shown));
    }
}

/// A JSON answer as plain text, for the tools without a text form of their
/// own: an object is `key: value` lines (nulls left out, a nested object
/// indented under its key), an array of flat objects is a table under its
/// key, any other array one `- item` block per element. Every element is
/// written: nothing is cut or counted away.
///
/// # Arguments
///
/// * `v` - the answer.
pub fn value(v: &crate::json::Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, "");
    out
}

fn scalar(v: &crate::json::Value) -> Option<String> {
    use crate::json::Value;
    match v {
        Value::Null => Some(String::new()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Whether every element is an object of scalars (a table's rows).
fn flat_rows(a: &[crate::json::Value]) -> bool {
    a.iter().all(|e| e.as_object().is_some_and(|m| m.values().all(|x| scalar(x).is_some())))
}

fn write_value(out: &mut String, v: &crate::json::Value, ind: &str) {
    use crate::json::Value;
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter() {
                write_entry(out, k, x, ind);
            }
        }
        Value::Array(a) => write_array(out, a, ind),
        x => {
            out.push_str(ind);
            push_text(out, &scalar(x).unwrap_or_default(), ind);
            out.push('\n');
        }
    }
}

/// A string value: kept whole, its later lines indented like its first.
fn push_text(out: &mut String, s: &str, ind: &str) {
    for (i, l) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(ind);
        }
        out.push_str(l.trim_end_matches('\r'));
    }
}

fn write_entry(out: &mut String, k: &str, x: &crate::json::Value, ind: &str) {
    use crate::json::Value;
    match x {
        Value::Null => {}
        Value::Array(a) if a.is_empty() => out.push_str(&format!("{ind}{k}: none\n")),
        Value::Array(a) => {
            out.push_str(&format!("{ind}{k}: {}\n", a.len()));
            write_array(out, a, ind);
        }
        Value::Object(m) if m.is_empty() => {}
        Value::Object(_) => {
            out.push_str(&format!("{ind}{k}:\n"));
            write_value(out, x, &format!("{ind}  "));
        }
        s => {
            out.push_str(&format!("{ind}{k}: "));
            push_text(out, &scalar(s).unwrap_or_default(), &format!("{ind}  "));
            out.push('\n');
        }
    }
}

fn write_array(out: &mut String, a: &[crate::json::Value], ind: &str) {
    if !a.is_empty() && flat_rows(a) {
        let mut cols: Vec<&str> = Vec::new();
        for e in a {
            for (k, _) in e.as_object().into_iter().flat_map(|m| m.iter()) {
                if !cols.contains(&k.as_str()) {
                    cols.push(k);
                }
            }
        }
        out.push_str(ind);
        let mut t = Table::new(out, &cols);
        for e in a {
            for c in &cols {
                t.field(&e.get(c).and_then(scalar).unwrap_or_default());
            }
        }
        // Rows are not indented: a tab row is one record at any depth.
        return;
    }
    let deeper = format!("{ind}  ");
    for e in a {
        match scalar(e) {
            Some(s) => {
                out.push_str(&format!("{ind}- "));
                push_text(out, &s, &deeper);
                out.push('\n');
            }
            None => {
                out.push_str(&format!("{ind}-\n"));
                write_value(out, e, &deeper);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_become_lines_and_tables_with_every_element() {
        let v = crate::json::parse(
            r#"{"queue":[{"thread":"a:D-1","age":3},{"thread":"b:D-2","age":null}],"n":2,"none":[],"skip":null,"body":"one\ntwo","nested":{"k":"v"},"posts":[{"p":1,"tags":["x","y"]}]}"#,
        )
        .unwrap();
        let t = value(&v);
        assert!(t.contains("queue: 2\nthread\tage\na:D-1\t3\nb:D-2\t\n"), "{t}");
        assert!(t.contains("n: 2\n") && t.contains("none: none\n") && !t.contains("skip"), "{t}");
        assert!(t.contains("body: one\n  two\n") && t.contains("nested:\n  k: v\n"), "{t}");
        assert!(t.contains("posts: 1\n-\n  p: 1\n  tags: 2\n  - x\n  - y\n"), "{t}");
    }

    #[test]
    fn fields_escape_tabs_and_newlines() {
        let mut s = String::new();
        push_field(&mut s, "a\tb\nc\\d");
        assert_eq!(s, "a\\tb\\nc\\d");
    }

    #[test]
    fn window_centres_on_the_match() {
        let text = format!("{}TARGET-ID{}", "x".repeat(500), "y".repeat(500));
        let w = window(&text, 500, 9, 41);
        assert_eq!(w, format!("…{}TARGET-ID{}…", "x".repeat(16), "y".repeat(16)));
        // Near the start, the unused left budget goes right.
        let w = window("AB-12 tail of the line here", 0, 5, 12);
        assert_eq!(w, "AB-12 tail o…");
        // Short text is returned whole, whitespace folded.
        assert_eq!(window("a  b\n c", 2, 1, 120), "a b c");
    }

    #[test]
    fn window_cuts_on_char_boundaries() {
        let text = "é".repeat(300) + "ID" + &"ü".repeat(300);
        let w = window(&text, 600, 2, 20);
        assert!(w.starts_with('…') && w.ends_with('…') && w.contains("ID"));
        assert_eq!(w.chars().count(), 22);
    }

    #[test]
    fn snippet_finds_case_insensitively() {
        assert_eq!(snippet("see d-002 here", &["D-002"]), "see d-002 here");
        assert_eq!(find_ci("abcABC", "Bc"), Some(1));
        assert_eq!(find_ci("ab", ""), None);
    }

    #[test]
    fn tables_put_names_once() {
        let mut s = String::new();
        {
            let mut t = Table::new(&mut s, &["id", "n"]);
            t.field("A\tB").num(3);
            t.field("C").num(4);
        }
        more(&mut s, 2, 5, 2);
        assert_eq!(s, "id\tn\nA\\tB\t3\nC\t4\n+3 more (limit=2)\n");
    }
}
