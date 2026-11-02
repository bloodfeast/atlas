//! The atlas tools: identifier lookup and listing, section search, the
//! source list and the misses log. A child module of `tools`, so it shares
//! the server's private state and helpers.
//!
//! None of these tools runs the per-call vault rescan; each checks
//! freshness itself (see [`crate::store::Store::freshness`]) for the sources
//! its answer drew on, and every answer carries that `freshness` block.

use std::time::Instant;

use crate::regex::RegexBuilder;
use crate::json::{Map, Value};
use crate::json;

use super::{arg_str, opt_bool, opt_str, opt_uint, Answer, Fmt, Server};
use crate::error::{Error, Result};
use crate::recall;
use crate::store::{DefText, Freshness, IdHit, Refresh};
use crate::text::{self, Table};

/// The most mentions `lookup` returns.
pub const MAX_MENTIONS: usize = 500;
/// Mention lines `lookup` returns by default (repeats in one section merge
/// into one line); the rest are counted on a `+N more (limit=3)` line.
/// Measured 2026-11-02: 9 of 14 lookups used none of their mentions.
pub const DEFAULT_MENTIONS: usize = 3;
/// Sections `search` ranks by default (snippets; `+more (limit=5)` past
/// them).
pub const DEFAULT_SECTIONS: usize = 5;
/// Identifiers `ids` lists by default.
pub const DEFAULT_IDS: usize = 50;
/// Misses `misses` lists by default.
pub const DEFAULT_MISSES: usize = 20;
/// The characters of defining text a plain-text `lookup` shows for its
/// strongest definition (all of it with `full`).
pub const DEF_TEXT_CHARS: usize = 600;
/// The most sections a section search returns.
pub const MAX_SECTIONS: usize = 50;

/// Tools that skip the per-call vault rescan and check freshness
/// themselves.
pub(super) const ATLAS_TOOLS: &[&str] = &["lookup", "symbol", "ids", "list_sources", "report_miss", "misses", "resolve_miss"];

fn refresh_arg(a: &Map) -> Result<Refresh> {
    Refresh::parse(opt_str(a, "refresh")?.unwrap_or("auto"))
}

fn short(s: &str, max: usize) -> Result<&str> {
    if s.len() > max || s.contains('\0') {
        return Err(Error::InvalidArgument(format!("text must be at most {max} bytes without NUL")));
    }
    Ok(s)
}

/// Builds an FTS5 query from plain text: each `"quoted phrase"` stays a
/// phrase, and every other whitespace-separated word becomes a phrase of
/// its letter and digit runs (so `ALPHA-BETA` is the phrase `alpha
/// beta`); the phrases are ANDed.
pub fn plain_query(q: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut rest = q;
    while !rest.is_empty() {
        if let Some(stripped) = rest.strip_prefix('"') {
            let end = stripped.find('"').unwrap_or(stripped.len());
            let w = recall::words(&stripped[..end]);
            if !w.is_empty() {
                parts.push(recall::phrase_query(&w));
            }
            rest = stripped.get(end + 1..).unwrap_or("");
            continue;
        }
        let end = rest.find([' ', '\t', '\n', '"']).unwrap_or(rest.len());
        let w = recall::words(&rest[..end]);
        if !w.is_empty() {
            parts.push(recall::phrase_query(&w));
        }
        rest = rest[end..].trim_start_matches([' ', '\t', '\n']);
    }
    parts.join(" ")
}

/// The freshness block of a JSON answer: only the stale sources (path,
/// state, whether re-indexed), or everything when verbose; `None` when there
/// is nothing to say.
fn fresh_json(fresh: &[Freshness], verbose: bool) -> Option<Value> {
    if verbose {
        return Some(json!(fresh));
    }
    let stale: Vec<Value> = fresh
        .iter()
        .filter(|f| f.stale_count > 0)
        .map(|f| match &f.reason {
            // Past the cap: a count and why, not one object per file.
            Some(why) => json!({"source": f.source, "stale_count": f.stale_count, "reason": why, "refreshed": f.refreshed}),
            None => json!({"source": f.source, "stale": f.stale, "refreshed": f.refreshed}),
        })
        .collect();
    (!stale.is_empty()).then_some(Value::Array(stale))
}

/// Plain-text freshness: one `stale` line per stale file, or one count line
/// with its reason past [`crate::store::STALE_LIST_CAP`]; nothing when fresh;
/// verbose adds one line per source checked.
fn fresh_text(out: &mut String, fresh: &[Freshness], verbose: bool) {
    for f in fresh {
        if verbose {
            out.push_str(&format!(
                "fresh {} {} files={} checked={} last_scan={} last_indexed={}\n",
                f.kind,
                f.source,
                f.files,
                f.checked,
                f.last_scan.as_deref().unwrap_or("-"),
                f.last_indexed.as_deref().unwrap_or("-")
            ));
        }
        let done = if f.refreshed { ", reindexed" } else { "" };
        if let Some(why) = &f.reason {
            out.push_str(&format!("stale {} {} files ({why}){done}\n", f.source, f.stale_count));
        }
        for s in &f.stale {
            out.push_str(&format!("stale {}:{} {}{done}\n", f.source, s.path, s.state));
        }
    }
}

/// About the bytes a hit takes as JSON, to size the answer's buffer once.
fn hit_bytes(h: &IdHit) -> usize {
    160 + h.source.len() + h.path.len() + h.id.len() + h.heading_path.len() + h.context.len() + h.text.as_ref().map_or(0, String::len)
}

fn loc(h: &IdHit) -> String {
    format!("{}:{}:{}", h.source, h.path, h.line)
}

/// The defining text, capped at [`DEF_TEXT_CHARS`] characters unless
/// `full`.
fn def_text(t: &str, full: bool) -> String {
    let t = t.trim();
    if full || t.chars().count() <= DEF_TEXT_CHARS {
        return t.to_string();
    }
    let end = t.char_indices().nth(DEF_TEXT_CHARS).map(|(i, _)| i).unwrap_or(t.len());
    format!("{}{}", &t[..end], text::ELLIPSIS)
}

impl Server {
    /// Runs the freshness check for an answer's sources, and reports whether
    /// the answer must be recomputed.
    fn fresh(&mut self, touched: &mut Vec<String>, refresh: Refresh) -> Result<(Vec<Freshness>, bool)> {
        touched.sort();
        touched.dedup();
        self.store.freshness(touched, refresh)
    }

    pub(super) fn lookup(&mut self, a: &Map, f: Fmt) -> Result<Answer> {
        let id = arg_str(a, "id")?.trim();
        if id.is_empty() || id.len() > 256 {
            return Err(Error::InvalidArgument("id must be 1 to 256 bytes".into()));
        }
        let limit = opt_uint(a, "limit", DEFAULT_MENTIONS, MAX_MENTIONS)?;
        let full = opt_bool(a, "full", false)?;
        let refresh = refresh_arg(a)?;
        // JSON carries every definition's text; plain text only the
        // strongest one's, unless full.
        let all_text = if f.json || full { DefText::All } else { DefText::First };
        let t0 = Instant::now();
        let mut l = self.store.lookup(id, limit, all_text)?;
        let query_us = t0.elapsed().as_micros();
        let t1 = Instant::now();
        // Every source that holds the id, not only those of the rows shown:
        // a stale file past the mention limit is checked too.
        let mut touched = self.store.lookup_sources(id)?;
        let (fresh, redo) = self.fresh(&mut touched, refresh)?;
        if redo {
            l = self.store.lookup(id, limit, all_text)?;
        }
        let fresh_us = t1.elapsed().as_micros();
        let found = !l.definitions.is_empty() || l.total_mentions > 0;
        // Not registered: say whether the text holds it anyway, so a gap in
        // the patterns shows instead of reading as "does not exist".
        let text_hits = if found {
            Vec::new()
        } else {
            self.store.search_sections(&plain_query(&format!("\"{id}\"")), None, None, 5).unwrap_or_default()
        };
        // A repository of the active project: its line from the project graph.
        let repo = self.memory.as_ref().and_then(|m| m.settings.graph.as_ref()).and_then(|g| g.describe(id));
        // Not registered: the code index may define it (a `.proto` message,
        // an rpc and its callers, a Rust or WGSL item).
        let code_lim = crate::code::Limits { defs: 8, refs: 24, impls: 4 };
        let code_ans = if found || self.code.is_none() {
            None
        } else {
            crate::code::symbol(self.store.conn(), id, code_lim).ok().filter(|a| a.totals.iter().any(|&n| n > 0))
        };
        if f.json {
            // Written straight from the hits: no `Value` copy of every string.
            let cap = 256 + l.definitions.iter().chain(&l.mentions).map(hit_bytes).sum::<usize>();
            let fr = fresh_json(&fresh, f.verbose);
            return Ok(Answer::JsonText(json::object(cap, |w| {
                w.field("found", &found)
                    .field("definitions", &l.definitions)
                    .field("mentions", &l.mentions)
                    .field("total_mentions", &l.total_mentions)
                    .field("truncated", &(l.mention_lines > l.mentions.len()));
                if let Some(fr) = &fr {
                    w.field("freshness", fr);
                }
                if f.verbose {
                    w.field("id", id).field("elapsed_us", &json!({"query": query_us, "freshness": fresh_us}));
                }
                if let Some(r) = &repo {
                    w.field("repo", r.as_str());
                }
                if let Some(a) = &code_ans {
                    w.field("code", crate::code::render(a, code_lim).as_str());
                }
                if !found {
                    let hits: Vec<Value> = text_hits
                        .iter()
                        .map(|h| json!({"source": h.source, "path": h.path, "heading_path": h.heading_path, "line_start": h.line_start, "line_end": h.line_end}))
                        .collect();
                    w.field("text_hits", &hits);
                }
            })));
        }
        let mut out = String::new();
        if let Some(r) = &repo {
            out.push_str(r);
            out.push('\n');
        }
        if let Some(a) = &code_ans {
            out.push_str("not registered; in the code index:\n");
            out.push_str(&crate::code::render(a, code_lim));
        } else if !found {
            out.push_str("not registered");
            if text_hits.is_empty() {
                out.push_str("; no section holds it\n");
            } else {
                out.push_str("; sections holding it:\n");
                let mut t = Table::new(&mut out, &["loc", "heading"]);
                for h in &text_hits {
                    t.field(&format!("{}:{}:{}-{}", h.source, h.path, h.line_start, h.line_end)).field(&h.heading_path);
                }
            }
        }
        if !l.definitions.is_empty() {
            out.push_str(&format!("defs {}\n", l.definitions.len()));
            {
                let mut t = Table::new(&mut out, &["loc", "why", "heading", "snippet"]);
                for d in &l.definitions {
                    t.field(&loc(d)).field(d.def.as_deref().unwrap_or("")).field(&d.heading_path).field(&d.context);
                }
            }
            // The lookup filled in the first definition's text, or every
            // one's when full.
            let shown = if full { &l.definitions[..] } else { &l.definitions[..1] };
            for d in shown {
                let (at, body) = (loc(d), def_text(d.text.as_deref().unwrap_or(""), full));
                if !body.is_empty() {
                    out.push_str(&format!("text {at}\n{body}\n"));
                }
            }
        }
        if l.total_mentions > 0 {
            out.push_str(&format!("mentions {} in {} places\n", l.total_mentions, l.mention_lines));
            {
                let mut t = Table::new(&mut out, &["loc", "n", "heading", "snippet"]);
                for m in &l.mentions {
                    t.field(&loc(m)).num(m.count).field(&m.heading_path).field(&m.context);
                }
            }
            text::more(&mut out, l.mentions.len(), l.mention_lines, limit);
        }
        fresh_text(&mut out, &fresh, f.verbose);
        if f.verbose {
            out.push_str(&format!("elapsed_us query={query_us} freshness={fresh_us}\n"));
        }
        Ok(Answer::Text(out))
    }

    pub(super) fn ids(&mut self, a: &Map, f: Fmt) -> Result<Answer> {
        let prefix = opt_str(a, "prefix")?;
        let pattern = match opt_str(a, "pattern")? {
            Some(p) => Some(
                RegexBuilder::new(p)
                    .build()
                    .map_err(|e| Error::InvalidArgument(format!("pattern: {e}")))?,
            ),
            None => None,
        };
        let kind = opt_str(a, "kind")?;
        let source = opt_str(a, "source")?;
        if let Some(s) = source {
            if !self.store.atlas_names().iter().any(|n| n == s) {
                return Err(Error::NotFound(format!("source {s:?}; served are {}", self.store.atlas_names().join(", "))));
            }
        }
        let limit = opt_uint(a, "limit", DEFAULT_IDS, super::MAX_LIST)?;
        let refresh = refresh_arg(a)?;
        let mut touched: Vec<String> = source.map(|s| vec![s.to_string()]).unwrap_or_default();
        let t0 = Instant::now();
        let (fresh, _) = self.fresh(&mut touched, refresh)?;
        let fresh_us = t0.elapsed().as_micros();
        let t1 = Instant::now();
        let (ids, total) = self.store.ids(prefix, pattern.as_ref(), kind, source, limit)?;
        let query_us = t1.elapsed().as_micros();
        if f.json {
            let fr = fresh_json(&fresh, f.verbose);
            return Ok(Answer::JsonText(json::object(64 + ids.len() * 96, |w| {
                w.field("total", &total).field("truncated", &(total > ids.len())).field("ids", &ids);
                if let Some(fr) = &fr {
                    w.field("freshness", fr);
                }
                if f.verbose {
                    w.field("elapsed_us", &json!({"query": query_us, "freshness": fresh_us}));
                }
            })));
        }
        let mut out = String::new();
        {
            let mut t = Table::new(&mut out, &["id", "kind", "defs", "occ", "last"]);
            for i in &ids {
                t.field(&i.id).field(&i.kind).num(i.definitions).num(i.occurrences).field(i.last_date.as_deref().unwrap_or(""));
            }
        }
        text::more(&mut out, ids.len(), total, limit);
        fresh_text(&mut out, &fresh, f.verbose);
        if f.verbose {
            out.push_str(&format!("elapsed_us query={query_us} freshness={fresh_us}\n"));
        }
        Ok(Answer::Text(out))
    }

    pub(super) fn search_sections(&mut self, a: &Map, f: Fmt) -> Result<Answer> {
        let query = arg_str(a, "query")?;
        let raw = opt_bool(a, "raw", false)?;
        let fts = if raw { query.to_string() } else { plain_query(query) };
        if fts.is_empty() {
            return Err(Error::InvalidArgument("query has no words to search for".into()));
        }
        let source = opt_str(a, "source")?.or(opt_str(a, "vault")?);
        if let Some(s) = source {
            if !self.store.atlas_names().iter().any(|n| n == s) {
                return Err(Error::NotFound(format!("source {s:?}; served are {}", self.store.atlas_names().join(", "))));
            }
        }
        let folder = opt_str(a, "folder")?;
        let limit = opt_uint(a, "limit", DEFAULT_SECTIONS, MAX_SECTIONS)?.max(1);
        let full = opt_bool(a, "full", false)?;
        let refresh = refresh_arg(a)?;
        let mode = opt_str(a, "mode")?.unwrap_or("sections");
        let mut note = String::new();
        let (mut hits, query_us, fresh, fresh_us);
        // Hybrid without a model on disk yet (it is downloaded at setup)
        // answers lexically and says so, rather than failing.
        let no_model = mode == "hybrid" && self.semantic.as_ref().is_some_and(|s| s.model.is_none() && !s.path.is_file());
        if no_model {
            note.push_str("model not downloaded yet (atlas --setup fetches it): lexical results only");
        }
        if mode == "sections" || no_model {
            // One past the limit tells whether more matched.
            let t0 = Instant::now();
            hits = self.store.search_sections(&fts, source, folder, limit + 1)?;
            query_us = t0.elapsed().as_micros();
            // Any source could hold a better hit than the ones found, so a
            // ranked search without a source filter checks every source.
            let mut touched: Vec<String> = source.map(|s| vec![s.to_string()]).unwrap_or_default();
            let t1 = Instant::now();
            let (fr, redo) = self.fresh(&mut touched, refresh)?;
            if redo {
                hits = self.store.search_sections(&fts, source, folder, limit + 1)?;
            }
            fresh = fr;
            fresh_us = t1.elapsed().as_micros();
        } else {
            // Freshness first: a re-index changes the sections the vectors
            // are looked up for.
            let mut touched: Vec<String> = source.map(|s| vec![s.to_string()]).unwrap_or_default();
            let t1 = Instant::now();
            fresh = self.fresh(&mut touched, refresh)?.0;
            fresh_us = t1.elapsed().as_micros();
            let t0 = Instant::now();
            let ids = self.semantic_ids(query, mode, source, folder, limit + 1, &mut note)?;
            hits = self.store.sections_by_id(&ids, source, folder)?;
            hits.truncate(limit + 1);
            query_us = t0.elapsed().as_micros();
        }
        let more = hits.len() > limit;
        hits.truncate(limit);
        if f.json {
            let fr = fresh_json(&fresh, f.verbose);
            let cap = 128 + hits.iter().map(|h| h.text.len() + h.path.len() + h.heading_path.len() + h.label.len() + 160).sum::<usize>();
            return Ok(Answer::JsonText(json::object(cap, |w| {
                w.field("results", &hits).field("truncated", &more);
                if let Some(fr) = &fr {
                    w.field("freshness", fr);
                }
                if !note.is_empty() {
                    w.field("vectors", &note);
                }
                if f.verbose {
                    w.field("mode", mode).field("fts_query", &fts).field("elapsed_us", &json!({"query": query_us, "freshness": fresh_us}));
                }
            })));
        }
        let words = recall::words(query);
        let needles: Vec<&str> = words.iter().map(String::as_str).collect();
        let mut out = String::new();
        if full {
            for h in &hits {
                out.push_str(&format!("## {}:{}:{}-{} {}\n{}\n", h.source, h.path, h.line_start, h.line_end, h.heading_path, h.text.trim_end()));
            }
        } else {
            let mut t = Table::new(&mut out, &["loc", "heading", "snippet"]);
            for h in &hits {
                t.field(&format!("{}:{}:{}-{}", h.source, h.path, h.line_start, h.line_end))
                    .field(&h.heading_path)
                    .field(&text::snippet(&h.text, &needles));
            }
        }
        if more {
            out.push_str(&format!("+more (limit={limit})\n"));
        }
        if !note.is_empty() {
            out.push_str(&format!("vectors {note}\n"));
        }
        fresh_text(&mut out, &fresh, f.verbose);
        if f.verbose {
            out.push_str(&format!("mode {mode}\nfts {fts}\nelapsed_us query={query_us} freshness={fresh_us}\n"));
        }
        Ok(Answer::Text(out))
    }

    /// Section ids for a semantic or hybrid search, best first. Loads the
    /// model and the vectors on first use, embeds a few missing sections
    /// inline and leaves the rest to the background embedder; `note` says
    /// how many sections still have no vector.
    fn semantic_ids(&mut self, query: &str, mode: &str, source: Option<&str>, folder: Option<&str>, want: usize, note: &mut String) -> Result<Vec<i64>> {
        let Some(sem) = self.semantic.as_mut() else {
            return Err(Error::InvalidArgument("semantic search is not set up".into()));
        };
        if sem.model.is_none() {
            let m = crate::embed::Model::load(&sem.path).map_err(|e| {
                Error::NotFound(format!("{e}; semantic search needs the model file: `atlas --setup` downloads the configured one (or set semantic.model)"))
            })?;
            sem.model = Some(std::sync::Arc::new(m));
        }
        let model = sem.model.clone().ok_or_else(|| Error::Io("model not loaded".into()))?;
        let conn = self.store.conn();
        let stale = match &sem.index {
            None => true,
            Some(ix) => ix.stale(conn)?,
        };
        if stale {
            let mut ix = crate::semantic::VecIndex::load(conn, &model.id, model.dim)?;
            if ix.missing > 0 && sem.embedder.as_ref().is_none_or(|e| e.done()) {
                // A few on this thread (an edit or two), the rest in the
                // background.
                if ix.missing <= crate::semantic::INLINE_BUDGET {
                    let mut c = rusqlite::Connection::open(&sem.db)?;
                    c.busy_timeout(std::time::Duration::from_secs(30))?;
                    crate::semantic::embed_pending(&mut c, &model, crate::semantic::INLINE_BUDGET, 1)?;
                    ix = crate::semantic::VecIndex::load(conn, &model.id, model.dim)?;
                } else {
                    sem.embedder = Some(crate::semantic::Embedder::start(sem.db.clone(), model.clone(), sem.threads));
                }
            }
            sem.index = Some(ix);
        }
        let ix = sem.index.as_ref().ok_or_else(|| Error::Io("vectors not loaded".into()))?;
        if ix.missing > 0 {
            *note = format!("missing for {} of {} sections (embedding in the background)", ix.missing, ix.missing + ix.ids.len());
            if let Some(e) = sem.embedder.as_ref().and_then(|e| e.error()) {
                *note = format!("missing for {} sections; embedding failed: {e}", ix.missing);
            }
        }
        let mut s = crate::embed::bert::Scratch::default();
        let qv = model.embed_query(query, &mut s);
        // Filters drop hits after ranking, so ask for more when filtering.
        let k = if source.is_some() || folder.is_some() { (want * 8).max(200) } else { want.max(50) };
        let vec_ids: Vec<i64> = ix.top(model.isa, &qv, k).into_iter().map(|x| x.0).collect();
        if mode == "semantic" {
            return Ok(vec_ids);
        }
        // Hybrid: the words ORed for FTS5 (a natural-language query rarely
        // has every word in one section), fused with the vector ranking.
        let words = recall::words(query);
        let or_query = words.iter().map(|w| recall::phrase_query(std::slice::from_ref(w))).collect::<Vec<_>>().join(" OR ");
        let fts_ids: Vec<i64> = if or_query.is_empty() { Vec::new() } else { self.store.search_sections(&or_query, source, folder, k)?.into_iter().map(|h| h.id).collect() };
        Ok(crate::semantic::rrf(&[&fts_ids, &vec_ids]))
    }

    pub(super) fn list_sources(&mut self, f: Fmt) -> Result<Answer> {
        let (fresh, _) = self.store.freshness(&[], Refresh::Check)?;
        let sources = self.store.list_sources()?;
        if f.json {
            let fr = fresh_json(&fresh, f.verbose);
            return Ok(Answer::JsonText(json::object(64 + sources.len() * 160, |w| {
                w.field("sources", &sources);
                if let Some(fr) = &fr {
                    w.field("freshness", fr);
                }
            })));
        }
        let mut out = String::new();
        {
            let mut t = Table::new(&mut out, &["name", "kind", "files", "sections", "root"]);
            for s in &sources {
                let root = if s.include.is_empty() { s.root.clone() } else { format!("{} {}", s.root, s.include.join(",")) };
                t.field(&s.name).field(s.kind).num(s.files).num(s.sections).field(&root);
            }
        }
        fresh_text(&mut out, &fresh, f.verbose);
        Ok(Answer::Text(out))
    }

    pub(super) fn report_miss(&mut self, a: &Map) -> Result<Value> {
        let tool = arg_str(a, "tool")?;
        if !["lookup", "ids", "search", "list_sources", "other"].contains(&tool) {
            return Err(Error::InvalidArgument("tool must be lookup, ids, search, list_sources or other".into()));
        }
        let query = short(arg_str(a, "query")?, 1024)?;
        let found = short(arg_str(a, "found")?, 4096)?;
        if found.trim().is_empty() {
            return Err(Error::InvalidArgument("found must say where grep or a reader found it".into()));
        }
        let returned = short(opt_str(a, "returned")?.unwrap_or(""), 4096)?;
        let reason = self.reason.clone();
        let client = self.client.clone();
        let m = self.store.report_miss(tool, query, returned, found, &reason, &client)?;
        Ok(json!({ "miss": m }))
    }

    pub(super) fn misses(&mut self, a: &Map, f: Fmt) -> Result<Answer> {
        let status = match opt_str(a, "status")?.unwrap_or("open") {
            "all" => None,
            s @ ("open" | "resolved") => Some(s),
            o => return Err(Error::InvalidArgument(format!("status {o:?}; use open, resolved or all"))),
        };
        let limit = opt_uint(a, "limit", DEFAULT_MISSES, super::MAX_LIST)?;
        let (rows, total) = self.store.misses(status, limit)?;
        if f.json {
            return Ok(Answer::JsonText(json::object(64 + rows.len() * 256, |w| {
                w.field("total", &total).field("truncated", &(total > rows.len())).field("misses", &rows);
            })));
        }
        let mut out = String::new();
        {
            let mut t = Table::new(&mut out, &["id", "time", "tool", "status", "query", "found", "resolution"]);
            for m in &rows {
                t.num(m.id)
                    .field(m.time.get(..16).unwrap_or(&m.time))
                    .field(&m.tool)
                    .field(&m.status)
                    .field(&m.query)
                    .field(&m.found)
                    .field(m.resolution.as_deref().unwrap_or(""));
            }
        }
        text::more(&mut out, rows.len(), total, limit);
        Ok(Answer::Text(out))
    }

    pub(super) fn resolve_miss(&mut self, a: &Map) -> Result<Value> {
        let id = match a.get("id") {
            Some(Value::Number(n)) => n.as_i64().ok_or_else(|| Error::InvalidArgument("id must be an integer".into()))?,
            _ => return Err(Error::InvalidArgument("id is required (an integer from report_miss or misses)".into())),
        };
        let note = self.reason.clone();
        let client = self.client.clone();
        // The miss's own query, asked again: a miss is resolved only when
        // the answer now reaches what was found.
        let (all, _) = self.store.misses(None, usize::MAX)?;
        let miss = all.into_iter().find(|m| m.id == id).ok_or_else(|| Error::NotFound(format!("miss {id}")))?;
        if miss.status == "open" {
            if let (Some(args), Some(want)) = (rerun_args(&miss.tool, &miss.query), found_file(&miss.found)) {
                let text = match self.run(&miss.tool, &args, false)? {
                    super::Answer::Text(t) | super::Answer::JsonText(t) => t,
                    super::Answer::Json(v) => v.to_string(),
                };
                if !text.contains(want) {
                    let head: String = text.chars().take(400).collect();
                    return Err(Error::InvalidArgument(format!(
                        "miss {id} is not fixed: {} {:?} still answers without {want} (found: {}). It answers:
{head}",
                        miss.tool, miss.query, miss.found
                    )));
                }
            }
        }
        let m = self.store.resolve_miss(id, &note, &client)?;
        Ok(json!({ "miss": m }))
    }
}

/// The arguments that ask a miss's query again, for the tools that can be
/// asked again.
fn rerun_args(tool: &str, query: &str) -> Option<Value> {
    match tool {
        "lookup" => Some(json!({"id": query, "limit": 200})),
        "search" => Some(json!({"query": query, "limit": 50})),
        _ => None,
    }
}

/// The file name a miss's `found` points at (`dir/NAME.md:12` gives
/// `NAME.md`), when it names one.
fn found_file(found: &str) -> Option<&str> {
    let tok = found.split_whitespace().find(|t| t.contains('/') || t.contains('\\') || t.contains(".md"))?;
    let name = tok.rsplit(['/', '\\']).next().unwrap_or(tok);
    let name = name.split(':').next().unwrap_or(name).trim_matches(|c: char| c == ',' || c == ';' || c == ')' || c == '(');
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::plain_query;

    #[test]
    fn plain_queries_become_phrases() {
        assert_eq!(plain_query("ALPHA-BETA gamma"), "\"alpha beta\" \"gamma\"");
        assert_eq!(plain_query("\"ground look\" #12"), "\"ground look\" \"12\"");
        assert_eq!(plain_query("--- !!"), "");
    }
}
