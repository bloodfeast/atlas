//! atlas's hooks as command hooks that hand their event to the session's
//! server over a Unix socket.
//!
//! Claude Code runs each hook as `atlas --hook`, a short process that reads
//! the event on stdin. That process finds the server its own Claude Code
//! started by the server's marker ([`crate::server_check`]), sends it the
//! event on the server's socket (`<server pid>.sock` beside the marker) and
//! prints the answer. The server works the event on its hook worker, as it
//! worked an `mcp_tool` hook's call before, so what it keeps between events
//! (a session's caps, its pending launches, what it already said) stays in
//! one process. A command hook keeps working when Claude Code loses the MCP
//! server, which an `mcp_tool` hook does not: it then says so instead of
//! failing unseen.
//!
//! Where the hook goes:
//!
//! - SessionStart and SessionEnd run in the hook's own process, as before:
//!   they run before a server is marked and after it is gone.
//! - A running server's socket takes every other event. Its answer is due
//!   by the event's budget ([`crate::hook_budget`]); the server answers a
//!   late one as the stdio watchdog does, and the worker's own answer is
//!   then lost ([`crate::hooks::Hooks::answer_lost`]).
//! - When every server of this session is gone, the hook does nothing, and
//!   UserPromptSubmit says the once-per-loss notice ([`crate::server_check`]).
//! - When nothing can be said of the servers, or no running server's socket
//!   answers (one started before its marker, or an older binary with no
//!   socket), the hook runs in its own process, as on Windows, where atlas
//!   has no socket.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::json::Value;

/// How long the hook waits for the server's answer: past the server's
/// latest deadline (the longest budget, [`crate::hook_budget::MAX_MS`], and
/// its grace) by [`CLIENT_MARGIN`], and inside the shipped hooks' 15 s
/// timeout. The server answers first, so a hook it answers late is never
/// also given the worker's answer.
pub const CLIENT_WAIT: Duration = Duration::from_millis(crate::hook_budget::MAX_MS + 1_000);

/// How much later than the server's latest deadline the hook gives up.
pub const CLIENT_MARGIN: Duration = Duration::from_millis(500);

/// The most a hook's input may be: a PostToolUse carries the tool's whole
/// output.
pub const MAX_INPUT: u64 = 64 << 20;

/// One hook for the worker: its arguments, when it was read, and where its
/// answer goes.
pub struct Job {
    /// The hook's input, with `event` set.
    pub args: Value,
    /// When it was read.
    pub arrived: Instant,
    /// Where its answer goes.
    pub reply: Reply,
}

/// Where a hook's answer goes.
pub enum Reply {
    /// The JSON-RPC response to this request id, on stdout.
    Rpc(Value),
    /// The waiting socket connection, keyed for the gate's reason
    /// ([`crate::hook_gate::why`]).
    Socket(String, Arc<Answer>),
}

impl Reply {
    /// The key [`crate::hook_gate`] names this hook by.
    pub fn key(&self) -> String {
        match self {
            Reply::Rpc(id) => id.to_string(),
            Reply::Socket(key, _) => key.clone(),
        }
    }
}

/// A socket hook's answer, given by the worker or, once its deadline has
/// passed, given up on by the connection (which then answers it itself).
#[derive(Default)]
pub struct Answer {
    slot: Mutex<Slot>,
    ready: Condvar,
}

#[derive(Default)]
enum Slot {
    #[default]
    Waiting,
    Given(String),
    Late,
}

impl Answer {
    /// Gives the worker's answer: false when the connection gave up on it
    /// already (the answer is lost).
    pub fn give(&self, text: String) -> bool {
        let Ok(mut s) = self.slot.lock() else { return false };
        if !matches!(*s, Slot::Waiting) {
            return false;
        }
        *s = Slot::Given(text);
        self.ready.notify_all();
        true
    }

    /// Whether the connection gave up on it already.
    pub fn late(&self) -> bool {
        self.slot.lock().map(|s| matches!(*s, Slot::Late)).unwrap_or(true)
    }

    /// The worker's answer, given by `until`; else `None`, and the answer
    /// is marked late, so the worker's [`Answer::give`] fails.
    pub fn wait(&self, until: Instant) -> Option<String> {
        let Ok(mut s) = self.slot.lock() else { return None };
        loop {
            if matches!(*s, Slot::Given(_)) {
                if let Slot::Given(t) = std::mem::take(&mut *s) {
                    return Some(t);
                }
            }
            let now = Instant::now();
            if now >= until {
                *s = Slot::Late;
                return None;
            }
            s = match self.ready.wait_timeout(s, until - now) {
                Ok((g, _)) => g,
                Err(_) => return None,
            };
        }
    }
}

/// A hook's input as the worker takes it: `event` set from Claude Code's
/// `hook_event_name` (the reader and [`crate::tools::dedup::Announcer`] read
/// `event`), and marked as a hook, never a model's tool call (whatever the
/// input says).
pub fn job_args(mut v: Value) -> Value {
    if let Some(m) = v.as_object_mut() {
        if m.get("event").and_then(Value::as_str).is_none_or(str::is_empty) {
            if let Some(e) = m.get("hook_event_name").cloned() {
                m.insert("event".into(), e);
            }
        }
    }
    crate::hooks::mark_tool_use(&v, None)
}

/// Whether a hook of `event` goes to the server. SessionStart comes before
/// the server is marked and SessionEnd after it is gone, so both run in the
/// hook's own process.
pub fn goes_to_server(event: &str) -> bool {
    !matches!(event, "SessionStart" | "SessionEnd" | "")
}

/// What the server takes from its socket: [`job_args`] of an event that
/// [`goes_to_server`], `None` for any other. The hook never sends one, so
/// one arriving was written to the socket by something else, and is
/// dropped.
pub fn socket_job(v: Value) -> Option<Value> {
    let args = job_args(v);
    goes_to_server(&crate::hooks::HookEvent::from_value(&args).event).then_some(args)
}

/// Where a command hook's event went.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// The server answered (printed as it is; empty says nothing).
    Answered(String),
    /// A server took it and did not answer within [`CLIENT_WAIT`].
    Late,
    /// Every server of this session is gone.
    Gone,
    /// To be run in the hook's own process, and why.
    InProcess(String),
}

#[cfg(unix)]
pub use unix::{listen, relay_in, Listener};

#[cfg(unix)]
mod unix {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::Sender;

    /// The worker's queue, shared with the connections; taken when the
    /// server stops, so the worker's queue can end.
    type Queue = Arc<Mutex<Option<Sender<Option<Job>>>>>;

    /// A server's hook socket. Dropping it stops taking hooks and removes
    /// the socket; the thread that accepts ends with the process.
    pub struct Listener {
        path: PathBuf,
        queue: Queue,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            if let Ok(mut q) = self.queue.lock() {
                *q = None;
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// When a hook read at `arrived` is answered for the worker:
    /// `budget` is the hooks' configured one, `None` when it is off (the
    /// longest budget then). Always before the hook gives up waiting
    /// ([`CLIENT_WAIT`]).
    pub(super) fn deadline(ev: &str, arrived: Instant, budget: Option<Duration>, grace: Duration) -> Instant {
        arrived + crate::hook_budget::for_event(budget.unwrap_or(MAX_BUDGET), ev).min(MAX_BUDGET) + grace
    }

    /// The longest budget.
    const MAX_BUDGET: Duration = Duration::from_millis(crate::hook_budget::MAX_MS);

    /// Listens for this session's hooks on `path` (replacing a stale socket
    /// there), sending each to the worker on `tx` and answering it on its
    /// connection. `budget` (`None`: off) and `grace` time the answer as the
    /// stdio watchdog does; `announcer` notes each hook before its answer can
    /// let a call it announces through.
    ///
    /// # Errors
    ///
    /// The socket cannot be made.
    pub fn listen(
        path: &Path,
        tx: Sender<Option<Job>>,
        announcer: crate::tools::dedup::Announcer,
        budget: Option<Duration>,
        grace: Duration,
    ) -> std::io::Result<Listener> {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let queue: Queue = Arc::new(Mutex::new(Some(tx)));
        let q = queue.clone();
        std::thread::Builder::new().name("atlas-hook-socket".into()).spawn(move || {
            let mut n: u64 = 0;
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                n += 1;
                let (q, a) = (q.clone(), announcer.clone());
                let key = format!("sock:{n}");
                let _ = std::thread::Builder::new().name("atlas-hook-conn".into()).spawn(move || serve_one(conn, key, &q, &a, budget, grace));
            }
        })?;
        Ok(Listener { path: path.to_path_buf(), queue })
    }

    /// One hook: read to the end, sent to the worker, answered by its
    /// deadline (else with [`crate::hooks::overrun_answer`]).
    fn serve_one(
        mut conn: UnixStream,
        key: String,
        queue: &Queue,
        announcer: &crate::tools::dedup::Announcer,
        budget: Option<Duration>,
        grace: Duration,
    ) {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
        let mut text = String::new();
        if (&mut conn).take(MAX_INPUT).read_to_string(&mut text).is_err() {
            return;
        }
        let arrived = Instant::now();
        let Some(args) = crate::json::parse(&text).ok().and_then(socket_job) else { return };
        let ev = crate::hooks::HookEvent::from_value(&args);
        let answer = Arc::new(Answer::default());
        let job = Job { args: args.clone(), arrived, reply: Reply::Socket(key.clone(), answer.clone()) };
        let sent = match queue.lock() {
            Ok(q) => q.as_ref().is_some_and(|tx| tx.send(Some(job)).is_ok()),
            Err(_) => false,
        };
        if !sent {
            // The server is stopping: nothing to say.
            return;
        }
        // Noted once sent, as the stdio reader notes it: a call it announces
        // comes only after this answer.
        announcer.hook(&args);
        let text = answer.wait(deadline(&ev.event, arrived, budget, grace)).unwrap_or_else(|| {
            let limit = crate::hook_budget::for_event(budget.unwrap_or(MAX_BUDGET), &ev.event).min(MAX_BUDGET);
            crate::hooks::overrun_answer(&ev, limit, &crate::hook_gate::why(&key, arrived))
        });
        let _ = conn.set_write_timeout(Some(Duration::from_secs(2)));
        let _ = conn.write_all(text.as_bytes());
    }

    /// Hands `input` (a command hook's stdin) to a running server of the
    /// Claude Code tagged `owner`, by its marker in `dir`, waiting `wait`
    /// for the answer.
    pub fn relay_in(dir: &Path, owner: &str, input: &[u8], wait: Duration) -> Route {
        use crate::server_check::{markers, socket_of_marker, verdict, Verdict};
        let found = match markers(dir, owner) {
            Ok(m) => m,
            Err(e) => return Route::InProcess(e),
        };
        match verdict(&found.iter().map(|(_, live)| *live).collect::<Vec<_>>()) {
            Verdict::Gone(_) => return Route::Gone,
            Verdict::Unknown(why) => return Route::InProcess(why),
            Verdict::Running => {}
        }
        // Every running one is tried, the last by name first.
        for (name, _) in found.iter().rev().filter(|(_, live)| *live == Some(true)) {
            let Some(sock) = socket_of_marker(dir, name) else { continue };
            let Ok(mut conn) = UnixStream::connect(&sock) else { continue };
            return exchange(&mut conn, input, wait);
        }
        Route::InProcess("no running server's socket answers".into())
    }

    /// Sends `input`, closes the writing side and reads the answer.
    fn exchange(conn: &mut UnixStream, input: &[u8], wait: Duration) -> Route {
        let _ = conn.set_write_timeout(Some(Duration::from_secs(2)));
        if conn.write_all(input).is_err() || conn.shutdown(std::net::Shutdown::Write).is_err() {
            return Route::InProcess("the server's socket took no input".into());
        }
        let _ = conn.set_read_timeout(Some(wait));
        let mut out = String::new();
        match conn.read_to_string(&mut out) {
            Ok(_) => Route::Answered(out),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Route::Late,
            // The server went away mid-answer: it said nothing.
            Err(_) => Route::Answered(String::new()),
        }
    }
}

/// What `atlas --hook` prints for `input` (its stdin), or `None` to run the
/// hook in this process. See the module's text for where each event goes.
pub fn command_hook(input: &str) -> Option<String> {
    #[cfg(unix)]
    {
        let v = crate::json::parse(input).ok()?;
        let ev = crate::hooks::HookEvent::from_value(&v);
        if !goes_to_server(&ev.event) {
            return None;
        }
        let dir = crate::server_check::live_dir()?;
        let owner = crate::server_check::owner_here().ok()?;
        match relay_in(&dir, &owner, input.as_bytes(), CLIENT_WAIT) {
            Route::Answered(t) => Some(t),
            Route::Late => Some(crate::hooks::overrun_answer(&ev, CLIENT_WAIT, "no answer from this session's atlas server")),
            Route::Gone if crate::hooks::takes_live_notice(&ev) => {
                let (verdict, say) = crate::server_check::check_once_in(&dir, &owner);
                Some(crate::server_check::hook_output(&verdict).filter(|_| say).unwrap_or_default())
            }
            Route::Gone => Some(String::new()),
            Route::InProcess(_) => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = input;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server answers a hook, its worker's answer or the late one, before
    /// the hook stops waiting, for every event, with the budget on at its
    /// longest or off: a hook the server answered late is never given the
    /// worker's answer too (for a launch, a deny the worker had admitted).
    #[cfg(unix)]
    #[test]
    fn the_server_answers_before_the_hook_stops_waiting() {
        let t = Instant::now();
        let longest = Duration::from_millis(crate::hook_budget::MAX_MS);
        for ev in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "SubagentStart", "SubagentStop", "Stop", "PreCompact", "SessionEnd"] {
            for budget in [Some(longest), Some(Duration::from_millis(1)), None] {
                let d = unix::deadline(ev, t, budget, crate::protocol::WATCH_GRACE);
                assert!(d + CLIENT_MARGIN <= t + CLIENT_WAIT, "{ev} {budget:?}");
            }
        }
        // Inside the shipped hooks' shortest timeout once the hook started.
        assert!(CLIENT_WAIT + Duration::from_secs(2) <= Duration::from_secs(15));
    }

    #[test]
    fn an_answer_given_in_time_is_taken_and_a_late_one_is_lost() {
        let a = Answer::default();
        assert!(a.give("x".into()));
        assert_eq!(a.wait(Instant::now()).as_deref(), Some("x"));
        let late = Answer::default();
        assert_eq!(late.wait(Instant::now()), None);
        assert!(late.late());
        assert!(!late.give("y".into()), "the connection answered it already");
    }

    #[test]
    fn a_command_hooks_input_is_named_by_its_event_and_marked_a_hook() {
        let v = crate::json::parse(r#"{"hook_event_name":"PreToolUse","tool_name":"Read","_tool_use":true}"#).unwrap();
        let a = job_args(v);
        assert_eq!(a.get("event").and_then(Value::as_str), Some("PreToolUse"));
        assert_eq!(a.get(crate::hooks::TOOL_USE_ARG), Some(&Value::Bool(false)));
    }

    #[test]
    fn the_server_drops_an_event_the_hook_never_sends_it() {
        let job = |t: &str| socket_job(crate::json::parse(t).unwrap());
        for e in ["SessionStart", "SessionEnd"] {
            assert_eq!(job(&format!(r#"{{"hook_event_name":"{e}"}}"#)), None, "{e}");
        }
        assert_eq!(job(r#"{"prompt":"hi"}"#), None, "no event");
        assert!(job(r#"{"hook_event_name":"Stop"}"#).is_some());
    }
}
