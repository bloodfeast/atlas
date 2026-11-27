//! The enforced dogfood mode of the task judge (`orchestrate.judge.mode:
//! "enforced"`, #412). At an admitted builder launch the hook scores the
//! brief with the classifier ([`crate::taskjudge::classify_with`], the
//! scoring `note op=judge` does), denies a launch whose `model` or `effort`
//! is off the pick, owns the explore counter (one explore launch every
//! `orchestrate.judge.explore_every` routed launches, default 5, at least
//! 2) and appends one row per admitted launch to the dogfood log
//! (`orchestrate.judge.log`).
//!
//! Exceptions, each a prompt line:
//! - `Judge: brandon-named <model>/<effort>`: Brandon named the model; the
//!   launch is allowed at that pair, logged with `brandon_named` true and
//!   never counted. The only way to launch at fable/max or any xhigh/max
//!   effort ([`brandon_tier`]).
//! - `Judge: explore`: allowed only when the explore launch is due, and
//!   only at exactly one rung below the pick ([`rungs`]).
//! - `Judge: explore <model>/<effort>`: allowed only when the explore
//!   launch is due, at any valid pair, on the map or off it, up, down or
//!   sideways (never the pick itself, never a [`brandon_tier`] pair).
//! - `Judge: climb <model>/<effort> [after <run>]`: a fix-round climb after
//!   a failed run, the lane's last dogfood row ([`last_row`]; an optional
//!   `after` naming another run by agent id or tool use id is denied).
//!   Allowed only at
//!   exactly the [`LADDER`] rung above that row's launched pair
//!   ([`next_rung`]), or, when that row was an explore below its pick, back
//!   at the pick (at most [`ladder_cap`] of it, fable/high under fable/max).
//!   A brandon-named row is a climb base like any row; a continue row is a
//!   base at its own launched pair but keeps the explore lineage of the last
//!   non-continue row before it, so a continuation below the pick of an
//!   explore below the pick climbs back to the pick, not one rung.
//!   fable/high is the top: a climb past it is denied, since
//!   fable/max is Brandon's call. Logged with `climb` true, counted neither
//!   as a routed launch nor as the explore, so a due explore stays due. A
//!   climb with no row for the lane (or no `Lane:` line) is denied.
//! - `Judge: continue [after <run>]`: a continuation after a context-cap or
//!   infra stop (not a failure). It needs a prior row for the lane, then is
//!   scored on its own brief (the leftover work) like a routed launch and
//!   admitted at the classifier's pick, below, at or above the continued
//!   run's pair (journal:TASK-JUDGE:0319/0320); a user-tier pick runs at the
//!   top ladder rung under it unless brandon-named. Never an explore, whether
//!   or not one is due: logged with `continue` true, `continued_pair` and
//!   `continued_run`, and never counted. A failed continuation climbs from
//!   its own row's pair (or back to the pick, the explore lineage above).
//!   With the classifier unavailable (no pick) it is admitted only at the
//!   continued run's pair. The word `after` matches in any case.
//!
//! A pick that is [`brandon_tier`] is suggested (`note op=judge` says to
//! ask Brandon) but a launch at it without `Judge: brandon-named` is
//! denied; the launch may run at the top ladder rung under it
//! ([`ladder_cap`], fable/high for fable/max).
//!
//! An explore or climb line whose pair does not parse is denied with the
//! expected form, never taken as the bare marker.
//!
//! A speculative brief (`Acceptance: none named`, with
//! `orchestrate.judge.speculative` set) is admitted only at the speculative
//! pair, logged with `speculative` true and never counted; an explore or
//! climb line on it is denied. With no pick (classifier unavailable) every
//! launch but a brandon-named or speculative one is denied, climbs included.
//!
//! While the explore launch is due, a launch at the pick is denied: the
//! denial names the explore rung, so a due explore is never skipped.

use std::path::{Path, PathBuf};

use crate::json::{self, Value};
use crate::taskjudge::{ClassPick, RulesMap, EFFORTS, MODELS};

/// The default explore cadence: every this-many routed builder launches,
/// one is the explore launch (`orchestrate.judge.explore_every`).
pub const EXPLORE_EVERY: usize = 5;

/// What a launch prompt's `Judge:` lines ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// No exception.
    Plain,
    /// `Judge: brandon-named <model>/<effort>`.
    BrandonNamed(String, String),
    /// `Judge: explore`.
    Explore,
    /// `Judge: explore <model>/<effort>`.
    ExploreAt(String, String),
    /// `Judge: climb <model>/<effort>`, with the run it follows when the
    /// line names one (`... after <agent id or tool use id>`).
    Climb(String, String, Option<String>),
    /// `Judge: continue`: a continuation after a context-cap or infra stop,
    /// re-judged on its own brief, with the run it follows when the line
    /// names one (`... after <agent id or tool use id>`).
    Continue(Option<String>),
    /// An explore or climb line whose pair does not parse: the marker
    /// (`explore` or `climb`) and the text after it.
    Malformed(&'static str, String),
}

impl Ask {
    /// The explore the line asks for, for the pick log: the named rung, or
    /// `rung-below` for the bare marker.
    pub fn explore(&self) -> Option<String> {
        match self {
            Ask::Explore => Some("rung-below".into()),
            Ask::ExploreAt(m, e) => Some(format!("{m}/{e}")),
            _ => None,
        }
    }

    /// The climb the line asks for, for the pick log.
    pub fn climb(&self) -> Option<String> {
        match self {
            Ask::Climb(m, e, _) => Some(format!("{m}/{e}")),
            _ => None,
        }
    }
}

/// The text after a `Judge:` line's `marker` word, when the line is that
/// marker (`explore`, not `explorer` or `explore-x`; `continue,` and `continue:` are the
/// marker), with leading punctuation trimmed.
fn after<'a>(rest: &'a str, marker: &str) -> Option<&'a str> {
    rest.strip_prefix(marker).filter(|p| !p.starts_with(|c: char| c.is_alphanumeric() || matches!(c, '-' | '_'))).map(|p| p.trim_start_matches([',', ':', ';']))
}

/// The exception a prompt's `Judge:` lines state. A brandon-named line
/// whose pair does not parse is plain (the launch is judged as usual); an
/// explore or climb line whose pair does not parse is [`Ask::Malformed`]
/// (the bare `Judge: explore` is the rung-below explore).
pub fn ask(prompt: &str) -> Ask {
    for l in prompt.lines().map(str::trim) {
        let Some(rest) = l.strip_prefix("Judge:").map(str::trim) else { continue };
        if let Some(p) = after(rest, "continue") {
            let mut words = p.split_whitespace();
            let run = words.next().is_some_and(|w| w.eq_ignore_ascii_case("after")).then(|| words.next()).flatten().map(|r| r.trim_end_matches(['.', ',', ';']).to_string());
            return Ask::Continue(run);
        }
        if let Some(p) = rest.strip_prefix("brandon-named") {
            let w = p.split_whitespace().next().unwrap_or("").trim_end_matches('.');
            if let Some((m, e)) = crate::taskjudge::parse_pick(&w.to_ascii_lowercase()) {
                return Ask::BrandonNamed(m, e);
            }
        } else if let Some((marker, p)) =
            after(rest, "explore").map(|p| ("explore", p)).or_else(|| after(rest, "climb").map(|p| ("climb", p)))
        {
            let text = p.trim().trim_end_matches('.').trim();
            let mut words = text.split_whitespace();
            let w = words.next().unwrap_or("");
            let run =
                words.next().is_some_and(|w| w.eq_ignore_ascii_case("after")).then(|| words.next()).flatten().map(|r| r.trim_end_matches(['.', ',', ';']).to_string());
            return match (crate::taskjudge::parse_pick(&w.to_ascii_lowercase()), marker) {
                (Some((m, e)), "explore") => Ask::ExploreAt(m, e),
                (Some((m, e)), _) => Ask::Climb(m, e, run),
                (None, "explore") if text.is_empty() => Ask::Explore,
                (None, _) => Ask::Malformed(marker, text.to_string()),
            };
        }
    }
    Ask::Plain
}

/// A launch's `model` and `effort` as `model/effort` tiers: the model by
/// the tier its name contains (`claude-opus-5-5` is opus), the effort as
/// written. `None` for either when missing or not a tier.
pub fn launched(model: &str, effort: &str) -> (Option<&'static str>, Option<&'static str>) {
    let m = model.to_ascii_lowercase();
    let e = effort.trim().to_ascii_lowercase();
    (MODELS.iter().copied().find(|t| m.contains(t)), EFFORTS.iter().copied().find(|t| *t == e))
}

// The unwrap_or(0) arms are unreachable for parsed pairs: every pair here
// comes from parse_pick or the tier constants.
fn rank(p: &(String, String)) -> (usize, usize) {
    (MODELS.iter().position(|m| *m == p.0).unwrap_or(0), EFFORTS.iter().position(|e| *e == p.1).unwrap_or(0))
}

/// The rungs the map can pick, cheapest first: its base entries and its
/// enabled overrides, by model tier then effort (the live map's
/// sonnet/medium < opus/medium < opus/high).
pub fn rungs(map: &RulesMap) -> Vec<(String, String)> {
    let mut r: Vec<(String, String)> = map.base.clone();
    r.extend([map.research.as_ref(), map.builder.as_ref()].into_iter().flatten().filter(|o| o.enabled).map(|o| o.pick.clone()));
    r.sort_by_key(rank);
    r.dedup();
    r
}

/// The climb ladder, cheapest first (Brandon 11-25, TASK-JUDGE 0305): a
/// `Judge: climb` runs at the rung one above the lane's failed run.
/// fable/high is the top rung; fable/max and every xhigh or max effort are
/// Brandon's call alone ([`brandon_tier`]), never reached by a climb or an
/// explore.
pub const LADDER: [(&str, &str); 7] = [
    ("haiku", "medium"),
    ("sonnet", "medium"),
    ("sonnet", "high"),
    ("opus", "medium"),
    ("opus", "high"),
    ("fable", "medium"),
    ("fable", "high"),
];

fn owned(r: &(&str, &str)) -> (String, String) {
    (r.0.to_string(), r.1.to_string())
}

/// The rung a climb from `p` runs at: the next ladder rung, or for a pair
/// off the ladder the lowest ladder rung above it (model, then effort).
/// `None` at or above the top rung.
pub fn next_rung(p: &(String, String)) -> Option<(String, String)> {
    match LADDER.iter().position(|r| r.0 == p.0 && r.1 == p.1) {
        Some(i) => LADDER.get(i + 1).map(owned),
        None => LADDER.iter().map(owned).find(|r| rank(r) > rank(p)),
    }
}

/// Whether a pair is Brandon's call alone (TASK-JUDGE 0306): an xhigh or
/// max effort, fable/max among them. The judge may suggest it; only a
/// `Judge: brandon-named` launch runs at it.
pub fn brandon_tier(p: &(String, String)) -> bool {
    matches!(p.1.as_str(), "xhigh" | "max")
}

/// The highest ladder rung at or below `p`: where a launch whose pick is
/// Brandon's call may run without his choice (fable/max: fable/high).
pub fn ladder_cap(p: &(String, String)) -> (String, String) {
    LADDER.iter().map(owned).rev().find(|r| rank(r) <= rank(p)).unwrap_or_else(|| owned(&LADDER[0]))
}

/// The suffix for a suggested pair that is Brandon's call, for the judge
/// line (`note op=judge`); `None` for any other pair.
pub fn suggest_suffix(p: &(String, String)) -> Option<String> {
    brandon_tier(p).then(|| format!(" {} needs Brandon's choice: ask him, then launch with Judge: brandon-named {}.", pair(p), pair(p)))
}

/// The denial clause for a launch at a pair that is Brandon's call.
fn brandon_call(p: &(String, String)) -> String {
    format!("{} is Brandon's call: ask Brandon, then launch with `Judge: brandon-named {}`", pair(p), pair(p))
}

/// The ladder as `a < b < c`.
fn ladder_list() -> String {
    LADDER.iter().map(|r| format!("{}/{}", r.0, r.1)).collect::<Vec<_>>().join(" < ")
}

/// The lane's last dogfood row: the run a climb follows and a continue
/// continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastRow {
    /// Its 1-based row number in the log.
    pub row: usize,
    /// The pair it launched at ([`row_pair`]).
    pub pair: Option<(String, String)>,
    /// Its agent id, when filled.
    pub agent_id: Option<String>,
    /// Its launch's tool use id.
    pub tool_use_id: Option<String>,
    /// Its pick, when it was an explore below that pick: a climb after it
    /// returns to the pick (TASK-JUDGE 0304), not one rung up.
    pub back_to: Option<(String, String)>,
}

/// The pair a log row launched at: `launched` when it is a pair, else the
/// pair its `explore` or `climb` string names (the legacy hand-written
/// rows), else its pick.
pub fn row_pair(v: &Value) -> Option<(String, String)> {
    let find = |s: &str| {
        s.split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | ';'))
            .find_map(|w| crate::taskjudge::parse_pick(&w.trim_end_matches(['.', ':']).to_ascii_lowercase()))
    };
    ["launched", "explore", "climb", "pick"].iter().find_map(|k| v[*k].as_str().and_then(find))
}

/// The last row of `lane` in the dogfood log text, if any. A brandon-named
/// row is a climb base like any row (0304: one rung above the failed run's
/// pair). A continue row keeps the explore lineage of the row it continues:
/// `back_to` is read from the last row before it that is not a continue.
pub fn last_row(log: &str, lane: &str) -> Option<LastRow> {
    let rows: Vec<(usize, Value)> = log
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .filter_map(|(i, l)| json::parse(l).ok().map(|v| (i, v)))
        .filter(|(_, v)| v["lane"].as_str() == Some(lane))
        .collect();
    let (i, v) = rows.last()?;
    let base = rows.iter().rev().map(|(_, v)| v).find(|v| v["continue"].as_bool() != Some(true)).unwrap_or(v);
    let s = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let pair = row_pair(v);
    let explore = base["explore"].as_bool() == Some(true) || base["explore"].as_str().is_some_and(|s| !s.trim().is_empty());
    let pick = base["pick"].as_str().and_then(crate::taskjudge::parse_pick);
    let back_to = pick.filter(|p| explore && pair.as_ref().is_some_and(|x| rank(x) < rank(p)));
    Some(LastRow { row: i + 1, pair, agent_id: s("agent_id"), tool_use_id: s("tool_use_id"), back_to })
}

/// The lane's last row for a launch, or why it cannot be had (the denial
/// clause): no `Lane:` line, no dogfood log, or no row for the lane
/// ([`last_row`]).
pub fn last_for(lane: Option<&str>, log: Option<&Path>) -> Result<LastRow, String> {
    let lane = lane.ok_or("the launch has no `Lane:` line, so the lane's last dogfood row cannot be found")?;
    let log = log.ok_or("there is no dogfood log (orchestrate.judge.log) to find the lane's last row in")?;
    let text = std::fs::read_to_string(log).unwrap_or_default();
    last_row(&text, lane).ok_or_else(|| format!("there is no prior row for lane {lane} in the dogfood log"))
}

/// The rung one below `pick`, if the map has one.
pub fn rung_below(map: &RulesMap, pick: &(String, String)) -> Option<(String, String)> {
    rungs(map).into_iter().rev().find(|r| rank(r) < rank(pick))
}

/// Whether the explore launch is due after `since` routed launches since
/// the last explore, with one explore every `every` routed launches.
pub fn due(since: usize, every: usize) -> bool {
    since + 1 >= every
}

/// The counter's state from a dogfood log: routed launches since the last
/// explore launch (brandon-named, speculative and climb rows are not
/// counted, nor is a hand-written row whose `explore` string starts with the
/// word `climb`, the legacy form of a climb row). An explore is `explore`
/// true or any other non-blank string: the legacy `"axis"` and
/// `"up: opus/high"` rows count as explores, since they ran at a pair off
/// the pick in place of the explore. A missed explore stays due, so the
/// next routed launch is the explore launch.
pub fn seed(log: &str) -> usize {
    let mut since = 0;
    for v in log.lines().filter_map(|l| json::parse(l).ok()) {
        let legacy_climb = v["explore"].as_str().is_some_and(|s| {
            let s = s.trim_start().to_ascii_lowercase();
            s.strip_prefix("climb").is_some_and(|r| !r.starts_with(|c: char| c.is_ascii_alphanumeric()))
        });
        if legacy_climb || ["brandon_named", "speculative", "climb", "continue"].iter().any(|k| v[*k].as_bool() == Some(true)) {
            continue;
        }
        let explore = v["explore"].as_bool() == Some(true) || v["explore"].as_str().is_some_and(|s| !s.trim().is_empty());
        since = if explore { 0 } else { since + 1 };
    }
    since
}

/// The explore counter, persisted beside the memory store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counter {
    path: PathBuf,
    /// Routed builder launches since the last explore launch.
    pub since: usize,
}

impl Counter {
    /// The counter file beside `store`, read; a missing file is seeded
    /// from the dogfood `log` (none or unreadable: zero).
    pub fn open(store: &Path, log: Option<&Path>) -> Counter {
        let path = store.parent().map_or_else(|| PathBuf::from("judge-explore.json"), |d| d.join("judge-explore.json"));
        let read = std::fs::read_to_string(&path).ok().and_then(|t| json::parse(&t).ok()).and_then(|v| v["since_explore"].as_u64());
        let since = match read {
            Some(n) => n as usize,
            None => log.and_then(|l| std::fs::read_to_string(l).ok()).map_or(0, |t| seed(&t)),
        };
        Counter { path, since }
    }

    /// Counts one admitted launch and writes the file.
    pub fn count(&mut self, a: &Admit) {
        if a.brandon_named || a.speculative || a.climb || a.continued.is_some() {
            return;
        }
        self.since = if a.explore { 0 } else { self.since + 1 };
        let body = json::object(32, |w| {
            w.field("since_explore", &self.since);
        });
        if let Err(e) = std::fs::write(&self.path, body) {
            eprintln!("atlas judge explore counter {}: {e}", self.path.display());
        }
    }
}

/// An admitted launch's facts for its log row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admit {
    /// The classifier's pick, when it scored.
    pub pick: Option<ClassPick>,
    /// `model/effort` as launched.
    pub launched: String,
    /// The explore launch.
    pub explore: bool,
    /// Brandon named the model.
    pub brandon_named: bool,
    /// A speculative brief admitted at the speculative pair.
    pub speculative: bool,
    /// A `Judge: climb` launch (not counted).
    pub climb: bool,
    /// A `Judge: continue` launch (re-judged on its own brief, not counted):
    /// the run it continues.
    pub continued: Option<Continued>,
    /// Why the classifier could not score, when it could not.
    pub unavailable: Option<String>,
    /// The context line for the launch.
    pub line: String,
}

/// The run a `Judge: continue` launch continues: the lane's last row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continued {
    /// The pair that run launched at (`model/effort`).
    pub pair: Option<String>,
    /// Its agent id, else its tool use id.
    pub run: Option<String>,
}

fn pair(p: &(String, String)) -> String {
    format!("{}/{}", p.0, p.1)
}

/// When the explore launch is next, for the advisory and denial text.
pub fn explore_text(map: &RulesMap, pick: &(String, String), since: usize, every: usize) -> String {
    match rung_below(map, pick) {
        _ if !due(since, every) => {
            let n = every - 1 - since;
            format!("explore launch not due ({n} routed launch{} until it)", if n == 1 { "" } else { "es" })
        }
        Some(r) => format!(
            "explore launch DUE: launch at {} with a `Judge: explore` line (or at another pair with `Judge: explore <model>/<effort>`)",
            pair(&r)
        ),
        None => format!(
            "explore launch due, but {} is the map's lowest rung: name another pair with `Judge: explore <model>/<effort>` (the map's rungs: {})",
            pair(pick),
            rung_list(map)
        ),
    }
}

/// The map's rungs as `a, b, c`.
fn rung_list(map: &RulesMap) -> String {
    rungs(map).iter().map(pair).collect::<Vec<_>>().join(", ")
}

/// The launch's standing beside its brief: the explore counter, the
/// cadence, the lane's last row and the speculative rule.
#[derive(Debug, Clone)]
pub struct Ctx<'a> {
    /// Routed launches since the last explore ([`Counter`]).
    pub since: usize,
    /// `orchestrate.judge.explore_every`.
    pub every: usize,
    /// The lane's last dogfood row, or why it cannot be had ([`last_for`]);
    /// read for a climb or a continue only.
    pub last: Result<LastRow, String>,
    /// The speculative rule's pair, when the rule is on and the brief says
    /// `Acceptance: none named`.
    pub speculative: Option<&'a (String, String)>,
}

/// The verdict on a builder launch: `Ok` with its row facts, or `Err` with
/// the denial text (which names the pick and the explore state).
/// `scored` is the classifier's pick or why it has none.
pub fn decide(scored: Result<ClassPick, String>, map: &RulesMap, ask: &Ask, model: &str, effort: &str, ctx: &Ctx) -> Result<Admit, String> {
    let (m, e) = launched(model, effort);
    let shown = format!(
        "{}/{}",
        m.unwrap_or(if model.is_empty() { "(none)" } else { model }),
        e.unwrap_or(if effort.is_empty() { "(none)" } else { effort })
    );
    let missing = if e.is_none() { " (a launch must pass effort)" } else { "" };
    let ok = |launch: &(String, String)| m == Some(launch.0.as_str()) && e == Some(launch.1.as_str());
    let (pick, unavailable) = match scored {
        Ok(p) => (Some(p), None),
        Err(why) => (None, Some(why)),
    };
    let why = unavailable.as_deref().map(|w| format!(" (classifier unavailable: {w})")).unwrap_or_default();
    if let Ask::BrandonNamed(bm, be) = ask {
        if m != Some(bm.as_str()) || e != Some(be.as_str()) {
            return Err(format!("atlas judge (enforced): the Judge line names {bm}/{be} (brandon-named) but the launch asks {shown}; pass model {bm:?} and effort {be:?}."));
        }
        let line = format!("atlas judge (enforced): brandon-named {bm}/{be}; logged, not counted{why}.");
        return Ok(Admit {
            pick,
            launched: shown,
            explore: false,
            brandon_named: true,
            speculative: false,
            climb: false,
            continued: None,
            unavailable,
            line,
        });
    }
    if let Ask::Malformed(marker, text) = ask {
        let written = if text.is_empty() { (*marker).to_string() } else { format!("{marker} {text}") };
        return Err(format!(
            "atlas judge (enforced): `Judge: {written}` names no model/effort pair; write it as `Judge: {marker} <model>/<effort>` (models {}, efforts {}){}.",
            MODELS.join(", "),
            EFFORTS.join(", "),
            if *marker == "explore" { ", or a bare `Judge: explore` for the rung below the pick" } else { "" }
        ));
    }
    // A continuation follows a prior run of the lane (its row names the run),
    // and is then judged on its own brief like a routed launch.
    let cont = match ask {
        Ask::Continue(run) => {
            let deny = |what: String| Err(format!("atlas judge (enforced): `Judge: continue` continues the lane's last run, {what}."));
            let last = match &ctx.last {
                Ok(l) => l,
                Err(why) => return deny(format!("but {why}")),
            };
            if let Some(r) = run.as_deref().filter(|r| last.agent_id.as_deref() != Some(*r) && last.tool_use_id.as_deref() != Some(*r)) {
                return deny(format!(
                    "but names run {r}, which is not the lane's last row (dogfood row {}, agent {})",
                    last.row,
                    last.agent_id.as_deref().unwrap_or("not yet filled")
                ));
            }
            Some(Continued { pair: last.pair.as_ref().map(pair), run: last.agent_id.clone().or_else(|| last.tool_use_id.clone()) })
        }
        _ => None,
    };
    if let Some(sp) = ctx.speculative {
        if let Some(marker) = ask.explore().map(|_| "explore").or(ask.climb().map(|_| "climb")) {
            return Err(format!(
                "atlas judge (enforced): a speculative brief (Acceptance: none named) is neither explored nor climbed; launch at {} without the {marker} line.",
                pair(sp)
            ));
        }
        if !ok(sp) {
            return Err(format!(
                "atlas judge (enforced): the brief says `Acceptance: none named`, so the speculative rule picks {}; the launch asks {shown}{missing}: pass model {:?} and effort {:?}.",
                pair(sp),
                sp.0,
                sp.1
            ));
        }
        let line = format!("atlas judge (enforced): speculative brief (Acceptance: none named) at {}; logged, not counted{why}.", pair(sp));
        return Ok(Admit {
            pick,
            launched: shown,
            explore: false,
            brandon_named: false,
            speculative: true,
            climb: false,
            continued: cont,
            unavailable,
            line,
        });
    }
    // With no pick a continuation cannot be re-judged: it keeps the continued
    // run's pair (the rule before re-judging) rather than being denied.
    if let (None, Some(c), Ok(last)) = (&pick, &cont, &ctx.last) {
        let deny = |what: String| {
            Err(format!("atlas judge (enforced): classifier unavailable: {}; `Judge: continue` keeps the pair of the lane's last run, {what}.", unavailable.as_deref().unwrap_or_default()))
        };
        let Some(lp) = &last.pair else {
            return deny(format!("but its row (dogfood row {}) names no model/effort pair", last.row));
        };
        if brandon_tier(lp) {
            return deny(format!("{} (dogfood row {}), and {}", pair(lp), last.row, brandon_call(lp)));
        }
        if !ok(lp) {
            return deny(format!("{} (dogfood row {}); the launch asks {shown}{missing}: pass model {:?} and effort {:?}", pair(lp), last.row, lp.0, lp.1));
        }
        let line = format!(
            "atlas judge (enforced): continue launch at {} (the lane's last run, dogfood row {}, kept: no pick to re-judge){why}; logged, not counted.",
            pair(lp),
            last.row
        );
        return Ok(Admit {
            pick,
            launched: shown,
            explore: false,
            brandon_named: false,
            speculative: false,
            climb: false,
            continued: Some(c.clone()),
            unavailable,
            line,
        });
    }
    let Some(p) = pick else {
        return Err(format!("atlas judge (enforced): classifier unavailable: {}; no pick, so no launch.", unavailable.unwrap_or_default()));
    };
    let want = (p.model.clone(), p.effort.clone());
    let head = format!("the pick is {} (demand score {}, {})", pair(&want), p.score, p.source);
    let below = rung_below(map, &want);
    let is_due = due(ctx.since, ctx.every);
    let expl = if cont.is_some() {
        "a continuation is not an explore (the counter is unchanged)".to_string()
    } else {
        explore_text(map, &want, ctx.since, ctx.every)
    };
    if let Ask::Climb(cm, ce, run) = ask {
        let c = (cm.clone(), ce.clone());
        let deny = |what: String| Err(format!("atlas judge (enforced): `Judge: climb {}` {what}; {head}.", pair(&c)));
        let last = match &ctx.last {
            Ok(l) => l,
            Err(why) => return deny(format!("follows a failed run, but {why}")),
        };
        let Some(from) = &last.pair else {
            return deny(format!("follows the lane's last run, but its row (dogfood row {}) names no model/effort pair", last.row));
        };
        // A failed explore below the pick climbs back to the pick, at most
        // the top rung under it (a pick that is Brandon's call), and only
        // when that is above the failed run.
        let back = last
            .back_to
            .as_ref()
            .map(|b| if brandon_tier(b) { ladder_cap(b) } else { b.clone() })
            .filter(|b| rank(b) > rank(from));
        let Some(next) = back.clone().or_else(|| next_rung(from)) else {
            return deny(format!(
                "follows {} (dogfood row {}): top rung reached; fable/max is Brandon's call (Judge: brandon-named): ask Brandon, then launch with `Judge: brandon-named fable/max`",
                pair(from),
                last.row
            ));
        };
        if let Some(r) = run.as_deref().filter(|r| last.agent_id.as_deref() != Some(*r) && last.tool_use_id.as_deref() != Some(*r)) {
            return deny(format!(
                "names run {r}, which is not the lane's last row (dogfood row {}, agent {}): a climb follows the lane's last run, and a climb from it runs at {}",
                last.row,
                last.agent_id.as_deref().unwrap_or("not yet filled"),
                pair(&next)
            ));
        }
        if brandon_tier(&c) {
            return deny(format!("is never a climb: {}; a climb from {} runs at {}", brandon_call(&c), pair(from), pair(&next)));
        }
        if let (true, Some(b)) = (c != next, back.as_ref().and(last.back_to.as_ref())) {
            let to = if brandon_tier(b) {
                format!("{}, the top rung under the pick {} (Brandon's call)", pair(&next), pair(b))
            } else {
                format!("the pick, {}", pair(&next))
            };
            return deny(format!(
                "is not the climb from the failed explore at {} (dogfood row {}): a failed explore below the pick climbs back to {to}",
                pair(from),
                last.row
            ));
        }
        if c != next {
            return deny(format!(
                "is not one rung above the failed run's {} (dogfood row {}): a climb runs at exactly {} (the ladder {})",
                pair(from),
                last.row,
                pair(&next),
                ladder_list()
            ));
        }
        if !ok(&c) {
            return Err(format!("atlas judge (enforced): the Judge line names climb {} but the launch asks {shown}{missing}; pass model {cm:?} and effort {ce:?}.", pair(&c)));
        }
        let line = format!("atlas judge (enforced): climb launch at {} ({head}); logged, not counted.", pair(&c));
        return Ok(Admit {
            pick: Some(p),
            launched: shown,
            explore: false,
            brandon_named: false,
            speculative: false,
            climb: true,
            continued: None,
            unavailable: None,
            line,
        });
    }
    if let Ask::ExploreAt(xm, xe) = ask {
        let x = (xm.clone(), xe.clone());
        let deny = |what: String| Err(format!("atlas judge (enforced): `Judge: explore {}` {what}; {head}.", pair(&x)));
        if brandon_tier(&x) {
            return deny(format!("is never an explore: {}", brandon_call(&x)));
        }
        if x == want {
            return deny("is the pick, not an explore: launch at it without the explore line".into());
        }
        if !is_due {
            return deny(format!("but {expl}. Launch at {} without the explore line", pair(&want)));
        }
        if !ok(&x) {
            return deny(format!("names {} but the launch asks {shown}{missing}: pass model {:?} and effort {:?}", pair(&x), x.0, x.1));
        }
        let line = format!("atlas judge (enforced): explore launch at {} (named pair; {head}); counter reset.", pair(&x));
        return Ok(Admit {
            pick: Some(p),
            launched: shown,
            explore: true,
            brandon_named: false,
            speculative: false,
            climb: false,
            continued: None,
            unavailable: None,
            line,
        });
    }
    if *ask == Ask::Explore {
        return match (&below, is_due) {
            (Some(r), true) if ok(r) => {
                let line = format!("atlas judge (enforced): explore launch at {} ({head}); counter reset.", pair(r));
                Ok(Admit { pick: Some(p), launched: shown, explore: true, brandon_named: false, speculative: false, climb: false, continued: None, unavailable: None, line })
            }
            (Some(r), true) => Err(format!("atlas judge (enforced): an explore launch runs at exactly {}, one rung below the pick, but the launch asks {shown}{missing}; {head}.", pair(r))),
            _ => Err(format!("atlas judge (enforced): `Judge: explore` but {expl}; {head}. Launch at {} without the explore line.", pair(&want))),
        };
    }
    // A continuation is never an explore: it neither waits for the cadence nor resets it.
    if let (Some(r), true, None) = (&below, is_due, &cont) {
        return Err(format!("atlas judge (enforced): the explore launch is due: launch at {} with a `Judge: explore` line (the launch asks {shown}); {head}.", pair(r)));
    }
    // A pick that is Brandon's call runs at the top ladder rung under it
    // unless he names it (TASK-JUDGE 0306).
    let tier = brandon_tier(&want);
    let run_at = if tier { ladder_cap(&want) } else { want.clone() };
    if tier && ok(&want) {
        return Err(format!(
            "atlas judge (enforced): {head}, and {}; without it launch at {}, the top rung under it.",
            brandon_call(&want),
            pair(&run_at)
        ));
    }
    if !ok(&run_at) {
        let note =
            if tier { format!(" ({}; without it the launch runs at the top rung under it)", brandon_call(&want)) } else { String::new() };
        return Err(format!(
            "atlas judge (enforced): the launch asks {shown}{missing}; {head}{note}: pass model {:?} and effort {:?}. {expl}.",
            run_at.0, run_at.1
        ));
    }
    let line = if let Some(c) = &cont {
        let from = c.pair.as_deref().unwrap_or("an unknown pair");
        let top = if tier { format!(", the top rung under the pick ({})", brandon_call(&want)) } else { String::new() };
        format!("atlas judge (enforced): continue launch at {}{top}, re-judged on its own brief (continues {from}), {head}; logged, not counted.", pair(&run_at))
    } else if tier {
        format!(
            "atlas judge (enforced): launch at {}, the top rung under the pick, {head} ({}); {expl}.",
            pair(&run_at),
            brandon_call(&want)
        )
    } else {
        format!("atlas judge (enforced): launch matches the pick, {head}; {expl}.")
    };
    Ok(Admit {
        pick: Some(p),
        launched: shown,
        explore: false,
        brandon_named: false,
        speculative: false,
        climb: false,
        continued: cont,
        unavailable: None,
        line,
    })
}

/// The dogfood log row for an admitted launch, in the log's own format:
/// `outcome` null until the review lands, `agent_id` null until the
/// agent starts (filled by [`fill_agent`]).
pub fn row(ts: &str, lane: &str, brief: &str, a: &Admit, tool_use_id: &str) -> String {
    let pick = a.pick.as_ref();
    json::object(384, |w| {
        w.field("ts", ts)
            .field("lane", lane)
            .field("brief", brief)
            .field("score", &pick.map(|p| p.score))
            .field("source", &pick.map(|p| p.source))
            .field("pick", &pick.map(|p| format!("{}/{}", p.model, p.effort)))
            .field("launched", a.launched.as_str())
            .field("explore", &a.explore)
            .field("brandon_named", &a.brandon_named)
            .field("speculative", &a.speculative)
            .field("climb", &a.climb)
            .field("continue", &a.continued.is_some())
            .field("continued_pair", &a.continued.as_ref().and_then(|c| c.pair.clone()))
            .field("continued_run", &a.continued.as_ref().and_then(|c| c.run.clone()))
            .field("classifier_unavailable", &a.unavailable)
            .field("judged_before_launch", &true)
            .field("outcome", &Value::Null)
            .field("agent_id", &Value::Null)
            .field("tool_use_id", tool_use_id);
    })
}

/// Fills `agent_id` in the log row of launch `tool_use_id`, when that row's
/// agent id is still null. The row text is otherwise unchanged.
pub fn fill_agent(log: &Path, tool_use_id: &str, agent_id: &str) -> std::io::Result<bool> {
    if tool_use_id.is_empty() || agent_id.is_empty() {
        return Ok(false);
    }
    let Ok(text) = std::fs::read_to_string(log) else { return Ok(false) };
    let from = format!("\"agent_id\":null,\"tool_use_id\":{}", json::to_string(tool_use_id));
    if !text.contains(&from) {
        return Ok(false);
    }
    let to = format!("\"agent_id\":{},\"tool_use_id\":{}", json::to_string(agent_id), json::to_string(tool_use_id));
    std::fs::write(log, text.replacen(&from, &to, 1))?;
    Ok(true)
}

/// The owed outcomes: each log row whose `outcome` is null, as
/// `dogfood row N LANE (outcome owed)`, oldest first.
pub fn owed(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .filter_map(|(i, l)| {
            let v = json::parse(l).ok()?;
            v["outcome"].is_null().then(|| format!("dogfood row {} {} (outcome owed)", i + 1, v["lane"].as_str().unwrap_or("?")))
        })
        .collect()
}
