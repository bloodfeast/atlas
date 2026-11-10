//! The memory tools: remember, recall, supersede, journal, handoff,
//! journal_status, memory_log, and the `hook` entry point.
//!
//! Answers are plain text: records as `== key kind status vN time` headers
//! with their text, lists as a header row plus tab-separated rows. Every
//! write regenerates the exports it touched and takes a snapshot when one is
//! due.

use std::collections::BTreeSet;

use crate::error::{Error, Result};
use crate::hooks::{HookEvent, Hooks};
use crate::json::{Map, Value};
use crate::memory::export::{self, Shape};
use crate::memory::recall;
use crate::memory::settings::Settings;
use crate::memory::{Draft, LogFilter, MemoryStore, Place, Provenance, ENTRIES};
use crate::project::USER_EXPORT as USER;
use crate::text::Table;

use super::{arg_nonblank, arg_str, opt_bool, opt_str, opt_uint, Answer, Server};

/// The lowest score a `recall {query}` pointer is listed at (it prints as
/// `0.1`; below, it matched next to nothing).
const MIN_POINTER_SCORE: f64 = 0.05;

/// The line before a `recall {query}` answer's pointers.
const QUERY_HEAD: &str = "more records, best first (key score: title; read one with recall key=KEY):\n";

/// How close to the best score a record must be to come whole.
const LEAD_RATIO: f64 = 0.8;

/// The most records a query answer gives whole.
const LEAD_BODIES: usize = 2;

/// The tools answered here (no vault rescan first).
pub(super) const MEMORY_TOOLS: &[&str] = &["remember", "recall", "supersede", "journal", "handoff", "journal_status", "memory_log", "hook"];

/// The memory store and hook state of a server, opened on first use.
pub struct MemState {
    /// The settings.
    pub settings: Settings,
    store: Option<MemoryStore>,
    hooks: Option<Hooks>,
}

impl MemState {
    /// Settings only; nothing is opened yet.
    pub fn new(settings: Settings) -> MemState {
        MemState { settings, store: None, hooks: None }
    }
}

fn strs(a: &Map, k: &str) -> Result<Vec<String>> {
    match a.get(k) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(v)) => v.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| Error::InvalidArgument(format!("{k} must be a list of strings")))).collect(),
        Some(_) => Err(Error::InvalidArgument(format!("{k} must be a list of strings"))),
    }
}

fn refs(a: &Map) -> Result<Vec<(String, String)>> {
    strs(a, "refs")?
        .into_iter()
        .map(|r| match r.split_once(':') {
            Some((k, t)) if !k.trim().is_empty() && !t.trim().is_empty() => Ok((k.trim().to_string(), t.trim().to_string())),
            _ => Err(Error::InvalidArgument(format!("ref {r:?}: expected kind:target"))),
        })
        .collect()
}

use crate::memory::{short_title, title_and_body, TITLE_CHARS};

/// The longest memory summary (its index hook).
const SUMMARY_CHARS: usize = 200;

/// A title or summary given as its own argument: one line of at most `max`
/// characters, refused rather than cut, since nothing else keeps the rest.
fn given_line(s: &str, what: &str, max: usize) -> Result<String> {
    let t = s.trim();
    if t.contains('\n') || t.chars().count() > max {
        return Err(Error::InvalidArgument(format!("{what} must be one line of at most {max} characters; put the detail in the text")));
    }
    Ok(t.to_string())
}

fn check_slug(k: &str) -> Result<()> {
    let ok = !k.is_empty() && k.len() <= 96 && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !k.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!("memory key {k:?}: use lowercase letters, digits and hyphens")))
    }
}

/// A rule's body as one list item of an items export, so the export splits
/// back into exactly its records: a body that is not a `- ` item already
/// becomes `- YYYY-MM-DD AUTHOR: text`, and its later lines are indented as
/// the item's continuation.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an empty body, a blank line inside it, or
/// a body holding more than one item.
fn rule_body(body: &str, author: &str) -> Result<String> {
    let text = body.trim_matches(['\n', '\r']);
    if text.trim().is_empty() {
        return Err(Error::InvalidArgument("body is empty".into()));
    }
    let mut out = String::with_capacity(text.len() + 32);
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            return Err(Error::InvalidArgument("a rule is one list item: no blank lines inside its body".into()));
        }
        if i == 0 {
            if !line.starts_with("- ") {
                let day: String = crate::memory::now().chars().take(10).collect();
                out.push_str(&format!("- {day} {author}: "));
            }
            out.push_str(line);
        } else if line.starts_with("- ") {
            return Err(Error::InvalidArgument("a rule is one list item: remember each rule on its own".into()));
        } else if line.starts_with(' ') || line.starts_with('\t') {
            out.push_str(line);
        } else {
            out.push_str("  ");
            out.push_str(line);
        }
        out.push('\n');
    }
    Ok(out)
}

fn with_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

impl Server {
    /// Serves the memory tools and hooks with these settings.
    pub fn set_memory(&mut self, settings: Settings) {
        self.store.set_memory_sync(Some(settings.clone()));
        self.memory = Some(MemState::new(settings));
    }

    fn mem(&mut self) -> Result<&mut MemState> {
        let m = self.memory.as_mut().ok_or_else(|| Error::NotFound("the memory store is not configured".into()))?;
        if m.store.is_none() {
            m.store = Some(MemoryStore::open(&m.settings.store)?);
        }
        Ok(m)
    }

    /// A memory tool's answer; with `format: "json"`, JSON: a record recalled
    /// by key and the lane status as structured fields, every other answer
    /// (writes, query and lane recalls, the log) as `{"text": ...}` around
    /// its text. The hook answers in JSON already.
    pub(super) fn run_memory(&mut self, name: &str, a: &Map) -> Result<Answer> {
        let json = match opt_str(a, "format")? {
            None | Some("text") => false,
            Some("json") => true,
            Some(o) => return Err(Error::InvalidArgument(format!("format {o:?}; use text or json"))),
        };
        if !json || name == "hook" {
            return self.run_memory_text(name, a);
        }
        match name {
            "journal_status" => return self.journal_status_json(a).map(Answer::Json),
            "recall" => {
                if let Some(v) = self.recall_record_json(a)? {
                    return Ok(Answer::Json(v));
                }
            }
            _ => {}
        }
        Ok(match self.run_memory_text(name, a)? {
            Answer::Text(t) => Answer::Json(crate::json!({"text": t})),
            other => other,
        })
    }

    /// `recall {key}` as fields, for a key that names a record (not a ref
    /// and not `history`); `None` leaves the answer to the text form.
    fn recall_record_json(&mut self, a: &Map) -> Result<Option<Value>> {
        let Some(k) = opt_str(a, "key")? else { return Ok(None) };
        if opt_bool(a, "history", false)? {
            return Ok(None);
        }
        let synced = self.store.memory_sync_sources();
        if !synced.is_empty() {
            self.store.freshness_with(&synced, crate::store::Refresh::Auto, false)?;
        }
        let m = self.mem()?;
        let store = m.store.as_ref().ok_or_else(|| Error::Db("store".into()))?;
        let Some(h) = store.head(k)? else { return Ok(None) };
        let mut v = crate::json!({
            "key": h.key, "kind": h.kind, "sub": h.sub, "scope": h.scope, "status": h.status,
            "version": h.seq, "time": h.time, "sha": h.sha, "title": h.title, "meta": h.meta, "body": h.body,
        });
        if let (Some(o), Some(by)) = (v.as_object_mut(), &h.superseded_by) {
            o.insert("superseded_by".into(), Value::String(by.clone()));
            o.insert("superseded_time".into(), Value::String(h.superseded_time.clone().unwrap_or_default()));
        }
        if let (Some(o), Some(anchor)) = (v.as_object_mut(), &h.anchor) {
            o.insert("anchor".into(), Value::String(anchor.clone()));
        }
        Ok(Some(v))
    }

    fn run_memory_text(&mut self, name: &str, a: &Map) -> Result<Answer> {
        match name {
            "hook" => {
                let ev = HookEvent::from_value(&Value::Object(a.clone()));
                let m = self.mem()?;
                if m.hooks.is_none() {
                    let mut h = Hooks::open(m.settings.clone())?;
                    // As in `protocol`: a tool call does not say a user is there.
                    h.set_unattended(true);
                    m.hooks = Some(h);
                }
                let h = m.hooks.as_mut().ok_or_else(|| Error::Db("hooks".into()))?;
                Ok(Answer::Text(h.handle(&ev).to_json(&ev.event)))
            }
            "recall" => {
                // The sources the store is synced from, refreshed first (a
                // watched, unchanged source costs a lookup): recall answers
                // what the files say now.
                let synced = self.store.memory_sync_sources();
                if !synced.is_empty() {
                    self.store.freshness_with(&synced, crate::store::Refresh::Auto, false)?;
                }
                self.recall(a).map(Answer::Text)
            }
            "journal_status" => self.journal_status(a).map(Answer::Text),
            "memory_log" => self.memory_log(a).map(Answer::Text),
            "remember" | "supersede" | "journal" | "handoff" => self.memory_write(name, a).map(Answer::Text),
            _ => Err(Error::NotFound(format!("tool {name:?}"))),
        }
    }

    fn recall(&mut self, a: &Map) -> Result<String> {
        let limit = opt_uint(a, "limit", 8, 200)?;
        let history = opt_bool(a, "history", false)?;
        // A compaction card's chain key (a group's range, a record's part,
        // a link): resolved from this project's transcript itself, never
        // the index ([`crate::chain`]). Off unless the transcript source or
        // the compaction work is on.
        if let Some(k) = opt_str(a, "key")?.filter(|k| crate::chain::parse_key(k).is_some()) {
            let on = {
                let m = self.mem()?;
                m.settings.transcripts || m.settings.compact.enabled
            };
            if !on {
                return Err(Error::InvalidArgument("chain keys read this project's transcript, which is off (memory.transcripts and compact.enabled)".into()));
            }
            let dir = crate::chain::projects_dir()
                .ok_or_else(|| Error::NotFound("no Claude Code folder: set CLAUDE_CONFIG_DIR".into()))?
                .join(crate::transcripts::project_of_cwd(&std::env::current_dir()?));
            return crate::chain::resolve(&dir, k);
        }
        // The opt-in transcript source: a turn ref, or a search of turns.
        let turn_key = opt_str(a, "key")?.and_then(crate::compact::parse_ref).filter(|r| matches!(r.target, crate::compact::Target::Turn { .. }));
        if turn_key.is_some() || opt_str(a, "kind")? == Some("turn") {
            let (on, idx) = {
                let m = self.mem()?;
                (m.settings.transcripts, m.settings.index_db.clone())
            };
            if !on {
                return Err(Error::InvalidArgument("the transcript source is off (memory.transcripts); turn refs and kind turn need it on".into()));
            }
            let conn = crate::store::open_raw(&idx)?;
            // This project's turns only: the folder Claude Code files this
            // session's transcripts under.
            let project = crate::transcripts::project_of_cwd(&std::env::current_dir()?);
            if let Some(r) = turn_key {
                let crate::compact::Target::Turn { session8, at } = &r.target else { unreachable!() };
                let not_indexed = || Error::NotFound(format!("{} is not indexed in this project (turns are indexed at each compaction)", r.text));
                // A checkpoint's byte ref: its line, from the indexed transcript.
                let line = match *at {
                    crate::compact::TurnAt::Line(n) => n,
                    crate::compact::TurnAt::Byte(o) => {
                        let path = crate::transcripts::path_of(&conn, &project, session8)?.ok_or_else(not_indexed)?;
                        crate::transcripts::line_at(&path, o).ok_or_else(|| Error::NotFound(format!("ref {:?} is dangling: no record starts at byte {o} of {}", r.text, path.display())))?
                    }
                };
                let t = crate::transcripts::get(&conn, &project, session8, line)?.ok_or_else(not_indexed)?;
                if !t.hash.starts_with(&r.hash) {
                    return Err(Error::NotFound(format!("ref {:?} is dangling: {} hashes to {}", r.text, t.id, t.hash)));
                }
                return Ok(format!("{}@{} {} (current)\n{}\n", t.id, t.hash, t.role, t.text));
            }
            let q = arg_str(a, "query").map_err(|_| Error::InvalidArgument("kind turn needs a query".into()))?;
            let mut out = String::new();
            for t in crate::transcripts::search(&conn, &project, q, limit)? {
                let first: String = t.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(160).collect();
                out.push_str(&format!("- {}@{} {}: {first}\n", t.id, t.hash, t.role));
            }
            return Ok(if out.is_empty() { "no turns match in this project\n".into() } else { out });
        }
        // `src:path:a-b@hash`: those lines, verbatim, when the hash still
        // names them; else the current lines and their new ref (the design:
        // a file keeps no old versions). Read as `read_note` reads, through
        // the vault's or the source's own checks, so a ref reaches nothing
        // `read_note` would refuse.
        if let Some(r) = opt_str(a, "key")?.and_then(crate::compact::parse_ref) {
            if let crate::compact::Target::Lines { src, path, a: from, b: to } = &r.target {
                if self.store.vaults().get(src).is_ok() || self.store.sources().iter().any(|s| &s.name == src) {
                    let text = match self.read_confined(src, path) {
                        Ok((_, _, bytes)) => String::from_utf8(bytes).map_err(|_| Error::InvalidArgument(format!("{src}:{path} is not valid UTF-8")))?,
                        Err(e) => return Err(Error::NotFound(format!("ref {:?} is dangling: {src}:{path} is not readable ({e})", r.text))),
                    };
                    let status = crate::compact::lines_status(&text, src, path, *from, *to, &r.hash);
                    let lines: Vec<&str> = text.lines().skip(from - 1).take(to + 1 - from).collect();
                    let head = match status {
                        crate::compact::Status::Current => format!("{} (current)", r.text),
                        crate::compact::Status::Changed(now) => format!("{} (changed: these are the current lines, {now}; the old text is gone from the file)", r.text),
                        _ => return Err(Error::NotFound(format!("ref {:?} is dangling: {src}:{path} has no lines {from}-{to}", r.text))),
                    };
                    return Ok(format!("{head}\n{}\n", lines.join("\n")));
                }
            }
        }
        let m = self.mem()?;
        let bpt = m.settings.bytes_per_token;
        let lane_budget = m.settings.recall.lane_budget_tokens;
        let anchors = m.settings.recall.pointer_anchors;
        let rules = m.settings.recall.rules;
        let min_terms = m.settings.recall.min_terms;
        let has_exports = m.settings.export_dir.is_some() && !m.settings.exports.is_empty();
        let store = m.store.as_ref().ok_or_else(|| Error::Db("store".into()))?;
        let mut out = String::new();
        if let Some(k) = opt_str(a, "key")? {
            // `key@hash`: that exact version, verbatim, or a clear dangling.
            if store.head(k)?.is_none() {
                if let Some(r) = crate::compact::parse_ref(k) {
                    if let crate::compact::Target::Record(key) = &r.target {
                        let (status, version) = crate::compact::resolve_record(store, key, &r.hash)?;
                        let Some(v) = version else {
                            return Err(Error::NotFound(format!("ref {k:?} is dangling: no version of {key:?} has that hash; recall {{key: {key:?}}} for the current text")));
                        };
                        let head = match &status {
                            crate::compact::Status::Changed(now) => {
                                format!("{} v{} {} ref {} (changed: the current text is {now})", v.key, v.seq, v.time.get(..16).unwrap_or(&v.time), crate::compact::record_ref(&v.key, &v.sha))
                            }
                            // The current version reads as `recall {key}` does
                            // (its anchor and metadata too), with its ref.
                            _ => {
                                if let Some(h) = store.head(key)? {
                                    recall::render(&mut out, &h);
                                    out.push_str(&format!("ref {} (current)\n", crate::compact::record_ref(&v.key, &v.sha)));
                                    return Ok(out);
                                }
                                format!("{} v{} {} ref {} (current)", v.key, v.seq, v.time.get(..16).unwrap_or(&v.time), crate::compact::record_ref(&v.key, &v.sha))
                            }
                        };
                        let body = if v.meta.trim().is_empty() { v.body.clone() } else { format!("{}\n{}", v.meta.trim_end(), v.body) };
                        recall::frame(&mut out, &head, (!v.title.is_empty()).then(|| v.title.trim_end()), &body);
                        return Ok(out);
                    }
                }
            }
            let h = store.head(k)?.ok_or_else(|| Error::NotFound(format!("record {k:?}; try recall {{query}} or grep the exports")))?;
            if history {
                for v in store.history(k)? {
                    let head = format!("{} v{} {} sha={} by {}: {}", v.key, v.seq, v.time, &v.sha[..16], v.author, v.reason);
                    recall::frame(&mut out, &head, None, &v.body);
                }
            } else {
                recall::render(&mut out, &h);
                out.push_str(&format!("sha {}\n", h.sha));
            }
            return Ok(out);
        }
        if let Some(l) = opt_str(a, "lane")? {
            let budget = (opt_uint(a, "budget_tokens", lane_budget, 100_000)? as f64 * bpt) as usize;
            let t = recall::lane(store, l, budget)?;
            return Ok(if t.is_empty() { format!("lane {l}: no journal or handoff records\n") } else { t });
        }
        let q = arg_str(a, "query").map_err(|_| Error::InvalidArgument("recall needs key, lane or query".into()))?;
        let budget = (opt_uint(a, "budget_tokens", 1500, 100_000)? as f64 * bpt) as usize;
        let (kind, scope) = (opt_str(a, "kind")?, opt_str(a, "scope")?);
        // A filtered search keeps every word up to the terms' cap, so past it words
        // go unsearched by position, and only the answer can say so.
        let (terms, unsearched) = if kind.is_some() || scope.is_some() { recall::terms_and_rest(q) } else { (recall::terms(q), 0) };
        let unsearched = (unsearched > 0).then(|| format!("{unsearched} more word{} not searched: a filter searches a query's first {} words", if unsearched == 1 { " was" } else { "s were" }, crate::memory::FILTERED_TERMS));
        let live = rules == crate::memory::rules::Mode::Live;
        // One past the limit, to know whether the limit cut anything. Zero stays
        // what it was: no limit but the search's own window.
        let cap = if limit == 0 { crate::memory::UNLIMITED_ROWS } else { limit };
        let mut raw = store.search_with(&terms, kind, scope, cap + 1, rules)?;
        let past_limit = raw.len() > cap;
        raw.truncate(cap);
        let mut hits: Vec<(crate::memory::Head, f64)> = Vec::with_capacity(raw.len());
        let mut tags: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
        let mut sure: std::collections::HashSet<i64> = std::collections::HashSet::new();
        if live {
            let need = min_terms.min(terms.len().max(1));
            for x in crate::memory::rules::live_hits(store, raw, &terms, kind, scope, need)? {
                if x.sure {
                    sure.insert(x.head.rid);
                }
                tags.insert(x.head.rid, x.tags);
                hits.push((x.head, x.score));
            }
        } else {
            hits.extend(raw.into_iter().map(|(h, score, _)| (h, score)));
        }
        if hits.is_empty() {
            let records: i64 = store.conn().query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
            let none: String = match (records, has_exports) {
                (0, _) => "no records match: the store holds none yet\n".into(),
                (_, true) => "no records match; grep the exports and report_miss what grep finds\n".into(),
                (_, false) => "no records match; report_miss what you looked for\n".into(),
            };
            return Ok(match unsearched {
                Some(u) if records > 0 => format!("{none}+{u}\n"),
                _ => none,
            });
        }
        // The leading records whole, the rest as pointers, best first. A
        // record leads when it scores within LEAD_RATIO of the best (at most
        // LEAD_BODIES): a clear winner comes whole alone; a near tie (often
        // a ruling and the memory that carries it) comes whole on both
        // sides, because a title cannot tell them apart. Pointers carry key,
        // bm25 score and the one-line title; a body is one `recall {key}`
        // away (measured 2026-11-02: bodies were half of all answer tokens,
        // and most were never used).
        let top = hits[0].1;
        let (mut more, mut weak, mut headed) = (0, 0, false);
        for (i, (h, score)) in hits.iter().enumerate() {
            if i < LEAD_BODIES && *score >= top * LEAD_RATIO {
                let mut one = String::new();
                recall::render(&mut one, h);
                if let Some(t) = tags.get(&h.rid).filter(|t| !t.is_empty()) {
                    one.push_str(&format!("[{}]\n", t.join("; ")));
                }
                if out.len() + one.len() <= budget || out.is_empty() {
                    out.push_str(&one);
                    continue;
                }
            }
            // A pointer that would read `0.0` is left out, and counted: it
            // can be a real match, since in a small store every word scores
            // next to nothing.
            let kept = sure.contains(&h.rid);
            if *score < MIN_POINTER_SCORE && !kept {
                weak += 1;
                continue;
            }
            if !headed {
                out.push_str(QUERY_HEAD);
                headed = true;
            }
            // With `recall.pointer_anchors`, the pointer ends with where the
            // record's text sits (`@src:path:line`), so a follow-up can read
            // those lines (`read_note`) instead of the record.
            let at = if anchors { recall::pointer_anchor(h).map(|a| format!(" @{a}")).unwrap_or_default() } else { String::new() };
            let tag = tags.get(&h.rid).filter(|t| !t.is_empty()).map(|t| format!(" [{}]", t.join("; "))).unwrap_or_default();
            let one = format!("- {} {score:.1}: {}{tag}{at}\n", h.key, crate::hooks::pointer_title(h));
            if out.len() + one.len() > budget && !kept {
                more += 1;
                continue;
            }
            out.push_str(&one);
        }
        if more > 0 {
            out.push_str(&format!("+{more} more over budget_tokens\n"));
        }
        if weak > 0 {
            out.push_str(&format!("+{weak} more scoring near zero; a narrower query finds them\n"));
        }
        if let Some(u) = unsearched {
            out.push_str(&format!("+{u}\n"));
        }
        if past_limit {
            out.push_str(&if limit == 0 { format!("+more (limit=0 shows the best {cap})\n") } else { format!("+more (limit={limit})\n") });
        }
        Ok(out)
    }

    fn journal_status(&mut self, a: &Map) -> Result<String> {
        let mut out = String::new();
        let mut t = Table::new(&mut out, &["lane", "entries", "last", "last_commit", "open_questions", "handoffs"]);
        for l in self.status_lanes(a)? {
            t.field(&l.lane).num(l.entries).field(l.last.get(..16).unwrap_or(&l.last)).field(&l.last_commit).num(l.open_questions).num(l.handoffs);
        }
        Ok(out)
    }

    fn journal_status_json(&mut self, a: &Map) -> Result<Value> {
        let lanes: Vec<Value> = self
            .status_lanes(a)?
            .into_iter()
            .map(|l| crate::json!({"lane": l.lane, "entries": l.entries, "last": l.last, "last_commit": l.last_commit, "open_questions": l.open_questions, "handoffs": l.handoffs}))
            .collect();
        Ok(crate::json!({"lanes": lanes}))
    }

    /// The lanes `journal_status` lists: every lane, or with `repo` those
    /// about the repositories it covers.
    fn status_lanes(&mut self, a: &Map) -> Result<Vec<crate::memory::LaneStatus>> {
        let covered = self.repo_coverage(a)?;
        let m = self.mem()?;
        let store = m.store.as_ref().ok_or_else(|| Error::Db("store".into()))?;
        let mut out = Vec::new();
        for l in store.lanes()? {
            if let Some(c) = &covered {
                // A lane is about a repository when its name or one of its
                // entries names it or a repository it talks to.
                let about = crate::project::Graph::mentions(&l.lane, c)
                    || store.heads(None, Some(&l.lane))?.iter().filter(|h| h.kind == "journal" || h.kind == "handoff").any(|h| crate::project::Graph::mentions(&format!("{}\n{}\n{}", h.title, h.meta, h.body), c));
                if !about {
                    continue;
                }
            }
            out.push(l);
        }
        Ok(out)
    }

    /// The repositories a `repo` argument covers (see
    /// [`crate::project::Graph::coverage`]), or `None` without one.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] when no project with repositories is
    /// active, or it has no such repository.
    pub(super) fn repo_coverage(&self, a: &Map) -> Result<Option<Vec<String>>> {
        let Some(name) = opt_str(a, "repo")? else { return Ok(None) };
        let g = self.memory.as_ref().and_then(|m| m.settings.graph.clone()).ok_or_else(|| Error::InvalidArgument("repo filters by the active project's repos, and no project with repos is active".into()))?;
        g.coverage(name).map(Some)
    }

    fn memory_log(&mut self, a: &Map) -> Result<String> {
        let f = LogFilter {
            key: opt_str(a, "key")?.map(str::to_string),
            kind: opt_str(a, "kind")?.map(str::to_string),
            scope: opt_str(a, "scope")?.map(str::to_string),
            since: opt_str(a, "since")?.map(str::to_string),
            author: opt_str(a, "author")?.map(str::to_string),
            limit: opt_uint(a, "limit", 20, 2000)?,
        };
        let m = self.mem()?;
        let store = m.store.as_ref().ok_or_else(|| Error::Db("store".into()))?;
        let mut out = String::new();
        let mut t = Table::new(&mut out, &["time", "key", "kind", "v", "author", "title", "reason"]);
        for v in store.log(&f)? {
            t.field(v.time.get(..19).unwrap_or(&v.time)).field(&v.key).field(&v.kind).num(v.seq).field(&v.author).field(&short_title(&v.title)).field(&v.reason);
        }
        Ok(out)
    }

    fn memory_write(&mut self, name: &str, a: &Map) -> Result<String> {
        let client = self.client.clone();
        let reason = if self.reason.is_empty() { format!("{name} entry") } else { self.reason.clone() };
        let author = opt_str(a, "author")?.unwrap_or(&client).to_string();
        let refs = refs(a)?;
        let m = self.mem()?;
        let settings = m.settings.clone();
        let store = m.store.as_mut().ok_or_else(|| Error::Db("store".into()))?;
        // A `queued` journal entry is planned work: read and checked as
        // `atlas queue plan` does, and filed with the same meta lines.
        let planned_meta = match (name, opt_str(a, "entry")?, opt_str(a, "amend")?) {
            ("journal", Some(crate::queue::ENTRY), None) => {
                let text = arg_nonblank(a, "text")?;
                let (head, rest) = text.split_once('\n').unwrap_or((text, ""));
                let p = crate::queue::planned_from_meta(arg_nonblank(a, "lane")?, head, rest, opt_str(a, "meta")?.unwrap_or("")).map_err(Error::InvalidArgument)?;
                crate::queue::check_planned(store, &p).map_err(Error::InvalidArgument)?;
                Some(crate::queue::planned_meta(&p))
            }
            _ => None,
        };
        let mut w = store.writer()?;
        let draft = |title: String, meta: String, body: String| Draft { title, meta, body, reason: reason.clone(), author: author.clone(), client: client.clone(), provenance: None, time: None };
        // A memory written to the user layer, and the keys it replaces: its
        // file goes to the user layer folder once the store has it.
        let mut user_write: Option<(String, Vec<String>)> = None;
        let written = match name {
            "journal" => {
                let lane = arg_nonblank(a, "lane")?;
                let entry = arg_str(a, "entry")?;
                if !ENTRIES.contains(&entry) {
                    return Err(Error::InvalidArgument(format!("entry {entry:?}; use one of {}", ENTRIES.join(", "))));
                }
                let review_meta = opt_str(a, "meta")?.unwrap_or("");
                // An amend keeps the entry's meta, so a review's is not asked again.
                let amend = opt_str(a, "amend")?;
                if entry == crate::reviews::ENTRY && amend.is_none() && (crate::reviews::Review::parse(review_meta).is_none() || !crate::reviews::verdicts_known(review_meta)) {
                    return Err(Error::InvalidArgument(
                        "a review entry needs meta lines `role:` (or several, comma-separated), `pr:`, `head:` (the sha it read, 7 or more hex digits) and `verdict:` (pass, merge, merge with follow-ups or request changes, each with an optional `(role)`); `report:` is kept".into(),
                    ));
                }
                let text = arg_nonblank(a, "text")?;
                let (head, rest) = text.split_once('\n').unwrap_or((text, ""));
                // `amend: KEY` rewrites that entry's title and body as a new
                // version; its lane and entry must be the ones given, and its
                // meta lines and refs stay as they are.
                if let Some(key) = amend {
                    let sha = opt_str(a, "expected_sha")?.ok_or_else(|| Error::InvalidArgument(format!("amend {key}: pass expected_sha, the head's sha (recall {{key}} shows it)")))?;
                    if opt_str(a, "meta")?.is_some() || !refs.is_empty() {
                        return Err(Error::InvalidArgument("amend rewrites the text only: its meta lines and refs are kept, so pass neither".into()));
                    }
                    let (title, body) = title_and_body(head, rest);
                    w.amend_journal(key, sha, lane, entry, &draft(title, String::new(), body))?
                } else {
                    let key = w.next_key(&format!("journal:{lane}:"), 4)?;
                    let meta = match &planned_meta {
                        Some(m) => with_newline(m.clone()),
                        None => opt_str(a, "meta")?.map(|s| with_newline(s.to_string())).unwrap_or_default(),
                    };
                    let (title, body) = title_and_body(head, rest);
                    let r = w.create(&key, "journal", entry, lane, &draft(title, meta, body), None)?;
                    let mut all = refs.clone();
                    all.push(("lane".into(), lane.to_string()));
                    w.add_refs(&key, &all)?;
                    r
                }
            }
            "handoff" => {
                let lane = arg_nonblank(a, "lane")?;
                let key = w.next_key(&format!("handoff:{lane}:"), 3)?;
                let place = settings.handoff_export().map(|e| (e.name.as_str(), Place::End));
                let text = arg_str(a, "text")?;
                // No title: the text's first line is the title, as in a
                // journal entry, and the rest is the body.
                let (title, body) = match opt_str(a, "title")?.map(str::trim).filter(|t| !t.is_empty()) {
                    Some(t) => title_and_body(t, text),
                    None => {
                        let text = text.trim_start();
                        let (head, rest) = text.split_once('\n').unwrap_or((text, ""));
                        if head.trim().is_empty() {
                            return Err(Error::InvalidArgument("title is required: pass title, or a text whose first line is the title".into()));
                        }
                        title_and_body(head, rest)
                    }
                };
                let d = draft(title, String::new(), with_newline(body));
                // A new handoff supersedes the lane's active one, so a resume
                // finds exactly one current handoff. Read under the write lock:
                // read before it, another session's handoff in between gets this
                // one refused, or leaves two active on an empty lane.
                let prior: Vec<String> = w.heads(Some("handoff"), Some(lane))?.into_iter().filter(|h| h.status == "active").map(|h| h.key).collect();
                let r = if prior.is_empty() { w.create(&key, "handoff", "", lane, &d, place)? } else { w.supersede(&prior, &key, "handoff", "", lane, &d, place)? };
                let mut all = refs.clone();
                all.push(("lane".into(), lane.to_string()));
                w.add_refs(&key, &all)?;
                r
            }
            _ => {
                let old = if name == "supersede" { strs(a, "old")? } else { Vec::new() };
                if name == "supersede" && old.is_empty() {
                    return Err(Error::InvalidArgument("old is required".into()));
                }
                let first_old = match old.first() {
                    Some(o) => {
                        let h: Option<crate::memory::Head> = rusqlite::OptionalExtension::optional(w_head(&w, o))?;
                        Some(h.ok_or_else(|| Error::NotFound(format!("record {o:?}")))?)
                    }
                    None => None,
                };
                let kind = opt_str(a, "kind")?.map(str::to_string).or_else(|| first_old.as_ref().map(|h| h.kind.clone())).unwrap_or_default();
                if !["memory", "rule", "decision"].contains(&kind.as_str()) {
                    return Err(Error::InvalidArgument(format!("kind {kind:?}; use memory, rule or decision")));
                }
                let body = arg_nonblank(a, "body")?.to_string();
                // A record in the user layer: its scope says so, or (with no
                // scope given) the record it revises or replaces is in it.
                let named_key = opt_str(a, "key")?.map(str::to_string);
                let in_user = |w: &crate::memory::Writer<'_>, k: &str| -> Result<bool> {
                    let h: Option<crate::memory::Head> = rusqlite::OptionalExtension::optional(w_head(w, k))?;
                    Ok(h.is_some_and(|h| h.scope == USER) || store_exports_of(w, k)?.iter().any(|e| e == USER))
                };
                let scope = match opt_str(a, "scope")? {
                    Some(s) => s.to_string(),
                    None => {
                        let inherited = match (&first_old, &named_key) {
                            (Some(h), _) if in_user(&w, &h.key)? => Some(USER.to_string()),
                            (Some(h), _) => Some(h.scope.clone()),
                            (None, Some(k)) if kind == "memory" && in_user(&w, k)? => Some(USER.to_string()),
                            _ => None,
                        };
                        inherited.unwrap_or_default()
                    }
                };
                let user_layer = scope == USER;
                if user_layer {
                    if kind != "memory" {
                        return Err(Error::InvalidArgument("scope user is for memories: the user layer is a memory folder".into()));
                    }
                    if settings.user_memory.is_none() || !settings.exports.iter().any(|e| e.name == USER) {
                        return Err(Error::InvalidArgument("scope user writes the user layer, and none is configured: set user.memory in the atlas configuration".into()));
                    }
                }
                let sub = opt_str(a, "sub")?.map(str::to_string).or_else(|| first_old.as_ref().map(|h| h.sub.clone())).unwrap_or_default();
                // Where it goes: an explicit export, else the user layer for
                // scope user, else next to the record it replaces, else the
                // kind's export (a project's own, never the user layer's).
                let export_name: Option<String> = match opt_str(a, "export")? {
                    Some(e) => Some(e.to_string()),
                    None if user_layer => Some(USER.to_string()),
                    None => match &first_old {
                        Some(h) => store_exports_of(&w, &h.key)?.into_iter().next(),
                        None => match kind.as_str() {
                            "memory" => settings.exports.iter().find(|e| e.shape == Shape::MemoryDir && e.name != USER).map(|e| e.name.clone()),
                            "rule" => settings.exports.iter().find(|e| e.shape == Shape::Items).map(|e| e.name.clone()),
                            _ => None,
                        },
                    },
                };
                // A record replaced from outside the user layer has no place
                // in it to follow: the new one goes at the end.
                let old_in_export = match (&first_old, &export_name) {
                    (Some(h), Some(e)) => store_exports_of(&w, &h.key)?.iter().any(|x| x == e),
                    _ => false,
                };
                let after = opt_str(a, "after")?.map(str::to_string).or_else(|| first_old.as_ref().filter(|_| !user_layer || old_in_export).map(|h| h.key.clone()));
                let place = export_name.as_deref().map(|e| (e, after.clone().map_or(Place::End, Place::After)));
                let (key, d) = match kind.as_str() {
                    "memory" => {
                        let key = match opt_str(a, "key")? {
                            Some(k) => k.to_string(),
                            None => return Err(Error::InvalidArgument("a memory needs a key (its file name)".into())),
                        };
                        check_slug(&key)?;
                        let title = opt_str(a, "title")?.map(|t| given_line(t, "a memory's title", TITLE_CHARS)).transpose()?.unwrap_or_else(|| key.clone());
                        let summary = opt_str(a, "summary")?.map(|t| given_line(t, "a memory's summary", SUMMARY_CHARS)).transpose()?.unwrap_or_default();
                        let meta = format!("---\nname: {key}\ndescription: {summary}\nmetadata:\n  type: {sub}\n---\n");
                        let line = if summary.is_empty() { format!("- [{title}]({key}.md)") } else { format!("- [{title}]({key}.md) - {summary}") };
                        let body = if body.starts_with('\n') { with_newline(body) } else { format!("\n{}", with_newline(body)) };
                        let mut d = draft(line, meta, body);
                        if user_layer {
                            // The file the user layer folder will hold: the
                            // sync in every project reads it as this source's.
                            let file = format!("{}{}", d.meta, d.body);
                            d.provenance = Some(Provenance { src: USER.into(), path: format!("{key}.md"), line: 1, sha: crate::sha256::hex(file.as_bytes()) });
                        }
                        (key, d)
                    }
                    "rule" => {
                        let prefix = format!("{}:r", export_name.clone().unwrap_or_else(|| "rule".into()));
                        let key = match opt_str(a, "key")? {
                            Some(k) => k.to_string(),
                            None => w.next_key(&prefix, 5)?,
                        };
                        let title = match opt_str(a, "title")? {
                            Some(t) => given_line(t, "a rule's title", TITLE_CHARS)?,
                            None => short_title(body.trim_start_matches('-').trim()),
                        };
                        (key, draft(title, String::new(), rule_body(&body, &author)?))
                    }
                    _ => {
                        let key = match opt_str(a, "key")? {
                            Some(k) => k.to_string(),
                            None => w.next_key("decision:", 4)?,
                        };
                        // A long given title stays whole in the body, as a
                        // first line does.
                        let (title, body) = match opt_str(a, "title")? {
                            Some(t) => title_and_body(t, &body),
                            None => (short_title(&body), body.to_string()),
                        };
                        (key, draft(title, String::new(), with_newline(body)))
                    }
                };
                let exists = rusqlite::OptionalExtension::optional(w_head(&w, &key))?.is_some();
                if user_layer && !exists {
                    // Never over a file the user layer holds that this
                    // project has not read (a hand-written memory).
                    let f = settings.user_memory.as_ref().map(|u| u.join(format!("{key}.md")));
                    if let Some(f) = f.filter(|f| f.exists()) {
                        return Err(Error::AlreadyExists(format!("{} is in the user layer already; recall {key} after the next sync, then revise it", f.display())));
                    }
                }
                if user_layer {
                    user_write = Some((key.clone(), old.clone()));
                }
                let r = if name == "supersede" {
                    w.supersede(&old, &key, &kind, &sub, &scope, &d, place)?
                } else if exists {
                    let sha = opt_str(a, "expected_sha")?.ok_or_else(|| Error::InvalidArgument(format!("{key} exists: pass expected_sha (recall {{key}}) to add a version, or use supersede")))?;
                    w.revise(&key, Some(sha), &d)?
                } else {
                    w.create(&key, &kind, &sub, &scope, &d, place)?
                };
                w.add_refs(&key, &refs)?;
                r
            }
        };
        w.commit()?;
        let mut out = format!("ok {} v{} sha={}{}\n", written.key, written.seq, &written.sha[..16], if written.changed { "" } else { " (unchanged)" });
        if let (Some((key, old)), Some(dir)) = (user_write, settings.user_memory.as_ref()) {
            let index = settings.exports.iter().find(|e| e.name == USER).map_or("MEMORY.md", |e| e.index.as_str());
            let store = self.mem()?.store.as_ref().ok_or_else(|| Error::Db("store".into()))?;
            out.push_str(&write_user_layer(store, dir, index, &key, &old)?);
        }
        out.push_str(&self.after_write(&written.key)?);
        Ok(out)
    }

    /// Regenerates the exports a record is in (and the memory export for a
    /// memory), then snapshots when one is due. Returns lines to report.
    fn after_write(&mut self, key: &str) -> Result<String> {
        let m = self.mem()?;
        let settings = m.settings.clone();
        let store = m.store.as_ref().ok_or_else(|| Error::Db("store".into()))?;
        let mut out = String::new();
        if let Some(dir) = &settings.export_dir {
            let mut names: BTreeSet<String> = store.exports_of(key)?.into_iter().collect();
            if store.head(key)?.is_some_and(|h| h.kind == "memory") {
                if let Some(e) = settings.memory_export() {
                    names.insert(e.name.clone());
                }
            }
            // The exports are rendered and written under the store's write
            // lock: no other session commits between the render and the
            // file, so the export written last always shows the newest
            // commit (without it, two sessions could leave a stale file).
            // The write is committed already, so a failure here is reported
            // in the answer, not as an error.
            let locked = store.conn().execute_batch("BEGIN IMMEDIATE");
            // Checked where the transaction opens: `export::write` runs
            // inside this one, so a store a newer atlas upgraded since this
            // session opened it is refused here, before any file.
            let refused = locked.as_ref().ok().and_then(|()| crate::memory::check_locked(store.conn(), store.path()).err()).map(|e| e.to_string());
            for spec in settings.exports.iter().filter(|e| names.contains(&e.name)) {
                let written = match &refused {
                    Some(e) => Err(e.clone()),
                    None => export::write(store, spec, dir, false).map_err(|e| e.to_string()),
                };
                match written {
                    Ok(rep) => {
                        for (p, why) in rep.refused {
                            out.push_str(&format!("export refused: {}: {why}\n", p.display()));
                        }
                        if !rep.written.is_empty() {
                            out.push_str(&format!("exported {}: {} files\n", spec.name, rep.written.len()));
                        }
                    }
                    Err(e) => out.push_str(&format!("export {} not written: {e} (--memory-check reports the gap)\n", spec.name)),
                }
            }
            if locked.is_ok() {
                let _ = store.conn().execute_batch("COMMIT");
            }
        }
        if settings.snapshot_secs > 0 {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let last: u64 = store.meta_get("last_snapshot_epoch").ok().flatten().and_then(|s| s.parse().ok()).unwrap_or(0);
            if now.saturating_sub(last) >= settings.snapshot_secs {
                // The write is committed: a snapshot that cannot be made now
                // is reported, never turned into an error, and the next write
                // tries again.
                match crate::memory::check::snapshot(store, &settings.snapshot_dir, settings.snapshot_cap) {
                    Ok(Some(s)) => out.push_str(&format!("snapshot {} ({} records)\n", s.path.display(), s.records)),
                    Ok(None) => out.push_str("snapshot skipped: another session was writing (the next write tries again)\n"),
                    Err(e) => out.push_str(&format!("snapshot not taken: {e} (the next write tries again)\n")),
                }
            }
        }
        Ok(out)
    }
}

/// Writes a user-layer memory into the user layer's folder, where every
/// project's sync reads it: its file, the files of the records it replaced
/// (marked superseded, as an export shows them), and its index line, in the
/// place of the first replaced record's line or at the end. Other lines are
/// kept byte for byte. Returns a line to report.
///
/// # Errors
///
/// [`Error::Io`] writing a file; [`Error::Db`].
fn write_user_layer(store: &MemoryStore, dir: &std::path::Path, index: &str, key: &str, old: &[String]) -> Result<String> {
    let head = store.head(key)?.ok_or_else(|| Error::NotFound(format!("record {key:?}")))?;
    crate::fsio::replace_atomic(&dir.join(format!("{key}.md")), export::memory_file(&head).as_bytes())?;
    for o in old {
        let f = dir.join(format!("{o}.md"));
        if let (true, Some(h)) = (f.is_file(), store.head(o)?) {
            crate::fsio::replace_atomic(&f, export::memory_file(&h).as_bytes())?;
        }
    }
    let ipath = dir.join(index);
    let text = std::fs::read_to_string(&ipath).unwrap_or_default();
    let names = |l: &str, k: &str| l.contains(&format!("({k}.md)"));
    let mut lines: Vec<String> = Vec::new();
    let mut placed = false;
    for l in text.split_inclusive('\n') {
        let bare = l.trim_end_matches(['\n', '\r']);
        if names(bare, key) || old.iter().any(|o| names(bare, o)) {
            if !placed {
                lines.push(format!("{}\n", head.title));
                placed = true;
            }
            continue;
        }
        lines.push(l.to_string());
    }
    if !placed {
        if lines.last().is_some_and(|l| !l.ends_with('\n')) {
            lines.push("\n".into());
        }
        lines.push(format!("{}\n", head.title));
    }
    let new = lines.concat();
    if new != text {
        crate::fsio::replace_atomic(&ipath, new.as_bytes())?;
    }
    Ok(format!("user layer: {}\n", dir.join(format!("{key}.md")).display()))
}

fn w_head(w: &crate::memory::Writer<'_>, key: &str) -> rusqlite::Result<crate::memory::Head> {
    w.head(key)
}

fn store_exports_of(w: &crate::memory::Writer<'_>, key: &str) -> Result<Vec<String>> {
    w.exports_of(key)
}
