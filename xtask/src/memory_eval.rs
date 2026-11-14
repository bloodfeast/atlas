//! `cargo xtask memory-eval`: the memory fidelity eval.
//!
//! atlas's first job is project memory, history and the work queue for
//! agents; every token cut is measured against it. This runs atlas over a
//! fixed copy of a store and its sources and scores three things:
//!
//! * (a) `--recall-check`'s misses (identifier, phrase and grammar), which
//!   must stay 0;
//! * (b) a question set answered from atlas's tool output alone, under an
//!   explicit agent policy ([`Policy`]): each question names the agent's
//!   first call, and at most one follow-up is made, chosen by rule from what
//!   the first call returned. A question passes when every required fact
//!   appears in what the calls returned. Tokens are the bytes returned over
//!   the configured bytes per token;
//! * (c) the work queue: `journal_status`, the lane view (`recall lane`),
//!   `decision_queue`, `awaiting` and `next_id` against counts taken from
//!   the database and the files directly.
//!
//! * (d) repeats: every question's first call made again by the same agent
//!   (announced by the PreToolUse hook), which may answer `unchanged since`
//!   or with the changed lines; read with the earlier answer, it must find
//!   the facts the first answer did.
//!
//! * (e) anchors: the question set again with `recall.pointer_anchors` on,
//!   the follow-up `recall {key}` as before, then `read_note` of a window of
//!   lines from the picked pointer's anchor instead: tokens and facts each way.
//!
//! * (f) a subagent without hooks: with no agent identity the server compares
//!   repeats per process, so a subagent asking what its parent asked may be
//!   told `unchanged since call #N`; it must reach the facts by following
//!   the `fresh: true` the reply names.
//!
//! Every question of (b), (c) and (e) is asked by its own agent, named to the
//! server by a PreToolUse hook as Claude Code does, so no answer is a
//! reference to another question's.
//!
//! `--floor F` (repeatable) also replays every recall question through the
//! UserPromptSubmit hook with `recall.inject_floor` at F: the injected
//! pointers, then the same one follow-up. Each floor runs twice, context
//! selection (`recall.select`) off and then on, the second with a
//! `selection:` line (pointers trimmed and collapsed, tokens whole and
//! sent); with the first floor,
//!
//! * (h) lane briefs: the lane questions as prompts, so the hook injects the
//!   lane's journal; then the lane follow-up of (c)'s rule. A question passes
//!   when its facts are in the injection and that follow-up, and the row
//!   also counts those found in the injection alone.
//!
//! Each replay also prints a `records:` line: items injected (pointers,
//! lane views and repository lines) per prompt, tokens per prompt, prompts
//! with an item, and items off-topic by the lane rule of [`Label::off`],
//! a question taking its source record's lane when that is a journal or
//! handoff record. `--offtopic FILE` replays a second set beside it at every
//! floor, `label<TAB>prompt` lines ([`parse_offtopic`]): `neg` prompts,
//! for which every injected item is off-topic, and prompts labelled with
//! their lanes, for which an item of another lane is; with `--rows`, one
//! `offtopic row` per prompt names each item and its cosine.
//!
//! The question file is plain text, one block per question:
//!
//! ```text
//! Q001 ruling
//! ask: What frame rate does the game target?
//! source: record:frame-rate-target
//! call: recall {"query": "frame rate target"}
//! fact: 240
//! fact: 120 | one hundred and twenty
//! ```
//!
//! `source` is `record:KEY` (a memory record) or `file:NAME:PATH` (a vault
//! or source file). Every fact must occur in the source, or the question is
//! reported invalid and the run fails: a fact the source does not hold
//! tests nothing.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use atlas::json::Value;

/// The start (g) builds its checkpoint card from: the whole eval store, on
/// purpose. An explicit start, so not the bounded unknown-start window a
/// live hook uses (the snapshot's records are older than that window).
const EVAL_CARD_SINCE: &str = "0";

/// One question of the set.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    /// `Q001`.
    pub id: String,
    /// ruling, reversal, who-when, lane, id, mention, section.
    pub kind: String,
    /// The question in words (the hook replay's prompt).
    pub ask: String,
    /// `record:KEY` or `file:NAME:PATH`.
    pub source: String,
    /// The first call's tool.
    pub tool: String,
    /// The first call's arguments.
    pub args: Value,
    /// Required facts; each is a set of accepted spellings.
    pub facts: Vec<Vec<String>>,
    /// Whether a `file:` source is a vault (read with `read_note`) rather
    /// than a source (read by `section`); set by the run.
    pub in_vault: bool,
    /// For a `file:` source, the store records imported or synced from that
    /// file (a version's provenance names it); set by the run.
    pub source_records: HashSet<String>,
}

/// Parses a question file.
///
/// # Errors
///
/// A message naming the line, for a field outside a block, an unknown
/// field, bad JSON, or a block missing `ask`, `source`, `call` or `fact`.
pub fn parse(text: &str) -> Result<Vec<Question>, String> {
    let mut out: Vec<Question> = Vec::new();
    let mut cur: Option<Question> = None;
    let finish = |q: Option<Question>, out: &mut Vec<Question>| -> Result<(), String> {
        if let Some(q) = q {
            if q.ask.is_empty() || q.source.is_empty() || q.tool.is_empty() || q.facts.is_empty() {
                return Err(format!("{}: needs ask, source, call and at least one fact", q.id));
            }
            out.push(q);
        }
        Ok(())
    };
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('Q') && !line.contains(':') {
            finish(cur.take(), &mut out)?;
            let mut it = line.split_whitespace();
            let id = it.next().unwrap_or("").to_string();
            let kind = it.next().ok_or_else(|| format!("line {}: {id} needs a kind", n + 1))?.to_string();
            cur = Some(Question {
                id,
                kind,
                ask: String::new(),
                source: String::new(),
                tool: String::new(),
                args: Value::Null,
                facts: Vec::new(),
                in_vault: false,
                source_records: HashSet::new(),
            });
            continue;
        }
        let q = cur.as_mut().ok_or_else(|| format!("line {}: a field outside a question", n + 1))?;
        let (field, val) = line.split_once(':').ok_or_else(|| format!("line {}: expected field: value", n + 1))?;
        let val = val.trim();
        match field.trim() {
            "ask" => q.ask = val.to_string(),
            "source" => q.source = val.to_string(),
            "call" => {
                let (tool, args) = val.split_once(' ').unwrap_or((val, "{}"));
                q.tool = tool.to_string();
                q.args = atlas::json::parse(args.trim()).map_err(|e| format!("line {}: call arguments: {e}", n + 1))?;
            }
            "fact" => q.facts.push(val.split(" | ").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()),
            f => return Err(format!("line {}: unknown field {f:?}", n + 1)),
        }
    }
    finish(cur, &mut out)?;
    let mut seen = HashSet::new();
    for q in &out {
        if !seen.insert(q.id.clone()) {
            return Err(format!("{}: duplicate id", q.id));
        }
    }
    Ok(out)
}

/// Text as facts are matched: lowercase, `*` and backticks dropped, every
/// run of whitespace one space.
pub fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c == '*' || c == '`' {
            continue;
        }
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// Whether a normalized haystack holds a fact (any of its spellings).
pub fn holds(hay: &str, fact: &[String]) -> bool {
    fact.iter().any(|f| hay.contains(&norm(f)))
}

/// Embeds every record of `settings.store` that has no vector for the
/// configured model, in the foreground, so a [`atlas::hooks::Hooks`] opened
/// afterwards finds nothing pending and starts no background embedder
/// (`Hooks::load_model` spawns one only when `pending` is non-empty, and
/// `refresh_vectors` only when `RecVecs::missing` is above 0; both read the
/// same recallable active heads).
///
/// # Returns
///
/// `None` when there is no model file (the hooks' gate is then lexical and
/// they spawn nothing), else how many records were embedded.
///
/// # Errors
///
/// The model or the store failing to load, or a write failing.
pub fn embed_all_pending(settings: &atlas::memory::settings::Settings) -> Result<Option<usize>, String> {
    if !settings.model.is_file() {
        return Ok(None);
    }
    let model = atlas::embed::Model::load(&settings.model).map_err(|e| e.to_string())?;
    let mem = atlas::memory::MemoryStore::open(&settings.store).map_err(|e| e.to_string())?;
    let mut n = 0;
    loop {
        let k = atlas::memory::vectors::embed_pending(&mem, &model, 128, settings.embed_threads).map_err(|e| e.to_string())?;
        if k == 0 {
            return Ok(Some(n));
        }
        n += k;
    }
}

/// [`embed_all_pending`] through a vector cache shared across stores:
/// before embedding, every vector the cache holds for a current version
/// (by the model and the version's content hash) is copied into the store;
/// after it, the store's vectors go back into the cache. A vector is a
/// function of the model and the text the hash covers, so a hit is what
/// embedding would write; only identical records skip the encoder.
///
/// # Errors
///
/// The store, the cache or the encoder failing, as text.
pub fn embed_all_pending_cached(settings: &atlas::memory::settings::Settings, cache: &Path) -> Result<Option<usize>, String> {
    if !settings.model.is_file() {
        return Ok(None);
    }
    let model = atlas::embed::Model::load(&settings.model).map_err(|e| e.to_string())?;
    let mem = atlas::memory::MemoryStore::open(&settings.store).map_err(|e| e.to_string())?;
    let err = |e: rusqlite::Error| format!("vector cache {}: {e}", cache.display());
    atlas::memory::vectors::ensure_schema(&mem).map_err(|e| e.to_string())?;
    mem.conn().execute("ATTACH DATABASE ?1 AS vc", [cache.to_string_lossy()]).map_err(err)?;
    mem.conn().execute_batch(&atlas::memory::vectors::SCHEMA.replace("EXISTS rec_vec", "EXISTS vc.rec_vec")).map_err(err)?;
    // The hits: the versions `embed_pending` would read.
    mem.conn()
        .execute(
            "INSERT OR IGNORE INTO main.rec_vec(model, sha, vec) SELECT model, sha, vec FROM vc.rec_vec WHERE model = ?1 AND sha IN
             (SELECT v.sha FROM records r JOIN versions v ON v.vid = r.head WHERE r.status = 'active' AND r.kind NOT IN ('block', 'journal'))",
            [&model.id],
        )
        .map_err(err)?;
    let mut n = 0;
    loop {
        let k = atlas::memory::vectors::embed_pending(&mem, &model, 128, settings.embed_threads).map_err(|e| e.to_string())?;
        if k == 0 {
            break;
        }
        n += k;
    }
    mem.conn().execute("INSERT OR IGNORE INTO vc.rec_vec(model, sha, vec) SELECT model, sha, vec FROM main.rec_vec WHERE model = ?1", [&model.id]).map_err(err)?;
    mem.conn().execute_batch("DETACH DATABASE vc").map_err(err)?;
    Ok(Some(n))
}

/// Record keys an answer lists, in order: `== KEY ...` (a record) and
/// `- KEY: title` or `- KEY score: title` (a pointer).
pub fn keys_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for l in text.lines() {
        let rest = match (l.strip_prefix("== "), l.strip_prefix("- ")) {
            (Some(r), _) | (None, Some(r)) => r,
            _ => continue,
        };
        let k = rest.split_whitespace().next().unwrap_or("").trim_end_matches(':');
        // A key has no spaces and is not a bare word of prose: it holds a
        // colon or a dash, or is a memory slug.
        if !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'))
            && (k.contains(':') || k.contains('-'))
        {
            out.push(k.to_string());
        }
    }
    out
}

/// How the one follow-up is chosen for a recall query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// The agent reads the listed record it judges relevant, modelled as
    /// the source record when the first answer lists its key.
    Informed,
    /// The agent reads the first record listed.
    Strict,
}

/// One question's result.
#[derive(Debug, Clone, Default)]
pub struct Scored {
    /// All facts found.
    pub pass: bool,
    /// Facts found.
    pub found: usize,
    /// Calls made (1 or 2).
    pub calls: usize,
    /// Bytes returned over the calls made.
    pub bytes: usize,
    /// The facts missing, first spelling each.
    pub missing: Vec<String>,
    /// Whether the first answer listed the source record or file.
    pub surfaced: bool,
}

/// Runs a tool call and returns its text (an error is its message: an
/// agent reads that too).
pub type Call<'a> = dyn FnMut(&str, &Value) -> String + 'a;

fn score_text(q: &Question, hay: &str) -> (usize, Vec<String>) {
    let h = norm(hay);
    let mut missing = Vec::new();
    let mut found = 0;
    for f in &q.facts {
        if holds(&h, f) {
            found += 1;
        } else {
            missing.push(f[0].clone());
        }
    }
    (found, missing)
}

fn source_key(q: &Question) -> Option<&str> {
    q.source.strip_prefix("record:")
}

/// The source file of a `file:` question: (name, path).
fn source_file(q: &Question) -> Option<(&str, &str)> {
    q.source.strip_prefix("file:").and_then(|r| r.split_once(':'))
}

/// Whether an answer names the question's source.
fn names_source(q: &Question, text: &str) -> bool {
    match (source_key(q), source_file(q)) {
        (Some(k), _) => keys_in(text).iter().any(|x| x == k),
        (None, Some((n, p))) => text.contains(&format!("{n}:{p}")),
        _ => false,
    }
}

/// The anchor a pointer line for `key` ends with: (source, path, line).
pub fn pointer_anchor(first: &str, key: &str) -> Option<(String, String, usize)> {
    let l = first.lines().find(|l| l.strip_prefix("- ").and_then(|r| r.split_whitespace().next()) == Some(key))?;
    let a = l.rsplit_once(" @")?.1.trim();
    let (src, rest) = a.split_once(':')?;
    let (path, line) = rest.rsplit_once(':')?;
    Some((src.to_string(), path.to_string(), line.parse().ok()?))
}

/// The follow-up an agent makes after `first`, by the policy; None when
/// the answer gives it nothing to follow. With `window`, a recall pointer
/// that carries an anchor is followed by `read_note` of that many lines from
/// the anchor, rather than `recall {key}`.
pub fn follow_up_with(q: &Question, first: &str, policy: Policy, window: Option<usize>) -> Option<(String, Value)> {
    let get = |k: &str| q.args.get(k).and_then(Value::as_str).map(str::to_string);
    match q.tool.as_str() {
        "recall" if get("query").is_some() => {
            let keys = keys_in(first);
            let pick = match policy {
                // The source record when listed; for a file source, the
                // first listed record that file's text was imported into.
                Policy::Informed => match source_key(q) {
                    Some(k) => keys.iter().any(|x| x == k).then(|| k.to_string()),
                    None => keys.iter().find(|x| q.source_records.contains(*x)).cloned(),
                },
                Policy::Strict => keys.first().cloned(),
            }?;
            if let Some((src, path, line)) = window.and_then(|_| pointer_anchor(first, &pick)) {
                let w = window.unwrap_or(1).max(1);
                return Some(("read_note".into(), atlas::json!({"vault": src, "path": path, "lines": format!("{line}-{}", line + w - 1)})));
            }
            Some(("recall".into(), atlas::json!({"key": pick})))
        }
        "recall" => {
            let lane = get("lane")?;
            first
                .lines()
                .any(|l| l.starts_with('+') && l.contains("more"))
                .then(|| ("recall".into(), atlas::json!({"lane": lane, "budget_tokens": 100000})))
        }
        "lookup" => {
            let id = get("id")?;
            if first.lines().any(|l| l.starts_with('+') && l.contains("more")) {
                Some(("lookup".into(), atlas::json!({"id": id, "limit": 500})))
            } else {
                Some(("lookup".into(), atlas::json!({"id": id, "full": true})))
            }
        }
        "search" => {
            // The first row from the source file: read that note, or that
            // source section.
            let (name, path) = source_file(q)?;
            let row = first.lines().find(|l| l.starts_with(&format!("{name}:{path}:")))?;
            let heading = row.split('\t').nth(1).unwrap_or("").rsplit('>').next().unwrap_or("").trim().to_string();
            if q.in_vault || heading.is_empty() {
                Some(("read_note".into(), atlas::json!({"vault": name, "path": path})))
            } else {
                Some(("section".into(), atlas::json!({"heading": heading, "source": name, "path": path})))
            }
        }
        _ => None,
    }
}

/// Scores one question: the first call, then at most one follow-up when a
/// fact is missing.
pub fn score(q: &Question, policy: Policy, call: &mut Call<'_>) -> Scored {
    let first = call(&q.tool, &q.args);
    score_after(q, &first, policy, call)
}

/// As [`score`], with the first answer already in hand (the hook replay).
pub fn score_after(q: &Question, first: &str, policy: Policy, call: &mut Call<'_>) -> Scored {
    score_after_with(q, first, policy, None, call)
}

/// As [`score_after`], the follow-up chosen by [`follow_up_with`].
pub fn score_after_with(q: &Question, first: &str, policy: Policy, window: Option<usize>, call: &mut Call<'_>) -> Scored {
    let mut s = Scored { calls: 1, bytes: first.len(), surfaced: names_source(q, first), ..Scored::default() };
    let (found, missing) = score_text(q, first);
    if missing.is_empty() {
        return Scored { pass: true, found, ..s };
    }
    let mut hay = first.to_string();
    // A hook injection is followed like a recall query: pointers, then a key.
    let fq = if q.tool == "hook" {
        Question { tool: "recall".into(), args: atlas::json!({"query": q.ask.clone()}), ..q.clone() }
    } else {
        q.clone()
    };
    if let Some((tool, args)) = follow_up_with(&fq, first, policy, window) {
        let second = call(&tool, &args);
        s.calls = 2;
        s.bytes += second.len();
        hay.push('\n');
        hay.push_str(&second);
    }
    let (found, missing) = score_text(q, &hay);
    Scored { pass: missing.is_empty(), found, missing, ..s }
}

/// Keys of the store records whose text came from a source file: any
/// version's provenance names the source (the export of the same name) and
/// the file.
fn records_from_file(mem: &atlas::memory::MemoryStore, name: &str, path: &str) -> Result<HashSet<String>, String> {
    let sql = "SELECT DISTINCT r.key FROM records r JOIN versions v ON v.rid = r.rid WHERE v.src = ?1 AND v.src_path = ?2";
    let mut st = mem.conn().prepare(sql).map_err(|e| e.to_string())?;
    let rows = st.query_map([name, path], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<HashSet<String>>>().map_err(|e| e.to_string())
}

/// The source text a question's facts must come from.
fn source_text(q: &Question, mem: &atlas::memory::MemoryStore, roots: &BTreeMap<String, PathBuf>) -> Result<String, String> {
    if let Some(k) = source_key(q) {
        let h = mem.head(k).map_err(|e| e.to_string())?.ok_or_else(|| format!("no record {k}"))?;
        return Ok(format!("{}\n{}", h.title, h.body));
    }
    if let Some((n, p)) = source_file(q) {
        let root = roots.get(n).ok_or_else(|| format!("no vault or source {n}"))?;
        return std::fs::read_to_string(root.join(p)).map_err(|e| format!("{n}:{p}: {e}"));
    }
    Err(format!("source {:?}: expected record:KEY or file:NAME:PATH", q.source))
}

/// Facts the source does not hold (the question tests nothing for them).
fn invalid_facts(q: &Question, src: &str) -> Vec<String> {
    let h = norm(src);
    q.facts.iter().filter(|f| !holds(&h, f)).map(|f| f.join(" | ")).collect()
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// A server over a configuration's index and memory store, as the MCP
/// server answers (no ledger, no code index, no watcher), with the index
/// brought up to date first.
///
/// # Errors
///
/// A message, when the configuration or a store cannot be opened.
pub fn server(c: &atlas::config::Config) -> Result<atlas::tools::Server, String> {
    let db = c.db.clone().ok_or("the configuration names no db")?;
    let vaults: Vec<(String, PathBuf)> = c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
    let mut setup = atlas::store::AtlasSetup::from_config(c).map_err(|e| e.to_string())?;
    setup.watch = false;
    let mut store =
        atlas::paths::Vaults::new(&vaults).and_then(|v| atlas::store::Store::open_with(&db, v, setup)).map_err(|e| e.to_string())?;
    store.rescan().map_err(|e| e.to_string())?;
    store.rescan_sources(None).map_err(|e| e.to_string())?;
    let mut server = atlas::tools::Server::new(store);
    server.set_memory(atlas::memory::settings::Settings::from_config(Some(c), &db).map_err(|e| e.to_string())?);
    Ok(server)
}

/// Options of one run.
pub struct Opts {
    /// The configuration (its db and memory store are the run's copies).
    pub config: PathBuf,
    /// The question file.
    pub questions: PathBuf,
    /// A folder whose files are copied over the db's folder first.
    pub fresh_from: Option<PathBuf>,
    /// Hook floors to replay.
    pub floors: Vec<f64>,
    /// One row per question.
    pub rows: bool,
    /// Questions whose calls and answers are printed whole, and their hook
    /// injections with the candidates that passed the gate.
    pub show: Vec<String>,
    /// The project to run as (default: the one the working directory and
    /// the configuration resolve to; see [`atlas::project`]).
    pub project: Option<String>,
    /// The folder the replayed session works in (default: the working
    /// directory): the project resolves from it, as the server's does, and
    /// every replayed hook event carries it as its `cwd`, so the active
    /// repository is the one a session started there would have.
    pub cwd: Option<PathBuf>,
    /// An off-topic prompt file (see [`parse_offtopic`]), replayed through
    /// the hook beside the recall questions at every floor.
    pub offtopic: Option<PathBuf>,
}

/// An off-topic prompt's label: `neg` (it should recall nothing) or the
/// lanes whose records are on its topic.
#[derive(Debug, Clone, PartialEq)]
pub enum Label {
    /// Every injected record is off-topic.
    Neg,
    /// A record of another lane is off-topic.
    Lanes(Vec<String>),
}

impl Label {
    fn name(&self) -> String {
        match self {
            Label::Neg => "neg".into(),
            Label::Lanes(l) => l.join("|"),
        }
    }

    /// Whether an injected item whose lanes are `lanes` (see [`item_lanes`])
    /// is off-topic: under `neg` every item is; under lanes, an item none of
    /// whose lanes is listed. An item with no lane is not; an item with one
    /// lane is when that lane is not listed; an ambiguous item (two or more
    /// lanes) is when no listed lane is among them.
    pub fn off(&self, lanes: &[String]) -> bool {
        match self {
            Label::Neg => true,
            Label::Lanes(l) => !lanes.is_empty() && !lanes.iter().any(|x| l.contains(x)),
        }
    }
}

/// Parses an off-topic prompt file: `label<TAB>prompt` lines (the
/// `--recall-scores` format), `#` comments and blank lines skipped. The
/// label is `neg` or lanes joined by `|`, each a lane of the store.
///
/// # Errors
///
/// A message naming the line, for a line without a tab or a lane the store
/// does not hold.
pub fn parse_offtopic(text: &str, known: &HashSet<String>) -> Result<Vec<(Label, String)>, String> {
    let mut out = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let (label, prompt) = line.split_once('\t').ok_or_else(|| format!("off-topic line {}: needs label<TAB>prompt", n + 1))?;
        let label = if label == "neg" {
            Label::Neg
        } else {
            let lanes: Vec<String> = label.split('|').map(str::to_string).collect();
            if let Some(l) = lanes.iter().find(|l| !known.contains(*l)) {
                return Err(format!("off-topic line {}: the store has no lane {l}", n + 1));
            }
            Label::Lanes(lanes)
        };
        out.push((label, prompt.trim().to_string()));
    }
    Ok(out)
}

/// The lane names a record can be labelled with: the store's lanes
/// without the `session:` scopes.
fn lane_names(mem: &atlas::memory::MemoryStore) -> Result<HashSet<String>, String> {
    Ok(mem.lanes().map_err(|e| e.to_string())?.into_iter().map(|l| l.lane).filter(|l| !l.starts_with("session:")).collect())
}

/// The names in `lanes` that equal a token of one of `texts`, sorted. A
/// token is a maximal run of `[A-Za-z0-9_-]`, matched case-sensitively, so
/// `QUARRY-FLOW-2` is not `QUARRY-FLOW`.
fn token_lanes(lanes: &HashSet<String>, texts: &[&str]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for t in texts {
        for tok in t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
            if !tok.is_empty() && lanes.contains(tok) {
                out.insert(tok.to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// A record's lanes by the widened rule (fixtures/offtopic/README.md): a
/// journal or handoff record's own lane; for an intent record (key
/// `intent:...`, kind rule or block) or a memory record (kind memory, a
/// bare key), the lane names that are tokens of its key or head title;
/// any other record has none.
fn record_lanes(lanes: &HashSet<String>, key: &str, kind: &str, scope: &str, title: &str) -> Vec<String> {
    if kind == "journal" || kind == "handoff" {
        return vec![scope.to_string()];
    }
    if is_intent_or_memory(key, kind) {
        token_lanes(lanes, &[key, title])
    } else {
        Vec::new()
    }
}

/// Whether a record is labelled by its lane tokens: an intent record (key
/// `intent:...`, kind rule or block) or a memory record (kind memory, a
/// bare key).
fn is_intent_or_memory(key: &str, kind: &str) -> bool {
    (key.starts_with("intent:") && (kind == "rule" || kind == "block")) || (kind == "memory" && !key.contains(':'))
}

/// The lanes of an injected item: `lane:X` and `lane-head:X` are lane X, a
/// record's are [`record_lanes`], anything else (a repository line) has
/// none.
fn item_lanes(mem: &atlas::memory::MemoryStore, lanes: &HashSet<String>, key: &str) -> Vec<String> {
    if let Some(l) = key.strip_prefix("lane:").or_else(|| key.strip_prefix("lane-head:")) {
        return vec![l.to_string()];
    }
    match mem.head(key).ok().flatten() {
        Some(h) => record_lanes(lanes, &h.key, &h.kind, &h.scope, &h.title),
        None => Vec::new(),
    }
}

/// A control question's label from its source record's lanes: one lane
/// labels it; none or two or more leave it unlabelled, the second counted
/// as ambiguous (the `bool`).
fn control_label(source: &[String]) -> (Option<Label>, bool) {
    match source.len() {
        1 => (Some(Label::Lanes(source.to_vec())), false),
        0 => (None, false),
        _ => (None, true),
    }
}

/// Injection counts over a set of hook prompts.
#[derive(Debug, Default)]
struct Tally {
    prompts: usize,
    /// Prompts with at least one injected item.
    any: usize,
    items: usize,
    bytes: usize,
    /// Prompts with a label (their items are labelled).
    labelled: usize,
    /// Labelled prompts with at least one off-topic item.
    any_off: usize,
    off: usize,
    /// Items with two or more lanes (labelled prompts only).
    amb: usize,
    /// Prompts left unlabelled because their source has two or more lanes
    /// (the control only).
    amb_src: usize,
}

impl Tally {
    /// Adds one prompt's injection; returns its row (items, off-topic
    /// items, bytes, then each item as `key[lane]`, with `*` when off-topic).
    fn add(
        &mut self,
        mem: &atlas::memory::MemoryStore,
        lanes: &HashSet<String>,
        label: Option<&Label>,
        injected: &[atlas::hooks::Injected],
        bytes: usize,
    ) -> String {
        self.prompts += 1;
        self.any += (!injected.is_empty()) as usize;
        self.items += injected.len();
        self.bytes += bytes;
        let (mut off, mut amb) = (0, 0);
        let mut keys = Vec::new();
        for k in injected {
            let item = item_lanes(mem, lanes, &k.key);
            let o = label.is_some_and(|l| l.off(&item));
            off += o as usize;
            amb += (item.len() > 1) as usize;
            let cos = k.cos.map_or("-".to_string(), |c| format!("{c:.3}"));
            let shown = if item.is_empty() { "-".to_string() } else { item.join("|") };
            keys.push(format!("{}{}[{shown}] cos {cos}", if o { "*" } else { "" }, k.key));
        }
        if label.is_some() {
            self.labelled += 1;
            self.any_off += (off > 0) as usize;
            self.off += off;
            self.amb += amb;
        }
        format!("{}\t{off}\t{bytes}\t{}", injected.len(), keys.join("; "))
    }

    fn line(&self, name: &str, bpt: f64) -> String {
        let n = self.prompts.max(1) as f64;
        format!(
            "{name}: {} prompts; items injected {} ({:.2} per prompt); tokens {:.0} ({:.1} per prompt); prompts with an item {}/{} ({:.1}%); labelled prompts {}, with an off-topic item {} ({:.1}%), off-topic items {} ({:.2} per labelled prompt); ambiguous items {}, ambiguous sources {}",
            self.prompts,
            self.items,
            self.items as f64 / n,
            self.bytes as f64 / bpt,
            self.bytes as f64 / bpt / n,
            self.any,
            self.prompts,
            100.0 * self.any as f64 / n,
            self.labelled,
            self.any_off,
            100.0 * self.any_off as f64 / self.labelled.max(1) as f64,
            self.off,
            self.off as f64 / self.labelled.max(1) as f64,
            self.amb,
            self.amb_src
        )
    }
}

fn copy_fresh(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    // A stale write-ahead log beside a fresh copy would be replayed into it.
    for e in std::fs::read_dir(to).map_err(|e| e.to_string())?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with(".jsonl") {
            std::fs::remove_file(e.path()).map_err(|e| e.to_string())?;
        }
    }
    for e in std::fs::read_dir(from).map_err(|e| e.to_string())?.flatten() {
        if e.path().is_file() {
            std::fs::copy(e.path(), to.join(e.file_name())).map_err(|e| format!("copy {}: {e}", e))?;
        }
    }
    Ok(())
}

/// Runs the eval and prints the report; Err when the question set is
/// invalid or a check fails to run.
pub fn run(o: &Opts) -> Result<bool, String> {
    let raw = atlas::config::Config::load(&o.config).map_err(|e| e.to_string())?;
    let cwd = o.cwd.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let resolved = atlas::project::resolve(&raw, &cwd, o.project.as_deref()).map_err(|e| e.to_string())?;
    if let Some(a) = &resolved.active {
        println!("project {} ({:?})", a.spec.name, a.why);
    }
    let graph = resolved.graph;
    if !graph.name.is_empty() {
        println!("graph: {} repos; cwd {}", graph.repos.len(), graph.repo_of(&cwd.to_string_lossy()).map_or("in no repo", |r| r.name.as_str()));
    }
    let c = resolved.config;
    let db = c.db.clone().ok_or("the configuration names no db")?;
    if let Some(from) = &o.fresh_from {
        copy_fresh(from, db.parent().ok_or("db has no folder")?)?;
    }
    let bpt = c.bytes_per_token.unwrap_or(atlas::tokens::DEFAULT_BYTES_PER_TOKEN);
    let vaults: Vec<(String, PathBuf)> = c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
    let mut roots: BTreeMap<String, PathBuf> = vaults.iter().cloned().collect();
    roots.extend(c.sources.iter().map(|s| (s.name.clone(), s.root.clone())));
    let mut setup = atlas::store::AtlasSetup::from_config(&c).map_err(|e| e.to_string())?;
    setup.watch = false;
    let open = || {
        atlas::paths::Vaults::new(&vaults).and_then(|v| atlas::store::Store::open_with(&db, v, setup.clone())).map_err(|e| e.to_string())
    };
    let mut settings = atlas::memory::settings::Settings::from_config(Some(&c), &db).map_err(|e| e.to_string())?;
    // The project's graph, as the server hands it to the tools and hooks.
    if !graph.name.is_empty() {
        settings.graph = Some(std::sync::Arc::new(graph));
    }
    let mut ok = true;

    // (a) recall-check.
    let mut store = open()?;
    store.set_memory_sync(Some(settings.clone()));
    store.rescan().map_err(|e| e.to_string())?;
    store.rescan_sources(None).map_err(|e| e.to_string())?;
    let r = atlas::recall::check(&store, &[]).map_err(|e| e.to_string())?;
    let grammar = atlas::grammar::recall_check(&store).map_err(|e| e.to_string())?;
    let source_roots: Vec<(String, PathBuf)> = store.sources().iter().map(|s| (s.name.clone(), s.root.clone())).collect();
    let targets = atlas::memory::sync::targets(&settings, &source_roots);
    let drift = {
        let mem = atlas::memory::MemoryStore::open(&settings.store).map_err(|e| e.to_string())?;
        atlas::memory::sync::drift(&mem, &targets).map_err(|e| e.to_string())?
    };
    println!(
        "(a) recall-check: {} documents, {} identifier occurrences; misses: {} identifier, {} phrase, {} grammar, {} memory drift",
        r.docs,
        r.id_occurrences,
        r.id_misses.len(),
        r.phrase_misses.len(),
        grammar.len(),
        drift.len()
    );
    for m in &r.id_misses {
        println!("    id miss ({}): {} at {}:{}:{}", m.stage, m.id, m.source, m.path, m.line);
    }
    for m in &r.phrase_misses {
        println!("    phrase miss: {:?} in {}:{}", m.phrase, m.source, m.path);
    }
    for g in &grammar {
        println!("    grammar: {g}");
    }
    for d in &drift {
        println!("    memory drift: {d}");
    }
    ok &= r.id_misses.is_empty() && r.phrase_misses.is_empty() && grammar.is_empty() && drift.is_empty();

    // (b) the question set.
    let text = std::fs::read_to_string(&o.questions).map_err(|e| format!("{}: {e}", o.questions.display()))?;
    let mut qs = parse(&text)?;
    for q in &mut qs {
        q.in_vault = source_file(q).is_some_and(|(n, _)| c.vaults.iter().any(|v| v.name == n));
    }
    let mem = atlas::memory::MemoryStore::open(&settings.store).map_err(|e| e.to_string())?;
    for q in &mut qs {
        if let Some((name, path)) = source_file(q) {
            q.source_records = records_from_file(&mem, name, path)?;
        }
    }
    let mut bad = 0;
    for q in &qs {
        match source_text(q, &mem, &roots) {
            Ok(src) => {
                let inv = invalid_facts(q, &src);
                if !inv.is_empty() {
                    println!("    INVALID {}: the source does not hold {}", q.id, inv.join("; "));
                    bad += 1;
                }
            }
            Err(e) => {
                println!("    INVALID {}: {e}", q.id);
                bad += 1;
            }
        }
    }
    if bad > 0 {
        return Err(format!("{bad} invalid questions"));
    }
    // With the index a tree: every question's memory file is reachable from
    // the root in at most 3 reads.
    if settings.tree.is_some() {
        let out = crate::memory_tree::check(&c, None)?;
        let mem_root = settings.tree.as_ref().and_then(|t| roots.get(&t.export)).cloned().unwrap_or_default();
        let (mut asked, mut reached) = (0, 0);
        for q in &qs {
            let stem = match (source_key(q), source_file(q)) {
                (Some(k), _) if mem_root.join(format!("{k}.md")).is_file() => k.to_string(),
                (_, Some((n, p))) if settings.tree.as_ref().is_some_and(|t| t.export == n) && !p.contains('/') => p.trim_end_matches(".md").to_string(),
                _ => continue,
            };
            asked += 1;
            match out.depth.get(&stem) {
                Some(&d) if d <= 3 => reached += 1,
                d => println!("    tree: {} source {stem} is {}", q.id, d.map_or("unreachable".to_string(), |d| format!("{d} reads deep"))),
            }
        }
        println!("(a) tree: {reached}/{asked} question memory sources reachable from the root in at most 3 reads; {} tree problems", out.problems.len());
        ok &= reached == asked && out.problems.is_empty();
    }
    let mut server = atlas::tools::Server::new(store);
    server.set_memory(settings.clone());
    // The agent asking: each question's own, announced by the PreToolUse
    // hook before every call (a repeat by one agent may be a reference).
    let agent = std::cell::RefCell::new(String::new());
    let mut call = |tool: &str, args: &Value| -> String {
        announce(&mut server, &agent.borrow(), tool);
        match server.answer(tool, args) {
            Ok(a) => a.into_text(),
            Err(e) => format!("error: {e}"),
        }
    };
    let mut kinds: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let (mut pass, mut strict, mut found, mut total, mut one) = (0, 0, 0, 0, 0);
    let (mut toks, mut calls) = (Vec::new(), Vec::new());
    let mut fails = Vec::new();
    let mut rows = Vec::new();
    for q in &qs {
        let shown = o.show.contains(&q.id);
        let mut traced = |t: &str, a: &Value| -> String {
            let out = call(t, a);
            if shown {
                println!(
                    "--- {} {t} {a}
{out}",
                    q.id
                );
            }
            out
        };
        *agent.borrow_mut() = format!("{}-informed", q.id);
        let s = score(q, Policy::Informed, &mut traced);
        *agent.borrow_mut() = format!("{}-strict", q.id);
        let st = if q.tool == "recall" && q.args.get("query").is_some() { score(q, Policy::Strict, &mut traced) } else { s.clone() };
        pass += s.pass as usize;
        strict += st.pass as usize;
        found += s.found;
        total += q.facts.len();
        one += (s.pass && s.calls == 1) as usize;
        toks.push(s.bytes as f64 / bpt);
        calls.push(s.calls as f64);
        let k = kinds.entry(q.kind.clone()).or_default();
        k.0 += 1;
        k.1 += s.pass as usize;
        k.2 += st.pass as usize;
        if !s.pass {
            fails.push(format!("    FAIL {} {}: missing {}", q.id, q.kind, s.missing.join("; ")));
        }
        rows.push(format!(
            "row\t{}\t{}\t{}\t{}\t{}/{}\t{}\t{:.0}\t{}",
            q.id,
            q.kind,
            s.pass as u8,
            st.pass as u8,
            s.found,
            q.facts.len(),
            s.calls,
            s.bytes as f64 / bpt,
            s.surfaced as u8
        ));
    }
    let n = qs.len();
    let sum: f64 = toks.iter().sum();
    println!(
        "(b) questions: {n}; pass {pass}/{n} informed, {strict}/{n} strict; facts {found}/{total}; answered in one call {one}; tokens per question mean {:.0}, median {:.0}, total {:.0}; calls per question mean {:.2}",
        sum / n.max(1) as f64,
        median(toks.clone()),
        sum,
        calls.iter().sum::<f64>() / n.max(1) as f64
    );
    for (k, (t, p, s)) in &kinds {
        println!("    {k}: {p}/{t} informed, {s}/{t} strict");
    }
    for f in &fails {
        println!("{f}");
    }
    if o.rows {
        println!("row\tid\tkind\tinformed\tstrict\tfacts\tcalls\ttokens\tsurfaced");
        for r in &rows {
            println!("{r}");
        }
    }

    // (c) the work queue.
    *agent.borrow_mut() = "work-queue".into();
    ok &= work_queue(&mut call, &settings.store, &roots, &c)?;
    drop(call);

    // (e) anchors on recall pointers: the set again with anchors on, followed
    // by `recall {key}`, then by a window of lines read from the anchor.
    for (label, on, window) in [("off, recall {key}", false, None), ("on, recall {key}", true, None), ("on, read_note 6 lines", true, Some(6)), ("on, read_note 12 lines", true, Some(12))] {
        let mut s = settings.clone();
        s.recall.pointer_anchors = on;
        server.set_memory(s);
        let (mut p, mut st, mut f, mut fr, mut anchored) = (0, 0, 0, 0, 0);
        let (mut toks, mut rtoks) = (Vec::new(), Vec::new());
        for q in &qs {
            let mut ask = |policy: Policy| {
                let id = format!("{}-anchors-{label}-{policy:?}", q.id);
                let mut c = |t: &str, a: &Value| -> String {
                    if t == "read_note" && policy == Policy::Informed {
                        anchored += 1;
                    }
                    announce(&mut server, &id, t);
                    server.answer(t, a).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}"))
                };
                let first = c(&q.tool, &q.args);
                score_after_with(q, &first, policy, window, &mut c)
            };
            let a = ask(Policy::Informed);
            let recall_q = q.tool == "recall" && q.args.get("query").is_some();
            let b = if recall_q { ask(Policy::Strict) } else { a.clone() };
            p += a.pass as usize;
            st += b.pass as usize;
            f += a.found;
            toks.push(a.bytes as f64 / bpt);
            if recall_q {
                fr += 1;
                rtoks.push(a.bytes as f64 / bpt);
            }
        }
        let n = qs.len();
        println!(
            "(e) anchors {label}: pass {p}/{n} informed, {st}/{n} strict; facts {f}/{total}; tokens per question mean {:.0}, median {:.0}; recall queries ({fr}) mean {:.0}; anchored follow-ups {anchored}",
            toks.iter().sum::<f64>() / n.max(1) as f64,
            median(toks.clone()),
            rtoks.iter().sum::<f64>() / fr.max(1) as f64
        );
    }
    // (e) the follow-up read itself: most questions are answered by their
    // first call, so the rows above take few follow-ups. Here every recall
    // query whose informed pick is an anchored pointer is followed both
    // ways, needed or not: `recall {key}` against lines read at the anchor.
    {
        let mut s = settings.clone();
        s.recall.pointer_anchors = true;
        server.set_memory(s);
        let mut rows: Vec<(&str, Option<usize>, usize, usize, usize)> = vec![("recall {key}", None, 0, 0, 0), ("read_note 6 lines", Some(6), 0, 0, 0), ("read_note 12 lines", Some(12), 0, 0, 0)];
        let mut n = 0;
        for q in qs.iter().filter(|q| q.tool == "recall" && q.args.get("query").is_some()) {
            let id = format!("{}-anchors-forced", q.id);
            announce(&mut server, &id, &q.tool);
            let first = server.answer(&q.tool, &q.args).map(|x| x.into_text()).unwrap_or_default();
            let Some((_, a)) = follow_up_with(q, &first, Policy::Informed, None) else { continue };
            let Some(key) = a.get("key").and_then(Value::as_str) else { continue };
            if pointer_anchor(&first, key).is_none() {
                continue;
            }
            n += 1;
            for r in rows.iter_mut() {
                let (t, args) = follow_up_with(q, &first, Policy::Informed, r.1).unwrap_or(("recall".into(), a.clone()));
                announce(&mut server, &format!("{id}-{}", r.0), &t);
                let out = server.answer(&t, &args).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}"));
                let (f, missing) = score_text(q, &format!("{first}\n{out}"));
                r.2 += out.len();
                r.3 += f;
                r.4 += missing.is_empty() as usize;
            }
        }
        for (label, _, b, f, p) in &rows {
            println!("(e) anchored follow-up forced, {label}: {n} recall queries; follow-up tokens mean {:.0}; facts {f}; pass {p}/{n}", *b as f64 / bpt / n.max(1) as f64);
        }
    }
    server.set_memory(settings.clone());

    // The hook replay, on the store as a live session sees it: the hooks
    // embed every record without a vector in the background once the model
    // loads (see `atlas::memory::vectors::spawn_embedder`), so a session
    // running for more than a minute has them all. Embedded here first, the
    // replay sees that store, and no question's score depends on how far a
    // background embedder got before it was asked.
    if !o.floors.is_empty() {
        let t0 = std::time::Instant::now();
        if let Some(n) = embed_all_pending(&settings)? {
            println!("hook replay: embedded {n} pending record vectors first in {:.1} s", t0.elapsed().as_secs_f64());
        }
    }
    let offtopic = match &o.offtopic {
        Some(p) => {
            let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
            let known: HashSet<String> = mem.lanes().map_err(|e| e.to_string())?.into_iter().map(|l| l.lane).collect();
            parse_offtopic(&text, &known)?
        }
        None => Vec::new(),
    };
    // The widened lane rule's labels on this store, once per run: intent and
    // memory records by how many lanes they name, and the recall questions'
    // intent or memory sources the same way.
    let lane_set = lane_names(&mem)?;
    {
        let count = |hs: &[atlas::memory::Head]| {
            let mut c = [0usize; 3];
            for h in hs {
                c[record_lanes(&lane_set, &h.key, &h.kind, &h.scope, &h.title).len().min(2)] += 1;
            }
            c
        };
        let mut intent = Vec::new();
        for kind in ["rule", "block"] {
            intent.extend(
                mem.heads(Some(kind), None).map_err(|e| e.to_string())?.into_iter().filter(|h| is_intent_or_memory(&h.key, &h.kind)),
            );
        }
        let memory: Vec<_> = mem
            .heads(Some("memory"), None)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|h| is_intent_or_memory(&h.key, &h.kind))
            .collect();
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        for q in qs.iter().filter(|q| q.tool == "recall" && q.args.get("query").is_some()) {
            if let Some(h) = source_key(q).and_then(|k| mem.head(k).ok().flatten()) {
                if is_intent_or_memory(&h.key, &h.kind) && seen.insert(h.key.clone()) {
                    sources.push(h);
                }
            }
        }
        let (i, m, q) = (count(&intent), count(&memory), count(&sources));
        let mut names: Vec<&str> = lane_set.iter().map(String::as_str).collect();
        names.sort_unstable();
        println!("lane rule: {} lanes ({})", names.len(), names.join(" "));
        println!(
            "lane rule labels: intent records {} (no lane {}, one lane {}, two or more {}); memory records {} (no lane {}, one lane {}, two or more {}); recall question intent or memory sources {} distinct (no lane {}, one lane {}, two or more {})",
            intent.len(),
            i[0],
            i[1],
            i[2],
            memory.len(),
            m[0],
            m[1],
            m[2],
            sources.len(),
            q[0],
            q[1],
            q[2]
        );
        for h in intent.iter().chain(memory.iter()) {
            let l = record_lanes(&lane_set, &h.key, &h.kind, &h.scope, &h.title);
            if !l.is_empty() {
                println!("    lane rule label {}[{}]", h.key, l.join("|"));
            }
        }
    }
    // Each floor twice: context selection off (the pointers as before it),
    // then on (the default); the selection line says what trim and
    // collapse saved. (h) replays the lane questions the same way: their
    // prompt names the lane, so the hook injects the lane's journal.
    for (fi, &floor) in o.floors.iter().enumerate() {
        for select in [false, true] {
            let mut s = settings.clone();
            s.recall.inject_floor = floor as f32;
            s.recall.select.enabled = select;
            let label = if select { "" } else { " select off" };
            let mut hooks = atlas::hooks::Hooks::open(s).map_err(|e| e.to_string())?;
            let recall_qs: Vec<&Question> = qs.iter().filter(|q| q.tool == "recall" && q.args.get("query").is_some()).collect();
            let (mut p, mut st, mut sur, mut inj) = (0, 0, 0, 0usize);
            let (mut lost, mut informed_fails, mut strict_fails) = (Vec::new(), Vec::new(), Vec::new());
            // (count, bytes sent, bytes whole) per selection outcome.
            let mut how: BTreeMap<&'static str, (usize, usize, usize)> = BTreeMap::new();
            let mut tally = Tally::default();
            for q in &recall_qs {
                let ev = atlas::hooks::HookEvent::from_value(
                    &atlas::json!({"event": "UserPromptSubmit", "session_id": format!("eval-{}-{floor}-{select}", q.id), "prompt": q.ask.clone(), "cwd": cwd.to_string_lossy().into_owned()}),
                );
                let ctx = hooks.handle(&ev).context;
                inj += ctx.len();
                // The question's lane is its source record's, when that has
                // one lane (see `control_label`); else its injections are not
                // labelled.
                let (own, amb) = control_label(&source_key(q).map(|k| item_lanes(&mem, &lane_set, k)).unwrap_or_default());
                tally.amb_src += amb as usize;
                tally.add(&mem, &lane_set, own.as_ref(), hooks.last_injected(), ctx.len());
                for k in hooks.last_injected() {
                    let e = how.entry(k.how.name()).or_default();
                    e.0 += 1;
                    e.1 += k.bytes;
                    e.2 += k.full;
                }
                if o.show.contains(&q.id) {
                    println!("--- {} hook floor {floor}{label}\n{ctx}", q.id);
                    for c in hooks.scores(&q.ask).map_err(|e| e.to_string())?.iter().filter(|c| c.pass).take(12) {
                        println!("    candidate {} fts {:.2} cos {}", c.head.key, c.fts, c.cos.map_or("-".to_string(), |x| format!("{x:.3}")));
                    }
                }
                let hq = Question { tool: "hook".into(), ..(*q).clone() };
                let a = score_after(&hq, &ctx, Policy::Informed, &mut |t, a| {
                    server.answer(t, a).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}"))
                });
                let b = score_after(&hq, &ctx, Policy::Strict, &mut |t, a| {
                    server.answer(t, a).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}"))
                });
                p += a.pass as usize;
                st += b.pass as usize;
                sur += a.surfaced as usize;
                if !a.surfaced {
                    lost.push(q.id.clone());
                }
                if !a.pass {
                    informed_fails.push(q.id.clone());
                }
                if !b.pass {
                    strict_fails.push(q.id.clone());
                }
            }
            let m = recall_qs.len();
            println!(
                "hook floor {floor}{label}: {m} recall questions as prompts; source injected {sur}/{m}; pass {p}/{m} informed, {st}/{m} strict (injection plus one recall key); injected tokens total {:.0}, mean {:.0}",
                inj as f64 / bpt,
                inj as f64 / bpt / m.max(1) as f64
            );
            println!("    source not injected: {}", lost.join(" "));
            println!("    fail informed: {}", informed_fails.join(" "));
            println!("    fail strict: {}", strict_fails.join(" "));
            println!("    {}", tally.line("records", bpt));
            // The off-topic set: prompts that should recall nothing (`neg`)
            // or only their own lane's records.
            if !offtopic.is_empty() {
                let (mut neg, mut lanes) = (Tally::default(), Tally::default());
                for (i, (label, prompt)) in offtopic.iter().enumerate() {
                    let ev = atlas::hooks::HookEvent::from_value(
                        &atlas::json!({"event": "UserPromptSubmit", "session_id": format!("eval-off-{i}-{floor}-{select}"), "prompt": prompt.clone(), "cwd": cwd.to_string_lossy().into_owned()}),
                    );
                    let ctx = hooks.handle(&ev).context;
                    let t = if matches!(label, Label::Neg) { &mut neg } else { &mut lanes };
                    let row = t.add(&mem, &lane_set, Some(label), hooks.last_injected(), ctx.len());
                    if o.rows {
                        println!("    offtopic row {}\t{}\t{}", i + 1, label.name(), row);
                    }
                }
                println!("    {}", neg.line("off-topic neg", bpt));
                println!("    {}", lanes.line("off-topic lane", bpt));
            }
            if select {
                let row = |k: &str| how.get(k).copied().unwrap_or_default();
                let (t, c, w) = (row("trim"), row("collapse"), row("whole"));
                println!(
                    "    selection: trimmed {} (tokens {:.0} -> {:.0}), collapsed {} (tokens {:.0} -> {:.0}), whole {} ({:.0} tokens)",
                    t.0,
                    t.2 as f64 / bpt,
                    t.1 as f64 / bpt,
                    c.0,
                    c.2 as f64 / bpt,
                    c.1 as f64 / bpt,
                    w.0,
                    w.1 as f64 / bpt
                );
            }
            if fi > 0 {
                continue;
            }
            // (h) the lane questions as prompts: the lane journal the hook
            // injects, then the lane follow-up the work queue rule makes.
            let lane_qs: Vec<&Question> = qs.iter().filter(|q| q.tool == "recall" && q.args.get("lane").is_some()).collect();
            let (mut lp, mut ls, mut linj) = (0, 0, 0usize);
            let mut lfails = Vec::new();
            for q in &lane_qs {
                let ev = atlas::hooks::HookEvent::from_value(
                    &atlas::json!({"event": "UserPromptSubmit", "session_id": format!("eval-brief-{}-{select}", q.id), "prompt": q.ask.clone(), "cwd": cwd.to_string_lossy().into_owned()}),
                );
                let ctx = hooks.handle(&ev).context;
                linj += ctx.len();
                if o.show.contains(&q.id) {
                    println!("--- {} lane hook{label}\n{ctx}", q.id);
                }
                let a = score_after(q, &ctx, Policy::Informed, &mut |t, a| server.answer(t, a).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}")));
                lp += a.pass as usize;
                ls += (a.pass && a.calls == 1) as usize;
                if !a.pass {
                    lfails.push(q.id.clone());
                }
            }
            let n = lane_qs.len();
            println!(
                "(h) lane briefs{label}: {n} lane questions as prompts; pass {lp}/{n} (injection plus the lane follow-up), {ls}/{n} from the injection alone; injected tokens total {:.0}, mean {:.0}; fail: {}",
                linj as f64 / bpt,
                linj as f64 / bpt / n.max(1) as f64,
                lfails.join(" ")
            );
        }
    }

    // (d) repeats in one session: every question's first call made twice by
    // one agent, announced by the PreToolUse hook as Claude Code does. A
    // repeat that answers `unchanged since` is read as the earlier answer
    // (the agent holds it), and a changed-lines answer with the earlier one;
    // the facts found must be the same as the first time.
    {
        let mut call = |t: &str, a: &Value| server.answer(t, a).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}"));
        let (mut b1, mut b2, mut f1, mut f2) = (0usize, 0usize, 0usize, 0usize);
        let (mut same, mut changed, mut by_rule) = (0usize, 0usize, 0usize);
        let mut first: Vec<String> = Vec::new();
        for pass in 0..2 {
            for (i, q) in qs.iter().enumerate() {
                let hook = atlas::json!({"event": "PreToolUse", "session_id": "eval-repeats", "agent_id": "", "tool_name": format!("mcp__atlas__{}", q.tool)});
                let _ = call("hook", &hook);
                let out = call(&q.tool, &q.args);
                if pass == 0 {
                    b1 += out.len();
                    f1 += score_text(q, &out).0;
                    first.push(out);
                } else {
                    b2 += out.len();
                    same += out.starts_with("unchanged since") as usize;
                    changed += out.starts_with("changed since") as usize;
                    // The work queue and lane journals answer whole by rule.
                    let whole_by_rule = (q.tool == "recall" && q.args.get("lane").is_some()) || ["decision_queue", "awaiting", "journal_status", "next_id"].contains(&q.tool.as_str());
                    by_rule += whole_by_rule as usize;
                    let hay = if out.starts_with("unchanged since") || out.starts_with("changed since") { format!("{}\n{out}", first[i]) } else { out };
                    f2 += score_text(q, &hay).0;
                }
            }
        }
        println!(
            "(d) repeats: each first call again by the same agent: tokens {:.0} first, {:.0} repeated ({same} unchanged, {changed} changed lines, the rest whole, {by_rule} of them by rule); facts {f2} on the repeat, {f1} the first time",
            b1 as f64 / bpt,
            b2 as f64 / bpt
        );
        ok &= f2 == f1;
        let _ = call("hook", &atlas::json!({"event": "PreCompact", "session_id": "eval-repeats"}));
    }

    // (f) a subagent without hooks: a new server that never sees a hook, so
    // it compares repeats per process. The parent makes every question's
    // first call; then a subagent, which holds none of the parent's answers,
    // makes the same call. A reference reply is all the subagent has until
    // it follows the `fresh: true` the reply names.
    {
        let mut store = open()?;
        store.set_memory_sync(Some(settings.clone()));
        store.rescan().map_err(|e| e.to_string())?;
        store.rescan_sources(None).map_err(|e| e.to_string())?;
        let mut bare = atlas::tools::Server::new(store);
        bare.set_memory(settings.clone());
        let mut call = |t: &str, a: &Value| bare.answer(t, a).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}"));
        let (mut parent, mut refs, mut named, mut alone, mut after, mut extra) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        for q in &qs {
            let mine = call(&q.tool, &q.args);
            let mine = if is_reference(&mine) { call(&q.tool, &fresh(&q.args)) } else { mine };
            parent += score_text(q, &mine).0;
        }
        for q in &qs {
            let got = call(&q.tool, &q.args);
            if !is_reference(&got) {
                alone += score_text(q, &got).0;
                after += score_text(q, &got).0;
                continue;
            }
            refs += 1;
            named += got.contains("fresh: true") as usize;
            alone += score_text(q, &got).0;
            let whole = call(&q.tool, &fresh(&q.args));
            extra += got.len() + whole.len();
            after += score_text(q, &format!("{got}\n{whole}")).0;
        }
        println!(
            "(f) subagent without hooks: {refs} of {} calls answered by reference to the parent's, {named} naming fresh: true; facts {alone} from the reply alone, {after} after following fresh, {parent} the parent found; the reference and fresh call cost {:.0} tokens",
            qs.len(),
            extra as f64 / bpt
        );
        ok &= after == parent && named == refs;
    }

    // (g) compaction: what survives a simulated compaction is refs, not
    // text. Each question's answer is replaced by the ref it rests on (a
    // record source: its head as key@hash; a file source: the lines holding
    // its facts as src:path:a-b@hash); after the compaction the agent makes
    // one call, `recall {key: ref}`, and the facts must still be found. A
    // file path a ref cannot spell (one with a space) is looked up again
    // with the question's own first call instead, and counted.
    {
        let mut store = open()?;
        store.set_memory_sync(Some(settings.clone()));
        let mut post = atlas::tools::Server::new(store);
        post.set_memory(settings.clone());
        let mem = atlas::memory::MemoryStore::open(&settings.store).map_err(|e| e.to_string())?;
        let (mut pass, mut found, mut total, mut resolved, mut dangling, mut again, mut bytes) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        let mut gfails: Vec<String> = Vec::new();
        for q in &qs {
            let r = match (source_key(q), source_file(q)) {
                (Some(k), _) => mem.head(k).map_err(|e| e.to_string())?.map(|h| atlas::compact::record_ref(k, &h.sha)),
                (None, Some((n, p))) => roots.get(n).and_then(|root| std::fs::read_to_string(root.join(p)).ok()).and_then(|text| {
                    let lines: Vec<String> = text.lines().map(norm).collect();
                    let at: Vec<usize> = q.facts.iter().filter_map(|f| lines.iter().position(|l| holds(l, f))).collect();
                    let (a, b) = if at.len() == q.facts.len() { (at.iter().min()? + 1, at.iter().max()? + 1) } else { (1, lines.len()) };
                    let h = atlas::compact::lines_hash(&text, a, b)?;
                    Some(format!("{n}:{p}:{a}-{b}@{h}"))
                }),
                _ => None,
            };
            let out = match r.filter(|r| atlas::compact::parse_ref(r).is_some()) {
                Some(r) => {
                    announce(&mut post, &format!("{}-compaction", q.id), "recall");
                    let out = post.answer("recall", &atlas::json!({"key": r})).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}"));
                    if out.starts_with("error:") && out.contains("dangling") {
                        dangling += 1;
                    } else if !out.starts_with("error:") {
                        resolved += 1;
                    }
                    out
                }
                None => {
                    again += 1;
                    // As (b) asks it: the first call, then the one follow-up.
                    let agent = format!("{}-compaction", q.id);
                    let mut c = |t: &str, a: &Value| -> String {
                        announce(&mut post, &agent, t);
                        post.answer(t, a).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}"))
                    };
                    let first = c(&q.tool, &q.args);
                    match follow_up_with(q, &first, Policy::Informed, None).filter(|_| !score_text(q, &first).1.is_empty()) {
                        Some((t, a)) => {
                            let second = c(&t, &a);
                            format!("{first}\n{second}")
                        }
                        None => first,
                    }
                }
            };
            bytes += out.len();
            let (f, missing) = score_text(q, &out);
            found += f;
            total += q.facts.len();
            pass += missing.is_empty() as usize;
            if !missing.is_empty() {
                gfails.push(format!("    FAIL (g) {}: missing {}", q.id, missing.join("; ")));
            }
        }
        let n = qs.len();
        // The checkpoint card the eval store would give a session now: its
        // size, and every ref in it resolving.
        let card = atlas::checkpoint::body(&atlas::checkpoint::state(&mem, "eval-compaction", EVAL_CARD_SINCE, "", atlas::checkpoint::Lanes::Store).map_err(|e| e.to_string())?, &atlas::checkpoint::Tail::default(), settings.bytes_for(settings.compact.card_tokens));
        let check = atlas::compact::check(&mem, &[], &card).map_err(|e| e.to_string())?;
        println!(
            "(g) compaction: {n} questions; {resolved} answered by recall of a ref ({dangling} dangling), {again} looked up again (no ref spells the path); pass {pass}/{n}; facts {found}/{total}; tokens per question after compaction mean {:.0}; card {:.0} tokens, {} refs, {} dangling, {} changed",
            bytes as f64 / bpt / n.max(1) as f64,
            card.len() as f64 / bpt,
            check.refs,
            check.dangling.len(),
            check.changed.len()
        );
        for f in &gfails {
            println!("{f}");
        }
        ok &= pass == n && dangling == 0 && check.dangling.is_empty();
    }

    Ok(ok)
}

/// Lowercase `D-` thread files under each vault's Discussions folder.
/// Names the agent of the next call to the server, as Claude Code's
/// PreToolUse hook does; nothing for an empty name.
fn announce(server: &mut atlas::tools::Server, agent: &str, tool: &str) {
    if !agent.is_empty() {
        let hook = atlas::json!({"event": "PreToolUse", "session_id": "eval", "agent_id": agent, "tool_name": format!("mcp__atlas__{tool}")});
        let _ = server.answer("hook", &hook);
    }
}

/// Whether an answer is a reference to an earlier one.
fn is_reference(out: &str) -> bool {
    out.starts_with("unchanged since") || out.starts_with("changed since")
}

/// The arguments with `fresh: true`.
fn fresh(args: &Value) -> Value {
    let mut a = args.clone();
    if a.is_null() {
        a = atlas::json!({});
    }
    a["fresh"] = atlas::json!(true);
    a
}

fn thread_files(roots: &BTreeMap<String, PathBuf>, c: &atlas::config::Config) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for v in &c.vaults {
        let Some(root) = roots.get(&v.name) else { continue };
        let Ok(rd) = std::fs::read_dir(root.join("Discussions")) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("D-") && name.ends_with(".md") {
                out.push((v.name.clone(), e.path()));
            }
        }
    }
    out
}

fn fm_status(text: &str) -> String {
    let fm = text.strip_prefix("---").and_then(|r| r.split("\n---").next()).unwrap_or("");
    fm.lines().find_map(|l| l.strip_prefix("status:")).map(|s| s.trim().trim_matches('"').to_string()).unwrap_or_else(|| "open".into())
}

/// Every file name under a folder, recursively.
fn names_under(root: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            names_under(&p, out);
        } else {
            out.push((p, name));
        }
    }
}

fn work_queue(call: &mut Call<'_>, store: &Path, roots: &BTreeMap<String, PathBuf>, c: &atlas::config::Config) -> Result<bool, String> {
    let conn = rusqlite::Connection::open(store).map_err(|e| e.to_string())?;
    let q = |sql: &str| -> Result<Vec<(String, i64, i64, i64)>, String> {
        let mut st = conn.prepare(sql).map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map_err(|e| e.to_string())?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())
    };
    // Truth, per lane: entries, open questions (no `answers` ref), handoffs.
    let truth = q("SELECT scope, COUNT(*),
            SUM(kind = 'journal' AND sub = 'question' AND key NOT IN (SELECT target FROM refs WHERE kind = 'answers')),
            SUM(kind = 'handoff')
        FROM records WHERE kind IN ('journal', 'handoff') AND scope <> '' GROUP BY scope ORDER BY scope")?;
    let mut ok = true;

    // journal_status: one row per lane, the same counts.
    let js = call("journal_status", &atlas::json!({}));
    let mut js_ok = 0;
    for (lane, entries, open, handoffs) in &truth {
        let row = js.lines().find(|l| l.split('\t').next() == Some(lane.as_str()));
        let good = row.is_some_and(|r| {
            let f: Vec<&str> = r.split('\t').collect();
            f.get(1) == Some(&entries.to_string().as_str())
                && f.get(4) == Some(&open.to_string().as_str())
                && f.get(5) == Some(&handoffs.to_string().as_str())
        });
        if good {
            js_ok += 1;
        } else {
            println!("    journal_status {lane}: expected {entries} entries, {open} open questions, {handoffs} handoffs; got {row:?}");
        }
    }
    ok &= js_ok == truth.len();

    // recall lane: every open question and the newest handoff shown; shown
    // plus the `+N more` count is every entry.
    let open_keys: Vec<(String, String)> = {
        let mut st = conn
            .prepare("SELECT scope, key FROM records WHERE kind = 'journal' AND sub = 'question' AND key NOT IN (SELECT target FROM refs WHERE kind = 'answers')")
            .map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())?
    };
    let newest: Vec<(String, String)> = {
        let mut st = conn
            .prepare("SELECT r.scope, r.key FROM records r JOIN versions v ON v.vid = r.head WHERE r.kind = 'handoff' AND r.status = 'active' AND v.time = (SELECT MAX(v2.time) FROM records r2 JOIN versions v2 ON v2.vid = r2.head WHERE r2.scope = r.scope AND r2.kind = 'handoff' AND r2.status = 'active')")
            .map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())?
    };
    // The newest `next` and `brief` of each lane: what a fresh agent acts on.
    let latest: Vec<(String, String)> = {
        let mut st = conn
            .prepare("SELECT scope, key FROM records r WHERE kind = 'journal' AND sub IN ('next', 'brief') AND rid = (SELECT MAX(rid) FROM records r2 WHERE r2.scope = r.scope AND r2.kind = 'journal' AND r2.sub = r.sub)")
            .map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())?
    };
    let (mut qs_shown, mut ho_shown, mut latest_shown, mut trimmed, mut counted) = (0, 0, 0, 0, 0i64);
    for (lane, entries, _, _) in &truth {
        let t = call("recall", &atlas::json!({"lane": lane.clone()}));
        let keys: HashSet<String> = keys_in(&t).into_iter().collect();
        for (_, k) in open_keys.iter().filter(|(l, _)| l == lane) {
            if keys.contains(k) {
                qs_shown += 1;
            } else {
                println!("    recall lane {lane}: open question {k} not shown");
            }
        }
        for (_, k) in newest.iter().filter(|(l, _)| l == lane) {
            if keys.contains(k) {
                ho_shown += 1;
            } else {
                println!("    recall lane {lane}: newest handoff {k} not shown");
            }
        }
        for (_, k) in latest.iter().filter(|(l, _)| l == lane) {
            if keys.contains(k) {
                latest_shown += 1;
            } else {
                println!("    recall lane {lane}: latest next or brief {k} not shown");
            }
        }
        let more: i64 =
            t.lines().filter_map(|l| l.strip_prefix('+')).filter_map(|r| r.split_whitespace().next()?.parse::<i64>().ok()).sum();
        if more > 0 {
            trimmed += 1;
            counted += more;
        }
        let _ = entries;
    }
    println!(
        "(c) work queue: journal_status {js_ok}/{} lanes exact; recall lane: open questions shown {qs_shown}/{}, newest handoff shown {ho_shown}/{}, latest next and brief shown {latest_shown}/{}; past the default budget: {trimmed} lanes, {counted} older decisions, measurements, commits or notes counted on a +N line",
        truth.len(),
        open_keys.len(),
        newest.len(),
        latest.len()
    );
    ok &= qs_shown == open_keys.len() && ho_shown == newest.len() && latest_shown == latest.len();

    // decision_queue and awaiting against the thread files.
    let threads = thread_files(roots, c);
    let unresolved = threads.iter().filter(|(_, p)| std::fs::read_to_string(p).is_ok_and(|t| fm_status(&t) == "unresolved")).count();
    // Counted from the JSON form; the text form is checked to list as many.
    let dq = call("decision_queue", &atlas::json!({"format": "json"}));
    let dq_text = call("decision_queue", &atlas::json!({}));
    let got = atlas::json::parse(&dq).ok().and_then(|v| v.get("queue").and_then(Value::as_array).map(Vec::len));
    let mut waiting = 0;
    for (_, p) in &threads {
        let Ok(t) = std::fs::read_to_string(p) else { continue };
        if fm_status(&t) == "concluded" {
            continue;
        }
        waiting += atlas::discussion::awaiting(&atlas::discussion::parse(&t), "Brandon").len();
    }
    let aw = call("awaiting", &atlas::json!({"author": "Brandon", "format": "json"}));
    let aw_text = call("awaiting", &atlas::json!({"author": "Brandon"}));
    let got_aw = atlas::json::parse(&aw).ok().and_then(|v| v.get("awaiting").and_then(Value::as_array).map(Vec::len));
    println!("    decision_queue: {got:?} items, {unresolved} unresolved thread files of {}; awaiting Brandon: {got_aw:?} items, {waiting} by the files", threads.len());
    ok &= got == Some(unresolved) && got_aw == Some(waiting);
    // The text answer (the default) names the same count: `key: N`, or
    // `key: none` for an empty list.
    let text_count = |t: &str, k: &str| -> Option<usize> {
        let v = t.lines().find_map(|l| l.strip_prefix(&format!("{k}: ")))?;
        if v == "none" { Some(0) } else { v.parse().ok() }
    };
    let (dq_n, aw_n) = (text_count(&dq_text, "queue"), text_count(&aw_text, "awaiting"));
    if dq_n != got || aw_n != got_aw {
        println!("    text answers list {dq_n:?} queued and {aw_n:?} awaiting, the JSON {got:?} and {got_aw:?}");
    }
    ok &= dq_n == got && aw_n == got_aw;

    // next_id: one past the highest number on disk, per vault and prefix.
    let (mut nid_ok, mut nid_all) = (0, 0);
    for v in &c.vaults {
        let Some(root) = roots.get(&v.name) else { continue };
        let mut names = Vec::new();
        names_under(root, &mut names);
        let mut best: BTreeMap<String, (u64, PathBuf, usize)> = BTreeMap::new();
        for (p, name) in &names {
            let Some(dash) = name.find('-') else { continue };
            let prefix = &name[..=dash];
            if !prefix[..dash].chars().all(|ch| ch.is_ascii_uppercase()) || dash == 0 {
                continue;
            }
            let digits: String = name[dash + 1..].chars().take_while(char::is_ascii_digit).collect();
            let Ok(n) = digits.parse::<u64>() else { continue };
            let e = best.entry(prefix.to_string()).or_insert((0, PathBuf::new(), digits.len()));
            if n > e.0 {
                *e = (n, p.clone(), digits.len());
            }
        }
        for (prefix, (n, p, width)) in &best {
            let folder =
                p.parent().and_then(|d| d.strip_prefix(root).ok()).map(|d| d.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            let want = format!("{prefix}{:0w$}", n + 1, w = (*width).max(3));
            let got = call(
                "next_id",
                &atlas::json!({"vault": v.name.clone(), "folder": folder, "prefix": prefix.clone(), "digits": (*width).max(3), "reason": "memory eval: the next id of each sequence"}),
            );
            nid_all += 1;
            if got.contains(&format!("\"{want}\"")) || got.lines().any(|l| l == format!("id: {want}")) {
                nid_ok += 1;
            } else {
                println!("    next_id {}:{prefix}: expected {want}, got {}", v.name, got.trim());
            }
        }
    }
    println!("    next_id: {nid_ok}/{nid_all} sequences one past the highest file");
    ok &= nid_ok == nid_all;
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = "# a comment\nQ001 ruling\nask: What is the frame target?\nsource: record:frame-target\ncall: recall {\"query\": \"frame target\"}\nfact: 240\nfact: 120 | one twenty\n\nQ002 id\nask: What is X-001?\nsource: file:v:Notes/X-001 A note.md\ncall: lookup {\"id\": \"X-001\"}\nfact: a note\n";

    #[test]
    fn offtopic_lines_parse_and_label_by_lane() {
        let known: HashSet<String> = ["TRACKS", "RAGDOLL"].iter().map(|s| s.to_string()).collect();
        let set = parse_offtopic("# c\n\nneg\tHow do tides work?\r\nTRACKS|RAGDOLL\tTrack marks behind tanks\n", &known).unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!(set[0], (Label::Neg, "How do tides work?".to_string()));
        assert_eq!(set[1].0, Label::Lanes(vec!["TRACKS".into(), "RAGDOLL".into()]));
        // neg: everything is off-topic, a laneless item too.
        assert!(Label::Neg.off(&[]) && Label::Neg.off(&["TRACKS".into()]));
        // lanes: another lane's item is off-topic; its own and a laneless one are not.
        assert!(set[1].0.off(&["FILL-TAIL".into()]));
        assert!(!set[1].0.off(&["RAGDOLL".into()]) && !set[1].0.off(&[]));
        assert!(parse_offtopic("FILL-TAIL\tx\n", &known).unwrap_err().contains("no lane FILL-TAIL"));
        assert!(parse_offtopic("no tab here\n", &known).unwrap_err().contains("label<TAB>prompt"));
    }

    fn lane_set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn widened_rule_labels_intent_and_memory_records_by_lane_tokens() {
        let l = lane_set(&["TURF", "QUARRY-FLOW", "RIDGE-MAP", "UNIT-LIGHTS"]);
        // One lane, from the title.
        assert_eq!(record_lanes(&l, "intent:r00001", "rule", "intent", "2026-10-18 Brandon: TURF ruled: grass"), vec!["TURF"]);
        // A memory record: `_` joins a token, so QUARRY-FLOW_notes is no lane; the title names one.
        assert_eq!(record_lanes(&l, "QUARRY-FLOW_notes", "memory", "", "- [x](x.md) - y"), Vec::<String>::new());
        assert_eq!(record_lanes(&l, "notes-on", "memory", "", "- [x](x.md) - QUARRY-FLOW rest"), vec!["QUARRY-FLOW"]);
        // No lane.
        assert!(record_lanes(&l, "intent:r00002", "block", "intent", "A rule about frames").is_empty());
        // Two lanes, sorted.
        assert_eq!(
            record_lanes(&l, "intent:r01178", "rule", "intent", "UNIT-LIGHTS and RIDGE-MAP (both)"),
            vec!["RIDGE-MAP", "UNIT-LIGHTS"]
        );
        // Case-sensitive: lowercase and title case do not match.
        assert!(record_lanes(&l, "intent:r00003", "rule", "intent", "turf and Turf and quarry-flow").is_empty());
        // Token boundary: QUARRY-FLOW-2 is not QUARRY-FLOW; punctuation is a boundary.
        assert!(record_lanes(&l, "intent:r00004", "rule", "intent", "QUARRY-FLOW-2 landed").is_empty());
        assert_eq!(record_lanes(&l, "intent:r00005", "rule", "intent", "(QUARRY-FLOW), done"), vec!["QUARRY-FLOW"]);
        // Journal and handoff records keep their scope; other records have none.
        assert_eq!(record_lanes(&l, "journal:TURF:0001", "journal", "TURF", "RIDGE-MAP"), vec!["TURF"]);
        assert_eq!(record_lanes(&l, "handoff:NEW-MAP:001", "handoff", "NEW-MAP", ""), vec!["NEW-MAP"]);
        assert!(record_lanes(&l, "restore:x", "section", "", "TURF").is_empty());
        // A memory kind with a prefixed key, or an intent key of another kind, is not labelled.
        assert!(record_lanes(&l, "x:TURF", "memory", "", "TURF").is_empty());
        assert!(record_lanes(&l, "intent:r9", "note", "", "TURF").is_empty());
    }

    #[test]
    fn widened_rule_counts_ambiguous_items_by_the_prompt_lane() {
        let p = Label::Lanes(vec!["RIDGE-MAP".into()]);
        let two = |a: &str, b: &str| vec![a.to_string(), b.to_string()];
        // Two lanes, P in S: not off-topic; P not in S: off-topic.
        assert!(!p.off(&two("RIDGE-MAP", "UNIT-LIGHTS")));
        assert!(p.off(&two("FILM-WICK", "UNIT-LIGHTS")));
        // One lane and no lane, unchanged.
        assert!(p.off(&["TURF".into()]) && !p.off(&["RIDGE-MAP".into()]) && !p.off(&[]));
        // neg: every item.
        assert!(Label::Neg.off(&two("RIDGE-MAP", "UNIT-LIGHTS")));
    }

    #[test]
    fn session_scopes_are_not_lanes_for_the_rule() {
        // A `session:` scope is never a token (':' splits it), so it names
        // no lane even if a lane set held it; lane_names drops them anyway.
        assert!(token_lanes(&lane_set(&["session:0f3a"]), &["session:0f3a notes"]).is_empty());
        let l = lane_set(&["TURF"]);
        assert!(record_lanes(&l, "intent:r00006", "rule", "intent", "session:0f3a notes").is_empty());
    }

    #[test]
    fn control_source_takes_one_lane_only() {
        assert_eq!(control_label(&[]), (None, false));
        assert_eq!(control_label(&["TURF".into()]), (Some(Label::Lanes(vec!["TURF".into()])), false));
        assert_eq!(control_label(&["RIDGE-MAP".into(), "UNIT-LIGHTS".into()]), (None, true));
    }

    #[test]
    fn parses_blocks_fields_and_alternatives() {
        let qs = parse(SET).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].id, "Q001");
        assert_eq!(qs[0].kind, "ruling");
        assert_eq!(qs[0].tool, "recall");
        assert_eq!(qs[0].args.get("query").and_then(Value::as_str), Some("frame target"));
        assert_eq!(qs[0].facts, vec![vec!["240".to_string()], vec!["120".to_string(), "one twenty".to_string()]]);
        assert_eq!(source_file(&qs[1]), Some(("v", "Notes/X-001 A note.md")));
    }

    #[test]
    fn rejects_incomplete_and_duplicate_blocks() {
        assert!(parse("Q001 ruling\nask: x\n").is_err());
        assert!(parse("ask: outside\n").is_err());
        let dup = format!("{SET}\n{}", SET.lines().skip(1).take(6).collect::<Vec<_>>().join("\n"));
        assert!(parse(&dup).unwrap_err().contains("duplicate"));
    }

    #[test]
    fn facts_match_across_wrapping_case_and_emphasis() {
        let hay = norm("Corpse baking is **off by\n  default** (`--bake`)");
        assert!(holds(&hay, &["off by default".into()]));
        assert!(holds(&hay, &["nothing".into(), "--BAKE".into()]));
        assert!(!holds(&hay, &["on by default".into()]));
    }

    #[test]
    fn keys_come_from_records_and_pointers() {
        let t = "== intent:r00059 rule active v1 2026-10-31T12:52\n- body line\n- corpse-baking-is-off: Corpse baking is off\n- journal:TURF:0010 12.5: Hand-back\n+4 more over budget_tokens\n";
        assert_eq!(keys_in(t), vec!["intent:r00059", "corpse-baking-is-off", "journal:TURF:0010"]);
    }

    #[test]
    fn the_follow_up_reads_a_listed_key_once() {
        let q = &parse(SET).unwrap()[0];
        let first = "- other-note: something\n- frame-target: Frame target\n";
        let mut seen = Vec::new();
        let mut call = |t: &str, a: &Value| -> String {
            seen.push((t.to_string(), a.get("key").and_then(Value::as_str).unwrap_or("").to_string()));
            if a.get("key").and_then(Value::as_str) == Some("frame-target") {
                "240 at optimal, 120 at highest".into()
            } else {
                "nothing".into()
            }
        };
        let s = score_after(q, first, Policy::Informed, &mut call);
        assert!(s.pass && s.calls == 2 && s.surfaced);
        let s = score_after(q, first, Policy::Strict, &mut call);
        assert!(!s.pass && s.calls == 2);
        assert_eq!(seen, vec![("recall".to_string(), "frame-target".to_string()), ("recall".to_string(), "other-note".to_string())]);
    }

    #[test]
    fn informed_follows_the_record_a_file_source_was_imported_into() {
        let mut q = parse("Q001 gap\nask: What is the rule?\nsource: file:rules:RULES.md\ncall: recall {\"query\": \"the rule\"}\nfact: sixty four\n").unwrap().remove(0);
        q.source_records = ["rules:r00007".to_string()].into_iter().collect();
        let first = "- other-note: something\n- rules:r00007 9.1: the rule\n";
        let mut seen = Vec::new();
        let mut call = |_: &str, a: &Value| -> String {
            let k = a.get("key").and_then(Value::as_str).unwrap_or("").to_string();
            seen.push(k.clone());
            if k == "rules:r00007" { "the bound is sixty four".into() } else { "nothing".into() }
        };
        assert!(score_after(&q, first, Policy::Informed, &mut call).pass);
        // Strict is unchanged: the first listed key.
        assert!(!score_after(&q, first, Policy::Strict, &mut call).pass);
        assert_eq!(seen, vec!["rules:r00007".to_string(), "other-note".to_string()]);
        // No record from the file listed: no follow-up.
        let mut none = |_: &str, _: &Value| -> String { panic!("no follow-up") };
        assert!(!score_after(&q, "- other-note: something\n", Policy::Informed, &mut none).pass);
    }

    #[test]
    fn an_answer_with_every_fact_takes_one_call() {
        let q = &parse(SET).unwrap()[0];
        let mut call = |_: &str, _: &Value| -> String { panic!("no follow-up") };
        let s = score_after(q, "== frame-target memory\n240 and one twenty", Policy::Informed, &mut call);
        assert!(s.pass && s.calls == 1 && s.found == 2);
    }
}
