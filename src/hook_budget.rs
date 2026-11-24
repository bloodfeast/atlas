//! A hook's time budget: every hook event answers well inside Claude Code's
//! hook timeout, whatever the machine is doing.
//!
//! On 2026-11-18 a `UserPromptSubmit` hook was discarded after Claude Code's
//! 30 s limit on a loaded machine (journal:ATLAS-RELEASE:0221, "nothing should
//! take 30s"). A [`Budget`] is the hook's deadline, counted from when the
//! event arrived (a server's hook may wait behind another before its turn).
//! The hook's work is split into named phases; each phase is entered through
//! [`Budget::enter`], which refuses it once the deadline has passed, so the
//! hook answers with what it has and one line naming what it skipped.
//!
//! A phase blocked on the store is bounded too: [`bound`] installs a SQLite
//! busy handler that waits for another writer only until the deadline of the
//! budget active on this thread ([`Active`]), and with no budget active waits
//! as long as [`crate::memory::BUSY_TIMEOUT`] did before.
//!
//! `ATLAS_HOOK_TIMINGS=1` prints each hook's phases and their milliseconds on
//! stderr (`atlas hook timings EVENT: queue=N open=N recall=N ... total=N`).

use std::cell::Cell;
use std::time::{Duration, Instant};

/// The default budget of every hook event, in milliseconds.
pub const DEFAULT_MS: u64 = 3_000;

/// The most a configured budget may be: Claude Code's shortest hook timeout
/// in the shipped hooks is 10 s.
pub const MAX_MS: u64 = 10_000;

/// The least budget of the events no gate answers whose work is a store's
/// first open and a checkpoint (#333 item 2): PreCompact, SessionStart and
/// SessionEnd. A cut PreCompact loses atlas's compaction instructions and a
/// cut SessionStart the session card, while the compaction itself takes tens
/// of seconds; their hook timeouts are 10 s.
pub const LONG_MS: u64 = 8_000;

/// The budget of `event` given the configured one: at least [`LONG_MS`] for
/// PreCompact, SessionStart and SessionEnd, the configured one otherwise.
/// Never more than [`MAX_MS`].
pub fn for_event(limit: Duration, event: &str) -> Duration {
    if matches!(event, "PreCompact" | "SessionStart" | "SessionEnd") {
        limit.max(Duration::from_millis(LONG_MS)).min(Duration::from_millis(MAX_MS))
    } else {
        limit
    }
}

/// The environment variable that turns the per-phase timings on.
pub const TIMINGS_VAR: &str = "ATLAS_HOOK_TIMINGS";

/// A configured budget in milliseconds, clamped to 1..=[`MAX_MS`]; `None`
/// is [`DEFAULT_MS`].
pub fn limit(ms: Option<u64>) -> Duration {
    Duration::from_millis(ms.unwrap_or(DEFAULT_MS).clamp(1, MAX_MS))
}

/// The environment variable that turns the budget off, for measuring the
/// hooks without it (`ATLAS_HOOK_BUDGET_OFF=1`): phases are timed, never
/// skipped, store waits are SQLite's 30 s, and no answer is sent early. It
/// turns off the fail-closed refusal too: a gate that runs past Claude
/// Code's own timeout is not refused by atlas (every gate still runs to the
/// end and refuses on its own checks).
pub const OFF_VAR: &str = "ATLAS_HOOK_BUDGET_OFF";

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Whether [`TIMINGS_VAR`] is set to something other than empty or `0`.
pub fn timings_on() -> bool {
    flag(TIMINGS_VAR)
}

/// Whether the budget is on (see [`OFF_VAR`]).
pub fn enforced() -> bool {
    !flag(OFF_VAR)
}

/// One hook event's deadline and the phases it ran or skipped.
#[derive(Debug, Clone)]
pub struct Budget {
    start: Instant,
    deadline: Instant,
    limit: Duration,
    enforce: bool,
    mark: Instant,
    current: Option<&'static str>,
    phases: Vec<(&'static str, Duration)>,
    skipped: Vec<&'static str>,
    /// Skipped housekeeping phases ([`Budget::enter_quiet`]): in the trace,
    /// never in [`Budget::skip_line`].
    quiet: Vec<&'static str>,
}

impl Budget {
    /// A budget of `limit` counted from `arrived`. The time between
    /// `arrived` and now is the `queue` phase.
    pub fn new(arrived: Instant, limit: Duration) -> Budget {
        let mut b = Budget {
            start: arrived,
            deadline: arrived + limit,
            limit,
            enforce: true,
            mark: arrived,
            current: None,
            phases: Vec::new(),
            skipped: Vec::new(),
            quiet: Vec::new(),
        };
        b.current = Some("queue");
        b.close();
        b
    }

    /// The deadline.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The time left, zero once spent.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Whether the deadline has passed.
    pub fn spent(&self) -> bool {
        self.enforce && Instant::now() >= self.deadline
    }

    /// A budget that only times its phases and never skips one.
    pub fn timing_only(arrived: Instant) -> Budget {
        Budget { enforce: false, ..Budget::new(arrived, Duration::from_millis(MAX_MS)) }
    }

    /// [`Budget::new`], or [`Budget::timing_only`] when [`OFF_VAR`] is set.
    pub fn from_env(arrived: Instant, limit: Duration) -> Budget {
        if enforced() {
            Budget::new(arrived, limit)
        } else {
            Budget::timing_only(arrived)
        }
    }

    /// Ends the phase running now, recording its time.
    fn close(&mut self) {
        let now = Instant::now();
        if let Some(p) = self.current.take() {
            self.phases.push((p, now.saturating_duration_since(self.mark)));
        }
        self.mark = now;
    }

    /// Starts phase `name`, ending the one before: true when it may run,
    /// false (and `name` is recorded as skipped) once the budget is spent.
    pub fn enter(&mut self, name: &'static str) -> bool {
        self.close();
        if self.spent() {
            self.skipped.push(name);
            return false;
        }
        self.current = Some(name);
        crate::hook_gate::phase(name);
        test_delay(name);
        true
    }

    /// [`Budget::enter`] for housekeeping the model cannot act on (#355 S2):
    /// a skip is in [`Budget::skipped`] and the trace, never in
    /// [`Budget::skip_line`].
    pub fn enter_quiet(&mut self, name: &'static str) -> bool {
        let run = self.enter(name);
        if !run {
            self.quiet.push(name);
        }
        run
    }

    /// Starts phase `name`, timed but never skipped: work whose answer
    /// other state depends on.
    pub fn mark(&mut self, name: &'static str) {
        self.close();
        self.current = Some(name);
        crate::hook_gate::phase(name);
    }

    /// Ends the last phase.
    pub fn finish(&mut self) {
        self.close();
    }

    /// The phases skipped, in order.
    pub fn skipped(&self) -> &[&'static str] {
        &self.skipped
    }

    /// The phases run and their times, in order.
    pub fn phases(&self) -> &[(&'static str, Duration)] {
        &self.phases
    }

    /// The one line a hook adds when it skipped something, else `None`.
    pub fn skip_line(&self) -> Option<String> {
        let told: Vec<&str> = self.skipped.iter().copied().filter(|p| !self.quiet.contains(p)).collect();
        if told.is_empty() {
            return None;
        }
        Some(format!("atlas: hook time budget ({} ms) spent; skipped: {}", self.limit.as_millis(), told.join(", ")))
    }

    /// `atlas hook timings EVENT: queue=N phase=N ... total=N` (ms).
    pub fn trace_line(&self, event: &str) -> String {
        let mut s = format!("atlas hook timings {event}:");
        for (p, d) in &self.phases {
            s.push_str(&format!(" {p}={:.1}", d.as_secs_f64() * 1e3));
        }
        if !self.skipped.is_empty() {
            s.push_str(&format!(" skipped={}", self.skipped.join(",")));
        }
        s.push_str(&format!(" total={:.1}", Instant::now().saturating_duration_since(self.start).as_secs_f64() * 1e3));
        s
    }
}

static PROCESS_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Records the process's start (the binary's `main` calls it first), the
/// instant a command hook's budget counts from.
pub fn mark_process_start() {
    let _ = PROCESS_START.set(Instant::now());
}

/// The instant [`mark_process_start`] recorded, else now.
pub fn process_start() -> Instant {
    *PROCESS_START.get_or_init(Instant::now)
}

/// Milliseconds since `t`, for a timings line.
pub fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    // When this thread's current wait on a lock began. Per thread, not per
    // connection: a busy handler runs inside one blocked SQLite call, so a
    // thread is in at most one wait at a time.
    static BUSY_SINCE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// The budget whose deadline bounds this thread's store waits, while it
/// lives (see [`bound`]).
pub struct Active(Option<Instant>);

impl Active {
    /// Makes `budget`'s deadline this thread's store-wait deadline until
    /// dropped (none for a budget that only times).
    pub fn set(budget: &Budget) -> Active {
        let deadline = budget.enforce.then_some(budget.deadline);
        Active(DEADLINE.with(|d| d.replace(deadline)))
    }

    /// Makes `deadline` this thread's store-wait deadline until dropped
    /// (work after a hook's answer: see [`AFTER_WAIT`]).
    pub fn until(deadline: Instant) -> Active {
        Active(DEADLINE.with(|d| d.replace(Some(deadline))))
    }
}

/// The longest a server's work after a hook's answer (re-indexing, owed
/// session starts, the ideas spool, the restart point) waits on another
/// writer's lock of the memory store, so the hook queued behind it is not
/// held (review S4 of #329). What the lock refused is retried after a later
/// answer: owed starts and the ideas spool are kept.
pub const AFTER_WAIT: Duration = crate::ideas::QUICK;

/// `wait`, cut to the time left before this thread's active deadline (an
/// external call a hook makes, such as the merge gate's `gh`, answers
/// inside the hook's budget); `wait` when none is active.
pub fn cut(wait: Duration) -> Duration {
    DEADLINE.with(Cell::get).map_or(wait, |d| wait.min(d.saturating_duration_since(Instant::now())))
}

impl Drop for Active {
    fn drop(&mut self) {
        DEADLINE.with(|d| d.set(self.0));
    }
}

/// The busy handler [`bound`] installs on every store connection and every
/// [`crate::store::open_raw`] connection: waits for another writer with
/// sleeps of at most 2 ms until this thread's active deadline, or with none
/// active for [`crate::memory::BUSY_TIMEOUT`]. `n` is SQLite's count of
/// earlier calls for this one lock.
pub fn busy(n: i32) -> bool {
    busy_within(n, crate::memory::BUSY_TIMEOUT)
}

/// [`busy`] with a limit of `SECS` seconds in place of
/// [`crate::memory::BUSY_TIMEOUT`] (see [`bound_within`]).
fn busy_secs<const SECS: u64>(n: i32) -> bool {
    busy_within(n, Duration::from_secs(SECS))
}

fn busy_within(n: i32, limit: Duration) -> bool {
    let now = Instant::now();
    let since = BUSY_SINCE.with(|s| {
        if n == 0 || s.get().is_none() {
            s.set(Some(now));
        }
        s.get().unwrap_or(now)
    });
    let end = DEADLINE.with(Cell::get).unwrap_or(since + limit).min(since + limit);
    let left = end.saturating_duration_since(now);
    if left.is_zero() {
        return false;
    }
    std::thread::sleep(poll_step(n).min(left));
    true
}

/// How long [`busy`] sleeps before try `n`: 1 ms, then 2 ms however long the
/// wait. No back-off past 2 ms: a waiter that sleeps longer the longer it has
/// waited loses every free moment to writers that just arrived, and under
/// load can wait out the whole timeout behind short holds (#340).
pub fn poll_step(n: i32) -> Duration {
    Duration::from_millis(if n < 2 { 1 } else { 2 })
}

/// Installs [`busy`] on `conn` in place of a fixed busy timeout.
///
/// # Errors
///
/// SQLite's, when the handler cannot be set.
pub fn bound(conn: &crate::db::Connection) -> crate::db::Result<()> {
    conn.busy_handler(Some(busy))
}

/// [`bound`] for a caller that waits at most `SECS` seconds, shorter than
/// [`crate::memory::BUSY_TIMEOUT`]: the same 2 ms polls, so it is not
/// starved by the store's writers, and still the active deadline when that
/// comes first.
///
/// # Errors
///
/// SQLite's, when the handler cannot be set.
pub fn bound_within<const SECS: u64>(conn: &crate::db::Connection) -> crate::db::Result<()> {
    conn.busy_handler(Some(busy_secs::<SECS>))
}

#[cfg(test)]
thread_local! {
    static DELAY: Cell<Option<(&'static str, Duration)>> = const { Cell::new(None) };
}

/// Tests only: makes phase `name` take `d` before it starts (a slow phase).
#[cfg(test)]
pub fn inject_delay(name: &'static str, d: Duration) {
    DELAY.with(|c| c.set(Some((name, d))));
}

/// The injected delay of phase `name`, slept as the phase's own work: its
/// time counts against the budget the next [`Budget::enter`] checks.
#[cfg(test)]
fn test_delay(name: &str) {
    if let Some((p, d)) = DELAY.with(Cell::get) {
        if p == name {
            std::thread::sleep(d);
        }
    }
}

#[cfg(not(test))]
fn test_delay(_name: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slow_phase_is_cut_at_the_deadline() {
        let mut b = Budget::new(Instant::now(), Duration::from_millis(40));
        inject_delay("recall", Duration::from_millis(80));
        let t = Instant::now();
        assert!(b.enter("recall"));
        // The phases after the slow one are skipped, and named.
        assert!(!b.enter("ideas"));
        assert!(!b.enter("main_context"));
        b.finish();
        inject_delay("none", Duration::ZERO);
        assert!(t.elapsed() < Duration::from_millis(500));
        assert_eq!(b.skipped(), ["ideas", "main_context"]);
        assert_eq!(b.skip_line().unwrap(), "atlas: hook time budget (40 ms) spent; skipped: ideas, main_context");
        assert_eq!(b.phases().iter().map(|p| p.0).collect::<Vec<_>>(), ["queue", "recall"]);
        assert!(b.trace_line("UserPromptSubmit").contains("skipped=ideas,main_context"));
    }

    #[test]
    fn a_budget_in_time_skips_nothing_and_timing_only_never_skips() {
        let mut b = Budget::new(Instant::now(), Duration::from_secs(5));
        assert!(b.enter("a") && b.enter("b"));
        assert!(b.skip_line().is_none());
        let mut t = Budget::timing_only(Instant::now() - Duration::from_secs(60));
        assert!(t.enter("a"));
        assert!(t.skip_line().is_none());
    }

    #[test]
    fn the_configured_budget_is_clamped() {
        assert_eq!(limit(None), Duration::from_millis(DEFAULT_MS));
        assert_eq!(limit(Some(0)), Duration::from_millis(1));
        assert_eq!(limit(Some(60_000)), Duration::from_millis(MAX_MS));
        assert_eq!(limit(Some(1_500)), Duration::from_millis(1_500));
    }

    /// Review N2 of #343: [`bound_within`] gives up at its own limit, with
    /// no active deadline, where [`bound`] would wait the 3 s hold out.
    #[test]
    fn bound_within_gives_up_at_its_limit() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("atlas-bound-within-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("s.sqlite");
        crate::db::Connection::open(&path).unwrap().execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x);").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let holder_path = path.clone();
        let holder = std::thread::spawn(move || {
            let c = crate::db::Connection::open(&holder_path).unwrap();
            c.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES (1);").unwrap();
            tx.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(3));
            c.execute_batch("COMMIT").unwrap();
        });
        rx.recv().unwrap();
        let waiter = crate::db::Connection::open(&path).unwrap();
        bound_within::<1>(&waiter).unwrap();
        let t = Instant::now();
        let r = waiter.execute("INSERT INTO t VALUES (2)", []);
        let waited = t.elapsed();
        holder.join().unwrap();
        drop(waiter);
        let _ = std::fs::remove_dir_all(&d);
        assert!(r.is_err(), "the 3 s hold outlasts the 1 s limit: {waited:?}");
        assert!(waited >= Duration::from_millis(900) && waited < Duration::from_millis(2_500), "{waited:?}");
    }

    #[test]
    fn a_store_wait_ends_at_the_deadline() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("atlas-hook-budget-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("s.sqlite");
        let holder = crate::db::Connection::open(&path).unwrap();
        holder.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); BEGIN IMMEDIATE; INSERT INTO t VALUES (1);").unwrap();
        let waiter = crate::db::Connection::open(&path).unwrap();
        bound(&waiter).unwrap();
        let b = Budget::new(Instant::now(), Duration::from_millis(150));
        let active = Active::set(&b);
        let t = Instant::now();
        let r = waiter.execute("INSERT INTO t VALUES (2)", []);
        let waited = t.elapsed();
        drop(active);
        assert!(r.is_err(), "the writer holds the lock");
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        assert!(waited < Duration::from_secs(2), "{waited:?}");
        // With no budget active the wait is the old one: the lock released
        // during it is taken.
        let released = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            holder.execute_batch("COMMIT").unwrap();
        });
        waiter.execute("INSERT INTO t VALUES (3)", []).unwrap();
        released.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Review S4 of #329: the work after a hook's answer waits on another
    /// writer for [`AFTER_WAIT`], not the old 30 s; and a gh call inside a
    /// hook waits only the budget left.
    #[test]
    fn work_after_the_answer_waits_briefly_on_the_lock() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("atlas-hook-after-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("s.sqlite");
        let holder = crate::db::Connection::open(&path).unwrap();
        holder.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); BEGIN IMMEDIATE; INSERT INTO t VALUES (1);").unwrap();
        let waiter = crate::db::Connection::open(&path).unwrap();
        bound(&waiter).unwrap();
        let t = Instant::now();
        let r = {
            let _wait = Active::until(Instant::now() + AFTER_WAIT);
            waiter.execute("INSERT INTO t VALUES (2)", [])
        };
        let waited = t.elapsed();
        assert!(r.is_err(), "the writer holds the lock");
        assert!(waited < Duration::from_secs(1), "{waited:?}");
        assert_eq!(DEADLINE.with(Cell::get), None, "restored");
        // A gh wait: cut to the budget left, whole with none active.
        assert_eq!(cut(Duration::from_secs(5)), Duration::from_secs(5));
        let b = Budget::new(Instant::now(), Duration::from_millis(300));
        let _a = Active::set(&b);
        assert!(cut(Duration::from_secs(5)) <= Duration::from_millis(300));
        drop(holder);
        let _ = std::fs::remove_dir_all(&d);
    }
}
