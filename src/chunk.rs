//! Cutting a document into sections, so a search hit or a definition comes
//! back as the whole ruling rather than one line.
//!
//! Sections are leaves and never overlap: every line of a document belongs
//! to exactly one section. A section starts at a heading (levels up to
//! [`ChunkRule::max_level`]) or, when [`ChunkRule::top_level_items`] is on,
//! at a list item that starts at column 0 (`- `, `* ` or `+ `), and runs to
//! the line before the next start. Text before the first start (the
//! frontmatter and any preamble) is section 0. Headings and items inside
//! fenced code or the frontmatter never start a section.

use std::sync::OnceLock;

use crate::regex::Regex;

use crate::markdown;

/// How a source's files are cut into sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRule {
    /// Also start a section at every list item at column 0 (for files that
    /// are long bullet lists under few headings).
    pub top_level_items: bool,
    /// Deepest heading level that starts a section (1 to 6, default 6);
    /// deeper headings stay inside their parent's section.
    pub max_level: usize,
}

impl Default for ChunkRule {
    fn default() -> Self {
        ChunkRule { top_level_items: false, max_level: 6 }
    }
}

/// One section of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 0 for the preamble, 1 to 6 for a heading, 7 for a top-level item.
    pub level: u8,
    /// The enclosing headings, outermost first, joined by ` > `; for a
    /// heading section it ends with the heading itself.
    pub heading_path: String,
    /// The heading text, or the item's first line (at most 160 bytes), or
    /// empty for the preamble.
    pub label: String,
    /// First line, 1-based.
    pub line_start: usize,
    /// Last line, 1-based, inclusive.
    pub line_end: usize,
    /// Byte offset of the first line.
    pub start: usize,
    /// Byte offset just after the last line.
    pub end: usize,
    /// The first `YYYY-MM-DD` in the label, else in the nearest enclosing
    /// heading that has one.
    pub date: Option<String>,
}

/// The first `YYYY-MM-DD` date in a string.
pub fn find_date(s: &str) -> Option<&str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\b(?:19|20)[0-9]{2}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12][0-9]|3[01])\b").expect("date regex"));
    re.find(s).map(|m| m.as_str())
}

/// Cuts `text` into sections.
///
/// # Arguments
///
/// * `text` - the document.
/// * `rule` - where sections start.
///
/// # Returns
///
/// The sections in document order, covering every line exactly once. An
/// empty document gives no sections.
pub fn chunks(text: &str, rule: &ChunkRule) -> Vec<Chunk> {
    let spans = markdown::line_spans(text);
    if spans.is_empty() {
        return Vec::new();
    }
    let code = markdown::code_lines(text, &spans);
    // (line index, level, label)
    let mut starts: Vec<(usize, u8, String)> = Vec::new();
    for (i, &(s, e, _)) in spans.iter().enumerate() {
        if code[i] {
            continue;
        }
        let line = &text[s..e];
        if let Some((level, h)) = heading(line) {
            if level <= rule.max_level {
                starts.push((i, level as u8, h.to_string()));
            }
            continue;
        }
        if rule.top_level_items && is_top_item(line) {
            starts.push((i, 7, cap(line.trim(), 160).to_string()));
        }
    }
    let mut out = Vec::with_capacity(starts.len() + 1);
    // The heading stack: (level, text, date).
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut push = |from: usize, to: usize, level: u8, label: String, stack: &Vec<(u8, String)>| {
        let path = stack.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" > ");
        let date = find_date(&label).map(str::to_string).or_else(|| stack.iter().rev().find_map(|(_, t)| find_date(t).map(str::to_string)));
        out.push(Chunk {
            level,
            heading_path: path,
            label,
            line_start: from + 1,
            line_end: to,
            start: spans[from].0,
            end: spans[to - 1].2,
            date,
        });
    };
    let first = starts.first().map(|s| s.0).unwrap_or(spans.len());
    if first > 0 {
        push(0, first, 0, String::new(), &stack);
    }
    for (k, (i, level, label)) in starts.iter().enumerate() {
        let next = starts.get(k + 1).map(|s| s.0).unwrap_or(spans.len());
        if *level <= 6 {
            while stack.last().is_some_and(|(l, _)| *l >= *level) {
                stack.pop();
            }
            stack.push((*level, label.clone()));
        }
        push(*i, next, *level, label.clone(), &stack);
    }
    out
}

/// An ATX heading's level and text, or `None`.
pub fn heading(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let level = t.bytes().take_while(|&c| c == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &t[level..];
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    let mut h = rest.trim();
    let stripped = h.trim_end_matches('#');
    if stripped.len() < h.len() && (stripped.is_empty() || stripped.ends_with(' ')) {
        h = stripped.trim_end();
    }
    Some((level, h))
}

fn is_top_item(line: &str) -> bool {
    let b = line.as_bytes();
    b.len() >= 2 && matches!(b[0], b'-' | b'*' | b'+') && (b[1] == b' ' || b[1] == b'\t')
}

/// `s` cut to at most `n` bytes on a character boundary.
pub fn cap(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\nname: x\n---\nintro\n# Top 2026-01-02\n\ntext\n## Sub\n- item one\n  more\n- item two\n```\n# not a heading\n- not an item\n```\n### Deep\nlast\n";

    #[test]
    fn heading_chunks_cover_every_line_once() {
        let c = chunks(DOC, &ChunkRule::default());
        let labels: Vec<_> = c.iter().map(|c| (c.level, c.heading_path.as_str(), c.line_start, c.line_end)).collect();
        assert_eq!(
            labels,
            vec![
                (0, "", 1, 4),
                (1, "Top 2026-01-02", 5, 7),
                (2, "Top 2026-01-02 > Sub", 8, 15),
                (3, "Top 2026-01-02 > Sub > Deep", 16, 17)
            ]
        );
        assert_eq!(c[2].date.as_deref(), Some("2026-01-02"));
        assert_eq!(c.last().unwrap().end, DOC.len());
        for w in c.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
    }

    #[test]
    fn top_level_items_split_but_not_in_code() {
        let c = chunks(DOC, &ChunkRule { top_level_items: true, max_level: 6 });
        let labels: Vec<_> = c.iter().map(|c| (c.level, c.label.as_str(), c.line_start, c.line_end)).collect();
        assert_eq!(
            labels,
            vec![
                (0, "", 1, 4),
                (1, "Top 2026-01-02", 5, 7),
                (2, "Sub", 8, 8),
                (7, "- item one", 9, 10),
                (7, "- item two", 11, 15),
                (3, "Deep", 16, 17)
            ]
        );
        assert_eq!(c[3].heading_path, "Top 2026-01-02 > Sub");
    }

    #[test]
    fn max_level_keeps_deep_headings_inside() {
        let c = chunks(DOC, &ChunkRule { top_level_items: false, max_level: 2 });
        assert_eq!(c.len(), 3);
        assert_eq!(c[2].line_end, 17);
    }
}
