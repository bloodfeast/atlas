//! `token-use`: where atlas's answer tokens go, from transcripts dated after
//! atlas, measured by the same episode rule as `calibrate-baseline`.
//!
//! Rules (fixed; the episode rule is the baseline's, with atlas's read tools
//! added as lookup calls):
//!
//! 1. Lookup calls: the baseline's (`calibrate::classify`), plus every call
//!    to an atlas read tool (`mcp__atlas__NAME` for NAME in
//!    `atlas::baseline::COMPARABLE`, `ids`, `symbol`, `slice`). Search kind:
//!    `lookup`, `search`, `grep`, `ids`, `symbol`, `recall {query}`. Read
//!    kind: `recall {key}` or `{lane}`, `section`, `read_note`, `slice`.
//! 2. Episodes: as the baseline (a search opens one; a read joins the open
//!    one when the previous tool call was a lookup call).
//! 3. Bytes: the tool result's text bytes, as the agent received it.
//! 4. Use: for each atlas answer, the share of its runs of 8 words
//!    (lowercased ASCII letter and digit runs) that appear in the same
//!    transcript's later assistant text or tool inputs. A share of 0 means
//!    nothing of the answer was quoted or carried into a later call.
//! 5. Mode: the tool, with `search`'s `mode` (default `substring`),
//!    `recall`'s form (`key`, `lane`, `query`) and `format: json` appended.
//! 6. Replay (`--replay`): each atlas call is run again, with its recorded
//!    arguments, against the configuration's stores, and its bytes are the
//!    replayed answer's; everything else (episodes, use) is as recorded. Two
//!    builds replayed on the same transcripts and stores compare answer
//!    shapes alone.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use atlas::json::Value;

use crate::calibrate::{classify, Kind, Targets};

/// One atlas answer.
#[derive(Debug, Clone)]
pub struct Answer {
    /// Tool and mode.
    pub mode: String,
    /// Result bytes.
    pub bytes: usize,
    /// Share of its 8-word runs used later.
    pub used: f64,
}

/// One episode.
#[derive(Debug, Clone, Default)]
pub struct Episode {
    /// Result bytes.
    pub bytes: usize,
    /// Calls in it.
    pub calls: usize,
    /// Of them, atlas calls.
    pub atlas: usize,
}

/// What one transcript gives.
#[derive(Debug, Clone, Default)]
pub struct Use {
    /// Atlas answers.
    pub answers: Vec<Answer>,
    /// Lookup episodes.
    pub episodes: Vec<Episode>,
}

const SHINGLE: usize = 8;

fn shingles(text: &str) -> Vec<u64> {
    let w: Vec<String> = text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect();
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

/// An atlas read tool's mode and kind, or `None` for other tools.
pub fn atlas_call(name: &str, input: &Value) -> Option<(String, Kind)> {
    let tool = name.strip_prefix("mcp__atlas__")?;
    let s = |k: &str| input.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    let json = if s("format") == Some("json") { " json" } else { "" };
    let (mode, kind) = match tool {
        "search" => (format!("search {}", s("mode").unwrap_or("substring")), Kind::Search),
        "recall" => match (s("key"), s("lane")) {
            (Some(_), _) => ("recall key".to_string(), Kind::Read),
            (None, Some(_)) => ("recall lane".to_string(), Kind::Read),
            _ => ("recall query".to_string(), Kind::Search),
        },
        "lookup" | "grep" | "ids" | "symbol" => (tool.to_string(), Kind::Search),
        "section" | "read_note" | "slice" => (tool.to_string(), Kind::Read),
        _ => return None,
    };
    Some((format!("{mode}{json}"), kind))
}

fn result_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Answers an atlas call (tool name without the `mcp__atlas__` prefix,
/// recorded input) with the bytes a replay returns.
pub type Replay<'a> = dyn FnMut(&str, &Value) -> usize + 'a;

/// Reads one transcript's calls at or after `since`; with `replay`, atlas
/// calls are answered again and measured by the replay.
pub fn scan(path: &Path, since: &str, t: &Targets, mut replay: Option<&mut Replay<'_>>) -> Use {
    let Ok(text) = std::fs::read_to_string(path) else { return Use::default() };
    // In order: (call id, atlas mode, kind) for tool calls; assistant text
    // and inputs with their order, for the use rule.
    type Call = (String, Option<String>, Option<Kind>, usize, Option<usize>);
    let mut calls: Vec<Call> = Vec::new();
    let mut later: Vec<(usize, Vec<u64>)> = Vec::new();
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut order = 0usize;
    for line in text.lines() {
        if !line.contains("\"tool_use\"") && !line.contains("\"tool_result\"") && !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = atlas::json::parse(line) else { continue };
        let ts = v.get("timestamp").and_then(Value::as_str).unwrap_or("");
        if ts < since {
            continue;
        }
        let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("");
        let Some(Value::Array(items)) = v.pointer("/message/content") else { continue };
        for x in items {
            order += 1;
            let id = |k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            match x.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let name = id("name");
                    let input = x.get("input").cloned().unwrap_or(Value::Null);
                    later.push((order, shingles(&input.to_string())));
                    let (mode, kind) = match atlas_call(&name, &input) {
                        Some((m, k)) => (Some(m), Some(k)),
                        None => (None, classify(&name, &input, cwd, t)),
                    };
                    let replayed = match (&mode, replay.as_mut()) {
                        (Some(_), Some(r)) => Some(r(name.trim_start_matches("mcp__atlas__"), &input)),
                        _ => None,
                    };
                    calls.push((id("id"), mode, kind, order, replayed));
                }
                Some("text") => later.push((order, shingles(x.get("text").and_then(Value::as_str).unwrap_or("")))),
                Some("tool_result") => {
                    results.insert(id("tool_use_id"), result_text(x.get("content")));
                }
                _ => {}
            }
        }
    }
    let mut u = Use::default();
    let mut prev: Option<Kind> = None;
    for (id, mode, kind, at, replayed) in calls {
        let Some(k) = kind else {
            prev = None;
            continue;
        };
        let res = results.get(&id).map(String::as_str).unwrap_or("");
        let b = replayed.unwrap_or(res.len());
        if let Some(m) = &mode {
            let mine: HashSet<u64> = shingles(res).into_iter().collect();
            let used = if mine.is_empty() {
                0.0
            } else {
                let seen: HashSet<u64> =
                    later.iter().filter(|(o, _)| *o > at).flat_map(|(_, s)| s.iter().copied()).filter(|h| mine.contains(h)).collect();
                seen.len() as f64 / mine.len() as f64
            };
            u.answers.push(Answer { mode: m.clone(), bytes: b, used });
        }
        let atlas = usize::from(mode.is_some());
        match (k, prev, u.episodes.last_mut()) {
            (Kind::Read, Some(_), Some(last)) => {
                last.bytes += b;
                last.calls += 1;
                last.atlas += atlas;
            }
            _ => u.episodes.push(Episode { bytes: b, calls: 1, atlas }),
        }
        prev = Some(k);
    }
    u
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(f64::total_cmp);
    v
}

/// Scans every transcript below `dir` and prints the breakdown.
pub fn report(dir: &Path, since: &str, t: &Targets, bpt: f64, baseline_median: Option<f64>, mut replay: Option<&mut Replay<'_>>) {
    let mut all = Use::default();
    let files = crate::calibrate::transcripts(dir);
    for (i, p) in files.iter().enumerate() {
        let u = scan(p, since, t, replay.as_deref_mut());
        all.answers.extend(u.answers);
        all.episodes.extend(u.episodes);
        if (i + 1) % 100 == 0 || i + 1 == files.len() {
            eprintln!("[{}/{}] transcripts scanned", i + 1, files.len());
        }
    }
    let tok = |b: f64| b / bpt;
    let total: f64 = all.answers.iter().map(|a| a.bytes as f64).sum();
    let every = sorted(all.answers.iter().map(|a| a.bytes as f64).collect());
    println!("atlas answers since {since}: {}, {:.0} tokens, median {:.0}", all.answers.len(), tok(total), tok(atlas::baseline::median(&every)));
    if let Some(bm) = baseline_median {
        println!("  ratio per answer, measured (pre-atlas episode median / atlas answer median): {:.1}x", bm / atlas::baseline::median(&every).max(1.0));
    }
    println!("mode\tcalls\tmedian\tp90\tmean\tshare\tused_median\tunused_calls");
    let mut by: BTreeMap<String, Vec<&Answer>> = BTreeMap::new();
    for a in &all.answers {
        by.entry(a.mode.clone()).or_default().push(a);
    }
    for (m, v) in &by {
        let b = sorted(v.iter().map(|a| a.bytes as f64).collect());
        let used = sorted(v.iter().map(|a| a.used).collect());
        let sum: f64 = b.iter().sum();
        println!(
            "{m}\t{}\t{:.0}\t{:.0}\t{:.0}\t{:.1}%\t{:.0}%\t{}",
            v.len(),
            tok(atlas::baseline::median(&b)),
            tok(atlas::baseline::quantile(&b, 0.9)),
            tok(sum / b.len() as f64),
            sum / total.max(1.0) * 100.0,
            atlas::baseline::median(&used) * 100.0,
            v.iter().filter(|a| a.used == 0.0).count()
        );
    }
    let with_atlas: Vec<&Episode> = all.episodes.iter().filter(|e| e.atlas > 0).collect();
    let eb = sorted(with_atlas.iter().map(|e| e.bytes as f64).collect());
    println!(
        "episodes since {since} (baseline rule, atlas calls counted as lookups): {} in all, {} with an atlas call",
        all.episodes.len(),
        with_atlas.len()
    );
    if !eb.is_empty() {
        let mut calls: BTreeMap<usize, usize> = BTreeMap::new();
        for e in &with_atlas {
            *calls.entry(e.calls.min(4)).or_default() += 1;
        }
        let dist: Vec<String> = calls.iter().map(|(c, n)| format!("{}{} calls: {n}", c, if *c == 4 { "+" } else { "" })).collect();
        println!(
            "  atlas episodes: tokens median {:.0}, mean {:.0}, p90 {:.0}; {}",
            tok(atlas::baseline::median(&eb)),
            tok(eb.iter().sum::<f64>() / eb.len() as f64),
            tok(atlas::baseline::quantile(&eb, 0.9)),
            dist.join(", ")
        );
        if let Some(bm) = baseline_median {
            println!(
                "  ratio per episode, measured (pre-atlas episode median / atlas episode median): {:.1}x",
                bm / atlas::baseline::median(&eb)
            );
        }
    }
}
