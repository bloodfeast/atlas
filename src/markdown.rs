//! Just enough Markdown for the vaults: YAML frontmatter, ATX headings and
//! their sections, links and tags.
//!
//! Every edit here works on byte offsets of whole lines, so bytes outside the
//! edited span are untouched, and inserted lines use the file's own line
//! ending.

use std::sync::OnceLock;

use crate::regex::Regex;
use crate::json::{Map, Value};

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

fn re_wiki() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(!?)\[\[([^\[\]\n]+?)\]\]").expect("static regex"))
}

fn re_md() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"!?\[(?:[^\[\]\n]|\[\[[^\]\n]*\]\])*\]\(\s*(<[^>\n]*>|[^)\s]+)(?:\s+"[^"\n]*")?\s*\)"#).expect("static regex")
    })
}

fn re_autolink() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"<(obsidian://[^>\s]+)>").expect("static regex"))
}

fn re_tag() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?:^|[\s(,])#([\p{L}\p{N}_/-]+)").expect("static regex"))
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
        for c in re_wiki().captures_iter(&line) {
            let embed = &c[1] == "!";
            let inner = &c[2];
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
                raw: c[0].to_string(),
                target,
                heading,
                line: i + 1,
            });
        }
        for c in re_md().captures_iter(&line) {
            let mut dest = c[1].to_string();
            if dest.starts_with('<') && dest.ends_with('>') {
                dest = dest[1..dest.len() - 1].to_string();
            }
            if dest.starts_with("obsidian://") {
                out.push(RawLink { kind: LinkKind::Uri, raw: c[0].to_string(), target: dest, heading: None, line: i + 1 });
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
            out.push(RawLink { kind: LinkKind::Md, raw: c[0].to_string(), target, heading, line: i + 1 });
        }
        for c in re_autolink().captures_iter(&line) {
            out.push(RawLink { kind: LinkKind::Uri, raw: c[0].to_string(), target: c[1].to_string(), heading: None, line: i + 1 });
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
        for c in re_tag().captures_iter(&line) {
            push(&c[1]);
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
