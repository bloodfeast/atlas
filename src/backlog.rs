//! `atlas backlog`: combs the store for ruled work that has no live lane, and
//! proposes it. It never reorders: the written order (`order:`, `after:`)
//! wins until the user or the orchestrator accepts a change (journal
//! ATLAS-V03:0416).
//!
//! ```text
//! atlas backlog [--all] [--as-of TIME] [--json] [--transcript PATH]
//! atlas backlog dismiss ID --because KEY
//! ```
//!
//! The comb reads store records only: the queue's items (placed work), its
//! marks and retirements, journal brief, commit, decision and answered
//! entries, INTENT items (`intent:r`, kind rule, and their section blocks),
//! `ruling:` records and their `defines:` IDs, plus vault notes whose
//! `status` starts with "queued" from the index database. It never reads
//! INTENT.md.
//!
//! - **Live** (ruled 2026-11-10, journal ATLAS-V03:0420): a `brief` or
//!   `commit` entry within [`Settings::live_days`] in the item's lane, or
//!   naming its ID as a whole token, or a running agent whose brief declares
//!   `Lane: ID` or names the ID. A queue entry, a note, a decision, a
//!   question, a measurement, a review, a `next` or a handoff is not live.
//! - **Done**: a `done:` mark, a retirement (started, done, dropped), a
//!   commit whose title says merged (or landed, deployed, shipped) in the
//!   item's lane or naming it, or a later closing ruling.
//! - **Aging** (the internal [`State::Overdue`] until BC-2): placed, not
//!   live, not done, and one of: asked again on or after its newest
//!   placement; its written-order turn came [`Settings::turn_days`] ago;
//!   placed more than [`Settings::overdue_days`] ago. It is shown with its
//!   days only and is not a call to pull the item forward.
//! - **Joins** are exact: an ID as a whole token (`TRACKS-2` and `UNTRACKS`
//!   do not name `TRACKS`), a record key, or the item's own lane. There are
//!   no topic joins (see [`crate::memory::rules`]).
//!
//! `--as-of T` reads every record at its newest version at or before T, and
//! ignores records, retirements and marks written after it. An INTENT item
//! is dated by the date it states, not by its import time.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::ExitCode;

use crate::checkpoint::epoch_secs;
use crate::json::Value;
use crate::memory::{Draft, MemoryStore};
use crate::Result;

/// The meta key of a dismissal note.
const DISMISS: &str = "dismiss";

const DAY: i64 = 86_400;

/// The largest day window a `backlog.*_days` setting may hold: 100 years.
const MAX_DAYS: i64 = 36_500;

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// The comb's weights, windows and cue lists: data, read from the
/// configuration's `backlog` section. The defaults are the dry run's
/// (Z:/scratch/v03release/BACKLOG_COMB_DESIGN.md, section 4).
#[derive(Debug, Clone)]
pub struct Settings {
    /// Rank per time asked.
    pub w_ask: i64,
    /// Rank when the item's written-order turn came.
    pub w_turn: i64,
    /// Rank for a front-of-queue ruling.
    pub w_front: i64,
    /// Rank per dated ruling past the first.
    pub w_ruling: i64,
    /// Rank for a placed item.
    pub w_placed: i64,
    /// Age counts one rank a day up to this many days.
    pub age_cap_days: i64,
    /// A brief or commit is live this many days (resume.rs LIVE_DAYS).
    pub live_days: i64,
    /// A turn that came this many days ago is shown as aging.
    pub turn_days: i64,
    /// A placed item older than this is shown as aging.
    pub overdue_days: i64,
    /// An unplaced, never-asked ID older than this is stale.
    pub stale_days: i64,
    /// Token Jaccard at which an ask is a near-duplicate of an earlier one.
    pub near_duplicate: f64,
    /// Phrases that ask again ("still waiting").
    pub ask: Vec<String>,
    /// Phrases that put an item at the front.
    pub front: Vec<String>,
    /// Phrases that place work (a ruling about it).
    pub place: Vec<String>,
    /// `Y <phrase> X`: Y folds into X.
    pub fold: Vec<String>,
    /// `X <phrase> Y`: Y folds into X.
    pub include: Vec<String>,
    /// `Y <phrase> ...`: Y waits, deferred.
    pub defer: Vec<String>,
    /// `Y <phrase>`: Y is closed (dropped, killed, merged).
    pub close: Vec<String>,
    /// Words in a commit title that mean the work landed.
    pub done: Vec<String>,
    /// ID-shaped words that are never work.
    pub stop: Vec<String>,
    /// The asker: the name that leads a line in the user's own words
    /// (`- 2026-11-04 Name:`, `Name 2026-11-10:`). Unset (the default), no
    /// lead is the asker's, so only an idea can count as an ask.
    pub asker: Option<String>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            w_ask: 40,
            w_turn: 15,
            w_front: 15,
            w_ruling: 5,
            w_placed: 5,
            age_cap_days: 30,
            live_days: 7,
            turn_days: 2,
            overdue_days: 14,
            stale_days: 30,
            near_duplicate: 0.5,
            ask: strings(&["still waiting", "fell out of", "fallen out", "fallen off", "fell off", "dropped out of", "second time", "third time", "never started", "asked again", "re-ask", "reask"]),
            front: strings(&["front of", "top of the", "next free slot", "next free builder slot", "takes the next", "before everything", "urgent", "head of"]),
            place: strings(&["queue", "queued", "next free", "front", "top of", "goes after", "goes right after", "goes before", "takes the next", "slot", "start", "build", "lane"]),
            fold: strings(&["folds into", "fold into", "folded into", "rides", "pulled into"]),
            include: strings(&["includes"]),
            defer: strings(&["stays in the", "waits for", "not now", "on hold", "deferred", "parked"]),
            close: strings(&["dropped", "killed", "superseded", "merged", "shipped", "not built", "won't be built"]),
            done: strings(&["merged", "landed", "deployed", "shipped"]),
            stop: strings(&[
                "UTC", "ASCII", "GPU", "CPU", "RTX", "NVIDIA", "OPEN", "MERGE", "STANDING", "README", "INTENT", "CLAUDE", "TODO", "JSON", "HTML", "WSL", "DLSS", "BLAS", "TLAS", "HDR", "SDR", "LOD", "API", "SDK", "FSR", "OAUTH",
                "HTTP", "STOP", "NOTE", "WARN", "MISS", "PASS", "FAIL", "DONE", "HOLD", "RESTART", "CHANGELOG", "ROADMAP", "VRAM", "SIMD", "AABB", "BVH", "WGSL", "HLSL", "SPIRV", "MEMORY", "RAM", "LOST", "IDEAS", "REVIEWS",
                "LAPSED", "ATLAS", "WORK", "QUEUE", "TASKS", "BEFORE", "AFTER", "FRONT", "NEXT", "ONLY", "NEVER", "ALWAYS", "FIRST", "THEN", "BOTH", "NONE", "LIGHT", "SHUTTER", "MORNING", "ALSO", "WITH", "FROM",
                "THIS", "THAT", "WHEN", "WHAT", "ORCHESTRATOR", "VULKAN", "OPTIX", "CUDA", "BEVY", "MESHY", "BLENDER", "LEONARDO", "GITHUB", "TERRAIN", "WATER", "RAIN", "MUD", "SHIP", "SKY", "ABBA", "RULED", "REVERSES", "REC",
                "PR", "MUST", "SHOULD", "NOT", "AND", "THE", "YES", "NO",
            ]),
            asker: None,
        }
    }
}

/// The keys of the `backlog` configuration section.
pub const KEYS: &[&str] = &[
    "w_ask", "w_turn", "w_front", "w_ruling", "w_placed", "age_cap_days", "live_days", "turn_days", "overdue_days", "stale_days", "near_duplicate", "ask", "front", "place", "fold", "include", "defer", "close", "done", "stop", "asker",
];

impl Settings {
    /// The settings from the configuration's `backlog` section (absent:
    /// the defaults). Every key is checked.
    ///
    /// # Errors
    ///
    /// A message naming a bad key or value.
    pub fn from_config(v: Option<&Value>) -> std::result::Result<Settings, String> {
        let mut s = Settings::default();
        let Some(v) = v else { return Ok(s) };
        let f = crate::config::Fields::new("backlog", v, KEYS).map_err(|e| e.to_string())?;
        let int = |k: &str, into: &mut i64| -> std::result::Result<(), String> {
            if let Some(n) = f.uint(k).map_err(|e| e.to_string())? {
                *into = i64::try_from(n).map_err(|_| format!("backlog.{k} is too large"))?;
            }
            Ok(())
        };
        int("w_ask", &mut s.w_ask)?;
        int("w_turn", &mut s.w_turn)?;
        int("w_front", &mut s.w_front)?;
        int("w_ruling", &mut s.w_ruling)?;
        int("w_placed", &mut s.w_placed)?;
        // A day window past 100 years is a typo, and would overflow the date
        // arithmetic (PR #144 finding 10): reject it, do not clamp.
        let days = |k: &str, into: &mut i64| -> std::result::Result<(), String> {
            int(k, into)?;
            if *into > MAX_DAYS {
                return Err(format!("backlog.{k} must be at most {MAX_DAYS} days (100 years)"));
            }
            Ok(())
        };
        days("age_cap_days", &mut s.age_cap_days)?;
        days("live_days", &mut s.live_days)?;
        days("turn_days", &mut s.turn_days)?;
        days("overdue_days", &mut s.overdue_days)?;
        days("stale_days", &mut s.stale_days)?;
        if let Some(x) = v.get("near_duplicate") {
            s.near_duplicate = x.as_f64().filter(|n| (0.0..=1.0).contains(n)).ok_or("backlog.near_duplicate must be a number from 0 to 1")?;
        }
        for (k, into) in [("ask", &mut s.ask), ("front", &mut s.front), ("place", &mut s.place), ("fold", &mut s.fold), ("include", &mut s.include), ("defer", &mut s.defer), ("close", &mut s.close), ("done", &mut s.done)] {
            if let Some(l) = f.strings(k).map_err(|e| e.to_string())? {
                *into = l.into_iter().map(|x| x.to_lowercase()).collect();
            }
        }
        if let Some(l) = f.strings("stop").map_err(|e| e.to_string())? {
            s.stop = l;
        }
        // An empty or blank name is no asker.
        s.asker = f.string("asker").map_err(|e| e.to_string())?.map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
        Ok(s)
    }
}

/// What one comb reads besides the store.
#[derive(Debug, Clone, Copy)]
pub struct Input<'a> {
    /// The comb's time (`YYYY-MM-DDTHH:MM`, UTC).
    pub as_of: &'a str,
    /// The running agents' briefs (only meaningful at the present time).
    pub running: &'a [String],
    /// The index database, for vault notes whose `status` starts with
    /// "queued". Absent or unreadable: no vault rows.
    pub index_db: Option<&'a Path>,
    /// List unplaced ruled IDs too (`--all`).
    pub all: bool,
}

/// Where a listed item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Placed, not live, not done, and asked again, or its turn came, or
    /// placed too long ago. Shown as `aging` until BC-2 reworks it to
    /// respect written orders and rulings (journal:ATLAS-V03:0444); it is
    /// not a call to pull the item forward.
    Overdue,
    /// Placed and legitimately waiting, with its reason.
    Waiting,
    /// Ruled with a placement word but never placed (`--all` only).
    Unplaced,
    /// Unplaced, older than [`Settings::stale_days`] and never asked
    /// (`--all` only, unranked).
    Stale,
}

impl State {
    /// The word for the state.
    pub fn name(self) -> &'static str {
        match self {
            State::Overdue => "aging",
            State::Waiting => "waiting",
            State::Unplaced => "unplaced",
            State::Stale => "stale",
        }
    }
}

/// One listed item.
#[derive(Debug, Clone)]
pub struct Row {
    /// Its work ID.
    pub id: String,
    /// The queue item's key, or the newest ruling's key when unplaced.
    pub key: String,
    /// Its lane.
    pub lane: String,
    /// Where it stands.
    pub state: State,
    /// Why, in a few words.
    pub reason: String,
    /// Distinct dates it was asked about again, in the asker's words.
    pub asks: usize,
    /// Distinct dates of rulings placing it.
    pub rulings_dated: usize,
    /// Days since it was first placed (or first ruled).
    pub age_days: i64,
    /// The written-order predecessor after which its turn came.
    pub turn_after: Option<String>,
    /// A ruling put it at the front.
    pub front: bool,
    /// The ruling and ask keys behind it, oldest first.
    pub rulings: Vec<String>,
    /// Its rank (0 when stale).
    pub score: i64,
    /// It has a queue item (or a `defines:` ruling, or a queued vault note).
    pub placed: bool,
}

/// The comb's answer: aging rows, then waiting, then (under `--all`)
/// unplaced and stale, each by rank.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// The rows.
    pub rows: Vec<Row>,
    /// Items not listed: live, done, folded or dismissed.
    pub hidden: usize,
}

impl Report {
    /// One row by ID.
    pub fn get(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }

    /// The rows in one state.
    pub fn of(&self, st: State) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(move |r| r.state == st)
    }
}

/// A record as it stood at the comb's time.
#[derive(Debug, Clone)]
struct Rec {
    rid: i64,
    key: String,
    kind: String,
    sub: String,
    scope: String,
    /// Its version's time at T, in seconds.
    time: i64,
    /// Its first version's time.
    first: i64,
    title: String,
    meta: String,
    body: String,
    /// At T: active, or retired (reason, time), or superseded.
    active: bool,
    retired: Option<(String, i64)>,
}

/// The first `YYYY-MM-DD` in a text, as seconds at midnight UTC.
fn stated(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    (0..b.len().saturating_sub(9)).find_map(|i| {
        let w = &b[i..i + 10];
        let shape = w.iter().enumerate().all(|(j, c)| if j == 4 || j == 7 { *c == b'-' } else { c.is_ascii_digit() });
        let edges = (i == 0 || !b[i - 1].is_ascii_digit()) && (i + 10 == b.len() || !b[i + 10].is_ascii_digit());
        if shape && edges && w[0] == b'2' {
            epoch_secs(&format!("{}T00:00", &text[i..i + 10]))
        } else {
            None
        }
    })
}

fn day(secs: i64) -> i64 {
    secs.div_euclid(DAY)
}

/// The words of a text that may be IDs: runs of letters, digits, `-` and
/// `_`, with `-` trimmed from the ends.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).map(|w| w.trim_matches('-')).filter(|w| !w.is_empty())
}

/// An upper-case work-ID shape: a capital, then capitals, digits and `-`.
fn upper_token(w: &str) -> bool {
    w.len() >= 2 && w.starts_with(|c: char| c.is_ascii_uppercase()) && w.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-') && !w.contains("--")
}

/// A word that is a work ID by shape alone: hyphenated or with a digit
/// (`TRACKS-2`, `VG2`), or a known lane. Bare capitalised words are not.
fn id_shaped(w: &str, stop: &HashSet<&str>, lanes: &HashSet<String>) -> bool {
    if !upper_token(w) || w.len() < 3 || stop.contains(w) {
        return false;
    }
    if w.split_once('-').is_some_and(|(head, _)| ["Q", "PR", "UTF", "SHA", "RFC", "ISO", "LANE"].contains(&head)) {
        return false;
    }
    w.contains('-') || w.chars().any(|c| c.is_ascii_digit()) || lanes.contains(w)
}

/// Lowercase text, whitespace collapsed, padded so a cue matches on word
/// edges.
fn lower(text: &str) -> String {
    format!(" {} ", crate::memory::rules::norm(text).replace(['.', ',', ';', ':', '"', '(', ')', '*'], " ").split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Where a cue starts in `low` ([`lower`]), on word edges: the earliest
/// occurrence of any cue that ends on a word edge, looking past one that
/// does not (`start` in " started ... start ", PR #144 finding 6).
fn cue_at(low: &str, cues: &[String]) -> Option<(usize, usize)> {
    cues.iter()
        .filter_map(|c| {
            let pat = format!(" {c}");
            low.match_indices(&pat).map(|(i, _)| (i + 1, c.len())).find(|(i, n)| low[i + n..].starts_with(|ch: char| !ch.is_ascii_alphanumeric()))
        })
        .min()
}

fn has_cue(low: &str, cues: &[String]) -> bool {
    cue_at(low, cues).is_some()
}

/// A text with its lead (`- 2026-11-04 Name:`, `Name 2026-11-10:`) taken
/// off, and whether that lead is the asker's ([`Settings::asker`]; unset,
/// never).
fn lead<'t>(text: &'t str, asker: Option<&str>) -> (&'t str, bool) {
    let t = text.trim_start().trim_start_matches(['-', '*', '#', ' ']).trim_start();
    let near = t.char_indices().nth(48).map_or(t.len(), |(i, _)| i);
    let head = &t[..near];
    let tail = head.get(10..).unwrap_or("").trim_start();
    let own = asker.is_some_and(|a| {
        head.strip_prefix(a).and_then(|r| r.strip_prefix(' ')).is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
            || (head.get(..10).and_then(stated).is_some() && named(tail.strip_prefix('(').unwrap_or(tail), a))
    });
    let rest = if own || t.starts_with(|c: char| c.is_ascii_digit()) { head.find(": ").map_or(t, |i| &t[i + 2..]) } else { t };
    (rest, own)
}

/// Whether `text` starts with the whole word `name` (`Ann` leads
/// `Ann:`, not `Anna:`).
fn named(text: &str, name: &str) -> bool {
    text.strip_prefix(name).is_some_and(|r| !r.starts_with(|c: char| c.is_alphanumeric()))
}

/// The token set of a text, for [`jaccard`].
fn tokens(text: &str) -> HashSet<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// Token Jaccard of two [`tokens`] sets.
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let inter = small.iter().filter(|w| big.contains(*w)).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        return 0.0;
    }
    inter as f64 / union as f64
}

/// Lowers `into[k]` to `v`.
fn min_into<'a>(into: &mut HashMap<&'a str, i64>, k: &'a str, v: i64) {
    into.entry(k).and_modify(|x| *x = (*x).min(v)).or_insert(v);
}

/// The smaller of two optional times.
fn min_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// The as-of time as seconds, and as a store time bound for SQL.
fn as_of(t: &str) -> std::result::Result<(i64, String), String> {
    let secs = epoch_secs(t).ok_or_else(|| format!("--as-of {t:?}: expected YYYY-MM-DDTHH:MM (UTC)"))?;
    let c = crate::discussion::civil(secs);
    Ok((secs, format!("{}.999Z", &c[..c.len() - 1])))
}

/// Every journal entry, rule and block as it stood at `bound`.
fn load(store: &MemoryStore, bound: &str, t: i64) -> Result<Vec<Rec>> {
    let sql = "SELECT r.rid, r.key, r.kind, r.sub, r.scope, r.status, v.time, v.title, v.meta, v.body,
            (SELECT MIN(v3.time) FROM versions v3 WHERE v3.rid = r.rid),
            (SELECT t.time || char(9) || t.reason FROM retirements t WHERE t.rid = r.rid AND t.time <= ?1 ORDER BY t.time DESC, t.rowid DESC LIMIT 1),
            (SELECT MIN(s.time) FROM supersedes s WHERE s.old_rid = r.rid AND s.time <= ?1)
        FROM records r JOIN versions v ON v.rid = r.rid
        WHERE r.kind IN ('journal', 'rule', 'block')
          AND v.seq = COALESCE((SELECT MAX(v2.seq) FROM versions v2 WHERE v2.rid = r.rid AND v2.time <= ?1), 1)
        ORDER BY r.rid";
    let mut st = store.conn().prepare(sql)?;
    let rows = st.query_map([bound], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, String>(9)?,
            r.get::<_, Option<String>>(10)?,
            r.get::<_, Option<String>>(11)?,
            r.get::<_, Option<String>>(12)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (rid, key, kind, sub, scope, status, time, title, meta, body, first, retired, superseded) = row?;
        let first = first.as_deref().and_then(epoch_secs).unwrap_or(i64::MAX);
        let time = epoch_secs(&time).unwrap_or(first);
        if first > t {
            // An imported ruling exists from the date it states.
            let dated = kind != "journal" && stated(if body.is_empty() { &title } else { &body }).is_some_and(|d| d <= t);
            if !dated {
                continue;
            }
        }
        let retired = retired.filter(|_| status == "retired").and_then(|x| x.split_once('\t').map(|(tm, why)| (why.to_string(), epoch_secs(tm).unwrap_or(t))));
        let superseded = superseded.is_some() && status == "superseded";
        out.push(Rec { rid, key, kind, sub, scope, time, first, title, meta, body, active: retired.is_none() && !superseded, retired });
    }
    Ok(out)
}

/// The record order of each export (`placement`), for section inheritance.
fn placed_order(store: &MemoryStore) -> Result<HashMap<i64, (String, i64)>> {
    let mut st = store.conn().prepare("SELECT export, ord, rid FROM placement")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(2)?, (r.get::<_, String>(0)?, r.get::<_, i64>(1)?))))?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

/// The IDs `ruling:` records define, by record id.
fn defines(store: &MemoryStore) -> Result<Vec<(i64, String)>> {
    let mut st = store.conn().prepare("SELECT rid, target FROM refs WHERE kind = 'defines'")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Vault notes whose frontmatter `status` starts with "queued": (id, status,
/// modified seconds).
fn vault_rows(db: Option<&Path>) -> Vec<(String, String, String, i64)> {
    let Some(db) = db.filter(|p| p.is_file()) else { return Vec::new() };
    let Ok(conn) = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return Vec::new() };
    let Ok(mut st) = conn.prepare("SELECT vault, path, title, frontmatter, mtime_ns FROM notes WHERE frontmatter LIKE '%queued%'") else { return Vec::new() };
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?)));
    let Ok(rows) = rows else { return Vec::new() };
    rows.filter_map(std::result::Result::ok)
        .filter_map(|(vault, path, title, fm, mtime)| {
            let v = crate::json::from_str::<Value>(&fm).ok()?;
            let status = v.get("status")?.as_str()?.to_string();
            if !status.to_ascii_lowercase().starts_with("queued") {
                return None;
            }
            let id = v.get("id").and_then(Value::as_str).map_or(title, str::to_string);
            Some((id, status, format!("{vault}:{path}"), mtime / 1_000_000_000))
        })
        .collect()
}

/// A text a ruling or ask can come from, read once.
struct Doc<'a> {
    rec: &'a Rec,
    /// Its date: the date it states, else its section's, else its time.
    date: i64,
    /// In the asker's words: its lead, its section heading, or an idea.
    asker: bool,
    /// The IDs its section heading names.
    heading_ids: Vec<String>,
    /// Its lines, title first.
    lines: Vec<&'a str>,
}

/// A queue item, as the comb sees it.
#[derive(Debug, Clone)]
struct Item {
    id: String,
    key: String,
    lane: String,
    /// Index into the records.
    rec: usize,
    order: Option<crate::lanes::Order>,
    after: Vec<String>,
}

/// The item's ID: the title's head before a colon when ID-shaped
/// (`TLAS-STABLE-SLOTS: ...`), else its lane when the lane holds only it,
/// else its key.
fn item_id(title: &str, lane: &str, key: &str, lane_items: usize) -> String {
    if let Some((head, _)) = title.split_once(':') {
        let h = head.trim();
        if upper_token(h) && h.chars().any(|c| c.is_ascii_alphabetic()) {
            return h.to_string();
        }
    }
    if lane != crate::queue::LANE && !lane.starts_with("session:") && lane_items == 1 {
        return lane.to_string();
    }
    key.to_string()
}

/// What closes or defers an item: a ruling whose subject it is.
#[derive(Debug, Clone)]
enum Close {
    Folded,
    Deferred(String),
    Done,
}

/// Per-item facts gathered from the rulings and asks.
#[derive(Debug, Clone, Default)]
struct Facts {
    asks: BTreeSet<i64>,
    newest_ask: Option<i64>,
    rulings: BTreeSet<i64>,
    keys: Vec<String>,
    front: bool,
    closes: Vec<(i64, Close)>,
    first_ruled: Option<i64>,
    last_ruled: Option<i64>,
}

impl Facts {
    fn key(&mut self, k: &str) {
        if !self.keys.iter().any(|x| x == k) {
            self.keys.push(k.to_string());
        }
    }
}

/// Combs the store as of `input.as_of`.
///
/// # Errors
///
/// [`crate::Error::Db`], or [`crate::Error::InvalidArgument`] for a bad
/// time.
pub fn comb(store: &MemoryStore, s: &Settings, input: &Input<'_>) -> Result<Report> {
    let (t, bound) = as_of(input.as_of).map_err(crate::Error::InvalidArgument)?;
    let recs = load(store, &bound, t)?;
    let order = placed_order(store)?;
    let by_rid: HashMap<i64, usize> = recs.iter().enumerate().map(|(i, r)| (r.rid, i)).collect();
    let by_key: HashMap<&str, usize> = recs.iter().enumerate().map(|(i, r)| (r.key.as_str(), i)).collect();
    let lanes: HashSet<String> = recs.iter().filter(|r| r.kind == "journal" && !r.scope.starts_with("session:") && r.scope != crate::queue::LANE).map(|r| r.scope.clone()).collect();

    // The queue: every item that existed at T, active or not.
    let queued: Vec<usize> = (0..recs.len()).filter(|&i| recs[i].kind == "journal" && recs[i].sub == crate::queue::ENTRY).collect();
    let mut lane_items: HashMap<&str, usize> = HashMap::new();
    for &i in &queued {
        *lane_items.entry(recs[i].scope.as_str()).or_default() += 1;
    }
    let items: Vec<Item> = queued
        .iter()
        .map(|&i| {
            let r = &recs[i];
            let m = crate::queue::qmeta(&r.meta);
            Item { id: item_id(&r.title, &r.scope, &r.key, lane_items[r.scope.as_str()]), key: r.key.clone(), lane: r.scope.clone(), rec: i, order: m.order, after: m.after }
        })
        .collect();
    let ids: HashSet<&str> = items.iter().map(|x| x.id.as_str()).collect();

    // Marks and dismissals written by T.
    let mut done_marks: HashMap<String, i64> = HashMap::new();
    let mut met: Vec<(String, String, i64)> = Vec::new();
    // A dismissal is a judgement about the ID, not an event: it holds at
    // every time, so a replay never resurfaces a dismissed false flag.
    let dismissed: HashSet<String> = store.note_metas(&format!("{DISMISS}: "))?.iter().filter_map(|m| crate::queue::meta_value(m, DISMISS)).map(str::to_string).collect();
    for r in recs.iter().filter(|r| r.kind == "journal" && r.sub == "note" && r.active) {
        if let Some(k) = crate::queue::meta_value(&r.meta, "done") {
            done_marks.entry(k.to_string()).or_insert(r.time);
        }
        if let Some(k) = crate::queue::meta_value(&r.meta, "met") {
            met.push((k.to_string(), crate::queue::meta_value(&r.meta, "condition").unwrap_or("*").to_string(), r.time));
        }
    }

    // Briefs and commits: live within the window, done when a commit says so.
    let window = t - s.live_days * DAY;
    let work: Vec<&Rec> = recs.iter().filter(|r| r.kind == "journal" && r.active && (r.sub == "brief" || r.sub == "commit")).collect();
    let single = |lane: &str| lane_items.get(lane).copied().unwrap_or(0) <= 1;
    // The asker's name in capitals is a word, never work.
    let asker_word = s.asker.as_deref().map(str::to_uppercase);
    let stop: HashSet<&str> = s.stop.iter().map(String::as_str).chain(asker_word.as_deref()).collect();
    let asker = s.asker.as_deref();
    // A record is in `id`'s lane when its scope is the lane named after it,
    // or the item's own lane when it holds only that item.
    // A brief makes an ID live by its lane or by naming it (a whole token);
    // a commit only by its lane, since commits list many IDs in passing.
    //
    // Both joins are read once per comb into maps (PR #144 finding 1): the
    // earliest in-window brief or commit per scope and per word a brief
    // names, and the earliest landing commit per scope and per title word.
    // A lookup then answers "in its lane, or names it as a whole token"
    // without a scan, so the comb is linear in the store.
    let mut live_scope: HashMap<&str, i64> = HashMap::new();
    let mut live_word: HashMap<&str, i64> = HashMap::new();
    let mut landed_scope: HashMap<&str, i64> = HashMap::new();
    let mut landed_word: HashMap<&str, i64> = HashMap::new();
    for r in work.iter().filter(|r| r.time <= t) {
        if r.time > window {
            min_into(&mut live_scope, r.scope.as_str(), r.time);
            if r.sub == "brief" {
                for w in words(&r.title).chain(words(&r.body)) {
                    min_into(&mut live_word, w, r.time);
                }
            }
        }
        // The done word last: it is the costly test.
        if r.sub == "commit" && has_cue(&lower(&r.title), &s.done) {
            min_into(&mut landed_scope, r.scope.as_str(), r.time);
            for w in words(&r.title) {
                min_into(&mut landed_word, w, r.time);
            }
        }
    }
    // The earliest time among records in `id`'s lane or naming it.
    let joined = |scope: &HashMap<&str, i64>, word: &HashMap<&str, i64>, id: &str, lane: &str| -> Option<i64> {
        let mut best = scope.get(id).copied();
        if lane == id || single(lane) {
            best = min_opt(best, scope.get(lane).copied());
        }
        min_opt(best, word.get(id).copied())
    };
    // The running agents' briefs: the words of a declared lane, and every
    // word.
    let running: Vec<(Option<Vec<&str>>, HashSet<&str>)> = input.running.iter().map(|b| (b.lines().take(40).find_map(|l| crate::lanes::decl(l, "lane")).map(|v| words(v).collect()), words(b).collect())).collect();
    let live_since = |id: &str, lane: &str| -> Option<i64> {
        let mut since = joined(&live_scope, &live_word, id, lane);
        for (decl, all) in &running {
            let declared = decl.as_ref().map(|ws| ws.iter().any(|&w| w == id || (w == lane && single(lane))));
            if declared == Some(true) || (!id.is_empty() && all.contains(id)) {
                since = Some(since.map_or(t, |x| x.min(t)));
            }
        }
        since
    };
    let landed = |id: &str, lane: &str| -> Option<i64> { joined(&landed_scope, &landed_word, id, lane) };

    // Documents: INTENT items with their section, rulings, decisions,
    // answers, ideas and the asker's journal words.
    let mut heads: HashMap<String, (String, i64)> = HashMap::new();
    let mut ordered: Vec<usize> = (0..recs.len()).filter(|&i| recs[i].kind != "journal").collect();
    ordered.sort_by_key(|&i| order.get(&recs[i].rid).map_or((recs[i].scope.clone(), i64::MAX, recs[i].rid), |(e, o)| (e.clone(), *o, recs[i].rid)));
    let mut docs: Vec<Doc<'_>> = Vec::new();
    for &i in &ordered {
        let r = &recs[i];
        let text = if r.body.is_empty() { r.title.as_str() } else { r.body.as_str() };
        // A block that opens a section sets the heading for what follows.
        if let Some(h) = text.lines().find(|l| l.starts_with("## ")) {
            let date = stated(h).unwrap_or(r.time);
            heads.insert(r.scope.clone(), (h.to_string(), date));
        }
        if !r.active {
            continue;
        }
        let (heading, hdate) = heads.get(&r.scope).cloned().unwrap_or_default();
        let hasker = lead(heading.trim_start_matches('#').trim(), asker).1;
        let heading_ids: Vec<String> = if heading.to_lowercase().contains("rulings") { Vec::new() } else { words(&heading).filter(|w| ids.contains(w) || id_shaped(w, &stop, &lanes)).map(str::to_string).collect() };
        let (_, own) = lead(text, asker);
        let date = stated(text).or((hdate > 0).then_some(hdate)).unwrap_or(r.time);
        docs.push(Doc { rec: r, date, asker: own || hasker, heading_ids, lines: text.lines().collect() });
    }
    for r in recs.iter().filter(|r| r.kind == "journal" && r.active && matches!(r.sub.as_str(), "decision" | "answered" | "idea" | "note" | "question" | "next")) {
        let (_, own) = lead(&r.title, asker);
        let by_asker = own || r.scope == "session:ideas";
        // Only the asker's notes and questions count: as asks.
        if !by_asker && !matches!(r.sub.as_str(), "decision" | "answered") {
            continue;
        }
        let date = if own { stated(&r.title).unwrap_or(r.time) } else { r.time };
        let mut lines = vec![r.title.as_str()];
        lines.extend(r.body.lines());
        docs.push(Doc { rec: r, date, asker: by_asker, heading_ids: Vec::new(), lines });
    }
    docs.sort_by_key(|d| (d.date, d.rec.rid));

    // Lane chains: the head of each lane's active items.
    let item_of_id: HashMap<&str, Vec<usize>> = items.iter().enumerate().fold(HashMap::new(), |mut m, (n, it)| {
        m.entry(it.id.as_str()).or_insert_with(Vec::new).push(n);
        m
    });
    let active_item = |n: usize| recs[items[n].rec].active;
    // Each lane's items in record order, the first item per key, and each
    // lane's head (its first active item by order, then key).
    let mut lane_list: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut item_of_key: HashMap<&str, usize> = HashMap::new();
    for (n, it) in items.iter().enumerate() {
        lane_list.entry(it.lane.as_str()).or_default().push(n);
        item_of_key.entry(it.key.as_str()).or_insert(n);
    }
    let heads_of: HashMap<&str, usize> = lane_list
        .iter()
        .filter_map(|(&l, ns)| ns.iter().copied().filter(|&n| active_item(n)).min_by(|&a, &b| crate::lanes::Order::key(items[a].order.as_ref()).cmp(&crate::lanes::Order::key(items[b].order.as_ref())).then(items[a].key.cmp(&items[b].key))).map(|h| (l, h)))
        .collect();
    let lane_head = |lane: &str| -> Option<usize> { heads_of.get(lane).copied() };
    // Several items of one lane named together are the chain: its head.
    let chain = |named: Vec<String>| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen_lanes: HashMap<String, Vec<String>> = HashMap::new();
        for id in named {
            let lane = item_of_id.get(id.as_str()).and_then(|v| v.iter().find(|&&n| active_item(n))).map(|&n| items[n].lane.clone());
            match lane {
                Some(l) if !single(&l) => seen_lanes.entry(l).or_default().push(id),
                _ => out.push(id),
            }
        }
        for (l, named) in seen_lanes {
            match lane_head(&l) {
                Some(h) if named.len() > 1 => out.push(items[h].id.clone()),
                _ => out.extend(named),
            }
        }
        out.sort();
        out.dedup();
        out
    };

    let mut facts: HashMap<String, Facts> = HashMap::new();
    // Each ID's asks so far, as token sets: a near-duplicate is judged per
    // ID (PR #144 finding 4).
    let mut asks_seen: HashMap<String, Vec<HashSet<String>>> = HashMap::new();
    let mut unplaced: HashMap<String, Facts> = HashMap::new();
    for d in &docs {
        let r = d.rec;
        let whole: String = d.lines.join("\n");
        let doc_ids: Vec<String> = {
            let mut v: Vec<String> = words(&whole).filter(|w| ids.contains(w)).map(str::to_string).collect();
            v.sort();
            v.dedup();
            v
        };
        let mut asked_here = false;
        for line in &d.lines {
            let (rest, _) = lead(line, asker);
            let low = lower(rest);
            let on_line: Vec<String> = {
                let mut v: Vec<String> = words(rest).filter(|w| ids.contains(w)).map(str::to_string).collect();
                v.dedup();
                v
            };
            // IDs this line repeats an earlier ask about: the ask counts, but
            // not again as a placement or a front ruling.
            let mut repeated: HashSet<String> = HashSet::new();
            let mut all_repeated = false;
            // Asks: the asker's words with an ask cue.
            if d.asker && has_cue(&low, &s.ask) && !asked_here {
                let mut named = if !on_line.is_empty() {
                    on_line.clone()
                } else if !doc_ids.is_empty() {
                    doc_ids.clone()
                } else if !d.heading_ids.iter().any(|h| ids.contains(h.as_str())) && r.kind == "journal" {
                    lane_head(&r.scope).map(|n| vec![items[n].id.clone()]).unwrap_or_default()
                } else {
                    d.heading_ids.iter().filter(|h| ids.contains(h.as_str())).cloned().collect()
                };
                if r.kind == "journal" && named.is_empty() {
                    named = lane_head(&r.scope).map(|n| vec![items[n].id.clone()]).unwrap_or_default();
                }
                let named = chain(named);
                if !named.is_empty() {
                    asked_here = true;
                    let norm = tokens(&low);
                    let n_named = named.len();
                    for id in named {
                        let seen = asks_seen.entry(id.clone()).or_default();
                        let dup = seen.iter().any(|a| jaccard(a, &norm) >= s.near_duplicate);
                        seen.push(norm.clone());
                        let f = facts.entry(id.clone()).or_default();
                        f.asks.insert(day(d.date));
                        f.newest_ask = Some(f.newest_ask.map_or(d.date, |x| x.max(d.date)));
                        f.key(&r.key);
                        if dup {
                            f.rulings.remove(&day(d.date));
                            repeated.insert(id);
                        }
                    }
                    all_repeated = repeated.len() == n_named;
                }
            }
            // Rulings: a placement word on a line naming the ID.
            let placing = has_cue(&low, &s.place);
            let front = has_cue(&low, &s.front);
            for id in on_line.iter().filter(|id| !all_repeated && !repeated.contains(*id)) {
                let f = facts.entry(id.clone()).or_default();
                if placing {
                    f.rulings.insert(day(d.date));
                    f.key(&r.key);
                }
                if front {
                    f.front = true;
                    f.key(&r.key);
                }
            }
            // The subject (first ID on the line) closed, folded or deferred.
            let subject = words(rest).find(|w| ids.contains(w) || id_shaped(w, &stop, &lanes)).map(str::to_string);
            if let Some(subj) = subject {
                let after_subject = low.find(&format!(" {} ", subj.to_lowercase())).map_or(low.as_str(), |i| &low[i..]);
                let close = if cue_at(after_subject, &s.fold).is_some() {
                    Some(Close::Folded)
                } else if cue_at(after_subject, &s.defer).is_some() {
                    let shown: String = rest.trim().chars().take(80).collect();
                    Some(Close::Deferred(format!("deferred by {}: {shown}", r.key)))
                } else if cue_at(after_subject, &s.close).is_some() {
                    Some(Close::Done)
                } else {
                    None
                };
                if let Some(c) = close {
                    facts.entry(subj.clone()).or_default().closes.push((d.date, c));
                }
                if let Some((i, n)) = cue_at(after_subject, &s.include) {
                    for child in words(&after_subject[i + n..]).map(str::to_uppercase).filter(|w| ids.contains(w.as_str())) {
                        if child != subj {
                            facts.entry(child).or_default().closes.push((d.date, Close::Folded));
                        }
                    }
                }
            }
            // Unplaced ruled IDs: work-shaped words on a placing line.
            if placing && input.all {
                for w in words(rest).filter(|w| !ids.contains(w) && id_shaped(w, &stop, &lanes)) {
                    let f = unplaced.entry(w.to_string()).or_default();
                    f.rulings.insert(day(d.date));
                    f.first_ruled = Some(f.first_ruled.map_or(d.date, |x| x.min(d.date)));
                    f.last_ruled = Some(f.last_ruled.map_or(d.date, |x| x.max(d.date)));
                    f.key(&r.key);
                    if front {
                        f.front = true;
                    }
                }
            }
        }
    }

    // When an ID's turn came: done, started or live, at the earliest.
    // Memoized: each predecessor is asked about once per comb.
    let turns: std::cell::RefCell<HashMap<(String, String), Option<i64>>> = std::cell::RefCell::default();
    let turn_time = |id: &str, lane: &str| -> Option<i64> {
        let k = (id.to_string(), lane.to_string());
        if let Some(&w) = turns.borrow().get(&k) {
            return w;
        }
        let mut best: Option<i64> = None;
        let mut take = |x: i64| best = Some(best.map_or(x, |b: i64| b.min(x)));
        for &n in item_of_id.get(id).map(Vec::as_slice).unwrap_or(&[]) {
            let r = &recs[items[n].rec];
            if let Some((_, when)) = &r.retired {
                take(*when);
            }
            if let Some(&m) = done_marks.get(&r.key) {
                take(m);
            }
        }
        if let Some(x) = landed(id, lane) {
            take(x);
        }
        if let Some(x) = live_since(id, lane) {
            take(x);
        }
        turns.borrow_mut().insert(k, best);
        best
    };
    let key_turn = |key: &str| -> Option<(String, Option<i64>)> {
        let n = *item_of_key.get(key)?;
        let r = &recs[items[n].rec];
        let own = r.retired.as_ref().map(|(_, w)| *w).into_iter().chain(done_marks.get(key).copied()).min();
        Some((items[n].id.clone(), own.or_else(|| turn_time(&items[n].id, &items[n].lane))))
    };

    // `met:` marks by the item key they name.
    let mut met_of: HashMap<&str, Vec<(&str, i64)>> = HashMap::new();
    for (k, cond, at) in &met {
        met_of.entry(k.as_str()).or_default().push((cond.as_str(), *at));
    }

    let mut rows = Vec::new();
    let mut hidden = 0usize;
    let mut listed: HashSet<String> = HashSet::new();
    for (n, it) in items.iter().enumerate() {
        let r = &recs[it.rec];
        if !r.active || dismissed.contains(&it.id) || done_marks.contains_key(&it.key) || listed.contains(&it.id) {
            continue;
        }
        let f = facts.get(&it.id).cloned().unwrap_or_default();
        // Its placement: the first version, not a later revision such as a
        // relaunch as another kind (PR #144 finding 5).
        let placed_at = r.first;
        // A later closing ruling wins over the placement.
        let close = f.closes.iter().filter(|(d, _)| day(*d) >= day(placed_at)).max_by_key(|(d, _)| *d).map(|(_, c)| c.clone());
        let live = live_since(&it.id, &it.lane).is_some();
        let done = landed(&it.id, &it.lane).is_some();
        if live || done || matches!(close, Some(Close::Folded | Close::Done)) {
            hidden += 1;
            continue;
        }
        // The written order: `after:` conditions, else the lane chain's
        // predecessor by `order:`.
        let mut preds: Vec<(String, Option<i64>)> = Vec::new();
        for a in &it.after {
            match crate::queue::dep(a) {
                crate::queue::Dep::Key(k) => preds.push(key_turn(&k).unwrap_or((k, None))),
                crate::queue::Dep::Condition(c) => {
                    let head: String = c.chars().take_while(|ch| !ch.is_whitespace() && *ch != '(').collect();
                    let head = head.trim_end_matches([',', '.', ';']).to_string();
                    let marked = met_of.get(it.key.as_str()).and_then(|v| v.iter().filter(|(cond, _)| *cond == c || *cond == "*").map(|x| x.1).min());
                    if marked.is_some() {
                        preds.push((c.clone(), marked));
                    } else if upper_token(&head) && head.len() >= 3 && (c[head.len()..].trim().is_empty() || c[head.len()..].trim_start().starts_with('(')) {
                        let lane = item_of_id.get(head.as_str()).and_then(|v| v.first()).map_or(head.clone(), |&k| items[k].lane.clone());
                        preds.push((head.clone(), turn_time(&head, &lane)));
                    } else {
                        preds.push((c.clone(), None));
                    }
                }
            }
        }
        if it.after.is_empty() {
            if let Some(o) = &it.order {
                let prev = lane_list[it.lane.as_str()]
                    .iter()
                    .copied()
                    .filter(|&k| k != n && items[k].order.as_ref().is_some_and(|x| x < o))
                    .max_by(|&a, &b| items[a].order.cmp(&items[b].order));
                if let Some(p) = prev {
                    preds.push(key_turn(&items[p].key).unwrap_or((items[p].id.clone(), None)));
                }
            }
        }
        let turn = if preds.is_empty() || preds.iter().any(|(_, w)| w.is_none()) { None } else { preds.iter().filter_map(|(_, w)| *w).max() };
        let turn_after = turn.and_then(|_| preds.iter().max_by_key(|(_, w)| *w).map(|(p, _)| p.clone()));
        let age = (t - r.first).div_euclid(DAY);
        let recent_ask = f.newest_ask.is_some_and(|a| day(a) >= day(placed_at));
        let turn_due = turn.is_some_and(|w| t - w >= s.turn_days * DAY);
        let (state, reason) = match &close {
            Some(Close::Deferred(why)) => (State::Waiting, why.clone()),
            _ if recent_ask => (State::Overdue, format!("asked again {}", &crate::discussion::civil(f.newest_ask.unwrap_or(t))[..10])),
            _ if turn_due => (State::Overdue, format!("turn came after {}", turn_after.clone().unwrap_or_default())),
            _ if age > s.overdue_days => (State::Overdue, format!("placed {age} days ago")),
            _ => {
                let waiting_on = preds.iter().find(|(_, w)| w.is_none()).map(|(p, _)| format!("after {p}"));
                (State::Waiting, waiting_on.or_else(|| turn_after.as_ref().map(|p| format!("turn came after {p} under {} days ago", s.turn_days))).unwrap_or_else(|| format!("placed {age} days ago")))
            }
        };
        let mut keys = f.keys.clone();
        keys.retain(|k| k != &it.key);
        let score = s.w_ask * f.asks.len() as i64 + s.w_turn * i64::from(turn.is_some()) + s.w_front * i64::from(f.front) + s.w_ruling * (f.rulings.len() as i64 - 1).max(0) + s.w_placed + age.clamp(0, s.age_cap_days);
        listed.insert(it.id.clone());
        rows.push(Row {
            id: it.id.clone(),
            key: it.key.clone(),
            lane: it.lane.clone(),
            state,
            reason,
            asks: f.asks.len(),
            rulings_dated: f.rulings.len(),
            age_days: age,
            turn_after,
            front: f.front,
            rulings: keys,
            score,
            placed: true,
        });
    }

    // IDs a ruling defines are placed work too.
    for (rid, id) in defines(store)? {
        let Some(&i) = by_rid.get(&rid) else { continue };
        if listed.contains(&id) || dismissed.contains(&id) || item_of_id.contains_key(id.as_str()) {
            continue;
        }
        let r = &recs[i];
        let f = facts.get(&id).cloned().unwrap_or_default();
        let since = stated(&r.body).unwrap_or(r.first);
        // As for a queue item: the newest closing ruling dated on or after
        // the definition wins, and a deferral waits (PR #144 finding 2).
        let close = f.closes.iter().filter(|(d, _)| day(*d) >= day(since)).max_by_key(|(d, _)| *d).map(|(_, c)| c.clone());
        if live_since(&id, &id).is_some() || landed(&id, &id).is_some() || matches!(close, Some(Close::Folded | Close::Done)) {
            hidden += 1;
            continue;
        }
        let age = (t - since).div_euclid(DAY);
        let recent = f.newest_ask.is_some_and(|a| day(a) >= day(since));
        let (state, reason) = match close {
            Some(Close::Deferred(why)) => (State::Waiting, why),
            _ if recent => (State::Overdue, "asked again".to_string()),
            _ if age > s.overdue_days => (State::Overdue, format!("defined {age} days ago, no queue item")),
            _ => (State::Waiting, format!("defined {age} days ago, no queue item")),
        };
        listed.insert(id.clone());
        rows.push(Row {
            reason,
            score: s.w_ask * f.asks.len() as i64 + s.w_front * i64::from(f.front) + s.w_placed + age.clamp(0, s.age_cap_days),
            id,
            key: r.key.clone(),
            lane: String::new(),
            state,
            asks: f.asks.len(),
            rulings_dated: f.rulings.len(),
            age_days: age,
            turn_after: None,
            front: f.front,
            rulings: f.keys.clone(),
            placed: true,
        });
    }

    // Vault notes with a queued status: placed, waiting on their status.
    for (id, status, at, mtime) in vault_rows(input.index_db) {
        if mtime > t || listed.contains(&id) || dismissed.contains(&id) || live_since(&id, &id).is_some() {
            continue;
        }
        let f = facts.get(&id).cloned().unwrap_or_default();
        // Closed as a queue item is (PR #144 finding 3): a done mark on its
        // ID or note, a commit that says it landed, or a closing ruling. A
        // note's file time is not a placement date, so the newest closing
        // ruling counts at any date: a note still queued after a drop is a
        // stale status.
        let close = f.closes.iter().max_by_key(|(d, _)| *d).map(|(_, c)| c.clone());
        if done_marks.contains_key(&id) || done_marks.contains_key(&at) || landed(&id, &id).is_some() || matches!(close, Some(Close::Folded | Close::Done)) {
            hidden += 1;
            continue;
        }
        let recent = f.newest_ask.is_some_and(|a| day(a) >= day(mtime));
        let age = (t - mtime).div_euclid(DAY);
        let (state, reason) = match close {
            Some(Close::Deferred(why)) => (State::Waiting, why),
            _ if recent => (State::Overdue, format!("status: {status}")),
            _ => (State::Waiting, format!("status: {status}")),
        };
        listed.insert(id.clone());
        rows.push(Row {
            state,
            reason,
            // A note's age is its file's, not a placement's: not ranked by it.
            score: s.w_ask * f.asks.len() as i64 + s.w_placed,
            id,
            key: at,
            lane: String::new(),
            asks: f.asks.len(),
            rulings_dated: f.rulings.len(),
            age_days: age,
            turn_after: None,
            front: f.front,
            rulings: f.keys.clone(),
            placed: true,
        });
    }

    // Under --all: ruled IDs never placed, stale ones unranked.
    if input.all {
        let mut ruled: Vec<(String, Facts)> = unplaced.into_iter().collect();
        ruled.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, f) in ruled {
            if listed.contains(&id) || dismissed.contains(&id) || by_key.contains_key(id.as_str()) {
                continue;
            }
            let asked = facts.get(&id).map_or(0, |x| x.asks.len());
            let closed = facts.get(&id).is_some_and(|x| !x.closes.is_empty());
            if closed || live_since(&id, &id).is_some() || landed(&id, &id).is_some() {
                continue;
            }
            let first = f.first_ruled.unwrap_or(t);
            let last = f.last_ruled.unwrap_or(t);
            let age = (t - first).div_euclid(DAY);
            let stale = (t - last).div_euclid(DAY) > s.stale_days && asked == 0;
            rows.push(Row {
                id,
                key: f.keys.last().cloned().unwrap_or_default(),
                lane: String::new(),
                state: if stale { State::Stale } else { State::Unplaced },
                reason: if stale { format!("ruled {age} days ago, never placed or asked") } else { "ruled, never placed".into() },
                asks: asked,
                rulings_dated: f.rulings.len(),
                age_days: age,
                turn_after: None,
                front: f.front,
                score: if stale { 0 } else { s.w_ask * asked as i64 + s.w_front * i64::from(f.front) + s.w_ruling * (f.rulings.len() as i64 - 1).max(0) + age.clamp(0, s.age_cap_days) },
                rulings: f.keys,
                placed: false,
            });
        }
    }

    let rank = |st: State| match st {
        State::Overdue => 0,
        State::Waiting => 1,
        State::Unplaced => 2,
        State::Stale => 3,
    };
    rows.sort_by(|a, b| rank(a.state).cmp(&rank(b.state)).then(b.score.cmp(&a.score)).then(b.age_days.cmp(&a.age_days)).then(a.id.cmp(&b.id)));
    Ok(Report { rows, hidden })
}

/// Dismisses an ID for good: a note in the queue's lane (client
/// `atlas-queue`, so it is not work) naming the record that says why.
///
/// # Errors
///
/// [`crate::Error::InvalidArgument`] when `because` names no record, or
/// [`crate::Error::Db`].
pub fn dismiss(store: &mut MemoryStore, id: &str, because: &str) -> Result<String> {
    if id.trim().is_empty() || id.chars().any(char::is_whitespace) {
        return Err(crate::Error::InvalidArgument(format!("dismiss {id:?}: one ID, no spaces")));
    }
    if store.head_no_body(because)?.is_none() {
        return Err(crate::Error::InvalidArgument(format!("--because {because}: no such record (the reason must be a record key)")));
    }
    let title = format!("backlog: {id} dismissed, because {because}");
    let d = Draft { title: title.clone(), meta: format!("{DISMISS}: {id}\nbecause: {because}"), body: title, reason: "atlas backlog dismiss".into(), author: "atlas".into(), client: "atlas-queue".into(), provenance: None, time: None };
    let lane = crate::queue::LANE;
    let mut w = store.writer()?;
    let key = w.next_key(&format!("journal:{lane}:"), 4)?;
    w.create(&key, "journal", "note", lane, &d, None)?;
    w.add_refs(&key, &[("ref".to_string(), because.to_string())])?;
    w.commit()?;
    Ok(key)
}

/// The usage of `atlas backlog`.
pub const USAGE: &str = "usage: atlas backlog [--all] [--as-of YYYY-MM-DDTHH:MM] [--json] [--transcript PATH] | dismiss ID --because KEY";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn row_json(r: &Row) -> Value {
    crate::json!({
        "id": r.id.clone(),
        "key": r.key.clone(),
        "lane": r.lane.clone(),
        "state": r.state.name().to_string(),
        "reason": r.reason.clone(),
        "asks": r.asks,
        "rulings_dated": r.rulings_dated,
        "age_days": r.age_days,
        "turn_after": r.turn_after.clone(),
        "front": r.front,
        "rulings": r.rulings.clone(),
        "score": r.score,
        "placed": r.placed
    })
}

fn row_line(r: &Row) -> String {
    let mut l = format!("{}  {}  {}d  {}x  {}", r.key, r.id, r.age_days, r.asks, r.reason);
    if !r.rulings.is_empty() {
        let shown: Vec<&str> = r.rulings.iter().rev().take(4).map(String::as_str).collect();
        l.push_str(&format!("  [{}]", shown.join(", ")));
    }
    l
}

/// `atlas backlog ARGS`. It writes only for `dismiss`.
///
/// # Errors
///
/// A message for a bad argument or an unreadable store.
pub fn cli(args: &[String], store: &mut MemoryStore, index_db: Option<&Path>, s: &Settings, orch: &crate::orchestrate::OrchSettings) -> std::result::Result<ExitCode, String> {
    if args.first().is_some_and(|a| a == "dismiss") {
        let id = args.get(1).filter(|a| !a.starts_with("--")).ok_or_else(|| format!("dismiss needs an ID\n{USAGE}"))?;
        let because = flag(args, "--because").ok_or_else(|| format!("dismiss needs --because KEY: the record that says why\n{USAGE}"))?;
        let key = dismiss(store, id, &because).map_err(|e| e.to_string())?;
        println!("dismissed {id} ({key}, because {because})");
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(a) = args.iter().find(|a| a.starts_with("--") && !matches!(a.as_str(), "--all" | "--json" | "--as-of" | "--transcript")) {
        return Err(format!("unknown option {a}\n{USAGE}"));
    }
    let now = crate::memory::now();
    let at = flag(args, "--as-of").unwrap_or_else(|| now[..16].to_string());
    let mut running = Vec::new();
    if let Some(tr) = flag(args, "--transcript") {
        let dir = crate::orchestrate::subagents_dir(&tr);
        let r = crate::orchestrate::Agents::new(None).running(orch, &dir, std::time::SystemTime::now()).map_err(|e| format!("{}: {e}", dir.display()))?;
        running = r.iter().map(|x| crate::orchestrate::brief_of(&dir.join(format!("agent-{}.jsonl", x.id))).unwrap_or_default()).collect();
    }
    let all = args.iter().any(|a| a == "--all");
    let report = comb(store, s, &Input { as_of: &at, running: &running, index_db, all }).map_err(|e| e.to_string())?;
    if args.iter().any(|a| a == "--json") {
        let rows: Vec<Value> = report.rows.iter().map(row_json).collect();
        println!("{}", crate::json!({"as_of": at.clone(), "hidden": report.hidden, "rows": rows}));
        return Ok(ExitCode::SUCCESS);
    }
    let aging: Vec<&Row> = report.of(State::Overdue).collect();
    let waiting: Vec<&Row> = report.of(State::Waiting).collect();
    println!("backlog as of {at} (proposed; the written order is unchanged):");
    println!("{} aging (days since placed or due; not a call to pull forward):", aging.len());
    for r in &aging {
        println!("  {}", row_line(r));
    }
    println!("{} waiting:", waiting.len());
    for r in &waiting {
        println!("  {}", row_line(r));
    }
    if all {
        let unplaced: Vec<&Row> = report.of(State::Unplaced).collect();
        println!("{} ruled, never placed:", unplaced.len());
        for r in &unplaced {
            println!("  {}", row_line(r));
        }
        let stale: Vec<&str> = report.of(State::Stale).map(|r| r.id.as_str()).collect();
        println!("{} stale (ruled over {} days ago, never asked): {}", stale.len(), s.stale_days, stale.join(", "));
    }
    println!("{} hidden (live, done, folded or dismissed). `atlas backlog dismiss ID --because KEY` silences one for good.", report.hidden);
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::stated;

    #[test]
    fn stated_takes_a_whole_date_only() {
        assert_eq!(stated("ruled 2026-11-04 by the user"), crate::checkpoint::epoch_secs("2026-11-04T00:00"));
        assert!(stated("2026-11-04").is_some());
        // A digit on either edge, a 1-led year, or no date shape at all.
        assert_eq!(stated("12026-11-04"), None);
        assert_eq!(stated("2026-11-041"), None);
        assert_eq!(stated("1999-01-01"), None);
        assert_eq!(stated("abcdefghij"), None);
    }
    #[test]
    fn cue_at_looks_past_a_failed_word_edge() {
        let cues = vec!["start".to_string()];
        assert_eq!(super::cue_at(" started then start ", &cues), Some((14, 5)));
        assert_eq!(super::cue_at(" started ", &cues), None);
    }
}
