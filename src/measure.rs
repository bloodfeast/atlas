//! Measurements for semantic search and the code index, run from the
//! command line (`--embed-check`, and with the `dev` feature `--judge`,
//! `--symbol-bench` and `--recall-scores`). They read reference files and
//! query sets kept outside the repository.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;
#[cfg(feature = "dev")]
use std::time::Duration;

use atlas::embed::bert::Scratch;
use atlas::embed::Model;
use atlas::json::{self, Value};
#[cfg(feature = "dev")]
use atlas::tools::{Answer, Server};

use crate::Setup;

#[cfg(feature = "dev")]
fn pct(mut v: Vec<Duration>) -> (Duration, Duration) {
    v.sort_unstable();
    if v.is_empty() {
        return (Duration::ZERO, Duration::ZERO);
    }
    (v[v.len() / 2], v[((v.len() * 99) / 100).min(v.len() - 1)])
}

#[cfg(feature = "dev")]
fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// Compares the owned tokenizer and encoder with reference files written
/// by a reference implementation: `tokens.tsv` (`sha \t JSON text \t ids`,
/// every section, no special tokens, no cut) and `vectors.tsv` (`key \t
/// JSON text \t comma floats`).
pub(crate) fn embed_check(setup: &Setup, dir: &Path, model_path: &Path) -> Result<(), String> {
    let model = Model::load(model_path).map_err(|e| e.to_string())?;
    let conn = rusqlite::Connection::open(&setup.db).map_err(|e| e.to_string())?;
    let mut texts: HashMap<String, String> = HashMap::new();
    {
        let mut st = conn
            .prepare("SELECT s.sha, MIN(s.heading_path), MIN(f.body) FROM sections s JOIN sections_fts f ON f.rowid = s.id GROUP BY s.sha")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, atlas::semantic::section_text(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?))))
            .map_err(|e| e.to_string())?;
        for r in rows {
            let (k, v) = r.map_err(|e| e.to_string())?;
            texts.insert(k, v);
        }
    }
    let tokens = std::fs::read_to_string(dir.join("tokens.tsv")).map_err(|e| e.to_string())?;
    let (mut n, mut same, mut ids_total, mut shown, mut in_db) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let t0 = Instant::now();
    for line in tokens.lines() {
        let mut parts = line.splitn(3, '\t');
        let (sha, text, ids) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let Ok(Value::String(text)) = json::parse(text) else { continue };
        in_db += usize::from(texts.contains_key(sha));
        n += 1;
        let want: Vec<u32> = ids.split(' ').filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        let got = model.tok.encode_all(&text);
        ids_total += want.len();
        if got == want {
            same += 1;
        } else if shown < 5 {
            shown += 1;
            let at = got.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(got.len().min(want.len()));
            println!("token mismatch {sha:.12} at {at}: got {:?} want {:?}", &got[at..(at + 6).min(got.len())], &want[at..(at + 6).min(want.len())]);
        }
    }
    println!(
        "tokenizer: {same}/{n} sections identical ({ids_total} reference tokens; {in_db} of them by hash in this database of {}), tokenised in {:.0} ms",
        texts.len(),
        t0.elapsed().as_secs_f64() * 1e3
    );
    let vectors = std::fs::read_to_string(dir.join("vectors.tsv")).map_err(|e| e.to_string())?;
    let mut s = Scratch::default();
    let mut cos: Vec<f64> = Vec::new();
    let mut cards: Vec<Vec<f32>> = Vec::new();
    for line in vectors.lines() {
        let mut parts = line.splitn(3, '\t');
        let (key, text, floats) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let text = match json::parse(text) {
            Ok(Value::String(t)) => t,
            _ => continue,
        };
        let want: Vec<f64> = floats.split(',').filter_map(|x| x.parse().ok()).collect();
        let got = model.embed(&text, &mut s);
        let dot: f64 = got.iter().zip(&want).map(|(a, b)| f64::from(*a) * b).sum();
        let ng: f64 = got.iter().map(|a| f64::from(*a) * f64::from(*a)).sum::<f64>().sqrt();
        let nw: f64 = want.iter().map(|b| b * b).sum::<f64>().sqrt();
        cos.push(dot / (ng * nw));
        if key.starts_with("card") {
            cards.push(got);
        }
    }
    cos.sort_by(f64::total_cmp);
    if !cos.is_empty() {
        let mean = cos.iter().sum::<f64>() / cos.len() as f64;
        println!(
            "encoder {}: {} vectors, cosine to reference min {:.6} p1 {:.6} median {:.6} mean {:.6}",
            model.id,
            cos.len(),
            cos[0],
            cos[cos.len() / 100],
            cos[cos.len() / 2],
            mean
        );
    }
    if cards.len() == 3 {
        let d = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        println!("model card sentences: {:.4} {:.4} {:.4}", d(&cards[0], &cards[1]), d(&cards[0], &cards[2]), d(&cards[1], &cards[2]));
    }
    Ok(())
}

/// One judged query: the text and the locations that answer it
/// (`source:path` or `source:path:line`, any one counts).
#[cfg(feature = "dev")]
struct Judged {
    query: String,
    want: Vec<(String, String, Option<i64>)>,
}

#[cfg(feature = "dev")]
fn read_judged(path: &Path) -> Result<Vec<Judged>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (q, w) = line.split_once('\t').ok_or_else(|| format!("judged line without a tab: {line}"))?;
        let want = w
            .split('|')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|s| {
                let mut p = s.splitn(3, ':');
                let src = p.next()?.to_string();
                let path = p.next()?.to_string();
                let line = p.next().and_then(|l| l.parse().ok());
                Some((src, path, line))
            })
            .collect();
        out.push(Judged { query: q.to_string(), want });
    }
    Ok(out)
}

/// Runs a judged query set through `search` in each mode: hit@5, mean
/// reciprocal rank in the top 10, answer tokens (bytes / 4 of the default
/// text answer) and latency.
#[cfg(feature = "dev")]
pub(crate) fn judge(setup: &Setup, file: &Path, modes: &[String], runs: usize) -> Result<(), String> {
    let judged = read_judged(file)?;
    let store = setup.open()?;
    let mut server = Server::new(store);
    server.set_semantic(atlas::tools::SemState::new(setup.semantic_model.clone(), setup.db.clone(), setup.embed_threads));
    // Warm: load the model and vectors, bring sources up to date.
    for m in modes {
        let _ = server.answer("search", &atlas::json!({"query": "warm up", "mode": m}));
    }
    println!("mode\thit@1\thit@5\tmrr@10\ttokens/answer\tmedian_us\tp99_us");
    for m in modes {
        let (mut h1, mut h5, mut rr, mut bytes) = (0usize, 0usize, 0f64, 0usize);
        let mut times = Vec::new();
        let mut misses = Vec::new();
        for j in &judged {
            let a = server.answer("search", &atlas::json!({"query": j.query, "mode": m})).map_err(|e| e.to_string())?;
            bytes += a.into_text().len();
            for _ in 0..runs {
                let t = Instant::now();
                let _ = server.answer("search", &atlas::json!({"query": j.query, "mode": m}));
                times.push(t.elapsed());
            }
            let v = server.call("search", &atlas::json!({"query": j.query, "mode": m, "format": "json", "limit": 10})).map_err(|e| e.to_string())?;
            let hits = v.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
            let rank = hits.iter().position(|h| {
                let src = h.get("source").and_then(Value::as_str).unwrap_or("");
                let path = h.get("path").and_then(Value::as_str).unwrap_or("");
                let (a, b) = (h.get("line_start").and_then(Value::as_i64).unwrap_or(0), h.get("line_end").and_then(Value::as_i64).unwrap_or(0));
                j.want.iter().any(|(s, p, l)| s == src && p == path && l.is_none_or(|l| a <= l && l <= b))
            });
            match rank {
                Some(r) => {
                    h1 += usize::from(r == 0);
                    h5 += usize::from(r < 5);
                    rr += 1.0 / (r + 1) as f64;
                }
                None => {}
            }
            if rank.is_none_or(|r| r >= 5) {
                misses.push(j.query.clone());
            }
        }
        let (med, p99) = pct(times);
        let n = judged.len().max(1);
        println!("{m}\t{h1}/{n}\t{h5}/{n}\t{:.3}\t{:.0}\t{:.0}\t{:.0}", rr / n as f64, bytes as f64 / 4.0 / n as f64, us(med), us(p99));
        for q in misses {
            println!("  {m} miss@5: {q}");
        }
    }
    Ok(())
}

/// Times `symbol` over a list of names (one per line): the store query
/// alone and the whole text answer, plus answer tokens and the grep
/// counterfactual of the token ledger for the same names.
#[cfg(feature = "dev")]
pub(crate) fn symbol_bench(setup: &Setup, file: &Path, runs: usize) -> Result<(), String> {
    let code = setup.code.clone().ok_or("no code roots configured")?;
    let names: Vec<String> = std::fs::read_to_string(file).map_err(|e| e.to_string())?.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    let store = setup.open()?;
    let mut server = Server::new(store);
    let roots: Vec<(String, std::path::PathBuf)> = code.roots.iter().map(|r| (r.name.clone(), r.root.clone())).collect();
    server.set_code(atlas::tools::CodeState { setup: code, indexer: None });
    let conn = rusqlite::Connection::open(&setup.db).map_err(|e| e.to_string())?;
    let lim = atlas::code::Limits { defs: 10, refs: 10, impls: 10 };
    let (mut q, mut whole) = (Vec::new(), Vec::new());
    let (mut bytes, mut found) = (0usize, 0usize);
    for n in &names {
        let a = atlas::code::symbol(&conn, n, lim).map_err(|e| e.to_string())?;
        found += usize::from(a.totals.iter().any(|&x| x > 0));
        for _ in 0..runs {
            let t = Instant::now();
            let _ = atlas::code::symbol(&conn, n, lim);
            q.push(t.elapsed());
            let t = Instant::now();
            let ans = server.answer("symbol", &atlas::json!({"name": n})).map_err(|e| e.to_string())?;
            whole.push(t.elapsed());
            if let Answer::Text(t) = &ans {
                let _ = t.len();
            }
        }
        bytes += server.answer("symbol", &atlas::json!({"name": n})).map_err(|e| e.to_string())?.into_text().len();
    }
    let (qm, qp) = pct(q);
    let (wm, wp) = pct(whole);
    println!("symbol: {} names ({} found), {runs} runs each", names.len(), found);
    println!("store query: median {:.1} us p99 {:.1} us; whole text answer: median {:.1} us p99 {:.1} us", us(qm), us(qp), us(wm), us(wp));
    let counter: usize = names.iter().map(|n| { let c = atlas::tokens::measure_code(&roots, n); c.grep + c.read }).sum();
    println!(
        "answer tokens: {:.0} total, {:.0} per answer; grep + read counterfactual {:.0} tokens ({:.1}x)",
        bytes as f64 / 4.0,
        bytes as f64 / 4.0 / names.len().max(1) as f64,
        counter as f64 / 4.0,
        counter as f64 / bytes.max(1) as f64
    );
    Ok(())
}

/// `--recall-scores FILE [--runs N]`: for each `label<TAB>prompt` line, a
/// summary row (the best lexical candidate's FTS5 score and matched-term
/// count, the best cosine, what the gate injects) and one `cand` row per
/// candidate with its scores, the inputs `cargo xtask calibrate-recall`
/// derives the floors from; then the warm time of the whole gated recall
/// (query embedding included) and of the `recall` tool's lane answer.
///
/// With the configured embedding model present, records without a vector
/// are embedded first and the gate is the hybrid one; without it, lexical.
#[cfg(feature = "dev")]
pub(crate) fn recall_scores(setup: &Setup, file: &Path, runs: usize) -> Result<(), String> {
    use atlas::memory::{recall, vectors};
    let m = &setup.memory;
    let store = atlas::memory::MemoryStore::open(&m.store).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let rows: Vec<(&str, &str)> = text.lines().filter_map(|l| l.split_once('\t')).collect();
    let budget = m.bytes_for(m.recall.budget_tokens);
    let skip = std::collections::HashSet::new();
    let model = if m.model.is_file() { Some(Model::load(&m.model).map_err(|e| e.to_string())?) } else { None };
    let vecs = match &model {
        Some(md) => {
            let t = Instant::now();
            let mut n = 0;
            loop {
                let k = vectors::embed_pending(&store, md, 256, m.embed_threads).map_err(|e| e.to_string())?;
                if k == 0 {
                    break;
                }
                n += k;
            }
            let v = vectors::RecVecs::load(&store, &md.id, md.dim).map_err(|e| e.to_string())?;
            println!("gate hybrid: model {} records with vectors {} (embedded now {n} in {:.1} s) missing {}", md.id, v.rids.len(), t.elapsed().as_secs_f64(), v.missing);
            Some(v)
        }
        None => {
            println!("gate lexical: no model at {}", m.model.display());
            None
        }
    };
    let gate = m.recall.gate(vecs.is_some());
    println!("gate fts_floor {} min_terms {} cos_floor {} cos_alone {} query_tokens {}", gate.fts_floor, gate.min_terms, gate.cos_floor, gate.cos_alone, m.recall.query_tokens);
    println!("label\tbest_score\tbest_matched\tterms\tbest_key\tinjected\tbytes\tbest_cos");
    let mut scratch = Scratch::default();
    let (mut times, mut embeds) = (Vec::new(), Vec::new());
    let mut cands_out = String::new();
    for (i, (label, prompt)) in rows.iter().enumerate() {
        let terms = recall::terms(prompt);
        let qv = model.as_ref().map(|md| md.embed_query_cut(prompt, m.recall.query_tokens, &mut scratch));
        let sem = match (&model, &vecs, &qv) {
            (Some(md), Some(v), Some(q)) => Some(recall::Semantic { vecs: v, query: q, isa: md.isa }),
            _ => None,
        };
        let cands = recall::candidates(&store, prompt, &gate, sem.as_ref()).map_err(|e| e.to_string())?;
        let c = recall::gated(&store, prompt, budget, &gate, sem.as_ref(), &skip).map_err(|e| e.to_string())?;
        for _ in 0..runs {
            let t = Instant::now();
            let q2 = model.as_ref().map(|md| md.embed_query_cut(prompt, m.recall.query_tokens, &mut scratch));
            embeds.push(t.elapsed());
            let sem2 = match (&model, &vecs, &q2) {
                (Some(md), Some(v), Some(q)) => Some(recall::Semantic { vecs: v, query: q, isa: md.isa }),
                _ => None,
            };
            let _ = recall::gated(&store, prompt, budget, &gate, sem2.as_ref(), &skip);
            times.push(t.elapsed());
        }
        for cd in &cands {
            let cos = cd.cos.map_or("-".to_string(), |x| format!("{x:.4}"));
            cands_out.push_str(&format!("cand\t{i}\t{label}\t{:.2}\t{}\t{}\t{cos}\t{}\n", cd.fts, cd.matched, cd.need, cd.head.key));
        }
        let best = cands.iter().filter(|c| c.fts > 0.0).max_by(|a, b| a.fts.total_cmp(&b.fts));
        let best_cos = cands.iter().filter_map(|c| c.cos).fold(f32::NAN, f32::max);
        match best {
            Some(b) => println!("{label}\t{:.2}\t{}\t{}\t{}\t{}\t{}\t{best_cos:.4}", b.fts, b.matched, terms.len(), b.head.key, c.heads.len(), c.text.len()),
            None => println!("{label}\t0\t0\t{}\t-\t{}\t{}\t{best_cos:.4}", terms.len(), c.heads.len(), c.text.len()),
        }
    }
    print!("{cands_out}");
    let (med, p99) = pct(times);
    let (emed, ep99) = pct(embeds);
    println!("gated recall warm: {} prompts x {runs} runs: median {:.1} us p99 {:.1} us (query embedding alone median {:.1} us p99 {:.1} us)", rows.len(), us(med), us(p99), us(emed), us(ep99));
    // recall by key: the store read alone, then the whole tool answer.
    let keys: Vec<String> = store.heads(Some("memory"), None).map_err(|e| e.to_string())?.into_iter().map(|h| h.key).take(50).collect();
    let mut server = Server::new(setup.open()?);
    server.set_memory(m.clone());
    let (mut q, mut whole) = (Vec::new(), Vec::new());
    for k in &keys {
        for _ in 0..runs {
            let t = Instant::now();
            let _ = store.head(k);
            q.push(t.elapsed());
            let t = Instant::now();
            let _ = server.answer("recall", &atlas::json!({"key": k}));
            whole.push(t.elapsed());
        }
    }
    let ((qm, qp), (wm, wp)) = (pct(q), pct(whole));
    println!("recall by key warm: {} keys x {runs} runs: store read median {:.1} us p99 {:.1} us; tool answer median {:.1} us p99 {:.1} us", keys.len(), us(qm), us(qp), us(wm), us(wp));
    Ok(())
}
