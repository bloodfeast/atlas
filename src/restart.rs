//! The restart point (#221, rulings journal:ATLAS-RELEASE:0066 Q6 and
//! 0070): what was running when the machine went down, and the next
//! session's offer of it.
//!
//! The resident server keeps a [`Point`] current in memory (the running
//! agents, their lanes, launch keys, worktrees, head shas and dirty counts,
//! the resources their briefs declare, and the open lanes), refreshed at
//! SubagentStart and SubagentStop and every few minutes, off the hook path
//! ([`Refresher`]). Its bytes wait in the platform catch
//! (`crate::platform::shutdown`), which writes them to this server's own
//! point file beside the store ([`point_path`]) when the machine shuts
//! down, the user logs off or the terminal closes: one write, no SQLite,
//! no git.
//!
//! The next SessionStart ([`offer`]) files every server's point as one
//! `restart:<time>` record, deletes the files and offers the agents in
//! them that did not finish, with their launch keys, once. Agents of other
//! sessions that no point names (a crash, or a kill no event reached) are
//! offered from their `launch` journal entries in the same offer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::json::Value;
use crate::memory::import::front_value;
use crate::memory::MemoryStore;

/// How every point file's name starts: `restart-point-<pid>-<ms>.json`, one
/// per server, so concurrent sessions never overwrite each other's.
pub const POINT_PREFIX: &str = "restart-point";
/// The catch's log, beside the store: each catch, the stdin close, and the
/// catch's setup at server start.
pub const LOG_FILE: &str = "shutdown.log";
/// The journal entry kind (and lane) a filed restart point has: atlas's
/// own, like `launch`.
pub const RECORD: &str = "restart";
/// The most characters the SessionStart offer takes.
pub const OFFER_CHARS: usize = 600;
/// The most agents the offer names; the rest are counted.
const MAX_LISTED: usize = 4;
/// Launch entries older than this are never offered in the crash case.
const CRASH_WINDOW: Duration = Duration::from_secs(3 * 24 * 3600);
/// An agent of a point is still running elsewhere only when its transcript
/// changed this long after the point was written (nothing writes while the
/// machine is down; the slack covers the catch's own last flushes).
const CATCH_SLACK: Duration = Duration::from_secs(5);
/// In the crash case, a transcript changed this recently means another
/// session still runs the agent.
const LIVE_WINDOW: Duration = Duration::from_secs(5 * 60);
/// How often the server refreshes the point while nothing else does.
pub const REFRESH_EVERY: Duration = Duration::from_secs(180);
/// The runq state file version this reads; any other is skipped.
const RUNQ_STATE: &str = "runq-state 1";
/// runq's own default root on Windows, used when neither `RUNQ_ROOT` nor
/// `orchestrate.runq_root` names one.
pub const RUNQ_DEFAULT_ROOT: &str = "Z:/scratch/gpulock/state";
/// The lock file beside the store that one session start holds while it
/// reads and files the crash case, so two starts at once offer it once.
pub const OFFER_LOCK: &str = "restart-offer.lock";
/// A lock file that names no process this long after it was made (one cut
/// off mid-write) is taken over.
const LOCK_UNNAMED_FOR: Duration = Duration::from_secs(60);

/// This process as a launch entry's `server:` line and the offer lock name
/// it: `<pid> <start>`, its start the platform's process start time, or
/// `-` where that cannot be read (macOS), so a reused pid is not taken for
/// the same server.
pub fn server_tag() -> String {
    let pid = std::process::id();
    match crate::platform::process::start_time(pid) {
        Some(t) => format!("{pid} {t}"),
        None => format!("{pid} -"),
    }
}

/// Whether the server `tag` ([`server_tag`]) names still runs: its pid is
/// alive and, when both are known, started at the recorded time. `None`
/// for a tag that does not parse.
pub fn server_live(tag: &str) -> Option<bool> {
    let mut f = tag.split_whitespace();
    let pid: u32 = f.next()?.parse().ok()?;
    let start = f.next().unwrap_or("-");
    if !crate::platform::process::alive(pid) {
        return Some(false);
    }
    if start == "-" {
        return Some(true);
    }
    let start: u64 = start.parse().ok()?;
    // Alive but its start unreadable: taken as the same server (the safe
    // side is not to offer a running agent).
    Some(crate::platform::process::start_time(pid).is_none_or(|now| now == start))
}

/// This server's point file beside the store file `store`:
/// `restart-point-<pid>-<start ms>.json`, fixed for the process.
pub fn point_path(store: &Path) -> PathBuf {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let name = NAME.get_or_init(|| {
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        format!("{POINT_PREFIX}-{}-{ms}.json", std::process::id())
    });
    store.with_file_name(name)
}

/// [`LOG_FILE`] beside the store file `store`.
pub fn log_path(store: &Path) -> PathBuf {
    store.with_file_name(LOG_FILE)
}

/// One running agent in a restart point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentPoint {
    /// The agent id.
    pub id: String,
    /// Its launch description.
    pub description: String,
    /// Its lane (the sub-lane it runs as, else its brief's).
    pub lane: String,
    /// The key of its `launch` journal entry.
    pub launch: String,
    /// Its launch's tool use id.
    pub tool_use_id: String,
    /// The session's agents folder (its meta file and transcript).
    pub dir: String,
    /// The worktree it owns, from the worktree ledger.
    pub worktree: String,
    /// The worktree's head commit, short.
    pub head: String,
    /// The worktree's changed paths (`git status --porcelain` lines).
    pub dirty: Option<u64>,
    /// The resources its brief declares (`Uses:`).
    pub uses: Vec<String>,
}

/// What was running, as the catch writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Point {
    /// When it was prepared (UTC).
    pub at: String,
    /// The running agents.
    pub agents: Vec<AgentPoint>,
    /// The open lanes: the agents' and those with work today.
    pub lanes: Vec<String>,
    /// The runq holders under this server's Claude Code process (`owner
    /// res`), read from runq's state file ([`runq_holders`]).
    pub holders: Vec<String>,
    /// The server that prepared it ([`server_tag`]); empty when not known.
    /// A point whose server still runs is not a machine that went down (a
    /// pending restart keeps one on disk, #221), so no start takes it.
    pub server: String,
    /// Written while a restart was pending (kept current every few minutes,
    /// #221), not by a catch: its time is not when the machine went down.
    pub held: bool,
    /// How its server ended while a restart was pending, when it wrote that
    /// (`stdin closed`: a plain end of session); else empty.
    pub ended: String,
}

fn strs(v: &[String]) -> Value {
    Value::Array(v.iter().map(|s| Value::String(s.clone())).collect())
}

impl Point {
    /// The point as one line of JSON.
    pub fn to_json(&self) -> String {
        let agents: Vec<Value> = self
            .agents
            .iter()
            .map(|a| {
                crate::json!({
                    "id": a.id.as_str(),
                    "description": a.description.as_str(),
                    "lane": a.lane.as_str(),
                    "launch": a.launch.as_str(),
                    "tool_use_id": a.tool_use_id.as_str(),
                    "dir": a.dir.as_str(),
                    "worktree": a.worktree.as_str(),
                    "head": a.head.as_str(),
                    "dirty": a.dirty.map_or(Value::Null, |d| crate::json!(d)),
                    "uses": strs(&a.uses)
                })
            })
            .collect();
        let mut v = crate::json!({"version": 1, "at": self.at.as_str(), "agents": Value::Array(agents), "lanes": strs(&self.lanes), "holders": strs(&self.holders), "server": self.server.as_str()});
        // Only when set: the catch's write-through adds them to plain bytes.
        if let Value::Object(m) = &mut v {
            if self.held {
                m.insert("held".into(), Value::Bool(true));
            }
            if !self.ended.is_empty() {
                m.insert("ended".into(), Value::String(self.ended.clone()));
            }
        }
        v.to_string()
    }

    /// Reads a point [`Point::to_json`] wrote; `None` for anything else.
    pub fn parse(text: &str) -> Option<Point> {
        let v = crate::json::parse(text).ok()?;
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let list = |v: &Value, k: &str| -> Vec<String> {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        let agents = v
            .get("agents")?
            .as_array()?
            .iter()
            .map(|a| AgentPoint {
                id: s(a, "id"),
                description: s(a, "description"),
                lane: s(a, "lane"),
                launch: s(a, "launch"),
                tool_use_id: s(a, "tool_use_id"),
                dir: s(a, "dir"),
                worktree: s(a, "worktree"),
                head: s(a, "head"),
                dirty: a.get("dirty").and_then(Value::as_u64),
                uses: list(a, "uses"),
            })
            .filter(|a| !a.id.is_empty())
            .collect();
        Some(Point {
            at: s(&v, "at"),
            agents,
            lanes: list(&v, "lanes"),
            holders: list(&v, "holders"),
            server: s(&v, "server"),
            held: v.get("held").and_then(Value::as_bool) == Some(true),
            ended: s(&v, "ended"),
        })
    }
}

/// Arms the catch: the point file and log beside the store file `store`,
/// and the platform's handlers. Returns the setup line, which is also
/// logged.
pub fn arm(store: &Path) -> String {
    crate::platform::shutdown::arm(point_path(store), log_path(store));
    let line = crate::platform::shutdown::install().line();
    crate::platform::shutdown::log(&line);
    line
}

/// Appends `line` to the catch's log ([`LOG_FILE`]).
pub fn note(line: &str) {
    crate::platform::shutdown::log(line);
}

/// The server's stdin closed (logged with its time). A catch that has
/// begun is waited for, up to about 2 s, since the process exits next.
pub fn stdin_closed() {
    crate::platform::shutdown::stdin_closed();
}

/// The server is about to exit: a catch that has begun (a shutdown signal
/// that came with the end of input) is waited for, up to about 2 s, so the
/// exit does not cut its write short.
pub fn before_exit() {
    crate::platform::shutdown::wait_for_catch(crate::platform::shutdown::CATCH_WAIT);
}

/// Hands `p`'s bytes to the catch; a point with no agents leaves it nothing
/// to write.
pub fn prepare(p: &Point) {
    crate::platform::shutdown::prepare((!p.agents.is_empty()).then(|| p.to_json().into_bytes()));
}

/// Arms only the catch's slot for the store file `store` (no handler is
/// installed): tests that watch what the server prepares.
#[doc(hidden)]
pub fn arm_slot(store: &Path) {
    crate::platform::shutdown::arm(point_path(store), log_path(store));
}

/// Waits up to `timeout` for the prepared bytes to satisfy `pred` (tests:
/// it waits on each prepare, never sleeps). Whether they did.
#[doc(hidden)]
pub fn wait_prepared(timeout: Duration, pred: impl FnMut(Option<&[u8]>) -> bool) -> bool {
    crate::platform::shutdown::wait_prepared(timeout, pred)
}

/// The runq root a test set ([`set_runq_root`]).
static RUNQ_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Points the runq read at `root` instead of `RUNQ_ROOT` or runq's default
/// (tests).
#[doc(hidden)]
pub fn set_runq_root(root: PathBuf) {
    *RUNQ_ROOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(root);
}

/// This server's parent process (Claude Code), read once.
#[doc(hidden)]
pub fn server_parent() -> Option<u32> {
    static PARENT: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *PARENT.get_or_init(crate::platform::process::parent)
}

/// The holders in runq's state file text `text` (`owner res` each): its
/// `entry` lines with `held=1` whose `chain` of ancestor pids holds
/// `parent` (this server's Claude Code process). With the parent unknown
/// there are none: another session's holders are not this one's. A text of
/// any other version than [`RUNQ_STATE`] gives none: runq's format is its
/// own, so a newer one is skipped, never guessed at.
pub fn runq_holders_in(text: &str, parent: Option<u32>) -> Vec<String> {
    let Some(p) = parent else { return Vec::new() };
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some(RUNQ_STATE) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for l in lines {
        let mut f = l.split('\t');
        if f.next() != Some("entry") {
            continue;
        }
        let kv: HashMap<&str, &str> = f.filter_map(|x| x.split_once('=')).collect();
        if kv.get("held") != Some(&"1") {
            continue;
        }
        let chain = kv.get("chain").copied().unwrap_or("");
        if !chain.split(',').any(|x| x.split(':').next().and_then(|n| n.parse::<u32>().ok()) == Some(p)) {
            continue;
        }
        let (owner, res) = (kv.get("owner").copied().unwrap_or(""), kv.get("res").copied().unwrap_or(""));
        if !owner.is_empty() || !res.is_empty() {
            out.push(format!("{owner} {res}"));
        }
    }
    out
}

/// The runq holders in the state file under runq root `root`; none when it
/// is missing or unreadable.
pub fn runq_holders_at(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("state")).map(|t| runq_holders_in(&t, server_parent())).unwrap_or_default()
}

/// The runq root the holders are read from: the root a test set, else the
/// `RUNQ_ROOT` environment variable (as runq itself reads it), else
/// `configured` (`orchestrate.runq_root`), else runq's default
/// [`RUNQ_DEFAULT_ROOT`] on Windows; elsewhere none (no holders are read).
pub fn runq_root(configured: Option<&Path>) -> Option<PathBuf> {
    let set = RUNQ_ROOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    set.or_else(|| std::env::var_os("RUNQ_ROOT").filter(|r| !r.is_empty()).map(PathBuf::from))
        .or_else(|| configured.map(Path::to_path_buf))
        .or_else(|| cfg!(windows).then(|| PathBuf::from(RUNQ_DEFAULT_ROOT)))
}

/// The runq holders now, from [`runq_root`]. Read in the [`Refresher`]
/// thread, never on a hook's path or in the catch.
pub fn runq_holders(configured: Option<&Path>) -> Vec<String> {
    runq_root(configured).map(|r| runq_holders_at(&r)).unwrap_or_default()
}

/// Runs `git -C dir ARGS`; its stdout, on success.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let mut c = std::process::Command::new("git");
    c.arg("-C").arg(dir).args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let o = c.output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Fills each agent's head sha and dirty count from its worktree.
pub fn fill_git(p: &mut Point) {
    for a in &mut p.agents {
        if a.worktree.is_empty() || !Path::new(&a.worktree).is_dir() {
            continue;
        }
        a.head = git(&a.worktree, &["rev-parse", "--short=12", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_default();
        a.dirty = git(&a.worktree, &["status", "--porcelain"]).map(|s| s.lines().filter(|l| !l.trim().is_empty()).count() as u64);
    }
}

/// The thread that turns each point the hook worker sends into the catch's
/// bytes, reading git on the way, so no hook waits on git.
#[derive(Debug)]
pub struct Refresher {
    tx: mpsc::Sender<Point>,
}

impl Refresher {
    /// Starts the thread, reading runq's holders under `runq_root`
    /// (`orchestrate.runq_root`; see [`runq_root`]); `None` if it cannot
    /// start.
    pub fn start(runq_root: Option<PathBuf>) -> Option<Refresher> {
        let (tx, rx) = mpsc::channel::<Point>();
        std::thread::Builder::new()
            .name("atlas-restart-point".into())
            .spawn(move || {
                while let Ok(mut p) = rx.recv() {
                    // Only the newest point matters.
                    while let Ok(newer) = rx.try_recv() {
                        p = newer;
                    }
                    fill_git(&mut p);
                    if !p.agents.is_empty() {
                        p.holders = runq_holders(runq_root.as_deref());
                    }
                    prepare(&p);
                }
            })
            .ok()?;
        Some(Refresher { tx })
    }

    /// Sends a point to prepare.
    pub fn send(&self, p: Point) {
        let _ = self.tx.send(p);
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for `t`.
fn utc(t: SystemTime) -> String {
    crate::discussion::civil(t.duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)))
}

/// `YYYY-MM-DDTHH:MM:SS.000Z` for `t`: the form of the store's times, so
/// the two compare as strings.
fn utc_ms(t: SystemTime) -> String {
    let s = utc(t);
    format!("{}.000Z", &s[..s.len() - 1])
}

/// Whether agent `a` did not finish: no release record in `store`, its meta
/// file (when its folder is known) does not say finished, and it is not
/// still running elsewhere (its transcript changed after `live_after`).
fn unfinished(released: &HashMap<String, SystemTime>, a: &AgentPoint, live_after: SystemTime) -> bool {
    if released.contains_key(&a.id) {
        return false;
    }
    if a.dir.is_empty() {
        return true;
    }
    let dir = Path::new(&a.dir);
    let meta = std::fs::read_to_string(dir.join(format!("agent-{}.meta.json", a.id))).ok().and_then(|t| crate::json::parse(&t).ok());
    if meta.as_ref().and_then(|m| m.get("finished")).and_then(Value::as_bool) == Some(true) {
        return false;
    }
    !last_write(a).is_some_and(|t| t > live_after)
}

/// When agent `a`'s transcript last changed, when its folder is known.
fn last_write(a: &AgentPoint) -> Option<SystemTime> {
    if a.dir.is_empty() {
        return None;
    }
    std::fs::metadata(Path::new(&a.dir).join(format!("agent-{}.jsonl", a.id))).and_then(|m| m.modified()).ok()
}

/// Files `agents` (the unfinished ones, from `source`) as a
/// `restart:<time>` record, with the runq `holders` the points named; its
/// key. Its `offered:` line (`now`, Unix milliseconds) is when the agents
/// were offered: an agent whose transcript changed after it was still
/// running, so the offer was wrong and does not hide its later end.
fn file_record(
    store: &mut MemoryStore,
    source: &str,
    at: &str,
    now: SystemTime,
    agents: &[AgentPoint],
    holders: &[String],
) -> crate::Result<String> {
    let stamp: String = crate::memory::now().chars().filter(char::is_ascii_alphanumeric).collect();
    let mut body = String::new();
    for a in agents {
        body.push_str(&format!("- {}\n", describe(a)));
    }
    if !holders.is_empty() {
        body.push_str(&format!("runq holders at the catch: {}\n", holders.join(", ")));
    }
    let d = crate::memory::Draft {
        title: format!("restart point: {} unfinished agent{} ({source})", agents.len(), if agents.len() == 1 { "" } else { "s" }),
        meta: format!(
            "source: {source}\nat: {at}\noffered: {}\nagents: {}\n",
            now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis()),
            agents.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")
        ),
        body,
        reason: "the restart point a session start imported (#221)".into(),
        author: "hook".into(),
        client: "atlas-hook".into(),
        provenance: None,
        time: None,
    };
    let mut w = store.writer()?;
    let mut key = format!("{RECORD}:{stamp}");
    let mut n = 1;
    while w.head(&key).is_ok() {
        n += 1;
        key = format!("{RECORD}:{stamp}-{n}");
    }
    w.create(&key, "journal", RECORD, RECORD, &d, None)?;
    w.commit()?;
    Ok(key)
}

/// One agent as the record and the offer name it.
fn describe(a: &AgentPoint) -> String {
    let mut parts = vec![format!("agent {}", a.id)];
    if !a.lane.is_empty() {
        parts.push(format!("lane {}", a.lane));
    }
    if !a.launch.is_empty() {
        parts.push(format!("launch {}", a.launch));
    }
    if !a.worktree.is_empty() {
        let mut w = format!("worktree {}", a.worktree);
        if !a.head.is_empty() {
            w.push_str(&format!(" at {}", a.head));
        }
        if let Some(d) = a.dirty {
            w.push_str(&format!(", {d} dirty"));
        }
        parts.push(w);
    }
    let name = if a.description.is_empty() { String::new() } else { format!("{} ", a.description) };
    format!("{name}({})", parts.join(", "))
}

/// The offer line, at most [`OFFER_CHARS`] characters.
fn offer_line(key: &str, why: &str, agents: &[AgentPoint]) -> String {
    let n = agents.len();
    let head = format!("atlas: {why}, {n} agent{} did not finish ({key}):", if n == 1 { "" } else { "s" });
    let tail = " Ask the user whether to relaunch them from their launch briefs (recall each launch key).";
    let mut listed = Vec::new();
    for a in agents.iter().take(MAX_LISTED) {
        let more = n - listed.len() - 1;
        let extra = if more > 0 { format!("; +{more} more in {key}") } else { String::new() };
        let next = describe(a);
        let len = head.chars().count()
            + 1
            + listed.iter().map(|l: &String| l.chars().count() + 2).sum::<usize>()
            + next.chars().count()
            + extra.chars().count()
            + tail.chars().count();
        if len > OFFER_CHARS && !listed.is_empty() {
            break;
        }
        listed.push(next);
    }
    let rest = n - listed.len();
    let mut line = format!("{head} {}", listed.join("; "));
    if rest > 0 {
        line.push_str(&format!("; +{rest} more in {key}"));
    }
    line.push('.');
    line.push_str(tail);
    if line.chars().count() > OFFER_CHARS {
        line = line.chars().take(OFFER_CHARS - 3).collect::<String>() + "...";
    }
    line
}

/// A point file this start took: its point, when it was written (the
/// catch), and where it now lies.
struct Claimed {
    point: Point,
    caught: SystemTime,
    path: PathBuf,
}

/// Takes every server's point file beside the store file `store`. Each is
/// first renamed to a name of this process's (atomic), so of two session
/// starts at once only one imports it. A file another start claimed whose
/// process is gone (it died between its claim and its delete) is claimed
/// again the same way. A file that does not parse is deleted and logged. A
/// point whose server still runs ([`Point::server`]) is left where it is.
/// A point kept while a restart was pending ([`Point::held`]) is taken only
/// when the machine booted after it was written (`boot`,
/// [`booted_since`]): else the machine never went down (its server ended
/// or died, #221 S2), so it is deleted, logged, and its agents are left to
/// the crash case, which names that right.
fn claim_points(store: &Path, now: SystemTime, boot: Option<SystemTime>) -> Vec<Claimed> {
    let Some(dir) = store.parent() else { return Vec::new() };
    let me = std::process::id();
    // (the path, its name as the catch wrote it)
    let mut found: Vec<(PathBuf, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_string();
            if !n.starts_with(POINT_PREFIX) {
                return None;
            }
            if n.ends_with(".json") {
                return Some((e.path(), n));
            }
            let (base, pid) = n.rsplit_once(".claimed-")?;
            let pid: u32 = pid.parse().ok()?;
            (base.ends_with(".json") && pid != me && !crate::platform::process::alive(pid)).then(|| (e.path(), base.to_string()))
        })
        .collect();
    found.sort();
    let mut out = Vec::new();
    for (path, base) in found {
        let live = std::fs::read_to_string(&path)
            .ok()
            .as_deref()
            .and_then(Point::parse)
            .is_some_and(|p| !p.server.is_empty() && server_live(&p.server) == Some(true));
        if live {
            continue;
        }
        let taken = path.with_file_name(format!("{base}.claimed-{me}"));
        if std::fs::rename(&path, &taken).is_err() {
            continue;
        }
        let caught = std::fs::metadata(&taken).and_then(|m| m.modified()).unwrap_or(now);
        match std::fs::read_to_string(&taken).ok().as_deref().and_then(Point::parse) {
            Some(point) if point.held && !booted_since(caught, boot) => {
                let how = if point.ended.is_empty() { String::new() } else { format!(", ended by {}", point.ended) };
                note(&format!(
                    "restart point {base} (kept while a restart was pending{how}, written {}) left to the crash case: no reboot since",
                    utc(caught)
                ));
                let _ = std::fs::remove_file(&taken);
            }
            Some(point) => out.push(Claimed { point, caught, path: taken }),
            None => {
                eprintln!("atlas restart point: {} does not parse; deleted", path.display());
                let _ = std::fs::remove_file(&taken);
            }
        }
    }
    out
}

/// Whether the machine booted (at `boot`, when known) after `written`: a
/// point written then is from a machine that went down since.
pub fn booted_since(written: SystemTime, boot: Option<SystemTime>) -> bool {
    boot.is_some_and(|b| b > written)
}

/// SessionStart: the restart point's offer, once ([`offer_at`] with this
/// machine's boot time).
///
/// # Errors
///
/// A store error (the claimed files are then put back).
pub fn offer(store: &mut MemoryStore, session: &str, now: SystemTime) -> crate::Result<Option<String>> {
    offer_at(store, session, now, crate::platform::boot::boot_time())
}

/// SessionStart: the restart point's offer, once, with the machine's boot
/// time `boot` (`None` when not known).
///
/// Under a lock ([`OFFER_LOCK`]): a start that finds it held by another
/// live start offers nothing. Takes every server's point file beside the
/// store (one per server, so concurrent sessions each leave their own) and
/// offers the agents in them
/// that did not finish. An agent of a point is still running elsewhere,
/// and not offered, only when its transcript changed after the point was
/// written; of a point kept while a restart was pending ([`Point::held`],
/// taken only after a reboot), only when it changed after the boot. Then the
/// crash case, never hidden by a point, read and filed
/// under the same lock so two starts at once offer it once: the
/// active `launch` entries of sessions other than `session` whose agents no
/// point names, did not finish, were not offered before and are not running
/// (see `crash_agents`). All of them are filed as one `restart:<time>`
/// record, the files are deleted, and one line offers them.
///
/// # Errors
///
/// A store error (the claimed files are then put back).
pub fn offer_at(store: &mut MemoryStore, session: &str, now: SystemTime, boot: Option<SystemTime>) -> crate::Result<Option<String>> {
    let released = crate::orchestrate::released(store)?;
    // One start at a time claims the points and reads and files the crash
    // case, in one pass; a start that finds the lock held leaves both to the
    // holder (else one agent could be offered from a point here and as a
    // crash there).
    let Some(_lock) = OfferLock::take(store.path(), now) else { return Ok(None) };
    let claimed = claim_points(store.path(), now, boot);
    let put_back = |claimed: &[Claimed]| {
        for c in claimed {
            if let Some(n) = c.path.file_name().and_then(|n| n.to_str()).and_then(|n| n.rsplit_once(".claimed-")).map(|x| x.0) {
                let _ = std::fs::rename(&c.path, c.path.with_file_name(n));
            }
        }
    };
    let mut covered: HashSet<String> = HashSet::new();
    let mut caught: Vec<String> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    let mut holders: Vec<String> = Vec::new();
    let mut open: Vec<AgentPoint> = Vec::new();
    let (mut from_caught, mut from_kept) = (0, 0);
    for c in &claimed {
        // A catch's point: written as the machine went down, so a transcript
        // changed after it runs elsewhere. A kept one (rewritten every few
        // minutes while a restart was pending, and the catch missed the
        // end): agents wrote on after it until the machine went down, so
        // only a change after the boot runs elsewhere (#221 S1).
        let live_after = match boot.filter(|_| c.point.held) {
            Some(b) => b.max(c.caught),
            None => c.caught + CATCH_SLACK,
        };
        if c.point.held { &mut kept } else { &mut caught }.push(utc(c.caught));
        holders.extend(c.point.holders.iter().cloned());
        for a in &c.point.agents {
            if covered.insert(a.id.clone()) && unfinished(&released, a, live_after) {
                open.push(a.clone());
                if c.point.held {
                    from_kept += 1;
                } else {
                    from_caught += 1;
                }
            }
        }
    }
    let from_points = open.len();
    // The crash case, read before this start files its own record.
    let crashed = match crash_agents(store, session, now, &released, &mut covered) {
        Ok(a) => a,
        Err(e) => {
            put_back(&claimed);
            return Err(e);
        }
    };
    open.extend(crashed);
    if claimed.is_empty() && open.is_empty() {
        return Ok(None);
    }
    holders.sort();
    holders.dedup();
    caught.sort();
    kept.sort();
    let mut why = Vec::new();
    if from_caught > 0 {
        why.push(format!("the server caught a shutdown, logoff, closed terminal or kill at {}", caught.last().map_or("", String::as_str)));
    }
    if from_kept > 0 {
        why.push(format!(
            "the machine restarted while a restart was pending (restart point kept current to {})",
            kept.last().map_or("", String::as_str)
        ));
    }
    if open.len() > from_points {
        why.push("a session ended with no restart point (a crash or a kill)".to_string());
    }
    let source = match (claimed.is_empty(), open.len() > from_points) {
        (false, true) => "shutdown and crash",
        (false, false) => "shutdown",
        _ => "crash",
    };
    caught.extend(kept);
    let at = if caught.is_empty() { utc(now) } else { caught.join(", ") };
    let key = match file_record(store, source, &at, now, &open, &holders) {
        Ok(k) => k,
        Err(e) => {
            put_back(&claimed);
            return Err(e);
        }
    };
    for c in &claimed {
        if let Err(e) = std::fs::remove_file(&c.path) {
            eprintln!("atlas restart point: {}: {e}", c.path.display());
        }
    }
    if open.is_empty() {
        return Ok(None);
    }
    Ok(Some(offer_line(&key, &why.join("; and "), &open)))
}

/// The crash case's agents: the active `launch` entries of the last
/// [`CRASH_WINDOW`], of every session other than `session`, whose agent is
/// not in `covered` (which they join), did not finish and is not running:
/// - the server that filed the entry (its `server:` line, pid and start
///   time) still runs, so its session does too: not offered;
/// - a restart record already offered the agent (`offered:`, `agents:`):
///   not offered again, unless its transcript changed after that offer (it
///   was running then, so the offer was wrong and its real end is new);
/// - its transcript changed in the last [`LIVE_WINDOW`] (an entry with no
///   `server:` line, or a server gone while its Claude Code runs on).
fn crash_agents(
    store: &MemoryStore,
    session: &str,
    now: SystemTime,
    released: &HashMap<String, SystemTime>,
    covered: &mut HashSet<String>,
) -> crate::Result<Vec<AgentPoint>> {
    // Each agent's latest offer. A record without `offered:` counts as made
    // now: it is never undone.
    let mut offered: HashMap<String, SystemTime> = HashMap::new();
    for h in store.heads(Some("journal"), Some(RECORD))?.into_iter().filter(|h| h.sub == RECORD) {
        let at = front_value(&h.meta, "offered")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(now, |ms| UNIX_EPOCH + Duration::from_millis(ms));
        for id in front_value(&h.meta, "agents").unwrap_or("").split(',').map(str::trim).filter(|i| !i.is_empty()) {
            let e = offered.entry(id.to_string()).or_insert(at);
            *e = (*e).max(at);
        }
    }
    let floor = utc_ms(now.checked_sub(CRASH_WINDOW).unwrap_or(UNIX_EPOCH));
    let live_after = now.checked_sub(LIVE_WINDOW).unwrap_or(UNIX_EPOCH);
    let mut out = Vec::new();
    for h in store.active_entries_no_body(crate::hooks::LAUNCH_ENTRY)?.into_iter().filter(|h| h.time > floor) {
        let s = front_value(&h.meta, "session").unwrap_or("");
        let id = front_value(&h.meta, "agent").unwrap_or("").to_string();
        if s.is_empty() || s == session || id.is_empty() || !covered.insert(id.clone()) {
            continue;
        }
        if front_value(&h.meta, "server").and_then(server_live) == Some(true) {
            continue;
        }
        let description = front_value(&h.meta, "description").unwrap_or("").to_string();
        let dir = front_value(&h.meta, "transcript")
            .filter(|t| !t.is_empty())
            .map(|t| crate::orchestrate::subagents_dir(t).to_string_lossy().into_owned())
            .unwrap_or_default();
        let a = AgentPoint { id, description, lane: h.scope.clone(), launch: h.key.clone(), dir, ..AgentPoint::default() };
        if let Some(&at) = offered.get(&a.id) {
            if !last_write(&a).is_some_and(|t| t > at + CATCH_SLACK) {
                continue;
            }
        }
        if unfinished(released, &a, live_after) {
            out.push(a);
        }
    }
    Ok(out)
}

/// The crash case's lock ([`OFFER_LOCK`]), held while one start reads and
/// files it; removed on drop.
struct OfferLock {
    path: PathBuf,
}

impl OfferLock {
    /// Takes the lock beside the store file `store`: made new with this
    /// process's [`server_tag`]. A stale lock is taken over: its process is
    /// gone, it names this process (offers in one process run one at a
    /// time, so it is a lock a failed unlink left), or it names none
    /// [`LOCK_UNNAMED_FOR`] after it was made. `None` while another live
    /// start holds it.
    fn take(store: &Path, now: SystemTime) -> Option<OfferLock> {
        use std::io::Write;
        let path = store.with_file_name(OFFER_LOCK);
        let me = server_tag();
        let mut denied = 0;
        for _ in 0..8 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let _ = f.write_all(me.as_bytes());
                    return Some(OfferLock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let held = std::fs::read_to_string(&path).unwrap_or_default();
                    if !lock_stale(&path, &held, &me, now) {
                        return None;
                    }
                    if !take_over(&path, &held) {
                        return None;
                    }
                }
                // Windows: a lock another start is deleting (delete pending)
                // denies a new one for a moment. Held, unless it lasts (then
                // the folder itself refuses, as below).
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && denied < 4 => {
                    denied += 1;
                    if path.exists() {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => {
                    // No lock can be made (a read-only folder): go on alone
                    // rather than never offer.
                    eprintln!("atlas restart point: {}: {e}", path.display());
                    return Some(OfferLock { path: PathBuf::new() });
                }
            }
        }
        None
    }
}

/// Whether the lock at `path`, which reads `held`, is stale for the start
/// whose tag is `me` (see [`OfferLock::take`]).
fn lock_stale(path: &Path, held: &str, me: &str, now: SystemTime) -> bool {
    if held.trim() == me {
        return true;
    }
    match server_live(held) {
        Some(live) => !live,
        None => std::fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).is_ok_and(|d| d > LOCK_UNNAMED_FOR)),
    }
}

/// Takes the stale lock at `path`, which read `held`, out of the way: it is
/// renamed to a name of this process's, so of two starts that judged it
/// stale only one moves it. If what moved is not the lock judged stale
/// (another start took it over in between), it is put back and the start
/// stands down. Whether the way is clear for a new lock.
fn take_over(path: &Path, held: &str) -> bool {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let aside = path.with_file_name(format!("{OFFER_LOCK}.stale-{}-{n}", std::process::id()));
    if std::fs::rename(path, &aside).is_err() {
        // Another start moved it first; try again from the start.
        return true;
    }
    if std::fs::read_to_string(&aside).unwrap_or_default() != held {
        let _ = std::fs::rename(&aside, path);
        return false;
    }
    if let Err(e) = std::fs::remove_file(&aside) {
        eprintln!("atlas restart point: {}: {e}", aside.display());
    }
    true
}

impl Drop for OfferLock {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            if let Err(e) = std::fs::remove_file(&self.path) {
                eprintln!("atlas restart point: {}: {e}", self.path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Point {
        Point {
            at: "2026-11-16T20:00:00Z".into(),
            agents: vec![AgentPoint {
                id: "a1".into(),
                description: "build the walrus".into(),
                lane: "WALRUS".into(),
                launch: "journal:WALRUS:0001".into(),
                tool_use_id: "t1".into(),
                dir: "/x/subagents".into(),
                worktree: "/w/walrus".into(),
                head: "abc123".into(),
                dirty: Some(3),
                uses: vec!["compile".into()],
            }],
            lanes: vec!["WALRUS".into()],
            holders: vec!["atlas-w4d light-compile".into()],
            server: "4294967290 1".into(),
            ..Point::default()
        }
    }

    #[test]
    fn a_point_reads_back_as_written() {
        let p = sample();
        assert_eq!(Point::parse(&p.to_json()), Some(p));
        assert_eq!(Point::parse("not json"), None);
    }

    #[test]
    fn the_offer_stays_within_its_budget() {
        let mut agents = Vec::new();
        for i in 0..20 {
            let mut a = sample().agents[0].clone();
            a.id = format!("agent-number-{i}");
            a.description = "x".repeat(80);
            agents.push(a);
        }
        let line = offer_line(
            "restart:20261116T200000000Z",
            "the server caught a shutdown, logoff, closed terminal or kill at 2026-11-16T20:00:00Z",
            &agents,
        );
        assert!(line.chars().count() <= OFFER_CHARS, "{} chars: {line}", line.chars().count());
        assert!(line.contains("agent agent-number-0") && line.contains("more in restart:"), "{line}");
        let one = offer_line("restart:k", "why", &sample().agents);
        assert!(
            one.contains("1 agent did not finish") && one.contains("launch journal:WALRUS:0001") && one.contains("at abc123, 3 dirty"),
            "{one}"
        );
    }

    /// The catch, as the console handler and the signal thread call it:
    /// with agents running it writes the point; with none it writes nothing.
    /// One test, since the catch's slot is process-wide.
    #[test]
    fn the_catch_writes_the_running_agents_and_nothing_without_them() {
        let dir = std::env::temp_dir().join(format!("atlas-catch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("memory.sqlite");
        let (point, log) = (point_path(&store), log_path(&store));
        let _ = std::fs::remove_file(&point);
        crate::platform::shutdown::arm(point.clone(), log.clone());
        prepare(&Point { agents: Vec::new(), ..sample() });
        assert!(!crate::platform::shutdown::catch("TEST-NONE"));
        assert!(!point.exists(), "nothing runs: nothing is written");
        prepare(&sample());
        // W4d-2, through the poll's own `on_change`: a pending restart
        // writes the point at once, marked kept, keeps it current and leaves
        // the warning; with no agents the point is removed; cleared, it is
        // removed and an untold warning dropped.
        use crate::restart_warn::{on_change, reboot_required, Change, Notices};
        let pending = reboot_required(Some(""), None).unwrap();
        let notices = Notices::default();
        let read = || Point::parse(&std::fs::read_to_string(&point).unwrap()).unwrap();
        on_change(&Change::Pending(pending.clone()), &notices);
        assert!(point.exists(), "a pending restart did not write the point");
        assert!(read().held, "a kept point is not marked kept: {:?}", read());
        prepare(&Point { agents: Vec::new(), ..sample() });
        assert!(!point.exists(), "a point with no agents was kept");
        prepare(&sample());
        assert!(point.exists(), "the point did not follow the prepare");
        assert_eq!(read().agents[0].id, "a1");
        on_change(&Change::Cleared, &notices);
        assert!(!point.exists(), "a cleared restart left its point");
        assert!(notices.take().is_none(), "a cleared restart left its untold warning");
        on_change(&Change::Pending(pending), &notices);
        assert!(notices.take().is_some(), "a pending restart left no warning");
        on_change(&Change::Cleared, &notices);
        #[cfg(windows)]
        {
            // S4: a shutdown that is cancelled after the query leaves no
            // point that later reads as a machine that went down.
            use crate::platform::shutdown::on_message;
            const WM_QUERYENDSESSION: u32 = 0x0011;
            const WM_ENDSESSION: u32 = 0x0016;
            assert_eq!(on_message(WM_QUERYENDSESSION, 1), Some(1), "the query is let through");
            assert!(point.exists(), "the query writes the point");
            let answer = on_message(WM_ENDSESSION, 0);
            assert!(!point.exists(), "a cancelled end of session left its restart point");
            assert_eq!(answer, Some(0));
            const CTRL_SHUTDOWN_EVENT: u32 = 6;
            assert_eq!(crate::platform::shutdown::on_ctrl(CTRL_SHUTDOWN_EVENT), 0, "the event goes on to the default handler");
        }
        #[cfg(not(windows))]
        assert!(crate::platform::shutdown::catch("SIGTERM"));
        let got = Point::parse(&std::fs::read_to_string(&point).unwrap()).unwrap();
        assert_eq!(got.agents[0].id, "a1");
        assert!(!got.held, "the catch's write is marked kept");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("no agents running: nothing written") && logged.contains("restart point written"), "{logged}");
        // The stdin state is process-wide: another test's server may have
        // seen its stdin close, so only the cause is checked here.
        if cfg!(windows) {
            assert!(logged.contains("CTRL_SHUTDOWN; stdin "), "{logged}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #221 W4d-2: a point whose server still runs (one a pending restart
    /// keeps on disk) is not taken by a session start; one whose server is
    /// gone is.
    #[test]
    fn a_point_of_a_running_server_is_not_claimed() {
        let dir = std::env::temp_dir().join(format!("atlas-claim-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("memory.sqlite");
        let (live, gone) = (dir.join("restart-point-1-1.json"), dir.join("restart-point-2-1.json"));
        std::fs::write(&live, Point { server: server_tag(), ..sample() }.to_json()).unwrap();
        std::fs::write(&gone, sample().to_json()).unwrap();
        let got = claim_points(&store, SystemTime::now(), None);
        assert_eq!(got.len(), 1, "a running server's point was claimed");
        assert!(live.exists(), "a running server's point was moved");
        assert!(!gone.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// runq's holders: held entries only, under this server's Claude Code
    /// process when it is known; a newer state version, an empty text and
    /// a missing file give none.
    #[test]
    fn runq_holders_are_read_from_its_state_file() {
        let text = "runq-state 1\nmeta\tnext_id=4\n\
            entry\tid=1\towner=atlas-w4d\tres=light-compile\theld=1\tchain=10:1:bash,44916:2:claude\n\
            entry\tid=2\towner=atlas-other\tres=linux-test\theld=1\tchain=11:1:bash,555:2:claude\n\
            entry\tid=3\towner=atlas-wait\tres=heavy-compile\theld=0\tchain=12:1:bash,44916:2:claude\n";
        assert_eq!(runq_holders_in(text, Some(44916)), vec!["atlas-w4d light-compile".to_string()]);
        assert!(runq_holders_in(text, None).is_empty(), "an unknown parent records no holders: {:?}", runq_holders_in(text, None));
        assert!(runq_holders_in(&text.replace("runq-state 1", "runq-state 2"), Some(44916)).is_empty(), "a newer version was read");
        assert!(runq_holders_in("", Some(44916)).is_empty());
        let missing = std::env::temp_dir().join(format!("atlas-runq-none-{}", std::process::id()));
        assert!(runq_holders_at(&missing).is_empty());
    }

    /// R1: a server tag names this process while it runs; the same pid with
    /// another start time (a reused pid) or a pid no process has does not.
    #[test]
    fn a_server_tag_is_live_only_for_the_same_process() {
        let me = server_tag();
        assert_eq!(server_live(&me), Some(true), "{me}");
        let (pid, start) = me.split_once(' ').unwrap();
        if let Ok(start) = start.parse::<u64>() {
            assert_eq!(server_live(&format!("{pid} {}", start + 1)), Some(false), "a reused pid was taken for the server");
        }
        assert_eq!(server_live("4294967290 1"), Some(false));
        assert_eq!(server_live("4294967290 -"), Some(false));
        assert_eq!(server_live("nonsense"), None);
    }

    /// Q9: the end-session window only where the console events cannot
    /// come (no console, or user32 loaded).
    #[cfg(windows)]
    #[test]
    fn the_end_session_window_is_made_only_where_console_events_cannot_come() {
        use crate::platform::shutdown::wants_window;
        assert!(!wants_window(true, false));
        assert!(wants_window(true, true));
        assert!(wants_window(false, false));
        assert!(wants_window(false, true));
    }

    /// Not a test: the child of the signal and stdin tests. Arms the catch
    /// in the folder `ATLAS_CATCH_HELPER` names, prepares a point with one
    /// agent, says `ready`, and reads stdin to its end, then notes the close
    /// and exits 0, as the server does. With `ATLAS_CATCH_HOLD` set, a catch
    /// says `catching` and then takes 500 ms before it writes. With
    /// `ATLAS_CATCH_PENDING` set, a restart is pending (the point is kept).
    #[test]
    #[ignore = "not a test: the catch's child process"]
    fn catch_helper() {
        use std::io::{Read, Write};
        let Some(dir) = std::env::var_os("ATLAS_CATCH_HELPER") else { return };
        if std::env::var_os("ATLAS_CATCH_HOLD").is_some() {
            fn hold() {
                let mut out = std::io::stdout();
                let _ = writeln!(out, "catching");
                let _ = out.flush();
                std::thread::sleep(Duration::from_millis(500));
            }
            *crate::platform::shutdown::BEFORE_WRITE.lock().unwrap() = Some(hold);
        }
        let store = PathBuf::from(dir).join("memory.sqlite");
        let _ = arm(&store);
        prepare(&sample());
        if std::env::var_os("ATLAS_CATCH_PENDING").is_some() {
            crate::platform::shutdown::hold(true, "TEST-PENDING");
        }
        let mut out = std::io::stdout();
        let _ = writeln!(out, "ready");
        let _ = out.flush();
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
        stdin_closed();
        std::process::exit(0);
    }

    /// The point files in `dir` (a helper child names its own).
    fn points_in(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(POINT_PREFIX)))
            .collect()
    }

    /// A helper child (see `catch_helper`), ready, and its stdout.
    fn helper(dir: &Path, hold: bool) -> (std::process::Child, std::io::BufReader<std::process::ChildStdout>) {
        helper_with(dir, hold, false)
    }

    /// [`helper`], with a restart pending when `pending`.
    fn helper_with(dir: &Path, hold: bool, pending: bool) -> (std::process::Child, std::io::BufReader<std::process::ChildStdout>) {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--ignored", "--exact", "restart::tests::catch_helper", "--nocapture", "--quiet", "--test-threads", "1"])
            .env("ATLAS_CATCH_HELPER", dir);
        if hold {
            cmd.env("ATLAS_CATCH_HOLD", "1");
        }
        if pending {
            cmd.env("ATLAS_CATCH_PENDING", "1");
        }
        let mut c = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut r = std::io::BufReader::new(c.stdout.take().unwrap());
        let mut line = String::new();
        while r.read_line(&mut line).unwrap() > 0 && !line.contains("ready") {
            line.clear();
        }
        assert!(line.contains("ready"), "the helper never got ready");
        (c, r)
    }

    /// Sends SIGTERM to `c` (kill(1), not a raw extern: the crate denies
    /// unsafe code).
    #[cfg(unix)]
    fn term(c: &std::process::Child) {
        let sent = std::process::Command::new("kill").args(["-TERM", &c.id().to_string()]).status().unwrap();
        assert!(sent.success(), "kill -TERM failed: {sent:?}");
    }

    /// Q10: a server whose stdin closes (a plain end of session) exits and
    /// writes no restart point, though agents run.
    #[test]
    fn a_closed_stdin_exit_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("atlas-catch-eof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut c, _out) = helper(&dir, false);
        drop(c.stdin.take());
        let st = c.wait().unwrap();
        assert_eq!(st.code(), Some(0));
        assert!(points_in(&dir).is_empty(), "a closed stdin wrote a restart point");
        let logged = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        assert!(logged.contains("catch installed") && logged.contains("stdin closed"), "{logged}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #221 S2: while a restart is pending, a server whose stdin closes (a
    /// plain end of session, no catch) leaves its kept point marked ended,
    /// not a catch's. The next start with no reboot since does not take it
    /// (it is deleted; the crash case has the agents); after a reboot it
    /// does.
    #[test]
    fn a_closed_stdin_with_a_restart_pending_marks_the_point_ended() {
        let dir = std::env::temp_dir().join(format!("atlas-catch-ended-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut c, _out) = helper_with(&dir, false, true);
        drop(c.stdin.take());
        assert_eq!(c.wait().unwrap().code(), Some(0));
        let files = points_in(&dir);
        assert_eq!(files.len(), 1, "the kept point is gone");
        let text = std::fs::read_to_string(&files[0]).unwrap();
        let p = Point::parse(&text).unwrap();
        assert!(p.held && p.ended == "stdin closed", "{p:?}");
        let logged = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        assert!(logged.contains("stdin closed; restart pending: restart point at") && logged.contains("marked ended"), "{logged}");
        let written = std::fs::metadata(&files[0]).unwrap().modified().unwrap();
        let store = dir.join("memory.sqlite");
        let hour = Duration::from_secs(3600);
        assert!(claim_points(&store, SystemTime::now(), Some(written - hour)).is_empty(), "taken with no reboot since");
        assert!(claim_points(&store, SystemTime::now(), None).is_empty(), "taken with the boot time unknown");
        assert!(points_in(&dir).is_empty(), "a point left to the crash case was not deleted");
        std::fs::write(&files[0], &text).unwrap();
        let after = std::fs::metadata(&files[0]).unwrap().modified().unwrap();
        let got = claim_points(&store, SystemTime::now(), Some(after + Duration::from_secs(1)));
        assert_eq!(got.len(), 1, "not taken after a reboot");
        assert!(got[0].point.held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The boot decision, with injected times: a point is from a machine
    /// that went down only when the boot came after it was written.
    #[test]
    fn a_point_is_from_before_a_reboot_only_when_the_boot_is_later() {
        let t = UNIX_EPOCH + Duration::from_secs(1_763_343_420);
        assert!(booted_since(t, Some(t + Duration::from_secs(1))));
        assert!(!booted_since(t, Some(t - Duration::from_secs(1))));
        assert!(!booted_since(t, None));
    }

    /// N1: of two starts that judged one lock stale, the second must not
    /// remove the fresh lock the first made in between: what it moved
    /// aside is not the lock it judged, so it goes back and the start
    /// stands down.
    #[test]
    fn a_takeover_never_removes_a_fresh_lock() {
        let dir = std::env::temp_dir().join(format!("atlas-lock-takeover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join(OFFER_LOCK);
        let fresh = format!("{} -", std::process::id());
        std::fs::write(&lock, &fresh).unwrap();
        assert!(!take_over(&lock, "4294967290 1"), "a fresh lock was taken over");
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), fresh, "the fresh lock was not put back");
        // The lock judged stale is moved aside and removed.
        std::fs::write(&lock, "4294967290 1").unwrap();
        assert!(take_over(&lock, "4294967290 1"));
        assert!(!lock.exists());
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unix: SIGTERM to the server writes the point with its running agents,
    /// and the process still dies by SIGTERM.
    #[cfg(unix)]
    #[test]
    fn sigterm_writes_the_point_and_still_ends_the_process() {
        use std::os::unix::process::ExitStatusExt;
        let dir = std::env::temp_dir().join(format!("atlas-catch-term-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut c, _out) = helper(&dir, false);
        // Held open across the wait (`Child::wait` closes stdin first): the
        // helper may end only by the signal.
        let stdin = c.stdin.take();
        term(&c);
        let st = c.wait().unwrap();
        drop(stdin);
        let logged = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap_or_default();
        assert!(logged.contains("SIGTERM; stdin open; restart point written"), "{logged}");
        assert_eq!(st.signal(), Some(15), "{st:?}");
        let got = Point::parse(&std::fs::read_to_string(&points_in(&dir)[0]).unwrap()).unwrap();
        assert_eq!(got.agents[0].id, "a1");
        assert_eq!(got.agents[0].launch, "journal:WALRUS:0001");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S2: a shutdown signal and the end of input together (systemd ends
    /// the whole session): the end-of-input exit waits for the catch, which
    /// writes the point and ends the process by the signal. The catch is
    /// held 500 ms mid-way, so without the wait the helper exits 0 first.
    #[cfg(unix)]
    #[test]
    fn a_shutdown_signal_with_the_end_of_input_still_writes_the_point() {
        use std::io::BufRead;
        use std::os::unix::process::ExitStatusExt;
        let dir = std::env::temp_dir().join(format!("atlas-catch-both-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut c, mut out) = helper(&dir, true);
        term(&c);
        let mut line = String::new();
        while out.read_line(&mut line).unwrap() > 0 && !line.contains("catching") {
            line.clear();
        }
        assert!(line.contains("catching"), "the catch never began");
        // The catch has begun and holds: now the end of input.
        drop(c.stdin.take());
        let st = c.wait().unwrap();
        let logged = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap_or_default();
        assert!(logged.contains("SIGTERM; stdin closed") && logged.contains("restart point written"), "{logged}");
        assert_eq!(st.signal(), Some(15), "{st:?}");
        assert_eq!(points_in(&dir).len(), 1, "{logged}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
