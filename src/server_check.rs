//! Whether this session's atlas server still runs, and the prompt hook's
//! notice when it does not.
//!
//! Most of atlas's hooks are `mcp_tool` hooks on the resident server. When
//! Claude Code loses that server (it exited, or a `/clear` dropped the
//! plugin's server as stale), each of those hooks fails and atlas falls
//! silent with no word to the user. So each server leaves a marker at
//! start naming the Claude Code process it serves ([`mark`]), and a command
//! hook on UserPromptSubmit (`atlas --server-check`), which needs no server,
//! says so when every marker of its own Claude Code names a server that no
//! longer runs ([`check`]).
//!
//! The markers live in [`LIVE_DIR`] in the data folder
//! ([`crate::dirs::data_dir`]), not beside the store, so the hook finds them
//! without reading a configuration. A marker's name is `<owner>.<server>`,
//! each a process tag (`<pid> <start>`, as [`crate::restart::server_tag`]
//! makes it) with its space written as `-`; it holds the server's tag. It is
//! kept when the server exits, so a server that is gone is seen; a starting
//! server removes the markers whose Claude Code is gone.
//!
//! The notice is said once for each loss, not at every prompt: the hook
//! writes the stopped servers' markers it told about to `<owner>.told`
//! ([`TOLD`]), and says it again only when that set changes (a reconnected
//! server that stopped too).

use std::path::{Path, PathBuf};

use crate::restart::{exe_name, server_live, server_tag, CLAUDE_CODE, WRAPPERS};

/// The markers' folder in the data folder.
pub const LIVE_DIR: &str = "live-servers";
/// The server part of the file that holds what the hook last told
/// (`<owner>.told`): no process tag, so never read as a marker.
pub const TOLD: &str = "told";

/// What the hook found about its session's servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A server this Claude Code started still runs.
    Running,
    /// Every server this Claude Code started has stopped: this many.
    Gone(usize),
    /// Nothing can be said, and why.
    Unknown(String),
}

/// Whether an executable name is Claude Code's: a name in [`CLAUDE_CODE`],
/// or a version number (`2.1.332`), the name the native install's binary
/// runs under when started by its full path.
pub fn is_claude_code(name: &str) -> bool {
    let n = exe_name(name);
    let version = n.contains('.') && n.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    version || CLAUDE_CODE.contains(&n.as_str())
}

/// The Claude Code process nearest above `me` in `procs` ((pid, parent pid,
/// executable name)), past known shells and launchers ([`WRAPPERS`]). The
/// nearest, not the topmost as [`crate::restart::owner_pid`] takes: a
/// background session runs under a `claude daemon` that every background
/// session shares. The walk stops at a parent that `started` dates after its
/// child (a reused pid). `None` when no such ancestor is found.
pub fn session_pid(procs: &[(u32, u32, String)], me: u32, started: impl Fn(u32) -> Option<u64>) -> Option<u32> {
    let row = |pid: u32| procs.iter().find(|p| p.0 == pid);
    let (mut child, mut pid) = (me, row(me)?.1);
    // A short walk: a wrapper chain is a few deep; a loop is cut off.
    for _ in 0..8 {
        let r = row(pid).filter(|_| pid > 1)?;
        if let (Some(p), Some(c)) = (started(pid), started(child)) {
            if p > c {
                return None;
            }
        }
        if is_claude_code(&r.2) {
            return Some(pid);
        }
        if !WRAPPERS.contains(&exe_name(&r.2).as_str()) {
            return None;
        }
        (child, pid) = (pid, r.1);
    }
    None
}

/// The verdict on one Claude Code's markers, each [`server_live`] of its
/// server tag (`None`: the tag did not parse).
pub fn verdict(servers: &[Option<bool>]) -> Verdict {
    if servers.is_empty() {
        return Verdict::Unknown("no server of this session left a marker".into());
    }
    if servers.contains(&Some(true)) {
        return Verdict::Running;
    }
    if servers.contains(&None) {
        return Verdict::Unknown("a marker names no process".into());
    }
    Verdict::Gone(servers.len())
}

/// A process tag (`<pid> <start>`) as a marker name's part.
fn part(tag: &str) -> String {
    tag.trim().replace(' ', "-")
}

/// The owner tag a marker's name holds, or `None` for another file.
fn owner_of(name: &str) -> Option<String> {
    let (owner, _) = name.split_once('.')?;
    let (pid, start) = owner.split_once('-')?;
    pid.parse::<u32>().ok()?;
    Some(format!("{pid} {start}"))
}

/// The markers' folder, or `None` when no data folder is known.
pub fn live_dir() -> Option<PathBuf> {
    crate::dirs::data_dir().map(|d| d.join(LIVE_DIR))
}

/// The tag of process `pid`, as [`server_tag`] tags this one.
fn tag_of(pid: u32) -> String {
    match crate::platform::process::start_time(pid) {
        Some(t) => format!("{pid} {t}"),
        None => format!("{pid} -"),
    }
}

/// The tag of the Claude Code process above this one, or why there is none.
fn owner_here() -> Result<String, String> {
    let procs = crate::platform::process::table();
    session_pid(&procs, std::process::id(), crate::platform::process::start_time)
        .map(tag_of)
        .ok_or_else(|| "no Claude Code process above this one".into())
}

/// Writes the marker of `server` for `owner` in `dir`, after removing the
/// markers whose own Claude Code no longer runs. Returns the marker's path.
pub fn mark_in(dir: &Path, owner: &str, server: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if owner_of(&name).is_some_and(|o| server_live(&o) == Some(false)) {
            // A removal that fails leaves one stale file; it is retried at
            // the next server start.
            let _ = std::fs::remove_file(e.path());
        }
    }
    let path = dir.join(format!("{}.{}", part(owner), part(server)));
    std::fs::write(&path, format!("{}\n", server.trim())).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// This server's marker ([`mark_in`] in [`live_dir`]), at server start. The
/// result is the line the server logs.
pub fn mark() -> String {
    let Some(dir) = live_dir() else { return "off: no data folder".into() };
    match owner_here().and_then(|owner| mark_in(&dir, &owner, &server_tag())) {
        Ok(p) => p.display().to_string(),
        Err(e) => format!("off: {e}"),
    }
}

/// The markers in `dir` of the Claude Code tagged `owner`, each its file
/// name, sorted, and [`server_live`] of the server it names; `Err` with the
/// reason when the folder cannot be read.
fn markers(dir: &Path, owner: &str) -> Result<Vec<(String, Option<bool>)>, String> {
    let prefix = format!("{}.", part(owner));
    let told = format!("{prefix}{TOLD}");
    let entries = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) || name == told {
            continue;
        }
        match std::fs::read_to_string(e.path()) {
            Ok(tag) => out.push((name, server_live(tag.trim()))),
            // A marker removed between the listing and the read is not one.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => out.push((name, None)),
        }
    }
    out.sort();
    Ok(out)
}

/// The verdict on the markers in `dir` of the Claude Code tagged `owner`.
pub fn check_in(dir: &Path, owner: &str) -> Verdict {
    match markers(dir, owner) {
        Ok(m) => verdict(&m.iter().map(|(_, live)| *live).collect::<Vec<_>>()),
        Err(e) => Verdict::Unknown(e),
    }
}

/// Whether to say the notice for `told` (what was last told, `None` when
/// nothing was) and `now` (the stopped servers' markers now): only when
/// they differ, so each loss is said once.
pub fn tell(told: Option<&str>, now: &str) -> bool {
    told != Some(now)
}

/// The verdict in `dir` for the Claude Code tagged `owner`, and whether the
/// hook says it: a [`Verdict::Gone`] not yet told, which is then recorded
/// as told. A record that cannot be written is told again next time.
pub fn check_once_in(dir: &Path, owner: &str) -> (Verdict, bool) {
    let v = check_in(dir, owner);
    if !matches!(v, Verdict::Gone(_)) {
        return (v, false);
    }
    let now: Vec<String> = markers(dir, owner).unwrap_or_default().into_iter().map(|(n, _)| n).collect();
    let now = now.join("\n");
    let path = dir.join(format!("{}.{TOLD}", part(owner)));
    let told = std::fs::read_to_string(&path).ok();
    if !tell(told.as_deref().map(str::trim_end), &now) {
        return (v, false);
    }
    let _ = std::fs::write(&path, format!("{now}\n"));
    (v, true)
}

/// The verdict for the Claude Code above this process, and whether to say
/// it ([`check_once_in`]; `once` false only looks).
pub fn check(once: bool) -> (Verdict, bool) {
    let Some(dir) = live_dir() else { return (Verdict::Unknown("no data folder".into()), false) };
    match owner_here() {
        Ok(owner) if once => check_once_in(&dir, &owner),
        Ok(owner) => (check_in(&dir, &owner), false),
        Err(e) => (Verdict::Unknown(e), false),
    }
}

/// What the user is told when the server is gone.
pub const USER_NOTICE: &str = "atlas: this session's atlas server is not running, so atlas's hooks and tools are off. Run /mcp to reconnect it. If /mcp does not list atlas, exit and restart Claude Code with `claude --continue`.";

/// What the model is told when the server is gone.
pub const MODEL_NOTICE: &str = "atlas: this session's atlas server is not running, so atlas's hooks and tools are off, and atlas's tools will fail. Tell the user: run /mcp to reconnect atlas, or, if /mcp does not list it, exit and restart Claude Code with `claude --continue`.";

/// The hook's output for a verdict: the notice as UserPromptSubmit JSON when
/// the server is gone, else nothing.
pub fn hook_output(v: &Verdict) -> Option<String> {
    match v {
        Verdict::Gone(_) => Some(
            crate::json!({
                "systemMessage": USER_NOTICE,
                "hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": MODEL_NOTICE}
            })
            .to_string(),
        ),
        Verdict::Running | Verdict::Unknown(_) => None,
    }
}

/// `atlas --server-check [--say]`: the UserPromptSubmit hook. It reads and
/// drops the hook's input, prints the notice when the server is gone and
/// that was not told yet, and always exits 0, so a prompt is never stopped.
/// `--say` prints the verdict whatever it is, reads no input and records
/// nothing, to look by hand.
pub fn cli(args: &[String]) -> std::process::ExitCode {
    let say = args.iter().any(|a| a == "--say");
    if !say {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    }
    let (v, say_it) = check(!say);
    if say {
        println!("{v:?}");
    } else if let Some(out) = hook_output(&v).filter(|_| say_it) {
        println!("{out}");
    }
    std::process::ExitCode::SUCCESS
}
