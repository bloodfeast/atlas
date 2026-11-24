//! Open questions that keep up with themselves (#406).
//!
//! A question closes when a later entry carries an `answers` ref to its key
//! ([`crate::memory::OPEN_QUESTION_SQL`]). This module adds what keeps the
//! open set honest without a writer's habit:
//!
//! * auto-link: a `decision` or `answered` entry that answers an open
//!   question of its lane family (its Q-id such as `Q-RIVER-3` leading a
//!   line, `answers <key>`, or a ref to its key) gets the `answers` ref
//!   ([`links`]), and the reply lists what is still open ([`nudge`]);
//! * reopen: a later `reopens` ref ([`REOPENS`], from a `reopen` entry)
//!   undoes every earlier `answers` ref, so a false close is not permanent;
//! * age: each question's age, and a stale flag past
//!   `memory.question_stale_days` (default [`STALE_DAYS`]);
//! * sweep: questions that look settled, with the evidence for each
//!   ([`sweep`]); it never closes anything;
//! * defer: a `defer` entry parks a question `until` a time or the owner's
//!   next session, with the option work went ahead on ([`State`]).

use std::collections::{BTreeSet, HashMap};

use crate::memory::{MemoryStore, OPEN_QUESTION_SQL};
use crate::Result;

/// Days after which an open question is flagged as possibly stale.
pub const STALE_DAYS: i64 = 3;

/// The journal entry that defers a question.
pub const ENTRY: &str = "defer";

/// The ref kind a `defer` entry names its questions by.
pub const DEFERS: &str = "defers";

/// The ref kind that reopens a question: `answers` refs older than it no
/// longer close it ([`crate::memory::OPEN_QUESTION_SQL`]).
pub const REOPENS: &str = "reopens";

/// The `until:` value that waits for the owner's next session.
pub const NEXT_SESSION: &str = "next-session";

/// Most questions one nudge lists.
const NUDGE_MAX: usize = 5;

/// When a deferred question comes due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Until {
    /// At this time (epoch seconds), as written.
    At(i64, String),
    /// At the next main-session start after the defer.
    NextSession,
}

impl Until {
    /// Reads an `until:` value: `next-session`, or a UTC time
    /// (`YYYY-MM-DDTHH:MM[:SS]`, or a bare date for its midnight).
    pub fn parse(s: &str) -> Option<Until> {
        let s = s.trim();
        if s.eq_ignore_ascii_case(NEXT_SESSION) {
            return Some(Until::NextSession);
        }
        let full = if s.len() == 10 { format!("{s}T00:00") } else { s.to_string() };
        crate::checkpoint::epoch_secs(&full).map(|t| Until::At(t, s.to_string()))
    }

    fn text(&self) -> &str {
        match self {
            Until::At(_, s) => s,
            Until::NextSession => NEXT_SESSION,
        }
    }
}

/// A question's `defer` entry (the newest active one naming it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defer {
    /// The defer entry's key.
    pub key: String,
    /// When it was written (epoch seconds).
    pub at: i64,
    /// When it comes due.
    pub until: Until,
    /// The option work went ahead on, if one was named.
    pub proceeding_with: String,
}

/// Where an open question stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Open, not deferred.
    Open,
    /// Deferred and not yet due: out of the stale and idle counts.
    Deferred,
    /// Deferred and due: to ask now.
    Due,
}

/// An open question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Its lane.
    pub lane: String,
    /// Its key.
    pub key: String,
    /// Its title.
    pub title: String,
    /// Its body.
    pub body: String,
    /// When it was first written.
    pub created: String,
    /// Its defer, if it has one.
    pub defer: Option<Defer>,
}

impl Question {
    /// Whole days since it was written (`None` when a time is unreadable).
    pub fn age_days(&self, now: &str) -> Option<i64> {
        crate::queue::minutes_between(&self.created, now).map(|m| m / (24 * 60))
    }

    /// Its state at `now`, `last_start` being the newest main-session start
    /// (epoch seconds), if any.
    pub fn state(&self, now: &str, last_start: Option<i64>) -> State {
        let Some(d) = &self.defer else { return State::Open };
        let due = match d.until {
            Until::At(t, _) => crate::checkpoint::epoch_secs(now).is_some_and(|n| n >= t),
            Until::NextSession => last_start.is_some_and(|s| s > d.at),
        };
        if due { State::Due } else { State::Deferred }
    }

    /// Whether it is open, not deferred, and older than `days`.
    pub fn stale(&self, now: &str, last_start: Option<i64>, days: i64) -> bool {
        self.state(now, last_start) == State::Open && crate::queue::minutes_between(&self.created, now).is_some_and(|m| m > days * 24 * 60)
    }

    /// Its own Q-id: the one leading its title, else the one leading its
    /// body. A Q-id later in the title ("Q-X-2: follow-up to Q-X-1") is a
    /// mention, not its id.
    pub fn qid(&self) -> Option<String> {
        lead_qid(&self.title).or_else(|| self.body.lines().find(|l| !l.trim().is_empty()).and_then(lead_qid))
    }

    /// Whether `text` names it anywhere, by key or by its Q-id: the sweep's
    /// evidence, which closes nothing.
    pub fn named_in(&self, text: &str) -> bool {
        names_key(text, &self.key) || self.qid().is_some_and(|q| qids(text).contains(&q))
    }

    /// Whether an entry's `text` answers it: its Q-id leads a line or a
    /// bullet, or `answers <key>` / `answers: <key>` (or its Q-id) appears. A
    /// passing mention ("Q-X-1 settled; Q-X-2 still open") answers only the
    /// leading one.
    pub fn answered_in(&self, text: &str) -> bool {
        let qid = self.qid();
        let named = |t: &str| t.strip_prefix(self.key.as_str()).is_some_and(|r| !r.starts_with(|c: char| c.is_ascii_alphanumeric())) || qid.as_ref().is_some_and(|q| lead_qid(t).as_ref() == Some(q));
        text.lines().any(|l| qid.as_ref().is_some_and(|q| lead_qid(l).as_ref() == Some(q)))
            || text.match_indices("answers").any(|(i, m)| {
                let before = text[..i].chars().next_back();
                let rest = text[i + m.len()..].strip_prefix(':').unwrap_or(&text[i + m.len()..]);
                !before.is_some_and(|c| c.is_ascii_alphanumeric()) && rest.starts_with(' ') && named(rest.trim_start())
            })
    }

    /// Its options, `A) ...`, `B) ...` in order, from title and body.
    pub fn options(&self) -> Vec<String> {
        options(&format!("{}\n{}", self.title, self.body))
    }
}

/// Every open question, newest first, with its defer.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn open(store: &MemoryStore) -> Result<Vec<Question>> {
    let sql = format!(
        "SELECT r.scope, r.key, v.title, v.body, COALESCE((SELECT v1.time FROM versions v1 WHERE v1.rid = r.rid AND v1.seq = 1), r.created)
         FROM records r JOIN versions v ON v.vid = r.head WHERE {OPEN_QUESTION_SQL} ORDER BY r.rid DESC"
    );
    let conn = store.conn();
    let mut st = conn.prepare_cached(&sql)?;
    let rows: Vec<(String, String, String, String, String)> =
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<crate::db::Result<Vec<_>>>()?;
    let defers = defers(store)?;
    Ok(rows
        .into_iter()
        .map(|(lane, key, title, body, created)| {
            let defer = defers.get(&key).cloned();
            Question { lane, key, title, body, created, defer }
        })
        .collect())
}

/// The newest active defer naming each question, by question key.
fn defers(store: &MemoryStore) -> Result<HashMap<String, Defer>> {
    let mut st = store.conn().prepare_cached(
        "SELECT f.target, r.key, COALESCE((SELECT v1.time FROM versions v1 WHERE v1.rid = r.rid AND v1.seq = 1), r.created), v.meta
         FROM refs f JOIN records r ON r.rid = f.rid JOIN versions v ON v.vid = r.head
         WHERE f.kind = 'defers' AND r.status = 'active' ORDER BY r.rid",
    )?;
    let rows: Vec<(String, String, String, String)> =
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<crate::db::Result<Vec<_>>>()?;
    let mut out = HashMap::new();
    for (target, key, time, meta) in rows {
        let Some(until) = crate::memory::import::front_value(&meta, "until").and_then(Until::parse) else { continue };
        let proceeding_with = crate::memory::import::front_value(&meta, "proceeding_with").unwrap_or("").trim().to_string();
        let at = crate::checkpoint::epoch_secs(&time).unwrap_or(0);
        out.insert(target, Defer { key, at, until, proceeding_with });
    }
    Ok(out)
}

/// The newest main-session start the hooks recorded (epoch seconds):
/// SessionStart `startup`, `clear` or `resume` with no agent id, so never a
/// subagent's.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn last_start(store: &MemoryStore) -> Result<Option<i64>> {
    let conn = store.conn();
    let exists: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_starts')", [], |r| r.get(0))?;
    if !exists {
        return Ok(None);
    }
    let n: Option<i64> = conn.query_row("SELECT MAX(time) FROM session_starts", [], |r| r.get(0))?;
    Ok(n.map(|n| n / 1_000_000_000))
}

/// Whether two lanes are one family (a lane and its numbered sub-lanes).
pub fn same_family(a: &str, b: &str) -> bool {
    crate::lanes::parent_lane(a).eq_ignore_ascii_case(crate::lanes::parent_lane(b))
}

/// The Q-ids in `text`: tokens `Q-` then letters, digits and dashes, with a
/// digit among them (`Q-RIVER-3`, `Q-CE-5`, `Q-12`).
pub fn qids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
        let tok = tok.trim_matches('-');
        if let Some(rest) = tok.strip_prefix("Q-") {
            if !rest.is_empty() && rest.bytes().any(|b| b.is_ascii_digit()) && !out.iter().any(|o: &String| o == tok) {
                out.push(tok.to_string());
            }
        }
    }
    out
}

/// The Q-id leading `line`, past indentation, a bullet (`-`, `*`, `+`,
/// `>`, `1.`, `1)`) and emphasis (`**`, `__`, a backtick, `[`, `(`).
fn lead_qid(line: &str) -> Option<String> {
    let mut l = line.trim_start();
    if let Some(r) = l.strip_prefix(['-', '*', '+', '>']).filter(|r| r.starts_with(char::is_whitespace)) {
        l = r;
    } else {
        let digits = l.len() - l.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            if let Some(r) = l[digits..].strip_prefix(['.', ')']).filter(|r| r.starts_with(char::is_whitespace)) {
                l = r;
            }
        }
    }
    let l = l.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '_' | '`' | '[' | '('));
    let tok = l.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).next().unwrap_or("").trim_end_matches('-');
    qids(tok).into_iter().next().filter(|q| q == tok)
}

/// Whether `text` holds `key` as a whole key (not a prefix of a longer one).
fn names_key(text: &str, key: &str) -> bool {
    let part = |c: char| c.is_ascii_alphanumeric() || c == ':' || c == '_';
    text.match_indices(key).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + key.len()..].chars().next();
        !before.is_some_and(part) && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// Options labelled `A) `, `B) `, ... in order, each up to the next label
/// or line end.
pub fn options(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    for label in 'A'..='H' {
        let pat = format!("{label}) ");
        let found = text[from..].match_indices(&pat).map(|(i, _)| from + i).find(|&i| text[..i].chars().next_back().is_none_or(|c| c == ' ' || c == '\n' || c == '('));
        let Some(i) = found else { break };
        let start = i + pat.len();
        let next = format!("{}) ", char::from(label as u8 + 1));
        let end_line = text[start..].find('\n').map_or(text.len(), |e| start + e);
        let end = text[start..end_line].find(&format!(" {next}")).map_or(end_line, |e| start + e);
        out.push(text[start..end].trim().trim_end_matches(['.', ';']).to_string());
        from = end;
    }
    out
}

/// The questions an entry in `lane` with `text` and `refs` answers: each
/// open question of the lane family it answers ([`Question::answered_in`]),
/// or names as a ref's target (a `defers` or `reopens` ref aside). Questions
/// it already answers by ref are left out.
pub fn links(open: &[Question], lane: &str, text: &str, refs: &[(String, String)]) -> Vec<String> {
    open.iter()
        .filter(|q| same_family(&q.lane, lane))
        .filter(|q| !refs.iter().any(|(k, t)| k == "answers" && t == &q.key))
        .filter(|q| refs.iter().any(|(k, t)| t == &q.key && k != DEFERS && k != REOPENS) || q.answered_in(text))
        .map(|q| q.key.clone())
        .collect()
}

/// A question as one line: key, age, flags, title.
fn line(q: &Question, now: &str, last_start: Option<i64>, days: i64) -> String {
    let mut s = q.key.clone();
    if let Some(a) = q.age_days(now) {
        s.push_str(&format!(" {a}d"));
    }
    match q.state(now, last_start) {
        State::Deferred => s.push_str(&format!(" deferred until {}", q.defer.as_ref().map_or("", |d| d.until.text()))),
        State::Due => s.push_str(" due"),
        State::Open if q.stale(now, last_start, days) => s.push_str(" stale"),
        State::Open => {}
    }
    s.push_str(&format!(" \"{}\"", clip(&q.title, 80)));
    s
}

/// The reply's nudge: the lane family's open questions, one line each
/// (capped), and how to close one. Empty when none is open.
pub fn nudge(open: &[Question], lane: &str, now: &str, last_start: Option<i64>, days: i64) -> String {
    let mine: Vec<&Question> = open.iter().filter(|q| same_family(&q.lane, lane)).collect();
    if mine.is_empty() {
        return String::new();
    }
    let mut out = format!("open questions in {} ({}):\n", crate::lanes::parent_lane(lane), mine.len());
    for q in mine.iter().take(NUDGE_MAX) {
        out.push_str(&format!("  {}\n", line(q, now, last_start, days)));
    }
    if mine.len() > NUDGE_MAX {
        out.push_str(&format!("  +{} more: recall op=status\n", mine.len() - NUDGE_MAX));
    }
    out.push_str("close one: journal entry=answered refs=[\"answers:<key>\"]\n");
    out
}

/// The open-question rows `recall op=status` lists under its table.
pub fn status_lines(open: &[Question], now: &str, last_start: Option<i64>, days: i64, lanes: &[String]) -> String {
    let shown: Vec<&Question> = open.iter().filter(|q| lanes.iter().any(|l| l == &q.lane)).collect();
    if shown.is_empty() {
        return String::new();
    }
    let stale = shown.iter().filter(|q| q.stale(now, last_start, days)).count();
    let mut out = format!("open questions ({}, {stale} stale past {days}d):\n", shown.len());
    for q in shown {
        out.push_str(&format!("  {}\n", line(q, now, last_start, days)));
    }
    out
}

/// A question the sweep thinks is settled, with its evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The question's key.
    pub key: String,
    /// Its lane.
    pub lane: String,
    /// Its title.
    pub title: String,
    /// One line per signal that applies, each naming its evidence key.
    pub evidence: Vec<String>,
}

/// Words too common to be a question's key terms.
const STOP: &[&str] = &[
    "about", "after", "again", "should", "would", "could", "which", "where", "there", "their", "these", "those", "other", "every", "still", "being", "before",
    "until", "while", "using", "first", "takes", "needs", "today", "whether", "option", "options", "question", "decide", "decision", "answer",
];

/// The key terms of a title: distinct words of five letters or more, not
/// common ones, lowercased.
fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 5 && !w.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_lowercase)
        .filter(|w| !STOP.contains(&w.as_str()))
        .collect()
}

/// Whether `text` shares the question's key terms: at least three of them,
/// or all of them when it has two.
fn shares_terms(q_terms: &BTreeSet<String>, text: &str) -> bool {
    if q_terms.len() < 2 {
        return false;
    }
    let t = terms(text);
    q_terms.iter().filter(|w| t.contains(*w)).count() >= q_terms.len().min(3)
}

/// Whether a commit entry records a merged PR.
fn merged_pr(text: &str) -> bool {
    let l = text.to_lowercase();
    l.contains("merged") && (l.contains('#') || l.contains("pr:") || l.contains("pr "))
}

/// The open questions that look settled, with the evidence for each:
///
/// * a later `decision` or `answered` entry in the lane family, or a later
///   rule or decision record, naming its Q-id or key, or (entries only)
///   sharing its key terms;
/// * a later lane `commit` entry recording a merged PR;
/// * a lane family idle past `days` (not for a deferred question).
///
/// Nothing is closed: closing takes a journal entry with `answers` refs.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn sweep(store: &MemoryStore, open: &[Question], now: &str, last_start: Option<i64>, days: i64) -> Result<Vec<Candidate>> {
    let Some(oldest) = open.iter().map(|q| q.created.as_str()).min() else { return Ok(Vec::new()) };
    // Rulings written since the oldest open question.
    let mut st = store.conn().prepare_cached(
        "SELECT r.key, v.title, v.body, r.created FROM records r JOIN versions v ON v.vid = r.head
         WHERE r.kind IN ('rule', 'decision') AND r.status = 'active' AND r.created > ?1",
    )?;
    let rulings: Vec<(String, String, String, String)> =
        st.query_map([oldest], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<crate::db::Result<Vec<_>>>()?;
    let lanes = store.lanes()?;
    let mut families: HashMap<String, Vec<crate::memory::Head>> = HashMap::new();
    let after = |t: &str, q: &Question| match (crate::checkpoint::epoch_secs(t), crate::checkpoint::epoch_secs(&q.created)) {
        // Within one second, the millisecond text decides.
        (Some(a), Some(b)) if a != b => a > b,
        _ => t > q.created.as_str(),
    };
    let mut out = Vec::new();
    for q in open {
        let root = crate::lanes::parent_lane(&q.lane).to_string();
        if !families.contains_key(&root) {
            families.insert(root.clone(), store.lane_family(&root)?);
        }
        let heads = &families[&root];
        let q_terms = terms(&q.title);
        let mut ev = Vec::new();
        for h in heads.iter().filter(|h| h.key != q.key && h.status == "active" && after(&h.time, q)) {
            let text = format!("{}\n{}", h.title, h.body);
            match h.sub.as_str() {
                "decision" | "answered" => {
                    if q.named_in(&text) {
                        ev.push(format!("{} {} names it", h.sub, h.key));
                    } else if shares_terms(&q_terms, &text) {
                        ev.push(format!("{} {} shares its terms", h.sub, h.key));
                    }
                }
                "commit" if merged_pr(&text) => ev.push(format!("commit {} records a merged PR", h.key)),
                _ => {}
            }
        }
        for (key, title, body, created) in &rulings {
            if after(created, q) && q.named_in(&format!("{title}\n{body}")) {
                ev.push(format!("ruling {key} names it"));
            }
        }
        if q.state(now, last_start) == State::Open {
            let last = lanes.iter().filter(|l| same_family(&l.lane, &root)).map(|l| l.last_work.as_str()).max().unwrap_or("");
            if let Some(m) = crate::queue::minutes_between(last, now).filter(|m| *m > days * 24 * 60) {
                ev.push(format!("lane {root} idle {}d", m / (24 * 60)));
            }
        }
        if !ev.is_empty() {
            out.push(Candidate { key: q.key.clone(), lane: q.lane.clone(), title: q.title.clone(), evidence: ev });
        }
    }
    Ok(out)
}

/// `recall op=sweep` as text.
pub fn sweep_text(c: &[Candidate]) -> String {
    if c.is_empty() {
        return "no open question looks settled\n".into();
    }
    let mut out = format!("{} open question{} look settled; nothing is closed until answered:\n", c.len(), if c.len() == 1 { "" } else { "s" });
    for x in c {
        out.push_str(&format!("{} \"{}\"\n", x.key, clip(&x.title, 80)));
        for e in &x.evidence {
            out.push_str(&format!("  {e}\n"));
        }
    }
    let refs: Vec<String> = c.iter().map(|x| format!("\"answers:{}\"", x.key)).collect();
    out.push_str(&format!("close the settled ones in one call: journal lane=<lane> entry=answered text=\"<what settled them>\" refs=[{}]\n", refs.join(", ")));
    out
}

/// `recall op=deferred` as text: due questions first, each with its
/// options and the option work went ahead on, then those still waiting.
pub fn deferred_text(open: &[Question], now: &str, last_start: Option<i64>) -> String {
    let due: Vec<&Question> = open.iter().filter(|q| q.state(now, last_start) == State::Due).collect();
    let waiting: Vec<&Question> = open.iter().filter(|q| q.state(now, last_start) == State::Deferred).collect();
    if due.is_empty() && waiting.is_empty() {
        return "no deferred question\n".into();
    }
    let mut out = String::new();
    if !due.is_empty() {
        out.push_str(&format!("due ({}), to ask as multiple choice:\n", due.len()));
        for q in due {
            let d = q.defer.as_ref();
            out.push_str(&format!("{} \"{}\" (deferred by {}", q.key, clip(&q.title, 100), d.map_or("", |d| d.key.as_str())));
            if let Some(p) = d.map(|d| d.proceeding_with.as_str()).filter(|p| !p.is_empty()) {
                out.push_str(&format!(", proceeding with {p}"));
            }
            out.push_str(")\n");
            for (i, o) in q.options().iter().enumerate() {
                out.push_str(&format!("  {}) {}\n", char::from(b'A' + i as u8), clip(o, 160)));
            }
        }
        out.push_str("answer one: journal entry=answered refs=[\"answers:<key>\"]\n");
    }
    if !waiting.is_empty() {
        out.push_str(&format!("waiting ({}):\n", waiting.len()));
        for q in waiting {
            let until = q.defer.as_ref().map_or("", |d| d.until.text());
            out.push_str(&format!("{} \"{}\" until {until}\n", q.key, clip(&q.title, 100)));
        }
    }
    out
}

/// The due questions as one card line, empty when none is due.
pub fn due_line(open: &[Question], now: &str, last_start: Option<i64>, max: usize) -> String {
    let due: Vec<&Question> = open.iter().filter(|q| q.state(now, last_start) == State::Due).collect();
    if due.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = due
        .iter()
        .take(max)
        .map(|q| {
            let mut s = format!("{} \"{}\"", q.key, clip(&q.title, 60));
            let opts: Vec<String> = q.options().iter().map(|o| clip(o, 24)).collect();
            if !opts.is_empty() {
                s.push_str(&format!(" [{}]", opts.join(" | ")));
            }
            if let Some(p) = q.defer.as_ref().map(|d| d.proceeding_with.as_str()).filter(|p| !p.is_empty()) {
                s.push_str(&format!(" proceeding with {}", clip(p, 24)));
            }
            s
        })
        .collect();
    let more = if due.len() > max { format!(" (+{} more)", due.len() - max) } else { String::new() };
    format!("- due questions{more}, ask as multiple choice (recall op=deferred): {}", shown.join("; "))
}

/// `s` cut to `n` characters, with `...` when cut.
fn clip(s: &str, n: usize) -> String {
    let s = s.lines().next().unwrap_or("").trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n.saturating_sub(3)).collect::<String>().trim_end())
    }
}

/// Checks a `defer` entry before it is written: at least one `defers:` ref,
/// each to an open question; an `until:` meta line that reads. Returns the
/// questions it defers.
///
/// # Errors
///
/// A message naming what is missing or wrong.
pub fn check_defer(open: &[Question], meta: &str, refs: &[(String, String)]) -> std::result::Result<Vec<String>, String> {
    let targets: Vec<String> = refs.iter().filter(|(k, _)| k == DEFERS).map(|(_, t)| t.clone()).collect();
    if targets.is_empty() {
        return Err("a defer entry names its questions: refs=[\"defers:<question key>\"]".into());
    }
    for t in &targets {
        if !open.iter().any(|q| &q.key == t) {
            return Err(format!("defers:{t}: not an open question"));
        }
    }
    match crate::memory::import::front_value(meta, "until") {
        None => Err(format!("a defer entry needs a meta line `until: <time>` (UTC, YYYY-MM-DDTHH:MM or a date) or `until: {NEXT_SESSION}`; `proceeding_with: <option>` is optional")),
        Some(u) if Until::parse(u).is_none() => Err(format!("until: {u:?}: use a UTC time (YYYY-MM-DDTHH:MM or a date) or {NEXT_SESSION}")),
        Some(_) => Ok(targets),
    }
}

/// The journal entry that reopens a question closed by mistake.
pub const REOPEN_ENTRY: &str = "reopen";

/// A `reopen` entry's check: at least one `reopens` ref, each to a question
/// that is closed (`open` holds the open ones; `is_question` says whether a
/// key is a question at all).
///
/// # Errors
///
/// A message naming what is missing or wrong.
pub fn check_reopen(open: &[Question], refs: &[(String, String)], is_question: impl Fn(&str) -> bool) -> std::result::Result<(), String> {
    let targets: Vec<&String> = refs.iter().filter(|(k, _)| k == REOPENS).map(|(_, t)| t).collect();
    if targets.is_empty() {
        return Err(format!("a reopen entry names its questions: refs=[\"{REOPENS}:<question key>\"]"));
    }
    for t in targets {
        if open.iter().any(|q| &q.key == t) {
            return Err(format!("{REOPENS}:{t}: already open"));
        }
        if !is_question(t) {
            return Err(format!("{REOPENS}:{t}: not a question"));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "questions_tests.rs"]
mod tests;
