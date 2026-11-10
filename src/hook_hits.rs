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

use rusqlite::{params, Connection};

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
    let rows = st.query_map([since], |r| {
        Ok(Injection {
            time: r.get(0)?,
            hook: r.get(1)?,
            session: r.get(2)?,
            agent: r.get(3)?,
            transcript: r.get(4)?,
            key: r.get(5)?,
            fts: r.get(6)?,
            cos: r.get::<_, Option<f64>>(7)?.map(|c| c as f32),
            bytes: r.get::<_, i64>(8)? as usize,
            read: r.get::<_, i64>(9)? as usize,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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
    records: HashMap<String, Option<RecordInfo>>,
}

impl<'a> Judge<'a> {
    /// A judge over the store, given each export's file name.
    pub fn new(mem: &'a MemoryStore, exports: &[crate::memory::export::ExportSpec]) -> Judge<'a> {
        let export_files = exports.iter().map(|e| (e.name.clone(), e.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default())).collect();
        Judge { mem, export_files, acts: HashMap::new(), records: HashMap::new() }
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
        if !self.acts.contains_key(&inj.transcript) {
            self.acts.insert(inj.transcript.clone(), acts(Path::new(&inj.transcript)));
        }
        let key = inj.key.clone();
        if let Some(lane) = key.strip_prefix("lane:") {
            let (j, h, l) = (format!("journal:{lane}:"), format!("handoff:{lane}:"), format!("\"lane\":\"{lane}\""));
            let acts = &self.acts[&inj.transcript];
            let used = acts.iter().filter(|a| a.time > inj.time && !a.tool.is_empty()).any(|a| a.text.contains(&j) || a.text.contains(&h) || (a.tool.ends_with("recall") && a.text.contains(&l)));
            return used.then_some("key");
        }
        if self.acts[&inj.transcript].iter().any(|a| a.time > inj.time && !a.tool.is_empty() && a.text.contains(&key)) {
            return Some("key");
        }
        let acts = self.acts.remove(&inj.transcript).unwrap_or_default();
        let verdict = self.record(&key).and_then(|r| {
            let after = || acts.iter().filter(|a| a.time > inj.time);
            if r.memory {
                let file = format!("{key}.md");
                if after().any(|a| !a.tool.is_empty() && a.text.contains(&file)) {
                    return Some("source");
                }
            } else {
                for a in after() {
                    let Some((pattern, fold, place)) = &a.grep else { continue };
                    if !r.files.iter().any(|f| place.contains(f.as_str())) {
                        continue;
                    }
                    let re = crate::regex::RegexBuilder::new(pattern).case_insensitive(*fold).build();
                    if re.is_ok_and(|re| re.is_match(&r.text)) {
                        return Some("source");
                    }
                }
            }
            after().any(|a| a.shingles.iter().any(|s| r.shingles.contains(s))).then_some("quote")
        });
        self.acts.insert(inj.transcript.clone(), acts);
        verdict
    }
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
/// of the bytes injected ([`FloorPoint::net`]). Candidates are 0, every
/// cosine in the data, and just above the highest; a tie goes to the lower floor (inject more).
///
/// # Arguments
///
/// * `rows` - injections and verdicts (from `--hook-report`).
/// * `scale` - the share of a hit's counterfactual credited (1 is the
///   ledger's upper bound; the measured ratio over the upper bound is the
///   measured alternative).
pub fn tune(rows: &[(Injection, Option<&'static str>)], scale: f64) -> FloorPoint {
    let mut cands: Vec<f32> = rows.iter().filter_map(|(i, _)| i.cos).collect();
    // Above every cosine: inject only the keys without one.
    if let Some(top) = cands.iter().copied().reduce(f32::max) {
        cands.push(top.next_up());
    }
    cands.push(0.0);
    cands.sort_by(f32::total_cmp);
    cands.dedup();
    let mut best = at_floor(rows, 0.0, scale);
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
        let b = tune(&rows, 1.0);
        assert_eq!((b.floor, b.kept, b.hits), (0.50, 2, 1));
        assert_eq!(b.net, 1000.0 - 200.0);
        // A hit worth less than the misses' cost: the floor drops it too, but
        // the key without a cosine stays.
        let b = tune(&rows, 0.05);
        assert_eq!((b.kept, b.hits), (1, 0));
        // All hits: nothing to gain, the lowest floor.
        let all: Vec<_> = rows.iter().map(|(i, _)| (i.clone(), Some("quote"))).collect();
        assert_eq!(tune(&all, 1.0).floor, 0.0);
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
}
