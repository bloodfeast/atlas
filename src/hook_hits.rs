//! Hook hit rate: whether a record a hook injected was used afterwards.
//!
//! The hooks log every injected key to `hook_inject` (on the log thread,
//! after the answer): the hook, the session, the transcript the injection
//! landed in, the key, the scores it passed the gate on, its injected bytes
//! and its counterfactual (the bytes reading it where it is written would
//! cost, [`crate::tokens::record_read`]). `atlas --hook-report` then reads
//! each transcript and marks an injection a **hit** by a fixed rule, the
//! first that applies, looking only at assistant messages timestamped after
//! the injection:
//!
//! 1. `key`: a tool call's input contains the key (a `recall key=`, a
//!    `lookup`, a grep for it). For `lane:NAME`, a tool input names a record
//!    of the lane (`journal:NAME:` or `handoff:NAME:`) or a `recall` of the
//!    lane.
//! 2. `source`: a tool call reads the record's source location: for a
//!    memory, an input naming its file (`KEY.md`); for any other record, a
//!    `Grep` whose path or glob names the file of one of the record's
//!    exports and whose pattern matches the record's title or text.
//! 3. `quote`: an assistant text or a tool input contains a run of
//!    [`SHINGLE`] consecutive words of the record's title and text (words
//!    are ASCII letter and digit runs, lowercased).
//!
//! Past sessions logged before the table existed are reconstructed from
//! their transcripts (`--transcripts DIR`): each `hook_success` attachment's
//! injected context gives the keys, and the hook's own scoring
//! ([`crate::hooks::Hooks::scores`]) on the prompt it answered gives the
//! scores. A transcript with logged rows is never reconstructed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::db::{params, Connection};

use crate::error::Result;
use crate::json::Value;
use crate::memory::MemoryStore;

/// Words in a quoted run.
pub const SHINGLE: usize = 8;

/// One injected key.
#[derive(Debug, Clone, PartialEq)]
pub struct Injection {
    /// When it was injected (UTC, `YYYY-MM-DDTHH:MM:SS.mmmZ`).
    pub time: String,
    /// `UserPromptSubmit`, `SubagentStart` or `PreToolUse`.
    pub hook: String,
    /// The session.
    pub session: String,
    /// The subagent, empty in the main session.
    pub agent: String,
    /// The transcript the injection landed in.
    pub transcript: String,
    /// The record key, or `lane:NAME`.
    pub key: String,
    /// The negated bm25 it passed on.
    pub fts: Option<f64>,
    /// The cosine it passed on.
    pub cos: Option<f32>,
    /// Injected bytes.
    pub bytes: usize,
    /// Its counterfactual in bytes.
    pub read: usize,
}

/// Logs one injected key.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn log_injection(conn: &Connection, hook: &str, session: &str, agent: &str, transcript: &str, k: &crate::hooks::Injected, read: usize) -> Result<()> {
    let time = crate::store::utc(SystemTime::now());
    crate::store::guarded(conn, || {
        conn.prepare_cached("INSERT INTO hook_inject(time, session, agent, transcript, hook, key, fts, cos, bytes, read) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)")?
            .execute(params![time, session, agent, transcript, hook, k.key, k.fts, k.cos.map(f64::from), k.bytes as i64, read as i64])?;
        Ok(())
    })
}

/// The logged injections, oldest first.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn logged(conn: &Connection, since: &str) -> Result<Vec<Injection>> {
    conn.execute_batch(crate::tokens::TOKEN_TABLE)?;
    let mut st = conn.prepare("SELECT time, hook, session, agent, transcript, key, fts, cos, bytes, read FROM hook_inject WHERE time >= ?1 ORDER BY id")?;
    let rows = st.query_map([since], |r| injection_at(r, 0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// An [`Injection`] from a row's columns `time, hook, session, agent,
/// transcript, key, fts, cos, bytes, read` from column `o` on.
fn injection_at(r: &crate::db::Row<'_>, o: usize) -> crate::db::Result<Injection> {
    Ok(Injection {
        time: r.get(o)?,
        hook: r.get(o + 1)?,
        session: r.get(o + 2)?,
        agent: r.get(o + 3)?,
        transcript: r.get(o + 4)?,
        key: r.get(o + 5)?,
        fts: r.get(o + 6)?,
        cos: r.get::<_, Option<f64>>(o + 7)?.map(|c| c as f32),
        bytes: r.get::<_, i64>(o + 8)? as usize,
        read: r.get::<_, i64>(o + 9)? as usize,
    })
}

/// The record keys already pointed to in one scope, read back from
/// `hook_inject` so a server restart or a command hook (a process per
/// event) does not point to them again: a main session's prompts (`agent`
/// empty, rows of `session` with no agent) or one subagent (`agent`, its
/// rows in any session). Lane (`lane:`) rows are left out: a lane view's
/// records are not logged one by one, so its key alone would hide the
/// view and leave its records unmarked. Read only and fail open: a missing
/// database, table or lock is an empty set.
pub fn injected_keys(db: &Path, session: &str, agent: &str) -> HashSet<String> {
    if session.is_empty() && agent.is_empty() {
        return HashSet::new();
    }
    let read = || -> crate::db::Result<HashSet<String>> {
        let conn = Connection::open_with_flags(db, crate::db::OpenFlags::SQLITE_OPEN_READ_ONLY | crate::db::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        let mut st = if agent.is_empty() {
            conn.prepare("SELECT key FROM hook_inject WHERE session = ?1 AND agent = '' AND key NOT LIKE 'lane:%'")?
        } else {
            conn.prepare("SELECT key FROM hook_inject WHERE agent = ?1 AND key NOT LIKE 'lane:%'")?
        };
        let id = if agent.is_empty() { session } else { agent };
        let keys = st.query_map([id], |r| r.get::<_, String>(0))?.collect::<crate::db::Result<HashSet<String>>>()?;
        Ok(keys)
    };
    read().unwrap_or_default()
}

/// One key parsed out of an injected context, with its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedKey {
    /// The key, or `lane:NAME`.
    pub key: String,
    /// The bytes of its part of the context.
    pub bytes: usize,
}

/// The keys an injected context names, in order: pointer lines
/// (`- KEY: title`), record headers (`== KEY ...`, the older body format)
/// and lane blocks (`lane NAME: N entries`, whose own records count as the
/// lane). Other lines (a cap warning, `+N more`) name nothing.
pub fn parse_context(text: &str) -> Vec<ParsedKey> {
    let mut out: Vec<ParsedKey> = Vec::new();
    let mut in_lane = false;
    for line in text.split_inclusive('\n') {
        let l = line.trim_end();
        if let Some(rest) = l.strip_prefix("lane ") {
            if let Some((name, tail)) = rest.split_once(": ") {
                if tail.ends_with(" entries") || tail.ends_with(" entry") {
                    in_lane = true;
                    out.push(ParsedKey { key: format!("lane:{name}"), bytes: line.len() });
                    continue;
                }
            }
        }
        if l.starts_with("atlas recall (") {
            in_lane = false;
            continue;
        }
        if in_lane {
            if let Some(last) = out.last_mut() {
                last.bytes += line.len();
            }
            continue;
        }
        let key = if let Some(rest) = l.strip_prefix("- ") {
            rest.split_once(": ").map(|(k, _)| k).filter(|k| !k.is_empty() && !k.contains(' ') && k.contains([':', '-']))
        } else if let Some(rest) = l.strip_prefix("== ") {
            rest.split(' ').next().filter(|k| !k.is_empty())
        } else {
            None
        };
        match key {
            Some(k) => out.push(ParsedKey { key: k.to_string(), bytes: line.len() }),
            // A body line of the older format belongs to the record above.
            None if !l.starts_with('+') => {
                if let Some(last) = out.last_mut() {
                    if !last.key.starts_with("lane:") {
                        last.bytes += line.len();
                    }
                }
            }
            None => {}
        }
    }
    out
}

/// A past injection read from a transcript, before scoring.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// The injection (scores and counterfactual not yet known).
    pub injection: Injection,
    /// The text the hook answered (the prompt or the brief).
    pub prompt: String,
}

fn transcripts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

fn message_text(v: &Value) -> Option<String> {
    match v.pointer("/message/content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            if a.iter().any(|x| x.get("type").and_then(Value::as_str) == Some("tool_result")) {
                return None;
            }
            let t: Vec<&str> = a.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect();
            (!t.is_empty()).then(|| t.join("\n"))
        }
        _ => None,
    }
}

/// The injections recorded in one transcript's `hook_success` attachments
/// at or after `since`.
pub fn found_in(path: &Path, since: &str) -> Vec<Found> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut brief: Option<Option<String>> = None;
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("\"hook_success\"") || !line.contains("additionalContext") {
            continue;
        }
        let Ok(v) = crate::json::parse(line) else { continue };
        let Some(att) = v.get("attachment") else { continue };
        let hook = att.get("hookEvent").and_then(Value::as_str).unwrap_or("");
        if !matches!(hook, "UserPromptSubmit" | "SubagentStart" | "PreToolUse") {
            continue;
        }
        let time = v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        if time.as_str() < since {
            continue;
        }
        let Some(ctx) = att.get("stdout").and_then(Value::as_str).and_then(|s| crate::json::parse(s).ok()) else { continue };
        let Some(context) = ctx.pointer("/hookSpecificOutput/additionalContext").and_then(Value::as_str) else { continue };
        let keys = parse_context(context);
        if keys.is_empty() {
            continue;
        }
        let prompt = if hook == "UserPromptSubmit" {
            // The prompt is the next user message that is not a tool result.
            lines[i + 1..].iter().take(60).filter(|l| l.contains("\"user\"")).filter_map(|l| crate::json::parse(l).ok()).filter(|u| u.get("type").and_then(Value::as_str) == Some("user")).find_map(|u| message_text(&u)).unwrap_or_default()
        } else {
            brief.get_or_insert_with(|| crate::hooks::first_user_text(path)).clone().unwrap_or_default()
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        for k in keys {
            out.push(Found {
                injection: Injection {
                    time: time.clone(),
                    hook: hook.to_string(),
                    session: s("sessionId"),
                    agent: s("agentId"),
                    transcript: path.to_string_lossy().into_owned(),
                    key: k.key,
                    fts: None,
                    cos: None,
                    bytes: k.bytes,
                    read: 0,
                },
                prompt: prompt.clone(),
            });
        }
    }
    out
}

/// The injections recorded in the transcripts below `dir` at or after
/// `since`, except in transcripts named in `skip`.
pub fn found_below(dir: &Path, since: &str, skip: &HashSet<String>) -> Vec<Found> {
    transcripts(dir).iter().filter(|p| !skip.contains(&*p.to_string_lossy())).flat_map(|p| found_in(p, since)).collect()
}

/// One assistant act after which a hit may be found.
#[derive(Debug, Clone, Default)]
struct Act {
    time: String,
    /// The tool, empty for assistant text.
    tool: String,
    /// The tool input as JSON text, or the text.
    text: String,
    /// A `Grep` call's pattern, case flag, and path and glob.
    grep: Option<(String, bool, String)>,
    shingles: Vec<u64>,
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect()
}

/// The hashes of every run of [`SHINGLE`] words of a text.
pub fn shingles(text: &str) -> Vec<u64> {
    let w = words(text);
    if w.len() < SHINGLE {
        return Vec::new();
    }
    w.windows(SHINGLE)
        .map(|run| {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for word in run {
                for b in word.bytes().chain(std::iter::once(b' ')) {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(0x0100_0000_01b3);
                }
            }
            h
        })
        .collect()
}

fn acts(path: &Path) -> Vec<Act> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out = Vec::new();
    for line in text.lines() {
        if !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = crate::json::parse(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let time = v.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(Value::Array(items)) = v.pointer("/message/content") else { continue };
        for x in items {
            match x.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let t = x.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                    out.push(Act { time: time.clone(), shingles: shingles(&t), text: t, ..Act::default() });
                }
                Some("tool_use") => {
                    let tool = x.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let input = x.get("input").cloned().unwrap_or(Value::Null);
                    let grep = (tool == "Grep").then(|| {
                        let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        (s("pattern"), input.get("-i").and_then(Value::as_bool).unwrap_or(false), format!("{} {}", s("path"), s("glob")))
                    });
                    let t = input.to_string();
                    out.push(Act { time: time.clone(), tool, shingles: shingles(&t), text: t, grep });
                }
                _ => {}
            }
        }
    }
    out
}

/// Hashes a shingle, already an FNV hash, as itself.
#[derive(Default)]
struct ShingleHasher(u64);

impl std::hash::Hasher for ShingleHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*b);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

/// Each shingle of a transcript's acts, with the acts it is in (their
/// indices, ascending): a quote check looks up the record's shingles
/// rather than scanning every later act's (#348).
type QuoteIndex = HashMap<u64, Vec<u32>, std::hash::BuildHasherDefault<ShingleHasher>>;

fn quote_index(acts: &[Act]) -> QuoteIndex {
    let mut index = QuoteIndex::default();
    for (i, a) in acts.iter().enumerate() {
        let i = i as u32;
        for &s in &a.shingles {
            let at = index.entry(s).or_default();
            if at.last() != Some(&i) {
                at.push(i);
            }
        }
    }
    index
}

/// What a hit check needs to know of a record.
struct RecordInfo {
    memory: bool,
    text: String,
    files: Vec<String>,
    shingles: HashSet<u64>,
}

/// Marks injections hit or not.
pub struct Judge<'a> {
    mem: &'a MemoryStore,
    export_files: HashMap<String, String>,
    acts: HashMap<String, Vec<Act>>,
    /// Each transcript's [`QuoteIndex`], built at its first quote check.
    quotes: HashMap<String, QuoteIndex>,
    records: HashMap<String, Option<RecordInfo>>,
    /// Only acts at or before this time are read (empty: all).
    until: String,
    /// Transcripts read and parsed so far.
    parsed: usize,
}

impl<'a> Judge<'a> {
    /// A judge over the store, given each export's file name.
    pub fn new(mem: &'a MemoryStore, exports: &[crate::memory::export::ExportSpec]) -> Judge<'a> {
        let export_files = exports.iter().map(|e| (e.name.clone(), e.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default())).collect();
        Judge { mem, export_files, acts: HashMap::new(), quotes: HashMap::new(), records: HashMap::new(), until: String::new(), parsed: 0 }
    }

    /// How many transcripts this judge has read and parsed.
    pub fn parsed(&self) -> usize {
        self.parsed
    }

    /// The same judge reading only the transcript acts at or before `until`
    /// (an RFC 3339 UTC time): the token report bounds it to the ledger's
    /// last row, so both sides cover the same window and a rerun on the
    /// same store gives the same verdicts while sessions go on.
    pub fn until(mut self, until: &str) -> Judge<'a> {
        self.until = until.to_string();
        self
    }

    fn record(&mut self, key: &str) -> Option<&RecordInfo> {
        if !self.records.contains_key(key) {
            let info = self.mem.head(key).ok().flatten().map(|h| {
                let text = format!("{}\n{}", h.title, h.body);
                let files = self.mem.exports_of(key).unwrap_or_default().iter().filter_map(|e| self.export_files.get(e).cloned()).filter(|f| !f.is_empty()).collect();
                RecordInfo { memory: h.kind == "memory", shingles: shingles(&text).into_iter().collect(), text, files }
            });
            self.records.insert(key.to_string(), info);
        }
        self.records.get(key).and_then(Option::as_ref)
    }

    /// The rule that marks this injection a hit (`key`, `source`, `quote`),
    /// or `None`.
    pub fn hit(&mut self, inj: &Injection) -> Option<&'static str> {
        self.hit_at(inj).map(|(rule, _)| rule)
    }

    /// [`Judge::hit`], with the time of the first act the rule counted: when
    /// the record was used (#297, S3).
    pub fn hit_at(&mut self, inj: &Injection) -> Option<(&'static str, String)> {
        if !self.acts.contains_key(&inj.transcript) {
            let mut a = acts(Path::new(&inj.transcript));
            self.parsed += 1;
            if !self.until.is_empty() {
                a.retain(|a| a.time <= self.until);
            }
            self.acts.insert(inj.transcript.clone(), a);
        }
        let key = inj.key.clone();
        // A lane view, or the head line an agent's report gets: either is
        // used when the lane's records or its recall are read after it.
        if let Some(lane) = key.strip_prefix("lane:").or_else(|| key.strip_prefix("lane-head:")) {
            let (j, h, l) = (format!("journal:{lane}:"), format!("handoff:{lane}:"), format!("\"lane\":\"{lane}\""));
            let acts = &self.acts[&inj.transcript];
            let used = first(
                acts.iter()
                    .filter(|a| a.time > inj.time && !a.tool.is_empty())
                    .filter(|a| a.text.contains(&j) || a.text.contains(&h) || (a.tool.ends_with("recall") && a.text.contains(&l))),
            );
            return used.map(|t| ("key", t));
        }
        if let Some(t) =
            first(self.acts[&inj.transcript].iter().filter(|a| a.time > inj.time && !a.tool.is_empty() && a.text.contains(&key)))
        {
            return Some(("key", t));
        }
        let acts = self.acts.remove(&inj.transcript).unwrap_or_default();
        let mut quotes = self.quotes.remove(&inj.transcript);
        let verdict = self.record(&key).and_then(|r| {
            let after = || acts.iter().filter(|a| a.time > inj.time);
            if r.memory {
                let file = format!("{key}.md");
                if let Some(t) = first(after().filter(|a| !a.tool.is_empty() && a.text.contains(&file))) {
                    return Some(("source", t));
                }
            } else {
                let source = first(after().filter(|a| {
                    let Some((pattern, fold, place)) = &a.grep else { return false };
                    r.files.iter().any(|f| place.contains(f.as_str()))
                        && crate::regex::RegexBuilder::new(pattern).case_insensitive(*fold).build().is_ok_and(|re| re.is_match(&r.text))
                }));
                if let Some(t) = source {
                    return Some(("source", t));
                }
            }
            // The earliest later act sharing a shingle, looked up by the
            // record's shingles in the transcript's index (#348).
            let index = quotes.get_or_insert_with(|| quote_index(&acts));
            let after = r.shingles.iter().filter_map(|s| index.get(s)).flatten().map(|&i| &acts[i as usize]).filter(|a| a.time > inj.time);
            first(after).map(|t| ("quote", t))
        });
        if let Some(q) = quotes {
            self.quotes.insert(inj.transcript.clone(), q);
        }
        self.acts.insert(inj.transcript.clone(), acts);
        verdict
    }
}

/// The earliest time of some acts, `None` when there is none.
fn first<'a>(acts: impl Iterator<Item = &'a Act>) -> Option<String> {
    acts.map(|a| a.time.as_str()).min().map(str::to_string)
}

/// The hooks' credit by hit (#217): for each logged injection the
/// [`Judge`] marks a hit, its `read` (the counterfactual the hook's
/// token-log row summed for it), by `hook:<event>` and day; an injection
/// that is no hit credits nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HitCredit {
    /// Credited bytes by (`hook:<event>`, `YYYY-MM-DD`).
    pub by: std::collections::BTreeMap<(String, String), i64>,
    /// Injections judged.
    pub injected: usize,
    /// Of them, hits.
    pub hits: usize,
    /// Each hit's place and credited bytes, for the credit side's re-read
    /// ([`crate::token_cost::Credits::hits`], #294).
    pub at: Vec<crate::token_cost::Hit>,
    /// Of the injections, those whose stored verdict was used (their
    /// transcript not read, #348).
    pub stored: usize,
    /// Transcripts read and parsed by this run.
    pub parsed: usize,
}

/// The hit judge's version (#357). Bump it whenever a change could change
/// a stored verdict: [`SHINGLE`] or how words are cut, how a transcript's
/// acts are parsed, the key, source or quote rules, or the export names
/// they match. A stored verdict of another version is judged again while
/// its transcript exists. A bump does not reach the verdicts of deleted
/// transcripts, which keep their old credit; to retract those, delete the
/// rows (or rename the table).
pub const HIT_JUDGE: i64 = 1;

/// The stored hit verdicts (#348): each settled `hook_inject` row's rule
/// (`key`, `source` or `quote`; NULL for no hit) and the time it was used,
/// by its `hook_inject` id, with the judge's version and its transcript's
/// [`crate::tokens::Stamp`] (#357). Created on first use, as `read_verdict`
/// is. The row's `key` is joined too: `hook_inject.id` has no
/// AUTOINCREMENT, so an id reused after its rows were deleted must not
/// inherit another injection's verdict (a prune of `hook_inject` should
/// delete the matching rows here as well).
const HIT_VERDICT_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS hit_verdict(id INTEGER PRIMARY KEY, key TEXT NOT NULL, judge INTEGER NOT NULL, mtime TEXT NOT NULL, size INTEGER NOT NULL, rule TEXT, used TEXT)";

/// A stored rule back as the judge's own name; `None` for a name it does
/// not give (the row is judged again).
fn rule_named(rule: &str) -> Option<&'static str> {
    ["key", "source", "quote"].into_iter().find(|r| *r == rule)
}

/// Judges every logged injection and sums the hits' reads ([`HitCredit`]).
/// An injection whose transcript has been still for
/// [`crate::tokens::SETTLED`] (and ends within the judge's bound) has its
/// verdict stored, as `read_credit` does; it is judged again only when the
/// judge's version or the transcript changes, or a run is bounded before
/// the transcript's last change (#357).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn hit_credit(conn: &Connection, judge: &mut Judge) -> Result<HitCredit> {
    hit_credit_at(conn, judge, SystemTime::now(), crate::tokens::SETTLED)
}

/// [`hit_credit`] as of `now`, storing the verdicts of transcripts still
/// for `settled`.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn hit_credit_at(conn: &Connection, judge: &mut Judge, now: SystemTime, settled: std::time::Duration) -> Result<HitCredit> {
    conn.execute_batch(crate::tokens::TOKEN_TABLE)?;
    conn.execute_batch(HIT_VERDICT_TABLE)?;
    type Verdict = Option<(&'static str, String)>;
    // A stored verdict: the rule and use, the judge's version, the stamp.
    type Stored = Option<(Verdict, i64, crate::tokens::Stamp)>;
    let rows: Vec<(i64, Injection, Stored)> = conn
        .prepare(
            "SELECT i.id, i.time, i.hook, i.session, i.agent, i.transcript, i.key, i.fts, i.cos, i.bytes, i.read, v.id, v.rule, v.used, v.judge, v.mtime, v.size \
             FROM hook_inject i LEFT JOIN hit_verdict v ON v.id = i.id AND v.key = i.key ORDER BY i.id",
        )?
        .query_map([], |r| {
            let verdict = match r.get::<_, Option<i64>>(11)? {
                None => None,
                Some(_) => match (r.get::<_, Option<String>>(12)?, r.get::<_, Option<String>>(13)?) {
                    (None, _) => Some(None),
                    (Some(rule), Some(used)) => rule_named(&rule).map(|rule| Some((rule, used))),
                    (Some(_), None) => None,
                },
            };
            let stored = match verdict {
                Some(v) => Some((v, r.get(14)?, crate::tokens::Stamp { mtime: r.get(15)?, size: r.get(16)? })),
                None => None,
            };
            Ok((r.get(0)?, injection_at(r, 1)?, stored))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut c = HitCredit::default();
    let parsed = judge.parsed();
    // Each transcript with stored rows as it is now, stat'ed once.
    let mut stamps: HashMap<String, Option<crate::tokens::Stamp>> = HashMap::new();
    // Whether each transcript's verdicts may be stored, asked once after it
    // is read, and the stamp to store with them.
    let mut still: HashMap<String, Option<crate::tokens::Stamp>> = HashMap::new();
    let mut verdicts: Vec<(i64, String, Verdict, crate::tokens::Stamp)> = Vec::new();
    for (id, i, stored) in rows {
        c.injected += 1;
        // A stored verdict stands while its transcript is unchanged (or gone)
        // and was still at the judge's bound (#357).
        let usable = stored.filter(|(_, version, s)| {
            (judge.until.is_empty() || s.mtime <= judge.until) && {
                let now = stamps.entry(i.transcript.clone()).or_insert_with(|| crate::tokens::stamp(Path::new(&i.transcript)));
                crate::tokens::verdict_current(Some(*version), HIT_JUDGE, (Some(s.mtime.as_str()), Some(s.size)), now.as_ref())
            }
        });
        let verdict = if let Some((v, _, _)) = usable {
            c.stored += 1;
            v
        } else {
            let v = judge.hit_at(&i);
            let keep = still.entry(i.transcript.clone()).or_insert_with(|| {
                crate::tokens::settled_since(Path::new(&i.transcript), now, settled).filter(|s| judge.until.is_empty() || s.mtime <= judge.until)
            });
            if let Some(s) = keep {
                verdicts.push((id, i.key.clone(), v.clone(), s.clone()));
            }
            v
        };
        if let Some((_, used)) = verdict {
            c.hits += 1;
            *c.by.entry((format!("hook:{}", i.hook), i.time.get(..10).unwrap_or("").to_string())).or_default() += i.read as i64;
            c.at.push(crate::token_cost::Hit {
                session: i.session,
                agent: i.agent,
                time: i.time,
                used,
                hook: i.hook,
                read: i.read as i64,
                bytes: i.bytes as i64,
            });
        }
    }
    c.parsed = judge.parsed() - parsed;
    if !verdicts.is_empty() {
        crate::store::guarded(conn, || {
            let mut st = conn.prepare_cached("INSERT OR REPLACE INTO hit_verdict(id, key, judge, mtime, size, rule, used) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
            for (id, key, v, s) in &verdicts {
                st.execute(params![id, key, HIT_JUDGE, s.mtime, s.size, v.as_ref().map(|v| v.0), v.as_ref().map(|v| v.1.as_str())])?;
            }
            Ok(())
        })?;
    }
    Ok(c)
}

/// One hook's totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookTotals {
    /// Injected keys.
    pub injected: usize,
    /// Of them, hits.
    pub hits: usize,
    /// Hits by rule: key, source, quote.
    pub by_rule: [usize; 3],
    /// Bytes injected.
    pub bytes: usize,
    /// Bytes injected for keys that were hits.
    pub hit_bytes: usize,
}

/// Totals by hook, from injections and their verdicts.
pub fn totals(rows: &[(Injection, Option<&'static str>)]) -> Vec<(String, HookTotals)> {
    let mut by: std::collections::BTreeMap<String, HookTotals> = std::collections::BTreeMap::new();
    for (inj, hit) in rows {
        let t = by.entry(inj.hook.clone()).or_default();
        t.injected += 1;
        t.bytes += inj.bytes;
        if let Some(rule) = hit {
            t.hits += 1;
            t.hit_bytes += inj.bytes;
            t.by_rule[match *rule {
                "key" => 0,
                "source" => 1,
                _ => 2,
            }] += 1;
        }
    }
    by.into_iter().collect()
}

/// One point of the relevance-floor curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloorPoint {
    /// The cosine floor.
    pub floor: f32,
    /// Injected keys kept.
    pub kept: usize,
    /// Of them, hits.
    pub hits: usize,
    /// Bytes injected for the kept keys.
    pub bytes: usize,
    /// Bytes the kept hits save (their counterfactual times the scale)
    /// less the bytes injected.
    pub net: f64,
}

/// The curve at one floor: keys with a cosine below it are dropped; keys
/// without a cosine are kept whatever the floor.
pub fn at_floor(rows: &[(Injection, Option<&'static str>)], floor: f32, scale: f64) -> FloorPoint {
    let mut p = FloorPoint { floor, kept: 0, hits: 0, bytes: 0, net: 0.0 };
    for (i, hit) in rows {
        if i.cos.is_some_and(|c| c < floor) {
            continue;
        }
        p.kept += 1;
        p.bytes += i.bytes;
        p.net -= i.bytes as f64;
        if hit.is_some() {
            p.hits += 1;
            p.net += i.read as f64 * scale;
        }
    }
    p
}

/// The relevance floor (`recall.inject_floor`) tuned on hit data, by a
/// fixed objective: the floor that maximises the bytes kept hits save net
/// of the bytes injected ([`FloorPoint::net`]). Candidates are the active
/// floor, every cosine above it in the data, and just above the highest; a
/// tie goes to the lower floor (inject more). The hook logs only what it
/// injected, so no row sits below the active floor and the data cannot
/// speak there: a floor below it is never recommended.
///
/// # Arguments
///
/// * `rows` - injections and verdicts (from `--hook-report`).
/// * `scale` - the share of a hit's counterfactual credited (1 is the
///   ledger's rule-based credit; the measured ratio over the ledger ratio is
///   the measured alternative).
/// * `active` - the floor in force (`recall.inject_floor`), the lowest
///   candidate.
pub fn tune(rows: &[(Injection, Option<&'static str>)], scale: f64, active: f32) -> FloorPoint {
    let mut cands: Vec<f32> = rows.iter().filter_map(|(i, _)| i.cos).filter(|c| *c > active).collect();
    // Above every cosine: inject only the keys without one.
    if let Some(top) = cands.iter().copied().reduce(f32::max) {
        cands.push(top.next_up());
    }
    cands.push(active);
    cands.sort_by(f32::total_cmp);
    cands.dedup();
    let mut best = at_floor(rows, active, scale);
    for f in cands {
        let p = at_floor(rows, f, scale);
        if p.net > best.net {
            best = p;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inj(cos: Option<f32>, bytes: usize, read: usize) -> Injection {
        Injection { time: String::new(), hook: "UserPromptSubmit".into(), session: String::new(), agent: String::new(), transcript: String::new(), key: "k".into(), fts: None, cos, bytes, read }
    }

    #[test]
    fn the_floor_maximises_net_bytes_and_ties_go_low() {
        let rows = vec![
            (inj(Some(0.40), 100, 1000), None),
            (inj(Some(0.50), 100, 1000), Some("key")),
            (inj(Some(0.45), 100, 1000), None),
            (inj(None, 100, 1000), None),
        ];
        // Dropping both misses below 0.50 saves 200 bytes and loses no hit.
        let b = tune(&rows, 1.0, 0.0);
        assert_eq!((b.floor, b.kept, b.hits), (0.50, 2, 1));
        assert_eq!(b.net, 1000.0 - 200.0);
        // A hit worth less than the misses' cost: the floor drops it too, but
        // the key without a cosine stays.
        let b = tune(&rows, 0.05, 0.0);
        assert_eq!((b.kept, b.hits), (1, 0));
        // All hits: nothing to gain, the lowest floor.
        let all: Vec<_> = rows.iter().map(|(i, _)| (i.clone(), Some("quote"))).collect();
        assert_eq!(tune(&all, 1.0, 0.0).floor, 0.0);
    }

    /// The hook logs only what it injected: under an active floor of 0.60
    /// no row sits below it, so every floor under 0.60 scores the same and
    /// the data cannot speak there. The tuner must not recommend one.
    #[test]
    fn the_tuner_never_goes_below_the_active_floor() {
        let rows =
            vec![(inj(Some(0.62), 100, 1000), Some("key")), (inj(Some(0.70), 100, 1000), Some("quote")), (inj(None, 100, 1000), None)];
        let b = tune(&rows, 1.0, 0.60);
        assert_eq!(b.floor, 0.60);
        assert_eq!((b.kept, b.hits), (3, 2));
    }

    #[test]
    fn parses_pointers_bodies_and_lanes() {
        let ctx = "lane L-1: 2 entries\n# brief\n== journal:L-1:0001 journal/brief lane=L-1\nbody\natlas recall (related records; read one with recall key=KEY):\n- intent:r00001: a rule\n- widget-cache: Widget cache - bound\n+2 more: recall {query}\n";
        let k = parse_context(ctx);
        let keys: Vec<&str> = k.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["lane:L-1", "intent:r00001", "widget-cache"]);
        assert_eq!(k[0].bytes, "lane L-1: 2 entries\n# brief\n== journal:L-1:0001 journal/brief lane=L-1\nbody\n".len());
        assert_eq!(k[1].bytes, "- intent:r00001: a rule\n".len());
        let old = "atlas recall (relevance-gated; 1 records)\n== restore:s00001 section active v1\nline one\nline two\n";
        let k = parse_context(old);
        assert_eq!(k, vec![ParsedKey { key: "restore:s00001".into(), bytes: old.len() - "atlas recall (relevance-gated; 1 records)\n".len() }]);
    }

    #[test]
    fn shingles_need_eight_words_and_ignore_case_and_punctuation() {
        assert!(shingles("one two three four five six seven").is_empty());
        let a = shingles("One, two three-four five six seven EIGHT nine");
        let b = shingles("one two three four five six seven eight");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0], b[0]);
    }

    /// A judge over an empty store, its record and acts given, so `hit_at`
    /// reads neither the store nor a transcript.
    fn judge_of<'a>(mem: &'a MemoryStore, key: &str, record: Option<RecordInfo>, acts: Vec<Act>) -> Judge<'a> {
        let mut j = Judge::new(mem, &[]);
        j.records.insert(key.to_string(), record);
        j.acts.insert("t.jsonl".into(), acts);
        j
    }

    fn injection(key: &str) -> Injection {
        Injection { time: "2026-11-15T10:00:01.000Z".into(), transcript: "t.jsonl".into(), key: key.into(), ..inj(None, 10, 100) }
    }

    fn act(time: &str, tool: &str, text: &str) -> Act {
        Act { time: time.into(), tool: tool.into(), text: text.into(), shingles: shingles(text), grep: None }
    }

    /// #297, S-b: a hit's use time is the first act its rule counted after
    /// the injection, whatever order the acts are read in; one per rule.
    #[test]
    fn a_hit_is_used_at_the_first_act_its_rule_counts() {
        let dir = std::env::temp_dir().join(format!("atlas-hook-hits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mem = MemoryStore::open(&dir.join("memory.sqlite")).unwrap();
        let (before, first, later) = ("2026-11-15T10:00:00.000Z", "2026-11-15T10:00:03.000Z", "2026-11-15T10:00:05.000Z");
        // key: the key in a tool input.
        let k = |t: &str| act(t, "mcp__atlas__recall", "{\"key\":\"widget-cache\"}");
        let mut j = judge_of(&mem, "widget-cache", None, vec![k(later), k(before), k(first)]);
        assert_eq!(j.hit_at(&injection("widget-cache")), Some(("key", first.to_string())));
        // source: a Grep in the record's file whose pattern matches its text.
        let text = "gadget exporter rule\n- the gadget exporter writes with temp files and renames them into place.";
        let rec = || RecordInfo {
            memory: false,
            text: text.into(),
            files: vec!["INTENT.md".into()],
            shingles: shingles(text).into_iter().collect(),
        };
        let g = |t: &str| Act {
            grep: Some(("gadget exporter".into(), false, "H:/x/INTENT.md ".into())),
            ..act(t, "Grep", "{\"pattern\":\"gadget exporter\"}")
        };
        let mut j = judge_of(&mem, "rules:r00001", Some(rec()), vec![g(later), g(before), g(first)]);
        assert_eq!(j.hit_at(&injection("rules:r00001")), Some(("source", first.to_string())));
        // quote: a run of the record's words in the assistant's text.
        let q = |t: &str| act(t, "", "Per the rule, the gadget exporter writes with temp files and renames them into place.");
        let mut j = judge_of(&mem, "rules:r00001", Some(rec()), vec![q(later), q(before), q(first)]);
        assert_eq!(j.hit_at(&injection("rules:r00001")), Some(("quote", first.to_string())));
        drop(mem);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
