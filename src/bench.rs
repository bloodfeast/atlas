//! `atlas --bench [RUNS] [--queries FILE] [--before EXE CONFIG]`: the
//! timings and token counts atlas is held to, measured in process on the
//! configured sources.
//!
//! Every timing is a median and p99 over many calls on a warm database (the
//! store already open and indexed), except where it says cold.
//!
//! Tokens are bytes divided by the configured ratio (see [`atlas::tokens`]).
//! For a fixed query set (a file of `lookup ID`, `search TEXT` and
//! `ids PREFIX` lines; without one, a sample of the corpus) the bench prints,
//! per query, the tokens of the plain-text answer, of the same answer as
//! JSON, and the counterfactual (grep plus reading). With `--before`, the
//! same queries also go to an older binary over stdio (with its own
//! configuration, so it keeps its own database), and its answers are counted
//! in the same run; so is its `tools/list`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};


use atlas::json::Value;
use atlas::store::{DefText, Refresh, Store};
use atlas::tools::Server;

use crate::Setup;

/// Options beyond the run count.
#[derive(Debug, Default, Clone)]
pub struct Opts {
    /// The query set file.
    pub queries: Option<PathBuf>,
    /// An older binary and its configuration, to compare against.
    pub before: Option<(PathBuf, PathBuf)>,
    /// Bytes per token.
    pub bytes_per_token: f64,
}

struct Stats {
    n: usize,
    median: Duration,
    p99: Duration,
}

fn stats(mut v: Vec<Duration>) -> Stats {
    v.sort_unstable();
    let n = v.len();
    if n == 0 {
        return Stats { n, median: Duration::ZERO, p99: Duration::ZERO };
    }
    Stats { n, median: v[n / 2], p99: v[((n * 99) / 100).min(n - 1)] }
}

fn show(label: &str, s: &Stats) {
    println!("{label:<52} n={:<6} median {:>10.1} us   p99 {:>10.1} us", s.n, s.median.as_secs_f64() * 1e6, s.p99.as_secs_f64() * 1e6);
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed())
}

/// The lookup with text as atlas-e answered it: the lookup without text,
/// then each of the first `n` definitions' text read back from the sections.
fn text_read_back(store: &Store, id: &str, n: usize) -> atlas::error::Result<usize> {
    let mut l = store.lookup(id, 8, DefText::None)?;
    for d in l.definitions.iter_mut().take(n) {
        d.text = Some(store.definition_text(d)?);
    }
    Ok(l.definitions.len())
}

/// One query of the fixed set: the tool and its arguments.
fn query_args(line: &str) -> Option<(&'static str, Value)> {
    let (tool, rest) = line.trim().split_once(' ')?;
    let rest = rest.trim();
    match tool {
        "lookup" => Some(("lookup", atlas::json!({"id": rest}))),
        "search" => Some(("search", atlas::json!({"query": rest, "mode": "sections"}))),
        "ids" => Some(("ids", atlas::json!({"prefix": rest}))),
        _ => None,
    }
}

fn sample_queries(store: &Store) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok((ids, _)) = store.ids(None, None, None, None, usize::MAX) {
        let mut by_occ: Vec<_> = ids.iter().filter(|i| i.kind != "note").collect();
        by_occ.sort_by_key(|i| std::cmp::Reverse(i.occurrences));
        out.extend(by_occ.iter().take(10).map(|i| format!("lookup {}", i.id)));
        out.extend(ids.iter().filter(|i| i.kind == "note").take(10).map(|i| format!("search {}", i.id)));
        let mut prefixes: Vec<String> = by_occ.iter().map(|i| i.id.chars().take(2).collect()).collect();
        prefixes.dedup();
        out.extend(prefixes.into_iter().take(3).map(|p| format!("ids {p}")));
    }
    out
}

/// A token row's columns, tab-separated, without the before column when
/// there is no binary to compare with.
fn columns(row: &[f64; 4], with_before: bool) -> String {
    let skip = usize::from(!with_before);
    row[skip..].iter().map(|v| format!("{v:.0}")).collect::<Vec<_>>().join("\t")
}

/// Sends requests to an older binary and returns each response's text
/// field bytes, in order, and the bytes of its `tools/list` result line.
fn ask_before(exe: &Path, config: &Path, calls: &[(&str, Value)]) -> Result<(Vec<usize>, usize), String> {
    let mut child = Command::new(exe)
        .arg("--config")
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", exe.display()))?;
    {
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let mut send = |v: Value| writeln!(stdin, "{v}").map_err(|e| e.to_string());
        send(atlas::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "bench"}}}))?;
        send(atlas::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))?;
        for (i, (tool, args)) in calls.iter().enumerate() {
            send(atlas::json!({"jsonrpc": "2.0", "id": i + 2, "method": "tools/call", "params": {"name": tool, "arguments": args}}))?;
        }
    }
    let out = child.stdout.take().ok_or("no stdout")?;
    let mut sizes = vec![0; calls.len()];
    let mut list = 0;
    for line in BufReader::new(out).lines() {
        let line = line.map_err(|e| e.to_string())?;
        let v: Value = atlas::json::parse(&line).map_err(|e| e.to_string())?;
        match v["id"].as_u64() {
            Some(1) => list = line.len(),
            Some(n) if n >= 2 => {
                // What the client reads: the text field (structuredContent
                // repeats it, and is counted too, as the old server sent it).
                let text = v["result"]["content"][0]["text"].as_str().map(str::len).unwrap_or(0);
                let structured = v["result"].get("structuredContent").map(|s| s.to_string().len()).unwrap_or(0);
                sizes[n as usize - 2] = text + structured;
            }
            _ => {}
        }
    }
    let _ = child.wait();
    Ok((sizes, list))
}

/// Runs the benchmark and prints one line per figure.
///
/// # Arguments
///
/// * `setup` - the resolved command line.
/// * `runs` - repetitions of each timed loop.
/// * `opts` - query set, comparison binary and token ratio.
///
/// # Errors
///
/// Any store error, as text.
pub fn run(setup: &Setup, runs: usize, opts: &Opts) -> Result<(), String> {
    let e = |x: atlas::Error| x.to_string();
    let bpt = opts.bytes_per_token;
    println!("atlas bench, {runs} runs per loop, db {}, {bpt} bytes per token", setup.db.display());

    // Cold: a fresh database beside the configured one.
    let cold_db = setup.db.with_file_name("atlas-bench-cold.sqlite");
    let clean = || {
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(cold_db.with_file_name(format!("atlas-bench-cold.sqlite{ext}")));
        }
    };
    clean();
    {
        let mut cold_setup = setup.atlas.clone();
        cold_setup.watch = false;
        let cold = Setup { vaults: setup.vaults.clone(), db: cold_db.clone(), atlas: cold_setup, code: None, semantic_model: setup.semantic_model.clone(), embed_threads: setup.embed_threads, memory: setup.memory.clone() };
        let (store, open) = time(|| cold.open());
        let mut store = store?;
        let (v, tv) = time(|| store.rescan());
        let v = v.map_err(e)?;
        let (s, ts) = time(|| store.rescan_sources(None));
        let s = s.map_err(e)?;
        println!(
            "cold index: open {:.1} ms, vaults {} files in {:.1} ms, sources {} files in {:.1} ms, total {:.1} ms",
            open.as_secs_f64() * 1e3,
            v.files,
            tv.as_secs_f64() * 1e3,
            s.files,
            ts.as_secs_f64() * 1e3,
            (open + tv + ts).as_secs_f64() * 1e3
        );
    }
    clean();

    // Two warm stores on the same database: one walking (the old way and
    // the fallback), one watching.
    let mut walk_setup = setup.atlas.clone();
    walk_setup.watch = false;
    let (store, open) = time(|| Setup { vaults: setup.vaults.clone(), db: setup.db.clone(), atlas: walk_setup.clone(), code: None, semantic_model: setup.semantic_model.clone(), embed_threads: setup.embed_threads, memory: setup.memory.clone() }.open());
    let mut walker = store?;
    println!("open (warm database): {:.2} ms", open.as_secs_f64() * 1e3);
    walker.rescan().map_err(e)?;
    walker.rescan_sources(None).map_err(e)?;
    // A second walking store that rereads the indexed paths on every walk,
    // as every walk did before the path maps were kept.
    let mut rereader = Setup { vaults: setup.vaults.clone(), db: setup.db.clone(), atlas: walk_setup, code: None, semantic_model: setup.semantic_model.clone(), embed_threads: setup.embed_threads, memory: setup.memory.clone() }.open()?;
    rereader.set_path_cache(false);
    let mut watch_setup = setup.atlas.clone();
    watch_setup.watch = true;
    let mut watcher = Setup { vaults: setup.vaults.clone(), db: setup.db.clone(), atlas: watch_setup, code: None, semantic_model: setup.semantic_model.clone(), embed_threads: setup.embed_threads, memory: setup.memory.clone() }.open()?;
    // The first check of each root walks it and sets the baseline.
    watcher.rescan().map_err(e)?;
    watcher.freshness(&[], Refresh::Check).map_err(e)?;

    println!("-- freshness and rescans, nothing changed (walk = no watch; reread = path maps read back every walk)");
    let mut rr = Vec::new();
    let mut rv = Vec::new();
    let mut rw = Vec::new();
    let mut fr = Vec::new();
    let mut fv = Vec::new();
    let mut fw = Vec::new();
    for _ in 0..runs.min(200) {
        rr.push(time(|| rereader.rescan()).1);
        rv.push(time(|| walker.rescan()).1);
        rw.push(time(|| watcher.rescan()).1);
        fr.push(time(|| rereader.freshness_with(&[], Refresh::Check, false)).1);
        fv.push(time(|| walker.freshness_with(&[], Refresh::Check, false)).1);
        fw.push(time(|| watcher.freshness_with(&[], Refresh::Check, false)).1);
    }
    show("rescan vaults, walk, reread paths", &stats(rr));
    show("rescan vaults, walk, kept paths", &stats(rv));
    show("rescan vaults, watch", &stats(rw));
    show("freshness all sources, walk, reread paths", &stats(fr));
    show("freshness all sources, walk, kept paths", &stats(fv));
    show("freshness all sources, watch", &stats(fw));
    let mut rs = Vec::new();
    let mut rsr = Vec::new();
    for _ in 0..runs.min(100) {
        rsr.push(time(|| rereader.rescan_sources(None)).1);
        rs.push(time(|| walker.rescan_sources(None)).1);
    }
    show("rescan sources, walk, reread paths", &stats(rsr));
    show("rescan sources, walk, kept paths", &stats(rs));

    // The fixed query set.
    let queries: Vec<String> = match &opts.queries {
        Some(p) => std::fs::read_to_string(p)
            .map_err(|x| format!("{}: {x}", p.display()))?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect(),
        None => sample_queries(&walker),
    };
    let calls: Vec<(&str, Value)> = queries.iter().filter_map(|q| query_args(q)).collect();

    // Latency of the store calls behind the answers.
    let lookups: Vec<&str> = calls.iter().filter(|(t, _)| *t == "lookup").filter_map(|(_, a)| a["id"].as_str()).collect();
    if !lookups.is_empty() {
        println!("-- lookup and ids (before = the old statements, after = the new)");
        // The pre-sorted table must answer exactly as the grouping statement
        // does, on every identifier in the corpus.
        let all: Vec<String> = {
            let mut st = walker.conn().prepare("SELECT DISTINCT lower(ident) FROM idents").map_err(|x| x.to_string())?;
            let rows = st.query_map([], |r| r.get(0)).map_err(|x| x.to_string())?;
            rows.collect::<Result<_, _>>().map_err(|x| x.to_string())?
        };
        let mut differ = Vec::new();
        for id in &all {
            for limit in [8, 1_000_000] {
                if walker.lookup(id, limit, DefText::All).map_err(e)? != walker.lookup_reference(id, limit, DefText::All).map_err(e)? {
                    differ.push(format!("{id} (limit {limit})"));
                }
            }
        }
        println!("lookup equals the reference on {} of {} ids (limits 8 and all){}", all.len() - differ.len().min(all.len()), all.len(),
            if differ.is_empty() { String::new() } else { format!("; differ: {}", differ.join(", ")) });
        let mut grouped = Vec::new();
        let mut old = Vec::new();
        let mut new = Vec::new();
        let mut text = Vec::new();
        let mut text_old = Vec::new();
        let mut first = Vec::new();
        let mut first_old = Vec::new();
        let mut enc = Vec::new();
        let mut enc_value = Vec::new();
        for k in 0..runs {
            let id = lookups[k % lookups.len()];
            old.push(time(|| legacy_lookup(walker.conn(), id)).1);
            grouped.push(time(|| walker.lookup_reference(id, 8, DefText::None)).1);
            let (l, d) = time(|| walker.lookup(id, 8, DefText::None));
            new.push(d);
            // Before: the lookup, then the text read back per definition
            // (up to three statements each), as atlas-e answered.
            text_old.push(time(|| text_read_back(&walker, id, usize::MAX)).1);
            text.push(time(|| walker.lookup(id, 8, DefText::All)).1);
            first_old.push(time(|| text_read_back(&walker, id, 1)).1);
            first.push(time(|| walker.lookup(id, 8, DefText::First)).1);
            let l = l.map_err(e)?;
            enc_value.push(time(|| atlas::json::ToJson::to_json(&l).to_string().len()).1);
            enc.push(time(|| atlas::json::to_string(&l).len()).1);
        }
        show("lookup, three statements (atlas-a)", &stats(old));
        show("lookup, before (one GROUP BY statement)", &stats(grouped));
        show("lookup, after (pre-sorted table)", &stats(new));
        show("lookup, every definition's text, before (read back)", &stats(text_old));
        show("lookup, every definition's text, after (stored)", &stats(text));
        show("lookup, first definition's text, before (read back)", &stats(first_old));
        show("lookup, first definition's text, after (stored)", &stats(first));
        show("lookup JSON encode, before (through a Value)", &stats(enc_value));
        show("lookup JSON encode, after (written directly)", &stats(enc));

        // The whole JSON answer of `lookup` (every definition's text), as
        // atlas-e wrote it (a `Value` built from the hits, then encoded once
        // for the token ledger and once for the client) and as it is written
        // now (straight from the hits, once). The two texts must be equal.
        let mut twice = Vec::new();
        let mut once = Vec::new();
        let mut same = 0usize;
        for k in 0..runs {
            let id = lookups[k % lookups.len()];
            let l = walker.lookup(id, 8, DefText::All).map_err(e)?;
            let (a, d) = time(|| {
                let v = atlas::json!({
                    "found": !l.definitions.is_empty() || l.total_mentions > 0,
                    "definitions": l.definitions,
                    "mentions": l.mentions,
                    "total_mentions": l.total_mentions,
                    "truncated": l.mention_lines > l.mentions.len(),
                });
                let ledger = v.to_string().len();
                (ledger, v.to_string())
            });
            twice.push(d);
            let (b, d) = time(|| {
                atlas::json::object(256, |w| {
                    w.field("found", &(!l.definitions.is_empty() || l.total_mentions > 0))
                        .field("definitions", &l.definitions)
                        .field("mentions", &l.mentions)
                        .field("total_mentions", &l.total_mentions)
                        .field("truncated", &(l.mention_lines > l.mentions.len()));
                })
            });
            once.push(d);
            same += usize::from(a.1 == b && a.0 == b.len());
        }
        println!("lookup JSON answer: written once equals built and encoded twice on {same} of {runs}");
        show("lookup JSON answer, before (Value, encoded twice)", &stats(twice));
        show("lookup JSON answer, after (written once)", &stats(once));
    }
    let prefixes: Vec<&str> = calls.iter().filter(|(t, _)| *t == "ids").filter_map(|(_, a)| a["prefix"].as_str()).collect();
    if !prefixes.is_empty() {
        let mut old = Vec::new();
        let mut new = Vec::new();
        for k in 0..runs {
            let p = prefixes[k % prefixes.len()];
            old.push(time(|| legacy_ids(walker.conn(), p)).1);
            new.push(time(|| walker.ids(Some(p), None, None, None, 50)).1);
        }
        show("ids prefix, before (GROUP BY)", &stats(old));
        show("ids prefix, after", &stats(new));
    }
    let mut ts = Vec::new();
    let searches: Vec<String> =
        calls.iter().filter(|(t, _)| *t == "search").filter_map(|(_, a)| a["query"].as_str()).map(atlas::tools::plain_query).collect();
    for k in 0..runs.min(400) {
        if searches.is_empty() {
            break;
        }
        let q = &searches[k % searches.len()];
        ts.push(time(|| walker.search_sections(q, None, None, 11)).1);
    }
    if !ts.is_empty() {
        show("section search (store call, top 11)", &stats(ts));
    }

    // Tokens per answer.
    let mut server = Server::new(watcher);
    let mut whole = Vec::new();
    for _ in 0..runs.min(50) {
        for (tool, args) in &calls {
            whole.push(time(|| server.answer(tool, args)).1);
        }
    }
    if !whole.is_empty() {
        show("whole answer (watch, text), all queries", &stats(whole));
    }
    let mut whole_json = Vec::new();
    for _ in 0..runs.min(50) {
        for (tool, args) in &calls {
            let mut a = args.clone();
            a["format"] = atlas::json!("json");
            whole_json.push(time(|| server.answer(tool, &a).map(|x| x.into_text().len())).1);
        }
    }
    if !whole_json.is_empty() {
        show("whole answer (watch, json), all queries", &stats(whole_json));
    }
    let before = match &opts.before {
        Some((exe, cfg)) => Some(ask_before(exe, cfg, &calls)?),
        None => None,
    };
    println!("-- tokens per answer ({} queries)", calls.len());
    // The before column only when there is a binary to ask.
    let cols = if before.is_some() { "query\tbefore\tjson\ttext\tgrep+read" } else { "query\tjson\ttext\tgrep+read" };
    println!("{cols}");
    let mut sum = [0.0f64; 4];
    let docs = server.store_mut().docs().map_err(e)?;
    for (i, (tool, args)) in calls.iter().enumerate() {
        let text = server.answer(tool, args).map_err(e)?.into_text().len();
        let mut jargs = args.clone();
        jargs["format"] = atlas::json!("json");
        let jsn = server.answer(tool, &jargs).map_err(e)?.into_text().len();
        let lit = args["id"].as_str().or(args["query"].as_str()).or(args["prefix"].as_str()).unwrap_or("");
        let mut cf = 0usize;
        for (source, path) in &docs {
            if let Ok(bytes) = server.store_mut().read_document(source, path) {
                let c = atlas::tokens::measure_file(&format!("{source}/{path}"), &bytes, lit.as_bytes());
                cf += c.grep + c.read;
            }
        }
        let b = before.as_ref().map(|(s, _)| s[i] as f64).unwrap_or(0.0);
        let row = [b / bpt, jsn as f64 / bpt, text as f64 / bpt, cf as f64 / bpt];
        for (s, r) in sum.iter_mut().zip(row) {
            *s += r;
        }
        println!("{tool} {lit}\t{}", columns(&row, before.is_some()));
    }
    println!("TOTAL\t{}", columns(&sum, before.is_some()));
    let list = atlas::json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": atlas::tools::tool_list()}}).to_string().len();
    match &before {
        Some((_, b)) => println!("tools/list: before {b} bytes ({:.0} tokens), after {list} bytes ({:.0} tokens)", *b as f64 / bpt, list as f64 / bpt),
        None => println!("tools/list: {list} bytes ({:.0} tokens)", list as f64 / bpt),
    }
    queue_rows(setup, runs)
}

/// The work queue's rows: `atlas queue next`'s work (read the items, check
/// their `after` conditions, propose) on a store seeded with 100 and with
/// 1,000 queued items (refused briefs of 2 KB, and of 20 KB as the hook
/// files them), and brief classification on a 20 KB brief with nothing
/// declared, against no lanes and against the configured memory store's
/// lanes. The queue store is a scratch file beside the database.
fn queue_rows(setup: &Setup, runs: usize) -> Result<(), String> {
    let e = |x: atlas::Error| x.to_string();
    let orch = atlas::orchestrate::OrchSettings::default();
    for (n, repeat) in [(100usize, 60usize), (1000, 60), (1000, 600)] {
        let path = setup.db.with_file_name("atlas-bench-queue.sqlite");
        let clean = || {
            for ext in ["", "-wal", "-shm", ".jsonl"] {
                let _ = std::fs::remove_file(path.with_file_name(format!("atlas-bench-queue.sqlite{ext}")));
            }
        };
        clean();
        let brief = "Kind: builder\nLane: L\nUses: compile\n\n".to_string() + &"Fix the reader and run the tests. ".repeat(repeat);
        {
            let mut m = atlas::memory::MemoryStore::open(&path).map_err(e)?;
            // Four in five are planned, in chains of ten on a lane (each
            // after the one before); the rest are refused launches with a
            // brief and the meta the hook writes.
            for i in 0..n {
                let lane = format!("BENCH-{}", i / 10);
                if i % 5 == 4 {
                    let class = atlas::lanes::classify(&brief, &[]);
                    let meta = class.meta();
                    let title = format!("refused {i}");
                    let l = atlas::queue::Launch { kind: atlas::orchestrate::Kind::Builder, lane: Some(&lane), description: &title, subagent_type: "general-purpose", prompt: &brief, session: "s", class_meta: &meta };
                    atlas::queue::add(&mut m, &l).map_err(e)?;
                } else {
                    let after = if i % 10 == 0 { Vec::new() } else { vec![format!("journal:{lane}:{:04}", i % 10)] };
                    let p = atlas::queue::Planned { lane, kind: "builder".into(), uses: vec!["compile".into()], after, order: i.to_string().parse().ok(), title: format!("planned {i}"), ..atlas::queue::Planned::default() };
                    atlas::queue::add_planned(&mut m, &p).map_err(e)?;
                }
            }
        }
        let m = atlas::memory::MemoryStore::open(&path).map_err(e)?;
        let now = atlas::memory::now();
        let mut t = Vec::with_capacity(runs);
        for _ in 0..runs {
            let (r, d) = time(|| atlas::queue::items(&m, &now).map(|(_, its)| atlas::queue::propose(&orch, &its, &[]).start.len()));
            r.map_err(e)?;
            t.push(d);
        }
        show(&format!("queue next (items, after, propose), {n} items, {} KB briefs", (brief.len() + 512) / 1024), &stats(t));
        drop(m);
        clean();
    }
    let brief = "Fix the reader, then update the notes and check the output by hand. ".repeat(300);
    // The lanes the hook passes: the configured store's, when it exists.
    let known = if setup.memory.store.exists() {
        atlas::memory::MemoryStore::open(&setup.memory.store).map(|m| atlas::queue::known_lanes(&m)).unwrap_or_default()
    } else {
        Vec::new()
    };
    for lanes in [&[][..], &known[..]] {
        let mut t = Vec::with_capacity(runs);
        for _ in 0..runs {
            let (_, d) = time(|| atlas::lanes::classify(&brief, lanes));
            t.push(d);
        }
        show(&format!("classify a {} KB brief, nothing declared, {} known lanes", brief.len() / 1024, lanes.len()), &stats(t));
    }
    Ok(())
}

/// The lookup as the first atlas lane wrote it: three statements
/// (definitions, mentions, mention count), kept here only to measure against.
fn legacy_lookup(conn: &rusqlite::Connection, id: &str) -> usize {
    const SELECT: &str = "SELECT d.source, d.kind, d.path, i.line, i.ident, i.kind, i.def, i.date, ifnull(s.heading_path, ''),
         ifnull(s.line_start, 0), ifnull(s.line_end, 0), i.context, i.section_id
         FROM idents i JOIN docs d ON d.id = i.doc_id LEFT JOIN sections s ON s.id = i.section_id";
    let mut n = 0;
    let mut run = |sql: &str, limit: i64| {
        if let Ok(mut st) = conn.prepare_cached(sql) {
            if let Ok(mut rows) = st.query(rusqlite::params![id, limit]) {
                while let Ok(Some(r)) = rows.next() {
                    let _: rusqlite::Result<String> = r.get(11);
                    n += 1;
                }
            }
        }
    };
    run(&format!("{SELECT} WHERE i.ident = ?1 AND i.rank > 0 AND ?2 >= 0 ORDER BY i.rank DESC, (i.date IS NULL), i.date DESC, d.mtime_ns DESC, d.source, d.path, i.line LIMIT 50"), 0);
    run(&format!("{SELECT} WHERE i.ident = ?1 AND i.rank = 0 ORDER BY (i.date IS NULL), i.date DESC, d.mtime_ns DESC, d.source, d.path, i.line LIMIT ?2"), 50);
    run("SELECT COUNT(*) FROM idents WHERE ident = ?1 AND rank = 0 AND ?2 >= 0", 0);
    n
}

/// The ids listing as it was before: a GROUP BY over every occurrence.
fn legacy_ids(conn: &rusqlite::Connection, prefix: &str) -> usize {
    let like = format!("{}%", prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let Ok(mut st) = conn.prepare_cached(
        "SELECT MIN(i.ident), i.kind, SUM(i.rank > 0), COUNT(*), MAX(i.date) FROM idents i JOIN docs d ON d.id = i.doc_id
         WHERE (?1 IS NULL OR i.ident LIKE ?1 ESCAPE '\\') GROUP BY i.ident, i.kind",
    ) else {
        return 0;
    };
    let mut n = 0;
    if let Ok(mut rows) = st.query([like]) {
        while let Ok(Some(_)) = rows.next() {
            n += 1;
        }
    }
    n
}
