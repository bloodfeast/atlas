//! The Claude Code hooks: relevance-gated recall and the context cap.
//!
//! All hook logic is here, behind [`Hooks::handle`], which takes a
//! [`HookEvent`] and returns a [`HookAnswer`]. Transports are thin adapters
//! around it: the `hook` MCP tool (Claude Code `mcp_tool` hooks, answered on
//! the server's hook worker thread), and `atlas --hook` (a command hook
//! reading the event JSON on stdin). A pipe would be one more adapter.
//!
//! What the hooks can see was measured (`docs/design/atlas-c.md`, section 1):
//! inside a subagent `transcript_path` is the parent's; the subagent's own
//! transcript is `<parent without .jsonl>/subagents/agent-<agent_id>.jsonl`;
//! a subagent's brief never passes through `UserPromptSubmit`; the context
//! size is the last assistant message's input, cache and output tokens.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use crate::error::Result;
use crate::json::Value;
use crate::embed::Model;
use crate::memory::recall;
use crate::memory::vectors::{self, RecVecs};
use crate::memory::settings::{Settings, SubagentContext};
use crate::memory::{Draft, MemoryStore};

/// Runs hook work `f` for `ev` under a panic boundary (#253): a bug in
/// hook code must not take the server down. Returns `f`'s value and
/// `false`, or on a panic the default value (for an answer, `""`: the call
/// passes through untouched) and `true`, which tells the caller to drop its
/// [`Hooks`]: its state after the unwind is not trusted. A panic is recorded
/// as one miss, `hook panic <event> <tool>`, through a fresh router; its
/// reason is the message only when the payload is a `&'static str` (as the
/// serve log's panic line keeps it).
pub fn guard<T: Default>(settings: &Settings, ev: &HookEvent, f: impl FnOnce() -> T) -> (T, bool) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => (v, false),
        Err(payload) => {
            let reason = payload.downcast_ref::<&'static str>().copied().unwrap_or("payload omitted");
            let query = format!("hook panic {} {}", ev.event, ev.tool_name);
            crate::hook_rewrite::Router::new(settings).miss_reason(query.trim_end(), reason);
            (T::default(), true)
        }
    }
}

/// A tool name whose hook panics in [`Hooks::handle`] (tests only).
#[cfg(test)]
pub const TEST_PANIC_TOOL: &str = "atlas_test_panic";

/// One hook event, from either transport.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookEvent {
    /// `UserPromptSubmit`, `SubagentStart`, `PreToolUse`, `PreCompact`, ...
    pub event: String,
    /// The session.
    pub session_id: String,
    /// The subagent, empty in the main session.
    pub agent_id: String,
    /// The subagent's type.
    pub agent_type: String,
    /// The transcript the hook input names (the parent's, in a subagent).
    pub transcript_path: String,
    /// The prompt (UserPromptSubmit).
    pub prompt: String,
    /// The tool (PreToolUse).
    pub tool_name: String,
    /// The tool call's id (PreToolUse, PostToolUse).
    pub tool_use_id: String,
    /// The tool input's `prompt` (the brief, when the tool spawns an agent).
    pub tool_prompt: String,
    /// What started a compaction.
    pub trigger: String,
    /// Why the session ends (SessionEnd: clear, resume, logout,
    /// prompt_input_exit, other).
    pub reason: String,
    /// What started the session (SessionStart: startup, resume, clear,
    /// compact).
    pub source: String,
    /// The session's working directory.
    pub cwd: String,
    /// Claude Code's permission mode (`default`, `plan`, `acceptEdits`,
    /// `auto`, `dontAsk`, `bypassPermissions`), empty when the input lacks it.
    pub permission_mode: String,
    /// The whole tool input (PreToolUse, PostToolUse), `Null` when absent.
    pub tool_input: Value,
    /// The whole tool result (PostToolUse), `Null` when absent.
    pub tool_response: Value,
    /// The agent's last message (SubagentStop `last_assistant_message`).
    pub last_message: String,
    /// Whether a stop hook already blocked this stop (SubagentStop).
    pub stop_hook_active: bool,
    /// Stop's and SubagentStop's `background_tasks` (Claude Code 2.1.325
    /// builds both inputs with it), `Null` when the input lacks it.
    pub background_tasks: Value,
    /// Stop's `session_crons`, `Null` when the input lacks it.
    pub session_crons: Value,
    /// The call is a model's tool call of the `hook` tool, not a hook:
    /// Claude Code names a tool call's `toolUseId` in the request's `_meta`,
    /// which a model cannot write. Set by the MCP transport (the reserved
    /// argument [`TOOL_USE_ARG`], which it always overwrites); false for a
    /// command hook.
    pub tool_use: bool,
}

/// The `hook` call argument the MCP transport sets to whether the request's
/// `_meta` named a tool call (see [`HookEvent::tool_use`]), overwriting any
/// value the caller sent.
pub const TOOL_USE_ARG: &str = "_tool_use";

/// Marks a `hook` call's arguments with whether its request's `_meta` names
/// a tool call (`use_id`). Whatever the caller put under [`TOOL_USE_ARG`]
/// is replaced.
pub fn mark_tool_use(args: &Value, use_id: Option<&str>) -> Value {
    let mut m = args.as_object().cloned().unwrap_or_default();
    m.insert(TOOL_USE_ARG.into(), Value::Bool(use_id.is_some_and(|u| !u.is_empty())));
    Value::Object(m)
}

/// An array field that arrives as an array (a command hook) or as its JSON text
/// (an `mcp_tool` hook). Anything else is `Null`.
fn array_field(v: &Value, k: &str) -> Value {
    match v.get(k) {
        Some(a @ Value::Array(_)) => a.clone(),
        Some(Value::String(t)) if t.starts_with('[') => crate::json::parse(t).ok().filter(|p| p.as_array().is_some()).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// An object field that arrives either as an object (a command hook's
/// input) or as its JSON text (an `mcp_tool` hook substitutes `${path}` of
/// an object as JSON text, measured 2026-10-31). Anything else is `Null`.
fn object_field(v: &Value, k: &str) -> Value {
    match v.get(k) {
        Some(o @ Value::Object(_)) => o.clone(),
        Some(Value::String(t)) if t.starts_with('{') => crate::json::parse(t).ok().filter(|p| p.as_object().is_some()).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

impl HookEvent {
    /// Reads an event from either an MCP `hook` call's arguments (flat
    /// fields) or Claude Code's hook input JSON (`hook_event_name`,
    /// `tool_input.prompt`).
    pub fn from_value(v: &Value) -> HookEvent {
        // A `${path}` the hook input lacked may arrive unsubstituted; it means empty.
        let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|x| !(x.starts_with("${") && x.ends_with('}'))).unwrap_or("").to_string();
        let event = match s("event") {
            e if !e.is_empty() => e,
            _ => s("hook_event_name"),
        };
        let tool_prompt = match s("tool_prompt") {
            p if !p.is_empty() => p,
            _ => v.pointer("/tool_input/prompt").and_then(Value::as_str).filter(|x| !x.starts_with("${")).unwrap_or("").to_string(),
        };
        HookEvent {
            event,
            session_id: s("session_id"),
            agent_id: s("agent_id"),
            agent_type: s("agent_type"),
            transcript_path: s("transcript_path"),
            prompt: s("prompt"),
            tool_name: s("tool_name"),
            tool_use_id: s("tool_use_id"),
            tool_prompt,
            trigger: s("trigger"),
            reason: s("reason"),
            source: s("source"),
            permission_mode: s("permission_mode"),
            cwd: s("cwd"),
            tool_input: object_field(v, "tool_input"),
            tool_response: object_field(v, "tool_response"),
            last_message: s("last_assistant_message"),
            background_tasks: array_field(v, "background_tasks"),
            session_crons: array_field(v, "session_crons"),
            stop_hook_active: match v.get("stop_hook_active") {
                Some(Value::Bool(b)) => *b,
                Some(Value::String(t)) => t == "true",
                _ => false,
            },
            tool_use: matches!(v.get(TOOL_USE_ARG), Some(Value::Bool(true))),
        }
    }
}

/// Whether `ev` may carry the running server's reconnect notice: a
/// UserPromptSubmit hook, never a model's tool call (see
/// [`HookEvent::tool_use`]).
pub fn takes_live_notice(ev: &HookEvent) -> bool {
    ev.event == "UserPromptSubmit" && !ev.tool_use
}

/// What a hook tells Claude Code.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookAnswer {
    /// Text added to the model's context.
    pub context: String,
    /// Deny the tool call, with this reason (PreToolUse only).
    pub deny: Option<String>,
    /// The replacement tool input (PreToolUse `updatedInput`).
    pub updated_input: Option<Value>,
    /// The replacement tool result (PostToolUse `updatedToolOutput`), in
    /// the tool's own output shape.
    pub updated_output: Option<Value>,
    /// Block the event, with this reason (PreCompact: `decision: block`).
    pub block: Option<String>,
    /// Instructions for the compaction summary (PreCompact). Claude Code adds
    /// a PreCompact hook's plain reply to the summarizer's instructions.
    pub instructions: &'static str,
    /// A line shown to the user and never added to the model's context
    /// (top-level `systemMessage`): the update notice (issue #190).
    pub system_message: String,
}

impl HookAnswer {
    /// The reply Claude Code acts on: the hook JSON, a PreCompact's plain
    /// instructions, or an empty string for "nothing" (outside PreCompact plain
    /// text is not injected, so empty is a no-op).
    pub fn to_json(&self, event: &str) -> String {
        if let Some(r) = &self.block {
            return crate::json::object(r.len() + 48, |w| {
                w.field("decision", "block").field("reason", r);
            });
        }
        if !self.instructions.is_empty() {
            return self.instructions.to_string();
        }
        let specific = !(self.deny.is_none() && self.context.is_empty() && self.updated_input.is_none() && self.updated_output.is_none());
        if !specific && self.system_message.is_empty() {
            return String::new();
        }
        crate::json::object(self.context.len() + self.system_message.len() + 128, |w| {
            if !self.system_message.is_empty() {
                w.field("systemMessage", &self.system_message);
            }
            if !specific {
                return;
            }
            let inner = crate::json::object(self.context.len() + 96, |h| {
                h.field("hookEventName", event);
                if let Some(r) = &self.deny {
                    h.field("permissionDecision", "deny").field("permissionDecisionReason", r);
                }
                if !self.context.is_empty() {
                    h.field("additionalContext", &self.context);
                }
                if let Some(u) = &self.updated_input {
                    h.field("updatedInput", u);
                }
                if let Some(u) = &self.updated_output {
                    h.field("updatedToolOutput", u);
                }
            });
            w.field("hookSpecificOutput", &crate::json::parse(&inner).unwrap_or(Value::Null));
        })
    }
}

/// The line an admitted builder launch with no `Lane:` line gets in
/// orchestrated mode (the lane declaration the orchestrate skill teaches).
const NO_LANE: &str = "atlas lanes: this builder's prompt declares no lane, so the queue cannot keep its work apart from other work. Declare it: `Kind:`, `Lane:`, `Priority:` and `Uses:` lines in the launch prompt's first 40 lines, and a `brief` journal entry in that lane with the same lines before its first launch.";

/// The hook events whose `additionalContext` reaches the session's own model
/// (SubagentStart's goes to the subagent; PreCompact's plain text is summary
/// instructions).
const NOTICE_EVENTS: [&str; 3] = ["SessionStart", "UserPromptSubmit", "PostToolUse"];

/// The hook answer while atlas refuses a store (an [`Error::StoreTooNew`] from
/// opening it): once per session, on its first event that can carry context,
/// the refusal as a `systemMessage` the user sees and as `additionalContext`
/// the model sees; every other event answers nothing, as a hook with no store
/// did before.
#[derive(Debug, Default)]
pub struct RefusalNotice {
    told: HashSet<String>,
}

impl RefusalNotice {
    /// The answer to `ev` while opening fails with `e`.
    pub fn answer(&mut self, e: &crate::Error, ev: &HookEvent) -> String {
        if !e.is_store_too_new() || !NOTICE_EVENTS.contains(&ev.event.as_str()) || !self.told.insert(ev.session_id.clone()) {
            return String::new();
        }
        refusal_json(e, &ev.event)
    }
}

/// The hook JSON that tells the user and the model atlas is off for this
/// session because of `e`.
pub fn refusal_json(e: &crate::Error, event: &str) -> String {
    let text = format!("atlas is off for this session: {e}");
    crate::json::object(text.len() * 2 + 96, |w| {
        w.field("systemMessage", &text);
        let inner = crate::json::object(text.len() + 64, |h| {
            h.field("hookEventName", event).field("additionalContext", &text);
        });
        w.field("hookSpecificOutput", &crate::json::parse(&inner).unwrap_or(Value::Null));
    })
}

/// The subagent's own transcript, derived from the parent's path and the
/// agent id (measured: Claude Code 2.1.314).
pub fn subagent_transcript(parent: &str, agent_id: &str) -> PathBuf {
    let base = parent.strip_suffix(".jsonl").unwrap_or(parent);
    let id = agent_id.strip_prefix("agent-").unwrap_or(agent_id);
    Path::new(base).join("subagents").join(format!("agent-{id}.jsonl"))
}

/// The last assistant message's usage in a transcript: its message id and
/// `input + cache_creation + cache_read + output` tokens (equal to Claude
/// Code's own context figure). Read backwards from the end, in 64 KiB steps,
/// at most 16 MiB.
///
/// # Errors
///
/// I/O errors opening or reading the file.
pub fn last_usage(path: &Path) -> std::io::Result<Option<(String, u64)>> {
    last_assistant(path, b"\"usage\"", |m| {
        let u = m.get("usage")?;
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let ctx = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens") + n("output_tokens");
        Some((m.get("id").and_then(Value::as_str).unwrap_or("").to_string(), ctx))
    })
}

/// The last assistant message's model in a transcript (`message.model`),
/// skipping Claude Code's `<synthetic>` messages. Read as [`last_usage`].
///
/// # Errors
///
/// I/O errors opening or reading the file.
pub fn last_model(path: &Path) -> std::io::Result<Option<String>> {
    last_assistant(path, b"\"model\"", |m| m.get("model").and_then(Value::as_str).filter(|s| !s.is_empty() && !s.starts_with('<')).map(str::to_string))
}

/// The newest assistant line holding `needle` whose `message` gives `f` a
/// value, read backwards from the end in 64 KiB steps, at most 16 MiB.
fn last_assistant<T>(path: &Path, needle: &[u8], f: impl Fn(&Value) -> Option<T>) -> std::io::Result<Option<T>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut end = len;
    let mut carry: Vec<u8> = Vec::new();
    let step = 64 * 1024u64;
    while end > 0 && len - end < 16 * 1024 * 1024 {
        let start = end.saturating_sub(step);
        let mut buf = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf)?;
        buf.extend_from_slice(&carry);
        // Whole lines only: the first partial line is carried to the next step.
        let first_nl = if start == 0 { None } else { buf.iter().position(|b| *b == b'\n') };
        let (head, body) = match first_nl {
            Some(i) => (buf[..i].to_vec(), &buf[i + 1..]),
            None => (Vec::new(), &buf[..]),
        };
        for line in body.rsplit(|b| *b == b'\n') {
            if line.len() < 16 || !contains(line, needle) || !contains(line, b"\"assistant\"") {
                continue;
            }
            let Ok(v) = crate::json::parse_bytes(line) else { continue };
            if let Some(t) = v.get("message").and_then(&f) {
                return Ok(Some(t));
            }
        }
        carry = head;
        end = start;
    }
    Ok(None)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The first user message of a transcript (a subagent's brief), at most
/// 64 KiB of text.
pub fn first_user_text(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines().take(200) {
        if !line.contains("\"user\"") {
            continue;
        }
        let v = crate::json::parse(line).ok()?;
        if v.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let c = v.pointer("/message/content")?;
        let s = match c {
            Value::String(s) => s.clone(),
            Value::Array(a) => a.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
            _ => continue,
        };
        return Some(s.chars().take(65_536).collect());
    }
    None
}

/// One subagent's run, as the hooks saw it.
#[derive(Debug, Clone, Default)]
struct AgentRun {
    /// The lane its brief names (see [`brief_lane`]).
    lane: Option<String>,
    /// When it started (UTC, the store's time format).
    start: String,
    /// The lanes it wrote journal entries or handoffs under, in order.
    wrote: Vec<String>,
    /// The key of its `launch` journal entry, once filed (#220), for the
    /// restart point (#221).
    launch: Option<String>,
    /// The resources its brief declares (`Uses:`), for the restart point.
    uses: Vec<String>,
}

/// The journal entry kind atlas files an agent's launch brief as (#220). It
/// is atlas's own: the `journal` tool refuses it (it is not in
/// [`crate::memory::ENTRIES`]).
pub const LAUNCH_ENTRY: &str = "launch";

/// What [`Hooks::file_launch`] files for one agent's start.
struct LaunchEntry<'a> {
    agent: &'a str,
    agent_type: &'a str,
    model: &'a str,
    description: &'a str,
    brief: &'a str,
    /// The session and its transcript, so a later session can tell which
    /// session's agents did not finish (#221's crash case).
    session: &'a str,
    transcript: &'a str,
}

/// An Agent launch the hook let through, kept for its agent's start or
/// first tool call.
#[derive(Debug, Clone)]
struct LaunchBrief {
    /// The prompt as sent.
    brief: String,
    /// The launch's tool use id; empty when the hook input carried none.
    tool_use_id: String,
    /// Its `subagent_type` (`general-purpose` when the input names none),
    /// matched against a start's `agent_type`.
    agent_type: String,
    /// When it was let through: one whose agent is never confirmed and whose
    /// refusal is never seen is dropped after
    /// [`crate::orchestrate::LAUNCH_HOLD_TTL`].
    at: std::time::Instant,
    /// Its Binding block, compiled when it was let through
    /// (`recall.subagent_context = "binding"` and a declared lane only).
    binding: Option<String>,
    /// The agent holding it: one whose start was given it before its meta
    /// file could say (checked at that agent's first tool call), or one
    /// whose meta file has named it since. A held launch is not waiting.
    holder: Option<String>,
    /// The launch's `model` (empty when it names none) and `description`,
    /// for the `launch` journal entry its agent's start files (#220).
    model: String,
    description: String,
    /// How far into the session transcript a refusal of it can be: its
    /// length when the launch was let through, moved on past each scan
    /// that found none.
    offset: u64,
}

/// A subagent whose launch its start could not confirm: Claude Code writes
/// the meta file (its `toolUseId`) only after SubagentStart answers
/// (journal:ATLAS-HOOK-VALUE:0006). Its first tool call checks it.
#[derive(Debug, Clone, Default)]
struct Unsettled {
    session: String,
    /// The tool use id of the launch its start was given; `None` when the
    /// start was ambiguous (or had nothing) and gave no launch.
    given: Option<String>,
    /// Whether the start's answer said anything, and named a sub-lane.
    told: bool,
    told_sub: bool,
    /// Its launch, once its meta file named it before its first tool call.
    confirmed: Option<String>,
    /// Its tool calls so far that came before its meta file
    /// ([`FIRST_CALL_TRIES`]).
    tries: u32,
}

/// The Binding compiler a server sets ([`crate::binding::compile`], BO-1):
/// the block's text, or nothing when the lane has nothing to bind.
pub fn binding_compiler() -> BindingFn {
    Box::new(|store, brief, lane| Ok(crate::binding::compile(store, brief, lane)?.map(|b| b.text).unwrap_or_default()))
}

/// The lane a brief's Binding block is compiled for: its `Lane:` line, else
/// the one exact upper-case id it names right after the word "lane". A
/// lower-case lane or the word "session" the brief only mentions is no lane.
pub fn binding_lane(brief: &str) -> Option<String> {
    crate::lanes::declared(brief).lane.or_else(|| brief_lane(brief, &[]))
}

/// `text` cut to the whole lines that fit in `max` characters (a line
/// longer than `max` alone is cut at a character boundary).
pub fn cap_lines(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut n = 0;
    for line in text.split_inclusive('\n') {
        let c = line.chars().count();
        if n + c > max {
            if out.is_empty() {
                out.extend(line.chars().take(max));
            }
            break;
        }
        out.push_str(line);
        n += c;
    }
    out
}

/// The records a Binding block names, one per line that carries a
/// `key@hash` ref (its first), for the token log.
fn binding_keys(block: &str) -> Vec<Injected> {
    block
        .split_inclusive('\n')
        .filter_map(|line| {
            let r = crate::compact::refs_in(line).into_iter().next()?;
            let key = r.text.rsplit_once('@').map_or(r.text.as_str(), |(k, _)| k).to_string();
            Some(Injected { key, fts: None, cos: None, bytes: line.len(), how: Sel::Whole, full: line.len() })
        })
        .collect()
}

/// A tool that hands a subagent's report back to its caller.
fn is_handback_tool(name: &str) -> bool {
    name == "SubagentHandback" || name.ends_with("__SubagentHandback")
}

/// The atlas tools that write to a lane journal.
fn is_journal_tool(name: &str) -> bool {
    is_handoff_tool(name) || name == "journal" || name.ends_with("__journal")
}

/// Whether `text` names `lane`, so that its view may be injected: the
/// text's `Lane:` line (`declared`, see [`crate::lanes::declared`])
/// declares it, or the text holds the lane's id itself. An id is a lane
/// name of two or more characters with an upper-case letter and no
/// lower-case one, matched exactly (case-sensitive) as a whole token, so a
/// longer id that starts with it does not match. A lane with lower-case
/// letters (`session`, `build-7`) is named only by a `Lane:` line: the word
/// "session" in a brief is not the `session` lane.
fn names_lane(text: &str, declared: Option<&str>, lane: &str) -> bool {
    if declared.is_some_and(|d| d.eq_ignore_ascii_case(lane)) {
        return true;
    }
    let id = lane.len() >= 2 && lane.bytes().any(|b| b.is_ascii_uppercase()) && !lane.bytes().any(|b| b.is_ascii_lowercase());
    id && has_token(text, lane)
}

/// Whether `word` occurs in `hay` as a whole token: no letter, digit, `-`
/// or `_` right before or after it. A match followed by `/` and a digit is
/// its sub-lane (`X/2`), which names that sub-lane, not `word`: the parent's
/// view already holds its sub-lanes' records, so naming both would inject
/// them twice.
fn has_token(hay: &str, word: &str) -> bool {
    let glued = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    let b = hay.as_bytes();
    hay.match_indices(word).any(|(s, w)| {
        let e = s + w.len();
        let sub = b.get(e) == Some(&b'/') && b.get(e + 1).is_some_and(u8::is_ascii_digit);
        (s == 0 || !glued(b[s - 1])) && (e >= b.len() || !glued(b[e])) && !sub
    })
}

/// The reports in a prompt's `<agent-message` frames, as Claude Code frames
/// them (read from real transcripts 2026-11-11): the opening tag at the
/// start of a line (after "Another Claude session sent a message:"), the
/// report indented, the closing tag at the start of its own line, and a note
/// after it. Each report is the text inside the frame after "The report
/// follows:" (when the frame says it), so the frame's own words ("session",
/// "subagent") name no lane. `None` when the prompt has no such frame: a
/// prompt that only mentions the tag is an ordinary prompt.
fn agent_reports(prompt: &str) -> Option<Vec<&str>> {
    const OPEN: &str = "<agent-message";
    const CLOSE: &str = "\n</agent-message>";
    let mut out = Vec::new();
    let mut rest = prompt;
    while let Some(i) = rest.find(OPEN) {
        let line_start = i == 0 || rest[..i].ends_with('\n');
        let after = &rest[i + OPEN.len()..];
        let Some(close) = after.find(CLOSE) else { break };
        if line_start {
            let inner = &after[..close];
            let inner = inner.split_once('>').map_or("", |(_, r)| r);
            out.push(inner.split_once("The report follows:").map_or(inner, |(_, r)| r));
        }
        rest = &after[close + CLOSE.len()..];
    }
    (!out.is_empty()).then_some(out)
}

/// Whether a `<task-notification>` is an interim stop: the agent ended its
/// turn waiting on its own background work and will notify again (Claude
/// Code says so in the summary and the note, read from transcripts
/// 2026-11-03). A real finish says "finished" and "no live background
/// children".
pub fn is_interim(notification: &str) -> bool {
    notification.contains("is waiting on its own background work") || notification.contains("the result below may be interim")
}

/// The lane a brief names for its agent, or `None` when it names none or
/// several. In order: the one lane the brief assigns ("Journal lane: X",
/// "journal under lane X", "You are LANE X", "your lane X"); else the one
/// lane named right after the word "lane"; else the one known lane the
/// brief mentions. A lane named after "lane" may be new (no entries yet)
/// when it reads as an id: two or more characters, no lowercase letters.
pub fn brief_lane(brief: &str, known: &[String]) -> Option<String> {
    brief_lane_lower(brief, &brief.to_ascii_lowercase(), known)
}

/// [`brief_lane`] given the brief's ASCII-lowercased copy (`lower`), for a
/// caller that already made one.
pub fn brief_lane_lower(brief: &str, lower: &str, known: &[String]) -> Option<String> {
    const SEP: [char; 7] = [' ', '\t', ':', '`', '=', '*', '"'];
    let b = lower.as_bytes();
    let mut strong: Vec<String> = Vec::new();
    let mut named: Vec<String> = Vec::new();
    let push = |v: &mut Vec<String>, l: &str| {
        if !v.iter().any(|x| x == l) {
            v.push(l.to_string());
        }
    };
    let mut from = 0;
    while let Some(i) = lower[from..].find("lane") {
        let s = from + i;
        let e = s + 4;
        from = e;
        let glued_before = s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'-' || b[s - 1] == b'_');
        let glued_after = e < b.len() && !SEP.contains(&(b[e] as char));
        if glued_before || glued_after {
            continue;
        }
        // The token after the separators, in the brief's own case.
        let rest = brief[e..].trim_start_matches(SEP);
        let tok: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        let tok = tok.trim_end_matches(['-', '_']);
        // A sub-lane the launch check assigned (`NAME/2`) is the lane.
        let sub: String =
            rest[tok.len()..].strip_prefix('/').map(|r| r.chars().take_while(char::is_ascii_digit).collect()).unwrap_or_default();
        let full = format!("{tok}/{sub}");
        let tok = if sub.is_empty() || tok.is_empty() { tok } else { full.as_str() };
        let is_known = known.iter().any(|k| k == tok);
        let id_like = tok.len() >= 2 && tok.chars().any(|c| c.is_ascii_uppercase()) && !tok.chars().any(|c| c.is_ascii_lowercase());
        if !(is_known || id_like) {
            continue;
        }
        push(&mut named, tok);
        let before = lower[..s].trim_end();
        if ["journal", "under", "you are", "your", "own"].iter().any(|w| before.ends_with(w)) {
            push(&mut strong, tok);
        }
    }
    let one = |v: Vec<String>| if v.len() == 1 { v.into_iter().next() } else { None };
    if !strong.is_empty() {
        return one(strong);
    }
    if !named.is_empty() {
        return one(named);
    }
    one(known.iter().filter(|l| crate::memory::contains_word(brief, l)).cloned().collect())
}

/// The lanes a subagent's own transcript shows it writing under: the
/// `lane` of each atlas `journal` or `handoff` call, in order.
fn written_lanes(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if !line.contains("\"tool_use\"") || !(line.contains("journal\"") || line.contains("handoff\"")) {
            continue;
        }
        let Ok(v) = crate::json::parse(line) else { continue };
        let Some(Value::Array(items)) = v.pointer("/message/content") else { continue };
        for it in items {
            if it.get("type").and_then(Value::as_str) != Some("tool_use") || !it.get("name").and_then(Value::as_str).is_some_and(is_journal_tool) {
                continue;
            }
            if let Some(l) = it.pointer("/input/lane").and_then(Value::as_str).map(str::trim).filter(|l| !l.is_empty()) {
                if !out.iter().any(|x| x == l) {
                    out.push(l.to_string());
                }
            }
        }
    }
    out
}

/// The first `timestamp` in a transcript (UTC, the store's time format).
fn first_timestamp(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().take(50).find_map(|l| crate::json::parse(l).ok()?.get("timestamp").and_then(Value::as_str).map(str::to_string))
}

/// The tool that may still run past the context cap.
pub fn is_handoff_tool(name: &str) -> bool {
    name == "handoff" || name.ends_with("__handoff")
}

/// The tools that may still run past the context cap: the handoff tool, and
/// Claude Code's `ToolSearch`, which only loads tool schemas. MCP tools are
/// deferred there, so without it an agent at the cap cannot load `handoff`
/// (seen live: the deny reached `ToolSearch` before the handoff call).
pub fn passes_cap(name: &str) -> bool {
    is_handoff_tool(name) || name == "ToolSearch"
}

/// The first line of a recall injection.
pub const POINTER_HEAD: &str = "atlas recall (related records; read one with recall key=KEY):
";

/// The longest title a pointer carries, in characters.
const POINTER_TITLE: usize = 90;

/// One recalled record as a pointer line: its key and a one-line title
/// (about 30 tokens). A memory title's markdown link becomes its text; a
/// record without a title uses its body's first line.
pub fn pointer(h: &crate::memory::Head) -> String {
    format!("- {}: {}
", h.key, pointer_title(h))
}

/// The pointer line of a record collapsed into a near-duplicate sent
/// before it: its key, and the key whose text it repeats.
pub fn collapsed(key: &str, same_as: &str) -> String {
    format!("- {key}: near-duplicate of {same_as}\n")
}

/// A record's one-line title as a pointer carries it (see [`pointer`]).
pub fn pointer_title(h: &crate::memory::Head) -> String {
    let mut t = h.title.trim().trim_start_matches("- ").trim().to_string();
    if t.is_empty() {
        t = h.body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").trim_start_matches("- ").to_string();
    }
    // `[Text](file.md) - description` reads as `Text - description`.
    if let (Some(0), Some(close)) = (t.find('['), t.find("](")) {
        if let Some(end) = t[close..].find(')') {
            t = format!("{}{}", &t[1..close], &t[close + end + 1..]);
        }
    }
    // A pointer is one line: a title's line breaks must not start another.
    if t.contains(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}']) {
        t = t.replace(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'], " ");
    }
    if t.chars().count() > POINTER_TITLE {
        t = t.chars().take(POINTER_TITLE - 3).collect::<String>() + "...";
    }
    t.trim().to_string()
}

/// One record (or lane) a hook injected, with the scores it passed the
/// gate on.
#[derive(Debug, Clone, PartialEq)]
pub struct Injected {
    /// The record key, or `lane:NAME`.
    pub key: String,
    /// The negated bm25 (none for a lane).
    pub fts: Option<f64>,
    /// The cosine with the prompt (none for a lane or a record without a
    /// vector, or without a model).
    pub cos: Option<f32>,
    /// Bytes of the injected text for it (the pointer line, or the lane).
    pub bytes: usize,
    /// What context selection did to it (see [`crate::memory::select`]).
    pub how: Sel,
    /// Bytes it would have taken whole (the untrimmed pointer line);
    /// `bytes` when sent whole.
    pub full: usize,
}

/// What context selection did to one injected record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sel {
    /// Sent as it was (selection off, nothing to trim, or not a pointer).
    Whole,
    /// Its pointer carries the matching clause (an excerpt).
    Trimmed,
    /// Sent as a ref to the near-duplicate injected before it.
    Collapsed,
}

impl Sel {
    /// The name logged in `inject_select`.
    pub fn name(self) -> &'static str {
        match self {
            Sel::Whole => "whole",
            Sel::Trimmed => "trim",
            Sel::Collapsed => "collapse",
        }
    }
}

/// One token-log row for a hook answer.
struct LogRow {
    tool: String,
    returned: usize,
    keys: Vec<Injected>,
    /// The session, the subagent (empty in the main session) and the
    /// transcript the injection lands in, for `hook_inject`.
    session: String,
    agent: String,
    transcript: String,
    /// A subagent start: (session, agent), logged as a "brief without
    /// atlas" row beside the answer's own.
    brief: Option<(String, String)>,
    /// A live post-compaction lookup row for `compact_miss`. A row with an
    /// empty `tool` carries only this.
    miss: Option<crate::compact::MissRow>,
}

/// The semantic side of the recall gate: the model, the record vectors,
/// and the background embedding of records that have none yet.
struct Sem {
    model: Arc<Model>,
    vecs: Option<RecVecs>,
    embedding: Option<Arc<AtomicBool>>,
    scratch: crate::embed::bert::Scratch,
}

/// Hook state for one server process (or one `--hook` run).
pub struct Hooks {
    sem: Option<Sem>,
    model_tried: bool,
    /// Record vectors read from the store over this state's life.
    vectors_read: usize,
    settings: Settings,
    store: MemoryStore,
    /// Each session's let-through Agent launches, oldest first, for their
    /// SubagentStart ([`Hooks::take_brief`]).
    briefs: HashMap<String, VecDeque<LaunchBrief>>,
    injected: HashMap<String, HashSet<String>>,
    /// Bytes of recall injected per session or subagent (the session budget).
    spent: HashMap<String, usize>,
    /// Subagents whose launch is checked at their first tool call.
    unsettled: HashMap<String, Unsettled>,
    /// Each subagent's run: its lane, start and the lanes it wrote under.
    agent_lane: HashMap<String, AgentRun>,
    /// Agents whose long report was sent back once (Q-SD3).
    report_sent_back: HashSet<String>,
    warned: HashMap<PathBuf, u64>,
    usage_cache: HashMap<PathBuf, (u64, Option<(String, u64)>)>,
    log: Option<mpsc::Sender<LogRow>>,
    /// The token-log thread, joined on drop so its rows reach the database
    /// before a command hook's process exits.
    log_thread: Option<std::thread::JoinHandle<()>>,
    router: crate::hook_rewrite::Router,
    /// Where the settings offer reads and records its state.
    offer: Option<crate::claude_settings::Paths>,
    /// The session has no user, so SessionStart asks nothing and spends no
    /// question (see [`crate::claude_settings::unattended_session`]).
    unattended: bool,
    /// The store's record count, at the stamp it was read.
    records: Option<((i64, u64), i64)>,
    /// Files an Edit or Write changed, re-indexed by [Hooks::after].
    edited: Vec<String>,
    /// Transcripts to index into the opt-in transcript source by [Hooks::after].
    transcripts: Vec<PathBuf>,
    /// Opens the index store over the vaults and sources (R7: an edited
    /// note or source file is re-indexed at once); None re-indexes code only.
    index_opener: Option<StoreOpener>,
    index: Option<crate::store::Store>,
    /// The folders atlas indexes, for the first-run project offer; None
    /// makes no offer.
    project_roots: Option<Vec<PathBuf>>,
    /// The configured projects, for the project offer of a project-scoped
    /// configuration (it replaces the folder offer there).
    offer_ctx: Option<crate::claude_settings::OfferCtx>,
    /// The repository each session works in (see [`crate::project::Tracker`]).
    tracker: crate::project::Tracker,
    /// The session of the event being handled.
    session: String,
    /// Its working directory.
    cwd: String,
    /// Lookups per session, for post-compaction misses.
    misses: crate::compact::MissTracker,
    /// Sessions already told about hook drift (at their first prompt).
    drift_told: HashSet<String>,
    /// A command hook's state (one process per event): `drift_told` would
    /// be empty on every prompt, so the drift is told at SessionStart.
    command_hook: bool,
    /// The records the last answer injected, for the memory eval.
    last: Vec<Injected>,
    /// The vaults and sources, by name, that lines refs resolve against.
    roots: Vec<crate::compact::RefRoot>,
    /// The Binding compiler (`recall.subagent_context = "binding"`); `None`
    /// compiles nothing, so a subagent is given no block.
    binding: Option<BindingFn>,
    /// Orchestrator mode: the agents counted against the caps.
    orch: crate::orchestrate::Agents,
    /// Reads a PR's number and head for the merge gate (`gh pr view`).
    pr_head: PrHead,
    /// Idea capture (`memory.ideas`); `None` when it is off, so an off store
    /// reads nothing, writes nothing and adds nothing to context.
    ideas: Option<crate::ideas::Live>,
    /// The main session's compaction prompts (atlas-dev#181).
    main: crate::autocompact::MainWatch,
    /// Per session: the store stamp last looked at and the record id it has
    /// looked past for PR event notices (`docs/design/pr-events.md` section 5).
    pr_seen: HashMap<String, ((i64, u64), i64)>,
    /// Each checkout's remote, read once for the repo-line check (E4).
    remotes: crate::project::Remotes,
    /// The resident server's restart point thread (#221); `None` in a
    /// command hook.
    restart: Option<crate::restart::Refresher>,
    /// An agent started or stopped since the restart point was refreshed.
    restart_due: bool,
    /// Each session whose agents the restart point lists, by its transcript.
    restart_sessions: HashMap<String, String>,
    /// The resident server as its launch entries name it
    /// ([`crate::restart::server_tag`]): while it runs, a crash offer skips
    /// its agents. `None` in a command hook.
    server: Option<String>,
    /// The scheduled-restart warnings the resident server's poll leaves for
    /// the main session's next prompt (#221); `None` in a command hook.
    restart_warn: Option<crate::restart_warn::Notices>,
}

/// How the merge gate reads a PR's number and head sha, given the merge
/// and the working folder.
pub type PrHead = fn(&crate::reviews::Merge, &str) -> crate::reviews::HeadResult;

/// The repository a record is about, lowercased: its `repo:` frontmatter,
/// else the `repo` the tree configuration maps its `branch:` to. `None`
/// for an untagged record.
pub fn repo_tag(meta: &str, tree: Option<&crate::memory::tree::TreeSpec>) -> Option<String> {
    use crate::memory::export::unquote;
    use crate::memory::import::front_value;
    if let Some(r) = front_value(meta, "repo").map(unquote).filter(|r| !r.is_empty()) {
        return Some(r.to_lowercase());
    }
    let b = front_value(meta, "branch").map(unquote)?;
    tree?.branches.iter().find(|x| x.name == b)?.repo.as_ref().map(|r| r.to_lowercase())
}

/// Opens the index store the hook worker re-indexes edited notes and
/// source files through (given by the server, which knows the vaults).
pub type StoreOpener = Box<dyn Fn() -> Result<crate::store::Store> + Send>;

/// Compiles the Binding block of a brief for the lane it declares: given the
/// store, the brief and the lane (`""` when it declares none), the block as
/// its agent will read it (empty for none). The seam BO-1's `binding::compile` plugs into
/// ([`Hooks::set_binding`]).
pub type BindingFn = Box<dyn Fn(&MemoryStore, &str, &str) -> Result<String> + Send>;

/// The most characters a subagent's start answer takes with
/// `recall.subagent_context = "binding"` (Claude Code cuts an answer past
/// 10,000 to a preview).
pub const SUBAGENT_CONTEXT_CHARS: usize = 9_000;

/// How many tool calls of a subagent whose meta file is not written yet
/// wait for it before its launch is settled without it (journal
/// ATLAS-HOOK-VALUE:0008, M1: a first tool call can come before the meta
/// file).
pub const FIRST_CALL_TRIES: u32 = 8;

impl Hooks {
    /// True once a background embedder has been started (for the rescore
    /// test that the pre-embed leaves it nothing to do; xtask cannot see
    /// `cfg(test)` items, so this is public and hidden).
    #[doc(hidden)]
    pub fn embedding_started(&self) -> bool {
        self.sem.as_ref().is_some_and(|s| s.embedding.is_some())
    }

    /// Record vectors read from the store so far (for the test that a
    /// commit touching no record's vector reads none again).
    #[doc(hidden)]
    pub fn vectors_read(&self) -> usize {
        self.vectors_read
    }

    /// Opens the hook state: its own store connection, and a token-log
    /// thread writing to the index database.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`] if the store cannot be opened.
    pub fn open(settings: Settings) -> Result<Hooks> {
        let store = MemoryStore::open(&settings.store)?;
        let (log, log_thread) = start_log(&settings).unzip();
        let router = crate::hook_rewrite::Router::new(&settings);
        let ideas = crate::ideas::Live::open(&settings.ideas, &settings.store);
        Ok(Hooks {
            sem: None,
            model_tried: false,
            vectors_read: 0,
            settings,
            store,
            briefs: HashMap::new(),
            injected: HashMap::new(),
            spent: HashMap::new(),
            unsettled: HashMap::new(),
            binding: None,
            agent_lane: HashMap::new(),
            report_sent_back: HashSet::new(),
            warned: HashMap::new(),
            usage_cache: HashMap::new(),
            log,
            log_thread,
            router,
            // Claude Code's live folder only when the caller asks
            // ([`Hooks::detect_offer_paths`]): a test never reads it.
            offer: None,
            records: None,
            edited: Vec::new(),
            transcripts: Vec::new(),
            index_opener: None,
            index: None,
            project_roots: None,
            offer_ctx: None,
            unattended: false,
            tracker: crate::project::Tracker::default(),
            session: String::new(),
            // Until an event names one: the folder the server runs in.
            cwd: std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default(),
            misses: crate::compact::MissTracker::default(),
            drift_told: HashSet::new(),
            command_hook: false,
            last: Vec::new(),
            roots: Vec::new(),
            orch: crate::orchestrate::Agents::default(),
            pr_head: crate::reviews::gh_head,
            ideas,
            main: crate::autocompact::MainWatch::default(),
            pr_seen: HashMap::new(),
            remotes: Default::default(),
            restart: None,
            restart_due: false,
            server: None,
            restart_sessions: HashMap::new(),
            restart_warn: None,
        })
    }

    /// Keeps the restart point current from now on (the resident server):
    /// refreshed after each SubagentStart and SubagentStop, and on
    /// [`Hooks::refresh_restart_point`] between hooks.
    pub fn enable_restart_point(&mut self) {
        if self.restart.is_none() {
            self.restart = crate::restart::Refresher::start(self.settings.orchestrate.runq_root.clone());
            self.server = Some(crate::restart::server_tag());
        }
    }

    /// Takes the scheduled-restart warnings `n` (the resident server's poll,
    /// [`crate::restart_warn::start`]): each is told once, in the main
    /// session's next prompt.
    pub fn set_restart_warnings(&mut self, n: crate::restart_warn::Notices) {
        self.restart_warn = Some(n);
    }

    /// Hands the restart point to its thread, which reads each worktree's
    /// git state and prepares the catch's bytes. A no-op unless enabled.
    pub fn refresh_restart_point(&mut self) {
        self.restart_due = false;
        if self.restart.is_none() {
            return;
        }
        let p = self.restart_point();
        if let Some(r) = &self.restart {
            r.send(p);
        }
    }

    /// The restart point as this state knows it, git state not yet read:
    /// the running agents of each session that started one, with their
    /// lane, launch key, worktree (from the worktree ledger) and declared
    /// resources, and the open lanes (the agents', and those with work
    /// today).
    pub fn restart_point(&mut self) -> crate::restart::Point {
        let now = std::time::SystemTime::now();
        let rows = crate::worktree_ledger::open_rows(self.store.conn()).unwrap_or_else(|e| {
            eprintln!("atlas restart point: worktree ledger: {e}");
            Vec::new()
        });
        let mut agents = Vec::new();
        let sessions: Vec<(String, String)> = self.restart_sessions.iter().map(|(s, t)| (s.clone(), t.clone())).collect();
        for (session, transcript) in sessions {
            let dir = crate::orchestrate::subagents_dir(&transcript);
            let running = match self.orch.running(&self.settings.orchestrate, &dir, Some(&self.store), now) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("atlas restart point: {}: {e}", dir.display());
                    continue;
                }
            };
            if running.is_empty() {
                self.restart_sessions.remove(&session);
            }
            for r in running {
                let run = self.agent_lane.get(&r.id).or_else(|| self.agent_lane.get(&format!("agent-{}", r.id)));
                let lane =
                    self.orch.sub_lane(&r.tool_use_id).map(str::to_string).or_else(|| run.and_then(|x| x.lane.clone())).unwrap_or_default();
                let worktree = rows
                    .iter()
                    .find(|w| !r.tool_use_id.is_empty() && w.tool_use_id == r.tool_use_id)
                    .or_else(|| rows.iter().find(|w| !lane.is_empty() && w.lane == lane))
                    .map(|w| w.path.clone())
                    .unwrap_or_default();
                agents.push(crate::restart::AgentPoint {
                    id: r.id.clone(),
                    description: r.description.clone(),
                    lane,
                    launch: run.and_then(|x| x.launch.clone()).unwrap_or_default(),
                    tool_use_id: r.tool_use_id.clone(),
                    dir: dir.to_string_lossy().into_owned(),
                    worktree,
                    head: String::new(),
                    dirty: None,
                    uses: run.map(|x| x.uses.clone()).unwrap_or_default(),
                });
            }
        }
        let mut lanes: Vec<String> = agents.iter().map(|a| a.lane.clone()).filter(|l| !l.is_empty()).collect();
        let today: String = crate::memory::now().chars().take(10).collect();
        if !agents.is_empty() {
            if let Ok(all) = self.store.lanes() {
                lanes.extend(all.into_iter().filter(|l| l.last_work.starts_with(&today)).map(|l| l.lane));
            }
        }
        lanes.sort();
        lanes.dedup();
        crate::restart::Point {
            at: crate::memory::now(),
            agents,
            lanes,
            holders: Vec::new(),
            server: self.server.clone().unwrap_or_default(),
            ..crate::restart::Point::default()
        }
    }

    /// Replaces how the merge gate reads a PR's head (tests; the default asks
    /// `gh pr view`).
    pub fn set_pr_head(&mut self, f: PrHead) {
        self.pr_head = f;
    }

    /// Sets the Binding compiler the `"binding"` subagent context uses.
    pub fn set_binding(&mut self, f: Option<BindingFn>) {
        self.binding = f;
    }

    /// The warning for a setup with `recall.subagent_context = "binding"`
    /// and no Binding compiler set (subagents then get nothing).
    pub fn binding_warning(&self) -> Option<&'static str> {
        (self.settings.recall.subagent_context == SubagentContext::Binding && self.binding.is_none())
            .then_some("atlas hooks: recall.subagent_context is \"binding\" but no Binding compiler is set, so subagents get no lane context (as with \"off\")")
    }

    /// Moves every launch let through so far `by` into the past (tests: a
    /// launch held at the permission prompt).
    #[doc(hidden)]
    pub fn age_launches(&mut self, by: std::time::Duration) {
        for b in self.briefs.values_mut().flatten() {
            b.at = b.at.checked_sub(by).unwrap_or(b.at);
        }
        self.orch.age(by);
    }

    /// Marks this state as a command hook's (one process per event): an
    /// unfinished agent is then not excluded for being older than the
    /// process, only for being stale. A prompt adds no hook drift line
    /// (every prompt would): SessionStart tells it once instead.
    pub fn set_command_hook(&mut self) {
        self.orch.set_started(None);
        self.command_hook = true;
    }

    /// Gives the hook state the index store's opener, so an edited note or
    /// source file is re-indexed at once (see [Hooks::after]).
    pub fn set_index_opener(&mut self, f: Option<StoreOpener>) {
        self.index_opener = f;
        self.index = None;
    }

    /// Work that waits until the hook's answer is sent: re-indexing the files
    /// an Edit, Write or MultiEdit changed (R7). A file under a code root gets
    /// its code rows rewritten from the line scan; a Markdown file under a
    /// vault or source is rescanned. Fails open: an error is logged as a miss
    /// and nothing else happens; the edit itself was never held.
    pub fn after(&mut self) {
        // An agent started or stopped: the restart point follows (#221).
        if self.restart_due {
            self.refresh_restart_point();
        }
        self.file_ideas();
        self.index_transcripts();
        for path in std::mem::take(&mut self.edited) {
            if let Err(e) = self.reindex(&path) {
                eprintln!("atlas reindex {path}: {e}");
                self.router.miss_text(&format!("reindex {path}"), &e);
            }
        }
    }

    /// Files the captures this event spooled; a command hook calls it after
    /// printing.
    pub fn file_ideas(&mut self) {
        if let Some(i) = self.ideas.as_mut() {
            i.after(&mut self.store);
        }
    }

    /// Indexes the transcripts a PreCompact queued into the opt-in transcript
    /// source (`memory.transcripts`); a command hook calls it after printing.
    pub fn index_transcripts(&mut self) {
        for path in std::mem::take(&mut self.transcripts) {
            let r = crate::store::open_raw(&self.settings.index_db).and_then(|c| {
                let _ = c.busy_timeout(std::time::Duration::from_secs(10));
                crate::transcripts::index(&c, &path)
            });
            if let Err(e) = r {
                eprintln!("atlas transcripts {}: {e}", path.display());
            }
        }
    }

    fn reindex(&mut self, path: &str) -> Result<()> {
        if path.ends_with(".rs") || path.ends_with(".wgsl") {
            self.router.reindex_code(path)?;
            return Ok(());
        }
        if !path.ends_with(".md") {
            return Ok(());
        }
        let Some(open) = &self.index_opener else { return Ok(()) };
        if self.index.is_none() {
            self.index = Some(open()?);
        }
        let Some(store) = self.index.as_mut() else { return Ok(()) };
        // Compared canonical: a root may be stored in another spelling
        // (`\\?\`, case, separators, a link) than the tool wrote the path in.
        let p = crate::project::path_key(Path::new(path));
        let under = |root: &Path| crate::project::key_within(&p, &crate::project::path_key(root));
        if store.vaults().all().iter().any(|v| under(&v.root)) {
            store.rescan()?;
        }
        let names: Vec<String> = store.sources().iter().filter(|s| under(&s.root)).map(|s| s.name.clone()).collect();
        if !names.is_empty() {
            let only: Vec<&str> = names.iter().map(String::as_str).collect();
            store.rescan_sources(Some(&only))?;
        }
        Ok(())
    }

    /// Turns on the one-time offer to add the session's folder as a source,
    /// given the folders atlas already indexes.
    pub fn set_project_roots(&mut self, roots: Option<Vec<PathBuf>>) {
        self.project_roots = roots;
    }

    /// The vaults and sources (name, root) that a lines ref (`src:path:a-b@hash`)
    /// resolves against; without them such refs are reported unchecked.
    pub fn set_roots(&mut self, roots: Vec<crate::compact::RefRoot>) {
        self.roots = roots;
    }

    /// Says the session has no user to answer SessionStart's questions: nothing
    /// is asked, and none is spent. `atlas --hook` reads it from the session; the
    /// MCP `hook` tool sets it always, since a server cannot tell.
    pub fn set_unattended(&mut self, unattended: bool) {
        self.unattended = unattended;
    }

    /// The setup the MCP server's hook route gives its hooks
    /// ([`crate::protocol::serve_with_hooks`]): unattended, with the Binding
    /// compiler set. The memory eval's subagent starts use it too, so they
    /// fire the route's own path.
    pub fn serve_setup(&mut self) {
        self.set_unattended(true);
        self.set_binding(Some(binding_compiler()));
    }

    /// Turns on the project offer of a project-scoped configuration.
    pub fn set_offer_ctx(&mut self, ctx: Option<crate::claude_settings::OfferCtx>) {
        self.offer_ctx = ctx;
    }

    /// The repository a session works in: the one its tool calls touched
    /// most recently and most, else the one holding its folder.
    pub fn active_repo(&self, session: &str, cwd: &str) -> Option<String> {
        let g = self.settings.graph.as_ref()?;
        self.tracker.active(g, session, cwd)
    }

    /// Points the settings offer and the hook drift check at the user's
    /// Claude Code folder (`$CLAUDE_CONFIG_DIR`, else `<home>/.claude`).
    /// [`Hooks::open`] leaves them off, so only a real hook process or
    /// server reads the live settings.json.
    pub fn detect_offer_paths(&mut self) {
        self.offer = crate::claude_settings::Paths::detect_in(None, &self.settings.index_db, self.settings.user_state.as_deref());
    }

    /// Points the settings offer at other files (tests use a
    /// scratch home), or turns it off with `None`.
    pub fn set_offer_paths(&mut self, p: Option<crate::claude_settings::Paths>) {
        self.offer = p;
    }

    /// The hook drift warning, when the live atlas hooks differ from
    /// hooks/hooks.json (see [`crate::claude_settings::hook_drift_line`]).
    fn drift_line(&self) -> Option<String> {
        let (ideas_on, asks_on) = (self.ideas.is_some(), self.ideas.as_ref().is_some_and(crate::ideas::Live::asks));
        self.offer.as_ref().and_then(|p| crate::claude_settings::hook_drift_line(p, ideas_on, asks_on))
    }

    /// Loads what the first recall would (the term frequencies, the model
    /// and the record vectors), so the first hook answers warm.
    pub fn warm(&mut self) {
        let _ = self.store.rarest(&[], 0);
        let _ = self.store.lanes();
        if let Err(e) = self.refresh_vectors() {
            eprintln!("atlas recall vectors: {e}");
        }
    }

    /// Loads the embedding model, if the file exists, and starts embedding
    /// the records that have no vector for it. Without a model the gate is
    /// lexical.
    fn load_model(&mut self) {
        if self.model_tried {
            return;
        }
        // Absent: looked for again on the next event (one stat), so a model
        // downloaded while the server runs is picked up without a restart.
        // A verified download is renamed into place whole, so a file that
        // exists is complete.
        if !self.settings.model.is_file() {
            return;
        }
        self.model_tried = true;
        match Model::load(&self.settings.model) {
            Ok(m) => {
                let model = Arc::new(m);
                let pending = vectors::pending(&self.store, &model.id, 1).map(|p| !p.is_empty()).unwrap_or(false);
                let embedding = pending.then(|| vectors::spawn_embedder(self.settings.store.clone(), model.clone(), self.settings.embed_threads));
                self.sem = Some(Sem { model, vecs: None, embedding, scratch: Default::default() });
            }
            Err(e) => eprintln!("atlas recall model: {e}"),
        }
    }

    /// The record vectors, reloaded when the store changed; a background
    /// embedding is restarted when records without a vector appear.
    fn refresh_vectors(&mut self) -> Result<()> {
        self.load_model();
        let Some(sem) = self.sem.as_mut() else { return Ok(()) };
        let stale = match &sem.vecs {
            None => true,
            Some(v) => v.stale(&self.store)?,
        };
        if stale {
            // Kept rows are not read again: most commits (a journal entry, a
            // token log line) touch no record's vector.
            let v = match sem.vecs.as_mut() {
                Some(v) => {
                    v.refresh(&self.store)?;
                    v
                }
                None => sem.vecs.insert(RecVecs::load(&self.store, &sem.model.id, sem.model.dim)?),
            };
            self.vectors_read += v.read;
        }
        // Only on a change: an embedder that failed is not restarted on every prompt.
        let idle = sem.embedding.as_ref().is_none_or(|d| d.load(Ordering::Acquire));
        if stale && idle && sem.vecs.as_ref().is_some_and(|v| v.missing > 0) {
            sem.embedding = Some(vectors::spawn_embedder(self.settings.store.clone(), sem.model.clone(), self.settings.embed_threads));
        }
        Ok(())
    }

    /// The store this hook state reads.
    pub fn store(&self) -> &MemoryStore {
        &self.store
    }

    /// Answers one event. Never fails: an error becomes an empty answer (a
    /// hook must not break the session), reported on stderr.
    pub fn handle(&mut self, ev: &HookEvent) -> HookAnswer {
        // A test-only trigger for the panic boundary (see `guard`).
        #[cfg(test)]
        if ev.tool_name == TEST_PANIC_TOOL {
            panic!("test hook panic");
        }
        let mut a = match self.try_handle(ev) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("atlas hook {}: {e}", ev.event);
                HookAnswer::default()
            }
        };
        // A real hook (never a model's call of the `hook` tool, which could
        // name any session and would take the notice into its context):
        // this server records the version it runs for the session (a no-op
        // outside a server), and a newer release on disk is told once, to
        // the user only, on the next prompt (issue #190).
        if !ev.tool_use {
            crate::update::note_session(&ev.session_id);
        }
        if takes_live_notice(ev) && a.block.is_none() && a.instructions.is_empty() {
            if let Some(m) = crate::update::take_live_notice() {
                a.system_message = m;
            }
        }
        a
    }

    fn try_handle(&mut self, ev: &HookEvent) -> Result<HookAnswer> {
        let mut a = HookAnswer::default();
        let mut keys = Vec::new();
        self.session = ev.session_id.clone();
        if !ev.cwd.is_empty() {
            self.cwd = ev.cwd.clone();
        }
        if matches!(ev.event.as_str(), "PreToolUse" | "PostToolUse") {
            if let Some(g) = self.settings.graph.clone() {
                self.tracker.touch_input(&g, &ev.session_id, &ev.tool_input);
            }
        }
        match ev.event.as_str() {
            "UserPromptSubmit" => {
                if ev.prompt.starts_with("<task-notification>") {
                    a.context = self.notification(&ev.prompt, &ev.transcript_path)?;
                } else if let Some(reports) = agent_reports(&ev.prompt) {
                    // Another session's report: a head line per lane it
                    // names, no lane view and no pointers.
                    let heads = self.peer_heads(&reports);
                    if let Some(i) = self.ideas.as_mut() {
                        i.on_prompt(&mut self.store, &ev.session_id, &ev.prompt, &ev.transcript_path);
                    }
                    (a.context, keys) = heads?;
                } else {
                    // After recall, so it cannot recall itself, and whatever
                    // recall answered: an error or a spent budget files all the same.
                    let recalled = self.recall(&ev.session_id, &ev.prompt);
                    if let Some(i) = self.ideas.as_mut() {
                        i.on_prompt(&mut self.store, &ev.session_id, &ev.prompt, &ev.transcript_path);
                    }
                    let (t, k) = recalled?;
                    a.context = t;
                    keys = k;
                }
                if ev.agent_id.is_empty() {
                    self.main_context(ev, &mut a);
                    // A restart pending or scheduled (#221): once per change.
                    if !ev.tool_use {
                        if let Some(p) = self.restart_warn.as_ref().and_then(crate::restart_warn::Notices::take) {
                            let n = self.restart_point().agents.len();
                            if !a.context.is_empty() {
                                a.context.push('\n');
                            }
                            a.context.push_str(&crate::restart_warn::line(&p, n));
                        }
                    }
                }
                // A session's first prompt: live atlas hooks that drifted from
                // hooks/hooks.json (a hook every setup has, unlike SessionStart).
                // A command hook is a new process per prompt, so it would tell
                // every prompt: SessionStart tells that setup instead.
                if !self.command_hook && self.drift_told.insert(ev.session_id.clone()) {
                    if let Some(line) = self.drift_line() {
                        if !a.context.is_empty() {
                            a.context.push('\n');
                        }
                        a.context.push_str(&line);
                    }
                }
            }
            "SubagentStart" => {
                // The launch's pending record ends here: the agent is running.
                if self.settings.orchestrate.enabled && !ev.transcript_path.is_empty() {
                    self.orch.started(&crate::orchestrate::subagents_dir(&ev.transcript_path), &ev.agent_id);
                }
                self.agent_lane.entry(ev.agent_id.clone()).or_insert_with(|| AgentRun { start: crate::memory::now(), ..AgentRun::default() });
                // The restart point lists it once the answer is out (#221).
                if !ev.transcript_path.is_empty() {
                    self.restart_sessions.insert(ev.session_id.clone(), ev.transcript_path.clone());
                }
                self.restart_due = true;
                // Its launch. Claude Code writes the meta file (its toolUseId)
                // only after this hook answers (44 ms to 4.3 s later in every
                // sample, journal:ATLAS-HOOK-VALUE:0006), so the launch is
                // taken only when that is unambiguous (`guess_launch`) and is
                // checked against the meta file at the agent's first tool
                // call; an ambiguous start is answered there instead. A meta
                // file already written names the launch. With no transcript
                // at all (no meta file can ever be read) the oldest launch is
                // the answer.
                let now = std::time::Instant::now();
                let (launch_id, brief, guessed) = if ev.transcript_path.is_empty() {
                    (None, self.take_brief(&ev.session_id, None, now), false)
                } else {
                    let dir = crate::orchestrate::subagents_dir(&ev.transcript_path);
                    match crate::orchestrate::launch_id_of(&dir, &ev.agent_id) {
                        Some(t) => {
                            let b = self.take_brief(&ev.session_id, Some(&t), now);
                            (Some(t), b, false)
                        }
                        // No agent type: compaction's summariser, or no agent
                        // launched through Agent or Task. It takes no launch.
                        None if ev.agent_type.is_empty() => (None, None, false),
                        None => (None, self.guess_launch(ev, &dir, now), true),
                    }
                };
                let launch = launch_id.as_deref().or(brief.as_ref().map(|b| b.tool_use_id.as_str()));
                let sub_lane = launch.and_then(|t| self.orch.sub_lane(t)).map(str::to_string);
                let sub_line = self.sub_lane_line(&ev.agent_id, launch);
                let told_sub = sub_line.is_some();
                // A guess is checked at the first tool call (one with no tool
                // use id cannot be, and stands); so is a start with no launch.
                let given = brief.as_ref().filter(|b| guessed && !b.tool_use_id.is_empty()).map(|b| b.tool_use_id.clone());
                let stray = ev.agent_type.is_empty() && !ev.transcript_path.is_empty() && launch_id.is_none();
                let unsettled = !stray && (given.is_some() || brief.is_none());
                match brief {
                    Some(LaunchBrief { brief: b, binding, model, description, .. }) => {
                        self.note_lane(&ev.agent_id, &b);
                        let (t, k) = self.subagent_context(&ev.agent_id, &b, binding)?;
                        a.context = self.with_sub_line(t, sub_line, SUBAGENT_CONTEXT_CHARS);
                        keys = k;
                        // After its context, so the agent is not handed its own brief.
                        let l = LaunchEntry {
                            agent: &ev.agent_id,
                            agent_type: &ev.agent_type,
                            model: &model,
                            description: &description,
                            brief: &b,
                            session: &ev.session_id,
                            transcript: &ev.transcript_path,
                        };
                        match self.file_launch(&l, sub_lane.as_deref()) {
                            Ok(key) => {
                                if let Some(run) = self.agent_lane.get_mut(&ev.agent_id) {
                                    run.launch = Some(key);
                                }
                            }
                            Err(e) => eprintln!("atlas hook: launch brief of agent {}: {e}", ev.agent_id),
                        }
                    }
                    None => a.context = self.with_sub_line(String::new(), sub_line, SUBAGENT_CONTEXT_CHARS),
                }
                if unsettled {
                    let u = Unsettled { session: ev.session_id.clone(), given, told: !a.context.is_empty(), told_sub, confirmed: None, tries: 0 };
                    self.unsettled.insert(ev.agent_id.clone(), u);
                }
            }
            "PreToolUse" => {
                let sub = !ev.agent_id.is_empty();
                // A main-session lookup made before the last compaction and
                // again after it is a post-compaction repeat; a miss when its
                // target is unchanged. Subagent calls neither count nor stamp.
                if !sub && self.settings.compact.enabled {
                    self.note_lookup(ev);
                }
                if sub {
                    let path = subagent_transcript(&ev.transcript_path, &ev.agent_id);
                    self.cap(ev, &path, &mut a);
                } else {
                    self.main_context(ev, &mut a);
                }
                if a.deny.is_none() && (ev.tool_name == "Agent" || ev.tool_name == "Task") {
                    let brief = if ev.tool_prompt.is_empty() {
                        ev.tool_input.get("prompt").and_then(Value::as_str).unwrap_or("")
                    } else {
                        &ev.tool_prompt
                    };
                    let launch = crate::orchestrate::Launch {
                        transcript: &ev.transcript_path,
                        tool_use_id: &ev.tool_use_id,
                        input: &ev.tool_input,
                        brief,
                        store: Some(&self.store),
                    };
                    a.deny = self.orch.admit(&self.settings.orchestrate, &launch, std::time::SystemTime::now(), std::time::Instant::now());
                    let admitted = a.deny.is_none();
                    let kind_refused = self.orch.kind_refused();
                    self.queue_launch(ev, brief, kind_refused, &mut a);
                    if admitted && a.deny.is_some() {
                        self.orch.release_last();
                    }
                    if a.deny.is_none() && self.settings.orchestrate.judge.enabled {
                        self.judge_launch(ev, brief, &mut a);
                    }
                    if a.deny.is_none() && self.settings.orchestrate.judge.classifier.enabled {
                        self.classify_launch(ev, brief, &mut a);
                    }
                    if a.deny.is_none() {
                        crate::worktree_ledger::hook(&self.store, &self.settings, ev, brief);
                    }
                }
                if a.deny.is_none() && ev.tool_name == "SendMessage" {
                    self.resume_gate(ev, &mut a);
                }
                if a.deny.is_none() && matches!(ev.tool_name.as_str(), "Bash" | "PowerShell") {
                    self.review_gate(ev, &mut a);
                }
                // A report handed back through a tool is the tool's input.
                if a.deny.is_none() && sub && is_handback_tool(&ev.tool_name) {
                    let report = ev.tool_input.get("message").and_then(Value::as_str).unwrap_or("");
                    a.deny = self.report_backstop(&ev.agent_id, report, false);
                }
                if sub && is_journal_tool(&ev.tool_name) {
                    // The lane the agent itself writes under, when the hook
                    // input carries the tool input (a command hook does).
                    if let Some(l) = ev.tool_input.get("lane").and_then(Value::as_str).map(str::trim).filter(|l| !l.is_empty()) {
                        let run = self.agent_lane.entry(ev.agent_id.clone()).or_insert_with(|| AgentRun { start: crate::memory::now(), ..AgentRun::default() });
                        if !run.wrote.iter().any(|w| w == l) {
                            run.wrote.push(l.to_string());
                        }
                    }
                }
                if a.deny.is_none() {
                    if (ev.tool_name == "Agent" || ev.tool_name == "Task") && !ev.tool_prompt.is_empty() {
                        // The prompt as sent: the hook never rewrites a launch.
                        // A second PreToolUse of one tool call replaces its entry.
                        // Its Binding block, compiled once the launch is let through.
                        let binding = self.binding_for(&ev.tool_prompt);
                        let agent_type = ev.tool_input.get("subagent_type").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or("general-purpose").to_string();
                        let q = self.briefs.entry(ev.session_id.clone()).or_default();
                        q.retain(|b| ev.tool_use_id.is_empty() || b.tool_use_id != ev.tool_use_id);
                        // A refusal of it is written after this: the
                        // transcript is scanned for one from here on.
                        let offset = std::fs::metadata(&ev.transcript_path).map(|m| m.len()).unwrap_or(0);
                        let field = |k: &str| ev.tool_input.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
                        let (model, description) = (field("model"), field("description"));
                        q.push_back(LaunchBrief {
                            brief: ev.tool_prompt.clone(),
                            tool_use_id: ev.tool_use_id.clone(),
                            agent_type,
                            at: std::time::Instant::now(),
                            binding,
                            holder: None,
                            offset,
                            model,
                            description,
                        });
                    }
                    // A subagent whose start could not confirm its launch: its
                    // first tool call, by when its meta file names the launch,
                    // checks a guess or answers an ambiguous start.
                    if sub {
                        if let Some(u) = self.unsettled.remove(&ev.agent_id) {
                            let (t, k) = self.first_call(ev, u, a.context.chars().count())?;
                            if !t.is_empty() {
                                if !a.context.is_empty() {
                                    a.context.push('\n');
                                }
                                a.context.push_str(&t);
                                keys = k;
                            }
                        }
                    }
                }
                if a.deny.is_none() {
                    // Routing fails open: an error leaves the call as it is.
                    match self.router.pre_tool_use(ev) {
                        Ok(u) => a.updated_input = u,
                        Err(e) => self.router.miss(ev, &e),
                    }
                }
            }
            // The turn is over: the one moment a question interrupts nothing.
            "Stop" if ev.agent_id.is_empty() => {
                if let Some(i) = self.ideas.as_mut() {
                    // Both arrays present and empty: an input without them says nothing.
                    let none = |v: &Value| v.as_array().is_some_and(Vec::is_empty);
                    let idle = none(&ev.background_tasks) && none(&ev.session_crons);
                    a.block = i.on_stop(&mut self.store, &ev.session_id, ev.stop_hook_active, idle, &ev.transcript_path);
                }
            }
            // Compaction's summariser fires this too, with an agent id and no agent
            // type: it is no agent's report, so it is neither filed nor sent back.
            "SubagentStop" if !ev.agent_id.is_empty() && !ev.agent_type.is_empty() => {
                self.end_unsettled(ev);
                self.restart_due = true;
                a.block = self.report_backstop(&ev.agent_id, &ev.last_message, ev.stop_hook_active);
                // atlas's own record of the finish (#204): Claude Code may
                // never write `"finished": true` (a full disk), and a held
                // lane must not wait `stale_minutes` for it. A stop sent back
                // keeps running; an agent that writes again counts again.
                // The record in the store is for other processes (`atlas
                // queue next`, `atlas backlog`, a command hook): best effort,
                // since on a full disk it fails and only this process knows.
                // An agent that stops while its own background work runs
                // (`background_tasks` not empty) has not finished: it keeps
                // its lane and slot (#222 R2). An input without the field
                // files the finish as before.
                let waiting = ev.background_tasks.as_array().is_some_and(|t| !t.is_empty());
                if a.block.is_none() && !waiting {
                    let id = ev.agent_id.strip_prefix("agent-").unwrap_or(&ev.agent_id);
                    let now = std::time::SystemTime::now();
                    self.orch.finished(id, now);
                    if let Err(e) = crate::orchestrate::record_finish(&self.store, id, now) {
                        eprintln!("atlas agent cap: release record of {id}: {e}");
                    }
                }
            }
            "PostToolUse" if ev.tool_name == "AskUserQuestion" => {
                if let (Some(i), true) = (self.ideas.as_mut(), ev.agent_id.is_empty()) {
                    i.on_ask(&mut self.store, &ev.session_id, &ev.tool_input, &ev.tool_response);
                }
            }
            "PostToolUse" => {
                // R7: a file an edit changed is re-indexed once the answer is out.
                if matches!(ev.tool_name.as_str(), "Edit" | "Write" | "MultiEdit") {
                    let f = ev.tool_input.get("file_path").and_then(Value::as_str).unwrap_or("");
                    if !f.is_empty() && !self.edited.iter().any(|x| x == f) {
                        self.edited.push(f.to_string());
                    }
                }
                match self.router.post_tool_use(ev) {
                    Ok(Some(u)) => a.updated_output = Some(u),
                    // A result that passes whole is annotated from the code index.
                    Ok(None) => match self.router.annotate(ev) {
                        Ok((u, note)) => {
                            a.updated_output = u;
                            a.context = note;
                        }
                        Err(e) => self.router.miss(ev, &e),
                    },
                    Err(e) => {
                        self.router.miss(ev, &e);
                        // The raw result carries only the tail's marker; say
                        // it plainly (issue bloodfeast/atlas#3).
                        a.context = crate::hook_rewrite::exit_note(ev).unwrap_or_default().to_string();
                    }
                }
            }
            // Nothing, except the first-session questions, each until answered:
            // the settings offer among them (at most 60 tokens).
            "SessionStart" => {
                // A session with no user gets no question, and none is spent: `-p`
                // would print the question as its output. The questions stay until
                // answered, so they are put once a session: a resume or a
                // compaction is the same session.
                let asks = !self.unattended && !matches!(ev.source.as_str(), "resume" | "compact");
                // The status line badge follows atlas to its new version
                // folder, silently (#194, ruling (a)).
                if let (Some(p), Some(exe)) = (self.offer.as_ref(), crate::claude_settings::badge_exe()) {
                    crate::claude_settings::repair_statusline(p, &exe);
                }
                if let Some(line) = self.offer.as_ref().filter(|_| asks).and_then(crate::claude_settings::session_start_line) {
                    a.context = line;
                }
                // Per folder: whether to add this project as a source (or,
                // with projects, its repositories as a project).
                let mut lines: Vec<String> = Vec::new();
                // Per user: whether to work in orchestrated mode.
                lines.extend(self.offer.as_ref().filter(|_| asks).and_then(crate::claude_settings::orchestrate_line));
                match (self.offer.as_ref(), self.offer_ctx.as_ref()) {
                    (Some(p), Some(ctx)) if ctx.scoped => {
                        let cwd = Path::new(&ev.cwd);
                        if asks {
                            lines.extend(crate::claude_settings::repos_line(p, cwd, ctx));
                            // A folder in no project and holding no repositories
                            // gets the question a fresh install's folder gets.
                            if !crate::claude_settings::folder_of_repos(cwd) && !crate::project::is_repo(cwd) && crate::project::find_project_file(cwd).is_none() {
                                let roots = self.project_roots.as_deref().unwrap_or(&ctx.known);
                                lines.extend(crate::claude_settings::project_line(p, cwd, roots));
                            }
                            // A notice, marked when shown: not spent on a session
                            // with no user, or on a resume.
                            lines.extend(crate::claude_settings::unmapped_line(p, ctx));
                        }
                    }
                    (Some(p), _) if asks => {
                        if let Some(roots) = self.project_roots.as_ref() {
                            // A folder of repositories is offered as a new
                            // project (add-project --new moves the setup into
                            // projects first); one repository gets the vault
                            // question.
                            let cwd = Path::new(&ev.cwd);
                            let line = match crate::claude_settings::folder_of_repos(cwd) {
                                true => crate::claude_settings::repos_line(p, cwd, &crate::claude_settings::OfferCtx::unscoped(roots)),
                                false => crate::claude_settings::project_line(p, cwd, roots),
                            };
                            lines.extend(line);
                        }
                    }
                    _ => {}
                }
                for line in lines {
                    if !a.context.is_empty() {
                        a.context.push(' ');
                    }
                    a.context.push_str(&line);
                }
                let fresh = ev.source == "startup" || ev.source == "clear";
                // Hook drift, once a session, when no prompt will tell it: a
                // recalling event routed to a command hook, and no mcp_tool
                // UserPromptSubmit (a resident server tells it at the first
                // prompt). A line of its own, outside the questions' budget.
                if fresh && self.offer.as_ref().is_some_and(crate::claude_settings::drift_told_at_session_start) {
                    if let Some(line) = self.drift_line() {
                        if !a.context.is_empty() {
                            a.context.push('\n');
                        }
                        a.context.push_str(&line);
                    }
                }
                // A new or cleared session: where the work stands
                // (`memory.resume_card`). Compaction has its own card. The
                // card is advisory: a store error loses it, not the lines
                // above.
                // A /clear: the card of the checkpoint SessionEnd (reason
                // clear) wrote from the ending transcript (advisory, as the
                // resume card is).
                if ev.source == "clear" && self.settings.compact.enabled {
                    match self.clear_card(ev) {
                        Ok(Some(card)) => {
                            if !a.context.is_empty() {
                                a.context.push('\n');
                            }
                            a.context.push_str(&card);
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("atlas clear card: {e}"),
                    }
                }
                let mut ideas_line = if fresh { self.ideas.as_ref().and_then(|i| i.resume_line(&self.store)) } else { None };
                if fresh && self.settings.resume_card {
                    match crate::resume::card_with(&self.store, &self.settings.orchestrate, &ev.transcript_path, &crate::memory::now(), ev.source == "clear", ideas_line.as_deref()) {
                        Ok(Some(card)) => {
                            // A full card may have cut it: then it stays a line of its own.
                            if ideas_line.as_deref().is_some_and(|l| card.contains(l)) {
                                ideas_line = None;
                            }
                            if !a.context.is_empty() {
                                a.context.push('\n');
                            }
                            a.context.push_str(&card);
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("atlas resume card: {e}"),
                    }
                }
                // No card to carry it (the card is off or has nothing else):
                // the user with no goal and no agents still sees the count.
                if let Some(line) = ideas_line {
                    if !a.context.is_empty() {
                        a.context.push('\n');
                    }
                    a.context.push_str(&line);
                }
                // The restart point of a machine that went down, or of a
                // session that ended with agents unfinished (#221): offered
                // once, to a session with a user, at most OFFER_CHARS.
                if !self.unattended && ev.agent_id.is_empty() && matches!(ev.source.as_str(), "startup" | "clear" | "resume") {
                    match crate::restart::offer(
                        &mut self.store,
                        &ev.session_id,
                        std::time::SystemTime::now(),
                    ) {
                        Ok(Some(line)) => {
                            if !a.context.is_empty() {
                                a.context.push('\n');
                            }
                            a.context.push_str(&line);
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("atlas restart point: {e}"),
                    }
                }
                if ev.source == "compact" && self.settings.compact.enabled {
                    // The checkpoint card first, then the ref report.
                    if let Some(h) = crate::checkpoint::newest(&self.store, &ev.session_id)? {
                        if !a.context.is_empty() {
                            a.context.push('\n');
                        }
                        a.context.push_str(&crate::checkpoint::card(&h));
                        a.context.push('\n');
                    }
                    let report = self.after_compact(ev)?;
                    if !report.is_empty() {
                        if !a.context.is_empty() {
                            a.context.push('\n');
                        }
                        a.context.push_str(&report);
                    }
                }
            }
            // A /clear ends the session first (ATLAS-CLEAR 0006): its
            // checkpoint is written here, from its own transcript, before
            // Claude Code clears it. Other ends write nothing, and the answer
            // is never read.
            "SessionEnd" if ev.reason == "clear" && ev.agent_id.is_empty() && self.settings.compact.enabled => {
                self.clear_checkpoint(ev)?;
            }
            "PreCompact" => {
                // The opt-in transcript source is brought up to date once the
                // answer is out (its own switch, `memory.transcripts`).
                if self.settings.transcripts && !ev.transcript_path.is_empty() {
                    self.transcripts.push(PathBuf::from(&ev.transcript_path));
                }
                // A subagent's compaction (review S4 of #192): blocked past
                // its handoff point under an enforced limit, so it hands off;
                // the checkpoint, the deferral and the backstop below are the
                // main session's (its `session:<id>` lane).
                if !ev.agent_id.is_empty() {
                    a.block = self.subagent_compaction_block(ev);
                    return Ok(a);
                }
                // The rest is the compaction work, on unless `compact.enabled`
                // is false.
                if !self.settings.compact.enabled {
                    return Ok(a);
                }
                let t_pre = std::time::Instant::now();
                // The segment since the last compaction: in-task state and
                // the context curve (visible records only). A transcript not
                // yet written is waited for briefly, then read from the
                // session it was forked from.
                let (t, from) = crate::checkpoint::tail_settled(Path::new(&ev.transcript_path), &ev.session_id, crate::checkpoint::SETTLE_WAIT);
                if self.settings.compact.defer {
                    // The window guess reads the whole session's largest
                    // context, which the tail alone does not hold.
                    let mut t = t.clone();
                    let read = match &from {
                        crate::checkpoint::TailFrom::Parent(p) => p.clone(),
                        _ => PathBuf::from(&ev.transcript_path),
                    };
                    t.max_ctx = t.max_ctx.max(crate::checkpoint::history_max_context(&read));
                    let w = crate::checkpoint::window(self.settings.compact.window, &t);
                    if let Some(reason) = crate::checkpoint::defer(&ev.trigger, &t, w) {
                        if let Some(l) = &self.log {
                            let _ = l.send(LogRow { tool: "compact:defer".into(), returned: 0, keys: Vec::new(), brief: None, session: ev.session_id.clone(), agent: String::new(), transcript: String::new(), miss: None });
                        }
                        a.block = Some(reason);
                        return Ok(a);
                    }
                }
                // The model's own checkpoint entry for this segment. Without
                // one, an automatic compaction inside the prompt zone waits
                // once for it (ATLAS-CLEAR 0003, the backstop).
                let model = crate::checkpoint::model_entry(&self.store, &ev.session_id, &t.start)?;
                if ev.trigger == "auto" && model.is_none() {
                    if let Some(reason) = self.main.wait_for_checkpoint(&self.settings, Path::new(&ev.transcript_path), &ev.session_id, &t.start, t.last_ctx) {
                        a.block = Some(reason);
                        return Ok(a);
                    }
                }
                self.misses.compact(&ev.session_id);
                let trigger = if ev.trigger.is_empty() { "unknown" } else { ev.trigger.as_str() };
                if self.write_checkpoint(&ev.session_id, &ev.transcript_path, trigger, &t, &from, model.as_ref(), t_pre)?.1 {
                    // Only with a card: the text tells the summarizer to leave
                    // out what the card lists. A later error answers nothing.
                    a.instructions = crate::claude_settings::COMPACT_INSTRUCTIONS;
                }
            }
            _ => {}
        }
        if let Some(line) = self.pr_notice(ev) {
            if !a.context.is_empty() {
                a.context.push('\n');
            }
            a.context.push_str(&line);
        }
        self.last = keys.clone();
        if matches!(ev.event.as_str(), "UserPromptSubmit" | "SubagentStart") || !a.context.is_empty() || !a.instructions.is_empty() || a.deny.is_some() || a.updated_input.is_some() || a.updated_output.is_some() || (ev.event == "Stop" && a.block.is_some()) {
            let returned = a.to_json(&ev.event).len();
            if let Some(l) = &self.log {
                let brief = (ev.event == "SubagentStart").then(|| (ev.session_id.clone(), ev.agent_id.clone()));
                // The injection lands in the subagent's own transcript, when in one.
                let transcript = if ev.agent_id.is_empty() || ev.transcript_path.is_empty() {
                    ev.transcript_path.clone()
                } else {
                    subagent_transcript(&ev.transcript_path, &ev.agent_id).to_string_lossy().into_owned()
                };
                let _ = l.send(LogRow {
                    tool: format!("hook:{}", ev.event),
                    returned,
                    keys,
                    brief,
                    session: ev.session_id.clone(),
                    agent: ev.agent_id.clone(),
                    transcript,
                    miss: None,
                });
            }
        }
        Ok(a)
    }

    /// Counts one main-session PreToolUse for the live miss figures: the
    /// call, the files it writes (or a shell command, which may write
    /// anything), and a lookup's row in `compact_miss` with, for a repeat
    /// of a lookup, the legacy `compact:miss` token-log row.
    fn note_lookup(&mut self, ev: &HookEvent) {
        let (s, m) = (&ev.session_id, &mut self.misses);
        m.call(s);
        match ev.tool_name.as_str() {
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                let f = |k: &str| ev.tool_input.get(k).and_then(Value::as_str).unwrap_or("");
                let p = if f("file_path").is_empty() { f("notebook_path") } else { f("file_path") };
                if !p.is_empty() {
                    m.wrote(s, crate::compact::norm_path(p, &ev.cwd));
                }
            }
            "Bash" | "PowerShell" => m.shell(s),
            _ => {}
        }
        let sig = match crate::compact::lookup_sig(&ev.tool_name, &ev.tool_input) {
            Some(sig) => (sig, false),
            None => match crate::compact::shell_sig(&ev.tool_name, &ev.tool_input) {
                Some(sig) => (sig, true),
                None => return,
            },
        };
        let stamp = m.stamp(s, &ev.tool_name, &ev.tool_input, &ev.cwd, &self.store);
        let Some(row) = m.lookup(s, &sig.0, stamp, sig.1) else { return };
        if let Some(l) = &self.log {
            // An empty tool writes no token-log row: only the legacy repeat has one.
            let tool = if row.legacy { "compact:miss" } else { "" };
            let _ = l.send(LogRow { tool: tool.into(), returned: 0, keys: Vec::new(), brief: None, session: s.clone(), agent: String::new(), transcript: String::new(), miss: Some(row) });
        }
    }

    /// The records the last answer injected, with what context selection
    /// did to each (the memory eval reads it).
    pub fn last_injected(&self) -> &[Injected] {
        &self.last
    }

    /// Every candidate the gate scores for a text (passing or not), in the
    /// hook's order, with the same model and query cut: what a replay of a
    /// past injection needs to score it (`--hook-report`).
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn scores(&mut self, text: &str) -> Result<Vec<recall::Candidate>> {
        let r = self.settings.recall.clone();
        self.refresh_vectors()?;
        let qv = self.sem.as_mut().map(|s| s.model.embed_query_cut(text, r.query_tokens, &mut s.scratch));
        let sem = match (&self.sem, &qv) {
            (Some(s), Some(q)) => s.vecs.as_ref().map(|vecs| recall::Semantic { vecs, query: q, isa: s.model.isa }),
            _ => None,
        };
        let gate = r.gate(sem.is_some());
        recall::candidates(&self.store, text, &gate, sem.as_ref())
    }

    /// The reports of an `<agent-message` prompt (another session's report
    /// or a subagent's hand-back): one [`recall::lane_head`] line for each
    /// lane a report names, found as [`Self::recall`] finds them; reports
    /// that name none get nothing. The lane view stays one `recall lane=`
    /// away. Logged as `lane-head:X`, not `lane:X`: the line is not the lane.
    fn peer_heads(&mut self, reports: &[&str]) -> Result<(String, Vec<Injected>)> {
        let mut out = String::new();
        let mut keys = Vec::new();
        let declared: Vec<Option<String>> = reports.iter().map(|r| crate::lanes::declared(r).lane).collect();
        for l in self.store.lanes()? {
            if l.lane.starts_with("session:") || !reports.iter().zip(&declared).any(|(r, d)| names_lane(r, d.as_deref(), &l.lane)) {
                continue;
            }
            if let Some(line) = recall::lane_head(&self.store, &l.lane)? {
                keys.push(Injected { key: format!("lane-head:{}", l.lane), fts: None, cos: None, bytes: line.len(), how: Sel::Whole, full: line.len() });
                out.push_str(&line);
            }
        }
        Ok((out, keys))
    }

    fn recall(&mut self, scope: &str, text: &str) -> Result<(String, Vec<Injected>)> {
        let r = self.settings.recall.clone();
        let mut out = String::new();
        let mut keys = Vec::new();
        // The session budget: past it, nothing more is injected.
        let left = match r.session_budget_tokens {
            0 => usize::MAX,
            cap => self.settings.bytes_for(cap).saturating_sub(self.spent.get(scope).copied().unwrap_or(0)),
        };
        if left == 0 {
            return Ok((out, keys));
        }
        // The scope is the session for a human prompt, else a subagent.
        let human = scope == self.session;
        // A scope this process has not seen yet starts from what was
        // pointed to in it before (a server restart, a command hook's
        // process per event): each key is pointed to once per session.
        if !self.injected.contains_key(scope) {
            let (session, agent) = if human { (scope, "") } else { (self.session.as_str(), scope) };
            let logged = crate::hook_hits::injected_keys(&self.settings.index_db, session, agent);
            self.injected.insert(scope.to_string(), logged);
        }
        // A subagent whose own lane's view (or its parent lane's, for a
        // sub-lane) is injected gets no pointers: its brief and lane
        // already say what it needs.
        let own_lane = if human { None } else { self.agent_lane.get(scope).and_then(|r| r.lane.clone()) };
        let mut own_dump = false;
        // A lane the text names gets its journal first.
        let declared = crate::lanes::declared(text).lane;
        let named: Vec<String> =
            self.store.lanes()?.into_iter().map(|l| l.lane).filter(|l| !l.starts_with("session:") && names_lane(text, declared.as_deref(), l)).collect();
        // A main-session prompt under `main_lane_bytes` gets one lane view,
        // its `Lane:` lane's (else the first it names), under that cap, and
        // a head line for each other lane it names (Q2).
        let capped = human && r.main_lane_bytes > 0;
        let primary = if capped { named.iter().find(|l| declared.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(l))).or(named.first()).cloned() } else { None };
        for lane in named {
            if capped && primary.as_deref() != Some(lane.as_str()) {
                let seen = self.injected.entry(scope.to_string()).or_default();
                if !seen.insert(format!("lane-head:{lane}")) {
                    continue;
                }
                if let Some(head) = recall::lane_head(&self.store, &lane)? {
                    out.push_str(&head);
                    keys.push(Injected { key: format!("lane:{lane}"), fts: None, cos: None, bytes: head.len(), how: Sel::Trimmed, full: head.len() });
                }
                continue;
            }
            let seen = self.injected.entry(scope.to_string()).or_default();
            if !seen.insert(format!("lane:{}", lane)) {
                continue;
            }
            // The lane brief: its state whole, older entries cut to the
            // clauses the text matches (context selection).
            let trim = (r.select.enabled && r.select.lane_chars > 0).then_some((text, r.select.lane_chars));
            let mut budget = self.settings.bytes_for(r.lane_budget_tokens);
            if capped {
                budget = budget.min(r.main_lane_bytes);
            }
            let v = recall::lane_with(&self.store, &lane, budget, trim, false)?;
            out.push_str(&v.text);
            // A sub-lane agent (`X/2`) given its parent's view, which holds
            // its own records, has had its own lane's view too.
            own_dump |= !v.text.is_empty() && own_lane.as_deref().is_some_and(|o| o == lane || crate::lanes::parent_lane(o) == lane);
            // A record the lane view shows whole or trimmed is not pointed to
            // again below (send once).
            if r.select.enabled {
                self.injected.entry(scope.to_string()).or_default().extend(v.keys.iter().cloned());
            }
            let bytes = v.text.len();
            let (how, full) = if v.trimmed > 0 { (Sel::Trimmed, bytes + v.trimmed_full - v.trimmed_sent) } else { (Sel::Whole, bytes) };
            keys.push(Injected { key: format!("lane:{}", lane), fts: None, cos: None, bytes, how, full });
        }
        // A repository the text names gets its line from the project graph
        // (its path, what it is, what it talks to and depends on, what uses
        // it), but only when neither the text nor the working directory says
        // where it is already (E4, journal:ATLAS-EFFICIENCY:0006: E3 found
        // the line informed almost no agent that had either).
        if let Some(g) = self.settings.graph.clone() {
            for r in g.named_in(text) {
                // Sent once per scope: a line already sent is not placed again.
                let k = format!("repo:{}", r.name);
                if self.injected.get(scope).is_some_and(|s| s.contains(&k)) || g.placed(r, text, &self.cwd, &mut self.remotes) {
                    continue;
                }
                self.injected.entry(scope.to_string()).or_default().insert(k);
                if let Some(line) = g.describe(&r.name) {
                    let before = out.len();
                    out.push_str(&format!("atlas project {}: {line}\n", g.name));
                    let bytes = out.len() - before;
                    keys.push(Injected { key: format!("repo:{}", r.name), fts: None, cos: None, bytes, how: Sel::Whole, full: bytes });
                }
            }
        }
        if own_dump {
            *self.spent.entry(scope.to_string()).or_default() += out.len();
            return Ok((out, keys));
        }
        let skip = self.injected.get(scope).cloned().unwrap_or_default();
        self.refresh_vectors()?;
        let qv = self.sem.as_mut().map(|s| s.model.embed_query_cut(text, r.query_tokens, &mut s.scratch));
        let sem = match (&self.sem, &qv) {
            (Some(s), Some(q)) => s.vecs.as_ref().map(|vecs| recall::Semantic { vecs, query: q, isa: s.model.isa }),
            _ => None,
        };
        let mut gate = r.gate(sem.is_some());
        // The calibrated bm25 floor follows the store's size.
        let stamp = self.store.stamp()?;
        let records = match self.records {
            Some((s, n)) if s == stamp => n,
            _ => {
                let n: i64 = self.store.conn().query_row("SELECT COUNT(*) FROM records_fts_docsize", [], |x| x.get(0))?;
                self.records = Some((stamp, n));
                n
            }
        };
        gate.fts_floor *= crate::memory::settings::floor_scale(records);
        // Pointers, not bodies: the body is one `recall key=` away.
        let budget = self.settings.bytes_for(r.budget_tokens).min(left);
        let mut lines = String::new();
        let mut chosen = Vec::new();
        let mut more = 0usize;
        let mut passed: Vec<recall::Candidate> = recall::candidates(&self.store, text, &gate, sem.as_ref())?.into_iter().filter(|c| c.pass && !skip.contains(&c.head.key)).collect();
        // With a model, the prompt's best cosine sets the bar: a record more
        // than `cos_margin` below it is a word match, not the topic. Best
        // meaning first; records without a vector keep their fused place.
        // The relevance floor (tuned on hit data, `--hook-report --tune-floor`):
        // below it a record is not injected at all.
        passed.retain(|c| c.cos.is_none_or(|x| x >= r.inject_floor));
        if let Some(best) = passed.iter().filter_map(|c| c.cos).reduce(f32::max) {
            passed.retain(|c| c.lifted() || c.cos.is_none_or(|x| x >= best - r.cos_margin));
            passed.sort_by(|a, b| b.cos.unwrap_or(f32::MIN).total_cmp(&a.cos.unwrap_or(f32::MIN)));
        }
        // Records about the active repository, then its neighbours, first:
        // a record is about the repository its tag names (see `repo_tag`);
        // a record without a tag gets no boost.
        if let Some(g) = self.settings.graph.clone() {
            if let Some(active) = self.tracker.active(&g, &self.session, &self.cwd) {
                let active = active.to_lowercase();
                let near: Vec<String> = g.neighbours(&active).iter().map(|n| n.to_lowercase()).collect();
                let tree = self.settings.tree.clone();
                let rank = |c: &recall::Candidate| -> u8 {
                    match repo_tag(&c.head.meta, tree.as_ref()) {
                        Some(t) if t == active => 0,
                        Some(t) if near.contains(&t) => 1,
                        _ => 2,
                    }
                };
                passed.sort_by_key(|c| rank(c));
            }
        }
        // The ruling that holds comes before what matches better.
        passed.sort_by_key(|c| !c.lifted());
        let sel = r.select;
        // The records sent so far, for the collapse check.
        let mut sent: Vec<(i64, String, String)> = Vec::new();
        let headlines = r.rules == crate::memory::rules::Mode::Live;
        for cand in passed {
            if human && r.prompt_pointers > 0 && chosen.len() >= r.prompt_pointers {
                more += 1;
                continue;
            }
            let whole = pointer(&cand.head);
            // A ruling's headline stands in for a fragment of it; a tag says
            // what it replaces or amends.
            let head_line = (headlines && crate::memory::rules::managed_kind(&cand.head.kind)).then(|| crate::memory::rules::pointer(&cand.head, &cand.tags));
            let (one, how) = if sel.enabled {
                let rtext = crate::memory::select::record_words_text(&cand.head);
                let dup = sent.iter().find(|(rid, _, t)| {
                    let near = sem.as_ref().and_then(|s| s.vecs.pair(s.isa, *rid, cand.head.rid)).is_some_and(|c| c >= sel.dup_cos);
                    near && crate::memory::select::overlap(t, &rtext) >= sel.dup_overlap
                });
                match dup {
                    Some((_, k, _)) => (collapsed(&cand.head.key, k), Sel::Collapsed),
                    None => {
                        let line = match (&head_line, crate::memory::select::excerpt(&cand.head, text, sel.trim_chars)) {
                            (Some(h), _) => h.clone(),
                            (None, Some(x)) => format!("- {}: {}\n", cand.head.key, x.render(&cand.head)),
                            (None, None) => whole.clone(),
                        };
                        let how = if line == whole { Sel::Whole } else { Sel::Trimmed };
                        sent.push((cand.head.rid, cand.head.key.clone(), rtext));
                        (line, how)
                    }
                }
            } else {
                match &head_line {
                    Some(h) => (h.clone(), Sel::Trimmed),
                    None => (whole.clone(), Sel::Whole),
                }
            };
            if POINTER_HEAD.len() + lines.len() + one.len() + 32 > budget {
                more += 1;
                if how != Sel::Collapsed {
                    sent.pop();
                }
                continue;
            }
            lines.push_str(&one);
            chosen.push(Injected { key: cand.head.key, fts: Some(cand.fts), cos: cand.cos, bytes: one.len(), how, full: whole.len() });
        }
        if !chosen.is_empty() {
            out.push_str(POINTER_HEAD);
            out.push_str(&lines);
            if more > 0 {
                out.push_str(&format!("+{more} more: recall {{query}}\n"));
            }
            let seen = self.injected.entry(scope.to_string()).or_default();
            for k in chosen {
                seen.insert(k.key.clone());
                keys.push(k);
            }
        }
        *self.spent.entry(scope.to_string()).or_default() += out.len();
        Ok((out, keys))
    }

    /// After a compaction (SessionStart, `source: compact`): checks every
    /// ref in the newest summary of the session's transcript, logs the
    /// compaction (`compact_log`), and returns one line per changed or
    /// dangling ref. Fails open: no transcript or no summary is no report.
    fn after_compact(&mut self, ev: &HookEvent) -> Result<String> {
        let t0 = std::time::Instant::now();
        let Some(c) = crate::compact::newest(Path::new(&ev.transcript_path)) else { return Ok(String::new()) };
        let mut check = crate::compact::check(&self.store, &self.roots, &c.summary)?;
        // Turn refs resolve against the transcripts beside this one.
        let dir = Path::new(&ev.transcript_path).parent().map(Path::to_path_buf).unwrap_or_default();
        for r in crate::compact::refs_in(&c.summary) {
            let crate::compact::Target::Turn { session8, at } = &r.target else { continue };
            let file = std::fs::read_dir(&dir).ok().and_then(|rd| rd.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "jsonl") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(session8.as_str()))));
            check.unchecked = check.unchecked.saturating_sub(1);
            if file.and_then(|f| crate::transcripts::check(&f, *at, &r.hash)) != Some(true) {
                check.dangling.push(r.text.clone());
            }
        }
        let report = crate::compact::report_lines(&check);
        let logged = crate::compact::Logged {
            session: ev.session_id.clone(),
            trigger: c.trigger,
            pre_tokens: c.pre_tokens,
            post_tokens: c.post_tokens,
            summary_bytes: c.summary.len(),
            check,
            hook_ms: t0.elapsed().as_secs_f64() * 1e3,
        };
        if let Ok(conn) = crate::store::open_raw(&self.settings.index_db) {
            let _ = conn.busy_timeout(std::time::Duration::from_secs(10));
            let _ = crate::compact::log(&conn, &logged);
        }
        Ok(report)
    }

    fn known_lanes(&self) -> Vec<String> {
        self.store.lanes().map(|ls| ls.into_iter().map(|l| l.lane).filter(|l| !l.starts_with("session:")).collect()).unwrap_or_default()
    }

    /// The line telling agent `agent_id` the sub-lane its launch (`launch`,
    /// a tool use id) was let through as, once: `None` with no sub-lane, or
    /// when the agent was already told it.
    fn sub_lane_line(&mut self, agent_id: &str, launch: Option<&str>) -> Option<String> {
        let s = launch.and_then(|t| self.orch.sub_lane(t)).map(str::to_string)?;
        let run = self.agent_lane.entry(agent_id.to_string()).or_insert_with(|| AgentRun { start: crate::memory::now(), ..AgentRun::default() });
        if run.lane.as_deref() == Some(s.as_str()) {
            return None;
        }
        run.lane = Some(s.clone());
        let p = crate::lanes::parent_lane(&s);
        Some(format!("atlas: you run as sub-lane {s} of {p}: running work holds that lane, so journal and hand off under {s} (its records show under {p} too)."))
    }

    /// A subagent's context answer: `body`, then the sub-lane line when
    /// there is one, with no blank line between. With
    /// `recall.subagent_context = "binding"` the whole is at most `max`
    /// characters, the sub-lane line kept and the body cut between lines.
    fn with_sub_line(&self, body: String, sub_line: Option<String>, max: usize) -> String {
        let capped = self.settings.recall.subagent_context == SubagentContext::Binding;
        let Some(line) = sub_line else {
            return if capped { cap_lines(&body, max) } else { body };
        };
        let mut out = if capped { cap_lines(&body, max.saturating_sub(line.chars().count() + 1)) } else { body };
        out.truncate(out.trim_end_matches('\n').len());
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
        out
    }

    /// The launch a starting agent with no meta file yet is given: the one
    /// launch waiting in its session, else the one waiting launch of its
    /// `agent_type`; `None` when that is ambiguous. A launch held by an
    /// earlier start, or named by an earlier agent's meta file by now, is
    /// not waiting; one the session transcript records as refused is
    /// dropped. The launch stays queued, held by the agent, until its first
    /// tool call checks it; one with no tool use id cannot be checked, and
    /// is taken.
    fn guess_launch(&mut self, ev: &HookEvent, dir: &Path, now: std::time::Instant) -> Option<LaunchBrief> {
        self.settle(&ev.session_id, dir);
        let q = self.briefs.get_mut(&ev.session_id)?;
        q.retain(|b| now.saturating_duration_since(b.at) < crate::orchestrate::LAUNCH_HOLD_TTL);
        let waiting = |q: &VecDeque<LaunchBrief>| q.iter().enumerate().filter(|(_, b)| b.holder.is_none()).map(|(i, _)| i).collect::<Vec<_>>();
        // Every waiting launch, a lone one too, is checked for a refusal
        // from where its last check (or its let-through) left off.
        let scan = |b: &LaunchBrief| b.holder.is_none() && !b.tool_use_id.is_empty();
        if let Some(from) = q.iter().filter(|b| scan(b)).map(|b| b.offset).min() {
            let ids: Vec<&str> = q.iter().filter(|b| scan(b)).map(|b| b.tool_use_id.as_str()).collect();
            let (refused, next) = crate::orchestrate::refused_launches(Path::new(&ev.transcript_path), from, &ids);
            for b in q.iter_mut().filter(|b| scan(b)) {
                b.offset = b.offset.max(next);
            }
            if !refused.is_empty() {
                q.retain(|b| !refused.contains(&b.tool_use_id));
                for t in &refused {
                    self.orch.forget_launch(t);
                }
            }
        }
        let w = waiting(q);
        let i = match w.len() {
            0 => return None,
            1 => w[0],
            _ => match w.into_iter().filter(|&i| q[i].agent_type == ev.agent_type).collect::<Vec<_>>()[..] {
                [i] => i,
                _ => return None,
            },
        };
        if q[i].tool_use_id.is_empty() {
            return q.remove(i);
        }
        q[i].holder = Some(ev.agent_id.clone());
        Some(q[i].clone())
    }

    /// Reads the meta files of `session`'s agents whose launch is not yet
    /// confirmed: a launch a meta file names is held by its agent from now
    /// on, and one that agent held by a wrong guess waits again.
    fn settle(&mut self, session: &str, dir: &Path) {
        let open: Vec<String> = self.unsettled.iter().filter(|(_, u)| u.session == session && u.confirmed.is_none()).map(|(a, _)| a.clone()).collect();
        for agent in open {
            let Some(t) = crate::orchestrate::launch_id_of(dir, &agent) else { continue };
            if let Some(q) = self.briefs.get_mut(session) {
                for b in q.iter_mut() {
                    if b.tool_use_id == t {
                        b.holder = Some(agent.clone());
                    } else if b.holder.as_deref() == Some(agent.as_str()) {
                        b.holder = None;
                    }
                }
            }
            if let Some(u) = self.unsettled.get_mut(&agent) {
                u.confirmed = Some(t);
            }
        }
    }

    /// The launch agent `agent` (of `session`) ran from: the one its start
    /// confirmed, else its meta file's.
    fn own_launch(ev: &HookEvent, u: &Unsettled) -> Option<String> {
        u.confirmed.clone().or_else(|| if ev.transcript_path.is_empty() { None } else { crate::orchestrate::launch_id_of(&crate::orchestrate::subagents_dir(&ev.transcript_path), &ev.agent_id) })
    }

    /// The first tool call of a subagent whose start was answered by a guess
    /// or not at all (`u`): what it is told now, in the room `used`
    /// characters of the answer leave. A right guess adds nothing, and a
    /// guess no meta file can check stands. A wrong one is told plainly that
    /// its start's context was another launch's and given its own; the
    /// launch it was given waits again for its own agent.
    fn first_call(&mut self, ev: &HookEvent, mut u: Unsettled, used: usize) -> Result<(String, Vec<Injected>)> {
        let agent = ev.agent_id.as_str();
        let own = Self::own_launch(ev, &u);
        // The meta file is not written yet (a tool call can come before it,
        // journal:ATLAS-HOOK-VALUE:0008): the agent waits for it, for at
        // most FIRST_CALL_TRIES calls, and is answered at the first call
        // that can read it.
        if own.is_none() && u.tries < FIRST_CALL_TRIES && !ev.transcript_path.is_empty() && !crate::orchestrate::meta_written(&crate::orchestrate::subagents_dir(&ev.transcript_path), agent) {
            u.tries += 1;
            self.unsettled.insert(agent.to_string(), u);
            return Ok((String::new(), Vec::new()));
        }
        // With no launch named, the one waiting launch whose brief is the
        // agent's own is its launch, so it is not left waiting.
        let own = own.or_else(|| if u.given.is_none() { self.launch_by_brief(&u.session, &ev.transcript_path, agent) } else { None });
        match (&u.given, &own) {
            (Some(g), Some(t)) if g == t => return Ok(self.drop_launch(&u.session, t)),
            // A guess no meta file can check (none came within the tries,
            // or it names no launch) stands, and its launch leaves the
            // queue. If the guess was wrong, the launch's own agent loses
            // only what its start would have been given: its first tool
            // call answers it from its own transcript, and a Binding block
            // is compiled again, though its sub-lane line is lost.
            (Some(g), None) => return Ok(self.drop_launch(&u.session, g)),
            _ => {}
        }
        if let Some(g) = &u.given {
            if let Some(q) = self.briefs.get_mut(&u.session) {
                for b in q.iter_mut().filter(|b| b.tool_use_id == *g && b.holder.as_deref() == Some(agent)) {
                    b.holder = None;
                }
            }
            // What the start took from the other launch is undone.
            if let Some(run) = self.agent_lane.get_mut(agent) {
                run.lane = None;
            }
            self.injected.insert(agent.to_string(), HashSet::new());
            self.spent.remove(agent);
        }
        let launch = own.as_deref().and_then(|t| self.take_brief(&u.session, Some(t), std::time::Instant::now()));
        let sub_line = self.sub_lane_line(agent, own.as_deref());
        let text = launch.as_ref().map(|l| l.brief.clone()).or_else(|| first_user_text(&subagent_transcript(&ev.transcript_path, agent)));
        let (mut body, mut keys) = (String::new(), Vec::new());
        // The agent's lane, launch key and resources are settled (or a
        // wrong guess undone) here, not at its start: the restart point
        // follows (#221), its session listed again in case the start found
        // none of its files yet and dropped it.
        self.restart_due = true;
        if !ev.transcript_path.is_empty() {
            self.restart_sessions.insert(u.session.clone(), ev.transcript_path.clone());
        }
        if let Some(b) = text {
            self.note_lane(agent, &b);
            self.correct_launch_entry(ev, &u, launch.as_ref(), own.as_deref(), &b);
            (body, keys) = self.subagent_context(agent, &b, launch.and_then(|l| l.binding))?;
        }
        let mut out = String::new();
        if u.given.is_some() && u.told {
            let sub = if u.told_sub { ", and the sub-lane it named" } else { "" };
            out = format!("atlas: correction: the context given at this agent's start was for another launch, not this one; disregard it{sub}.");
        }
        let taken = used + usize::from(used > 0) + out.chars().count() + usize::from(!out.is_empty());
        let rest = self.with_sub_line(body, sub_line, SUBAGENT_CONTEXT_CHARS.saturating_sub(taken));
        if !rest.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&rest);
        }
        Ok((out, keys))
    }

    /// The `launch` entry of an agent whose start guessed another launch, or
    /// took none, made right at its first tool call (#221, the W4c caveat):
    /// the entry its start filed from the wrong launch is retired, and its
    /// own brief `brief` (from its launch, else its transcript) is filed,
    /// so a restart offer names the right brief for the right agent.
    fn correct_launch_entry(&mut self, ev: &HookEvent, u: &Unsettled, launch: Option<&LaunchBrief>, own: Option<&str>, brief: &str) {
        let agent = ev.agent_id.as_str();
        if u.given.is_some() {
            if let Some(old) = self.agent_lane.get_mut(agent).and_then(|r| r.launch.take()) {
                let retired = self.store.writer().and_then(|mut w| {
                    w.retire(&old, "its agent's start was given another launch; corrected at the agent's first tool call")?;
                    w.commit()
                });
                if let Err(e) = retired {
                    eprintln!("atlas hook: launch entry {old} of agent {agent}: {e}");
                }
            }
        }
        if self.agent_lane.get(agent).is_some_and(|r| r.launch.is_some()) {
            return;
        }
        let sub_lane = own.and_then(|t| self.orch.sub_lane(t)).map(str::to_string);
        let (model, description) = launch.map_or(("", ""), |l| (l.model.as_str(), l.description.as_str()));
        let agent_type = launch.map_or("", |l| l.agent_type.as_str());
        let transcript = ev.transcript_path.clone();
        let l = LaunchEntry { agent, agent_type, model, description, brief, session: &u.session, transcript: &transcript };
        match self.file_launch(&l, sub_lane.as_deref()) {
            Ok(key) => {
                if let Some(run) = self.agent_lane.get_mut(agent) {
                    run.launch = Some(key);
                }
            }
            Err(e) => eprintln!("atlas hook: launch brief of agent {agent}: {e}"),
        }
    }

    /// The tool use id of the one launch waiting in `session` whose brief is
    /// agent `agent`'s first message (its transcript's); `None` when no
    /// transcript is there yet, or none or several match.
    fn launch_by_brief(&self, session: &str, transcript: &str, agent: &str) -> Option<String> {
        if transcript.is_empty() {
            return None;
        }
        let text = first_user_text(&subagent_transcript(transcript, agent))?;
        let q = self.briefs.get(session)?;
        match q.iter().filter(|b| b.holder.is_none() && !b.tool_use_id.is_empty() && b.brief.trim() == text.trim()).collect::<Vec<_>>()[..] {
            [b] => Some(b.tool_use_id.clone()),
            _ => None,
        }
    }

    /// Takes launch `tool_use_id` off `session`'s queue: its agent is
    /// confirmed. Nothing to tell.
    fn drop_launch(&mut self, session: &str, tool_use_id: &str) -> (String, Vec<Injected>) {
        if let Some(q) = self.briefs.get_mut(session) {
            q.retain(|b| b.tool_use_id != tool_use_id);
        }
        (String::new(), Vec::new())
    }

    /// A subagent that stopped before its first tool call: its launch (by
    /// its meta file, else the one its start was given) leaves the queue,
    /// and any other it held waits again.
    fn end_unsettled(&mut self, ev: &HookEvent) {
        let Some(u) = self.unsettled.remove(&ev.agent_id) else { return };
        let own = Self::own_launch(ev, &u).or(u.given);
        if let Some(q) = self.briefs.get_mut(&u.session) {
            q.retain(|b| own.as_deref() != Some(b.tool_use_id.as_str()));
            for b in q.iter_mut().filter(|b| b.holder.as_deref() == Some(ev.agent_id.as_str())) {
                b.holder = None;
            }
        }
    }

    /// What a subagent with brief `brief` is given from the store
    /// (`recall.subagent_context`): today's recall, its launch's Binding
    /// block (`stashed`, else compiled now), or nothing.
    fn subagent_context(&mut self, agent_id: &str, brief: &str, stashed: Option<String>) -> Result<(String, Vec<Injected>)> {
        match self.settings.recall.subagent_context {
            SubagentContext::Recall => self.recall(agent_id, brief),
            SubagentContext::Off => Ok((String::new(), Vec::new())),
            SubagentContext::Binding => {
                let block = stashed.or_else(|| self.binding_for(brief)).unwrap_or_default();
                let keys = binding_keys(&block);
                Ok((block, keys))
            }
        }
    }

    /// The Binding block of `brief`, with `recall.subagent_context =
    /// "binding"` and a compiler set, for its declared lane ([`binding_lane`],
    /// `""` for none); `None` otherwise or when empty. A compiler error is
    /// logged and gives no block.
    fn binding_for(&self, brief: &str) -> Option<String> {
        if self.settings.recall.subagent_context != SubagentContext::Binding {
            return None;
        }
        let f = self.binding.as_ref()?;
        // With no lane, the keys the brief names are still bound.
        let lane = binding_lane(brief).unwrap_or_default();
        match f(&self.store, brief, &lane) {
            Ok(b) => (!b.is_empty()).then_some(b),
            Err(e) => {
                eprintln!("atlas hook: binding for lane {lane}: {e}");
                None
            }
        }
    }

    fn note_lane(&mut self, agent_id: &str, brief: &str) {
        let lane = brief_lane(brief, &self.known_lanes());
        let run = self.agent_lane.entry(agent_id.to_string()).or_insert_with(|| AgentRun { start: crate::memory::now(), ..AgentRun::default() });
        if run.lane.is_none() {
            run.lane = lane;
        }
        run.uses = crate::lanes::declared(brief).uses.unwrap_or_default();
    }

    /// Files an agent's launch brief as a `launch` journal entry (#220, ruling
    /// journal:ATLAS-RELEASE:0066 Q5) on its lane: the sub-lane it was given,
    /// else the brief's `Lane:` line, else `agent:<id>`. The brief goes
    /// through [`crate::redact::redact`] first. When an active `launch` entry
    /// in the lane or its sub-lanes already holds the same text, the entry is
    /// one line naming it. Never a `brief` entry: a lane with one gives a
    /// second writer no sub-lane ([`Hooks::assign_sub_lane`]).
    fn file_launch(&mut self, l: &LaunchEntry<'_>, sub_lane: Option<&str>) -> Result<String> {
        let agent = l.agent.strip_prefix("agent-").unwrap_or(l.agent);
        let lane = match sub_lane {
            Some(s) => s.to_string(),
            None => crate::lanes::declared(l.brief).lane.unwrap_or_else(|| format!("agent:{agent}")),
        };
        let text = crate::redact::redact(l.brief);
        let same = self
            .store
            .lane_family(crate::lanes::parent_lane(&lane))?
            .into_iter()
            .find(|h| h.kind == "journal" && h.sub == LAUNCH_ENTRY && h.status == "active" && h.body == text)
            .map(|h| h.key);
        let description = crate::redact::redact(l.description.trim());
        let model = if l.model.is_empty() { "inherit" } else { l.model };
        let server = self.server.as_deref().map_or(String::new(), |t| format!("server: {t}\n"));
        let body = match &same {
            Some(k) => format!("The same brief as {k}; recall {k} for its text.\n"),
            None => text,
        };
        let d = crate::memory::Draft {
            title: if description.is_empty() { format!("launch of agent {agent}") } else { format!("launch: {description}") },
            meta: format!(
                "agent: {agent}\ntype: {}\nmodel: {model}\ndescription: {description}\nstart: {}\nsession: {}\ntranscript: {}\n{server}",
                l.agent_type,
                crate::memory::now(),
                l.session,
                l.transcript
            ),
            body,
            reason: "an agent's launch brief, filed by the SubagentStart hook".into(),
            author: "hook".into(),
            client: "atlas-hook".into(),
            provenance: None,
            time: None,
        };
        let mut w = self.store.writer()?;
        let key = w.next_key(&format!("journal:{lane}:"), 4)?;
        w.create(&key, "journal", LAUNCH_ENTRY, &lane, &d, None)?;
        w.add_refs(&key, &[("lane".to_string(), lane.clone())])?;
        w.commit()?;
        Ok(key)
    }

    /// A `<task-notification>`: when a subagent really finished (not an
    /// interim stop while its own background work runs) and wrote no
    /// journal entry since it started, one line says so. The lane is the
    /// one the agent wrote under (its own journal or handoff calls), else
    /// the one its brief names; with neither, nothing is said.
    fn notification(&mut self, prompt: &str, transcript: &str) -> Result<String> {
        let id = prompt.split("<task-id>").nth(1).and_then(|r| r.split("</task-id>").next()).unwrap_or("").trim().to_string();
        if !id.is_empty() && is_interim(prompt) {
            // The agent waits on its own background work: the finish its
            // SubagentStop recorded is undone, in memory and in the store, so
            // its lane and slot stay held through the wait (#207 review S1).
            self.orch.unfinished(&id);
            if let Err(e) = crate::orchestrate::clear_finish(&self.store, &id) {
                eprintln!("atlas agent cap: release record of {id}: {e}");
            }
        }
        if id.is_empty() || is_interim(prompt) {
            // A goal check-in has no task id; an interim stop gets nothing.
            let checkin = id.is_empty() && prompt.contains("Goal check-in:");
            return Ok(if checkin { self.ideas_line() } else { String::new() });
        }
        self.orch.finished(&id, std::time::SystemTime::now());
        let fresh = self.fresh_line(&id);
        let rest = self.journal_check(&id, transcript)?;
        // A slot may have freed: the launches waiting for one.
        // Scheduling reads declarations only, so no known lanes are needed.
        let queued = match crate::queue::items(&self.store, &crate::memory::now()) {
            Ok((heads, items)) if !items.is_empty() => {
                let running = self.holders(transcript, &[]);
                let p = crate::queue::propose(&self.settings.orchestrate, &items, &running);
                crate::queue::proposal_line(&items, &heads, &p, 3)
            }
            Ok(_) => String::new(),
            Err(e) => {
                eprintln!("atlas queue: {e}");
                String::new()
            }
        };
        Ok(fresh + &rest + &queued + &self.ideas_line())
    }

    /// The PR event notices for a main session's event that reaches its own
    /// model ([`NOTICE_EVENTS`]): one line per event note filed since the
    /// session last looked that woke a waiting item. A session's first event
    /// starts it at the store's newest record (it is told what lands while it
    /// lives); an unchanged store stamp reads nothing. Errors tell nothing.
    fn pr_notice(&mut self, ev: &HookEvent) -> Option<String> {
        if !ev.agent_id.is_empty() || ev.session_id.is_empty() || !NOTICE_EVENTS.contains(&ev.event.as_str()) {
            return None;
        }
        let r = (|| -> Result<Option<String>> {
            let stamp = self.store.stamp()?;
            let after = match self.pr_seen.get(&ev.session_id) {
                None => {
                    let top = self.store.max_rid()?;
                    self.pr_seen.insert(ev.session_id.clone(), (stamp, top));
                    return Ok(None);
                }
                Some((s, _)) if *s == stamp => return Ok(None),
                Some((_, after)) => *after,
            };
            let (lines, next) = crate::pr_events::notices_after(&self.store, after)?;
            self.pr_seen.insert(ev.session_id.clone(), (stamp, next));
            Ok((!lines.is_empty()).then(|| lines.join("\n")))
        })();
        r.unwrap_or_else(|e| {
            eprintln!("atlas pr-events notice: {e}");
            None
        })
    }

    /// The pending ideas' line for a check-in or a task notification, when
    /// this session has not seen the newest of them.
    fn ideas_line(&mut self) -> String {
        let session = self.session.clone();
        self.ideas.as_mut().and_then(|i| i.line(&mut self.store, &session)).unwrap_or_default()
    }

    /// Takes the let-through launch of an agent from `session`'s list: the
    /// one with tool use id `launch_id` (its meta file's), else, with no id
    /// known, the oldest one waiting. Launches let through
    /// [`crate::orchestrate::LAUNCH_HOLD_TTL`] or longer before `now` are
    /// dropped first (a backstop: a launch leaves when its agent is
    /// confirmed or its refusal is seen, and one can wait at the permission
    /// prompt far past the pending TTL).
    fn take_brief(&mut self, session: &str, launch_id: Option<&str>, now: std::time::Instant) -> Option<LaunchBrief> {
        let q = self.briefs.get_mut(session)?;
        q.retain(|b| now.saturating_duration_since(b.at) < crate::orchestrate::LAUNCH_HOLD_TTL);
        let waiting_unnamed = |b: &LaunchBrief| b.tool_use_id.is_empty() && b.holder.is_none();
        let i = match launch_id {
            // A hook input with no tool use id records none: such a launch
            // is taken in order.
            Some(t) => q.iter().position(|b| b.tool_use_id == t).or_else(|| q.iter().position(waiting_unnamed)),
            None => q.iter().position(|b| b.holder.is_none()),
        };
        q.remove(i?)
    }

    /// The session's running agents, and the launches admitted that have not
    /// started, as lane and resource holders (none without a transcript or
    /// with the caps off). A running agent's lane is the sub-lane it was
    /// given. The list the cap's admission just read is reused.
    fn holders(&mut self, transcript: &str, known: &[String]) -> Vec<crate::lanes::Holder> {
        if transcript.is_empty() || !self.settings.orchestrate.enabled {
            return Vec::new();
        }
        let dir = crate::orchestrate::subagents_dir(transcript);
        let running = match self.orch.take_seen(&dir) {
            Some(r) => Ok(r),
            None => self.orch.running(&self.settings.orchestrate, &dir, Some(&self.store), std::time::SystemTime::now()),
        };
        match running {
            Ok(r) => {
                let orch = &self.orch;
                let mut hs =
                    crate::queue::holders(&r, &dir, known, &self.settings.orchestrate.lanes, &|t| orch.sub_lane(t).map(str::to_string));
                hs.extend(orch.pending_holders(&r));
                hs
            }
            Err(e) => {
                eprintln!("atlas queue: {}: {e}", dir.display());
                Vec::new()
            }
        }
    }

    /// The launch queue around an Agent launch the cap decided on: a denied
    /// launch is filed (and the reason names its key), an admitted one that
    /// was waiting leaves the queue. Errors file nothing and change no
    /// decision.
    /// `kind_refused`: the deny in `a`, if any, is the kind cap's.
    /// The task judge at an admitted launch: one advisory context line when
    /// the launch's model or its `Judge:` line differs from the pick, and a
    /// line in the pick log. Never denies and never changes the input; with
    /// no readable cells file it does nothing.
    fn judge_launch(&mut self, ev: &HookEvent, brief: &str, a: &mut HookAnswer) {
        let Some(path) = self.settings.orchestrate.judge.cells.as_deref() else { return };
        let cells = match crate::taskjudge::Cells::load(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("atlas judge: {e}");
                return;
            }
        };
        let f = |k: &str| ev.tool_input.get(k).and_then(Value::as_str).unwrap_or("");
        let kind = crate::taskjudge::launch_kind(brief, f("subagent_type"), f("description"));
        let pick = crate::taskjudge::judge(&cells, brief, kind);
        if let Some(line) = crate::taskjudge::note(&pick, f("model"), brief) {
            if !a.context.is_empty() {
                a.context.push('\n');
            }
            a.context.push_str(&line);
        }
        let stated = crate::taskjudge::stated(brief).map(|(m, e)| format!("{m}, {e}")).unwrap_or_default();
        let row = crate::json::object(256, |w| {
            w.field("time", crate::memory::now().as_str())
                .field("tool_use_id", ev.tool_use_id.as_str())
                .field("description", f("description"))
                .field("kind", kind)
                .field("scope", pick.scope)
                .field("unknowns", pick.unknowns)
                .field("model", pick.model)
                .field("effort", pick.effort)
                .field("split", &pick.split)
                .field("source", pick.source)
                .field("asked_model", f("model"))
                .field("judge_line", stated.as_str());
        });
        if let Some(dir) = self.settings.store.parent() {
            let log = dir.join("judge-picks.jsonl");
            let r = std::fs::OpenOptions::new().create(true).append(true).open(&log).and_then(|mut fh| std::io::Write::write_all(&mut fh, format!("{row}\n").as_bytes()));
            if let Err(e) = r {
                eprintln!("atlas judge log {}: {e}", log.display());
            }
        }
    }

    /// The work classifier at an admitted launch that names a brief: one
    /// advisory context line with the demand score and the rules map's pick.
    /// Never denies and never changes the input. With the head source the
    /// hook adds nothing: a warm embed of one brief measured far over the
    /// 500 ms launch budget, so only `judge_task` runs the head.
    fn classify_launch(&mut self, ev: &HookEvent, brief: &str, a: &mut HookAnswer) {
        let s = &self.settings.orchestrate.judge.classifier;
        if s.source == crate::taskjudge::ScoreSource::Head {
            return;
        }
        let f = |k: &str| ev.tool_input.get(k).and_then(Value::as_str).unwrap_or("");
        let kind = crate::taskjudge::launch_kind(brief, f("subagent_type"), f("description"));
        if let Some(p) = crate::taskjudge::classify(s, brief, kind, None, &mut crate::embed::bert::Scratch::default()) {
            if !a.context.is_empty() {
                a.context.push('\n');
            }
            a.context.push_str(&crate::taskjudge::class_line(&p));
        }
    }

    /// An admitted writer launch into a declared lane that a running writer
    /// holds, when the lane has no `brief` journal entry of its own, runs as
    /// the first free sub-lane (`NAME/2`, `NAME/3`, ...;
    /// [`crate::lanes::sub_lane`]). Its prompt is never changed: a changed
    /// input with no permission decision can be denied in auto mode. The
    /// caller records the sub-lane by tool use id, and the agent is told it
    /// at its SubagentStart (a resident server only: a command hook keeps no
    /// record, and the line says so). A read-only launch neither takes a sub-lane nor
    /// makes one ([`crate::lanes::held_lane`]). A lane with a brief is
    /// planned work that continues there, and keeps its lane. Records in a
    /// sub-lane show under the parent in `recall lane=NAME` and
    /// `atlas queue --lane NAME`. Returns the sub-lane and the line for the
    /// orchestrator.
    fn assign_sub_lane(
        &self,
        kind: crate::orchestrate::Kind,
        class: &crate::lanes::Class,
        running: &[crate::lanes::Holder],
        known: &[String],
        tool_use_id: &str,
    ) -> Option<(String, String)> {
        use crate::lanes::{held_lane, Source};
        let lane = held_lane(kind, class).map(String::as_str).filter(|_| class.lane_src == Source::Declared)?;
        let held: Vec<&crate::lanes::Holder> =
            running.iter().filter(|h| h.class.lane_src == Source::Declared && held_lane(h.kind, &h.class).is_some()).collect();
        let lanes: Vec<&str> = held.iter().filter_map(|h| h.class.lane.as_deref()).collect();
        let sub = crate::lanes::sub_lane(lane, &lanes)?;
        // A brief filed under any spelling of the lane keeps it.
        let briefed = |l: &str| match self.store.lane(l) {
            Ok(hs) => Some(hs.iter().any(|h| h.kind == "journal" && h.sub == "brief" && h.status == "active")),
            Err(e) => {
                eprintln!("atlas queue: {e}");
                None
            }
        };
        let mut spellings: Vec<&str> = known.iter().map(String::as_str).filter(|l| crate::lanes::same_lane(l, lane)).collect();
        if !spellings.contains(&lane) {
            spellings.push(lane);
        }
        for l in spellings {
            if briefed(l) != Some(false) {
                return None;
            }
        }
        let holders: Vec<&str> = held
            .iter()
            .filter(|h| h.class.lane.as_deref().is_some_and(|l| crate::lanes::same_lane(l, lane)))
            .map(|h| h.name.as_str())
            .collect();
        // The record that tells the agent lives in this process and is kept
        // by tool use id: a command hook (a process per event) or a launch
        // without an id keeps none, so its agent is never told.
        let told = if self.command_hook || tool_use_id.is_empty() {
            format!("atlas runs as a command hook here (or the launch has no tool use id) and keeps no record of it, so its agent is not told: unless told otherwise it journals under {lane}")
        } else {
            "its agent is told so when it starts".to_string()
        };
        let line = format!(
            "atlas queue: lane {lane} is in use by running work ({}) and has no `brief` journal entry of its own, so this launch runs as sub-lane {sub}; {told} (the prompt is unchanged). Its records show under {lane} in `recall lane={lane}` and `atlas queue --lane {lane}`.",
            holders.join(", ")
        );
        Some((sub, line))
    }

    fn queue_launch(&mut self, ev: &HookEvent, brief: &str, kind_refused: bool, a: &mut HookAnswer) {
        use crate::lanes::Decision;
        if !self.settings.orchestrate.enabled {
            return;
        }
        let f = |k: &str| ev.tool_input.get(k).and_then(Value::as_str).unwrap_or("");
        let description = f("description");
        let (kind, source) = crate::orchestrate::classify_source(&self.settings.orchestrate, brief, f("subagent_type"), f("isolation"));
        let known = self.known_lanes();
        let class = crate::lanes::classify_in(brief, &known, &self.settings.orchestrate.lanes);
        if a.deny.is_none() {
            // Q-QL2, 2026-11-14: a lane name is a label, never a lock. A
            // read-only launch is let through; writers that share a worktree
            // or a repo and branch, and a full capped resource, each declared
            // on both sides, are denied and queued ("Queue them"). One that
            // rests on an inference is let through with a note.
            let running = self.holders(&ev.transcript_path, &known);
            let notes = match crate::lanes::decide(kind, &class, &running, &self.settings.orchestrate.lanes, None) {
                Decision::Conflict(what) => {
                    a.deny = Some(format!(
                        "atlas queue: this launch conflicts with running work ({what}), declared on both sides: two writers must not share a worktree or a branch. Do not retry now; start it when that work finishes."
                    ));
                    Vec::new()
                }
                Decision::Queue(what) => {
                    a.deny = Some(format!("atlas queue: this launch needs a capped resource that running work holds ({what}), declared on both sides. Do not retry now; start it when that work finishes."));
                    Vec::new()
                }
                Decision::Allow(notes) => notes,
            };
            if a.deny.is_none() {
                let mut lines: Vec<String> = Vec::new();
                // Only a launch that holds its declared lane replaces a waiting
                // one there: reviews and read-only launches share the lane.
                let lane =
                    crate::lanes::held_lane(kind, &class).map(String::as_str).filter(|_| class.lane_src == crate::lanes::Source::Declared);
                match crate::queue::started(&mut self.store, description, brief, lane, kind) {
                    Ok(crate::queue::Started::NotWaiting(k)) => {
                        lines.push(format!("atlas queue: the prompt's `Queue: {k}` names no waiting item (`atlas queue` lists them)."))
                    }
                    Ok(crate::queue::Started::Retired(k)) => lines.push(format!("atlas queue: this launch took {k} off the queue.")),
                    Ok(crate::queue::Started::SameLane { key, others }) => lines.push(format!(
                        "atlas queue: this launch took {key} off the queue (the oldest waiting {} launch in its lane){}.",
                        kind.name(),
                        if others > 0 {
                            format!("; {others} more in the lane still wait, so drop any this launch replaces (`atlas queue drop KEY`)")
                        } else {
                            String::new()
                        }
                    )),
                    Ok(crate::queue::Started::Duplicate(d)) => lines.push(format!(
                        "atlas queue: this launch has the same brief as waiting {}, so atlas took {} off the queue.",
                        d.iter().map(|(k, l)| format!("{k} (lane {l})")).collect::<Vec<_>>().join(", "),
                        if d.len() == 1 { "it" } else { "them" }
                    )),
                    Ok(crate::queue::Started::None) => {}
                    Err(e) => eprintln!("atlas queue: {e}"),
                }
                // The sub-lane is kept by tool use id, never written into the
                // prompt; the launch holds its class until its agent starts.
                let sub = match self.assign_sub_lane(kind, &class, &running, &known, &ev.tool_use_id) {
                    Some((sub, line)) => {
                        lines.push(line);
                        Some(sub)
                    }
                    None => None,
                };
                self.orch.hold_last(class.clone(), sub);
                if !notes.is_empty() {
                    lines.push(format!(
                        "atlas queue: this launch may conflict with running work ({}); atlas inferred it, so it was let through. If the conflict is real, stop it and start it when that work finishes; a `Uses:` line in the prompt corrects a wrong inference.",
                        notes.join("; ")
                    ));
                }
                // A builder with no declared lane is let through and the orchestrator
                // reminded; never a deny.
                if kind == crate::orchestrate::Kind::Builder && class.lane_src != crate::lanes::Source::Declared {
                    lines.push(NO_LANE.to_string());
                }
                for line in lines {
                    if !a.context.is_empty() {
                        a.context.push('\n');
                    }
                    a.context.push_str(&line);
                }
                return;
            }
        }
        let meta = class.meta();
        let launch = crate::queue::Launch { kind, lane: class.lane.as_deref(), description, subagent_type: f("subagent_type"), prompt: brief, session: &ev.session_id, class_meta: &meta };
        match crate::queue::add(&mut self.store, &launch) {
            Ok((key, new)) => {
                if let Some(r) = a.deny.as_mut() {
                    r.push_str(&format!(
                        " atlas {} it as {key}: launch it again with the same description and prompt once a slot is free, and it leaves the queue (`atlas queue` lists what waits).",
                        if new { "queued" } else { "has already queued" }
                    ));
                    // A relaunch with a new `Kind:` line is another prompt:
                    // its `Queue:` line keeps it this entry (Q-QU3). Only a
                    // kind-cap refusal is caused by the kind.
                    if kind_refused && source != crate::orchestrate::KindSource::Line {
                        r.push_str(&format!(" If you relaunch it with a `Kind:` line, add `Queue: {key}` too: the entry then takes the new kind and leaves the queue when it starts."));
                    }
                }
            }
            Err(e) => eprintln!("atlas queue: {e}"),
        }
    }

    /// The SubagentStop backstop (Q-SD3, refined 11-04): a report over
    /// [`crate::report::SEND_BACK_OVER`] bytes is sent back once, in
    /// orchestrated mode. atlas files it whole in the agent's lane and asks
    /// for the digest form, without a draft. A shorter report, a second
    /// stop, or a filing error lets the stop through.
    fn report_backstop(&mut self, agent_id: &str, report: &str, already_blocked: bool) -> Option<String> {
        let report = report.trim();
        if !self.settings.orchestrate.enabled || already_blocked || report.len() <= crate::report::SEND_BACK_OVER || self.report_sent_back.contains(agent_id) {
            return None;
        }
        let lane = match self.lane_of(agent_id) {
            l if l.starts_with("agent:") => "reports".to_string(),
            l => l,
        };
        let first = report.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let d = crate::memory::Draft {
            // The library title rule; the report itself is the body.
            title: crate::memory::short_title(&format!("full report of agent {agent_id}: {first}")),
            meta: format!("agent: {agent_id}
bytes: {}", report.len()),
            body: report.to_string(),
            reason: "a report over the digest bound, filed whole by the SubagentStop backstop".into(),
            author: "hook".into(),
            client: "atlas-hook".into(),
            provenance: None,
            time: None,
        };
        let filed = (|| -> Result<String> {
            let mut w = self.store.writer()?;
            let key = w.next_key(&format!("journal:{lane}:"), 4)?;
            w.create(&key, "journal", "note", &lane, &d, None)?;
            w.commit()?;
            Ok(key)
        })();
        let key = match filed {
            Ok(k) => k,
            Err(e) => {
                eprintln!("atlas report backstop: {e}");
                return None;
            }
        };
        self.report_sent_back.insert(agent_id.to_string());
        Some(format!(
            "atlas report backstop: this report is {} bytes; atlas filed it whole as {key}. Hand back the digest form within {} bytes: the outcome first (PR, branch, merge, verdict), then the numbers, every open question under `## Questions`, and every risk (a denied action, an unverified claim, an open failure); cite {key} for the rest. Asked once.",
            report.len(),
            crate::report::WHOLE_UP_TO
        ))
    }

    /// The line that follows a finished agent that passed its handoff point:
    /// start a fresh agent from its handoff (`fresh: auto`) or ask the user
    /// first (`fresh: ask`). Never resume it.
    fn fresh_line(&mut self, id: &str) -> String {
        use crate::orchestrate::Fresh;
        let Some((lane, kind)) = self.orch.take_passed(id) else { return String::new() };
        match self.settings.orchestrate.limits(kind).fresh {
            Fresh::Auto => format!("atlas: agent {id} passed its handoff point; do not resume it. Start a FRESH {} agent from its handoff (recall lane={lane}).\n", kind.name()),
            Fresh::Ask => format!("atlas: agent {id} passed its handoff point; do not resume it. Ask the user before starting a fresh {} agent from its handoff (recall lane={lane}).\n", kind.name()),
        }
    }

    /// The no-journal check of a finished agent (see [`Hooks::notification`]).
    fn journal_check(&mut self, id: &str, transcript: &str) -> Result<String> {
        let id = id.to_string();
        let own = Some(transcript).filter(|t| !t.is_empty()).map(|t| subagent_transcript(t, &id)).filter(|p| p.is_file());
        let mut run = self.agent_lane.get(&id).cloned().unwrap_or_default();
        if let Some(p) = &own {
            for l in written_lanes(p) {
                if !run.wrote.contains(&l) {
                    run.wrote.push(l);
                }
            }
            if run.start.is_empty() {
                run.start = first_timestamp(p).unwrap_or_default();
            }
            if run.lane.is_none() && run.wrote.is_empty() {
                run.lane = first_user_text(p).and_then(|b| brief_lane(&b, &self.known_lanes()));
            }
        }
        // No start: not an agent this server saw start (a background
        // command, or a server restarted since) and no transcript says when.
        if run.start.is_empty() {
            return Ok(String::new());
        }
        let lanes: Vec<String> = if run.wrote.is_empty() { run.lane.into_iter().collect() } else { run.wrote };
        let Some(first) = lanes.first().cloned() else { return Ok(String::new()) };
        for lane in &lanes {
            // The launch entry atlas filed at its start is not the agent's own.
            if self.store.lane(lane)?.iter().any(|h| h.time >= run.start && h.sub != LAUNCH_ENTRY) {
                return Ok(String::new());
            }
        }
        Ok(format!("atlas: agent {id} finished with no journal entry for lane {first} since it started; check its report against the journal.\n"))
    }

    /// Writes a session's checkpoint from its segment `t` (read from `from`)
    /// and the store, as PreCompact does: the key, and whether the chain card
    /// was added. `model` is the model's own checkpoint entry, named in the
    /// body (or said missing) while `compact.checkpoint_prompt` is on.
    #[allow(clippy::too_many_arguments)]
    fn write_checkpoint(
        &mut self,
        session: &str,
        transcript: &str,
        trigger: &str,
        t: &crate::checkpoint::Tail,
        from: &crate::checkpoint::TailFrom,
        model: Option<&crate::memory::Head>,
        t_pre: std::time::Instant,
    ) -> Result<(String, bool)> {
        use crate::checkpoint::TailFrom;
        let ctx = match from {
            TailFrom::Own => if t.last_ctx > 0 { t.last_ctx } else { last_usage(Path::new(transcript)).ok().flatten().map_or(0, |(_, c)| c) },
            TailFrom::Parent(_) => t.last_ctx,
            TailFrom::Unread => 0,
        };
        let lane = format!("session:{session}");
        let scope = crate::checkpoint::Lanes::for_project(self.settings.graph.as_deref());
        // Bounded by the transcript's own newest record: never a record the
        // session could not have seen.
        let state = crate::checkpoint::state(&self.store, session, &t.start, &t.end, scope)?;
        let mut body = crate::checkpoint::body(&state, t, self.settings.bytes_for(self.settings.compact.card_tokens));
        if self.settings.compact.checkpoint_prompt {
            body.push_str(&crate::checkpoint::model_line(model));
        }
        // The chain card: every part of the segment by a group key,
        // then the last links (its own cap, outside the body's).
        let read = match from {
            TailFrom::Parent(p) => p.clone(),
            _ => PathBuf::from(transcript),
        };
        let chain = crate::chain::card(&read);
        if let Some(c) = &chain {
            body.push_str(c);
        }
        // Never a silent 0-context card: say what is missing.
        if matches!(from, TailFrom::Unread) {
            body.push_str(crate::checkpoint::UNREAD_LINE);
            body.push('\n');
        }
        let at = match (ctx, from) {
            (0, TailFrom::Unread) => "transcript unread".to_string(),
            (0, _) => "at unknown context tokens".to_string(),
            _ => format!("at {ctx} context tokens"),
        };
        let tail_from = match from {
            TailFrom::Parent(p) => format!("tail_from: {}\n", p.display()),
            _ => String::new(),
        };
        let d = Draft {
            title: format!("checkpoint ({trigger}) {at}"),
            meta: format!("transcript: {transcript}\n{tail_from}hook_ms: {:.1}\n", t_pre.elapsed().as_secs_f64() * 1e3),
            body,
            reason: format!("context {} checkpoint written by the {} hook", if trigger == "clear" { "clear" } else { "compaction" }, if trigger == "clear" { "SessionEnd" } else { "PreCompact" }),
            author: "hook".into(),
            client: "atlas-hook".into(),
            provenance: None,
            time: None,
        };
        let mut w = self.store.writer()?;
        let key = w.next_key(&format!("journal:{lane}:"), 4)?;
        w.create(&key, "journal", "checkpoint", &lane, &d, None)?;
        w.commit()?;
        Ok((key, chain.is_some()))
    }

    /// At a /clear (SessionEnd, `reason: clear`): the ending session's
    /// checkpoint, written from its own transcript (the event's), before
    /// Claude Code clears it. Nothing when that transcript holds no request.
    fn clear_checkpoint(&mut self, ev: &HookEvent) -> Result<()> {
        let t_pre = std::time::Instant::now();
        if ev.session_id.is_empty() || ev.transcript_path.is_empty() {
            return Ok(());
        }
        let path = Path::new(&ev.transcript_path);
        let t = crate::checkpoint::tail(path, &ev.session_id);
        if t.last_ctx == 0 {
            return Ok(());
        }
        let model = crate::checkpoint::model_entry(&self.store, &ev.session_id, &t.start)?;
        self.write_checkpoint(&ev.session_id, &ev.transcript_path, "clear", &t, &crate::checkpoint::TailFrom::Own, model.as_ref(), t_pre)?;
        Ok(())
    }

    /// At the cleared session's start (SessionStart, `source: clear`): the
    /// card of the checkpoint SessionEnd wrote for the session it replaces
    /// (see [`crate::checkpoint::cleared`]). `None` when there is none.
    fn clear_card(&mut self, ev: &HookEvent) -> Result<Option<String>> {
        let Some(dir) = Path::new(&ev.transcript_path).parent().filter(|d| !d.as_os_str().is_empty()) else { return Ok(None) };
        Ok(crate::checkpoint::cleared(&self.store, dir, &ev.session_id)?.map(|h| crate::checkpoint::card_before(&h, "the /clear")))
    }

    /// The main session's compaction prompts on a tool call or a prompt
    /// (see [`crate::autocompact::MainWatch`]): appended to the context.
    fn main_context(&mut self, ev: &HookEvent, a: &mut HookAnswer) {
        if ev.transcript_path.is_empty() {
            return;
        }
        let path = PathBuf::from(&ev.transcript_path);
        let Some(ctx) = self.context_of(&path) else { return };
        let text = self.main.lines(&self.settings, &path, &ev.session_id, ctx);
        if !text.is_empty() {
            if !a.context.is_empty() {
                a.context.push('\n');
            }
            a.context.push_str(&text);
        }
    }

    /// The context size of a transcript's last request, cached by file length.
    fn context_of(&mut self, path: &Path) -> Option<u64> {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let usage = match self.usage_cache.get(path) {
            Some((l, u)) if *l == len => u.clone(),
            _ => {
                let u = last_usage(path).ok().flatten();
                self.usage_cache.insert(path.to_path_buf(), (len, u.clone()));
                u
            }
        };
        usage.map(|(_, c)| c)
    }

    /// The lane an agent writes under, else the sub-lane it was given or its
    /// brief's, else `agent:<id>`.
    fn lane_of(&self, agent_id: &str) -> String {
        self.agent_lane.get(agent_id).and_then(|r| r.wrote.first().cloned().or_else(|| r.lane.clone())).unwrap_or_else(|| format!("agent:{agent_id}"))
    }

    /// The calling agent's limits: its kind, handoff point, hard limit and
    /// mode. A subagent in orchestrated mode has its kind's; any other (and
    /// every agent with orchestrated mode off) has the plain cap, enforced
    /// unless orchestrated mode says otherwise.
    fn limits_of(&mut self, ev: &HookEvent) -> (crate::orchestrate::Kind, u64, u64, crate::orchestrate::Mode) {
        use crate::orchestrate::{Kind, Mode};
        let sub = !ev.agent_id.is_empty();
        let orchestrated = self.settings.orchestrate.enabled;
        if sub && orchestrated {
            let dir = crate::orchestrate::subagents_dir(&ev.transcript_path);
            let kind = self.orch.kind_of(&self.settings.orchestrate, &dir, &ev.agent_id, &ev.agent_type);
            let lim = self.settings.orchestrate.limits(kind);
            return (kind, lim.handoff, lim.limit, lim.mode);
        }
        (Kind::None, self.settings.cap.warn_tokens, self.settings.cap.block_tokens, if orchestrated { self.settings.orchestrate.other.mode } else { Mode::Enforced })
    }

    /// Review S4 of #192 (ATLAS-CLEAR 0008): whether a subagent's automatic
    /// compaction is blocked, with the reason. Claude Code 2.1.323 runs
    /// PreCompact for a subagent too and skips a compaction a hook blocks;
    /// with the auto-compact window at atlas's hard limit, a subagent would
    /// otherwise be compacted where its hard limit's deny starts, and carry
    /// on from a summary instead of handing off. Blocked when the limit is
    /// enforced, the agent is past its handoff point, and the hard limit is
    /// below Claude Code's own blocking limit on the model's window (else the
    /// agent would fail on a too-long prompt before any deny). Every time:
    /// the next tool call is the deny.
    fn subagent_compaction_block(&mut self, ev: &HookEvent) -> Option<String> {
        if ev.agent_id.is_empty() || ev.trigger != "auto" {
            return None;
        }
        let (_, handoff, limit, mode) = self.limits_of(ev);
        if mode != crate::orchestrate::Mode::Enforced {
            return None;
        }
        let path = subagent_transcript(&ev.transcript_path, &ev.agent_id);
        let ctx = self.context_of(&path)?;
        // Its own model's window (#195, ruling ATLAS-VERSION 0016): the
        // transcript's last real model, else the meta file's `model` alias;
        // neither known, `compact.window`, else the inferred one. The meta
        // file's `pickedEffortModel` is the parent's model and is not read.
        let c = &self.settings.compact;
        let of = |m: Option<String>| m.and_then(|m| crate::autocompact::model_window(&m, &c.model_windows, c.claude.window));
        let window = of(last_model(&path).ok().flatten())
            .or_else(|| of(crate::orchestrate::model_alias_of(&crate::orchestrate::subagents_dir(&ev.transcript_path), &ev.agent_id)))
            .unwrap_or(if c.window > 0 { c.window } else { c.claude.window });
        if ctx < handoff || (window > 0 && limit + 3_000 >= crate::autocompact::effective(window)) {
            return None;
        }
        let lane = self.lane_of(&ev.agent_id);
        Some(format!(
            "atlas context cap: automatic compaction is blocked for this agent ({ctx} context tokens, hard limit {limit}); it hands off instead. Write your handoff with the atlas `handoff` tool now (lane {lane}); a fresh agent continues from it."
        ))
    }

    /// The context limit of the calling subagent (the main session has
    /// [`Hooks::main_context`] instead, advice only): past
    /// the handoff point it is told to wrap up, past the hard limit every
    /// tool but the handoff is denied (mode `enforced`). With orchestrated
    /// mode off this is the plain cap of v0.2 (`cap.warn_tokens` and
    /// `cap.block_tokens`, enforced, for every agent): the per-kind limits
    /// and modes apply only in orchestrated mode.
    fn cap(&mut self, ev: &HookEvent, path: &Path, a: &mut HookAnswer) {
        use crate::orchestrate::Mode;
        let Some(ctx) = self.context_of(path) else { return };
        let sub = !ev.agent_id.is_empty();
        let orchestrated = self.settings.orchestrate.enabled;
        let (kind, handoff, limit, mode) = self.limits_of(ev);
        if mode == Mode::Off || ctx < handoff {
            return;
        }
        let every = self.settings.cap.warn_every;
        let lane = self.lane_of(&ev.agent_id);
        if sub && orchestrated {
            self.orch.passed(&ev.agent_id, &lane, kind);
        }
        let what = match (sub, orchestrated) {
            (false, _) => "main session".to_string(),
            (true, true) => format!("{} limits", kind.name()),
            (true, false) => "cap".to_string(),
        };
        if ctx >= limit && mode == Mode::Enforced {
            if !passes_cap(&ev.tool_name) {
                a.deny = Some(format!(
                    "atlas context cap: this agent's context is {ctx} tokens (hard limit {limit}, {what}). Every tool is blocked except the atlas `handoff` tool: write your handoff now (lane {lane}); a fresh agent continues from it."
                ));
                if self.warned.get(path).copied().unwrap_or(0) < limit {
                    self.warned.insert(path.to_path_buf(), limit);
                    self.cap_entry(&lane, ctx, "blocked");
                }
            }
            return;
        }
        let last = self.warned.get(path).copied().unwrap_or(0);
        if last == 0 || ctx >= last + every {
            self.warned.insert(path.to_path_buf(), ctx);
            a.context = if ctx >= limit {
                format!(
                    "atlas context cap: this agent's context is {ctx} tokens, past its hard limit {limit} ({what}, advisory). Stop new work: commit, push, and write your handoff with the atlas `handoff` tool now (lane {lane}); a fresh agent continues from it."
                )
            } else {
                format!(
                    "atlas context cap: this agent's context is {ctx} tokens (handoff point {handoff}, hard limit {limit}, {what}). Wrap up at your next clean stopping point: finish the current step, commit, push, and write your handoff with the atlas `handoff` tool (lane {lane}). A fresh agent continues from it."
                )
            };
            if last == 0 {
                self.cap_entry(&lane, ctx, "warned");
            }
        }
    }

    /// The required-reviews merge gate (`orchestrate.reviews`): a shell
    /// command with a `gh pr merge` whose PR lacks a review of its head
    /// from each role is denied (enforced) or warned about (advisory).
    /// Fails open on a store error.
    fn review_gate(&mut self, ev: &HookEvent, a: &mut HookAnswer) {
        let s = &self.settings.orchestrate.reviews;
        let command = ev.tool_input.get("command").and_then(Value::as_str).unwrap_or("");
        if !s.on() {
            return;
        }
        let (head_of, cwd) = (self.pr_head, if ev.cwd.is_empty() { self.cwd.as_str() } else { ev.cwd.as_str() });
        match crate::reviews::gate(&self.store, s, command, &|m| head_of(m, cwd)) {
            Ok(Some(g)) if g.deny => a.deny = Some(g.message),
            Ok(Some(g)) => {
                if !a.context.is_empty() {
                    a.context.push('\n');
                }
                a.context.push_str(&g.message);
            }
            Ok(None) => {}
            Err(e) => self.router.miss_text("review gate", &e),
        }
    }

    /// A SendMessage to an agent: refused past the agent's hard limit
    /// (mode `enforced`), and past its handoff point the sender is told to
    /// send a wrap-up only. An agent named rather than given by id, or
    /// without a transcript, gives no decision.
    fn resume_gate(&mut self, ev: &HookEvent, a: &mut HookAnswer) {
        use crate::orchestrate::Mode;
        let to = ev.tool_input.get("to").and_then(Value::as_str).unwrap_or("").trim();
        if !self.settings.orchestrate.enabled || to.is_empty() || ev.transcript_path.is_empty() {
            return;
        }
        let id = to.strip_prefix("agent-").unwrap_or(to).to_string();
        // An id is a file-name part; anything else (a path) is not looked up.
        if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return;
        }
        let dir = crate::orchestrate::subagents_dir(&ev.transcript_path);
        let path = dir.join(format!("agent-{id}.jsonl"));
        if !path.is_file() {
            return;
        }
        let Some(ctx) = self.context_of(&path) else { return };
        let kind = self.orch.kind_of(&self.settings.orchestrate, &dir, &id, "");
        let lim = self.settings.orchestrate.limits(kind).clone();
        if lim.mode == Mode::Off || ctx < lim.handoff {
            return;
        }
        let lane = self.lane_of(&id);
        if ctx >= lim.limit && lim.mode == Mode::Enforced {
            a.deny = Some(format!(
                "atlas context cap: agent {id} is at {ctx} context tokens, past its hard limit {} ({} limits). Do not resume it: start a fresh agent from its handoff and the lane {lane} journal.",
                lim.limit,
                kind.name()
            ));
        } else {
            a.context = format!(
                "atlas context cap: agent {id} is past its handoff point ({ctx} of {} tokens, hard limit {}). Send it a wrap-up message only; new work goes to a fresh agent started from its handoff (lane {lane}).",
                lim.handoff, lim.limit
            );
        }
    }

    /// Files the cap's journal entry. A write error is logged, never
    /// returned: the deny or warning it records is already decided and
    /// must not be dropped with it (ORCH-RETRO F3).
    fn cap_entry(&mut self, lane: &str, ctx: u64, what: &str) {
        if let Err(e) = self.write_cap_entry(lane, ctx, what) {
            eprintln!("atlas context cap: journal entry for lane {lane}: {e}");
        }
    }

    fn write_cap_entry(&mut self, lane: &str, ctx: u64, what: &str) -> Result<()> {
        let d = Draft {
            title: format!("context cap {what} at {ctx} tokens"),
            meta: String::new(),
            body: String::new(),
            reason: format!("context cap {what}, recorded by the PreToolUse hook"),
            author: "hook".into(),
            client: "atlas-hook".into(),
            provenance: None,
            time: None,
        };
        let mut w = self.store.writer()?;
        let key = w.next_key(&format!("journal:{lane}:"), 4)?;
        w.create(&key, "journal", "cap", lane, &d, None)?;
        w.commit()
    }
}

/// The token-log thread: one row per hook answer in the index database's
/// `token_log`, with the counterfactual computed by a fixed rule
/// ([`crate::tokens::record_read`]): for each injected memory, its whole
/// file (meta and body); for each other record, its text plus 40 lines
/// either side of it in its export (the window a reader opens); for a lane,
/// every record of the lane. A subagent start also logs the size of the
/// rulebook at that moment to `brief_log`: the agents the report counts and
/// its "if read whole" line. The "brief without atlas" itself is measured
/// from pre-atlas reads ([`crate::tokens::brief_measured`], #219).
fn start_log(s: &Settings) -> Option<(mpsc::Sender<LogRow>, std::thread::JoinHandle<()>)> {
    let db = s.index_db.clone();
    let store = s.store.clone();
    let reference = s.brief_reference.clone();
    let (tx, rx) = mpsc::channel::<LogRow>();
    let handle = std::thread::Builder::new()
        .name("atlas-hook-log".into())
        .spawn(move || {
            // Nothing is opened until the first row, so a server's start-up
            // does not contend with this thread for the database.
            let Ok(first) = rx.recv() else { return };
            let Ok(conn) = crate::store::open_raw(&db) else { return };
            let _ = conn.busy_timeout(std::time::Duration::from_secs(30));
            if conn.execute_batch(crate::tokens::TOKEN_TABLE).is_err() {
                return;
            }
            let Ok(mem) = MemoryStore::open(&store) else { return };
            for r in std::iter::once(first).chain(rx) {
                if let Some(m) = &r.miss {
                    let _ = crate::compact::log_miss(&conn, m);
                }
                if r.tool.is_empty() {
                    continue;
                }
                let reads: Vec<usize> = r.keys.iter().map(|k| crate::tokens::record_read(&mem, &k.key)).collect();
                let cf = (!r.keys.is_empty()).then_some(crate::tokens::Counterfactual { grep: 0, read: reads.iter().sum() });
                let _ = crate::tokens::log_row(&conn, &r.tool, r.returned, cf);
                let hook = r.tool.strip_prefix("hook:").unwrap_or(&r.tool);
                for (k, read) in r.keys.iter().zip(&reads) {
                    let _ = crate::hook_hits::log_injection(&conn, hook, &r.session, &r.agent, &r.transcript, k, *read);
                    let _ = crate::tokens::log_select(&conn, hook, &k.key, k.how.name(), k.bytes, k.full);
                }
                if let (Some((session, agent)), Some(p)) = (&r.brief, &reference) {
                    if let Ok(m) = std::fs::metadata(p) {
                        let _ = crate::tokens::log_brief(&conn, session, agent, m.len());
                    }
                }
            }
        })
        .ok()?;
    Some((tx, handle))
}

impl Drop for Hooks {
    /// Closes the token log and waits for its thread to write the rows
    /// queued so far (a command hook exits right after its answer).
    fn drop(&mut self) {
        self.log = None;
        if let Some(t) = self.log_thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #253: a panic inside the guard answers empty, records exactly one
    /// miss, and does not escape; a body that does not panic is untouched.
    #[test]
    fn a_panic_in_the_guard_answers_empty_and_is_one_miss() {
        let dir = std::env::temp_dir().join(format!("atlas-hook-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vault = dir.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let db = dir.join("vaults.sqlite");
        drop(crate::store::Store::open(&db, crate::paths::Vaults::new(&[("v".into(), vault)]).unwrap()).unwrap());
        let settings = Settings::from_config(None, &db).unwrap();
        let ev = HookEvent { event: "PreToolUse".into(), tool_name: "Read".into(), ..HookEvent::default() };
        let (text, panicked) = guard(&settings, &ev, || -> String { panic!("boom") });
        assert_eq!((text.as_str(), panicked), ("", true));
        let (text, panicked) = guard(&settings, &ev, || "kept".to_string());
        assert_eq!((text.as_str(), panicked), ("kept", false));
        let c = rusqlite::Connection::open(&db).unwrap();
        let rows: Vec<(String, String, String, String)> = c
            .prepare("SELECT tool, query, reason, client FROM misses")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let want = [("route".to_string(), "hook panic PreToolUse Read".to_string(), "boom".to_string(), "atlas-hook".to_string())];
        assert_eq!(rows, want);
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derives_the_subagent_transcript() {
        let p = subagent_transcript("/h/p/abc.jsonl", "a1b2");
        assert_eq!(p, Path::new("/h/p/abc").join("subagents").join("agent-a1b2.jsonl"));
        assert_eq!(subagent_transcript("/h/p/abc.jsonl", "agent-a1b2"), p);
    }

    #[test]
    fn a_model_call_of_the_hook_tool_never_takes_the_update_notice() {
        // The transport marks the call from its `_meta`, over anything the
        // caller wrote under the reserved argument.
        let args = crate::json!({"event": "UserPromptSubmit", "session_id": "s", "prompt": "p", "_tool_use": false});
        let model = HookEvent::from_value(&mark_tool_use(&args, Some("toolu_1")));
        assert!(model.tool_use);
        assert!(!takes_live_notice(&model), "a model's call");
        let hook = HookEvent::from_value(&mark_tool_use(&crate::json!({"event": "UserPromptSubmit", "_tool_use": true}), None));
        assert!(!hook.tool_use);
        assert!(takes_live_notice(&hook), "a real hook");
        // A command hook's input has no mark.
        assert!(takes_live_notice(&HookEvent::from_value(&crate::json!({"hook_event_name": "UserPromptSubmit"}))));
        assert!(!takes_live_notice(&HookEvent::from_value(&crate::json!({"hook_event_name": "Stop"}))));
    }

    #[test]
    fn the_update_notice_is_for_the_user_only() {
        let a = HookAnswer { system_message: "atlas 0.2.0 ready: /mcp to reconnect".into(), ..HookAnswer::default() };
        assert_eq!(a.to_json("UserPromptSubmit"), r#"{"systemMessage":"atlas 0.2.0 ready: /mcp to reconnect"}"#);
        let a = HookAnswer { context: "x".into(), ..a };
        let j = a.to_json("SessionStart");
        let v = crate::json::parse(&j).unwrap();
        assert_eq!(v.get("systemMessage").and_then(Value::as_str), Some("atlas 0.2.0 ready: /mcp to reconnect"));
        assert_eq!(v.pointer("/hookSpecificOutput/additionalContext").and_then(Value::as_str), Some("x"), "the notice is not in the context");
    }

    #[test]
    fn answers_are_hook_json_or_nothing() {
        assert_eq!(HookAnswer::default().to_json("UserPromptSubmit"), "");
        let a = HookAnswer { context: "x".into(), ..HookAnswer::default() };
        assert_eq!(a.to_json("SubagentStart"), r#"{"hookSpecificOutput":{"hookEventName":"SubagentStart","additionalContext":"x"}}"#);
        let d = HookAnswer { deny: Some("no".into()), ..HookAnswer::default() };
        assert_eq!(d.to_json("PreToolUse"), r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no"}}"#);
        let o = HookAnswer { updated_output: Some(crate::json!({"stdout": "s"})), ..HookAnswer::default() };
        assert_eq!(o.to_json("PostToolUse"), r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":{"stdout":"s"}}}"#);
        let i = HookAnswer { updated_input: Some(crate::json!({"command": "c"})), ..HookAnswer::default() };
        assert_eq!(i.to_json("PreToolUse"), r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"c"}}}"#);
    }

    #[test]
    fn reads_tool_objects_from_either_transport() {
        // An mcp_tool hook substitutes an object as its JSON text; a command
        // hook gets the object itself. An unsubstituted or non-object value
        // is Null.
        let text = crate::json!({"event": "PostToolUse", "tool_input": "{\"command\":\"ls\"}", "tool_response": "${tool_response}"});
        let ev = HookEvent::from_value(&text);
        assert_eq!(ev.tool_input, crate::json!({"command": "ls"}));
        assert_eq!(ev.tool_response, Value::Null);
        let obj = crate::json!({"hook_event_name": "PostToolUse", "tool_response": {"stdout": "x"}, "cwd": "/w"});
        let ev = HookEvent::from_value(&obj);
        assert_eq!(ev.event, "PostToolUse");
        assert_eq!(ev.tool_response, crate::json!({"stdout": "x"}));
        assert_eq!(ev.cwd, "/w");
    }

    #[test]
    fn reads_the_last_usage_from_the_end() {
        let dir = std::env::temp_dir().join(format!("atlas-usage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let mut text = String::new();
        let rec = |id: &str, i: u64| {
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"id\":\"{id}\",\"usage\":{{\"input_tokens\":2,\"cache_creation_input_tokens\":{i},\"cache_read_input_tokens\":100,\"output_tokens\":5}}}}}}\n"
            )
        };
        text.push_str(&rec("m1", 10));
        // Padding past one read step, so the scan crosses a chunk boundary.
        for _ in 0..2000 {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"padding padding padding padding padding\"}}\n");
        }
        text.push_str(&rec("m2", 1000));
        text.push_str(&rec("m2", 1000));
        text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"tool result\"}}\n");
        std::fs::write(&p, &text).unwrap();
        assert_eq!(last_usage(&p).unwrap(), Some(("m2".to_string(), 1107)));
        std::fs::write(&p, rec("m1", 10) + &"{\"type\":\"user\"}\n".repeat(3000)).unwrap();
        assert_eq!(last_usage(&p).unwrap(), Some(("m1".to_string(), 117)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lane_is_named_by_its_lane_line_or_its_exact_id() {
        let named = |text: &str, lane: &str| names_lane(text, crate::lanes::declared(text).lane.as_deref(), lane);
        // The bare word is not the `session` lane; its `Lane:` line is.
        assert!(!named("this session continues the work", "session"));
        assert!(named("Kind: builder\nLane: session\n", "session"));
        // A lower-case lane only by its `Lane:` line, in any case.
        assert!(!named("see build-7 for context", "build-7"));
        assert!(named("- **Lane:** `BUILD-7`", "build-7"));
        // An id: exact, case-sensitive, a whole token.
        assert!(named("continue ATLAS-BO0.", "ATLAS-BO0"));
        assert!(named("journal:ATLAS-BO0:0001", "ATLAS-BO0"));
        assert!(!named("continue atlas-bo0", "ATLAS-BO0"));
        assert!(!named("continue ATLAS-BO0-2", "ATLAS-BO0"));
        assert!(!named("continue XATLAS-BO0", "ATLAS-BO0"));
        assert!(!named("continue ATLAS-BO0_X", "ATLAS-BO0"));
        assert!(named("ATLAS-BO0-2 and ATLAS-BO0", "ATLAS-BO0"));
        // A sub-lane names itself, not its parent (whose view holds it).
        assert!(named("continue ATLAS-BO0/2.", "ATLAS-BO0/2"));
        assert!(!named("continue ATLAS-BO0/2.", "ATLAS-BO0"));
        assert!(!named("journal:ATLAS-BO0/12:0001", "ATLAS-BO0"));
        assert!(named("ATLAS-BO0/2 and ATLAS-BO0", "ATLAS-BO0"));
        assert!(named("ATLAS-BO0/notes", "ATLAS-BO0"));
        assert!(named("lane=ATLAS-BO0 and `ATLAS-BO0`", "ATLAS-BO0"));
    }
}
