//! The enforced dogfood mode of the task judge (`orchestrate.judge.mode:
//! "enforced"`, #412). At an admitted builder launch the hook scores the
//! brief with the classifier ([`crate::taskjudge::classify_with`], the
//! scoring `note op=judge` does), denies a launch whose `model` or `effort`
//! is off the pick, owns the every-5th explore counter and appends one row
//! per admitted launch to the dogfood log (`orchestrate.judge.log`).
//!
//! Exceptions, each a prompt line:
//! - `Judge: brandon-named <model>/<effort>`: Brandon named the model; the
//!   launch is allowed at that pair, logged with `brandon_named` true and
//!   never counted.
//! - `Judge: explore`: allowed only when the explore launch is due, and
//!   only at exactly one rung below the pick ([`rungs`]).
//!
//! While the explore launch is due, a launch at the pick is denied: the
//! denial names the explore rung, so a due explore is never skipped.

use std::path::{Path, PathBuf};

use crate::json::{self, Value};
use crate::taskjudge::{ClassPick, RulesMap, EFFORTS, MODELS};

/// Every this-many routed builder launches, one runs a rung below the pick.
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
}

/// The exception a prompt's `Judge:` lines state. A brandon-named line
/// whose pair does not parse is plain (the launch is judged as usual).
pub fn ask(prompt: &str) -> Ask {
    for l in prompt.lines().map(str::trim) {
        let Some(rest) = l.strip_prefix("Judge:").map(str::trim) else { continue };
        if let Some(p) = rest.strip_prefix("brandon-named") {
            let w = p.split_whitespace().next().unwrap_or("").trim_end_matches('.');
            if let Some((m, e)) = crate::taskjudge::parse_pick(&w.to_ascii_lowercase()) {
                return Ask::BrandonNamed(m, e);
            }
        } else if rest.split_whitespace().next().map(|w| w.trim_end_matches('.')) == Some("explore") {
            return Ask::Explore;
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

/// The rung one below `pick`, if the map has one.
pub fn rung_below(map: &RulesMap, pick: &(String, String)) -> Option<(String, String)> {
    rungs(map).into_iter().rev().find(|r| rank(r) < rank(pick))
}

/// Whether the explore launch is due after `since` routed launches since
/// the last explore.
pub fn due(since: usize) -> bool {
    since + 1 >= EXPLORE_EVERY
}

/// The counter's state from a dogfood log: routed launches since the last
/// explore launch (brandon-named rows are not counted). A missed explore
/// stays due, so the next routed launch is the explore launch.
pub fn seed(log: &str) -> usize {
    let mut since = 0;
    for v in log.lines().filter_map(|l| json::parse(l).ok()) {
        if v["brandon_named"].as_bool() == Some(true) {
            continue;
        }
        since = if v["explore"].as_bool() == Some(true) { 0 } else { since + 1 };
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
        if a.brandon_named {
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
    /// The context line for the launch.
    pub line: String,
}

fn pair(p: &(String, String)) -> String {
    format!("{}/{}", p.0, p.1)
}

/// When the explore launch is next, for the advisory and denial text.
pub fn explore_text(map: &RulesMap, pick: &(String, String), since: usize) -> String {
    match rung_below(map, pick) {
        _ if !due(since) => {
            let n = EXPLORE_EVERY - 1 - since;
            format!("explore launch not due ({n} routed launch{} until it)", if n == 1 { "" } else { "es" })
        }
        Some(r) => format!("explore launch DUE: launch at {} with a `Judge: explore` line", pair(&r)),
        None => format!("explore launch due, but {} is the map's lowest rung", pair(pick)),
    }
}

/// The verdict on a builder launch: `Ok` with its row facts, or `Err` with
/// the denial text (which names the pick and the explore state).
pub fn decide(pick: Option<ClassPick>, map: &RulesMap, ask: &Ask, model: &str, effort: &str, since: usize) -> Result<Admit, String> {
    let (m, e) = launched(model, effort);
    let shown = format!("{}/{}", m.unwrap_or(if model.is_empty() { "(none)" } else { model }), e.unwrap_or(if effort.is_empty() { "(none)" } else { effort }));
    if let Ask::BrandonNamed(bm, be) = ask {
        if m != Some(bm.as_str()) || e != Some(be.as_str()) {
            return Err(format!("atlas judge (enforced): the Judge line names {bm}/{be} (brandon-named) but the launch asks {shown}; pass model {bm:?} and effort {be:?}."));
        }
        let line = format!("atlas judge (enforced): brandon-named {bm}/{be}; logged, not counted.");
        return Ok(Admit { pick, launched: shown, explore: false, brandon_named: true, line });
    }
    let Some(p) = pick else {
        return Err("atlas judge (enforced): a builder launch must name a readable BRIEF*.md file the classifier can score (or a `Judge: brandon-named <model>/<effort>` line); no pick, so no launch.".into());
    };
    let want = (p.model.clone(), p.effort.clone());
    let head = format!("the pick is {} (demand score {}, {})", pair(&want), p.score, p.source);
    let below = rung_below(map, &want);
    let is_due = due(since);
    let expl = explore_text(map, &want, since);
    let ok = |launch: &(String, String)| m == Some(launch.0.as_str()) && e == Some(launch.1.as_str());
    let missing = if e.is_none() { " (a launch must pass effort)" } else { "" };
    if *ask == Ask::Explore {
        return match (&below, is_due) {
            (Some(r), true) if ok(r) => {
                let line = format!("atlas judge (enforced): explore launch at {} ({head}); counter reset.", pair(r));
                Ok(Admit { pick: Some(p), launched: shown, explore: true, brandon_named: false, line })
            }
            (Some(r), true) => Err(format!("atlas judge (enforced): an explore launch runs at exactly {}, one rung below the pick, but the launch asks {shown}{missing}; {head}.", pair(r))),
            _ => Err(format!("atlas judge (enforced): `Judge: explore` but {expl}; {head}. Launch at {} without the explore line.", pair(&want))),
        };
    }
    if let (Some(r), true) = (&below, is_due) {
        return Err(format!("atlas judge (enforced): the explore launch is due: launch at {} with a `Judge: explore` line (the launch asks {shown}); {head}.", pair(r)));
    }
    if !ok(&want) {
        return Err(format!("atlas judge (enforced): the launch asks {shown}{missing}; {head}: pass model {:?} and effort {:?}. {expl}.", want.0, want.1));
    }
    let line = format!("atlas judge (enforced): launch matches the pick, {head}; {expl}.");
    Ok(Admit { pick: Some(p), launched: shown, explore: false, brandon_named: false, line })
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
