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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
pub const PENDING_TTL: Duration = Duration::from_secs(120);
/// How long a launch whose agent has not been seen yet keeps its brief, its
/// Binding block and its sub-lane, as a backstop only: they go when its agent
/// is confirmed (its meta file names the launch) or its refusal is seen in
/// the session transcript. A launch can wait at the permission prompt far
/// past [`PENDING_TTL`]; one kept too long only makes a later start
/// ambiguous (answered at its first tool call), never wrong.
pub const LAUNCH_HOLD_TTL: Duration = Duration::from_secs(3600);
/// How long an admitted launch whose agent has not started holds its class
/// (its lane, worktree, branch and resources) for later launches and the
/// queue: the launches of one message reach the hook within seconds, and a
/// launch refused after atlas answered must not hold its worktree against
/// its own relaunch for the cap slot's whole [`PENDING_TTL`].
pub const CLASS_HOLD_TTL: Duration = Duration::from_secs(15);
/// How much earlier than the process clock a file time may read.
const FILE_CLOCK_SLACK: Duration = Duration::from_secs(1);
/// `atlas queue release` refuses, without `--force`, an agent that wrote its
/// transcript or meta file this recently: it may still be alive.
pub const RELEASE_QUIET: Duration = Duration::from_secs(5 * 60);
/// The suffix of the release record files atlas 0.6.1 and earlier wrote
/// beside an agent's meta file, `agent-<id>.atlas-released` (#204). They are
/// read once into [`RELEASE_TABLE`] by [`Agents::running`] and deleted; atlas
/// writes none now (#222).
pub const RELEASED_SUFFIX: &str = ".atlas-released";
/// atlas's own release records, in the memory store (#222, ruling
/// journal:ATLAS-RELEASE:0066 Q3): one row per agent atlas counts finished,
/// `time` its release in nanoseconds since the Unix epoch. The agent counts
/// again once it writes after that, and the row then goes. Written by
/// `atlas queue release` and at a SubagentStop, so every process that opens
/// the store (the hooks, `atlas queue next`, `atlas backlog`) sees it.
/// Claude Code's meta file is never edited.
pub const RELEASE_TABLE: &str = "CREATE TABLE IF NOT EXISTS agent_releases(agent TEXT PRIMARY KEY, time INTEGER NOT NULL);";
/// A release row older than this before a new one is written is pruned: an
/// agent that wrote nothing for this long is never counted anyway.
const RELEASE_KEEP: Duration = Duration::from_secs(30 * 24 * 3600);
/// How much of a transcript's end is read first for its last message line.
const TAIL_BYTES: u64 = 64 * 1024;
/// The most of a transcript's end read for its last message line: a tool
/// result line carries its output twice (`content` and `toolUseResult`), so
/// one line may be far longer than [`TAIL_BYTES`].
const TAIL_MAX_BYTES: u64 = 4 << 20;
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
    /// Minutes after which an unfinished agent whose transcript ends in a
    /// disk-full tool error is not counted (default 10; 0: off).
    pub disk_full_minutes: Option<usize>,
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
    /// The folders a `worktree:NAME` resolves under (`orchestrate.worktree_roots`;
    /// none: every linked worktree of the project's repositories).
    pub worktree_roots: Vec<std::path::PathBuf>,
    /// runq's state root, for the restart point's holders (`orchestrate.runq_root`;
    /// `RUNQ_ROOT` comes first, runq's Windows default last: see
    /// [`crate::restart::runq_root`]).
    pub runq_root: Option<std::path::PathBuf>,
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
    /// An unfinished agent whose transcript ends in a disk-full tool error
    /// and that wrote nothing for this long is not counted (zero: off).
    pub disk_full: Duration,
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
    /// The worktree ledger's roots (`orchestrate.worktree_roots`).
    pub worktree_roots: Vec<std::path::PathBuf>,
    /// runq's state root (`orchestrate.runq_root`).
    pub runq_root: Option<std::path::PathBuf>,
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

/// The model's window inferred from Claude Code's environment: 200k when
/// the 1M context is turned off, else 1M. `CLAUDE_CODE_AUTO_COMPACT_WINDOW`
/// is where Claude Code compacts, not the model's window (atlas writes it
/// from its hard limit, ATLAS-CLEAR 0006), so it is never read here.
pub fn inferred_window(env: &dyn Fn(&str) -> Option<String>) -> u64 {
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
        disk_full: Duration::from_secs(o.disk_full_minutes.unwrap_or(10) as u64 * 60),
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
        worktree_roots: o.worktree_roots.clone(),
        runq_root: o.runq_root.clone(),
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
    /// No `subagent_type` and an empty brief: `other`.
    Empty,
    /// `orchestrate.default_kind`.
    Default,
}

/// An agent's kind: its brief's `Kind:` line, else its `subagent_type` in
/// the map, else a worktree agent is a builder, else an agent with no type
/// and an empty brief is `other`, else the default kind. The
/// brief is the Agent launch prompt, not a file it names.
pub fn classify(s: &OrchSettings, brief: &str, subagent_type: &str, isolation: &str) -> Kind {
    classify_source(s, brief, subagent_type, isolation).0
}

/// [`classify`], and which rule decided it.
pub fn classify_source(s: &OrchSettings, brief: &str, subagent_type: &str, isolation: &str) -> (Kind, KindSource) {
    let subagent_type = subagent_type.trim();
    if let Some(k) = brief_kind(brief) {
        return (k, KindSource::Line);
    }
    if let Some((_, k)) = s.types.iter().find(|(t, _)| !subagent_type.is_empty() && t.eq_ignore_ascii_case(subagent_type)) {
        return (*k, KindSource::Type);
    }
    if isolation.eq_ignore_ascii_case("worktree") {
        return (Kind::Builder, KindSource::Worktree);
    }
    // #196: nothing to go on (a hook input with only an agent_id) is `other`.
    if subagent_type.is_empty() && brief.trim().is_empty() {
        return (Kind::None, KindSource::Empty);
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
        KindSource::Empty => "its prompt is empty and it has no subagent_type, so it counts as other".to_string(),
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

thread_local! {
    /// Transcript tails [`ends_in_disk_full`] read on this thread (a test seam).
    static TAIL_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many transcript tails [`ends_in_disk_full`] has read on this thread
/// (tests: a quiet agent's tail is read once, not on every count).
#[doc(hidden)]
pub fn tail_reads() -> usize {
    TAIL_READS.with(std::cell::Cell::get)
}

/// Whether the `user` lines after a transcript's last `assistant` line hold
/// a tool error that says the disk is full: `ENOSPC`, "no space left" (Node,
/// Git Bash), or Windows' "not enough space on the disk" / "os error 112"
/// (Rust std, cargo, rustc, linkers), in any case. Parallel tool calls
/// answer with one result line each, so an error is seen though a good
/// result follows it (#222 R1). The lines Claude Code writes after a tool
/// result and before the model answers (a hook's `attachment`, the next
/// `api-request`, `system` lines) and a torn last line are passed over
/// (#207 review). The end is read from [`TAIL_BYTES`], further back up to
/// [`TAIL_MAX_BYTES`] while no `assistant` line is whole in it; a missing or
/// unreadable file is no.
pub fn ends_in_disk_full(transcript: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    TAIL_READS.with(|n| n.set(n.get() + 1));
    let Ok(mut f) = std::fs::File::open(transcript) else { return false };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut window = TAIL_BYTES;
    loop {
        let start = len.saturating_sub(window);
        if f.seek(SeekFrom::Start(start)).is_err() {
            return false;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return false;
        }
        let text = String::from_utf8_lossy(&buf);
        // A read from mid-file starts mid-line: its first piece is no line.
        for line in text.split('\n').skip(usize::from(start > 0)).collect::<Vec<_>>().into_iter().rev() {
            let Ok(v) = crate::json::parse(line.trim()) else { continue };
            match v.get("type").and_then(Value::as_str) {
                Some("assistant") => return false,
                Some("user") if is_disk_full_error(&v) => return true,
                _ => {}
            }
        }
        if start == 0 || window >= TAIL_MAX_BYTES {
            return false;
        }
        window = (window * 4).min(TAIL_MAX_BYTES);
    }
}

/// Whether a `user` transcript line holds a tool error that says the disk
/// is full (see [`ends_in_disk_full`]).
fn is_disk_full_error(v: &Value) -> bool {
    let Some(Value::Array(content)) = v.get("message").and_then(|m| m.get("content")) else { return false };
    let full = |t: &str| {
        let t = t.to_ascii_lowercase();
        ["enospc", "no space left", "not enough space on the disk", "os error 112"].iter().any(|w| t.contains(w))
    };
    content.iter().any(|c| {
        if c.get("type").and_then(Value::as_str) != Some("tool_result") || c.get("is_error").and_then(Value::as_bool) != Some(true) {
            return false;
        }
        match c.get("content") {
            Some(Value::String(t)) => full(t),
            Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).any(full),
            _ => false,
        }
    })
}

/// The agents folder (`<session>/subagents`) under Claude Code's projects
/// folder that holds agent `id`'s meta file, if any.
pub fn find_agent_dir(projects: &Path, id: &str) -> Option<PathBuf> {
    let meta = format!("agent-{id}.meta.json");
    std::fs::read_dir(projects).ok()?.flatten().find_map(|p| {
        std::fs::read_dir(p.path()).ok()?.flatten().map(|s| s.path().join("subagents")).find(|d| d.join(&meta).is_file())
    })
}

/// Records agent `id` (in the agents folder `dir`) finished in atlas's own
/// release record in `store` (#204, [`RELEASE_TABLE`]), for an agent whose
/// finish Claude Code never wrote (a full disk): its lane, cap slot and
/// resources are free at once, until it writes again. An agent that wrote
/// in the last [`RELEASE_QUIET`] is refused unless `force`. Claude Code's
/// meta file is never edited.
///
/// # Errors
///
/// A message: no such agent, one that may be alive, or a failed write.
pub fn release(
    store: &crate::memory::MemoryStore,
    dir: &Path,
    id: &str,
    force: bool,
    now: SystemTime,
) -> std::result::Result<String, String> {
    let id = id.strip_prefix("agent-").unwrap_or(id);
    let meta_path = dir.join(format!("agent-{id}.meta.json"));
    let Ok(text) = std::fs::read_to_string(&meta_path) else {
        return Err(format!("no agent {id} in {}", dir.display()));
    };
    let meta = crate::json::parse(&text).ok();
    let field = |k: &str| meta.as_ref().and_then(|m| m.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
    let name = match field("description") {
        d if d.is_empty() => id.to_string(),
        d => d,
    };
    if meta.as_ref().and_then(|m| m.get("finished")).and_then(Value::as_bool) == Some(true) {
        return Ok(format!("agent {id} ({name}) already finished: it holds nothing"));
    }
    let last = [modified(&meta_path), modified(&dir.join(format!("agent-{id}.jsonl")))].into_iter().flatten().max().unwrap_or(UNIX_EPOCH);
    let quiet = now.duration_since(last).unwrap_or_default();
    if !force && quiet < RELEASE_QUIET {
        return Err(format!(
            "agent {id} ({name}) wrote {} s ago and may still be running; stop it first, or pass --force to release it anyway",
            quiet.as_secs()
        ));
    }
    // Never older than its last write, whatever the file clock reads.
    record_finish(store, id, now.max(last)).map_err(|e| format!("{}: {e}", store.path().display()))?;
    Ok(format!(
        "released agent {id} ({name}): atlas counts it finished, so its lane, cap slot and resources are free. It counts again if it writes again."
    ))
}

/// A time as [`RELEASE_TABLE`] stores it: nanoseconds since the Unix epoch.
fn release_nanos(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Writes atlas's release record for agent `id` in `store`, timed `at`
/// ([`RELEASE_TABLE`]; a later record of the same agent wins): by `atlas
/// queue release`, and at a SubagentStop so that another process (`atlas
/// queue next`, `atlas backlog`, a command hook) sees that finish too (#207
/// review S2). Rows older than [`RELEASE_KEEP`] before `at` are pruned.
///
/// # Errors
///
/// [`crate::Error::StoreTooNew`] (nothing written) or [`crate::Error::Db`].
pub fn record_finish(store: &crate::memory::MemoryStore, id: &str, at: SystemTime) -> crate::Result<()> {
    let id = id.strip_prefix("agent-").unwrap_or(id);
    let conn = store.conn();
    let nanos = release_nanos(at);
    let keep = release_nanos(at.checked_sub(RELEASE_KEEP).unwrap_or(UNIX_EPOCH));
    crate::memory::guarded(conn, store.path(), || {
        conn.execute_batch(RELEASE_TABLE)?;
        conn.execute(
            "INSERT INTO agent_releases(agent, time) VALUES(?1, ?2) ON CONFLICT(agent) DO UPDATE SET time = max(time, excluded.time)",
            rusqlite::params![id, nanos],
        )?;
        conn.execute("DELETE FROM agent_releases WHERE time < ?1", [keep])?;
        Ok(())
    })
}

/// Removes agent `id`'s release record from `store`, if any: the agent
/// only waits on its own background work (an interim stop), so it still
/// holds what it held (#207 review S1); or it wrote again after it.
///
/// # Errors
///
/// [`crate::Error::StoreTooNew`] (nothing written) or [`crate::Error::Db`].
pub fn clear_finish(store: &crate::memory::MemoryStore, id: &str) -> crate::Result<()> {
    let id = id.strip_prefix("agent-").unwrap_or(id);
    if !has_release_table(store.conn())? {
        return Ok(());
    }
    let conn = store.conn();
    crate::memory::guarded(conn, store.path(), || {
        conn.execute("DELETE FROM agent_releases WHERE agent = ?1", [id])?;
        Ok(())
    })
}

/// Whether the store has [`RELEASE_TABLE`]: a read, never DDL.
fn has_release_table(conn: &rusqlite::Connection) -> crate::Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'agent_releases')", [], |r| r.get(0))?)
}

/// Every release record in `store`, by agent id (none without the table).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn released(store: &crate::memory::MemoryStore) -> crate::Result<HashMap<String, SystemTime>> {
    let conn = store.conn();
    if !has_release_table(conn)? {
        return Ok(HashMap::new());
    }
    let mut st = conn.prepare_cached("SELECT agent, time FROM agent_releases")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().map(|(a, n)| (a, UNIX_EPOCH + Duration::from_nanos(u64::try_from(n).unwrap_or(0)))).collect())
}

/// Reads the release record files of atlas 0.6.1 and earlier among
/// `entries` (of the agents folder) into `store`, then deletes each one
/// recorded. A file that cannot be recorded stays for the next pass.
fn import_release_files(store: &crate::memory::MemoryStore, entries: &[std::fs::DirEntry]) {
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(RELEASED_SUFFIX)) else { continue };
        let Some(at) = e.metadata().and_then(|m| m.modified()).ok() else { continue };
        match record_finish(store, id, at) {
            Ok(()) => {
                if let Err(err) = std::fs::remove_file(e.path()) {
                    eprintln!("atlas agent cap: {}: {err}", e.path().display());
                }
            }
            Err(err) => eprintln!("atlas agent cap: release record of {id}: {err}"),
        }
    }
}

/// The tool use id of the launch that started agent `id` (in the session
/// folder `dir`): its meta file's `toolUseId`. None when the meta file is
/// not written yet or names no id.
pub fn launch_id_of(dir: &Path, id: &str) -> Option<String> {
    meta_str(dir, id, "toolUseId")
}

/// The model alias agent `id` was launched with: its meta file's `model`
/// (`haiku`, `sonnet`, `opus`, or `inherit`). Not `pickedEffortModel`:
/// Claude Code writes the parent's model there (ruling ATLAS-VERSION 0016).
pub fn model_alias_of(dir: &Path, id: &str) -> Option<String> {
    meta_str(dir, id, "model")
}

/// A non-empty string field of agent `id`'s meta file.
fn meta_str(dir: &Path, id: &str, key: &str) -> Option<String> {
    let id = id.strip_prefix("agent-").unwrap_or(id);
    let meta = std::fs::read_to_string(dir.join(format!("agent-{id}.meta.json"))).ok().and_then(|t| crate::json::parse(&t).ok())?;
    meta.get(key).and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string)
}

/// Whether agent `id`'s meta file (in the session folder `dir`) is written.
pub fn meta_written(dir: &Path, id: &str) -> bool {
    let id = id.strip_prefix("agent-").unwrap_or(id);
    dir.join(format!("agent-{id}.meta.json")).is_file()
}

/// The launches among `ids` that the session transcript `transcript`
/// records as refused: a tool result with `is_error` for the launch's tool
/// use id, as Claude Code writes a refusal at the permission prompt. The
/// transcript is read from byte `from` (where the launches were let
/// through, or the last scan ended). Also the offset to scan from next
/// time: just past the last whole line read. A refused launch never starts.
pub fn refused_launches(transcript: &Path, from: u64, ids: &[&str]) -> (Vec<String>, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let mut out: Vec<String> = Vec::new();
    if ids.is_empty() {
        return (out, from);
    }
    let Ok(mut f) = std::fs::File::open(transcript) else { return (out, from) };
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.read_to_end(&mut buf).is_err() {
        return (out, from);
    }
    // A line still being written is read again next time.
    let whole = buf.iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
    buf.truncate(whole);
    for line in String::from_utf8_lossy(&buf).lines() {
        if !line.contains("\"is_error\"") || !line.contains("tool_result") || !ids.iter().any(|i| line.contains(i)) {
            continue;
        }
        let Ok(v) = crate::json::parse(line) else { continue };
        let Some(items) = v.pointer("/message/content").and_then(Value::as_array) else { continue };
        for it in items {
            let refused = it.get("type").and_then(Value::as_str) == Some("tool_result") && it.get("is_error").and_then(Value::as_bool) == Some(true);
            match it.get("tool_use_id").and_then(Value::as_str) {
                Some(t) if refused && ids.contains(&t) && !out.iter().any(|o| o == t) => out.push(t.to_string()),
                _ => {}
            }
        }
    }
    (out, from + whole as u64)
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
#[derive(Clone, Copy)]
pub struct Launch<'a> {
    /// The session's transcript path.
    pub transcript: &'a str,
    /// The tool call's id.
    pub tool_use_id: &'a str,
    /// The Agent tool's input.
    pub input: &'a Value,
    /// Its prompt (the brief).
    pub brief: &'a str,
    /// The memory store atlas's release records live in ([`RELEASE_TABLE`]);
    /// `None` reads none.
    pub store: Option<&'a crate::memory::MemoryStore>,
}

impl std::fmt::Debug for Launch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launch")
            .field("transcript", &self.transcript)
            .field("tool_use_id", &self.tool_use_id)
            .field("input", &self.input)
            .field("brief", &self.brief)
            .field("store", &self.store.map(crate::memory::MemoryStore::path))
            .finish()
    }
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
    /// The sub-lane each admitted launch runs as, by tool use id
    /// ([`Agents::hold_last`]): told to its agent at SubagentStart and read
    /// for its lane while it runs, never written into its prompt. Pruned in
    /// [`Agents::running`] once its agent was seen running and is no longer,
    /// or after [`LAUNCH_HOLD_TTL`] unseen; or when its refusal is seen
    /// ([`Agents::forget_launch`]).
    sub_lanes: HashMap<String, SubLane>,
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
    /// [`ends_in_disk_full`]'s answer per transcript, with the length and
    /// time it was read at: an unchanged transcript is not read again (#222 R3).
    disk_full: HashMap<PathBuf, (u64, SystemTime, bool)>,
}

/// The most transcripts [`Agents`] keeps a disk-full answer for; past it the
/// cache starts over.
const DISK_FULL_CACHE: usize = 1024;

/// The sub-lane an admitted launch runs as.
#[derive(Debug, Clone)]
struct SubLane {
    name: String,
    /// When it was admitted.
    at: Instant,
    /// Whether its agent has been seen running.
    started: bool,
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
    /// What it holds until its agent starts: its classification, its lane
    /// already its sub-lane, once the queue check set it
    /// ([`Agents::hold_last`]). Two launches of one message see each other.
    class: Option<crate::lanes::Class>,
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
            sub_lanes: HashMap::new(),
            kinds: HashMap::new(),
            finished: HashMap::new(),
            passed: HashMap::new(),
            seen: None,
            kind_refused: false,
            disk_full: HashMap::new(),
        }
    }

    /// [`ends_in_disk_full`], read again only when the transcript's length
    /// or time changed since the last read (`md`: its metadata).
    fn ends_in_disk_full_cached(&mut self, transcript: &Path, md: Option<&std::fs::Metadata>) -> bool {
        let stamp = md.and_then(|m| Some((m.len(), m.modified().ok()?)));
        if let (Some((len, at)), Some(&(l, t, full))) = (stamp, self.disk_full.get(transcript)) {
            if len == l && at == t {
                return full;
            }
        }
        let full = ends_in_disk_full(transcript);
        if let Some((len, at)) = stamp {
            if self.disk_full.len() >= DISK_FULL_CACHE {
                self.disk_full.clear();
            }
            self.disk_full.insert(transcript.to_path_buf(), (len, at, full));
        }
        full
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

    /// Records what the launch `admit` just let through holds until its
    /// agent starts: its class, and the sub-lane it runs as (the class's lane
    /// is then that sub-lane). A later launch of the same message counts it
    /// ([`Agents::pending_holders`]); its agent learns the sub-lane at its
    /// start, by its meta file's `toolUseId` ([`launch_id_of`]).
    pub fn hold_last(&mut self, mut class: crate::lanes::Class, sub_lane: Option<String>) {
        let Some(n) = self.last else { return };
        let Some(p) = self.pending.iter_mut().find(|p| p.seq == n) else { return };
        if let Some(sub) = sub_lane {
            if !p.tool_use_id.is_empty() {
                self.sub_lanes.insert(p.tool_use_id.clone(), SubLane { name: sub.clone(), at: Instant::now(), started: false });
            }
            class.lane = Some(sub);
        }
        p.class = Some(class);
    }

    /// Moves every launch admitted so far `by` into the past (tests: a
    /// launch held at the permission prompt).
    #[doc(hidden)]
    pub fn age(&mut self, by: Duration) {
        for p in &mut self.pending {
            p.at = p.at.checked_sub(by).unwrap_or(p.at);
            p.wall = p.wall.checked_sub(by).unwrap_or(p.wall);
        }
        for s in self.sub_lanes.values_mut() {
            s.at = s.at.checked_sub(by).unwrap_or(s.at);
        }
    }

    /// Forgets launch `tool_use_id`, refused at the permission prompt: it
    /// starts no agent, so it holds no slot and no sub-lane.
    pub fn forget_launch(&mut self, tool_use_id: &str) {
        self.sub_lanes.remove(tool_use_id);
        self.pending.retain(|p| p.tool_use_id != tool_use_id);
    }

    /// The sub-lane the launch with tool use id `tool_use_id` runs as.
    pub fn sub_lane(&self, tool_use_id: &str) -> Option<&str> {
        self.sub_lanes.get(tool_use_id).map(|s| s.name.as_str())
    }

    /// The launches admitted and not yet started, as holders of what their
    /// class declares (those [`Agents::hold_last`] classified), less any
    /// whose agent is already in `running` and any admitted
    /// [`CLASS_HOLD_TTL`] or longer ago.
    pub fn pending_holders(&self, running: &[Running]) -> Vec<crate::lanes::Holder> {
        let clock = Instant::now();
        self.pending
            .iter()
            .filter(|p| clock.saturating_duration_since(p.at) < CLASS_HOLD_TTL)
            .filter(|p| p.tool_use_id.is_empty() || !running.iter().any(|r| r.tool_use_id == p.tool_use_id))
            .filter_map(|p| {
                let name = if p.tool_use_id.is_empty() {
                    "a launch moments ago".to_string()
                } else {
                    format!("a launch moments ago ({})", p.tool_use_id)
                };
                Some(crate::lanes::Holder { name, kind: p.kind, class: p.class.clone()? })
            })
            .collect()
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
        let tool_use_id = launch_id_of(dir, id).unwrap_or_default();
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

    /// Forgets the finish recorded for an agent that only waits on its own
    /// background work (an interim stop): it counts again at once.
    pub fn unfinished(&mut self, id: &str) {
        self.finished.remove(id);
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
    /// brief (`agent_type` when the meta file is missing). Cached, unless
    /// there was nothing to classify it by (an empty brief and no type).
    pub fn kind_of(&mut self, s: &OrchSettings, dir: &Path, id: &str, agent_type: &str) -> Kind {
        let id = id.strip_prefix("agent-").unwrap_or(id);
        if let Some(k) = self.kinds.get(id) {
            return *k;
        }
        let meta = std::fs::read_to_string(dir.join(format!("agent-{id}.meta.json"))).ok().and_then(|t| crate::json::parse(&t).ok());
        let field = |k: &str| meta.as_ref().and_then(|m| m.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
        let (k, source) = match self.admitted.get(&field("toolUseId")) {
            Some(k) => (*k, KindSource::Line),
            None => {
                let ty = match field("agentType") {
                    t if t.is_empty() => agent_type.to_string(),
                    t => t,
                };
                let brief = brief_of(&dir.join(format!("agent-{id}.jsonl"))).unwrap_or_default();
                classify_source(s, &brief, &ty, "")
            }
        };
        // Nothing to go on yet (no meta file, no transcript): not cached, so
        // a later hook reads them once they exist.
        if source != KindSource::Empty {
            self.kinds.insert(id.to_string(), k);
        }
        k
    }

    /// The session's running agents, from the meta files in `dir`, in one
    /// pass over the folder. A missing folder is no agents.
    ///
    /// The same pass is the fallback for a lost SubagentStart: every meta
    /// file created since the oldest pending launch, in any state (running
    /// or already finished), pairs with its launch's record.
    ///
    /// atlas's release records are read from `store` ([`RELEASE_TABLE`]);
    /// the release files of atlas 0.6.1 and earlier found in the folder are
    /// read into it once and deleted, and a record its agent wrote after is
    /// removed. `None` reads no release records. A store error is logged and
    /// reads as none.
    ///
    /// # Errors
    ///
    /// I/O errors listing the folder.
    pub fn running(
        &mut self,
        s: &OrchSettings,
        dir: &Path,
        store: Option<&crate::memory::MemoryStore>,
        now: SystemTime,
    ) -> std::io::Result<Vec<Running>> {
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
        let entries: Vec<std::fs::DirEntry> = rd.flatten().collect();
        // atlas's own release records (`atlas queue release`, SubagentStop),
        // by agent id, after any old release files are read into the store.
        let released: HashMap<String, SystemTime> = match store {
            Some(st) => {
                import_release_files(st, &entries);
                released(st).unwrap_or_else(|e| {
                    eprintln!("atlas agent cap: release records: {e}");
                    HashMap::new()
                })
            }
            None => HashMap::new(),
        };
        // Records whose agent wrote after them: the agent counts again.
        let mut written_since: Vec<String> = Vec::new();
        for e in entries {
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
            let tmd = std::fs::metadata(&transcript).ok();
            let Some(last) =
                [md.as_ref().and_then(|m| m.modified().ok()), tmd.as_ref().and_then(|m| m.modified().ok())].into_iter().flatten().max()
            else {
                continue;
            };
            if released.get(id).is_some_and(|r| *r < last) {
                written_since.push(id.to_string());
            }
            if self.started.is_some_and(|st| last < st) {
                continue;
            }
            let idle = now.duration_since(last).unwrap_or_default();
            if idle > s.stale {
                continue;
            }
            if [self.finished.get(id), released.get(id)].into_iter().flatten().any(|f| *f >= last) {
                continue;
            }
            let Some(meta) = meta.or_else(|| read(&e.path())) else { continue };
            if meta.get("finished").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            // An agent that died on a full disk never gets its finish written
            // (#204): its last message line says so, and a short idle window
            // ends it. After the meta, so a finished agent costs no tail read.
            if !s.disk_full.is_zero() && idle > s.disk_full && self.ends_in_disk_full_cached(&transcript, tmd.as_ref()) {
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
        if let Some(st) = store {
            for id in written_since {
                if let Err(e) = clear_finish(st, &id) {
                    eprintln!("atlas agent cap: release record of {id}: {e}");
                }
            }
        }
        // A sub-lane is kept while its launch is pending or its agent runs,
        // and, until LAUNCH_HOLD_TTL, while its agent has not been seen (a
        // launch held at the permission prompt past PENDING_TTL): an agent
        // that ran and ended (or went stale) gives its record up.
        if !self.sub_lanes.is_empty() {
            let pending = &self.pending;
            let clock = Instant::now();
            self.sub_lanes.retain(|t, s| {
                let running = out.iter().any(|r| r.tool_use_id == *t);
                s.started |= running;
                running || pending.iter().any(|p| p.tool_use_id == *t) || (!s.started && clock.saturating_duration_since(s.at) < LAUNCH_HOLD_TTL)
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Whether a launch may start: `None` (no decision) or a deny reason.
    /// Errors give no decision.
    pub fn admit(&mut self, s: &OrchSettings, launch: &Launch<'_>, now: SystemTime, clock: Instant) -> Option<String> {
        let Launch { transcript, tool_use_id, input, brief, store } = *launch;
        self.seen = None;
        self.last = None;
        self.kind_refused = false;
        if !s.enabled || transcript.is_empty() {
            return None;
        }
        let f = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let (kind, source) = classify_source(s, brief, &f("subagent_type"), &f("isolation"));
        let dir = subagents_dir(transcript);
        let running = match self.running(s, &dir, store, now) {
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
        self.pending.push(Pending { seq: self.seq, tool_use_id: tool_use_id.to_string(), kind, at: clock, wall: now, class: None });
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
        // ATLAS-CLEAR 0006: the auto-compact window is where Claude Code
        // compacts (atlas may write it from the hard limit), not the model's
        // window, so the limits never scale to it.
        let ac = resolve(None, None, None, env(&[("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "483000")]));
        assert_eq!((ac.reviewer.handoff, ac.reviewer.limit, ac.reviewer.window), (285_000, 450_000, 1_000_000));
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

    /// #196: a subagent with no type and no brief (a PreCompact or
    /// PreToolUse input with only its agent_id) is `other`, whatever the
    /// default kind, so `orchestrate.other`'s limits apply to it.
    #[test]
    fn an_agent_with_no_type_and_no_brief_is_other() {
        let o = OrchestrateSpec { default_kind: Some(Kind::Research), ..Default::default() };
        for s in [resolve(None, None, None, env(&[])), resolve(Some(&o), None, None, env(&[]))] {
            assert_eq!(classify(&s, "", "", ""), Kind::None);
            assert_eq!(classify(&s, " \n", "", ""), Kind::None);
            // A brief or a type still decides as before.
            assert_eq!(classify(&s, "x", "", ""), s.default_kind);
            assert_eq!(classify(&s, "", "builder", ""), Kind::Builder);
            assert_eq!(classify(&s, "", "", "worktree"), Kind::Builder);
            // No meta file and no transcript: the hook input alone.
            let t = std::env::temp_dir().join("atlas-no-such-subagents-196");
            assert_eq!(Agents::new(None).kind_of(&s, &t, "a1", ""), Kind::None);
            // A blank subagent_type is no type.
            assert_eq!(classify(&s, "", " ", ""), Kind::None);
            assert_eq!(classify(&s, " ", "\t", ""), Kind::None);
        }
    }

    /// Review S2 on #201: `other` from nothing to go on is not cached, so a
    /// later hook reclassifies the agent once its meta file exists.
    #[test]
    fn an_empty_classification_is_not_cached() {
        let s = resolve(None, None, None, env(&[]));
        let t = std::env::temp_dir().join(format!("atlas-kind-not-cached-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        let mut a = Agents::new(None);
        assert_eq!(a.kind_of(&s, &t, "a1", ""), Kind::None);
        std::fs::write(t.join("agent-a1.meta.json"), r#"{"agentType": "builder"}"#).unwrap();
        assert_eq!(a.kind_of(&s, &t, "a1", ""), Kind::Builder);
        // A real classification is cached: the meta file going away changes nothing.
        std::fs::remove_file(t.join("agent-a1.meta.json")).unwrap();
        assert_eq!(a.kind_of(&s, &t, "a1", ""), Kind::Builder);
        let _ = std::fs::remove_dir_all(&t);
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
