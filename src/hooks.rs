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
use crate::memory::settings::Settings;
use crate::memory::{Draft, MemoryStore};

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
    /// Stop's `background_tasks`, `Null` when the input lacks it.
    pub background_tasks: Value,
    /// Stop's `session_crons`, `Null` when the input lacks it.
    pub session_crons: Value,
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
        }
    }
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
        if self.deny.is_none() && self.context.is_empty() && self.updated_input.is_none() && self.updated_output.is_none() {
            return String::new();
        }
        crate::json::object(self.context.len() + 128, |w| {
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
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut end = len;
    let mut carry: Vec<u8> = Vec::new();
    let step = 64 * 1024u64;
    while end > 0 && len - end < 16 * 1024 * 1024 {
        let start = end.saturating_sub(step);
        let mut buf = vec![0u8; (end - start) as usize];
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut buf)?;
        buf.extend_from_slice(&carry);
        // Whole lines only: the first partial line is carried to the next step.
        let first_nl = if start == 0 { None } else { buf.iter().position(|b| *b == b'\n') };
        let (head, body) = match first_nl {
            Some(i) => (buf[..i].to_vec(), &buf[i + 1..]),
            None => (Vec::new(), &buf[..]),
        };
        for line in body.rsplit(|b| *b == b'\n') {
            if line.len() < 16 || !contains(line, b"\"usage\"") || !contains(line, b"\"assistant\"") {
                continue;
            }
            let Ok(v) = crate::json::parse_bytes(line) else { continue };
            let Some(m) = v.get("message") else { continue };
            let Some(u) = m.get("usage") else { continue };
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            let ctx = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens") + n("output_tokens");
            let id = m.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            return Ok(Some((id, ctx)));
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
}

/// A tool that hands a subagent's report back to its caller.
fn is_handback_tool(name: &str) -> bool {
    name == "SubagentHandback" || name.ends_with("__SubagentHandback")
}

/// The atlas tools that write to a lane journal.
fn is_journal_tool(name: &str) -> bool {
    is_handoff_tool(name) || name == "journal" || name.ends_with("__journal")
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
    settings: Settings,
    store: MemoryStore,
    briefs: HashMap<String, VecDeque<String>>,
    injected: HashMap<String, HashSet<String>>,
    /// Bytes of recall injected per session or subagent (the session budget).
    spent: HashMap<String, usize>,
    pending_recall: HashSet<String>,
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
    /// The records the last answer injected, for the memory eval.
    last: Vec<Injected>,
    /// The vaults and sources, by name, that lines refs resolve against.
    roots: Vec<crate::compact::RefRoot>,
    /// Orchestrator mode: the agents counted against the caps.
    orch: crate::orchestrate::Agents,
    /// Reads a PR's number and head for the merge gate (`gh pr view`).
    pr_head: PrHead,
    /// Idea capture (`memory.ideas`); `None` when it is off, so an off store
    /// reads nothing, writes nothing and adds nothing to context.
    ideas: Option<crate::ideas::Live>,
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

impl Hooks {
    /// True once a background embedder has been started (for the rescore
    /// test that the pre-embed leaves it nothing to do; xtask cannot see
    /// `cfg(test)` items, so this is public and hidden).
    #[doc(hidden)]
    pub fn embedding_started(&self) -> bool {
        self.sem.as_ref().is_some_and(|s| s.embedding.is_some())
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
        let offer = crate::claude_settings::Paths::detect_in(None, &settings.index_db, settings.user_state.as_deref());
        let ideas = crate::ideas::Live::open(&settings.ideas, &settings.store);
        Ok(Hooks {
            sem: None,
            model_tried: false,
            settings,
            store,
            briefs: HashMap::new(),
            injected: HashMap::new(),
            spent: HashMap::new(),
            pending_recall: HashSet::new(),
            agent_lane: HashMap::new(),
            report_sent_back: HashSet::new(),
            warned: HashMap::new(),
            usage_cache: HashMap::new(),
            log,
            log_thread,
            router,
            offer,
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
            last: Vec::new(),
            roots: Vec::new(),
            orch: crate::orchestrate::Agents::default(),
            pr_head: crate::reviews::gh_head,
            ideas,
        })
    }

    /// Replaces how the merge gate reads a PR's head (tests; the default asks
    /// `gh pr view`).
    pub fn set_pr_head(&mut self, f: PrHead) {
        self.pr_head = f;
    }

    /// Marks this state as a command hook's (one process per event): an
    /// unfinished agent is then not excluded for being older than the
    /// process, only for being stale.
    pub fn set_command_hook(&mut self) {
        self.orch.set_started(None);
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
            let r = rusqlite::Connection::open(&self.settings.index_db).map_err(crate::Error::from).and_then(|c| {
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

    /// Points the settings offer at other files (tests use a
    /// scratch home), or turns it off with `None`.
    pub fn set_offer_paths(&mut self, p: Option<crate::claude_settings::Paths>) {
        self.offer = p;
    }

    /// Loads what the first recall would (the term frequencies), so the
    /// first hook answers warm.
    pub fn warm(&mut self) {
        let _ = self.store.rarest(&[], 0);
        let _ = self.store.lanes();
        self.load_model();
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
            sem.vecs = Some(RecVecs::load(&self.store, &sem.model.id, sem.model.dim)?);
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
        match self.try_handle(ev) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("atlas hook {}: {e}", ev.event);
                HookAnswer::default()
            }
        }
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
                // A session's first prompt: live atlas hooks that drifted from
                // hooks/hooks.json (a hook every setup has, unlike SessionStart).
                if self.drift_told.insert(ev.session_id.clone()) {
                    let (ideas_on, asks_on) = (self.ideas.is_some(), self.ideas.as_ref().is_some_and(crate::ideas::Live::asks));
                    if let Some(line) = self.offer.as_ref().and_then(|p| crate::claude_settings::hook_drift_line(p, ideas_on, asks_on)) {
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
                let brief = self.briefs.get_mut(&ev.session_id).and_then(VecDeque::pop_front);
                match brief {
                    Some(b) => {
                        self.note_lane(&ev.agent_id, &b);
                        let (t, k) = self.recall(&ev.agent_id, &b)?;
                        a.context = t;
                        keys = k;
                    }
                    None => {
                        self.pending_recall.insert(ev.agent_id.clone());
                    }
                }
            }
            "PreToolUse" => {
                let sub = !ev.agent_id.is_empty();
                // A main-session lookup made before the last compaction and
                // again after it is a post-compaction miss.
                if !sub {
                    if let Some(sig) = crate::compact::lookup_sig(&ev.tool_name, &ev.tool_input) {
                        if self.misses.lookup(&ev.session_id, sig) {
                            if let Some(l) = &self.log {
                                let _ = l.send(LogRow { tool: "compact:miss".into(), returned: 0, keys: Vec::new(), brief: None, session: ev.session_id.clone(), agent: String::new(), transcript: String::new() });
                            }
                        }
                    }
                }
                if sub || self.settings.cap.main_session {
                    let path = if sub { subagent_transcript(&ev.transcript_path, &ev.agent_id) } else { PathBuf::from(&ev.transcript_path) };
                    self.cap(ev, &path, &mut a);
                }
                if a.deny.is_none() && (ev.tool_name == "Agent" || ev.tool_name == "Task") {
                    let brief = if ev.tool_prompt.is_empty() { ev.tool_input.get("prompt").and_then(Value::as_str).unwrap_or("") } else { &ev.tool_prompt };
                    let launch = crate::orchestrate::Launch { transcript: &ev.transcript_path, tool_use_id: &ev.tool_use_id, input: &ev.tool_input, brief };
                    a.deny = self.orch.admit(&self.settings.orchestrate, &launch, std::time::SystemTime::now(), std::time::Instant::now());
                    let admitted = a.deny.is_none();
                    self.queue_launch(ev, brief, &mut a);
                    if admitted && a.deny.is_some() {
                        self.orch.release_last();
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
                    if (ev.tool_name == "Agent"|| ev.tool_name == "Task") && !ev.tool_prompt.is_empty() {
                        self.briefs.entry(ev.session_id.clone()).or_default().push_back(ev.tool_prompt.clone());
                    }
                    if sub && self.pending_recall.remove(&ev.agent_id) {
                        let path = subagent_transcript(&ev.transcript_path, &ev.agent_id);
                        if let Some(b) = first_user_text(&path) {
                            self.note_lane(&ev.agent_id, &b);
                            let (t, k) = self.recall(&ev.agent_id, &b)?;
                            if !t.is_empty() {
                                if !a.context.is_empty() {
                                    a.context.push('\n');
                                }
                                a.context.push_str(&t);
                            }
                            keys = k;
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
                a.block = self.report_backstop(&ev.agent_id, &ev.last_message, ev.stop_hook_active);
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
                // A new or cleared session: where the work stands
                // (`memory.resume_card`). Compaction has its own card. The
                // card is advisory: a store error loses it, not the lines
                // above.
                let fresh = ev.source == "startup" || ev.source == "clear";
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
            "PreCompact" => {
                // The opt-in transcript source is brought up to date once the
                // answer is out (its own switch, `memory.transcripts`).
                if self.settings.transcripts && !ev.transcript_path.is_empty() {
                    self.transcripts.push(PathBuf::from(&ev.transcript_path));
                }
                // The rest is the experimental compaction work, off unless
                // `compact.enabled` is set.
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
                            let _ = l.send(LogRow { tool: "compact:defer".into(), returned: 0, keys: Vec::new(), brief: None, session: ev.session_id.clone(), agent: String::new(), transcript: String::new() });
                        }
                        a.block = Some(reason);
                        return Ok(a);
                    }
                }
                self.misses.compact(&ev.session_id);
                let ctx = match &from {
                    crate::checkpoint::TailFrom::Own => if t.last_ctx > 0 { t.last_ctx } else { last_usage(Path::new(&ev.transcript_path)).ok().flatten().map_or(0, |(_, c)| c) },
                    crate::checkpoint::TailFrom::Parent(_) => t.last_ctx,
                    crate::checkpoint::TailFrom::Unread => 0,
                };
                let lane = format!("session:{}", ev.session_id);
                let scope = crate::checkpoint::Lanes::for_project(self.settings.graph.as_deref());
                // Bounded by the transcript's own newest record: never a record the
                // session could not have seen.
                let state = crate::checkpoint::state(&self.store, &ev.session_id, &t.start, &t.end, scope)?;
                let mut body = crate::checkpoint::body(&state, &t, self.settings.bytes_for(self.settings.compact.card_tokens));
                // The chain card: every part of the segment by a group key,
                // then the last links (its own cap, outside the body's).
                let read = match &from {
                    crate::checkpoint::TailFrom::Parent(p) => p.clone(),
                    _ => PathBuf::from(&ev.transcript_path),
                };
                if let Some(c) = crate::chain::card(&read) {
                    body.push_str(&c);
                    // Only with a card: the text tells the summarizer to leave
                    // out what the card lists. A later error answers nothing.
                    a.instructions = crate::claude_settings::COMPACT_INSTRUCTIONS;
                }
                // Never a silent 0-context card: say what is missing.
                if matches!(from, crate::checkpoint::TailFrom::Unread) {
                    body.push_str(crate::checkpoint::UNREAD_LINE);
                    body.push('\n');
                }
                let at = match (ctx, &from) {
                    (0, crate::checkpoint::TailFrom::Unread) => "transcript unread".to_string(),
                    (0, _) => "at unknown context tokens".to_string(),
                    _ => format!("at {ctx} context tokens"),
                };
                let tail_from = match &from {
                    crate::checkpoint::TailFrom::Parent(p) => format!("tail_from: {}\n", p.display()),
                    _ => String::new(),
                };
                let d = Draft {
                    title: format!("checkpoint ({}) {at}", if ev.trigger.is_empty() { "unknown" } else { &ev.trigger }),
                    meta: format!("transcript: {}\n{tail_from}hook_ms: {:.1}\n", ev.transcript_path, t_pre.elapsed().as_secs_f64() * 1e3),
                    body,
                    reason: "context compaction checkpoint written by the PreCompact hook".into(),
                    author: "hook".into(),
                    client: "atlas-hook".into(),
                    provenance: None,
                    time: None,
                };
                let mut w = self.store.writer()?;
                let key = w.next_key(&format!("journal:{lane}:"), 4)?;
                w.create(&key, "journal", "checkpoint", &lane, &d, None)?;
                w.commit()?;
            }
            _ => {}
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
                });
            }
        }
        Ok(a)
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
        // A lane the text names gets its journal first.
        for l in self.store.lanes()? {
            if l.lane.starts_with("session:") || !crate::memory::contains_word(text, &l.lane) {
                continue;
            }
            let seen = self.injected.entry(scope.to_string()).or_default();
            if !seen.insert(format!("lane:{}", l.lane)) {
                continue;
            }
            // The lane brief: its state whole, older entries cut to the
            // clauses the text matches (context selection).
            let trim = (r.select.enabled && r.select.lane_chars > 0).then_some((text, r.select.lane_chars));
            let v = recall::lane_with(&self.store, &l.lane, self.settings.bytes_for(r.lane_budget_tokens), trim)?;
            out.push_str(&v.text);
            // A record the lane view shows whole or trimmed is not pointed to
            // again below (send once).
            if r.select.enabled {
                self.injected.entry(scope.to_string()).or_default().extend(v.keys.iter().cloned());
            }
            let bytes = v.text.len();
            let (how, full) = if v.trimmed > 0 { (Sel::Trimmed, bytes + v.trimmed_full - v.trimmed_sent) } else { (Sel::Whole, bytes) };
            keys.push(Injected { key: format!("lane:{}", l.lane), fts: None, cos: None, bytes, how, full });
        }
        // A repository the text names gets its line from the project graph:
        // what it is, what it talks to and depends on, what uses it.
        if let Some(g) = self.settings.graph.clone() {
            for r in g.named_in(text) {
                let seen = self.injected.entry(scope.to_string()).or_default();
                if !seen.insert(format!("repo:{}", r.name)) {
                    continue;
                }
                if let Some(line) = g.describe(&r.name) {
                    let before = out.len();
                    out.push_str(&format!("atlas project {}: {line}\n", g.name));
                    let bytes = out.len() - before;
                    keys.push(Injected { key: format!("repo:{}", r.name), fts: None, cos: None, bytes, how: Sel::Whole, full: bytes });
                }
            }
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
        if let Ok(conn) = rusqlite::Connection::open(&self.settings.index_db) {
            let _ = conn.busy_timeout(std::time::Duration::from_secs(10));
            let _ = crate::compact::log(&conn, &logged);
        }
        Ok(report)
    }

    fn known_lanes(&self) -> Vec<String> {
        self.store.lanes().map(|ls| ls.into_iter().map(|l| l.lane).filter(|l| !l.starts_with("session:")).collect()).unwrap_or_default()
    }

    fn note_lane(&mut self, agent_id: &str, brief: &str) {
        let lane = brief_lane(brief, &self.known_lanes());
        let run = self.agent_lane.entry(agent_id.to_string()).or_insert_with(|| AgentRun { start: crate::memory::now(), ..AgentRun::default() });
        if run.lane.is_none() {
            run.lane = lane;
        }
    }

    /// A `<task-notification>`: when a subagent really finished (not an
    /// interim stop while its own background work runs) and wrote no
    /// journal entry since it started, one line says so. The lane is the
    /// one the agent wrote under (its own journal or handoff calls), else
    /// the one its brief names; with neither, nothing is said.
    fn notification(&mut self, prompt: &str, transcript: &str) -> Result<String> {
        let id = prompt.split("<task-id>").nth(1).and_then(|r| r.split("</task-id>").next()).unwrap_or("").trim().to_string();
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

    /// The pending ideas' line for a check-in or a task notification, when
    /// this session has not seen the newest of them.
    fn ideas_line(&mut self) -> String {
        let session = self.session.clone();
        self.ideas.as_mut().and_then(|i| i.line(&mut self.store, &session)).unwrap_or_default()
    }

    /// The session's running agents as lane and resource holders (none
    /// without a transcript or with the caps off). The list the cap's
    /// admission just read is reused.
    fn holders(&mut self, transcript: &str, known: &[String]) -> Vec<crate::lanes::Holder> {
        if transcript.is_empty() || !self.settings.orchestrate.enabled {
            return Vec::new();
        }
        let dir = crate::orchestrate::subagents_dir(transcript);
        let running = match self.orch.take_seen(&dir) {
            Some(r) => Ok(r),
            None => self.orch.running(&self.settings.orchestrate, &dir, std::time::SystemTime::now()),
        };
        match running {
            Ok(r) => crate::queue::holders(&r, &dir, known),
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
    fn queue_launch(&mut self, ev: &HookEvent, brief: &str, a: &mut HookAnswer) {
        if !self.settings.orchestrate.enabled {
            return;
        }
        let f = |k: &str| ev.tool_input.get(k).and_then(Value::as_str).unwrap_or("");
        let description = f("description");
        let kind = crate::orchestrate::classify(&self.settings.orchestrate, brief, f("subagent_type"), f("isolation"));
        let known = self.known_lanes();
        let class = crate::lanes::classify(brief, &known);
        if a.deny.is_none() {
            // Q-QL2: a conflict declared on both sides is denied and
            // queued; one that rests on an inference is let through with a
            // warning.
            let running = self.holders(&ev.transcript_path, &known);
            let c = crate::lanes::conflicts(kind, &class, &running, &self.settings.orchestrate.lanes);
            let (hard, soft): (Vec<_>, Vec<_>) = c.into_iter().partition(|c| c.declared);
            if !hard.is_empty() {
                a.deny = Some(format!(
                    "atlas queue: this launch conflicts with running work ({}), declared on both sides. Do not retry now; start it when that work finishes.",
                    hard.iter().map(|c| c.what.as_str()).collect::<Vec<_>>().join("; ")
                ));
            } else {
                let line = match crate::queue::started(&mut self.store, description, brief) {
                    Ok(crate::queue::Started::NotWaiting(k)) => format!("atlas queue: the brief's `Queue: {k}` names no waiting item (`atlas queue` lists them)."),
                    Ok(_) => String::new(),
                    Err(e) => {
                        eprintln!("atlas queue: {e}");
                        String::new()
                    }
                };
                if !line.is_empty() {
                    if !a.context.is_empty() {
                        a.context.push('\n');
                    }
                    a.context.push_str(&line);
                }
                if !soft.is_empty() {
                    if !a.context.is_empty() {
                        a.context.push('\n');
                    }
                    a.context.push_str(&format!(
                        "atlas queue: this launch may conflict with running work ({}); atlas inferred it, so it was let through. If the conflict is real, stop it and start it when that work finishes; a `Lane:` or `Uses:` line in the brief corrects a wrong inference.",
                        soft.iter().map(|c| c.what.as_str()).collect::<Vec<_>>().join("; ")
                    ));
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
                        " atlas {} it as {key}: launch it again with the same description and brief once a slot is free, and it leaves the queue (`atlas queue` lists what waits).",
                        if new { "queued" } else { "has already queued" }
                    ));
                }
            }
            Err(e) => eprintln!("atlas queue: {e}"),
        }
    }

    /// The SubagentStop backstop (Q-SD3): a report over
    /// [`crate::report::WHOLE_UP_TO`] bytes is sent back once, in orchestrated
    /// mode. atlas files it whole in the agent's lane and asks for the
    /// digest form, with its mechanical draft attached. A report within the
    /// bound, a second stop, or a filing error lets the stop through.
    fn report_backstop(&mut self, agent_id: &str, report: &str, already_blocked: bool) -> Option<String> {
        let report = report.trim();
        if !self.settings.orchestrate.enabled || already_blocked || report.len() <= crate::report::WHOLE_UP_TO || self.report_sent_back.contains(agent_id) {
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
        let draft = crate::report::digest(report, crate::report::budget(report.len()));
        Some(format!(
            "atlas report backstop: this report is {} bytes, over the {}-byte digest bound, and stays in the orchestrator's context for every later turn. atlas filed it whole as {key}. Hand back the digest form instead: the outcome first (PR, branch, merge, verdict), then the numbers, every open question under a `## Questions` heading, and every risk (a denied action, an unverified claim, a failure left open); cite {key} for the rest. Keep every fact the orchestrator acts on. This is asked once. atlas's mechanical draft ({} bytes) follows; correct and shorten it:

{draft}",
            report.len(),
            crate::report::WHOLE_UP_TO,
            draft.len()
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
            if self.store.lane(lane)?.iter().any(|h| h.time >= run.start) {
                return Ok(String::new());
            }
        }
        Ok(format!("atlas: agent {id} finished with no journal entry for lane {first} since it started; check its report against the journal.\n"))
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

    /// The lane an agent writes under, else its brief's, else `agent:<id>`.
    fn lane_of(&self, agent_id: &str) -> String {
        self.agent_lane.get(agent_id).and_then(|r| r.wrote.first().cloned().or_else(|| r.lane.clone())).unwrap_or_else(|| format!("agent:{agent_id}"))
    }

    /// The context limit of the calling agent (or the main session): past
    /// the handoff point it is told to wrap up, past the hard limit every
    /// tool but the handoff is denied (mode `enforced`). With orchestrated
    /// mode off this is the plain cap of v0.2 (`cap.warn_tokens` and
    /// `cap.block_tokens`, enforced, for every agent): the per-kind limits
    /// and modes apply only in orchestrated mode.
    fn cap(&mut self, ev: &HookEvent, path: &Path, a: &mut HookAnswer) {
        use crate::orchestrate::{Kind, Mode};
        let Some(ctx) = self.context_of(path) else { return };
        let sub = !ev.agent_id.is_empty();
        let orchestrated = self.settings.orchestrate.enabled;
        let kind = if sub && orchestrated {
            let dir = crate::orchestrate::subagents_dir(&ev.transcript_path);
            self.orch.kind_of(&self.settings.orchestrate, &dir, &ev.agent_id, &ev.agent_type)
        } else {
            Kind::None
        };
        let (handoff, limit, mode) = if sub && orchestrated {
            let lim = self.settings.orchestrate.limits(kind);
            (lim.handoff, lim.limit, lim.mode)
        } else {
            (self.settings.cap.warn_tokens, self.settings.cap.block_tokens, if orchestrated { self.settings.orchestrate.other.mode } else { Mode::Enforced })
        };
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
/// rulebook at that moment to `brief_log` (the "brief without atlas").
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
            let Ok(conn) = rusqlite::Connection::open(&db) else { return };
            let _ = conn.busy_timeout(std::time::Duration::from_secs(30));
            if conn.execute_batch(crate::tokens::TOKEN_TABLE).is_err() {
                return;
            }
            let Ok(mem) = MemoryStore::open(&store) else { return };
            for r in std::iter::once(first).chain(rx) {
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

    #[test]
    fn derives_the_subagent_transcript() {
        let p = subagent_transcript("/h/p/abc.jsonl", "a1b2");
        assert_eq!(p, Path::new("/h/p/abc").join("subagents").join("agent-a1b2.jsonl"));
        assert_eq!(subagent_transcript("/h/p/abc.jsonl", "agent-a1b2"), p);
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
}
