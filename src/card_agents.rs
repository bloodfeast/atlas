//! The card's live-agents line: each agent the card names (#316) with the
//! PR it is working on, and a parked builder kept on it (#327).
//!
//! The PR is resolved without the agent's help, in order: the lane's newest
//! `pr:` journal entry written after the launch (house rule 8: `pr:N
//! head:SHA`, in the text's first line or as meta lines); else the launch's
//! `Uses: worktree:NAME` -> that worktree's branch and head (read from its
//! `.git` files, no process) -> a PR event atlas filed for that head (the
//! pr-events store) -> one `gh pr list --head BRANCH` per (repo, branch),
//! all by the caller's deadline. A lookup that fails leaves the agent
//! without a PR and one short note says why. A PR the journal or a PR event
//! says is merged or closed is not shown.
//!
//! An entry counts for an agent only when it is the agent's own: its
//! `agent:` meta names it, or it names the agent's worktree or branch (a
//! branch only as `branch:`); one naming neither counts only on a lane no
//! other agent of the session shares. On a worktree reused by a later
//! agent, an entry tied by worktree or branch is the newest agent's
//! launched before it, and the line names a worktree once, for its newest
//! launch. An agent the host no longer lists as running stays on the line
//! only as `parked (own job running)`, and only while its own newest entry
//! since the launch is a `waiting:` note (#330's spelling). Decided from the
//! journal, never from the host's task status.
//!
//! The line holds at most [`crate::checkpoint::PER_LINE`] agents, running
//! ones first, then `+N more`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::memory::{Head, MemoryStore};
use crate::queue::meta_value;
use crate::Result;

/// The most all of one card's gh calls together may take; a caller with a
/// hook budget passes the smaller of this and the time left in it.
pub const GH_BUDGET: Duration = Duration::from_millis(1500);

/// The gh deadline at `now` within a hook whose own deadline is `hook`:
/// [`GH_BUDGET`] from now, never past the hook's (#331 r2 S5).
pub fn gh_deadline(now: Instant, hook: Instant) -> Instant {
    (now + GH_BUDGET).min(hook)
}

/// The mark of a parked agent.
pub const PARKED: &str = "parked (own job running)";

/// One open PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    /// Its number.
    pub number: u64,
    /// Its head sha (empty: unknown).
    pub head: String,
    /// Draft or ready; `None`: not known (a journal entry or an event).
    pub draft: Option<bool>,
}

impl Pr {
    /// `PR #N @head7 (ready|draft)`, each part only when known.
    pub fn show(&self) -> String {
        let mut s = format!("PR #{}", self.number);
        if !self.head.is_empty() {
            s.push_str(&format!(" @{}", &self.head[..self.head.len().min(7)]));
        }
        match self.draft {
            Some(true) => s.push_str(" (draft)"),
            Some(false) => s.push_str(" (ready)"),
            None => {}
        }
        s
    }
}

/// Why gh gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhFail {
    /// No `gh` on the path.
    Missing,
    /// gh is not logged in.
    NoAuth,
    /// The shared deadline passed.
    Timeout,
    /// Anything else, its first line.
    Failed(String),
}

impl GhFail {
    /// The note; `budget` is the time the deadline allowed.
    fn note(&self, budget: Duration) -> String {
        match self {
            GhFail::Missing => "gh not found".into(),
            GhFail::NoAuth => "gh not logged in".into(),
            // No time left, so no gh was started (#331 r4 nit 2).
            GhFail::Timeout if budget.is_zero() => "gh skipped: hook budget spent".into(),
            GhFail::Timeout => format!("gh over {:.1} s", budget.as_secs_f64()),
            GhFail::Failed(m) => format!("gh: {m}"),
        }
    }
}

/// Asks for the open PR of `branch`, run in the worktree `dir`, by the
/// shared `deadline`.
pub type GhList = dyn Fn(&Path, &str, Instant) -> std::result::Result<Option<Pr>, GhFail> + Send + Sync;

/// One agent the card may name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Agent {
    /// The agent id.
    pub id: String,
    /// Its lane.
    pub lane: String,
    /// When it was launched (store time; empty: unknown).
    pub launched: String,
    /// The `worktree:NAME` its brief declares (empty: none).
    pub worktree: String,
    /// That worktree's folder, from the ledger (empty: unknown).
    pub path: String,
    /// Whether the host lists it as running.
    pub running: bool,
}

/// The first line of an entry's text (its body, else its title), trimmed.
fn first_line(h: &Head) -> &str {
    h.body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_else(|| h.title.trim())
}

/// `key:value` words of a line such as `pr:330 head:abc1234 branch:x`,
/// `pr: 330` and `(worktree:x)` too.
fn words(line: &str) -> HashMap<String, String> {
    let line = line.replace(": ", ":");
    let bare = |c: char| ",;.()[]".contains(c);
    line.split_whitespace()
        .filter_map(|w| w.trim_matches(bare).split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim_matches(bare).to_string()))
        .collect()
}

/// A `key:` value of an entry: its meta line, else a word of its first line.
fn named(h: &Head, k: &str) -> String {
    meta_value(&h.meta, k).map(str::to_string).or_else(|| words(first_line(h)).remove(k)).unwrap_or_default()
}

/// A `pr:` value as the journal tool takes it (`N`, `#N`, `repo#N`,
/// `owner/repo#N`): the number and the repo it names (empty: none).
fn pr_ref(s: &str) -> Option<(u64, String)> {
    let s = s.trim();
    let (repo, n) = s.rsplit_once('#').unwrap_or(("", s));
    let n = n.parse::<u64>().ok().filter(|n| *n > 0)?;
    Some((n, repo.trim().to_ascii_lowercase()))
}

/// Whether a `pr:` entry's repo (`named`: `owner/repo`, `repo` or empty)
/// may be the agent's (`repo`: [`git_repo`], empty when unknown): yes
/// unless both are known as names and differ.
fn same_repo(named: &str, repo: &str) -> bool {
    let slug = repo.matches('/').count() == 1 && !repo.contains(':') && !repo.starts_with('/');
    if named.is_empty() || !slug {
        return true;
    }
    if named.contains('/') {
        named.eq_ignore_ascii_case(repo)
    } else {
        repo.rsplit('/').next().is_some_and(|r| r.eq_ignore_ascii_case(named))
    }
}

/// Whether words say merged or closed (#331 r4 S1). The `key:value`
/// words (`pr:N`, `head:SHA`, `branch:x`) and dashes (`--`) are skipped;
/// the first other word must be `merged`/`closed` and end its clause:
/// last, followed by punctuation, by `as`/`into`/... or by `#N` (`merged
/// as abc`, `Merged.`, `Merged #331`), not by an object (`merged origin/main`, which merges
/// main into the branch). `not merged` and `verdict: x` (no value) stay
/// open.
fn says_ended<'a>(ws: impl Iterator<Item = &'a str>) -> bool {
    let kv = |w: &str| w.split_once(':').is_some_and(|(k, v)| !k.is_empty() && !v.is_empty());
    let mut ws = ws.skip_while(|w| kv(w) || !w.chars().any(char::is_alphanumeric));
    let Some(w) = ws.next() else { return false };
    let word = w.trim_matches(|c: char| !c.is_ascii_alphabetic()).to_ascii_lowercase();
    if word != "merged" && word != "closed" {
        return false;
    }
    let clause_ends = w.ends_with(|c: char| ",;.:)]!".contains(c));
    let joins =
        |n: &str| matches!(n.to_ascii_lowercase().as_str(), "as" | "into" | "to" | "in" | "on" | "at" | "by" | "via" | "with" | "and");
    // `merged #331 as abc` names the PR itself.
    clause_ends || ws.next().is_none_or(|n| joins(n) || n.starts_with('#') || !n.chars().any(char::is_alphanumeric))
}

/// Whether a line's words from its `pr:` value on say merged or closed
/// (`pr:329 merged`, `pr: o/r#9 head:abc -- merged as def`).
fn after_pr(line: &str) -> bool {
    let line = line.to_ascii_lowercase().replace("pr: ", "pr:");
    says_ended(line.split_whitespace().skip_while(|w| !w.starts_with("pr:")))
}

/// A `pr:` entry: the PR, whether the entry says it is merged or closed,
/// and the repo it names (empty: none). Merged or closed counts only right
/// after the `pr:` value (`pr:329 merged`) or as the value of `state:` or
/// `verdict:`, so a hand-back line saying "Not merged." or "merged
/// origin/main" keeps its PR (#331 r3 S1). A one-line meta `pr: o/r#N
/// head:SHA` is read word by word (#331 r3 N3).
fn pr_entry(h: &Head) -> Option<(Pr, bool, String)> {
    let line = first_line(h);
    let meta = meta_value(&h.meta, "pr").unwrap_or("");
    let mut meta_words = meta.split_whitespace();
    let (n, repo) = meta_words
        .next()
        .and_then(pr_ref)
        .or_else(|| line.to_ascii_lowercase().starts_with("pr:").then(|| words(line).get("pr").and_then(|v| pr_ref(v))).flatten())?;
    let mut head = named(h, "head");
    if head.is_empty() {
        head = words(meta).remove("head").unwrap_or_default();
    }
    let head: String = head.chars().take_while(char::is_ascii_hexdigit).collect();
    let state = named(h, "state").to_ascii_lowercase();
    let draft = match state.as_str() {
        "draft" => Some(true),
        "ready" => Some(false),
        _ => None,
    };
    let ended = |w: &str| matches!(w.to_ascii_lowercase().trim_matches(|c: char| !c.is_ascii_alphabetic()), "merged" | "closed");
    // A meta `pr:` reads its own words after the value, and the text's
    // first words (`Merged.`, `Merged #331 as abc`).
    let by_meta = !meta.is_empty() && (says_ended(meta_words) || says_ended(line.split_whitespace()));
    let closed = ended(&state) || ended(&named(h, "verdict")) || by_meta || after_pr(line);
    Some((Pr { number: n, head, draft }, closed, repo))
}

/// How an entry is an agent's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tie {
    /// Not its own.
    No,
    /// Its `agent:` meta names it.
    Agent,
    /// It names the agent's worktree or branch.
    Place,
    /// It names nobody, on a lane no other agent shares.
    Lane,
}

/// Whether entry `h` is agent `a`'s own (`branch`: its worktree's branch,
/// empty when unknown): its `agent:` meta names it; or it names a's
/// worktree or branch, as a `worktree:`/`branch:` word or meta line, or
/// a's worktree as a word or path part of its first line (a branch only
/// as `branch:`: `main` is a common word); or, naming nobody, it is on a
/// lane no other agent shares (`shared` false).
fn owns(h: &Head, a: &Agent, branch: &str, shared: bool) -> Tie {
    let tie = |b: bool, t: Tie| if b { t } else { Tie::No };
    if let Some(id) = meta_value(&h.meta, "agent") {
        return tie(id == a.id, Tie::Agent);
    }
    let (wt, br) = (named(h, "worktree"), named(h, "branch"));
    let is_wt = |s: &str| !a.worktree.is_empty() && s.eq_ignore_ascii_case(&a.worktree);
    if !wt.is_empty() || !br.is_empty() {
        let last = |s: &str| s.replace('\\', "/").trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
        return tie(is_wt(&last(&wt)) || (!branch.is_empty() && br == branch), Tie::Place);
    }
    // `:` splits too: `waiting:worktree:x` is how `waiting: worktree:x` reads.
    let mentioned = first_line(h)
        .split(|c: char| c.is_whitespace() || "/\\:".contains(c))
        .map(|w| w.trim_matches(|c: char| ",;.()[]`'\"".contains(c)))
        .any(is_wt);
    if mentioned {
        Tie::Place
    } else {
        tie(!shared, Tie::Lane)
    }
}

/// Whether an entry is a `waiting: <job>` note.
fn waiting(h: &Head) -> bool {
    first_line(h).to_ascii_lowercase().starts_with("waiting:")
}

/// A worktree's branch (empty: detached) and head sha, from its `.git`
/// files; `None` when they cannot be read.
pub fn git_head(wt: &Path) -> Option<(String, String)> {
    let (dir, common) = git_dirs(wt)?;
    let head = std::fs::read_to_string(dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(r) = head.strip_prefix("ref:").map(str::trim) else { return Some((String::new(), head.to_string())) };
    let loose = |d: &Path| std::fs::read_to_string(d.join(r)).ok().map(|s| s.trim().to_string());
    let packed = || {
        std::fs::read_to_string(common.join("packed-refs"))
            .ok()?
            .lines()
            .find_map(|l| l.strip_suffix(r).and_then(|x| x.strip_suffix(' ')).map(str::to_string))
    };
    let sha = loose(&dir).or_else(|| loose(&common)).or_else(packed).unwrap_or_default();
    Some((r.strip_prefix("refs/heads/").unwrap_or(r).to_string(), sha))
}

/// A worktree's git dir and the repository's common dir.
fn git_dirs(wt: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot = wt.join(".git");
    let dir = if dot.is_dir() {
        dot
    } else {
        let t = std::fs::read_to_string(&dot).ok()?;
        let p = PathBuf::from(t.trim().strip_prefix("gitdir:")?.trim());
        if p.is_absolute() {
            p
        } else {
            wt.join(p)
        }
    };
    let common = std::fs::read_to_string(dir.join("commondir")).ok().map_or_else(|| dir.clone(), |c| dir.join(c.trim()));
    Some((dir, common))
}

/// Which repository a worktree is of: `owner/name` from its `origin`
/// remote's url, else its common dir (two worktrees of one clone share
/// it); empty when unreadable.
pub fn git_repo(wt: &Path) -> String {
    let Some((_, common)) = git_dirs(wt) else { return String::new() };
    let config = std::fs::read_to_string(common.join("config")).unwrap_or_default();
    let mut origin = false;
    for l in config.lines().map(str::trim) {
        if l.starts_with('[') {
            origin = l == "[remote \"origin\"]";
        } else if let Some(url) = l.strip_prefix("url").map(str::trim_start).and_then(|x| x.strip_prefix('=')).filter(|_| origin) {
            let url = url.trim().trim_end_matches('/').trim_end_matches(".git");
            let parts: Vec<&str> = url.rsplit(['/', ':']).take(2).collect();
            if parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) {
                return format!("{}/{}", parts[1], parts[0]).to_ascii_lowercase();
            }
        }
    }
    std::fs::canonicalize(&common).unwrap_or(common).to_string_lossy().to_ascii_lowercase()
}

/// The open PR whose head a PR event atlas filed names, among `events`
/// (notes with an `event:` meta line); a merged or closed one is not open.
fn event_pr(events: &[Head], sha: &str) -> Option<Pr> {
    if sha.len() < 7 {
        return None;
    }
    let ev = events
        .iter()
        .filter(|h| meta_value(&h.meta, "head").is_some_and(|x| x.len() >= 7 && (sha.starts_with(x) || x.starts_with(sha))))
        .max_by(|a, b| a.time.cmp(&b.time))?;
    if matches!(meta_value(&ev.meta, "verdict"), Some("merged" | "closed")) {
        return None;
    }
    let number = meta_value(&ev.meta, "pr")?.parse().ok()?;
    Some(Pr { number, head: sha.to_string(), draft: None })
}

/// Whether a PR event filed at or after `after` says PR `n` was merged or
/// closed (one naming another repo than `repo`, when both are known as
/// `owner/name`, does not count).
fn event_closed(events: &[Head], n: u64, repo: &str, after: &str) -> bool {
    let slug = (repo.matches('/').count() == 1 && !repo.contains(':') && !repo.starts_with('/')).then_some(repo);
    events.iter().any(|h| {
        h.time.as_str() >= after
            && meta_value(&h.meta, "pr").and_then(|p| p.trim_start_matches('#').parse::<u64>().ok()) == Some(n)
            && matches!(meta_value(&h.meta, "verdict"), Some("merged" | "closed"))
            && match (slug, meta_value(&h.meta, "repo")) {
                (Some(s), Some(r)) => r.eq_ignore_ascii_case(s),
                _ => true,
            }
    })
}

/// The PR events filed since the earliest launch among `agents`.
fn events_since(store: &MemoryStore, agents: &[Agent]) -> Result<Vec<Head>> {
    let floor = agents.iter().map(|x| x.launched.as_str()).filter(|t| !t.is_empty()).min().unwrap_or("");
    let all = if floor.is_empty() { store.active_entries_no_body("note")? } else { store.active_entries_since_no_body("note", floor)? };
    Ok(all.into_iter().filter(|h| h.meta.starts_with(crate::pr_events::EVENT_PREFIX)).collect())
}

/// One agent's row on the line while its PR is resolved.
struct Row {
    label: String,
    mark: &'static str,
    pr: Option<Pr>,
    /// The (repo, branch) to ask gh for (empty branch: none), and its folder.
    ask: (String, String),
    dir: PathBuf,
}

/// The live-agents line's items, running agents first (else in `agents`
/// order), at most [`crate::checkpoint::PER_LINE`] of them and then `+N
/// more`, and the note on what left a PR unresolved (empty: nothing). An
/// agent not running is kept only while parked. gh runs once per (repo,
/// branch), in parallel, each call ended at `deadline`; the caller passes
/// now + the smaller of [`GH_BUDGET`] and its hook budget's time left.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn items(store: &MemoryStore, agents: &[Agent], gh: &std::sync::Arc<GhList>, deadline: Instant) -> Result<(Vec<String>, String)> {
    let budget = deadline.saturating_duration_since(Instant::now());
    let mut lanes: HashMap<&str, Vec<Head>> = HashMap::new();
    for a in agents {
        if !lanes.contains_key(a.lane.as_str()) {
            lanes.insert(&a.lane, store.lane(&a.lane)?);
        }
    }
    let shared = |lane: &str| agents.iter().filter(|a| a.lane == lane).count() > 1;
    let mut notes: Vec<String> = Vec::new();
    let mut note = |n: String| {
        if !notes.contains(&n) {
            notes.push(n);
        }
    };
    let mut events: Option<Vec<Head>> = None;
    let mut rows: Vec<Row> = Vec::new();
    let mut more = 0usize;
    let mut order: Vec<&Agent> = agents.iter().collect();
    order.sort_by_key(|a| !a.running);
    let same_wt = |a: &Agent, b: &Agent| !a.worktree.is_empty() && a.worktree.eq_ignore_ascii_case(&b.worktree);
    // The agents the card may name, each with its own entries and mark.
    let mut named_agents = Vec::new();
    for a in order {
        let heads = &lanes[a.lane.as_str()];
        let place = (!a.path.is_empty()).then(|| PathBuf::from(&a.path));
        let git = place.as_deref().filter(|p| p.is_dir()).and_then(git_head);
        let branch = git.as_ref().map_or("", |g| g.0.as_str());
        let shared_lane = shared(&a.lane);
        // An entry tied by worktree or branch is the newest agent's launched
        // on that worktree before it (#331 r2 S1).
        let next = agents.iter().filter(|b| same_wt(a, b) && b.launched > a.launched).map(|b| b.launched.as_str()).min();
        // The agent's own entries since its launch.
        let own: Vec<&Head> = heads
            .iter()
            .filter(|h| {
                !a.launched.is_empty()
                    && h.time >= a.launched
                    && h.sub != "launch"
                    && match owns(h, a, branch, shared_lane) {
                        Tie::No => false,
                        Tie::Place => next.is_none_or(|n| h.time.as_str() < n),
                        Tie::Agent | Tie::Lane => true,
                    }
            })
            .collect();
        let mark = if a.running {
            ""
        } else if own.last().is_some_and(|h| waiting(h)) {
            PARKED
        } else {
            continue;
        };
        named_agents.push((a, place, git, own, mark, shared_lane));
    }
    // One row per worktree, and only for its newest launch: a later launch
    // on the worktree, named or finished, means this agent is done
    // (#331 r3 N1).
    let newest: Vec<bool> = named_agents.iter().map(|(a, ..)| !agents.iter().any(|b| same_wt(a, b) && b.launched > a.launched)).collect();
    for ((a, place, git, own, mark, shared_lane), newest) in named_agents.into_iter().zip(newest) {
        if !newest {
            continue;
        }
        if rows.len() == crate::checkpoint::PER_LINE {
            more += 1;
            continue;
        }
        let name = if !a.worktree.is_empty() {
            a.worktree.clone()
        } else if let Some(p) = place.as_deref().and_then(Path::file_name) {
            p.to_string_lossy().into_owned()
        } else {
            a.id.chars().take(7).collect()
        };
        let label = match (a.lane.is_empty(), shared_lane) {
            (true, _) => name.clone(),
            (false, true) => format!("{}/{name}", a.lane),
            (false, false) => a.lane.clone(),
        };
        let mut row = Row { label, mark, pr: None, ask: (String::new(), String::new()), dir: PathBuf::new() };
        let repo = place.as_deref().filter(|_| git.is_some()).map(git_repo).unwrap_or_default();
        // 1. The agent's newest own `pr:` entry, unless it, or a PR event
        // since, says the PR is merged or closed.
        let mine = own.iter().rev().find_map(|h| pr_entry(h).filter(|p| same_repo(&p.2, &repo)).map(|p| (*h, p)));
        if let Some((h, (pr, closed, _))) = mine {
            let closed = closed || {
                if events.is_none() {
                    events = Some(events_since(store, agents)?);
                }
                event_closed(events.as_deref().unwrap_or_default(), pr.number, &repo, &h.time)
            };
            if !closed {
                row.pr = Some(pr);
                rows.push(row);
                continue;
            }
        }
        // 2. The declared worktree's branch.
        let Some(dir) = place else {
            if !a.worktree.is_empty() {
                note(format!("worktree {} not in the ledger", a.worktree));
            }
            rows.push(row);
            continue;
        };
        let Some((branch, sha)) = git else {
            note(format!("worktree {name} gone"));
            rows.push(row);
            continue;
        };
        if branch.is_empty() {
            rows.push(row);
            continue;
        }
        // 3. A PR event atlas filed for that head.
        if events.is_none() {
            events = Some(events_since(store, agents)?);
        }
        row.pr = event_pr(events.as_deref().unwrap_or_default(), &sha);
        if row.pr.is_none() {
            row.ask = (repo, branch);
            row.dir = dir;
        }
        rows.push(row);
    }
    // 4. gh, once per (repo, branch), in parallel by one deadline.
    let mut asks: Vec<((String, String), PathBuf)> = Vec::new();
    for r in &rows {
        if !r.ask.1.is_empty() && !asks.iter().any(|(x, _)| *x == r.ask) {
            asks.push((r.ask.clone(), r.dir.clone()));
        }
    }
    // Each call on its own thread, waited for only until the deadline: a
    // call past it is left to finish (or be killed by its own deadline) and
    // counts as timed out.
    let (tx, rx) = std::sync::mpsc::channel();
    for (i, ((_, b), d)) in asks.iter().enumerate() {
        let (tx, gh, b, d) = (tx.clone(), std::sync::Arc::clone(gh), b.clone(), d.clone());
        std::thread::spawn(move || {
            let _ = tx.send((i, gh(&d, &b, deadline)));
        });
    }
    drop(tx);
    let mut answers: Vec<Option<std::result::Result<Option<Pr>, GhFail>>> = vec![None; asks.len()];
    while answers.iter().any(Option::is_none) {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((i, r)) => answers[i] = Some(r),
            Err(_) => break,
        }
    }
    let mut got: HashMap<&(String, String), Option<Pr>> = HashMap::new();
    for ((k, _), r) in asks.iter().zip(answers) {
        match r.unwrap_or(Err(GhFail::Timeout)) {
            Ok(p) => {
                got.insert(k, p);
            }
            Err(e) => note(e.note(budget)),
        }
    }
    let mut out: Vec<String> = rows
        .into_iter()
        .map(|r| {
            let pr = r.pr.or_else(|| got.get(&r.ask).cloned().flatten());
            let mut s = r.label;
            for part in [r.mark.to_string(), pr.map(|p| p.show()).unwrap_or_default()] {
                if !part.is_empty() {
                    s.push(' ');
                    s.push_str(&part);
                }
            }
            s
        })
        .collect();
    if more > 0 {
        out.push(format!("+{more} more"));
    }
    Ok((out, notes.join("; ")))
}

/// The [`GhList`] the hooks start with: the `gh` CLI ([`gh_pr_list`]), or,
/// in a build with test seams, an answer of no PR, so no test spawns a real
/// gh unless it sets one.
pub fn default_gh() -> std::sync::Arc<GhList> {
    if cfg!(any(test, feature = "test-seams")) {
        std::sync::Arc::new(|_: &Path, _: &str, _| Ok(None))
    } else {
        std::sync::Arc::new(gh_pr_list)
    }
}

/// Whether `branch` is a plain ref name gh may be given: not empty, not an
/// option, and only letters, digits and `/._-+`.
fn plain_branch(branch: &str) -> bool {
    !branch.is_empty() && !branch.starts_with('-') && branch.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c))
}

/// [`GhList`] by the `gh` CLI: `gh pr list --head=BRANCH --state open`, run
/// in the worktree so gh reads its remote, ended at `deadline` with every
/// process it started (its own group or job: a git it runs goes with it).
pub fn gh_pr_list(dir: &Path, branch: &str, deadline: Instant) -> std::result::Result<Option<Pr>, GhFail> {
    use std::process::{Command, Stdio};
    if !plain_branch(branch) {
        return Err(GhFail::Failed("branch name not plain".into()));
    }
    // A spent deadline spawns nothing (#331 r2 S5).
    if Instant::now() >= deadline {
        return Err(GhFail::Timeout);
    }
    let mut c = Command::new("gh");
    c.current_dir(dir)
        .args(["pr", "list", &format!("--head={branch}"), "--state", "open", "--json", "number,headRefOid,isDraft", "--limit", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::platform::process::own_group(&mut c);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // own_group's new process group, and no console window.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(GhFail::Missing),
        Err(e) => return Err(GhFail::Failed(e.to_string())),
    };
    let tie = crate::platform::process::tie(&child);
    // The answer is a few bytes, so the pipes cannot fill before the exit.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                crate::platform::process::terminate_tree(child.id());
                let _ = child.kill();
                let _ = child.wait();
                tie.end();
                return Err(GhFail::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(GhFail::Failed(e.to_string())),
        }
    }
    let o = child.wait_with_output().map_err(|e| GhFail::Failed(e.to_string()))?;
    if !o.status.success() {
        return Err(gh_error(&String::from_utf8_lossy(&o.stderr)));
    }
    parse_pr_list(&String::from_utf8_lossy(&o.stdout))
}

/// What a failed gh's stderr says.
pub fn gh_error(stderr: &str) -> GhFail {
    let l = stderr.to_ascii_lowercase();
    if l.contains("gh auth login") || l.contains("not logged in") || l.contains("authentication") {
        return GhFail::NoAuth;
    }
    if l.contains("error connecting") || l.contains("could not resolve") || l.contains("dial tcp") {
        return GhFail::Failed("no network".into());
    }
    GhFail::Failed(stderr.lines().map(str::trim).find(|x| !x.is_empty()).unwrap_or("failed").chars().take(60).collect())
}

/// The first PR of `gh pr list --json number,headRefOid,isDraft` output.
pub fn parse_pr_list(stdout: &str) -> std::result::Result<Option<Pr>, GhFail> {
    use crate::json::Value;
    let v = crate::json::parse(stdout.trim()).map_err(|e| GhFail::Failed(format!("unreadable answer: {e}")))?;
    let Some(first) = v.as_array().and_then(|a| a.first()) else { return Ok(None) };
    let Some(number) = first.get("number").and_then(Value::as_u64).filter(|n| *n >= 1) else { return Ok(None) };
    let head = first.get("headRefOid").and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Some(Pr { number, head, draft: first.get("isDraft").and_then(Value::as_bool) }))
}

#[cfg(test)]
#[path = "card_agents_tests.rs"]
mod tests;
