//! `cargo xtask report-eval`: the report digest fidelity eval.
//!
//! A finished subagent hands its report to the session that launched it.
//! A digest of the report (the outcome, the numbers, the questions, the
//! risks; the full report one recall away) is only safe if every fact the
//! orchestrator acts on survives it. This scores [`atlas::report::digest`]
//! on a corpus of real reports whose needed facts were labelled by hand:
//!
//! * `whole`: every labelled fact is in the digest;
//! * `partial`: some are, some are not;
//! * `missed`: none are.
//!
//! A fact counts as kept when its text occurs in the digest. Tokens are the
//! bytes of the digest and of the report over
//! [`atlas::tokens::DEFAULT_BYTES_PER_TOKEN`], as the other evals count them.
//!
//! The case file is plain text, one block per case:
//!
//! ```text
//! R001 builder
//! file: reports/R001.txt
//! from: <session>/subagents/agent-<id>.jsonl
//! need: outcome | PR #93
//! need: number | 743/0/18
//! need: question | Q-OR1
//! need: risk | denied by the auto mode classifier
//! ```
//!
//! The word after the id is the agent kind (`builder`, `reviewer`,
//! `research`). `need: CATEGORY | TEXT` names a fact (category `outcome`,
//! `number`, `question` or `risk`) and a distinctive part of it; TEXT must
//! occur in the report, or the case is invalid and the run fails.
//!
//! Output: a summary line, a table by kind and one by category, a `miss`
//! line per fact the digest dropped; `--rows` adds a `row` line per case;
//! `--show ID` prints a case's digest. `--budget BYTES` scores one fixed
//! budget in place of the digest bound, [`atlas::report::WHOLE_UP_TO`].
//! Each run also scores the [`FIXED`] budgets, whose summary, `miss@B` and
//! `row@B` lines gate tighter digests than the bound ever needs.
//!
//! `--gate ROWS` reads an earlier run's output and fails, at the default
//! and at each fixed budget, if any case keeps fewer facts now, if fewer
//! facts are kept in all, or if a fact is missed now that was kept then
//! (the set of misses, not only their count).
//!
//! The corpus is private and lives outside the repository: the reports are
//! a user's own sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The fixed budgets every run also scores (the 11-04 refinement of Q-SD3).
pub const FIXED: [usize; 2] = [4_000, 8_000];

/// The fact categories, in report order.
pub const CATEGORIES: [&str; 4] = ["outcome", "number", "question", "risk"];

/// One labelled report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Case {
    /// `R001`.
    pub id: String,
    /// The agent's kind.
    pub kind: String,
    /// The report, relative to the case file's folder.
    pub file: String,
    /// Where it came from.
    pub from: String,
    /// The facts: (category, text).
    pub needs: Vec<(String, String)>,
}

/// Parses a case file.
///
/// # Errors
///
/// A message naming the line for a field outside a block, an unknown field
/// or category, or a block missing `file`, `from` or a `need`.
pub fn parse(text: &str) -> Result<Vec<Case>, String> {
    let mut out: Vec<Case> = Vec::new();
    let finish = |c: &Case| -> Result<(), String> {
        if c.file.is_empty() || c.from.is_empty() || c.needs.is_empty() {
            return Err(format!("{}: needs file, from and at least one need", c.id));
        }
        Ok(())
    };
    for (n, line) in text.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = l.split_once(':').filter(|(k, _)| !k.contains(' ')) {
            let c = out.last_mut().ok_or_else(|| format!("line {}: {k} outside a case", n + 1))?;
            let v = v.trim().to_string();
            match k {
                "file" => c.file = v,
                "from" => c.from = v,
                "need" => {
                    let (cat, t) = v.split_once('|').ok_or_else(|| format!("line {}: need is CATEGORY | TEXT", n + 1))?;
                    let cat = cat.trim();
                    if !CATEGORIES.contains(&cat) {
                        return Err(format!("line {}: unknown category {cat:?}", n + 1));
                    }
                    let t = t.trim();
                    if t.is_empty() {
                        return Err(format!("line {}: empty need", n + 1));
                    }
                    c.needs.push((cat.to_string(), t.to_string()));
                }
                _ => return Err(format!("line {}: unknown field {k:?}", n + 1)),
            }
            continue;
        }
        if let Some(prev) = out.last() {
            finish(prev)?;
        }
        let mut w = l.split_whitespace();
        let id = w.next().unwrap_or_default().to_string();
        let kind = w.next().ok_or_else(|| format!("line {}: a case is `ID KIND`", n + 1))?.to_string();
        out.push(Case { id, kind, ..Case::default() });
    }
    if let Some(prev) = out.last() {
        finish(prev)?;
    }
    Ok(out)
}

/// Checks a case against its report: every need's text occurs in it.
///
/// # Errors
///
/// A message naming the case and the text.
pub fn validate(c: &Case, report: &str) -> Result<(), String> {
    for (_, t) in &c.needs {
        if !report.contains(t.as_str()) {
            return Err(format!("{}: need {t:?} is not in {}", c.id, c.file));
        }
    }
    Ok(())
}

/// One scored case.
#[derive(Debug, Clone)]
pub struct Scored {
    /// The case.
    pub case: Case,
    /// Each need: kept or not.
    pub kept: Vec<bool>,
    /// Bytes of the digest and of the report.
    pub bytes: [usize; 2],
    /// The digest.
    pub digest: String,
}

impl Scored {
    fn n_kept(&self) -> usize {
        self.kept.iter().filter(|k| **k).count()
    }
    fn level(&self) -> &'static str {
        match self.n_kept() {
            n if n == self.kept.len() => "whole",
            0 => "missed",
            _ => "partial",
        }
    }
}

/// Scores one case with a budget (`None`: the digest bound).
pub fn score(c: &Case, report: &str, budget: Option<usize>) -> Scored {
    let digest = atlas::report::digest(report, budget.unwrap_or(atlas::report::WHOLE_UP_TO));
    let kept = c.needs.iter().map(|(_, t)| digest.contains(t.as_str())).collect();
    Scored { case: c.clone(), kept, bytes: [digest.len(), report.len()], digest }
}

/// The bytes of the report's pieces (as [`atlas::report::pieces`] splits
/// its lines) that hold a labelled fact, one newline each: the smallest
/// digest built from whole pieces that keeps every fact.
pub fn oracle_bytes(c: &Case, report: &str) -> usize {
    report
        .trim()
        .lines()
        .flat_map(atlas::report::pieces)
        .filter(|p| c.needs.iter().any(|(_, t)| p.contains(t.as_str())))
        .map(|p| p.len() + 1)
        .sum()
}

fn tok(b: usize) -> f64 {
    b as f64 / atlas::tokens::DEFAULT_BYTES_PER_TOKEN
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    cases: usize,
    whole: usize,
    needs: usize,
    kept: usize,
    bytes: [usize; 2],
}

/// The report's lines. Stable for a given corpus and atlas.
pub fn report(scored: &[Scored], rows: bool) -> String {
    let mut by_kind: BTreeMap<&str, Tally> = BTreeMap::new();
    let mut all = Tally::default();
    let mut by_cat: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for s in scored {
        for t in [by_kind.entry(s.case.kind.as_str()).or_default(), &mut all] {
            t.cases += 1;
            t.whole += usize::from(s.level() == "whole");
            t.needs += s.kept.len();
            t.kept += s.n_kept();
            t.bytes[0] += s.bytes[0];
            t.bytes[1] += s.bytes[1];
        }
        for ((cat, _), k) in s.case.needs.iter().zip(&s.kept) {
            let e = by_cat.entry(cat.as_str()).or_default();
            e.0 += 1;
            e.1 += usize::from(*k);
        }
    }
    let mut out = format!(
        "report-eval: {} cases; whole {}, partial {}, missed {}; facts kept {}/{}; tokens digest {:.0}, report {:.0} ({:.1}%)\n",
        all.cases,
        all.whole,
        scored.iter().filter(|s| s.level() == "partial").count(),
        scored.iter().filter(|s| s.level() == "missed").count(),
        all.kept,
        all.needs,
        tok(all.bytes[0]),
        tok(all.bytes[1]),
        100.0 * all.bytes[0] as f64 / all.bytes[1].max(1) as f64
    );
    out.push_str("kind\tcases\twhole\tfacts\tkept\ttok_digest\ttok_report\n");
    let line = |name: &str, t: &Tally| format!("{name}\t{}\t{}\t{}\t{}\t{:.0}\t{:.0}\n", t.cases, t.whole, t.needs, t.kept, tok(t.bytes[0]), tok(t.bytes[1]));
    for (k, t) in &by_kind {
        out.push_str(&line(k, t));
    }
    out.push_str(&line("all", &all));
    out.push_str("category\tfacts\tkept\n");
    for c in CATEGORIES {
        let (n, k) = by_cat.get(c).copied().unwrap_or_default();
        out.push_str(&format!("{c}\t{n}\t{k}\n"));
    }
    out.push_str(&lines("", scored, rows));
    out
}

/// The summary line of a fixed budget, then its `miss@B` and `row@B` lines.
pub fn fixed_report(budget: usize, scored: &[Scored], rows: bool) -> String {
    let whole = scored.iter().filter(|s| s.level() == "whole").count();
    let (kept, needs) = scored.iter().fold((0, 0), |(k, n), s| (k + s.n_kept(), n + s.kept.len()));
    let (d, r) = scored.iter().fold((0, 0), |(d, r), s| (d + s.bytes[0], r + s.bytes[1]));
    let mut out = format!("fixed {budget} B: whole {whole}/{}; facts kept {kept}/{needs}; tokens digest {:.0} ({:.1}%)\n", scored.len(), tok(d), 100.0 * d as f64 / r.max(1) as f64);
    out.push_str(&lines(&format!("@{budget}"), scored, rows));
    out
}

/// The `miss` line of every dropped fact and, with `rows`, a `row` line per
/// case; `label` follows the line's name (`@4000` for a fixed budget).
fn lines(label: &str, scored: &[Scored], rows: bool) -> String {
    let mut out = String::new();
    for s in scored {
        for ((cat, t), k) in s.case.needs.iter().zip(&s.kept) {
            if !k {
                out.push_str(&format!("miss{label}\t{}\t{}\t{cat}\t{t}\n", s.case.id, s.case.kind));
            }
        }
    }
    if rows {
        for s in scored {
            out.push_str(&format!("row{label}\t{}\t{}\t{}\t{}\t{}\t{:.0}\t{:.0}\n", s.case.id, s.case.kind, s.level(), s.n_kept(), s.kept.len(), tok(s.bytes[0]), tok(s.bytes[1])));
        }
    }
    out
}

/// What fails the gate against an earlier run's `row{label}` and
/// `miss{label}` lines: each case keeping fewer facts than it did, each
/// fact missed now that it kept, and the total kept falling. A case the
/// earlier run has no row for is not compared.
pub fn regressions(earlier: &str, label: &str, scored: &[Scored]) -> Vec<String> {
    let (row, miss) = (format!("row{label}"), format!("miss{label}"));
    let mut before: BTreeMap<&str, usize> = BTreeMap::new();
    let mut missed: BTreeSet<(&str, &str)> = BTreeSet::new();
    for l in earlier.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        match (f.first(), f.get(1)) {
            // A miss line's text is everything after its fourth tab.
            (Some(&m), Some(&id)) if m == miss => {
                if let Some(t) = l.splitn(5, '\t').nth(4) {
                    missed.insert((id, t));
                }
            }
            (Some(&r), Some(&id)) if r == row => {
                if let Some(k) = f.get(4).and_then(|k| k.parse().ok()) {
                    before.insert(id, k);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for s in scored {
        let Some(&b) = before.get(s.case.id.as_str()) else { continue };
        if s.n_kept() < b {
            out.push(format!("{}{label}: kept {b} -> {}", s.case.id, s.n_kept()));
        }
        for ((_, t), k) in s.case.needs.iter().zip(&s.kept) {
            if !k && !missed.contains(&(s.case.id.as_str(), t.as_str())) {
                out.push(format!("{}{label}: newly missed {t:?}", s.case.id));
            }
        }
    }
    let was: usize = before.values().sum();
    let now: usize = scored.iter().filter(|s| before.contains_key(s.case.id.as_str())).map(Scored::n_kept).sum();
    if now < was {
        out.push(format!("facts kept{label}: {was} -> {now}"));
    }
    out
}

/// The options of a run.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// The case file.
    pub cases: PathBuf,
    /// Print a `row` line per case.
    pub rows: bool,
    /// Cases to print in full.
    pub show: Vec<String>,
    /// An earlier run's rows to gate against.
    pub gate: Option<PathBuf>,
    /// A fixed budget in bytes.
    pub budget: Option<usize>,
}

/// Runs the eval and prints its report. Returns false when the gate found
/// a regression.
///
/// # Errors
///
/// An unreadable or invalid case file or report.
pub fn run(o: &Opts) -> Result<bool, String> {
    let text = std::fs::read_to_string(&o.cases).map_err(|e| format!("{}: {e}", o.cases.display()))?;
    let cases = parse(&text)?;
    let base = o.cases.parent().unwrap_or(Path::new("."));
    let mut scored = Vec::new();
    let mut bad = Vec::new();
    for c in &cases {
        let r = std::fs::read_to_string(base.join(&c.file)).map_err(|e| format!("{}: {}: {e}", c.id, c.file))?;
        match validate(c, &r) {
            Ok(()) => scored.push(score(c, &r, o.budget)),
            Err(e) => bad.push(e),
        }
    }
    if !bad.is_empty() {
        return Err(format!("invalid cases:\n{}", bad.join("\n")));
    }
    print!("{}", report(&scored, o.rows));
    let texts: Vec<String> = cases.iter().map(|c| std::fs::read_to_string(base.join(&c.file)).unwrap_or_default()).collect();
    let need_bytes: usize = cases.iter().zip(&texts).map(|(c, r)| oracle_bytes(c, r)).sum();
    let all_bytes: usize = texts.iter().map(String::len).sum();
    let fixed = FIXED.map(|b| (b, cases.iter().zip(&texts).map(|(c, r)| score(c, r, Some(b))).collect::<Vec<_>>()));
    for (b, s) in &fixed {
        print!("{}", fixed_report(*b, s, o.rows));
    }
    println!(
        "oracle: the pieces holding a fact are {:.0} tokens of {:.0} ({:.1}%), the smallest digest that keeps every fact",
        tok(need_bytes),
        tok(all_bytes),
        100.0 * need_bytes as f64 / all_bytes.max(1) as f64
    );
    for id in &o.show {
        let s = scored.iter().find(|s| &s.case.id == id).ok_or_else(|| format!("--show {id}: no such case"))?;
        println!("== {id} {} ({}, {} of {} facts)\n{}", s.case.kind, s.level(), s.n_kept(), s.kept.len(), s.digest);
    }
    if let Some(g) = &o.gate {
        let earlier = std::fs::read_to_string(g).map_err(|e| format!("{}: {e}", g.display()))?;
        let mut r = regressions(&earlier, "", &scored);
        for (b, s) in &fixed {
            r.extend(regressions(&earlier, &format!("@{b}"), s));
        }
        for l in &r {
            println!("regression\t{l}");
        }
        return Ok(r.is_empty());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &str = "# a corpus\nR001 builder\nfile: reports/R001.txt\nfrom: s/subagents/agent-a.jsonl\nneed: outcome | PR #93\nneed: number | 743/0/18\n\nR002 reviewer\nfile: reports/R002.txt\nfrom: s/subagents/agent-b.jsonl\nneed: risk | denied\n";

    #[test]
    fn parses_cases_and_refuses_bad_ones() {
        let c = parse(CASES).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].needs, [("outcome".to_string(), "PR #93".to_string()), ("number".to_string(), "743/0/18".to_string())]);
        assert_eq!(c[1].kind, "reviewer");
        assert!(parse("R1 builder\nfile: a\nfrom: b\nneed: mood | x\n").unwrap_err().contains("unknown category"));
        assert!(parse("R1 builder\nfile: a\nfrom: b\n").unwrap_err().contains("at least one need"));
        assert!(parse("file: a\n").unwrap_err().contains("outside a case"));
        assert!(validate(&c[0], "PR #93 only").unwrap_err().contains("743/0/18"));
    }

    #[test]
    fn scores_reports_and_gates_on_kept_facts() {
        let c = parse(CASES).unwrap();
        let a = score(&c[0], "PR #93 merged. Tests 743/0/18.", None);
        assert_eq!(a.level(), "whole");
        let b = score(&c[1], "Nothing was denied.", None);
        assert_eq!(b.level(), "whole");
        let out = report(&[a.clone(), b.clone()], true);
        assert!(out.starts_with("report-eval: 2 cases; whole 2, partial 0, missed 0; facts kept 3/3"), "{out}");
        assert!(out.contains("row\tR001\tbuilder\twhole\t2\t2\t"), "{out}");
        let mut worse = a.clone();
        worse.kept = vec![true, false];
        let r = regressions(&out, "", &[worse, b]);
        assert_eq!(r, ["R001: kept 2 -> 1", "R001: newly missed \"743/0/18\"", "facts kept: 3 -> 2"]);
    }

    /// The same count of misses is still a regression when a different fact
    /// is missed; a fixed budget's lines are gated under their own label.
    #[test]
    fn the_gate_compares_which_facts_are_missed() {
        let c = parse(CASES).unwrap();
        let mut a = score(&c[0], "PR #93 merged. Tests 743/0/18.", Some(4_000));
        a.kept = vec![true, false];
        let earlier = fixed_report(4_000, std::slice::from_ref(&a), true);
        assert!(earlier.starts_with("fixed 4000 B: whole 0/1; facts kept 1/2"), "{earlier}");
        assert!(earlier.contains("miss@4000\tR001\tbuilder\tnumber\t743/0/18\n") && earlier.contains("row@4000\tR001\t"), "{earlier}");
        assert!(regressions(&earlier, "@4000", std::slice::from_ref(&a)).is_empty());
        let mut swapped = a.clone();
        swapped.kept = vec![false, true];
        assert_eq!(regressions(&earlier, "@4000", &[swapped.clone()]), ["R001@4000: newly missed \"PR #93\""]);
        assert!(regressions(&earlier, "", &[swapped]).is_empty(), "other labels are not compared");
    }
}
