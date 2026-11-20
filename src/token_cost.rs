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
//!   tool search (each `tool_reference` naming one). With transcripts, its
//!   re-read too (#297): the definitions sit in every request's prefix and
//!   no compaction drops them, so the start is re-read by every request of
//!   its transcript after the first, and a load by every one after it.
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
//!   total for all sessions, never per session or turn. With transcripts,
//!   its re-read too (#294): each ledger row is matched to the answer or
//!   routed digest a transcript recorded ([`match_answers`]: tool, time and
//!   size), and carried as an injection is.
//!
//! **Both sides re-read alike (#294).** Text that stays in context is
//! re-sent with each later request, and so would the counterfactual have
//! been: grep output, whole files, raw tool output. So with transcripts each
//! credited answer's counterfactual is carried by the same multiplier
//! ([`carried`]) as the answer: a ledger row's by its matched answer's, a
//! hook hit's by its injection's, the measured rulebook's by each
//! subagent's first SubagentStart injection's. An answer with no transcript
//! data counts once on both sides ([`once_only`]), and the report says how
//! much. The headline net has re-reads on both sides; the once-only net
//! stays beside it. Only the hook text no injection row holds stays
//! once-only on the cost side: it carries no time to price a re-read from.
//!
//! **What kind of number the net is (#297).** The counterfactual is
//! assumed to be carried as long as the answer, with three corrections: it
//! stops at the counterfactual session's own earlier compaction
//! ([`early_compactions`]), a hook hit's read enters when it was used, and a
//! repeated lookup's Read is Claude Code's "file unchanged" stub. What is
//! still unpriced or estimated is named beside the net, each with its size
//! where one is known, and the net says which side is larger ([`bound`]).
//!
//! **The compaction simulation counts what Net credits (S-c).** The excess
//! that makes a counterfactual session compact early is the excess of the
//! credits Net counts: with a measured rulebook, Net credits the rulebook
//! at each measured subagent's start and SubagentStart hits 0, so the
//! rulebook's excess over those injections is in and the hits' is out;
//! without one, the hits' is in. Both are in or both out, never one alone.
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

use crate::db::Connection;

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
    /// The context it sent, in tokens (uncached input, cache reads and
    /// cache writes), when the transcript has its usage (#297, S1).
    pub fn size(&self) -> Option<u64> {
        self.usage.map(|u| u.input + u.cache_read + u.write_5m + u.write_1h)
    }
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
    /// The context sizes (`preTokens`) its automatic compactions fired at
    /// (#297, S1): where a counterfactual session would compact too.
    pub compact_at: Vec<u64>,
    /// Atlas tools loaded through tool search: (time, tool name after the
    /// last `__`).
    pub loads: Vec<(String, String)>,
    /// The tool results atlas put there, oldest first (#294).
    pub answers: Vec<Answer>,
}

/// One tool result in a transcript that atlas wrote: an atlas tool's answer,
/// or a routed digest in place of a tool's output (#294).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Answer {
    /// When the result was recorded.
    pub time: String,
    /// The atlas tool (its name after the last `__`), or `route:<Tool>` for
    /// a digest, as the ledger names its row.
    pub tool: String,
    /// The result's text bytes.
    pub bytes: usize,
    /// The tool call's id (#297, S4): a routed Read's ledger row records it.
    pub tool_use: String,
    /// Whether the call named an `op`: a listed tool whose operation logs
    /// under its own tool's name (`note` op `read` as `read_note`).
    pub op: bool,
    /// Whether it repeats an answer already in the same context (#297, S2):
    /// atlas's `unchanged since` reply, or the same tool and input as an
    /// earlier call since the last compaction. Its counterfactual Read of
    /// the same files would return Claude Code's "file unchanged" stub.
    pub repeat: bool,
}

/// Bytes of the result Claude Code gives a Read of a file already read,
/// unchanged, in the same context ("File unchanged since last read", about
/// 160 bytes): what a repeated counterfactual Read adds (#297, S2).
pub const READ_STUB_BYTES: i64 = 160;

/// Whether a tool result's text is a routed digest: it opens with the
/// handle `h:` and ten hex digits, then a space ([`crate::digest::digest`]),
/// after at most a Read's line-number prefix.
fn is_digest(text: &str) -> bool {
    let head = text.trim_start_matches(|c: char| c.is_ascii_whitespace() || c.is_ascii_digit() || c == '\u{2192}');
    let Some(rest) = head.strip_prefix("h:") else { return false };
    let b = rest.as_bytes();
    b.len() > 10 && b[..10].iter().all(u8::is_ascii_hexdigit) && b[10] == b' '
}

/// The text of a tool result's `content`: a string, or the text items of an
/// array.
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect(),
        _ => String::new(),
    }
}

/// The `tool_use_id`s a raw transcript line names, without parsing it.
fn result_ids(line: &str) -> impl Iterator<Item = &str> {
    const KEY: &str = "\"tool_use_id\":\"";
    line.match_indices(KEY).filter_map(move |(i, _)| {
        let rest = &line[i + KEY.len()..];
        rest.find('"').map(|end| &rest[..end])
    })
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
    // Tool calls by id: their tool names, inputs and whether they name an
    // op, for the results that follow.
    let mut uses: HashMap<String, (String, String, bool)> = HashMap::new();
    // Atlas calls since the last compaction, by tool and input (#297, S2).
    let mut asked: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for line in text.lines() {
        let assistant = line.contains("\"assistant\"");
        let boundary = line.contains("\"compact_boundary\"");
        let load = line.contains("\"tool_reference\"");
        // A result worth parsing: an atlas tool's, or a digest's.
        let result = line.contains("\"tool_result\"")
            && (line.contains("\"content\":\"h:")
                || line.contains("\"text\":\"h:")
                || line.contains("\u{2192}h:")
                || line.contains("\\u2192h:")
                || result_ids(line).any(|id| uses.get(id).is_some_and(|(n, _, _)| atlas_tool(n).is_some())));
        if !(assistant || boundary || load || result) {
            continue;
        }
        let Ok(v) = crate::json::parse(line) else { continue };
        let time = v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        if !until.is_empty() && time.as_str() > until {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
                t.boundaries.push(time);
                asked.clear();
                let meta = v.get("compactMetadata");
                if meta.and_then(|m| m.get("trigger")).and_then(Value::as_str) == Some("auto") {
                    if let Some(n) = meta.and_then(|m| m.get("preTokens")).and_then(Value::as_f64).filter(|n| *n > 0.0) {
                        t.compact_at.push(n as u64);
                    }
                }
            }
            Some("assistant") => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                let model = m.get("model").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(Value::Array(items)) = m.get("content") {
                    for x in items.iter().filter(|x| x.get("type").and_then(Value::as_str) == Some("tool_use")) {
                        if let (Some(id), Some(name)) = (x.get("id").and_then(Value::as_str), x.get("name").and_then(Value::as_str)) {
                            let input = x.get("input");
                            let op = input.and_then(|i| i.get("op")).is_some();
                            uses.insert(id.to_string(), (name.to_string(), input.map(Value::to_string).unwrap_or_default(), op));
                        }
                    }
                }
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
                    if x.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let id = x.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                        let (name, input, op) = uses.get(id).map_or(("", "", false), |(n, i, o)| (n.as_str(), i.as_str(), *o));
                        let text = result_text(x.get("content"));
                        let (tool, repeat) = match atlas_tool(name) {
                            Some(tool) => {
                                let again = !asked.insert((tool.to_string(), input.to_string()));
                                (Some(tool.to_string()), again || text.starts_with("unchanged since"))
                            }
                            None if !name.is_empty() && is_digest(&text) => (Some(format!("route:{name}")), false),
                            None => (None, false),
                        };
                        if let Some(tool) = tool {
                            t.answers.push(Answer { time: time.clone(), tool, bytes: text.len(), tool_use: id.to_string(), op, repeat });
                        }
                    }
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
    t.answers.sort_by(|a, b| a.time.cmp(&b.time));
    Some(t)
}

/// The re-reads of text put into context at `time` (an injection, a tool
/// answer, a digest): the requests after the first one that follows it, up
/// to the next compaction boundary (or the end). The first request is the
/// send, counted as sent.
pub fn rereads<'t>(t: &'t Transcript, time: &str) -> impl Iterator<Item = &'t Request> + 't {
    let end = t.boundaries.iter().find(|b| b.as_str() > time).cloned();
    let time = time.to_string();
    t.requests.iter().filter(move |r| r.time > time && end.as_ref().is_none_or(|e| r.time < *e)).skip(1)
}

/// How text put into context at `time` was carried (#294): the re-reads
/// ([`rereads`]) as a multiplier, the one `f` both sides of the ledger are
/// priced by. Any amount `x` of tokens there costs `x * priced` re-read,
/// whether it is atlas's answer or the counterfactual it is credited with.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Carried {
    /// Re-read requests.
    pub n: i64,
    /// Of them, priced at the full rate ([`Request::weight`]).
    pub full: i64,
    /// The sum of their price weights: priced re-read tokens per token.
    pub priced: f64,
}

/// [`Carried`] for text put into context at `time`.
pub fn carried(t: &Transcript, time: &str) -> Carried {
    carried_until(t, time, None)
}

/// [`Carried`] for text put into context at `time` that leaves it at `end`
/// at the latest (a counterfactual session's own earlier compaction, #297
/// S1): the re-reads before `end`.
pub fn carried_until(t: &Transcript, time: &str, end: Option<&str>) -> Carried {
    sum_carried(rereads(t, time).filter(|q| end.is_none_or(|e| q.time.as_str() < e)))
}

/// The re-reads of text no compaction drops (#297, B1): tool definitions
/// and instructions sit in every request's prefix, so text there since
/// `time` is re-read by every request after the first that follows it, to
/// the transcript's end.
pub fn through<'t>(t: &'t Transcript, time: &str) -> impl Iterator<Item = &'t Request> + 't {
    let time = time.to_string();
    t.requests.iter().filter(move |r| r.time > time).skip(1)
}

fn sum_carried<'t>(requests: impl Iterator<Item = &'t Request>) -> Carried {
    let mut c = Carried::default();
    for q in requests {
        let (w, full) = q.weight();
        c.n += 1;
        c.full += i64::from(full);
        c.priced += w;
    }
    c
}

/// The times a counterfactual session would have compacted before the real
/// one did (#297, S1). Its context is the real one plus the running excess
/// of each counterfactual over atlas's answer (`excess`: time, tokens), from
/// the last compaction; when a request's real context size plus that excess
/// reaches `limit` (the size the session compacts at), the counterfactual
/// compacts at that request: its excess is dropped, and its later context is
/// measured from that request's size. A real compaction resets both.
/// Requests without a usage block are not tested. No limit: none.
pub fn early_compactions(t: &Transcript, excess: &[(String, f64)], limit: Option<f64>) -> Vec<String> {
    let Some(limit) = limit else { return Vec::new() };
    let mut items: Vec<&(String, f64)> = excess.iter().collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let (mut i, mut b) = (0usize, 0usize);
    let (mut extra, mut base) = (0.0f64, 0.0f64);
    let mut out = Vec::new();
    for q in &t.requests {
        loop {
            let item = items.get(i).filter(|x| x.0 < q.time);
            let bound = t.boundaries.get(b).filter(|x| **x < q.time);
            match (item, bound) {
                (Some(x), Some(y)) if x.0 < *y => {
                    extra += x.1;
                    i += 1;
                }
                (_, Some(_)) => {
                    extra = 0.0;
                    base = 0.0;
                    b += 1;
                }
                (Some(x), None) => {
                    extra += x.1;
                    i += 1;
                }
                (None, None) => break,
            }
        }
        let Some(size) = q.size() else { continue };
        if extra > 0.0 && size as f64 - base + extra >= limit {
            out.push(q.time.clone());
            extra = 0.0;
            base = size as f64;
        }
    }
    out
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
    /// A: re-read tokens, raw (#297, B1): the start and each load carried
    /// by every later request of its transcript ([`through`]).
    pub start_reread: f64,
    /// A: re-read tokens, price-weighted.
    pub start_reread_priced: f64,
    /// Requests in the transcripts read, over the sessions and subagents
    /// with one: R, their mean, is [`Cost::requests_per_scope`].
    pub requests: i64,
    /// B: sent tokens by event, from `hook_inject`.
    pub sent: BTreeMap<String, f64>,
    /// B: hook text no `hook_inject` row holds, by event.
    pub framing: BTreeMap<String, f64>,
    /// B: re-read tokens by event, raw.
    pub reread: BTreeMap<String, f64>,
    /// B: re-read tokens by event, price-weighted (full-input-price tokens).
    pub reread_priced: BTreeMap<String, f64>,
    /// Re-read requests priced at the full rate (no usage, or a model not
    /// in [`PRICES`]), for A, B and C.
    pub full_rate: i64,
    /// Re-read requests priced, for A, B and C.
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
    /// C: re-read tokens by tool, raw (#294): each ledger row matched to an
    /// answer in a transcript ([`match_answers`]), carried like an injection.
    pub output_reread: BTreeMap<String, f64>,
    /// C: re-read tokens by tool, price-weighted.
    pub output_reread_priced: BTreeMap<String, f64>,
    /// The credit side's re-read by net row key (#294): each credited
    /// answer's counterfactual times the same multiplier as its own
    /// re-read ([`Carried`]); hook hits by their injection's, ledger rows by
    /// their matched answer's.
    pub credit_reread: BTreeMap<String, f64>,
    /// Credit of answers with no transcript data, by net row key: counted
    /// once (#294).
    pub once_credit: BTreeMap<String, f64>,
    /// Cost of answers with no transcript data, by net row key: counted
    /// once (hook injections sent, tool output returned).
    pub once_cost: BTreeMap<String, f64>,
    /// Ledger rows matched to an answer in a transcript.
    pub matched: i64,
    /// Of [`Cost::matched`], matched by equal bytes.
    pub matched_exact: i64,
    /// Ledger rows no transcript answer matched.
    pub unmatched: i64,
    /// The output of [`Cost::matched`] rows counted once, in tokens: what
    /// their priced re-read ([`Cost::output_reread_priced`]) is carried from.
    pub matched_out: f64,
    /// Of [`Cost::matched`], routed Reads joined by their tool call id.
    pub matched_id: i64,
    /// Transcript answers found ([`Transcript::answers`]).
    pub answers: i64,
    /// Of them, left unmatched to any ledger row.
    pub answers_unmatched: i64,
    /// The credit re-read carried by time-only matches (#297, S4).
    pub credit_reread_near: f64,
    /// The cost re-read carried by time-only matches.
    pub cost_reread_near: f64,
    /// Matched rows whose answer repeats one in the same context, their
    /// counterfactual Read carried as the stub ([`READ_STUB_BYTES`], S2).
    pub repeats: i64,
    /// Counterfactual compactions before the real one ([`early_compactions`]).
    pub early: i64,
    /// The rulebook's re-read (#294): the sum of the multipliers
    /// ([`Carried::priced`]) of each measured subagent's first
    /// SubagentStart injection (its first row without one), over those with
    /// a transcript.
    pub start_carried: f64,
    /// Subagents the rulebook's credit was measured on (in `brief_log`
    /// and `hook_inject`, as [`crate::tokens::MatchRow::agents`]) with a
    /// transcript (#297, N2).
    pub start_seen: i64,
    /// Of those subagents, without a transcript.
    pub start_unseen: i64,
}

/// What the credit side brings to [`measure_with`] (#294): the credits not
/// in `token_log`'s own columns, each with where it was put into context.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Credits {
    /// Hook hits (#217), each its injection's place and credited bytes.
    pub hits: Vec<Hit>,
    /// `route:Read` credit in bytes by `token_log` id (E4's judged rows).
    pub reads: HashMap<i64, i64>,
    /// The measured rulebook's credit in Net, in tokens ([`rulebook_net`]),
    /// `None` when not measured (#297, S-c). With it, the counterfactual
    /// compaction counts the rulebook's excess at each measured subagent's
    /// start, and no SubagentStart hit's, as Net credits: spread over the
    /// briefed subagents with a SubagentStart row, the count Net spreads it
    /// on ([`Cost::rulebook_reread`]).
    pub rulebook: Option<f64>,
}

/// The measured rulebook's credit as Net credits it
/// ([`Cost::rulebook_net_credit`]), for [`Credits::rulebook`]: `None` when
/// not measured.
pub fn rulebook_net(matched: Option<&crate::tokens::MatchRow>, recall_records: f64) -> Option<f64> {
    matched?.rulebook.map(|rb| rb - rb.min(recall_records.max(0.0)))
}

/// One credited hook hit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hit {
    /// The session.
    pub session: String,
    /// The subagent, empty in the main session.
    pub agent: String,
    /// When it was injected.
    pub time: String,
    /// When it was used (#297, S3): the first act the judge counted, where
    /// the counterfactual read would have entered context. Empty: unknown,
    /// the injection's time is used.
    pub used: String,
    /// The hook event.
    pub hook: String,
    /// Credited bytes.
    pub read: i64,
    /// Bytes injected.
    pub bytes: i64,
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

    /// The rulebook's re-read credit (#294): its credit in Net
    /// ([`Cost::rulebook_net_credit`]) spread evenly over the subagents
    /// with a SubagentStart row, each share times that subagent's start
    /// multiplier; a subagent without a transcript adds nothing here (its
    /// share is once-only, [`Cost::rulebook_once`]). 0 when not measured.
    #[must_use]
    pub fn rulebook_reread(&self) -> f64 {
        let agents = (self.start_seen + self.start_unseen) as f64;
        match self.rulebook_net_credit() {
            Some(credit) if agents > 0.0 => credit * self.start_carried / agents,
            _ => 0.0,
        }
    }

    /// The share of the rulebook's credit in Net whose subagents had no
    /// transcript: counted once.
    #[must_use]
    pub fn rulebook_once(&self) -> f64 {
        let agents = (self.start_seen + self.start_unseen) as f64;
        match self.rulebook_net_credit() {
            Some(credit) if agents > 0.0 => credit * self.start_unseen as f64 / agents,
            _ => 0.0,
        }
    }

    /// R: mean requests per session or subagent with a transcript (#297,
    /// B1), 0 without one.
    #[must_use]
    pub fn requests_per_scope(&self) -> f64 {
        if self.with_transcript > 0 {
            self.requests as f64 / self.with_transcript as f64
        } else {
            0.0
        }
    }

    /// Every bucket's tokens, the re-reads price-weighted.
    pub fn total(&self) -> f64 {
        self.a() + self.b() + self.c()
    }

    /// Every bucket's tokens counted once: no re-read.
    pub fn total_once(&self) -> f64 {
        self.total() - self.reread_total()
    }

    /// The priced re-reads of A, B and C.
    pub fn reread_total(&self) -> f64 {
        self.start_reread_priced + sum_of(&self.reread_priced) + sum_of(&self.output_reread_priced)
    }

    /// Bucket A, the re-read price-weighted (#297).
    pub fn a(&self) -> f64 {
        self.a_once() + self.start_reread_priced
    }

    /// Bucket A counted once: the starts and loads as sent.
    pub fn a_once(&self) -> f64 {
        self.start_tokens + self.load_tokens
    }

    /// Bucket B, the re-read price-weighted.
    pub fn b(&self) -> f64 {
        sum_of(&self.sent) + sum_of(&self.framing) + sum_of(&self.reread_priced)
    }

    /// Bucket C, the re-read price-weighted (#294).
    pub fn c(&self) -> f64 {
        sum_of(&self.output) + sum_of(&self.output_reread_priced)
    }

    /// Whether any part is a floor: no transcripts asked for, or a session
    /// without one.
    pub fn floor(&self) -> bool {
        !self.transcripts || self.without_transcript > 0
    }

    /// The B cost of one `hook:<event>` row's event: sent, framing and the
    /// priced re-read.
    pub fn hook(&self, event: &str) -> f64 {
        self.hook_once(event) + self.reread_priced.get(event).copied().unwrap_or(0.0)
    }

    /// The B cost of one event counted once: sent and framing.
    pub fn hook_once(&self, event: &str) -> f64 {
        let g = |m: &BTreeMap<String, f64>| m.get(event).copied().unwrap_or(0.0);
        g(&self.sent) + g(&self.framing)
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
    measure_with(conn, bytes_per_token, transcripts, &Credits::default())
}

/// [`measure`], with the credits `token_log`'s columns do not hold, so the
/// credit side's re-read is priced as the cost side's is (#294).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn measure_with(conn: &Connection, bytes_per_token: f64, transcripts: Option<&Path>, credits: &Credits) -> Result<Cost> {
    let has = |t: &str| -> Result<bool> {
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)", [t], |r| r.get(0))?)
    };
    let mut c = Cost { transcripts: transcripts.is_some(), ..Cost::default() };
    let tok = |b: f64| b / bytes_per_token;
    // C, and the hook rows' returned bytes for the framing line.
    let mut hook_returned: BTreeMap<String, f64> = BTreeMap::new();
    let mut until = String::new();
    let mut ledger: Vec<LedgerRow> = Vec::new();
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
        // The answers' rows, each with its credit as the ledger gives it
        // (capped as the report caps it), to match to the transcripts.
        let credit = crate::tokens::credit_sql(crate::tokens::lookup_cap(conn)?);
        // A routed Read's tool call id, where the store has the column
        // (#297, S4): such a row is joined to its answer by it.
        let has_id: bool =
            conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('token_log') WHERE name = 'tool_use')", [], |r| r.get(0))?;
        let id_col = if has_id { "ifnull(tool_use, '')" } else { "''" };
        // Rows from this id on were logged with the digest trimmed (#303);
        // a store with no marker has only older rows.
        let trimmed_from = crate::tokens::trimmed_from(conn)?;
        let mut st = conn.prepare(&format!(
            "SELECT id, time, tool, returned, {credit}, ifnull(grep, 0), ifnull(read, 0), {id_col} FROM token_log WHERE tool NOT LIKE 'hook%' AND tool NOT LIKE 'compact:%' ORDER BY time, id"
        ))?;
        for row in st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                LedgerRow {
                    secs: secs(&r.get::<_, String>(1)?),
                    tool: r.get(2)?,
                    returned: r.get(3)?,
                    credit: r.get(4)?,
                    grep: r.get(5)?,
                    read: r.get(6)?,
                    tool_use: r.get(7)?,
                    old: true,
                },
            ))
        })? {
            let (id, mut row) = row?;
            row.old = trimmed_from.is_none_or(|m| id < m);
            if row.tool == "route:Read" {
                row.credit = credits.reads.get(&id).copied().unwrap_or(0);
            }
            ledger.push(row);
        }
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
            .collect::<crate::db::Result<Vec<_>>>()?;
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
    // The subagents the rulebook's credit is measured on (#297, N2): those
    // in `brief_log` too, as [`crate::tokens::MatchRow::agents`] counts.
    let briefed: std::collections::HashSet<(String, String)> = if has("brief_log")? {
        let mut st = conn.prepare("SELECT DISTINCT session, agent FROM brief_log")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<crate::db::Result<_>>()?;
        v
    } else {
        Default::default()
    };
    let mut per_turn: Vec<f64> = Vec::new();
    let mut per_agent: Vec<f64> = Vec::new();
    let mut hits_by: HashMap<(&str, &str), Vec<&Hit>> = HashMap::new();
    for h in &credits.hits {
        hits_by.entry((h.session.as_str(), h.agent.as_str())).or_default().push(h);
    }
    // The transcripts read, each once, and the answers found in them.
    let mut trs: Vec<Transcript> = Vec::new();
    let mut tr_of: HashMap<String, usize> = HashMap::new();
    let mut seen: Vec<Seen> = Vec::new();
    // The credits to carry once the ledger is matched, and the rulebook's
    // start in each measured subagent with a transcript.
    let mut items: Vec<CreditItem> = Vec::new();
    let mut rule_starts: Vec<(usize, String)> = Vec::new();
    let mut rule_excess: Vec<(usize, String, f64)> = Vec::new();
    for ((session, agent), list) in &scopes {
        let path = list.iter().map(|r| r.transcript.as_str()).find(|p| !p.is_empty()).unwrap_or("");
        let ti = match transcripts {
            Some(dir) if !path.is_empty() && Path::new(path).starts_with(dir) => match tr_of.get(path) {
                Some(&i) => Some(i),
                None => read_transcript(Path::new(path), &until).map(|t| {
                    let i = trs.len();
                    seen.extend(t.answers.iter().map(|a| Seen {
                        tr: i,
                        time: a.time.clone(),
                        secs: secs(&a.time),
                        tool: a.tool.clone(),
                        bytes: a.bytes,
                        tool_use: a.tool_use.clone(),
                        op: a.op,
                        repeat: a.repeat,
                        carried: carried(&t, &a.time),
                    }));
                    trs.push(t);
                    tr_of.insert(path.to_string(), i);
                    i
                }),
            },
            _ => None,
        };
        match ti {
            Some(_) => c.with_transcript += 1,
            None => c.without_transcript += 1,
        }
        // The scope's events in time order, each with its tokens: the start
        // first, then each injection sent, each load, each re-read.
        let mut events: Vec<(String, f64)> = vec![(list.first().map(|r| r.time.clone()).unwrap_or_default(), start)];
        for r in list {
            events.push((r.time.clone(), tok(r.bytes as f64)));
        }
        if let Some(t) = ti.map(|i| &trs[i]) {
            c.requests += t.requests.len() as i64;
            // A (#297, B1): the definitions are in every request's prefix
            // from the scope's first request, a load's from its time, and no
            // compaction drops them.
            let mut defs: Vec<(&str, f64)> = vec![("", start)];
            for (time, tool) in &t.loads {
                let b = tok(tool_bytes(tool) as f64);
                c.loads += 1;
                c.load_tokens += b;
                events.push((time.clone(), b));
                defs.push((time.as_str(), b));
            }
            for (time, b) in defs {
                for q in through(t, time) {
                    let (w, full) = q.weight();
                    c.priced += 1;
                    c.full_rate += i64::from(full);
                    c.start_reread += b;
                    c.start_reread_priced += b * w;
                    events.push((q.time.clone(), b * w));
                }
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
        // #294: a hit's counterfactual is carried from when it was used
        // (#297, S3); the answers in the transcript are matched to the
        // ledger below. With no transcript, the scope's hits and injections
        // count once.
        let hits = hits_by.get(&(session.as_str(), agent.as_str())).map_or(&[][..], Vec::as_slice);
        match ti {
            Some(i) => {
                for h in hits {
                    let key = format!("hook:{}", h.hook);
                    items.push(CreditItem {
                        tr: i,
                        time: if h.used.is_empty() { h.time.clone() } else { h.used.clone() },
                        credit: tok(h.read as f64),
                        out: tok(h.bytes as f64),
                        // S-c: a measured rulebook stands in for these hits,
                        // which Net then credits 0.
                        excess: !(credits.rulebook.is_some() && key == START_HOOK),
                        key,
                        near: false,
                    });
                }
            }
            None => {
                for h in hits {
                    *c.once_credit.entry(format!("hook:{}", h.hook)).or_default() += tok(h.read as f64);
                }
                for r in list {
                    *c.once_cost.entry(format!("hook:{}", r.hook)).or_default() += tok(r.bytes as f64);
                }
            }
        }
        // The rulebook is read at a measured subagent's start: carried as
        // its first SubagentStart injection is (its first row without one).
        if !agent.is_empty() && briefed.contains(&(session.clone(), agent.clone())) {
            let first = list.iter().find(|r| r.hook == "SubagentStart").or(list.first()).map(|r| r.time.clone()).unwrap_or_default();
            match ti {
                Some(i) => {
                    c.start_seen += 1;
                    // S-c: the rulebook read in place of the subagent's
                    // SubagentStart injections, its excess over them.
                    if credits.rulebook.is_some() {
                        let injected: f64 = list.iter().filter(|r| r.hook == "SubagentStart").map(|r| tok(r.bytes as f64)).sum();
                        rule_excess.push((i, first.clone(), injected));
                    }
                    rule_starts.push((i, first));
                }
                None => c.start_unseen += 1,
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
    // #294: each ledger row matched to an answer is carried as that answer
    // was, its output on the cost side and its credit on the credit side by
    // the same multiplier; an unmatched row counts once on both.
    let found: Vec<Found> = seen.iter().map(|s| Found { secs: s.secs, tool: &s.tool, bytes: s.bytes, op: s.op, id: &s.tool_use }).collect();
    let logged: Vec<Logged> =
        ledger.iter().map(|r| Logged { secs: r.secs, tool: &r.tool, returned: r.returned, id: &r.tool_use, old: r.old }).collect();
    let matches = match_answers(&found, &logged);
    c.answers = seen.len() as i64;
    c.answers_unmatched = c.answers - matches.iter().flatten().count() as i64;
    for (r, m) in ledger.iter().zip(matches) {
        let (out, credit) = (tok(r.returned as f64), tok(r.credit as f64));
        match m {
            Some((i, how)) => {
                let a = &seen[i];
                let k = a.carried;
                c.matched += 1;
                c.matched_out += out;
                c.matched_exact += i64::from(how == How::Exact);
                c.matched_id += i64::from(how == How::Id);
                c.priced += k.n;
                c.full_rate += k.full;
                *c.output_reread.entry(r.tool.clone()).or_default() += out * k.n as f64;
                *c.output_reread_priced.entry(r.tool.clone()).or_default() += out * k.priced;
                if how == How::Near {
                    c.cost_reread_near += out * k.priced;
                }
                // S2: a repeated answer's counterfactual Read is the stub.
                let carried_credit = if a.repeat && r.read > 0 {
                    c.repeats += 1;
                    tok(r.credit.min(r.grep + READ_STUB_BYTES) as f64)
                } else {
                    credit
                };
                if carried_credit > 0.0 {
                    items.push(CreditItem {
                        tr: a.tr,
                        time: a.time.clone(),
                        credit: carried_credit,
                        out,
                        key: r.tool.clone(),
                        near: how == How::Near,
                        excess: true,
                    });
                }
            }
            None => {
                c.unmatched += 1;
                *c.once_cost.entry(r.tool.clone()).or_default() += out;
                if credit > 0.0 {
                    *c.once_credit.entry(r.tool.clone()).or_default() += credit;
                }
            }
        }
    }
    // S1: each counterfactual is carried to the next compaction, the real
    // one or the counterfactual session's own earlier one. A transcript
    // that never compacted on its own takes the median size the others
    // compacted at; this assumes a subagent compacts where the main
    // sessions did, though it may run another model or window (N-a). The
    // excess counted is the one Net credits (S-c): with a measured rulebook,
    // the rulebook's at each measured subagent's start and no SubagentStart
    // hit's; without one, the hits'.
    let mut sizes: Vec<f64> = trs.iter().flat_map(|t| t.compact_at.iter().map(|&n| n as f64)).collect();
    sizes.sort_by(f64::total_cmp);
    let fallback = (!sizes.is_empty()).then(|| crate::baseline::median(&sizes));
    let mut by_tr: Vec<Vec<usize>> = vec![Vec::new(); trs.len()];
    for (j, it) in items.iter().enumerate().filter(|(_, it)| it.excess) {
        by_tr[it.tr].push(j);
    }
    let mut rule_by_tr: Vec<Vec<(String, f64)>> = vec![Vec::new(); trs.len()];
    // N-2 (#297 review 3): the rulebook's credit per subagent over the same
    // count Net spreads it on ([`Cost::rulebook_reread`]), less what each
    // subagent's SubagentStart injected.
    let starts = (c.start_seen + c.start_unseen) as f64;
    if let Some(rb) = credits.rulebook.filter(|_| starts > 0.0) {
        for (tr, time, injected) in rule_excess {
            rule_by_tr[tr].push((time, rb / starts - injected));
        }
    }
    let mut early_of: Vec<Vec<String>> = Vec::with_capacity(trs.len());
    for ((t, list), rule) in trs.iter().zip(&by_tr).zip(rule_by_tr) {
        let limit = t.compact_at.iter().min().map(|&n| n as f64).or(fallback);
        let mut excess: Vec<(String, f64)> = list.iter().map(|&j| (items[j].time.clone(), items[j].credit - items[j].out)).collect();
        excess.extend(rule);
        let early = early_compactions(t, &excess, limit);
        c.early += early.len() as i64;
        early_of.push(early);
    }
    let end_of = |tr: usize, time: &str| early_of[tr].iter().find(|e| e.as_str() > time).cloned();
    for it in &items {
        let k = carried_until(&trs[it.tr], &it.time, end_of(it.tr, &it.time).as_deref());
        *c.credit_reread.entry(it.key.clone()).or_default() += it.credit * k.priced;
        if it.near {
            c.credit_reread_near += it.credit * k.priced;
        }
    }
    for (tr, time) in &rule_starts {
        c.start_carried += carried_until(&trs[*tr], time, end_of(*tr, time).as_deref()).priced;
    }
    Ok(c)
}

/// One credit to carry on the credit side (#294): where it entered
/// context, its counterfactual and atlas's answer in tokens, and its net
/// row.
struct CreditItem {
    tr: usize,
    time: String,
    credit: f64,
    out: f64,
    key: String,
    near: bool,
    /// Whether its excess counts toward the counterfactual's compaction:
    /// not for a credit Net replaces (S-c).
    excess: bool,
}

/// One `token_log` row an answer in a transcript may match.
struct LedgerRow {
    secs: f64,
    tool: String,
    returned: i64,
    credit: i64,
    grep: i64,
    read: i64,
    tool_use: String,
    /// Logged before the store's trimmed-digest marker (#303).
    old: bool,
}

/// One answer found in a transcript, with how it was carried.
struct Seen {
    tr: usize,
    time: String,
    secs: f64,
    tool: String,
    bytes: usize,
    tool_use: String,
    op: bool,
    repeat: bool,
    carried: Carried,
}

/// Seconds since the epoch of a UTC time (`YYYY-MM-DDTHH:MM:SS[.fff]Z`),
/// NaN for any other form.
fn secs(time: &str) -> f64 {
    let whole = time.get(..19).and_then(|s| crate::pr_supervise::parse_utc(&format!("{s}Z")));
    let frac = time
        .get(19..)
        .and_then(|f| f.strip_prefix('.'))
        .and_then(|f| format!("0.{}", f.trim_end_matches('Z')).parse::<f64>().ok())
        .unwrap_or(0.0);
    whole.map_or(f64::NAN, |w| w as f64 + frac)
}

/// How far apart, in seconds, a ledger row and a transcript answer of
/// equal bytes may be logged and still match.
pub const MATCH_EXACT_SECS: f64 = 600.0;

/// How far apart a ledger row and an answer of other bytes may be.
pub const MATCH_NEAR_SECS: f64 = 60.0;

/// The size tolerance of a time-only match (#297, S4): the answer and the
/// row may differ by at most this share of the larger, or
/// [`MATCH_NEAR_BYTES`], whichever is more.
pub const MATCH_NEAR_SHARE: f64 = 0.25;

/// The least size difference a time-only match tolerates, in bytes.
pub const MATCH_NEAR_BYTES: i64 = 64;

/// Whether an answer's and a row's sizes agree within the time-only
/// tolerance.
fn near_size(bytes: usize, returned: i64) -> bool {
    let a = bytes as i64;
    (a - returned).abs() as f64 <= (MATCH_NEAR_BYTES as f64).max(a.max(returned) as f64 * MATCH_NEAR_SHARE)
}

/// Whether a routed row was logged by an older build, with the digest's
/// trailing newline that Claude Code trims from the tool result (#297, R1):
/// one byte over the answer, and below the store's trimmed-digest marker
/// ([`Logged::old`]). The equal-bytes pass accepts it; an equal size wins
/// only a tie in time. A row from the marker on must equal its answer
/// (#303).
fn old_digest(r: &Logged, bytes: i64) -> bool {
    r.old && r.tool.starts_with("route:") && r.returned == bytes + 1
}

/// Which ledger rows an answer may match: a digest only its own route's
/// rows; an atlas tool any atlas tool's, since a listed tool (`note`)
/// answers under its operation's row (`read_note`).
fn pool(tool: &str) -> &str {
    if tool.starts_with("route:") {
        tool
    } else {
        "atlas"
    }
}

/// One transcript answer as [`match_answers`] reads it.
#[derive(Debug, Clone, Copy)]
pub struct Found<'a> {
    /// Its time in seconds ([`secs`]); NaN never matches.
    pub secs: f64,
    /// Its tool, as [`Answer::tool`].
    pub tool: &'a str,
    /// Its bytes.
    pub bytes: usize,
    /// Whether its call named an `op` ([`Answer::op`]).
    pub op: bool,
    /// Its tool call id.
    pub id: &'a str,
}

/// One ledger row as [`match_answers`] reads it.
#[derive(Debug, Clone, Copy)]
pub struct Logged<'a> {
    /// Its time in seconds; NaN never matches.
    pub secs: f64,
    /// Its tool.
    pub tool: &'a str,
    /// Bytes returned.
    pub returned: i64,
    /// The tool call id a routed Read's row records, else empty.
    pub id: &'a str,
    /// Whether it was logged before atlas trimmed digests (#303): its id is
    /// below the store's `token_log_trimmed_from` marker, or the store has
    /// none. Only such a routed row may be one byte over ([`old_digest`]).
    pub old: bool,
}

/// How a ledger row was matched to an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// By its tool call id (a routed Read).
    Id,
    /// By equal bytes, in time.
    Exact,
    /// By time, tool and size within the tolerance only.
    Near,
}

/// Matches transcript answers to ledger rows, each at most once (#294):
/// `token_log` records no session for an atlas answer, so it is told by
/// its tool, time and size. A row with a tool call id (a routed Read) joins
/// only the answer of that call (#297, S4). The others: first equal sizes
/// (or a routed row one byte over, [`old_digest`]) within
/// [`MATCH_EXACT_SECS`], every such pair across all answers taken nearest
/// in time first (#297 review 3, M-1); then, per answer, within [`MATCH_NEAR_SECS`], the same
/// tool (or any, for a call that named an op) of a size within
/// [`MATCH_NEAR_SHARE`] or [`MATCH_NEAR_BYTES`]; among candidates the same
/// tool name first, then the nearest in time. A missing time never matches
/// (N1). Gives, per row, the answer's index and how it matched.
pub fn match_answers(answers: &[Found], rows: &[Logged]) -> Vec<Option<(usize, How)>> {
    let mut out: Vec<Option<(usize, How)>> = vec![None; rows.len()];
    let mut used = vec![false; answers.len()];
    let by_id: HashMap<&str, usize> = answers.iter().enumerate().filter(|(_, a)| !a.id.is_empty()).map(|(i, a)| (a.id, i)).collect();
    for (j, r) in rows.iter().enumerate().filter(|(_, r)| !r.id.is_empty()) {
        if let Some(&i) = by_id.get(r.id).filter(|&&i| !used[i]) {
            out[j] = Some((i, How::Id));
            used[i] = true;
        }
    }
    let mut by_pool: HashMap<&str, Vec<usize>> = HashMap::new();
    for (j, r) in rows.iter().enumerate().filter(|(_, r)| r.id.is_empty() && r.secs.is_finite()) {
        by_pool.entry(pool(r.tool)).or_default().push(j);
    }
    for v in by_pool.values_mut() {
        v.sort_by(|a, b| rows[*a].secs.total_cmp(&rows[*b].secs));
    }
    let mut order: Vec<usize> = (0..answers.len()).filter(|&i| answers[i].secs.is_finite()).collect();
    order.sort_by(|a, b| answers[*a].secs.total_cmp(&answers[*b].secs));
    // Equal bytes, or a routed row one byte over, within MATCH_EXACT_SECS:
    // every fitting pair across all answers, taken nearest in time first
    // (#297 review 3, M-1); size only breaks a tie in time. A per-answer
    // pass, even with the one-byte rows in a pass of their own, lets the
    // earlier answer take a later answer's nearer row. On rows below the
    // trimmed-digest marker a row one byte over, nearer in time than the
    // answer's own row, still wins, and greedy can leave an answer unmatched
    // that a maximum matching would cover (#303, case b); rows from the
    // marker on take equal sizes only.
    let mut pairs: Vec<(bool, f64, bool, usize, usize)> = Vec::new();
    for (rank, &i) in order.iter().enumerate() {
        let a = answers[i];
        if used[i] {
            continue;
        }
        let Some(pool) = by_pool.get(pool(a.tool)) else { continue };
        let from = pool.partition_point(|&j| rows[j].secs < a.secs - MATCH_EXACT_SECS);
        let bytes = a.bytes as i64;
        for &j in &pool[from..] {
            let r = rows[j];
            if r.secs > a.secs + MATCH_EXACT_SECS {
                break;
            }
            if out[j].is_none() && (r.returned == bytes || old_digest(&r, bytes)) {
                pairs.push((r.tool != a.tool, (r.secs - a.secs).abs(), r.returned != bytes, rank, j));
            }
        }
    }
    pairs.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.total_cmp(&y.1)).then(x.2.cmp(&y.2)).then(x.3.cmp(&y.3)).then(x.4.cmp(&y.4)));
    for (_, _, _, rank, j) in pairs {
        let i = order[rank];
        if !used[i] && out[j].is_none() {
            out[j] = Some((i, How::Exact));
            used[i] = true;
        }
    }
    // Then, per answer in time order, a near size within MATCH_NEAR_SECS.
    for &i in &order {
        if used[i] {
            continue;
        }
        let a = answers[i];
        let Some(pool) = by_pool.get(pool(a.tool)) else { continue };
        let from = pool.partition_point(|&j| rows[j].secs < a.secs - MATCH_NEAR_SECS);
        let mut best: Option<(bool, f64, usize)> = None;
        for &j in &pool[from..] {
            let r = rows[j];
            if r.secs > a.secs + MATCH_NEAR_SECS {
                break;
            }
            if out[j].is_some() || !((r.tool == a.tool || a.op) && near_size(a.bytes, r.returned)) {
                continue;
            }
            let key = (r.tool != a.tool, (r.secs - a.secs).abs(), j);
            if best.is_none_or(|b| (key.0, key.1) < (b.0, b.1)) {
                best = Some(key);
            }
        }
        if let Some((_, _, j)) = best {
            out[j] = Some((i, How::Near));
            used[i] = true;
        }
    }
    out
}

/// The floor wording of the net line, while any cost is unseen.
pub const FLOOR: &str = "counts only costs atlas can see; the true net is lower";

/// One row of the net table: a tool's credit (the ledger, unchanged) and
/// its cost, in tokens, each counted once and re-read (#294).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetRow {
    /// The tool, or the bucket A row.
    pub key: String,
    /// Credited savings, each answer counted once.
    pub credit: f64,
    /// Measured cost, each answer counted once.
    pub cost: f64,
    /// The credit's re-read: the counterfactual carried as the answer was.
    pub credit_reread: f64,
    /// The cost's re-read, priced.
    pub cost_reread: f64,
}

impl NetRow {
    /// Credit with its re-read.
    pub fn credit_total(&self) -> f64 {
        self.credit + self.credit_reread
    }

    /// Cost with its re-read.
    pub fn cost_total(&self) -> f64 {
        self.cost + self.cost_reread
    }

    /// The net with re-reads on both sides: the headline's (#294).
    pub fn net(&self) -> f64 {
        self.credit_total() - self.cost_total()
    }

    /// The net with each answer counted once, on both sides.
    pub fn net_once(&self) -> f64 {
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
    let g = |m: &BTreeMap<String, f64>, k: &str| m.get(k).copied().unwrap_or(0.0);
    // A row's cost, once and re-read: B for a hook event, C for a tool.
    let costs = |key: &str| match key.strip_prefix("hook:") {
        Some(event) => (cost.hook_once(event), g(&cost.reread_priced, event)),
        None => (g(&cost.output, key), g(&cost.output_reread_priced, key)),
    };
    let mut rows: Vec<NetRow> = Vec::new();
    for t in by_tool.iter().filter(|t| t.key != DOUBLE_LOGGED && t.key != ROUTED) {
        let (c, c_reread) = costs(&t.key);
        // A measured rulebook replaces the subagent-start hit credit: both
        // credit what those injections saved, so only one may count.
        let replaced = t.key == START_HOOK && cost.rulebook.is_some();
        let (credit, credit_reread) = if replaced { (0.0, 0.0) } else { (t.counterfactual, g(&cost.credit_reread, &t.key)) };
        rows.push(NetRow { key: t.key.clone(), credit, cost: c, credit_reread, cost_reread: c_reread });
    }
    // Cost with no ledger row (a hook event that never logged a row).
    for event in cost.sent.keys().chain(cost.framing.keys()).chain(cost.reread_priced.keys()) {
        let key = format!("hook:{event}");
        if !rows.iter().any(|r| r.key == key) {
            let (c, cost_reread) = costs(&key);
            rows.push(NetRow { key, cost: c, cost_reread, ..NetRow::default() });
        }
    }
    for tool in cost.output.keys() {
        if !rows.iter().any(|r| &r.key == tool) {
            let (c, cost_reread) = costs(tool);
            rows.push(NetRow { key: tool.clone(), cost: c, cost_reread, ..NetRow::default() });
        }
    }
    rows.push(NetRow { key: DEFINITIONS.into(), cost: cost.a_once(), cost_reread: cost.start_reread_priced, ..NetRow::default() });
    // Recall by key or lane may credit the same rulebook windows: the
    // rulebook row is reduced by that credit, so neither counts it twice.
    if let Some(credit) = cost.rulebook_net_credit() {
        rows.push(NetRow { key: RULEBOOK.into(), credit, credit_reread: cost.rulebook_reread(), ..NetRow::default() });
    }
    rows
}

/// The credit and cost of answers with no transcript data, counted once
/// (#294), over the net table's rules: a measured rulebook replaces the
/// SubagentStart hits, and its own share without a transcript counts.
pub fn once_only(cost: &Cost) -> (f64, f64) {
    let credit: f64 =
        cost.once_credit.iter().filter(|(k, _)| !(k.as_str() == START_HOOK && cost.rulebook.is_some())).map(|(_, v)| v).sum::<f64>()
            + cost.rulebook_once();
    (credit + 0.0, sum_of(&cost.once_cost))
}

/// The issue that tracks the one large overstatement still unpriced:
/// different lookups that would Read the same file (#297, S-e).
pub const CROSS_LOOKUP_ISSUE: &str = "bloodfeast/atlas-dev#300";

/// One reason the net may be too high or too low: its words, and its size
/// in tokens where one can be estimated.
#[derive(Debug, Clone, PartialEq)]
pub struct Reason {
    /// What it is, with its size when known.
    pub text: String,
    /// Its estimated size, `None` when unknown (not weighed).
    pub size: Option<f64>,
}

/// Why the net may be too high or too low (#297, B2, S-d), each side
/// largest first, reasons of unknown size last. Too high: cost the
/// transcripts do not show (unknown), the hook text counted once on the
/// cost side (its re-read sized at the injections' priced re-read per token
/// sent), the loose (time-only) matches' re-read net, and a lookup's
/// counterfactual Read carried in full when another lookup read the same
/// file (unknown, [`CROSS_LOOKUP_ISSUE`]). Answers with no transcript count
/// once on both sides; both would be carried by one multiplier (the matched
/// output's priced re-read per token), so they move the net by their credit
/// less their cost times it, on the side of its sign.
pub fn bound_reasons(cost: &Cost) -> (Vec<Reason>, Vec<Reason>) {
    use crate::token_report::compact;
    let (mut high, mut low) = (Vec::new(), Vec::new());
    if cost.floor() {
        high.push(Reason { text: format!("costs unseen ({})", coverage(cost)), size: None });
    }
    let framing = sum_of(&cost.framing);
    if cost.transcripts && framing > 0.0 {
        let sent = sum_of(&cost.sent);
        let size = (sent > 0.0).then(|| framing * sum_of(&cost.reread_priced) / sent);
        let about = size.map_or(String::new(), |v| format!(", about {}", compact(v)));
        high.push(Reason {
            text: format!("{} of hook text no injection row holds counts once, its re-read unpriced{about}", compact(framing)),
            size,
        });
    }
    let (oc, ok) = once_only(cost);
    let carry = (cost.matched_out > 0.0).then(|| sum_of(&cost.output_reread_priced) / cost.matched_out);
    let size = carry.map(|k| (oc - ok).abs() * k);
    let about = size.map_or(String::new(), |v| format!(", about {} re-read", compact(v)));
    let once =
        Reason { text: format!("answers with no transcript count once (credit {}, cost {}{about})", compact(oc), compact(ok)), size };
    if oc > ok {
        low.push(once);
    } else if ok > oc {
        high.push(once);
    }
    let near = cost.credit_reread_near - cost.cost_reread_near;
    if cost.credit_reread_near > 0.0 || cost.cost_reread_near > 0.0 {
        let r = Reason {
            text: format!(
                "loose (time-only) matches carry {} of credit re-read and {} of cost re-read",
                compact(cost.credit_reread_near),
                compact(cost.cost_reread_near)
            ),
            size: Some(near.abs()),
        };
        if near >= 0.0 {
            high.push(r);
        } else {
            low.push(r);
        }
    }
    let lookups: f64 = cost.credit_reread.iter().filter(|(k, _)| crate::tokens::CAPPED.contains(&k.as_str())).map(|(_, v)| v).sum();
    if lookups > 0.0 {
        high.push(Reason {
            text: format!(
                "a lookup's counterfactual Read is cut to the stub only when the same lookup repeats, not when another lookup read the file ({} of lookup credit re-read, share unknown, {CROSS_LOOKUP_ISSUE})",
                compact(lookups)
            ),
            size: None,
        });
    }
    let order = |v: &mut Vec<Reason>| v.sort_by(|a, b| b.size.unwrap_or(-1.0).total_cmp(&a.size.unwrap_or(-1.0)));
    order(&mut high);
    order(&mut low);
    (high, low)
}

/// How much larger one side's known size must be than the other's before
/// the net is called that side's bound (#297, S-d).
pub const BOUND_MARGIN: f64 = 2.0;

/// What kind of number the net is (#297, B2, S-d), with why
/// ([`bound_reasons`]), or `None` when every part is priced. With reasons on
/// one side only: "an upper bound" or "a lower bound". With both, the known
/// sizes are weighed: the larger side names it ("more likely an upper
/// bound") when it is over [`BOUND_MARGIN`] times the other and the other
/// holds no reason of unknown size (which could outweigh it); otherwise
/// "neither an upper nor a lower bound". Reasons of unknown size are listed
/// but not weighed.
pub fn bound(cost: &Cost) -> Option<String> {
    use crate::token_report::compact;
    let (high, low) = bound_reasons(cost);
    let join = |v: &[Reason]| v.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("; ");
    let known = |v: &[Reason]| v.iter().filter_map(|r| r.size).sum::<f64>() + 0.0;
    let unknown = |v: &[Reason]| v.iter().any(|r| r.size.is_none());
    match (high.is_empty(), low.is_empty()) {
        (true, true) => None,
        (false, true) => Some(format!("an upper bound because: {}", join(&high))),
        (true, false) => Some(format!("a lower bound because: {}", join(&low))),
        (false, false) => {
            let (h, l) = (known(&high), known(&low));
            let sizes = format!("known sizes too high {}, too low {}", compact(h), compact(l));
            let kind = if h > BOUND_MARGIN * l && !unknown(&low) {
                format!("more likely an upper bound ({sizes})")
            } else if l > BOUND_MARGIN * h && !unknown(&high) {
                format!("more likely a lower bound ({sizes})")
            } else if h > BOUND_MARGIN * l || l > BOUND_MARGIN * h {
                // The side that blocks the call is the smaller one (#297
                // review 3, S-2): the larger may hold an unknown too.
                let side = if h > BOUND_MARGIN * l { "too low" } else { "too high" };
                format!("neither an upper nor a lower bound ({sizes}; a {side} reason of unknown size is not weighed)")
            } else if h == 0.0 && l == 0.0 {
                format!("neither an upper nor a lower bound ({sizes}, no reason sized)")
            } else {
                format!("neither an upper nor a lower bound ({sizes}, within {BOUND_MARGIN}x)")
            };
            Some(format!("{kind}: too high because: {}; too low because: {}", join(&high), join(&low)))
        }
    }
}

/// The net with the loose (time-only) matches' re-reads left out on both
/// sides (#297, S4, N-e): those answers counted once.
pub fn net_near_once(cost: &Cost, rows: &[NetRow]) -> f64 {
    totals(rows).2 - (cost.credit_reread_near - cost.cost_reread_near)
}

/// The name of the measured rulebook's row in the net table (#219): its
/// credit only; the injections that replace it are the `hook:SubagentStart`
/// row's cost.
pub const RULEBOOK: &str = "rulebook (measured)";

/// The subagent-start hook's ledger row, whose hit credit a measured
/// rulebook replaces.
pub const START_HOOK: &str = "hook:SubagentStart";

/// Total credit, total cost and the net, over [`net_rows`], with re-reads
/// on both sides (#294): the headline's.
pub fn totals(rows: &[NetRow]) -> (f64, f64, f64) {
    let credit: f64 = rows.iter().map(NetRow::credit_total).sum();
    let cost: f64 = rows.iter().map(NetRow::cost_total).sum();
    (credit, cost, credit - cost)
}

/// [`totals`] with each answer counted once, on both sides.
pub fn totals_once(rows: &[NetRow]) -> (f64, f64, f64) {
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
fn formula(cost: &Cost, bytes_per_token: f64) -> String {
    let rereads = if cost.transcripts {
        "; with transcripts, re-reads priced on both sides: an answer and the counterfactual it is credited with are carried alike, to the next compaction or the end (the counterfactual's own earlier one when its larger context would have compacted first); tool definitions to the end. Assumed: the counterfactual is carried as long as the answer"
    } else {
        "; no transcripts read, so each answer counts once on both sides"
    };
    format!(
        "net = credited savings (the ledger, unchanged) - measured cost (A tool definitions + B hook injections + C tool output), all in tokens at {bytes_per_token} bytes per token{rereads}"
    )
}

/// Which net the headline is, as one line (#294): with transcripts, re-reads
/// on both sides, the once-only net beside it and the share of answers
/// with no transcript data; without, once-only.
pub fn headline_net(cost: &Cost, by_tool: &[crate::tokens::ReportRow]) -> String {
    use crate::token_report::compact;
    let rows = net_rows(cost, by_tool);
    let (_, _, net) = totals(&rows);
    let (credit1, c1, net1) = totals_once(&rows);
    let mut s = if cost.transcripts {
        format!("{} with re-reads on both sides ({} once-only); {}", compact(net), compact(net1), once_share(cost, credit1, c1))
    } else {
        format!("{} once-only: no transcripts read, so neither side's re-reads are counted", compact(net))
    };
    if cost.floor() {
        s.push_str(&format!("; {FLOOR}"));
    }
    if let Some(b) = bound(cost) {
        s.push_str(&format!("; {b}"));
    }
    s
}

/// The share of answers with no transcript data, counted once.
fn once_share(cost: &Cost, credit_once: f64, cost_once: f64) -> String {
    use crate::token_report::compact;
    let (credit, c) = once_only(cost);
    format!(
        "{} of the credit ({}) and {} of the cost ({}) is answers with no transcript, counted once",
        compact(credit),
        share(credit, credit_once),
        compact(c),
        share(c, cost_once)
    )
}

/// The net block the human report shows after its headline.
pub fn net_block(cost: &Cost, by_tool: &[crate::tokens::ReportRow], bytes_per_token: f64) -> String {
    use crate::token_report::{block, compact};
    let rows = net_rows(cost, by_tool);
    let (credit, c, net) = totals(&rows);
    let floor = |s: String| if cost.floor() { format!("{s} ({FLOOR}; {})", coverage(cost)) } else { s };
    let kind = bound(cost);
    if !cost.transcripts {
        let mut items = vec![("credit", compact(credit)), ("cost", compact(c)), ("net", floor(format!("{} once-only", compact(net))))];
        if let Some(b) = kind {
            items.push(("bound", b));
        }
        return block("Net", &formula(cost, bytes_per_token), &items);
    }
    let (credit1, c1, net1) = totals_once(&rows);
    let mut items = vec![
        ("credit", format!("{} ({} once + {} re-read)", compact(credit), compact(credit1), compact(credit - credit1))),
        ("cost", format!("{} ({} once + {} re-read, priced)", compact(c), compact(c1), compact(c - c1))),
        ("net", floor(format!("{} (re-reads on both sides)", compact(net)))),
    ];
    if let Some(b) = kind {
        items.push(("bound", b));
    }
    items.extend([
        (
            "net once-only",
            format!("{} (credit {} - cost {}: each answer counted once on both sides)", compact(net1), compact(credit1), compact(c1)),
        ),
        (
            "net, loose",
            format!(
                "{} (the net with the {} credit and {} cost re-read of loose, time-only matches counted once)",
                compact(net_near_once(cost, &rows)),
                compact(cost.credit_reread_near),
                compact(cost.cost_reread_near)
            ),
        ),
        ("no transcript", once_share(cost, credit1, c1)),
        ("R", format!("{:.1} requests per session or subagent with a transcript", cost.requests_per_scope())),
    ]);
    block("Net", &formula(cost, bytes_per_token), &items)
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
        line("A", "re-read, priced", cost.start_reread_priced),
        line("B", "hook injections sent", sent),
        line("B", "hook text no injection row holds", framing),
        line("B", "re-read, priced", priced),
        line("C", "tool output (all sessions)", sum_of(&cost.output)),
        line("C", "re-read, priced", sum_of(&cost.output_reread_priced)),
        line("", "total", total),
    ];
    let al = [Align::Left, Align::Left, Align::Right, Align::Right];
    out.push_str(&table(&["bucket", "what", "tokens", "share"], &al, &rows));
    let _ = writeln!(
        out,
        "  re-read: A {} tokens raw, {} priced (to the transcript's end, R = {:.1} requests per session or subagent); B {} raw, {} priced; C {} raw, {} priced (each request's atlas share spread over its input, cache-read and cache-write tokens by their proportions); {} of {} re-read requests priced at the full rate (no usage block, or a model not in the price table)",
        compact(cost.start_reread),
        compact(cost.start_reread_priced),
        cost.requests_per_scope(),
        compact(raw),
        compact(priced),
        compact(sum_of(&cost.output_reread)),
        compact(sum_of(&cost.output_reread_priced)),
        cost.full_rate,
        cost.priced
    );
    if cost.transcripts {
        let _ = writeln!(
            out,
            "  C re-read: {} of {} ledger rows matched to an answer in a transcript ({} by tool call id, {} by equal bytes, {} by time only: same tool, size within {:.0}% or {} bytes); the {} unmatched count once. {} answers found in transcripts, {} left unmatched. Time-only matches carry {} of credit re-read and {} of cost re-read. {} repeated lookups carried as the Read stub; {} counterfactual compactions before the real one",
            cost.matched,
            cost.matched + cost.unmatched,
            cost.matched_id,
            cost.matched_exact,
            cost.matched - cost.matched_id - cost.matched_exact,
            MATCH_NEAR_SHARE * 100.0,
            MATCH_NEAR_BYTES,
            cost.unmatched,
            cost.answers,
            cost.answers_unmatched,
            compact(cost.credit_reread_near),
            compact(cost.cost_reread_near),
            cost.repeats,
            cost.early
        );
    }
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
    let rows: Vec<Vec<String>> = c
        .iter()
        .map(|(t, v)| {
            let re = cost.output_reread_priced.get(*t).copied().unwrap_or(0.0);
            vec![(*t).clone(), compact(**v), compact(re), share(**v + re, total)]
        })
        .collect();
    out.push_str(&table(&["tool", "tokens", "re-read priced", "share"], &[Align::Left, Align::Right, Align::Right, Align::Right], &rows));
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
    let net1 = totals_once(&rows).2;
    out.push_str("Net by tool\n");
    let _ = writeln!(
        out,
        "  {}. Credit and cost with their re-reads; net once-only counts each answer once on both sides.",
        formula(cost, bytes_per_token)
    );
    let mut cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| vec![r.key.clone(), compact(r.credit_total()), compact(r.cost_total()), compact(r.net()), compact(r.net_once())])
        .collect();
    cells.push(vec!["total".into(), compact(credit), compact(c), compact(net), compact(net1)]);
    out.push_str(&table(
        &["tool", "credit", "cost", "net", "net once-only"],
        &[Align::Left, Align::Right, Align::Right, Align::Right, Align::Right],
        &cells,
    ));
    if cost.floor() {
        let _ = writeln!(out, "  net ({FLOOR}): {}.", coverage(cost));
    }
    if let Some(b) = bound(cost) {
        let _ = writeln!(out, "  net is {b}.");
    }
    out
}

/// The cost lines `--tsv` appends after the report's own lines.
pub fn tsv(cost: &Cost, by_tool: &[crate::tokens::ReportRow], bytes_per_token: f64) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "{}", formula(cost, bytes_per_token));
    let _ = writeln!(
        out,
        "cost\tA starts {} ({:.0} tokens)\tA loads {} ({:.0} tokens)\tB sent {:.0}\tB other text {:.0}\tB re-read raw {:.0}\tB re-read priced {:.0}\tC output {:.0}\ttotal {:.0}\tC re-read raw {:.0}\tC re-read priced {:.0}\tA re-read raw {:.0}\tA re-read priced {:.0}",
        cost.starts,
        cost.start_tokens,
        cost.loads,
        cost.load_tokens,
        sum_of(&cost.sent),
        sum_of(&cost.framing),
        sum_of(&cost.reread),
        sum_of(&cost.reread_priced),
        sum_of(&cost.output),
        cost.total(),
        sum_of(&cost.output_reread),
        sum_of(&cost.output_reread_priced),
        cost.start_reread,
        cost.start_reread_priced
    );
    let _ = writeln!(
        out,
        "re-read requests\tpriced {}\tat the full rate {}\trequests per scope (R) {:.1}",
        cost.priced,
        cost.full_rate,
        cost.requests_per_scope()
    );
    let _ = writeln!(
        out,
        "answers matched\tmatched {}\tby equal bytes {}\tunmatched {}\tby tool call id {}\tfound {}\tfound unmatched {}\trepeats {}\tearly compactions {}",
        cost.matched,
        cost.matched_exact,
        cost.unmatched,
        cost.matched_id,
        cost.answers,
        cost.answers_unmatched,
        cost.repeats,
        cost.early
    );
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
    let _ = writeln!(out, "net tool\tcredit\tcost\tnet\tcredit once\tcost once\tnet once");
    let rows = net_rows(cost, by_tool);
    for r in &rows {
        let _ = writeln!(
            out,
            "{}\t{:.0}\t{:.0}\t{:.0}\t{:.0}\t{:.0}\t{:.0}",
            r.key,
            r.credit_total(),
            r.cost_total(),
            r.net(),
            r.credit,
            r.cost,
            r.net_once()
        );
    }
    let (credit1, c1, net1) = totals_once(&rows);
    let _ = writeln!(out, "net once-only\tcredit {credit1:.0}\tcost {c1:.0}\tnet {net1:.0}");
    let (oc, ok) = once_only(cost);
    let _ = writeln!(out, "no transcript (counted once)\tcredit {oc:.0}\tcost {ok:.0}");
    let _ = writeln!(
        out,
        "loose matches\tcredit re-read {:.0}\tcost re-read {:.0}\tnet with them once {:.0}",
        cost.credit_reread_near,
        cost.cost_reread_near,
        net_near_once(cost, &rows)
    );
    if let Some(b) = bound(cost) {
        let _ = writeln!(out, "bound\t{b}");
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
            crate::db::params![time, session, agent, transcript, hook, bytes],
        )
        .unwrap();
    }

    fn log(c: &Connection, time: &str, tool: &str, returned: i64) {
        c.execute(
            "INSERT INTO token_log(time, day, tool, returned, grep, read) VALUES (?1, substr(?1, 1, 10), ?2, ?3, NULL, NULL)",
            crate::db::params![time, tool, returned],
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

    /// A ledger row with a counterfactual of `grep + read` bytes.
    fn log_cf(c: &Connection, time: &str, tool: &str, returned: i64, grep: i64, read: i64) {
        c.execute(
            "INSERT INTO token_log(time, day, tool, returned, grep, read) VALUES (?1, substr(?1, 1, 10), ?2, ?3, ?4, ?5)",
            crate::db::params![time, tool, returned, grep, read],
        )
        .unwrap();
    }

    /// The pre-atlas p90 lookup episode: the cap of the CAPPED tools.
    fn cap(c: &Connection, p90: f64) {
        c.execute_batch(&format!(
            "CREATE TABLE baseline_log(id INTEGER PRIMARY KEY, time TEXT NOT NULL, before TEXT NOT NULL, transcripts INTEGER NOT NULL, used INTEGER NOT NULL, episodes INTEGER NOT NULL, mean REAL NOT NULL, median REAL NOT NULL, p90 REAL NOT NULL, total INTEGER NOT NULL);
             INSERT INTO baseline_log(time, before, transcripts, used, episodes, mean, median, p90, total) VALUES('t', 'b', 10, 10, 50, 1, 1, {p90}, 1);"
        ))
        .unwrap();
    }

    /// An assistant message calling one tool.
    fn tool_use(time: &str, id: &str, use_id: &str, name: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{time}\",\"message\":{{\"id\":\"{id}\",\"model\":\"<synthetic>\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"{use_id}\",\"name\":\"{name}\",\"input\":{{}}}}]}}}}"
        )
    }

    /// The result of a tool call, as text items or as one string.
    fn tool_result(time: &str, use_id: &str, text: &str, items: bool) -> String {
        let content = if items { format!("[{{\"type\":\"text\",\"text\":\"{text}\"}}]") } else { format!("\"{text}\"") };
        format!(
            "{{\"type\":\"user\",\"timestamp\":\"{time}\",\"message\":{{\"role\":\"user\",\"content\":[{{\"tool_use_id\":\"{use_id}\",\"type\":\"tool_result\",\"content\":{content}}}]}}}}"
        )
    }

    /// Usage blocks of a known mix: (input, cache read, 5m write, 1h write).
    fn usage(input: u64, read: u64, w5: u64, w1: u64) -> String {
        format!(
            "{{\"input_tokens\":{input},\"cache_read_input_tokens\":{read},\"cache_creation_input_tokens\":{},\"cache_creation\":{{\"ephemeral_5m_input_tokens\":{w5},\"ephemeral_1h_input_tokens\":{w1}}}}}",
            w5 + w1
        )
    }

    /// The price weight of one Opus 5.5 request of that mix.
    fn weight(input: u64, read: u64, w5: u64, w1: u64) -> f64 {
        Request { time: String::new(), model: OPUS.into(), usage: Some(Usage { input, cache_read: read, write_5m: w5, write_1h: w1 }) }
            .weight()
            .0
    }

    /// Writes a transcript: an atlas `recall` answer of `answer` bytes at
    /// 10:00:10 and a hook injection at 10:00:10 too, then `n` requests of
    /// known mixes, with a compaction after the `k`-th when given.
    fn carried_fixture(name: &str, answer: usize, n: usize, k: Option<usize>) -> (Connection, std::path::PathBuf, Vec<f64>) {
        let c = store();
        cap(&c, 2000.0);
        let dir = folder(name);
        let t = dir.join("main.jsonl");
        let mixes = [(100, 900, 0, 0), (10, 1000, 50, 0), (0, 2000, 0, 100), (500, 500, 500, 500), (1, 99, 0, 0)];
        let mut lines = vec![
            tool_use("2026-11-15T10:00:09.000Z", "u1", "toolu_r", "mcp__atlas__recall"),
            tool_result("2026-11-15T10:00:10.000Z", "toolu_r", &"x".repeat(answer), true),
        ];
        let mut weights = Vec::new();
        for i in 0..n {
            let m = mixes[i % mixes.len()];
            lines.push(request(&format!("2026-11-15T10:01:{:02}.000Z", i + 1), &format!("m{i}"), OPUS, &usage(m.0, m.1, m.2, m.3)));
            weights.push(weight(m.0, m.1, m.2, m.3));
            if Some(i + 1) == k {
                lines.push(boundary(&format!("2026-11-15T10:01:{:02}.500Z", i + 1)));
            }
        }
        std::fs::write(&t, lines.join("\n")).unwrap();
        let path = t.to_string_lossy().into_owned();
        inject(&c, "2026-11-15T10:00:10.000Z", "s", "", &path, "UserPromptSubmit", 400);
        // The ledger row of the answer, logged a second after it, with a
        // counterfactual of 5000 bytes, capped at the p90's 2000.
        log_cf(&c, "2026-11-15T10:00:11.000Z", "recall", answer as i64, 1000, 4000);
        log(&c, "2026-11-15T11:00:00Z", "journal", 0);
        (c, dir, weights)
    }

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
        // Neither request has a usage block: both priced at the full rate,
        // as are the three that re-read the tool definitions (#297).
        assert_eq!(cost.reread_priced.get("UserPromptSubmit"), Some(&200.0));
        assert_eq!((cost.priced, cost.full_rate), (2 + 3, 2 + 3));
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
        assert!(tsv(&cost, &by_tool, 4.0).contains("rulebook (measured)\t500\t0\t500\t500\t0\t500\n"));
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

    /// The subagents of session `s` the rulebook's credit is measured on.
    fn brief(c: &Connection, agents: &[&str]) {
        for a in agents {
            c.execute("INSERT INTO brief_log(time, day, session, agent, bytes) VALUES('t', 'd', 's', ?1, 100)", [a]).unwrap();
        }
    }

    fn close(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-6 * want.abs().max(1.0), "{got} against {want}");
    }

    fn get(m: &BTreeMap<String, f64>, k: &str) -> f64 {
        m.get(k).copied().unwrap_or(0.0)
    }

    /// The hit of the injection in [`carried_fixture`], 3000 bytes credited.
    fn hit() -> Credits {
        Credits {
            hits: vec![Hit {
                session: "s".into(),
                agent: String::new(),
                time: "2026-11-15T10:00:10.000Z".into(),
                used: String::new(),
                hook: "UserPromptSubmit".into(),
                read: 3000,
                bytes: 400,
            }],
            ..Credits::default()
        }
    }

    /// #294: an answer of A bytes carried by N later requests at a known
    /// mix costs A x f re-read, and its counterfactual of C bytes (capped as
    /// the ledger caps it) is credited C x f, with the same f; a hook hit
    /// and its injection likewise.
    #[test]
    fn an_answer_and_its_counterfactual_are_re_read_by_the_same_multiplier() {
        let (c, dir, w) = carried_fixture("s1", 800, 5, None);
        let cost = measure_with(&c, 4.0, Some(dir.as_path()), &hit()).unwrap();
        // The first request sends it; the other four re-read it.
        let f: f64 = w[1..].iter().sum();
        assert_eq!(get(&cost.output_reread, "recall"), 200.0 * 4.0);
        close(get(&cost.output_reread_priced, "recall"), 200.0 * f);
        // 5000 bytes of counterfactual, capped at the p90's 2000: 500 tokens.
        close(get(&cost.credit_reread, "recall"), 500.0 * f);
        close(get(&cost.reread_priced, "UserPromptSubmit"), 100.0 * f);
        close(get(&cost.credit_reread, "hook:UserPromptSubmit"), 750.0 * f);
        assert_eq!((cost.matched, cost.matched_exact, cost.unmatched), (1, 1, 1));
        let by_tool = [row("recall", 500.0), row("hook:UserPromptSubmit", 750.0)];
        let rows = net_rows(&cost, &by_tool);
        let r = rows.iter().find(|r| r.key == "recall").unwrap();
        assert_eq!((r.credit, r.cost), (500.0, 200.0));
        close(r.credit_reread, 500.0 * f);
        close(r.cost_reread, 200.0 * f);
        close(r.net(), (500.0 - 200.0) * (1.0 + f));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #294: a compaction after k of the N requests ends both sides' re-read
    /// at k.
    #[test]
    fn a_compaction_ends_both_sides_re_read() {
        let (c, dir, w) = carried_fixture("s2", 800, 5, Some(3));
        let cost = measure_with(&c, 4.0, Some(dir.as_path()), &hit()).unwrap();
        let f: f64 = w[1..3].iter().sum();
        assert_eq!(get(&cost.output_reread, "recall"), 200.0 * 2.0);
        close(get(&cost.output_reread_priced, "recall"), 200.0 * f);
        close(get(&cost.credit_reread, "recall"), 500.0 * f);
        close(get(&cost.reread_priced, "UserPromptSubmit"), 100.0 * f);
        close(get(&cost.credit_reread, "hook:UserPromptSubmit"), 750.0 * f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #294: answers whose transcript is missing count once on both sides,
    /// and the Net block and the headline line say how much.
    #[test]
    fn answers_without_a_transcript_count_once_and_their_share_is_named() {
        let c = store();
        cap(&c, 2000.0);
        let dir = folder("s3");
        inject(&c, "2026-11-15T10:00:10.000Z", "s", "", &dir.join("gone.jsonl").to_string_lossy(), "UserPromptSubmit", 400);
        log_cf(&c, "2026-11-15T10:00:11.000Z", "recall", 800, 1000, 4000);
        let cost = measure_with(&c, 4.0, Some(dir.as_path()), &hit()).unwrap();
        assert!(cost.credit_reread.is_empty() && cost.output_reread_priced.is_empty() && cost.reread_priced.is_empty(), "{cost:?}");
        assert_eq!(once_only(&cost), (500.0 + 750.0, 200.0 + 100.0));
        let by_tool = [row("recall", 500.0), row("hook:UserPromptSubmit", 750.0)];
        let rows = net_rows(&cost, &by_tool);
        assert_eq!(totals(&rows), totals_once(&rows));
        let once_cost = totals_once(&rows).1;
        let share = format!(
            "1.25k of the credit (100.0%) and 300 of the cost ({:.1}%) is answers with no transcript, counted once",
            300.0 / once_cost * 100.0
        );
        let block = net_block(&cost, &by_tool, 4.0);
        assert!(block.contains(&format!("  no transcript  {share}")), "{block}");
        assert!(headline_net(&cost, &by_tool).contains(&share), "{}", headline_net(&cost, &by_tool));
        assert!(tsv(&cost, &by_tool, 4.0).contains("no transcript (counted once)\tcredit 1250\tcost 300\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #294: with transcripts the headline net has re-reads on both sides
    /// and names the once-only net beside it; the Net block and the tsv keep
    /// the once-only lines, labelled. Without transcripts it says once-only.
    #[test]
    fn the_headline_says_which_net_it_is() {
        let by_tool = [row("recall", 1000.0)];
        let mut cost = Cost { transcripts: true, with_transcript: 1, ..Cost::default() };
        cost.output.insert("recall".into(), 100.0);
        cost.output_reread_priced.insert("recall".into(), 50.0);
        cost.credit_reread.insert("recall".into(), 500.0);
        // Credit 1000 + 500 re-read, cost 100 + 50: net 1350, once-only 900.
        assert_eq!(totals(&net_rows(&cost, &by_tool)), (1500.0, 150.0, 1350.0));
        let line = headline_net(&cost, &by_tool);
        assert!(line.starts_with("1.35k with re-reads on both sides (900 once-only); 0 of the credit (0.0%)"), "{line}");
        let block = net_block(&cost, &by_tool, 4.0);
        for want in [
            "  credit         1.5k (1k once + 500 re-read)",
            "  cost           150 (100 once + 50 re-read, priced)",
            "  net            1.35k (re-reads on both sides)",
            "  net once-only  900 (credit 1k - cost 100: each answer counted once on both sides)",
        ] {
            assert!(block.contains(want), "{want}\n{block}");
        }
        let t = tsv(&cost, &by_tool, 4.0);
        assert!(t.contains("recall\t1500\t150\t1350\t1000\t100\t900\n"), "{t}");
        assert!(t.contains("net once-only\tcredit 1000\tcost 100\tnet 900\n"), "{t}");
        assert!(t.ends_with("net\tcredit 1500\tcost 150\tnet 1350\n"), "{t}");
        let h = human(&cost, &by_tool, 4.0);
        assert!(h.contains("net once-only") && h.contains("  C       re-read, priced"), "{h}");
        let without = Cost { output: cost.output.clone(), ..Cost::default() };
        assert!(headline_net(&without, &by_tool).starts_with("900 once-only: no transcripts read"), "{}", headline_net(&without, &by_tool));
    }

    /// #294: tool output and routed digests stay in context like
    /// injections: their re-read is priced on the cost side and their
    /// counterfactuals' on the credit side. A listed tool (`note`) matches
    /// its operation's row (`read_note`); a plain Bash output is no digest.
    #[test]
    fn digests_and_tool_output_are_re_read_and_so_are_their_counterfactuals() {
        let c = store();
        cap(&c, 2000.0);
        let dir = folder("s5");
        let t = dir.join("main.jsonl");
        let digest = "h:0123456789 cargo test, 40 lines, 9 KB";
        let mut lines = vec![
            tool_use("2026-11-15T10:00:09.000Z", "u1", "toolu_b", "Bash"),
            tool_result("2026-11-15T10:00:10.000Z", "toolu_b", digest, false),
            tool_use("2026-11-15T10:00:19.000Z", "u2", "toolu_n", "mcp__atlas__note"),
            tool_result("2026-11-15T10:00:20.000Z", "toolu_n", &"y".repeat(600), true),
            tool_use("2026-11-15T10:00:29.000Z", "u3", "toolu_x", "Bash"),
            tool_result("2026-11-15T10:00:30.000Z", "toolu_x", "plain output", false),
        ];
        let mut w = Vec::new();
        for (i, m) in [(100, 900, 0, 0), (10, 1000, 50, 0), (0, 2000, 0, 100)].iter().enumerate() {
            lines.push(request(&format!("2026-11-15T10:01:0{}.000Z", i + 1), &format!("m{i}"), OPUS, &usage(m.0, m.1, m.2, m.3)));
            w.push(weight(m.0, m.1, m.2, m.3));
        }
        std::fs::write(&t, lines.join("\n")).unwrap();
        let tr = read_transcript(&t, "").unwrap();
        let found: Vec<(&str, usize)> = tr.answers.iter().map(|a| (a.tool.as_str(), a.bytes)).collect();
        assert_eq!(found, [("route:Bash", digest.len()), ("note", 600)]);
        inject(&c, "2026-11-15T10:00:00.000Z", "s", "", &t.to_string_lossy(), "UserPromptSubmit", 0);
        log_cf(&c, "2026-11-15T10:00:10.500Z", "route:Bash", digest.len() as i64, 0, 40_000);
        log_cf(&c, "2026-11-15T10:00:21.000Z", "read_note", 600, 0, 700);
        log(&c, "2026-11-15T11:00:00Z", "journal", 0);
        let cost = measure_with(&c, 4.0, Some(dir.as_path()), &Credits::default()).unwrap();
        let f: f64 = w[1..].iter().sum();
        close(get(&cost.output_reread_priced, "route:Bash"), digest.len() as f64 / 4.0 * f);
        // A digest is credited at most what Claude Code would have shown.
        close(get(&cost.credit_reread, "route:Bash"), crate::tokens::SHOWN_CHARS as f64 / 4.0 * f);
        close(get(&cost.output_reread_priced, "read_note"), 150.0 * f);
        close(get(&cost.credit_reread, "read_note"), 175.0 * f);
        assert_eq!((cost.matched, cost.unmatched), (2, 1));
        // C's re-read is in the cost total and the C bucket.
        close(cost.c(), (digest.len() as f64 / 4.0 + 150.0) * (1.0 + f));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #294: the measured rulebook is read at a subagent's start, so its
    /// credit is carried as each subagent's first SubagentStart injection;
    /// a subagent without a transcript holds its share once.
    #[test]
    fn the_rulebook_credit_is_carried_as_its_subagent_start() {
        let c = store();
        let dir = folder("s6");
        let t = dir.join("agent.jsonl");
        let mut lines = Vec::new();
        let mut w = Vec::new();
        for (i, m) in [(100, 900, 0, 0), (10, 1000, 50, 0), (0, 2000, 0, 100)].iter().enumerate() {
            lines.push(request(&format!("2026-11-15T10:01:0{}.000Z", i + 1), &format!("m{i}"), OPUS, &usage(m.0, m.1, m.2, m.3)));
            w.push(weight(m.0, m.1, m.2, m.3));
        }
        std::fs::write(&t, lines.join("\n")).unwrap();
        inject(&c, "2026-11-15T10:00:00.000Z", "s", "a1", &t.to_string_lossy(), "SubagentStart", 400);
        inject(&c, "2026-11-15T10:00:30.000Z", "s", "a1", &t.to_string_lossy(), "SubagentStart", 400);
        inject(&c, "2026-11-15T10:00:00.000Z", "s", "a2", &dir.join("gone.jsonl").to_string_lossy(), "SubagentStart", 400);
        // a3 has a transcript but no brief_log row: the rulebook's credit
        // was not measured on it, so it is no part of the average (N2).
        inject(&c, "2026-11-15T10:00:00.000Z", "s", "a3", &t.to_string_lossy(), "SubagentStart", 400);
        brief(&c, &["a1", "a2"]);
        log(&c, "2026-11-15T11:00:00Z", "journal", 0);
        let mut cost = measure(&c, 4.0, Some(dir.as_path())).unwrap();
        let f: f64 = w[1..].iter().sum();
        assert_eq!((cost.start_seen, cost.start_unseen), (1, 1));
        close(cost.start_carried, f);
        let m = crate::tokens::MatchRow { agents: 2, rulebook: Some(1000.0), whole: 0.0, injected: 0.0, start: 0.0 };
        cost.credit_rulebook(Some(&m), 0.0);
        close(cost.rulebook_reread(), 500.0 * f);
        assert_eq!(cost.rulebook_once(), 500.0);
        let rows = net_rows(&cost, &[]);
        let r = rows.iter().find(|r| r.key == RULEBOOK).unwrap();
        close(r.credit_total(), 1000.0 + 500.0 * f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn found<'a>(v: &[(f64, &'a str, usize)]) -> Vec<Found<'a>> {
        v.iter().map(|&(secs, tool, bytes)| Found { secs, tool, bytes, op: false, id: "" }).collect()
    }

    fn logged<'a>(v: &[(f64, &'a str, i64)]) -> Vec<Logged<'a>> {
        v.iter().map(|&(secs, tool, returned)| Logged { secs, tool, returned, id: "", old: true }).collect()
    }

    /// #294: answers match ledger rows each at most once: equal sizes first
    /// (over a tool name), within ten minutes; then any size within a
    /// minute; a digest only its own route's rows.
    #[test]
    fn answers_match_ledger_rows_by_size_then_time_each_once() {
        let answers = found(&[(100.0, "recall", 50), (100.5, "note", 70), (200.0, "route:Bash", 30), (300.0, "recall", 10)]);
        let rows = logged(&[
            (101.0, "recall", 70),
            (101.2, "read_note", 50),
            (700.0, "route:Bash", 30),
            (330.0, "recall", 12),
            (200.0, "route:Grep", 30),
            (1000.0, "recall", 10),
        ]);
        assert_eq!(
            match_answers(&answers, &rows),
            [Some((1, How::Exact)), Some((0, How::Exact)), Some((2, How::Exact)), Some((3, How::Near)), None, None]
        );
    }

    /// A compaction boundary Claude Code made itself, at `pre` tokens.
    fn auto_boundary(time: &str, pre: u64) -> String {
        format!(
            "{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"timestamp\":\"{time}\",\"compactMetadata\":{{\"trigger\":\"auto\",\"preTokens\":{pre}}}}}"
        )
    }

    /// An assistant message calling one tool with an input.
    fn tool_call(time: &str, id: &str, use_id: &str, name: &str, input: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{time}\",\"message\":{{\"id\":\"{id}\",\"model\":\"<synthetic>\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"{use_id}\",\"name\":\"{name}\",\"input\":{input}}}]}}}}"
        )
    }

    /// #297, B1: tool definitions sit in every request's prefix and no
    /// compaction drops them: the start is re-read by every request after
    /// the first, a load by every one after the first that follows it, and
    /// R is the mean requests per scope with a transcript.
    #[test]
    fn tool_definitions_are_re_read_by_every_later_request_through_compactions() {
        let c = store();
        let dir = folder("b1");
        let t = dir.join("main.jsonl");
        let lines = [
            request("2026-11-15T10:00:01.000Z", "m1", "model-not-in-the-table", ""),
            load("2026-11-15T10:00:01.500Z", "mcp__atlas__recall"),
            request("2026-11-15T10:00:02.000Z", "m2", "model-not-in-the-table", ""),
            boundary("2026-11-15T10:00:02.500Z"),
            request("2026-11-15T10:00:03.000Z", "m3", "model-not-in-the-table", ""),
            request("2026-11-15T10:00:04.000Z", "m4", "model-not-in-the-table", ""),
        ];
        std::fs::write(&t, lines.join("\n")).unwrap();
        inject(&c, "2026-11-15T10:00:00Z", "s", "", &t.to_string_lossy(), "UserPromptSubmit", 0);
        log(&c, "2026-11-15T11:00:00Z", "journal", 0);
        let cost = measure(&c, 4.0, Some(dir.as_path())).unwrap();
        let start = start_bytes() as f64 / 4.0;
        let load = tool_bytes("recall") as f64 / 4.0;
        // The start: m2, m3, m4 re-read it, the compaction notwithstanding;
        // the load: m2 sends it, m3 and m4 re-read it.
        assert_eq!(cost.start_reread, 3.0 * start + 2.0 * load);
        assert_eq!(cost.start_reread_priced, 3.0 * start + 2.0 * load);
        assert_eq!(cost.a(), start + load + 3.0 * start + 2.0 * load);
        assert_eq!(cost.requests_per_scope(), 4.0);
        let rows = net_rows(&cost, &[]);
        let a = rows.iter().find(|r| r.key == DEFINITIONS).unwrap();
        assert_eq!((a.cost, a.cost_reread), (start + load, 3.0 * start + 2.0 * load));
        assert!(net_block(&cost, &[], 4.0).contains("  R              4.0 requests per session or subagent with a transcript"));
        assert!(human(&cost, &[], 4.0).contains("R = 4.0 requests per session or subagent"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #297, B2: while any part is unpriced or estimated the net says which
    /// bound it is and why, whether or not it is a floor; with every part
    /// priced it says neither.
    #[test]
    fn the_net_says_which_bound_it_is_and_why() {
        let by_tool = [row("recall", 1000.0)];
        let full = Cost { transcripts: true, with_transcript: 4, ..Cost::default() };
        let mut framed = full.clone();
        framed.framing.insert("SessionStart".into(), 50.0);
        let mut once = full.clone();
        once.once_credit.insert("recall".into(), 300.0);
        once.once_cost.insert("recall".into(), 20.0);
        let mut both = framed.clone();
        both.once_credit = once.once_credit.clone();
        both.once_cost = once.once_cost.clone();
        for (cost, want, why) in [
            (&framed, "an upper bound because: ", "50 of hook text no injection row holds counts once"),
            (&once, "a lower bound because: ", "answers with no transcript count once (credit 300, cost 20)"),
            (
                &both,
                "neither an upper nor a lower bound (known sizes too high 0, too low 0, no reason sized): ",
                "too low because: answers with no transcript",
            ),
        ] {
            for out in [headline_net(cost, &by_tool), net_block(cost, &by_tool, 4.0), tsv(cost, &by_tool, 4.0), human(cost, &by_tool, 4.0)]
            {
                assert!(out.contains(want) && out.contains(why), "{want} {why}\n{out}");
            }
        }
        for out in [headline_net(&full, &by_tool), net_block(&full, &by_tool, 4.0), tsv(&full, &by_tool, 4.0)] {
            assert!(!out.contains("bound"), "{out}");
        }
        // A floor is an upper bound too, and says so.
        let partial = Cost { transcripts: true, with_transcript: 3, without_transcript: 1, ..Cost::default() };
        assert!(
            headline_net(&partial, &by_tool).contains("an upper bound because: costs unseen (1 of 4 sessions"),
            "{}",
            headline_net(&partial, &by_tool)
        );
    }

    /// #297, S1: a counterfactual larger than atlas's answer fills the
    /// context sooner, so its session compacts at the request where the real
    /// context plus the excess reaches the size the session compacted at;
    /// its credit re-read stops there, the answer's goes on.
    #[test]
    fn a_counterfactual_compacts_where_its_larger_context_would() {
        let c = store();
        cap(&c, 2000.0);
        let dir = folder("s1e");
        let t = dir.join("main.jsonl");
        let mut lines = vec![
            tool_use("2026-11-15T10:00:09.000Z", "u1", "toolu_r", "mcp__atlas__recall"),
            tool_result("2026-11-15T10:00:10.000Z", "toolu_r", &"x".repeat(800), true),
        ];
        // Real sizes 500..900; the answer's excess is 500 - 200 = 300 tokens,
        // so the counterfactual reaches 1000 at the third request.
        for (i, size) in [500u64, 600, 700, 800, 900].iter().enumerate() {
            lines.push(request(&format!("2026-11-15T10:01:0{}.000Z", i + 1), &format!("m{i}"), OPUS, &usage(*size, 0, 0, 0)));
        }
        lines.push(auto_boundary("2026-11-15T10:01:05.500Z", 1000));
        std::fs::write(&t, lines.join("\n")).unwrap();
        inject(&c, "2026-11-15T10:00:00.000Z", "s", "", &t.to_string_lossy(), "UserPromptSubmit", 0);
        log_cf(&c, "2026-11-15T10:00:11.000Z", "recall", 800, 1000, 4000);
        log(&c, "2026-11-15T11:00:00Z", "journal", 0);
        let cost = measure_with(&c, 4.0, Some(dir.as_path()), &Credits::default()).unwrap();
        assert_eq!(read_transcript(&t, "").unwrap().compact_at, [1000]);
        assert_eq!(cost.early, 1);
        // Uncached input only: each request weighs 1. The answer is re-read
        // by requests 2 to 5; the counterfactual by request 2 alone.
        close(get(&cost.output_reread_priced, "recall"), 200.0 * 4.0);
        close(get(&cost.credit_reread, "recall"), 500.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #297, S2: a lookup asked again in the same context (or answered
    /// `unchanged since`) would have Read the same files again, and Claude
    /// Code answers that Read with its short stub: its counterfactual is
    /// carried as grep plus the stub. A compaction ends the repeat.
    #[test]
    fn a_repeated_lookup_carries_the_read_stub_not_the_file() {
        let c = store();
        cap(&c, 20_000.0);
        let dir = folder("s2r");
        let t = dir.join("main.jsonl");
        let lines = vec![
            tool_call("2026-11-15T10:00:09.000Z", "u1", "toolu_1", "mcp__atlas__recall", "{\"query\":\"x\"}"),
            tool_result("2026-11-15T10:00:10.000Z", "toolu_1", &"x".repeat(800), true),
            tool_call("2026-11-15T10:00:19.000Z", "u2", "toolu_2", "mcp__atlas__recall", "{\"query\":\"x\"}"),
            tool_result("2026-11-15T10:00:20.000Z", "toolu_2", &"y".repeat(400), true),
            tool_call("2026-11-15T10:00:29.000Z", "u3", "toolu_3", "mcp__atlas__recall", "{\"query\":\"z\"}"),
            tool_result("2026-11-15T10:00:30.000Z", "toolu_3", &format!("unchanged since your call #1{}", "w".repeat(272)), true),
            request("2026-11-15T10:01:01.000Z", "m1", OPUS, &usage(1, 0, 0, 0)),
            request("2026-11-15T10:01:02.000Z", "m2", OPUS, &usage(1, 0, 0, 0)),
            boundary("2026-11-15T10:01:02.500Z"),
            tool_call("2026-11-15T10:01:03.000Z", "u4", "toolu_4", "mcp__atlas__recall", "{\"query\":\"x\"}"),
            tool_result("2026-11-15T10:01:04.000Z", "toolu_4", &"v".repeat(600), true),
            request("2026-11-15T10:01:05.000Z", "m3", OPUS, &usage(1, 0, 0, 0)),
            request("2026-11-15T10:01:06.000Z", "m4", OPUS, &usage(1, 0, 0, 0)),
        ];
        std::fs::write(&t, lines.join("\n")).unwrap();
        let tr = read_transcript(&t, "").unwrap();
        assert_eq!(tr.answers.iter().map(|a| a.repeat).collect::<Vec<_>>(), [false, true, true, false]);
        inject(&c, "2026-11-15T10:00:00.000Z", "s", "", &t.to_string_lossy(), "UserPromptSubmit", 0);
        log_cf(&c, "2026-11-15T10:00:10.500Z", "recall", 800, 1000, 4000);
        log_cf(&c, "2026-11-15T10:00:20.500Z", "recall", 400, 1000, 4000);
        log_cf(&c, "2026-11-15T10:00:30.500Z", "recall", 300, 1000, 4000);
        log_cf(&c, "2026-11-15T10:01:04.500Z", "recall", 600, 1000, 4000);
        log(&c, "2026-11-15T11:00:00Z", "journal", 0);
        let cost = measure_with(&c, 4.0, Some(dir.as_path()), &Credits::default()).unwrap();
        assert_eq!((cost.matched, cost.repeats), (4, 2));
        // Each re-read once (m2, then m4): the first and last whole (5000
        // bytes), the two repeats grep plus the stub.
        let stub = (1000 + READ_STUB_BYTES) as f64 / 4.0;
        close(get(&cost.credit_reread, "recall"), 1250.0 + 2.0 * stub + 1250.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #297, S3: a hook hit's counterfactual read enters context when the
    /// record was used, so it is carried from then, not from the injection.
    #[test]
    fn a_hook_hit_is_carried_from_its_use() {
        let (c, dir, w) = carried_fixture("s3u", 800, 5, None);
        let mut credits = hit();
        credits.hits[0].used = "2026-11-15T10:01:02.500Z".into();
        let cost = measure_with(&c, 4.0, Some(dir.as_path()), &credits).unwrap();
        // Request 3 sends it; 4 and 5 re-read it.
        close(get(&cost.credit_reread, "hook:UserPromptSubmit"), 750.0 * (w[3] + w[4]));
        // The injection itself is carried from its own time.
        close(get(&cost.reread_priced, "UserPromptSubmit"), 100.0 * w[1..].iter().sum::<f64>());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #297, S4: a routed Read's row joins its answer by tool call id, never
    /// by time; a time-only match needs the same tool (or a call that named
    /// an op) and sizes within the tolerance.
    #[test]
    fn a_routed_read_joins_by_id_and_a_time_only_match_is_bounded() {
        let answers = vec![
            Found { secs: 5000.0, tool: "route:Read", bytes: 90, op: false, id: "toolu_far" },
            Found { secs: 100.0, tool: "route:Read", bytes: 40, op: false, id: "toolu_near" },
            Found { secs: 200.0, tool: "section", bytes: 300, op: false, id: "a" },
            Found { secs: 300.0, tool: "section", bytes: 300, op: false, id: "b" },
            Found { secs: 400.0, tool: "note", bytes: 300, op: true, id: "c" },
        ];
        let rows = vec![
            Logged { secs: 100.5, tool: "route:Read", returned: 40, id: "toolu_far", old: true },
            Logged { secs: 210.0, tool: "recall", returned: 29_292, id: "", old: true },
            Logged { secs: 211.0, tool: "recall", returned: 320, id: "", old: true },
            Logged { secs: 310.0, tool: "section", returned: 360, id: "", old: true },
            Logged { secs: 410.0, tool: "read_note", returned: 330, id: "", old: true },
        ];
        assert_eq!(match_answers(&answers, &rows), [Some((0, How::Id)), None, None, Some((3, How::Near)), Some((4, How::Near))]);
    }

    /// #297, N1: an answer or a row with no time never matches.
    #[test]
    fn a_missing_time_never_matches() {
        let answers = found(&[(f64::NAN, "recall", 2), (100.0, "recall", 7)]);
        let rows = logged(&[(5000.0, "recall", 2), (f64::NAN, "recall", 7)]);
        assert_eq!(match_answers(&answers, &rows), [None, None]);
    }

    /// #297, S4: the report shows the credit and cost re-read time-only
    /// matches carry, the net with them counted once, and the transcript
    /// answers found and left unmatched; the net is then an upper bound.
    #[test]
    fn time_only_matches_are_reported_with_a_sensitivity_net() {
        let (c, dir, w) = carried_fixture("s4r", 800, 5, None);
        // The row logged 50 bytes off the answer: a time-only match.
        c.execute("UPDATE token_log SET returned = 850 WHERE tool = 'recall'", []).unwrap();
        let cost = measure_with(&c, 4.0, Some(dir.as_path()), &Credits::default()).unwrap();
        let f: f64 = w[1..].iter().sum();
        assert_eq!((cost.matched, cost.matched_exact, cost.answers, cost.answers_unmatched), (1, 0, 1, 0));
        close(cost.credit_reread_near, 500.0 * f);
        close(cost.cost_reread_near, 212.5 * f);
        let by_tool = [row("recall", 500.0)];
        let rows = net_rows(&cost, &by_tool);
        let sens = totals(&rows).2 - (500.0 - 212.5) * f;
        close(net_near_once(&cost, &rows), sens);
        let block = net_block(&cost, &by_tool, 4.0);
        assert!(block.contains(&format!("  net, loose     {} (the net with the ", crate::token_report::compact(sens))), "{block}");
        assert!(headline_net(&cost, &by_tool).contains("loose (time-only) matches carry "), "{}", headline_net(&cost, &by_tool));
        assert!(tsv(&cost, &by_tool, 4.0).contains(&format!("loose matches\tcredit re-read {:.0}\t", 500.0 * f)));
        assert!(human(&cost, &by_tool, 4.0).contains("1 answers found in transcripts, 0 left unmatched"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #297, R1: a routed digest logged by an older build holds the newline
    /// Claude Code trims, one byte over its answer: still an equal-bytes
    /// match, where a row of the exact size wins a tie in time; an atlas
    /// tool's row is not.
    #[test]
    fn a_routed_row_one_byte_over_its_answer_matches_by_size() {
        let answers = found(&[(100.0, "route:Bash", 299), (200.0, "route:PowerShell", 50), (300.0, "recall", 299)]);
        let rows =
            logged(&[(100.1, "route:Bash", 300), (200.1, "route:PowerShell", 51), (200.1, "route:PowerShell", 50), (300.1, "recall", 300)]);
        assert_eq!(match_answers(&answers, &rows), [Some((0, How::Exact)), None, Some((1, How::Exact)), Some((2, How::Near))]);
        // #297 review 4, S-2: with the row one byte over nearer in time
        // (200.1) than the exact row (200.3), time wins and the exact row is
        // left over (#303).
        let rows =
            logged(&[(100.1, "route:Bash", 300), (200.1, "route:PowerShell", 51), (200.3, "route:PowerShell", 50), (300.1, "recall", 300)]);
        assert_eq!(match_answers(&answers, &rows), [Some((0, How::Exact)), Some((1, How::Exact)), None, Some((2, How::Near))]);
    }

    /// #297, S-a: a time-only match of the same tool still needs sizes
    /// within the tolerance.
    #[test]
    fn a_time_only_match_of_the_same_tool_needs_a_near_size() {
        let answers = found(&[(100.0, "section", 300), (200.0, "section", 300)]);
        let rows = logged(&[(110.0, "section", 1000), (210.0, "section", 360)]);
        assert_eq!(match_answers(&answers, &rows), [None, Some((1, How::Near))]);
    }

    /// A subagent transcript of five requests (real sizes 500..900 tokens)
    /// that compacted on its own at 1000, its SubagentStart injection of 400
    /// bytes at 10:00:00, measured for the rulebook.
    fn start_fixture(name: &str) -> (Connection, std::path::PathBuf) {
        let c = store();
        let dir = folder(name);
        let t = dir.join("agent.jsonl");
        let mut lines = Vec::new();
        for (i, size) in [500u64, 600, 700, 800, 900].iter().enumerate() {
            lines.push(request(&format!("2026-11-15T10:01:0{}.000Z", i + 1), &format!("m{i}"), OPUS, &usage(*size, 0, 0, 0)));
        }
        lines.push(auto_boundary("2026-11-15T10:01:05.500Z", 1000));
        std::fs::write(&t, lines.join("\n")).unwrap();
        inject(&c, "2026-11-15T10:00:00.000Z", "s", "a1", &t.to_string_lossy(), "SubagentStart", 400);
        brief(&c, &["a1"]);
        log(&c, "2026-11-15T11:00:00Z", "journal", 0);
        (c, dir)
    }

    /// #297, S-c: the counterfactual compaction counts the excess Net
    /// credits. Without a measured rulebook a SubagentStart hit's excess is
    /// in; with one, the hit (credited 0) is out and the rulebook's excess
    /// over the injection is in.
    #[test]
    fn the_compaction_counts_the_excess_net_credits() {
        let (c, dir) = start_fixture("s-c");
        // Its excess: 400 - 100 = 300 tokens, so 700 + 300 reaches 1000.
        let hit = Hit {
            session: "s".into(),
            agent: "a1".into(),
            time: "2026-11-15T10:00:00.000Z".into(),
            used: String::new(),
            hook: "SubagentStart".into(),
            read: 1600,
            bytes: 400,
        };
        let early = |hits: Vec<Hit>, rulebook: Option<f64>| {
            measure_with(&c, 4.0, Some(dir.as_path()), &Credits { hits, rulebook, ..Credits::default() }).unwrap().early
        };
        assert_eq!(early(vec![hit.clone()], None), 1);
        // A rulebook of 100 tokens per agent: no excess over the 100 injected.
        assert_eq!(early(vec![hit], Some(100.0)), 0);
        // A rulebook of 400: the same excess as the hit had.
        assert_eq!(early(Vec::new(), Some(400.0)), 1);
        let m = crate::tokens::MatchRow { agents: 2, rulebook: Some(1000.0), whole: 0.0, injected: 0.0, start: 0.0 };
        assert_eq!(rulebook_net(Some(&m), 200.0), Some(800.0));
        assert_eq!(rulebook_net(Some(&crate::tokens::MatchRow { rulebook: None, ..m }), 0.0), None);
        // #297 review 3, N-2: the excess is spread over the count Net spreads
        // the rulebook on, the briefed subagents with a start row, seen or
        // not. A rulebook of 300 on one subagent: 300 - 100 = 200, so 900 +
        // 200 passes 1000. A second briefed subagent without a transcript
        // halves the share: 150 - 100 = 50, and 900 + 50 stays under it.
        assert_eq!(early(Vec::new(), Some(300.0)), 1);
        inject(&c, "2026-11-15T10:00:00.000Z", "s", "a2", "", "SubagentStart", 400);
        brief(&c, &["a2"]);
        let two = measure_with(&c, 4.0, Some(dir.as_path()), &Credits { rulebook: Some(300.0), ..Credits::default() }).unwrap();
        assert_eq!((two.start_seen, two.start_unseen, two.early), (1, 1, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #297, S-d: with reasons on both sides the known sizes are weighed;
    /// a lopsided pair names the larger side, and each side lists its
    /// largest reason first.
    #[test]
    fn the_bound_names_the_larger_side_by_size() {
        let by_tool = [row("recall", 1000.0)];
        let mut cost = Cost { transcripts: true, with_transcript: 4, ..Cost::default() };
        // Hook text 50, its re-read at 10000 / 100 per token sent: 5000.
        cost.framing.insert("SessionStart".into(), 50.0);
        cost.sent.insert("UserPromptSubmit".into(), 100.0);
        cost.reread_priced.insert("UserPromptSubmit".into(), 10_000.0);
        // Loose matches: 9000 - 1000 = 8000 too high.
        cost.credit_reread_near = 9000.0;
        cost.cost_reread_near = 1000.0;
        // No transcript: (300 - 20) at 20 / 10 = 2 per token: 560 too low.
        cost.once_credit.insert("recall".into(), 300.0);
        cost.once_cost.insert("recall".into(), 20.0);
        cost.matched_out = 10.0;
        cost.output_reread_priced.insert("recall".into(), 20.0);
        let b = bound(&cost).unwrap();
        assert!(
            b.starts_with(
                "more likely an upper bound (known sizes too high 13k, too low 560): too high because: loose (time-only) matches"
            ),
            "{b}"
        );
        assert!(b.find("loose (time-only)") < b.find("of hook text"), "{b}");
        assert!(b.ends_with("too low because: answers with no transcript count once (credit 300, cost 20, about 560 re-read)"), "{b}");
        // The other way round.
        cost.credit_reread_near = 0.0;
        cost.cost_reread_near = 0.0;
        cost.output_reread_priced.insert("recall".into(), 1000.0);
        let b = bound(&cost).unwrap();
        assert!(b.starts_with("more likely a lower bound (known sizes too high 5k, too low 28k)"), "{b}");
        // Within the margin: neither.
        cost.output_reread_priced.insert("recall".into(), 300.0);
        let b = headline_net(&cost, &by_tool);
        assert!(b.contains("neither an upper nor a lower bound (known sizes too high 5k, too low 8.4k, within 2x)"), "{b}");
        // Lopsided, but the smaller side holds a reason of unknown size (a
        // lookup's cross-lookup Read): it is not outweighed.
        cost.output_reread_priced.insert("recall".into(), 1000.0);
        cost.credit_reread.insert("recall".into(), 50.0);
        let b = bound(&cost).unwrap();
        assert!(
            b.starts_with("neither an upper nor a lower bound (known sizes too high 5k, too low 28k; a too high reason of unknown size is not weighed)"),
            "{b}"
        );
        // #297 review 3, S-2: the larger side holds an unknown too (the
        // cross-lookup Read on the high side; no matched output leaves the
        // no-transcript reason unsized on the low side). The low side is the
        // one not weighed.
        cost.matched_out = 0.0;
        let b = bound(&cost).unwrap();
        assert!(
            b.starts_with(
                "neither an upper nor a lower bound (known sizes too high 5k, too low 0; a too low reason of unknown size is not weighed)"
            ),
            "{b}"
        );
    }

    /// #297 review 3, M-1: pairs are taken nearest in time first across all
    /// answers, so an earlier answer does not take a later answer's
    /// equal-size row when that row is nearer to the later answer. The
    /// residual cases (a nearer row one byte over; old-store contention)
    /// are #303.
    #[test]
    fn pairs_are_taken_nearest_in_time_first_across_answers() {
        // B (300 bytes) has its own row of 301 at +0.1 s; A (299 bytes) has
        // its own row of 300 at +0.1 s, 300 s later.
        let answers = found(&[(100.0, "route:Bash", 300), (400.0, "route:Bash", 299)]);
        let rows = logged(&[(100.1, "route:Bash", 301), (400.1, "route:Bash", 300)]);
        assert_eq!(match_answers(&answers, &rows), [Some((0, How::Exact)), Some((1, How::Exact))]);
        // Closer in time: B (300 at 100.0) has its row of 301 at 100.5; A
        // (299 at 100.2) has its row of 300 at 100.3, nearer to A than to B.
        let answers = found(&[(100.0, "route:Bash", 300), (100.2, "route:Bash", 299)]);
        let rows = logged(&[(100.3, "route:Bash", 300), (100.5, "route:Bash", 301)]);
        assert_eq!(match_answers(&answers, &rows), [Some((1, How::Exact)), Some((0, How::Exact))]);
    }

    /// #297, N-a: a transcript that never compacted on its own compacts its
    /// counterfactual at the median size the others compacted at.
    #[test]
    fn a_transcript_that_never_compacted_takes_the_median_compaction_size() {
        let c = store();
        cap(&c, 2000.0);
        let dir = folder("n-a");
        let other = dir.join("other.jsonl");
        std::fs::write(
            &other,
            [request("2026-11-15T09:00:01.000Z", "o1", OPUS, &usage(1, 0, 0, 0)), auto_boundary("2026-11-15T09:00:02.000Z", 1000)]
                .join("\n"),
        )
        .unwrap();
        let t = dir.join("main.jsonl");
        let mut lines = vec![
            tool_use("2026-11-15T10:00:09.000Z", "u1", "toolu_r", "mcp__atlas__recall"),
            tool_result("2026-11-15T10:00:10.000Z", "toolu_r", &"x".repeat(800), true),
        ];
        for (i, size) in [500u64, 600, 700, 800, 900].iter().enumerate() {
            lines.push(request(&format!("2026-11-15T10:01:0{}.000Z", i + 1), &format!("m{i}"), OPUS, &usage(*size, 0, 0, 0)));
        }
        std::fs::write(&t, lines.join("\n")).unwrap();
        inject(&c, "2026-11-15T09:00:00.000Z", "o", "", &other.to_string_lossy(), "UserPromptSubmit", 0);
        inject(&c, "2026-11-15T10:00:00.000Z", "s", "", &t.to_string_lossy(), "UserPromptSubmit", 0);
        log_cf(&c, "2026-11-15T10:00:11.000Z", "recall", 800, 1000, 4000);
        log(&c, "2026-11-15T11:00:00Z", "journal", 0);
        let cost = measure_with(&c, 4.0, Some(dir.as_path()), &Credits::default()).unwrap();
        assert!(read_transcript(&t, "").unwrap().compact_at.is_empty());
        assert_eq!(cost.early, 1);
        close(get(&cost.credit_reread, "recall"), 500.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #297, N-a: only an automatic compaction gives the size a session
    /// compacts at; a manual one is a boundary but no size.
    #[test]
    fn only_an_automatic_compaction_gives_a_size() {
        let dir = folder("n-a-auto");
        let t = dir.join("main.jsonl");
        let manual = "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"timestamp\":\"2026-11-15T10:00:01.000Z\",\"compactMetadata\":{\"trigger\":\"manual\",\"preTokens\":600}}";
        std::fs::write(&t, [manual.to_string(), auto_boundary("2026-11-15T10:00:02.000Z", 1000)].join("\n")).unwrap();
        let tr = read_transcript(&t, "").unwrap();
        assert_eq!((tr.boundaries.len(), tr.compact_at.clone()), (2, vec![1000]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #297, N-a: a real compaction drops the counterfactual's excess and
    /// its base; later excess counts from the real context again.
    #[test]
    fn a_real_compaction_resets_the_counterfactual() {
        let q = |time: &str, size: u64| Request {
            time: time.into(),
            model: OPUS.into(),
            usage: Some(Usage { input: size, cache_read: 0, write_5m: 0, write_1h: 0 }),
        };
        // Excess 600 makes the first request compact early (base 500); the
        // real compaction after it resets the base, so 800 + 300 compacts
        // again.
        let t = Transcript { requests: vec![q("t1", 500), q("t3", 800)], boundaries: vec!["t1.5".into()], ..Transcript::default() };
        let excess = vec![("t0".to_string(), 600.0), ("t2".to_string(), 300.0)];
        assert_eq!(early_compactions(&t, &excess, Some(1000.0)), ["t1", "t3"]);
        let t = Transcript {
            requests: vec![q("t1", 500), q("t2", 600), q("t3", 750)],
            boundaries: vec!["t2.5".into()],
            ..Transcript::default()
        };
        // The real compaction drops the excess: 750 + 300 would reach 1000.
        assert!(early_compactions(&t, &[("t0".to_string(), 300.0)], Some(1000.0)).is_empty());
    }

    /// #303, case (a): above the store's trimmed-digest marker a routed row
    /// must equal its answer's size, so an orphan row one byte over, nearer
    /// in time, no longer takes the answer from its own row.
    #[test]
    fn a_row_one_byte_over_above_the_marker_does_not_steal() {
        let answers = found(&[(100.0, "route:Bash", 300)]);
        let mut rows = logged(&[(100.1, "route:Bash", 301), (100.5, "route:Bash", 300)]);
        rows.iter_mut().for_each(|r| r.old = false);
        assert_eq!(match_answers(&answers, &rows), [None, Some((0, How::Exact))]);
    }

    /// #303: the report reads the store's marker, so a routed answer above
    /// it is matched to its own equal row, not to the one byte over.
    #[test]
    fn the_report_reads_the_trimmed_digest_marker() {
        let c = store();
        let dir = folder("i303");
        let t = dir.join("main.jsonl");
        let digest = format!("h:0123456789 {}", "x".repeat(287));
        assert_eq!(digest.len(), 300);
        let lines = [
            tool_use("2026-11-15T10:00:09.000Z", "u1", "toolu_b", "Bash"),
            tool_result("2026-11-15T10:00:10.000Z", "toolu_b", &digest, true),
            request("2026-11-15T10:01:00.000Z", "m1", OPUS, &usage(100, 900, 0, 0)),
        ];
        std::fs::write(&t, lines.join("\n")).unwrap();
        inject(&c, "2026-11-15T10:00:10.000Z", "s", "", &t.to_string_lossy(), "UserPromptSubmit", 400);
        log(&c, "2026-11-15T10:00:10.100Z", "route:Bash", 301);
        log(&c, "2026-11-15T10:00:10.500Z", "route:Bash", 300);
        let out = |c: &Connection| measure_with(c, 4.0, Some(dir.as_path()), &Credits::default()).unwrap().matched_out;
        // No marker: both rows may be an older build's; the nearer +1 wins.
        assert_eq!(out(&c), 301.0 / 4.0);
        c.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL); INSERT INTO meta VALUES('token_log_trimmed_from', '1');",
        )
        .unwrap();
        assert_eq!(out(&c), 300.0 / 4.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #303: below the marker (or on a store with none) a routed row one
    /// byte over still matches, as an older build logged it.
    #[test]
    fn a_row_one_byte_over_below_the_marker_still_matches() {
        let answers = found(&[(100.0, "route:Bash", 300)]);
        let rows = logged(&[(100.1, "route:Bash", 301), (100.5, "route:Bash", 300)]);
        assert_eq!(match_answers(&answers, &rows), [Some((0, How::Exact)), None]);
        let rows = logged(&[(100.1, "route:Bash", 301)]);
        assert_eq!(match_answers(&answers, &rows), [Some((0, How::Exact))]);
    }
}
