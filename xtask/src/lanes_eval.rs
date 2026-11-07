//! `cargo xtask lanes-eval`: scores the work queue's brief classification
//! (`atlas::lanes::classify`) on a corpus of real briefs labelled by hand.
//!
//! The corpus is a folder: `labels.tsv` (a header, then
//! `id, lane, priority, uses, writes, evidence`, tab-separated; `-` for no
//! lane or no uses) and `briefs/ID.txt`. Each brief is classified with no
//! declarations added and compared field by field:
//!
//! * lane: the inferred lane equals the label (`-` is none);
//! * priority: the inferred class equals the label;
//! * resources: a labelled resource not inferred is missed, an inferred one
//!   not labelled is spurious. `file:` is declared only and not scored.
//!
//! `--bar` checks the ruled bar (Q-QL3): no missed `gpu` or `compile`, lane
//! accuracy at least 95%, spurious resources on at most 10% of briefs.
//! `--rows` prints a `row` line per brief; `--gate ROWS` fails when a brief
//! misses a `gpu` or `compile` it did not miss in ROWS, or loses its lane.

use std::path::PathBuf;

/// One labelled brief.
#[derive(Debug, Clone)]
pub struct Label {
    /// Its id (`B001`).
    pub id: String,
    /// Its lane, if any.
    pub lane: Option<String>,
    /// Its priority.
    pub priority: String,
    /// Its resources, without `file:` ones.
    pub uses: Vec<String>,
    /// Whether it writes.
    pub writes: bool,
}

/// Parses `labels.tsv`.
///
/// # Errors
///
/// A malformed line.
pub fn parse(text: &str) -> Result<Vec<Label>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 5 {
            return Err(format!("labels.tsv line {}: expected id, lane, priority, uses, writes", n + 1));
        }
        let none = |s: &str| s.trim().is_empty() || s.trim() == "-";
        let lane = (!none(f[1])).then(|| f[1].trim().to_string());
        let priority = f[2].trim().to_ascii_lowercase();
        if atlas::lanes::Priority::parse(&priority).is_none() {
            return Err(format!("labels.tsv line {}: priority {priority:?}", n + 1));
        }
        let mut uses: Vec<String> = if none(f[3]) { Vec::new() } else { f[3].split(',').map(|x| x.trim().to_ascii_lowercase()).filter(|x| !x.is_empty() && !x.starts_with("file:")).collect() };
        uses.sort();
        out.push(Label { id: f[0].trim().to_string(), lane, priority, uses, writes: f[4].trim().eq_ignore_ascii_case("yes") });
    }
    Ok(out)
}

/// One brief's score.
#[derive(Debug, Clone)]
pub struct Scored {
    /// The label.
    pub label: Label,
    /// The inferred lane.
    pub lane: Option<String>,
    /// The inferred priority.
    pub priority: String,
    /// Labelled, not inferred.
    pub missed: Vec<String>,
    /// Inferred, not labelled.
    pub spurious: Vec<String>,
    /// The words each inferred resource matched (`gpu: "capture"`).
    pub evidence: Vec<String>,
}

impl Scored {
    fn lane_ok(&self) -> bool {
        self.lane == self.label.lane
    }

    fn priority_ok(&self) -> bool {
        self.priority == self.label.priority
    }

    fn hard_missed(&self) -> Vec<&str> {
        self.missed.iter().map(String::as_str).filter(|r| matches!(*r, "gpu" | "compile")).collect()
    }
}

/// Scores one brief.
pub fn score(label: &Label, brief: &str) -> Scored {
    let c = atlas::lanes::classify(brief, &[]);
    let inferred: Vec<String> = c.resources().filter(|r| !r.starts_with("file:")).map(str::to_string).collect();
    let evidence = c.uses.iter().map(|(r, s)| format!("{r}: {s:?}")).collect();
    Scored {
        missed: label.uses.iter().filter(|r| !inferred.contains(r)).cloned().collect(),
        spurious: inferred.iter().filter(|r| !label.uses.contains(r)).cloned().collect(),
        lane: c.lane,
        priority: c.priority.name().to_string(),
        label: label.clone(),
        evidence,
    }
}

/// The totals, and whether the ruled bar holds.
pub fn report(scored: &[Scored], rows: bool) -> (String, bool) {
    let n = scored.len().max(1);
    let lane = scored.iter().filter(|s| s.lane_ok()).count();
    let prio = scored.iter().filter(|s| s.priority_ok()).count();
    let hard: usize = scored.iter().map(|s| s.hard_missed().len()).sum();
    let missed: usize = scored.iter().map(|s| s.missed.len()).sum();
    let spurious_briefs = scored.iter().filter(|s| !s.spurious.is_empty()).count();
    let labelled: usize = scored.iter().map(|s| s.label.uses.len()).sum();
    let mut out = String::new();
    if rows {
        for s in scored {
            out.push_str(&format!(
                "row\t{}\tlane {}\tpriority {}\tmissed {}\tspurious {}\n",
                s.label.id,
                if s.lane_ok() { "ok" } else { "wrong" },
                if s.priority_ok() { "ok" } else { "wrong" },
                if s.missed.is_empty() { "-".into() } else { s.missed.join(",") },
                if s.spurious.is_empty() { "-".into() } else { s.spurious.join(",") }
            ));
        }
    }
    for s in scored.iter().filter(|s| !s.lane_ok() || !s.priority_ok() || !s.missed.is_empty() || !s.spurious.is_empty()) {
        let mut why = Vec::new();
        if !s.lane_ok() {
            why.push(format!("lane {:?} labelled {:?}", s.lane, s.label.lane));
        }
        if !s.priority_ok() {
            why.push(format!("priority {} labelled {}", s.priority, s.label.priority));
        }
        if !s.missed.is_empty() {
            why.push(format!("missed {}", s.missed.join(",")));
        }
        if !s.spurious.is_empty() {
            why.push(format!("spurious {} [{}]", s.spurious.join(","), s.evidence.join("; ")));
        }
        out.push_str(&format!("diff\t{}\t{}\n", s.label.id, why.join("; ")));
    }
    let pct = |k: usize| 100.0 * k as f64 / n as f64;
    out.push_str(&format!("briefs {}\n", scored.len()));
    out.push_str(&format!("lane {lane}/{} ({:.1}%)\n", scored.len(), pct(lane)));
    out.push_str(&format!("priority {prio}/{} ({:.1}%)\n", scored.len(), pct(prio)));
    out.push_str(&format!("resources labelled {labelled}, missed {missed} (gpu or compile {hard}), briefs with a spurious one {spurious_briefs}/{} ({:.1}%)\n", scored.len(), pct(spurious_briefs)));
    out.push_str(&format!("read-only labelled {} (declared only, not inferred)\n", scored.iter().filter(|s| !s.label.writes).count()));
    let bar = hard == 0 && lane * 100 >= 95 * scored.len() && spurious_briefs * 10 <= scored.len();
    out.push_str(&format!("bar (Q-QL3: 0 missed gpu or compile, lane >= 95%, spurious <= 10%): {}\n", if bar { "met" } else { "NOT met" }));
    (out, bar)
}

/// What fails the gate against an earlier run's rows.
pub fn regressions(earlier: &str, scored: &[Scored]) -> Vec<String> {
    let mut out = Vec::new();
    for line in earlier.lines().filter(|l| l.starts_with("row\t")) {
        let f: Vec<&str> = line.split('\t').collect();
        let Some(s) = scored.iter().find(|s| f.get(1) == Some(&s.label.id.as_str())) else { continue };
        if f.get(2) == Some(&"lane ok") && !s.lane_ok() {
            out.push(format!("{}: lane now wrong", s.label.id));
        }
        let was: Vec<&str> = f.get(4).and_then(|m| m.strip_prefix("missed ")).map(|m| m.split(',').collect()).unwrap_or_default();
        for r in s.hard_missed() {
            if !was.contains(&r) {
                out.push(format!("{}: now misses {r}", s.label.id));
            }
        }
    }
    out
}

/// Options.
#[derive(Debug, Default)]
pub struct Opts {
    /// The corpus folder.
    pub corpus: PathBuf,
    /// Print a `row` line per brief.
    pub rows: bool,
    /// Fail unless the ruled bar holds.
    pub bar: bool,
    /// An earlier run's rows to gate against.
    pub gate: Option<PathBuf>,
}

/// Runs the eval. Returns false when the bar (with `--bar`) or the gate
/// fails.
///
/// # Errors
///
/// An unreadable corpus or a malformed label.
pub fn run(o: &Opts) -> Result<bool, String> {
    let lp = o.corpus.join("labels.tsv");
    let labels = parse(&std::fs::read_to_string(&lp).map_err(|e| format!("{}: {e}", lp.display()))?)?;
    let mut scored = Vec::new();
    for l in &labels {
        let p = o.corpus.join("briefs").join(format!("{}.txt", l.id));
        let b = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        scored.push(score(l, &b));
    }
    let (text, bar) = report(&scored, o.rows);
    print!("{text}");
    let mut ok = !o.bar || bar;
    if let Some(g) = &o.gate {
        let earlier = std::fs::read_to_string(g).map_err(|e| format!("{}: {e}", g.display()))?;
        let r = regressions(&earlier, &scored);
        for l in &r {
            println!("regression\t{l}");
        }
        ok &= r.is_empty();
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_lanes_priorities_and_resources() {
        let labels = parse("id\tlane\tpriority\tuses\twrites\tevidence\nB1\tPARSER-FIX\tnormal\tcompile,gpu,file:src/a.rs\tyes\tx\nB2\t-\turgent\t-\tno\ty\n").unwrap();
        assert_eq!(labels[0].uses, ["compile", "gpu"], "file: is not scored");
        let a = score(&labels[0], "Journal to lane PARSER-FIX. Run cargo test.");
        assert!(a.lane_ok() && a.priority_ok());
        assert_eq!(a.missed, ["gpu"]);
        let b = score(&labels[1], "This is a blocker. Take a capture.");
        assert_eq!(b.spurious, ["gpu"]);
        let (text, bar) = report(&[a.clone(), b.clone()], true);
        assert!(!bar && text.contains("missed 1 (gpu or compile 1)"), "{text}");
        assert!(text.contains("row\tB1\tlane ok\tpriority ok\tmissed gpu\tspurious -"), "{text}");
        // Gate: B1 already missed gpu, so only a new miss regresses.
        assert!(regressions("row\tB1\tlane ok\tpriority ok\tmissed gpu\tspurious -", std::slice::from_ref(&a)).is_empty());
        assert_eq!(regressions("row\tB1\tlane ok\tpriority ok\tmissed -\tspurious -", &[a]), ["B1: now misses gpu"]);
    }
}
