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

use super::{arg_str, opt_bool, opt_str, opt_uint, Answer, Server};

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

fn one_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().chars().take(200).collect()
}

fn check_slug(k: &str) -> Result<()> {
    let ok = !k.is_empty() && k.len() <= 96 && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !k.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!("memory key {k:?}: use lowercase letters, digits and hyphens")))
    }
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

    pub(super) fn run_memory(&mut self, name: &str, a: &Map) -> Result<Answer> {
        match name {
            "hook" => {
                let ev = HookEvent::from_value(&Value::Object(a.clone()));
                let m = self.mem()?;
                if m.hooks.is_none() {
                    m.hooks = Some(Hooks::open(m.settings.clone())?);
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
        let m = self.mem()?;
        let bpt = m.settings.bytes_per_token;
        let lane_budget = m.settings.recall.lane_budget_tokens;
        let store = m.store.as_ref().ok_or_else(|| Error::Db("store".into()))?;
        let mut out = String::new();
        if let Some(k) = opt_str(a, "key")? {
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
        let terms = recall::terms(q);
        let hits = store.search(&terms, opt_str(a, "kind")?, opt_str(a, "scope")?, limit)?;
        if hits.is_empty() {
            return Ok("no records match; grep the exports and report_miss what grep finds\n".into());
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
        let (mut more, mut headed) = (0, false);
        for (i, (h, score, _)) in hits.iter().enumerate() {
            if i < LEAD_BODIES && *score >= top * LEAD_RATIO {
                let mut one = String::new();
                recall::render(&mut one, h);
                if out.len() + one.len() <= budget || out.is_empty() {
                    out.push_str(&one);
                    continue;
                }
            }
            if !headed {
                out.push_str(QUERY_HEAD);
                headed = true;
            }
            let one = format!("- {} {score:.1}: {}\n", h.key, crate::hooks::pointer_title(h));
            if out.len() + one.len() > budget {
                more += 1;
                continue;
            }
            out.push_str(&one);
        }
        if more > 0 {
            out.push_str(&format!("+{more} more over budget_tokens\n"));
        }
        Ok(out)
    }

    fn journal_status(&mut self, a: &Map) -> Result<String> {
        let covered = self.repo_coverage(a)?;
        let m = self.mem()?;
        let store = m.store.as_ref().ok_or_else(|| Error::Db("store".into()))?;
        let mut out = String::new();
        let mut t = Table::new(&mut out, &["lane", "entries", "last", "last_commit", "open_questions", "handoffs"]);
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
            t.field(&l.lane).num(l.entries).field(l.last.get(..16).unwrap_or(&l.last)).field(&l.last_commit).num(l.open_questions).num(l.handoffs);
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
            t.field(v.time.get(..19).unwrap_or(&v.time)).field(&v.key).field(&v.kind).num(v.seq).field(&v.author).field(&one_line(&v.title)).field(&v.reason);
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
        // A new handoff supersedes the lane's active one, so a resume finds
        // exactly one current handoff.
        let prior: Vec<String> = match (name, opt_str(a, "lane")?) {
            ("handoff", Some(lane)) => store.heads(Some("handoff"), Some(lane))?.into_iter().filter(|h| h.status == "active").map(|h| h.key).collect(),
            _ => Vec::new(),
        };
        let mut w = store.writer()?;
        let draft = |title: String, meta: String, body: String| Draft { title, meta, body, reason: reason.clone(), author: author.clone(), client: client.clone(), provenance: None, time: None };
        // A memory written to the user layer, and the keys it replaces: its
        // file goes to the user layer folder once the store has it.
        let mut user_write: Option<(String, Vec<String>)> = None;
        let written = match name {
            "journal" => {
                let lane = arg_str(a, "lane")?;
                let entry = arg_str(a, "entry")?;
                if !ENTRIES.contains(&entry) {
                    return Err(Error::InvalidArgument(format!("entry {entry:?}; use one of {}", ENTRIES.join(", "))));
                }
                let text = arg_str(a, "text")?;
                let (head, rest) = text.split_once('\n').unwrap_or((text, ""));
                let key = w.next_key(&format!("journal:{lane}:"), 4)?;
                let meta = opt_str(a, "meta")?.map(|s| with_newline(s.to_string())).unwrap_or_default();
                let r = w.create(&key, "journal", entry, lane, &draft(one_line(head), meta, rest.to_string()), None)?;
                let mut all = refs.clone();
                all.push(("lane".into(), lane.to_string()));
                w.add_refs(&key, &all)?;
                r
            }
            "handoff" => {
                let lane = arg_str(a, "lane")?;
                let key = w.next_key(&format!("handoff:{lane}:"), 3)?;
                let place = settings.handoff_export().map(|e| (e.name.as_str(), Place::End));
                let d = draft(one_line(arg_str(a, "title")?), String::new(), with_newline(arg_str(a, "text")?.to_string()));
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
                let body = arg_str(a, "body")?.to_string();
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
                        let title = opt_str(a, "title")?.map(one_line).unwrap_or_else(|| key.clone());
                        let summary = opt_str(a, "summary")?.map(one_line).unwrap_or_default();
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
                        let title = opt_str(a, "title")?.map(one_line).unwrap_or_else(|| one_line(body.trim_start_matches('-').trim()));
                        (key, draft(title, String::new(), with_newline(body)))
                    }
                    _ => {
                        let key = match opt_str(a, "key")? {
                            Some(k) => k.to_string(),
                            None => w.next_key("decision:", 4)?,
                        };
                        let title = opt_str(a, "title")?.map(one_line).unwrap_or_else(|| one_line(&body));
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
            for spec in settings.exports.iter().filter(|e| names.contains(&e.name)) {
                let rep = export::write(store, spec, dir, false)?;
                for (p, why) in rep.refused {
                    out.push_str(&format!("export refused: {}: {why}\n", p.display()));
                }
                if !rep.written.is_empty() {
                    out.push_str(&format!("exported {}: {} files\n", spec.name, rep.written.len()));
                }
            }
        }
        if settings.snapshot_secs > 0 {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let last: u64 = store.meta_get("last_snapshot_epoch")?.and_then(|s| s.parse().ok()).unwrap_or(0);
            if now.saturating_sub(last) >= settings.snapshot_secs {
                let s = crate::memory::check::snapshot(store, &settings.snapshot_dir, settings.snapshot_cap)?;
                out.push_str(&format!("snapshot {} ({} records)\n", s.path.display(), s.records));
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
