//! The compaction checkpoint (part (a) of `docs/design/compaction.md`):
//! what PreCompact records, what SessionStart(compact) injects, the
//! `compact.defer` decision, and the per-request context curve the
//! threshold sweep replays.
//!
//! A checkpoint is a `journal` record of entry kind `checkpoint` in the
//! session's lane (`session:<id>`). Its body is ref lines, not restated
//! text:
//!
//! ```text
//! since: journal:session:<id>:0003@3f9c2a1b7e40
//! lanes: build-7 (step journal:build-7:0012@...), docs (step ...)
//! open: journal:build-7:0009@...
//! owed: journal:build-7:0010@...
//! rulings: rules:r00412@...
//! files: src/a.rs, src/b.rs
//! tools: Read src/a.rs -> turn:1a2b3c4d:812@...; Bash cargo test -> turn:...
//! pending: Bash cargo test --no-fail-fast
//! intent: Next I will run the tests and fix the failing one.
//! ```
//!
//! Only visible state is read: user prompts, assistant text, tool calls and
//! tool results. Thinking blocks are never read, stored or measured. Every
//! line taken from the transcript (a tool target, the last assistant line)
//! passes [`crate::redact`] first.
//!
//! A tool result's ref is `turn:<session8>:<n>@hash`: `n` is the 1-based line
//! of the transcript record holding the result, `hash` the first 12 hex
//! digits of SHA-256 over that record's visible text ([`turn_text`], the text
//! and tool-result blocks joined with `\n`). Transcripts are append-only, so a
//! turn ref stays valid for the transcript's life.

use std::collections::HashMap;
use std::path::Path;

use crate::compact::{record_ref, HASH_LEN};
use crate::error::Result;
use crate::json::Value;
use crate::memory::{Head, MemoryStore};

/// The tool calls a checkpoint lists, newest last.
pub const TOOLS: usize = 6;

/// The files a checkpoint lists, newest first.
pub const FILES: usize = 12;

/// The most refs per line (lanes, open, owed, rulings).
pub const PER_LINE: usize = 6;

/// The longest tool target or intent kept, in characters.
const TARGET_CHARS: usize = 100;
const INTENT_CHARS: usize = 200;

/// The fewest tokens of headroom `compact.defer` keeps.
pub const DEFER_MIN_MARGIN: u64 = 40_000;

/// How long PreCompact waits for a transcript Claude Code has not written
/// yet (a session forked and compacted at once) before reading the session
/// it was forked from.
pub const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// How often the wait re-reads the transcript.
const SETTLE_STEP: std::time::Duration = std::time::Duration::from_millis(100);

/// A sibling transcript written this recently is taken as the parent of a
/// session whose own transcript is not written yet, when it is the only one.
const PARENT_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

/// How far back the window reaches when the segment's start is unknown (an
/// empty or unreadable transcript): never all time.
pub const UNKNOWN_START_HOURS: i64 = 24;

/// The card line for a checkpoint whose transcript could not be read.
pub const UNREAD_LINE: &str = "unread: the transcript held no usage record at compaction (not written yet); files, tools, pending and intent are missing";

/// Which lanes a checkpoint lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lanes {
    /// Every lane in the store: the store is the session's project's own.
    Store,
    /// The session's own lane only (no project is known, so the store may
    /// hold other projects' lanes and rulings).
    Session,
}

impl Lanes {
    /// [`Lanes::Store`] when a project is active (its store is its own),
    /// else [`Lanes::Session`].
    pub fn for_project(g: Option<&crate::project::Graph>) -> Lanes {
        if g.is_some_and(|g| !g.name.is_empty()) {
            Lanes::Store
        } else {
            Lanes::Session
        }
    }
}

/// Where a checkpoint's transcript side came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailFrom {
    /// The session's own transcript.
    Own,
    /// The transcript of the session it was forked from (its own was not
    /// written yet).
    Parent(std::path::PathBuf),
    /// Nothing readable: no usage record in the session's transcript, and no
    /// single recent sibling to fall back to.
    Unread,
}

/// One tool call in the segment before a compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The call's id.
    pub id: String,
    /// The tool.
    pub tool: String,
    /// Its target, redacted and cut (a path, a pattern, a command).
    pub target: String,
    /// Its result's turn ref, once the result is in.
    pub result: Option<String>,
}

/// What a transcript's segment since its last compaction shows: the
/// in-task state a compaction would cut, and the context curve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tail {
    /// The segment's first record's time (UTC, as the transcript writes it).
    pub start: String,
    /// Files edited or written, newest first, each once.
    pub files: Vec<String>,
    /// The last [`TOOLS`] tool calls, oldest first.
    pub steps: Vec<Step>,
    /// Whether the last assistant record ends in a tool call (a tool chain
    /// is running), with no person's prompt after it.
    pub mid_task: bool,
    /// The last assistant text's last line, redacted and cut.
    pub intent: String,
    /// The newest request's context tokens.
    pub last_ctx: u64,
    /// The most context any one request added in the segment.
    pub max_growth: u64,
    /// The largest context in the whole transcript (for the window guess).
    pub max_ctx: u64,
}

/// A record's visible text: its text blocks and tool-result blocks (or its
/// string content), joined with `\n`. Thinking blocks are never read.
pub fn turn_text(v: &Value) -> String {
    let mut out: Vec<&str> = Vec::new();
    match v.pointer("/message/content") {
        Some(Value::String(s)) => out.push(s),
        Some(Value::Array(a)) => {
            for x in a {
                match x.get("type").and_then(Value::as_str) {
                    Some("text") => out.extend(x.get("text").and_then(Value::as_str)),
                    Some("tool_result") => match x.get("content") {
                        Some(Value::String(s)) => out.push(s),
                        Some(Value::Array(c)) => out.extend(c.iter().filter(|y| y.get("type").and_then(Value::as_str) == Some("text")).filter_map(|y| y.get("text").and_then(Value::as_str))),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out.join("\n")
}

/// The ref of a transcript turn: `turn:<session8>:<line>@hash`.
pub fn turn_ref(session: &str, line: usize, text: &str) -> String {
    let s8: String = session.chars().take(8).collect();
    format!("turn:{s8}:{line}@{}", &crate::sha256::hex(text.as_bytes())[..HASH_LEN])
}

/// One line, cut to `n` characters on a char boundary.
fn cut(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(3)).collect::<String>() + "..."
}

/// A tool call's target, as a person would name it.
pub fn target(tool: &str, input: &Value) -> String {
    let f = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let t = match tool {
        "Read" | "Edit" | "Write" | "MultiEdit" => f("file_path"),
        "NotebookEdit" => f("notebook_path"),
        "Grep" => [f("pattern"), f("path"), f("glob")].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" "),
        "Glob" => [f("pattern"), f("path")].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" "),
        "Bash" | "PowerShell" => f("command"),
        "Agent" | "Task" => f("description"),
        "WebFetch" => f("url"),
        "WebSearch" => f("query"),
        _ => ["key", "query", "path", "id", "heading", "name", "pattern", "lane"].iter().map(|k| f(k)).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" "),
    };
    // One line only: a command's first line, never its heredoc body.
    let first = t.lines().next().unwrap_or("");
    cut(&crate::redact::redact(first), TARGET_CHARS).replace('\\', "/")
}

/// Reads a transcript's segment since its last compaction boundary.
/// Subagent (sidechain) records are skipped. Fails open: an unreadable file
/// is an empty tail.
pub fn tail(path: &Path, session: &str) -> Tail {
    let Ok(bytes) = std::fs::read(path) else { return Tail::default() };
    let text = String::from_utf8_lossy(&bytes);
    // The record after the newest boundary starts the segment: a main-chain
    // record whose subtype is compact_boundary, as `segments` reads it (the
    // bare word may sit in any field, a Grep pattern for one).
    let mut end = text.len();
    let mut found = None;
    while let Some(at) = text[..end].rfind("\"compact_boundary\"") {
        let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        let stop = text[at..].find('\n').map_or(text.len(), |i| at + i + 1);
        if is_boundary(&text[start..stop]) {
            found = Some((start, stop));
            break;
        }
        end = start;
    }
    let (from, first_line) = match found {
        Some((start, stop)) => (stop, text.as_bytes()[..start].iter().filter(|b| **b == b'\n').count() + 2),
        None => (0, 1),
    };
    let max_ctx = max_context(&text[..from]);
    let mut t = scan(&text[from..], first_line, session);
    t.max_ctx = t.max_ctx.max(max_ctx);
    t
}

/// Whether a transcript line is a compaction boundary of the main chain.
fn is_boundary(line: &str) -> bool {
    line.contains("\"compact_boundary\"")
        && !line.contains("\"isSidechain\":true")
        && crate::json::parse(line.trim_end()).is_ok_and(|v| v.get("subtype").and_then(Value::as_str) == Some("compact_boundary"))
}

/// Whether a tail shows the transcript was read: a request with usage.
fn has_usage(t: &Tail) -> bool {
    t.last_ctx > 0
}

/// Whether a transcript holds any user or assistant record yet (a missing
/// file holds none).
fn has_turns(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|b| {
        let s = String::from_utf8_lossy(&b);
        s.contains("\"type\":\"user\"") || s.contains("\"type\":\"assistant\"")
    })
}

/// [`tail`], for a PreCompact that may run before Claude Code has written
/// the transcript (a session forked and compacted at once). While the file
/// shows no request with usage it is read again every 100 ms for up to
/// `wait`. A transcript that already holds turns is the session's own, even
/// without usage. One that still holds none is read from the session it was
/// forked from: the only other transcript in the same folder written in the
/// last ten minutes. Without one the tail is empty and [`TailFrom::Unread`].
pub fn tail_settled(path: &Path, session: &str, wait: std::time::Duration) -> (Tail, TailFrom) {
    let t0 = std::time::Instant::now();
    loop {
        let t = tail(path, session);
        if has_usage(&t) {
            return (t, TailFrom::Own);
        }
        if t0.elapsed() + SETTLE_STEP > wait {
            if has_turns(path) {
                return (t, TailFrom::Own);
            }
            break;
        }
        std::thread::sleep(SETTLE_STEP);
    }
    if let Some(p) = parent_transcript(path) {
        let stem = p.file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        let t = tail(&p, &stem);
        if has_usage(&t) {
            return (t, TailFrom::Parent(p));
        }
    }
    (Tail::default(), TailFrom::Unread)
}

/// The one other `.jsonl` beside `path` modified within [`PARENT_WINDOW`];
/// `None` when there is none or more than one (which one was forked from
/// is not known then).
fn parent_transcript(path: &Path) -> Option<std::path::PathBuf> {
    let dir = path.parent()?;
    let own = path.file_name()?;
    let now = std::time::SystemTime::now();
    let mut found = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.file_name() == Some(own) || p.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let recent = e.metadata().and_then(|m| m.modified()).is_ok_and(|m| now.duration_since(m).unwrap_or_default() <= PARENT_WINDOW);
        if recent {
            if found.is_some() {
                return None;
            }
            found = Some(p);
        }
    }
    found
}

/// Seconds since the epoch of a UTC time as the store and transcripts write
/// it (`YYYY-MM-DDTHH:MM:SS`, any fraction or zone suffix ignored).
fn epoch_secs(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |a: usize, z: usize| t.get(a..z)?.parse::<i64>().ok();
    let (y, m, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (hh, mm, ss) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // Days from civil (H. Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// The UTC time `hours` before `until` (now when `until` is empty or not a
/// time).
fn hours_before(until: &str, hours: i64) -> String {
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let base = epoch_secs(until).unwrap_or_else(now);
    crate::discussion::civil(base - hours * 3600)
}

/// The largest context a text of transcript records shows.
fn max_context(text: &str) -> u64 {
    let mut best = 0;
    for line in text.lines().filter(|l| l.contains("\"usage\"") && !l.contains("\"isSidechain\":true")) {
        if let Ok(v) = crate::json::parse(line) {
            best = best.max(usage_ctx(&v).unwrap_or(0));
        }
    }
    best
}

/// A record's context tokens: `input + cache_creation + cache_read + output`.
pub fn usage_ctx(v: &Value) -> Option<u64> {
    let u = v.pointer("/message/usage")?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens") + n("output_tokens"))
}

/// Scans transcript records (`segment`, whose first line is line
/// `first_line` of the file) for the tail state.
pub fn scan(segment: &str, first_line: usize, session: &str) -> Tail {
    let mut t = Tail::default();
    let mut steps: Vec<Step> = Vec::new();
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut last_msg = String::new();
    let mut prev_ctx = 0u64;
    for (i, line) in segment.lines().enumerate() {
        let n = first_line + i;
        if line.contains("\"isSidechain\":true") {
            continue;
        }
        let user = line.contains("\"type\":\"user\"");
        let assistant = line.contains("\"type\":\"assistant\"");
        if !(user || assistant) {
            continue;
        }
        let Ok(v) = crate::json::parse(line) else { continue };
        if t.start.is_empty() {
            t.start = v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        }
        if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let blocks = v.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
        if assistant {
            let id = v.pointer("/message/id").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(c) = usage_ctx(&v) {
                if id != last_msg && c > 0 {
                    if prev_ctx > 0 && c > prev_ctx {
                        t.max_growth = t.max_growth.max(c - prev_ctx);
                    }
                    prev_ctx = c;
                    t.last_ctx = c;
                    t.max_ctx = t.max_ctx.max(c);
                }
                last_msg = id;
            }
            for b in &blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let tool = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        let input = b.get("input").cloned().unwrap_or(Value::Null);
                        if matches!(tool.as_str(), "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
                            let f = target(&tool, &input);
                            if !f.is_empty() {
                                t.files.retain(|x| x != &f);
                                t.files.insert(0, f);
                            }
                        }
                        let id = b.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                        ids.insert(id.clone(), steps.len());
                        steps.push(Step { id, target: target(&tool, &input), tool, result: None });
                        t.mid_task = true;
                    }
                    Some("text") => {
                        let said = b.get("text").and_then(Value::as_str).unwrap_or("");
                        if let Some(l) = said.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
                            t.intent = cut(&crate::redact::redact(l), INTENT_CHARS);
                        }
                        t.mid_task = false;
                    }
                    _ => {}
                }
            }
        } else {
            let mut results = false;
            for b in &blocks {
                if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                    results = true;
                    if let Some(&k) = b.get("tool_use_id").and_then(Value::as_str).and_then(|id| ids.get(id)) {
                        steps[k].result = Some(turn_ref(session, n, &turn_text(&v)));
                    }
                }
            }
            // A person's prompt ends any tool chain.
            if !results && v.get("isMeta").and_then(Value::as_bool) != Some(true) {
                t.mid_task = false;
            }
        }
    }
    t.files.truncate(FILES);
    let k = steps.len().saturating_sub(TOOLS);
    t.steps = steps.split_off(k);
    t
}

/// The window `compact.defer` measures against: the configured one, else
/// 1,000,000 when the session has ever held more than 200,000 tokens, else
/// 200,000.
pub fn window(configured: u64, t: &Tail) -> u64 {
    if configured > 0 {
        configured
    } else if t.max_ctx > 200_000 {
        1_000_000
    } else {
        200_000
    }
}

/// Whether `compact.defer` blocks an automatic compaction now: only while a
/// tool chain is running, and never past the ceiling (the window less three
/// times the largest per-request growth, at least [`DEFER_MIN_MARGIN`]).
/// `Some(reason)` blocks.
pub fn defer(trigger: &str, t: &Tail, window: u64) -> Option<String> {
    if trigger != "auto" || !t.mid_task || t.last_ctx == 0 {
        return None;
    }
    let margin = (3 * t.max_growth).max(DEFER_MIN_MARGIN);
    (t.last_ctx + margin < window).then(|| {
        format!(
            "atlas compact.defer: a tool chain is running; compaction waits for the next quiet point ({} of {} context tokens, ceiling {})",
            t.last_ctx,
            window,
            window - margin
        )
    })
}

/// The store side of a checkpoint, from records written at or after
/// `since` and at or before `until` (empty: no upper bound).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// The previous checkpoint's ref.
    pub since: Option<String>,
    /// Active lanes, newest first: (lane, its current step's ref).
    pub lanes: Vec<(String, Option<String>)>,
    /// Unanswered questions.
    pub open: Vec<String>,
    /// `next` entries no commit followed.
    pub owed: Vec<String>,
    /// Rule records written or revised in the window.
    pub rulings: Vec<String>,
}

fn href(h: &Head) -> String {
    record_ref(&h.key, &h.sha)
}

/// Reads the store side of a checkpoint for `session`: the previous
/// checkpoint, the lanes with an entry since `since` (the previous
/// checkpoint's time when there is one), their open questions, owed `next`
/// entries and current step, and the rulings since. With
/// [`Lanes::Session`] only the previous checkpoint is read. An empty `since`
/// is an unknown start, never all time: it is [`UNKNOWN_START_HOURS`]
/// before `until` (or now).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn state(store: &MemoryStore, session: &str, since: &str, until: &str, lanes: Lanes) -> Result<State> {
    let mut s = State::default();
    let bounded;
    let since = if since.is_empty() {
        bounded = hours_before(until, UNKNOWN_START_HOURS);
        bounded.as_str()
    } else {
        since
    };
    let in_window = |time: &str| time >= since && (until.is_empty() || time <= until);
    let own = format!("session:{session}");
    let prev = store.lane(&own)?.into_iter().rev().find(|h| h.sub == "checkpoint" && (until.is_empty() || h.time.as_str() <= until));
    let since = match &prev {
        Some(p) => {
            s.since = Some(href(p));
            p.time.clone().max(since.to_string())
        }
        None => since.to_string(),
    };
    let in_window = |time: &str| in_window(time) && time >= since.as_str();
    if lanes == Lanes::Session {
        return Ok(s);
    }
    for l in store.lanes()? {
        if l.lane.starts_with("session:") || l.last.as_str() < since.as_str() {
            continue;
        }
        let heads = store.lane(&l.lane)?;
        let heads: Vec<&Head> = heads.iter().filter(|h| until.is_empty() || h.time.as_str() <= until).collect();
        if !heads.iter().any(|h| in_window(&h.time)) {
            continue;
        }
        let step = heads.iter().rev().find(|h| matches!(h.sub.as_str(), "next" | "brief") || h.kind == "handoff").map(|h| href(h));
        if s.lanes.len() < PER_LINE {
            s.lanes.push((l.lane.clone(), step));
        }
        let answered = store.answered(&l.lane)?;
        for h in heads.iter().filter(|h| h.sub == "question" && !answered.contains(&h.key)) {
            if s.open.len() < PER_LINE {
                s.open.push(href(h));
            }
        }
        let last_commit = heads.iter().rposition(|h| h.sub == "commit");
        for h in heads.iter().skip(last_commit.map_or(0, |i| i + 1)).filter(|h| h.sub == "next") {
            if s.owed.len() < PER_LINE {
                s.owed.push(href(h));
            }
        }
    }
    let mut rules: Vec<Head> = store.heads(Some("rule"), None)?.into_iter().filter(|h| h.status == "active" && in_window(&h.time)).collect();
    rules.sort_by(|a, b| b.time.cmp(&a.time));
    s.rulings = rules.iter().take(PER_LINE).map(href).collect();
    Ok(s)
}

/// The checkpoint body: one line per non-empty part, the store's parts
/// first, then the transcript's, within `budget` bytes (whole items are
/// dropped from a line's end, then whole lines from the end).
pub fn body(s: &State, t: &Tail, budget: usize) -> String {
    let mut lines: Vec<(&str, Vec<String>, &str)> = Vec::new();
    if let Some(p) = &s.since {
        lines.push(("since", vec![p.clone()], ", "));
    }
    let lanes: Vec<String> = s.lanes.iter().map(|(l, st)| st.as_ref().map_or_else(|| l.clone(), |r| format!("{l} (step {r})"))).collect();
    lines.push(("lanes", lanes, ", "));
    lines.push(("open", s.open.clone(), ", "));
    lines.push(("owed", s.owed.clone(), ", "));
    lines.push(("rulings", s.rulings.clone(), ", "));
    let steps: Vec<String> = t.steps.iter().map(|x| match &x.result {
        Some(r) => format!("{} {} -> {r}", x.tool, x.target),
        None => format!("{} {}", x.tool, x.target),
    }).collect();
    lines.push(("files", t.files.clone(), ", "));
    lines.push(("tools", steps, "; "));
    if t.mid_task {
        if let Some(p) = t.steps.last().filter(|x| x.result.is_none()) {
            lines.push(("pending", vec![format!("{} {}", p.tool, p.target)], ""));
        }
    }
    if !t.intent.is_empty() {
        lines.push(("intent", vec![t.intent.clone()], ""));
    }
    let mut out = String::new();
    for (name, items, sep) in lines {
        let mut line = String::new();
        for it in items {
            let add = if line.is_empty() { format!("{name}: {it}") } else { format!("{sep}{it}") };
            if out.len() + line.len() + add.len() + 1 > budget {
                break;
            }
            line.push_str(&add);
        }
        if !line.is_empty() {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// The card SessionStart(compact) injects: the checkpoint's own ref and its
/// body.
pub fn card(h: &Head) -> String {
    format!("atlas checkpoint {} (the state before this compaction; recall any key@hash for its exact text)\n{}", href(h), h.body.trim_end())
}

/// The newest checkpoint of a session, if any.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn newest(store: &MemoryStore, session: &str) -> Result<Option<Head>> {
    Ok(store.lane(&format!("session:{session}"))?.into_iter().rev().find(|h| h.sub == "checkpoint"))
}

/// A transcript's per-request context curve (the main chain, one point per
/// distinct assistant message) and where it was compacted: for each
/// compaction, the index of the first request after it and Claude Code's
/// `preTokens`, `postTokens` and trigger.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Curve {
    /// Context tokens per request.
    pub points: Vec<u64>,
    /// (first request after, preTokens, postTokens, trigger).
    pub compactions: Vec<(usize, u64, u64, String)>,
}

/// The tail of the segment before each compaction boundary of a transcript,
/// oldest first, with the boundary's time.
pub fn segments(path: &Path, session: &str) -> Vec<(String, Tail)> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    let (mut seg_start, mut seg_line, mut pos) = (0usize, 1usize, 0usize);
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if is_boundary(line) {
            let time = crate::json::parse(line.trim_end()).ok().and_then(|v| v.get("timestamp").and_then(Value::as_str).map(str::to_string)).unwrap_or_default();
            let t = scan(&text[seg_start..pos], seg_line, session);
            out.push((time, t));
            seg_start = pos + line.len();
            seg_line = i + 2;
        }
        pos += line.len();
    }
    out
}

/// Reads a transcript's context curve. Requests (by message id) and
/// boundaries (by time) already in `seen` are skipped, so a resumed session
/// that copies its parent's history is not counted twice. Fails open (an
/// unreadable file is an empty curve).
pub fn curve(path: &Path, seen: &mut std::collections::HashSet<String>) -> Curve {
    use std::io::BufRead;
    let mut c = Curve::default();
    let Ok(f) = std::fs::File::open(path) else { return c };
    let mut last = String::new();
    for line in std::io::BufReader::new(f).lines().map_while(std::result::Result::ok) {
        if line.contains("\"isSidechain\":true") {
            continue;
        }
        if line.contains("\"compact_boundary\"") {
            let Ok(v) = crate::json::parse(&line) else { continue };
            if v.get("subtype").and_then(Value::as_str) != Some("compact_boundary") {
                continue;
            }
            if !seen.insert(format!("boundary {}", v.get("timestamp").and_then(Value::as_str).unwrap_or(""))) {
                continue;
            }
            let m = v.get("compactMetadata").cloned().unwrap_or(Value::Null);
            let n = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
            c.compactions.push((c.points.len(), n("preTokens"), n("postTokens"), m.get("trigger").and_then(Value::as_str).unwrap_or("").to_string()));
            continue;
        }
        if !line.contains("\"usage\"") || !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        let Ok(v) = crate::json::parse(&line) else { continue };
        let id = v.pointer("/message/id").and_then(Value::as_str).unwrap_or("").to_string();
        if id == last || (!id.is_empty() && seen.contains(&id)) {
            continue;
        }
        seen.insert(id.clone());
        if let Some(x) = usage_ctx(&v).filter(|x| *x > 0) {
            c.points.push(x);
            last = id;
        }
    }
    c
}

/// One arm of the threshold sweep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Arm {
    /// Compact when the context would pass this many tokens.
    pub threshold: u64,
    /// The context right after a compaction, before the summary: system
    /// prompt, tools and memory files.
    pub overhead: u64,
    /// The summary's tokens (and, in the atlas arm, the card's).
    pub summary: u64,
    /// Tool-result tokens read again after each compaction (misses).
    pub reread: u64,
}

/// What an arm costs over a curve.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    /// Compactions.
    pub compactions: usize,
    /// Input tokens: every request's context, every summarizer read and the
    /// re-reads.
    pub raw: f64,
    /// Price-weighted, in input-token units: cache reads 0.1, cache writes
    /// 1.25, the summarizer's read 1.0 and its summary (output) 5.0.
    pub weighted: f64,
}

/// Replays a curve under an arm: each request adds the recorded growth (a
/// recorded compaction's drop is replaced by the growth after it), and
/// whenever the context would pass the threshold it is compacted first, to
/// the overhead plus the summary.
pub fn simulate(c: &Curve, a: &Arm) -> Cost {
    let mut cost = Cost::default();
    let Some(&first) = c.points.first() else { return cost };
    let mut ctx = first as f64;
    cost.raw += ctx;
    cost.weighted += 1.25 * ctx;
    let mut k = 0;
    for i in 1..c.points.len() {
        let mut base = c.points[i - 1] as f64;
        while k < c.compactions.len() && c.compactions[k].0 <= i {
            if c.compactions[k].0 == i && c.compactions[k].2 > 0 {
                base = c.compactions[k].2 as f64;
            }
            k += 1;
        }
        let delta = c.points[i] as f64 - base;
        let mut cold = false;
        if ctx + delta > a.threshold as f64 {
            cost.compactions += 1;
            cost.raw += ctx + a.reread as f64;
            cost.weighted += ctx + 5.0 * a.summary as f64 + 1.25 * a.reread as f64;
            ctx = (a.overhead + a.summary) as f64;
            cold = true;
        }
        let prev = ctx;
        ctx = (ctx + delta).max((a.overhead + a.summary) as f64);
        cost.raw += ctx;
        cost.weighted += if cold { 1.25 * ctx } else { 0.1 * prev.min(ctx) + 1.25 * (ctx - prev).max(0.0) };
    }
    cost
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, id: &str, content: &str, ctx: u64) -> String {
        format!(r#"{{"type":"{kind}","timestamp":"2026-11-01T00:00:00Z","message":{{"id":"{id}","usage":{{"input_tokens":{ctx}}},"content":{content}}}}}"#)
    }

    #[test]
    fn the_tail_reads_steps_files_and_mid_task_since_the_boundary() {
        let lines = [
            rec("assistant", "m0", r#"[{"type":"tool_use","id":"t0","name":"Read","input":{"file_path":"old.rs"}}]"#, 1000),
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto","preTokens":900000}}"#.to_string(),
            rec("user", "", r#"[{"type":"text","text":"fix the bug"}]"#, 0),
            rec("assistant", "m1", r#"[{"type":"thinking","thinking":"SECRET REASONING"},{"type":"text","text":"Looking.\nI will read main.rs next."}]"#, 10000),
            rec("assistant", "m2", r#"[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src\\main.rs"}}]"#, 12000),
            rec("user", "", r#"[{"type":"tool_result","tool_use_id":"t1","content":"fn main() {}"}]"#, 0),
            rec("assistant", "m3", r#"[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/main.rs"}}]"#, 20000),
            rec("user", "", r#"[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]"#, 0),
            rec("assistant", "m4", r#"[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"export API_TOKEN=abcdef0123456789xyz"}}]"#, 21000),
        ];
        let t = scan(&lines[2..].join("\n"), 3, "abcdef0123");
        assert_eq!(t.files, ["src/main.rs"]);
        assert_eq!(t.steps.len(), 3);
        assert_eq!(t.steps[0].target, "src/main.rs");
        assert_eq!(t.steps[0].result.as_deref(), Some(turn_ref("abcdef0123", 6, "fn main() {}").as_str()));
        assert!(t.steps[0].result.as_deref().unwrap().starts_with("turn:abcdef01:6@"));
        assert_eq!(t.steps[2].target, "[redacted]");
        assert!(t.mid_task);
        assert_eq!(t.intent, "I will read main.rs next.");
        assert_eq!((t.last_ctx, t.max_growth), (21000, 8000));
        let all = format!("{t:?}");
        assert!(!all.contains("SECRET REASONING"), "thinking must never be read");
        // From a file: the segment after the boundary only.
        let dir = std::env::temp_dir().join(format!("atlas-ckpt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let f = tail(&p, "abcdef0123");
        assert_eq!(f.steps, t.steps);
        assert_eq!(f.max_ctx, 21000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding 8: the newest `"compact_boundary"` literal started the
    /// segment, whatever record held it: a Grep for the word, or a
    /// subagent's own boundary.
    #[test]
    fn the_tail_starts_at_the_newest_main_chain_boundary_only() {
        let lines = [
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto","preTokens":900000}}"#.to_string(),
            rec("assistant", "m1", r#"[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/a.rs"}}]"#, 10000),
            rec("user", "", r#"[{"type":"tool_result","tool_use_id":"t1","content":"a"}]"#, 0),
            rec("assistant", "m2", r#"[{"type":"tool_use","id":"t2","name":"Grep","input":{"pattern":"compact_boundary"}}]"#, 11000),
            rec("user", "", r#"[{"type":"tool_result","tool_use_id":"t2","content":"none"}]"#, 0),
            r#"{"type":"system","subtype":"compact_boundary","isSidechain":true,"compactMetadata":{"trigger":"auto","preTokens":100}}"#.to_string(),
            rec("assistant", "m3", r#"[{"type":"tool_use","id":"t3","name":"Read","input":{"file_path":"src/b.rs"}}]"#, 12000),
            rec("user", "", r#"[{"type":"tool_result","tool_use_id":"t3","content":"b"}]"#, 0),
        ];
        let dir = std::env::temp_dir().join(format!("atlas-ckpt-boundary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let f = tail(&p, "abcdef0123");
        let _ = std::fs::remove_dir_all(&dir);
        let ids: Vec<&str> = f.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["t1", "t2", "t3"], "{f:?}");
        assert_eq!(f.steps[0].result.as_deref(), Some(turn_ref("abcdef0123", 3, "a").as_str()));
    }

    #[test]
    fn defer_blocks_only_auto_mid_task_below_the_ceiling() {
        let t = Tail { mid_task: true, last_ctx: 500_000, max_growth: 20_000, ..Tail::default() };
        assert!(defer("auto", &t, 1_000_000).is_some());
        assert!(defer("manual", &t, 1_000_000).is_none());
        assert!(defer("auto", &Tail { mid_task: false, ..t.clone() }, 1_000_000).is_none());
        // Past the ceiling: 1M less 3 x 20k is 940k.
        assert!(defer("auto", &Tail { last_ctx: 945_000, ..t.clone() }, 1_000_000).is_none());
        // A 200k window never defers near its own threshold.
        assert!(defer("auto", &Tail { last_ctx: 167_000, ..t.clone() }, 200_000).is_none());
        assert_eq!(window(0, &Tail { max_ctx: 300_000, ..t.clone() }), 1_000_000);
    }

    #[test]
    fn the_body_keeps_to_its_budget() {
        let s = State { lanes: vec![("build".into(), Some("journal:build:0002@0123456789ab".into()))], rulings: vec!["rules:r1@0123456789ab".into()], ..State::default() };
        let t = Tail { files: (0..20).map(|i| format!("src/file{i}.rs")).collect(), intent: "next: run tests".into(), ..Tail::default() };
        let b = body(&s, &t, 10_000);
        assert!(b.starts_with("lanes: build (step journal:build:0002@0123456789ab)\nrulings: rules:r1@0123456789ab\nfiles: src/file0.rs"), "{b}");
        assert!(b.ends_with("intent: next: run tests\n"));
        let small = body(&s, &t, 120);
        assert!(small.len() <= 120, "{small}");
        assert!(small.contains("files: src/file0.rs") && !small.contains("intent"), "{small}");
    }

    #[test]
    fn the_sweep_compacts_at_the_threshold() {
        let c = Curve { points: (1..=10).map(|i| i * 100_000).collect(), compactions: Vec::new() };
        let a = Arm { threshold: 350_000, overhead: 10_000, summary: 5_000, reread: 0 };
        let r = simulate(&c, &a);
        assert_eq!(r.compactions, 3);
        let none = simulate(&c, &Arm { threshold: u64::MAX, ..a });
        assert_eq!(none.compactions, 0);
        assert!(r.raw < none.raw, "{r:?} {none:?}");
        // A recorded compaction's drop is replaced by the growth after it.
        let rc = Curve { points: vec![100_000, 200_000, 20_000, 120_000], compactions: vec![(2, 200_000, 15_000, "manual".into())] };
        let nr = simulate(&rc, &Arm { threshold: u64::MAX, ..a });
        assert_eq!(nr.raw, 100_000.0 + 200_000.0 + 205_000.0 + 305_000.0);
    }
}
