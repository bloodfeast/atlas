//! Orchestrator mode: agent kinds, caps on how many agents of each kind run
//! at once, and per-kind context limits (`docs/design/orchestrate.md`).
//!
//! The cap reads Claude Code's own record of the session's agents: each
//! launch writes `<session>/subagents/agent-<id>.meta.json` (`agentType`,
//! `description`, `toolUseId`, and `"finished": true` once done). It holds
//! no count of its own beyond the launches admitted and not yet started: a
//! pending record lives from the launch's PreToolUse until its agent starts
//! (SubagentStart, or its meta file seen in any state), so two launches of
//! one message both count before either agent exists. It only ever denies, on positive
//! evidence; every error and every unknown gives no decision.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::json::Value;

/// Claude Code's concurrent-subagent limit when the environment does not
/// set one (the sub-agents documentation, 2026-11).
pub const CLAUDE_CODE_DEFAULT_LIMIT: usize = 20;
/// The environment variable that sets Claude Code's concurrent-subagent limit.
pub const LIMIT_ENV: &str = "CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS";
/// The window the default limits are stated for.
pub const REFERENCE_WINDOW: u64 = 1_000_000;
/// The default hard limit on [`REFERENCE_WINDOW`], scaled to the window.
pub const DEFAULT_LIMIT: u64 = 450_000;
/// The default handoff point on [`REFERENCE_WINDOW`], scaled to the window.
pub const DEFAULT_HANDOFF: u64 = 285_000;
/// How long an admitted launch whose agent never starts counts: a launch
/// refused after atlas answered (a permission prompt, another hook) has no
/// SubagentStart and no meta file to retire it.
const PENDING_TTL: Duration = Duration::from_secs(120);
/// How much earlier than the process clock a file time may read.
const FILE_CLOCK_SLACK: Duration = Duration::from_secs(1);
/// Lines of an Agent launch prompt a `Kind:` line is looked for in (a
/// brief file the prompt names is never read).
const KIND_LINES: usize = 40;

/// What an agent is for, as the caps count it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Writes and compiles code.
    Builder,
    /// Reads and reviews; does not build.
    Reviewer,
    /// Researches or designs; does not build.
    Research,
    /// Writes briefs for other agents; does not build. Its own cap,
    /// uncapped by default (Claude Code's own limit still applies).
    BriefWriter,
    /// Not counted against any cap.
    None,
}

impl Kind {
    /// The kinds that have a cap.
    pub const COUNTED: [Kind; 4] = [Kind::Builder, Kind::Reviewer, Kind::Research, Kind::BriefWriter];

    /// A kind by name (`builder`, `reviewer`, `research`, `brief-writer`,
    /// `none`; case is ignored, and a plural, `review` / `build` or
    /// `brief writer` is accepted).
    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "builder" | "builders" | "build" => Some(Kind::Builder),
            "reviewer" | "reviewers" | "review" => Some(Kind::Reviewer),
            "research" | "researcher" | "design" => Some(Kind::Research),
            "brief-writer" | "brief-writers" | "brief writer" | "brief writers" => Some(Kind::BriefWriter),
            "none" => Some(Kind::None),
            _ => None,
        }
    }

    /// Its name in the configuration.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Builder => "builder",
            Kind::Reviewer => "reviewer",
            Kind::Research => "research",
            Kind::BriefWriter => "brief-writer",
            Kind::None => "none",
        }
    }
}

/// How a context limit is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Past the hard limit every tool but the handoff is denied.
    Enforced,
    /// The agent is told; nothing is denied.
    Advisory,
    /// Nothing.
    Off,
}

impl Mode {
    /// A mode by name.
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "enforced" => Some(Mode::Enforced),
            "advisory" => Some(Mode::Advisory),
            "off" => Some(Mode::Off),
            _ => None,
        }
    }
}

/// What the orchestrator is told when an agent that passed its handoff
/// point finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fresh {
    /// Start a fresh agent from the handoff.
    Auto,
    /// Ask the user first.
    Ask,
}

impl Fresh {
    /// A choice by name.
    pub fn parse(s: &str) -> Option<Fresh> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Fresh::Auto),
            "ask" => Some(Fresh::Ask),
            _ => None,
        }
    }
}

/// One kind's settings as configured (`orchestrate.kinds.<kind>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KindSpec {
    /// Agents of this kind at once.
    pub max: Option<usize>,
    /// The context window its limits scale to.
    pub window: Option<usize>,
    /// The hard limit, in tokens.
    pub limit: Option<usize>,
    /// The handoff point, in tokens.
    pub handoff: Option<usize>,
    /// The handoff point as a percentage of the limit.
    pub handoff_pct: Option<usize>,
    /// How the limit is applied.
    pub mode: Option<Mode>,
    /// What follows a handoff.
    pub fresh: Option<Fresh>,
}

/// The `orchestrate` section as configured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrchestrateSpec {
    /// Turn the agent caps on (default false).
    pub enabled: Option<bool>,
    /// The context window limits scale to (0 or absent: inferred).
    pub window: Option<usize>,
    /// Minutes after which an unfinished agent that wrote nothing is not
    /// counted (default 360).
    pub stale_minutes: Option<usize>,
    /// The kind of an agent no rule classifies (default builder).
    pub default_kind: Option<Kind>,
    /// The mode of a kind that sets none (default enforced).
    pub mode: Option<Mode>,
    /// What follows a handoff, for a kind that sets none (default auto).
    pub fresh: Option<Fresh>,
    /// `subagent_type` to kind, over the built-in map.
    pub types: Vec<(String, Kind)>,
    /// Per-kind settings.
    pub kinds: Vec<(Kind, KindSpec)>,
    /// Resource capacities for the work queue (`orchestrate.resources`).
    pub resources: Vec<(String, usize)>,
    /// Minutes of waiting that raise a queued item one priority class.
    pub aging_minutes: Option<usize>,
    /// The bare gpu words (`capture`, `screenshot`, `bench`, ...) infer gpu
    /// (`orchestrate.bare_gpu_words`; a project file may set it; default off).
    pub bare_gpu_words: Option<bool>,
    /// The review roles every PR needs before it merges (`orchestrate.reviews.roles`).
    pub review_roles: Vec<String>,
    /// How the merge gate applies (`orchestrate.reviews.mode`, default advisory).
    pub review_mode: Option<Mode>,
    /// The task judge's advisory line at launches (`orchestrate.judge.enabled`, default off).
    pub judge_enabled: Option<bool>,
    /// The task judge's cells file (`orchestrate.judge.cells`).
    pub judge_cells: Option<std::path::PathBuf>,
    /// The work classifier (`orchestrate.judge.classifier`).
    pub judge_classifier: crate::taskjudge::ClassifierSettings,
}

/// One kind's resolved settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindLimits {
    /// Agents of this kind at once (clamped to Claude Code's limit).
    pub max: usize,
    /// The window the limits were scaled to.
    pub window: u64,
    /// The hard limit, in context tokens.
    pub limit: u64,
    /// The handoff (soft) point, at most `limit`.
    pub handoff: u64,
    /// How the limit is applied.
    pub mode: Mode,
    /// What follows a handoff.
    pub fresh: Fresh,
}

/// The resolved orchestrator settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchSettings {
    /// The agent caps are on.
    pub enabled: bool,
    /// Claude Code's own concurrent-subagent limit.
    pub claude_limit: usize,
    /// An unfinished agent that wrote nothing for this long is not counted.
    pub stale: Duration,
    /// The kind of an agent no rule classifies.
    pub default_kind: Kind,
    /// `subagent_type` to kind (configured entries first, then built-in).
    pub types: Vec<(String, Kind)>,
    /// Builders.
    pub builder: KindLimits,
    /// Reviewers.
    pub reviewer: KindLimits,
    /// Research and design agents.
    pub research: KindLimits,
    /// Brief writers.
    pub brief_writer: KindLimits,
    /// Uncounted agents and the main session (the `cap` values).
    pub other: KindLimits,
    /// The work queue's resource capacities and aging.
    pub lanes: crate::lanes::Policy,
    /// The required-reviews merge gate.
    pub reviews: crate::reviews::ReviewSettings,
    /// The task judge (see [`crate::taskjudge`]).
    pub judge: crate::taskjudge::JudgeSettings,
}

impl Default for OrchSettings {
    fn default() -> Self {
        resolve(None, None, None, |_| None)
    }
}

impl OrchSettings {
    /// A kind's settings.
    pub fn limits(&self, k: Kind) -> &KindLimits {
        match k {
            Kind::Builder => &self.builder,
            Kind::Reviewer => &self.reviewer,
            Kind::Research => &self.research,
            Kind::BriefWriter => &self.brief_writer,
            Kind::None => &self.other,
        }
    }
}

/// The built-in `subagent_type` map (Claude Code's own agent types).
const BUILT_IN_TYPES: &[(&str, Kind)] = &[
    ("Explore", Kind::Research),
    ("Plan", Kind::Research),
    ("claude-code-guide", Kind::Research),
    ("statusline-setup", Kind::None),
    ("builder", Kind::Builder),
    ("reviewer", Kind::Reviewer),
    ("research", Kind::Research),
    ("brief-writer", Kind::BriefWriter),
];

/// The window inferred from Claude Code's environment: the auto-compact
/// window, else 200k when the 1M context is turned off, else 1M.
pub fn inferred_window(env: &dyn Fn(&str) -> Option<String>) -> u64 {
    if let Some(w) = env("CLAUDE_CODE_AUTO_COMPACT_WINDOW").and_then(|v| v.trim().parse::<u64>().ok()).filter(|w| *w > 0) {
        return w;
    }
    let off = env("CLAUDE_CODE_DISABLE_1M_CONTEXT").is_some_and(|v| !matches!(v.trim(), "" | "0" | "false"));
    if off {
        200_000
    } else {
        REFERENCE_WINDOW
    }
}

/// Resolves the settings. `cap_warn` and `cap_block` are the configured
/// `cap.warn_tokens` and `cap.block_tokens`, the fallbacks of every kind;
/// `env` reads Claude Code's environment.
pub fn resolve(
    spec: Option<&OrchestrateSpec>,
    cap_warn: Option<usize>,
    cap_block: Option<usize>,
    env: impl Fn(&str) -> Option<String>,
) -> OrchSettings {
    let o = spec.cloned().unwrap_or_default();
    let claude_limit = env(LIMIT_ENV).and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(CLAUDE_CODE_DEFAULT_LIMIT);
    let base_window = o.window.filter(|w| *w > 0).map_or_else(|| inferred_window(&env), |w| w as u64);
    let mode = o.mode.unwrap_or(Mode::Enforced);
    let fresh = o.fresh.unwrap_or(Fresh::Auto);
    let none = KindSpec::default();
    let resolve_kind = |k: Kind, default_max: usize| {
        let ks = o.kinds.iter().find(|(x, _)| *x == k).map_or(&none, |(_, s)| s);
        let window = ks.window.filter(|w| *w > 0).map_or(base_window, |w| w as u64);
        let scaled = |t: u64| ((t as u128 * window as u128) / REFERENCE_WINDOW as u128) as u64;
        let limit = ks.limit.or(cap_block).map_or_else(|| scaled(DEFAULT_LIMIT), |l| l as u64).max(1);
        let handoff = ks
            .handoff
            .map(|h| h as u64)
            .or_else(|| ks.handoff_pct.map(|p| limit * p.min(100) as u64 / 100))
            .or(cap_warn.map(|w| w as u64))
            .unwrap_or_else(|| scaled(DEFAULT_HANDOFF))
            .min(limit);
        KindLimits {
            max: ks.max.unwrap_or(default_max).min(claude_limit),
            window,
            limit,
            handoff,
            mode: ks.mode.unwrap_or(mode),
            fresh: ks.fresh.unwrap_or(fresh),
        }
    };
    let mut types = o.types.clone();
    for (t, k) in BUILT_IN_TYPES {
        if !types.iter().any(|(x, _)| x.eq_ignore_ascii_case(t)) {
            types.push(((*t).to_string(), *k));
        }
    }
    OrchSettings {
        enabled: o.enabled.unwrap_or(false),
        claude_limit,
        stale: Duration::from_secs(o.stale_minutes.unwrap_or(360) as u64 * 60),
        default_kind: o.default_kind.unwrap_or(Kind::Builder),
        types,
        builder: resolve_kind(Kind::Builder, 2),
        reviewer: resolve_kind(Kind::Reviewer, 2),
        research: resolve_kind(Kind::Research, 1),
        // Uncapped by default: only Claude Code's own limit applies.
        brief_writer: resolve_kind(Kind::BriefWriter, claude_limit),
        other: resolve_kind(Kind::None, claude_limit),
        lanes: {
            let mut p = crate::lanes::Policy::default();
            for (n, c) in &o.resources {
                match p.resources.iter_mut().find(|(x, _)| x == n) {
                    Some(e) => e.1 = *c,
                    None => p.resources.push((n.clone(), *c)),
                }
            }
            if let Some(a) = o.aging_minutes {
                p.aging_minutes = a as i64;
            }
            p.bare_gpu_words = o.bare_gpu_words.unwrap_or(false);
            p
        },
        reviews: crate::reviews::ReviewSettings {
            roles: o.review_roles.iter().map(|r| r.trim().to_ascii_lowercase()).filter(|r| !r.is_empty()).collect(),
            mode: o.review_mode.unwrap_or(Mode::Advisory),
        },
        judge: crate::taskjudge::JudgeSettings {
            enabled: o.judge_enabled.unwrap_or(false),
            cells: o.judge_cells.clone(),
            classifier: o.judge_classifier.clone(),
        },
    }
}

/// The kind a brief (the Agent launch prompt itself) states on a `Kind:`
/// line among its first lines (markdown bullets, quotes and bold around it
/// are allowed).
pub fn brief_kind(brief: &str) -> Option<Kind> {
    for line in brief.lines().take(KIND_LINES) {
        let l = line.trim().trim_start_matches(['-', '*', '>', '#', ' ', '\t']).replace("**", "");
        let l = l.trim();
        let Some(head) = l.get(..5) else { continue };
        if !head.eq_ignore_ascii_case("kind:") {
            continue;
        }
        let rest = l[5..].trim().trim_matches(['`', '"', '\'']);
        let word = |s: &str| -> String { s.chars().take_while(|c| c.is_ascii_alphabetic()).collect() };
        let first = word(rest);
        // A two-word kind (`brief writer`, `brief-writer`) is tried first.
        let second = rest[first.len()..].strip_prefix([' ', '-']).map(|t| word(t.trim_start())).filter(|w| !w.is_empty());
        if let Some(k) = second.and_then(|w| Kind::parse(&format!("{first} {w}"))).or_else(|| Kind::parse(&first)) {
            return Some(k);
        }
    }
    None
}

/// A brief's kind with no agent type or isolation to go on: its `Kind:`
/// line, else the default kind.
pub fn brief_kind_or(s: &OrchSettings, brief: &str) -> Kind {
    brief_kind(brief).unwrap_or(s.default_kind)
}

/// How [`classify_source`] decided an agent's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindSource {
    /// A `Kind:` line in the launch prompt's first [`KIND_LINES`] lines.
    Line,
    /// Its `subagent_type` in `orchestrate.types` (or the built-in map).
    Type,
    /// `isolation: "worktree"`.
    Worktree,
    /// `orchestrate.default_kind`.
    Default,
}

/// An agent's kind: its brief's `Kind:` line, else its `subagent_type` in
/// the map, else a worktree agent is a builder, else the default kind. The
/// brief is the Agent launch prompt, not a file it names.
pub fn classify(s: &OrchSettings, brief: &str, subagent_type: &str, isolation: &str) -> Kind {
    classify_source(s, brief, subagent_type, isolation).0
}

/// [`classify`], and which rule decided it.
pub fn classify_source(s: &OrchSettings, brief: &str, subagent_type: &str, isolation: &str) -> (Kind, KindSource) {
    if let Some(k) = brief_kind(brief) {
        return (k, KindSource::Line);
    }
    if let Some((_, k)) = s.types.iter().find(|(t, _)| !subagent_type.is_empty() && t.eq_ignore_ascii_case(subagent_type)) {
        return (*k, KindSource::Type);
    }
    if isolation.eq_ignore_ascii_case("worktree") {
        return (Kind::Builder, KindSource::Worktree);
    }
    (s.default_kind, KindSource::Default)
}

/// Why a launch counts as `kind`, for the cap's refusal: the rule that
/// decided it and, when no `Kind:` line did, how to set one.
pub fn kind_reason(kind: Kind, source: KindSource, subagent_type: &str) -> String {
    let none = format!("its prompt has no `Kind:` line naming a kind in its first {KIND_LINES} lines");
    let why = match source {
        KindSource::Line => return format!("This launch counts as a {}: its prompt's `Kind:` line says so.", noun(kind)),
        KindSource::Type => format!("{none}, and the agent-type map (orchestrate.types) gives its subagent_type `{subagent_type}` the kind {}", kind.name()),
        KindSource::Worktree => format!("{none}, and an agent with isolation: \"worktree\" counts as a builder"),
        KindSource::Default => format!("{none} and no other rule classifies it, so orchestrate.default_kind ({}) applies", kind.name()),
    };
    format!(
        "This launch counts as a {}: {why}. If the work is of another kind, put `Kind: builder`, `Kind: reviewer`, `Kind: research` or `Kind: brief-writer` in the prompt's first {KIND_LINES} lines (a brief file the prompt names is not read).",
        noun(kind)
    )
}

/// A kind as a noun for an agent: `builder`, `reviewer`, `research agent`.
fn noun(kind: Kind) -> &'static str {
    match kind {
        Kind::Research => "research agent",
        Kind::BriefWriter => "brief writer",
        Kind::None => "uncounted agent",
        k => k.name(),
    }
}

/// The folder Claude Code keeps a session's agents in, from the session's
/// transcript path (a subagent's hook input names the parent's).
pub fn subagents_dir(transcript: &str) -> PathBuf {
    Path::new(transcript.strip_suffix(".jsonl").unwrap_or(transcript)).join("subagents")
}

/// The first user message of a transcript (an agent's brief), read line by
/// line (at most 200 lines, 64 KiB of text), so a long transcript is not
/// read whole.
pub fn brief_of(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(f).lines().take(200) {
        let line = line.ok()?;
        if !line.contains("\"user\"") {
            continue;
        }
        let Ok(v) = crate::json::parse(&line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let s = match v.pointer("/message/content")? {
            Value::String(s) => s.clone(),
            Value::Array(a) => a.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
            _ => continue,
        };
        return Some(s.chars().take(65_536).collect());
    }
    None
}

/// One running agent, as counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    /// The agent id.
    pub id: String,
    /// Its kind.
    pub kind: Kind,
    /// Its launch description.
    pub description: String,
    /// Since its transcript or meta file last changed.
    pub idle: Duration,
    /// The launch's tool use id.
    pub tool_use_id: String,
}

/// One Agent launch, as its PreToolUse sees it.
#[derive(Debug, Clone, Copy)]
pub struct Launch<'a> {
    /// The session's transcript path.
    pub transcript: &'a str,
    /// The tool call's id.
    pub tool_use_id: &'a str,
    /// The Agent tool's input.
    pub input: &'a Value,
    /// Its prompt (the brief).
    pub brief: &'a str,
}

/// The orchestrator's state in one hook process.
#[derive(Debug)]
pub struct Agents {
    /// When this process started, for a resident server; `None` for a
    /// command hook (a process per event).
    started: Option<SystemTime>,
    /// Launches admitted whose agent has not started yet, oldest first.
    pending: Vec<Pending>,
    /// The next pending record's number.
    seq: u64,
    /// The pending record the last admitted launch made, until the hook's
    /// answer for it is final (see [`Agents::release_last`]).
    last: Option<u64>,
    /// Agents already paired with a pending record (or seen at their start).
    paired: HashSet<String>,
    /// The kind each admitted launch was given, by tool use id.
    admitted: HashMap<String, Kind>,
    /// Each agent's kind, once known.
    kinds: HashMap<String, Kind>,
    /// When a real finish was seen for each agent.
    finished: HashMap<String, SystemTime>,
    /// Agents told to wrap up: (lane, kind), for the fresh-agent line.
    passed: HashMap<String, (String, Kind)>,
    /// The running list the last admitted launch was counted against, and
    /// its folder, for the queue's holders in the same hook call.
    seen: Option<(PathBuf, Vec<Running>)>,
    /// The last launch [`Agents::admit`] judged was refused at its kind's
    /// cap (not Claude Code's own limit).
    kind_refused: bool,
}

/// A launch admitted and not yet started.
#[derive(Debug, Clone)]
struct Pending {
    /// Its number in this process.
    seq: u64,
    /// The launch's tool use id; empty when the hook input carried none.
    tool_use_id: String,
    kind: Kind,
    /// When it was admitted, on the process clock (the TTL) and the wall
    /// clock (meta files created since are its agent's candidates).
    at: Instant,
    wall: SystemTime,
}

impl Default for Agents {
    fn default() -> Self {
        Agents::new(Some(SystemTime::now()))
    }
}

fn modified(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Agents {
    /// State for a process started at `started` (`None`: a command hook).
    pub fn new(started: Option<SystemTime>) -> Agents {
        Agents {
            started,
            pending: Vec::new(),
            seq: 0,
            last: None,
            paired: HashSet::new(),
            admitted: HashMap::new(),
            kinds: HashMap::new(),
            finished: HashMap::new(),
            passed: HashMap::new(),
            seen: None,
            kind_refused: false,
        }
    }

    /// Whether the last launch [`Agents::admit`] judged was refused at its
    /// kind's cap, where its kind is the cause.
    pub fn kind_refused(&self) -> bool {
        self.kind_refused
    }

    /// Frees the slot of the launch `admit` just let through, when a later
    /// gate of the same hook call denied it (the queue's lane conflict): a
    /// launch that does not start holds no slot (ORCH-RETRO F1).
    pub fn release_last(&mut self) {
        if let Some(n) = self.last.take() {
            self.pending.retain(|p| p.seq != n);
        }
    }

    /// Pairs a started agent with the pending record of its launch and
    /// retires the record. While any record carries a tool use id, only the
    /// record with the agent's `toolUseId` is retired; an agent whose meta
    /// names no id yet (not written at its SubagentStart) stays unpaired, so
    /// the meta pass pairs it by id once the meta exists. Only when no
    /// record has an id (hook input without `tool_use_id`, which the drift
    /// check flags) is the oldest record retired. An agent pairs once.
    fn pair(&mut self, agent: &str, tool_use_id: &str) {
        if self.paired.contains(agent) {
            return;
        }
        let ids = self.pending.iter().any(|p| !p.tool_use_id.is_empty());
        if ids && tool_use_id.is_empty() {
            return;
        }
        self.paired.insert(agent.to_string());
        let i = if ids { self.pending.iter().position(|p| p.tool_use_id == tool_use_id) } else { (!self.pending.is_empty()).then_some(0) };
        if let Some(i) = i {
            self.pending.remove(i);
        }
    }

    /// SubagentStart: the harness started agent `id` (in the session folder
    /// `dir`), so its launch is running, no longer pending.
    pub fn started(&mut self, dir: &Path, id: &str) {
        let id = id.strip_prefix("agent-").unwrap_or(id);
        if id.is_empty() {
            return;
        }
        let tool_use_id = std::fs::read_to_string(dir.join(format!("agent-{id}.meta.json")))
            .ok()
            .and_then(|t| crate::json::parse(&t).ok())
            .and_then(|m| m.get("toolUseId").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        self.pair(id, &tool_use_id);
    }

    /// Takes the running list the last admitted launch read in `dir`.
    ///
    /// An ordering contract: `admit` sets the list, and the hook takes it
    /// in the same call (`queue_launch`, then `holders`), so it is never
    /// read stale; `admit` also clears it on entry. A caller that admits
    /// without taking it next must not rely on it later.
    pub fn take_seen(&mut self, dir: &Path) -> Option<Vec<Running>> {
        match self.seen.take() {
            Some((d, r)) if d == dir => Some(r),
            _ => None,
        }
    }

    /// Sets the process start (`None` for a command hook).
    pub fn set_started(&mut self, started: Option<SystemTime>) {
        self.started = started;
    }

    /// Records a real finish of an agent, seen now.
    pub fn finished(&mut self, id: &str, at: SystemTime) {
        self.finished.insert(id.to_string(), at);
    }

    /// Records that an agent passed its handoff point.
    pub fn passed(&mut self, id: &str, lane: &str, kind: Kind) {
        self.passed.entry(id.to_string()).or_insert_with(|| (lane.to_string(), kind));
    }

    /// The lane and kind of an agent that passed its handoff point, once
    /// (taken when it finishes).
    pub fn take_passed(&mut self, id: &str) -> Option<(String, Kind)> {
        self.passed.remove(id)
    }

    /// An agent's kind: as admitted, else classified from its meta file and
    /// brief (`agent_type` when the meta file is missing). Cached.
    pub fn kind_of(&mut self, s: &OrchSettings, dir: &Path, id: &str, agent_type: &str) -> Kind {
        let id = id.strip_prefix("agent-").unwrap_or(id);
        if let Some(k) = self.kinds.get(id) {
            return *k;
        }
        let meta = std::fs::read_to_string(dir.join(format!("agent-{id}.meta.json"))).ok().and_then(|t| crate::json::parse(&t).ok());
        let field = |k: &str| meta.as_ref().and_then(|m| m.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
        let k = match self.admitted.get(&field("toolUseId")) {
            Some(k) => *k,
            None => {
                let ty = match field("agentType") {
                    t if t.is_empty() => agent_type.to_string(),
                    t => t,
                };
                let brief = brief_of(&dir.join(format!("agent-{id}.jsonl"))).unwrap_or_default();
                classify(s, &brief, &ty, "")
            }
        };
        self.kinds.insert(id.to_string(), k);
        k
    }

    /// The session's running agents, from the meta files in `dir`, in one
    /// pass over the folder. A missing folder is no agents.
    ///
    /// The same pass is the fallback for a lost SubagentStart: every meta
    /// file created since the oldest pending launch, in any state (running
    /// or already finished), pairs with its launch's record.
    ///
    /// # Errors
    ///
    /// I/O errors listing the folder.
    pub fn running(&mut self, s: &OrchSettings, dir: &Path, now: SystemTime) -> std::io::Result<Vec<Running>> {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        // File times run on a coarser clock than the process's: a meta file
        // written just after a launch may read up to a tick earlier.
        let since = self.pending.iter().map(|p| p.wall).min().map(|w| w.checked_sub(FILE_CLOCK_SLACK).unwrap_or(w));
        let read = |p: &Path| std::fs::read_to_string(p).ok().and_then(|t| crate::json::parse(&t).ok());
        let mut out = Vec::new();
        let mut started: Vec<(SystemTime, String, String)> = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".meta.json")) else { continue };
            // The listing's own times (free on Windows); a meta is read only
            // to pair a new agent or to count a live one.
            let md = e.metadata().ok();
            let mut meta = None;
            if let (Some(since), Some(created)) = (since, md.as_ref().and_then(|m| m.created().or_else(|_| m.modified()).ok())) {
                if created >= since && !self.paired.contains(id) {
                    meta = read(&e.path());
                    let t = meta.as_ref().and_then(|m| m.get("toolUseId")).and_then(Value::as_str).unwrap_or("").to_string();
                    started.push((created, id.to_string(), t));
                }
            }
            let transcript = dir.join(format!("agent-{id}.jsonl"));
            let Some(last) = [md.as_ref().and_then(|m| m.modified().ok()), modified(&transcript)].into_iter().flatten().max() else { continue };
            if self.started.is_some_and(|st| last < st) {
                continue;
            }
            let idle = now.duration_since(last).unwrap_or_default();
            if idle > s.stale {
                continue;
            }
            if self.finished.get(id).is_some_and(|f| *f >= last) {
                continue;
            }
            let Some(meta) = meta.or_else(|| read(&e.path())) else { continue };
            if meta.get("finished").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let kind = self.kind_of(s, dir, id, "");
            let f = |k: &str| meta.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            out.push(Running { id: id.to_string(), kind, description: f("description"), idle, tool_use_id: f("toolUseId") });
        }
        started.sort();
        for (_, id, t) in started {
            self.pair(&id, &t);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Whether a launch may start: `None` (no decision) or a deny reason.
    /// Errors give no decision.
    pub fn admit(&mut self, s: &OrchSettings, launch: &Launch<'_>, now: SystemTime, clock: Instant) -> Option<String> {
        let Launch { transcript, tool_use_id, input, brief } = *launch;
        self.seen = None;
        self.last = None;
        self.kind_refused = false;
        if !s.enabled || transcript.is_empty() {
            return None;
        }
        let f = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let (kind, source) = classify_source(s, brief, &f("subagent_type"), &f("isolation"));
        let dir = subagents_dir(transcript);
        let running = match self.running(s, &dir, now) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("atlas agent cap: {}: {e}", dir.display());
                return None;
            }
        };
        // A launch never counts against itself: a second PreToolUse of the
        // same tool call replaces its own pending slot.
        self.pending.retain(|p| clock.duration_since(p.at) < PENDING_TTL && (tool_use_id.is_empty() || p.tool_use_id != tool_use_id));
        let of = |k: Kind| running.iter().filter(|r| r.kind == k).count() + self.pending.iter().filter(|p| p.kind == k).count();
        let total = running.len() + self.pending.len();
        let lim = s.limits(kind);
        let list = |k: Option<Kind>| {
            let mut v: Vec<String> = running
                .iter()
                .filter(|r| k.is_none_or(|k| r.kind == k))
                .map(|r| {
                    format!(
                        "{} ({}, idle {} min)",
                        if r.description.is_empty() { &r.id } else { &r.description },
                        r.id,
                        r.idle.as_secs() / 60
                    )
                })
                .collect();
            let n = self.pending.iter().filter(|p| k.is_none_or(|k| p.kind == k)).count();
            if n > 0 {
                v.push(format!("{n} launched moments ago"));
            }
            v.join("; ")
        };
        if kind != Kind::None && of(kind) >= lim.max {
            self.kind_refused = true;
            return Some(format!(
                "atlas agent cap: {} of {} {} slots are in use: {}. Do not retry now. Queue this launch and start it when one of them finishes (its task-notification). {} The cap is orchestrate.kinds.{}.max in atlas's configuration.",
                of(kind),
                lim.max,
                kind.name(),
                list(Some(kind)),
                kind_reason(kind, source, &f("subagent_type")),
                kind.name()
            ));
        }
        if total >= s.claude_limit {
            return Some(format!(
                "atlas agent cap: {total} agents are running, Claude Code's own limit of {} ({LIMIT_ENV}): {}. Do not retry now. Queue this launch and start it when one of them finishes.",
                s.claude_limit,
                list(None)
            ));
        }
        if !tool_use_id.is_empty() {
            self.admitted.insert(tool_use_id.to_string(), kind);
        }
        self.seq += 1;
        self.last = Some(self.seq);
        self.pending.push(Pending { seq: self.seq, tool_use_id: tool_use_id.to_string(), kind, at: clock, wall: now });
        self.seen = Some((dir, running));
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_string())
    }

    #[test]
    fn defaults_scale_to_the_window() {
        let s = resolve(None, None, None, env(&[]));
        assert!(!s.enabled);
        assert_eq!((s.builder.max, s.reviewer.max, s.research.max), (2, 2, 1));
        assert_eq!((s.builder.handoff, s.builder.limit, s.builder.window), (285_000, 450_000, 1_000_000));
        assert_eq!((s.builder.mode, s.builder.fresh), (Mode::Enforced, Fresh::Auto));
        let small = resolve(None, None, None, env(&[("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1")]));
        assert_eq!((small.research.handoff, small.research.limit), (57_000, 90_000));
        let ac = resolve(None, None, None, env(&[("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "500000")]));
        assert_eq!((ac.reviewer.handoff, ac.reviewer.limit), (142_500, 225_000));
        // A configured window wins over the environment.
        let o = OrchestrateSpec { window: Some(2_000_000), ..Default::default() };
        let w = resolve(Some(&o), None, None, env(&[("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1")]));
        assert_eq!(w.builder.limit, 900_000);
    }

    #[test]
    fn settings_clamp_and_fall_back() {
        let o = OrchestrateSpec {
            mode: Some(Mode::Advisory),
            kinds: vec![
                (Kind::Builder, KindSpec { max: Some(50), limit: Some(500_000), handoff: Some(900_000), ..Default::default() }),
                (
                    Kind::Reviewer,
                    KindSpec { max: Some(0), handoff_pct: Some(60), fresh: Some(Fresh::Ask), mode: Some(Mode::Off), ..Default::default() },
                ),
            ],
            ..Default::default()
        };
        let s = resolve(Some(&o), Some(300_000), Some(400_000), env(&[(LIMIT_ENV, "6")]));
        assert_eq!(s.claude_limit, 6);
        // A max above Claude Code's limit is clamped to it; a handoff above
        // the limit is clamped to the limit.
        assert_eq!((s.builder.max, s.builder.limit, s.builder.handoff, s.builder.mode), (6, 500_000, 500_000, Mode::Advisory));
        // handoff_pct of the legacy cap.block_tokens; per-kind mode and fresh.
        assert_eq!(
            (s.reviewer.max, s.reviewer.limit, s.reviewer.handoff, s.reviewer.mode, s.reviewer.fresh),
            (0, 400_000, 240_000, Mode::Off, Fresh::Ask)
        );
        // Legacy cap values for a kind with nothing of its own.
        assert_eq!((s.research.max, s.research.handoff, s.research.limit), (1, 300_000, 400_000));
        // A bad environment value falls back to Claude Code's default.
        assert_eq!(resolve(None, None, None, env(&[(LIMIT_ENV, "lots")])).claude_limit, CLAUDE_CODE_DEFAULT_LIMIT);
        assert_eq!(resolve(None, None, None, env(&[(LIMIT_ENV, "3")])).builder.max, 2);
        assert_eq!(resolve(None, None, None, env(&[(LIMIT_ENV, "1")])).builder.max, 1);
    }

    #[test]
    fn classification_follows_its_precedence() {
        let o = OrchestrateSpec {
            types: vec![("critic".into(), Kind::Reviewer), ("explore".into(), Kind::Builder)],
            default_kind: Some(Kind::Research),
            ..Default::default()
        };
        let s = resolve(Some(&o), None, None, env(&[]));
        assert_eq!(classify(&s, "Do it.\n- **Kind:** `reviewer`\n", "Explore", "worktree"), Kind::Reviewer);
        assert_eq!(classify(&s, "kind: none", "", ""), Kind::None);
        // A configured type overrides the built-in map, case-insensitively.
        assert_eq!(classify(&s, "x", "Explore", ""), Kind::Builder);
        assert_eq!(classify(&s, "x", "Plan", ""), Kind::Research);
        assert_eq!(classify(&s, "x", "critic", ""), Kind::Reviewer);
        assert_eq!(classify(&s, "x", "general-purpose", "worktree"), Kind::Builder);
        assert_eq!(classify(&s, "x", "general-purpose", ""), Kind::Research);
        // A `Kind:` line past the first lines, or naming no kind, is not read.
        let late = format!("{}Kind: reviewer", "line\n".repeat(KIND_LINES));
        assert_eq!(brief_kind(&late), None);
        assert_eq!(brief_kind("Kind: whatever"), None);
        assert_eq!(classify(&resolve(None, None, None, env(&[])), "x", "", ""), Kind::Builder);
        // Which rule decided it.
        assert_eq!(classify_source(&s, "Kind: reviewer", "Explore", "worktree"), (Kind::Reviewer, KindSource::Line));
        assert_eq!(classify_source(&s, "Kind: whatever", "critic", "worktree"), (Kind::Reviewer, KindSource::Type));
        assert_eq!(classify_source(&s, "x", "general-purpose", "Worktree"), (Kind::Builder, KindSource::Worktree));
        assert_eq!(classify_source(&s, "x", "", ""), (Kind::Research, KindSource::Default));
    }

    /// The built-in map gives agent type `t` the kind `k`; a `Kind:` line still wins.
    fn kind_named_type_maps(t: &str, k: Kind) {
        let s = resolve(None, None, None, env(&[]));
        // With no `Kind:` line, the subagent_type decides, over the worktree rule.
        assert_eq!(classify_source(&s, "x", t, ""), (k, KindSource::Type), "{t}");
        assert_eq!(classify_source(&s, "x", t, "worktree"), (k, KindSource::Type), "{t}");
        // A `Kind:` line naming the same kind decides it.
        assert_eq!(classify_source(&s, &format!("Kind: {}", k.name()), t, ""), (k, KindSource::Line), "{t}");
        // A `Kind:` line wins even when it disagrees.
        assert_eq!(classify_source(&s, "Kind: none", t, "worktree"), (Kind::None, KindSource::Line), "{t}");
    }

    #[test]
    fn the_builder_agent_type_is_a_builder() {
        kind_named_type_maps("builder", Kind::Builder);
    }

    #[test]
    fn the_reviewer_agent_type_is_a_reviewer() {
        kind_named_type_maps("reviewer", Kind::Reviewer);
    }

    #[test]
    fn the_research_agent_type_is_research() {
        kind_named_type_maps("research", Kind::Research);
    }

    #[test]
    fn a_configured_type_overrides_the_kind_named_ones() {
        let o = OrchestrateSpec { types: vec![("Reviewer".into(), Kind::Research), ("research".into(), Kind::None)], ..Default::default() };
        let s = resolve(Some(&o), None, None, env(&[]));
        assert_eq!(classify_source(&s, "x", "reviewer", ""), (Kind::Research, KindSource::Type));
        assert_eq!(classify_source(&s, "x", "research", ""), (Kind::None, KindSource::Type));
        assert_eq!(classify_source(&s, "Kind: builder", "reviewer", ""), (Kind::Builder, KindSource::Line));
    }

    #[test]
    fn a_brief_writer_parses_maps_and_is_uncapped_by_default() {
        for n in ["brief writer", "brief-writer", "brief-writers", "Brief Writers", " BRIEF-WRITER "] {
            assert_eq!(Kind::parse(n), Some(Kind::BriefWriter), "{n}");
        }
        assert_eq!(Kind::parse("brief"), None);
        assert_eq!(Kind::BriefWriter.name(), "brief-writer");
        assert!(Kind::COUNTED.contains(&Kind::BriefWriter));
        // A `Kind:` line in either spelling, with words after it.
        for b in [
            "Kind: brief writer",
            "Kind: brief-writer",
            "- **Kind:** `brief-writers`",
            "Kind: Brief writer (no build)",
            "kind: brief writer, uncapped",
        ] {
            assert_eq!(brief_kind(b), Some(Kind::BriefWriter), "{b}");
        }
        // A one-word kind followed by more words still reads.
        assert_eq!(brief_kind("Kind: builder writes code"), Some(Kind::Builder));
        assert_eq!(brief_kind("Kind: reviewer-only"), Some(Kind::Reviewer));
        assert_eq!(brief_kind("Kind: brief"), None);
        let s = resolve(None, None, None, env(&[]));
        assert_eq!(classify_source(&s, "x", "brief-writer", ""), (Kind::BriefWriter, KindSource::Type));
        assert_eq!(classify(&s, "x", "Brief-Writer", "worktree"), Kind::BriefWriter);
        // Uncapped by default: Claude Code's own limit, whatever it is.
        assert_eq!(s.brief_writer.max, CLAUDE_CODE_DEFAULT_LIMIT);
        assert_eq!(resolve(None, None, None, env(&[(LIMIT_ENV, "7")])).brief_writer.max, 7);
        assert_eq!(s.limits(Kind::BriefWriter), &s.brief_writer);
        // A configured cap, 0 included, is its own and never the builder's.
        let o = OrchestrateSpec { kinds: vec![(Kind::BriefWriter, KindSpec { max: Some(0), ..Default::default() })], ..Default::default() };
        let c = resolve(Some(&o), None, None, env(&[]));
        assert_eq!((c.brief_writer.max, c.builder.max, c.research.max), (0, 2, 1));
        assert_eq!(noun(Kind::BriefWriter), "brief writer");
    }

    #[test]
    fn the_kind_reason_names_its_rule() {
        let line = kind_reason(Kind::Reviewer, KindSource::Line, "");
        assert_eq!(line, "This launch counts as a reviewer: its prompt's `Kind:` line says so.");
        let ty = kind_reason(Kind::Research, KindSource::Type, "Explore");
        assert!(
            ty.starts_with("This launch counts as a research agent: ")
                && ty.contains("no `Kind:` line naming a kind in its first 40 lines, and the agent-type map (orchestrate.types) gives its subagent_type `Explore` the kind research"),
            "{ty}"
        );
        let wt = kind_reason(Kind::Builder, KindSource::Worktree, "");
        assert!(wt.contains("an agent with isolation: \"worktree\" counts as a builder"), "{wt}");
        let d = kind_reason(Kind::Research, KindSource::Default, "");
        assert!(d.starts_with("This launch counts as a research agent: ") && d.contains("so orchestrate.default_kind (research) applies"), "{d}");
        for r in [&ty, &wt, &d] {
            assert!(r.contains("put `Kind: builder`, `Kind: reviewer`, `Kind: research` or `Kind: brief-writer` in the prompt's first 40 lines (a brief file the prompt names is not read)"), "{r}");
        }
    }

    #[test]
    fn kind_and_mode_names_round_trip() {
        for k in [Kind::Builder, Kind::Reviewer, Kind::Research, Kind::None] {
            assert_eq!(Kind::parse(k.name()), Some(k));
        }
        assert_eq!(Mode::parse("Advisory"), Some(Mode::Advisory));
        assert_eq!(Fresh::parse("ask"), Some(Fresh::Ask));
        assert_eq!(Kind::parse("wizard"), None);
    }
}
