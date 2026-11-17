//! The cost side of `atlas --token-report` (#227): what atlas itself puts
//! into context, measured from bytes, never estimated by a model, beside
//! what the ledger ([`crate::tokens`]) credits it with.
//!
//! Three buckets, each in bytes, made tokens by the one `bytes_per_token`
//! ratio of the configuration:
//!
//! * **A. Tool definitions**: the bytes the server sends at a session start
//!   (its tool list and the instructions' habits paragraph, from this
//!   binary), once per session or subagent the store shows (distinct
//!   `(session, agent)` in `hook_inject`). With transcripts, plus the
//!   definition bytes of every atlas tool a session loaded later through
//!   tool search (each `tool_reference` naming one).
//! * **B. Hook injections**: the `hook_inject` bytes per event, as sent,
//!   hit or not; and the hook text no `hook_inject` row holds (headers, the
//!   SessionStart card, PreCompact), from the hook rows' returned bytes.
//!   With transcripts, the re-read: an injection stays in context for every
//!   later request of its transcript up to the next compaction boundary (or
//!   the end), and each request after the first that read it reads it
//!   again. Each re-read is priced by what that request was billed
//!   ([`PRICES`]): its atlas share split over input, cache-read and
//!   cache-write tokens by their proportions in the request's `usage`.
//! * **C. Tool output**: the ledger's returned bytes per tool, every row but
//!   the hook rows (counted in B) and the double logs (the bare `hook` row,
//!   `hook:PostToolUse`). `token_log` has no session column, so C is one
//!   total for all sessions, never per session or turn.
//!
//! A turn is one UserPromptSubmit: the distinct seconds of a main
//! session's UserPromptSubmit rows in `hook_inject` ([`Turns`], the rule
//! the report's injections per prompt use too). A prompt that injected
//! nothing leaves no row, so turns are undercounted and the cost per turn
//! errs high. A subagent has no prompts: its unit is the agent.
//!
//! The net table holds one row no ledger row has: the rulebook's measured
//! credit (#219, [`Cost::rulebook`]), when the caller measured it.
//!
//! Transcripts are read only, and only their events at or before the
//! ledger's last row, so both sides cover the same window. Without them,
//! or for a session whose transcript is gone, the re-read and the tool
//! search loads are not seen, and the figures that miss them are floors.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use rusqlite::Connection;

use crate::error::Result;
use crate::json::Value;

/// Price ratios of one model against its base input price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ratios {
    /// A cache read.
    pub read: f64,
    /// A cache write with the 5-minute TTL.
    pub write_5m: f64,
    /// A cache write with the 1-hour TTL.
    pub write_1h: f64,
}

/// Price ratios by model id prefix, longest first so a longer id matches
/// before its family. Source: the Anthropic API pricing page,
/// https://platform.claude.com/docs/en/about-claude/pricing, read
/// 2026-11-15: cache writes cost 1.25x (5 minutes) and 2x (1 hour) the base
/// input price on every model; a cache read 0.1x on most, 0.05x on Opus 5.5
/// and Sonnet 5.5, and 0.025x on Fable and Mythos 5.1 and 5.2.
pub const PRICES: &[(&str, Ratios)] = &[
    ("claude-fable-5-2", Ratios { read: 0.025, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-mythos-5-2", Ratios { read: 0.025, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-fable-5-1", Ratios { read: 0.025, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-mythos-5-1", Ratios { read: 0.025, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-sonnet-5-5", Ratios { read: 0.05, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-haiku-5-5", Ratios { read: 0.1, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-haiku-4-5", Ratios { read: 0.1, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-opus-5-5", Ratios { read: 0.05, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-fable-5", Ratios { read: 0.1, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-mythos-5", Ratios { read: 0.1, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-sonnet-5", Ratios { read: 0.1, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-sonnet-4", Ratios { read: 0.1, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-opus-5", Ratios { read: 0.1, write_5m: 1.25, write_1h: 2.0 }),
    ("claude-opus-4", Ratios { read: 0.1, write_5m: 1.25, write_1h: 2.0 }),
];

/// A model's ratios, or `None` when the table does not have it.
pub fn ratios(model: &str) -> Option<Ratios> {
    PRICES.iter().find(|(p, _)| model.starts_with(p)).map(|(_, r)| *r)
}

/// One request's billed input tokens, from its `usage` block.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Uncached input, at the full price.
    pub input: u64,
    /// Cache reads.
    pub cache_read: u64,
    /// Cache writes, 5-minute TTL (all of them when the split is absent).
    pub write_5m: u64,
    /// Cache writes, 1-hour TTL.
    pub write_1h: u64,
}

/// One API request a transcript records (an assistant message).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    /// Its time.
    pub time: String,
    /// The model.
    pub model: String,
    /// What it was billed, when the transcript has it.
    pub usage: Option<Usage>,
}

impl Request {
    /// The price weight of one token of this request's input against the
    /// full input price, by its usage proportions, and whether it was
    /// priced at the full rate for want of a usage block or of the model in
    /// [`PRICES`].
    pub fn weight(&self) -> (f64, bool) {
        let (Some(u), Some(r)) = (self.usage, ratios(&self.model)) else { return (1.0, true) };
        let total = (u.input + u.cache_read + u.write_5m + u.write_1h) as f64;
        if total <= 0.0 {
            return (1.0, true);
        }
        ((u.input as f64 + u.cache_read as f64 * r.read + u.write_5m as f64 * r.write_5m + u.write_1h as f64 * r.write_1h) / total, false)
    }
}

/// What the cost side reads from one transcript.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    /// Its requests, one per message id, oldest first.
    pub requests: Vec<Request>,
    /// Its compaction boundaries' times, oldest first.
    pub boundaries: Vec<String>,
    /// Atlas tools loaded through tool search: (time, tool name after the
    /// last `__`).
    pub loads: Vec<(String, String)>,
}

/// Whether a tool name is one of atlas's (`mcp__atlas__*`, or a plugin's
/// `mcp__plugin_atlas_atlas__*`).
fn atlas_tool(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (server == "atlas" || server.ends_with("_atlas")).then_some(tool)
}

/// The number at `key` of a JSON object, 0 when absent.
fn num(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_f64).map_or(0, |n| n.max(0.0) as u64)
}

/// Reads one transcript's requests, compaction boundaries and atlas tool
/// loads, at or before `until` (all when empty). Read only; a missing file
/// reads as `None`.
pub fn read_transcript(path: &Path, until: &str) -> Option<Transcript> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut t = Transcript::default();
    let mut ids: HashMap<String, usize> = HashMap::new();
    for line in text.lines() {
        let assistant = line.contains("\"assistant\"");
        let boundary = line.contains("\"compact_boundary\"");
        let load = line.contains("\"tool_reference\"");
        if !(assistant || boundary || load) {
            continue;
        }
        let Ok(v) = crate::json::parse(line) else { continue };
        let time = v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        if !until.is_empty() && time.as_str() > until {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => t.boundaries.push(time),
            Some("assistant") => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                let model = m.get("model").and_then(Value::as_str).unwrap_or("").to_string();
                // Claude Code's own notes are no requests.
                if model == "<synthetic>" {
                    continue;
                }
                let usage = m.get("usage").map(|u| {
                    let created = num(u, "cache_creation_input_tokens");
                    let split = u
                        .get("cache_creation")
                        .filter(|c| c.get("ephemeral_5m_input_tokens").is_some() || c.get("ephemeral_1h_input_tokens").is_some());
                    let (w5, w1) = match split {
                        Some(c) => (num(c, "ephemeral_5m_input_tokens"), num(c, "ephemeral_1h_input_tokens")),
                        None => (created, 0),
                    };
                    Usage { input: num(u, "input_tokens"), cache_read: num(u, "cache_read_input_tokens"), write_5m: w5, write_1h: w1 }
                });
                let id = m.get("id").and_then(Value::as_str).map_or_else(|| format!("line:{}", t.requests.len()), str::to_string);
                match ids.get(&id) {
                    // A streamed message repeats its id: keep the last usage seen.
                    Some(&i) => {
                        if usage.is_some() {
                            t.requests[i].usage = usage;
                        }
                    }
                    None => {
                        ids.insert(id, t.requests.len());
                        t.requests.push(Request { time, model, usage });
                    }
                }
            }
            Some("user") => {
                let Some(Value::Array(items)) = v.pointer("/message/content") else { continue };
                for x in items {
                    let Some(Value::Array(refs)) = x.get("content") else { continue };
                    for r in refs {
                        if r.get("type").and_then(Value::as_str) != Some("tool_reference") {
                            continue;
                        }
                        if let Some(tool) = r.get("tool_name").and_then(Value::as_str).and_then(atlas_tool) {
                            t.loads.push((time.clone(), tool.to_string()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    t.requests.sort_by(|a, b| a.time.cmp(&b.time));
    t.boundaries.sort();
    Some(t)
}

/// The re-reads of an injection made at `time`: the requests after the
/// first one that follows it, up to the next compaction boundary (or the
/// end). The first request is the send, counted as sent.
pub fn rereads<'t>(t: &'t Transcript, time: &str) -> impl Iterator<Item = &'t Request> + 't {
    let end = t.boundaries.iter().find(|b| b.as_str() > time).cloned();
    let time = time.to_string();
    t.requests.iter().filter(move |r| r.time > time && end.as_ref().is_none_or(|e| r.time < *e)).skip(1)
}

/// The bytes a session start sends: the tool list and the instructions'
/// habits paragraph ([`crate::tools::GUIDE`]) and style line
/// ([`crate::tools::STYLE`]), each after a line break. The instructions'
/// first line names the served vaults, which depend on the configuration;
/// it is left out, so this is a floor.
pub fn start_bytes() -> usize {
    let tools = Value::Array(crate::tools::tool_list());
    tools.to_string().len() + 1 + crate::tools::GUIDE.len() + 1 + crate::tools::STYLE.len()
}

/// The definition bytes of one atlas tool by name (its listed schema, else
/// its full one), 0 for an unknown name.
pub fn tool_bytes(name: &str) -> usize {
    let find =
        |list: Vec<Value>| list.into_iter().find(|t| t.get("name").and_then(Value::as_str) == Some(name)).map(|t| t.to_string().len());
    find(crate::tools::tool_list()).or_else(|| find(crate::tools::full_tool_list())).unwrap_or(0)
}

/// Mean, median, 95th percentile and largest of some values (zeros when
/// empty).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spread {
    /// How many values.
    pub n: usize,
    /// Mean.
    pub mean: f64,
    /// Median.
    pub p50: f64,
    /// 95th percentile ([`crate::baseline::quantile`]).
    pub p95: f64,
    /// Largest.
    pub max: f64,
}

/// The [`Spread`] of some values.
pub fn spread(mut v: Vec<f64>) -> Spread {
    if v.is_empty() {
        return Spread::default();
    }
    v.sort_by(f64::total_cmp);
    Spread {
        n: v.len(),
        mean: v.iter().sum::<f64>() / v.len() as f64,
        p50: crate::baseline::median(&v),
        p95: crate::baseline::quantile(&v, 0.95),
        max: v[v.len() - 1],
    }
}

/// A main session's turns, by the turn rule: the distinct seconds of its
/// `UserPromptSubmit` rows in `hook_inject`. A prompt that injected nothing
/// leaves no row, so turns are undercounted. The one rule for the cost per
/// turn ([`measure`]) and the injections per prompt
/// ([`crate::tokens::inject_report`], #212).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turns(Vec<String>);

impl Turns {
    /// The turns of one main session's rows, each `(time, hook)`.
    pub fn of<'a>(rows: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut turns: Vec<String> =
            rows.into_iter().filter(|(_, hook)| *hook == "UserPromptSubmit").map(|(time, _)| second(time).to_string()).collect();
        turns.sort();
        turns.dedup();
        Turns(turns)
    }

    /// How many turns.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the session has no turn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The turn a time falls in: the last turn at or before its second. A
    /// time before the first prompt counts in the first turn.
    #[must_use]
    pub fn index(&self, time: &str) -> usize {
        self.0.partition_point(|t| t.as_str() <= second(time)).max(1) - 1
    }
}

/// A timestamp to the second.
fn second(time: &str) -> &str {
    time.get(..19).unwrap_or(time)
}

/// The cost side, in tokens.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cost {
    /// A: session starts seen, and their tokens.
    pub starts: i64,
    /// A: the start tokens.
    pub start_tokens: f64,
    /// A: atlas tools loaded through tool search, and their tokens.
    pub loads: i64,
    /// A: the loads' tokens.
    pub load_tokens: f64,
    /// B: sent tokens by event, from `hook_inject`.
    pub sent: BTreeMap<String, f64>,
    /// B: hook text no `hook_inject` row holds, by event.
    pub framing: BTreeMap<String, f64>,
    /// B: re-read tokens by event, raw.
    pub reread: BTreeMap<String, f64>,
    /// B: re-read tokens by event, price-weighted (full-input-price tokens).
    pub reread_priced: BTreeMap<String, f64>,
    /// Re-read requests priced at the full rate (no usage, or a model not
    /// in [`PRICES`]).
    pub full_rate: i64,
    /// Re-read requests priced.
    pub priced: i64,
    /// C: returned tokens by tool, all sessions.
    pub output: BTreeMap<String, f64>,
    /// Main sessions: cost per turn (A and B).
    pub per_turn: Spread,
    /// Turns counted.
    pub turns: i64,
    /// Subagents: cost per agent (A and B).
    pub per_agent: Spread,
    /// Sessions and subagents with a transcript read.
    pub with_transcript: i64,
    /// Sessions and subagents without one (not given, gone, or outside the
    /// folder).
    pub without_transcript: i64,
    /// Whether transcripts were asked for.
    pub transcripts: bool,
    /// The rulebook's measured credit, in tokens (#219): the per-agent
    /// pre-atlas read ([`crate::tokens::brief_measured`]) times the agents
    /// in both the brief and the injection tables. Set by the caller, never
    /// by [`measure`]; `None` when not measured, and then it is no row of
    /// the net table (the rulebook's file size never is).
    pub rulebook: Option<f64>,
    /// Ledger credit, in tokens, of the recall answers that read records
    /// where they are written (`recall {key}` and `{lane}`,
    /// [`crate::tokens::recall_records_credit`]). Such an answer's
    /// counterfactual can be the same rulebook window a measured rulebook
    /// already credits, and `token_log` records no agent or export per
    /// answer to tell which, so the rulebook's net row is reduced by it, to
    /// 0 at most ([`net_rows`]). It errs low: main-session recalls and other
    /// exports are taken off too.
    pub rulebook_overlap: f64,
}

impl Cost {
    /// Sets the measured rulebook credit from the brief-vs-injections match
    /// (the agents both tables hold), `None` without one, and the
    /// recall-by-record credit it overlaps ([`Cost::rulebook_overlap`]).
    /// Net counts the rulebook only as measured, never its file size.
    pub fn credit_rulebook(&mut self, matched: Option<&crate::tokens::MatchRow>, recall_records: f64) {
        self.rulebook = matched.and_then(|m| m.rulebook);
        self.rulebook_overlap = recall_records.max(0.0);
    }

    /// The rulebook's credit in Net: the measured credit less the
    /// recall-by-record credit that may stand for the same reads, never
    /// below 0; `None` when not measured.
    #[must_use]
    pub fn rulebook_net_credit(&self) -> Option<f64> {
        self.rulebook.map(|rb| rb - rb.min(self.rulebook_overlap))
    }

    /// The part of the measured rulebook credit taken off for recall by
    /// record ([`Cost::rulebook_overlap`]): 0 when not measured.
    #[must_use]
    pub fn rulebook_taken_off(&self) -> f64 {
        self.rulebook.map_or(0.0, |rb| rb.min(self.rulebook_overlap))
    }

    /// Every bucket's tokens, the re-read price-weighted.
    pub fn total(&self) -> f64 {
        self.a() + self.b() + self.c()
    }

    /// Bucket A.
    pub fn a(&self) -> f64 {
        self.start_tokens + self.load_tokens
    }

    /// Bucket B, the re-read price-weighted.
    pub fn b(&self) -> f64 {
        sum_of(&self.sent) + sum_of(&self.framing) + sum_of(&self.reread_priced)
    }

    /// Bucket C.
    pub fn c(&self) -> f64 {
        sum_of(&self.output)
    }

    /// Whether any part is a floor: no transcripts asked for, or a session
    /// without one.
    pub fn floor(&self) -> bool {
        !self.transcripts || self.without_transcript > 0
    }

    /// The B cost of one `hook:<event>` row's event: sent, framing and the
    /// priced re-read.
    pub fn hook(&self, event: &str) -> f64 {
        let g = |m: &BTreeMap<String, f64>| m.get(event).copied().unwrap_or(0.0);
        g(&self.sent) + g(&self.framing) + g(&self.reread_priced)
    }
}

/// The sum of a map's values: 0 when empty (an empty `f64` sum is -0,
/// which prints as "-0").
fn sum_of(m: &BTreeMap<String, f64>) -> f64 {
    m.values().sum::<f64>() + 0.0
}

/// One `hook_inject` row as the cost side reads it.
struct Inject {
    time: String,
    session: String,
    agent: String,
    transcript: String,
    hook: String,
    bytes: i64,
}

/// Measures the cost side.
///
/// # Arguments
///
/// * `conn` - the index database (read only: no table is made).
/// * `bytes_per_token` - the ratio.
/// * `transcripts` - the folder transcripts are read below, if asked for.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn measure(conn: &Connection, bytes_per_token: f64, transcripts: Option<&Path>) -> Result<Cost> {
    let has = |t: &str| -> Result<bool> {
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)", [t], |r| r.get(0))?)
    };
    let mut c = Cost { transcripts: transcripts.is_some(), ..Cost::default() };
    let tok = |b: f64| b / bytes_per_token;
    // C, and the hook rows' returned bytes for the framing line.
    let mut hook_returned: BTreeMap<String, f64> = BTreeMap::new();
    let mut until = String::new();
    if has("token_log")? {
        let mut st = conn.prepare("SELECT tool, SUM(returned) FROM token_log GROUP BY tool")?;
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (tool, b) = row?;
            if tool == crate::token_report::DOUBLE_LOGGED || tool == crate::token_report::ROUTED {
                continue;
            }
            match tool.strip_prefix("hook:") {
                Some(event) => {
                    hook_returned.insert(event.to_string(), b as f64);
                }
                None => {
                    c.output.insert(tool, tok(b as f64));
                }
            }
        }
        until = conn.query_row("SELECT ifnull(MAX(time), '') FROM token_log", [], |r| r.get(0))?;
    }
    let rows: Vec<Inject> = if has("hook_inject")? {
        let mut st = conn.prepare("SELECT time, session, agent, transcript, hook, bytes FROM hook_inject ORDER BY id")?;
        let v = st
            .query_map([], |r| {
                Ok(Inject {
                    time: r.get(0)?,
                    session: r.get(1)?,
                    agent: r.get(2)?,
                    transcript: r.get(3)?,
                    hook: r.get(4)?,
                    bytes: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    } else {
        Vec::new()
    };
    let mut injected: BTreeMap<String, i64> = BTreeMap::new();
    for r in &rows {
        *injected.entry(r.hook.clone()).or_default() += r.bytes;
    }
    for (event, b) in &injected {
        c.sent.insert(event.clone(), tok(*b as f64));
    }
    for (event, b) in &hook_returned {
        let extra = b - *injected.get(event).unwrap_or(&0) as f64;
        if extra > 0.0 {
            c.framing.insert(event.clone(), tok(extra));
        }
    }
    // Per (session, agent): its rows, its transcript, its A and B.
    let mut scopes: BTreeMap<(String, String), Vec<&Inject>> = BTreeMap::new();
    for r in &rows {
        scopes.entry((r.session.clone(), r.agent.clone())).or_default().push(r);
    }
    let start = tok(start_bytes() as f64);
    c.starts = scopes.len() as i64;
    c.start_tokens = start * c.starts as f64;
    let mut per_turn: Vec<f64> = Vec::new();
    let mut per_agent: Vec<f64> = Vec::new();
    for ((_, agent), list) in &scopes {
        let path = list.iter().map(|r| r.transcript.as_str()).find(|p| !p.is_empty()).unwrap_or("");
        let t = match transcripts {
            Some(dir) if !path.is_empty() && Path::new(path).starts_with(dir) => read_transcript(Path::new(path), &until),
            _ => None,
        };
        match &t {
            Some(_) => c.with_transcript += 1,
            None => c.without_transcript += 1,
        }
        // The scope's events in time order, each with its tokens: the start
        // first, then each injection sent, each load, each re-read.
        let mut events: Vec<(String, f64)> = vec![(list.first().map(|r| r.time.clone()).unwrap_or_default(), start)];
        for r in list {
            events.push((r.time.clone(), tok(r.bytes as f64)));
        }
        if let Some(t) = &t {
            for (time, tool) in &t.loads {
                let b = tok(tool_bytes(tool) as f64);
                c.loads += 1;
                c.load_tokens += b;
                events.push((time.clone(), b));
            }
            for r in list {
                let raw = tok(r.bytes as f64);
                for q in rereads(t, &r.time) {
                    let (w, full) = q.weight();
                    c.priced += 1;
                    c.full_rate += i64::from(full);
                    *c.reread.entry(r.hook.clone()).or_default() += raw;
                    *c.reread_priced.entry(r.hook.clone()).or_default() += raw * w;
                    events.push((q.time.clone(), raw * w));
                }
            }
        }
        if agent.is_empty() {
            let turns = Turns::of(list.iter().map(|r| (r.time.as_str(), r.hook.as_str())));
            if turns.is_empty() {
                continue;
            }
            c.turns += turns.len() as i64;
            let mut cost = vec![0.0f64; turns.len()];
            for (time, v) in &events {
                cost[turns.index(time)] += v;
            }
            per_turn.extend(cost);
        } else {
            per_agent.push(events.iter().map(|(_, v)| v).sum());
        }
    }
    c.per_turn = spread(per_turn);
    c.per_agent = spread(per_agent);
    Ok(c)
}

/// The floor wording of the net line, while any cost is unseen.
pub const FLOOR: &str = "counts only costs atlas can see; the true net is lower";

/// One row of the net table: a tool's credit (the ledger, unchanged) and
/// its cost, in tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct NetRow {
    /// The tool, or the bucket A row.
    pub key: String,
    /// Credited savings.
    pub credit: f64,
    /// Measured cost.
    pub cost: f64,
}

impl NetRow {
    /// Credit minus cost.
    pub fn net(&self) -> f64 {
        self.credit - self.cost
    }
}

/// The name of the bucket A row in the net table.
pub const DEFINITIONS: &str = "tool definitions (A)";

/// The net table: every ledger row but the double logs, credit beside cost
/// (C for a tool, B for a `hook:<event>` row), then bucket A, which no tool
/// row holds, and the measured rulebook ([`RULEBOOK`]) when there is one.
/// Totals are the sums of these rows. No source is credited twice: a
/// measured rulebook replaces the `hook:SubagentStart` row's hit credit
/// (#217), which then credits 0; without one, the hit credit stands. The
/// measured rulebook is credited less the recall-by-record credit
/// ([`Cost::rulebook_overlap`]), which may read the same rulebook windows.
pub fn net_rows(cost: &Cost, by_tool: &[crate::tokens::ReportRow]) -> Vec<NetRow> {
    use crate::token_report::{DOUBLE_LOGGED, ROUTED};
    let mut rows: Vec<NetRow> = Vec::new();
    for t in by_tool.iter().filter(|t| t.key != DOUBLE_LOGGED && t.key != ROUTED) {
        let c = match t.key.strip_prefix("hook:") {
            Some(event) => cost.hook(event),
            None => cost.output.get(&t.key).copied().unwrap_or(0.0),
        };
        // A measured rulebook replaces the subagent-start hit credit: both
        // credit what those injections saved, so only one may count.
        let credit = if t.key == START_HOOK && cost.rulebook.is_some() { 0.0 } else { t.counterfactual };
        rows.push(NetRow { key: t.key.clone(), credit, cost: c });
    }
    // Cost with no ledger row (a hook event that never logged a row).
    for event in cost.sent.keys().chain(cost.framing.keys()).chain(cost.reread_priced.keys()) {
        let key = format!("hook:{event}");
        if !rows.iter().any(|r| r.key == key) {
            rows.push(NetRow { key, credit: 0.0, cost: cost.hook(event) });
        }
    }
    for (tool, c) in &cost.output {
        if !rows.iter().any(|r| &r.key == tool) {
            rows.push(NetRow { key: tool.clone(), credit: 0.0, cost: *c });
        }
    }
    rows.push(NetRow { key: DEFINITIONS.into(), credit: 0.0, cost: cost.a() });
    // Recall by key or lane may credit the same rulebook windows: the
    // rulebook row is reduced by that credit, so neither counts it twice.
    if let Some(credit) = cost.rulebook_net_credit() {
        rows.push(NetRow { key: RULEBOOK.into(), credit, cost: 0.0 });
    }
    rows
}

/// The name of the measured rulebook's row in the net table (#219): its
/// credit only; the injections that replace it are the `hook:SubagentStart`
/// row's cost.
pub const RULEBOOK: &str = "rulebook (measured)";

/// The subagent-start hook's ledger row, whose hit credit a measured
/// rulebook replaces.
pub const START_HOOK: &str = "hook:SubagentStart";

/// Total credit, total cost and the net, over [`net_rows`].
pub fn totals(rows: &[NetRow]) -> (f64, f64, f64) {
    let credit: f64 = rows.iter().map(|r| r.credit).sum();
    let cost: f64 = rows.iter().map(|r| r.cost).sum();
    (credit, cost, credit - cost)
}

/// How much of the store the transcripts covered.
fn coverage(cost: &Cost) -> String {
    if cost.transcripts {
        format!(
            "{} of {} sessions and subagents without a transcript",
            cost.without_transcript,
            cost.with_transcript + cost.without_transcript
        )
    } else {
        "no transcripts read: tool search loads and re-reads not counted".to_string()
    }
}

/// The share of a total, in percent.
fn share(v: f64, total: f64) -> String {
    if total > 0.0 {
        format!("{:.1}%", v / total * 100.0)
    } else {
        "--".into()
    }
}

/// The formula line.
fn formula(bytes_per_token: f64) -> String {
    format!(
        "net = credited savings (the ledger, unchanged) - measured cost (A tool definitions + B hook injections + C tool output), all in tokens at {bytes_per_token} bytes per token"
    )
}

/// The net block the human report shows after its headline.
pub fn net_block(cost: &Cost, by_tool: &[crate::tokens::ReportRow], bytes_per_token: f64) -> String {
    use crate::token_report::{block, compact};
    let (credit, c, net) = totals(&net_rows(cost, by_tool));
    let net = if cost.floor() { format!("{} ({FLOOR}; {})", compact(net), coverage(cost)) } else { compact(net) };
    block("Net", &formula(bytes_per_token), &[("credit", compact(credit)), ("cost", compact(c)), ("net", net)])
}

/// The hook events any bucket B map names.
fn events(cost: &Cost) -> std::collections::BTreeSet<&String> {
    cost.sent.keys().chain(cost.framing.keys()).chain(cost.reread.keys()).chain(cost.reread_priced.keys()).collect()
}

/// The cost section of the human report.
pub fn human(cost: &Cost, by_tool: &[crate::tokens::ReportRow], bytes_per_token: f64) -> String {
    use crate::token_report::{block, compact, table, Align};
    use std::fmt::Write as _;
    let total = cost.total();
    let mut out = String::from("Cost of atlas\n");
    let _ = writeln!(out, "  What atlas itself put into context, from bytes (no model estimates), at {bytes_per_token} bytes per token.");
    let raw: f64 = sum_of(&cost.reread);
    let priced: f64 = sum_of(&cost.reread_priced);
    let sent: f64 = sum_of(&cost.sent);
    let framing: f64 = sum_of(&cost.framing);
    let line = |b: &str, what: &str, v: f64| vec![b.to_string(), what.to_string(), compact(v), share(v, total)];
    let rows = vec![
        line("A", "session starts (tool list and instructions)", cost.start_tokens),
        line("A", "tool search loads", cost.load_tokens),
        line("B", "hook injections sent", sent),
        line("B", "hook text no injection row holds", framing),
        line("B", "re-read, priced", priced),
        line("C", "tool output (all sessions)", cost.c()),
        line("", "total", total),
    ];
    let al = [Align::Left, Align::Left, Align::Right, Align::Right];
    out.push_str(&table(&["bucket", "what", "tokens", "share"], &al, &rows));
    let _ = writeln!(
        out,
        "  re-read: {} tokens raw, {} priced (each request's atlas share spread over its input, cache-read and cache-write tokens by their proportions); {} of {} re-read requests priced at the full rate (no usage block, or a model not in the price table)",
        compact(raw),
        compact(priced),
        cost.full_rate,
        cost.priced
    );
    out.push('\n');

    out.push_str("By hook event (B)\n");
    let rows: Vec<Vec<String>> = events(cost)
        .iter()
        .map(|e| {
            let g = |m: &BTreeMap<String, f64>| m.get(*e).copied().unwrap_or(0.0);
            vec![
                (*e).clone(),
                compact(g(&cost.sent)),
                compact(g(&cost.framing)),
                compact(g(&cost.reread)),
                compact(g(&cost.reread_priced)),
                share(cost.hook(e), total),
            ]
        })
        .collect();
    let al = [Align::Left, Align::Right, Align::Right, Align::Right, Align::Right, Align::Right];
    out.push_str(&table(&["event", "sent", "other text", "re-read raw", "re-read priced", "share"], &al, &rows));
    out.push('\n');

    out.push_str("Tool output (C), not attributable to a session\n");
    let mut c: Vec<(&String, &f64)> = cost.output.iter().collect();
    c.sort_by(|a, b| b.1.total_cmp(a.1).then(a.0.cmp(b.0)));
    let rows: Vec<Vec<String>> = c.iter().map(|(t, v)| vec![(*t).clone(), compact(**v), share(**v, total)]).collect();
    out.push_str(&table(&["tool", "tokens", "share"], &[Align::Left, Align::Right, Align::Right], &rows));
    out.push('\n');

    let spread = |s: &Spread| format!("mean {}, p50 {}, p95 {}, max {}", compact(s.mean), compact(s.p50), compact(s.p95), compact(s.max));
    out.push_str(&block(
        "Per turn and per agent (A and B)",
        "A turn is a distinct second of a main session's UserPromptSubmit rows; a prompt that injected nothing leaves no row, so turns are undercounted and the cost per turn errs high. The store cannot yet split subagents into builder, reviewer and research.",
        &[
            ("main sessions", format!("{} turns: {}", cost.turns, spread(&cost.per_turn))),
            ("subagents", format!("{} agents: {}", cost.per_agent.n, spread(&cost.per_agent))),
            ("transcripts", coverage(cost)),
        ],
    ));
    out.push('\n');

    let mut rows = net_rows(cost, by_tool);
    rows.sort_by(|a, b| a.net().total_cmp(&b.net()).then(a.key.cmp(&b.key)));
    let (credit, c, net) = totals(&rows);
    out.push_str("Net by tool\n");
    let _ = writeln!(out, "  {}.", formula(bytes_per_token));
    let mut cells: Vec<Vec<String>> =
        rows.iter().map(|r| vec![r.key.clone(), compact(r.credit), compact(r.cost), compact(r.net())]).collect();
    cells.push(vec!["total".into(), compact(credit), compact(c), compact(net)]);
    out.push_str(&table(&["tool", "credit", "cost", "net"], &[Align::Left, Align::Right, Align::Right, Align::Right], &cells));
    if cost.floor() {
        let _ = writeln!(out, "  net ({FLOOR}): {}.", coverage(cost));
    }
    out
}

/// The cost lines `--tsv` appends after the report's own lines.
pub fn tsv(cost: &Cost, by_tool: &[crate::tokens::ReportRow], bytes_per_token: f64) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "{}", formula(bytes_per_token));
    let _ = writeln!(
        out,
        "cost\tA starts {} ({:.0} tokens)\tA loads {} ({:.0} tokens)\tB sent {:.0}\tB other text {:.0}\tB re-read raw {:.0}\tB re-read priced {:.0}\tC output {:.0}\ttotal {:.0}",
        cost.starts,
        cost.start_tokens,
        cost.loads,
        cost.load_tokens,
        sum_of(&cost.sent),
        sum_of(&cost.framing),
        sum_of(&cost.reread),
        sum_of(&cost.reread_priced),
        cost.c(),
        cost.total()
    );
    let _ = writeln!(out, "re-read requests\tpriced {}\tat the full rate {}", cost.priced, cost.full_rate);
    let _ = writeln!(out, "transcripts\tread {}\twith {}\twithout {}", cost.transcripts, cost.with_transcript, cost.without_transcript);
    let s = |name: &str, n: usize, p: &Spread| {
        format!("{name}\tn {n}\tmean {:.0}\tp50 {:.0}\tp95 {:.0}\tmax {:.0}", p.mean, p.p50, p.p95, p.max)
    };
    let _ = writeln!(out, "{}", s("cost per turn (main, A+B)", cost.turns as usize, &cost.per_turn));
    let _ = writeln!(out, "{}", s("cost per agent (subagents, A+B)", cost.per_agent.n, &cost.per_agent));
    let _ = writeln!(out, "event\tsent\tother text\tre-read raw\tre-read priced");
    for e in events(cost) {
        let g = |m: &BTreeMap<String, f64>| m.get(e).copied().unwrap_or(0.0);
        let _ = writeln!(out, "{e}\t{:.0}\t{:.0}\t{:.0}\t{:.0}", g(&cost.sent), g(&cost.framing), g(&cost.reread), g(&cost.reread_priced));
    }
    let _ = writeln!(out, "net tool\tcredit\tcost\tnet");
    let rows = net_rows(cost, by_tool);
    for r in &rows {
        let _ = writeln!(out, "{}\t{:.0}\t{:.0}\t{:.0}", r.key, r.credit, r.cost, r.net());
    }
    let (credit, c, net) = totals(&rows);
    let label = if cost.floor() { format!("net ({FLOOR})") } else { "net".to_string() };
    let _ = writeln!(out, "{label}\tcredit {credit:.0}\tcost {c:.0}\tnet {net:.0}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::ReportRow;

    fn store() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::tokens::TOKEN_TABLE).unwrap();
        c
    }

    fn inject(c: &Connection, time: &str, session: &str, agent: &str, transcript: &str, hook: &str, bytes: i64) {
        c.execute(
            "INSERT INTO hook_inject(time, session, agent, transcript, hook, key, bytes, read) VALUES (?1, ?2, ?3, ?4, ?5, 'k', ?6, 0)",
            rusqlite::params![time, session, agent, transcript, hook, bytes],
        )
        .unwrap();
    }

    fn log(c: &Connection, time: &str, tool: &str, returned: i64) {
        c.execute(
            "INSERT INTO token_log(time, day, tool, returned, grep, read) VALUES (?1, substr(?1, 1, 10), ?2, ?3, NULL, NULL)",
            rusqlite::params![time, tool, returned],
        )
        .unwrap();
    }

    /// A fresh folder for synthetic transcripts.
    fn folder(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("atlas-token-cost-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn request(time: &str, id: &str, model: &str, usage: &str) -> String {
        let usage = if usage.is_empty() { String::new() } else { format!(",\"usage\":{usage}") };
        format!("{{\"type\":\"assistant\",\"timestamp\":\"{time}\",\"message\":{{\"id\":\"{id}\",\"model\":\"{model}\"{usage}}}}}")
    }

    fn boundary(time: &str) -> String {
        format!("{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"timestamp\":\"{time}\"}}")
    }

    fn load(time: &str, tool: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"timestamp\":\"{time}\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"content\":[{{\"type\":\"tool_reference\",\"tool_name\":\"{tool}\"}}]}}]}}}}"
        )
    }

    const OPUS: &str = "claude-opus-5-5";

    #[test]
    fn two_session_starts_cost_twice_the_start_bytes_and_a_tool_search_load_adds_its_schema() {
        let c = store();
        let dir = folder("a");
        let t = dir.join("main.jsonl");
        std::fs::write(
            &t,
            [
                request("2026-11-15T10:00:01Z", "m1", OPUS, ""),
                load("2026-11-15T10:00:02Z", "mcp__atlas__recall"),
                load("2026-11-15T10:00:03Z", "mcp__other__thing"),
            ]
            .join("\n"),
        )
        .unwrap();
        let t = t.to_string_lossy().into_owned();
        inject(&c, "2026-11-15T10:00:00Z", "s", "", &t, "UserPromptSubmit", 0);
        inject(&c, "2026-11-15T10:00:00Z", "s", "agent1", "", "SubagentStart", 0);
        log(&c, "2026-11-15T11:00:00Z", "recall", 0);
        let start = start_bytes() as f64 / 4.0;
        let none = measure(&c, 4.0, None).unwrap();
        assert_eq!(none.starts, 2);
        assert_eq!(none.start_tokens, 2.0 * start);
        assert_eq!((none.loads, none.load_tokens), (0, 0.0));
        let with = measure(&c, 4.0, Some(dir.as_path())).unwrap();
        assert!(tool_bytes("recall") > 0);
        assert_eq!((with.loads, with.load_tokens), (1, tool_bytes("recall") as f64 / 4.0));
        assert_eq!(with.a(), 2.0 * start + tool_bytes("recall") as f64 / 4.0);
        // The subagent's transcript was not given: one scope of two is unseen.
        assert_eq!((with.with_transcript, with.without_transcript), (1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_bytes_sum_per_event_and_turns_are_distinct_prompt_seconds() {
        let c = store();
        for (time, hook, bytes) in [
            ("2026-11-15T10:00:00.100Z", "UserPromptSubmit", 400),
            ("2026-11-15T10:00:00.900Z", "UserPromptSubmit", 400),
            ("2026-11-15T10:00:05Z", "UserPromptSubmit", 800),
            ("2026-11-15T10:00:01Z", "PreToolUse", 40),
            ("2026-11-15T10:00:02Z", "PreToolUse", 40),
            ("2026-11-15T09:59:00Z", "SessionStart", 4000),
        ] {
            inject(&c, time, "s", "", "", hook, bytes);
        }
        inject(&c, "2026-11-15T10:00:03Z", "s", "a1", "", "SubagentStart", 1200);
        let cost = measure(&c, 4.0, None).unwrap();
        let sent: Vec<(&str, f64)> = cost.sent.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(sent, [("PreToolUse", 20.0), ("SessionStart", 1000.0), ("SubagentStart", 300.0), ("UserPromptSubmit", 400.0)]);
        // Three UserPromptSubmit rows in two distinct seconds: two turns.
        assert_eq!(cost.turns, 2);
        assert_eq!(cost.per_agent.n, 1);
        // The main session's A and B fall in its two turns: their sum is
        // its start plus everything it was sent.
        let start = start_bytes() as f64 / 4.0;
        assert!((cost.per_turn.mean * 2.0 - (start + 100.0 + 100.0 + 200.0 + 10.0 + 10.0 + 1000.0)).abs() < 1e-6, "{cost:?}");
        assert_eq!(cost.per_agent.max, start + 300.0);
    }

    #[test]
    fn an_injection_is_read_again_until_the_next_compaction() {
        let c = store();
        let dir = folder("b");
        let t = dir.join("main.jsonl");
        // The first request after the injection sends it; the next two read
        // it again; the compaction drops it, so the last one does not.
        let lines = [
            request("2026-11-15T10:00:01Z", "m1", "model-not-in-the-table", ""),
            request("2026-11-15T10:00:02Z", "m2", "model-not-in-the-table", ""),
            request("2026-11-15T10:00:03Z", "m3", "model-not-in-the-table", ""),
            boundary("2026-11-15T10:00:04Z"),
            request("2026-11-15T10:00:05Z", "m4", "model-not-in-the-table", ""),
        ];
        std::fs::write(&t, lines.join("\n")).unwrap();
        inject(&c, "2026-11-15T10:00:00Z", "s", "", &t.to_string_lossy(), "UserPromptSubmit", 400);
        log(&c, "2026-11-15T11:00:00Z", "recall", 0);
        let cost = measure(&c, 4.0, Some(dir.as_path())).unwrap();
        assert_eq!(cost.reread.get("UserPromptSubmit"), Some(&200.0));
        // Neither request has a usage block: both priced at the full rate.
        assert_eq!(cost.reread_priced.get("UserPromptSubmit"), Some(&200.0));
        assert_eq!((cost.priced, cost.full_rate), (2, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_re_read_is_priced_by_its_requests_cache_mix() {
        let usage = Usage { input: 100, cache_read: 1000, write_5m: 200, write_1h: 100 };
        // Opus 5.5: cache read 0.05, 5m write 1.25, 1h write 2.
        let opus = Request { time: String::new(), model: OPUS.into(), usage: Some(usage) };
        let (w, full) = opus.weight();
        assert!(!full);
        assert!((w - (100.0 + 1000.0 * 0.05 + 200.0 * 1.25 + 100.0 * 2.0) / 1400.0).abs() < 1e-12, "{w}");
        // Another model in the table uses its own ratios (cache read 0.1).
        let haiku = Request { model: "claude-haiku-4-5-20251001".into(), ..opus.clone() };
        assert!((haiku.weight().0 - (100.0 + 1000.0 * 0.1 + 200.0 * 1.25 + 100.0 * 2.0) / 1400.0).abs() < 1e-12);
        // No usage block: the full rate, flagged.
        assert_eq!(Request { usage: None, ..opus.clone() }.weight(), (1.0, true));
        // A model the table does not have: the full rate, flagged.
        assert_eq!(Request { model: "some-other-model".into(), ..opus }.weight(), (1.0, true));
    }

    #[test]
    fn a_transcripts_usage_split_is_read_as_billed() {
        let dir = folder("c");
        let t = dir.join("x.jsonl");
        let usage = "{\"input_tokens\":100,\"cache_read_input_tokens\":1000,\"cache_creation_input_tokens\":300,\"cache_creation\":{\"ephemeral_5m_input_tokens\":200,\"ephemeral_1h_input_tokens\":100}}";
        let lines = [
            request("2026-11-15T10:00:01Z", "m1", OPUS, usage),
            // A streamed message repeats its id: one request.
            request("2026-11-15T10:00:01Z", "m1", OPUS, usage),
            request("2026-11-15T10:00:02Z", "m2", OPUS, "{\"input_tokens\":5,\"cache_creation_input_tokens\":7}"),
            request("2026-11-15T10:00:03Z", "m3", "<synthetic>", ""),
            request("2026-11-15T12:00:00Z", "m4", OPUS, ""),
        ];
        std::fs::write(&t, lines.join("\n")).unwrap();
        let tr = read_transcript(&t, "2026-11-15T11:00:00Z").unwrap();
        let got: Vec<Option<Usage>> = tr.requests.iter().map(|r| r.usage).collect();
        assert_eq!(
            got,
            [
                Some(Usage { input: 100, cache_read: 1000, write_5m: 200, write_1h: 100 }),
                // Without the split, every write counts at the 5-minute price.
                Some(Usage { input: 5, cache_read: 0, write_5m: 7, write_1h: 0 }),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_output_sums_per_tool_without_the_double_logs() {
        let c = store();
        for (tool, b) in [("recall", 400), ("search", 800), ("search", 800), ("hook", 999), ("hook:PostToolUse", 999)] {
            log(&c, "2026-11-15T10:00:00Z", tool, b);
        }
        let cost = measure(&c, 4.0, None).unwrap();
        let got: Vec<(&str, f64)> = cost.output.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(got, [("recall", 100.0), ("search", 400.0)]);
        assert_eq!(cost.c(), 500.0);
    }

    fn row(key: &str, cf: f64) -> ReportRow {
        ReportRow { key: key.into(), answers: 1, returned: 0.0, counterfactual: cf, returned_comparable: 0.0 }
    }

    #[test]
    fn a_row_with_cost_and_no_credit_nets_negative_and_the_net_is_credit_minus_cost() {
        let mut cost = Cost { transcripts: true, ..Cost::default() };
        cost.output.insert("journal".into(), 50.0);
        cost.output.insert("recall".into(), 100.0);
        cost.sent.insert("UserPromptSubmit".into(), 30.0);
        cost.start_tokens = 20.0;
        let by_tool = [
            row("journal", 0.0),
            row("recall", 1000.0),
            row("hook:UserPromptSubmit", 90.0),
            row("hook", 5.0),
            row("hook:PostToolUse", 7.0),
        ];
        let rows = net_rows(&cost, &by_tool);
        let journal = rows.iter().find(|r| r.key == "journal").unwrap();
        assert_eq!(journal.net(), -50.0);
        assert_eq!(rows.iter().find(|r| r.key == "hook:UserPromptSubmit").unwrap().net(), 60.0);
        assert!(!rows.iter().any(|r| r.key == "hook" || r.key == "hook:PostToolUse"));
        let (credit, c, net) = totals(&rows);
        assert_eq!((credit, c, net), (1090.0, 200.0, 890.0));
        assert_eq!(c, cost.total());
        let t = tsv(&cost, &by_tool, 4.0);
        assert!(t.ends_with("net\tcredit 1090\tcost 200\tnet 890\n"), "{t}");
    }

    #[test]
    fn the_net_is_labelled_a_floor_until_transcripts_cover_every_session() {
        let by_tool = [row("recall", 1000.0)];
        let without = Cost::default();
        let partial = Cost { transcripts: true, with_transcript: 3, without_transcript: 1, ..Cost::default() };
        let full = Cost { transcripts: true, with_transcript: 4, ..Cost::default() };
        for cost in [&without, &partial] {
            for out in [human(cost, &by_tool, 4.0), tsv(cost, &by_tool, 4.0), net_block(cost, &by_tool, 4.0)] {
                assert!(out.contains(FLOOR), "{out}");
            }
        }
        assert!(net_block(&partial, &by_tool, 4.0).contains("1 of 4 sessions and subagents without a transcript"));
        // An empty bucket prints 0, not the -0 of an empty f64 sum.
        assert!(!tsv(&without, &by_tool, 4.0).contains("-0"), "{}", tsv(&without, &by_tool, 4.0));
        for out in [human(&full, &by_tool, 4.0), tsv(&full, &by_tool, 4.0), net_block(&full, &by_tool, 4.0)] {
            assert!(!out.contains(FLOOR), "{out}");
            assert!(!out.contains("upper bound") && !out.contains("upper-bound"), "{out}");
        }
    }

    /// #212: the turn rule, one helper for the cost per turn and the
    /// injections per prompt. Only UserPromptSubmit rows make turns, one per
    /// distinct second; a time falls in the last turn at or before it, and
    /// one before the first prompt in the first.
    #[test]
    fn the_turn_rule_is_distinct_prompt_seconds_and_a_row_falls_in_the_last_turn_before_it() {
        let rows = [
            ("2026-11-15T10:00:00.100Z", "UserPromptSubmit"),
            ("2026-11-15T10:00:00.900Z", "UserPromptSubmit"),
            ("2026-11-15T10:00:05Z", "UserPromptSubmit"),
            ("2026-11-15T10:00:02Z", "PreToolUse"),
            ("2026-11-15T09:59:00Z", "SessionStart"),
        ];
        let turns = Turns::of(rows);
        assert_eq!((turns.len(), turns.is_empty()), (2, false));
        assert_eq!(turns.index("2026-11-15T09:59:00Z"), 0);
        assert_eq!(turns.index("2026-11-15T10:00:00.500Z"), 0);
        assert_eq!(turns.index("2026-11-15T10:00:04.999Z"), 0);
        assert_eq!(turns.index("2026-11-15T10:00:05.700Z"), 1);
        assert_eq!(turns.index("2026-11-16T00:00:00Z"), 1);
        assert!(Turns::of([("2026-11-15T10:00:00Z", "PreToolUse")]).is_empty());
    }

    /// #219: the measured rulebook is one row of the net table, credit only,
    /// and in the totals; without a measurement there is no such row.
    #[test]
    fn the_measured_rulebook_is_a_net_row_and_counts_in_the_totals() {
        let by_tool = [row("recall", 1000.0)];
        let mut cost = Cost { transcripts: true, ..Cost::default() };
        assert!(!net_rows(&cost, &by_tool).iter().any(|r| r.key == RULEBOOK));
        cost.rulebook = Some(500.0);
        let rows = net_rows(&cost, &by_tool);
        let r = rows.iter().find(|r| r.key == RULEBOOK).unwrap();
        assert_eq!((r.credit, r.cost), (500.0, 0.0));
        assert_eq!(totals(&rows).0, 1500.0);
        assert!(tsv(&cost, &by_tool, 4.0).contains("rulebook (measured)\t500\t0\t500\n"));
    }

    /// Review of #277, finding 1: recall by key credits the window of the
    /// rulebook a record sits in, and the measured rulebook credits the
    /// same reading, so the rulebook row is reduced by the recall-by-record
    /// credit, never below 0; the recall row keeps its ledger credit.
    #[test]
    fn recall_by_record_is_taken_off_the_rulebook_once() {
        let by_tool = [row("recall", 1000.0)];
        for (rulebook, overlap, want) in [(500.0, 200.0, 300.0), (500.0, 900.0, 0.0), (500.0, 0.0, 500.0)] {
            let mut cost = Cost { transcripts: true, ..Cost::default() };
            let m = crate::tokens::MatchRow { agents: 5, rulebook: Some(rulebook), whole: 0.0, injected: 0.0, start: 0.0 };
            cost.credit_rulebook(Some(&m), overlap);
            let rows = net_rows(&cost, &by_tool);
            let r = rows.iter().find(|r| r.key == RULEBOOK).unwrap();
            assert_eq!(r.credit, want, "rulebook {rulebook}, recall by record {overlap}");
            assert_eq!(rows.iter().find(|r| r.key == "recall").unwrap().credit, 1000.0);
            assert_eq!(totals(&rows).0, 1000.0 + want);
            assert_eq!(cost.rulebook_taken_off(), rulebook - want);
        }
        // The wiring: no match, no rulebook row, whatever recall credited.
        let mut cost = Cost::default();
        cost.credit_rulebook(None, 900.0);
        assert_eq!((cost.rulebook, cost.rulebook_taken_off()), (None, 0.0));
        assert!(!net_rows(&cost, &by_tool).iter().any(|r| r.key == RULEBOOK));
    }
}
