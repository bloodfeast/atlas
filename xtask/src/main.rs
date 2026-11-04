//! `cargo xtask`: atlas's development tasks, in Rust, never shipped.
//!
//! ```text
//! cargo xtask calibrate-baseline --config CONFIG --transcripts DIR --before TIME [--handoff RE]
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
//! cargo xtask langs-bench --lang LANG --root DIR [--scip FILE]
//! ```
//!
//! * `calibrate-baseline` measures what agents spent on lookups in the
//!   transcripts below DIR before TIME (read only), saves it to the
//!   configuration's database for `atlas --token-report`, and prints the
//!   measured ratio beside the upper bound (see `calibrate.rs`).
//! * `token-use --config CONFIG --transcripts DIR --since TIME` breaks down
//!   atlas's answers after TIME by tool and mode: sizes, how much of each was
//!   used later, and episodes by the baseline's rule (see `usage.rs`);
//!   `--replay CONFIG` answers every recorded atlas call again against
//!   CONFIG's stores, so two builds compare on the same transcripts.
//! * `memory-eval` scores atlas as a memory store on a fixed copy of one:
//!   recall-check misses, a question set answered from tool output alone,
//!   and the work-queue tools against the database (see `memory_eval.rs`).
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

mod attrib;
mod calibrate;
mod calibrate_recall;
mod compact_live;
mod coverage;
mod langs_bench;
mod leak;
mod memory_eval;
mod memory_tree;
mod pack_embedder;
mod public_repo;
mod py;
mod release;
mod release_ci;
mod tar;
mod ucd;
mod usage;
mod verify_macos;
mod wiki;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask calibrate-baseline --config CONFIG --transcripts DIR --before TIME [--handoff RE]
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
       cargo xtask memory-tree-check --config CONFIG [--dir DIR]
       cargo xtask compact-live --transcripts DIR --work DIR [--atlas-bin EXE --atlas-config FILE] [--claude EXE] [--sessions N] [--per-session N] [--thresholds T,T] [--per-threshold N] [--only JOB,JOB] [--dry-run | --score]
       cargo xtask langs-bench --lang LANG --root DIR [--scip FILE]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let args: Vec<String> = args.collect();
    let r = match cmd.as_str() {
        "calibrate-baseline" => calibrate_baseline(args, false),
        "token-use" => calibrate_baseline(args, true),
        "memory-eval" => memory_eval(args),
        "memory-tree-check" => memory_tree_check(args),
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
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value\n{USAGE}"));
        match a.as_str() {
            "--config" => config = Some(PathBuf::from(val()?)),
            "--transcripts" => dir = Some(PathBuf::from(val()?)),
            "--before" | "--since" => before = Some(val()?),
            "--handoff" => handoff = val()?,
            "--replay" if token_use => replay = Some(PathBuf::from(val()?)),
            _ => return Err(USAGE.into()),
        }
    }
    let (config, dir, before) = match (config, dir, before) {
        (Some(c), Some(d), Some(b)) => (c, d, b),
        _ => return Err(USAGE.into()),
    };
    let c = atlas::config::Config::load(&config).map_err(|e| e.to_string())?;
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
    let b = calibrate::measure(&dir, &before, &t);
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
