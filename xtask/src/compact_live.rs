//! `cargo xtask compact-live`: the live head-to-head of atlas-assisted
//! against plain Claude Code compaction, paired per compaction point
//! (`docs/design/compaction.md`, section 5). It spends Claude usage, so it
//! never runs without the maintainer's word; `--dry-run` picks the points,
//! writes nothing and prints the plan and its token estimate.
//!
//! ```text
//! cargo xtask compact-live --transcripts DIR --work DIR --atlas-bin EXE --atlas-config FILE
//!     [--claude EXE] [--sessions 4] [--per-session 3] [--thresholds 150000,250000,400000]
//!     [--per-threshold N] [--only JOB,JOB] [--dry-run | --score]
//! ```
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
//! Each job copies its cut into a scratch project under `--work`, resumes it
//! headless with `--fork-session` (the copy is never changed) and runs
//! `/compact`: `plain` with no hooks, no MCP servers and no CLAUDE.md
//! (`--setting-sources project`, empty project settings,
//! `--strict-mcp-config`); `atlas` with atlas's hooks and server from
//! `--atlas-bin` and `--atlas-config` and the Compact instructions as the
//! project's CLAUDE.md. One scripted turn then replays the session's real
//! next prompt in plan mode with edits, writes and shells refused, so nothing
//! outside the scratch folder can change.
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
//! Scoring, per compaction point (`rec` and each T): atlas against plain on
//! ratio pre/(summary+card), fidelity, post-compaction misses, dangling
//! refs, resume tokens and the run's input tokens; an axis passes when atlas
//! is at least as good on all pairs but one and on the mean. Latency is
//! judged on its own bar: the atlas hooks' time at most 1% of the
//! compaction's own time on every pair and under 250 ms at p95. No
//! transcript text is printed or written outside the work folder.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use atlas::json::Value;

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
    dry_run: bool,
    score: bool,
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
        dry_run: false,
        score: false,
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
            "--dry-run" => o.dry_run = true,
            "--score" => o.score = true,
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

/// Picks the points: per session, the compactions in range, deduplicated
/// across resumed copies; the sessions with the most, spread within each.
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
/// past it, with an earlier request of that segment below T. Returns
/// (segment start, cut, context at the cut).
fn crossings(file: &Path, t: u64) -> Vec<(usize, usize, u64)> {
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
                if c <= t + t / 4 {
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
fn jobs(o: &Opts, rec: &[Point], ranked: &[PathBuf]) -> (Vec<Point>, Vec<Job>) {
    let mut ps: Vec<Point> = rec.to_vec();
    let mut js = Vec::new();
    for (i, p) in rec.iter().enumerate() {
        for arm in ["plain", "atlas"] {
            js.push(Job { name: format!("p{i:02}-{arm}@rec"), point: i, arm: arm.into(), at: "rec".into(), keep: p.line, ctx: p.pre });
        }
    }
    let want = o.per_threshold.unwrap_or(rec.len());
    for &t in &o.thresholds {
        let mut got = 0;
        for f in ranked {
            if got >= want {
                break;
            }
            let cs = crossings(f, t);
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
                for arm in ["plain", "atlas"] {
                    js.push(Job { name: format!("t{t}-{got:02}-{arm}"), point: i, arm: arm.into(), at: t.to_string(), keep: cut, ctx });
                }
                got += 1;
            }
        }
    }
    (ps, js)
}

/// The session's next prompt after line `from`: the first person's text
/// (kept in memory; passed to the scripted turn only).
fn next_prompt(file: &Path, from: usize) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    for l in text.lines().skip(from) {
        if !l.contains("\"type\":\"user\"") || l.contains("\"isCompactSummary\":true") || l.contains("\"tool_result\"") || l.contains("\"isMeta\":true") || l.contains("\"isSidechain\":true") {
            continue;
        }
        let v = atlas::json::parse(l).ok()?;
        let t = atlas::checkpoint::turn_text(&v);
        if !t.trim().is_empty() && !t.starts_with('<') {
            return Some(t);
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

/// Writes a job's scratch project: the cut transcript where Claude Code
/// resumes it, the settings and MCP files, and for the atlas arm the
/// Compact instructions. Returns (cwd, mcp config).
fn stage(o: &Opts, p: &Point, j: &Job) -> Result<(PathBuf, PathBuf), String> {
    let e = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let cwd = std::path::absolute(o.work.join("jobs").join(&j.name)).map_err(|x| e(&o.work, x))?;
    std::fs::create_dir_all(cwd.join(".claude")).map_err(|x| e(&cwd, x))?;
    let text = std::fs::read_to_string(&p.file).map_err(|x| e(&p.file, x))?;
    let kept: Vec<&str> = text.lines().take(j.keep).collect();
    let proj = projects_dir()?.join(atlas::transcripts::project_of_cwd(&cwd));
    std::fs::create_dir_all(&proj).map_err(|x| e(&proj, x))?;
    let dst = proj.join(format!("{}.jsonl", p.session));
    std::fs::write(&dst, kept.join("\n") + "\n").map_err(|x| e(&dst, x))?;
    let bin = o.atlas_bin.to_string_lossy().replace('\\', "/");
    let cfg = o.atlas_config.to_string_lossy().replace('\\', "/");
    let (settings, mcp) = if j.arm == "atlas" {
        let hook = atlas::json!([{"hooks": [{"type": "command", "command": format!("\"{bin}\" --config \"{cfg}\" --hook"), "timeout": 30}]}]);
        std::fs::write(cwd.join("CLAUDE.md"), atlas::claude_settings::COMPACT_BLOCK).map_err(|x| e(&cwd, x))?;
        (
            atlas::json!({"hooks": {"PreCompact": hook.clone(), "SessionStart": hook.clone(), "PreToolUse": hook}}),
            atlas::json!({"mcpServers": {"atlas": {"command": bin, "args": ["--config", cfg]}}}),
        )
    } else {
        (atlas::json!({}), atlas::json!({"mcpServers": {}}))
    };
    std::fs::write(cwd.join(".claude").join("settings.json"), atlas::json::to_string_pretty(&settings)).map_err(|x| e(&cwd, x))?;
    let mcp_path = cwd.join("mcp.json");
    std::fs::write(&mcp_path, atlas::json::to_string_pretty(&mcp)).map_err(|x| e(&cwd, x))?;
    Ok((cwd, mcp_path))
}

/// The isolating flags every call carries.
fn base_args(mcp: &Path) -> Vec<String> {
    ["--setting-sources", "project", "--strict-mcp-config", "--mcp-config"].iter().map(|s| s.to_string()).chain([mcp.to_string_lossy().into_owned(), "--output-format".into(), "json".into()]).collect()
}

/// One claude call; an exit without a JSON result, or a result marked as an
/// error, is an infrastructure failure (the job is retried next launch).
fn claude(o: &Opts, cwd: &Path, args: &[String]) -> Result<Value, String> {
    let out = Command::new(&o.claude).args(args).current_dir(cwd).output().map_err(|e| format!("{}: {e}", o.claude))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = atlas::json::parse(text.trim()).map_err(|_| format!("claude exited {:?} without JSON output ({} bytes on stderr)", out.status.code(), out.stderr.len()))?;
    if v.get("is_error").and_then(Value::as_bool) == Some(true) {
        return Err(format!("claude reported an error result ({})", v.get("subtype").and_then(Value::as_str).unwrap_or("error")));
    }
    Ok(v)
}

/// Input tokens a claude result reports (input, cache writes and reads).
fn input_tokens(v: &Value) -> u64 {
    let u = v.get("usage").cloned().unwrap_or(Value::Null);
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens")
}

/// Runs one job in two stages, each recorded so it is never repeated:
/// `/compact` on a fork (`compacted.json`), then the scripted turn
/// (`result.json`).
fn run_job(o: &Opts, run: &str, p: &Point, j: &Job) -> Result<(), String> {
    let (cwd, mcp) = stage(o, p, j)?;
    let compacted = cwd.join("compacted.json");
    let (fork, compact_tokens) = match read_json(&compacted).filter(|v| v.get("run").and_then(Value::as_str) == Some(run)) {
        Some(v) => (v.get("fork").and_then(Value::as_str).unwrap_or("").to_string(), v.get("tokens").and_then(Value::as_u64).unwrap_or(0)),
        None => {
            let mut a = vec!["-p".to_string(), "/compact".into(), "--resume".into(), p.session.clone(), "--fork-session".into()];
            a.extend(base_args(&mcp));
            let r = claude(o, &cwd, &a)?;
            let fork = r.get("session_id").and_then(Value::as_str).ok_or("no session_id in claude's output")?.to_string();
            let t = input_tokens(&r);
            std::fs::write(&compacted, atlas::json!({"run": run, "fork": fork.clone(), "tokens": t}).to_string()).map_err(|e| e.to_string())?;
            (fork, t)
        }
    };
    let mut turn_tokens = 0;
    let mut turn = "none";
    if let Some(prompt) = next_prompt(&p.file, j.keep) {
        let mut a = vec!["-p".to_string(), prompt, "--resume".into(), fork.clone(), "--max-turns".into(), "15".into(), "--permission-mode".into(), "plan".into()];
        a.extend(["--disallowedTools", "Bash", "PowerShell", "Edit", "Write", "MultiEdit", "NotebookEdit"].iter().map(|s| s.to_string()));
        a.extend(base_args(&mcp));
        match claude(o, &cwd, &a) {
            Ok(r) => {
                turn_tokens = input_tokens(&r);
                turn = "ok";
            }
            // Reaching the turn limit is a finished turn, not a failure.
            Err(e) if e.contains("error_max_turns") => turn = "max_turns",
            Err(e) => return Err(e),
        }
    }
    let res = atlas::json!({"run": run, "fork": fork, "cwd": cwd.to_string_lossy().into_owned(), "turn": turn, "tokens": compact_tokens + turn_tokens});
    std::fs::write(cwd.join("result.json"), res.to_string()).map_err(|e| e.to_string())
}

/// Estimated input tokens: each compaction reads its context, each scripted
/// turn about 15 requests over the compacted context plus their results.
fn estimate(js: &[Job]) -> (u64, u64) {
    let compaction: u64 = js.iter().map(|j| j.ctx).sum();
    (compaction + js.len() as u64 * 210_000, js.len() as u64 * 5_000)
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
    /// The atlas hooks' time (PreCompact plus the ref check), and the
    /// compaction's own.
    hook_ms: f64,
    duration_ms: f64,
}

fn measure(o: &Opts, p: &Point, j: &Job, store: &atlas::memory::MemoryStore, index_db: &Path, bpt: f64) -> Option<Measured> {
    let res = read_json(&o.work.join("jobs").join(&j.name).join("result.json"))?;
    let fork = res.get("fork").and_then(Value::as_str).unwrap_or("").to_string();
    let cwd = PathBuf::from(res.get("cwd").and_then(Value::as_str).unwrap_or(""));
    let comps = atlas::compact::replay(&projects_dir().ok()?.join(atlas::transcripts::project_of_cwd(&cwd)).join(format!("{fork}.jsonl")));
    let c = comps.last()?;
    let facts = atlas::compact::facts_at(&p.file, p.start, j.keep);
    let (card, pre_ms) = match (j.arm == "atlas").then(|| atlas::checkpoint::newest(store, &fork).ok().flatten()).flatten() {
        Some(h) => {
            let ms = h.meta.lines().find_map(|l| l.strip_prefix("hook_ms: ")).and_then(|x| x.trim().parse::<f64>().ok()).unwrap_or(0.0);
            (atlas::checkpoint::card(&h), ms)
        }
        None => (String::new(), 0.0),
    };
    let check_ms: f64 = rusqlite::Connection::open(index_db).ok().and_then(|conn| conn.query_row("SELECT ifnull(MAX(hook_ms), 0) FROM compact_log WHERE session = ?1", [&fork], |r| r.get(0)).ok()).unwrap_or(0.0);
    let have = format!("{}\n{card}", c.summary);
    let kept = facts.iter().filter(|f| have.contains(f.as_str())).count();
    let check = atlas::compact::check(store, &[], &c.summary).unwrap_or_default();
    let refs = check.refs + check.bare;
    Some(Measured {
        ratio: c.pre_tokens as f64 / ((c.summary.len() + card.len()) as f64 / bpt).max(1.0),
        fidelity: if facts.is_empty() { 1.0 } else { kept as f64 / facts.len() as f64 },
        misses: c.misses as f64,
        dangling: if refs > 0 { (check.dangling.len() + check.bare_missing) as f64 / refs as f64 } else { 0.0 },
        resume: c.resume_bytes as f64 / bpt,
        tokens: res.get("tokens").and_then(Value::as_u64).unwrap_or(0) as f64,
        hook_ms: if j.arm == "atlas" { pre_ms + check_ms } else { 0.0 },
        duration_ms: c.duration_ms as f64,
    })
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
    let c = atlas::config::Config::load(&o.atlas_config).map_err(|e| e.to_string())?;
    let db = c.db.clone().ok_or("the atlas config names no db")?;
    let s = atlas::memory::settings::Settings::from_config(Some(&c), &db).map_err(|e| e.to_string())?;
    let store = atlas::memory::MemoryStore::open(&s.store).map_err(|e| e.to_string())?;
    let bpt = s.bytes_per_token;
    let axes: [(&str, bool, fn(&Measured) -> f64); 6] = [
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
    let mut winners = Vec::new();
    for at in &ats {
        let pairs: Vec<(Measured, Measured)> = (0..run.points.len())
            .filter_map(|i| {
                let get = |arm: &str| run.jobs.iter().find(|j| j.point == i && j.at == *at && j.arm == arm).and_then(|j| measure(o, &run.points[i], j, &store, &s.index_db, bpt));
                Some((get("plain")?, get("atlas")?))
            })
            .collect();
        let n = pairs.len();
        println!("\ncompaction point {at}: {n} pairs");
        if n == 0 {
            continue;
        }
        let need = n.saturating_sub(1);
        let mut all = true;
        for (name, up, f) in axes {
            let wins = pairs.iter().filter(|(p, a)| if up { f(a) >= f(p) } else { f(a) <= f(p) }).count();
            let (mp, ma) = (pairs.iter().map(|x| f(&x.0)).sum::<f64>() / n as f64, pairs.iter().map(|x| f(&x.1)).sum::<f64>() / n as f64);
            let pass = wins >= need && if up { ma >= mp } else { ma <= mp };
            all &= pass;
            println!("  {name}\tatlas at least as good on {wins}/{n}\tmean plain {mp:.3}\tmean atlas {ma:.3}\t{}", if pass { "PASS" } else { "FAIL" });
        }
        let mut ms: Vec<f64> = pairs.iter().map(|x| x.1.hook_ms).collect();
        ms.sort_by(f64::total_cmp);
        let p95 = ms[((ms.len() as f64 * 0.95).ceil() as usize).clamp(1, ms.len()) - 1];
        let share = pairs.iter().filter(|x| x.1.duration_ms > 0.0).map(|x| x.1.hook_ms / x.1.duration_ms).fold(0.0, f64::max);
        let lat = p95 < 250.0 && share <= 0.01;
        all &= lat;
        println!("  latency\tatlas hooks p95 {p95:.1} ms, largest share of compaction time {:.3}%\t{}", 100.0 * share, if lat { "PASS" } else { "FAIL" });
        println!("  verdict at {at}: {}", if all { "atlas wins its pair" } else { "atlas loses an axis" });
        if all && at != "rec" {
            winners.push(at.clone());
        }
    }
    println!(
        "\nthe offered compaction point is chosen only among the thresholds where atlas wins its pair: {}",
        if winners.is_empty() { "none".to_string() } else { winners.join(", ") }
    );
    Ok(())
}

/// Runs `cargo xtask compact-live`.
///
/// # Errors
///
/// A message for the user.
pub fn run(args: Vec<String>) -> Result<(), String> {
    let o = parse(args)?;
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
            println!("resuming run {} ({} jobs)", r.id, r.jobs.len());
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
    let (input, output) = estimate(&run.jobs);
    let ats: HashSet<&str> = run.jobs.iter().map(|j| j.at.as_str()).collect();
    println!("compact-live plan: {} points from {} sessions, {} jobs ({} compaction points, plain and atlas paired at each){}", run.points.len(), run.points.iter().map(|p| &p.session).collect::<HashSet<_>>().len(), run.jobs.len(), ats.len(), if run.smoke { "; smoke run" } else { "" });
    for (i, p) in run.points.iter().enumerate() {
        let short: String = p.session.chars().take(8).collect();
        println!("  p{i:02} {short} line {} {} preTokens {}", p.line, p.trigger, p.pre);
    }
    let mut at_list: Vec<&str> = ats.into_iter().collect();
    at_list.sort();
    for at in at_list {
        let js: Vec<&Job> = run.jobs.iter().filter(|j| j.at == at).collect();
        println!("  at {at}: {} pairs, {:.1}M input tokens", js.len() / 2, (js.iter().map(|j| j.ctx).sum::<u64>() + js.len() as u64 * 210_000) as f64 / 1e6);
    }
    println!("estimate: about {:.1}M input and {:.2}M output tokens (each compaction reads its context; each scripted turn about 210k)", input as f64 / 1e6, output as f64 / 1e6);
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
    write_run(&o.work, &run)?;
    let mut failed = 0;
    for j in &run.jobs {
        if read_json(&o.work.join("jobs").join(&j.name).join("result.json")).is_some() {
            println!("{}: finished before, never rerun", j.name);
            continue;
        }
        match run_job(&o, &run.id, &run.points[j.point], j) {
            Ok(()) => println!("{}: done", j.name),
            Err(e) => {
                failed += 1;
                println!("{}: failed, resumable: {e}", j.name);
            }
        }
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
        let _ = std::fs::remove_dir_all(&work);
    }
}
