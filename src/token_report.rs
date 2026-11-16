//! The text of `atlas --token-report`: pure functions from the report's
//! structs to a `String`, so the layout is tested without a store.
//!
//! [`human`] is the default: titled blocks, each with a one-line
//! explanation, aligned columns and compact numbers ([`compact`]).
//! [`tsv`] is `--token-report --tsv`: the tab-separated lines the report
//! printed before the human layout, byte for byte, for anything that reads
//! them by machine. Neither computes a figure: both print what
//! [`crate::tokens`], [`crate::baseline`] and [`crate::compact`] measured.
//!
//! Two rows the human layout does not list as they are. The bare `hook`
//! row ([`DOUBLE_LOGGED`]) logs the hooks' answers a second time beside
//! their `hook:<event>` rows, so it is left out of the tables and the day
//! totals, with one line saying so. `hook:PostToolUse` ([`ROUTED`]) is
//! mostly routed digests whose bytes the `route:*` rows log again, so it is
//! shown as routing's net saving (the outputs replaced minus all it
//! returned), not as cost. `--tsv` lists both as before.

use crate::baseline::{AtlasSide, Baseline};
use crate::compact::Totals;
use crate::tokens::{BriefRow, InjectRow, MatchRow, ReportRow, SelectRow};
use std::fmt::Write as _;

/// Everything one token report prints.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The bytes one token is counted as.
    pub bytes_per_token: f64,
    /// The latest calibration and when it was taken, or `None` before the
    /// first one.
    pub baseline: Option<(&'a str, &'a Baseline)>,
    /// The atlas side of the ratios.
    pub side: &'a AtlasSide,
    /// The configured targets' fingerprint
    /// ([`crate::calibrate::Plan::fingerprint`]), when there is a plan: a
    /// baseline measured against other targets is stale.
    pub fingerprint: Option<&'a str>,
    /// Totals by tool, ordered by tool.
    pub by_tool: &'a [ReportRow],
    /// Totals by day, oldest first.
    pub by_day: &'a [ReportRow],
    /// Totals by day without the [`NOT_IN_DAYS`] rows, oldest first: the
    /// human report's day table ([`crate::tokens::report_days_without`]).
    pub by_day_net: &'a [ReportRow],
    /// The "brief without atlas" totals.
    pub brief: Option<&'a BriefRow>,
    /// The recall hooks' injection totals.
    pub inject: Option<&'a InjectRow>,
    /// The brief against the injections on the agents both tables hold.
    pub matched: Option<&'a MatchRow>,
    /// What context selection did, by outcome.
    pub select: &'a [SelectRow],
    /// The compaction totals.
    pub compact: Option<&'a Totals>,
    /// The hook injections judged and, of them, hits: only hits are
    /// credited (#217). `None` when no memory store was there to judge
    /// them against, and then no hook row is credited.
    pub hook_hits: Option<(usize, usize)>,
    /// What atlas itself cost (#227), when it was measured: a net block
    /// after the headline and a cost section at the end. The lines before
    /// it are the same with or without it.
    pub cost: Option<&'a crate::token_cost::Cost>,
}

/// The line on how hook rows are credited: by hit, with the judge's count.
fn hook_line(r: &Inputs) -> String {
    match r.hook_hits {
        Some((injected, hits)) => format!(
            "{hits} of {injected} injections are hits ({:.1}%); only hits are credited, judged against memory as it is at report time",
            if injected > 0 { hits as f64 / injected as f64 * 100.0 } else { 0.0 }
        ),
        None => "no memory store to judge injections against: hook rows credited nothing".to_string(),
    }
}

/// A number in at most three significant figures with a unit: `999`,
/// `1k`, `9.41k`, `504k`, `2.46M`, `588M`, `1.2B`. Below 1000 (after
/// rounding) it is the whole number; trailing zeros after the point are
/// dropped, and a value that rounds up to 1000 of a unit is shown in the
/// next one (999_999 is `1M`).
pub fn compact(v: f64) -> String {
    const UNITS: [&str; 4] = ["k", "M", "B", "T"];
    let a = v.abs();
    let sign = if v < 0.0 && a.round() > 0.0 { "-" } else { "" };
    if a.round() < 1000.0 {
        return format!("{sign}{a:.0}");
    }
    let mut s = a;
    for (i, unit) in UNITS.iter().enumerate() {
        s /= 1000.0;
        let dec = if s < 10.0 {
            2
        } else if s < 100.0 {
            1
        } else {
            0
        };
        let txt = format!("{s:.dec$}");
        if txt.parse::<f64>().unwrap_or(s) >= 1000.0 && i + 1 < UNITS.len() {
            continue;
        }
        let txt = if txt.contains('.') { txt.trim_end_matches('0').trim_end_matches('.') } else { &txt };
        return format!("{sign}{txt}{unit}");
    }
    unreachable!("the last unit always returns")
}

/// How the columns of a [`table`] line up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Padded on the right.
    Left,
    /// Padded on the left.
    Right,
}

/// An aligned table, indented two spaces, columns two spaces apart, every
/// cell padded to its column's widest (the header included). No trailing
/// spaces.
pub fn table(header: &[&str], align: &[Align], rows: &[Vec<String>]) -> String {
    let mut width: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (w, c) in width.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    let mut out = String::new();
    let head: Vec<String> = header.iter().map(|h| h.to_string()).collect();
    for r in std::iter::once(&head).chain(rows) {
        let mut line = String::from(" ");
        for ((c, w), a) in r.iter().zip(&width).zip(align) {
            let pad = w - c.chars().count();
            line.push(' ');
            match a {
                Align::Left => {
                    line.push_str(c);
                    line.push_str(&" ".repeat(pad));
                }
                Align::Right => {
                    line.push_str(&" ".repeat(pad));
                    line.push_str(c);
                }
            }
            line.push(' ');
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// A titled key/value block: the title, a one-line explanation, then one
/// item per line with the keys padded to the widest.
pub fn block(title: &str, explain: &str, items: &[(&str, String)]) -> String {
    let mut out = format!("{title}\n  {explain}\n");
    let w = items.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    for (k, v) in items {
        let _ = writeln!(out, "  {k:<w$}  {v}");
    }
    out
}

/// Why a baseline is stale, or `None`; with `newer` set when a newer atlas
/// measured it (its rules are later than this binary's).
fn staleness(b: &Baseline, fingerprint: Option<&str>) -> (Option<String>, bool) {
    let stale = match (b.rules, fingerprint) {
        (None, _) => Some("it has no rules version".to_string()),
        (Some(v), _) if v < crate::calibrate::RULES_VERSION => Some(format!("these lookup rules are v{}", crate::calibrate::RULES_VERSION)),
        // Newer rules than this binary's: a newer atlas measured it, and it stands.
        (Some(v), _) if v > crate::calibrate::RULES_VERSION => None,
        (_, Some(f)) if b.fingerprint.as_deref() != Some(f) => Some("the configured targets changed".to_string()),
        _ => None,
    };
    let newer = stale.is_none() && b.rules.is_some_and(|v| v > crate::calibrate::RULES_VERSION);
    (stale, newer)
}

/// What the baseline was measured from, as one phrase.
fn measured_from(b: &Baseline) -> String {
    let source = b.cutoff_source.as_ref().map(|s| format!(" ({s})")).unwrap_or_default();
    format!("{} pre-atlas lookup episodes in {} of {} transcripts before {}{source}", b.episodes, b.used, b.transcripts, b.before)
}

/// The lookup rules a baseline used, as one phrase.
fn rules(b: &Baseline) -> String {
    b.rules.map_or_else(|| "unversioned lookup rules".to_string(), |v| format!("lookup rules v{v}"))
}

/// The lookup tools' per-answer cap as the report applies it
/// ([`crate::tokens::lookup_cap`]): the latest baseline's p90 episode, or
/// the line that says the lookup tools are credited nothing.
fn cap_line(r: &Inputs) -> String {
    match r.baseline {
        Some((_, b)) if b.episodes > 0 && !b.too_few && b.p90 > 0.0 => {
            format!("each lookup answer credited at most {} bytes (the pre-atlas p90 episode)", b.p90.round())
        }
        _ => "no pre-atlas baseline: lookup credit not counted".to_string(),
    }
}

/// The measured-ratio lines of the TSV report and of `--calibrate`: the
/// ratio, or why there is none, then a stale or newer-atlas note.
pub fn measured_tsv(b: &Baseline, when: Option<&str>, side: &AtlasSide, fingerprint: Option<&str>) -> String {
    let mut out = String::new();
    let when = when.map(|w| format!(", calibrated {}", w.get(..16).unwrap_or(w))).unwrap_or_default();
    let rules = rules(b);
    let from = measured_from(b);
    if b.too_few {
        let _ = writeln!(
            out,
            "measured ratio per answer: none, too little pre-atlas history: {from}; needs {} episodes in {} transcripts ({rules}{when}). It runs again when the lookup rules or the configured targets change, or with atlas --calibrate; the ledger ratio below stands alone",
            crate::calibrate::MIN_EPISODES,
            crate::calibrate::MIN_TRANSCRIPTS
        );
    } else {
        let (med, mean) = side.measured(b);
        let _ = writeln!(
            out,
            "measured ratio per answer: {mean:.1}x (mean, a ratio of sums; median {med:.1}x, not additive): {from}, {rules}{when}, against {} atlas answers",
            side.returned.len()
        );
    }
    let (stale, newer) = staleness(b, fingerprint);
    if let Some(why) = stale {
        let _ = writeln!(out, "  stale ({why}): the server recalibrates in the background at its next start, or run atlas --calibrate");
    } else if newer {
        let _ = writeln!(
            out,
            "  measured by a newer atlas (lookup rules v{}, these are v{}): this one does not judge it or recalibrate over it",
            b.rules.unwrap_or_default(),
            crate::calibrate::RULES_VERSION
        );
    }
    out
}

/// The machine report (`--token-report --tsv`): the tab-separated lines,
/// exactly as the report printed them before the human layout.
pub fn tsv(r: &Inputs) -> String {
    let mut out = String::new();
    match r.baseline {
        Some((when, b)) => out.push_str(&measured_tsv(b, Some(when), r.side, r.fingerprint)),
        None => out.push_str("measured ratio per answer: not calibrated yet; the server calibrates in the background at its next start, or run atlas --calibrate\n"),
    }
    let _ =
        writeln!(out, "ledger ratio per answer (rule-based, not a bound; grep + read counterfactual / returned): {:.1}x", r.side.ledger());
    let _ = writeln!(out, "ledger median per answer (credit / returned): {:.2}x", r.side.ledger_median());
    let _ = writeln!(out, "lookup cap: {}", cap_line(r));
    let _ = writeln!(out, "injection credit: {}", hook_line(r));
    let _ = writeln!(
        out,
        "tokens at {} bytes per token; the counterfactual columns are the rule-based ledger, not a bound (grep output + files read, see atlas::tokens)",
        r.bytes_per_token
    );
    for (title, rows) in [("tool", r.by_tool), ("day", r.by_day)] {
        let _ = writeln!(out, "{title}\tanswers\treturned\tcounterfactual (ledger)\tledger ratio");
        for row in rows {
            let ratio = if row.returned_comparable > 0.0 { row.counterfactual / row.returned_comparable } else { 0.0 };
            let _ = writeln!(out, "{}\t{}\t{:.0}\t{:.0}\t{:.1}x", row.key, row.answers, row.returned, row.counterfactual, ratio);
        }
    }
    match r.brief {
        Some(b) => {
            let _ = writeln!(
                out,
                "brief without atlas (rulebook read in full once per agent, at its first start; a resume is not a second read; not in the ratios above)\tagents {}\ttokens {:.0}\tper agent {:.0}\tstarts {} ({} resumes)\tlatest {:.0}",
                b.agents,
                b.tokens,
                b.per_agent(),
                b.starts,
                b.resumes(),
                b.latest
            );
        }
        None => out.push_str("brief without atlas: no subagent starts logged\n"),
    }
    match r.inject {
        Some(i) => {
            let _ = writeln!(
                out,
                "hook injections (recall pointers per agent, a main session or a subagent; judge against the brief row above)\tkeys {}\tagents {}\ttokens {:.0}\tper agent {:.0}\tlargest agent {:.0}",
                i.keys,
                i.agents,
                i.tokens,
                i.per_agent(),
                i.max_agent
            );
        }
        None => out.push_str("hook injections: none logged\n"),
    }
    match r.matched {
        Some(m) => {
            let _ = writeln!(
                out,
                "brief vs injections (agents in both)\tagents {}\trulebook {:.0}\tinjected {:.0}\trulebook/injected {:.1}x",
                m.agents,
                m.rulebook,
                m.injected,
                m.ratio()
            );
        }
        None => out.push_str("brief vs injections: no agent in both\n"),
    }
    if r.select.is_empty() {
        out.push_str("context selection: none logged\n");
    } else {
        out.push_str("context selection (hook injections; tokens whole -> sent)");
        let (full, sent) = select_totals(r.select);
        for s in r.select {
            let _ = write!(out, "\t{} {} ({:.0} -> {:.0})", s.how, s.n, s.full, s.sent);
        }
        let _ = writeln!(out, "\tsent {sent:.0} of {full:.0} whole");
    }
    match r.compact {
        Some(c) => {
            let _ = writeln!(
                out,
                "compactions\tn {}\tratio pre/summary {:.1}x\tpre/post {:.1}x\trefs {}\tdangling {} ({:.1}%)\tchanged {}\tbare keys {} (missing {})\tpost-compaction repeats (legacy, unfiltered) {}\tslowest hook {:.0} ms",
                c.n,
                pre_summary(c),
                pre_post(c),
                c.refs,
                c.dangling,
                dangling_pct(c),
                c.changed,
                c.bare,
                c.bare_missing,
                c.misses,
                c.max_ms
            );
            // The first figure is the replay's rule, comparable with its
            // offline baseline; the second drops repeats of changed targets.
            let _ = writeln!(
                out,
                "post-compaction lookups\twithin {} calls: repeats {} of {} ({:.1}%), misses of unchanged targets {} ({:.1}%; a folder repeat after any shell command counts as changed)\tmisses uncapped {}\tchanged targets {}\tunstamped {} (shell {})",
                crate::compact::MISS_WINDOW,
                c.window_repeats,
                c.window_lookups,
                window_pct(c, c.window_repeats),
                c.window_misses,
                window_pct(c, c.window_misses),
                c.stamped_misses,
                c.changed_targets,
                c.unstamped,
                c.unstamped_shell
            );
        }
        None => out.push_str("compactions: none logged\n"),
    }
    if let Some(c) = r.cost {
        out.push_str(&crate::token_cost::tsv(c, r.by_tool, r.bytes_per_token));
    }
    out
}

/// Context selection's whole and sent tokens, summed over the outcomes.
fn select_totals(select: &[SelectRow]) -> (f64, f64) {
    select.iter().fold((0.0, 0.0), |(f, s), r| (f + r.full, s + r.sent))
}

/// Context before a compaction against its summary.
fn pre_summary(c: &Totals) -> f64 {
    if c.summary > 0.0 {
        c.pre / c.summary
    } else {
        0.0
    }
}

/// Context before a compaction against just after it.
fn pre_post(c: &Totals) -> f64 {
    if c.post > 0.0 {
        c.pre_with_post / c.post
    } else {
        0.0
    }
}

/// Dangling refs, in percent of refs.
fn dangling_pct(c: &Totals) -> f64 {
    if c.refs > 0 {
        c.dangling as f64 / c.refs as f64 * 100.0
    } else {
        0.0
    }
}

/// `n` in percent of the lookups within the window.
fn window_pct(c: &Totals, n: i64) -> f64 {
    if c.window_lookups > 0 {
        n as f64 / c.window_lookups as f64 * 100.0
    } else {
        0.0
    }
}

/// A ratio column: `--` with nothing to compare with.
fn ratio_cell(r: &ReportRow) -> String {
    if r.counterfactual > 0.0 && r.returned_comparable > 0.0 {
        format!("{:.1}x", r.counterfactual / r.returned_comparable)
    } else {
        "--".to_string()
    }
}

/// The rows of a tool or day table.
fn rows_of(rows: &[&ReportRow]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| {
            let cost = if r.counterfactual > 0.0 { compact(r.counterfactual) } else { "--".to_string() };
            vec![r.key.clone(), compact(r.answers as f64), compact(r.returned), cost, ratio_cell(r)]
        })
        .collect()
}

/// The ledger table columns.
const ALIGN: [Align; 5] = [Align::Left, Align::Right, Align::Right, Align::Right, Align::Right];

/// Rows sorted by tokens returned, most first, ties by key.
fn by_returned<'a>(rows: impl Iterator<Item = &'a ReportRow>) -> Vec<&'a ReportRow> {
    let mut v: Vec<&ReportRow> = rows.collect();
    v.sort_by(|a, b| b.returned.total_cmp(&a.returned).then_with(|| a.key.cmp(&b.key)));
    v
}

/// The headline block: the measured ratio (a ratio of sums) and what it was
/// measured against, the medians' ratio (not additive), the ledger ratio
/// and its per-answer median, the calibration date.
fn headline(r: &Inputs) -> String {
    fn ledger_line(r: &Inputs) -> String {
        format!("{:.1}x (rule-based, not a bound: grep + read counterfactual / returned)", r.side.ledger())
    }
    fn ledger_median_line(r: &Inputs) -> String {
        format!("{:.2}x per answer (median of credit / returned)", r.side.ledger_median())
    }
    let mut items: Vec<(&str, String)> = Vec::new();
    match r.baseline {
        None => items.push((
            "measured ratio",
            "not calibrated yet; the server calibrates in the background at its next start, or run atlas --calibrate".into(),
        )),
        Some((when, b)) => {
            if b.too_few {
                items.push(("measured ratio", "none: too little pre-atlas history".into()));
                items.push((
                    "measured against",
                    format!(
                        "{}; needs {} episodes in {} transcripts ({})",
                        measured_from(b),
                        crate::calibrate::MIN_EPISODES,
                        crate::calibrate::MIN_TRANSCRIPTS,
                        rules(b)
                    ),
                ));
                items.push((
                    "recalibrates",
                    "when the lookup rules or the configured targets change, or with atlas --calibrate; the ledger ratio below stands alone"
                        .into(),
                ));
            } else {
                let (med, mean) = r.side.measured(b);
                items.push((
                    "measured ratio",
                    format!(
                        "{mean:.1}x mean (ratio of sums: pre-atlas episode bytes / atlas answer bytes), over {} atlas answers",
                        r.side.returned.len()
                    ),
                ));
                items.push(("median ratio", format!("{med:.1}x (not additive)")));
                items.push(("measured against", format!("{}, {}", measured_from(b), rules(b))));
            }
            let (stale, newer) = staleness(b, r.fingerprint);
            if let Some(why) = stale {
                items.push((
                    "stale",
                    format!("{why}: the server recalibrates in the background at its next start, or run atlas --calibrate"),
                ));
            } else if newer {
                items.push((
                    "measured by",
                    format!(
                        "a newer atlas (lookup rules v{}, these are v{}): this one does not judge it or recalibrate over it",
                        b.rules.unwrap_or_default(),
                        crate::calibrate::RULES_VERSION
                    ),
                ));
            }
            items.push(("ledger ratio", ledger_line(r)));
            items.push(("ledger median", ledger_median_line(r)));
            items.push(("calibrated", when.get(..16).unwrap_or(when).replacen('T', " ", 1)));
        }
    }
    if r.baseline.is_none() {
        items.push(("ledger ratio", ledger_line(r)));
        items.push(("ledger median", ledger_median_line(r)));
    }
    items.push(("lookup cap", cap_line(r)));
    items.push(("injection credit", hook_line(r)));
    items.push(("tokens", format!("counted at {} bytes per token", r.bytes_per_token)));
    block("Headline", "Tokens an atlas answer saves against the same lookup done the pre-atlas way (grep, then read the files).", &items)
}

/// The bare `hook` row: the hooks' answers logged a second time, beside
/// their `hook:<event>` rows (nearly all of it 0 bytes or a copy of one of
/// those within seconds). The human report leaves it out.
pub const DOUBLE_LOGGED: &str = "hook";

/// The PostToolUse hook's row: mostly routed digests, whose bytes the
/// `route:*` rows log again. The human report shows it as routing's net
/// saving, not as cost.
pub const ROUTED: &str = "hook:PostToolUse";

/// The tools the human report's day table leaves out
/// ([`crate::tokens::report_days_without`]).
pub const NOT_IN_DAYS: [&str; 2] = [DOUBLE_LOGGED, ROUTED];

/// The routing block: the digests (`route:*`), the outputs they replaced,
/// what PostToolUse returned in all, and the net.
fn routing(r: &Inputs) -> Option<String> {
    let routes: Vec<&ReportRow> = r.by_tool.iter().filter(|t| t.key.starts_with("route:")).collect();
    let post = r.by_tool.iter().find(|t| t.key == ROUTED);
    if routes.is_empty() && post.is_none() {
        return None;
    }
    let answers: i64 = routes.iter().map(|t| t.answers).sum();
    let returned: f64 = routes.iter().map(|t| t.returned).sum();
    let replaced: f64 = routes.iter().map(|t| t.counterfactual).sum();
    let (post_n, post_tokens) = post.map_or((0, 0.0), |p| (p.answers, p.returned));
    let net = replaced - post_tokens;
    let ratio = if post_tokens > 0.0 { format!(" ({:.1}x)", replaced / post_tokens) } else { String::new() };
    Some(block(
        "Routing (net)",
        "Routed digests replace larger tool outputs; net = the outputs the route:* rows replaced minus all hook:PostToolUse returned (the digests are logged under both).",
        &[
            ("digests (route:*)", format!("{} answers, {} returned", compact(answers as f64), compact(returned))),
            ("outputs replaced", compact(replaced)),
            (ROUTED, format!("{} answers, {} returned; counted here once, not as cost", compact(post_n as f64), compact(post_tokens))),
            ("net saving", format!("{}{ratio}", compact(net))),
        ],
    ))
}

/// The human report, the default: titled blocks with aligned columns and
/// compact numbers, every figure of [`tsv`] kept but the double-logged
/// `hook` row ([`DOUBLE_LOGGED`], named in one line) and the PostToolUse row
/// ([`ROUTED`], shown as routing's net).
pub fn human(r: &Inputs) -> String {
    let mut out = headline(r);
    out.push('\n');
    if let Some(c) = r.cost {
        out.push_str(&crate::token_cost::net_block(c, r.by_tool, r.bytes_per_token));
        out.push('\n');
    }

    let shown = |t: &&ReportRow| t.key != DOUBLE_LOGGED && t.key != ROUTED;
    let head = ["tool", "answers", "returned", "would have cost", "ratio"];
    let lookups = by_returned(r.by_tool.iter().filter(shown).filter(|t| t.counterfactual > 0.0));
    out.push_str("Lookups (compared with pre-atlas grep + read)\n");
    out.push_str("  Tokens returned, against what grep output plus the files read would have cost; the ratio is that rule-based ledger, not a bound.\n");
    out.push_str(&table(&head, &ALIGN, &rows_of(&lookups)));
    out.push('\n');

    if let Some(b) = routing(r) {
        out.push_str(&b);
        out.push('\n');
    }

    let cost = by_returned(r.by_tool.iter().filter(shown).filter(|t| t.counterfactual <= 0.0));
    out.push_str("Cost only (no pre-atlas equivalent to compare with)\n");
    out.push_str("  Tokens these tools and hooks returned, with nothing to divide them by.\n");
    out.push_str(&table(&head, &ALIGN, &rows_of(&cost)));
    out.push_str(
        "  Write tools record things nothing did before atlas, so they have no comparison. Hooks listed here are unjudged cost.\n",
    );
    if let Some(h) = r.by_tool.iter().find(|t| t.key == DOUBLE_LOGGED) {
        let _ = writeln!(
            out,
            "  Left out: the bare hook row ({} answers, {} returned), a double log of the hook:<event> rows (mostly 0-byte copies).",
            compact(h.answers as f64),
            compact(h.returned)
        );
    }
    out.push('\n');

    let days: Vec<&ReportRow> = r.by_day_net.iter().collect();
    out.push_str("By day (newest last)\n");
    out.push_str("  Every answer that day but the bare hook and hook:PostToolUse rows (counted above); the ratio is the rule-based ledger, not a bound.\n");
    out.push_str(&table(&["day", "answers", "returned", "would have cost", "ratio"], &ALIGN, &rows_of(&days)));
    out.push('\n');

    let explain = "The rulebook read in full once per agent, at its first start; not in the ratios above.";
    match r.brief {
        Some(b) => out.push_str(&block(
            "Brief without atlas",
            explain,
            &[
                ("agents", b.agents.to_string()),
                ("tokens", compact(b.tokens)),
                ("per agent", compact(b.per_agent())),
                (
                    "subagent starts",
                    format!("{} ({} resumes: a resumed agent starts again, keeps its context and is counted once)", b.starts, b.resumes()),
                ),
                ("latest rulebook", compact(b.latest)),
            ],
        )),
        None => out.push_str(&block("Brief without atlas", explain, &[("subagent starts", "none logged".into())])),
    }
    out.push('\n');

    let explain = "What the recall hooks injected as pointers, per agent (a main session or a subagent); judge it against the brief above.";
    match r.inject {
        Some(i) => out.push_str(&block(
            "Hook injections",
            explain,
            &[
                ("keys", i.keys.to_string()),
                ("agents", i.agents.to_string()),
                ("tokens", compact(i.tokens)),
                ("per agent", compact(i.per_agent())),
                ("largest agent", compact(i.max_agent)),
            ],
        )),
        None => out.push_str(&block("Hook injections", explain, &[("keys", "none logged".into())])),
    }
    out.push('\n');

    let explain = "The agents in both tables: the rulebook each would have read against what the hooks injected into it.";
    match r.matched {
        Some(m) => out.push_str(&block(
            "Brief vs injections",
            explain,
            &[
                ("agents", m.agents.to_string()),
                ("rulebook", compact(m.rulebook)),
                ("injected", compact(m.injected)),
                ("rulebook per injected", format!("{:.1}x", m.ratio())),
            ],
        )),
        None => out.push_str(&block("Brief vs injections", explain, &[("agents", "none in both".into())])),
    }
    out.push('\n');

    out.push_str("Context selection\n");
    out.push_str("  What selection did to the hook injections: tokens whole, then as sent.\n");
    if r.select.is_empty() {
        out.push_str("  none logged\n");
    } else {
        let rows: Vec<Vec<String>> =
            r.select.iter().map(|s| vec![s.how.clone(), s.n.to_string(), compact(s.full), compact(s.sent)]).collect();
        out.push_str(&table(&["outcome", "records", "whole", "sent"], &[Align::Left, Align::Right, Align::Right, Align::Right], &rows));
        let (full, sent) = select_totals(r.select);
        let _ = writeln!(out, "  sent {} of {} whole", compact(sent), compact(full));
    }
    out.push('\n');

    let explain = "Context compactions as SessionStart(compact) saw them.";
    match r.compact {
        Some(c) => {
            out.push_str(&block(
                "Compactions",
                explain,
                &[
                    ("compactions", c.n.to_string()),
                    ("context before / summary", format!("{:.1}x", pre_summary(c))),
                    ("context before / after", format!("{:.1}x", pre_post(c))),
                    ("refs", c.refs.to_string()),
                    ("dangling", format!("{} ({:.1}%)", c.dangling, dangling_pct(c))),
                    ("changed", c.changed.to_string()),
                    ("bare keys", format!("{} ({} missing)", c.bare, c.bare_missing)),
                    ("repeats (legacy, unfiltered)", c.misses.to_string()),
                    ("slowest hook", format!("{:.0} ms", c.max_ms)),
                ],
            ));
            out.push('\n');
            out.push_str(&block(
                "Post-compaction lookups",
                &format!(
                    "Lookups within {} calls after a compaction; a folder repeat after any shell command counts as changed.",
                    crate::compact::MISS_WINDOW
                ),
                &[
                    ("lookups", c.window_lookups.to_string()),
                    ("repeats", format!("{} ({:.1}%)", c.window_repeats, window_pct(c, c.window_repeats))),
                    ("misses of unchanged targets", format!("{} ({:.1}%)", c.window_misses, window_pct(c, c.window_misses))),
                    ("misses uncapped", c.stamped_misses.to_string()),
                    ("changed targets", c.changed_targets.to_string()),
                    ("unstamped", format!("{} (shell {})", c.unstamped, c.unstamped_shell)),
                ],
            ));
        }
        None => out.push_str(&block("Compactions", explain, &[("compactions", "none logged".into())])),
    }
    if let Some(c) = r.cost {
        out.push('\n');
        out.push_str(&crate::token_cost::human(c, r.by_tool, r.bytes_per_token));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, answers: i64, returned: f64, cf: f64) -> ReportRow {
        ReportRow { key: key.into(), answers, returned, counterfactual: cf, returned_comparable: if cf > 0.0 { returned } else { 0.0 } }
    }

    struct Fixture {
        baseline: Baseline,
        side: AtlasSide,
        tools: Vec<ReportRow>,
        days: Vec<ReportRow>,
        days_net: Vec<ReportRow>,
        brief: BriefRow,
        inject: InjectRow,
        matched: MatchRow,
        select: Vec<SelectRow>,
        compact: Totals,
    }

    fn fixture() -> Fixture {
        Fixture {
            baseline: Baseline {
                before: "2026-10-08T19:20:26.776Z".into(),
                transcripts: 66,
                used: 44,
                episodes: 96,
                mean: 4000.0,
                median: 2000.0,
                p90: 9000.0,
                total: 384_000,
                rules: Some(crate::calibrate::RULES_VERSION),
                fingerprint: Some("fp".into()),
                cutoff_source: Some("the first atlas call in the transcripts".into()),
                skips: None,
                too_few: false,
            },
            side: AtlasSide {
                returned: vec![100.0, 200.0, 300.0],
                counterfactual: 6000.0,
                returned_comparable: 600.0,
                ratios: vec![5.0, 10.0, 20.0],
            },
            tools: vec![
                row("handoff", 193, 3656.0, 0.0),
                row("hook", 22145, 504_160.0, 0.0),
                row("hook:PostToolUse", 2835, 787_384.0, 0.0),
                row("journal", 2346, 30135.0, 0.0),
                row("recall", 4416, 2_459_544.0, 15_242_874.0),
                row("route:Bash", 1635, 336_130.0, 29_903_335.0),
                row("route:Read", 984, 231_600.0, 5_690_908.0),
                row("search", 670, 243_874.0, 8_337_138.0),
                row("symbol", 7, 989.0, 4_430_299.0),
            ],
            days: vec![row("2026-11-13", 3438, 1_116_513.0, 17_844_963.0), row("2026-11-14", 12, 900.0, 0.0)],
            days_net: vec![row("2026-11-13", 1000, 300_000.0, 17_844_963.0), row("2026-11-14", 12, 900.0, 0.0)],
            brief: BriefRow { agents: 1300, starts: 3203, tokens: 240_000_000.0, latest: 189_770.0 },
            inject: InjectRow { keys: 13828, agents: 1311, tokens: 2_499_940.0, max_agent: 200_740.0 },
            matched: MatchRow { agents: 1250, rulebook: 230_000_000.0, injected: 2_000_000.0 },
            select: vec![
                SelectRow { how: "collapse".into(), n: 51, sent: 804.0, full: 1376.0 },
                SelectRow { how: "trim".into(), n: 8813, sent: 2_276_041.0, full: 16_461_616.0 },
            ],
            compact: Totals {
                n: 57,
                pre: 2120.0,
                post: 10.0,
                pre_with_post: 109.0,
                summary: 10.0,
                refs: 10,
                changed: 0,
                dangling: 1,
                bare: 111,
                bare_missing: 4,
                misses: 537,
                window_lookups: 14,
                window_repeats: 3,
                window_misses: 1,
                stamped_misses: 1,
                changed_targets: 0,
                unstamped: 2,
                unstamped_shell: 0,
                max_ms: 40.0,
            },
        }
    }

    fn inputs(f: &Fixture) -> Inputs<'_> {
        Inputs {
            bytes_per_token: 4.0,
            baseline: Some(("2026-11-14T02:17:55Z", &f.baseline)),
            side: &f.side,
            fingerprint: Some("fp"),
            by_tool: &f.tools,
            by_day: &f.days,
            by_day_net: &f.days_net,
            brief: Some(&f.brief),
            inject: Some(&f.inject),
            matched: Some(&f.matched),
            select: &f.select,
            compact: Some(&f.compact),
            hook_hits: Some((10, 3)),
            cost: None,
        }
    }

    #[test]
    fn compact_numbers_at_the_boundaries() {
        assert_eq!(compact(0.0), "0");
        assert_eq!(compact(999.0), "999");
        assert_eq!(compact(999.4), "999");
        assert_eq!(compact(999.6), "1k");
        assert_eq!(compact(1000.0), "1k");
        assert_eq!(compact(9407.0), "9.41k");
        assert_eq!(compact(9400.0), "9.4k");
        assert_eq!(compact(504_160.0), "504k");
        assert_eq!(compact(999_999.0), "1M");
        assert_eq!(compact(1_000_000.0), "1M");
        assert_eq!(compact(2_459_544.0), "2.46M");
        assert_eq!(compact(588_231_250.0), "588M");
        assert_eq!(compact(1_200_000_000.0), "1.2B");
        assert_eq!(compact(-1500.0), "-1.5k");
        assert_eq!(compact(-0.2), "0");
    }

    #[test]
    fn table_columns_line_up() {
        let t = table(
            &["tool", "n", "ratio"],
            &[Align::Left, Align::Right, Align::Right],
            &[vec!["a".into(), "1".into(), "2.0x".into()], vec!["longer".into(), "1000".into(), "--".into()]],
        );
        assert_eq!(t, "  tool       n  ratio\n  a          1   2.0x\n  longer  1000     --\n");
        // Every line's right-aligned column ends at the same place.
        let ends: Vec<usize> = t.lines().map(|l| l.len()).collect();
        assert!(ends.iter().all(|&e| e == ends[0]), "{t}");
        assert!(!t.contains('\t'));
    }

    #[test]
    fn zero_counterfactual_rows_show_two_hyphens_never_zero() {
        let f = fixture();
        let h = human(&inputs(&f));
        let cost = h.split("Cost only").nth(1).unwrap().split("By day").next().unwrap();
        assert!(!cost.contains("0.0x"), "{cost}");
        assert!(
            cost.lines().any(|l| l.trim_start().starts_with("journal ") && l.split_whitespace().rev().take(2).all(|c| c == "--")),
            "{cost}"
        );
        // The cost-only rows are sorted by returned, most first.
        let order: Vec<&str> = cost
            .lines()
            .skip(3)
            .filter_map(|l| l.split_whitespace().next())
            .filter(|w| w.chars().all(|c| c.is_ascii_lowercase()))
            .collect();
        assert_eq!(order, ["journal", "handoff"]);
        // The lookups: only rows with a counterfactual, by returned.
        let look = h.split("Lookups (").nth(1).unwrap().split("Routing").next().unwrap();
        let order: Vec<&str> = look.lines().skip(3).filter_map(|l| l.split_whitespace().next()).collect();
        assert_eq!(order, ["recall", "route:Bash", "search", "route:Read", "symbol"]);
        // A day with no comparison shows "--" too.
        let day = h.lines().find(|l| l.starts_with("  2026-11-14")).unwrap();
        assert!(day.trim_end().ends_with("--") && !day.contains("0.0x"), "{day}");
    }

    /// #209: the headline is the measured mean (a ratio of sums), the
    /// median is a secondary line marked not additive, the per-answer ledger
    /// median stands beside them, and the ledger ratio is never called a
    /// bound.
    #[test]
    fn the_headline_is_the_ratio_of_sums_and_the_median_is_not_additive() {
        let f = fixture();
        let (h, t) = (human(&inputs(&f)), tsv(&inputs(&f)));
        assert!(
            h.contains(
                "  measured ratio    20.0x mean (ratio of sums: pre-atlas episode bytes / atlas answer bytes), over 3 atlas answers"
            ),
            "{h}"
        );
        assert!(h.contains("  median ratio      10.0x (not additive)"), "{h}");
        assert!(h.contains("  ledger median     "), "{h}");
        assert!(h.contains("  ledger ratio      10.0x (rule-based, not a bound: grep + read counterfactual / returned)"), "{h}");
        assert!(t.starts_with("measured ratio per answer: 20.0x (mean, a ratio of sums; median 10.0x, not additive)"), "{t}");
        for out in [&h, &t] {
            assert!(!out.contains("upper bound") && !out.contains("upper-bound"), "{out}");
        }
    }

    #[test]
    fn the_bare_hook_row_is_left_out_and_named() {
        let f = fixture();
        let h = human(&inputs(&f));
        // In no table: no line starts with the bare key.
        assert!(!h.lines().any(|l| l.split_whitespace().next() == Some("hook")), "{h}");
        assert!(h.contains("  Left out: the bare hook row (22.1k answers, 504k returned), a double log"), "{h}");
        // The day table is the one without it (by_day_net), not by_day.
        let day = h.lines().find(|l| l.starts_with("  2026-11-13")).unwrap();
        assert!(day.contains(" 1k ") && day.contains(" 300k "), "{day}");
        assert!(!h.contains("1.12M"), "{h}");
        // Without a bare hook row there is no "Left out" line.
        let mut g = fixture();
        g.tools.retain(|t| t.key != DOUBLE_LOGGED);
        assert!(!human(&inputs(&g)).contains("Left out"));
    }

    #[test]
    fn post_tool_use_is_routing_net_not_cost() {
        let f = fixture();
        let h = human(&inputs(&f));
        let cost = h.split("Cost only").nth(1).unwrap().split("By day").next().unwrap();
        assert!(!cost.contains("hook:PostToolUse"), "{cost}");
        let routing = h.split("Routing (net)\n").nth(1).unwrap().split("\n\n").next().unwrap();
        // Replaced: 29_903_335 + 5_690_908 = 35_594_243; net of all
        // PostToolUse returned (787_384): 34_806_859, 45.2x.
        assert!(routing.contains("digests (route:*)  2.62k answers, 568k returned"), "{routing}");
        assert!(routing.contains("outputs replaced   35.6M"), "{routing}");
        assert!(routing.contains("hook:PostToolUse   2.83k answers, 787k returned; counted here once, not as cost"), "{routing}");
        assert!(routing.contains("net saving         34.8M (45.2x)"), "{routing}");
        // No routing at all: no block.
        let mut g = fixture();
        g.tools.retain(|t| t.key != ROUTED && !t.key.starts_with("route:"));
        assert!(!human(&inputs(&g)).contains("Routing"));
    }

    /// The report's format before the human layout, written out by hand:
    /// `--tsv` must reproduce it byte for byte.
    #[test]
    fn tsv_is_the_old_format_exactly() {
        let f = fixture();
        let want = "measured ratio per answer: 20.0x (mean, a ratio of sums; median 10.0x, not additive): 96 pre-atlas lookup episodes in 44 of 66 transcripts before 2026-10-08T19:20:26.776Z (the first atlas call in the transcripts), lookup rules v3, calibrated 2026-11-14T02:17, against 3 atlas answers
ledger ratio per answer (rule-based, not a bound; grep + read counterfactual / returned): 10.0x
ledger median per answer (credit / returned): 10.00x
lookup cap: each lookup answer credited at most 9000 bytes (the pre-atlas p90 episode)
injection credit: 3 of 10 injections are hits (30.0%); only hits are credited, judged against memory as it is at report time
tokens at 4 bytes per token; the counterfactual columns are the rule-based ledger, not a bound (grep output + files read, see atlas::tokens)
tool\tanswers\treturned\tcounterfactual (ledger)\tledger ratio
handoff\t193\t3656\t0\t0.0x
hook\t22145\t504160\t0\t0.0x
hook:PostToolUse\t2835\t787384\t0\t0.0x
journal\t2346\t30135\t0\t0.0x
recall\t4416\t2459544\t15242874\t6.2x
route:Bash\t1635\t336130\t29903335\t89.0x
route:Read\t984\t231600\t5690908\t24.6x
search\t670\t243874\t8337138\t34.2x
symbol\t7\t989\t4430299\t4479.6x
day\tanswers\treturned\tcounterfactual (ledger)\tledger ratio
2026-11-13\t3438\t1116513\t17844963\t16.0x
2026-11-14\t12\t900\t0\t0.0x
brief without atlas (rulebook read in full once per agent, at its first start; a resume is not a second read; not in the ratios above)\tagents 1300\ttokens 240000000\tper agent 184615\tstarts 3203 (1903 resumes)\tlatest 189770
hook injections (recall pointers per agent, a main session or a subagent; judge against the brief row above)\tkeys 13828\tagents 1311\ttokens 2499940\tper agent 1907\tlargest agent 200740
brief vs injections (agents in both)\tagents 1250\trulebook 230000000\tinjected 2000000\trulebook/injected 115.0x
context selection (hook injections; tokens whole -> sent)\tcollapse 51 (1376 -> 804)\ttrim 8813 (16461616 -> 2276041)\tsent 2276845 of 16462992 whole
compactions\tn 57\tratio pre/summary 212.0x\tpre/post 10.9x\trefs 10\tdangling 1 (10.0%)\tchanged 0\tbare keys 111 (missing 4)\tpost-compaction repeats (legacy, unfiltered) 537\tslowest hook 40 ms
post-compaction lookups\twithin 40 calls: repeats 3 of 14 (21.4%), misses of unchanged targets 1 (7.1%; a folder repeat after any shell command counts as changed)\tmisses uncapped 1\tchanged targets 0\tunstamped 2 (shell 0)
";
        assert_eq!(tsv(&inputs(&f)), want);
        // And the empty store's lines.
        let side = AtlasSide { returned: vec![], counterfactual: 0.0, returned_comparable: 0.0, ratios: vec![] };
        let empty = Inputs {
            baseline: None,
            side: &side,
            fingerprint: None,
            by_tool: &[],
            by_day: &[],
            by_day_net: &[],
            brief: None,
            inject: None,
            matched: None,
            select: &[],
            compact: None,
            hook_hits: None,
            cost: None,
            bytes_per_token: 4.0,
        };
        assert_eq!(
            tsv(&empty),
            "measured ratio per answer: not calibrated yet; the server calibrates in the background at its next start, or run atlas --calibrate
ledger ratio per answer (rule-based, not a bound; grep + read counterfactual / returned): 0.0x
ledger median per answer (credit / returned): 0.00x
lookup cap: no pre-atlas baseline: lookup credit not counted
injection credit: no memory store to judge injections against: hook rows credited nothing
tokens at 4 bytes per token; the counterfactual columns are the rule-based ledger, not a bound (grep output + files read, see atlas::tokens)
tool\tanswers\treturned\tcounterfactual (ledger)\tledger ratio
day\tanswers\treturned\tcounterfactual (ledger)\tledger ratio
brief without atlas: no subagent starts logged
hook injections: none logged
brief vs injections: no agent in both
context selection: none logged
compactions: none logged
"
        );
    }

    #[test]
    fn stale_and_newer_notes() {
        let mut f = fixture();
        f.baseline.fingerprint = Some("other".into());
        let i = inputs(&f);
        assert!(tsv(&i).contains("  stale (the configured targets changed): the server recalibrates"));
        assert!(human(&i).contains("  stale             the configured targets changed: the server recalibrates"), "{}", human(&i));
        f.baseline.rules = Some(crate::calibrate::RULES_VERSION + 1);
        let i = inputs(&f);
        assert!(tsv(&i).contains("  measured by a newer atlas"));
        assert!(human(&i).contains("measured by       a newer atlas"));
    }

    #[test]
    fn a_full_render_snapshot() {
        let f = fixture();
        let want = SNAPSHOT;
        let got = human(&inputs(&f));
        assert_eq!(got, want, "\n{got}");
        assert!(!got.contains('\t'));
    }

    const SNAPSHOT: &str = "Headline
  Tokens an atlas answer saves against the same lookup done the pre-atlas way (grep, then read the files).
  measured ratio    20.0x mean (ratio of sums: pre-atlas episode bytes / atlas answer bytes), over 3 atlas answers
  median ratio      10.0x (not additive)
  measured against  96 pre-atlas lookup episodes in 44 of 66 transcripts before 2026-10-08T19:20:26.776Z (the first atlas call in the transcripts), lookup rules v3
  ledger ratio      10.0x (rule-based, not a bound: grep + read counterfactual / returned)
  ledger median     10.00x per answer (median of credit / returned)
  calibrated        2026-11-14 02:17
  lookup cap        each lookup answer credited at most 9000 bytes (the pre-atlas p90 episode)
  injection credit  3 of 10 injections are hits (30.0%); only hits are credited, judged against memory as it is at report time
  tokens            counted at 4 bytes per token

Lookups (compared with pre-atlas grep + read)
  Tokens returned, against what grep output plus the files read would have cost; the ratio is that rule-based ledger, not a bound.
  tool        answers  returned  would have cost    ratio
  recall        4.42k     2.46M            15.2M     6.2x
  route:Bash    1.64k      336k            29.9M    89.0x
  search          670      244k            8.34M    34.2x
  route:Read      984      232k            5.69M    24.6x
  symbol            7       989            4.43M  4479.6x

Routing (net)
  Routed digests replace larger tool outputs; net = the outputs the route:* rows replaced minus all hook:PostToolUse returned (the digests are logged under both).
  digests (route:*)  2.62k answers, 568k returned
  outputs replaced   35.6M
  hook:PostToolUse   2.83k answers, 787k returned; counted here once, not as cost
  net saving         34.8M (45.2x)

Cost only (no pre-atlas equivalent to compare with)
  Tokens these tools and hooks returned, with nothing to divide them by.
  tool     answers  returned  would have cost  ratio
  journal    2.35k     30.1k               --     --
  handoff      193     3.66k               --     --
  Write tools record things nothing did before atlas, so they have no comparison. Hooks listed here are unjudged cost.
  Left out: the bare hook row (22.1k answers, 504k returned), a double log of the hook:<event> rows (mostly 0-byte copies).

By day (newest last)
  Every answer that day but the bare hook and hook:PostToolUse rows (counted above); the ratio is the rule-based ledger, not a bound.
  day         answers  returned  would have cost  ratio
  2026-11-13       1k      300k            17.8M  59.5x
  2026-11-14       12       900               --     --

Brief without atlas
  The rulebook read in full once per agent, at its first start; not in the ratios above.
  agents           1300
  tokens           240M
  per agent        185k
  subagent starts  3203 (1903 resumes: a resumed agent starts again, keeps its context and is counted once)
  latest rulebook  190k

Hook injections
  What the recall hooks injected as pointers, per agent (a main session or a subagent); judge it against the brief above.
  keys           13828
  agents         1311
  tokens         2.5M
  per agent      1.91k
  largest agent  201k

Brief vs injections
  The agents in both tables: the rulebook each would have read against what the hooks injected into it.
  agents                 1250
  rulebook               230M
  injected               2M
  rulebook per injected  115.0x

Context selection
  What selection did to the hook injections: tokens whole, then as sent.
  outcome   records  whole   sent
  collapse       51  1.38k    804
  trim         8813  16.5M  2.28M
  sent 2.28M of 16.5M whole

Compactions
  Context compactions as SessionStart(compact) saw them.
  compactions                   57
  context before / summary      212.0x
  context before / after        10.9x
  refs                          10
  dangling                      1 (10.0%)
  changed                       0
  bare keys                     111 (4 missing)
  repeats (legacy, unfiltered)  537
  slowest hook                  40 ms

Post-compaction lookups
  Lookups within 40 calls after a compaction; a folder repeat after any shell command counts as changed.
  lookups                      14
  repeats                      3 (21.4%)
  misses of unchanged targets  1 (7.1%)
  misses uncapped              1
  changed targets              0
  unstamped                    2 (shell 0)
";
}
