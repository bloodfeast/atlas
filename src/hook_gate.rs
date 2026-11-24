//! Gate timing (#416): how long each hook event took on the hook worker,
//! and whether its answer refused the call or came from the watchdog past
//! the budget; the rows `atlas --hook-report` sums per event.
//!
//! A server answers every hook of its session, subagents' too, on one
//! worker thread, so a slow event delays every event queued behind it. The
//! watchdog names the cause in what it answers ([`why`]): the event the
//! worker is on and the phase it is in, as [`begin`] and the budget's
//! phases publish them.

use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use crate::db::{params, Connection};
use crate::error::Result;

/// The table of gate rows, made with the token tables.
pub const GATE_TABLE: &str = "
CREATE TABLE IF NOT EXISTS hook_gate(
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    event TEXT NOT NULL,
    tool TEXT NOT NULL,
    sub INTEGER NOT NULL,
    queue_ms REAL NOT NULL,
    work_ms REAL NOT NULL,
    total_ms REAL NOT NULL,
    outcome TEXT NOT NULL,
    phase TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS hook_gate_time ON hook_gate(time);
";

/// How a hook event's answer went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Answered in time; the call (if any) went ahead.
    Pass,
    /// Answered in time with a refusal.
    Deny,
    /// The watchdog answered past the budget and let the call through
    /// (the check was skipped).
    OverrunOpen,
    /// The watchdog answered past the budget with a refusal.
    OverrunDeny,
    /// The watchdog answered past the budget an event that is no gate.
    Overrun,
}

impl Outcome {
    /// Its name in the table.
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Deny => "deny",
            Outcome::OverrunOpen => "overrun_open",
            Outcome::OverrunDeny => "overrun_deny",
            Outcome::Overrun => "overrun",
        }
    }

    fn of(name: &str) -> Outcome {
        match name {
            "deny" => Outcome::Deny,
            "overrun_open" => Outcome::OverrunOpen,
            "overrun_deny" => Outcome::OverrunDeny,
            "overrun" => Outcome::Overrun,
            _ => Outcome::Pass,
        }
    }

    /// The call was refused.
    pub fn refused(self) -> bool {
        matches!(self, Outcome::Deny | Outcome::OverrunDeny)
    }

    /// The watchdog answered it.
    pub fn overrun(self) -> bool {
        matches!(self, Outcome::OverrunOpen | Outcome::OverrunDeny | Outcome::Overrun)
    }
}

/// One hook event's timing.
#[derive(Debug, Clone, PartialEq)]
pub struct GateRow {
    /// When it was answered (UTC, the store's time format).
    pub time: String,
    /// The hook event.
    pub event: String,
    /// The tool (PreToolUse, PostToolUse), else empty.
    pub tool: String,
    /// A subagent's event.
    pub sub: bool,
    /// Waiting behind other events on the worker, ms.
    pub queue_ms: f64,
    /// The worker's own work on it, ms (0 when it was dropped unworked).
    pub work_ms: f64,
    /// From its arrival to the worker's end of it, ms.
    pub total_ms: f64,
    /// How its answer went.
    pub outcome: Outcome,
    /// Its slowest phase, `name=ms`.
    pub phase: String,
}

/// Writes one row.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn log(conn: &Connection, r: &GateRow) -> Result<()> {
    crate::store::guarded(conn, || {
        conn.prepare_cached("INSERT INTO hook_gate(time, event, tool, sub, queue_ms, work_ms, total_ms, outcome, phase) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)")?
            .execute(params![r.time, r.event, r.tool, r.sub as i64, r.queue_ms, r.work_ms, r.total_ms, r.outcome.name(), r.phase])?;
        Ok(())
    })
}

/// The rows since `since`, oldest first; none when the table is missing.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn logged(conn: &Connection, since: &str) -> Result<Vec<GateRow>> {
    let exists: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'hook_gate'", [], |r| r.get(0))?;
    if exists == 0 {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare("SELECT time, event, tool, sub, queue_ms, work_ms, total_ms, outcome, phase FROM hook_gate WHERE time >= ?1 ORDER BY id")?;
    let rows = st.query_map([since], |r| {
        Ok(GateRow {
            time: r.get(0)?,
            event: r.get(1)?,
            tool: r.get(2)?,
            sub: r.get::<_, i64>(3)? != 0,
            queue_ms: r.get(4)?,
            work_ms: r.get(5)?,
            total_ms: r.get(6)?,
            outcome: Outcome::of(&r.get::<_, String>(7)?),
            phase: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// One event's sums in the report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventTotals {
    /// Rows.
    pub n: usize,
    /// Time from arrival, median, ms.
    pub p50: f64,
    /// Time from arrival, 95th percentile, ms.
    pub p95: f64,
    /// Time from arrival, most, ms.
    pub max: f64,
    /// Queue time p95, ms.
    pub queue_p95: f64,
    /// Refused calls (in time or by the watchdog).
    pub refused: usize,
    /// Watchdog answers.
    pub overruns: usize,
    /// Of those, the calls let through unchecked.
    pub open: usize,
    /// The slowest phase seen and its row's `name=ms`.
    pub slowest: String,
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// The rows' totals per event (a subagent's PreToolUse and PostToolUse
/// apart from the main session's, as `PreToolUse sub`), sorted by name.
pub fn totals(rows: &[GateRow]) -> Vec<(String, EventTotals)> {
    let mut by: std::collections::BTreeMap<String, Vec<&GateRow>> = std::collections::BTreeMap::new();
    for r in rows {
        let name = if r.sub { format!("{} sub", r.event) } else { r.event.clone() };
        by.entry(name).or_default().push(r);
    }
    by.into_iter()
        .map(|(name, rs)| {
            let mut total: Vec<f64> = rs.iter().map(|r| r.total_ms).collect();
            total.sort_by(f64::total_cmp);
            let mut queue: Vec<f64> = rs.iter().map(|r| r.queue_ms).collect();
            queue.sort_by(f64::total_cmp);
            let slow = rs.iter().max_by(|a, b| a.total_ms.total_cmp(&b.total_ms)).map(|r| r.phase.clone()).unwrap_or_default();
            let t = EventTotals {
                n: rs.len(),
                p50: pct(&total, 0.5),
                p95: pct(&total, 0.95),
                max: total.last().copied().unwrap_or(0.0),
                queue_p95: pct(&queue, 0.95),
                refused: rs.iter().filter(|r| r.outcome.refused()).count(),
                overruns: rs.iter().filter(|r| r.outcome.overrun()).count(),
                open: rs.iter().filter(|r| r.outcome == Outcome::OverrunOpen).count(),
                slowest: slow,
            };
            (name, t)
        })
        .collect()
}

/// The report's gate table, a header and one line per event.
pub fn report_lines(rows: &[GateRow]) -> Vec<String> {
    let mut out = vec!["gate\tevents\tp50_ms\tp95_ms\tmax_ms\tqueue_p95_ms\trefused\toverruns\toverrun_open\tslowest".to_string()];
    for (name, t) in totals(rows) {
        out.push(format!("{name}\t{}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{}\t{}\t{}\t{}", t.n, t.p50, t.p95, t.max, t.queue_p95, t.refused, t.overruns, t.open, t.slowest));
    }
    out
}

/// A row for an event answered now, from its budget's phases.
pub fn row(event: &str, tool: &str, sub: bool, arrived: Instant, phases: &[(&'static str, std::time::Duration)], outcome: Outcome) -> GateRow {
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let total_ms = ms(arrived.elapsed());
    let queue_ms = phases.iter().find(|(p, _)| *p == "queue").map_or(total_ms, |(_, d)| ms(*d));
    let slow = phases.iter().max_by_key(|(_, d)| *d).map_or_else(|| format!("queue={queue_ms:.0}"), |(p, d)| format!("{p}={:.0}", ms(*d)));
    GateRow {
        time: crate::store::utc(SystemTime::now()),
        event: event.to_string(),
        tool: tool.to_string(),
        sub,
        queue_ms,
        work_ms: (total_ms - queue_ms).max(0.0),
        total_ms,
        outcome,
        phase: slow,
    }
}

/// What the hook worker is on, for the watchdog's reason.
struct Now {
    id: String,
    what: String,
    phase: &'static str,
    phase_at: Instant,
}

static NOW: Mutex<Option<Now>> = Mutex::new(None);

/// The worker starts on hook `id` (`what`: its event and tool).
pub fn begin(id: &str, what: &str) {
    if let Ok(mut n) = NOW.lock() {
        *n = Some(Now { id: id.to_string(), what: what.to_string(), phase: "start", phase_at: Instant::now() });
    }
}

/// The worker's event enters phase `name` (the budget calls it).
pub fn phase(name: &'static str) {
    if let Ok(mut n) = NOW.lock() {
        if let Some(n) = n.as_mut() {
            n.phase = name;
            n.phase_at = Instant::now();
        }
    }
}

/// The worker is done with its event.
pub fn end() {
    if let Ok(mut n) = NOW.lock() {
        *n = None;
    }
}

/// Why hook `id`, which arrived at `arrived`, is not answered yet: the
/// phase it is in and for how long, else its wait in the queue and what the
/// worker is on meanwhile. Times in ms.
pub fn why(id: &str, arrived: Instant) -> String {
    let now = Instant::now();
    let ms = |t: Instant| now.saturating_duration_since(t).as_millis();
    let n = NOW.lock().ok();
    match n.as_ref().and_then(|n| n.as_ref()) {
        Some(n) if n.id == id => format!("phase {} {} ms", n.phase, ms(n.phase_at)),
        Some(n) => format!("queue {} ms, behind {} in phase {} for {} ms", ms(arrived), n.what, n.phase, ms(n.phase_at)),
        None => format!("queue {} ms", ms(arrived)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(event: &str, sub: bool, total: f64, outcome: Outcome) -> GateRow {
        GateRow { time: "t".into(), event: event.into(), tool: "Read".into(), sub, queue_ms: total / 2.0, work_ms: total / 2.0, total_ms: total, outcome, phase: "cap=1".into() }
    }

    #[test]
    fn totals_count_refusals_and_overruns_per_event() {
        let rows = [
            r("PreToolUse", true, 10.0, Outcome::Pass),
            r("PreToolUse", true, 4_000.0, Outcome::OverrunOpen),
            r("PreToolUse", true, 4_100.0, Outcome::OverrunDeny),
            r("PreToolUse", false, 5.0, Outcome::Deny),
            r("PostToolUse", true, 3_500.0, Outcome::Overrun),
        ];
        let t: std::collections::HashMap<String, EventTotals> = totals(&rows).into_iter().collect();
        let sub = &t["PreToolUse sub"];
        assert_eq!((sub.n, sub.refused, sub.overruns, sub.open), (3, 1, 2, 1));
        assert_eq!(sub.max, 4_100.0);
        assert_eq!((t["PreToolUse"].refused, t["PreToolUse"].overruns), (1, 0));
        assert_eq!((t["PostToolUse sub"].refused, t["PostToolUse sub"].overruns), (0, 1));
        assert_eq!(report_lines(&rows).len(), 4);
    }

    #[test]
    fn rows_round_trip_through_the_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(GATE_TABLE).unwrap();
        let a = r("PreToolUse", true, 12.5, Outcome::OverrunOpen);
        log(&conn, &a).unwrap();
        assert_eq!(logged(&conn, "").unwrap(), vec![a]);
        assert!(logged(&Connection::open_in_memory().unwrap(), "").unwrap().is_empty(), "no table is no rows");
    }
}
