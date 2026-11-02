//! `cargo xtask calibrate-recall SCORES.tsv [MIN_TERMS]`: derive the recall
//! hook's relevance floors from `atlas --recall-scores` output.
//!
//! SCORES.tsv is what `atlas --config C --recall-scores PROMPTS.tsv` prints
//! for a prompt file of `label<TAB>prompt` lines, where label is `neg` for
//! prompts that should recall nothing (unrelated to the corpus) and anything
//! else for real prompts. It holds one summary row per prompt and one `cand`
//! row per candidate (`cand, prompt, label, fts, matched, need, cos, key`;
//! cos is `-` when the record has no vector or no model is loaded).
//!
//! The gate (`passes` in src/memory/recall.rs): a candidate passes lexically
//! when fts >= the floor and matched >= need (the floor is `fts_floor`
//! without a model, `hybrid_fts_floor` with one), and then also needs
//! cos >= cos_floor; or it passes on meaning alone when cos >= cos_alone.
//!
//! The rule, with no judgement in it (change the prompt file, not the rule):
//!
//! * Lexical only (no cosines in the file): fts_floor is the highest
//!   best-candidate score among `neg` prompts that also clear MIN_TERMS
//!   (default 3) distinct words, plus 0.05: the lowest floor at which no
//!   unrelated prompt recalls anything.
//! * With cosines: cos_alone is the highest cosine of any `neg` candidate
//!   plus 0.01, so no unrelated prompt recalls anything on meaning alone.
//!   Then for each cos_floor from 0.00 to cos_alone in steps of 0.01,
//!   hybrid_fts_floor is the highest fts among `neg` candidates that clear
//!   MIN_TERMS and that cos_floor, plus 0.05 (0 when there is none): again no
//!   unrelated prompt recalls anything. Of these pairs the one under which
//!   the most real prompts recall something wins; a tie goes to the higher
//!   cos_floor (more agreement between the two scores).
//!
//! Prints the floors and how many prompts of each label recall something.
//! The rounding is Python's (see `py.rs`), so the floors are those the
//! Python script this replaced printed.

use crate::py;
use std::collections::{HashMap, HashSet};

/// Usage.
pub const USAGE: &str = "usage: cargo xtask calibrate-recall SCORES.tsv [MIN_TERMS]";

struct Cand {
    prompt: i64,
    label: String,
    fts: f64,
    matched: i64,
    need: i64,
    cos: Option<f64>,
}

/// `math.ceil(round(x / step, 6)) * step`
fn up(x: f64, step: f64) -> f64 {
    py::round(x / step, 6).ceil() * step
}

/// Reads the scores in `text` and returns the report line.
pub fn report(text: &str, min_terms: i64) -> Result<String, String> {
    let (mut summary, mut cands) = (Vec::<(String, f64, i64)>::new(), Vec::<Cand>::new());
    for line in py::text_lines(text) {
        let line = line.strip_suffix('\n').unwrap_or(&line);
        let r: Vec<&str> = line.split('\t').collect();
        if r[0] == "cand" && r.len() >= 8 {
            let bad = |f: &str| format!("calibrate-recall: a cand row has a bad {f}: {line}");
            let cos = if r[6] == "-" { None } else { Some(py::parse_float(r[6]).ok_or_else(|| bad("cos"))?) };
            let need = min_terms.min(py::parse_int(r[5]).ok_or_else(|| bad("need"))?);
            cands.push(Cand {
                prompt: py::parse_int(r[1]).ok_or_else(|| bad("prompt"))?,
                label: r[2].to_string(),
                fts: py::parse_float(r[3]).ok_or_else(|| bad("fts"))?,
                matched: py::parse_int(r[4]).ok_or_else(|| bad("matched"))?,
                need,
                cos,
            });
        } else if r.len() >= 3 && r[0] != "label" && !r[0].starts_with("gate") && !r[0].starts_with("recall") {
            if let (Some(s), Some(m)) = (py::parse_float(r[1]), py::parse_int(r[2])) {
                summary.push((r[0].to_string(), s, m));
            }
        }
    }
    let prompts: HashMap<i64, &str> = cands.iter().map(|c| (c.prompt, c.label.as_str())).collect();
    let n_neg = summary.iter().filter(|(l, _, _)| l == "neg").count();
    let n_real = summary.len() - n_neg;
    let max = |xs: &mut dyn Iterator<Item = f64>| xs.fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| if x > m { x } else { m })));

    if !cands.iter().any(|c| c.cos.is_some()) {
        let neg = max(&mut summary.iter().filter(|(l, _, m)| l == "neg" && *m >= min_terms).map(|s| s.1));
        let floor = neg.map_or(0.0, |x| py::round(x + 0.05, 2));
        let passing = summary.iter().filter(|(l, s, m)| l != "neg" && *s >= floor && *m >= min_terms).count();
        return Ok(format!(
            "lexical: fts_floor {} (min_terms {min_terms}): {n_neg} unrelated prompts all gated; {passing} of {n_real} real prompts clear it",
            py::repr(floor)
        ));
    }
    let neg: Vec<&Cand> = cands.iter().filter(|c| c.label == "neg").collect();
    let cos_alone = py::round(up(max(&mut neg.iter().filter_map(|c| c.cos)).unwrap_or(0.0) + 0.01, 0.01), 2);
    let fts_floor_for = |cf: f64| {
        let with = neg.iter().filter(|c| c.matched >= c.need && c.cos.is_some_and(|x| x >= cf)).map(|c| c.fts);
        let without = neg.iter().filter(|c| c.matched >= c.need && c.cos.is_none()).map(|c| c.fts);
        max(&mut with.chain(without)).map_or(0.0, |x| py::round(x + 0.05, 2))
    };
    let passes = |c: &Cand, ff: f64, cf: f64| {
        let lexical = c.fts >= ff && c.matched >= c.need;
        match c.cos {
            Some(cos) => (lexical && cos >= cf) || cos >= cos_alone,
            None => lexical,
        }
    };
    let recalled = |ff: f64, cf: f64| {
        let hit: HashSet<i64> = cands.iter().filter(|c| passes(c, ff, cf)).map(|c| c.prompt).collect();
        let real = hit.iter().filter(|i| prompts[i] != "neg").count();
        (real, hit.len() - real)
    };
    let mut best: Option<(usize, f64, f64)> = None;
    let steps = py::round_int(cos_alone / 0.01);
    for k in 0..=steps {
        let cf = py::round(k as f64 * 0.01, 2);
        let ff = fts_floor_for(cf);
        let (real, negs) = recalled(ff, cf);
        if negs != 0 {
            return Err(format!("calibrate-recall: assertion failed: ({}, {}, {negs})", py::repr(cf), py::repr(ff)));
        }
        if best.is_none_or(|b| real >= b.0) {
            best = Some((real, ff, cf));
        }
    }
    let (real, ff, cf) = best.expect("the loop runs at least once");
    Ok(format!(
        "hybrid: hybrid_fts_floor {} cos_floor {} cos_alone {} (min_terms {min_terms}): {n_neg} unrelated prompts all gated; {real} of {n_real} real prompts recall something",
        py::repr(ff),
        py::repr(cf),
        py::repr(cos_alone)
    ))
}
