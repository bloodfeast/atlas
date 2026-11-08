//! IDEA-CAPTURE part 0: fit and score the detector. Everything it reads is a path, so
//! a later label file or corpus reruns it unchanged, and every output records the
//! sha256 of what it used. One `rustc -O`.
//!
//!   eval fit --corpus TUNE.jsonl --labels L.tsv --out WEIGHTS.txt
//!   eval score --corpus C.jsonl --labels L.tsv --weights WEIGHTS.txt --out REPORT.md
//!        [--per-day typed=46.6,queued=5.1,ask=3.8] [--errors FILE --at T]
//!   eval score ... --heldout
//!
//! `--heldout` scores a corpus once. It records the corpus's sha in `report/spent/`
//! beside the corpus before reading a row, and refuses that sha afterwards with any
//! label file; a retest needs prompts newer than the freeze. It never writes text,
//! only ids and metrics. Without it, a corpus named heldout or a spent sha is refused.

#[path = "../../src/ideas/detect.rs"]
#[allow(dead_code)]
mod detect;
#[path = "../../src/sha256.rs"]
#[allow(dead_code)]
mod sha256;

use detect::{score_with, N};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

const DETECTOR: &str = include_str!("../../src/ideas/detect.rs");

/// The recall targets frozen as named thresholds by `fit`, on prompt-hook positives.
const POINTS: [(&str, f64); 4] = [("zero", 1.0), ("wide", 0.98), ("mid", 0.95), ("narrow", 0.90)];

struct Row {
    id: String,
    stratum: String,
    repeats: u32,
    text: String,
    label: bool,
    source: String,
}

/// The text field of a row `JSON.stringify` wrote: it is the last key. Astral
/// characters are written raw, so a `\u` escape is a control or a lone surrogate.
fn decode_text(line: &str) -> String {
    let at = line.find("\"text\":\"").expect("a text field") + 8;
    let mut out = String::new();
    let mut it = line[at..].chars();
    while let Some(c) = it.next() {
        match c {
            '"' => break,
            '\\' => match it.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('u') => {
                    let h: String = it.by_ref().take(4).collect();
                    let u = u32::from_str_radix(&h, 16).expect("hex");
                    out.push(char::from_u32(u).unwrap_or('\u{fffd}'));
                }
                Some(o) => out.push(o),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

fn field<'a>(line: &'a str, key: &str) -> &'a str {
    let pat = format!("\"{key}\":");
    let at = line.find(&pat).expect("a field") + pat.len();
    let rest = &line[at..];
    let end = rest.find(',').expect("not the last field");
    rest[..end].trim_matches('"')
}

fn load(corpus: &str, labels: &str) -> Vec<Row> {
    let mut by_id: HashMap<&str, (bool, &str)> = HashMap::new();
    for l in labels.lines() {
        let mut p = l.trim_end().split('\t');
        let (i, a, s) = (p.next().unwrap(), p.next().expect("a label"), p.next().unwrap_or("unknown"));
        by_id.insert(i, (a == "1", s));
    }
    let rows: Vec<Row> = corpus
        .lines()
        .filter_map(|l| {
            let id = field(l, "id");
            let &(label, source) = by_id.get(id)?;
            Some(Row { id: id.to_string(), stratum: field(l, "stratum").to_string(), repeats: field(l, "repeats").parse().expect("repeats"), text: decode_text(l), label, source: source.to_string() })
        })
        .collect();
    let lines = corpus.lines().count();
    assert_eq!(rows.len(), lines, "{} corpus rows have no label", lines - rows.len());
    rows
}

/// Logistic regression by batch gradient descent. A row whose source is the owner's
/// own answer counts three times, since it is the only label checked as truth.
fn fit(rows: &[(usize, [f32; N])], data: &[Row]) -> [f32; N] {
    let mut w = [0f32; N];
    let total: f32 = rows.iter().map(|(i, _)| row_weight(&data[*i])).sum();
    for _ in 0..12_000 {
        let mut g = [0f32; N];
        for (i, f) in rows {
            let r = &data[*i];
            let z: f32 = f.iter().zip(&w).map(|(a, b)| a * b).sum();
            let p = 1.0 / (1.0 + (-z).exp());
            let e = row_weight(r) * (p - f32::from(u8::from(r.label)));
            for k in 0..N {
                g[k] += e * f[k];
            }
        }
        for k in 0..N {
            w[k] -= 0.15 * (g[k] / total + 0.002 * w[k]);
        }
    }
    w
}

fn row_weight(r: &Row) -> f32 {
    if r.source == "brandon" { 3.0 } else { 1.0 }
}

/// Exact one-sided 95% upper bound on a binomial rate (Clopper-Pearson), by bisection.
fn upper95(k: usize, n: usize) -> f64 {
    if n == 0 {
        return 1.0;
    }
    let cdf = |p: f64| -> f64 {
        let (mut sum, mut term) = (0.0, (1.0 - p).powi(n as i32));
        for i in 0..=k {
            sum += term;
            term *= (n - i) as f64 / (i + 1) as f64 * p / (1.0 - p);
        }
        sum
    };
    let (mut lo, mut hi) = (k as f64 / n as f64, 1.0 - 1e-12);
    for _ in 0..80 {
        let mid = (lo + hi) / 2.0;
        if cdf(mid) > 0.05 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

fn table(rows: &[Row], scores: &[f32], t: f32, per_day: &HashMap<String, f64>, out: &mut String) {
    let sources: BTreeSet<&str> = rows.iter().map(|r| r.source.as_str()).collect();
    writeln!(out, "| stratum | source | rows | P | misses | miss 95% UB | captures/100 | false/100 | captures/day | false/day |\n|---|---|---|---|---|---|---|---|---|---|").unwrap();
    for st in ["typed", "queued", "prompt", "ask"] {
        for src in std::iter::once("all").chain(sources.iter().copied()) {
            let sel: Vec<usize> = (0..rows.len()).filter(|&i| (rows[i].stratum == st || (st == "prompt" && rows[i].stratum != "ask")) && (src == "all" || rows[i].source == src)).collect();
            let n = sel.len();
            let p = sel.iter().filter(|&&i| rows[i].label).count();
            let miss = sel.iter().filter(|&&i| rows[i].label && scores[i] < t).count();
            let cap = sel.iter().filter(|&&i| scores[i] >= t).count();
            let fals = sel.iter().filter(|&&i| !rows[i].label && scores[i] >= t).count();
            // Per day is the raw stream's rate, so a row counts with its repeats.
            let weigh = |f: &dyn Fn(usize) -> bool| sel.iter().filter(|&&i| f(i)).map(|&i| 1.0 + f64::from(rows[i].repeats)).sum::<f64>();
            let wt = weigh(&|_| true);
            let day = if st == "prompt" { per_day.get("typed").zip(per_day.get("queued")).map(|(a, b)| a + b) } else { per_day.get(st).copied() };
            let (dc, df) = match day {
                Some(d) if src == "all" && wt > 0.0 => (format!("{:.1}", weigh(&|i| scores[i] >= t) / wt * d), format!("{:.1}", weigh(&|i| !rows[i].label && scores[i] >= t) / wt * d)),
                _ => ("-".into(), "-".into()),
            };
            writeln!(out, "| {st} | {src} | {n} | {p} | {miss} | {:.1}% | {:.1} | {:.1} | {dc} | {df} |", upper95(miss, p) * 100.0, cap as f64 * 100.0 / n.max(1) as f64, fals as f64 * 100.0 / n.max(1) as f64).unwrap();
        }
    }
}

/// The score at which a share `recall` of the prompt-hook positives is still captured.
fn threshold_for(rows: &[Row], scores: &[f32], recall: f64) -> f32 {
    let mut pos: Vec<f32> = rows.iter().zip(scores).filter(|(r, _)| r.label && r.stratum != "ask").map(|(_, s)| *s).collect();
    pos.sort_by(f32::total_cmp);
    pos[((1.0 - recall) * pos.len() as f64).floor() as usize]
}

/// Recall, share captured and precision among the rows `keep` selects.
fn tally(rows: &[Row], scores: &[f32], t: f32, keep: &dyn Fn(&Row) -> bool) -> (f64, f64, f64, usize) {
    let (mut p, mut hit, mut cap, mut n) = (0, 0, 0, 0);
    for (r, s) in rows.iter().zip(scores).filter(|(r, _)| keep(r)) {
        n += 1;
        p += usize::from(r.label);
        hit += usize::from(r.label && *s >= t);
        cap += usize::from(*s >= t);
    }
    (hit as f64 * 100.0 / p.max(1) as f64, cap as f64 * 100.0 / n.max(1) as f64, hit as f64 * 100.0 / cap.max(1) as f64, p)
}

fn latency(rows: &[Row], w: &[f32; N], out: &mut String) {
    let longest = rows.iter().max_by_key(|r| r.text.len()).unwrap();
    let mut sink = 0f32;
    let cold = Instant::now();
    sink += score_with(&rows[0].text, w);
    let cold = cold.elapsed().as_nanos();
    let mut all = Vec::with_capacity(rows.len() * 5);
    for _ in 0..5 {
        for r in rows {
            let t = Instant::now();
            sink += score_with(&r.text, w);
            all.push(t.elapsed().as_nanos() as u64);
        }
    }
    all.sort_unstable();
    let mut long = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        sink += score_with(&longest.text, w);
        long.push(t.elapsed().as_nanos() as u64);
    }
    long.sort_unstable();
    writeln!(out, "\nlatency ns: first call {cold}; over rows p50 {} p99 {} max {}; longest row ({} bytes) p50 {} p99 {} (sink {:.0})", all[all.len() / 2], all[all.len() * 99 / 100], all[all.len() - 1], longest.text.len(), long[long.len() / 2], long[long.len() * 99 / 100], sink.signum()).unwrap();
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn sha(text: &str) -> String {
    sha256::hex(text.as_bytes())
}

fn parse_weights(text: &str) -> ([f32; N], Vec<(String, f32)>) {
    let mut w = [0f32; N];
    let mut points = Vec::new();
    for l in text.lines() {
        let mut p = l.split_whitespace();
        match p.next() {
            Some("weights") => {
                let v: Vec<f32> = p.map(|x| x.parse().expect("a weight")).collect();
                w = v.try_into().expect("this detector's feature count: refit");
            }
            Some("point") => points.push((p.next().unwrap().to_string(), p.next().unwrap().parse().unwrap())),
            _ => {}
        }
    }
    (w, points)
}

fn run_fit(corpus: &str, labels: &str, opt: &dyn Fn(&str) -> Option<String>) {
    let (c, l) = (read(corpus), read(labels));
    let data = load(&c, &l);
    let feats: Vec<(usize, [f32; N])> = data.iter().enumerate().map(|(i, r)| (i, detect::features(&r.text))).collect();
    let mut out = String::new();
    // Forward chaining: fit on the older 70%, thresholds from it, read on the newer 30%.
    let cut = data.len() * 7 / 10;
    let w_old = fit(&feats[..cut], &data);
    let s_old: Vec<f32> = data[..cut].iter().map(|r| score_with(&r.text, &w_old)).collect();
    let s_new: Vec<f32> = data[cut..].iter().map(|r| score_with(&r.text, &w_old)).collect();
    writeln!(out, "forward check, newer 30%, prompts (target: recall captured% precision):").unwrap();
    for (name, target) in POINTS {
        let t = threshold_for(&data[..cut], &s_old, target);
        let (r, cap, pr, p) = tally(&data[cut..], &s_new, t, &|r: &Row| r.stratum != "ask");
        writeln!(out, "  {name} {target:.2}: {r:.1} {cap:.1} {pr:.1} (P={p})").unwrap();
    }
    let w = fit(&feats, &data);
    let scores: Vec<f32> = data.iter().map(|r| score_with(&r.text, &w)).collect();
    let mut file = format!("# corpus {}\n# labels {}\n# detector {}\nweights", sha(&c), sha(&l), sha(DETECTOR));
    for x in w {
        write!(file, " {x:.4}").unwrap();
    }
    file.push('\n');
    writeln!(out, "all rows, frozen points (prompt recall captured% precision | ask recall captured%):").unwrap();
    for (name, target) in POINTS {
        let t = threshold_for(&data, &scores, target);
        writeln!(file, "point {name} {t:.4}").unwrap();
        let (r, cap, pr, _) = tally(&data, &scores, t, &|r: &Row| r.stratum != "ask");
        let (ar, ac, _, _) = tally(&data, &scores, t, &|r: &Row| r.stratum == "ask");
        writeln!(out, "  {name} {t:+.3}: {r:.1} {cap:.1} {pr:.1} | {ar:.1} {ac:.1}").unwrap();
    }
    // The bar to beat: a word count alone.
    write!(out, "baseline words>=K, prompts (K: recall/captured%):").unwrap();
    for k in [3usize, 5, 7, 9, 12, 15, 20] {
        let wc: Vec<f32> = data.iter().map(|r| r.text.split_whitespace().count() as f32).collect();
        let (r, cap, _, _) = tally(&data, &wc, k as f32, &|r: &Row| r.stratum != "ask");
        write!(out, " {k}: {r:.1}/{cap:.1}").unwrap();
    }
    writeln!(out).unwrap();
    std::fs::write(opt("--out").expect("--out"), &file).unwrap();
    print!("{out}");
}

fn run_score(corpus: &str, labels: &str, opt: &dyn Fn(&str) -> Option<String>, held: bool) {
    let c = read(corpus);
    let corpus_sha = sha(&c);
    let spent_dir = Path::new(corpus).parent().unwrap_or(Path::new(".")).join("report/spent");
    let spent = spent_dir.join(format!("{corpus_sha}.txt"));
    if held {
        std::fs::create_dir_all(&spent_dir).unwrap();
        // create_new: the set is spent the moment it is read, whatever the run then does.
        std::fs::OpenOptions::new().write(true).create_new(true).open(&spent).expect("this held-out corpus is spent: a retest needs prompts newer than the freeze");
        assert!(opt("--errors").is_none(), "held-out never writes text");
    } else {
        assert!(!corpus.contains("heldout") && !spent.exists(), "held-out text is read only with --heldout, once");
    }
    let l = read(labels);
    let wpath = opt("--weights").expect("--weights");
    let wtext = read(&wpath);
    let (w, points) = parse_weights(&wtext);
    let data = load(&c, &l);
    let per_day: HashMap<String, f64> = opt("--per-day").unwrap_or_default().split(',').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.parse().expect("a rate"))).collect();
    let scores: Vec<f32> = data.iter().map(|r| score_with(&r.text, &w)).collect();
    let mut out = String::new();
    writeln!(out, "corpus {corpus_sha}\nlabels {}\nweights {}\ndetector {}", sha(&l), sha(&wtext), sha(DETECTOR)).unwrap();
    for (name, t) in &points {
        writeln!(out, "\n## point {name}, threshold {t:+.3}").unwrap();
        table(&data, &scores, *t, &per_day, &mut out);
        if held {
            let miss: Vec<&str> = data.iter().zip(&scores).filter(|(r, s)| r.label && **s < *t).map(|(r, _)| r.id.as_str()).collect();
            std::fs::write(spent_dir.join(format!("{}.miss.{name}.ids", &corpus_sha[..12])), miss.join("\n") + "\n").unwrap();
        }
    }
    latency(&data, &w, &mut out);
    std::fs::write(opt("--out").expect("--out"), &out).unwrap();
    if let Some(p) = opt("--errors") {
        let t: f32 = opt("--at").expect("--at").parse().unwrap();
        let mut e: Vec<(f32, String)> = Vec::new();
        for (r, s) in data.iter().zip(&scores) {
            if r.label != (*s >= t) {
                let short: String = r.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(150).collect();
                e.push((*s, format!("{} {} {} {:+.2} {short}", if r.label { "MISS" } else { "FALSE" }, r.source, &r.stratum[..1], s)));
            }
        }
        e.sort_by(|a, b| a.0.total_cmp(&b.0));
        std::fs::write(p, e.into_iter().map(|x| x.1).collect::<Vec<_>>().join("\n") + "\n").unwrap();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opt = |k: &str| args.iter().position(|a| a == k).map(|i| args[i + 1].clone());
    let corpus = opt("--corpus").expect("--corpus");
    let labels = opt("--labels").expect("--labels");
    match args.get(1).map(String::as_str) {
        Some("fit") => {
            assert!(!corpus.contains("heldout"), "a fit never reads held-out");
            run_fit(&corpus, &labels, &opt);
        }
        Some("score") => run_score(&corpus, &labels, &opt, args.iter().any(|a| a == "--heldout")),
        _ => panic!("eval fit|score"),
    }
}
