//! `cargo xtask`: atlas's development tasks, in Rust, never shipped.
//!
//! ```text
//! cargo xtask calibrate-baseline --config CONFIG --transcripts DIR --before TIME [--handoff RE] [--project NAME] [--skip TEXT ...] [--keep-duplicates] [--audit FILE]
//! cargo xtask token-use --config CONFIG --transcripts DIR --since TIME [--handoff RE] [--replay CONFIG]
//! cargo xtask attrib TRANSCRIPT.jsonl [TRANSCRIPT.jsonl ...]
//! cargo xtask calibrate-recall SCORES.tsv [MIN_TERMS]
//! cargo xtask pack-embedder MODEL_DIR OUT.atm [--int8] [--query-prefix TEXT] [--ucd UCD_DIR]
//! cargo xtask release --build-only
//! cargo xtask release --remote URL [--push | --dry-run] [--allow-missing] [--skip-tests]
//! cargo xtask release --pr --remote URL [--expect vX.Y.Z] [--dry-run]
//! cargo xtask release assets --dir DIR --notes FILE [--expect vX.Y.Z]
//! cargo xtask release sign vX.Y.Z [--remote URL] [--dry-run]
//! cargo xtask verify-macos
//! cargo xtask coverage [--debug] [--html] [--no-run] [-- TEST_ARGS...]
//! cargo xtask memory-eval --config CONFIG --questions FILE [--project NAME] [--cwd DIR] [--fresh-from DIR] [--floor F ...] [--rows] [--show QID ...]
//! cargo xtask memory-tree-check --config CONFIG [--dir DIR]
//! cargo xtask digest-eval --cases FILE [--rows] [--show ID ...] [--gate ROWS]
//! cargo xtask report-eval --cases FILE [--rows] [--show ID ...] [--gate ROWS] [--budget BYTES]
//! cargo xtask lanes-eval --corpus DIR [--rows] [--bar] [--gate ROWS]
//! cargo xtask clipped --store FILE
//! cargo xtask restore-clipped --store FILE --from JSON
//! cargo xtask langs-bench --lang LANG --root DIR [--scip FILE]
//! ```
//!
//! * `calibrate-baseline` measures what agents spent on lookups in the
//!   transcripts below DIR before TIME (read only), saves it to the
//!   configuration's database for `atlas --token-report`, and prints the
//!   measured ratio beside the upper bound (see `atlas::calibrate`).
//! * `token-use --config CONFIG --transcripts DIR --since TIME` breaks down
//!   atlas's answers after TIME by tool and mode: sizes, how much of each was
//!   used later, and episodes by the baseline's rule (see `usage.rs`);
//!   `--replay CONFIG` answers every recorded atlas call again against
//!   CONFIG's stores, so two builds compare on the same transcripts.
//! * `memory-eval` scores atlas as a memory store on a fixed copy of one:
//!   recall-check misses, a question set answered from tool output alone,
//!   and the work-queue tools against the database (see `memory_eval.rs`).
//! * `digest-eval` scores run digests on a corpus of real failing runs
//!   with hand-labelled root-cause lines: the cause in the digest alone, in
//!   the digest plus one follow-up slice, or missed, and the tokens of each
//!   against the raw output (see `digest_eval.rs`).
//! * `report-eval` scores report digests on a corpus of real subagent
//!   reports with the facts an orchestrator needed labelled by hand: whole,
//!   partial or missed, and the tokens against the report (see
//!   `report_eval.rs`).
//! * `lanes-eval` scores the work queue's brief classification (lane,
//!   priority, resources) on a corpus of real briefs labelled by hand (see
//!   `lanes_eval.rs`).
//! * `memory-tree-check` checks the memory index tree: every memory within 3
//!   reads of the root, pins in the root, no orphans or duplicates, and a
//!   rebuild that would write nothing (see `memory_tree.rs`).
//! * `attrib` attributes the tokens a Claude Code session processed to what
//!   put them in context (see `attrib.rs`).
//! * `calibrate-recall` derives the recall hook's relevance floors from
//!   `atlas --recall-scores` output (see `calibrate_recall.rs`).
//! * `pack-embedder` packs a BERT sentence embedder into atlas's model file
//!   (see `pack_embedder.rs`).
//! * `release` builds the binaries and makes the public release commit
//!   and the wiki commit, after checking the public repository on GitHub
//!   (see `release.rs`, `wiki.rs` and `public_repo.rs`); `--pr`, `assets`
//!   and `sign` are the CI release (see `release_ci.rs`).
//! * `verify-macos` runs the suite, the fixture checks and the benches and
//!   prints what to paste back (see `verify_macos.rs`).
//! * `langs-bench` measures a language of the code index's language table
//!   on a pinned public corpus: scanner recall and item ends against the
//!   language's SCIP indexer, and SCIP rows against the grep truth (see
//!   `langs_bench.rs`).
//! * `coverage` runs the tests with rustc's coverage instrumentation and
//!   prints line and region coverage per module (see `coverage.rs`).

mod accept;
mod attrib;
mod bpe;
use atlas::calibrate;
mod calibrate_recall;
mod compact_live;
mod coverage;
mod digest_eval;
mod langs_bench;
mod leak;
mod memory_eval;
mod rules_eval;
mod memory_tree;
mod pack_embedder;
mod lanes_eval;
mod report_eval;
mod restore_clipped;
mod resume_eval;
mod public_repo;
mod py;
mod release;
mod release_check;
mod release_ci;
mod release_ship;
mod secret_scan;
mod tar;
mod ucd;
mod usage;
mod verify_macos;
mod wiki;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask calibrate-baseline --config CONFIG --transcripts DIR --before TIME [--handoff RE] [--project NAME] [--skip TEXT ...] [--keep-duplicates] [--audit FILE]
       cargo xtask token-use --config CONFIG --transcripts DIR --since TIME [--handoff RE] [--replay CONFIG]
       cargo xtask attrib TRANSCRIPT.jsonl [TRANSCRIPT.jsonl ...]
       cargo xtask calibrate-recall SCORES.tsv [MIN_TERMS]
       cargo xtask pack-embedder MODEL_DIR OUT.atm [--int8] [--query-prefix TEXT] [--ucd UCD_DIR]
       cargo xtask release --build-only
       cargo xtask release --remote URL [--push | --dry-run] [--allow-missing] [--skip-tests]
       cargo xtask release --pr --remote URL [--expect vX.Y.Z] [--dry-run]
       cargo xtask release assets --dir DIR --notes FILE [--expect vX.Y.Z]
       cargo xtask release sign vX.Y.Z [--remote URL] [--dry-run]
       cargo xtask verify-macos
       cargo xtask coverage [--debug] [--html] [--no-run] [-- TEST_ARGS...]
       cargo xtask memory-eval --config CONFIG --questions FILE [--project NAME] [--cwd DIR] [--fresh-from DIR] [--floor F ...] [--rows] [--show QID ...]
       cargo xtask rules-eval --config CONFIG --holds FILE [--project NAME] [--cwd DIR] [--rows] [--dump DIR]
       cargo xtask memory-tree-check --config CONFIG [--dir DIR]
       cargo xtask digest-eval --cases FILE [--rows] [--show ID ...] [--gate ROWS]
       cargo xtask report-eval --cases FILE [--rows] [--show ID ...] [--gate ROWS] [--budget BYTES]
       cargo xtask lanes-eval --corpus DIR [--rows] [--bar] [--gate ROWS]
       cargo xtask clipped --store FILE
       cargo xtask restore-clipped --store FILE --from JSON
       cargo xtask resume-eval --store FILE --transcript PATH --now TIME --questions FILE [--source clear|startup] [--max-tokens N] [--rows]
       cargo xtask token-ratio FILE ... [--bench]
       cargo xtask compact-live --transcripts DIR --work DIR [--atlas-bin EXE --atlas-config FILE] [--claude EXE] [--sessions N] [--per-session N] [--thresholds T,T] [--per-threshold N] [--samples N] [--calibrate DIR] [--index-newer-ok] [--stop-at TOKENS] [--only JOB,JOB] [--dry-run | --score | --spend | --measure]
           --only runs the jobs whose names start with one of JOB, on a fresh or a resumed plan; run.json keeps every job.
           --stop-at is checked only before each job, never mid-job, against the input tokens of finished jobs (cache
           reads and writes included), so the first job always runs and a run can pass it by about one job. It is not a
           dollar guard: a smoke's guard is --only plus the orchestrator's go.
       cargo xtask hook-time LOG -- EXE ARGS...
       cargo xtask langs-bench --lang LANG --root DIR [--scip FILE]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let args: Vec<String> = args.collect();
    let r = match cmd.as_str() {
        "calibrate-baseline" => calibrate_baseline(args, false),
        "token-use" => calibrate_baseline(args, true),
        "memory-eval" => memory_eval(args),
        "rules-eval" => rules_eval(args),
        "memory-tree-check" => memory_tree_check(args),
        "digest-eval" => digest_eval(args),
        "report-eval" => report_eval(args),
        "lanes-eval" => lanes_eval(args),
        "clipped" => clipped(args),
        "restore-clipped" => restore_clipped(args),
        "resume-eval" => resume_eval(args),
        "token-ratio" => {
            let bench = args.iter().any(|a| a == "--bench");
            let files: Vec<String> = args.into_iter().filter(|a| a != "--bench").collect();
            if files.is_empty() {
                Err(USAGE.to_string())
            } else {
                bpe::token_ratio(&files, bench)
            }
        }
        "attrib" => attrib(args),
        "calibrate-recall" => calibrate_recall(args),
        "pack-embedder" => pack_embedder::Args::parse(args).and_then(|a| pack_embedder::run(&a)).map(|s| println!("{s}")),
        "release" => match release::run(args) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("release: {e}");
                return ExitCode::FAILURE;
            }
        },
        "coverage" => coverage::run(args),
        "compact-live" => compact_live::run(args),
        "hook-time" => match compact_live::hook_time(args) {
            Ok(code) => return ExitCode::from(code),
            Err(e) => Err(e),
        },
        "langs-bench" => langs_bench::run(args),
        "verify-macos" => match verify_macos::run() {
            Ok(0) => Ok(()),
            Ok(code) => return ExitCode::from(u8::try_from(code).unwrap_or(1)),
            Err(e) => Err(e),
        },
        _ => Err(USAGE.into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn attrib(args: Vec<String>) -> Result<(), String> {
    if args.is_empty() {
        return Err(attrib::USAGE.into());
    }
    print!("{}", attrib::report(&args)?);
    Ok(())
}

fn calibrate_recall(args: Vec<String>) -> Result<(), String> {
    let (path, min_terms) = match &args[..] {
        [p] => (p, 3),
        [p, m] => (p, m.parse().map_err(|_| format!("MIN_TERMS {m:?} is not an integer"))?),
        _ => return Err(calibrate_recall::USAGE.into()),
    };
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    println!("{}", calibrate_recall::report(&text, min_terms)?);
    Ok(())
}

fn memory_tree_check(args: Vec<String>) -> Result<(), String> {
    let (mut config, mut dir) = (None, None);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match (a.as_str(), it.next()) {
            ("--config", Some(v)) => config = Some(PathBuf::from(v)),
            ("--dir", Some(v)) => dir = Some(PathBuf::from(v)),
            _ => return Err(USAGE.into()),
        }
    }
    let c = atlas::config::Config::load(&config.ok_or(USAGE)?).map_err(|e| e.to_string())?;
    let out = memory_tree::check(&c, dir)?;
    if out.problems.is_empty() {
        Ok(())
    } else {
        Err(format!("memory tree: {} problems", out.problems.len()))
    }
}

fn digest_eval(args: Vec<String>) -> Result<(), String> {
    let mut o = digest_eval::Opts::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value
{USAGE}"));
        match a.as_str() {
            "--cases" => o.cases = PathBuf::from(val()?),
            "--rows" => o.rows = true,
            "--show" => o.show.push(val()?),
            "--gate" => o.gate = Some(PathBuf::from(val()?)),
            _ => return Err(USAGE.into()),
        }
    }
    if o.cases.as_os_str().is_empty() {
        return Err(USAGE.into());
    }
    if digest_eval::run(&o)? {
        Ok(())
    } else {
        Err("digest eval: a case is caught at a worse level than in the gate's rows".into())
    }
}

fn rules_eval(args: Vec<String>) -> Result<(), String> {
    let mut o = rules_eval::Opts { config: PathBuf::new(), holds: PathBuf::new(), project: None, cwd: None, rows: false, dump: None };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value\n{USAGE}"));
        match a.as_str() {
            "--config" => o.config = PathBuf::from(val()?),
            "--holds" => o.holds = PathBuf::from(val()?),
            "--project" => o.project = Some(val()?),
            "--cwd" => o.cwd = Some(PathBuf::from(val()?)),
            "--dump" => o.dump = Some(PathBuf::from(val()?)),
            "--rows" => o.rows = true,
            _ => return Err(USAGE.into()),
        }
    }
    if o.config.as_os_str().is_empty() || o.holds.as_os_str().is_empty() {
        return Err(USAGE.into());
    }
    rules_eval::run(&o)
}

fn memory_eval(args: Vec<String>) -> Result<(), String> {
    let mut o = memory_eval::Opts { config: PathBuf::new(), questions: PathBuf::new(), fresh_from: None, floors: Vec::new(), rows: false, show: Vec::new(), project: None, cwd: None };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next().ok_or_else(|| {
                format!(
                    "{a} needs a value
{USAGE}"
                )
            })
        };
        match a.as_str() {
            "--config" => o.config = PathBuf::from(val()?),
            "--questions" => o.questions = PathBuf::from(val()?),
            "--fresh-from" => o.fresh_from = Some(PathBuf::from(val()?)),
            "--project" => o.project = Some(val()?),
            "--cwd" => o.cwd = Some(PathBuf::from(val()?)),
            "--floor" => o.floors.push(val()?.parse().map_err(|e| format!("--floor: {e}"))?),
            "--rows" => o.rows = true,
            "--show" => o.show.push(val()?),
            _ => return Err(USAGE.into()),
        }
    }
    if o.config.as_os_str().is_empty() || o.questions.as_os_str().is_empty() {
        return Err(USAGE.into());
    }
    if memory_eval::run(&o)? {
        Ok(())
    } else {
        Err("memory eval: a recall-check or work-queue check failed".into())
    }
}

fn calibrate_baseline(args: Vec<String>, token_use: bool) -> Result<(), String> {
    let (mut config, mut dir, mut before, mut handoff, mut replay) = (None, None, None, calibrate::DEFAULT_HANDOFF.to_string(), None);
    let mut audit: Option<PathBuf> = None;
    let mut project: Option<String> = None;
    let mut skip: Vec<String> = Vec::new();
    let mut dedup = true;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value\n{USAGE}"));
        match a.as_str() {
            "--config" => config = Some(PathBuf::from(val()?)),
            "--transcripts" => dir = Some(PathBuf::from(val()?)),
            "--before" | "--since" => before = Some(val()?),
            "--handoff" => handoff = val()?,
            "--audit" if !token_use => audit = Some(PathBuf::from(val()?)),
            "--project" => project = Some(val()?),
            "--skip" if !token_use => skip.push(val()?),
            "--keep-duplicates" if !token_use => dedup = false,
            "--replay" if token_use => replay = Some(PathBuf::from(val()?)),
            _ => return Err(USAGE.into()),
        }
    }
    let (config, dir, before) = match (config, dir, before) {
        (Some(c), Some(d), Some(b)) => (c, d, b),
        _ => return Err(USAGE.into()),
    };
    // A configuration with projects resolves to one (`--project`, else the
    // working folder's), as the server does.
    let raw = atlas::config::Config::load(&config).map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let c = atlas::project::resolve(&raw, &cwd, project.as_deref()).map_err(|e| e.to_string())?.config;
    let db = c.db.clone().ok_or("the configuration names no db")?;
    let vaults: Vec<PathBuf> = c.vaults.iter().map(|v| v.path.clone()).collect();
    let sources: Vec<(PathBuf, Vec<String>)> = c.sources.iter().map(|s| (s.root.clone(), s.include.clone())).collect();
    let t = calibrate::Targets::new(&vaults, &sources, &handoff)?;
    let bpt = c.bytes_per_token.unwrap_or(atlas::tokens::DEFAULT_BYTES_PER_TOKEN);
    if token_use {
        let conn = rusqlite::Connection::open(&db).map_err(|e| e.to_string())?;
        let base = atlas::baseline::latest(&conn).map_err(|e| e.to_string())?.map(|(_, b)| b.median);
        match replay {
            Some(rc) => {
                let rc = atlas::config::Config::load(&rc).map_err(|e| e.to_string())?;
                let mut server = memory_eval::server(&rc)?;
                let mut answer = |tool: &str, input: &atlas::json::Value| -> usize {
                    match server.answer(tool, input) {
                        Ok(a) => a.into_text().len(),
                        Err(e) => e.to_string().len(),
                    }
                };
                usage::report(&dir, &before, &t, bpt, base, Some(&mut answer));
            }
            None => usage::report(&dir, &before, &t, bpt, base, None),
        }
        return Ok(());
    }
    let b = match &audit {
        Some(p) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?);
            calibrate::measure_audited(&dir, &before, &t, &skip, dedup, Some(&mut f))
        }
        None => calibrate::measure_audited(&dir, &before, &t, &skip, dedup, None),
    };
    let conn = rusqlite::Connection::open(&db).map_err(|e| e.to_string())?;
    atlas::baseline::save(&conn, &b).map_err(|e| e.to_string())?;
    let side = atlas::baseline::AtlasSide::read(&conn).map_err(|e| e.to_string())?;
    println!("pre-atlas lookups (calls before {before}): {} episodes in {} of {} transcripts", b.episodes, b.used, b.transcripts);
    println!(
        "  tokens per episode: mean {:.0}, median {:.0}, p90 {:.0}; total {:.0}",
        b.mean / bpt,
        b.median / bpt,
        b.p90 / bpt,
        b.total as f64 / bpt
    );
    let (med, mean) = side.measured(&b);
    println!(
        "atlas answers ({}): {}; tokens returned per answer: median {:.0}",
        atlas::baseline::COMPARABLE.join(","),
        side.returned.len(),
        atlas::baseline::median(&side.returned) / bpt
    );
    println!("ratio per answer, measured (pre-atlas episode / atlas answer): median {med:.1}x, mean {mean:.1}x");
    println!("ratio per answer, upper bound (grep + read counterfactual / returned): {:.1}x", side.upper());
    println!("saved to {}", db.display());
    Ok(())
}

/// Lists, read-only, the journal and handoff entries whose text the title
/// clip cut: the head version's title is exactly
/// [`restore_clipped::CLIP_CHARS`] characters and its body is empty, so
/// everything past that character of a one-line text was lost (journals from b5304ff, handoffs from 39db6c9, until the
/// title clip kept the long line in the body).
fn clipped(args: Vec<String>) -> Result<(), String> {
    let store = match args.as_slice() {
        [flag, path] if flag == "--store" => PathBuf::from(path),
        _ => return Err(USAGE.into()),
    };
    let db = rusqlite::Connection::open_with_flags(&store, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| format!("{}: {e}", store.display()))?;
    let mut q = db
        .prepare(
            "SELECT r.key, r.status, v.time FROM records r JOIN versions v ON v.vid = r.head \
             WHERE r.kind IN ('journal', 'handoff') AND length(v.title) = ?1 AND trim(v.body, ' ' || char(10) || char(13)) = '' ORDER BY v.time",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, String)> = q
        .query_map([restore_clipped::CLIP_CHARS as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .and_then(Iterator::collect)
        .map_err(|e| e.to_string())?;
    for (key, status, time) in &rows {
        println!("{key}\t{status}\t{time}");
    }
    println!("clipped: {}", rows.len());
    Ok(())
}

/// Applies the restore JSON to a store (see `restore_clipped`).
fn restore_clipped(args: Vec<String>) -> Result<(), String> {
    let (mut store, mut from) = (None, None);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--store" => store = it.next().map(PathBuf::from),
            "--from" => from = it.next().map(PathBuf::from),
            _ => return Err(USAGE.into()),
        }
    }
    let (Some(store), Some(from)) = (store, from) else { return Err(USAGE.into()) };
    let t = restore_clipped::run(&store, &from)?;
    for (what, keys) in [("intact (nothing was cut)", &t.intact), ("not clipped any more", &t.not_clipped), ("mismatch (lane, entry or title)", &t.mismatch), ("missing from the store", &t.missing), ("not a journal entry", &t.not_journal)] {
        if !keys.is_empty() {
            println!("skipped, {what}: {}", keys.join(", "));
        }
    }
    println!(
        "restored {}, intact {}, not clipped {}, mismatch {}, missing {}, not a journal entry {}",
        t.restored.len(),
        t.intact.len(),
        t.not_clipped.len(),
        t.mismatch.len(),
        t.missing.len(),
        t.not_journal.len()
    );
    Ok(())
}

fn resume_eval(args: Vec<String>) -> Result<(), String> {
    let mut o = resume_eval::Opts::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value
{USAGE}"));
        match a.as_str() {
            "--store" => o.store = PathBuf::from(val()?),
            "--transcript" => o.transcript = val()?,
            "--now" => o.now = val()?,
            "--questions" => o.questions = PathBuf::from(val()?),
            "--source" => o.source = val()?,
            "--max-tokens" => o.max_tokens = Some(val()?.parse().map_err(|_| "--max-tokens needs an integer".to_string())?),
            "--rows" => o.rows = true,
            _ => return Err(USAGE.into()),
        }
    }
    if o.store.as_os_str().is_empty() || o.now.is_empty() || o.questions.as_os_str().is_empty() {
        return Err(USAGE.into());
    }
    if resume_eval::run(&o)? {
        Ok(())
    } else {
        Err("resume eval: a question missed a fact in the card, or the card is over its token ceiling".into())
    }
}

fn lanes_eval(args: Vec<String>) -> Result<(), String> {
    let mut o = lanes_eval::Opts::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value
{USAGE}"));
        match a.as_str() {
            "--corpus" => o.corpus = PathBuf::from(val()?),
            "--rows" => o.rows = true,
            "--bar" => o.bar = true,
            "--gate" => o.gate = Some(PathBuf::from(val()?)),
            _ => return Err(USAGE.into()),
        }
    }
    if o.corpus.as_os_str().is_empty() {
        return Err(USAGE.into());
    }
    if lanes_eval::run(&o)? {
        Ok(())
    } else {
        Err("lanes eval: the bar is not met, or a brief regressed against the gate's rows".into())
    }
}

fn report_eval(args: Vec<String>) -> Result<(), String> {
    let mut o = report_eval::Opts::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value\n{USAGE}"));
        match a.as_str() {
            "--cases" => o.cases = PathBuf::from(val()?),
            "--rows" => o.rows = true,
            "--show" => o.show.push(val()?),
            "--gate" => o.gate = Some(PathBuf::from(val()?)),
            "--budget" => o.budget = Some(val()?.parse().map_err(|_| format!("--budget needs a number of bytes\n{USAGE}"))?),
            _ => return Err(USAGE.into()),
        }
    }
    if o.cases.as_os_str().is_empty() {
        return Err(USAGE.into());
    }
    if report_eval::run(&o)? {
        Ok(())
    } else {
        Err("report eval: a case keeps fewer facts than in the gate's rows".into())
    }
}
