//! Just enough Markdown for the vaults: YAML frontmatter, ATX headings and
//! their sections, links and tags.
//!
//! Every edit here works on byte offsets of whole lines, so bytes outside the
//! edited span are untouched, and inserted lines use the file's own line
//! ending.

use crate::json::{Map, Value};
use crate::lex;

use crate::error::{Error, Result};

/// The line ending a file uses: CRLF if its first line break is CRLF,
/// otherwise LF (also for files with no line break).
pub fn eol(text: &str) -> &'static str {
    match text.find('\n') {
        Some(i) if i > 0 && text.as_bytes()[i - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

/// Splits text into lines with their byte span `(start, end_without_eol,
/// end_with_eol)`.
fn lines(text: &str) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let nl = text[start..].find('\n').map(|i| start + i);
        let (content_end, next) = match nl {
            Some(i) => (if i > start && bytes[i - 1] == b'\r' { i - 1 } else { i }, i + 1),
            None => (bytes.len(), bytes.len()),
        };
        out.push((start, content_end, next));
        start = next;
    }
    out
}

fn bom_len(text: &str) -> usize {
    if text.starts_with('\u{feff}') {
        3
    } else {
        0
    }
}

/// Byte span of a frontmatter block.
#[derive(Debug, Clone, Copy)]
pub struct FrontmatterSpan {
    /// Start of the opening `---` line.
    pub open: usize,
    /// Start of the first line after the opening `---`.
    pub inner_start: usize,
    /// Start of the closing `---` line.
    pub close: usize,
    /// First byte after the closing line (its line ending included).
    pub end: usize,
}

/// Finds the frontmatter block: a first line of `---` and a later line of
/// `---` or `...`.
pub fn frontmatter_span(text: &str) -> Option<FrontmatterSpan> {
    let b = bom_len(text);
    let ls = lines(&text[b..]);
    let (s0, e0, n0) = *ls.first()?;
    if &text[b + s0..b + e0] != "---" {
        return None;
    }
    for &(s, e, n) in ls.iter().skip(1) {
        let l = text[b + s..b + e].trim_end();
        if l == "---" || l == "..." {
            return Some(FrontmatterSpan { open: b + s0, inner_start: b + n0, close: b + s, end: b + n });
        }
    }
    None
}

/// Byte offset where the body starts (after frontmatter, or after a BOM).
pub fn body_start(text: &str) -> usize {
    frontmatter_span(text).map(|f| f.end).unwrap_or_else(|| bom_len(text))
}

fn strip_comment(v: &str) -> &str {
    match v.find(" #") {
        Some(i) => &v[..i],
        None if v.starts_with('#') => "",
        None => v,
    }
}

fn unquote_double(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn split_flow(inner: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in inner.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                cur.push(c);
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            None if c == ',' => items.push(std::mem::take(&mut cur)),
            None => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        items.push(cur);
    }
    items.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn parse_scalar(raw: &str) -> Value {
    let t = raw.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        return Value::String(unquote_double(&t[1..t.len() - 1]));
    }
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return Value::String(t[1..t.len() - 1].replace("''", "'"));
    }
    let t = strip_comment(t).trim();
    if t.starts_with('[') && t.ends_with(']') {
        return Value::Array(split_flow(&t[1..t.len() - 1]).iter().map(|s| parse_scalar(s)).collect());
    }
    match t {
        "" | "~" | "null" => return Value::Null,
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    let digits = t.strip_prefix('-').unwrap_or(t);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && !(digits.len() > 1 && digits.starts_with('0')) {
        if let Ok(i) = t.parse::<i64>() {
            return Value::from(i);
        }
    }
    Value::String(t.to_string())
}

fn top_level_key(line: &str) -> Option<(&str, &str)> {
    let first = line.chars().next()?;
    if first.is_whitespace() || first == '-' || first == '#' {
        return None;
    }
    let colon = line.find(':')?;
    let after = &line[colon + 1..];
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    let key = line[..colon].trim();
    if key.is_empty() {
        return None;
    }
    Some((key, after))
}

/// Parses the frontmatter into a JSON object.
///
/// Top-level `key: value` pairs are read: quoted strings, flow lists
/// (`[a, b]`), block lists (`- item` lines), booleans, integers and plain
/// strings. Anything nested deeper is kept as text. A note without
/// frontmatter gives an empty object.
pub fn parse_frontmatter(text: &str) -> Map {
    let mut map = Map::new();
    let Some(span) = frontmatter_span(text) else { return map };
    let inner = &text[span.inner_start..span.close];
    let ls: Vec<&str> = inner.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut i = 0;
    while i < ls.len() {
        let Some((key, after)) = top_level_key(ls[i]) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        let mut cont = Vec::new();
        while j < ls.len() && top_level_key(ls[j]).is_none() && !ls[j].trim().is_empty() {
            cont.push(ls[j].trim());
            j += 1;
        }
        let value = if strip_comment(after.trim()).trim().is_empty() && !cont.is_empty() {
            if cont.iter().all(|c| c.starts_with("- ") || *c == "-") {
                Value::Array(cont.iter().map(|c| parse_scalar(c.trim_start_matches('-'))).collect())
            } else {
                Value::String(cont.join(" "))
            }
        } else {
            parse_scalar(after)
        };
        map.insert(key.to_string(), value);
        i = j;
    }
    map
}

fn bare_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-'))
        && s.as_bytes()[0].is_ascii_alphabetic()
        && !matches!(s, "true" | "false" | "null" | "yes" | "no" | "on" | "off")
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Renders a JSON value as a one-line YAML value.
///
/// Strings are always double-quoted; list items are bare when that is
/// unambiguous (`[perf, area/gp]`).
///
/// # Errors
///
/// [`Error::InvalidArgument`] for null, objects and nested lists.
pub fn render_yaml(v: &Value) -> Result<String> {
    Ok(match v {
        Value::String(s) => quote(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for it in items {
                parts.push(match it {
                    Value::String(s) if bare_ok(s) => s.clone(),
                    Value::String(s) => quote(s),
                    Value::Bool(_) | Value::Number(_) => it.to_string(),
                    _ => return Err(Error::InvalidArgument("list items must be strings, numbers or booleans".into())),
                });
            }
            format!("[{}]", parts.join(", "))
        }
        Value::Null => return Err(Error::InvalidArgument("null is not a frontmatter value; use \"\" instead".into())),
        Value::Object(_) => return Err(Error::InvalidArgument("nested objects are not supported in frontmatter".into())),
    })
}

/// Checks a frontmatter key: 1 to 64 characters of `[A-Za-z0-9_ .-]`,
/// starting with a letter, digit or `_`.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the key does not fit.
pub fn check_key(k: &str) -> Result<()> {
    let ok = !k.is_empty()
        && k.len() <= 64
        && (k.as_bytes()[0].is_ascii_alphanumeric() || k.as_bytes()[0] == b'_')
        && k.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b' '))
        && !k.ends_with(' ');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!("frontmatter key {k:?} must be 1 to 64 characters of A-Z a-z 0-9 _ - . space")))
    }
}

/// Sets named frontmatter keys in place.
///
/// Each named key's line (and any indented continuation lines under it) is
/// replaced by `key: value`; a missing key is added just before the closing
/// `---`; a note without frontmatter gets a new block at the top. Every byte
/// outside the replaced lines, the whole body included, is unchanged.
///
/// # Arguments
///
/// * `text` - the note.
/// * `updates` - key to value; see [`render_yaml`] for accepted values.
///
/// # Returns
///
/// The edited note.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an empty update, a bad key or an
/// unrenderable value.
pub fn set_frontmatter(text: &str, updates: &Map) -> Result<String> {
    if updates.is_empty() {
        return Err(Error::InvalidArgument("no keys to set".into()));
    }
    let mut rendered = Vec::with_capacity(updates.len());
    for (k, v) in updates {
        check_key(k)?;
        rendered.push((k.as_str(), render_yaml(v)?));
    }
    let nl = eol(text);
    let Some(span) = frontmatter_span(text) else {
        let b = bom_len(text);
        let mut out = String::with_capacity(text.len() + 64);
        out.push_str(&text[..b]);
        out.push_str("---");
        out.push_str(nl);
        for (k, v) in &rendered {
            out.push_str(&format!("{k}: {v}{nl}"));
        }
        out.push_str("---");
        out.push_str(nl);
        out.push_str(&text[b..]);
        return Ok(out);
    };
    // Lines of the frontmatter interior, absolute offsets.
    let inner_lines: Vec<(usize, usize, usize)> = lines(&text[span.inner_start..span.close])
        .into_iter()
        .map(|(s, e, n)| (s + span.inner_start, e + span.inner_start, n + span.inner_start))
        .collect();
    // (start, end) byte spans to replace, with the replacement.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut appended = String::new();
    for (k, v) in &rendered {
        let mut found = None;
        for (idx, &(s, e, _)) in inner_lines.iter().enumerate() {
            if let Some((key, _)) = top_level_key(&text[s..e]) {
                if key == *k {
                    found = Some(idx);
                    break;
                }
            }
        }
        match found {
            Some(idx) => {
                let (s, e, _) = inner_lines[idx];
                let mut end = e;
                let mut j = idx + 1;
                while j < inner_lines.len() {
                    let (cs, ce, _) = inner_lines[j];
                    let line = &text[cs..ce];
                    if line.trim().is_empty()
                        || top_level_key(line).is_some()
                        || !(line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-'))
                    {
                        break;
                    }
                    end = ce;
                    j += 1;
                }
                edits.push((s, end, format!("{k}: {v}")));
            }
            None => appended.push_str(&format!("{k}: {v}{nl}")),
        }
    }
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(text.len() + appended.len() + 64);
    let mut pos = 0;
    for (s, e, rep) in edits {
        out.push_str(&text[pos..s]);
        out.push_str(&rep);
        pos = e;
    }
    out.push_str(&text[pos..span.close]);
    if !appended.is_empty() {
        if !out.ends_with('\n') {
            out.push_str(nl);
        }
        out.push_str(&appended);
    }
    out.push_str(&text[span.close..]);
    Ok(out)
}

/// One ATX heading.
#[derive(Debug, Clone)]
pub struct Heading {
    /// 1 to 6.
    pub level: usize,
    /// The heading text, trimmed, with closing `#`s removed.
    pub text: String,
    /// Byte offset of the heading line.
    pub start: usize,
    /// Byte offset just after the heading line's line ending.
    pub line_end: usize,
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|&x| x == c).count();
    (n >= 3).then_some((c, n))
}

/// Marks which lines are inside fenced code blocks or the frontmatter.
fn code_mask(text: &str, ls: &[(usize, usize, usize)]) -> Vec<bool> {
    let fm_end = frontmatter_span(text).map(|f| f.end).unwrap_or(0);
    let mut mask = Vec::with_capacity(ls.len());
    let mut open: Option<(char, usize)> = None;
    for &(s, e, _) in ls {
        if s < fm_end {
            mask.push(true);
            continue;
        }
        let line = &text[s..e];
        match open {
            Some((c, n)) => {
                mask.push(true);
                if let Some((c2, n2)) = fence_marker(line) {
                    if c2 == c && n2 >= n && line.trim().chars().all(|x| x == c) {
                        open = None;
                    }
                }
            }
            None => {
                if let Some(m) = fence_marker(line) {
                    open = Some(m);
                    mask.push(true);
                } else {
                    mask.push(false);
                }
            }
        }
    }
    mask
}

/// Each line's byte span `(start, end_without_eol, end_with_eol)`.
pub fn line_spans(text: &str) -> Vec<(usize, usize, usize)> {
    lines(text)
}

/// For each line of [`line_spans`], true when it is inside the frontmatter
/// or a fenced code block (fence lines included).
pub fn code_lines(text: &str, spans: &[(usize, usize, usize)]) -> Vec<bool> {
    code_mask(text, spans)
}

/// Parses the frontmatter like [`parse_frontmatter`], except that a key
/// whose value is a block of indented `key: value` lines becomes an object
/// (one level deep), so `metadata:` followed by an indented `type: x` line
/// gives `{"metadata": {"type": "x"}}`.
pub fn parse_frontmatter_nested(text: &str) -> Map {
    let mut map = parse_frontmatter(text);
    let Some(span) = frontmatter_span(text) else { return map };
    let inner = &text[span.inner_start..span.close];
    let ls: Vec<&str> = inner.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut i = 0;
    while i < ls.len() {
        let Some((key, after)) = top_level_key(ls[i]) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        let mut obj = Map::new();
        let mut nested = after.trim().is_empty();
        while j < ls.len() && top_level_key(ls[j]).is_none() && !ls[j].trim().is_empty() {
            match top_level_key(ls[j].trim_start()) {
                Some((k, v)) if ls[j].starts_with([' ', '\t']) => {
                    obj.insert(k.to_string(), parse_scalar(v));
                }
                _ => nested = false,
            }
            j += 1;
        }
        if nested && !obj.is_empty() {
            map.insert(key.to_string(), Value::Object(obj));
        }
        i = j.max(i + 1);
    }
    map
}

/// Every ATX heading outside the frontmatter and fenced code, in order.
pub fn headings(text: &str) -> Vec<Heading> {
    let ls = lines(text);
    let mask = code_mask(text, &ls);
    let mut out = Vec::new();
    for (i, &(s, e, n)) in ls.iter().enumerate() {
        if mask[i] {
            continue;
        }
        let line = &text[s..e];
        let t = line.trim_start_matches(' ');
        if line.len() - t.len() > 3 {
            continue;
        }
        let level = t.chars().take_while(|&c| c == '#').count();
        if level == 0 || level > 6 {
            continue;
        }
        let rest = &t[level..];
        if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
            continue;
        }
        let mut h = rest.trim();
        let stripped = h.trim_end_matches('#');
        if stripped.len() < h.len() && (stripped.is_empty() || stripped.ends_with(' ')) {
            h = stripped.trim_end();
        }
        out.push(Heading { level, text: h.to_string(), start: s, line_end: n });
    }
    out
}

/// A heading's section: from just after the heading line to the next heading
/// of the same or a higher level, or the end of the note.
#[derive(Debug, Clone)]
pub struct Section {
    /// The heading.
    pub heading: Heading,
    /// First byte of the section body.
    pub body_start: usize,
    /// First byte after the body (the next heading's start, or the end).
    pub body_end: usize,
}

/// Finds the one section whose heading text is `name`.
///
/// # Arguments
///
/// * `text` - the note.
/// * `name` - the heading text, without `#`s. An exact match is tried first,
///   then a case-insensitive one.
/// * `level` - restrict to this heading level when given.
///
/// # Returns
///
/// `Ok(None)` if no heading matches.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if more than one heading matches.
pub fn find_section(text: &str, name: &str, level: Option<usize>) -> Result<Option<Section>> {
    let hs = headings(text);
    let pick = |exact: bool| -> Vec<usize> {
        hs.iter()
            .enumerate()
            .filter(|(_, h)| level.is_none_or(|l| h.level == l))
            .filter(|(_, h)| if exact { h.text == name } else { h.text.eq_ignore_ascii_case(name) })
            .map(|(i, _)| i)
            .collect()
    };
    let mut hits = pick(true);
    if hits.is_empty() {
        hits = pick(false);
    }
    match hits.len() {
        0 => Ok(None),
        1 => {
            let i = hits[0];
            let h = hs[i].clone();
            let end = hs[i + 1..].iter().find(|n| n.level <= h.level).map(|n| n.start).unwrap_or(text.len());
            Ok(Some(Section { body_start: h.line_end, body_end: end, heading: h }))
        }
        n => Err(Error::InvalidArgument(format!("{n} headings are named {name:?}; give `level` to pick one"))),
    }
}

/// Normalises line endings in caller text to the file's own.
pub fn to_eol(s: &str, nl: &str) -> String {
    let lf = s.replace("\r\n", "\n");
    if nl == "\n" {
        lf
    } else {
        lf.replace('\n', nl)
    }
}

/// Appends `line` at the end of a section's content, creating the section at
/// the end of the note (as a level-2 heading) if it is missing.
///
/// The line goes after the section's last non-blank line, so trailing blank
/// lines before the next heading stay in place.
///
/// # Arguments
///
/// * `text` - the note.
/// * `heading` - the section's heading text.
/// * `line` - the text to insert; may span several lines.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if several headings share the name.
pub fn append_to_section(text: &str, heading: &str, line: &str) -> Result<String> {
    let nl = eol(text);
    let line = to_eol(line.trim_end_matches(['\r', '\n']), nl);
    match find_section(text, heading, None)? {
        Some(sec) => {
            let body = &text[sec.body_start..sec.body_end];
            let last_nonblank =
                lines(body).into_iter().rfind(|&(s, e, _)| !body[s..e].trim().is_empty()).map(|(_, _, n)| sec.body_start + n);
            let at = last_nonblank.unwrap_or(sec.body_start);
            let mut ins = String::new();
            if at > 0 && !text[..at].ends_with('\n') {
                ins.push_str(nl);
            }
            if last_nonblank.is_none() {
                ins.push_str(nl);
            }
            ins.push_str(&line);
            ins.push_str(nl);
            if sec.body_end < text.len() && at == sec.body_end {
                ins.push_str(nl);
            }
            let mut out = String::with_capacity(text.len() + ins.len());
            out.push_str(&text[..at]);
            out.push_str(&ins);
            out.push_str(&text[at..]);
            Ok(out)
        }
        None => {
            let mut out = String::with_capacity(text.len() + line.len() + heading.len() + 16);
            out.push_str(text);
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push_str(nl);
                }
                if !out.ends_with(&format!("{nl}{nl}")) && out != nl {
                    out.push_str(nl);
                }
            }
            out.push_str(&format!("## {heading}{nl}{nl}{line}{nl}"));
            Ok(out)
        }
    }
}

/// Replaces one section's body.
///
/// The new body is laid out as one blank line, the content with its leading
/// and trailing blank lines trimmed, and one blank line before the next
/// heading (none at the end of the note). The heading line and everything
/// outside the section are unchanged.
///
/// # Arguments
///
/// * `text` - the note.
/// * `heading` - the section's heading text.
/// * `level` - optional heading level to disambiguate.
/// * `content` - the new body.
///
/// # Errors
///
/// [`Error::NotFound`] if no heading matches; [`Error::InvalidArgument`] if
/// several do.
pub fn replace_section(text: &str, heading: &str, level: Option<usize>, content: &str) -> Result<String> {
    let nl = eol(text);
    let sec = find_section(text, heading, level)?.ok_or_else(|| Error::NotFound(format!("heading {heading:?}")))?;
    let body = to_eol(content, nl);
    let trimmed = body.trim_matches(|c| c == '\r' || c == '\n');
    let mut rep = String::new();
    if !text[..sec.body_start].ends_with('\n') {
        rep.push_str(nl);
    }
    rep.push_str(nl);
    if !trimmed.is_empty() {
        rep.push_str(trimmed);
        rep.push_str(nl);
    }
    if sec.body_end < text.len() {
        rep.push_str(nl);
    }
    let mut out = String::with_capacity(text.len() + rep.len());
    out.push_str(&text[..sec.body_start]);
    out.push_str(&rep);
    out.push_str(&text[sec.body_end..]);
    Ok(out)
}

/// Replaces the preamble: the text between the frontmatter (or the start of
/// the note) and the first heading.
///
/// Layout: one blank line after the frontmatter (if any), the content with
/// its leading and trailing blank lines trimmed, and one blank line before
/// the first heading (if any). Nothing outside the preamble changes.
pub fn replace_preamble(text: &str, content: &str) -> String {
    let nl = eol(text);
    let fm = frontmatter_span(text);
    let start = body_start(text);
    let end = headings(text).first().map(|h| h.start).unwrap_or(text.len());
    let body = to_eol(content, nl);
    let trimmed = body.trim_matches(|c| c == '\r' || c == '\n');
    let mut rep = String::new();
    if fm.is_some() && !(trimmed.is_empty() && end == text.len()) {
        rep.push_str(nl);
    }
    if !trimmed.is_empty() {
        rep.push_str(trimmed);
        rep.push_str(nl);
        if end < text.len() {
            rep.push_str(nl);
        }
    }
    let mut out = String::with_capacity(text.len() + rep.len());
    out.push_str(&text[..start]);
    out.push_str(&rep);
    out.push_str(&text[end..]);
    out
}

/// How a link was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// `[[target]]`
    Wiki,
    /// `![[target]]`
    Embed,
    /// `[text](relative/path.md)`
    Md,
    /// `obsidian://...`, as a Markdown link or `<autolink>`.
    Uri,
}

impl LinkKind {
    /// The name stored in the `links` table.
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Wiki => "wiki",
            LinkKind::Embed => "embed",
            LinkKind::Md => "md",
            LinkKind::Uri => "uri",
        }
    }
}

/// A link as written in a note, before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLink {
    /// How it was written.
    pub kind: LinkKind,
    /// The exact source text of the link.
    pub raw: String,
    /// For wiki, embed and md links the target as written (decoded, without
    /// `#heading`); for a URI the whole URI.
    pub target: String,
    /// The `#heading` part, if any.
    pub heading: Option<String>,
    /// 1-based line number.
    pub line: usize,
}

/// The inner text of `[[...]]` at byte `i` (after the brackets): one or
/// more characters but `[`, `]` and a newline, then `]]`. Returns the end
/// of the inner text.
pub(crate) fn wiki_inner(s: &str, i: usize) -> Option<usize> {
    let e = lex::run1(s, i, |c| !matches!(c, '[' | ']' | '\n'))?;
    lex::lit(s, e, "]]").map(|_| e)
}

/// The leftmost wikilink from byte `from` (was `(!?)\[\[([^\[\]\n]+?)\]\]`):
/// groups 0, the `!` and the inner text.
pub(crate) fn find_wiki(s: &str, from: usize) -> Option<[lex::Span; 3]> {
    let mut q = from;
    while let Some(p) = lex::find_str(s, q, "[[") {
        if let Some(e) = wiki_inner(s, p + 2) {
            let bang = p > from && s.as_bytes()[p - 1] == b'!';
            let start = if bang { p - 1 } else { p };
            return Some([Some((start, e + 2)), Some((start, if bang { p } else { start })), Some((p + 2, e))]);
        }
        q = p + 1;
    }
    None
}

/// Every Markdown link, left to right (was the `captures_iter` of
/// `!?\[(?:[^\[\]\n]|\[\[[^\]\n]*\]\])*\]\(\s*(<[^>\n]*>|[^)\s]+)(?:\s+"[^"\n]*")?\s*\)`):
/// groups 0 and the destination.
pub(crate) fn md_links(s: &str) -> Vec<[lex::Span; 2]> {
    let b = s.as_bytes();
    let mut text = LinkText {
        memo: std::collections::HashMap::new(),
        walks: 0,
        path: Vec::new(),
        stop: lex::Next::new(lex::stop2(b']', b'\n')),
        bracket: lex::Next::new(lex::stop2(b'[', b']')),
        newline: lex::Next::new(|s: &str, x| lex::line_end(s, x)),
    };
    let mut rest = LinkRest {
        angle: lex::Next::new(lex::stop2(b'>', b'\n')),
        bare: lex::Next::new(|s: &str, x| lex::run(s, x, |c| c != ')' && !lex::space(c))),
        quote: lex::Next::new(lex::stop2(b'"', b'\n')),
        last: None,
    };
    let mut out = Vec::new();
    let (mut from, mut q) = (0, 0);
    while let Some(p) = lex::find_byte(s, q, b'[') {
        match text.end(s, p + 1).and_then(|j| rest.at(s, j)) {
            Some((d, end)) => {
                let start = if p > from && b[p - 1] == b'!' { p - 1 } else { p };
                out.push([Some((start, end)), Some(d)]);
                (from, q) = (end, end);
            }
            None => q = p + 1,
        }
    }
    out
}

/// Where a link text that starts at a byte ends (its `]`). Plain text is
/// skipped by vector search to the next bracket or newline; the walk only
/// stops at a walk's start and after each `[[...]]`, and those stops are
/// remembered: texts from different `[` meet at them, and a line of
/// brackets would otherwise be walked once per bracket.
struct LinkText<F, G, H> {
    /// Stop byte to the walk's answer, from the second walk of the line on.
    memo: std::collections::HashMap<usize, Option<usize>>,
    /// Walks so far.
    walks: usize,
    /// The current walk's stops.
    path: Vec<usize>,
    /// The first `]` or newline.
    stop: lex::Next<F>,
    /// The first `[` or `]`.
    bracket: lex::Next<G>,
    /// The first newline.
    newline: lex::Next<H>,
}

impl<F: Fn(&str, usize) -> usize, G: Fn(&str, usize) -> usize, H: Fn(&str, usize) -> usize> LinkText<F, G, H> {
    /// The text from byte `j0`: characters but brackets and newlines, or
    /// whole `[[...]]`s, up to the first `]`. Returns that `]`'s byte.
    fn end(&mut self, s: &str, j0: usize) -> Option<usize> {
        let b = s.as_bytes();
        // The first walk of a line needs no memory (most lines have one
        // link); later walks are remembered so they cannot add up.
        let remember = self.walks > 0;
        self.walks += 1;
        let mut j = j0;
        let res = loop {
            if remember {
                if let Some(&r) = self.memo.get(&j) {
                    break r;
                }
                self.path.push(j);
            }
            let k = self.bracket.find(s, j).min(self.newline.find(s, j));
            match b.get(k) {
                None | Some(b'\n') => break None,
                Some(b']') => break Some(k),
                _ => {
                    if b.get(k + 1) != Some(&b'[') {
                        break None;
                    }
                    let t = self.stop.find(s, k + 2);
                    if lex::lit(s, t, "]]").is_none() {
                        break None;
                    }
                    j = t + 2;
                }
            }
        };
        for p in self.path.drain(..) {
            self.memo.insert(p, res);
        }
        res
    }
}

type Last = Option<(usize, Option<((usize, usize), usize)>)>;

/// The rest of a Markdown link after its text, with its searches kept
/// (see [`lex::Next`]) and its last answer: texts from several `[` that
/// end at the same `]` ask for the same rest.
struct LinkRest<A, B, Q> {
    /// The first `>` or newline (an angle destination's end).
    angle: lex::Next<A>,
    /// The first `)` or space (a bare destination's end).
    bare: lex::Next<B>,
    /// The first `"` or newline (a title's end).
    quote: lex::Next<Q>,
    /// The last answer: the `]` asked from and what it gave.
    last: Last,
}

impl<A: Fn(&str, usize) -> usize, B: Fn(&str, usize) -> usize, Q: Fn(&str, usize) -> usize> LinkRest<A, B, Q> {
    /// After the text's `]` at byte `j`: `(\s*(<[^>\n]*>|[^)\s]+)(?:\s+"[^"\n]*")?\s*\)`,
    /// giving the destination and the end.
    fn at(&mut self, s: &str, j: usize) -> Option<((usize, usize), usize)> {
        if let Some((k, r)) = self.last {
            if k == j {
                return r;
            }
        }
        let r = self.compute(s, j);
        self.last = Some((j, r));
        r
    }

    fn compute(&mut self, s: &str, j: usize) -> Option<((usize, usize), usize)> {
        let b = s.as_bytes();
        let u = lex::spaces(s, lex::lit(s, j, "](")?);
        let mut dests: [Option<usize>; 2] = [None, None];
        if b.get(u) == Some(&b'<') {
            let k = self.angle.find(s, u + 1);
            if b.get(k) == Some(&b'>') {
                dests[0] = Some(k + 1);
            }
        }
        let k = self.bare.find(s, u);
        dests[1] = (k > u).then_some(k);
        for v in dests.into_iter().flatten() {
            // An optional `"title"`, then `)`.
            let titled = lex::spaces1(s, v).filter(|&t| b.get(t) == Some(&b'"')).and_then(|t| {
                let k = self.quote.find(s, t + 1);
                (b.get(k) == Some(&b'"')).then_some(k + 1)
            });
            for w in [titled, Some(v)].into_iter().flatten() {
                let e = lex::spaces(s, w);
                if b.get(e) == Some(&b')') {
                    return Some(((u, v), e + 1));
                }
            }
        }
        None
    }
}

/// The leftmost `<obsidian://...>` from byte `from` (was
/// `<(obsidian://[^>\s]+)>`): groups 0 and the URI.
pub(crate) fn find_autolink(s: &str, from: usize) -> Option<[lex::Span; 2]> {
    let mut q = from;
    while let Some(p) = lex::find_str(s, q, "<obsidian://") {
        let e = lex::run(s, p + 12, |c| c != '>' && !lex::space(c));
        if e > p + 12 && s.as_bytes().get(e) == Some(&b'>') {
            return Some([Some((p, e + 1)), Some((p + 1, e))]);
        }
        // Every opener up to `e` runs to the same `e` and fails too.
        q = e.max(p + 1);
    }
    None
}

/// A character of an inline tag: `[\p{L}\p{N}_/-]`.
fn tag_char(c: char) -> bool {
    const TAG: lex::Ascii = lex::Ascii::new(b"_/-", &[(b'a', b'z'), (b'A', b'Z'), (b'0', b'9')]);
    if c.is_ascii() {
        TAG.has(c as u8)
    } else {
        c.is_alphabetic() || c.is_numeric()
    }
}

/// The leftmost inline tag from byte `from` (was
/// `(?:^|[\s(,])#([\p{L}\p{N}_/-]+)`): groups 0 and the tag.
pub(crate) fn find_tag(s: &str, from: usize) -> Option<[lex::Span; 2]> {
    let mut q = from;
    while let Some(h) = lex::find_byte(s, q, b'#') {
        let e = lex::run(s, h + 1, tag_char);
        if e > h + 1 {
            if h == 0 {
                return Some([Some((0, e)), Some((1, e))]);
            }
            if let Some(c) = lex::before(s, h).filter(|&c| lex::space(c) || c == '(' || c == ',') {
                let p = h - c.len_utf8();
                if p >= from {
                    return Some([Some((p, e)), Some((h + 1, e))]);
                }
            }
        }
        q = h + 1;
    }
    None
}

#[cfg(test)]
pub(crate) fn lex_sites() -> Vec<lex::props::Site> {
    use lex::props::{v, Mode, Site};
    let toks: &'static [&'static str] =
        &["[[", "]]", "[", "]", "!", "(", ")", "<", ">", "\"", " ", "a", "|", "#", "](", "[t](u)", "[[x]]", "![[y]]", "<u v>", " \"t\"", "obsidian://", "<obsidian://a>", "\n"];
    let ex: &'static [&'static str] = &[
        "see [[Note]] and ![[img.png|300]] here",
        "[t](a/b.md) [x [[y]] z](<c d.md> \"title\" ) ![i](obsidian://open?x=1)",
        "<obsidian://open?vault=a&file=b> [a](b \"t\")",
    ];
    vec![
        Site {
            name: "markdown wikilink",
            pattern: r"(!?)\[\[([^\[\]\n]+?)\]\]".into(),
            tokens: toks,
            examples: ex,
            mode: Mode::All(|s| lex::all(s, find_wiki).into_iter().map(v).collect()),
        },
        Site {
            name: "markdown link",
            pattern: r#"!?\[(?:[^\[\]\n]|\[\[[^\]\n]*\]\])*\]\(\s*(<[^>\n]*>|[^)\s]+)(?:\s+"[^"\n]*")?\s*\)"#.into(),
            tokens: toks,
            examples: ex,
            mode: Mode::All(|s| md_links(s).into_iter().map(v).collect()),
        },
        Site {
            name: "markdown autolink",
            pattern: r"<(obsidian://[^>\s]+)>".into(),
            tokens: toks,
            examples: ex,
            mode: Mode::All(|s| lex::all(s, find_autolink).into_iter().map(v).collect()),
        },
        Site {
            name: "markdown tag",
            pattern: r"(?:^|[\s(,])#([\p{L}\p{N}_/-]+)".into(),
            tokens: &["#", "##", " #", "(#", ",#", "a", "/", "-", "_", "\u{663}", "\u{e9}", " "],
            examples: &["#tag and (#area/gp, #x-1_y)", "text #2026 #\u{e9}t\u{e9}"],
            mode: Mode::All(|s| lex::all(s, find_tag).into_iter().map(v).collect()),
        },
    ]
}

/// Blanks out inline code spans so links inside them are not seen.
fn blank_code_spans(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = line.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'`' {
            let n = b[i..].iter().take_while(|&&c| c == b'`').count();
            let open_end = i + n;
            let mut j = open_end;
            let mut closed = None;
            while j < b.len() {
                if b[j] == b'`' {
                    let m = b[j..].iter().take_while(|&&c| c == b'`').count();
                    if m == n {
                        closed = Some(j);
                        break;
                    }
                    j += m;
                } else {
                    j += 1;
                }
            }
            match closed {
                Some(c) => {
                    for x in out.iter_mut().take(c).skip(open_end) {
                        if x.is_ascii() {
                            *x = b' ';
                        }
                    }
                    i = c + n;
                }
                None => i = open_end,
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| line.to_string())
}

/// Splits `target#heading` and trims both parts.
fn split_heading(s: &str) -> (String, Option<String>) {
    match s.find('#') {
        Some(i) => {
            let h = s[i + 1..].trim();
            (s[..i].trim().to_string(), (!h.is_empty()).then(|| h.to_string()))
        }
        None => (s.trim().to_string(), None),
    }
}

/// Every link in a note outside frontmatter, fenced code and inline code.
///
/// Links to a heading of the same note (`[[#Heading]]`) and web links are
/// skipped; they do not point at another note.
pub fn extract_links(text: &str) -> Vec<RawLink> {
    let ls = lines(text);
    let mask = code_mask(text, &ls);
    let mut out = Vec::new();
    for (i, &(s, e, _)) in ls.iter().enumerate() {
        if mask[i] {
            continue;
        }
        let line = blank_code_spans(&text[s..e]);
        if !line.contains('[') && !line.contains("obsidian://") {
            continue;
        }
        let span = |g: lex::Span| g.map_or("", |(a, b)| &line[a..b]);
        for c in lex::all(&line, find_wiki) {
            let embed = span(c[1]) == "!";
            let inner = span(c[2]);
            let target_part = match inner.find('|') {
                Some(p) => inner[..p].trim_end_matches('\\'),
                None => inner,
            };
            let (target, heading) = split_heading(target_part);
            if target.is_empty() {
                continue;
            }
            out.push(RawLink {
                kind: if embed { LinkKind::Embed } else { LinkKind::Wiki },
                raw: span(c[0]).to_string(),
                target,
                heading,
                line: i + 1,
            });
        }
        for c in md_links(&line) {
            let raw = span(c[0]).to_string();
            let mut dest = span(c[1]).to_string();
            if dest.starts_with('<') && dest.ends_with('>') {
                dest = dest[1..dest.len() - 1].to_string();
            }
            if dest.starts_with("obsidian://") {
                out.push(RawLink { kind: LinkKind::Uri, raw, target: dest, heading: None, line: i + 1 });
                continue;
            }
            if dest.starts_with('#') || dest.contains("://") || dest.starts_with("mailto:") || dest.contains(':') {
                continue;
            }
            let decoded = crate::links::percent_decode(&dest);
            let (target, heading) = split_heading(&decoded);
            if target.is_empty() {
                continue;
            }
            out.push(RawLink { kind: LinkKind::Md, raw, target, heading, line: i + 1 });
        }
        for c in lex::all(&line, find_autolink) {
            out.push(RawLink { kind: LinkKind::Uri, raw: span(c[0]).to_string(), target: span(c[1]).to_string(), heading: None, line: i + 1 });
        }
    }
    out
}

/// Tags from the frontmatter `tags` key and inline `#tags` in the body,
/// lowercased, deduplicated and sorted. A tag needs at least one character
/// that is not a digit, as in Obsidian.
pub fn extract_tags(text: &str, fm: &Map) -> Vec<String> {
    let mut tags = Vec::new();
    let mut push = |t: &str| {
        let t = t.trim().trim_start_matches('#');
        if !t.is_empty() && !t.chars().all(|c| c.is_ascii_digit()) {
            tags.push(t.to_lowercase());
        }
    };
    match fm.get("tags") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).for_each(&mut push),
        Some(Value::String(s)) => s.split([',', ' ']).for_each(&mut push),
        _ => {}
    }
    let ls = lines(text);
    let mask = code_mask(text, &ls);
    for (i, &(s, e, _)) in ls.iter().enumerate() {
        if mask[i] {
            continue;
        }
        let line = blank_code_spans(&text[s..e]);
        if !line.contains('#') {
            continue;
        }
        for c in lex::all(&line, find_tag) {
            if let Some((a, b)) = c[1] {
                push(&line[a..b]);
            }
        }
    }
    tags.sort();
    tags.dedup();
    tags
}

/// The body text with the frontmatter removed.
pub fn body(text: &str) -> &str {
    &text[body_start(text)..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    const NOTE: &str = "---\nid: PI-001\ntitle: \"A: b\"\nstatus: \"open\"\nmeasured: false\ntags: [perf, area/gp]\nlist:\n  - a\n  - b\n---\n\n# PI-001 A\n\nbody [[x]] text\n\n## History\n\nAppend-only.\n\n- 2026-10-08: proposed\n";

    #[test]
    fn parses_frontmatter() {
        let fm = parse_frontmatter(NOTE);
        assert_eq!(fm["id"], json!("PI-001"));
        assert_eq!(fm["title"], json!("A: b"));
        assert_eq!(fm["measured"], json!(false));
        assert_eq!(fm["tags"], json!(["perf", "area/gp"]));
        assert_eq!(fm["list"], json!(["a", "b"]));
    }

    #[test]
    fn set_frontmatter_keeps_body() {
        let mut u = Map::new();
        u.insert("status".into(), json!("parked"));
        u.insert("list".into(), json!(["c"]));
        u.insert("new key".into(), json!(3));
        let out = set_frontmatter(NOTE, &u).unwrap();
        assert_eq!(body(&out), body(NOTE));
        let fm = parse_frontmatter(&out);
        assert_eq!(fm["status"], json!("parked"));
        assert_eq!(fm["list"], json!(["c"]));
        assert_eq!(fm["new key"], json!(3));
        assert_eq!(fm["id"], json!("PI-001"));
    }

    #[test]
    fn set_frontmatter_crlf_and_absent() {
        let crlf = NOTE.replace('\n', "\r\n");
        let mut u = Map::new();
        u.insert("status".into(), json!("done"));
        let out = set_frontmatter(&crlf, &u).unwrap();
        assert!(!out.replace("\r\n", "").contains('\n'));
        assert_eq!(body(&out), body(&crlf));
        let plain = "# Title\n\ntext\n";
        let out = set_frontmatter(plain, &u).unwrap();
        assert_eq!(body(&out), plain);
    }

    #[test]
    fn history_append() {
        let out = append_to_section(NOTE, "History", "- 2026-10-09: parked").unwrap();
        assert!(out.ends_with("- 2026-10-08: proposed\n- 2026-10-09: parked\n"));
        let out = append_to_section("# T\n\ntext", "History", "- x").unwrap();
        assert_eq!(out, "# T\n\ntext\n\n## History\n\n- x\n");
        let mid = "# T\n\n## Related\n## Next\n";
        let out = append_to_section(mid, "Related", "- [[a]]").unwrap();
        assert_eq!(out, "# T\n\n## Related\n\n- [[a]]\n\n## Next\n");
    }

    #[test]
    fn section_replace() {
        let t = "# T\n\n## A\n\nold\n\n## B\n\nkeep\n";
        let out = replace_section(t, "A", None, "new\nlines").unwrap();
        assert_eq!(out, "# T\n\n## A\n\nnew\nlines\n\n## B\n\nkeep\n");
        assert!(replace_section(t, "Z", None, "x").is_err());
    }

    #[test]
    fn links_and_tags() {
        let t = "---\ntags: [perf]\n---\n[[A\\|b]] ![[img.png]] [[Folder/N#Head|al]] [m](Other%20note.md#h) [u](obsidian://open?vault=V&file=N) <obsidian://vault/V/N>\n`[[code]]`\n```\n[[fenced]]\n```\n#tag1 and #123 and a#b [w](https://x.org)\n";
        let l = extract_links(t);
        let kinds: Vec<_> = l.iter().map(|x| (x.kind, x.target.as_str(), x.heading.as_deref())).collect();
        assert_eq!(
            kinds,
            vec![
                (LinkKind::Wiki, "A", None),
                (LinkKind::Embed, "img.png", None),
                (LinkKind::Wiki, "Folder/N", Some("Head")),
                (LinkKind::Md, "Other note.md", Some("h")),
                (LinkKind::Uri, "obsidian://open?vault=V&file=N", None),
                (LinkKind::Uri, "obsidian://vault/V/N", None),
            ]
        );
        let fm = parse_frontmatter(t);
        assert_eq!(extract_tags(t, &fm), vec!["perf", "tag1"]);
    }
}
