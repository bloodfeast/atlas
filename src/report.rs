//! A finished subagent's report, digested for the session that launched it.
//!
//! The orchestrator acts on four things in a report: the outcome (a PR, a
//! branch, a merge, a verdict), the numbers (test counts, measurements,
//! eval lines), the questions put to the user, and the risks flagged (a
//! denied action, an unverified claim, a failure left open). [`digest`]
//! keeps the report's sentences that carry them, in the report's order,
//! within a byte budget; the full report stays one recall away.
//!
//! Nothing here decides whether a digest replaces a report: that is gated
//! on `cargo xtask report-eval` (a corpus of real reports with the facts an
//! orchestrator needed labelled by hand), as run digests are gated on
//! `digest-eval`.

/// What a kept piece carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Carries {
    /// A PR, branch, merge, commit or verdict.
    Outcome,
    /// An open question or a decision for the user.
    Question,
    /// A flagged risk or open failure.
    Risk,
    /// A measurement or count.
    Number,
    /// A heading.
    Heading,
}

/// Reports up to this many bytes are never digested: on the report-eval
/// corpus (30 real reports, 284 labelled facts) every budget below it drops
/// a fact, and Q-SD2 ships a digest only when every fact is kept.
pub const WHOLE_UP_TO: usize = 14_000;

/// The budget for a report of `len` bytes: [`WHOLE_UP_TO`], or an eighth of
/// a longer report when that is more.
pub fn budget(len: usize) -> usize {
    WHOLE_UP_TO.max(len / 8)
}

fn has_word(lower: &str, words: &[&str]) -> bool {
    words.iter().any(|w| {
        lower.match_indices(w).any(|(i, _)| {
            let before = lower[..i].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric());
            let after = lower[i + w.len()..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric());
            before && after
        })
    })
}

/// A hex run of 7 to 40 characters with at least one digit and one letter:
/// a commit.
fn has_sha(s: &str) -> bool {
    s.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        (7..=40).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) && w.bytes().any(|b| b.is_ascii_digit()) && w.bytes().any(|b| b.is_ascii_alphabetic())
    })
}

fn has_number(s: &str) -> bool {
    let b = s.as_bytes();
    // A ratio or count like 743/0/18, a percentage, a decimal, a figure with
    // a unit, or a number of three or more digits.
    let mut run = 0;
    for (i, &c) in b.iter().enumerate() {
        if c.is_ascii_digit() {
            run += 1;
            continue;
        }
        if run > 0 && (matches!(c, b'/' | b'%') || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))) {
            return true;
        }
        if run >= 3 && !matches!(c, b'-' | b':') {
            return true;
        }
        if run > 0 && c == b' ' {
            let rest = s[i + 1..].to_ascii_lowercase();
            if ["ms", "tokens", "passed", "failed", "ignored", "tests", "cases", "of ", "k ", "mb", "gb", "s ", "min", "x "].iter().any(|u| rest.starts_with(u)) {
                return true;
            }
        }
        run = 0;
    }
    run >= 3
}

/// What a piece of text carries, most important first.
pub fn classify(s: &str) -> Option<Carries> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let outcome = t.contains("PR #")
        || t.contains("pull/")
        || has_sha(t)
        || has_word(&lower, &["merged", "merge", "branch", "pushed", "shipped", "verdict", "landed", "committed", "approve", "approved", "blocking", "pass", "fail", "passes", "fails"]);
    let question = t.ends_with('?')
        || t.contains("Q-")
        || has_word(&lower, &["question", "questions", "decide", "decision", "choose", "recommended", "recommend", "needs your", "ask"]);
    let risk = has_word(
        &lower,
        &["risk", "risks", "denied", "unverified", "not verified", "untested", "regression", "regressed", "broken", "warning", "blocked", "failed", "failing", "flaky", "unresolved", "open", "caveat", "limitation", "not done", "left", "todo", "bug", "leak"],
    );
    if outcome {
        Some(Carries::Outcome)
    } else if question {
        Some(Carries::Question)
    } else if risk {
        Some(Carries::Risk)
    } else if has_number(t) {
        Some(Carries::Number)
    } else if is_heading(t) {
        Some(Carries::Heading)
    } else {
        None
    }
}

/// A report line split into sentences (a long line holds several facts; a
/// short one is kept whole).
pub fn pieces(line: &str) -> Vec<&str> {
    if line.len() <= 200 {
        return vec![line];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let b = line.as_bytes();
    for i in 0..b.len() {
        let end = matches!(b[i], b'.' | b';' | b'?') && b.get(i + 1).is_none_or(|c| *c == b' ') && !(b[i] == b'.' && i > 0 && b[i - 1].is_ascii_digit() && b.get(i + 1).is_some_and(u8::is_ascii_digit));
        if end {
            out.push(&line[start..=i]);
            start = i + 1;
        }
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out.into_iter().map(str::trim).filter(|p| !p.is_empty()).collect()
}

/// Whether a heading opens a section of questions or decisions for the user.
fn asks(heading: &str) -> bool {
    let l = heading.to_ascii_lowercase();
    ["question", "decision", "decide", "for the user", "forks", "open item", "needs you"].iter().any(|w| l.contains(w))
}

/// Whether a piece is a heading: a Markdown heading, a bold line, or a short
/// line ending in a colon.
fn is_heading(t: &str) -> bool {
    t.starts_with('#') || (t.starts_with("**") && t.ends_with("**")) || (t.ends_with(':') && t.len() <= 60)
}

/// The digest of a report, at most `max` bytes plus a closing line naming
/// what was left out. A report within the budget is returned whole.
///
/// Pieces are kept by tier, then in report order: the opening paragraph,
/// the questions (and every piece of a section whose heading asks for a
/// decision), then outcomes, risks, numbers and headings.
pub fn digest(report: &str, max: usize) -> String {
    let report = report.trim();
    if report.len() <= max {
        return report.to_string();
    }
    // Every piece, with its tier (`None`: never kept) and where it is.
    let mut all: Vec<(usize, &str, Option<u8>)> = Vec::new();
    // The opening: its first three sentences that are not headings.
    let mut lead = 3u8;
    let mut asking = false;
    for line in report.lines() {
        if line.trim().is_empty() {
            if !all.is_empty() {
                lead = 0;
            }
            continue;
        }
        for p in pieces(line) {
            let c = classify(p);
            let heading = is_heading(p.trim());
            if heading {
                asking = asks(p);
            }
            let opening = lead > 0 && !heading;
            if opening {
                lead -= 1;
            }
            let tier = if opening || asking || c == Some(Carries::Question) {
                Some(0)
            } else {
                c.map(|c| match c {
                    Carries::Outcome => 1,
                    Carries::Risk => 2,
                    Carries::Number => 3,
                    Carries::Question => 0,
                    Carries::Heading => 4,
                })
            };
            all.push((all.len(), p, tier));
        }
    }
    // Picked by tier, then in order; a piece that does not fit is skipped
    // and a smaller one may still fit.
    let mut order: Vec<usize> = (0..all.len()).filter(|&i| all[i].2.is_some()).collect();
    order.sort_by_key(|&i| (all[i].2, i));
    let mut keep = vec![false; all.len()];
    let mut used = 0;
    for i in order {
        let n = all[i].1.len() + 1;
        if used + n <= max {
            keep[i] = true;
            used += n;
        }
    }
    let mut out = String::new();
    let mut gap = false;
    for (i, p, _) in &all {
        if keep[*i] {
            if gap && !out.is_empty() {
                out.push_str("...\n");
            }
            out.push_str(p);
            out.push('\n');
            gap = false;
        } else {
            gap = true;
        }
    }
    let dropped = keep.iter().filter(|k| !**k).count();
    out.push_str(&format!("({dropped} of {} sentences left out; the full report is one recall away)\n", all.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_outcomes_questions_risks_and_numbers() {
        assert_eq!(classify("PR #93 merged as 8d32844."), Some(Carries::Outcome));
        assert_eq!(classify("- Q-OS1: does a no write the config?"), Some(Carries::Question));
        assert_eq!(classify("Stop-Process was denied by the classifier"), Some(Carries::Risk));
        assert_eq!(classify("Tests: 743/0/18"), Some(Carries::Number));
        assert_eq!(classify("## Findings"), Some(Carries::Heading));
        assert_eq!(classify("I read the module and the tests."), None);
        assert!(has_sha("at e847c7e on main") && !has_sha("the deadbeef word") && !has_sha("1234567"));
    }

    #[test]
    fn a_short_report_is_whole_and_a_long_one_keeps_its_facts_in_order() {
        assert_eq!(digest("Done. PR #1.", 700), "Done. PR #1.");
        let filler = "I looked at the surrounding code to understand it and nothing here matters for the next step. ".repeat(30);
        let report = format!("## Summary\n{filler}\nPR #95 merged as 1bc6def.\n{filler}\nTests: 760/0/18.\nQ-QU1: who files it?\nThe kill was denied.\n");
        let d = digest(&report, 400);
        assert!(d.len() < 520, "{}", d.len());
        let at = |s: &str| d.find(s).unwrap_or_else(|| panic!("{s}: {d}"));
        assert!(at("PR #95") < at("760/0/18") && at("760/0/18") < at("Q-QU1") && at("Q-QU1") < at("denied"));
        assert!(d.contains("left out; the full report is one recall away"));
    }

    #[test]
    fn the_opening_and_a_questions_section_come_first() {
        let filler = "Some context that the orchestrator does not need at all, repeated. ".repeat(40);
        let report = format!("Done: the lexer is rewritten.

{filler}
PR #7 merged as 1bc6def.

## Questions for the user
- Keep the old flag (a) or drop it (b)
");
        let d = digest(&report, 110);
        assert!(d.contains("Done: the lexer is rewritten.") && d.contains("Keep the old flag"), "{d}");
        assert!(!d.contains("PR #7"), "{d}");
    }

    #[test]
    fn budgets_scale_within_bounds() {
        assert_eq!(budget(300), WHOLE_UP_TO);
        assert_eq!(budget(30_000), WHOLE_UP_TO);
        assert_eq!(budget(200_000), 25_000);
    }
}
