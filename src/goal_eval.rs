//! ATLAS-GOAL build step 2 (docs/design/atlas-goal.md sections 3 and 4.1):
//! the `metric:` lines of `measurement` journal entries and the evaluator.
//!
//! The evaluator reads only the store: no transcript, no model, no prose
//! (Q-AG3). A gate never fed is "not measured", never met; an aim is
//! reported and never decides met.

use super::{key, meta_value, parse_spec, split, Bar, Cmp, Take, KIND};
use crate::error::{Error, Result};
use crate::memory::MemoryStore;

/// What starts a metric line.
pub const METRIC_PREFIX: &str = "metric:";

/// A metric line's form, for refusals.
pub const METRIC_FORM: &str = "`metric: <metric> <value> <unit> [arm <arm>] @<commit> <quiet|contended>`";

/// The journal entry kind whose metric lines feed gates and aims.
pub const MEASUREMENT: &str = "measurement";

/// What starts a goal checkpoint's value line (journal:ATLAS-GOAL:0039 Q-GS7).
pub const VALUE_PREFIX: &str = "value:";

/// One parsed `metric:` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    /// A dotted name with an optional `@case`.
    pub metric: String,
    /// The value.
    pub value: f64,
    /// The value as written.
    pub raw: String,
    /// The unit.
    pub unit: String,
    /// The arm, if the line names one.
    pub arm: Option<String>,
    /// The commit measured, lower-cased, without `@`.
    pub commit: String,
    /// `quiet` (true) or `contended` (false).
    pub quiet: bool,
}

/// Parses one metric line (section 3).
///
/// # Errors
///
/// Why the line is not in the form, with the form.
pub fn parse_metric(line: &str) -> std::result::Result<Metric, String> {
    let rest = line.trim().strip_prefix(METRIC_PREFIX).ok_or_else(|| format!("not a metric line: {METRIC_FORM}"))?;
    let w: Vec<&str> = rest.split_whitespace().collect();
    let metric = match w.first() {
        Some(m) if super::is_metric(m) => *m,
        _ => return Err(format!("no metric name (a letter, then letters, digits, `._-`, optional `@case`): {METRIC_FORM}")),
    };
    let raw = w.get(1).filter(|v| super::is_number(v)).ok_or_else(|| format!("{metric} has no numeric value: {METRIC_FORM}"))?;
    let unit = w.get(2).filter(|u| !u.starts_with('@') && !matches!(**u, "arm" | "quiet" | "contended")).ok_or_else(|| format!("{metric} has no unit: {METRIC_FORM}"))?;
    let mut i = 3;
    let mut arm = None;
    if w.get(i) == Some(&"arm") {
        let a = w.get(i + 1).filter(|a| !a.starts_with('@') && !matches!(**a, "quiet" | "contended")).ok_or_else(|| format!("{metric}: arm names no arm: {METRIC_FORM}"))?;
        arm = Some(a.to_string());
        i += 2;
    }
    let commit = w
        .get(i)
        .and_then(|c| c.strip_prefix('@'))
        .filter(|c| (7..=40).contains(&c.len()) && c.chars().all(|x| x.is_ascii_hexdigit()))
        .ok_or_else(|| format!("{metric}: no @<commit> (7 to 40 hex digits) after the unit{}: {METRIC_FORM}", if arm.is_some() { " and arm" } else { "" }))?;
    let quiet = match w.get(i + 1) {
        Some(&"quiet") => true,
        Some(&"contended") => false,
        _ => return Err(format!("{metric}: the machine state after @{commit} is quiet or contended: {METRIC_FORM}")),
    };
    if let Some(extra) = w.get(i + 2) {
        return Err(format!("{metric}: {extra:?} after the machine state: {METRIC_FORM}"));
    }
    Ok(Metric { metric: metric.to_string(), value: raw.parse().unwrap_or(f64::NAN), raw: raw.to_string(), unit: unit.to_string(), arm, commit: commit.to_ascii_lowercase(), quiet })
}

/// Every metric line of a text with its 1-based line number, parsed: a
/// line is a metric line when it starts with `metric:` (leading blanks
/// allowed).
pub fn metric_lines(text: &str) -> Vec<(usize, std::result::Result<Metric, String>)> {
    text.lines().enumerate().filter(|(_, l)| l.trim_start().starts_with(METRIC_PREFIX)).map(|(i, l)| (i + 1, parse_metric(l))).collect()
}

/// The write-time check of a measurement entry's text (title line first):
/// the number of metric lines, or the first one refused.
///
/// # Errors
///
/// [`Error::InvalidArgument`] naming the line and why, nothing written.
pub fn check_metric_lines(text: &str) -> Result<usize> {
    let lines = metric_lines(text);
    for (n, r) in &lines {
        if let Err(e) = r {
            return Err(Error::InvalidArgument(format!("measurement refused, nothing written: line {n}: {e}")));
        }
    }
    Ok(lines.len())
}

/// A gate's or aim's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No usable line (no full set, or never fed): never met.
    NotMeasured,
    /// The value holds the bar.
    Met,
    /// The value misses the bar.
    Not,
}

/// The value a gate or aim took.
#[derive(Debug, Clone, PartialEq)]
pub struct Taken {
    /// The value.
    pub value: f64,
    /// As written in its line.
    pub raw: String,
    /// The arm it came from (the worst arm, by default).
    pub arm: Option<String>,
    /// The commit of the set.
    pub commit: String,
    /// Every line used was quiet.
    pub quiet: bool,
    /// The measurement entry of the newest line used.
    pub source: String,
}

/// A newer set than the one used that lacks some of the arms: reported,
/// not used (section 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial {
    /// Its commit.
    pub commit: String,
    /// The arms it has, in the gate's order.
    pub have: Vec<String>,
    /// The arms it lacks, in the gate's order.
    pub missing: Vec<String>,
}

/// One gate or aim, evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct BarEval {
    /// `G1`, `A1`.
    pub id: String,
    /// An aim (reported, never decides met).
    pub aim: bool,
    /// The spec line.
    pub bar: Bar,
    /// The value taken, if any.
    pub taken: Option<Taken>,
    /// Met, not, or not measured.
    pub verdict: Verdict,
    /// The value minus the last goal checkpoint's value of this id.
    pub delta: Option<f64>,
    /// A newer partial set, when there is one.
    pub partial: Option<Partial>,
}

impl BarEval {
    /// The value minus the bar, when measured.
    pub fn over(&self) -> Option<f64> {
        Some(self.taken.as_ref()?.value - self.bar.bar.parse::<f64>().ok()?)
    }
}

/// A goal, evaluated (section 4: one evaluator for every check-in kind).
#[derive(Debug, Clone, PartialEq)]
pub struct Eval {
    /// `goal:<slug>`.
    pub key: String,
    /// The record version evaluated.
    pub version: i64,
    /// Gates, then aims, each in spec order.
    pub bars: Vec<BarEval>,
    /// Gates met.
    pub gates_met: usize,
    /// Gates.
    pub gates: usize,
    /// Aims met (reported only).
    pub aims_met: usize,
    /// Aims.
    pub aims: usize,
    /// The work-order step: the first whose `done-when` does not hold on
    /// a quiet line (a step with none is never passed). `None` with no
    /// steps, or every step done.
    pub step: Option<u32>,
    /// The number of steps.
    pub steps: usize,
    /// Per serving lane, the time of its newest journal entry.
    pub lanes: Vec<(String, Option<String>)>,
    /// The goal checkpoint the deltas are against.
    pub checkpoint: Option<String>,
    /// Lines not used and why: a malformed line that reached the store,
    /// a unit unlike the gate's (journal:ATLAS-GOAL:0039 Q-GS6, Q-GS9).
    pub skipped: Vec<String>,
    /// Why the spec did not parse, when it did not.
    pub spec_error: Option<String>,
}

impl Eval {
    /// Every gate met, with at least one gate; aims never count.
    pub fn all_met(&self) -> bool {
        self.gates > 0 && self.gates_met == self.gates
    }

    /// The gate or aim `id`.
    pub fn bar(&self, id: &str) -> Option<&BarEval> {
        self.bars.iter().find(|b| b.id == id)
    }
}

/// A metric line read from the store, with where it stands.
#[derive(Debug, Clone)]
struct Line {
    /// Store order: larger is newer.
    pos: usize,
    /// The measurement entry.
    source: String,
    m: Metric,
}

/// Every metric line of every active measurement entry, oldest first; the
/// lines that do not parse go to `skipped`.
fn read_lines(store: &MemoryStore, skipped: &mut Vec<String>) -> Result<Vec<Line>> {
    let mut st = store.conn().prepare_cached(
        "SELECT r.key, v.title, v.body FROM records r JOIN versions v ON v.vid = r.head
         WHERE r.kind = 'journal' AND r.sub = ?1 AND r.status = 'active' AND (v.body LIKE '%metric:%' OR v.title LIKE '%metric:%')
         ORDER BY r.rid",
    )?;
    let rows: Vec<(String, String, String)> = st.query_map([MEASUREMENT], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
    let mut out = Vec::new();
    for (k, title, body) in rows {
        for (n, r) in metric_lines(&format!("{title}\n{body}")) {
            match r {
                Ok(m) => out.push(Line { pos: out.len(), source: k.clone(), m }),
                Err(e) => skipped.push(format!("{k} line {n}: {e}")),
            }
        }
    }
    Ok(out)
}

/// How far a value is on the wrong side of the bar: larger is worse.
fn badness(cmp: Cmp, bar: f64, v: f64) -> f64 {
    match cmp {
        Cmp::Lt | Cmp::Le => v - bar,
        Cmp::Gt | Cmp::Ge => bar - v,
        Cmp::Eq => (v - bar).abs(),
    }
}

/// Whether `v` holds `cmp bar`.
fn holds(cmp: Cmp, v: f64, bar: f64) -> bool {
    match cmp {
        Cmp::Lt => v < bar,
        Cmp::Le => v <= bar,
        Cmp::Gt => v > bar,
        Cmp::Ge => v >= bar,
        Cmp::Eq => v == bar,
    }
}

/// The value a gate or aim takes from `lines` (section 3): quiet only for
/// a `quiet` bar, its unit only; without `over`, the newest line; with
/// `over`, the newest commit with every arm, worst (or median) arm.
fn take(b: &Bar, lines: &[Line], skipped: &mut Vec<String>) -> (Option<Taken>, Option<Partial>) {
    let bar: f64 = b.bar.parse().unwrap_or(f64::NAN);
    let mut fed: Vec<&Line> = Vec::new();
    for l in lines.iter().filter(|l| l.m.metric == b.metric && (!b.quiet || l.m.quiet)) {
        if l.m.unit != b.unit {
            skipped.push(format!("{}: {} {} {} is not in {}'s unit {}", l.source, l.m.metric, l.m.raw, l.m.unit, b.id, b.unit));
            continue;
        }
        if b.over.is_empty() || l.m.arm.as_ref().is_some_and(|a| b.over.contains(a)) {
            fed.push(l);
        }
    }
    let taken = |used: &[&Line], pick: &Line| Taken {
        value: pick.m.value,
        raw: pick.m.raw.clone(),
        arm: pick.m.arm.clone(),
        commit: pick.m.commit.clone(),
        quiet: used.iter().all(|l| l.m.quiet),
        source: used.iter().max_by_key(|l| l.pos).map(|l| l.source.clone()).unwrap_or_default(),
    };
    if b.over.is_empty() {
        return (fed.iter().max_by_key(|l| l.pos).map(|l| taken(&[l], l)), None);
    }
    // Per commit, the newest line of each arm; commits newest first.
    let mut sets: Vec<(usize, String, Vec<&Line>)> = Vec::new();
    for l in &fed {
        match sets.iter_mut().find(|s| s.1 == l.m.commit) {
            Some(s) => {
                s.0 = s.0.max(l.pos);
                s.2.retain(|x| x.m.arm != l.m.arm);
                s.2.push(l);
            }
            None => sets.push((l.pos, l.m.commit.clone(), vec![l])),
        }
    }
    sets.sort_by_key(|s| std::cmp::Reverse(s.0));
    let has = |s: &[&Line], a: &String| s.iter().any(|l| l.m.arm.as_ref() == Some(a));
    let mut partial = None;
    for (_, commit, set) in &sets {
        if b.over.iter().all(|a| has(set, a)) {
            let mut by: Vec<&Line> = set.clone();
            by.sort_by(|x, y| badness(b.cmp, bar, x.m.value).total_cmp(&badness(b.cmp, bar, y.m.value)));
            let pick = match b.take {
                Take::Worst => by[by.len() - 1],
                Take::Median => by[by.len() / 2],
            };
            return (Some(taken(&by, pick)), partial);
        }
        if partial.is_none() {
            let (have, missing): (Vec<String>, Vec<String>) = b.over.iter().cloned().partition(|a| has(set, a));
            partial = Some(Partial { commit: commit.clone(), have, missing });
        }
    }
    (None, partial)
}

/// A goal checkpoint's key and its values by id.
type Checkpoint = (String, Vec<(String, f64)>);

/// The newest goal checkpoint of `k` in `lane`: its key and its values by
/// id (journal:ATLAS-GOAL:0039 Q-GS7).
fn last_checkpoint(store: &MemoryStore, k: &str, lane: &str) -> Result<Option<Checkpoint>> {
    let mut st = store.conn().prepare_cached(
        "SELECT r.key, v.meta, v.body FROM records r JOIN versions v ON v.vid = r.head
         WHERE r.kind = 'journal' AND r.sub = 'checkpoint' AND r.scope = ?1 AND v.meta LIKE '%goal:%' ORDER BY r.rid DESC",
    )?;
    let rows: Vec<(String, String, String)> = st.query_map([lane], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
    Ok(rows.into_iter().find(|(_, meta, _)| meta_value(meta, "goal") == Some(k)).map(|(ck, _, body)| {
        let values = body
            .lines()
            .filter_map(|l| {
                let w: Vec<&str> = l.trim().strip_prefix(VALUE_PREFIX)?.split_whitespace().collect();
                Some((w.first()?.to_string(), w.get(1)?.parse().ok()?))
            })
            .collect();
        (ck, values)
    }))
}

/// A goal checkpoint's body: one `value: <id> <value> <unit> @<commit>`
/// line per measured gate and aim (journal:ATLAS-GOAL:0039 Q-GS7). The
/// entry that carries it is a `checkpoint` journal entry in the goal's
/// lane with meta `goal: goal:<slug>` (written by build step 4).
pub fn checkpoint_values(e: &Eval) -> String {
    e.bars.iter().filter_map(|b| b.taken.as_ref().map(|t| format!("{VALUE_PREFIX} {} {} {} @{}\n", b.id, t.raw, b.bar.unit, t.commit))).collect()
}

/// The newest journal entry time of a lane.
fn lane_time(store: &MemoryStore, lane: &str) -> Result<Option<String>> {
    Ok(store
        .conn()
        .prepare_cached("SELECT MAX(v.time) FROM records r JOIN versions v ON v.vid = r.head WHERE r.kind = 'journal' AND r.scope = ?1")?
        .query_row([lane], |r| r.get(0))?)
}

/// Evaluates `goal` (a slug or `goal:<slug>`) against the store: per gate
/// and aim the full-set selection, quiet filter and worst (or median) arm,
/// met or not, the delta against the goal's last checkpoint; the met
/// counts, the work-order step and each serving lane's last entry time.
///
/// # Errors
///
/// [`Error::NotFound`] for no such goal; [`Error::Db`].
pub fn evaluate(store: &MemoryStore, goal: &str) -> Result<Eval> {
    let k = if goal.starts_with("goal:") { goal.to_string() } else { key(goal) };
    let h = store.head(&k)?.filter(|h| h.kind == KIND).ok_or_else(|| Error::NotFound(format!("goal {k}")))?;
    let mut e = Eval {
        key: k.clone(),
        version: h.seq,
        bars: Vec::new(),
        gates_met: 0,
        gates: 0,
        aims_met: 0,
        aims: 0,
        step: None,
        steps: 0,
        lanes: Vec::new(),
        checkpoint: None,
        skipped: Vec::new(),
        spec_error: None,
    };
    let spec = match split(&h.body).spec.map(|s| parse_spec(&s)) {
        None => return Ok(e),
        Some(Err(why)) => {
            e.spec_error = Some(why);
            return Ok(e);
        }
        Some(Ok(s)) => s,
    };
    let lines = read_lines(store, &mut e.skipped)?;
    let cp = last_checkpoint(store, &k, &h.scope)?;
    for (b, aim) in spec.gates.iter().map(|b| (b, false)).chain(spec.aims.iter().map(|b| (b, true))) {
        let (taken, partial) = take(b, &lines, &mut e.skipped);
        let verdict = match &taken {
            None => Verdict::NotMeasured,
            Some(t) if b.bar.parse().is_ok_and(|bar| holds(b.cmp, t.value, bar)) => Verdict::Met,
            Some(_) => Verdict::Not,
        };
        let delta = match (&taken, &cp) {
            (Some(t), Some((_, vals))) => vals.iter().find(|(id, _)| *id == b.id).map(|(_, v)| t.value - v),
            _ => None,
        };
        if aim {
            e.aims += 1;
            e.aims_met += usize::from(verdict == Verdict::Met);
        } else {
            e.gates += 1;
            e.gates_met += usize::from(verdict == Verdict::Met);
        }
        e.bars.push(BarEval { id: b.id.clone(), aim, bar: b.clone(), taken, verdict, delta, partial });
    }
    e.checkpoint = cp.map(|(ck, _)| ck);
    e.steps = spec.steps.len();
    e.step = spec
        .steps
        .iter()
        .find(|st| {
            let Some((metric, cmp, n)) = &st.done_when else { return true };
            let newest = lines.iter().filter(|l| &l.m.metric == metric && l.m.quiet).max_by_key(|l| l.pos);
            !newest.is_some_and(|l| n.parse().is_ok_and(|bar| holds(*cmp, l.m.value, bar)))
        })
        .map(|st| st.n);
    for (lane, _) in &spec.lanes {
        if !e.lanes.iter().any(|(l, _)| l == lane) {
            e.lanes.push((lane.clone(), lane_time(store, lane)?));
        }
    }
    e.skipped.dedup();
    Ok(e)
}

#[cfg(test)]
#[path = "goal_eval_tests.rs"]
mod tests;
