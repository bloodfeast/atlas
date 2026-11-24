//! The pr-events listener's auto-start at SessionStart (#386; the design,
//! journal:ATLAS-ISSUES:0004: SessionStart starts the listener when none
//! runs, with a lock and a pid file so only one runs; no OS service).
//!
//! One listener per memory store. Beside the store sit three files:
//! `<store>.pr-events.lock`, `<store>.pr-events.pid` and
//! `<store>.pr-events.log`. A listener holds an exclusive lock on the lock
//! file for its whole life ([`hold`]); the OS lets go of it however the
//! process ends, so a listener that exits or is killed leaves nothing that
//! looks alive. The pid file names the holder (`PID START`), for the lines
//! that say which process runs, and for a file system without locks, where
//! it is all there is ([`recorded`]: a dead pid, or a pid whose start time
//! differs, is no listener).
//!
//! [`ensure`] is SessionStart's step: it tries the lock. Held: a listener
//! runs (or another session is starting one), so nothing is done. Free,
//! and the pid file, written within [`START_WINDOW`], names a live
//! listener: one a session started a moment ago, which has not taken the
//! lock yet, so nothing is done. Free, and no such listener named: it starts the listener while it holds the lock,
//! writes its pid, and lets go; the new listener (`atlas pr-events serve
//! --auto`) waits up to [`WAIT`] for it. So two sessions that start at once
//! start one listener, whichever order they run in; and two listeners that
//! start at once (a session's, and one started by hand) cannot both hold
//! the lock, so the one started with `--auto` leaves.
//!
//! On a file system without locks the pid file alone decides, best-effort:
//! two sessions that read it at the same moment can each start a listener,
//! and both may serve (a second one serves without forwarders, or fails to
//! bind the port). std reports no locks only where it has no lock call at
//! all, so this is near-unreachable on the platforms atlas ships for.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a listener started with `--auto` waits for the lock: the
/// starter holds it only while it starts the process.
pub const WAIT: Duration = Duration::from_secs(5);

/// The listener's files beside a memory store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Held by the running listener.
    pub lock: PathBuf,
    /// `PID START` of the listener that holds the lock.
    pub pid: PathBuf,
    /// Auto-started listeners' stdout and stderr, appended at each start
    /// (cut when over [`LOG_CAP`] at a start, when no listener writes it).
    pub log: PathBuf,
}

impl Paths {
    /// The files beside the store at `store`.
    pub fn for_store(store: &Path) -> Paths {
        let with = |ext: &str| {
            let mut n = store.file_name().unwrap_or_default().to_os_string();
            n.push(ext);
            store.with_file_name(n)
        };
        Paths { lock: with(".pr-events.lock"), pid: with(".pr-events.pid"), log: with(".pr-events.log") }
    }
}

/// A listener's hold on the lock, kept for the process's life.
#[derive(Debug)]
pub struct Hold {
    _file: Option<File>,
}

enum Try {
    Got(File),
    Held,
    NoLocks,
}

fn try_take(lock: &Path) -> std::result::Result<Try, String> {
    if let Some(d) = lock.parent() {
        fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock)
        .map_err(|e| format!("{}: {e}", lock.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(Try::Got(f)),
        Err(fs::TryLockError::WouldBlock) => Ok(Try::Held),
        Err(fs::TryLockError::Error(e)) if e.kind() == ErrorKind::Unsupported => Ok(Try::NoLocks),
        Err(fs::TryLockError::Error(e)) => Err(format!("{}: {e}", lock.display())),
    }
}

/// The pid file's text for `pid`.
fn pid_text(pid: u32) -> String {
    format!("{pid} {}\n", crate::platform::process::start_time(pid).unwrap_or(0))
}

fn write_pid(paths: &Paths, pid: u32) -> std::result::Result<(), String> {
    crate::fsio::replace_atomic(&paths.pid, pid_text(pid).as_bytes()).map_err(|e| format!("{}: {e}", paths.pid.display()))
}

/// The listener the pid file names, when it still runs: the process is
/// alive and, where start times are read, started when the file says (so a
/// pid the OS gave to another process since is no listener).
pub fn recorded(paths: &Paths) -> Option<u32> {
    let text = fs::read_to_string(&paths.pid).ok()?;
    let mut w = text.split_whitespace();
    let pid: u32 = w.next()?.parse().ok()?;
    let start: u64 = w.next()?.parse().ok()?;
    let same = start == 0 || crate::platform::process::start_time(pid).is_none_or(|s| s == start);
    (pid != 0 && crate::platform::process::alive(pid) && same).then_some(pid)
}

/// How long after a start the pid file, with the lock free, still names a
/// listener that is starting: the listener takes the lock within [`WAIT`]
/// or leaves, so past this the pid is no starting listener.
const START_WINDOW: Duration = Duration::from_secs(2 * WAIT.as_secs());

/// The listener the pid file names while the lock is free ([`recorded`]),
/// only when the file was written within [`START_WINDOW`] (either way, for
/// a clock that steps): a session's start a moment ago. An older file with a
/// live pid names a process the OS gave that pid since (a reboot or a
/// crash), which a start time cannot tell apart where none is read.
fn starting(paths: &Paths) -> Option<u32> {
    let written = fs::metadata(&paths.pid).and_then(|m| m.modified()).ok()?;
    let age = std::time::SystemTime::now().duration_since(written).unwrap_or_else(|e| e.duration());
    if age > START_WINDOW {
        return None;
    }
    recorded(paths)
}

/// Takes the lock for this process's life, waiting up to `wait`, and
/// writes the pid file. `None` when another listener holds it past `wait`.
/// On a file system without locks, the pid file decides: `None` when it
/// names another live listener.
///
/// # Errors
///
/// A message naming the lock or pid file when it cannot be opened or
/// written.
pub fn hold(paths: &Paths, wait: Duration) -> std::result::Result<Option<Hold>, String> {
    let start = Instant::now();
    loop {
        match try_take(&paths.lock)? {
            Try::Got(f) => {
                write_pid(paths, std::process::id())?;
                return Ok(Some(Hold { _file: Some(f) }));
            }
            Try::NoLocks => {
                if recorded(paths).is_some_and(|p| p != std::process::id()) {
                    return Ok(None);
                }
                write_pid(paths, std::process::id())?;
                return Ok(Some(Hold { _file: None }));
            }
            Try::Held if start.elapsed() >= wait => return Ok(None),
            Try::Held => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// What [`ensure`] found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    /// A listener runs (its pid when the pid file names it), or another
    /// session is starting one: nothing started.
    Running(Option<u32>),
    /// A listener was started: its pid.
    Started(u32),
    /// A listener was started (its pid), but the pid file could not be
    /// written (why): it runs, unnamed.
    Unnamed(u32, String),
}

/// Starts a listener with `spawn` (which answers its pid) unless one runs
/// for the store, deciding under the lock: see the module documentation.
///
/// # Errors
///
/// A message when the lock file cannot be opened or `spawn` fails.
pub fn ensure(paths: &Paths, spawn: impl FnOnce(&Paths) -> io::Result<u32>) -> std::result::Result<Ensured, String> {
    match try_take(&paths.lock)? {
        Try::Held => Ok(Ensured::Running(recorded(paths))),
        Try::Got(f) => {
            // A live listener the pid file names, with the lock free: one
            // started by a session a moment ago that has not taken it yet.
            if let Some(p) = starting(paths) {
                return Ok(Ensured::Running(Some(p)));
            }
            let pid = spawn(paths).map_err(|e| format!("starting the pr-events listener: {e}"))?;
            // Name it before letting go, so a session in the gap before it
            // takes the lock sees it starting (#402).
            let named = write_pid(paths, pid);
            // Let go: the listener waits for the lock (`WAIT`).
            drop(f);
            Ok(match named {
                Ok(()) => Ensured::Started(pid),
                Err(e) => Ensured::Unnamed(pid, e),
            })
        }
        Try::NoLocks => match recorded(paths) {
            Some(p) => Ok(Ensured::Running(Some(p))),
            None => {
                let pid = spawn(paths).map_err(|e| format!("starting the pr-events listener: {e}"))?;
                Ok(match write_pid(paths, pid) {
                    Ok(()) => Ensured::Started(pid),
                    Err(e) => Ensured::Unnamed(pid, e),
                })
            }
        },
    }
}

/// SessionStart's line for a listener that started as `pid` but could not be
/// named in the pid file (`e`): it runs, so the line must not say the start
/// failed (#402).
fn unnamed_line(pid: u32, e: &str) -> String {
    format!("atlas: started the pr-events listener (pr_events.autostart), pid {pid}, but could not write its pid file ({e}), so a session starting in the next moments may start another")
}

/// Starts `cmd` apart from this process ([`crate::platform::process::detach`]):
/// no console, and on Unix a session of its own (no hangup when the terminal
/// closes); on Windows out of this process's job where the job allows it, so
/// the end of the session that started it does not end it. Its stdio is the
/// caller's to set, and it gets none of this process's own stdio
/// ([`crate::platform::process::without_std_inheritance`]): a hook's stdout
/// is a pipe the session reads to its end, which the child would otherwise
/// keep open for its life (#402).
///
/// # Errors
///
/// The spawn's error.
pub fn spawn_detached(mut cmd: Command) -> io::Result<std::process::Child> {
    crate::platform::process::without_std_inheritance(|| {
        crate::platform::process::detach(&mut cmd, true);
        match cmd.spawn() {
            // A Windows job that does not allow breakaway refuses the flag.
            Err(e) if cfg!(windows) && e.kind() == ErrorKind::PermissionDenied => {
                crate::platform::process::detach(&mut cmd, false);
                cmd.spawn()
            }
            r => r,
        }
    })
}

/// The log's size past which a start cuts it.
pub const LOG_CAP: u64 = 1 << 20;

/// The line the last listener ended on, when it ended on an error: atlas's
/// error line (`atlas: ...`), when no line of the listener's own
/// (`atlas pr-events: ...`) comes after it. An error may run on for lines
/// (the usage), so the log's last line alone does not tell. A listener that
/// serves until it is stopped ends on its own lines, so a start that fails
/// again and again is told, not taken for a clean one.
pub fn last_failure(log: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(4096))).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    let last = tail.lines().rev().map(str::trim).find(|l| l.starts_with("atlas: ") || l.starts_with("atlas pr-events: "))?;
    last.starts_with("atlas: ").then(|| last.to_string())
}

/// The log, opened to append (two starts never cut each other's output),
/// cut first when over [`LOG_CAP`]: a start writes it only when no listener
/// runs, so nothing else writes it then.
fn open_log(log: &Path) -> io::Result<File> {
    if fs::metadata(log).is_ok_and(|m| m.len() > LOG_CAP) {
        File::create(log)?;
    }
    OpenOptions::new().create(true).append(true).open(log)
}

/// The options of this process's command line (`args`, past the program)
/// that pick its configuration, `--config PATH` and `--project NAME`, for
/// the listener to read the configuration this process read: a hook or
/// server started with `--config` would otherwise start one that reads
/// `ATLAS_CONFIG` or the user configuration instead (#402).
/// The last of each wins, as atlas's own reading of them does.
pub fn config_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let (mut config, mut project) = (None, None);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let slot = match a.as_str() {
            "--config" => &mut config,
            "--project" => &mut project,
            _ => continue,
        };
        if let Some(v) = it.next() {
            *slot = Some(v);
        }
    }
    let mut out = Vec::new();
    for (flag, v) in [("--config", config), ("--project", project)] {
        if let Some(v) = v {
            out.extend([flag.to_string(), v]);
        }
    }
    out
}

/// The real listener: this atlas, `pr-events serve --auto`, its output
/// appended to the log file ([`open_log`]), from this process's folder and
/// environment and with its [`config_args`], so it reads the configuration
/// this process read; stdin
/// closed. Never waited on, except by a thread that reaps it, so no zombie
/// is left in a long-lived server.
///
/// # Errors
///
/// The log file cannot be made or the process cannot start.
pub fn spawn_listener(paths: &Paths) -> io::Result<u32> {
    let exe = std::env::current_exe()?;
    let log = open_log(&paths.log)?;
    let mut cmd = Command::new(exe);
    cmd.args(config_args(std::env::args().skip(1))).args(["pr-events", "serve", "--auto"]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    let mut child = spawn_detached(cmd)?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// SessionStart's step: with `pr_events.autostart` on, [`ensure`] with the
/// real listener; a line for the session when one was started or the start
/// failed, none when one runs or the setting is off.
pub fn session_start(settings: &crate::memory::settings::Settings) -> Option<String> {
    settings.pr_events.as_ref().filter(|s| s.autostart)?;
    let paths = Paths::for_store(&settings.store);
    // Read before the start appends to it; it matters only if a listener is
    // started, which means none ran.
    let failed = last_failure(&paths.log);
    match ensure(&paths, spawn_listener) {
        Ok(Ensured::Running(_)) => None,
        Ok(Ensured::Started(pid)) => Some(started_line(pid, &paths.log, failed.as_deref())),
        Ok(Ensured::Unnamed(pid, e)) => Some(unnamed_line(pid, &e)),
        Err(e) => Some(format!("atlas: the pr-events auto-start failed: {e}")),
    }
}

/// SessionStart's line for a listener started as `pid`, `failed` the line
/// the last one ended on ([`last_failure`]).
fn started_line(pid: u32, log: &Path, failed: Option<&str>) -> String {
    match failed {
        None => format!("atlas: started the pr-events listener (pr_events.autostart), pid {pid}; its log: {}", log.display()),
        Some(f) => format!(
            "atlas: started the pr-events listener again (pr_events.autostart), pid {pid}, but the last one it started failed ({f}), and this one may too; its log: {}",
            log.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    //! #386: one listener per store, whatever starts at once. The listener and
    //! the sessions are copies of this test binary (`fake_listener`,
    //! `fake_session`), real processes, so the lock is tried across processes.
    //! They report on stderr and wait on stdin, so every step here waits on a
    //! pipe, never on the clock.

    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, ChildStderr};

    use super::*;

    /// The folder the copies work in; set only in a copy.
    const DIR: &str = "ATLAS_FAKE_AUTOSTART_DIR";
    /// How long a copy of the listener waits for the lock, in ms.
    const WAIT_MS: &str = "ATLAS_FAKE_AUTOSTART_WAIT_MS";
    /// What starts a copy's report line on stderr.
    const SAY: &str = "AUTOSTART ";

    fn say(w: &str) {
        eprintln!("{SAY}{w}");
    }

    /// A test folder, removed at the end.
    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn dir(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("atlas-autostart-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    fn paths(d: &Path) -> Paths {
        Paths::for_store(&d.join("memory.sqlite"))
    }

    /// This test binary, running the ignored test `role` in `d`, its stdin and
    /// stderr piped.
    fn copy(d: &Path, role: &str) -> Command {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["--ignored", "--exact", &format!("pr_autostart::tests::{role}"), "--quiet", "--nocapture", "--test-threads", "1"])
            .env(DIR, d)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        c
    }

    /// Up to `n` report lines from `r`, fewer when it ends first.
    fn words(r: &mut BufReader<ChildStderr>, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut line = String::new();
        while out.len() < n {
            line.clear();
            if r.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if let Some(w) = line.trim_end().strip_prefix(SAY) {
                out.push(w.to_string());
            }
        }
        out
    }

    /// A copy of the listener this test started, with its report.
    struct Listener {
        child: Child,
        err: BufReader<ChildStderr>,
    }

    impl Listener {
        fn spawn(d: &Path, wait_ms: Option<u64>) -> Listener {
            let mut c = copy(d, "fake_listener");
            if let Some(w) = wait_ms {
                c.env(WAIT_MS, w.to_string());
            }
            let mut child = spawn_detached(c).unwrap();
            let err = BufReader::new(child.stderr.take().unwrap());
            Listener { child, err }
        }

        /// Its first report: `held PID` or `lost PID`.
        fn said(&mut self) -> String {
            words(&mut self.err, 1).pop().unwrap_or_else(|| "nothing".into())
        }

        /// Closes its stdin, which ends it, and waits for it.
        fn stop(mut self) {
            drop(self.child.stdin.take());
            assert!(self.child.wait().unwrap().success());
        }
    }

    /// [`ensure`] with a copy of the listener as the one it starts.
    fn ensure_fake(p: &Paths) -> (Ensured, Option<Listener>) {
        let mut started = None;
        let got = ensure(p, |p| {
            let l = Listener::spawn(p.lock.parent().unwrap(), None);
            let pid = l.child.id();
            started = Some(l);
            Ok(pid)
        })
        .unwrap();
        (got, started)
    }

    /// A stand-in listener: holds the lock, as `serve --auto` does, says
    /// `held PID` and keeps it until its stdin ends; or says `lost PID` when
    /// another holds it past its wait.
    #[test]
    #[ignore = "run by the tests as a copy of the listener"]
    fn fake_listener() {
        let Some(d) = std::env::var_os(DIR).map(PathBuf::from) else { return };
        let wait = std::env::var(WAIT_MS).ok().and_then(|w| w.parse().ok()).map_or(WAIT, Duration::from_millis);
        let me = std::process::id();
        match hold(&paths(&d), wait).unwrap() {
            Some(h) => {
                say(&format!("held {me}"));
                let mut rest = Vec::new();
                let r = io::stdin().read_to_end(&mut rest);
                drop(h);
                say(&format!("eof {r:?} {}", String::from_utf8_lossy(&rest).trim()));
            }
            None => say(&format!("lost {me}")),
        }
    }

    /// A stand-in session: says `ready`, waits for a line on stdin, then runs
    /// SessionStart's [`ensure`] and says `started` or `running`. The listener
    /// it starts shares its stdin and stderr.
    #[test]
    #[ignore = "run by the tests as a copy of a session"]
    fn fake_session() {
        let Some(d) = std::env::var_os(DIR).map(PathBuf::from) else { return };
        say("ready");
        let mut b = [0u8; 1];
        while io::stdin().read(&mut b).unwrap() == 1 && b[0] != b'\n' {}
        let got = ensure(&paths(&d), |p| {
            let mut c = copy(p.lock.parent().unwrap(), "fake_listener");
            c.stdin(Stdio::inherit()).stderr(Stdio::inherit());
            spawn_detached(c).map(|c| c.id())
        })
        .unwrap();
        say(match got {
            Ensured::Started(_) | Ensured::Unnamed(..) => "started",
            Ensured::Running(_) => "running",
        });
    }

    #[test]
    fn two_sessions_starting_at_once_give_one_listener() {
        // Whichever runs first, the other finds the lock held, or free with a
        // live listener named (the start window), so one "started" holds
        // in any order (#402); `a_session_in_the_start_window_starts_nothing`
        // pins the window itself.
        for round in 0..3 {
            let d = dir(&format!("race{round}"));
            let mut sessions: Vec<Child> = (0..2).map(|_| copy(&d.0, "fake_session").spawn().unwrap()).collect();
            let mut errs: Vec<_> = sessions.iter_mut().map(|s| BufReader::new(s.stderr.take().unwrap())).collect();
            for e in &mut errs {
                assert_eq!(words(e, 1), ["ready"], "round {round}");
            }
            // Both are ready: let them go together. The stdins are kept out of
            // the `Child`, whose `wait` would close them: they are the
            // listener's stdin too.
            let mut ins: Vec<_> = sessions.iter_mut().map(|s| s.stdin.take().unwrap()).collect();
            for i in &mut ins {
                i.write_all(b"go\n").unwrap();
            }
            let mut said: Vec<String> = Vec::new();
            for (s, e) in sessions.iter_mut().zip(&mut errs) {
                let w = words(e, 1);
                assert!(s.wait().unwrap().success(), "round {round}");
                said.extend(w.clone());
                if w == ["started"] {
                    // Its listener says whether it holds the lock.
                    said.extend(words(e, 1));
                }
            }
            said.sort();
            assert_eq!(said.len(), 3, "round {round}: {said:?}");
            assert_eq!(said[1..], ["running", "started"], "round {round}: one session starts the listener: {said:?}");
            assert!(said[0].starts_with("held "), "round {round}: {said:?}");
            let pid: u32 = said[0]["held ".len()..].parse().unwrap();
            if recorded(&paths(&d.0)) != Some(pid) {
                let alive = crate::platform::process::alive(pid);
                let file = fs::read_to_string(&paths(&d.0).pid);
                let rest: Vec<String> = if alive { Vec::new() } else { errs.iter_mut().flat_map(|e| words(e, usize::MAX)).collect() };
                panic!("round {round}: the pid file does not name the listener: {file:?}, alive {alive}, then it said {rest:?}");
            }
            // End the listener (its stdin is the sessions' stdin) and read what
            // is left: its end only, so no second listener ran.
            drop(ins);
            let rest: Vec<String> = errs.iter_mut().flat_map(|e| words(e, usize::MAX)).collect();
            assert_eq!(rest, ["eof Ok(0)"], "round {round}");
        }
    }

    #[test]
    fn two_listeners_starting_at_once_leave_one_holding() {
        // A session's listener and one started by hand: one holds the lock, the
        // other leaves once its wait is over.
        let d = dir("twolisteners");
        let mut ls = [Listener::spawn(&d.0, Some(500)), Listener::spawn(&d.0, Some(500))];
        let mut said: Vec<String> = ls.iter_mut().map(|l| l.said().split(' ').next().unwrap().to_string()).collect();
        said.sort();
        assert_eq!(said, ["held", "lost"]);
        for l in ls {
            l.stop();
        }
    }

    /// A pid that ran and is gone.
    fn dead_pid() -> u32 {
        let mut c = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "no_such_test", "--quiet"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    #[test]
    fn a_pid_file_naming_a_dead_pid_starts_a_listener() {
        let d = dir("deadpid");
        let p = paths(&d.0);
        let dead = dead_pid();
        fs::write(&p.pid, format!("{dead} 0\n")).unwrap();
        assert_eq!(recorded(&p), None, "a dead pid is no listener");
        let (got, l) = ensure_fake(&p);
        let mut l = l.expect("no listener started");
        assert_eq!(got, Ensured::Started(l.child.id()));
        assert_eq!(l.said(), format!("held {}", l.child.id()));
        assert_eq!(recorded(&p), Some(l.child.id()), "the listener rewrote the pid file");
        l.stop();
    }

    #[test]
    fn a_pid_file_naming_a_reused_pid_starts_a_listener() {
        // The pid is alive (this process), but it is not the process the file
        // recorded: its start time differs.
        let d = dir("reusedpid");
        let p = paths(&d.0);
        let me = std::process::id();
        let start = crate::platform::process::start_time(me);
        fs::write(&p.pid, format!("{me} {}\n", start.map_or(1, |s| s + 1))).unwrap();
        if start.is_none() {
            // No start time to tell them apart (a Unix other than Linux and
            // macOS): a live pid in a fresh pid file is taken for a listener
            // that is starting.
            assert_eq!(ensure(&p, |_| panic!("started a listener")).unwrap(), Ensured::Running(Some(me)));
            return;
        }
        assert_eq!(recorded(&p), None, "a pid with another start time is no listener");
        let (got, l) = ensure_fake(&p);
        let mut l = l.expect("no listener started");
        assert_eq!(got, Ensured::Started(l.child.id()));
        assert_eq!(l.said(), format!("held {}", l.child.id()));
        assert_eq!(recorded(&p), Some(l.child.id()));
        l.stop();
    }

    #[test]
    fn an_old_pid_file_naming_a_live_pid_with_the_lock_free_starts_a_listener() {
        // #402 re-review: a free lock and a live pid the pid file names mean
        // a listener is starting only within the start window. Past it, the
        // pid is one the OS gave to another process (after a reboot or a
        // crash), whether or not a start time tells them apart.
        let d = dir("oldpid");
        let p = paths(&d.0);
        let me = std::process::id();
        fs::write(&p.pid, pid_text(me)).unwrap();
        let old = std::time::SystemTime::now() - 4 * WAIT;
        File::options().write(true).open(&p.pid).unwrap().set_modified(old).unwrap();
        assert_eq!(recorded(&p), Some(me), "the pid is alive and its start time matches");
        let (got, l) = ensure_fake(&p);
        let mut l = l.expect("no listener started");
        assert_eq!(got, Ensured::Started(l.child.id()));
        assert_eq!(l.said(), format!("held {}", l.child.id()));
        l.stop();
    }

    #[test]
    fn a_listener_that_exits_is_started_again() {
        let d = dir("exits");
        let p = paths(&d.0);
        let (_, l) = ensure_fake(&p);
        let mut first = l.expect("no listener started");
        let pid = first.child.id();
        assert_eq!(first.said(), format!("held {pid}"));
        // While it runs, a session starts nothing.
        assert_eq!(ensure(&p, |_| panic!("started a second listener")).unwrap(), Ensured::Running(Some(pid)));
        first.stop();
        // Its pid file is left behind; the free lock says no listener runs.
        let (got, l) = ensure_fake(&p);
        let mut second = l.expect("no listener started again");
        assert_eq!(got, Ensured::Started(second.child.id()));
        assert_eq!(second.said(), format!("held {}", second.child.id()));
        second.stop();
    }

    /// A stand-in hook: its stdout is its reader's pipe, as Claude Code runs
    /// a command hook. It starts a stand-in listener as [`spawn_listener`]
    /// does (stdin closed, output to a log), says its pid on stdout, and
    /// exits.
    #[test]
    #[ignore = "run by the tests as a copy of a hook"]
    fn fake_hook() {
        let Some(d) = std::env::var_os(DIR).map(PathBuf::from) else { return };
        let log = File::create(d.join("listener.log")).unwrap();
        let mut c = copy(&d, "fake_blocked_listener");
        c.stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log);
        // Not waited on: the hook exits while its listener runs.
        let pid = spawn_detached(c).map(|l| l.id()).unwrap();
        println!("{SAY}pid {pid}");
    }

    /// A stand-in listener that lives until it takes `blocker` in its folder,
    /// which the test holds.
    #[test]
    #[ignore = "run by the tests as a copy of a listener"]
    fn fake_blocked_listener() {
        let Some(d) = std::env::var_os(DIR).map(PathBuf::from) else { return };
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(d.join("blocker")).unwrap();
        f.lock().unwrap();
    }

    #[test]
    fn a_hook_s_output_ends_when_the_hook_does_while_its_listener_runs() {
        // #402: the listener a hook starts must not keep the hook's stdout,
        // or the session waits on it for the listener's life.
        let d = dir("hookpipe");
        let blocker = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(d.0.join("blocker")).unwrap();
        blocker.lock().unwrap();
        let mut hook = copy(&d.0, "fake_hook");
        hook.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut hook = hook.spawn().unwrap();
        let mut out = String::new();
        hook.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        assert!(hook.wait().unwrap().success());
        let pid: u32 = out.lines().find_map(|l| l.strip_prefix(SAY)?.strip_prefix("pid ")?.parse().ok()).unwrap_or_else(|| panic!("no pid in {out:?}"));
        assert!(crate::platform::process::alive(pid), "the listener ran while the hook's output ended");
        drop(blocker);
    }

    #[test]
    fn session_start_does_nothing_with_autostart_off() {
        let d = dir("off");
        let mut s = crate::memory::settings::Settings::from_config(None, &d.0.join("i.sqlite")).unwrap();
        s.store = d.0.join("memory.sqlite");
        assert_eq!(session_start(&s), None);
        s.pr_events = Some(crate::pr_listen::Settings { port: Some(8765), ..Default::default() });
        assert_eq!(session_start(&s), None);
        assert!(!paths(&s.store).lock.exists(), "the lock was not even tried");
    }

    #[test]
    fn the_listener_reads_the_configuration_its_starter_read() {
        let v = |a: &[&str]| config_args(a.iter().map(|s| (*s).to_string()));
        assert_eq!(v(&["--config", "C:/p/atlas.json", "--hook"]), ["--config", "C:/p/atlas.json"]);
        // The last of each, as main.rs reads them (re-review nit 3).
        assert_eq!(v(&["--project", "web", "--config", "a.json", "--project", "other"]), ["--config", "a.json", "--project", "other"]);
        assert_eq!(v(&["--config", "a.json", "--config", "b.json", "--hook"]), ["--config", "b.json"]);
        assert!(v(&["--hook"]).is_empty());
        assert!(v(&["--config"]).is_empty(), "a flag with no value");
    }

    #[test]
    fn a_session_in_the_start_window_starts_nothing() {
        // #402: a session that finds the lock free while the listener another
        // session started has not taken it yet. The stand-in is a live copy
        // that never takes the lock (a session waiting for its `go`).
        let d = dir("window");
        let p = paths(&d.0);
        let mut starting = copy(&d.0, "fake_session").spawn().unwrap();
        let mut err = BufReader::new(starting.stderr.take().unwrap());
        assert_eq!(words(&mut err, 1), ["ready"]);
        let pid = starting.id();
        assert_eq!(ensure(&p, |_| Ok(pid)).unwrap(), Ensured::Started(pid));
        assert_eq!(recorded(&p), Some(pid), "named before the lock was let go");
        assert_eq!(ensure(&p, |_| panic!("started a second listener")).unwrap(), Ensured::Running(Some(pid)));
        starting.kill().unwrap();
        starting.wait().unwrap();
        // Gone before it took the lock: the next session starts one.
        let (got, l) = ensure_fake(&p);
        let mut l = l.expect("no listener started");
        assert_eq!(got, Ensured::Started(l.child.id()));
        assert_eq!(l.said(), format!("held {}", l.child.id()));
        l.stop();
    }

    #[test]
    fn the_log_is_appended_to_and_a_failed_last_listener_is_told() {
        let d = dir("log");
        let log = paths(&d.0).log;
        assert_eq!(last_failure(&log), None, "no log");
        fs::write(&log, "atlas pr-events: listening on 127.0.0.1:8765\natlas pr-events: POST 200 review\n").unwrap();
        assert_eq!(last_failure(&log), None, "a listener that served and was stopped");
        {
            use std::io::Write as _;
            let mut f = open_log(&log).unwrap();
            f.write_all(b"atlas: bind 127.0.0.1:8765: address in use\n\n").unwrap();
        }
        assert!(fs::read_to_string(&log).unwrap().starts_with("atlas pr-events: listening"), "appended, not cut");
        let failed = last_failure(&log);
        assert_eq!(failed.as_deref(), Some("atlas: bind 127.0.0.1:8765: address in use"));
        let line = started_line(7, &log, failed.as_deref());
        assert!(line.contains("again") && line.contains("address in use") && line.contains("pid 7"), "{line}");
        assert!(!started_line(7, &log, None).contains("failed"));
        // An error that runs on for lines (the usage after it), as the live
        // SessionStart check showed.
        fs::write(&log, "atlas: pr-events serve needs --port\natlas pr-events serve [--port N]\n  Listens on 127.0.0.1:PORT\n").unwrap();
        assert_eq!(last_failure(&log).as_deref(), Some("atlas: pr-events serve needs --port"));
        // A later start that served: not a failure.
        fs::write(&log, "atlas: bind: in use\natlas pr-events: listening on 127.0.0.1:8765\n").unwrap();
        assert_eq!(last_failure(&log), None);
        // Over the cap, a start cuts it.
        fs::write(&log, vec![b'x'; LOG_CAP as usize + 1]).unwrap();
        drop(open_log(&log).unwrap());
        assert_eq!(fs::metadata(&log).unwrap().len(), 0);
    }

    #[test]
    fn an_invalid_section_asking_for_autostart_is_told() {
        // Review nit 6: the settings' `.ok()` never drops one in silence, as
        // the configuration does not load with it, and says why.
        let d = dir("invalid");
        let e = crate::config::Config::parse(r#"{"pr_events":{"autostart":true}}"#, &d.0).unwrap_err().to_string();
        assert!(e.contains("pr_events.autostart needs pr_events.port"), "{e}");
        let c = crate::config::Config::parse(r#"{"pr_events":{"autostart":true,"port":8765}}"#, &d.0).unwrap();
        let s = crate::memory::settings::Settings::from_config(Some(&c), &d.0.join("i.sqlite")).unwrap();
        assert!(s.pr_events.is_some_and(|p| p.autostart));
    }

    #[test]
    fn a_listener_started_but_not_named_is_told_as_started() {
        // Re-review nit 5: the pid file cannot be written (a folder stands
        // in its place) after the listener started; it runs, so the line
        // says it started, not that the start failed.
        let d = dir("unnamed");
        let p = paths(&d.0);
        fs::create_dir_all(&p.pid).unwrap();
        let got = ensure(&p, |_| Ok(7)).unwrap();
        let Ensured::Unnamed(7, e) = got else { panic!("{got:?}") };
        let line = unnamed_line(7, &e);
        assert!(line.starts_with("atlas: started the pr-events listener") && line.contains("pid 7") && !line.contains("failed"), "{line}");
    }

    #[test]
    fn a_process_s_start_time_is_read_where_atlas_ships() {
        // Re-review should-fix 1: macOS reads it too, so a reused pid is told
        // apart there as on Windows and Linux.
        let me = std::process::id();
        let s = crate::platform::process::start_time(me);
        if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
            assert!(s.is_some_and(|s| s != 0), "{s:?}");
            assert_eq!(crate::platform::process::start_time(me), s, "the same each read");
        }
    }
}
