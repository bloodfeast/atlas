//! `atlas` command line.
//!
//! ```text
//! atlas [--config PATH] [--project NAME] [--vault NAME=PATH ...] [--db PATH]
//!       [--rescan | --recall-check [--phrase TEXT ...] | --token-report | --calibrate [--audit FILE]
//!        | --hook-report [--transcripts DIR] [--since TIME] [--rows]
//!        | --code-index [NAME ...] [--scip FILE] | --version]
//! ```
//!
//! Without a mode flag it serves MCP on stdin and stdout until stdin closes.
//! The configuration comes from `--config`, else (when no `--vault` is
//! given) from the `ATLAS_CONFIG` environment variable, else from
//! `atlas.json` in the folder above the binary, else from `atlas.json` in
//! the user's configuration folder (see [`atlas::dirs`]). When none exists,
//! the first run writes the template there (with an empty `notes` vault
//! beside it) and uses it. `--vault` and `--db` work with or without a
//! configuration, as they did before it existed.
//!
//! * `--setup` writes the configuration if there is none, creates the data
//!   folder, downloads every model file the configuration lists (resuming a
//!   partial download), checks each against its SHA-256, and prints what to
//!   do next. The server also downloads missing models in the background
//!   on start, unless `models.auto_download` is false (see
//!   [`atlas::models`]).
//!
//! * `--rescan` brings the index up to date, prints one JSON line of stats
//!   and exits.
//! * `--recall-check` indexes, then compares the index with a plain scan of
//!   the same files (see [`atlas::recall`]); prints the misses and exits 1
//!   when there are any.
//! * `--bench` prints timings (see `bench.rs`). A measuring mode, built only
//!   with the `dev` feature, as `cargo xtask` builds it; a release binary
//!   does not know the flag.
//! * `--code-index` indexes the configured code roots now (all, or the
//!   named ones), in the foreground, and prints one line of stats per root
//!   (see [`atlas::code_index::index_root`]); `--scip FILE` decodes a SCIP
//!   file already written instead of running rust-analyzer: rust-analyzer's,
//!   or any other SCIP indexer's for that language's rows.
//! * `--embed [THREADS]` brings the index up to date, then embeds every
//!   section without a vector for the semantic model (see
//!   [`atlas::semantic`]) and prints the count and time.
//! * `--embed-check DIR` compares the tokenizer and encoder with reference
//!   files (see `measure.rs`).
//! * With the `dev` feature only: `--judge FILE [--modes M,M] [--runs N]`,
//!   `--symbol-bench FILE [--runs N]` and `--recall-scores FILE [--runs N]`
//!   print the semantic search, code index and recall gate measurements
//!   (see `measure.rs`), and `--bench [RUNS] [--queries FILE] [--before EXE
//!   CONFIG]` the timings.
//! * `atlas digest [--as SHAPE] FILE` prints the digest of a saved output
//!   (a check log: its totals tables, failures, panics and verdicts only);
//!   `atlas crop IN OUT ...` cuts frames or a region out of a picture strip
//!   into a small PNG (see [`atlas::crop`]). Neither reads a configuration.
//! * `--token-report` prints the token ledger's totals by tool and by day:
//!   tokens returned, the counterfactual, and their ratio (see
//!   [`atlas::tokens`]), headed by the measured ratio and the upper bound,
//!   with the brief-without-atlas and hook-injection lines at the end.
//! * `--calibrate [--audit FILE]` measures the pre-atlas baseline the
//!   measured ratio divides by, now, printing progress and the result (see
//!   [`atlas::calibrate`]); the server also runs it in the background at a
//!   start when the stored one is missing or stale. `--audit` writes one line
//!   per lookup episode.
//! * `--compact-report --transcripts DIR [--since TIME] [--rows] [--sweep T,T,...]` replays
//!   every compaction in recorded Claude Code transcripts and prints numbers
//!   only: the compaction ratio, refs and their dangling rate, post-compaction
//!   misses, fact fidelity, resume tokens and the ref check's latency (see
//!   [`atlas::compact`]). The token report ends with the live compaction line.
//! * `--misses-week [--since TIME]` is the weekly misses pass: the ten open
//!   misses seen most since TIME (default seven days ago) and the ten
//!   injected keys that cost the most tokens.
//! * `--hook-report [--transcripts DIR] [--since TIME] [--rows] [--tune-floor SCALE]`
//!   prints the hooks' hit rate: each injected key, marked hit when the
//!   session later used it (see [`atlas::hook_hits`]); `--transcripts` adds
//!   injections reconstructed from past transcripts, `--rows` one row per
//!   key, `--tune-floor` the relevance-floor curve and the tuned floor.

use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;

use atlas::config::Config;
use atlas::paths::Vaults;
use atlas::store::{AtlasSetup, Store};
use atlas::tools::Server;

#[cfg(feature = "dev")]
mod bench;
mod measure;

#[cfg(not(feature = "dev"))]
const USAGE: &str = "usage: atlas [--config PATH] [--project NAME] [--vault NAME=PATH ...] [--db PATH] [--setup | --rescan | --recall-check [--phrase TEXT ...] | --token-report | --calibrate [--audit FILE] | --misses-week [--since TIME] | --hook-report [--transcripts DIR] [--since TIME] [--rows] [--tune-floor SCALE] | --compact-report --transcripts DIR [--since TIME] [--rows] [--sweep T,T,...] | --code-index [NAME ...] [--scip FILE] | --embed [THREADS] | --embed-check DIR | settings on|off update | --version]";
/// With the `dev` feature: the measuring modes too.
#[cfg(feature = "dev")]
const USAGE: &str = "usage: atlas [--config PATH] [--project NAME] [--vault NAME=PATH ...] [--db PATH] [--setup | --rescan | --recall-check [--phrase TEXT ...] | --bench [RUNS] [--queries FILE] [--before EXE CONFIG] | --token-report | --calibrate [--audit FILE] | --misses-week [--since TIME] | --hook-report [--transcripts DIR] [--since TIME] [--rows] [--tune-floor SCALE] | --compact-report --transcripts DIR [--since TIME] [--rows] [--sweep T,T,...] | --code-index [NAME ...] [--scip FILE] | --embed [THREADS] | --embed-check DIR | --judge FILE [--modes M,M] [--runs N] | --symbol-bench FILE [--runs N] | --recall-scores FILE [--runs N] | settings on|off update | --version]";

/// `atlas --help`: every mode, subcommand and flag, one per line.
const HELP: &str = "\
atlas: the project-memory MCP server. With no mode it serves MCP on stdin and stdout.

Where it reads from (before any mode or subcommand):
  --config PATH          atlas.json to use (default: ATLAS_CONFIG, else the user configuration)
  --project NAME         the project of a configuration with projects (default: from the folder)
  --vault NAME=PATH      serve a vault without a configuration (repeatable)
  --db PATH              the index database

Modes:
  --setup                first-run setup: the model, the Claude Code settings offer, the project
  --rescan               re-index every vault and source, then exit
  --hook                 a Claude Code command hook: the event JSON on stdin, the answer on stdout
  --recall-check [--phrase TEXT ...]   every identifier and phrase is found by lookup
  --token-report         the token ledger: what atlas answered against what reading would cost
  --calibrate [--audit FILE]   measure the pre-atlas baseline of the measured ratio now
  --misses-week [--since TIME]         lookups that found nothing, this week
  --hook-report [--transcripts DIR] [--since TIME] [--rows] [--tune-floor SCALE]
  --compact-report --transcripts DIR [--since TIME] [--rows] [--sweep T,T,...]
  --code-index [NAME ...] [--scip FILE]   index code roots now (or one from a SCIP file)
  --embed [THREADS]      embed every section for semantic search
  --embed-check DIR      compare the tokenizer and encoder with reference files in DIR
  --memory-check         the memory store: chains, the .jsonl copy, exports on disk, the round trip
  --memory-snapshot      a verified snapshot of the memory store now
  --memory-as-of KEY TIME   a record as it was at TIME (from the snapshots and archive)
  --memory-export [--adopt] write every export (--adopt: take over files atlas did not write)
  --memory-import EXPORT=PATH ...   import hand-written files into the store
  --memory-tree [DIR] [--check]     rebuild the memory tree in DIR and check it (--check: write nothing)
  --version, -V          the version
  --help, -h             this text

Subcommands:
  atlas run [--digest SHAPE] [--timeout S] [--label TEXT] -- CMD [ARGS...]   run a command, keep its output as a handle
  atlas slice h:ID [--lines A-B | --item NAME | --failures | --grep RE [--context N] | --head N | --tail N | --page K | --digest]
  atlas slice --list [N]  the newest handles
  atlas digest [--as SHAPE] FILE|-   a digest of a file or stdin
  atlas doctor                       say what it can check of the atlas hooks and project roots
  atlas crop IN OUT [--frames A[-B] --of N [--axis x|y]] [--rect X,Y,W,H] [--max W[xH]]   cut a picture
  atlas ruling --by NAME [--defines ID ...] [--ref REF ...] [--date YYYY-MM-DD] [--] TEXT
  atlas entry --title TITLE [--ref REF ...] [--date YYYY-MM-DD] [--time HH:MM] [--] TEXT
  atlas grammar list | accept N | reject N   identifier shapes found in the notes
  atlas settings list | set ID VALUE | off ID | undo [--yes] | offer [--yes ID...]
  atlas settings compact-md [DIR] [--yes]
  atlas settings add-project [--vault | --vault-at FOLDER | --new [--name NAME] [--vault-at FOLDER | --no-vault] | --to PROJECT] [--dry-run] (DIR | --yes)
  atlas settings add-project --no DIR   record the answer no to the session's question about DIR
  atlas settings migrate-projects NAME
  atlas settings orchestrate [--yes [--builders N] [--reviewers N] [--research N] [--mode M] [--handoff N] [--limit N] [--fresh auto|ask] | --no]
  atlas settings on|off update
  atlas rules [scan | counts | sitting 1|2 | link | answer KEY | unmark KEY]   which ruling replaces which (experimental, off by default)
  atlas queue [list | show KEY | drop KEY | done KEY | met KEY [CONDITION] | next | add | plan]   the work queue: launches waiting to start, and the batch that can start now
  atlas ideas [list | show ID | file TEXT... | dismiss ID... | answer ID yes|no | convert ID ruling|queue | labels]   ideas caught from prompts, and the questions about them

SHAPE is test, build, grep, listing, source, summary or generic. More: https://github.com/bloodfeast/atlas/wiki
";

enum Mode {
    Serve,
    Setup,
    Rescan,
    Recall(Vec<String>),
    #[cfg(feature = "dev")]
    Bench(usize, bench::Opts),
    TokenReport,
    /// `--calibrate [--audit FILE]`.
    Calibrate(Option<PathBuf>),
    /// `--misses-week [--since TIME]`: the weekly top-10 misses pass.
    MissesWeek(String),
    /// `--hook-report [--transcripts DIR] [--since TIME] [--rows]`.
    HookReport { transcripts: Option<PathBuf>, since: String, rows: bool, tune: Option<f64> },
    CompactReport { transcripts: Option<PathBuf>, since: String, rows: bool, sweep: Vec<u64> },
    CodeIndex(Vec<String>),
    Embed(usize),
    EmbedCheck(PathBuf),
    #[cfg(feature = "dev")]
    Judge(PathBuf),
    #[cfg(feature = "dev")]
    SymbolBench(PathBuf),
    MemoryCheck,
    #[cfg(feature = "dev")]
    RecallScores(PathBuf),
    MemorySnapshot,
    MemoryAsOf(String, String),
    MemoryExport(bool),
    /// `--memory-tree [DIR] [--check]`.
    MemoryTree(Option<PathBuf>, bool),
    MemoryImport(Vec<(String, PathBuf)>),
    Hook,
    /// `atlas settings on|off update`.
    Settings(bool, String),
    /// A bare-word subcommand (`run`, `slice`, `settings`, `grammar`) and the words
    /// after it.
    Sub(String, Vec<String>),
}

/// `--memory-tree [DIR] [--check]`: rebuilds the memory index tree in DIR
/// (default: the folder of the source the tree's export syncs from) and
/// prints the churn, then checks it as a reader would walk it. `--check`
/// writes nothing and reports what a rebuild would change.
fn memory_tree(setup: &Setup, dir: Option<PathBuf>, check_only: bool) -> Result<ExitCode, String> {
    use atlas::memory::tree;
    let m = &setup.memory;
    let spec = m.tree.as_ref().ok_or("memory.tree is not configured")?;
    let export = m.exports.iter().find(|e| e.name == spec.export).ok_or_else(|| format!("no memory export named {:?}", spec.export))?;
    let dir = match dir {
        Some(d) => d,
        None => setup.atlas.sources.iter().find(|s| s.name == spec.export).map(|s| s.root.clone()).ok_or_else(|| format!("no source named {:?}; pass the memory folder", spec.export))?,
    };
    let t0 = std::time::Instant::now();
    let mut embed = tree::embedder(&m.model, m.embed_threads);
    let (churn, _) = tree::rebuild(&dir, &export.index, spec, &mut *embed, !check_only).map_err(|e| e.to_string())?;
    let verb = if check_only { "would change" } else { "changed" };
    println!("memory tree {}: {verb}: {} ({:.0} ms)", atlas::paths::strip_verbatim(dir.clone()).display(), churn.line(), t0.elapsed().as_secs_f64() * 1e3);
    for f in &churn.written {
        println!("  write {f}");
    }
    for f in &churn.deleted {
        println!("  delete {f}");
    }
    if let Some(b) = &churn.backup {
        println!("  the hand-written index was saved to {b}");
    }
    let nav = tree::check(&dir, &export.index, spec).map_err(|e| e.to_string())?;
    let bpt = m.bytes_per_token;
    println!(
        "navigation: {} memories, {} hubs; reads 1/2/3/more: {}/{}/{}/{}; widest hub {} lines; root {} lines, {} bytes (~{:.0} tokens); root plus path mean {:.0} bytes (~{:.0} tokens), max {} bytes; {} problems",
        nav.leaves,
        nav.hubs,
        nav.reads[1],
        nav.reads[2],
        nav.reads[3],
        nav.reads[4],
        nav.fanout_max,
        nav.root_lines,
        nav.root_bytes,
        nav.root_bytes as f64 / bpt,
        nav.path_bytes_mean,
        nav.path_bytes_mean / bpt,
        nav.path_bytes_max,
        nav.problems.len()
    );
    for p in &nav.problems {
        println!("  {p}");
    }
    let clean = nav.problems.is_empty() && !(check_only && churn.changed());
    Ok(if clean { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

/// `--hook-report`: the injected keys logged by the hooks (and, with
/// `--transcripts`, those reconstructed from past transcripts), each marked
/// hit or not by [`atlas::hook_hits::Judge`]; totals by hook, and with
/// `--rows` one tab-separated row per key for `scripts/tune_inject_floor.py`.
/// The vaults and sources by name, for lines refs.
fn ref_roots(setup: &Setup) -> Vec<atlas::compact::RefRoot> {
    let mut roots: Vec<atlas::compact::RefRoot> = setup.vaults.iter().map(|(n, r)| atlas::compact::RefRoot::Vault(n.clone(), r.clone())).collect();
    roots.extend(setup.atlas.sources.iter().map(|s| atlas::compact::RefRoot::Source(s.clone())));
    roots
}

/// `--compact-report`: replays every compaction in the transcripts below
/// `dir` (see [`atlas::compact::replay`]) and prints numbers only; no
/// transcript text is printed.
fn compact_report(setup: &Setup, dir: &std::path::Path, since: &str, rows: bool, sweep: &[u64], bpt: f64) -> Result<(), String> {
    let s = |e: atlas::Error| e.to_string();
    let mem = atlas::memory::MemoryStore::open(&setup.memory.store).map_err(s)?;
    let roots = ref_roots(setup);
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                // A subagent's transcript never holds the session's compactions.
                Ok(t) if t.is_dir() && p.file_name().is_some_and(|n| n != "subagents") => stack.push(p),
                Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") => files.push(p),
                _ => {}
            }
        }
    }
    files.sort();
    let med = |mut v: Vec<f64>| -> f64 {
        v.sort_by(f64::total_cmp);
        if v.is_empty() { 0.0 } else { atlas::baseline::median(&v) }
    };
    let p95 = |mut v: Vec<f64>| -> f64 {
        v.sort_by(f64::total_cmp);
        if v.is_empty() { 0.0 } else { atlas::baseline::quantile(&v, 0.95) }
    };
    let (mut n, mut manual, mut pre, mut summ, mut refs, mut dangling, mut changed, mut bare, mut missing) = (0usize, 0usize, 0.0, 0.0, 0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut after, mut misses, mut named, mut facts, mut kept) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut ratios, mut post_ratios, mut resume, mut check_ms, mut durations) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    if rows {
        println!("file\ttime\ttrigger\tpre\tpost\tsummary_tokens\tratio\trefs\tdangling\tchanged\tbare\tbare_missing\tlookups_after\tmisses\tmisses_named\tfacts\tfacts_in_summary\tresume_tokens\tcheck_ms");
    }
    // A resumed session copies its parent's history, compactions included.
    let mut counted = std::collections::HashSet::new();
    // The atlas arm's offline side: the checkpoint card each boundary would
    // have had, built from the segment before it and the store as of then.
    let budget = setup.memory.bytes_for(setup.memory.compact.card_tokens);
    let (mut card_toks, mut ckpt_ms, mut hook_ms, mut hook_share) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut miss_cov, mut fact_cov, mut miss_tok, mut overheads, mut summaries, mut auto_pre) = (0usize, 0usize, 0.0, Vec::new(), Vec::new(), Vec::new());
    for f in &files {
        let all = atlas::compact::replay(f);
        if all.is_empty() {
            continue;
        }
        let session = f.file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        let t_read = std::time::Instant::now();
        let segs = atlas::checkpoint::segments(f, &session);
        // A PreCompact hook reads the whole file once; charge each boundary the read.
        let read_ms = t_read.elapsed().as_secs_f64() * 1e3 / segs.len().max(1) as f64;
        for (i, c) in all.into_iter().enumerate() {
            if c.time.as_str() < since || c.summary.is_empty() || c.pre_tokens == 0 || !counted.insert((c.time.clone(), c.pre_tokens)) {
                continue;
            }
            let t0 = std::time::Instant::now();
            let k = atlas::compact::check(&mem, &roots, &c.summary).map_err(s)?;
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            let st = c.summary.len() as f64 / bpt;
            let card = match segs.get(i) {
                Some((time, tail)) => {
                    let t1 = std::time::Instant::now();
                    let state = atlas::checkpoint::state(&mem, &session, &tail.start, time, atlas::checkpoint::Lanes::for_project(setup.memory.graph.as_deref())).map_err(s)?;
                    let b = atlas::checkpoint::body(&state, tail, budget);
                    let built = t1.elapsed().as_secs_f64() * 1e3 + read_ms;
                    ckpt_ms.push(built);
                    hook_ms.push(built + ms);
                    if c.duration_ms > 0 {
                        hook_share.push((built + ms) / c.duration_ms as f64);
                    }
                    b
                }
                None => String::new(),
            };
            card_toks.push(card.len() as f64 / bpt);
            let both = format!("{}\n{}", c.summary, card).to_lowercase();
            miss_cov += c.miss_names.iter().filter(|m| !m.is_empty() && both.contains(m.as_str())).count();
            let both_raw = format!("{}\n{}", c.summary, card);
            fact_cov += c.fact_list.iter().filter(|t| both_raw.contains(t.as_str())).count();
            miss_tok += c.miss_bytes as f64 / bpt;
            summaries.push(st);
            if c.post_tokens > 0 {
                overheads.push((c.post_tokens as f64 - st).max(0.0));
            }
            if c.trigger == "auto" {
                auto_pre.push(c.pre_tokens as f64);
            }
            n += 1;
            manual += usize::from(c.trigger == "manual");
            pre += c.pre_tokens as f64;
            summ += st;
            ratios.push(c.pre_tokens as f64 / st.max(1.0));
            if c.post_tokens > 0 {
                post_ratios.push(c.pre_tokens as f64 / c.post_tokens as f64);
            }
            refs += k.refs;
            dangling += k.dangling.len();
            changed += k.changed.len();
            bare += k.bare;
            missing += k.bare_missing;
            after += c.lookups_after;
            misses += c.misses;
            named += c.misses_named;
            facts += c.facts;
            kept += c.facts_in_summary;
            if c.resumed {
                resume.push(c.resume_bytes as f64 / bpt);
            }
            check_ms.push(ms);
            if c.duration_ms > 0 {
                durations.push(c.duration_ms as f64);
            }
            if rows {
                let stem: String = f.file_stem().map(|x| x.to_string_lossy().chars().take(8).collect()).unwrap_or_default();
                println!(
                    "{stem}\t{}\t{}\t{}\t{}\t{st:.0}\t{:.1}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.0}\t{ms:.1}",
                    c.time.get(..16).unwrap_or(&c.time),
                    c.trigger,
                    c.pre_tokens,
                    c.post_tokens,
                    c.pre_tokens as f64 / st.max(1.0),
                    k.refs,
                    k.dangling.len(),
                    k.changed.len(),
                    k.bare,
                    k.bare_missing,
                    c.lookups_after,
                    c.misses,
                    c.misses_named,
                    c.facts,
                    c.facts_in_summary,
                    c.resume_bytes as f64 / bpt
                );
            }
        }
    }
    let pct = |a: usize, b: usize| if b > 0 { 100.0 * a as f64 / b as f64 } else { 0.0 };
    println!("compact report: {n} compactions ({manual} manual) in {} transcripts; tokens at {bpt} bytes per token", files.len());
    if n == 0 {
        return Ok(());
    }
    println!("ratio pre/summary: {:.1}x pooled, {:.1}x median; pre/post (Claude Code's postTokens): {:.1}x median", pre / summ, med(ratios), med(post_ratios));
    println!(
        "refs: {refs} key@hash refs, {dangling} dangling ({:.1}%), {changed} changed; bare record keys {bare}, {missing} not found ({:.1}%)",
        pct(dangling, refs),
        pct(missing, bare)
    );
    println!("post-compaction misses: {misses} of {after} lookups in the {} calls after each boundary ({:.1}%), {named} of them named in the summary", atlas::compact::MISS_WINDOW, pct(misses, after));
    println!("fidelity: {kept} of {facts} facts used after a boundary are in its summary ({:.1}%)", pct(kept, facts));
    println!("resume tokens (tool results before the first edit, write or commit): median {:.0} over {} resumed compactions", med(resume.clone()), resume.len());
    println!(
        "latency: ref check median {:.1} ms, p95 {:.1} ms; Claude Code's compaction median {:.0} ms",
        med(check_ms.clone()),
        p95(check_ms),
        med(durations.clone())
    );
    // The atlas arm, offline: the checkpoint card beside the same summary.
    println!(
        "checkpoint card: median {:.0} tokens, p95 {:.0}, max {:.0} (budget {}); ratio pre/(summary+card) {:.1}x pooled",
        med(card_toks.clone()),
        p95(card_toks.clone()),
        card_toks.iter().copied().fold(0.0, f64::max),
        setup.memory.compact.card_tokens,
        pre / (summ + card_toks.iter().sum::<f64>())
    );
    println!("checkpoint coverage: misses named in summary or card {miss_cov} of {misses} ({:.1}%; summary alone {named}); facts in summary or card {fact_cov} of {facts} ({:.1}%; summary alone {kept})", pct(miss_cov, misses), pct(fact_cov, facts));
    println!(
        "hook latency (checkpoint build incl. transcript read, plus ref check): median {:.1} ms, p95 {:.1} ms; checkpoint alone p95 {:.1} ms; share of compaction time median {:.3}%, max {:.3}%",
        med(hook_ms.clone()),
        p95(hook_ms),
        p95(ckpt_ms),
        100.0 * med(hook_share.clone()),
        100.0 * hook_share.iter().copied().fold(0.0, f64::max)
    );
    if !sweep.is_empty() {
        threshold_sweep(&files, sweep, med(overheads), med(summaries), med(card_toks), miss_tok / n as f64, med(auto_pre));
    }
    Ok(())
}

/// `--sweep T,T,...`: the token side of each compaction point, with and
/// without atlas (see [`atlas::checkpoint::simulate`]). At every threshold a
/// plain arm and an atlas arm replay the same sessions; each atlas row is
/// judged against plain at the same threshold. The atlas arm's summary is
/// the plain median plus the card (its steered summary needs live runs, so
/// this is its worst case). Plain at Claude Code's default point (the median
/// `preTokens` of recorded automatic compactions) is printed as a labelled
/// reference row only.
fn threshold_sweep(files: &[PathBuf], thresholds: &[u64], overhead: f64, summary: f64, card: f64, reread: f64, auto_pre: f64) {
    use atlas::checkpoint::{simulate, Arm, Cost};
    let mut seen = std::collections::HashSet::new();
    let curves: Vec<atlas::checkpoint::Curve> = files.iter().map(|f| atlas::checkpoint::curve(f, &mut seen)).filter(|c| c.points.len() > 1).collect();
    let requests: usize = curves.iter().map(|c| c.points.len()).sum();
    println!(
        "sweep: {} sessions, {requests} requests; overhead {overhead:.0}, summary {summary:.0}, card {card:.0}, re-read per compaction {reread:.0} tokens (both arms)",
        curves.len()
    );
    let run = |t: u64, s: f64| -> Cost {
        let arm = Arm { threshold: t, overhead: overhead as u64, summary: s as u64, reread: reread as u64 };
        curves.iter().map(|c| simulate(c, &arm)).fold(Cost::default(), |a, b| Cost { compactions: a.compactions + b.compactions, raw: a.raw + b.raw, weighted: a.weighted + b.weighted })
    };
    let pct = |a: f64, b: f64| 100.0 * (a / b.max(1.0) - 1.0);
    println!("threshold\tarm\tcompactions\tinput_tokens\tweighted\tmisses_reread\tatlas_vs_plain_raw\tatlas_vs_plain_weighted");
    let mut wins = Vec::new();
    for &t in thresholds {
        let p = run(t, summary);
        let a = run(t, summary + card);
        println!("{t}\tplain\t{}\t{:.0}\t{:.0}\t{:.0}\t\t", p.compactions, p.raw, p.weighted, p.compactions as f64 * reread);
        println!("{t}\tatlas\t{}\t{:.0}\t{:.0}\t{:.0}\t{:+.2}%\t{:+.2}%", a.compactions, a.raw, a.weighted, a.compactions as f64 * reread, pct(a.raw, p.raw), pct(a.weighted, p.weighted));
        if a.raw <= p.raw && a.weighted <= p.weighted {
            wins.push(t);
        }
    }
    if auto_pre > 0.0 {
        let d = run(auto_pre as u64, summary);
        println!("reference: plain at Claude Code's default point {} (median preTokens of recorded auto compactions): {} compactions, {:.0} input, {:.0} weighted", auto_pre as u64, d.compactions, d.raw, d.weighted);
    }
    println!(
        "thresholds where atlas wins its pair on tokens: {}; the offered point is chosen among the thresholds where atlas wins its pair on tokens and on the live sample's accuracy",
        if wins.is_empty() { "none (offline the card is pure cost: the steered summary is live-only)".to_string() } else { wins.iter().map(u64::to_string).collect::<Vec<_>>().join(", ") }
    );
}

fn hook_report(setup: &Setup, transcripts: Option<&std::path::Path>, since: &str, rows: bool, tune: Option<f64>) -> Result<(), String> {
    use atlas::hook_hits::{self, Injection};
    let s = |e: atlas::Error| e.to_string();
    let conn = atlas::store::open_raw(&setup.db).map_err(|e| e.to_string())?;
    let mut all = hook_hits::logged(&conn, since).map_err(s)?;
    let mem = atlas::memory::MemoryStore::open(&setup.memory.store).map_err(s)?;
    if let Some(dir) = transcripts {
        let skip: std::collections::HashSet<String> = all.iter().map(|r| r.transcript.clone()).collect();
        let found = hook_hits::found_below(dir, since, &skip);
        eprintln!("hook report: {} injected keys reconstructed from transcripts; scoring them", found.len());
        let mut hooks = atlas::hooks::Hooks::open(setup.memory.clone()).map_err(s)?;
        hooks.set_offer_paths(None);
        let mut scored: std::collections::HashMap<String, Vec<atlas::memory::recall::Candidate>> = std::collections::HashMap::new();
        for (i, f) in found.into_iter().enumerate() {
            if !scored.contains_key(&f.prompt) {
                let c = hooks.scores(&f.prompt).map_err(s)?;
                scored.insert(f.prompt.clone(), c);
            }
            let mut inj: Injection = f.injection;
            if let Some(c) = scored[&f.prompt].iter().find(|c| c.head.key == inj.key) {
                inj.fts = Some(c.fts);
                inj.cos = c.cos;
            }
            inj.read = atlas::tokens::record_read(&mem, &inj.key);
            all.push(inj);
            if (i + 1) % 200 == 0 {
                eprintln!("hook report: scored {}", i + 1);
            }
        }
    }
    all.sort_by(|a, b| a.time.cmp(&b.time));
    let mut judge = hook_hits::Judge::new(&mem, &setup.memory.exports);
    let verdicts: Vec<(Injection, Option<&'static str>)> = all.into_iter().map(|i| {
        let h = judge.hit(&i);
        (i, h)
    }).collect();
    let bpt = setup.memory.bytes_per_token;
    println!("hook\tinjected\thits\thit_rate\tby_key\tby_source\tby_quote\ttokens_injected\ttokens_on_hits");
    for (hook, t) in hook_hits::totals(&verdicts) {
        let rate = if t.injected > 0 { t.hits as f64 / t.injected as f64 } else { 0.0 };
        println!("{hook}\t{}\t{}\t{:.1}%\t{}\t{}\t{}\t{:.0}\t{:.0}", t.injected, t.hits, rate * 100.0, t.by_rule[0], t.by_rule[1], t.by_rule[2], t.bytes as f64 / bpt, t.hit_bytes as f64 / bpt);
    }
    if let Some(scale) = tune {
        println!("floor\tkept\thits\thit_rate\ttokens_injected\thits_per_1k_tokens\tnet_tokens");
        let free = verdicts.iter().filter(|(i, _)| i.cos.is_none()).count();
        for step in 0..=100 {
            let p = hook_hits::at_floor(&verdicts, step as f32 / 100.0, scale);
            let rate = if p.kept > 0 { p.hits as f64 / p.kept as f64 * 100.0 } else { 0.0 };
            let per = if p.bytes > 0 { p.hits as f64 / (p.bytes as f64 / bpt) * 1000.0 } else { 0.0 };
            println!("{:.2}\t{}\t{}\t{rate:.1}%\t{:.0}\t{per:.2}\t{:.0}", p.floor, p.kept, p.hits, p.bytes as f64 / bpt, p.net / bpt);
            if p.kept == free {
                break;
            }
        }
        let b = hook_hits::tune(&verdicts, scale);
        println!("tuned recall.inject_floor {:.4} (hit scale {scale}): keeps {} of {}, {} hits, net {:.0} tokens", b.floor, b.kept, verdicts.len(), b.hits, b.net / bpt);
    }
    if rows {
        println!("row\thook\ttime\tsession\tagent\tkey\tcos\tfts\tbytes\tread\thit");
        for (i, h) in &verdicts {
            let opt = |x: Option<f64>| x.map_or("-".to_string(), |v| format!("{v:.4}"));
            println!("row\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}", i.hook, i.time, i.session, i.agent, i.key, opt(i.cos.map(f64::from)), opt(i.fts), i.bytes, i.read, h.unwrap_or("-"));
        }
    }
    Ok(())
}

/// The bare words recognised as `argv[1]`, or after leading
/// [`LEADING_OPTIONS`].
const SUBCOMMANDS: &[&str] = &["run", "slice", "settings", "grammar", "ruling", "entry", "queue", "ideas", "rules"];

/// The usage of a subcommand asked for its help, `None` when it was not: only
/// the first word after it counts, so `atlas run -- cargo --help` still runs and a
/// value or a note's text is never taken for it, plus the word after a settings
/// command, which has help of its own. `help` counts only where the word
/// already means help.
fn sub_help(word: &str, rest: &[String]) -> Option<&'static str> {
    let is_help = |w: Option<&String>| w.is_some_and(|w| w == "-h" || w == "--help");
    let word_means_help = matches!(word, "settings" | "queue" | "ideas") && rest.first().is_some_and(|w| w == "help");
    if !(is_help(rest.first()) || word_means_help || (word == "settings" && is_help(rest.get(1)))) {
        return None;
    }
    Some(match word {
        "run" => atlas::run::USAGE,
        "slice" => atlas::handles::SLICE_USAGE,
        "settings" => atlas::claude_settings::HELP,
        "grammar" => atlas::grammar::USAGE,
        "ruling" | "entry" => atlas::records::USAGE,
        "queue" => atlas::queue::USAGE,
        "ideas" => atlas::ideas::cli::USAGE,
        "rules" => atlas::memory::rules::USAGE,
        _ => return None,
    })
}

/// The options (each with one value) that may come before a subcommand
/// word and apply to it.
const LEADING_OPTIONS: &[&str] = &["--config", "--project"];

/// Splits a subcommand off the command line: the word after any leading
/// `--config PATH` and `--project NAME`, and everything after the word.
/// `argv` keeps the leading options for the option loop. `None` (and
/// `argv` untouched) when there is no subcommand word there.
fn split_subcommand(argv: &mut Vec<String>) -> Option<(String, Vec<String>)> {
    let mut i = 0;
    while i + 1 < argv.len() && LEADING_OPTIONS.contains(&argv[i].as_str()) {
        i += 2;
    }
    if !argv.get(i).is_some_and(|w| SUBCOMMANDS.contains(&w.as_str())) {
        return None;
    }
    let rest = argv.split_off(i + 1);
    let word = argv.pop()?;
    Some((word, rest))
}

/// What the command line resolved to.
pub(crate) struct Setup {
    pub(crate) vaults: Vec<(String, PathBuf)>,
    pub(crate) db: PathBuf,
    pub(crate) atlas: AtlasSetup,
    pub(crate) code: Option<atlas::code::CodeSetup>,
    pub(crate) semantic_model: PathBuf,
    /// Encoder threads (config `semantic.threads`).
    pub(crate) embed_threads: usize,
    /// The memory store, recall and cap settings.
    pub(crate) memory: atlas::memory::settings::Settings,
}

impl Setup {
    pub(crate) fn open(&self) -> Result<Store, String> {
        self.open_typed().map_err(|e| e.to_string())
    }

    /// As [`Setup::open`], keeping the error's kind (a store too new for this
    /// binary is served as a refusal, not a failed start).
    pub(crate) fn open_typed(&self) -> atlas::Result<Store> {
        let vaults = Vaults::new(&self.vaults)?;
        let mut store = Store::open_with(&self.db, vaults, self.atlas.clone())?;
        store.set_memory_sync(Some(self.memory.clone()));
        Ok(store)
    }
}

fn config_path(explicit: Option<PathBuf>, have_vaults: bool) -> Option<PathBuf> {
    if explicit.is_some() {
        return explicit;
    }
    if have_vaults {
        return None;
    }
    if let Some(p) = std::env::var_os("ATLAS_CONFIG").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let beside = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.join("atlas.json")));
    if let Some(b) = beside.filter(|b| b.is_file()) {
        return Some(b);
    }
    atlas::dirs::config_dir().map(|d| d.join("atlas.json"))
}

/// Writes the template configuration and its empty `notes` vault when the
/// file does not exist yet. Returns true when it wrote one.
fn ensure_config(path: &std::path::Path) -> Result<bool, String> {
    if path.exists() {
        return Ok(false);
    }
    let dir = path.parent().ok_or("the configuration path has no folder")?;
    std::fs::create_dir_all(dir.join("notes")).map_err(|e| format!("{}: {e}", dir.display()))?;
    // create_new: two first runs at once write it once.
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            std::io::Write::write_all(&mut f, atlas::config::TEMPLATE.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn run() -> Result<ExitCode, String> {
    let mut specs: Vec<(String, PathBuf)> = Vec::new();
    let mut db: Option<PathBuf> = None;
    let mut config: Option<PathBuf> = None;
    let mut project: Option<String> = None;
    let mut mode = Mode::Serve;
    let mut phrases = Vec::new();
    #[cfg(feature = "dev")]
    let mut bench_opts = bench::Opts::default();
    let mut scip_file: Option<PathBuf> = None;
    #[cfg(feature = "dev")]
    let mut modes: Vec<String> = vec!["sections".into(), "semantic".into(), "hybrid".into()];
    #[cfg(feature = "dev")]
    let mut runs = 20usize;
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    // File tools that need no configuration, run before one is looked for.
    if argv.first().is_some_and(|w| w == "doctor") {
        // `config_path`, not `ensure_config`: the doctor writes nothing.
        return atlas::claude_settings::doctor_cli(&argv[1..], config_path(None, false).as_deref());
    }
    if let Some(w) = argv.first().filter(|w| *w == "digest" || *w == "crop").cloned() {
        let rest = argv.split_off(1);
        return if w == "digest" { atlas::digest::cli(rest) } else { atlas::crop::cli(rest) };
    }
    if let Some((word, rest)) = split_subcommand(&mut argv) {
        // Help touches nothing: no first-run configuration, no store, no
        // settings file, whatever state those are in.
        if let Some(usage) = sub_help(&word, &rest) {
            println!("{usage}");
            return Ok(ExitCode::SUCCESS);
        }
        // Everything after the word is the subcommand's own; the
        // configuration comes from `--config` or `--project` before the
        // word, else ATLAS_CONFIG or beside the binary.
        mode = Mode::Sub(word, rest);
    }
    let mut args = argv.into_iter().peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--vault" => {
                let v = args.next().ok_or("--vault needs NAME=PATH")?;
                let (name, path) = v.split_once('=').ok_or_else(|| format!("--vault {v:?}: expected NAME=PATH"))?;
                specs.push((name.to_string(), PathBuf::from(path)));
            }
            "--db" => db = Some(PathBuf::from(args.next().ok_or("--db needs a path")?)),
            "--config" => config = Some(PathBuf::from(args.next().ok_or("--config needs a path")?)),
            "--rescan" => mode = Mode::Rescan,
            "--setup" => mode = Mode::Setup,
            "--token-report" => mode = Mode::TokenReport,
            "--calibrate" => mode = Mode::Calibrate(None),
            "--audit" => match &mut mode {
                Mode::Calibrate(a) => *a = Some(PathBuf::from(args.next().ok_or("--audit needs a file")?)),
                _ => return Err("--audit goes after --calibrate".into()),
            },
            "--misses-week" => mode = Mode::MissesWeek(String::new()),
            "--hook-report" => mode = Mode::HookReport { transcripts: None, since: String::new(), rows: false, tune: None },
            "--tune-floor" => match &mut mode {
                Mode::HookReport { tune, .. } => *tune = Some(args.next().and_then(|s| s.parse().ok()).ok_or("--tune-floor needs the hit scale (1 for the upper bound)")?),
                _ => return Err("--tune-floor goes after --hook-report".into()),
            },
            "--compact-report" => mode = Mode::CompactReport { transcripts: None, since: String::new(), rows: false, sweep: Vec::new() },
            "--sweep" => match &mut mode {
                Mode::CompactReport { sweep, .. } => {
                    *sweep = args.next().ok_or("--sweep needs thresholds, e.g. 150000,250000,400000")?.split(',').map(|t| t.trim().parse::<u64>().map_err(|_| format!("--sweep: {t:?} is not a token count"))).collect::<Result<_, _>>()?
                }
                _ => return Err("--sweep goes after --compact-report".into()),
            },
            "--transcripts" => match &mut mode {
                Mode::HookReport { transcripts, .. } | Mode::CompactReport { transcripts, .. } => *transcripts = Some(PathBuf::from(args.next().ok_or("--transcripts needs a folder")?)),
                _ => return Err("--transcripts goes after --hook-report or --compact-report".into()),
            },
            "--since" => match &mut mode {
                Mode::HookReport { since, .. } | Mode::MissesWeek(since) | Mode::CompactReport { since, .. } => *since = args.next().ok_or("--since needs a time")?,
                _ => return Err("--since goes after --hook-report, --compact-report or --misses-week".into()),
            },
            "--rows" => match &mut mode {
                Mode::HookReport { rows, .. } | Mode::CompactReport { rows, .. } => *rows = true,
                _ => return Err("--rows goes after --hook-report or --compact-report".into()),
            },
            "--code-index" => {
                let mut names = Vec::new();
                while let Some(n) = args.peek().filter(|n| !n.starts_with("--")) {
                    names.push(n.clone());
                    args.next();
                }
                mode = Mode::CodeIndex(names);
            }
            "--embed" => {
                let threads = match args.peek() {
                    Some(n) if n.parse::<usize>().is_ok() => args.next().and_then(|n| n.parse().ok()).unwrap_or(0),
                    _ => 0,
                };
                mode = Mode::Embed(threads);
            }
            "--embed-check" => mode = Mode::EmbedCheck(PathBuf::from(args.next().ok_or("--embed-check needs a folder")?)),
            #[cfg(feature = "dev")]
            "--judge" => mode = Mode::Judge(PathBuf::from(args.next().ok_or("--judge needs a file")?)),
            #[cfg(feature = "dev")]
            "--symbol-bench" => mode = Mode::SymbolBench(PathBuf::from(args.next().ok_or("--symbol-bench needs a file")?)),
            #[cfg(feature = "dev")]
            "--modes" => modes = args.next().ok_or("--modes needs a list")?.split(',').map(str::to_string).collect(),
            #[cfg(feature = "dev")]
            "--runs" => runs = args.next().and_then(|n| n.parse().ok()).ok_or("--runs needs a number")?,
            "--scip" => scip_file = Some(PathBuf::from(args.next().ok_or("--scip needs a file")?)),
            "--recall-check" => mode = Mode::Recall(Vec::new()),
            "--memory-check" => mode = Mode::MemoryCheck,
            #[cfg(feature = "dev")]
            "--recall-scores" => mode = Mode::RecallScores(PathBuf::from(args.next().ok_or("--recall-scores needs a file")?)),
            "--memory-snapshot" => mode = Mode::MemorySnapshot,
            "--memory-as-of" => {
                let key = args.next().ok_or("--memory-as-of needs KEY TIME")?;
                let time = args.next().ok_or("--memory-as-of needs KEY TIME")?;
                mode = Mode::MemoryAsOf(key, time);
            }
            "--memory-export" => mode = Mode::MemoryExport(false),
            "--memory-tree" => {
                let dir = args.peek().filter(|n| !n.starts_with("--")).map(PathBuf::from);
                if dir.is_some() {
                    args.next();
                }
                mode = Mode::MemoryTree(dir, false);
            }
            "--check" => {
                if let Mode::MemoryTree(_, c) = &mut mode {
                    *c = true;
                }
            }
            "--adopt" => {
                if let Mode::MemoryExport(a) = &mut mode {
                    *a = true;
                }
            }
            "--memory-import" => {
                let mut pairs = Vec::new();
                while let Some(n) = args.peek().filter(|n| !n.starts_with("--")) {
                    let (name, path) = n.split_once('=').ok_or_else(|| format!("--memory-import {n:?}: expected EXPORT=PATH"))?;
                    pairs.push((name.to_string(), PathBuf::from(path)));
                    args.next();
                }
                mode = Mode::MemoryImport(pairs);
            }
            "--hook" => mode = Mode::Hook,
            "--project" => project = Some(args.next().ok_or("--project needs a name")?),
            "--phrase" => phrases.push(args.next().ok_or("--phrase needs text")?),
            #[cfg(feature = "dev")]
            "--queries" => bench_opts.queries = Some(PathBuf::from(args.next().ok_or("--queries needs a file")?)),
            #[cfg(feature = "dev")]
            "--before" => {
                let exe = args.next().ok_or("--before needs EXE CONFIG")?;
                let cfg = args.next().ok_or("--before needs EXE CONFIG")?;
                bench_opts.before = Some((PathBuf::from(exe), PathBuf::from(cfg)));
            }
            #[cfg(feature = "dev")]
            "--bench" => {
                let runs = match args.peek() {
                    Some(n) if n.parse::<usize>().is_ok() => args.next().and_then(|n| n.parse().ok()).unwrap_or(200),
                    _ => 200,
                };
                mode = Mode::Bench(runs.max(10), bench::Opts::default());
            }
            "settings" => {
                let state = args.next().ok_or("settings needs on or off, then a setting (update)")?;
                let on = match state.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err(format!("settings {state:?}: expected on or off")),
                };
                let what = args.next().ok_or("settings needs a setting (update)")?;
                if what != "update" {
                    return Err(format!("settings: unknown setting {what:?} (known: update)"));
                }
                mode = Mode::Settings(on, what);
            }
            "--version" | "-V" => {
                println!("atlas {}", env!("CARGO_PKG_VERSION"));
                return Ok(ExitCode::SUCCESS);
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(ExitCode::SUCCESS);
            }
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if let Mode::Recall(p) = &mut mode {
        *p = phrases;
    }
    let project = project.or_else(|| std::env::var(atlas::project::PROJECT_VAR).ok().filter(|p| !p.is_empty()));
    // A command hook's event names the session's folder, which picks the
    // project; it is read before the configuration for that reason.
    let hook_input = if matches!(mode, Mode::Hook) {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut io::stdin(), &mut input).map_err(|e| e.to_string())?;
        Some(atlas::json::parse(&input).map_err(|e| format!("hook input: {e}"))?)
    } else {
        None
    };
    let event_cwd = hook_input.as_ref().and_then(|v| v.get("cwd")).and_then(atlas::json::Value::as_str).filter(|c| !c.is_empty()).map(PathBuf::from);
    let cwd = event_cwd.or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    let mut graph = atlas::project::Graph::default();
    let mut offer_ctx: Option<atlas::claude_settings::OfferCtx> = None;
    let explicit = config.is_some() || std::env::var_os("ATLAS_CONFIG").is_some_and(|p| !p.is_empty());
    let config = config_path(config, !specs.is_empty());
    if let Some(cp) = config.as_ref().filter(|_| !explicit) {
        // The user's own configuration folder: the first run writes the
        // template there. A path given with --config or ATLAS_CONFIG is
        // never created.
        if ensure_config(cp)? {
            eprintln!("atlas: first run: wrote the configuration {} (edit it to add vaults and sources)", cp.display());
        }
    }
    let mut atlas_setup = AtlasSetup::default();
    let mut db_path = atlas::dirs::default_db();
    let mut bytes_per_token = atlas::tokens::DEFAULT_BYTES_PER_TOKEN;
    let mut code_spec: Option<atlas::config::CodeSpec> = None;
    let mut model_path: Option<PathBuf> = None;
    let mut embed_threads = atlas::config::DEFAULT_EMBED_THREADS;
    let mut loaded: Option<Config> = None;
    // The user configuration and the active project, for calibration.
    let mut calib_src: Option<(Config, Option<atlas::project::Active>)> = None;
    // Only the modes that calibrate or report it build a plan: never a hook.
    let calibrates = matches!(mode, Mode::Serve | Mode::Calibrate(_) | Mode::TokenReport);
    if let Some(cp) = &config {
        let raw = Config::load(cp).map_err(|e| e.to_string())?;
        // The active project's own configuration over the user-level one
        // (unchanged for a configuration without projects).
        let r = atlas::project::resolve(&raw, &cwd, project.as_deref()).map_err(|e| e.to_string())?;
        // Once: the per-project offer state and routing switch move to the
        // user level (one stat when done).
        if let Err(e) = atlas::project::migrate_user_state(&raw) {
            eprintln!("atlas: moving the offer state and routing switch to the user level: {e}");
        }
        offer_ctx = Some(atlas::claude_settings::OfferCtx::new(&raw, &r.graph));
        if calibrates {
            calib_src = Some((raw.clone(), r.active.clone()));
        }
        let c = r.config;
        graph = r.graph;
        if let Some(d) = &c.db {
            db_path = d.clone();
        }
        let mut vs: Vec<(String, PathBuf)> = c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
        vs.append(&mut specs);
        specs = vs;
        atlas_setup = AtlasSetup::from_config(&c).map_err(|e| e.to_string())?;
        bytes_per_token = c.bytes_per_token.unwrap_or(bytes_per_token);
        code_spec = c.code.clone();
        model_path = c.semantic.as_ref().and_then(|s| s.model.clone());
        embed_threads = c.semantic.as_ref().and_then(|s| s.threads).unwrap_or(embed_threads).max(1);
        loaded = Some(c);
    }
    if let Some(d) = db {
        db_path = d;
    }
    if config.is_none() {
        // Watching is on unless a configuration turns it off.
        atlas_setup.watch = true;
    }
    let code = code_spec.as_ref().map(|c| atlas::code::CodeSetup::from_spec(c, &db_path, atlas_setup.watch));
    let semantic_model = model_path.unwrap_or_else(|| atlas::models::semantic_model(loaded.as_ref(), &db_path));
    let models = atlas::models::Models::new(loaded.as_ref().and_then(|c| c.models.as_ref()), &db_path);
    let mut memory = atlas::memory::settings::Settings::from_config(loaded.as_ref(), &db_path).map_err(|e| e.to_string())?;
    if !graph.name.is_empty() {
        memory.graph = Some(std::sync::Arc::new(graph));
    }
    let setup = Setup { vaults: specs, db: db_path, atlas: atlas_setup, code, semantic_model, embed_threads, memory };
    let calib_plan = match (&calib_src, loaded.as_ref()) {
        (Some((raw, active)), Some(c)) => Some(atlas::calibrate::Plan::new(raw, c, active.as_ref(), &setup.db)),
        _ => None,
    };
    match mode {
        Mode::Setup => setup_mode(config.as_deref(), &setup, &models, loaded.as_ref()),
        Mode::Settings(on, what) => settings_mode(config.as_deref(), on, &what),
        Mode::Serve => {
            if let Some(code) = update_at_start(loaded.as_ref(), &setup, &models) {
                return Ok(code);
            }
            if models.auto_download && !models.missing().is_empty() {
                // In the background: the first answer is not delayed, and
                // until the files are in place recall is lexical.
                let m = models.clone();
                std::thread::spawn(move || match m.fetch_missing(true) {
                    Ok(done) => {
                        for (id, r) in done {
                            match r {
                                Ok(_) => eprintln!("atlas: model {id} downloaded and verified"),
                                Err(e) => eprintln!("atlas: model {id}: {e}"),
                            }
                        }
                    }
                    Err(e) => eprintln!("atlas: models: {e}"),
                });
            }
            // No scan at startup: every tool brings what it touches up to date
            // (vault tools rescan the vaults, atlas tools check the sources
            // their answer draws on), so the first answer is not delayed.
            let store = match setup.open_typed() {
                Ok(s) => s,
                // A newer atlas's index store: refused, but never as a failed
                // start (Claude Code would then hold the server off for 15
                // minutes and show nothing). The server comes up and answers
                // every call with the refusal; nothing else is started.
                Err(e) if e.is_store_too_new() => {
                    eprintln!("atlas: {e}");
                    atlas::protocol::serve_refused(&e, BufReader::new(io::stdin()), io::stdout()).map_err(|e| e.to_string())?;
                    return Ok(ExitCode::SUCCESS);
                }
                Err(e) => return Err(e.to_string()),
            };
            // The whole-corpus recall test, once per configuration, on its own connection.
            // A second open: a store a newer atlas upgraded since the one
            // above (or any other failure) skips the check, never the start.
            match setup.open() {
                Ok(s) => atlas::grammar::spawn_first_index_check(s, setup.db.clone()),
                Err(e) => eprintln!("atlas: the first index check is skipped: {e}"),
            }
            // The baseline of the measured ratio, when missing or stale: its
            // own thread and connection, never a hook's path.
            match calib_plan {
                Some(Ok(p)) => atlas::calibrate::spawn(p),
                Some(Err(e)) => eprintln!("atlas calibrate: {e}"),
                None => {}
            }
            let roots: Vec<(String, PathBuf)> = store
                .vaults()
                .all()
                .iter()
                .map(|v| (v.name.clone(), v.root.clone()))
                .chain(store.sources().iter().map(|s| (s.name.clone(), s.root.clone())))
                .collect();
            let mut server = Server::new(store);
            // Nothing is loaded until the first semantic or hybrid search.
            server.set_semantic(atlas::tools::SemState::new(setup.semantic_model.clone(), setup.db.clone(), setup.embed_threads));
            if let Some(code) = &setup.code {
                // The indexer runs on its own thread with its own connection;
                // it starts rust-analyzer only when a root is stale.
                let indexer = atlas::code_index::Indexer::start(code.clone(), setup.db.clone(), None);
                server.set_code(atlas::tools::CodeState { setup: code.clone(), indexer: Some(indexer) });
            }
            match atlas::tokens::Ledger::start(&setup.db, roots) {
                Ok(l) => {
                    if let Some(code) = &setup.code {
                        l.set_code_roots(code.roots.iter().map(|r| (r.name.clone(), r.root.clone())).collect());
                    }
                    l.set_memory(atlas::tokens::MemoryPaths { store: setup.memory.store.clone(), export_dir: setup.memory.export_dir.clone() });
                    server.set_ledger(l)
                }
                Err(e) => eprintln!("atlas: token ledger off: {e}"),
            }
            server.set_memory(setup.memory.clone());
            // The hook worker opens its own store connection on its first call,
            // and the index store when an edited note needs re-indexing (R7).
            let (vaults, db, mut atlas_setup, mem) = (setup.vaults.clone(), setup.db.clone(), setup.atlas.clone(), setup.memory.clone());
            // The server watches the files; the worker only rescans on an edit.
            atlas_setup.watch = false;
            let index: atlas::hooks::StoreOpener = Box::new(move || {
                let mut s = Store::open_with(&db, Vaults::new(&vaults)?, atlas_setup.clone())?;
                s.set_memory_sync(Some(mem.clone()));
                Ok(s)
            });
            atlas::protocol::serve_with_hooks(&mut server, Some(setup.memory.clone()), Some(index), BufReader::new(io::stdin()), io::stdout()).map_err(|e| e.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        Mode::Sub(word, rest) => match word.as_str() {
            "run" => atlas::run::cli(rest, &setup.db, loaded.as_ref().and_then(|c| c.lean.as_ref())),
            "slice" => atlas::handles::cli(rest, &setup.db, loaded.as_ref().and_then(|c| c.lean.as_ref())),
            // atlas's own switch; every other form is the settings command.
            "settings" if rest.len() == 2 && matches!(rest[0].as_str(), "on" | "off") && rest[1] == "update" => {
                settings_mode(config.as_deref(), rest[0] == "on", "update")
            }
            "settings" => atlas::claude_settings::cli(rest, config.as_deref(), &setup.db, setup.memory.user_state.as_deref(), project.as_deref()),
            "queue" => {
                let mut store = atlas::memory::MemoryStore::open(&setup.memory.store).map_err(|e| e.to_string())?;
                let orch = atlas::orchestrate::resolve(loaded.as_ref().and_then(|c| c.orchestrate.as_ref()), None, None, |k| std::env::var(k).ok());
                atlas::queue::cli(&rest, &mut store, &orch)
            }
            "ideas" => {
                let mut store = atlas::memory::MemoryStore::open(&setup.memory.store).map_err(|e| e.to_string())?;
                let mut ruling = |a: Vec<String>| {
                    let mut rules = setup.open()?;
                    atlas::records::cli("ruling", a, &mut rules, &setup.memory, config.as_deref())
                };
                atlas::ideas::cli::cli(&rest, &mut store, &setup.memory.store, &mut ruling)
            }
            "rules" => {
                let mut store = atlas::memory::MemoryStore::open(&setup.memory.store).map_err(|e| e.to_string())?;
                atlas::memory::rules::cli(rest, &mut store)
            }
            "ruling" | "entry" => {
                let mut store = setup.open()?;
                atlas::records::cli(&word, rest, &mut store, &setup.memory, config.as_deref())
            }
            _ => {
                let mut store = setup.open()?;
                atlas::grammar::cli(rest, &mut store, config.as_deref(), &setup.db)
            }
        },
        Mode::Hook => {
            // A command hook: the event JSON on stdin, the answer on stdout.
            let v = hook_input.unwrap_or(atlas::json::Value::Null);
            let ev = atlas::hooks::HookEvent::from_value(&v);
            // A store a newer atlas made is told to the user and the model (a
            // failing command hook shows only in verbose mode). The notice
            // belongs to this process, and each `--hook` run is its own: the
            // shipped hooks route only SessionStart here, so a session is
            // told once, but a legacy settings file that routes
            // UserPromptSubmit or PostToolUse here tells it on every event.
            let refuse = |e: atlas::Error| -> Result<ExitCode, String> {
                print!("{}", atlas::hooks::RefusalNotice::default().answer(&e, &ev));
                let _ = std::io::Write::flush(&mut std::io::stdout());
                Ok(ExitCode::SUCCESS)
            };
            // The index store first, read only: the hook's token log and
            // reports write to it, and nothing may once a newer atlas made it.
            if let Some(e) = atlas::store::index_skew(&setup.db).into_iter().next() {
                return refuse(e);
            }
            let mut h = match atlas::hooks::Hooks::open(setup.memory.clone()) {
                Ok(h) => h,
                Err(e) if e.is_store_too_new() => return refuse(e),
                Err(e) => return Err(e.to_string()),
            };
            h.set_command_hook();
            h.set_unattended(atlas::claude_settings::unattended_session(|k| std::env::var(k).ok()));
            // SessionStart's first-run project offer needs the configuration file
            // (to add to) and the folders it already indexes.
            if let Some(c) = loaded.as_ref() {
                h.set_offer_paths(atlas::claude_settings::Paths::detect_in(config.as_deref(), &setup.db, setup.memory.user_state.as_deref()));
                h.set_project_roots(Some(atlas::claude_settings::configured_roots(c)));
                h.set_offer_ctx(offer_ctx.clone());
            }
            h.set_roots(ref_roots(&setup));
            print!("{}", h.handle(&ev).to_json(&ev.event));
            let _ = std::io::Write::flush(&mut std::io::stdout());
            h.file_ideas();
            h.index_transcripts();
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(feature = "dev")]
        Mode::RecallScores(file) => {
            measure::recall_scores(&setup, &file, runs)?;
            Ok(ExitCode::SUCCESS)
        }
        Mode::MemoryCheck => {
            let m = &setup.memory;
            let store = atlas::memory::MemoryStore::open(&m.store).map_err(|e| e.to_string())?;
            let t0 = std::time::Instant::now();
            let r = atlas::memory::check::check(&store, &m.exports, m.export_dir.as_deref(), true).map_err(|e| e.to_string())?;
            println!(
                "memory check: {} records, {} versions, {} copy lines, {} export files compared, {} problems ({:.0} ms)",
                r.records,
                r.versions,
                r.copy_lines,
                r.files,
                r.problems.len(),
                t0.elapsed().as_secs_f64() * 1e3
            );
            for p in &r.problems {
                println!("{p}");
            }
            for w in &r.warnings {
                println!("warning: {w}");
            }
            Ok(if r.ok() { ExitCode::SUCCESS } else { ExitCode::from(1) })
        }
        Mode::MemorySnapshot => {
            let m = &setup.memory;
            let store = atlas::memory::MemoryStore::open(&m.store).map_err(|e| e.to_string())?;
            let Some(s) = atlas::memory::check::snapshot(&store, &m.snapshot_dir, m.snapshot_cap).map_err(|e| e.to_string())? else {
                println!("snapshot skipped: another session kept writing during three tries; run it again");
                return Ok(ExitCode::from(1));
            };
            println!("snapshot {} ({} bytes, {} records verified); {} older folded into the archive", s.path.display(), s.bytes, s.records, s.rolled.len());
            for f in &s.rolled {
                println!("folded {}: {} versions, {} new to the archive", f.snapshot.display(), f.versions, f.added);
            }
            Ok(ExitCode::SUCCESS)
        }
        Mode::MemoryAsOf(key, time) => {
            let m = &setup.memory;
            let store = m.store.exists().then_some(m.store.as_path());
            let sources = atlas::memory::archive::sources(&m.snapshot_dir, store).map_err(|e| e.to_string())?;
            match atlas::memory::archive::as_of(&sources, &key, &time).map_err(|e| e.to_string())? {
                None => {
                    println!("{key}: no version at or before {time} in {} sources", sources.len());
                    Ok(ExitCode::from(1))
                }
                Some(a) => {
                    let sup = a.superseded.as_ref().map_or(String::new(), |(by, t)| format!(", superseded {t} by {by}"));
                    println!("{} v{} written {} ({} {}{sup})\nsha {}\n{}\n{}{}", a.key, a.seq, a.time, a.kind, a.scope, a.sha, a.title, a.meta, a.body);
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
        Mode::MemoryExport(adopt) => {
            let m = &setup.memory;
            let dir = m.export_dir.as_ref().ok_or("memory.export_dir is not configured")?;
            let store = atlas::memory::MemoryStore::open(&m.store).map_err(|e| e.to_string())?;
            let mut refused = 0;
            for spec in &m.exports {
                let t0 = std::time::Instant::now();
                let r = atlas::memory::export::write(&store, spec, dir, adopt).map_err(|e| e.to_string())?;
                println!("{}: {} written, {} unchanged, {} refused ({:.1} ms)", spec.name, r.written.len(), r.unchanged, r.refused.len(), t0.elapsed().as_secs_f64() * 1e3);
                for (p, why) in &r.refused {
                    println!("refused {}: {why}", p.display());
                }
                refused += r.refused.len();
            }
            Ok(if refused == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) })
        }
        Mode::MemoryTree(dir, check_only) => memory_tree(&setup, dir, check_only),
        Mode::MemoryImport(pairs) => {
            let m = &setup.memory;
            let mut store = atlas::memory::MemoryStore::open(&m.store).map_err(|e| e.to_string())?;
            let existing: i64 = store.counts().map_err(|e| e.to_string())?.iter().map(|(_, n)| n).sum();
            if existing > 0 {
                return Err(format!("the store {} already holds {existing} records; import only into an empty store", m.store.display()));
            }
            let t0 = std::time::Instant::now();
            let mut w = store.writer().map_err(|e| e.to_string())?;
            let mut reports = Vec::new();
            for (name, path) in &pairs {
                let spec = m.exports.iter().find(|e| &e.name == name).ok_or_else(|| format!("no export named {name:?} in memory.exports"))?;
                reports.push(atlas::memory::import::import(&mut w, spec, path, "atlas --memory-import").map_err(|e| format!("{name}: {e}"))?);
            }
            w.commit().map_err(|e| e.to_string())?;
            for r in &reports {
                let kinds: Vec<String> = r.kinds.iter().map(|(k, n)| format!("{k} {n}")).collect();
                println!("{}: {} bytes, {} lines: {}", r.export, r.bytes, r.lines, kinds.join(", "));
            }
            println!("imported in {:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
            Ok(ExitCode::SUCCESS)
        }
        Mode::Rescan => {
            let mut store = setup.open()?;
            let v = store.rescan().map_err(|e| e.to_string())?;
            let s = store.rescan_sources(None).map_err(|e| e.to_string())?;
            let m = store.take_memory_sync_reports();
            println!("{}", atlas::json!({"vaults": v, "sources": s, "memory": m}));
            Ok(ExitCode::SUCCESS)
        }
        Mode::Recall(extra) => {
            let mut store = setup.open()?;
            store.rescan().map_err(|e| e.to_string())?;
            store.rescan_sources(None).map_err(|e| e.to_string())?;
            let r = atlas::recall::check(&store, &extra).map_err(|e| e.to_string())?;
            println!(
                "recall: {} documents, {} identifier occurrences ({} distinct), {} phrases ({} document hits)",
                r.docs, r.id_occurrences, r.ids, r.phrases, r.phrase_hits
            );
            for m in &r.id_misses {
                println!("id miss ({}): {} at {}:{}:{}", m.stage, m.id, m.source, m.path, m.line);
            }
            for m in &r.phrase_misses {
                println!("phrase miss: {:?} in {}:{}", m.phrase, m.source, m.path);
            }
            println!("misses: {} identifier, {} phrase", r.id_misses.len(), r.phrase_misses.len());
            let mut complete = r.complete();
            // The whole-corpus check: id-shaped tokens in a defining
            // position that no configured pattern indexes.
            let grammar = atlas::grammar::recall_check(&store).map_err(|e| e.to_string())?;
            for line in &grammar {
                println!("grammar: {line}");
            }
            complete &= grammar.is_empty();
            // The memory store against the files it was imported from: every
            // item or memory file it does not hold as the source has it.
            let roots: Vec<(String, PathBuf)> = store.sources().iter().map(|s| (s.name.clone(), s.root.clone())).collect();
            let targets = atlas::memory::sync::targets(&setup.memory, &roots);
            if !targets.is_empty() {
                let mem = atlas::memory::MemoryStore::open(&setup.memory.store).map_err(|e| e.to_string())?;
                let drift = atlas::memory::sync::drift(&mem, &targets).map_err(|e| e.to_string())?;
                for d in &drift {
                    println!("memory drift: {d}");
                }
                println!("memory drift: {} source items not in the store as the source has them", drift.len());
                complete &= drift.is_empty();
            }
            if let Some(code) = &setup.code {
                let c = atlas::recall::code_check(store.conn(), &code.roots).map_err(|e| e.to_string())?;
                println!(
                    "code recall: {} rust and {} wgsl files, {} definitions by grep ({} more in comments or strings): {} indexed, {} from the line scan only, {} missing",
                    c.files.0,
                    c.files.1,
                    c.truth,
                    c.excluded,
                    c.indexed,
                    c.scan_only,
                    c.misses.iter().filter(|m| m.lang == "rust" || m.lang == "wgsl").count()
                );
                for l in &c.langs {
                    println!(
                        "code recall {}: {} files, {} definitions by grep ({} more in comments or strings): {} indexed, {} from the scanner only, {} missing",
                        l.lang, l.files, l.truth, l.excluded, l.indexed, l.scan_only, l.missing
                    );
                }
                for m in &c.misses {
                    println!("code miss: {} {} at {}:{}:{}", m.keyword, m.name, m.root, m.path, m.line);
                }
                if !c.unindexed.is_empty() {
                    println!("code recall: not indexed yet, not checked: {} (run atlas --code-index)", c.unindexed.join(", "));
                }
                complete &= c.misses.is_empty();
            }
            Ok(if complete { ExitCode::SUCCESS } else { ExitCode::from(1) })
        }
        Mode::HookReport { transcripts, since, rows, tune } => {
            hook_report(&setup, transcripts.as_deref(), &since, rows, tune)?;
            Ok(ExitCode::SUCCESS)
        }
        Mode::CompactReport { transcripts, since, rows, sweep } => {
            let dir = transcripts.ok_or("--compact-report needs --transcripts DIR")?;
            compact_report(&setup, &dir, &since, rows, &sweep, bytes_per_token)?;
            Ok(ExitCode::SUCCESS)
        }
        Mode::Calibrate(audit) => {
            let plan = calib_plan.ok_or("atlas --calibrate needs a configuration")??;
            let _lock = atlas::fsio::FileLock::acquire_within(&plan.lock_target(), std::time::Duration::ZERO)
                .map_err(|_| "another atlas process is calibrating this store; run it again when that has finished".to_string())?;
            let conn = atlas::calibrate::open(&plan.db)?;
            let why = atlas::calibrate::stale(&conn, &plan)?.unwrap_or_else(|| "by hand".into());
            match &plan.projects {
                Some(p) => eprintln!("calibrating ({why}): transcripts in {}, skipping folders that match {}", p.display(), plan.skips.join(", ")),
                None => eprintln!("calibrating ({why}): no home folder, so no Claude Code transcripts to read"),
            }
            let mut file = match &audit {
                Some(p) => Some(std::io::BufWriter::new(std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?)),
                None => None,
            };
            let o = atlas::calibrate::run(&plan, &conn, &mut |m| eprintln!("{m}"), file.as_mut().map(|f| f as &mut dyn std::io::Write))?;
            drop(file);
            println!("{}", atlas::calibrate::summary(&o));
            let side = atlas::baseline::AtlasSide::read(&conn).map_err(|e| e.to_string())?;
            print_measured(o.kept.as_ref().unwrap_or(&o.baseline), None, &side, Some(&plan));
            println!("upper bound per answer (grep + read counterfactual / returned): {:.1}x", side.upper());
            Ok(ExitCode::SUCCESS)
        }
        Mode::TokenReport => {
            let conn = atlas::store::open_raw(&setup.db).map_err(|e| e.to_string())?;
            // A background calibration may be writing; its first read may add columns.
            conn.busy_timeout(std::time::Duration::from_secs(30)).map_err(|e| e.to_string())?;
            let (by_tool, by_day) = atlas::tokens::report(&conn, bytes_per_token).map_err(|e| e.to_string())?;
            let side = atlas::baseline::AtlasSide::read(&conn).map_err(|e| e.to_string())?;
            match atlas::baseline::latest(&conn).map_err(|e| e.to_string())? {
                Some((when, b)) => print_measured(&b, Some(&when), &side, calib_plan.as_ref().and_then(|p| p.as_ref().ok())),
                None => println!("measured ratio per answer: not calibrated yet; the server calibrates in the background at its next start, or run atlas --calibrate"),
            }
            println!("upper bound per answer (grep + read counterfactual / returned): {:.1}x", side.upper());
            println!("tokens at {bytes_per_token} bytes per token; the counterfactual columns are the upper bound (grep output + files read, see atlas::tokens)");
            for (title, rows) in [("tool", by_tool), ("day", by_day)] {
                println!("{title}\tanswers\treturned\tcounterfactual (upper bound)\tupper-bound ratio");
                for r in rows {
                    let ratio = if r.returned_comparable > 0.0 { r.counterfactual / r.returned_comparable } else { 0.0 };
                    println!("{}\t{}\t{:.0}\t{:.0}\t{:.1}x", r.key, r.answers, r.returned, r.counterfactual, ratio);
                }
            }
            // Its own line, never in the ratios above.
            match atlas::tokens::brief_report(&conn, bytes_per_token).map_err(|e| e.to_string())? {
                Some(b) => println!("brief without atlas (rulebook read in full, per subagent start; not in the ratios above)\tstarts {}\ttokens {:.0}\tlatest {:.0}", b.starts, b.tokens, b.latest),
                None => println!("brief without atlas: no subagent starts logged"),
            }
            // What the recall hooks inject, judged against that row: the
            // pointers a session gets in place of a brief that loads it all.
            match atlas::tokens::inject_report(&conn, bytes_per_token).map_err(|e| e.to_string())? {
                Some(i) => println!(
                    "hook injections (recall pointers; judge against the brief row above)\tkeys {}\tsessions {}\ttokens {:.0}\tper session {:.0}\tlargest session {:.0}",
                    i.keys,
                    i.sessions,
                    i.tokens,
                    i.tokens / i.sessions.max(1) as f64,
                    i.max_session
                ),
                None => println!("hook injections: none logged"),
            }
            // What context selection did to those injections: pointers trimmed
            // to their matching clauses, near-duplicates sent as refs, lane
            // briefs with older entries trimmed.
            let sel = atlas::tokens::select_report(&conn, bytes_per_token).map_err(|e| e.to_string())?;
            if sel.is_empty() {
                println!("context selection: none logged");
            } else {
                let mut line = String::from("context selection (hook injections; tokens whole -> sent)");
                let (mut full, mut sent) = (0.0, 0.0);
                for s in &sel {
                    line.push_str(&format!("	{} {} ({:.0} -> {:.0})", s.how, s.n, s.full, s.sent));
                    full += s.full;
                    sent += s.sent;
                }
                line.push_str(&format!("	saved {:.0} of {:.0} ({:.1}%)", full - sent, full, if full > 0.0 { (full - sent) / full * 100.0 } else { 0.0 }));
                println!("{line}");
            }
            // Compactions, as SessionStart(compact) saw them, and the
            // lookups made again after one.
            match atlas::compact::totals(&conn, bytes_per_token).map_err(|e| e.to_string())? {
                Some(c) => {
                    let rate = if c.refs > 0 { c.dangling as f64 / c.refs as f64 } else { 0.0 };
                    println!(
                        "compactions\tn {}\tratio pre/summary {:.1}x\tpre/post {:.1}x\trefs {}\tdangling {} ({:.1}%)\tchanged {}\tbare keys {} (missing {})\tpost-compaction misses {}\tslowest hook {:.0} ms",
                        c.n,
                        if c.summary > 0.0 { c.pre / c.summary } else { 0.0 },
                        if c.post > 0.0 { c.pre_with_post / c.post } else { 0.0 },
                        c.refs,
                        c.dangling,
                        rate * 100.0,
                        c.changed,
                        c.bare,
                        c.bare_missing,
                        c.misses,
                        c.max_ms
                    );
                }
                None => println!("compactions: none logged"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Mode::MissesWeek(since) => {
            let since = if since.is_empty() {
                atlas::store::utc(std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 86_400))
            } else {
                since
            };
            let conn = atlas::store::open_raw(&setup.db).map_err(|e| e.to_string())?;
            let (misses, keys) = atlas::tokens::weekly_misses(&conn, &since, bytes_per_token).map_err(|e| e.to_string())?;
            println!("weekly misses pass since {since}: the open misses seen most, then the injected keys that cost the most");
            for (title, rows) in [("missed (report_miss, open)", misses), ("injected (hook_inject)", keys)] {
                println!("{title}\ttimes\ttokens\tlast\tquery or key");
                if rows.is_empty() {
                    println!("none");
                }
                for r in rows {
                    println!("{}\t{}\t{:.0}\t{}\t{}", r.what, r.count, r.tokens, r.last.get(..16).unwrap_or(&r.last), r.query.replace(['\t', '\n'], " "));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Mode::EmbedCheck(dir) => {
            measure::embed_check(&setup, &dir, &setup.semantic_model)?;
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(feature = "dev")]
        Mode::Judge(file) => {
            measure::judge(&setup, &file, &modes, runs)?;
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(feature = "dev")]
        Mode::SymbolBench(file) => {
            measure::symbol_bench(&setup, &file, runs)?;
            Ok(ExitCode::SUCCESS)
        }
        Mode::Embed(threads) => {
            let mut store = setup.open()?;
            store.rescan().map_err(|e| e.to_string())?;
            store.rescan_sources(None).map_err(|e| e.to_string())?;
            drop(store);
            let t0 = std::time::Instant::now();
            let model = atlas::embed::Model::load(&setup.semantic_model).map_err(|e| e.to_string())?;
            let load_ms = t0.elapsed().as_secs_f64() * 1e3;
            let threads = if threads == 0 { setup.embed_threads } else { threads };
            let mut conn = atlas::store::open_raw(&setup.db).map_err(|e| e.to_string())?;
            conn.busy_timeout(std::time::Duration::from_secs(30)).map_err(|e| e.to_string())?;
            let pending = atlas::semantic::missing(&conn, &model.id).map_err(|e| e.to_string())?;
            let t1 = std::time::Instant::now();
            let mut done = 0;
            loop {
                let n = atlas::semantic::embed_pending(&mut conn, &model, 512, threads).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                done += n;
            }
            let pruned = atlas::semantic::prune(&conn, &model.id).map_err(|e| e.to_string())?;
            println!(
                "model {} ({:?}, {} MB of weights) loaded in {load_ms:.0} ms; {pending} sections pending, {done} embedded on {threads} threads in {:.1} s; {pruned} stale vectors pruned",
                model.id,
                model.isa,
                model.weight_bytes() >> 20,
                t1.elapsed().as_secs_f64()
            );
            Ok(ExitCode::SUCCESS)
        }
        Mode::CodeIndex(names) => {
            let code = setup.code.as_ref().ok_or("no code roots are configured (config key \"code\")")?;
            let rust = match &scip_file {
                Some(f) => atlas::code_index::RustSource::File(f.clone()),
                None => match atlas::code_index::find_rust_analyzer(code.rust_analyzer.as_deref()) {
                    Ok(p) => atlas::code_index::RustSource::Analyzer(p),
                    Err(why) => atlas::code_index::RustSource::ScanOnly(why),
                },
            };
            // Opening the store brings the schema up to date first.
            drop(setup.open()?);
            let mut conn = atlas::store::open_raw(&setup.db).map_err(|e| e.to_string())?;
            conn.busy_timeout(std::time::Duration::from_secs(30)).map_err(|e| e.to_string())?;
            let holder = format!("pid {} (--code-index)", std::process::id());
            if !atlas::code_index::take_lease(&mut conn, &holder, code.timeout * 2).map_err(|e| e.to_string())? {
                return Err("another atlas process is indexing code now".into());
            }
            let mut result = Ok(ExitCode::SUCCESS);
            for r in code.roots.iter().filter(|r| names.is_empty() || names.contains(&r.name)) {
                match atlas::code_index::index_root(code, &mut conn, r, &rust) {
                    Ok(rep) => {
                        println!(
                        "{}: {} | ra {} | scip {} bytes, decode {:.1} ms, {} bytes in memory, {} docs {} occurrences {} symbols | rows: {} | wrote {} in {:.0} ms | total {:.0} ms",
                        rep.root,
                        rep.rust,
                        rep.ra.as_ref().map_or("-".to_string(), |x| format!("{:.1} s exit {}{}", x.seconds, x.code.map_or_else(|| "none".to_string(), |c| c.to_string()), if x.timed_out { " timed out" } else { "" })),
                        rep.scip_bytes,
                        rep.decode_ms,
                        rep.decode_mem,
                        rep.scip_counts.0,
                        rep.scip_counts.1,
                        rep.scip_counts.2,
                        rep.rows,
                        rep.write,
                        rep.write_ms,
                        rep.total_ms
                        );
                        for (unit, x) in &rep.runs {
                            println!("{}: {unit}: indexer {:.1} s exit {}{}", rep.root, x.seconds, x.code.map_or_else(|| "none".to_string(), |c| c.to_string()), if x.timed_out { " timed out" } else { "" });
                        }
                    }
                    Err(e) => {
                        eprintln!("{}: {e}", r.name);
                        result = Ok(ExitCode::FAILURE);
                    }
                }
            }
            let _ = atlas::code_index::release_lease(&conn, &holder);
            result
        }
        #[cfg(feature = "dev")]
        Mode::Bench(runs, _opts) => {
            bench_opts.bytes_per_token = bytes_per_token;
            bench::run(&setup, runs, &bench_opts)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The measured-ratio line of a baseline: the ratio with what it was
/// measured from, or, with too little history, why there is none and when it
/// runs again; then, judged against `plan`, whether it is stale.
fn print_measured(b: &atlas::baseline::Baseline, when: Option<&str>, side: &atlas::baseline::AtlasSide, plan: Option<&atlas::calibrate::Plan>) {
    let when = when.map(|w| format!(", calibrated {}", w.get(..16).unwrap_or(w))).unwrap_or_default();
    let rules = b.rules.map_or_else(|| "unversioned lookup rules".to_string(), |v| format!("lookup rules v{v}"));
    let source = b.cutoff_source.as_ref().map(|s| format!(" ({s})")).unwrap_or_default();
    let from = format!("{} pre-atlas lookup episodes in {} of {} transcripts before {}{source}", b.episodes, b.used, b.transcripts, b.before);
    if b.too_few {
        println!(
            "measured ratio per answer: none, too little pre-atlas history: {from}; needs {} episodes in {} transcripts ({rules}{when}). It runs again when the lookup rules or the configured targets change, or with atlas --calibrate; the upper bound below stands alone",
            atlas::calibrate::MIN_EPISODES,
            atlas::calibrate::MIN_TRANSCRIPTS
        );
    } else {
        let (med, mean) = side.measured(b);
        println!("measured ratio per answer: {med:.1}x (median; mean {mean:.1}x): {from}, {rules}{when}, against {} atlas answers", side.returned.len());
    }
    let stale = match (b.rules, plan) {
        (None, _) => Some("it has no rules version".to_string()),
        (Some(v), _) if v < atlas::calibrate::RULES_VERSION => Some(format!("these lookup rules are v{}", atlas::calibrate::RULES_VERSION)),
        // Newer rules than this binary's: a newer atlas measured it, and it stands.
        (Some(v), _) if v > atlas::calibrate::RULES_VERSION => None,
        (_, Some(p)) if b.fingerprint.as_deref() != Some(p.fingerprint.as_str()) => Some("the configured targets changed".to_string()),
        _ => None,
    };
    if let Some(why) = stale {
        println!("  stale ({why}): the server recalibrates in the background at its next start, or run atlas --calibrate");
    } else if b.rules.is_some_and(|v| v > atlas::calibrate::RULES_VERSION) {
        println!(
            "  measured by a newer atlas (lookup rules v{}, these are v{}): this one does not judge it or recalibrate over it",
            b.rules.unwrap_or_default(),
            atlas::calibrate::RULES_VERSION
        );
    }
}

/// The release check before the server starts: see [`atlas::update`].
/// `Some(code)` when a newer release was swapped in and has served the
/// session (its exit code is this process's); `None` to serve from this
/// binary.
fn update_at_start(c: Option<&Config>, setup: &Setup, models: &atlas::models::Models) -> Option<ExitCode> {
    use atlas::update::{self, Outcome, Updater};
    if !update::release_build() || std::env::var_os(update::UPDATED_ENV).is_some() {
        return None;
    }
    let spec = c.and_then(|c| c.update.as_ref());
    if spec.and_then(|s| s.enabled) == Some(false) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let target = update::target()?;
    let state_dir = atlas::dirs::data_dir().map(|d| d.join("update")).unwrap_or_else(|| setup.db.with_file_name("update"));
    let u = Updater {
        url: spec.and_then(|s| s.url.clone()).unwrap_or_else(|| update::DEFAULT_URL.into()),
        exe: exe.clone(),
        state_dir,
        curl: models.curl.clone(),
        key: update::trusted_key(),
        current: update::version().into(),
        target: target.into(),
        pin: spec.and_then(|s| s.pin.clone()),
        check_every: update::CHECK_EVERY,
        max_time: 60,
    };
    // A refusal or failure is a miss, recorded through its own store
    // connection (the server's is not open yet, or is busy).
    let (vaults, db, atlas_setup) = (setup.vaults.clone(), setup.db.clone(), setup.atlas.clone());
    let url = u.url.clone();
    let record = move |o: &Outcome| {
        let Some(m) = o.miss() else { return };
        eprintln!("atlas: update: {m} (starting the installed version)");
        let logged = Vaults::new(&vaults)
            .map_err(|e| e.to_string())
            .and_then(|v| Store::open_with(&db, v, atlas_setup.clone()).map_err(|e| e.to_string()))
            .and_then(|mut s| s.report_miss("update", &url, m, "", "release check at session start (fail open)", "atlas-update").map_err(|e| e.to_string()));
        if let Err(e) = logged {
            eprintln!("atlas: update: could not record the miss: {e}");
        }
    };
    let late = record.clone();
    match update::run_capped(u, update::DEFAULT_CAP, move |o| late(&o)) {
        Some(Outcome::Swapped { from, to }) => {
            eprintln!("atlas: updated {from} -> {to}");
            match update::reexec(&exe) {
                Ok(st) => return Some(ExitCode::from(st.code().unwrap_or(1).clamp(0, 255) as u8)),
                Err(e) => eprintln!("atlas: update: cannot start {to}: {e}; serving {from} this session"),
            }
        }
        Some(o) => record(&o),
        None => eprintln!("atlas: update: the release check is still running; starting the installed version (a newer one takes effect next start)"),
    }
    None
}

/// `atlas settings on|off update`: sets `update.enabled` in the
/// configuration file (rewritten as formatted JSON).
fn settings_mode(config: Option<&std::path::Path>, on: bool, what: &str) -> Result<ExitCode, String> {
    let cp = config.ok_or("no configuration file to change (pass --config PATH)")?;
    // The whole file is rewritten from this read. No backup here, as before.
    let _held = atlas::fsio::FileLock::acquire(cp)?;
    let text = std::fs::read_to_string(cp).map_err(|e| format!("{}: {e}", cp.display()))?;
    let mut v = atlas::json::parse(&text).map_err(|e| format!("{}: {e}", cp.display()))?;
    let top = v.as_object_mut().ok_or_else(|| format!("{}: not a JSON object", cp.display()))?;
    let entry = top.entry_or_null(what);
    if !entry.is_object() {
        *entry = atlas::json::Value::Object(atlas::json::Map::new());
    }
    entry.as_object_mut().expect("just made an object").insert("enabled".into(), atlas::json::Value::Bool(on));
    let out = atlas::json::to_string_pretty(&v) + "\n";
    // Checked before writing: the result must still load.
    atlas::fsio::replace_atomic(cp, out.as_bytes()).map_err(|e| format!("{}: {e}", cp.display()))?;
    Config::load(cp).map_err(|e| format!("{}: {e}", cp.display()))?;
    println!("{what}: {} ({})", if on { "on" } else { "off" }, cp.display());
    Ok(ExitCode::SUCCESS)
}

/// `--setup`: the configuration, the data folder and the models.
fn setup_mode(config: Option<&std::path::Path>, setup: &Setup, models: &atlas::models::Models, c: Option<&Config>) -> Result<ExitCode, String> {
    match config {
        Some(c) => println!("config   {}", c.display()),
        None => println!("config   (none: --vault flags only)"),
    }
    if let Some(dir) = setup.db.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    println!("database {}", setup.db.display());
    println!("memory   {}", setup.memory.store.display());
    println!("models   {}", models.dir.display());
    let mut failed = 0;
    for f in &models.files {
        println!("model {} ({}, {:.1} MB) from {}", f.id, f.file, f.size as f64 / 1e6, f.url);
        match models.fetch(f, true, false) {
            Ok(atlas::models::Fetched::Present) => println!("  present, sha256 verified"),
            Ok(atlas::models::Fetched::Downloaded { resumed_from, secs }) => {
                let resumed = if resumed_from > 0 { format!(" (resumed at {resumed_from} bytes)") } else { String::new() };
                println!("  downloaded and verified in {secs:.1} s{resumed}");
            }
            Err(e) => {
                println!("  {e}");
                failed += 1;
            }
        }
    }
    if models.files.is_empty() {
        println!("no models are listed (config key models.files): semantic search is off and recall is lexical");
    }
    if setup.semantic_model.is_file() {
        println!("semantic model {}", setup.semantic_model.display());
    } else {
        println!("semantic model {} is absent: recall stays lexical until it is in place", setup.semantic_model.display());
    }
    let up = c.and_then(|c| c.update.as_ref());
    let state = if !atlas::update::release_build() {
        "off (a build from source never replaces itself)".to_string()
    } else if let Some(p) = up.and_then(|u| u.pin.as_deref()) {
        format!("off (pinned to {p}: config update.pin)")
    } else if up.and_then(|u| u.enabled) == Some(false) {
        "off (atlas settings on update turns it on)".to_string()
    } else {
        "on".to_string()
    };
    println!("updates  {state}");
    println!("  At session start atlas checks for a newer release (at most once an hour), verifies its signature against");
    println!("  the key built into atlas, downloads it, checks its SHA-256 and swaps it in. The check waits at most 10 s,");
    println!("  then finishes in the background for the next start; offline or on any error, the installed version starts.");
    println!("  Turn it off with `atlas settings off update`, or pin a version with update.pin in the configuration.");
    // The Claude Code settings atlas offers, each an opt-in (after the model step).
    atlas::claude_settings::offer_interactive(config, &setup.db, setup.memory.user_state.as_deref())?;
    // Whether to add the folder setup runs in as a source (once per folder).
    if let (Some(c), Ok(cwd)) = (c, std::env::current_dir()) {
        atlas::claude_settings::offer_project_interactive(config, &setup.db, setup.memory.user_state.as_deref(), &cwd, &atlas::claude_settings::configured_roots(c))?;
    }
    println!("next: add vaults and sources to the configuration, then restart Claude Code (or run /mcp and reconnect atlas) so the server reloads it");
    Ok(if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn main() -> ExitCode {
    match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("atlas: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::split_subcommand;

    fn split(args: &[&str]) -> (Vec<String>, Option<(String, Vec<String>)>) {
        let mut v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let sub = split_subcommand(&mut v);
        (v, sub)
    }

    #[test]
    fn a_subcommand_takes_the_leading_config_and_project() {
        let (lead, sub) = split(&["--config", "c.json", "settings", "migrate-projects", "alpha"]);
        assert_eq!(lead, vec!["--config", "c.json"]);
        assert_eq!(sub, Some(("settings".into(), vec!["migrate-projects".into(), "alpha".into()])));
        let (lead, sub) = split(&["--project", "p", "--config", "c.json", "run", "--", "x"]);
        assert_eq!(lead, vec!["--project", "p", "--config", "c.json"]);
        assert_eq!(sub.unwrap().0, "run");
        let (lead, sub) = split(&["settings", "list"]);
        assert!(lead.is_empty());
        assert_eq!(sub, Some(("settings".into(), vec!["list".into()])));
        // Not a subcommand: the option loop sees every argument.
        for args in [&["--config", "c.json", "--rescan"][..], &["--config"], &["--rescan", "settings", "off", "update"], &["--config", "settings"]] {
            let (lead, sub) = split(args);
            assert!(sub.is_none(), "{args:?}");
            assert_eq!(lead.len(), args.len());
        }
    }
}
