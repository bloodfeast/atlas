//! `cargo xtask compact-live`: the live head-to-head of atlas-assisted
//! against plain Claude Code compaction, paired per compaction point
//! (`docs/design/compaction.md`, section 5). It spends Claude usage, so it
//! never runs without the maintainer's word; `--dry-run` picks the points,
//! writes nothing and prints the plan and its token estimate.
//!
//! ```text
//! cargo xtask compact-live --transcripts DIR --work DIR --atlas-bin EXE --atlas-config FILE
//!     [--claude EXE] [--sessions 4] [--per-session 3] [--thresholds 150000,250000,400000]
//!     [--per-threshold N] [--samples 3] [--calibrate SMOKE_WORK] [--only JOB,JOB]
//!     [--dry-run | --score | --spend | --rescore [--job DIR]]
//! ```
//!
//! `--rescore` scores the saved jobs again, spending nothing, on the
//! three-way split: in the summary, dropped but recoverable in one hop, and
//! lost (`compact_live/rescore.rs`); `--job` prints one job fact by fact.
//!
//! Points: the recorded compactions with `preTokens` 140,000 to 810,000, from
//! the sessions with the most of them (sessions written in the last two
//! hours are skipped), three per session spread over it. Every compaction
//! point is a pair, `plain@P` and `atlas@P`, on the same transcript cut:
//!
//! * `rec`: cut just before the recorded boundary;
//! * each threshold `T`: as many cuts as there are recorded points (or
//!   `--per-threshold N`), each where a segment's context crosses T (the
//!   first request at or past T after a completed turn, at most a quarter
//!   past it), from the same sessions first, then the next in rank.
//!
//! Each job copies its cut into a scratch project under `--work` under a
//! session id of its own (the recording is never changed), resumes that
//! copy headless and runs `/compact`. Both arms run atlas's hooks and server
//! from `--atlas-bin` (`--setting-sources project`, `--strict-mcp-config`),
//! so the baseline carries the same tool list and session lines. Each job
//! runs on its own copy of `--atlas-config` and of its index and memory
//! store (`atlas.json` and `state/` in the job's folder), so no job's hooks
//! can feed another's. The store copy is pruned to the cut's time (the
//! newest record time in the kept lines): no record written after the cut
//! is there (`as-of.json` in the job's folder). The index is the
//! configuration's: a cut older than it is refused unless
//! `--index-newer-ok` (the score counts those jobs). `plain`: `compact.enabled` false, no CLAUDE.md and a
//! bare `/compact`. `atlas`: the compaction features on, the Compact
//! instructions as the `/compact` argument (a resumed session attaches
//! CLAUDE.md only after the boundary, so only the argument reaches the
//! summarizer) and as the project's CLAUDE.md, which is part of the shipped
//! feature and steers the turn after the boundary. The copy is resumed in
//! place, not forked: a fork's transcript is written only after PreCompact
//! has run, so the checkpoint would read nothing. A `/compact` that leaves
//! no boundary at or past the cut (the copy still holds the recording's
//! earlier ones) is a failure, retried, and is never measured. One
//! scripted turn then
//! replays the session's real next prompt in plan mode with edits, writes
//! and shells refused, so nothing outside the scratch folder can change.
//!
//! A cut that leaves a /goal active has it cleared first in both arms
//! (`/goal clear`, Claude Code's own command): a restored goal's Stop check
//! re-prompts the turn, which measures the goal, not the compaction.
//! `goals_after` in `result.json` counts the goal records past the boundary.
//! Resume cost is the ruled one: tool-result bytes from the boundary to the
//! first edit attempt, or to `ExitPlanMode` (the turn is in plan mode).
//!
//! Token counts are the run's own: each claude result's `modelUsage` when
//! present, else `usage`, kept for a turn that hit its limit. A `/compact`
//! whose result reports no usage is counted at its boundary's `preTokens`
//! (what the summarizer read), marked `tokens_source: preTokens`, with the
//! output unknown. The goal clear is booked apart from the scored tokens.
//! `--spend` prints the totals of a work folder. `--score` refuses a run
//! where any job resumed another session or any atlas card was not read
//! from its own transcript.
//!
//! The run happens once. `--work/run.json` holds the plan and the run's id
//! from the first launch; a later launch on the same folder resumes it:
//! a job whose compaction finished is never compacted again (its fork is
//! kept in `compacted.json`), a finished job (`result.json`) is never rerun,
//! and only a job that failed for an infrastructure reason (claude exited
//! without a result, rate limits included) is retried. `--only` makes a
//! smoke run: its folder is marked, a full run refuses it, and `--score`
//! refuses it, so a smoke test never counts. `--score` refuses a folder
//! with an unfinished job or a result from another run id.
//!
//! Every pair runs `--samples` times (3 by default; job names end `-sK`),
//! the arm order alternating across samples and points. The scripted turn
//! has `ExitPlanMode` refused too, so it ends when the plan is written:
//! nothing scored follows it.
//!
//! Scoring, per compaction point (`rec` and each T), on each arm's median
//! over its samples: atlas against plain on ratio pre/(summary+card),
//! fidelity, post-compaction misses, dangling refs, resume tokens and the
//! run's input tokens; an axis passes when atlas is at least as good on all
//! pairs but one and on the mean (INTENT r01407). A point with an
//! unmeasurable sample fails. Latency is judged over every atlas sample:
//! the hooks' process wall time, spawn to exit (each hook runs under
//! `xtask hook-time`, which logs it to `hooks.log`), PreCompact plus the
//! SessionStart after it, at most 1% of the compaction's own time on each
//! and under 250 ms at p95; a missing timing or compaction duration fails it.
//! `--calibrate` prices the plan from a finished smoke folder's measured
//! spend. No transcript text is printed or written outside the work folder.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use atlas::json::Value;

mod rescore;

/// One compaction point.
#[derive(Debug, Clone)]
struct Point {
    file: PathBuf,
    session: String,
    /// 0-based line of the boundary record.
    line: usize,
    /// First line of its segment (after the previous boundary).
    start: usize,
    pre: u64,
    trigger: String,
}

/// One run: a point, an arm, the compaction point, and the cut.
#[derive(Debug, Clone)]
struct Job {
    name: String,
    point: usize,
    arm: String,
    /// `rec` or the threshold.
    at: String,
    /// Lines kept (the cut).
    keep: usize,
    /// Context at the cut.
    ctx: u64,
}

struct Opts {
    transcripts: PathBuf,
    work: PathBuf,
    atlas_bin: PathBuf,
    atlas_config: PathBuf,
    claude: String,
    sessions: usize,
    per_session: usize,
    thresholds: Vec<u64>,
    only: Vec<String>,
    /// Pairs per threshold (default: as many as recorded points).
    per_threshold: Option<usize>,
    /// The fewest facts a point may score on (`fact_count`): a point with
    /// fewer is skipped before the picks, at `rec` and at each threshold.
    /// Brandon's ruling on bar 2: at 8 facts both arms reached 1.000, a tie
    /// that fails the bar while saying nothing about the arms.
    min_facts: usize,
    dry_run: bool,
    score: bool,
    spend: bool,
    /// Runs of each pair (scored on medians).
    samples: usize,
    /// A finished smoke folder whose measured spend calibrates the estimate.
    calibrate: Option<PathBuf>,
    /// Runs cuts older than the index (whose sources are a later snapshot)
    /// instead of refusing them; the score then says how many there are.
    index_newer_ok: bool,
    /// The run pauses before the next job once its measured input passes
    /// this (Brandon's ruling: ~27-33M).
    stop_at: u64,
    /// Prints each finished job's measures (a smoke folder included),
    /// spending nothing and judging nothing.
    measure: bool,
    /// Rescores the saved jobs on the three-way split ([`rescore`]),
    /// spending nothing; with `job`, that one job only, fact by fact.
    rescore: bool,
    job: Option<PathBuf>,
}

fn parse(args: Vec<String>) -> Result<Opts, String> {
    let mut o = Opts {
        transcripts: PathBuf::new(),
        work: PathBuf::new(),
        atlas_bin: PathBuf::new(),
        atlas_config: PathBuf::new(),
        claude: "claude".into(),
        sessions: 4,
        per_session: 3,
        thresholds: vec![150_000, 250_000, 400_000],
        only: Vec::new(),
        per_threshold: None,
        min_facts: MIN_FACTS,
        dry_run: false,
        score: false,
        spend: false,
        samples: 3,
        calibrate: None,
        index_newer_ok: false,
        stop_at: 33_000_000,
        measure: false,
        rescore: false,
        job: None,
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--transcripts" => o.transcripts = PathBuf::from(val()?),
            "--work" => o.work = PathBuf::from(val()?),
            "--atlas-bin" => o.atlas_bin = PathBuf::from(val()?),
            "--atlas-config" => o.atlas_config = PathBuf::from(val()?),
            "--claude" => o.claude = val()?,
            "--sessions" => o.sessions = val()?.parse().map_err(|_| "--sessions needs a number")?,
            "--per-session" => o.per_session = val()?.parse().map_err(|_| "--per-session needs a number")?,
            "--thresholds" => o.thresholds = val()?.split(',').map(|t| t.trim().parse().map_err(|_| format!("--thresholds: {t:?}"))).collect::<Result<_, _>>()?,
            "--only" => o.only = val()?.split(',').map(str::to_string).collect(),
            "--per-threshold" => o.per_threshold = Some(val()?.parse().map_err(|_| "--per-threshold needs a number")?),
            "--min-facts" => o.min_facts = val()?.parse().map_err(|_| "--min-facts needs a number")?,
            "--dry-run" => o.dry_run = true,
            "--score" => o.score = true,
            "--spend" => o.spend = true,
            "--measure" => o.measure = true,
            "--rescore" => o.rescore = true,
            "--job" => o.job = Some(PathBuf::from(val()?)),
            "--samples" => o.samples = val()?.parse().map_err(|_| "--samples needs a number")?,
            "--calibrate" => o.calibrate = Some(PathBuf::from(val()?)),
            "--index-newer-ok" => o.index_newer_ok = true,
            "--stop-at" => o.stop_at = val()?.parse().map_err(|_| "--stop-at needs a number of input tokens")?,
            x => return Err(format!("compact-live: unknown argument {x:?}")),
        }
    }
    if o.work.as_os_str().is_empty() {
        return Err("compact-live needs --work DIR".into());
    }
    Ok(o)
}

/// The main-session transcripts below a folder (never a subagent's).
fn transcripts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() && p.file_name().is_some_and(|n| n != "subagents") => stack.push(p),
                Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// 0-based lines of a transcript's compaction boundaries (main chain).
fn boundary_lines(lines: &[&str]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains("\"compact_boundary\"") && !l.contains("\"isSidechain\":true"))
        .filter(|(_, l)| atlas::json::parse(l).ok().and_then(|v| v.get("subtype").and_then(Value::as_str).map(str::to_string)).as_deref() == Some("compact_boundary"))
        .map(|(i, _)| i)
        .collect()
}

/// The default `--min-facts`. Set from the 514ed9e run's 9 points (facts
/// 8, 14, 27, 28, 32, 49, 51, 70, 89): plain reached the ceiling only at 8,
/// and at every point from 14 up it kept at most 0.59, so 14 is the
/// smallest count the data shows separating the arms.
const MIN_FACTS: usize = 14;

/// The facts the rescore scores at a cut of `file` (segment from line
/// `start`, `cut` lines kept): its named facts whose text is in the
/// segment's visible records, the same rule as the rescore's `facts`.
fn fact_count(file: &Path, text: &str, start: usize, cut: usize) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let off = |n: usize| -> usize { lines[..n.min(lines.len())].iter().map(|l| l.len() + 1).sum::<usize>().min(text.len()) };
    let (a, b) = (off(start), off(cut));
    let seg: String = atlas::chain::records(&text.as_bytes()[a..b], a as u64).iter().map(|r| format!("{}\n", r.text)).collect();
    atlas::compact::facts_at_by(file, start, cut, atlas::compact::FactRule::Named).iter().filter(|(f, _)| seg.contains(f.as_str())).count()
}

/// Picks the points: per session, the compactions in range, deduplicated
/// across resumed copies, with at least `--min-facts` facts; the sessions
/// with the most, spread within each.
fn points(o: &Opts) -> (Vec<Point>, Vec<PathBuf>) {
    let mut seen = HashSet::new();
    let mut by_session: Vec<Vec<Point>> = Vec::new();
    for f in transcripts(&o.transcripts) {
        // A session still being written is not a recording yet.
        let live = std::fs::metadata(&f).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d.as_secs() < 7200);
        if live {
            continue;
        }
        let comps = atlas::compact::replay(&f);
        if comps.is_empty() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        let at = boundary_lines(&lines);
        let session = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut ps = Vec::new();
        for (i, c) in comps.iter().enumerate() {
            let Some(&line) = at.get(i) else { break };
            if !(140_000..=810_000).contains(&c.pre_tokens) || c.summary.is_empty() || !seen.insert((c.time.clone(), c.pre_tokens)) {
                continue;
            }
            let start = if i == 0 { 0 } else { at[i - 1] + 1 };
            if fact_count(&f, &text, start, line) < o.min_facts {
                continue;
            }
            ps.push(Point { file: f.clone(), session: session.clone(), line, start, pre: c.pre_tokens, trigger: c.trigger.clone() });
        }
        if !ps.is_empty() {
            by_session.push(ps);
        }
    }
    by_session.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].session.cmp(&b[0].session)));
    let ranked: Vec<PathBuf> = by_session.iter().map(|ps| ps[0].file.clone()).collect();
    let mut out = Vec::new();
    for ps in by_session.into_iter().take(o.sessions) {
        let n = ps.len();
        let k = o.per_session.min(n);
        let picks: Vec<usize> = if k <= 1 { vec![n - 1] } else { (0..k).map(|j| j * (n - 1) / (k - 1)).collect() };
        out.extend(picks.into_iter().map(|i| ps[i].clone()));
    }
    (out, ranked)
}

/// A record's context tokens, when it is a main-chain assistant record.
fn ctx_of(line: &str) -> Option<u64> {
    if !line.contains("\"type\":\"assistant\"") || line.contains("\"isSidechain\":true") {
        return None;
    }
    atlas::checkpoint::usage_ctx(&atlas::json::parse(line).ok()?)
}

/// Every place a transcript's context crosses `t` inside a segment: the
/// first request at or past T after a completed turn, at most a quarter
/// past it, with an earlier request of that segment below T, and at least
/// `min_facts` facts at the cut. Returns (segment start, cut, context at
/// the cut).
fn crossings(file: &Path, t: u64, min_facts: usize) -> Vec<(usize, usize, u64)> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    let lines: Vec<&str> = text.lines().collect();
    let mut bounds = boundary_lines(&lines);
    bounds.push(lines.len());
    let mut out = Vec::new();
    let mut start = 0;
    for &end in &bounds {
        let mut below = false;
        for j in start.max(1)..end {
            let Some(c) = ctx_of(lines[j]) else { continue };
            if c < t {
                below = true;
            } else if below && lines[j - 1].contains("\"type\":\"user\"") {
                if c <= t + t / 4 && fact_count(file, &text, start, j) >= min_facts {
                    out.push((start, j, c));
                }
                break;
            }
        }
        start = end + 1;
    }
    out
}

/// The jobs and the points they cut at. Each compaction point is a pair,
/// plain and atlas on one cut: `rec` at each recorded point, and for each
/// threshold T as many crossings of T as there are recorded points, taken
/// from the same sessions first (spread within each), then from the next
/// sessions in rank.
///
/// Each pair is run `--samples` times (scored on medians), the arm order
/// alternating from one sample and point to the next, so neither arm is
/// always the one that runs on a warm cache.
fn jobs(o: &Opts, rec: &[Point], ranked: &[PathBuf]) -> (Vec<Point>, Vec<Job>) {
    let mut ps: Vec<Point> = rec.to_vec();
    let mut js = Vec::new();
    let samples = o.samples.max(1);
    let pair = |js: &mut Vec<Job>, name: &dyn Fn(&str) -> String, point: usize, at: &str, keep: usize, ctx: u64| {
        for k in 1..=samples {
            let arms = if (point + k).is_multiple_of(2) { ["plain", "atlas"] } else { ["atlas", "plain"] };
            for arm in arms {
                js.push(Job { name: format!("{}-s{k}", name(arm)), point, arm: arm.into(), at: at.into(), keep, ctx });
            }
        }
    };
    for (i, p) in rec.iter().enumerate() {
        pair(&mut js, &|arm| format!("p{i:02}-{arm}@rec"), i, "rec", p.line, p.pre);
    }
    let want = o.per_threshold.unwrap_or(rec.len());
    for &t in &o.thresholds {
        let mut got = 0;
        for f in ranked {
            if got >= want {
                break;
            }
            let cs = crossings(f, t, o.min_facts);
            if cs.is_empty() {
                continue;
            }
            let k = o.per_session.min(cs.len()).min(want - got);
            let picks: Vec<usize> = if k <= 1 { vec![cs.len() - 1] } else { (0..k).map(|j| j * (cs.len() - 1) / (k - 1)).collect() };
            let session = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            for x in picks {
                let (start, cut, ctx) = cs[x];
                let i = ps.len();
                ps.push(Point { file: f.clone(), session: session.clone(), line: cut, start, pre: ctx, trigger: format!("cross {t}") });
                pair(&mut js, &|arm| format!("t{t}-{got:02}-{arm}"), i, &t.to_string(), cut, ctx);
                got += 1;
            }
        }
    }
    (ps, js)
}

/// The session's next prompt after line `from`: the first person's text
/// (kept in memory; passed to the scripted turn only).
fn next_prompt(file: &Path, from: usize) -> Option<String> {
    next_prompt_at(file, from).map(|(_, t)| t)
}

/// [`next_prompt`] with its 0-based line.
fn next_prompt_at(file: &Path, from: usize) -> Option<(usize, String)> {
    next_prompt_in(&std::fs::read_to_string(file).ok()?, from)
}

/// [`next_prompt_at`] over a transcript's text already read.
fn next_prompt_in(text: &str, from: usize) -> Option<(usize, String)> {
    for (i, l) in text.lines().enumerate().skip(from) {
        if !l.contains("\"type\":\"user\"") || l.contains("\"isCompactSummary\":true") || l.contains("\"tool_result\"") || l.contains("\"isMeta\":true") || l.contains("\"isSidechain\":true") {
            continue;
        }
        let v = atlas::json::parse(l).ok()?;
        let t = atlas::checkpoint::turn_text(&v);
        // A slash command (`/compact`, `/goal`) is Claude Code's, not a turn.
        if !t.trim().is_empty() && !t.starts_with('<') && !t.trim_start().starts_with('/') {
            return Some((i, t));
        }
    }
    None
}

fn projects_dir() -> Result<PathBuf, String> {
    let env = |n: &str| std::env::var_os(n).filter(|v| !v.is_empty()).map(PathBuf::from);
    env("CLAUDE_CONFIG_DIR").or_else(|| env(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| h.join(".claude"))).map(|d| d.join("projects")).ok_or("no home folder".into())
}

// ------------------------------------------------------------- the plan

fn plan_json(ps: &[Point], js: &[Job]) -> Value {
    let points: Vec<Value> = ps
        .iter()
        .map(|p| atlas::json!({"file": p.file.to_string_lossy().into_owned(), "session": p.session.clone(), "line": p.line as u64, "start": p.start as u64, "pre": p.pre, "trigger": p.trigger.clone()}))
        .collect();
    let jobs: Vec<Value> = js
        .iter()
        .map(|j| atlas::json!({"name": j.name.clone(), "point": j.point as u64, "arm": j.arm.clone(), "at": j.at.clone(), "keep": j.keep as u64, "ctx": j.ctx}))
        .collect();
    atlas::json!({"points": points, "jobs": jobs})
}

fn plan_from(v: &Value) -> Option<(Vec<Point>, Vec<Job>)> {
    let s = |x: &Value, k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let n = |x: &Value, k: &str| x.get(k).and_then(Value::as_u64).unwrap_or(0);
    let ps = v.get("points")?.as_array()?.iter().map(|p| Point { file: PathBuf::from(s(p, "file")), session: s(p, "session"), line: n(p, "line") as usize, start: n(p, "start") as usize, pre: n(p, "pre"), trigger: s(p, "trigger") }).collect();
    let js = v.get("jobs")?.as_array()?.iter().map(|j| Job { name: s(j, "name"), point: n(j, "point") as usize, arm: s(j, "arm"), at: s(j, "at"), keep: n(j, "keep") as usize, ctx: n(j, "ctx") }).collect();
    Some((ps, js))
}

/// The jobs one launch runs: every job of the plan, or those whose name
/// starts with one of `only`. A resumed plan is filtered the same way as a
/// fresh one; the caller still writes the whole plan to run.json.
fn selected<'a>(jobs: &'a [Job], only: &[String]) -> Result<Vec<&'a Job>, String> {
    if only.is_empty() {
        return Ok(jobs.iter().collect());
    }
    let js: Vec<&Job> = jobs.iter().filter(|j| only.iter().any(|x| j.name.starts_with(x.as_str()))).collect();
    if js.is_empty() {
        return Err(format!("--only {} names no job of the plan ({})", only.join(","), jobs.iter().map(|j| j.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    Ok(js)
}

/// The run manifest: the run id, whether it is a smoke run, and the plan.
struct Run {
    id: String,
    smoke: bool,
    points: Vec<Point>,
    jobs: Vec<Job>,
}

fn read_run(work: &Path) -> Result<Option<Run>, String> {
    let p = work.join("run.json");
    let Ok(t) = std::fs::read_to_string(&p) else { return Ok(None) };
    let v = atlas::json::parse(&t).map_err(|_| format!("{} is not a run manifest", p.display()))?;
    let (points, jobs) = plan_from(v.get("plan").unwrap_or(&Value::Null)).ok_or(format!("{} holds no plan", p.display()))?;
    Ok(Some(Run { id: v.get("run").and_then(Value::as_str).unwrap_or("").to_string(), smoke: v.get("smoke").and_then(Value::as_bool).unwrap_or(false), points, jobs }))
}

fn write_run(work: &Path, r: &Run) -> Result<(), String> {
    std::fs::create_dir_all(work).map_err(|e| format!("{}: {e}", work.display()))?;
    let v = atlas::json!({"run": r.id.clone(), "smoke": r.smoke, "plan": plan_json(&r.points, &r.jobs)});
    std::fs::write(work.join("run.json"), atlas::json::to_string_pretty(&v)).map_err(|e| e.to_string())
}

fn read_json(p: &Path) -> Option<Value> {
    std::fs::read_to_string(p).ok().and_then(|t| atlas::json::parse(&t).ok())
}

// ------------------------------------------------------------- running

/// A job's own session id: a UUID derived from the run and the job, so a
/// relaunch finds the same copy and no two jobs share a session (the
/// checkpoint store is keyed by session).
fn job_session(run: &str, job: &str) -> String {
    let h = atlas::sha256::hex(format!("{run}/{job}").as_bytes());
    format!("{}-{}-4{}-8{}-{}", &h[..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
}

/// The cut transcript under a new session id: every `sessionId` field of
/// the recorded session is rewritten, nothing else.
fn rekey(lines: &[&str], from: &str, to: &str) -> String {
    let (a, b) = (format!("\"sessionId\":\"{from}\""), format!("\"sessionId\":\"{to}\""));
    let mut out = String::new();
    for l in lines {
        out.push_str(&l.replace(&a, &b));
        out.push('\n');
    }
    out
}

/// The `/compact` prompt: bare for `plain`; for `atlas` with the Compact
/// instructions as the argument.
fn compact_prompt(arm: &str) -> String {
    if arm != "atlas" {
        return "/compact".into();
    }
    format!("/compact {}", atlas::claude_settings::COMPACT_INSTRUCTIONS)
}

/// Writes a job's scratch project: the cut transcript under the job's own
/// session id where Claude Code resumes it (only when `fresh`: once the
/// compaction ran, the copy is the compacted session and is kept), the
/// settings and MCP files, and for the atlas arm the Compact instructions.
/// Returns (cwd, mcp config).
fn stage(o: &Opts, p: &Point, j: &Job, sid: &str, fresh: bool) -> Result<(PathBuf, PathBuf), String> {
    stage_in(&projects_dir()?, o, p, j, sid, fresh)
}

/// A job's folder (its scratch project's cwd), absolute.
fn job_dir(o: &Opts, j: &Job) -> PathBuf {
    let d = o.work.join("jobs").join(&j.name);
    std::path::absolute(&d).unwrap_or(d)
}

/// A session's transcript in the Claude Code projects folder of `cwd`.
fn transcript_in(projects: &Path, cwd: &Path, session: &str) -> PathBuf {
    projects.join(atlas::transcripts::project_of_cwd(cwd)).join(format!("{session}.jsonl"))
}

fn transcript(cwd: &Path, session: &str) -> Result<PathBuf, String> {
    Ok(transcript_in(&projects_dir()?, cwd, session))
}

/// [`stage`] under a given Claude Code projects folder.
fn stage_in(projects: &Path, o: &Opts, p: &Point, j: &Job, sid: &str, fresh: bool) -> Result<(PathBuf, PathBuf), String> {
    let e = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let cwd = job_dir(o, j);
    std::fs::create_dir_all(cwd.join(".claude")).map_err(|x| e(&cwd, x))?;
    if fresh {
        let text = std::fs::read_to_string(&p.file).map_err(|x| e(&p.file, x))?;
        let kept: Vec<&str> = text.lines().take(j.keep).collect();
        let dst = transcript_in(projects, &cwd, sid);
        std::fs::create_dir_all(dst.parent().unwrap_or(projects)).map_err(|x| e(&dst, x))?;
        std::fs::write(&dst, rekey(&kept, &p.session, sid)).map_err(|x| e(&dst, x))?;
        let cut = cut_time(&kept).ok_or(format!("{}: no record time in the cut's {} lines", p.file.display(), j.keep))?;
        job_config(o, j, &cwd, &cut)?;
        // A retried compaction times its hooks afresh.
        let _ = std::fs::remove_file(cwd.join(HOOKS_LOG));
    }
    let bin = o.atlas_bin.to_string_lossy().replace('\\', "/");
    // Both arms run atlas's server and hooks, so the baseline carries the
    // same tool list and session lines; each job has its own configuration
    // and state (`job_config`), the plain arm's with the compaction
    // features off, and only the atlas arm gets the Compact instructions.
    // Each hook runs under `xtask hook-time`, which logs its process wall
    // time, spawn to exit ([`hook_time`]).
    let cfg = job_config_path(&cwd).to_string_lossy().replace('\\', "/");
    let timer = std::env::current_exe().map_err(|x| format!("xtask's own path: {x}"))?.to_string_lossy().replace('\\', "/");
    let log = cwd.join(HOOKS_LOG).to_string_lossy().replace('\\', "/");
    let hook = atlas::json!([{"hooks": [{"type": "command", "command": format!("\"{timer}\" hook-time \"{log}\" -- \"{bin}\" --config \"{cfg}\" --hook"), "timeout": 30}]}]);
    if j.arm == "atlas" {
        std::fs::write(cwd.join("CLAUDE.md"), atlas::claude_settings::compact_block()).map_err(|x| e(&cwd, x))?;
    }
    let settings = atlas::json!({"hooks": {"PreCompact": hook.clone(), "SessionStart": hook.clone(), "PreToolUse": hook}});
    let mcp = atlas::json!({"mcpServers": {"atlas": {"command": bin, "args": ["--config", cfg]}}});
    std::fs::write(cwd.join(".claude").join("settings.json"), atlas::json::to_string_pretty(&settings)).map_err(|x| e(&cwd, x))?;
    let mcp_path = cwd.join("mcp.json");
    std::fs::write(&mcp_path, atlas::json::to_string_pretty(&mcp)).map_err(|x| e(&cwd, x))?;
    Ok((cwd, mcp_path))
}

/// A job's own atlas configuration.
fn job_config_path(cwd: &Path) -> PathBuf {
    cwd.join("atlas.json")
}

/// A job's hook timings, one JSON line per hook process ([`hook_time`]).
const HOOKS_LOG: &str = "hooks.log";

/// `xtask hook-time LOG -- EXE ARGS...`: runs one hook process with the
/// event on stdin, passing its output and exit code through, and appends
/// `{"event", "source", "ms", "code"}` to LOG, where `ms` is the process's
/// wall time from spawn to exit (the latency the session waits for; the
/// event is read before the clock starts). Returns the hook's exit code.
///
/// # Errors
///
/// A usage error, or the hook could not be started.
pub fn hook_time(args: Vec<String>) -> Result<u8, String> {
    use std::io::{Read, Write};
    let (log, cmd) = match args.split_first() {
        Some((log, rest)) if rest.len() > 1 && rest[0] == "--" => (PathBuf::from(log), &rest[1..]),
        _ => return Err("usage: xtask hook-time LOG -- EXE ARGS...".into()),
    };
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).map_err(|e| format!("stdin: {e}"))?;
    let ev = atlas::json::parse(String::from_utf8_lossy(&input).trim()).unwrap_or(Value::Null);
    let field = |k: &str| ev.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let t0 = std::time::Instant::now();
    let mut child = Command::new(&cmd[0]).args(&cmd[1..]).stdin(std::process::Stdio::piped()).spawn().map_err(|e| format!("{}: {e}", cmd[0]))?;
    if let Some(mut w) = child.stdin.take() {
        // A hook that exits without reading its input is not an error here.
        let _ = w.write_all(&input);
    }
    let status = child.wait().map_err(|e| format!("{}: {e}", cmd[0]))?;
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    let code = status.code().unwrap_or(1);
    let line = atlas::json!({"event": field("hook_event_name"), "source": field("source"), "ms": ms, "code": code as i64});
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
        let _ = writeln!(f, "{line}");
    }
    Ok(u8::try_from(code).unwrap_or(1))
}

/// The compaction's hooks' wall time from a job's hook log: the first
/// PreCompact and the first SessionStart after a compaction (`source:
/// compact`), the job's own `/compact` (a later one in the turn is not it).
/// NaN (a FAIL on the latency bar) when either was not timed.
fn hook_wall(log: &str) -> f64 {
    let (mut pre, mut post) = (None, None);
    for v in log.lines().filter_map(|l| atlas::json::parse(l).ok()) {
        let ms = v.get("ms").and_then(Value::as_f64).unwrap_or(f64::NAN);
        match (v.get("event").and_then(Value::as_str), v.get("source").and_then(Value::as_str)) {
            (Some("PreCompact"), _) => pre = pre.or(Some(ms)),
            (Some("SessionStart"), Some("compact")) if pre.is_some() => post = post.or(Some(ms)),
            _ => {}
        }
    }
    match (pre, post) {
        (Some(a), Some(b)) => a + b,
        _ => f64::NAN,
    }
}

/// Copies `src` and its companions beside it (`-wal`, `-shm`, `.jsonl`:
/// every file whose name starts with its name) into `dir`. Returns the copy.
fn copy_db(src: &Path, dir: &Path) -> Result<PathBuf, String> {
    let name = src.file_name().ok_or(format!("{} names no file", src.display()))?.to_string_lossy().into_owned();
    let from = src.parent().unwrap_or(Path::new("."));
    for e in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with(&name) && e.file_type().is_ok_and(|t| t.is_file()) {
            std::fs::copy(e.path(), dir.join(&n)).map_err(|x| format!("{}: {x}", e.path().display()))?;
        }
    }
    Ok(dir.join(name))
}

/// A cut's time: the newest record time in its kept lines (UTC, as the
/// transcript writes it). `None` when no kept line has one.
fn cut_time(kept: &[&str]) -> Option<String> {
    kept.iter().rev().filter(|l| l.contains("\"timestamp\"")).find_map(|l| atlas::json::parse(l).ok()?.get("timestamp")?.as_str().filter(|t| !t.is_empty()).map(str::to_string))
}

/// Seconds since the epoch of a UTC `YYYY-MM-DDTHH:MM:SS` time (any
/// fraction and zone suffix ignored).
fn utc_secs(t: &str) -> Option<i64> {
    let n = |a: usize, b: usize| t.get(a..b)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // Days from civil (H. Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + hh * 3_600 + mm * 60 + ss)
}

/// [`cut_time`] of a point's cut, read from its recording.
fn point_time(p: &Point) -> Option<String> {
    let text = std::fs::read_to_string(&p.file).ok()?;
    cut_time(&text.lines().take(p.line).collect::<Vec<_>>())
}

/// What an index knows up to: its newest indexed file's mtime (UTC, with
/// milliseconds). A snapshot's files carry the time it was taken.
fn index_as_of(db: &Path) -> Result<String, String> {
    let conn = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| format!("{}: {e}", db.display()))?;
    conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', MAX(m) / 1e9, 'unixepoch') FROM (SELECT MAX(mtime_ns) AS m FROM docs UNION ALL SELECT MAX(mtime_ns) FROM notes)", [], |r| r.get::<_, Option<String>>(0))
        .map_err(|e| format!("{}: {e}", db.display()))?
        .ok_or(format!("{} indexes no file", db.display()))
}

/// Prunes a copied memory store to what existed at `as_of`: every version
/// written after it goes, with the records left without one; heads, status
/// (supersessions and retirements after `as_of` undone; a record retired
/// before it and reactivated after it is retired again, from the `.jsonl`
/// copy, the only place a reactivation is written), refs, placement and
/// the search index follow. A version's time is the source item's own
/// date when it states one, else the import's, so an undated item imported
/// after `as_of` is dropped too (both arms run on the same pruned store).
/// One gap, symmetric and empty in the eval store: refs carry no time, so
/// a ref added after the cut to a record that survives it stays (the
/// eval store, synced from the snapshot's sources, holds no refs).
/// The `.jsonl` copy is emptied: it would hold the dropped versions.
/// Returns (versions, records) dropped.
fn prune_store(path: &Path, as_of: &str) -> Result<(usize, usize), String> {
    prune_store_keeping(path, as_of, "")
}

/// [`prune_store`], except that records whose key starts with `keep` (when
/// not empty) keep every version: the rescore keeps a job's own checkpoint.
fn prune_store_keeping(path: &Path, as_of: &str, keep: &str) -> Result<(usize, usize), String> {
    let e = |x: rusqlite::Error| format!("{}: {x}", path.display());
    let mut conn = rusqlite::Connection::open(path).map_err(e)?;
    let tx = conn.transaction().map_err(e)?;
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS versions_no_update; DROP TRIGGER IF EXISTS versions_no_delete; DROP TRIGGER IF EXISTS records_no_delete;
         DROP TRIGGER IF EXISTS records_head_forward; DROP TRIGGER IF EXISTS records_key_fixed;",
    )
    .map_err(e)?;
    let versions = tx
        .execute("DELETE FROM versions WHERE time > ?1 AND NOT (?2 <> '' AND rid IN (SELECT rid FROM records WHERE substr(key, 1, length(?2)) = ?2))", [as_of, keep])
        .map_err(e)?;
    let records = tx.execute("DELETE FROM records WHERE rid NOT IN (SELECT rid FROM versions)", []).map_err(e)?;
    tx.execute("DELETE FROM supersedes WHERE time > ?1 OR new_rid NOT IN (SELECT rid FROM records) OR old_rid NOT IN (SELECT rid FROM records)", [as_of]).map_err(e)?;
    tx.execute("DELETE FROM retirements WHERE time > ?1 OR rid NOT IN (SELECT rid FROM records)", [as_of]).map_err(e)?;
    tx.execute_batch(
        "UPDATE records SET head = (SELECT v.vid FROM versions v WHERE v.rid = records.rid ORDER BY v.seq DESC LIMIT 1);
         UPDATE records SET status = 'active' WHERE (status = 'superseded' AND rid NOT IN (SELECT old_rid FROM supersedes))
             OR (status = 'retired' AND rid NOT IN (SELECT rid FROM retirements));
         DELETE FROM refs WHERE rid NOT IN (SELECT rid FROM records);
         DELETE FROM placement WHERE rid NOT IN (SELECT rid FROM records);
         DELETE FROM records_fts;
         INSERT INTO records_fts(rowid, title, body) SELECT r.rid, v.title, v.body FROM records r JOIN versions v ON v.vid = r.head;
         INSERT OR REPLACE INTO mem_meta(key, value) VALUES('jsonl_end', '0');",
    )
    .map_err(e)?;
    // A reactivation is in no table, only in the `.jsonl` copy: a record
    // whose last retire or reactivate at or before the cut is a retire was
    // retired then, whatever it is now.
    let mut last: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for v in std::fs::read_to_string(atlas::memory::jsonl_path(path)).unwrap_or_default().lines().filter(|l| l.contains("\"op\":\"re")).filter_map(|l| atlas::json::parse(l).ok()) {
        let (Some(op), Some(key), Some(t)) = (v.get("op").and_then(Value::as_str), v.get("key").and_then(Value::as_str), v.get("time").and_then(Value::as_str)) else { continue };
        if matches!(op, "retire" | "reactivate") && t <= as_of {
            last.insert(key.to_string(), op == "retire");
        }
    }
    for (key, _) in last.iter().filter(|(_, &retired)| retired) {
        tx.execute("UPDATE records SET status = 'retired' WHERE key = ?1 AND status = 'active'", [key]).map_err(e)?;
    }
    tx.execute_batch(atlas::memory::SCHEMA).map_err(e)?;
    tx.commit().map_err(e)?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").map_err(e)?;
    let jsonl = atlas::memory::jsonl_path(path);
    std::fs::write(&jsonl, "").map_err(|x| format!("{}: {x}", jsonl.display()))?;
    Ok((versions, records))
}

/// Writes a job's own configuration and state, so no job's hooks or server
/// can feed another's (both arms of a cut run in turn): `--atlas-config`
/// with the index and memory store copied into the job's `state` folder,
/// exports and snapshots there too, and for the plain arm
/// `compact.enabled` false. The memory store copy is pruned to the cut's
/// time ([`prune_store`]); `as-of.json` records the cut, what the index
/// knows up to and what the prune dropped.
fn job_config(o: &Opts, j: &Job, cwd: &Path, cut: &str) -> Result<PathBuf, String> {
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let text = std::fs::read_to_string(&o.atlas_config).map_err(|e| err(&o.atlas_config, e))?;
    let mut v = atlas::json::parse(&text).map_err(|_| format!("{} is not JSON", o.atlas_config.display()))?;
    let state = cwd.join("state");
    std::fs::create_dir_all(&state).map_err(|e| err(&state, e))?;
    let slash = |p: &Path| Value::String(p.to_string_lossy().replace('\\', "/"));
    let root = v.as_object_mut().ok_or("the atlas config is not an object")?;
    let db = root.get("db").and_then(Value::as_str).map(PathBuf::from).ok_or("the atlas config names no db")?;
    let db_copy = copy_db(&db, &state)?;
    root.insert("db".into(), slash(&db_copy));
    let mut memory = root.get("memory").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(store) = memory.get("store").and_then(Value::as_str).map(PathBuf::from) {
        let copy = copy_db(&store, &state)?;
        let (versions, records) = prune_store(&copy, cut)?;
        let asof = atlas::json!({"cut": cut, "index_as_of": index_as_of(&db_copy).unwrap_or_default(), "versions_dropped": versions as u64, "records_dropped": records as u64});
        std::fs::write(cwd.join("as-of.json"), asof.to_string()).map_err(|e| err(cwd, e))?;
        memory.insert("store".into(), slash(&copy));
    }
    memory.insert("export_dir".into(), slash(&state.join("exports")));
    memory.insert("snapshot_dir".into(), slash(&state.join("snapshots")));
    root.insert("memory".into(), Value::Object(memory));
    if j.arm != "atlas" {
        let mut compact = root.get("compact").and_then(Value::as_object).cloned().unwrap_or_default();
        compact.insert("enabled".into(), Value::Bool(false));
        root.insert("compact".into(), Value::Object(compact));
    }
    let out = job_config_path(cwd);
    std::fs::write(&out, atlas::json::to_string_pretty(&v)).map_err(|e| err(&out, e))?;
    Ok(out)
}

/// The 0-based line of a transcript's newest boundary when it is the job's
/// own, at or past the cut (`keep` lines): the resumed copy still holds the
/// recording's earlier boundaries, and a `/compact` that did not compact
/// leaves only those.
fn new_boundary(path: &Path, keep: usize) -> Option<usize> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    boundary_lines(&lines).last().copied().filter(|&b| b >= keep)
}

/// A file's line count (0 when unreadable).
fn line_count(path: &Path) -> usize {
    std::fs::read(path).map_or(0, |b| b.split(|&c| c == b'\n').filter(|l| !l.is_empty()).count())
}

/// Cuts a transcript back to its first `n` lines; a shorter one is kept.
fn truncate_lines(path: &Path, n: usize) -> Result<(), String> {
    let e = |x: std::io::Error| format!("{}: {x}", path.display());
    let b = std::fs::read(path).map_err(e)?;
    let mut seen = 0;
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' && (i == 0 || b[i - 1] != b'\n') {
            seen += 1;
            if seen == n {
                if i + 1 < b.len() {
                    std::fs::OpenOptions::new().write(true).open(path).and_then(|f| f.set_len(i as u64 + 1)).map_err(e)?;
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The isolating flags every call carries.
fn base_args(mcp: &Path) -> Vec<String> {
    ["--setting-sources", "project", "--strict-mcp-config", "--mcp-config"].iter().map(|s| s.to_string()).chain([mcp.to_string_lossy().into_owned(), "--output-format".into(), "json".into()]).collect()
}

/// One claude call; an exit without a JSON result, or a result marked as an
/// error, is an infrastructure failure (the job is retried next launch). A
/// turn that hit `--max-turns` is a finished turn and keeps its usage.
fn claude(o: &Opts, cwd: &Path, args: &[String]) -> Result<Value, String> {
    let out = Command::new(&o.claude).args(args).current_dir(cwd).output().map_err(|e| format!("{}: {e}", o.claude))?;
    result_of(&out.stdout, out.status.code(), out.stderr.len())
}

/// [`claude`]'s reading of its output.
fn result_of(stdout: &[u8], code: Option<i32>, stderr_len: usize) -> Result<Value, String> {
    let text = String::from_utf8_lossy(stdout);
    let v = atlas::json::parse(text.trim()).map_err(|_| format!("claude exited {code:?} without JSON output ({stderr_len} bytes on stderr)"))?;
    let sub = v.get("subtype").and_then(Value::as_str).unwrap_or("error");
    if v.get("is_error").and_then(Value::as_bool) == Some(true) && sub != "error_max_turns" {
        return Err(format!("claude reported an error result ({sub})"));
    }
    Ok(v)
}

/// A claude result's running totals, (input, output, cost): input counts
/// cache writes and reads. A resumed session's `modelUsage` and
/// `total_cost_usd` are cumulative over the whole session, the recording's
/// history included (the smoke run of 2026-11-04 showed $1,156 on a local
/// command), so a call's own spend is the difference from the previous
/// call's totals ([`spent`]).
fn totals(v: &Value) -> (u64, u64, f64) {
    let n = |o: &Value, k: &str| o.get(k).and_then(Value::as_u64).unwrap_or(0);
    let (i, out) = v.get("modelUsage").and_then(Value::as_object).map_or((0, 0), |m| {
        m.values().fold((0, 0), |(i, out), x| (i + n(x, "inputTokens") + n(x, "cacheCreationInputTokens") + n(x, "cacheReadInputTokens"), out + n(x, "outputTokens")))
    });
    (i, out, v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0))
}

/// One call's own spend: its totals less the previous call's.
fn spent(after: (u64, u64, f64), before: (u64, u64, f64)) -> (u64, u64, f64) {
    (after.0.saturating_sub(before.0), after.1.saturating_sub(before.1), (after.2 - before.2).max(0.0))
}

/// The baseline call must be local: no model request, so its totals are
/// the session's history alone.
fn local_only(v: &Value) -> Result<(), String> {
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    if n("num_turns") == 0 && n("duration_api_ms") == 0 { Ok(()) } else { Err("the baseline call (/goal clear) reached a model; its totals cannot be the baseline".into()) }
}

/// A `/compact` call's tokens, (input, output, source): its own spend
/// ([`spent`]), or, when that is zero, what the summarizer read, the
/// newest boundary's `preTokens` in `transcript`, with the output unknown
/// (`None`). The caller has checked that boundary is the job's own
/// ([`new_boundary`]).
fn compact_spend(own: (u64, u64, f64), transcript: &Path) -> (u64, Option<u64>, &'static str) {
    if own.0 > 0 {
        return (own.0, Some(own.1), "usage");
    }
    match atlas::compact::replay(transcript).last().map_or(0, |x| x.pre_tokens) {
        0 => (0, None, "none"),
        pre => (pre, None, "preTokens"),
    }
}

/// The main-chain records after a transcript's newest compaction boundary.
fn after_boundary(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let lines: Vec<&str> = text.lines().collect();
    let from = boundary_lines(&lines).last().map_or(lines.len(), |&i| i + 1);
    lines[from..].iter().filter(|l| !l.contains("\"isSidechain\":true")).filter_map(|l| atlas::json::parse(l).ok()).collect()
}

/// Resume cost as ruled: tool-result bytes from the boundary to the first
/// edit attempt or `ExitPlanMode`. The scripted turn disallows both (and
/// shells), so in the harness this counts the whole turn's tool results,
/// the same rule in both arms.
fn resume_bytes(path: &Path) -> usize {
    let mut n = 0;
    for v in after_boundary(path) {
        let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else { continue };
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("tool_use") if matches!(b.get("name").and_then(Value::as_str), Some("ExitPlanMode" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit")) => return n,
                Some("tool_result") => {
                    n += match b.get("content") {
                        Some(Value::String(t)) => t.len(),
                        Some(Value::Array(a)) => a.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).map(str::len).sum(),
                        _ => 0,
                    }
                }
                _ => {}
            }
        }
    }
    n
}

/// Whether a cut leaves a /goal active: its newest `goal_status` is not
/// met and no `/goal clear` follows it. Claude Code restores such a goal on
/// resume and its Stop check re-prompts the turn, which measures the goal,
/// not the compaction.
fn goal_active(lines: &[&str]) -> bool {
    let mut active = false;
    for l in lines {
        if l.contains("\"type\":\"goal_status\"") {
            active = l.contains("\"met\":false");
        } else if l.contains("<command-name>/goal</command-name>") && l.contains("<command-args>clear</command-args>") {
            active = false;
        }
    }
    active
}

/// `goal_status` records after the newest boundary (0 once a goal is
/// cleared).
fn goals_after(path: &Path) -> usize {
    after_boundary(path).iter().filter(|v| v.pointer("/attachment/type").and_then(Value::as_str) == Some("goal_status")).count()
}

/// Where an atlas checkpoint read its transcript: `own` (the race is
/// closed), `parent` (the fork fallback), `unread`, or `none` (no card).
fn card_source(store: &atlas::memory::MemoryStore, session: &str) -> &'static str {
    match atlas::checkpoint::newest(store, session).ok().flatten() {
        None => "none",
        Some(h) if h.title.contains("transcript unread") => "unread",
        Some(h) if h.meta.contains("tail_from: ") => "parent",
        Some(_) => "own",
    }
}

/// Runs one job in two stages, each recorded so it is never repeated:
/// `/compact` on the job's own copy (`compacted.json`), then the scripted
/// turn (`result.json`). Each stage keeps claude's raw result beside it
/// (`compact-result.json`, `turn-result.json`). A `/compact` that leaves
/// no boundary of its own is a failure (retried). Returns where the atlas
/// arm's card read its transcript (`-` for the plain arm).
fn run_job(o: &Opts, run: &str, p: &Point, j: &Job) -> Result<String, String> {
    let sid = job_session(run, &j.name);
    let done = read_json(&job_dir(o, j).join("compacted.json")).filter(|v| v.get("run").and_then(Value::as_str) == Some(run));
    let (cwd, mcp) = stage(o, p, j, &sid, done.is_none())?;
    let write = |name: &str, v: &Value| std::fs::write(cwd.join(name), v.to_string()).map_err(|e| e.to_string());
    let u = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let c = match done {
        Some(v) => v,
        None => {
            // Both arms, always: `/goal clear` (Claude Code's own command,
            // local) ends an active goal, so its Stop re-prompts never reach
            // the turn, and its totals are the baseline the compaction's own
            // spend is measured from.
            let text = std::fs::read_to_string(&p.file).map_err(|e| format!("{}: {e}", p.file.display()))?;
            let kept: Vec<&str> = text.lines().take(j.keep).collect();
            let goal = goal_active(&kept);
            let mut a = vec!["-p".to_string(), "/goal clear".into(), "--resume".into(), sid.clone()];
            a.extend(base_args(&mcp));
            let r0 = claude(o, &cwd, &a)?;
            write("goal-clear-result.json", &r0)?;
            local_only(&r0)?;
            let base = totals(&r0);
            let mut a = vec!["-p".to_string(), compact_prompt(&j.arm), "--resume".into(), sid.clone()];
            a.extend(base_args(&mcp));
            let r = claude(o, &cwd, &a)?;
            write("compact-result.json", &r)?;
            let session = r.get("session_id").and_then(Value::as_str).ok_or("no session_id in claude's output")?.to_string();
            let tx = transcript(&cwd, &session)?;
            let boundary = new_boundary(&tx, j.keep).ok_or(format!("/compact wrote no boundary past the cut (line {}) in {}", j.keep, tx.display()))?;
            let after = totals(&r);
            let own = spent(after, base);
            let (input, output, source) = compact_spend(own, &tx);
            let card = if j.arm == "atlas" { card_source(&open_store(&job_config_path(&cwd))?.0, &session) } else { "-" };
            let v = atlas::json!({
                "run": run, "session": session.clone(), "same_session": session == sid, "boundary": boundary as u64,
                "tokens": input, "output_tokens": output.map_or(Value::Null, Value::from), "tokens_source": source,
                "cost_usd": own.2, "card": card, "goal_cleared": goal,
                "totals_in": after.0, "totals_out": after.1, "totals_cost_usd": after.2,
                "lines": line_count(&tx) as u64
            });
            write("compacted.json", &v)?;
            v
        }
    };
    let session = c.get("session").and_then(Value::as_str).unwrap_or("").to_string();
    // A turn that failed or was killed left its partial attempt in the
    // session: cut back to the compacted session, so the retry neither
    // counts that attempt's tool results nor resumes on top of it.
    let lines = c.get("lines").and_then(Value::as_u64).ok_or("compacted.json records no line count; compact the job again")?;
    truncate_lines(&transcript(&cwd, &session)?, lines as usize)?;
    let (mut turn_in, mut turn_out, mut turn_cost, mut turns) = (0, 0, 0.0, 0);
    let mut turn = "none";
    if let Some(prompt) = next_prompt(&p.file, j.keep) {
        let mut a = vec!["-p".to_string(), prompt, "--resume".into(), session.clone(), "--max-turns".into(), "15".into(), "--permission-mode".into(), "plan".into()];
        a.extend(["--disallowedTools", "Bash", "PowerShell", "Edit", "Write", "MultiEdit", "NotebookEdit", "ExitPlanMode"].iter().map(|s| s.to_string()));
        a.extend(base_args(&mcp));
        let r = claude(o, &cwd, &a)?;
        write("turn-result.json", &r)?;
        let before = (u(&c, "totals_in"), u(&c, "totals_out"), c.get("totals_cost_usd").and_then(Value::as_f64).unwrap_or(0.0));
        (turn_in, turn_out, turn_cost) = spent(totals(&r), before);
        turns = u(&r, "num_turns");
        // Reaching the turn limit is a finished turn, not a failure.
        turn = if r.get("subtype").and_then(Value::as_str) == Some("error_max_turns") { "max_turns" } else { "ok" };
    }
    let card = c.get("card").and_then(Value::as_str).unwrap_or("-").to_string();
    let tx = transcript(&cwd, &session)?;
    let f = |k: &str| c.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    // `tokens` is the scored axis (the compaction and the turn); the goal
    // clear is spend, booked apart (the same in both arms).
    let res = atlas::json!({
        "run": run, "session": session, "same_session": c.get("same_session").and_then(Value::as_bool).unwrap_or(false),
        "boundary": u(&c, "boundary"), "cwd": cwd.to_string_lossy().into_owned(), "turn": turn, "turns": turns,
        "goal_cleared": c.get("goal_cleared").and_then(Value::as_bool).unwrap_or(false), "goals_after": goals_after(&tx) as u64,
        "compact_tokens": u(&c, "tokens"), "compact_output_tokens": c.get("output_tokens").cloned().unwrap_or(Value::Null), "compact_tokens_source": c.get("tokens_source").cloned().unwrap_or(Value::Null),
        "turn_tokens": turn_in, "turn_output_tokens": turn_out,
        "tokens": u(&c, "tokens") + turn_in,
        "cost_usd": f("cost_usd") + turn_cost,
        "card": card.clone()
    });
    write("result.json", &res)?;
    Ok(card)
}

/// The spend of a work folder's finished jobs: (jobs, input, output, cost,
/// compactions counted at preTokens, whose output is unknown and not in
/// the output total). Input includes the goal clears.
fn spend(work: &Path) -> (usize, u64, u64, f64, usize) {
    let mut t = (0, 0, 0, 0.0, 0);
    let Ok(rd) = std::fs::read_dir(work.join("jobs")) else { return t };
    for e in rd.flatten() {
        let Some(v) = read_json(&e.path().join("result.json")) else { continue };
        let u = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        t.0 += 1;
        t.1 += u("tokens");
        t.2 += u("compact_output_tokens") + u("turn_output_tokens");
        t.3 += v.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
        t.4 += usize::from(v.get("compact_tokens_source").and_then(Value::as_str) == Some("preTokens"));
    }
    t
}

fn print_spend(work: &Path) {
    let (n, i, out, cost, est) = spend(work);
    println!("spend: {n} jobs, {i} input tokens (cache reads and writes included), {out} output tokens, ${cost:.2} as claude reports it");
    if est > 0 {
        println!("  {est} compactions reported no usage: their input is counted at preTokens and their output is unknown (not in the output total)");
    }
}

/// A point's transcript bytes before its segment (through the previous
/// boundary) and of the segment up to the recorded boundary.
fn point_bytes(p: &Point) -> Option<(u64, u64)> {
    let b = std::fs::read(&p.file).ok()?;
    let (mut at, mut before, mut upto) = (0u64, None, None);
    for (i, l) in b.split_inclusive(|&c| c == b'\n').enumerate() {
        if i == p.start {
            before = Some(at);
        }
        if i == p.line {
            upto = Some(at);
            break;
        }
        at += l.len() as u64;
    }
    let (before, upto) = (before?, upto?);
    Some((before, upto.saturating_sub(before)))
}

/// Estimated input tokens: each compaction reads its context, each scripted
/// turn about 15 requests over the compacted context plus their results.
/// With a calibration (input per context token of a compaction, input per
/// scripted turn), the measured rates replace the defaults (1 and 210k).
fn estimate(js: &[Job], cal: Option<(f64, f64)>) -> (u64, u64) {
    let (per_ctx, per_turn) = cal.unwrap_or((1.0, 210_000.0));
    let compaction: f64 = js.iter().map(|j| j.ctx as f64 * per_ctx).sum();
    ((compaction + js.len() as f64 * per_turn) as u64, js.len() as u64 * 5_000)
}

/// The measured rates of a finished (smoke) work folder: the compactions'
/// input per context token at the cut and the mean scripted-turn input
/// (goal clears included). `None` without a finished job.
fn calibration(work: &Path) -> Option<(f64, f64)> {
    let run = read_run(work).ok()??;
    let (mut ctx, mut compact, mut turn, mut n) = (0.0, 0.0, 0.0, 0.0);
    for j in &run.jobs {
        let Some(v) = read_json(&work.join("jobs").join(&j.name).join("result.json")) else { continue };
        let u = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0) as f64;
        ctx += j.ctx as f64;
        compact += u("compact_tokens");
        turn += u("turn_tokens");
        n += 1.0;
    }
    (n > 0.0 && ctx > 0.0).then(|| (compact / ctx, turn / n))
}

// ------------------------------------------------------------- scoring

/// One job's measures from its forked transcript.
#[derive(Debug, Default, Clone)]
struct Measured {
    ratio: f64,
    fidelity: f64,
    misses: f64,
    dangling: f64,
    resume: f64,
    tokens: f64,
    /// The atlas hooks' process wall time (PreCompact and the SessionStart
    /// after it, spawn to exit), and the compaction's own.
    hook_ms: f64,
    duration_ms: f64,
}

/// One job's measures, from its transcript and its own atlas state; `None`
/// when the job has no result or no boundary of its own.
fn measure(o: &Opts, p: &Point, j: &Job) -> Option<Measured> {
    let res = read_json(&job_dir(o, j).join("result.json"))?;
    let session = res.get("session").and_then(Value::as_str).unwrap_or("").to_string();
    let cwd = PathBuf::from(res.get("cwd").and_then(Value::as_str).unwrap_or(""));
    let tx = transcript(&cwd, &session).ok()?;
    new_boundary(&tx, j.keep)?;
    let (store, s) = open_store(&job_config_path(&cwd)).ok()?;
    let bpt = s.bytes_per_token;
    let comps = atlas::compact::replay(&tx);
    let c = comps.last()?;
    let facts = atlas::compact::facts_at(&p.file, p.start, j.keep);
    // The hooks' latency is their processes' wall time, spawn to exit
    // (`hooks.log`); a missing card or timing is not 0 ms: NaN fails it.
    let (card, hook_ms) = match (j.arm == "atlas").then(|| atlas::checkpoint::newest(&store, &session).ok().flatten()).flatten() {
        Some(h) => (atlas::checkpoint::card(&h), hook_wall(&std::fs::read_to_string(cwd.join(HOOKS_LOG)).unwrap_or_default())),
        None => (String::new(), f64::NAN),
    };
    let have = format!("{}\n{card}", c.summary);
    let kept = facts.iter().filter(|f| have.contains(f.as_str())).count();
    let check = atlas::compact::check(&store, &[], &c.summary).unwrap_or_default();
    let refs = check.refs + check.bare;
    Some(Measured {
        ratio: c.pre_tokens as f64 / ((c.summary.len() + card.len()) as f64 / bpt).max(1.0),
        fidelity: if facts.is_empty() { 1.0 } else { kept as f64 / facts.len() as f64 },
        misses: c.misses as f64,
        dangling: if refs > 0 { (check.dangling.len() + check.bare_missing) as f64 / refs as f64 } else { 0.0 },
        resume: resume_bytes(&tx) as f64 / bpt,
        tokens: res.get("tokens").and_then(Value::as_u64).unwrap_or(0) as f64,
        hook_ms: if j.arm == "atlas" { hook_ms } else { 0.0 },
        duration_ms: c.duration_ms as f64,
    })
}

/// The headline of a compaction point, before the pass/fail axes: tokens
/// saved (the compaction plus the scripted turn's input, which holds the
/// resume reads; resume shown apart) and accuracy gained (facts kept,
/// misses, dangling refs), atlas against plain on the points' medians, in
/// absolute terms and as a percentage, with the spread of the samples
/// (each arm's widest range over a point's samples).
fn headline(pairs: &[(Measured, Measured)], sampled: &[(Vec<Measured>, Vec<Measured>)]) -> Vec<String> {
    let n = pairs.len().max(1) as f64;
    let mean = |arm: usize, f: fn(&Measured) -> f64| pairs.iter().map(|x| f(if arm == 0 { &x.0 } else { &x.1 })).sum::<f64>() / n;
    let spread = |arm: usize, f: fn(&Measured) -> f64| {
        sampled
            .iter()
            .map(|x| {
                let v: Vec<f64> = (if arm == 0 { &x.0 } else { &x.1 }).iter().map(f).collect();
                v.iter().copied().fold(f64::MIN, f64::max) - v.iter().copied().fold(f64::MAX, f64::min)
            })
            .fold(0.0, f64::max)
    };
    let pct = |base: f64, d: f64| if base != 0.0 { format!("{:+.1}%", 100.0 * d / base) } else { "n/a".to_string() };
    let mut out = Vec::new();
    let saved = |name: &str, f: fn(&Measured) -> f64, out: &mut Vec<String>| {
        let (p, a) = (mean(0, f), mean(1, f));
        out.push(format!("{name}: plain {p:.0}, atlas {a:.0}, saved {:.0} ({}); sample spread plain {:.0}, atlas {:.0}", p - a, pct(p, p - a), spread(0, f), spread(1, f)));
    };
    saved("TOKENS per job (compaction + scripted turn input)", |m| m.tokens, &mut out);
    saved("  of which resume reads (tool results to the plan)", |m| m.resume, &mut out);
    let gained = |name: &str, f: fn(&Measured) -> f64, up: bool, out: &mut Vec<String>| {
        let (p, a) = (mean(0, f), mean(1, f));
        let g = if up { a - p } else { p - a };
        out.push(format!("{name}: plain {p:.3}, atlas {a:.3}, gained {g:+.3} ({}); sample spread plain {:.3}, atlas {:.3}", pct(p, g), spread(0, f), spread(1, f)));
    };
    gained("ACCURACY facts kept (fidelity)", |m| m.fidelity, true, &mut out);
    gained("ACCURACY post-compaction misses (fewer is a gain)", |m| m.misses, false, &mut out);
    gained("ACCURACY dangling or missing refs (fewer is a gain)", |m| m.dangling, false, &mut out);
    out
}

/// The per-field median of a point's samples (one arm).
fn median_of(ms: &[Measured]) -> Measured {
    let med = |f: fn(&Measured) -> f64| {
        let mut v: Vec<f64> = ms.iter().map(f).collect();
        v.sort_by(f64::total_cmp);
        let k = v.len();
        if k % 2 == 1 { v[k / 2] } else { (v[k / 2 - 1] + v[k / 2]) / 2.0 }
    };
    Measured {
        ratio: med(|m| m.ratio),
        fidelity: med(|m| m.fidelity),
        misses: med(|m| m.misses),
        dangling: med(|m| m.dangling),
        resume: med(|m| m.resume),
        tokens: med(|m| m.tokens),
        hook_ms: med(|m| m.hook_ms),
        duration_ms: med(|m| m.duration_ms),
    }
}

/// The latency bar over every atlas sample (not medians): hooks p95 under
/// 250 ms and at most 1% of the compaction's own time on each. A missing
/// timing (NaN) or a compaction without a duration fails it, and is named.
fn latency(runs: &[Measured]) -> (bool, String) {
    let missing = runs.iter().filter(|m| m.hook_ms.is_nan()).count();
    let zero = runs.iter().filter(|m| m.duration_ms <= 0.0).count();
    let mut ms: Vec<f64> = runs.iter().map(|m| m.hook_ms).filter(|x| !x.is_nan()).collect();
    ms.sort_by(f64::total_cmp);
    let p95 = if ms.is_empty() { f64::NAN } else { ms[((ms.len() as f64 * 0.95).ceil() as usize).clamp(1, ms.len()) - 1] };
    let share = runs.iter().filter(|m| m.duration_ms > 0.0 && !m.hook_ms.is_nan()).map(|m| m.hook_ms / m.duration_ms).fold(0.0, f64::max);
    let ok = !runs.is_empty() && missing == 0 && zero == 0 && p95 < 250.0 && share <= 0.01;
    (ok, format!("atlas hooks p95 {p95:.1} ms over {} samples, largest share of compaction time {:.3}%, {missing} without a timing, {zero} without a compaction duration", runs.len(), 100.0 * share))
}

/// The atlas arm's memory store and settings, from `--atlas-config`.
fn open_store(cfg: &Path) -> Result<(atlas::memory::MemoryStore, atlas::memory::settings::Settings), String> {
    let c = atlas::config::Config::load(cfg).map_err(|e| e.to_string())?;
    let db = c.db.clone().ok_or("the atlas config names no db")?;
    let s = atlas::memory::settings::Settings::from_config(Some(&c), &db).map_err(|e| e.to_string())?;
    let store = atlas::memory::MemoryStore::open(&s.store).map_err(|e| e.to_string())?;
    Ok((store, s))
}

fn score(o: &Opts, run: &Run) -> Result<(), String> {
    if run.smoke {
        return Err("this work folder holds a smoke run (--only); it is never scored".into());
    }
    // One run only: every job finished, every result from this run.
    let mut unfinished = Vec::new();
    for j in &run.jobs {
        match read_json(&o.work.join("jobs").join(&j.name).join("result.json")) {
            None => unfinished.push(j.name.clone()),
            Some(v) if v.get("run").and_then(Value::as_str) != Some(run.id.as_str()) => return Err(format!("{} holds a result from another run; refusing to mix runs", j.name)),
            Some(_) => {}
        }
    }
    if !unfinished.is_empty() {
        return Err(format!("{} jobs are not finished ({}); launch the run again to resume them", unfinished.len(), unfinished.join(", ")));
    }
    // The harness conditions are refusals, not warnings: every job resumed
    // its own session, and every atlas card read its own transcript.
    let mut bad = Vec::new();
    for j in &run.jobs {
        let v = read_json(&job_dir(o, j).join("result.json")).unwrap_or(Value::Null);
        if v.get("same_session").and_then(Value::as_bool) != Some(true) {
            bad.push(format!("{} resumed another session", j.name));
        }
        if j.arm == "atlas" && v.get("card").and_then(Value::as_str) != Some("own") {
            bad.push(format!("{} has a card not read from its own transcript ({})", j.name, v.get("card").and_then(Value::as_str).unwrap_or("none")));
        }
    }
    if !bad.is_empty() {
        return Err(format!("refusing to score: {}", bad.join("; ")));
    }
    type Axis = (&'static str, bool, fn(&Measured) -> f64);
    let axes: [Axis; 6] = [
        ("ratio pre/(summary+card)", true, |m| m.ratio),
        ("fidelity", true, |m| m.fidelity),
        ("post-compaction misses", false, |m| m.misses),
        ("dangling or missing refs", false, |m| m.dangling),
        ("resume tokens", false, |m| m.resume),
        ("input tokens", false, |m| m.tokens),
    ];
    let mut ats: Vec<String> = Vec::new();
    for j in &run.jobs {
        if !ats.contains(&j.at) {
            ats.push(j.at.clone());
        }
    }
    println!("compact-live score: run {}", run.id);
    // What each job could know: its store as of its cut, the index as of
    // the snapshot. A cut older than the index searched later sources (in
    // both arms; the summary and card never read the index).
    let asof: Vec<Value> = run.jobs.iter().filter_map(|j| read_json(&job_dir(o, j).join("as-of.json"))).collect();
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let older = asof.iter().filter(|v| s(v, "cut") < s(v, "index_as_of")).count();
    let dropped: Vec<u64> = asof.iter().map(|v| v.get("versions_dropped").and_then(Value::as_u64).unwrap_or(0)).collect();
    println!(
        "as of: {} of {} jobs ran a cut older than the index ({}); stores pruned to the cut, {}-{} versions dropped per job; {} jobs without an as-of record",
        older,
        run.jobs.len(),
        asof.first().map(|v| s(v, "index_as_of")).unwrap_or_default(),
        dropped.iter().min().unwrap_or(&0),
        dropped.iter().max().unwrap_or(&0),
        run.jobs.len() - asof.len()
    );
    let mut winners = Vec::new();
    for at in &ats {
        // Per point and arm, every sample is measured; the pair is the two
        // arms' medians. A point with an unmeasurable sample fails the point.
        let mut pairs: Vec<(Measured, Measured)> = Vec::new();
        let mut sampled: Vec<(Vec<Measured>, Vec<Measured>)> = Vec::new();
        let mut atlas_runs: Vec<Measured> = Vec::new();
        let mut broken = Vec::new();
        for i in 0..run.points.len() {
            let runs = |arm: &str| -> Option<Vec<Measured>> {
                let js: Vec<&Job> = run.jobs.iter().filter(|j| j.point == i && j.at == *at && j.arm == arm).collect();
                if js.is_empty() {
                    return Some(Vec::new());
                }
                js.iter().map(|j| measure(o, &run.points[i], j)).collect()
            };
            match (runs("plain"), runs("atlas")) {
                (Some(p), Some(a)) if !p.is_empty() && !a.is_empty() => {
                    atlas_runs.extend(a.iter().cloned());
                    pairs.push((median_of(&p), median_of(&a)));
                    sampled.push((p, a));
                }
                (Some(p), Some(a)) if p.is_empty() && a.is_empty() => {}
                _ => broken.push(format!("p{i:02}")),
            }
        }
        let n = pairs.len();
        println!("\ncompaction point {at}: {n} pairs (medians of their samples)");
        if !broken.is_empty() {
            println!("  unmeasurable (no boundary of its own or no result): {}\tFAIL", broken.join(", "));
        }
        if n == 0 {
            continue;
        }
        for l in headline(&pairs, &sampled) {
            println!("  {l}");
        }
        // How far the index (its sources' snapshot) is ahead of each cut.
        let leads: Vec<f64> = run.jobs.iter().filter(|j| j.at == *at).filter_map(|j| read_json(&job_dir(o, j).join("as-of.json"))).filter_map(|v| Some((utc_secs(v.get("index_as_of")?.as_str()?)? - utc_secs(v.get("cut")?.as_str()?)?) as f64 / 3600.0)).collect();
        if !leads.is_empty() {
            println!("  index lead over the cut: {:.1} to {:.1} hours over {} jobs (both arms can search sources that much newer in the turn)", leads.iter().copied().fold(f64::MAX, f64::min), leads.iter().copied().fold(f64::MIN, f64::max), leads.len());
        }
        let need = n.saturating_sub(1);
        let mut all = broken.is_empty();
        for (name, up, f) in axes {
            let wins = pairs.iter().filter(|(p, a)| if up { f(a) >= f(p) } else { f(a) <= f(p) }).count();
            let (mp, ma) = (pairs.iter().map(|x| f(&x.0)).sum::<f64>() / n as f64, pairs.iter().map(|x| f(&x.1)).sum::<f64>() / n as f64);
            let pass = wins >= need && if up { ma >= mp } else { ma <= mp };
            all &= pass;
            println!("  {name}\tatlas at least as good on {wins}/{n}\tmean plain {mp:.3}\tmean atlas {ma:.3}\t{}", if pass { "PASS" } else { "FAIL" });
        }
        let (lat, line) = latency(&atlas_runs);
        all &= lat;
        println!("  latency\t{line}\t{}", if lat { "PASS" } else { "FAIL" });
        println!("  verdict at {at}: {}", if all { "atlas wins its pair" } else { "atlas loses an axis" });
        if all && at != "rec" {
            winners.push(at.clone());
        }
    }
    println!(
        "\nthe offered compaction point is chosen only among the thresholds where atlas wins its pair: {}",
        if winners.is_empty() { "none".to_string() } else { winners.join(", ") }
    );
    print_spend(&o.work);
    Ok(())
}

/// `--measure`: every finished job's measures, atlas beside plain per
/// compaction point and sample, from the work folder alone (no claude
/// call, no verdict; a smoke folder is allowed). A job that cannot be
/// measured says so.
fn measure_only(o: &Opts, run: &Run) -> Result<(), String> {
    println!("compact-live measures: run {}{} (not a score)", run.id, if run.smoke { ", smoke" } else { "" });
    println!("job\tfacts kept\tmisses\tdangling\tresume tokens\tinput tokens\tratio\thook ms\tcompaction ms");
    let mut js: Vec<&Job> = run.jobs.iter().collect();
    js.sort_by(|a, b| (a.point, &a.at, a.name.rsplit('-').next(), &a.arm).cmp(&(b.point, &b.at, b.name.rsplit('-').next(), &b.arm)));
    for j in js {
        match measure(o, &run.points[j.point], j) {
            Some(m) => println!("{}\t{:.3}\t{:.0}\t{:.3}\t{:.0}\t{:.0}\t{:.1}\t{:.1}\t{:.0}", j.name, m.fidelity, m.misses, m.dangling, m.resume, m.tokens, m.ratio, m.hook_ms, m.duration_ms),
            None => println!("{}\tunmeasurable (no result or no boundary of its own)", j.name),
        }
    }
    Ok(())
}

/// Runs `cargo xtask compact-live`.
///
/// # Errors
///
/// A message for the user.
pub fn run(args: Vec<String>) -> Result<(), String> {
    let o = parse(args)?;
    if o.spend {
        print_spend(&o.work);
        return Ok(());
    }
    if o.measure {
        let run = read_run(&o.work)?.ok_or("no run.json in --work: nothing to measure")?;
        return measure_only(&o, &run);
    }
    if o.rescore {
        let run = read_run(&o.work)?.ok_or("no run.json in --work: nothing to rescore")?;
        return rescore::run(&o, &run);
    }
    if o.score {
        let run = read_run(&o.work)?.ok_or("no run.json in --work: nothing to score")?;
        return score(&o, &run);
    }
    let existing = read_run(&o.work)?;
    let run = match existing {
        Some(r) => {
            // Resuming: the plan is the one recorded, never re-picked.
            if r.smoke != !o.only.is_empty() {
                return Err(if r.smoke { "this work folder holds a smoke run; use a new --work for the scored run".into() } else { "this work folder holds the scored run; a smoke test needs its own --work".into() });
            }
            println!("resuming run {} ({} jobs in its plan)", r.id, r.jobs.len());
            r
        }
        None => {
            if o.transcripts.as_os_str().is_empty() {
                return Err("a new run needs --transcripts DIR".into());
            }
            let (rec, ranked) = points(&o);
            if rec.is_empty() {
                return Err("no recorded compaction in range under --transcripts".into());
            }
            let (ps, mut js) = jobs(&o, &rec, &ranked);
            if !o.only.is_empty() {
                js.retain(|j| o.only.iter().any(|x| j.name.starts_with(x.as_str())));
            }
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            Run { id: format!("{stamp}-{}", &atlas::sha256::hex(plan_json(&ps, &js).to_string().as_bytes())[..8]), smoke: !o.only.is_empty(), points: ps, jobs: js }
        }
    };
    // What this launch runs: `--only` filters a resumed plan too, while
    // run.json keeps every job of the plan.
    let todo = selected(&run.jobs, &o.only)?;
    let owned: Vec<Job> = todo.iter().map(|j| (*j).clone()).collect();
    let used: HashSet<usize> = todo.iter().map(|j| j.point).collect();
    let cal = o.calibrate.as_deref().and_then(calibration);
    let (input, output) = estimate(&owned, cal);
    let ats: HashSet<&str> = todo.iter().map(|j| j.at.as_str()).collect();
    println!("compact-live plan: {} points from {} sessions, {} jobs ({} compaction points, plain and atlas paired at each){}", used.len(), run.points.iter().enumerate().filter(|(i, _)| used.contains(i)).map(|(_, p)| &p.session).collect::<HashSet<_>>().len(), todo.len(), ats.len(), if run.smoke { "; smoke run" } else { "" });
    if todo.len() < run.jobs.len() {
        println!("  --only: {} of the plan's {} jobs: {}", todo.len(), run.jobs.len(), todo.iter().map(|j| j.name.as_str()).collect::<Vec<_>>().join(", "));
    }
    // What the index knows up to, against each cut: every job's memory
    // store is pruned to its cut, but the index is the configuration's.
    let index_time = atlas::config::Config::load(&o.atlas_config).ok().and_then(|c| c.db).and_then(|db| index_as_of(&db).ok());
    let mut older = Vec::new();
    for (i, p) in run.points.iter().enumerate().filter(|(i, _)| used.contains(i)) {
        let short: String = p.session.chars().take(8).collect();
        let t = point_time(p).unwrap_or_default();
        let old = index_time.as_ref().is_some_and(|x| t.as_str() < x.as_str());
        if old {
            older.push(format!("p{i:02}"));
        }
        println!("  p{i:02} {short} line {} {} preTokens {} cut {t}{}", p.line, p.trigger, p.pre, if old { " (older than the index)" } else { "" });
        // What PreCompact reads at the recorded cut: the segment since the
        // previous boundary (the history before it is never read).
        if o.dry_run {
            match point_bytes(p) {
                Some((before, seg)) => {
                    let facts = std::fs::read_to_string(&p.file).map_or(0, |t| fact_count(&p.file, &t, p.start, p.line));
                    println!("    segment {:.1} MB after {:.1} MB of history, {facts} facts", seg as f64 / 1048576.0, before as f64 / 1048576.0)
                }
                None => println!("    segment size unknown ({} unreadable)", p.file.display()),
            }
        }
    }
    match &index_time {
        Some(x) => println!("index as of {x}: {} of {} cuts are older ({}); each job's memory store is pruned to its cut, the index is not", older.len(), used.len(), if older.is_empty() { "none".to_string() } else { older.join(", ") }),
        None => println!("index time unknown (no readable index in --atlas-config)"),
    }
    let mut at_list: Vec<&str> = ats.into_iter().collect();
    at_list.sort();
    for at in at_list {
        let js: Vec<Job> = owned.iter().filter(|j| j.at == at).cloned().collect();
        println!("  at {at}: {} jobs, {:.1}M input tokens", js.len(), estimate(&js, cal).0 as f64 / 1e6);
    }
    match cal {
        Some((c, t)) => println!("estimate: about {:.1}M input and {:.2}M output tokens, calibrated on the measured smoke spend ({c:.2} input per context token per compaction, {:.0}k per scripted turn)", input as f64 / 1e6, output as f64 / 1e6, t / 1e3),
        None => println!("estimate: about {:.1}M input and {:.2}M output tokens (each compaction reads its context; each scripted turn about 210k; --calibrate SMOKE_WORK uses a smoke run's measured spend)", input as f64 / 1e6, output as f64 / 1e6),
    }
    if o.dry_run {
        println!("dry run: nothing written and claude was not called");
        return Ok(());
    }
    for p in [&o.atlas_bin, &o.atlas_config] {
        if !p.is_file() {
            return Err(format!("{} is not a file (--atlas-bin and --atlas-config are needed to run)", p.display()));
        }
    }
    // The atlas arm measures the compaction features, which are off unless
    // the configuration turns them on: a run without them measures nothing.
    let c = atlas::config::Config::load(&o.atlas_config).map_err(|e| e.to_string())?;
    if c.compact.as_ref().and_then(|x| x.enabled) != Some(true) {
        return Err(format!("{} does not set \"compact\": {{\"enabled\": true}}; the atlas arm would run without the compaction features", o.atlas_config.display()));
    }
    // A cut older than the index would search sources it could not have
    // seen, in both arms: refused unless the run says it accepts that.
    let Some(index_time) = index_time else { return Err(format!("{}: cannot tell what its index knows up to", o.atlas_config.display())) };
    if !older.is_empty() && !o.index_newer_ok {
        return Err(format!("{} cuts are older than the index (as of {index_time}): {}; --index-newer-ok runs them anyway (the score says how many)", older.len(), older.join(", ")));
    }
    write_run(&o.work, &run)?;
    let mut failed = 0;
    let mut races = 0;
    let finished = |j: &Job| read_json(&o.work.join("jobs").join(&j.name).join("result.json")).is_some();
    for &j in &todo {
        if finished(j) {
            println!("{}: finished before, never rerun", j.name);
            continue;
        }
        // The spend stop: the run pauses (resumable) once its measured input
        // passes --stop-at; a relaunch with a higher one goes on.
        let (done, spent_in, ..) = spend(&o.work);
        if spent_in > o.stop_at {
            print_spend(&o.work);
            let left = todo.iter().filter(|j| !finished(j)).count();
            return Err(format!(
                "paused: {spent_in} input tokens spent by the {done} finished jobs, past --stop-at {}; {left} jobs unfinished, and a failed or unfinished attempt's spend is not counted (the overshoot is about one job); launch again with a higher --stop-at to resume",
                o.stop_at
            ));
        }
        match run_job(&o, &run.id, &run.points[j.point], j) {
            Ok(card) => {
                races += usize::from(j.arm == "atlas" && card != "own");
                println!("{}: done (card read from: {card})", j.name);
            }
            Err(e) => {
                failed += 1;
                println!("{}: failed, resumable: {e}", j.name);
            }
        }
    }
    print_spend(&o.work);
    if races > 0 {
        println!("warning: {races} atlas checkpoints did not read their own transcript (the PreCompact race)");
    }
    if failed > 0 {
        return Err(format!("{failed} jobs failed; launch the same command again to resume them"));
    }
    if run.smoke {
        println!("smoke run finished; it is never scored");
        return Ok(());
    }
    score(&o, &run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str) -> Job {
        Job { name: name.into(), point: 0, arm: "plain".into(), at: "rec".into(), keep: 1, ctx: 1 }
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atlas-compact-live-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const OLD: &str = "11111111-2222-3333-4444-555555555555";

    /// A seeded plan of 5 jobs resumed with `--only` naming 1: the launch
    /// runs that one (the dry run counts it alone), run.json keeps all 5,
    /// and an `--only` that names nothing is refused before any spend.
    #[test]
    fn only_filters_a_resumed_plan_and_keeps_the_file_whole() {
        let work = scratch("only");
        let names = ["t150000-01-atlas-s1", "t150000-01-plain-s1", "t250000-01-atlas-s1", "p01-plain-s1", "t150000-00-atlas-s1"];
        let point = Point { file: work.join("none.jsonl"), session: OLD.into(), line: 1, start: 0, pre: 1, trigger: "auto".into() };
        let seeded = Run { id: "seed".into(), smoke: true, points: vec![point], jobs: names.iter().map(|n| job(n)).collect() };
        write_run(&work, &seeded).unwrap();
        let r = read_run(&work).unwrap().unwrap();
        assert_eq!(r.jobs.len(), 5);
        let only = vec!["t150000-01-atlas".to_string()];
        let todo = selected(&r.jobs, &only).unwrap();
        assert_eq!(todo.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(), ["t150000-01-atlas-s1"]);
        assert_eq!(selected(&r.jobs, &[]).unwrap().len(), 5);
        assert!(selected(&r.jobs, &["t999".to_string()]).is_err());
        // The resumed launch: the dry run passes with the filter, refuses an
        // unmatched one, and the file it read still holds every job.
        let w = work.to_string_lossy().to_string();
        let args = |only: &str| vec!["--work".to_string(), w.clone(), "--only".into(), only.into(), "--dry-run".into()];
        run(args("t150000-01-atlas")).unwrap();
        assert!(run(args("t999")).unwrap_err().contains("names no job"));
        write_run(&work, &r).unwrap();
        assert_eq!(read_run(&work).unwrap().unwrap().jobs.len(), 5);
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A recorded session: a prompt, an answer with usage, a second prompt.
    fn recording() -> String {
        [
            format!(r#"{{"type":"user","sessionId":"{OLD}","uuid":"u1","timestamp":"2026-11-01T10:00:00.000Z","message":{{"role":"user","content":"first"}},"cwd":"x"}}"#),
            format!(r#"{{"type":"assistant","sessionId":"{OLD}","uuid":"a1","parentUuid":"u1","timestamp":"2026-11-01T10:05:00.000Z","message":{{"id":"m1","role":"assistant","content":[{{"type":"text","text":"ok"}}],"usage":{{"input_tokens":10,"cache_read_input_tokens":150000,"output_tokens":5}}}}}}"#),
            format!(r#"{{"type":"user","sessionId":"{OLD}","uuid":"u2","parentUuid":"a1","timestamp":"2026-11-01T12:00:00.000Z","message":{{"role":"user","content":"next"}}}}"#),
        ]
        .join("\n")
            + "\n"
    }

    #[test]
    fn atlas_arm_passes_the_compact_instructions_as_the_argument() {
        assert_eq!(compact_prompt("plain"), "/compact");
        let a = compact_prompt("atlas");
        assert!(a.starts_with("/compact When compacting, write keys and deltas"), "{a}");
        assert!(a.contains("End with the current step and the next action."));
        assert!(!a.contains("<!--") && !a.contains("## Compact instructions"));
    }

    #[test]
    fn each_job_gets_its_own_stable_session_id() {
        let a = job_session("run-a", "p00-atlas@rec");
        assert_eq!(a, job_session("run-a", "p00-atlas@rec"));
        assert_ne!(a, job_session("run-a", "p00-plain@rec"));
        assert_ne!(a, job_session("run-b", "p00-atlas@rec"));
        let parts: Vec<usize> = a.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
        assert!(a.as_bytes()[14] == b'4' && a.as_bytes()[19] == b'8' && a.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }

    #[test]
    fn rekey_rewrites_only_the_session_field() {
        let l = format!(r#"{{"sessionId":"{OLD}","message":{{"content":"see {OLD}"}}}}"#);
        let out = rekey(&[l.as_str()], OLD, "new-id");
        assert_eq!(out, format!("{{\"sessionId\":\"new-id\",\"message\":{{\"content\":\"see {OLD}\"}}}}\n"));
    }

    /// The PreCompact race: the job's transcript is its own staged copy,
    /// written before claude starts, so the hook reads it at once (a fork's
    /// transcript did not exist yet). A relaunch keeps the compacted copy.
    #[test]
    fn staged_copy_is_readable_by_precompact_and_kept_on_relaunch() {
        let d = scratch("stage");
        let rec = d.join(format!("{OLD}.jsonl"));
        std::fs::write(&rec, recording()).unwrap();
        let o = fake_config(&d);
        let p = Point { file: rec, session: OLD.into(), line: 2, start: 0, pre: 150_015, trigger: "manual".into() };
        let j = Job { name: "p00-atlas@rec".into(), point: 0, arm: "atlas".into(), at: "rec".into(), keep: 2, ctx: 150_015 };
        let sid = job_session("run-a", &j.name);
        let projects = d.join("projects");
        let (cwd, _) = stage_in(&projects, &o, &p, &j, &sid, true).unwrap();
        let tx = transcript_in(&projects, &cwd, &sid);
        let text = std::fs::read_to_string(&tx).unwrap();
        assert_eq!(text.lines().count(), 2, "the cut keeps 2 lines");
        assert!(!text.contains(OLD) && text.contains(&sid));
        assert!(std::fs::read_to_string(cwd.join("CLAUDE.md")).unwrap().contains("Compact instructions"));
        let t0 = std::time::Instant::now();
        let (t, from) = atlas::checkpoint::tail_settled(&tx, &sid, atlas::checkpoint::SETTLE_WAIT);
        assert!(matches!(from, atlas::checkpoint::TailFrom::Own));
        assert_eq!(t.last_ctx, 150_015);
        assert!(t0.elapsed() < std::time::Duration::from_millis(500), "no settle wait: {:?}", t0.elapsed());
        // After the compaction the copy is the compacted session: kept.
        std::fs::write(&tx, "compacted\n").unwrap();
        stage_in(&projects, &o, &p, &j, &sid, false).unwrap();
        assert_eq!(std::fs::read_to_string(&tx).unwrap(), "compacted\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A memory store at `path` with one version per (key, time).
    fn store_with(path: &Path, recs: &[(&str, &str)]) {
        let mut s = atlas::memory::MemoryStore::open(path).unwrap();
        let mut w = s.writer().unwrap();
        for (key, time) in recs {
            let d = atlas::memory::Draft { title: format!("{key} title"), body: format!("{key} body {time}"), time: Some(time.to_string()), ..Default::default() };
            if w.revise(key, None, &d).is_err() {
                w.create(key, "rule", "", "", &d, None).unwrap();
            }
        }
        w.commit().unwrap();
    }

    /// A base atlas config under `d/base`: an index with a WAL beside it and
    /// a memory store with its `.jsonl` copy, one record from before the
    /// recording's cut and one from after it. Returns the options for it.
    fn fake_config(d: &Path) -> Opts {
        let base = d.join("base");
        std::fs::create_dir_all(&base).unwrap();
        for (f, body) in [("x.sqlite", "index"), ("x.sqlite-wal", "wal"), ("other.sqlite", "no")] {
            std::fs::write(base.join(f), body).unwrap();
        }
        store_with(&base.join("memory.sqlite"), &[("rule:old", "2026-11-01T09:00:00.000Z"), ("rule:new", "2026-11-02T09:00:00.000Z")]);
        let b = base.to_string_lossy().replace('\\', "/");
        let cfg = d.join("atlas.json");
        std::fs::write(&cfg, format!(r#"{{"db": "{b}/x.sqlite", "memory": {{"store": "{b}/memory.sqlite", "export_dir": "{b}/exports", "snapshot_hours": 7}}, "compact": {{"enabled": true, "card_tokens": 900}}, "watch": false}}"#)).unwrap();
        parse(vec!["--work".into(), d.join("work").to_string_lossy().into_owned(), "--atlas-config".into(), cfg.to_string_lossy().into_owned(), "--atlas-bin".into(), "atlas.exe".into()]).unwrap()
    }

    /// The baseline is symmetric and every job is isolated: both arms run
    /// the same atlas server and hooks, each from its own config over its
    /// own copy of the state; the plain arm's has the compaction features
    /// off. Nothing points at the shared base state.
    #[test]
    fn each_job_runs_atlas_on_its_own_state_plain_with_compaction_off() {
        let d = scratch("plain");
        let o = fake_config(&d);
        let rec = d.join(format!("{OLD}.jsonl"));
        std::fs::write(&rec, recording()).unwrap();
        let p = Point { file: rec, session: OLD.into(), line: 2, start: 0, pre: 1, trigger: "manual".into() };
        let projects = d.join("projects");
        for arm in ["plain", "atlas"] {
            let j = Job { name: format!("p00-{arm}@rec"), point: 0, arm: arm.into(), at: "rec".into(), keep: 2, ctx: 1 };
            let (cwd, mcp) = stage_in(&projects, &o, &p, &j, &job_session("r", &j.name), true).unwrap();
            let cfg = job_config_path(&cwd);
            let want = cfg.to_string_lossy().replace('\\', "/");
            let m = std::fs::read_to_string(mcp).unwrap();
            let s = std::fs::read_to_string(cwd.join(".claude").join("settings.json")).unwrap();
            assert!(m.contains("\"atlas\"") && m.contains(&want), "{arm}: {m}");
            assert!(s.contains("PreCompact") && s.contains("SessionStart") && s.contains(&want), "{arm}: {s}");
            assert_eq!(cwd.join("CLAUDE.md").exists(), arm == "atlas");
            let text = std::fs::read_to_string(&cfg).unwrap();
            assert!(!text.contains("/base/"), "{arm} points at the shared state: {text}");
            let v = atlas::json::parse(&text).unwrap();
            assert_eq!(v.pointer("/compact/enabled").and_then(Value::as_bool), Some(arm == "atlas"));
            assert_eq!(v.pointer("/compact/card_tokens").and_then(Value::as_u64), Some(900));
            assert_eq!(v.pointer("/memory/snapshot_hours").and_then(Value::as_u64), Some(7));
            let st = cwd.join("state");
            for (f, body) in [("x.sqlite", "index"), ("x.sqlite-wal", "wal"), ("memory.sqlite.jsonl", "")] {
                assert_eq!(std::fs::read_to_string(st.join(f)).unwrap(), body, "{arm}: {f}");
            }
            // The store as of the cut (10:05): the record written after it is gone.
            let store = atlas::memory::MemoryStore::open(&st.join("memory.sqlite")).unwrap();
            assert!(store.head("rule:old").unwrap().is_some() && store.head("rule:new").unwrap().is_none(), "{arm}");
            let asof = read_json(&cwd.join("as-of.json")).unwrap();
            assert_eq!(asof.get("cut").and_then(Value::as_str), Some("2026-11-01T10:05:00.000Z"));
            assert_eq!(asof.get("records_dropped").and_then(Value::as_u64), Some(1));
            assert!(s.contains("hook-time") && s.contains(HOOKS_LOG), "{arm}: hooks are timed: {s}");
            assert!(!st.join("other.sqlite").exists());
            assert!(v.get("db").and_then(Value::as_str).unwrap().ends_with("/state/x.sqlite"));
            assert!(v.pointer("/memory/export_dir").and_then(Value::as_str).unwrap().ends_with("/state/exports"));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The store as of a cut: later versions are dropped and the head moves
    /// back; supersessions and retirements after it are undone; search
    /// finds only what was there; the store still opens and takes writes.
    #[test]
    fn a_store_is_pruned_to_the_cut() {
        let d = scratch("prune");
        let path = d.join("m.sqlite");
        store_with(&path, &[("rule:a", "2026-10-01T00:00:00.000Z"), ("rule:a", "2026-10-03T00:00:00.000Z"), ("rule:b", "2026-10-01T00:00:00.000Z"), ("rule:c", "2026-10-04T00:00:00.000Z"), ("rule:e", "2026-10-01T00:00:00.000Z")]);
        {
            let mut s = atlas::memory::MemoryStore::open(&path).unwrap();
            let mut w = s.writer().unwrap();
            w.retire("rule:b", "gone").unwrap();
            w.commit().unwrap();
        }
        // rule:e was retired before the cut and reactivated after it (the
        // reactivation is only in the `.jsonl` copy).
        let mut log = std::fs::read_to_string(atlas::memory::jsonl_path(&path)).unwrap();
        log.push_str(r#"{"op":"retire","key":"rule:e","time":"2026-10-01T12:00:00.000Z","reason":"x"}"#);
        log.push(char::from(10));
        log.push_str(r#"{"op":"reactivate","key":"rule:e","time":"2026-10-03T12:00:00.000Z","reason":"y"}"#);
        log.push(char::from(10));
        std::fs::write(atlas::memory::jsonl_path(&path), log).unwrap();
        // rule:b's retirement is stamped now (after the cut), so it is undone.
        assert_eq!(prune_store(&path, "2026-10-02T00:00:00.000Z").unwrap(), (2, 1));
        let mut s = atlas::memory::MemoryStore::open(&path).unwrap();
        let a = s.head("rule:a").unwrap().unwrap();
        assert_eq!((a.seq, a.body.as_str()), (1, "rule:a body 2026-10-01T00:00:00.000Z"));
        assert_eq!(s.head("rule:b").unwrap().unwrap().status, "active");
        assert!(s.head("rule:c").unwrap().is_none());
        assert_eq!(s.head("rule:e").unwrap().unwrap().status, "retired");
        assert!(s.search(&["rule".into()], None, None, 10).unwrap().iter().all(|(h, _, _)| h.key != "rule:c"));
        let mut w = s.writer().unwrap();
        w.create("rule:d", "rule", "", "", &atlas::memory::Draft { title: "d".into(), body: "d".into(), ..Default::default() }, None).unwrap();
        w.commit().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The rescore's exception: the job's own checkpoint lane (its card,
    /// written after the cut) keeps every version; everything else after the
    /// cut goes, another session's checkpoint included.
    #[test]
    fn the_prune_keeps_the_jobs_own_checkpoint() {
        let d = scratch("prune-keep");
        let path = d.join("m.sqlite");
        let (before, after) = ("2026-10-01T00:00:00.000Z", "2026-10-03T00:00:00.000Z");
        store_with(&path, &[("rule:a", before), ("rule:a", after), ("journal:session:s1:0001", after), ("journal:session:s2:0001", after), ("rule:c", after)]);
        assert_eq!(prune_store_keeping(&path, "2026-10-02T00:00:00.000Z", "journal:session:s1:").unwrap(), (3, 2));
        let s = atlas::memory::MemoryStore::open(&path).unwrap();
        assert_eq!(s.head("journal:session:s1:0001").unwrap().unwrap().time, after);
        assert!(s.head("journal:session:s2:0001").unwrap().is_none() && s.head("rule:c").unwrap().is_none());
        assert_eq!(s.head("rule:a").unwrap().unwrap().seq, 1);
        // An empty `keep` is the plain prune: the checkpoint goes too.
        let path = d.join("m2.sqlite");
        store_with(&path, &[("journal:session:s1:0001", after)]);
        assert_eq!(prune_store_keeping(&path, "2026-10-02T00:00:00.000Z", "").unwrap(), (1, 1));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The vector cache: a second store with the same records embeds
    /// nothing and ends with the same vectors as the first, which embedded
    /// them; a store with one new record embeds only that one.
    #[test]
    fn the_vector_cache_embeds_each_record_text_once() {
        use atlas::memory::vectors::RecVecs;
        let d = scratch("vec-cache");
        let cache = d.join("vectors.sqlite");
        let settings = |n: &str| {
            let mut s = atlas::memory::settings::Settings::from_config(None, &d.join(n).join("index.sqlite")).unwrap();
            s.snapshot_secs = 0;
            std::fs::create_dir_all(s.model.parent().unwrap()).unwrap();
            std::fs::write(&s.model, atlas::embed::tiny_model_bytes(true, 7)).unwrap();
            s
        };
        let at = "2026-10-01T00:00:00.000Z";
        let (a, b, c) = (settings("a"), settings("b"), settings("c"));
        store_with(&a.store, &[("rule:a", at), ("rule:b", at)]);
        store_with(&b.store, &[("rule:a", at), ("rule:b", at)]);
        store_with(&c.store, &[("rule:a", at), ("rule:b", at), ("rule:c", at)]);
        assert_eq!(crate::memory_eval::embed_all_pending_cached(&a, &cache).unwrap(), Some(2));
        assert_eq!(crate::memory_eval::embed_all_pending_cached(&b, &cache).unwrap(), Some(0));
        assert_eq!(crate::memory_eval::embed_all_pending_cached(&c, &cache).unwrap(), Some(1));
        let m = atlas::embed::Model::load(&a.model).unwrap();
        let vecs = |s: &atlas::memory::settings::Settings| {
            let store = atlas::memory::MemoryStore::open(&s.store).unwrap();
            let v = RecVecs::load(&store, &m.id, m.dim).unwrap();
            assert_eq!(v.missing, 0);
            let mut rows: Vec<(String, Vec<u8>)> = store.conn().prepare("SELECT sha, vec FROM rec_vec").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
            rows.sort();
            rows
        };
        assert_eq!(vecs(&a), vecs(&b));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The rescore's pre-embed: a prune that rolls a record back to a version
    /// with no vector leaves it pending, which would start the hooks' detached
    /// embedder. After `embed_all_pending` nothing is pending or missing, the
    /// hooks open and answer a prompt, and once they are dropped nothing holds
    /// the state folder: removing it succeeds.
    #[test]
    fn the_pre_embed_leaves_the_hooks_nothing_to_embed() {
        use atlas::memory::vectors::{self, RecVecs};
        let d = scratch("pre-embed");
        let state = d.join("state");
        let mut s = atlas::memory::settings::Settings::from_config(None, &state.join("index.sqlite")).unwrap();
        s.snapshot_secs = 0;
        std::fs::create_dir_all(s.model.parent().unwrap()).unwrap();
        std::fs::write(&s.model, atlas::embed::tiny_model_bytes(true, 7)).unwrap();
        let (before, after) = ("2026-10-01T00:00:00.000Z", "2026-10-03T00:00:00.000Z");
        store_with(&s.store, &[("rule:a", before), ("rule:b", before), ("rule:a", after)]);
        let m = atlas::embed::Model::load(&s.model).unwrap();
        {
            // Every head has a vector before the prune, as in a live store.
            let store = atlas::memory::MemoryStore::open(&s.store).unwrap();
            while vectors::embed_pending(&store, &m, 128, 1).unwrap() > 0 {}
            assert!(vectors::pending(&store, &m.id, 1).unwrap().is_empty());
        }
        assert_eq!(prune_store_keeping(&s.store, "2026-10-02T00:00:00.000Z", "").unwrap(), (1, 0));
        {
            let store = atlas::memory::MemoryStore::open(&s.store).unwrap();
            assert_eq!(vectors::pending(&store, &m.id, 10).unwrap().len(), 1, "rule:a's head is now v1, which has no vector");
        }
        assert_eq!(crate::memory_eval::embed_all_pending(&s).unwrap(), Some(1));
        {
            let store = atlas::memory::MemoryStore::open(&s.store).unwrap();
            assert!(vectors::pending(&store, &m.id, 1).unwrap().is_empty());
            assert_eq!(RecVecs::load(&store, &m.id, m.dim).unwrap().missing, 0);
        }
        let mut h = atlas::hooks::Hooks::open(s.clone()).unwrap();
        h.set_command_hook();
        let ev = atlas::hooks::HookEvent::from_value(&atlas::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "s1",
            "transcript_path": d.join("t.jsonl").to_string_lossy().into_owned(),
            "cwd": d.to_string_lossy().into_owned(),
            "prompt": "what does rule a say about the body",
        }));
        let _ = h.handle(&ev);
        assert!(!h.embedding_started(), "the pre-embed left nothing for a background embedder");
        drop(h);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// A cut's time is its newest kept record's; the hooks' latency is the
    /// PreCompact and the SessionStart after it, and a missing one is NaN.
    #[test]
    fn cut_time_and_hook_wall_time() {
        let r = recording();
        let lines: Vec<&str> = r.lines().collect();
        assert_eq!(cut_time(&lines[..2]).as_deref(), Some("2026-11-01T10:05:00.000Z"));
        assert_eq!(cut_time(&["{}"]), None);
        assert_eq!(utc_secs("1970-01-02T00:00:01.500Z"), Some(86_401));
        assert_eq!(utc_secs("2026-11-02T15:19:37.834Z").unwrap() - utc_secs("2026-11-02T02:30:38.022Z").unwrap(), 12 * 3600 + 48 * 60 + 59);
        // The scripted turn replays a person's prompt, never a slash command
        // (a recorded `/compact` would compact again instead of working).
        let d = scratch("prompt");
        let f = d.join("s.jsonl");
        std::fs::write(&f, [r#"{"type":"user","message":{"role":"user","content":"/compact"}}"#, r#"{"type":"user","message":{"role":"user","content":"carry on"}}"#].join("\n")).unwrap();
        assert_eq!(next_prompt(&f, 0).as_deref(), Some("carry on"));
        let _ = std::fs::remove_dir_all(&d);
        let log = [
            r#"{"event":"SessionStart","source":"resume","ms":30.0,"code":0}"#,
            r#"{"event":"PreCompact","source":"","ms":41.5,"code":0}"#,
            r#"{"event":"SessionStart","source":"compact","ms":12.5,"code":0}"#,
            r#"{"event":"PreToolUse","source":"","ms":9.0,"code":0}"#,
        ]
        .join("\n");
        assert_eq!(hook_wall(&log), 54.0);
        let twice = format!("{log}
{}", r#"{"event":"PreCompact","source":"","ms":900.0,"code":0}"#);
        assert_eq!(hook_wall(&twice), 54.0, "a second compaction in the turn is not the job's");
        assert!(hook_wall(&log.replace("\"compact\"", "\"resume\"")).is_nan());
        assert!(hook_wall("").is_nan());
    }

    /// B2: a retried turn starts from the compacted session, never on top
    /// of a failed attempt.
    #[test]
    fn a_retried_turn_starts_from_the_compacted_session() {
        let d = scratch("retry");
        let f = d.join("s.jsonl");
        std::fs::write(&f, "a\nb\nc\n").unwrap();
        let n = line_count(&f);
        assert_eq!(n, 3);
        std::fs::write(&f, "a\nb\nc\npartial 1\npartial 2\n").unwrap();
        truncate_lines(&f, n).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\nb\nc\n");
        truncate_lines(&f, 9).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\nb\nc\n", "a shorter one is kept");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Samples per pair, and the arm order alternating across samples and
    /// points.
    #[test]
    fn pairs_are_sampled_with_alternating_arm_order() {
        let mut o = parse(vec!["--work".into(), "w".into(), "--thresholds".into(), "1".into()]).unwrap();
        o.thresholds.clear();
        let pt = |line| Point { file: PathBuf::from("x.jsonl"), session: "s".into(), line, start: 0, pre: 1, trigger: "manual".into() };
        let (_, js) = jobs(&o, &[pt(5), pt(9)], &[]);
        let names: Vec<&str> = js.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "p00-atlas@rec-s1", "p00-plain@rec-s1", "p00-plain@rec-s2", "p00-atlas@rec-s2", "p00-atlas@rec-s3", "p00-plain@rec-s3",
                "p01-plain@rec-s1", "p01-atlas@rec-s1", "p01-atlas@rec-s2", "p01-plain@rec-s2", "p01-plain@rec-s3", "p01-atlas@rec-s3",
            ]
        );
        assert!(js.iter().all(|j| j.keep == if j.point == 0 { 5 } else { 9 }));
        o.samples = 1;
        assert_eq!(jobs(&o, &[pt(5)], &[]).1.len(), 2);
    }

    fn m(tokens: f64, hook_ms: f64, duration_ms: f64) -> Measured {
        Measured { tokens, hook_ms, duration_ms, ..Default::default() }
    }

    #[test]
    fn medians_per_field_and_strict_latency() {
        assert_eq!(median_of(&[m(9.0, 1.0, 0.0), m(1.0, 3.0, 0.0), m(5.0, 2.0, 0.0)]).tokens, 5.0);
        assert_eq!(median_of(&[m(1.0, 0.0, 0.0), m(4.0, 0.0, 0.0)]).tokens, 2.5);
        assert!(latency(&[m(0.0, 50.0, 60_000.0), m(0.0, 80.0, 40_000.0)]).0);
        let (ok, line) = latency(&[m(0.0, 50.0, 60_000.0), m(0.0, f64::NAN, 40_000.0)]);
        assert!(!ok && line.contains("1 without a timing"), "{line}");
        let (ok, line) = latency(&[m(0.0, 50.0, 0.0)]);
        assert!(!ok && line.contains("1 without a compaction duration"), "{line}");
        assert!(!latency(&[m(0.0, 300.0, 600_000.0)]).0, "p95 at 250 ms or more fails");
        assert!(!latency(&[m(0.0, 200.0, 10_000.0)]).0, "over 1% of the compaction fails");
        assert!(!latency(&[]).0);
    }

    #[test]
    fn the_headline_leads_with_tokens_saved_and_accuracy_gained() {
        let a = |tokens, fidelity| Measured { tokens, fidelity, ..Default::default() };
        let pairs = [(a(1000.0, 0.5), a(800.0, 0.6))];
        let sampled = [(vec![a(900.0, 0.5), a(1000.0, 0.5), a(1200.0, 0.5)], vec![a(800.0, 0.6), a(700.0, 0.6), a(850.0, 0.6)])];
        let h = headline(&pairs, &sampled);
        assert!(h[0].starts_with("TOKENS") && h[0].contains("saved 200 (+20.0%)") && h[0].contains("spread plain 300, atlas 150"), "{}", h[0]);
        assert!(h[2].starts_with("ACCURACY facts kept") && h[2].contains("gained +0.100 (+20.0%)"), "{}", h[2]);
    }

    #[test]
    fn the_estimate_uses_a_smoke_runs_measured_rates() {
        let d = scratch("cal");
        let mut j = job("t150000-00-plain-s1");
        j.ctx = 200_000;
        write_run(&d, &Run { id: "r".into(), smoke: true, points: Vec::new(), jobs: vec![j.clone()] }).unwrap();
        assert_eq!(calibration(&d), None);
        std::fs::create_dir_all(d.join("jobs").join(&j.name)).unwrap();
        std::fs::write(d.join("jobs").join(&j.name).join("result.json"), r#"{"compact_tokens":220000,"turn_tokens":301000}"#).unwrap();
        assert_eq!(calibration(&d), Some((1.1, 301_000.0)));
        assert_eq!(estimate(&[j.clone()], Some((1.1, 301_000.0))).0, 521_000);
        assert_eq!(estimate(&[j], None).0, 410_000);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// B1: only a boundary at or past the cut is the job's own.
    #[test]
    fn a_boundary_counts_only_past_the_cut() {
        let d = scratch("boundary");
        let tx = d.join("s.jsonl");
        let b = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"preTokens":5}}"#;
        let u = r#"{"type":"user","message":{"content":"x"}}"#;
        std::fs::write(&tx, [u, b, u, u].join("\n")).unwrap();
        assert_eq!(new_boundary(&tx, 4), None, "the recording's own boundary is not the job's");
        std::fs::write(&tx, [u, b, u, u, b, u].join("\n")).unwrap();
        assert_eq!(new_boundary(&tx, 4), Some(4));
        assert_eq!(new_boundary(&d.join("missing.jsonl"), 0), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_active_goal_is_seen_and_a_clear_ends_it() {
        let set = r#"{"type":"attachment","attachment":{"type":"goal_status","met":false,"condition":"x"}}"#;
        let met = r#"{"type":"attachment","attachment":{"type":"goal_status","met":true,"condition":"x"}}"#;
        let clear = r#"{"type":"user","message":{"content":"<command-name>/goal</command-name>\n<command-args>clear</command-args>"}}"#;
        assert!(!goal_active(&[]));
        assert!(goal_active(&[set]));
        assert!(!goal_active(&[set, met]));
        assert!(!goal_active(&[set, clear]));
        assert!(goal_active(&[set, clear, set]));
        let d = scratch("goals");
        let tx = d.join("s.jsonl");
        let boundary = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"preTokens":5}}"#;
        std::fs::write(&tx, [set, boundary, set, set].join("\n")).unwrap();
        assert_eq!(goals_after(&tx), 2);
        std::fs::write(&tx, [set, boundary].join("\n")).unwrap();
        assert_eq!(goals_after(&tx), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Resume as ruled: tool-result bytes after the boundary, to the first
    /// edit attempt or `ExitPlanMode`; prompts and text are not counted.
    #[test]
    fn resume_counts_tool_results_to_the_plan_or_first_edit() {
        let d = scratch("resume");
        let tx = d.join("s.jsonl");
        let before = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t0","content":"BEFORE THE BOUNDARY"}]}}"#;
        let boundary = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"preTokens":5}}"#;
        let prompt = r#"{"type":"user","message":{"content":"a long next prompt that is not a tool result"}}"#;
        let read = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#;
        let r1 = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"0123456789"}]}}"#;
        let r2 = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":[{"type":"text","text":"abcde"}]}]}}"#;
        let plan = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t3","name":"ExitPlanMode","input":{}}]}}"#;
        let side = r#"{"type":"user","isSidechain":true,"message":{"content":[{"type":"tool_result","tool_use_id":"s","content":"SIDECHAIN"}]}}"#;
        std::fs::write(&tx, [before, boundary, prompt, read, r1, side, r2, plan, r1].join("\n")).unwrap();
        assert_eq!(resume_bytes(&tx), 15);
        std::fs::write(&tx, [before, boundary, prompt, read, r1, r2].join("\n")).unwrap();
        assert_eq!(resume_bytes(&tx), 15, "no plan or edit: to the end of the turn");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A resumed session's totals are cumulative (its history included): a
    /// call's own spend is the difference from the previous call's totals,
    /// and the baseline call must be local.
    #[test]
    fn spend_is_the_difference_of_cumulative_totals() {
        let r = |i: u64, o: u64, cost: f64| atlas::json::parse(&format!(r#"{{"total_cost_usd":{cost},"num_turns":0,"duration_api_ms":0,"modelUsage":{{"fable":{{"inputTokens":{i},"cacheReadInputTokens":1000,"outputTokens":{o}}},"opus":{{"inputTokens":5}}}}}}"#)).unwrap();
        let base = totals(&r(119_868, 856_293, 1156.5));
        assert_eq!(base, (120_873, 856_293, 1156.5));
        let own = spent(totals(&r(270_050, 862_178, 1158.25)), base);
        assert_eq!((own.0, own.1), (150_182, 5885));
        assert!((own.2 - 1.75).abs() < 1e-9);
        assert_eq!(spent((1, 1, 1.0), (5, 5, 2.0)), (0, 0, 0.0));
        assert!(local_only(&r(1, 1, 1.0)).is_ok());
        let model = atlas::json::parse(r#"{"num_turns":1,"duration_api_ms":900}"#).unwrap();
        assert!(local_only(&model).unwrap_err().contains("reached a model"));
    }

    /// compact_tokens was always 0: a `/compact` without spend of its own
    /// counts the boundary's preTokens and says so.
    #[test]
    fn compact_spend_falls_back_to_pre_tokens_only_without_usage() {
        let d = scratch("spend");
        let tx = d.join("s.jsonl");
        let boundary = r#"{"type":"system","subtype":"compact_boundary","isSidechain":false,"content":"Conversation compacted","timestamp":"2026-11-04T18:00:00Z","compactMetadata":{"trigger":"manual","preTokens":263199,"durationMs":49054}}"#;
        let summary = r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":[{"type":"text","text":"Summary: the work"}]},"timestamp":"2026-11-04T18:00:01Z"}"#;
        std::fs::write(&tx, format!("{}{boundary}\n{summary}\n", recording())).unwrap();
        assert_eq!(compact_spend((0, 0, 0.0), &tx), (263_199, None, "preTokens"), "the output is unknown, not 0");
        assert_eq!(compact_spend((263_009, 3000, 1.0), &tx), (263_009, Some(3000), "usage"));
        assert_eq!(compact_spend((0, 0, 0.0), &d.join("missing.jsonl")), (0, None, "none"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn max_turns_is_a_finished_turn() {
        assert!(result_of(br#"{"is_error":true,"subtype":"error_max_turns","num_turns":16}"#, Some(1), 0).is_ok());
        assert!(result_of(br#"{"is_error":true,"subtype":"error_during_execution"}"#, Some(1), 0).unwrap_err().contains("error_during_execution"));
        assert!(result_of(b"not json", Some(1), 12).unwrap_err().contains("without JSON"));
    }

    #[test]
    fn spend_sums_finished_jobs() {
        let d = scratch("sum");
        for (j, v) in [
            ("a", r#"{"tokens":300000,"compact_output_tokens":4000,"turn_output_tokens":1000,"cost_usd":1.5,"compact_tokens_source":"usage"}"#),
            ("b", r#"{"tokens":200000,"compact_output_tokens":null,"turn_output_tokens":0,"cost_usd":0.5,"compact_tokens_source":"preTokens"}"#),
        ] {
            std::fs::create_dir_all(d.join("jobs").join(j)).unwrap();
            std::fs::write(d.join("jobs").join(j).join("result.json"), v).unwrap();
        }
        std::fs::create_dir_all(d.join("jobs").join("unfinished")).unwrap();
        assert_eq!(spend(&d), (2, 500_000, 5000, 2.0, 1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn score_refuses_smoke_unfinished_and_mixed_runs() {
        let work = std::env::temp_dir().join(format!("atlas-compact-live-{}", std::process::id()));
        let o = parse(vec!["--work".into(), work.to_string_lossy().into_owned()]).unwrap();
        let point = Point { file: PathBuf::from("x.jsonl"), session: "s".into(), line: 1, start: 0, pre: 1, trigger: "manual".into() };
        let mut run = Run { id: "run-a".into(), smoke: true, points: vec![point], jobs: vec![job("p00-plain@rec"), job("p00-atlas@rec")] };
        assert!(score(&o, &run).unwrap_err().contains("smoke"));
        run.smoke = false;
        write_run(&work, &run).unwrap();
        // Round trip: the plan is read back, never re-picked.
        let back = read_run(&work).unwrap().unwrap();
        assert_eq!((back.id.as_str(), back.jobs.len(), back.smoke), ("run-a", 2, false));
        assert!(score(&o, &run).unwrap_err().contains("not finished"));
        for (j, id) in [("p00-plain@rec", "run-a"), ("p00-atlas@rec", "run-b")] {
            let d = work.join("jobs").join(j);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("result.json"), atlas::json!({"run": id}).to_string()).unwrap();
        }
        assert!(score(&o, &run).unwrap_err().contains("another run"));
        // The harness conditions refuse: a job on another session, an atlas
        // card not read from its own transcript.
        run.jobs[1].arm = "atlas".into();
        for (j, v) in [("p00-plain@rec", atlas::json!({"run": "run-a", "same_session": false, "card": "-"})), ("p00-atlas@rec", atlas::json!({"run": "run-a", "same_session": true, "card": "parent"}))] {
            std::fs::write(work.join("jobs").join(j).join("result.json"), v.to_string()).unwrap();
        }
        let e = score(&o, &run).unwrap_err();
        assert!(e.contains("p00-plain@rec resumed another session") && e.contains("p00-atlas@rec has a card not read from its own transcript (parent)"), "{e}");
        let _ = std::fs::remove_dir_all(&work);
    }
}
