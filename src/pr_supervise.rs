//! The forwarder supervisor of `atlas pr-events serve` (design:
//! `docs/design/pr-events.md` section 7).
//!
//! One `gh webhook forward` child per repository the listener serves (the
//! project's `repos` list, each with a remote), started from an argument
//! vector, never a shell line. Before each start the repository's secret is
//! rotated ([`Secrets::rotate`]) and handed to gh as `--secret`, the one
//! place it goes: never a file, a log or the journal. A child that exits is
//! restarted with [`Backoff`]. Stop ends every child with its whole process
//! tree (gh runs the extension as its own child), and each child is tied to
//! atlas's life ([`crate::platform::process::tie`]).
//!
//! Hooks (Q6, journal:ATLAS-USES:0018, and the review's H1): gh-webhook makes
//! a `cli` hook on the repository at each start, and nothing in the hook
//! tells atlas's apart from anyone else's. So ownership is proved, never
//! inferred: after a start, each `cli` hook that is new, asks for exactly
//! [`EVENTS`] and was made around the start is a candidate and is pinged;
//! a candidate is recorded only once the listener has had a delivery that
//! names it, signed with the secret atlas made for this start ([`Proofs`]).
//! Cleanup, before the next start and on stop, lists the hooks and deletes
//! only recorded ids still named `cli` with those events, each delete logged.
//! A hook atlas did not prove is never recorded and never deleted; one of
//! atlas's own that was never proved is left in place.
//!
//! The recorded ids persist (Q10, journal:ATLAS-USES:0023) in the memory
//! store's `mem_meta` table ([`HOOKS_KEY`], ids only, never a secret)
//! through [`Ids`]. Only the holder of the forwarder lease ([`LEASE_KEY`],
//! M2) runs forwarders, sweeps what an earlier run left or deletes anything;
//! another `serve` on the same store serves its listener without forwarders
//! until the lease is free or stale.
//!
//! Every call to GitHub goes through [`Gh`], so the tests drive a fake gh
//! and never reach the network.

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Read};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::json::Value;
use crate::memory::{Draft, MemoryStore};
use crate::pr_listen::{say, Secrets, PATH};

/// The events a forwarder asks GitHub for (design section 2).
pub const EVENTS: &str = "pull_request_review,pull_request_review_comment,issue_comment,pull_request";

/// The name gh-webhook gives every hook it makes.
pub const HOOK_NAME: &str = "cli";

/// The `mem_meta` key that holds the hook ids atlas recorded and has not yet
/// deleted: one line per repository, `OWNER/NAME ID ID ...` (on a host other
/// than github.com, `HOST/OWNER/NAME ID ...`). Ids only.
pub const HOOKS_KEY: &str = "pr_events_hooks";

/// The `mem_meta` key of the forwarder lease: `PID START HEARTBEAT_MS` of the
/// one `serve` that runs forwarders, or empty when none does.
pub const LEASE_KEY: &str = "pr_events_lease";

/// The stored hook ids, per repository (`OWNER/NAME`, its ids).
pub type Stored = Vec<(String, Vec<u64>)>;

/// The recorded hook ids in the store, per repository (`OWNER/NAME`), each
/// repository with at least one id.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn stored_hooks(store: &MemoryStore) -> crate::Result<Stored> {
    Ok(parse_stored(&store.meta_get(HOOKS_KEY)?.unwrap_or_default()))
}

/// Writes `ids` as `repo`'s whole list of recorded hook ids (none drops the
/// repository), leaving every other repository's list as it is. The read and
/// the write are one transaction, so two processes cannot lose each other's
/// ids.
///
/// # Errors
///
/// [`crate::Error::Db`] or [`crate::Error::StoreTooNew`].
pub fn store_hooks(store: &MemoryStore, repo: &str, ids: &[u64]) -> crate::Result<()> {
    store.meta_update(|s| {
        let mut all = parse_stored(&s.meta_get(HOOKS_KEY)?.unwrap_or_default());
        all.retain(|(r, _)| r != repo);
        if !ids.is_empty() {
            all.push((repo.to_string(), ids.to_vec()));
        }
        all.sort();
        let text: Vec<String> =
            all.iter().map(|(r, v)| std::iter::once(r.clone()).chain(v.iter().map(u64::to_string)).collect::<Vec<_>>().join(" ")).collect();
        s.meta_set(HOOKS_KEY, &text.join("\n"))
    })
}

fn parse_stored(text: &str) -> Stored {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let repo = w.next()?.to_string();
            let ids: Vec<u64> = w.filter_map(|x| x.parse().ok()).collect();
            (!ids.is_empty()).then_some((repo, ids))
        })
        .collect()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A process that may hold the forwarder lease: its pid and its start time
/// ([`crate::platform::process::start_time`], 0 where it is not read), so a
/// reused pid is not taken for the holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holder {
    /// The process id.
    pub pid: u32,
    /// When it started, or 0.
    pub start: u64,
}

impl Holder {
    /// This process.
    pub fn me() -> Holder {
        let pid = std::process::id();
        Holder { pid, start: crate::platform::process::start_time(pid).unwrap_or(0) }
    }

    /// Whether a lease this holder last beat at `beat_ms` is free to take at
    /// `now_ms`: the process is gone, its start differs (the pid was
    /// reused), or the heartbeat is older than `stale`.
    fn stale(self, beat_ms: u64, now_ms: u64, stale: Duration) -> bool {
        !crate::platform::process::alive(self.pid)
            || (self.start != 0 && crate::platform::process::start_time(self.pid).is_some_and(|s| s != self.start))
            || now_ms.saturating_sub(beat_ms) > u64::try_from(stale.as_millis()).unwrap_or(u64::MAX)
    }
}

fn parse_lease(v: &str) -> Option<(Holder, u64)> {
    let mut w = v.split_whitespace();
    let (pid, start, beat) = (w.next()?.parse().ok()?, w.next()?.parse().ok()?, w.next()?.parse().ok()?);
    Some((Holder { pid, start }, beat))
}

fn lease_text(h: Holder, beat_ms: u64) -> String {
    format!("{} {} {beat_ms}", h.pid, h.start)
}

/// Takes the forwarder lease for `me` when it is absent, empty, stale or
/// already `me`'s: one transaction reads and writes it. False when another
/// live holder has it.
///
/// # Errors
///
/// [`crate::Error::Db`] or [`crate::Error::StoreTooNew`].
pub fn lease_acquire(store: &MemoryStore, me: Holder, now_ms: u64, stale: Duration) -> crate::Result<bool> {
    store.meta_update(|s| {
        if let Some((h, beat)) = s.meta_get(LEASE_KEY)?.as_deref().and_then(parse_lease) {
            if h != me && !h.stale(beat, now_ms, stale) {
                return Ok(false);
            }
        }
        s.meta_set(LEASE_KEY, &lease_text(me, now_ms))?;
        Ok(true)
    })
}

/// The holder's heartbeat: a compare-and-set that writes `now_ms` only while
/// the lease still names `me`. False: the lease is lost.
///
/// # Errors
///
/// [`crate::Error::Db`] or [`crate::Error::StoreTooNew`].
pub fn lease_beat(store: &MemoryStore, me: Holder, now_ms: u64) -> crate::Result<bool> {
    store.meta_update(|s| {
        if lease_names(s, me)? {
            s.meta_set(LEASE_KEY, &lease_text(me, now_ms))?;
            Ok(true)
        } else {
            Ok(false)
        }
    })
}

/// Gives the lease up on a clean stop: the same compare-and-set, writing the
/// empty value. False when `me` did not hold it.
///
/// # Errors
///
/// [`crate::Error::Db`] or [`crate::Error::StoreTooNew`].
pub fn lease_release(store: &MemoryStore, me: Holder) -> crate::Result<bool> {
    store.meta_update(|s| {
        if lease_names(s, me)? {
            s.meta_set(LEASE_KEY, "")?;
            Ok(true)
        } else {
            Ok(false)
        }
    })
}

/// Whether the lease names `me`.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn lease_names(store: &MemoryStore, me: Holder) -> crate::Result<bool> {
    Ok(store.meta_get(LEASE_KEY)?.as_deref().and_then(parse_lease).is_some_and(|(h, _)| h == me))
}

/// Where the supervisor keeps the hook ids it recorded, so that a run that
/// did not stop cleanly leaves them for the next one to sweep, and the
/// forwarder lease.
pub trait Ids: Send + Sync {
    /// Every repository's stored ids ([`stored_hooks`]).
    ///
    /// # Errors
    ///
    /// The store could not be read, in one line.
    fn load(&self) -> std::result::Result<Stored, String>;

    /// Replaces `repo`'s stored ids with `ids` ([`store_hooks`]).
    ///
    /// # Errors
    ///
    /// The store could not be written, in one line.
    fn save(&self, repo: &str, ids: &[u64]) -> std::result::Result<(), String>;

    /// [`lease_acquire`].
    ///
    /// # Errors
    ///
    /// The store failed, in one line.
    fn acquire(&self, me: Holder, now_ms: u64, stale: Duration) -> std::result::Result<bool, String>;

    /// [`lease_beat`].
    ///
    /// # Errors
    ///
    /// The store failed, in one line.
    fn beat(&self, me: Holder, now_ms: u64) -> std::result::Result<bool, String>;

    /// [`lease_release`].
    ///
    /// # Errors
    ///
    /// The store failed, in one line.
    fn release(&self, me: Holder) -> std::result::Result<bool, String>;

    /// [`lease_names`]; a store that fails counts as not holding.
    fn holds(&self, me: Holder) -> bool;
}

/// The memory store itself, under a lock.
impl Ids for Mutex<MemoryStore> {
    fn load(&self) -> std::result::Result<Stored, String> {
        stored_hooks(&self.lock().unwrap_or_else(PoisonError::into_inner)).map_err(|e| e.to_string())
    }

    fn save(&self, repo: &str, ids: &[u64]) -> std::result::Result<(), String> {
        store_hooks(&self.lock().unwrap_or_else(PoisonError::into_inner), repo, ids).map_err(|e| e.to_string())
    }

    fn acquire(&self, me: Holder, now_ms: u64, stale: Duration) -> std::result::Result<bool, String> {
        lease_acquire(&self.lock().unwrap_or_else(PoisonError::into_inner), me, now_ms, stale).map_err(|e| e.to_string())
    }

    fn beat(&self, me: Holder, now_ms: u64) -> std::result::Result<bool, String> {
        lease_beat(&self.lock().unwrap_or_else(PoisonError::into_inner), me, now_ms).map_err(|e| e.to_string())
    }

    fn release(&self, me: Holder) -> std::result::Result<bool, String> {
        lease_release(&self.lock().unwrap_or_else(PoisonError::into_inner), me).map_err(|e| e.to_string())
    }

    fn holds(&self, me: Holder) -> bool {
        lease_names(&self.lock().unwrap_or_else(PoisonError::into_inner), me).unwrap_or(false)
    }
}

/// The hooks the listener has proof of, per repository: a delivery that
/// named the hook and verified under the secret of the repository's current
/// forwarder start ([`crate::pr_listen::proof`]). Cleared at each start.
#[derive(Debug, Clone, Default)]
pub struct Proofs(Arc<Mutex<Vec<(String, u64)>>>);

impl Proofs {
    /// No proofs yet.
    pub fn new() -> Proofs {
        Proofs::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(String, u64)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a proof that hook `id` belongs to `repo`'s forwarder.
    pub fn add(&self, repo: &str, id: u64) {
        let repo = repo.to_ascii_lowercase();
        let mut v = self.lock();
        if !v.iter().any(|(r, i)| *r == repo && *i == id) {
            v.push((repo, id));
        }
    }

    /// Whether hook `id` is proved to be `repo`'s.
    pub fn has(&self, repo: &str, id: u64) -> bool {
        self.lock().iter().any(|(r, i)| r.eq_ignore_ascii_case(repo) && *i == id)
    }

    /// Drops `repo`'s proofs (a new start, a new secret).
    fn clear(&self, repo: &str) {
        self.lock().retain(|(r, _)| !r.eq_ignore_ascii_case(repo));
    }
}

/// What gh-webhook prints when GitHub answers 403 to the hook it asks for.
const NO_ACCESS: &str = "you do not have access to this feature";

/// What gh prints when the gh-webhook extension is not installed.
const NO_EXTENSION: &str = "unknown command \"webhook\"";

/// The longest a `gh api` call may take.
const API_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest one line of a forwarder's output is shown.
const LINE_CHARS: usize = 300;

/// How far around a start a candidate hook's `created_at` may be (clock
/// skew between this machine and GitHub).
const SKEW: Duration = Duration::from_secs(120);

/// Runs gh. [`SystemGh`] runs the real one; the tests use a fake.
pub trait Gh: Send + Sync {
    /// Runs `gh ARGS` to its end (bounded), output captured.
    ///
    /// # Errors
    ///
    /// gh could not start ([`io::ErrorKind::NotFound`]: not installed) or
    /// did not finish in time.
    fn run(&self, args: &[String]) -> io::Result<Output>;

    /// Starts `gh ARGS` in a process group of its own
    /// ([`crate::platform::process::own_group`]), stdin closed, stdout and
    /// stderr piped.
    ///
    /// # Errors
    ///
    /// gh could not start.
    fn spawn(&self, args: &[String]) -> io::Result<Child>;
}

/// The `gh` on `PATH`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGh;

fn gh() -> Command {
    Command::new("gh")
}

impl Gh for SystemGh {
    fn run(&self, args: &[String]) -> io::Result<Output> {
        let mut c = gh();
        c.args(args);
        output_within(c, API_TIMEOUT)
    }

    fn spawn(&self, args: &[String]) -> io::Result<Child> {
        let mut c = gh();
        c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        crate::platform::process::own_group(&mut c);
        c.spawn()
    }
}

/// Runs `cmd` to its end, output captured; past `limit` it is ended with
/// its process tree and the answer is [`io::ErrorKind::TimedOut`].
fn output_within(mut cmd: Command, limit: Duration) -> io::Result<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::platform::process::own_group(&mut cmd);
    let mut child = cmd.spawn()?;
    let _tie = crate::platform::process::tie(&child);
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            end(&mut child);
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("gh did not finish within {} s", limit.as_secs())));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let joined = |h: Option<JoinHandle<Vec<u8>>>| h.and_then(|h| h.join().ok()).unwrap_or_default();
    Ok(Output { status, stdout: joined(out), stderr: joined(err) })
}

fn drain(mut r: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = r.read_to_end(&mut b);
        b
    })
}

/// Ends a child and everything it started, and reaps it.
fn end(child: &mut Child) {
    crate::platform::process::terminate_tree(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// How long the supervisor waits, and for what.
#[derive(Debug, Clone)]
pub struct Timing {
    /// The first restart's wait.
    pub first: Duration,
    /// The longest restart wait (each one doubles up to it).
    pub max: Duration,
    /// A child that ran this long starts the waits over.
    pub reset_after: Duration,
    /// The waits between the hook lists after a start, until the new hook
    /// is proved.
    pub hook_waits: Vec<Duration>,
    /// How often a running child is checked.
    pub tick: Duration,
    /// How often the lease holder beats, and a non-holder tries to take it.
    pub lease_tick: Duration,
    /// A lease not beaten for this long may be taken over.
    pub lease_stale: Duration,
}

impl Default for Timing {
    /// 1 s doubling to 5 min, reset after 10 min up; the hook looked for at
    /// 1, 3, 7 and 15 s after a start; the lease beaten every 15 s and stale
    /// after 60 s.
    fn default() -> Timing {
        Timing {
            first: Duration::from_secs(1),
            max: Duration::from_secs(300),
            reset_after: Duration::from_secs(600),
            hook_waits: [1, 2, 4, 8].into_iter().map(Duration::from_secs).collect(),
            tick: Duration::from_millis(250),
            lease_tick: Duration::from_secs(15),
            lease_stale: Duration::from_secs(60),
        }
    }
}

/// The restart waits: `first`, doubling to `max`, back to `first` after a
/// child that ran `reset_after` or longer.
#[derive(Debug, Clone)]
pub struct Backoff {
    next: Duration,
    first: Duration,
    max: Duration,
    reset_after: Duration,
}

impl Backoff {
    /// The waits `t` sets.
    pub fn new(t: &Timing) -> Backoff {
        Backoff { next: t.first, first: t.first, max: t.max, reset_after: t.reset_after }
    }

    /// The wait before the next start, after a child that ran `up`.
    pub fn after(&mut self, up: Duration) -> Duration {
        if up >= self.reset_after {
            self.next = self.first;
        }
        let d = self.next.min(self.max);
        self.next = d.saturating_mul(2).min(self.max);
        d
    }
}

/// What a log line is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    /// A forwarder started.
    Started,
    /// The hook a forwarder made was proved and recorded.
    Recorded,
    /// A forwarder ended and will start again.
    Restart,
    /// A forwarder will not start again (gh or gh-webhook missing, no
    /// access, a repository name that does not read).
    Fatal,
    /// What a cleanup's hook list found.
    Found,
    /// A recorded hook was deleted.
    Deleted,
    /// Something failed and was skipped.
    Error,
    /// The repository's forwarder stopped for good.
    Stopped,
    /// The forwarder lease was not free, was taken, or was lost.
    Lease,
}

impl What {
    /// The word the journal note's meta carries.
    pub fn name(self) -> &'static str {
        match self {
            What::Started => "started",
            What::Recorded => "recorded",
            What::Restart => "restart",
            What::Fatal => "fatal",
            What::Found => "found",
            What::Deleted => "deleted",
            What::Error => "error",
            What::Stopped => "stopped",
            What::Lease => "lease",
        }
    }
}

/// One thing the supervisor did. The line never holds a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    /// The repository, `owner/name`.
    pub repo: String,
    /// What it is about.
    pub what: What,
    /// One line.
    pub line: String,
    /// Whether it is a change of state worth a journal note (M4); every
    /// line goes to stderr.
    pub keep: bool,
}

impl Log {
    /// Whether it goes to the journal as well as stderr: a fatal end, a
    /// delete, the first restart of a run and a summary every 10 restarts
    /// or every hour, and a cleanup that found something to delete or a
    /// foreign `cli` hook not seen before. Repeats go to stderr only.
    pub fn journal(&self) -> bool {
        self.keep
    }
}

/// Where the supervisor's log lines go.
pub type Sink = Arc<dyn Fn(Log) + Send + Sync>;

/// One repository's log: [`Say::out`] for stderr, [`Say::keep`] for the
/// journal too.
struct Say<'a> {
    sink: &'a Sink,
    repo: &'a str,
}

impl Say<'_> {
    /// A line for stderr; a fatal end and a delete go to the journal too.
    fn out(&self, what: What, line: String) {
        let keep = matches!(what, What::Fatal | What::Deleted);
        (self.sink)(Log { repo: self.repo.to_string(), what, line, keep });
    }

    /// A line for stderr and the journal.
    fn keep(&self, what: What, line: String) {
        (self.sink)(Log { repo: self.repo.to_string(), what, line, keep: true });
    }
}

/// Files a log line as a journal note in `lane`, meta `forwarder:` and
/// `what:` (never `event:`, `met:` or `done:`, which other readers own).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn journal(store: &mut MemoryStore, lane: &str, l: &Log) -> crate::Result<String> {
    let title = format!("pr-events forwarder {}: {}", l.repo, l.line);
    let d = Draft {
        title: title.chars().take(200).collect(),
        meta: format!("forwarder: {}\nwhat: {}", l.repo, l.what.name()),
        body: title,
        reason: "the PR events forwarder supervisor (atlas pr-events serve)".into(),
        author: "atlas".into(),
        client: "atlas-pr-events".into(),
        provenance: None,
        time: None,
    };
    let mut w = store.writer()?;
    let key = w.next_key(&format!("journal:{lane}:"), 4)?;
    w.create(&key, "journal", "note", lane, &d, None)?;
    w.commit()?;
    Ok(key)
}

/// Whether `repo` is `OWNER/NAME`, each part of letters, digits and `-_.`
/// and neither `.` nor `..`: the only form that goes into a gh argument or
/// an API path.
pub fn valid_repo(repo: &str) -> bool {
    let part = |p: &str| !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    repo.split_once('/').is_some_and(|(o, n)| part(o) && part(n))
}

/// A served repository: `OWNER/NAME` on github.com, or on another host
/// (L9), whose gh calls then carry that host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The host, when it is not github.com.
    pub host: Option<String>,
    /// `OWNER/NAME`, lowercased as the secrets are.
    pub repo: String,
}

impl Target {
    /// Reads `OWNER/NAME` or `HOST/OWNER/NAME` (github.com as the host is
    /// the same as none); `None` when it is not one of those forms.
    pub fn parse(spec: &str) -> Option<Target> {
        let (host, repo) = match spec.split_once('/') {
            Some((h, rest)) if rest.contains('/') => (Some(h), rest),
            _ => (None, spec),
        };
        if !valid_repo(repo) {
            return None;
        }
        let host = match host {
            None => None,
            Some(h) if h.eq_ignore_ascii_case("github.com") => None,
            Some(h) if !h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric() || "-.:".contains(c)) && !h.starts_with('-') => {
                Some(h.to_ascii_lowercase())
            }
            Some(_) => return None,
        };
        Some(Target { host, repo: repo.to_ascii_lowercase() })
    }

    /// The key its stored ids go under: `OWNER/NAME`, or `HOST/OWNER/NAME`.
    pub fn key(&self) -> String {
        match &self.host {
            None => self.repo.clone(),
            Some(h) => format!("{h}/{}", self.repo),
        }
    }

    /// `gh api` with this host's `--hostname`, then `--method METHOD PATH`.
    fn api(&self, method: &str, path: &str) -> Vec<String> {
        let mut v = vec!["api".to_string()];
        if let Some(h) = &self.host {
            v.extend(["--hostname".to_string(), h.clone()]);
        }
        v.extend(["--method".to_string(), method.to_string(), path.to_string()]);
        v
    }
}

/// The forwarder's arguments, after `gh`.
pub fn forward_args(t: &Target, port: u16, secret: &str) -> Vec<String> {
    let mut v: Vec<String> = [
        "webhook",
        "forward",
        "--repo",
        &t.repo,
        "--events",
        EVENTS,
        "--url",
        &format!("http://127.0.0.1:{port}{PATH}"),
        "--secret",
        secret,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    if let Some(h) = &t.host {
        v.extend(["--github-host".to_string(), h.clone()]);
    }
    v
}

/// The arguments that list a repository's hooks, every page, as an array of
/// pages (L8).
pub fn list_args(t: &Target) -> Vec<String> {
    let mut v = t.api("GET", &format!("repos/{}/hooks?per_page=100", t.repo));
    let path = v.pop().unwrap_or_default();
    v.extend(["--paginate".to_string(), "--slurp".to_string(), path]);
    v
}

/// The arguments that read hook `id` (a 404 means it is gone).
pub fn get_args(t: &Target, id: u64) -> Vec<String> {
    t.api("GET", &format!("repos/{}/hooks/{id}", t.repo))
}

/// The arguments that delete hook `id`.
pub fn delete_args(t: &Target, id: u64) -> Vec<String> {
    t.api("DELETE", &format!("repos/{}/hooks/{id}", t.repo))
}

/// The arguments that ask GitHub to ping hook `id`.
pub fn ping_args(t: &Target, id: u64) -> Vec<String> {
    t.api("POST", &format!("repos/{}/hooks/{id}/pings", t.repo))
}

/// One hook in a hook list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    /// Its id.
    pub id: u64,
    /// Its name (`cli` for gh-webhook's).
    pub name: String,
    /// The events it asks for.
    pub events: Vec<String>,
    /// When it was made, seconds since the Unix epoch, if it reads.
    pub created: Option<i64>,
}

impl Hook {
    /// Whether it is a `cli` hook asking for exactly [`EVENTS`].
    fn is_forwarders(&self) -> bool {
        let want: HashSet<&str> = EVENTS.split(',').collect();
        let have: HashSet<&str> = self.events.iter().map(String::as_str).collect();
        self.name == HOOK_NAME && have == want
    }
}

/// Seconds since the Unix epoch of an RFC 3339 UTC time as GitHub writes it
/// (`2026-11-16T01:49:47Z`); `None` for any other form.
pub fn parse_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    // Days from 1970-01-01 to the date (H. Hinnant's days_from_civil).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

/// The hooks in a `GET repos/OWNER/NAME/hooks` answer: one array of hooks,
/// or (`--paginate --slurp`) an array of such pages.
///
/// # Errors
///
/// A message when it is not that, or a hook has no id.
pub fn parse_hooks(body: &[u8]) -> std::result::Result<Vec<Hook>, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the hook list is not UTF-8".to_string())?;
    let v = crate::json::parse(text).map_err(|e| format!("the hook list is not JSON: {e}"))?;
    let Value::Array(items) = v else { return Err("the hook list is not an array".into()) };
    let mut hooks = Vec::new();
    for item in &items {
        match item {
            Value::Array(page) => hooks.extend(page.iter()),
            h => hooks.push(h),
        }
    }
    hooks
        .into_iter()
        .map(|h| -> std::result::Result<Hook, String> {
            let id = h.get("id").and_then(Value::as_u64).ok_or("a hook without an id")?;
            let name = h.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let events = match h.get("events") {
                Some(Value::Array(e)) => e.iter().filter_map(Value::as_str).map(str::to_string).collect(),
                _ => Vec::new(),
            };
            let created = h.get("created_at").and_then(Value::as_str).and_then(parse_utc);
            Ok(Hook { id, name, events, created })
        })
        .collect()
}

/// Why a gh call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fail {
    /// gh is not on `PATH`.
    Missing,
    /// Anything else, in one line.
    Other(String),
}

fn fail(e: &io::Error) -> Fail {
    if e.kind() == io::ErrorKind::NotFound {
        Fail::Missing
    } else {
        Fail::Other(e.to_string())
    }
}

/// gh's complaint in one line (its first non-empty stderr line, cut).
fn complaint(o: &Output) -> String {
    let err = String::from_utf8_lossy(&o.stderr);
    let line = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no message");
    format!("{} ({})", line.chars().take(LINE_CHARS).collect::<String>(), o.status)
}

fn list_hooks(gh: &dyn Gh, t: &Target) -> std::result::Result<Vec<Hook>, Fail> {
    let o = gh.run(&list_args(t)).map_err(|e| fail(&e))?;
    if !o.status.success() {
        return Err(Fail::Other(complaint(&o)));
    }
    parse_hooks(&o.stdout).map_err(Fail::Other)
}

/// Whether hook `id` is gone: `GET` answers 404. Any other failure, or an
/// answer, means it may still be there.
fn gone(gh: &dyn Gh, t: &Target, id: u64) -> bool {
    match gh.run(&get_args(t, id)) {
        Ok(o) => !o.status.success() && String::from_utf8_lossy(&o.stderr).contains("(HTTP 404)"),
        Err(_) => false,
    }
}

fn ids(v: &[u64]) -> String {
    if v.is_empty() {
        "none".into()
    } else {
        v.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
    }
}

/// One forwarder generation: the threads one lease holding runs. Stop ends
/// them with cleanup; a lost lease ends them without (`abandon`), since the
/// new holder sweeps.
#[derive(Debug, Default)]
struct Gen {
    set: Mutex<bool>,
    cv: Condvar,
    /// The lease was lost: end the forwarders, delete nothing.
    abandon: AtomicBool,
    /// The stored ids did not load: keep this run's ids in memory only (L7).
    no_store: AtomicBool,
}

impl Gen {
    fn is_set(&self) -> bool {
        *self.set.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits up to `d`; true when stop was asked for.
    fn wait(&self, d: Duration) -> bool {
        let g = self.set.lock().unwrap_or_else(PoisonError::into_inner);
        let (g, _) = self.cv.wait_timeout_while(g, d, |s| !*s).unwrap_or_else(PoisonError::into_inner);
        *g
    }

    fn raise(&self) {
        *self.set.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cv.notify_all();
    }
}

/// Writes `recorded` as the repository's stored ids; a failure is logged.
/// Nothing is written while the stored ids did not load (L7).
fn persist(s: &Setup, g: &Gen, key: &str, recorded: &[u64], say: &Say<'_>) {
    if g.no_store.load(Ordering::SeqCst) {
        return;
    }
    if let Err(e) = s.ids.save(key, recorded) {
        say.out(What::Error, format!("storing the recorded hook ids ({}): {e}", ids(recorded)));
    }
}

/// Records `id`, a hook proved to be this start's, and stores the whole list.
fn record(s: &Setup, g: &Gen, key: &str, recorded: &mut Vec<u64>, id: u64, say: &Say<'_>) {
    recorded.push(id);
    persist(s, g, key, recorded, say);
    say.out(What::Recorded, format!("recorded the {HOOK_NAME} hook it made, proved by a signed delivery: {id}"));
}

/// Lists the repository's hooks, reports what it found, then deletes each
/// recorded id still there as gh-webhook's hook (named `cli`, asking for
/// [`EVENTS`]), each delete logged. A recorded id missing from the list is
/// dropped only once a `GET` of it answers 404 (L8). `seen` holds the
/// foreign `cli` hooks already reported to the journal.
fn cleanup(s: &Setup, g: &Gen, t: &Target, recorded: &mut Vec<u64>, seen: &mut HashSet<u64>, say: &Say<'_>) {
    let gh = &*s.gh;
    let key = t.key();
    if !s.ids.holds(s.me) {
        say.out(What::Error, format!("not the lease holder; {} left for the holder", ids(recorded)));
        return;
    }
    let hooks = match list_hooks(gh, t) {
        Ok(h) => h,
        Err(f) => {
            let why = match f {
                Fail::Missing => "gh is not installed".to_string(),
                Fail::Other(m) => m,
            };
            say.out(What::Error, format!("listing hooks before deleting {}: {why}; nothing deleted", ids(recorded)));
            return;
        }
    };
    let foreign: Vec<u64> = hooks.iter().filter(|h| h.name == HOOK_NAME && !recorded.contains(&h.id)).map(|h| h.id).collect();
    let mut mine = Vec::new();
    let mut changed_hook = Vec::new();
    let mut missing = Vec::new();
    for id in recorded.iter().copied() {
        match hooks.iter().find(|h| h.id == id) {
            Some(h) if h.is_forwarders() => mine.push(id),
            Some(_) => changed_hook.push(id),
            None => missing.push(id),
        }
    }
    let gone_ids: Vec<u64> = missing.iter().copied().filter(|id| gone(gh, t, *id)).collect();
    let new_foreign = foreign.iter().any(|id| !seen.contains(id));
    seen.extend(foreign.iter().copied());
    let line = format!(
        "{} hooks; recorded by atlas and present: {}; recorded but gone already: {}; recorded but missing from the list, kept: {}; recorded but no longer gh-webhook's, not deleted: {}; {HOOK_NAME} hooks atlas did not record, left alone: {}",
        hooks.len(),
        ids(&mine),
        ids(&gone_ids),
        ids(&missing.iter().copied().filter(|id| !gone_ids.contains(id)).collect::<Vec<_>>()),
        ids(&changed_hook),
        ids(&foreign)
    );
    if !mine.is_empty() || new_foreign {
        say.keep(What::Found, line);
    } else {
        say.out(What::Found, line);
    }
    if !gone_ids.is_empty() || !changed_hook.is_empty() {
        recorded.retain(|id| !gone_ids.contains(id) && !changed_hook.contains(id));
        persist(s, g, &key, recorded, say);
    }
    for id in mine {
        if !s.ids.holds(s.me) {
            say.out(What::Error, format!("the lease was lost; hook {id} not deleted"));
            return;
        }
        match gh.run(&delete_args(t, id)) {
            Ok(o) if o.status.success() => {
                say.out(What::Deleted, format!("deleted hook {id}, the {HOOK_NAME} hook atlas recorded"));
                recorded.retain(|x| *x != id);
                persist(s, g, &key, recorded, say);
            }
            Ok(o) => say.out(What::Error, format!("deleting hook {id}: {}", complaint(&o))),
            Err(e) => say.out(What::Error, format!("deleting hook {id}: {e}")),
        }
    }
}

/// Sweeps the ids a run that did not stop cleanly left for `key`, a
/// repository this run does not serve.
fn sweep(key: &str, s: &Setup, g: &Gen, mut left: Vec<u64>) {
    let say = Say { sink: &s.log, repo: key };
    let Some(t) = Target::parse(key) else {
        say.out(What::Error, format!("stored hook ids {} are not under an OWNER/NAME repository; left in the store", ids(&left)));
        return;
    };
    say.out(
        What::Found,
        format!("hook ids left by an earlier run that did not stop cleanly: {}; a repository this run does not serve", ids(&left)),
    );
    cleanup(s, g, &t, &mut left, &mut HashSet::new(), &say);
}

/// What the supervisor needs.
#[derive(Clone)]
pub struct Setup {
    /// How gh runs.
    pub gh: Arc<dyn Gh>,
    /// The listener's secrets, rotated at each start.
    pub secrets: Secrets,
    /// The hooks the listener has proof of.
    pub proofs: Proofs,
    /// The listener's port on 127.0.0.1.
    pub port: u16,
    /// The waits.
    pub timing: Timing,
    /// Where log lines go.
    pub log: Sink,
    /// Where the recorded hook ids and the lease are stored.
    pub ids: Arc<dyn Ids>,
    /// Who this supervisor is to the lease ([`Holder::me`]).
    pub me: Holder,
}

/// The running supervisor. Dropping it stops it, as [`Supervisor::stop`]
/// does.
pub struct Supervisor {
    stop: Arc<Gen>,
    current: Arc<Mutex<Option<Arc<Gen>>>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Supervisor")
    }
}

/// One generation's threads.
type Running = (Arc<Gen>, Vec<JoinHandle<()>>);

/// Starts the forwarders of one lease holding: one thread per repository in
/// `repos`, after the hook ids a run that did not stop cleanly left in the
/// store are swept (a served repository's before its first forwarder
/// starts, any other repository's in a thread of its own).
fn launch(repos: &[String], s: &Setup) -> Running {
    let g = Arc::new(Gen::default());
    let mut left = s.ids.load().unwrap_or_else(|e| {
        g.no_store.store(true, Ordering::SeqCst);
        let say = Say { sink: &s.log, repo: &repos.join(", ") };
        say.out(What::Error, format!("reading the stored hook ids: {e}; none swept, and this run keeps its ids in memory only"));
        Vec::new()
    });
    let mut threads: Vec<JoinHandle<()>> = repos
        .iter()
        .map(|r| {
            let key = Target::parse(r).map_or_else(|| r.clone(), |t| t.key());
            let earlier = left.iter().position(|(x, _)| *x == key).map(|i| left.remove(i).1).unwrap_or_default();
            let (repo, s, g) = (r.clone(), s.clone(), Arc::clone(&g));
            std::thread::spawn(move || run_repo(&repo, &s, &g, earlier))
        })
        .collect();
    for (key, ids) in left {
        let (s, g) = (s.clone(), Arc::clone(&g));
        threads.push(std::thread::spawn(move || sweep(&key, &s, &g, ids)));
    }
    (g, threads)
}

/// Ends a generation and waits for its threads.
fn finish(r: Running, abandon: bool) {
    let (g, threads) = r;
    if abandon {
        g.abandon.store(true, Ordering::SeqCst);
    }
    g.raise();
    for t in threads {
        let _ = t.join();
    }
}

/// Starts the supervisor over `repos` (`OWNER/NAME`, or `HOST/OWNER/NAME`
/// off github.com). When this process takes the forwarder lease, the
/// forwarders start before this returns; otherwise it tries again every
/// `lease_tick`. The holder beats the lease every `lease_tick`; when the
/// beat finds the lease taken, the forwarders end without cleanup.
pub fn start(repos: &[String], setup: &Setup) -> Supervisor {
    let stop = Arc::new(Gen::default());
    let current: Arc<Mutex<Option<Arc<Gen>>>> = Arc::default();
    let say_all = repos.join(", ");
    let take = |s: &Setup| -> Option<Running> {
        match s.ids.acquire(s.me, now_ms(), s.timing.lease_stale) {
            Ok(true) => Some(launch(repos, s)),
            Ok(false) => None,
            Err(e) => {
                Say { sink: &s.log, repo: &say_all }.out(What::Error, format!("reading the forwarder lease: {e}"));
                None
            }
        }
    };
    let mut running = take(setup);
    if running.is_none() {
        Say { sink: &setup.log, repo: &say_all }.out(
            What::Lease,
            format!("another atlas pr-events serve holds the forwarder lease ({LEASE_KEY}); serving without forwarders until it is free"),
        );
    }
    *current.lock().unwrap_or_else(PoisonError::into_inner) = running.as_ref().map(|r| Arc::clone(&r.0));
    let (s, st, cur, repos) = (setup.clone(), Arc::clone(&stop), Arc::clone(&current), repos.to_vec());
    let thread = std::thread::spawn(move || {
        let say = Say { sink: &s.log, repo: &say_all };
        while !st.wait(s.timing.lease_tick) {
            if running.is_some() {
                match s.ids.beat(s.me, now_ms()) {
                    Ok(true) => {}
                    Ok(false) => {
                        say.out(What::Lease, "the forwarder lease was taken over; ending the forwarders without cleanup".into());
                        *cur.lock().unwrap_or_else(PoisonError::into_inner) = None;
                        if let Some(r) = running.take() {
                            finish(r, true);
                        }
                    }
                    Err(e) => say.out(What::Error, format!("beating the forwarder lease: {e}")),
                }
            } else if let Ok(true) = s.ids.acquire(s.me, now_ms(), s.timing.lease_stale) {
                say.out(What::Lease, "took the forwarder lease; starting the forwarders".into());
                let r = launch(&repos, &s);
                let mut c = cur.lock().unwrap_or_else(PoisonError::into_inner);
                *c = Some(Arc::clone(&r.0));
                if st.is_set() {
                    r.0.raise();
                }
                drop(c);
                running = Some(r);
            }
        }
        if let Some(r) = running.take() {
            finish(r, false);
            if let Err(e) = s.ids.release(s.me) {
                say.out(What::Error, format!("releasing the forwarder lease: {e}"));
            }
        }
    });
    Supervisor { stop, current, thread: Some(thread) }
}

impl Supervisor {
    /// Stops every forwarder: each child is ended with its process tree, then
    /// its recorded hooks are cleaned up and the lease is released. Returns
    /// once all are done.
    pub fn stop(mut self) {
        self.halt();
    }

    /// Asks for the stop at once, from any thread, without waiting for it.
    #[cfg(test)]
    pub(crate) fn raiser(&self) -> impl Fn() + Send + Sync + 'static {
        let (stop, current) = (Arc::clone(&self.stop), Arc::clone(&self.current));
        move || {
            stop.raise();
            if let Some(g) = current.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                g.raise();
            }
        }
    }

    fn halt(&mut self) {
        self.stop.raise();
        if let Some(g) = self.current.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            g.raise();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.halt();
    }
}

/// How one forwarder run ended.
enum Run {
    /// Stop was asked for; the child is ended.
    Stopped,
    /// It must not start again.
    Fatal(String),
    /// It ended (or did not start) after running `up`; start it again.
    Again { up: Duration, why: String },
}

fn shown(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{} s", d.as_secs())
    }
}

/// How often a restart is journaled after the first: every this many.
const RESTART_NOTE_EVERY: u32 = 10;

/// And at least this often while restarts go on.
const RESTART_NOTE_AFTER: Duration = Duration::from_secs(3600);

/// One repository's forwarder, started and restarted until stop. `earlier`
/// holds the ids a run that did not stop cleanly left in the store.
fn run_repo(spec: &str, s: &Setup, g: &Gen, earlier: Vec<u64>) {
    let say = Say { sink: &s.log, repo: spec };
    let Some(t) = Target::parse(spec) else {
        say.out(What::Fatal, "not an OWNER/NAME repository; no forwarder".into());
        return;
    };
    let mut backoff = Backoff::new(&s.timing);
    if !earlier.is_empty() {
        say.keep(What::Found, format!("hook ids left by an earlier run that did not stop cleanly: {}", ids(&earlier)));
    }
    let mut recorded = earlier;
    let mut seen = HashSet::new();
    let (mut restarts, mut since_note) = (0u32, 0u32);
    let mut noted = Instant::now();
    while !g.is_set() {
        // The previous forwarder's hook (or one an earlier run left) goes
        // before the next one makes its own.
        if !recorded.is_empty() {
            cleanup(s, g, &t, &mut recorded, &mut seen, &say);
        }
        match run_once(&t, s, g, &mut recorded, &say) {
            Run::Stopped => break,
            Run::Fatal(why) => {
                say.out(What::Fatal, format!("{why}; not restarted"));
                break;
            }
            Run::Again { up, why } => {
                let delay = backoff.after(up);
                restarts += 1;
                since_note += 1;
                let line = format!("restart {restarts} in {}: {why}", shown(delay));
                if restarts == 1 || restarts % RESTART_NOTE_EVERY == 0 || noted.elapsed() >= RESTART_NOTE_AFTER {
                    say.keep(
                        What::Restart,
                        if restarts == 1 { line } else { format!("{line} ({since_note} restarts since the last note)") },
                    );
                    since_note = 0;
                    noted = Instant::now();
                } else {
                    say.out(What::Restart, line);
                }
                if g.wait(delay) {
                    break;
                }
            }
        }
    }
    if !recorded.is_empty() && !g.abandon.load(Ordering::SeqCst) {
        cleanup(s, g, &t, &mut recorded, &mut seen, &say);
    }
    say.out(What::Stopped, "stopped".into());
}

/// Ends what is left of a child's tree after it exited by itself (L5): the
/// process group on Unix (still killable while a member lives), the job
/// object and the tree on Windows (the child is not reaped there until its
/// handle is dropped, so its pid is not reused).
fn end_after_exit(child: &Child, tie: crate::platform::process::Tie) {
    tie.end();
    #[cfg(unix)]
    crate::platform::process::terminate_group(child.id());
    #[cfg(not(unix))]
    crate::platform::process::terminate_tree(child.id());
}

/// Starts one forwarder and runs it until it ends or stop is asked for.
fn run_once(t: &Target, s: &Setup, g: &Gen, recorded: &mut Vec<u64>, say: &Say<'_>) -> Run {
    let missing = || Run::Fatal("gh is not installed (not found on PATH)".into());
    let before: HashSet<u64> = match list_hooks(&*s.gh, t) {
        Ok(h) => h.iter().map(|h| h.id).collect(),
        Err(Fail::Missing) => return missing(),
        Err(Fail::Other(m)) => return Run::Again { up: Duration::ZERO, why: format!("listing hooks before the start: {m}") },
    };
    // L4: a stop raised during the cleanup or the list starts nothing.
    if g.is_set() || !s.ids.holds(s.me) {
        return Run::Stopped;
    }
    s.proofs.clear(&t.repo);
    let secret = match s.secrets.rotate(&t.repo) {
        Ok(x) => x,
        Err(e) => return Run::Fatal(format!("the random source: {e}")),
    };
    if g.is_set() {
        return Run::Stopped;
    }
    let spawned = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let mut child = match s.gh.spawn(&forward_args(t, s.port, &secret)) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return missing(),
        Err(e) => return Run::Again { up: Duration::ZERO, why: format!("starting gh: {e}") },
    };
    let tie = crate::platform::process::tie(&child);
    let started = Instant::now();
    say.out(What::Started, format!("started gh webhook forward, pid {}, with a new secret", child.id()));
    let fatal: Arc<Mutex<Option<String>>> = Arc::default();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let mut readers = 0;
    let streams: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|o| Box::new(o) as Box<dyn Read + Send>),
        child.stderr.take().map(|o| Box::new(o) as Box<dyn Read + Send>),
    ];
    for r in streams.into_iter().flatten() {
        readers += 1;
        let (repo, secret, fatal, done) = (t.repo.clone(), secret.clone(), Arc::clone(&fatal), done_tx.clone());
        std::thread::spawn(move || watch_output(r, &repo, &secret, &fatal, &done));
    }
    drop(done_tx);
    drop(secret);
    // H1: a candidate is a new `cli` hook asking for EVENTS, made around
    // this start; it is pinged, and recorded only on proof.
    let waited: Duration = s.timing.hook_waits.iter().sum();
    let skew = i64::try_from(SKEW.as_secs()).unwrap_or(0);
    let window = (spawned - skew, spawned + i64::try_from(waited.as_secs()).unwrap_or(0) + skew);
    let mut candidates: Vec<u64> = Vec::new();
    let mut proved = false;
    let look = |candidates: &mut Vec<u64>| match list_hooks(&*s.gh, t) {
        Ok(hooks) => {
            for h in hooks {
                let made_now = h.created.is_some_and(|c| c >= window.0 && c <= window.1);
                if !before.contains(&h.id) && h.is_forwarders() && made_now && !candidates.contains(&h.id) {
                    candidates.push(h.id);
                    if !s.ids.holds(s.me) {
                        continue;
                    }
                    match s.gh.run(&ping_args(t, h.id)) {
                        Ok(o) if o.status.success() => {}
                        Ok(o) => say.out(What::Error, format!("pinging hook {}: {}", h.id, complaint(&o))),
                        Err(e) => say.out(What::Error, format!("pinging hook {}: {e}", h.id)),
                    }
                }
            }
        }
        Err(Fail::Missing) => say.out(What::Error, "listing hooks after the start: gh is not installed".into()),
        Err(Fail::Other(m)) => say.out(What::Error, format!("listing hooks after the start: {m}")),
    };
    let key = t.key();
    let check = |candidates: &[u64], recorded: &mut Vec<u64>, proved: &mut bool| {
        for id in candidates {
            if !recorded.contains(id) && s.proofs.has(&t.repo, *id) {
                record(s, g, &key, recorded, *id, say);
                *proved = true;
            }
        }
    };
    for d in &s.timing.hook_waits {
        if g.wait(*d) || !matches!(child.try_wait(), Ok(None)) {
            break;
        }
        look(&mut candidates);
        check(&candidates, recorded, &mut proved);
        if proved {
            break;
        }
    }
    let mut tie = Some(tie);
    let status = loop {
        check(&candidates, recorded, &mut proved);
        match child.try_wait() {
            Ok(Some(st)) => {
                if let Some(tie) = tie.take() {
                    end_after_exit(&child, tie);
                }
                break Some(st.to_string());
            }
            Ok(None) => {}
            Err(e) => {
                end(&mut child);
                break Some(format!("lost track of it: {e}"));
            }
        }
        if g.wait(s.timing.tick) {
            end(&mut child);
            break None;
        }
    };
    if let Some(tie) = tie {
        tie.end();
    }
    // The output ends once the whole tree is gone; wait a little for the
    // last lines, which say why it ended.
    for _ in 0..readers {
        if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            break;
        }
    }
    check(&candidates, recorded, &mut proved);
    if !proved {
        say.out(What::Error, format!("no proven {HOOK_NAME} hook for this start; left in place"));
    }
    let Some(status) = status else { return Run::Stopped };
    if let Some(why) = fatal.lock().unwrap_or_else(PoisonError::into_inner).take() {
        return Run::Fatal(why);
    }
    Run::Again { up: started.elapsed(), why: format!("gh webhook forward ended ({status}) after {}", shown(started.elapsed())) }
}

/// Shows a forwarder's output on stderr, one line at a time, cut, with the
/// secret masked should gh ever print it; notes the lines that mean it will
/// never work (gh-webhook missing, no access). A failed write to stderr
/// does not stop it reading, so the pipe never fills.
fn watch_output(r: Box<dyn Read + Send>, repo: &str, secret: &str, fatal: &Mutex<Option<String>>, done: &mpsc::Sender<()>) {
    for line in BufReader::new(r).lines() {
        let Ok(line) = line else { break };
        let line = line.replace(secret, "[secret]");
        let line: String = line.trim().chars().take(LINE_CHARS).collect();
        if line.is_empty() {
            continue;
        }
        let why = if line.contains(NO_ACCESS) {
            Some("GitHub refused the hook: \"you do not have access to this feature\" (403)")
        } else if line.contains(NO_EXTENSION) {
            Some("the gh-webhook extension is not installed (gh extension install cli/gh-webhook)")
        } else {
            None
        };
        if let Some(why) = why {
            let mut f = fatal.lock().unwrap_or_else(PoisonError::into_inner);
            if f.is_none() {
                *f = Some(why.to_string());
            }
        }
        say(format_args!("atlas pr-events: gh {repo}: {line}"));
    }
    let _ = done.send(());
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;

    const REPO: &str = "octo-org/widgets";

    /// Not a test: the fake forwarder the supervisor tests start, a copy of
    /// this test binary. `run` waits, `exit` ends at once, `noaccess` says
    /// what gh-webhook says on a 403 and ends, `tree` starts a `run` child
    /// of its own (as gh starts the extension) and waits, `orphan` starts a
    /// `run` child, writes its pid and exits at once, `parent` starts a
    /// tied `run` child as the supervisor does, writes its pid and waits.
    #[test]
    #[ignore = "not a test: the fake forwarder the supervisor tests start"]
    fn fake_forwarder() {
        let Ok(mode) = std::env::var("ATLAS_FAKE_FORWARDER") else { return };
        let dir = || std::path::PathBuf::from(std::env::var("ATLAS_FAKE_FORWARDER_DIR").unwrap());
        let write_pid = |name: &str, pid: u32| {
            std::fs::write(dir().join(format!("{name}.tmp")), pid.to_string()).unwrap();
            std::fs::rename(dir().join(format!("{name}.tmp")), dir().join(format!("{name}.pid"))).unwrap();
        };
        match mode.as_str() {
            "exit" => std::process::exit(3),
            "noaccess" => {
                eprintln!("error creating webhook: you do not have access to this feature");
                std::process::exit(1)
            }
            "tree" => {
                let mut c = helper("run", &dir()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
                write_pid("grandchild", c.id());
                std::thread::sleep(Duration::from_secs(60));
                let _ = c.wait();
            }
            "orphan" => {
                let c = helper("run", &dir()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
                write_pid("grandchild", c.id());
                std::process::exit(0)
            }
            "parent" => {
                let mut cmd = helper("run", &dir());
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
                crate::platform::process::own_group(&mut cmd);
                let mut c = cmd.spawn().unwrap();
                let tie = crate::platform::process::tie(&c);
                write_pid("tied", c.id());
                std::thread::sleep(Duration::from_secs(60));
                tie.end();
                let _ = c.kill();
                let _ = c.wait();
            }
            _ => std::thread::sleep(Duration::from_secs(60)),
        }
    }

    fn helper(mode: &str, dir: &std::path::Path) -> Command {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["--ignored", "--exact", "pr_supervise::tests::fake_forwarder", "--nocapture", "--quiet"])
            .env("ATLAS_FAKE_FORWARDER", mode)
            .env("ATLAS_FAKE_FORWARDER_DIR", dir)
            .stdin(Stdio::null());
        c
    }

    #[cfg(unix)]
    fn status(code: i32) -> std::process::ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn status(code: i32) -> std::process::ExitStatus {
        std::os::windows::process::ExitStatusExt::from_raw(code as u32)
    }

    /// Seconds since the epoch, now.
    fn now_s() -> i64 {
        i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap()
    }

    /// The RFC 3339 UTC form of `secs` (H. Hinnant's civil_from_days).
    fn utc(secs: i64) -> String {
        let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
    }

    /// One hook on the fake GitHub.
    #[derive(Debug, Clone)]
    struct FakeHook {
        id: u64,
        name: String,
        events: Vec<String>,
        created: i64,
        /// The forwarder that made it: the port it forwards to and the
        /// secret it signs with.
        owner: Option<(u16, String)>,
    }

    fn hook(id: u64, name: &str, events: &[&str], created: i64) -> FakeHook {
        FakeHook { id, name: name.to_string(), events: events.iter().map(|e| (*e).to_string()).collect(), created, owner: None }
    }

    fn all_events() -> Vec<&'static str> {
        EVENTS.split(',').collect()
    }

    /// A fake gh: `api` calls answer from an in-memory hook list (the
    /// repository on GitHub), and `webhook forward` makes a `cli` hook there,
    /// as gh-webhook does, then starts a real child process: a fake
    /// forwarder from [`fake_forwarder`]. A ping of a hook a forwarder made
    /// is delivered to the listener on that forwarder's port, signed with
    /// its secret, through the listener's own check
    /// ([`crate::pr_listen::proof`]); a ping of any other hook reaches no one.
    pub(crate) struct FakeGh {
        dir: std::path::PathBuf,
        /// Each repository's hooks; a repository not here has the two
        /// [`FakeGh::new`] starts with.
        hooks: Mutex<std::collections::BTreeMap<String, Vec<FakeHook>>>,
        /// The listeners, by port: their secrets and where proofs go.
        listeners: Mutex<HashMap<u16, (Secrets, Proofs)>>,
        /// Hook lists fail (GitHub unreachable).
        fail_list: AtomicBool,
        /// How many more hook lists succeed before they fail; -1: all.
        lists_left: std::sync::atomic::AtomicI64,
        /// Deletes of these ids fail (and leave the hook).
        fail_delete: Mutex<HashSet<u64>>,
        /// A `GET` of these ids answers 500.
        fail_get: Mutex<HashSet<u64>>,
        /// Hooks per page of a list.
        page: Mutex<usize>,
        /// Called on each delete, before it is done.
        on_delete: Mutex<Option<Box<dyn Fn() + Send>>>,
        /// Foreign `cli` hooks the next `webhook forward` finds made beside
        /// its own (another forwarder started at the same moment).
        rivals: Mutex<Vec<u64>>,
        next_id: AtomicU64,
        /// Each call's arguments, the secret replaced by its index in
        /// `secrets`.
        calls: Mutex<Vec<Vec<String>>>,
        /// Each `--secret` handed to a forwarder (kept in memory only).
        secrets: Mutex<Vec<String>>,
        /// Each forwarder's pid.
        pub(crate) pids: Mutex<Vec<u32>>,
        mode: &'static str,
        missing: bool,
    }

    impl FakeGh {
        pub(crate) fn new(tag: &str, mode: &'static str) -> Arc<FakeGh> {
            let dir = std::env::temp_dir().join(format!("atlas-pr-supervise-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Arc::new(FakeGh {
                dir,
                hooks: Mutex::default(),
                listeners: Mutex::default(),
                fail_list: false.into(),
                lists_left: (-1).into(),
                fail_delete: Mutex::default(),
                fail_get: Mutex::default(),
                page: Mutex::new(100),
                on_delete: Mutex::default(),
                rivals: Mutex::default(),
                next_id: AtomicU64::new(100),
                calls: Mutex::default(),
                secrets: Mutex::default(),
                pids: Mutex::default(),
                mode,
                missing: false,
            })
        }

        /// Delivers what a forwarder on `port` receives to that listener.
        pub(crate) fn listen(&self, port: u16, secrets: &Secrets, proofs: &Proofs) {
            self.listeners.lock().unwrap().insert(port, (secrets.clone(), proofs.clone()));
        }

        fn hook_ids(&self) -> Vec<u64> {
            self.hook_ids_of(REPO)
        }

        fn hook_ids_of(&self, repo: &str) -> Vec<u64> {
            self.with_hooks(repo, |h| h.iter().map(|h| h.id).collect())
        }

        /// Runs `f` on `repo`'s hooks, which start as a web hook and a `cli`
        /// hook some other forwarder made long ago, which atlas must leave
        /// alone.
        fn with_hooks<T>(&self, repo: &str, f: impl FnOnce(&mut Vec<FakeHook>) -> T) -> T {
            let mut all = self.hooks.lock().unwrap();
            f(all
                .entry(repo.to_string())
                .or_insert_with(|| vec![hook(1, "web", &["push"], now_s() - 86_400), hook(2, HOOK_NAME, &all_events(), now_s() - 86_400)]))
        }

        fn deletes(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().filter(|c| c.get(2).is_some_and(|m| m == "DELETE")).map(|c| c[3].clone()).collect()
        }

        fn spawns(&self) -> usize {
            self.calls.lock().unwrap().iter().filter(|c| c[0] == "webhook").count()
        }
    }

    impl Drop for FakeGh {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Gh for FakeGh {
        fn run(&self, args: &[String]) -> io::Result<Output> {
            if self.missing {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no gh"));
            }
            self.calls.lock().unwrap().push(args.to_vec());
            let answer = |code: i32, out: String, err: &str| Output {
                status: status(code),
                stdout: out.into_bytes(),
                stderr: err.as_bytes().to_vec(),
            };
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            // `repos/OWNER/NAME/hooks...` as (OWNER/NAME, the rest).
            let hooks_of =
                |p: &str| p.strip_prefix("repos/").and_then(|r| r.split_once("/hooks")).map(|(r, rest)| (r.to_string(), rest.to_string()));
            let id_of = |rest: &str| rest.strip_prefix('/').and_then(|n| n.split('/').next()).and_then(|n| n.parse::<u64>().ok());
            Ok(match a.as_slice() {
                ["api", "--method", "GET", "--paginate", "--slurp", p] if hooks_of(p).is_some_and(|(_, rest)| rest == "?per_page=100") => {
                    let left = self.lists_left.load(Ordering::SeqCst);
                    if self.fail_list.load(Ordering::SeqCst) || left == 0 {
                        return Ok(answer(1, String::new(), "error connecting to api.github.com"));
                    }
                    if left > 0 {
                        self.lists_left.fetch_sub(1, Ordering::SeqCst);
                    }
                    let (repo, _) = hooks_of(p).unwrap();
                    let per = *self.page.lock().unwrap();
                    let pages: Vec<String> = self.with_hooks(&repo, |h| {
                        h.chunks(per.max(1))
                            .map(|page| {
                                let items: Vec<String> = page
                                    .iter()
                                    .map(|h| {
                                        let events: Vec<String> = h.events.iter().map(|e| format!("\"{e}\"")).collect();
                                        format!(
                                            r#"{{"type":"Repository","id":{},"name":"{}","active":true,"events":[{}],"config":{{"content_type":"json","insecure_ssl":"0","secret":"********"}},"created_at":"{}"}}"#,
                                            h.id,
                                            h.name,
                                            events.join(","),
                                            utc(h.created)
                                        )
                                    })
                                    .collect();
                                format!("[{}]", items.join(","))
                            })
                            .collect()
                    });
                    answer(0, format!("[{}]", pages.join(",")), "")
                }
                ["api", "--method", "GET", p] if hooks_of(p).is_some_and(|(_, rest)| id_of(&rest).is_some()) => {
                    let (repo, rest) = hooks_of(p).unwrap();
                    let id = id_of(&rest).unwrap();
                    if self.fail_get.lock().unwrap().contains(&id) {
                        return Ok(answer(1, String::new(), "gh: Server Error (HTTP 500)"));
                    }
                    if self.with_hooks(&repo, |h| h.iter().any(|x| x.id == id)) {
                        answer(0, format!("{{\"id\":{id}}}"), "")
                    } else {
                        answer(1, String::new(), "gh: Not Found (HTTP 404)")
                    }
                }
                ["api", "--method", "POST", p] if hooks_of(p).is_some_and(|(_, rest)| rest.ends_with("/pings")) => {
                    let (repo, rest) = hooks_of(p).unwrap();
                    let id = id_of(&rest).unwrap();
                    let Some(owner) = self.with_hooks(&repo, |h| h.iter().find(|x| x.id == id).map(|x| x.owner.clone())) else {
                        return Ok(answer(1, String::new(), "gh: Not Found (HTTP 404)"));
                    };
                    // The hook's own forwarder relays GitHub's signed ping;
                    // an `exit` forwarder ends before it dials, so it relays
                    // nothing.
                    if let Some((port, secret)) = owner.filter(|_| self.mode != "exit") {
                        if let Some((secrets, proofs)) = self.listeners.lock().unwrap().get(&port).cloned() {
                            let body = format!("{{\"zen\":\"Keep it logically awesome.\",\"hook_id\":{id}}}");
                            let sig = sign(&secret, body.as_bytes());
                            if let Some((r, i)) =
                                crate::pr_listen::proof(&secrets, "ping", body.as_bytes(), Some(&sig), Some(&id.to_string()))
                            {
                                proofs.add(&r, i);
                            }
                        }
                    }
                    answer(0, String::new(), "")
                }
                ["api", "--method", "DELETE", p] if hooks_of(p).is_some() => {
                    if let Some(f) = self.on_delete.lock().unwrap().as_ref() {
                        f();
                    }
                    let (repo, rest) = hooks_of(p).unwrap();
                    let id = id_of(&rest);
                    if id.is_some_and(|id| self.fail_delete.lock().unwrap().contains(&id)) {
                        return Ok(answer(1, String::new(), "gh: Server Error (HTTP 502)"));
                    }
                    self.with_hooks(&repo, |h| match id.and_then(|id| h.iter().position(|x| x.id == id)) {
                        Some(i) => {
                            h.remove(i);
                            answer(0, String::new(), "")
                        }
                        None => answer(1, String::new(), "gh: Not Found (HTTP 404)"),
                    })
                }
                _ => answer(2, String::new(), "fake gh: unknown call"),
            })
        }

        fn spawn(&self, args: &[String]) -> io::Result<Child> {
            if self.missing {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no gh"));
            }
            let mut shown = args.to_vec();
            let i = shown.iter().position(|a| a == "--secret").unwrap() + 1;
            let mut secrets = self.secrets.lock().unwrap();
            let n = secrets.len();
            let secret = std::mem::replace(&mut shown[i], format!("secret#{n}"));
            secrets.push(secret.clone());
            drop(secrets);
            let repo = shown[shown.iter().position(|a| a == "--repo").unwrap() + 1].clone();
            let url = &shown[shown.iter().position(|a| a == "--url").unwrap() + 1];
            let port: u16 = url.trim_start_matches("http://127.0.0.1:").split('/').next().unwrap().parse().unwrap();
            self.calls.lock().unwrap().push(shown);
            // Another forwarder's hooks, made in the same moment.
            for id in std::mem::take(&mut *self.rivals.lock().unwrap()) {
                self.with_hooks(&repo, |hooks| hooks.push(hook(id, HOOK_NAME, &all_events(), now_s())));
            }
            // gh-webhook makes its hook on GitHub.
            let mut h = hook(self.next_id.fetch_add(1, Ordering::SeqCst), HOOK_NAME, &all_events(), now_s());
            h.owner = Some((port, secret));
            self.with_hooks(&repo, |hooks| hooks.push(h));
            let mut c = helper(self.mode, &self.dir);
            c.stdout(Stdio::piped()).stderr(Stdio::piped());
            crate::platform::process::own_group(&mut c);
            let child = c.spawn()?;
            self.pids.lock().unwrap().push(child.id());
            Ok(child)
        }
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        format!("sha256={}", crate::sha256::hmac(secret.as_bytes(), body).iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    pub(crate) fn fast() -> Timing {
        Timing {
            first: Duration::from_millis(10),
            max: Duration::from_millis(40),
            reset_after: Duration::from_secs(3600),
            hook_waits: vec![Duration::from_millis(10); 3],
            tick: Duration::from_millis(20),
            lease_tick: Duration::from_millis(50),
            lease_stale: Duration::from_secs(60),
        }
    }

    const REPO2: &str = "octo-org/gadgets";

    /// A memory store in a fresh folder of its own; dropped with the folder.
    struct Store {
        dir: std::path::PathBuf,
        store: Arc<Mutex<MemoryStore>>,
    }

    impl Store {
        fn new(tag: &str) -> Store {
            let dir = std::env::temp_dir().join(format!("atlas-pr-supervise-store-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store = Arc::new(Mutex::new(MemoryStore::open(&dir.join("memory.sqlite")).unwrap()));
            Store { dir, store }
        }

        /// A second connection to the same memory.sqlite, as another atlas
        /// process would open it.
        fn other(&self) -> Arc<Mutex<MemoryStore>> {
            Arc::new(Mutex::new(MemoryStore::open(&self.dir.join("memory.sqlite")).unwrap()))
        }

        /// Closes memory.sqlite and opens it again, as a new atlas process
        /// would after the old one died.
        fn reopen(&mut self) {
            assert_eq!(Arc::strong_count(&self.store), 1, "the store is still held");
            self.store = Arc::new(Mutex::new(MemoryStore::open(&self.dir.join("memory.sqlite")).unwrap()));
        }

        fn ids(&self) -> Arc<dyn Ids> {
            Arc::clone(&self.store) as Arc<dyn Ids>
        }

        fn stored(&self) -> Stored {
            stored_hooks(&self.store.lock().unwrap()).unwrap()
        }
    }

    impl Drop for Store {
        fn drop(&mut self) {
            // Best effort: a store still held elsewhere keeps its folder.
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The setup the tests start supervisors with on `port`, log lines
    /// arriving on the returned channel.
    fn setup_on(gh: &Arc<FakeGh>, secrets: &Secrets, ids: Arc<dyn Ids>, port: u16) -> (Setup, mpsc::Receiver<Log>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let proofs = Proofs::new();
        gh.listen(port, secrets, &proofs);
        let setup = Setup {
            gh: Arc::clone(gh) as Arc<dyn Gh>,
            secrets: secrets.clone(),
            proofs,
            port,
            timing: fast(),
            log: Arc::new(move |l: Log| {
                let _ = tx.lock().unwrap().send(l);
            }),
            ids,
            me: Holder::me(),
        };
        (setup, rx)
    }

    fn setup(gh: &Arc<FakeGh>, secrets: &Secrets, ids: Arc<dyn Ids>) -> (Setup, mpsc::Receiver<Log>) {
        setup_on(gh, secrets, ids, 8765)
    }

    /// A supervisor over one repository, its log lines arriving on the
    /// returned channel, its ids in a store of its own.
    fn supervise(gh: &Arc<FakeGh>, secrets: &Secrets) -> (Supervisor, mpsc::Receiver<Log>, Store) {
        let store = Store::new(&gh.dir.file_name().unwrap().to_string_lossy());
        let (s, rx) = setup(gh, secrets, store.ids());
        (start(&[REPO.to_string()], &s), rx, store)
    }

    /// Log lines until one satisfies `until` (which is returned last), or
    /// panics after 60 s.
    fn read_until(rx: &mpsc::Receiver<Log>, seen: &mut Vec<Log>, until: impl Fn(&Log, &[Log]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let l = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("timed out; log so far: {seen:#?}"));
            let hit = until(&l, seen);
            seen.push(l);
            if hit {
                return;
            }
        }
    }

    pub(crate) fn gone_within(pid: u32, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        while crate::platform::process::running(pid) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    fn wait_file(path: &std::path::Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !path.exists() {
            assert!(Instant::now() < deadline, "{} never came", path.display());
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read_to_string(path).unwrap().parse().unwrap()
    }

    fn target() -> Target {
        Target::parse(REPO).unwrap()
    }

    #[test]
    fn backoff_doubles_to_its_cap_and_starts_over_after_a_long_run() {
        let t = Timing {
            first: Duration::from_secs(1),
            max: Duration::from_secs(5),
            reset_after: Duration::from_secs(600),
            ..Timing::default()
        };
        let mut b = Backoff::new(&t);
        let short = Duration::from_secs(2);
        let waits: Vec<u64> = (0..5).map(|_| b.after(short).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 5, 5]);
        // A child that ran ten minutes starts the waits over.
        assert_eq!(b.after(Duration::from_secs(600)).as_secs(), 1);
        assert_eq!(b.after(short).as_secs(), 2);
        let d = Timing::default();
        assert_eq!((d.first, d.max, d.reset_after), (Duration::from_secs(1), Duration::from_secs(300), Duration::from_secs(600)));
        assert_eq!((d.lease_tick, d.lease_stale), (Duration::from_secs(15), Duration::from_secs(60)));
    }

    #[test]
    fn the_command_lines_are_argument_vectors() {
        let t = target();
        assert_eq!(
            forward_args(&t, 8765, "s3cr3t"),
            [
                "webhook",
                "forward",
                "--repo",
                REPO,
                "--events",
                "pull_request_review,pull_request_review_comment,issue_comment,pull_request",
                "--url",
                "http://127.0.0.1:8765/github",
                "--secret",
                "s3cr3t"
            ]
        );
        assert_eq!(list_args(&t), ["api", "--method", "GET", "--paginate", "--slurp", "repos/octo-org/widgets/hooks?per_page=100"]);
        assert_eq!(delete_args(&t, 42), ["api", "--method", "DELETE", "repos/octo-org/widgets/hooks/42"]);
        assert_eq!(get_args(&t, 42), ["api", "--method", "GET", "repos/octo-org/widgets/hooks/42"]);
        assert_eq!(ping_args(&t, 42), ["api", "--method", "POST", "repos/octo-org/widgets/hooks/42/pings"]);
        for ok in ["octo-org/widgets", "o/w.rs", "O_1/n-2"] {
            assert!(valid_repo(ok), "{ok}");
        }
        for bad in ["widgets", "o/", "/w", "o/w/x", "../w", "o/..", "o w/x", "o/w?x=1", "o/w#1", ""] {
            assert!(!valid_repo(bad), "{bad}");
        }
        let hooks = parse_hooks(
            br#"[{"id":7,"name":"cli","events":["a","b"],"created_at":"2000-03-01T00:00:00Z"},{"id":8,"name":"web","config":{}}]"#,
        )
        .unwrap();
        assert_eq!((hooks[0].id, hooks[0].name.as_str(), hooks[0].events.len(), hooks[0].created), (7, "cli", 2, Some(951_868_800)));
        assert_eq!((hooks[1].id, hooks[1].created), (8, None));
        // Pages, as `--paginate --slurp` gives them.
        let pages = parse_hooks(br#"[[{"id":7,"name":"cli"}],[{"id":9,"name":"cli"}]]"#).unwrap();
        assert_eq!(pages.iter().map(|h| h.id).collect::<Vec<_>>(), [7, 9]);
        assert!(parse_hooks(b"{}").is_err() && parse_hooks(b"[{\"name\":\"cli\"}]").is_err() && parse_hooks(b"nope").is_err());
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        for s in [0, 951_868_800, 1_794_793_787, 4_102_444_799] {
            assert_eq!(parse_utc(&utc(s)), Some(s), "{}", utc(s));
        }
        for bad in ["2026-11-16T01:49:47", "2026-11-16 01:49:47Z", "2026-13-16T01:49:47Z", "x"] {
            assert_eq!(parse_utc(bad), None, "{bad}");
        }
    }

    /// L9: a remote on another host carries it on every gh call; github.com
    /// stays as it was, and so do its stored lines.
    #[test]
    fn a_host_other_than_github_com_reaches_every_gh_call() {
        let ghe = Target::parse("ghe.corp.example/Team/App").unwrap();
        assert_eq!(
            (ghe.host.as_deref(), ghe.repo.as_str(), ghe.key().as_str()),
            (Some("ghe.corp.example"), "team/app", "ghe.corp.example/team/app")
        );
        let fwd = forward_args(&ghe, 8765, "s");
        assert_eq!(&fwd[fwd.len() - 2..], ["--github-host", "ghe.corp.example"]);
        for args in [list_args(&ghe), get_args(&ghe, 1), delete_args(&ghe, 1), ping_args(&ghe, 1)] {
            assert_eq!(&args[..3], ["api", "--hostname", "ghe.corp.example"], "{args:?}");
            assert!(args.last().unwrap().starts_with("repos/team/app/hooks"), "{args:?}");
        }
        let dot_com = Target::parse("github.com/octo-org/widgets").unwrap();
        assert_eq!(dot_com, target());
        assert_eq!(dot_com.key(), REPO);
        assert!(!forward_args(&dot_com, 1, "s").iter().any(|a| a.contains("host")));
        assert!(Target::parse("-x/o/n").is_none() && Target::parse("a b/o/n").is_none() && Target::parse("o/n/x/y").is_none());
        assert_eq!(
            parse_stored("ghe.corp.example/team/app 5\nocto-org/widgets 7"),
            [("ghe.corp.example/team/app".to_string(), vec![5]), (REPO.to_string(), vec![7])]
        );
    }

    #[test]
    fn stop_ends_the_forwarder_and_its_child_and_deletes_only_the_recorded_hook() {
        let gh = FakeGh::new("stop", "tree");
        let secrets = Secrets::new();
        let (sup, rx, _store) = supervise(&gh, &secrets);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Recorded);
        // The hook list was read before the forwarder started.
        let calls = gh.calls.lock().unwrap().clone();
        assert_eq!(calls[0], list_args(&target()));
        assert_eq!(&calls[1][..2], ["webhook", "forward"]);
        let recorded = log.last().unwrap().line.clone();
        assert!(recorded.ends_with(": 100"), "{recorded}");
        // The forwarder's own child (gh's extension) is up.
        let grandchild = wait_file(&gh.dir.join("grandchild.pid"));
        let child = gh.pids.lock().unwrap()[0];
        assert!(crate::platform::process::running(grandchild));
        sup.stop();
        log.extend(rx.try_iter());
        assert!(gone_within(child, Duration::from_secs(10)), "the forwarder still runs");
        assert!(gone_within(grandchild, Duration::from_secs(10)), "the forwarder's child still runs");
        // Only the recorded hook went; the web hook and the other `cli` hook stay.
        assert_eq!(gh.deletes(), ["repos/octo-org/widgets/hooks/100"]);
        assert_eq!(gh.hook_ids(), [1, 2]);
        let found = log.iter().find(|l| l.what == What::Found).unwrap();
        assert!(found.line.contains("recorded by atlas and present: 100") && found.line.ends_with("left alone: 2"), "{}", found.line);
        assert!(log.iter().any(|l| l.what == What::Deleted && l.line.contains("hook 100")), "{log:#?}");
        assert_eq!(log.last().unwrap().what, What::Stopped);
        assert!(!log.iter().any(|l| l.what == What::Restart), "{log:#?}");
    }

    #[test]
    fn a_dying_forwarder_restarts_with_backoff_and_a_new_secret_and_its_unproved_hook_stays() {
        let gh = FakeGh::new("restart", "exit");
        let secrets = Secrets::new();
        let (sup, rx, _store) = supervise(&gh, &secrets);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, seen| l.what == What::Restart && seen.iter().filter(|x| x.what == What::Restart).count() == 4);
        sup.stop();
        log.extend(rx.try_iter());
        let restarts: Vec<&str> = log.iter().filter(|l| l.what == What::Restart).map(|l| l.line.as_str()).collect();
        for (r, wait) in restarts.iter().zip(["in 10 ms", "in 20 ms", "in 40 ms", "in 40 ms", "in 40 ms"]) {
            assert!(r.contains(wait) && r.contains("ended"), "{r}");
        }
        assert!(restarts.len() >= 5, "{restarts:?}");
        // Each start had a fresh secret, and the listener holds the last one.
        let given = gh.secrets.lock().unwrap().clone();
        assert!(given.len() >= 5 && given.iter().collect::<HashSet<_>>().len() == given.len());
        assert!(given.iter().all(|s| s.len() == 64));
        let body = b"{}";
        assert!(crate::pr_listen::signature_ok(given.last().unwrap().as_bytes(), body, Some(&sign(given.last().unwrap(), body))));
        assert_eq!(secrets.signer(body, Some(&sign(given.last().unwrap(), body))).as_deref(), Some(REPO));
        assert_eq!(secrets.signer(body, Some(&sign(&given[0], body))), None);
        // No secret in any log line or recorded argument list.
        let calls = gh.calls.lock().unwrap().clone();
        for s in &given {
            assert!(log.iter().all(|l| !l.line.contains(s.as_str())));
            assert!(calls.iter().flatten().all(|a| a != s));
        }
        // A forwarder that died before its hook was proved leaves the hook
        // in place (H1): nothing recorded, nothing deleted.
        assert!(gh.deletes().is_empty(), "{:?}", gh.deletes());
        assert!(!log.iter().any(|l| l.what == What::Recorded), "{log:#?}");
        assert!(log.iter().any(|l| l.line.contains("no proven cli hook for this start; left in place")), "{log:#?}");
    }

    #[test]
    fn no_access_and_a_missing_gh_are_not_restarted() {
        let gh = FakeGh::new("noaccess", "noaccess");
        let (sup, rx, _store) = supervise(&gh, &Secrets::new());
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Stopped);
        sup.stop();
        let fatal: Vec<&Log> = log.iter().filter(|l| l.what == What::Fatal).collect();
        assert_eq!(fatal.len(), 1, "{log:#?}");
        assert!(fatal[0].line.contains("do not have access") && fatal[0].line.ends_with("not restarted"), "{}", fatal[0].line);
        assert!(fatal[0].journal());
        assert!(!log.iter().any(|l| l.what == What::Restart), "{log:#?}");
        assert_eq!(gh.secrets.lock().unwrap().len(), 1);
        let mut missing = FakeGh::new("missing", "run");
        Arc::get_mut(&mut missing).unwrap().missing = true;
        let (sup, rx, _store) = supervise(&missing, &Secrets::new());
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Stopped);
        sup.stop();
        assert!(log.iter().any(|l| l.what == What::Fatal && l.line.contains("gh is not installed")), "{log:#?}");
        assert!(missing.secrets.lock().unwrap().is_empty());
    }

    #[test]
    fn a_repository_that_does_not_read_gets_no_forwarder() {
        let gh = FakeGh::new("badrepo", "run");
        let store = Store::new("badrepo");
        let (setup, rx) = setup(&gh, &Secrets::new(), store.ids());
        start(&["local".to_string()], &setup).stop();
        let log: Vec<Log> = rx.try_iter().collect();
        let fatal: Vec<&Log> = log.iter().filter(|l| l.what == What::Fatal).collect();
        assert_eq!(fatal.len(), 1, "{log:#?}");
        assert!(gh.calls.lock().unwrap().is_empty());
    }

    /// Records `ids` for `repo` in `store` as a run that died would have.
    fn left(store: &Store, repo: &str, ids: &[u64]) {
        store_hooks(&store.store.lock().unwrap(), repo, ids).unwrap();
    }

    /// Adds a gh-webhook hook `id` to `repo` on the fake GitHub, made now by
    /// a forwarder that is not this one.
    fn made(gh: &FakeGh, repo: &str, id: u64) {
        gh.with_hooks(repo, |h| h.push(hook(id, HOOK_NAME, &all_events(), now_s())));
    }

    #[test]
    fn the_stored_ids_are_one_line_per_repository_and_ids_only() {
        let store = Store::new("format");
        let s = store.store.lock().unwrap();
        store_hooks(&s, REPO, &[7, 9]).unwrap();
        store_hooks(&s, REPO2, &[8]).unwrap();
        assert_eq!(s.meta_get(HOOKS_KEY).unwrap().unwrap(), "octo-org/gadgets 8\nocto-org/widgets 7 9");
        store_hooks(&s, REPO, &[9]).unwrap();
        assert_eq!(stored_hooks(&s).unwrap(), [(REPO2.to_string(), vec![8]), (REPO.to_string(), vec![9])]);
        store_hooks(&s, REPO, &[]).unwrap();
        store_hooks(&s, REPO2, &[]).unwrap();
        assert_eq!(s.meta_get(HOOKS_KEY).unwrap().unwrap(), "");
        assert!(stored_hooks(&s).unwrap().is_empty());
    }

    #[test]
    fn the_ids_are_stored_on_record_and_a_clean_stop_leaves_the_store_empty() {
        let gh = FakeGh::new("persist", "run");
        let store = Store::new("persist");
        let secrets = Secrets::new();
        let (s, rx) = setup(&gh, &secrets, store.ids());
        let sup = start(&[REPO.to_string()], &s);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Recorded);
        // In the store once recorded, before anything else happens.
        assert_eq!(store.stored(), [(REPO.to_string(), vec![100])]);
        // No secret in the store, only the ids.
        let value = store.store.lock().unwrap().meta_get(HOOKS_KEY).unwrap().unwrap();
        assert_eq!(value, "octo-org/widgets 100");
        for given in gh.secrets.lock().unwrap().iter() {
            assert!(!value.contains(given.as_str()));
        }
        sup.stop();
        drop(s);
        assert_eq!(gh.hook_ids(), [1, 2]);
        assert!(store.stored().is_empty());
        assert_eq!(store.store.lock().unwrap().meta_get(HOOKS_KEY).unwrap().as_deref(), Some(""));
        // A clean stop gives the lease up.
        assert_eq!(store.store.lock().unwrap().meta_get(LEASE_KEY).unwrap().as_deref(), Some(""));
    }

    #[test]
    fn a_restart_after_a_crash_sweeps_exactly_the_stored_ids() {
        let gh = FakeGh::new("crash", "run");
        let mut store = Store::new("crash");
        // The crashed run: its forwarders made hooks 50 and 51 and recorded
        // them, and 55, which someone has since deleted on GitHub. It never
        // stopped, so nothing was cleaned up.
        made(&gh, REPO, 50);
        made(&gh, REPO, 51);
        left(&store, REPO, &[50, 51, 55]);
        // A new process opens memory.sqlite again.
        store.reopen();
        let secrets = Secrets::new();
        let (s, rx) = setup(&gh, &secrets, store.ids());
        let sup = start(&[REPO.to_string()], &s);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Recorded);
        // Swept before the new forwarder started: the two recorded hooks
        // still there, nothing else.
        let calls = gh.calls.lock().unwrap().clone();
        let forward = calls.iter().position(|c| c[0] == "webhook").unwrap();
        let deletes_before: Vec<&String> = calls[..forward].iter().filter(|c| c[2] == "DELETE").map(|c| &c[3]).collect();
        assert_eq!(deletes_before, ["repos/octo-org/widgets/hooks/50", "repos/octo-org/widgets/hooks/51"]);
        // The unrecorded `cli` hook 2 stays; the new forwarder's hook is the
        // only stored id.
        assert_eq!(gh.hook_ids(), [1, 2, 100]);
        assert_eq!(store.stored(), [(REPO.to_string(), vec![100])]);
        let found = log.iter().find(|l| l.what == What::Found && l.line.contains("not stop cleanly")).unwrap();
        assert!(found.line.ends_with("50, 51, 55"), "{}", found.line);
        let report = log.iter().find(|l| l.what == What::Found && l.line.contains("present")).unwrap();
        assert!(
            report.line.contains("present: 50, 51; recorded but gone already: 55;") && report.line.ends_with("left alone: 2"),
            "{}",
            report.line
        );
        // An id already gone is cleared without an error.
        assert!(!log.iter().any(|l| l.what == What::Error), "{log:#?}");
        sup.stop();
        drop(s);
        assert_eq!(gh.deletes().len(), 3);
        assert_eq!(gh.hook_ids(), [1, 2]);
        assert!(store.stored().is_empty());
    }

    #[test]
    fn two_repositories_keep_separate_ids_and_an_unserved_one_is_swept() {
        let gh = FakeGh::new("two", "run");
        let mut store = Store::new("two");
        made(&gh, REPO2, 60);
        left(&store, REPO2, &[60]);
        store.reopen();
        // Both served: each repository's forwarder records its own hook, and
        // REPO2's earlier id is swept from REPO2 only.
        let (s, rx) = setup(&gh, &Secrets::new(), store.ids());
        let sup = start(&[REPO.to_string(), REPO2.to_string()], &s);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, seen| l.what == What::Recorded && seen.iter().any(|x| x.what == What::Recorded));
        let stored = store.stored();
        let (one, two) = (gh.hook_ids_of(REPO), gh.hook_ids_of(REPO2));
        assert_eq!(stored.len(), 2, "{stored:?}");
        assert_eq!(stored[0], (REPO2.to_string(), vec![*two.last().unwrap()]));
        assert_eq!(stored[1], (REPO.to_string(), vec![*one.last().unwrap()]));
        assert!(!two.contains(&60) && one.len() == 3 && two.len() == 3, "{one:?} {two:?}");
        assert_eq!(gh.deletes(), ["repos/octo-org/gadgets/hooks/60"]);
        sup.stop();
        drop(s);
        assert!(store.stored().is_empty());
        assert_eq!((gh.hook_ids_of(REPO), gh.hook_ids_of(REPO2)), (vec![1, 2], vec![1, 2]));
        // Ids left on a repository this run does not serve are swept too.
        made(&gh, REPO2, 70);
        left(&store, REPO2, &[70]);
        store.reopen();
        let (s, rx) = setup(&gh, &Secrets::new(), store.ids());
        let sup = start(&[REPO.to_string()], &s);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Recorded);
        sup.stop();
        drop(s);
        assert!(log.iter().any(|l| l.repo == REPO2 && l.what == What::Found && l.line.contains("does not serve")), "{log:#?}");
        assert_eq!(gh.hook_ids_of(REPO2), [1, 2]);
        assert!(store.stored().is_empty());
    }

    #[test]
    fn a_failed_list_keeps_every_stored_id_and_a_failed_delete_keeps_only_its_own() {
        let gh = FakeGh::new("fail", "run");
        let mut store = Store::new("fail");
        made(&gh, REPO, 50);
        made(&gh, REPO, 51);
        left(&store, REPO, &[50, 51]);
        store.reopen();
        // GitHub cannot be reached: nothing is deleted, every id stays.
        gh.fail_list.store(true, Ordering::SeqCst);
        let (s, rx) = setup(&gh, &Secrets::new(), store.ids());
        start(&[], &s).stop();
        drop(s);
        let log: Vec<Log> = rx.try_iter().collect();
        assert!(log.iter().any(|l| l.what == What::Error && l.line.contains("nothing deleted")), "{log:#?}");
        assert!(gh.deletes().is_empty());
        assert_eq!(store.stored(), [(REPO.to_string(), vec![50, 51])]);
        // The list works again, but deleting 51 fails: only 51 stays.
        gh.fail_list.store(false, Ordering::SeqCst);
        gh.fail_delete.lock().unwrap().insert(51);
        store.reopen();
        let (s, rx) = setup(&gh, &Secrets::new(), store.ids());
        start(&[], &s).stop();
        drop(s);
        let log: Vec<Log> = rx.try_iter().collect();
        assert!(log.iter().any(|l| l.what == What::Error && l.line.starts_with("deleting hook 51")), "{log:#?}");
        assert_eq!(gh.hook_ids(), [1, 2, 51]);
        assert_eq!(store.stored(), [(REPO.to_string(), vec![51])]);
    }

    #[test]
    fn a_log_line_files_as_a_note_the_queue_and_notice_do_not_read() {
        let d = std::env::temp_dir().join(format!("atlas-pr-supervise-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut s = MemoryStore::open(&d.join("memory.sqlite")).unwrap();
        let l = Log { repo: REPO.into(), what: What::Restart, line: "restart 1 in 1 s: gh webhook forward ended".into(), keep: true };
        assert!(l.journal() && !Log { keep: false, ..l.clone() }.journal());
        let key = journal(&mut s, "pr-events", &l).unwrap();
        let n = s.head(&key).unwrap().unwrap();
        assert_eq!(n.meta, "forwarder: octo-org/widgets\nwhat: restart");
        assert!(s.notes_after(0, crate::pr_events::EVENT_PREFIX).unwrap().is_empty());
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- H1: ownership is proved, never inferred.

    /// H1 (a): a foreign gh-webhook hook there before the start, and one
    /// that appears while the forwarder runs (inside the time window, with
    /// the same events), are pinged at most but never recorded or deleted.
    #[test]
    fn foreign_cli_hooks_before_and_during_a_run_are_never_recorded_or_deleted() {
        let gh = FakeGh::new("h1a", "run");
        // Hook 2 (a day old) is there before; 90 appears with our own hook,
        // after the list before the start: a candidate that is pinged and
        // never proved.
        gh.rivals.lock().unwrap().push(90);
        let (sup, rx, store) = supervise(&gh, &Secrets::new());
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Recorded);
        assert_eq!(store.stored(), [(REPO.to_string(), vec![100])]);
        assert!(
            gh.calls.lock().unwrap().iter().any(|c| c.last().is_some_and(|p| p.ends_with("/hooks/90/pings"))),
            "90 was not a candidate"
        );
        sup.stop();
        log.extend(rx.try_iter());
        assert_eq!(gh.deletes(), ["repos/octo-org/widgets/hooks/100"]);
        let mut ids = gh.hook_ids();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2, 90]);
        assert!(store.stored().is_empty());
    }

    /// H1 (b), the review's scenario 1: every look after the start fails;
    /// hours later another forwarder's hook appears; the child exits. Nothing
    /// foreign is recorded and nothing is deleted.
    #[test]
    fn a_failed_look_and_a_late_foreign_hook_record_and_delete_nothing() {
        let gh = FakeGh::new("h1b", "run");
        let store = Store::new("h1b");
        let (s, rx) = setup(&gh, &Secrets::new(), store.ids());
        // The list before the start works; every look after it fails.
        gh.lists_left.store(1, Ordering::SeqCst);
        let sup = start(&[REPO.to_string()], &s);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Error && l.line.starts_with("listing hooks after the start"));
        // Hours later: a teammate's forwarder makes its hook, GitHub answers
        // again, then this child exits.
        gh.with_hooks(REPO, |h| h.push(hook(9001, HOOK_NAME, &all_events(), now_s() + 6 * 3600)));
        gh.lists_left.store(-1, Ordering::SeqCst);
        let child = gh.pids.lock().unwrap()[0];
        crate::platform::process::terminate_tree(child);
        read_until(&rx, &mut log, |l, _| l.what == What::Restart);
        sup.stop();
        drop(s);
        log.extend(rx.try_iter());
        assert!(!gh.deletes().iter().any(|d| d.ends_with("/9001") || d.ends_with("/2")), "{:?}", gh.deletes());
        assert!(gh.hook_ids().contains(&9001));
        assert!(!store.stored().iter().any(|(_, ids)| ids.contains(&9001)), "{:?}", store.stored());
    }

    /// H1 (c), the review's scenario 2: two supervisors with two stores
    /// serve one repository and start within the window: each records only
    /// its own hook, and on stop each deletes only its own.
    #[test]
    fn two_supervisors_on_one_repository_record_only_their_own_hooks() {
        let gh = FakeGh::new("h1c", "run");
        let (a_store, b_store) = (Store::new("h1c-a"), Store::new("h1c-b"));
        let (a_set, a_rx) = setup_on(&gh, &Secrets::new(), a_store.ids(), 8001);
        let (b_set, b_rx) = setup_on(&gh, &Secrets::new(), b_store.ids(), 8002);
        let a = start(&[REPO.to_string()], &a_set);
        let b = start(&[REPO.to_string()], &b_set);
        let (mut la, mut lb) = (Vec::new(), Vec::new());
        read_until(&a_rx, &mut la, |l, _| l.what == What::Recorded);
        read_until(&b_rx, &mut lb, |l, _| l.what == What::Recorded);
        let (sa, sb) = (a_store.stored(), b_store.stored());
        assert_eq!((sa.len(), sb.len()), (1, 1), "{sa:?} {sb:?}");
        assert_eq!((sa[0].1.len(), sb[0].1.len()), (1, 1), "{sa:?} {sb:?}");
        assert_ne!(sa[0].1, sb[0].1);
        let (ha, hb) = (sa[0].1[0], sb[0].1[0]);
        a.stop();
        assert_eq!(gh.deletes(), [format!("repos/octo-org/widgets/hooks/{ha}")]);
        assert!(gh.hook_ids().contains(&hb), "a deleted b's live hook");
        b.stop();
        drop((a_set, b_set));
        assert_eq!(gh.hook_ids(), [1, 2]);
    }

    // ---- M2: the forwarder lease.

    #[test]
    fn the_lease_is_taken_when_absent_empty_stale_or_ours_only() {
        let store = Store::new("lease-unit");
        let s = store.store.lock().unwrap();
        let me = Holder::me();
        let stale = Duration::from_secs(60);
        let t = 1_000_000;
        // Absent, then ours again.
        assert!(lease_acquire(&s, me, t, stale).unwrap());
        assert!(lease_acquire(&s, me, t + 1, stale).unwrap());
        // Empty.
        s.meta_set(LEASE_KEY, "").unwrap();
        assert!(lease_acquire(&s, me, t, stale).unwrap());
        // A live holder (a running child with its real start) keeps it, until
        // its heartbeat is older than the bound.
        let dir = std::env::temp_dir();
        let mut c = helper("run", &dir).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let other = Holder { pid: c.id(), start: crate::platform::process::start_time(c.id()).unwrap_or(0) };
        s.meta_set(LEASE_KEY, &lease_text(other, t)).unwrap();
        assert!(!lease_acquire(&s, me, t + 59_000, stale).unwrap());
        assert!(!lease_beat(&s, me, t).unwrap() && !lease_release(&s, me).unwrap());
        assert!(lease_acquire(&s, me, t + 61_000, stale).unwrap(), "old heartbeat");
        // Changed start: the pid was reused.
        if other.start != 0 {
            s.meta_set(LEASE_KEY, &lease_text(Holder { start: other.start + 1, ..other }, t)).unwrap();
            assert!(lease_acquire(&s, me, t, stale).unwrap(), "changed start");
        }
        // A dead pid.
        crate::platform::process::terminate_tree(c.id());
        let _ = c.wait();
        s.meta_set(LEASE_KEY, &lease_text(other, t)).unwrap();
        assert!(lease_acquire(&s, me, t, stale).unwrap(), "dead pid");
        // Beat and release are compare-and-set on this holder.
        assert!(lease_beat(&s, me, t + 5).unwrap());
        assert_eq!(s.meta_get(LEASE_KEY).unwrap().unwrap(), lease_text(me, t + 5));
        assert!(lease_release(&s, me).unwrap());
        assert_eq!(s.meta_get(LEASE_KEY).unwrap().as_deref(), Some(""));
        assert!(!lease_beat(&s, me, t).unwrap());
    }

    /// Not a test: one racer for the lease race test. Waits for the `go`
    /// file in its folder, then tries to take the lease of the store there
    /// and writes `held` or `not` to a file named for its pid.
    #[test]
    #[ignore = "not a test: one racer of the lease race test"]
    fn lease_racer() {
        let Ok(dir) = std::env::var("ATLAS_LEASE_RACER") else { return };
        let dir = std::path::PathBuf::from(dir);
        let store = MemoryStore::open(&dir.join("memory.sqlite")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !dir.join("go").exists() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let got = lease_acquire(&store, Holder::me(), now_ms(), Duration::from_secs(60)).unwrap();
        let me = std::process::id();
        std::fs::write(dir.join(format!("{me}.tmp")), if got { "held" } else { "not" }).unwrap();
        std::fs::rename(dir.join(format!("{me}.tmp")), dir.join(format!("{me}.out"))).unwrap();
        // Stay alive until told, so the winner's lease stays live.
        while !dir.join("done").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// M2 (b): two processes take the lease of one store at the same
    /// instant, 20 rounds: exactly one holds it each round.
    #[test]
    fn two_processes_racing_for_the_lease_never_both_hold_it() {
        let store = Store::new("lease-race");
        for round in 0..20 {
            for f in ["go", "done"] {
                let _ = std::fs::remove_file(store.dir.join(f));
            }
            store.store.lock().unwrap().meta_set(LEASE_KEY, "").unwrap();
            let mut kids: Vec<Child> = (0..2)
                .map(|_| {
                    Command::new(std::env::current_exe().unwrap())
                        .args(["--ignored", "--exact", "pr_supervise::tests::lease_racer", "--quiet"])
                        .env("ATLAS_LEASE_RACER", &store.dir)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap()
                })
                .collect();
            // Both open the store before the start.
            std::thread::sleep(Duration::from_millis(300));
            std::fs::write(store.dir.join("go"), "").unwrap();
            let outs: Vec<String> = kids
                .iter()
                .map(|k| {
                    let p = store.dir.join(format!("{}.out", k.id()));
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while !p.exists() {
                        assert!(Instant::now() < deadline, "round {round}: a racer never answered");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    std::fs::read_to_string(&p).unwrap()
                })
                .collect();
            std::fs::write(store.dir.join("done"), "").unwrap();
            for k in &mut kids {
                let _ = k.wait();
                let _ = std::fs::remove_file(store.dir.join(format!("{}.out", k.id())));
            }
            assert_eq!(outs.iter().filter(|o| *o == "held").count(), 1, "round {round}: {outs:?}");
        }
    }

    /// M2 (c): a holder whose heartbeat stops is taken over by another
    /// process within the bound; the old holder's next beat finds it lost.
    #[test]
    fn a_holder_that_stops_beating_is_taken_over_within_the_bound() {
        let store = Store::new("lease-stale");
        let (a, b) = (store.store.lock().unwrap(), store.other());
        let b = b.lock().unwrap();
        let me = Holder::me();
        // Another live process, as b sees it.
        let mut c = helper("run", &std::env::temp_dir()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let other = Holder { pid: c.id(), start: crate::platform::process::start_time(c.id()).unwrap_or(0) };
        let stale = Duration::from_secs(60);
        let t = 5_000_000;
        assert!(lease_acquire(&a, other, t, stale).unwrap());
        assert!(lease_beat(&a, other, t + 15_000).unwrap());
        // The beats stop at t + 15 s: taken at the first try past 60 s.
        assert!(!lease_acquire(&b, me, t + 15_000 + 60_000, stale).unwrap());
        assert!(lease_acquire(&b, me, t + 15_000 + 60_001, stale).unwrap());
        assert!(!lease_beat(&a, other, t + 80_000).unwrap(), "the old holder still beats");
        crate::platform::process::terminate_tree(c.id());
        let _ = c.wait();
    }

    /// Writes a lease held by a live process other than this one.
    fn foreign_lease(store: &Store) -> Child {
        let c = helper("run", &std::env::temp_dir()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let other = Holder { pid: c.id(), start: crate::platform::process::start_time(c.id()).unwrap_or(0) };
        store.store.lock().unwrap().meta_set(LEASE_KEY, &lease_text(other, now_ms())).unwrap();
        c
    }

    /// M2: a supervisor that does not hold the lease never lists, starts,
    /// pings or deletes anything, even with ids left in the store.
    #[test]
    fn a_non_holder_never_calls_gh() {
        let gh = FakeGh::new("nonholder", "run");
        let store = Store::new("nonholder");
        made(&gh, REPO, 50);
        left(&store, REPO, &[50]);
        let mut holder = foreign_lease(&store);
        let (s, rx) = setup(&gh, &Secrets::new(), store.ids());
        let sup = start(&[REPO.to_string(), REPO2.to_string()], &s);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Lease);
        // Several lease ticks go by.
        std::thread::sleep(Duration::from_millis(400));
        sup.stop();
        drop(s);
        assert!(gh.calls.lock().unwrap().is_empty(), "{:?}", gh.calls.lock().unwrap());
        assert_eq!(store.stored(), [(REPO.to_string(), vec![50])]);
        crate::platform::process::terminate_tree(holder.id());
        let _ = holder.wait();
    }

    /// M2 (d): a holder that loses the lease ends its forwarder without
    /// deleting anything, and starts nothing more.
    #[test]
    fn a_holder_that_lost_the_lease_stops_forwarding_and_deletes_nothing() {
        let gh = FakeGh::new("lost", "run");
        let (sup, rx, store) = supervise(&gh, &Secrets::new());
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Recorded);
        let child = gh.pids.lock().unwrap()[0];
        let mut holder = foreign_lease(&store);
        read_until(&rx, &mut log, |l, _| l.what == What::Stopped);
        assert!(log.iter().any(|l| l.what == What::Lease && l.line.contains("taken over")), "{log:#?}");
        assert!(gone_within(child, Duration::from_secs(10)), "the forwarder still runs");
        let spawns = gh.spawns();
        std::thread::sleep(Duration::from_millis(300));
        sup.stop();
        assert!(gh.deletes().is_empty(), "{:?}", gh.deletes());
        assert_eq!(gh.spawns(), spawns);
        // The id stays for the new holder to sweep, and its lease stays.
        assert_eq!(store.stored(), [(REPO.to_string(), vec![100])]);
        assert!(!store.store.lock().unwrap().meta_get(LEASE_KEY).unwrap().unwrap().is_empty());
        crate::platform::process::terminate_tree(holder.id());
        let _ = holder.wait();
    }

    // ---- M4, L4, L5, L7, L8.

    /// M4: a forwarder that cannot start 100 times journals the first
    /// restart and one summary every 10, not 100 notes.
    #[test]
    fn a_flapping_forwarder_journals_state_changes_not_repeats() {
        let gh = FakeGh::new("flap", "run");
        gh.fail_list.store(true, Ordering::SeqCst);
        let store = Store::new("flap");
        let (mut s, rx) = setup(&gh, &Secrets::new(), store.ids());
        s.timing.first = Duration::ZERO;
        s.timing.max = Duration::ZERO;
        let sup = start(&[REPO.to_string()], &s);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, seen| l.what == What::Restart && seen.iter().filter(|x| x.what == What::Restart).count() == 99);
        sup.stop();
        drop(s);
        let kept: Vec<&Log> = log.iter().filter(|l| l.journal()).collect();
        assert_eq!(kept.len(), 11, "{kept:#?}");
        assert!(kept[0].line.starts_with("restart 1 in"), "{}", kept[0].line);
        assert!(
            kept[1].line.starts_with("restart 10 in") && kept[1].line.ends_with("(9 restarts since the last note)"),
            "{}",
            kept[1].line
        );
    }

    /// L4: a stop raised while the startup cleanup runs starts nothing.
    /// A stop to raise from inside the fake gh, set once the supervisor exists.
    type Raise = Option<Box<dyn Fn() + Send + Sync>>;

    #[test]
    fn a_stop_during_the_cleanup_starts_no_forwarder() {
        let gh = FakeGh::new("stopclean", "run");
        let store = Store::new("stopclean");
        made(&gh, REPO, 50);
        left(&store, REPO, &[50]);
        let (s, rx) = setup(&gh, &Secrets::new(), store.ids());
        let raise: Arc<Mutex<Raise>> = Arc::default();
        let r = Arc::clone(&raise);
        *gh.on_delete.lock().unwrap() = Some(Box::new(move || {
            if let Some(f) = r.lock().unwrap().as_ref() {
                f();
            }
        }));
        // Held until the raiser is in place, so the cleanup waits for it.
        let held = raise.lock().unwrap();
        let sup = start(&[REPO.to_string()], &s);
        let mut held = held;
        *held = Some(Box::new(sup.raiser()));
        drop(held);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Stopped);
        sup.stop();
        drop(s);
        assert_eq!(gh.deletes(), ["repos/octo-org/widgets/hooks/50"]);
        assert_eq!(gh.spawns(), 0, "a forwarder started after the stop");
    }

    /// L5: a forwarder that exits by itself while its own child lives does
    /// not leave that child running.
    #[test]
    fn a_forwarder_that_exits_leaves_no_child_behind() {
        let gh = FakeGh::new("orphan", "orphan");
        let (sup, rx, _store) = supervise(&gh, &Secrets::new());
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Restart);
        let grandchild = wait_file(&gh.dir.join("grandchild.pid"));
        sup.stop();
        assert!(gone_within(grandchild, Duration::from_secs(10)), "the exited forwarder's child still runs");
    }

    /// An [`Ids`] whose load fails, over a real store.
    struct NoLoad(Mutex<MemoryStore>);

    impl Ids for NoLoad {
        fn load(&self) -> std::result::Result<Stored, String> {
            Err("database is locked".into())
        }
        fn save(&self, repo: &str, ids: &[u64]) -> std::result::Result<(), String> {
            self.0.save(repo, ids)
        }
        fn acquire(&self, me: Holder, now_ms: u64, stale: Duration) -> std::result::Result<bool, String> {
            self.0.acquire(me, now_ms, stale)
        }
        fn beat(&self, me: Holder, now_ms: u64) -> std::result::Result<bool, String> {
            self.0.beat(me, now_ms)
        }
        fn release(&self, me: Holder) -> std::result::Result<bool, String> {
            self.0.release(me)
        }
        fn holds(&self, me: Holder) -> bool {
            self.0.holds(me)
        }
    }

    /// L7: when the stored ids do not load, a record does not overwrite the
    /// line a crashed run left: this run keeps its ids in memory only.
    #[test]
    fn a_failed_load_then_a_record_leaves_the_stored_line_as_it_was() {
        let gh = FakeGh::new("noload", "run");
        let dir = std::env::temp_dir().join(format!("atlas-pr-supervise-noload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = MemoryStore::open(&dir.join("memory.sqlite")).unwrap();
        store_hooks(&store, REPO, &[50, 51]).unwrap();
        let ids = Arc::new(NoLoad(Mutex::new(store)));
        let (s, rx) = setup(&gh, &Secrets::new(), Arc::clone(&ids) as Arc<dyn Ids>);
        let sup = start(&[REPO.to_string()], &s);
        let mut log = Vec::new();
        read_until(&rx, &mut log, |l, _| l.what == What::Recorded);
        assert_eq!(stored_hooks(&ids.0.lock().unwrap()).unwrap(), [(REPO.to_string(), vec![50, 51])]);
        sup.stop();
        drop(s);
        // Its own hook was still deleted on stop, from memory.
        assert_eq!(gh.deletes(), ["repos/octo-org/widgets/hooks/100"]);
        assert_eq!(stored_hooks(&ids.0.lock().unwrap()).unwrap(), [(REPO.to_string(), vec![50, 51])]);
        assert!(log.iter().filter(|l| l.line.contains("in memory only")).count() == 1, "{log:#?}");
        drop(ids);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// L8: every page of the hook list is read; a recorded id missing from
    /// it is dropped only on a 404, and kept on any other failure.
    #[test]
    fn a_recorded_id_is_dropped_only_when_github_says_404() {
        let gh = FakeGh::new("pages", "run");
        *gh.page.lock().unwrap() = 2;
        let s_ = Store::new("pages");
        // 50 is on page 2 (after 1 and 2); 55 is gone; 56 is missing from
        // the list but its GET fails with a 500.
        made(&gh, REPO, 50);
        gh.fail_get.lock().unwrap().insert(56);
        let (s, rx) = setup(&gh, &Secrets::new(), s_.ids());
        let g = Gen::default();
        let mut recorded = vec![50, 55, 56];
        let say = Say { sink: &s.log, repo: REPO };
        assert!(lease_acquire(&s_.store.lock().unwrap(), s.me, now_ms(), Duration::from_secs(60)).unwrap());
        cleanup(&s, &g, &target(), &mut recorded, &mut HashSet::new(), &say);
        assert_eq!(recorded, [56]);
        assert_eq!(gh.deletes(), ["repos/octo-org/widgets/hooks/50"]);
        let found: Vec<Log> = rx.try_iter().filter(|l| l.what == What::Found).collect();
        assert!(
            found[0].line.contains("gone already: 55;") && found[0].line.contains("missing from the list, kept: 56;"),
            "{}",
            found[0].line
        );
    }

    /// Cleanup deletes a recorded id only while the hook is still
    /// gh-webhook's: named `cli`, asking for exactly EVENTS.
    #[test]
    fn a_recorded_hook_that_changed_is_not_deleted() {
        let gh = FakeGh::new("changed", "run");
        let s_ = Store::new("changed");
        gh.with_hooks(REPO, |h| {
            h.push(hook(60, "web", &all_events(), now_s()));
            h.push(hook(61, HOOK_NAME, &["push"], now_s()));
        });
        let (s, _rx) = setup(&gh, &Secrets::new(), s_.ids());
        assert!(lease_acquire(&s_.store.lock().unwrap(), s.me, now_ms(), Duration::from_secs(60)).unwrap());
        let mut recorded = vec![60, 61];
        cleanup(&s, &Gen::default(), &target(), &mut recorded, &mut HashSet::new(), &Say { sink: &s.log, repo: REPO });
        assert!(gh.deletes().is_empty(), "{:?}", gh.deletes());
        assert!(recorded.is_empty());
    }

    // ---- L2, L6: processes tied to atlas.

    /// L2: a forwarder whose atlas is killed hard goes with it (Windows:
    /// the kill-on-close job; Linux: PR_SET_PDEATHSIG).
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_forwarder_does_not_outlive_an_atlas_killed_hard() {
        let dir = std::env::temp_dir().join(format!("atlas-pr-supervise-tied-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut parent = helper("parent", &dir).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let tied = wait_file(&dir.join("tied.pid"));
        assert!(crate::platform::process::running(tied));
        // A hard kill: no handler, no cleanup in the parent.
        parent.kill().unwrap();
        let _ = parent.wait();
        let gone = gone_within(tied, Duration::from_secs(30));
        if !gone {
            crate::platform::process::terminate_tree(tied);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "the forwarder outlived its atlas");
    }

    /// L6: the Windows kill checks the creation time on the handle it kills
    /// with: a different time (a reused pid) is not killed.
    #[cfg(windows)]
    #[test]
    fn the_tree_kill_refuses_a_process_born_at_another_time() {
        let mut c = helper("run", &std::env::temp_dir()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let born = crate::platform::process::start_time(c.id()).unwrap();
        assert!(!crate::platform::process::terminate_if_born(c.id(), born + 1));
        assert!(crate::platform::process::running(c.id()));
        assert!(crate::platform::process::terminate_if_born(c.id(), born));
        let _ = c.wait();
    }
}
