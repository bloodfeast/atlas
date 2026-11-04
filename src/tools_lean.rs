//! The core read verbs (R8): `section` returns one section's text exactly,
//! and `grep` searches the indexed sources with context, answering with a
//! handle and a digest when the result is large. A child module of `tools`,
//! so it shares the server's private state.
//!
//! Both replace a shell pair the model would otherwise guess with: `grep -n`
//! then `sed -n A,Bp` for a section, and a repository-wide `grep -C` for a
//! search over the notes and read-only sources.

use std::path::Path;

use crate::json;
use crate::json::{Map, Value};

use super::{arg_str, obj, opt_str, opt_uint, s, Answer, Fmt, Server, ToolDef};
use crate::error::{Error, Result};
use crate::store::{DefText, Refresh, SectionHit};

fn n(desc: &str) -> Value {
    json!({"type": "integer", "description": desc})
}

/// The tools this module serves, appended to the tool list.
pub(super) const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "section",
        description: "One section's exact text by heading or by the id it defines, with its file and line range. Replaces grep -n then sed -n.",
        read_only: true,
        schema: || {
            obj(
                json!({
                    "heading": s("The heading text (case-insensitive), or the end of its heading path (`Parent > Child`)."),
                    "id": s("An identifier: the section that defines it."),
                    "source": s("Only this vault or source."),
                    "path": s("Only this file (path below the root)."),
                }),
                &[],
            )
        },
    },
    ToolDef {
        name: "grep",
        description: "Regex search over the indexed vaults and sources, with context lines. A large result comes back as a digest plus a handle for atlas slice.",
        read_only: true,
        schema: || {
            obj(
                json!({
                    "pattern": s("A regular expression, matched per line."),
                    "source": s("Only this vault or source."),
                    "path": s("Only paths starting with this (a file or a folder below the root)."),
                    "context": n("Lines of context around each match (0 to 10, default 0)."),
                }),
                &["pattern"],
            )
        },
    },
];

/// The most sections one `section` answer returns in full.
const MAX_SECTIONS: usize = 3;
/// The most matches `grep` collects before it stops reading.
const MAX_MATCHES: usize = 20_000;

impl Server {
    /// Runs one of [`TOOLS`], or `None` when `name` is not one of them.
    pub(super) fn run_lean(&mut self, name: &str, a: &Map, f: Fmt) -> Option<Result<Answer>> {
        match name {
            "section" => Some(self.section(a, f)),
            "grep" => Some(self.grep_sources(a, f)),
            _ => None,
        }
    }

    /// Every source and vault name, for freshness.
    fn all_sources(&self) -> Result<Vec<String>> {
        Ok(self.store.list_sources()?.into_iter().map(|s| s.name).collect())
    }

    /// Re-indexes stale files of these sources (a few at once) before an answer.
    fn bring_up_to_date(&mut self, source: Option<&str>) -> Result<()> {
        let mut touched = match source {
            Some(s) => vec![s.to_string()],
            None => self.all_sources()?,
        };
        touched.sort();
        touched.dedup();
        self.store.freshness(&touched, Refresh::Auto)?;
        Ok(())
    }

    fn section(&mut self, a: &Map, f: Fmt) -> Result<Answer> {
        let heading = opt_str(a, "heading")?.map(str::trim).filter(|h| !h.is_empty());
        let id = opt_str(a, "id")?.map(str::trim).filter(|h| !h.is_empty());
        let source = opt_str(a, "source")?.filter(|s| !s.is_empty());
        let path = opt_str(a, "path")?.filter(|s| !s.is_empty());
        if heading.is_some() == id.is_some() {
            return Err(Error::InvalidArgument("give exactly one of heading or id".into()));
        }
        self.bring_up_to_date(source)?;
        let ids: Vec<i64> = if let Some(id) = id {
            let l = self.store.lookup(id, 1, DefText::None)?;
            let conn = self.store.conn();
            let mut out = Vec::new();
            for d in l.definitions.iter().filter(|d| source.is_none_or(|s| d.source == s) && path.is_none_or(|p| d.path == p)) {
                let sid: Option<i64> = conn
                    .query_row(
                        "SELECT s.id FROM sections s JOIN docs dd ON dd.id = s.doc_id WHERE dd.source = ?1 AND dd.path = ?2 AND s.line_start <= ?3 AND s.line_end >= ?3 ORDER BY s.line_start DESC LIMIT 1",
                        rusqlite::params![d.source, d.path, d.line],
                        |r| r.get(0),
                    )
                    .ok();
                out.extend(sid);
            }
            out
        } else {
            let h = heading.unwrap_or("");
            let conn = self.store.conn();
            let mut st = conn.prepare_cached(
                "SELECT s.id FROM sections s JOIN docs d ON d.id = s.doc_id
                 WHERE (s.label = ?1 COLLATE NOCASE OR s.heading_path = ?1 COLLATE NOCASE OR s.heading_path LIKE '%> ' || ?4 ESCAPE '\\')
                   AND (?2 IS NULL OR d.source = ?2) AND (?3 IS NULL OR d.path = ?3)
                 ORDER BY d.source, d.path, s.line_start LIMIT 50",
            )?;
            let rows = st.query_map(rusqlite::params![h, source, path, h.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")], |r| r.get::<_, i64>(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut ids = ids;
        ids.dedup();
        let total = ids.len();
        let hits: Vec<SectionHit> = self.store.sections_by_id(&ids[..total.min(MAX_SECTIONS)], None, None)?;
        let others: Vec<SectionHit> = if total > MAX_SECTIONS { self.store.sections_by_id(&ids[MAX_SECTIONS..total.min(50)], None, None)? } else { Vec::new() };
        if f.json {
            return Ok(Answer::Json(json!({"sections": hits, "more": others.iter().map(|h| format!("{}/{}:{}-{}", h.source, h.path, h.line_start, h.line_end)).collect::<Vec<_>>(), "total": total})));
        }
        if hits.is_empty() {
            let what = id.map(|i| format!("id {i}")).unwrap_or_else(|| format!("heading {:?}", heading.unwrap_or("")));
            return Ok(Answer::Text(format!("no section for {what}; search finds sections by their words\n")));
        }
        let mut out = String::new();
        for h in &hits {
            out.push_str(&format!("{}/{}:{}-{} {}\n", h.source, h.path, h.line_start, h.line_end, h.heading_path));
            out.push_str(&h.text);
            if !h.text.ends_with('\n') {
                out.push('\n');
            }
            if h.truncated {
                out.push_str(&format!("(cut at {} B; read the file's lines {}-{} for the rest)\n", h.text.len(), h.line_start, h.line_end));
            }
        }
        for o in &others {
            out.push_str(&format!("also {}/{}:{}-{} {}\n", o.source, o.path, o.line_start, o.line_end, o.heading_path));
        }
        if total > MAX_SECTIONS + others.len() {
            out.push_str(&format!("+{} more\n", total - MAX_SECTIONS - others.len()));
        }
        Ok(Answer::Text(out))
    }

    fn grep_sources(&mut self, a: &Map, f: Fmt) -> Result<Answer> {
        let pattern = arg_str(a, "pattern")?;
        if pattern.is_empty() || pattern.len() > 1024 {
            return Err(Error::InvalidArgument("pattern must be 1 to 1024 bytes".into()));
        }
        let re = crate::regex::Regex::new(pattern).map_err(|e| Error::InvalidArgument(format!("pattern: {e}")))?;
        let source = opt_str(a, "source")?.filter(|s| !s.is_empty());
        let prefix = opt_str(a, "path")?.unwrap_or("").replace('\\', "/");
        let ctx = opt_uint(a, "context", 0, 10)?;
        self.bring_up_to_date(source)?;
        let mut docs = self.store.docs()?;
        docs.retain(|(s, p)| source.is_none_or(|x| s == x) && p.starts_with(prefix.as_str()));
        docs.sort();
        let (out, matches, files) = grep_docs(&docs, &re, ctx, &mut |s, p| self.store.read_document(s, p).ok())?;
        let label = format!("grep {pattern:?}{}{}", source.map(|s| format!(" in {s}")).unwrap_or_default(), if prefix.is_empty() { String::new() } else { format!(" under {prefix}") });
        if f.json {
            return Ok(Answer::Json(json!({"matches": matches, "files": files, "text": out})));
        }
        if matches == 0 {
            return Ok(Answer::Text(format!("{label}: no matches in {} file(s)\n", docs.len())));
        }
        // Large: kept whole under a handle; the model gets the digest.
        let cfg = crate::handles::HandleCfg::default();
        if out.len() >= cfg.threshold_bytes {
            if let Some(db) = self.store.conn().path().filter(|p| !p.is_empty()).map(|p| Path::new(p).to_path_buf()) {
                let mut h = crate::handles::Handles::open(&db, cfg)?;
                let meta = crate::handles::Meta { kind: "grep".into(), label: label.clone(), cmd: label.clone(), ..crate::handles::Meta::default() };
                let row = h.put(out.as_bytes(), &meta)?;
                let shape = crate::handles::Handles::shape_of(&row, &out);
                return Ok(Answer::Text(crate::digest::digest(&row.id, &row.meta.label, shape, &out, None, cfg.digest_bytes)));
            }
        }
        Ok(Answer::Text(format!("{label}: {matches} match(es) in {files} file(s)\n{out}")))
    }
}

/// Greps documents: `source/path:N:line` for a match and
/// `source/path-N-line` for context, `--` between separate runs, as grep
/// prints them. Returns the text, the match count and the files with a
/// match.
///
/// # Errors
///
/// Never; an unreadable document is skipped.
/// The most of one line `grep` prints, in bytes.
const MAX_GREP_LINE: usize = 400;

/// A line cut to [`MAX_GREP_LINE`] bytes (on character boundaries) around
/// the match at byte `at` when there is one, with `...` where text was left
/// out and the line's full length named.
fn capped_line(line: &str, at: Option<usize>) -> std::borrow::Cow<'_, str> {
    if line.len() <= MAX_GREP_LINE {
        return std::borrow::Cow::Borrowed(line);
    }
    let floor = |mut i: usize| {
        while !line.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let from = floor(at.unwrap_or(0).saturating_sub(MAX_GREP_LINE / 4).min(line.len() - MAX_GREP_LINE));
    let to = floor((from + MAX_GREP_LINE).min(line.len()));
    let mut s = String::with_capacity(MAX_GREP_LINE + 48);
    if from > 0 {
        s.push_str("...");
    }
    s.push_str(&line[from..to]);
    if to < line.len() {
        s.push_str("...");
    }
    s.push_str(&format!(" [line of {} bytes cut]", line.len()));
    std::borrow::Cow::Owned(s)
}

fn grep_docs(docs: &[(String, String)], re: &crate::regex::Regex, ctx: usize, read: &mut dyn FnMut(&str, &str) -> Option<Vec<u8>>) -> Result<(String, usize, usize)> {
    let mut out = String::new();
    let (mut matches, mut files) = (0usize, 0usize);
    for (s, p) in docs {
        let Some(bytes) = read(s, p) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let hit: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, _)| i).collect();
        if hit.is_empty() {
            continue;
        }
        files += 1;
        let mut last: Option<usize> = None;
        for &i in &hit {
            let from = i.saturating_sub(ctx);
            let to = (i + ctx).min(lines.len() - 1);
            let start = match last {
                Some(l) if from <= l + 1 => l + 1,
                Some(_) => {
                    if ctx > 0 {
                        out.push_str("--\n");
                    }
                    from
                }
                None => from,
            };
            for (j, line) in lines.iter().enumerate().take(to + 1).skip(start) {
                let sep = if hit.binary_search(&j).is_ok() { ':' } else { '-' };
                let shown = capped_line(line, if sep == ':' { re.find(line).map(|m| m.start()) } else { None });
                out.push_str(&format!("{s}/{p}{sep}{}{sep}{shown}\n", j + 1));
            }
            last = Some(to.max(last.unwrap_or(0)));
            matches += 1;
            if matches >= MAX_MATCHES {
                out.push_str(&format!("(stopped at {MAX_MATCHES} matches)\n"));
                return Ok((out, matches, files));
            }
        }
    }
    Ok((out, matches, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_prints_matches_context_and_separators_like_grep() {
        let docs = vec![("notes".to_string(), "a.md".to_string()), ("notes".to_string(), "b.md".to_string())];
        let re = crate::regex::Regex::new("TK-1").unwrap();
        let mut read = |_: &str, p: &str| -> Option<Vec<u8>> {
            Some(if p == "a.md" { b"one\nTK-1 here\nthree\nfour\nfive\nTK-1 again\n".to_vec() } else { b"nothing\n".to_vec() })
        };
        let (out, m, f) = grep_docs(&docs, &re, 1, &mut read).unwrap();
        assert_eq!((m, f), (2, 1));
        assert_eq!(out, "notes/a.md-1-one\nnotes/a.md:2:TK-1 here\nnotes/a.md-3-three\n--\nnotes/a.md-5-five\nnotes/a.md:6:TK-1 again\n");
        let (out, _, _) = grep_docs(&docs, &re, 0, &mut read).unwrap();
        assert_eq!(out, "notes/a.md:2:TK-1 here\nnotes/a.md:6:TK-1 again\n");
    }
}
