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
use crate::tokens::{BriefMeasure, BriefRow, InjectRow, MatchRow, ReportRow, SelectRow};
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
    /// The `brief_log` totals: the agents, and the "if read whole" sum.
    pub brief: Option<&'a BriefRow>,
    /// The rulebook as read before atlas, per agent (#219), or `None`
    /// when not measured.
    pub measured: Option<&'a BriefMeasure>,
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
    /// How the `route:Read` rows were credited (E4): judged against later
    /// Reads of the same file, or 0 for rows logged before judging. Its
    /// credit is already in the rows above; this only says how.
    pub read_credit: Option<&'a crate::tokens::ReadCredit>,
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
            let measured = match r.measured {
                Some(m) => format!(
                    "tokens {:.0}\tper agent {:.0}\tmeasured from {}",
                    m.per_agent * b.agents as f64,
                    m.per_agent,
                    measured_reads(m)
                ),
                None => "tokens not measured".to_string(),
            };
            let _ = writeln!(
                out,
                "brief without atlas (the rulebook as read before atlas, measured per agent over the pre-atlas subagent transcripts; once per agent, a resume is not a second read; not in the ratios above)\tagents {}\t{measured}\tif read whole {:.0} (per agent {:.0})\tstarts {} ({} resumes)\tlatest {:.0}",
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
            let s = |p: &crate::token_cost::Spread| format!("p50 {:.0} p95 {:.0} max {:.0}", p.p50, p.p95, p.max);
            let _ = writeln!(
                out,
                "hook injections (recall pointers; a main session per prompt, a subagent per agent)\tkeys {}\ttokens {:.0}\tmain sessions {}\tprompts {}\tper prompt {}\tsubagents {}\tper agent {}\tlargest {:.0} ({})",
                i.keys,
                i.tokens,
                i.sessions,
                i.prompts,
                s(&i.per_prompt),
                i.subagents,
                s(&i.per_agent),
                i.largest,
                if i.largest_main { "main session" } else { "subagent" }
            );
        }
        None => out.push_str("hook injections: none logged\n"),
    }
    match r.matched {
        Some(m) => {
            let rulebook = match (m.rulebook, m.ratio()) {
                (Some(rb), Some(x)) => format!("rulebook {rb:.0}\tinjected {:.0}\trulebook/injected {x:.1}x", m.injected),
                (Some(rb), None) => format!("rulebook {rb:.0}\tinjected {:.0}", m.injected),
                (None, _) => format!("rulebook not measured\tinjected {:.0}", m.injected),
            };
            let _ = writeln!(out, "brief vs injections (agents in both)\tagents {}\t{rulebook}\tif read whole {:.0}", m.agents, m.whole);
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
        out.push_str("where\tsource\tcredit\tcost\tnet\tagainst\n");
        for w in where_rows(r, c) {
            let credit = if w.shown() { format!("{:.0}", w.credit) } else { "--".into() };
            let _ = writeln!(out, "where\t{}\t{credit}\t{:.0}\t{:.0}\t{}", w.source, w.cost, w.net(), w.against);
        }
    }
    out
}

/// What the measured rulebook read was taken from, as one phrase.
fn measured_reads(m: &BriefMeasure) -> String {
    format!("{} reads in {} of {} pre-atlas subagent transcripts before {}", m.reads, m.readers, m.transcripts, m.cutoff)
}

/// One source of the "Where the savings live" block (journal 0069): its
/// credit, its cost and the yardstick the credit is measured against.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereRow {
    /// The source.
    pub source: &'static str,
    /// Credited savings, in tokens.
    pub credit: f64,
    /// Whether the source has a measured counterfactual: without one, and
    /// with no credit, the credit prints as `--`.
    pub measured: bool,
    /// Measured cost, in tokens.
    pub cost: f64,
    /// What the credit is measured against.
    pub against: String,
}

impl WhereRow {
    /// Credit minus cost.
    #[must_use]
    pub fn net(&self) -> f64 {
        self.credit - self.cost
    }

    /// Whether the credit is printed as a number, not `--`.
    #[must_use]
    pub fn shown(&self) -> bool {
        self.measured || self.credit != 0.0
    }
}

/// The rulebook's source in [`where_rows`].
pub const RULEBOOK_SOURCE: &str = "rulebook (subagent starts)";

/// The prompts' source in [`where_rows`].
pub const PROMPTS_SOURCE: &str = "prompts and tool calls (main sessions; PreToolUse in subagents too)";

/// The cost-only remainder's source in [`where_rows`].
pub const OTHER_SOURCE: &str = "writes and other";

/// The main-session hook events: what a main session's prompts and tool
/// calls are injected.
const PROMPT_HOOKS: [&str; 4] = ["hook:UserPromptSubmit", "hook:PreToolUse", "hook:SessionStart", "hook:PreCompact"];

/// Where the savings live (journal 0069): the net table's rows
/// ([`crate::token_cost::net_rows`]) by source, so the rows sum to Net's
/// credit, cost and net exactly, ordered by net, largest saving first.
///
/// * lookups: the [`crate::tokens::CAPPED`] tools but `read_note` (and
///   `recall`, by key and by query alike: `token_log` does not record
///   which), against grep + read capped at the pre-atlas p90 episode;
/// * routing: the `route:*` rows, against the tool output they replaced;
/// * the rulebook: the measured credit ([`crate::token_cost::RULEBOOK`])
///   and the `SubagentStart` injections into the agents in both the brief
///   and the injection tables; not measured, its credit is `--`;
/// * prompts and tool calls: [`PROMPT_HOOKS`], with no pre-atlas
///   equivalent (main sessions, and a subagent's `PreToolUse` too: hook
///   cost is kept by event, not by scope);
/// * memory and note reads: `read_note`, 1x by design;
/// * tool definitions (A), and every other row (writes, unjudged hooks, the
///   rest of `SubagentStart`): cost only.
///
/// A hook row's hit credit (#217) stays on its source's row, but no source
/// is credited twice: a measured rulebook replaces the `hook:SubagentStart`
/// hit credit ([`crate::token_cost::net_rows`] credits it 0), and without a
/// measurement that hit credit is the rulebook row's credit. The measured
/// rulebook is credited less the recall-by-key and by-lane credit
/// ([`crate::token_cost::Cost::rulebook_overlap`]); other lookups whose
/// counterfactual opens the rulebook (recall by query, search, section) are
/// not told apart in `token_log`, and may overlap the rest of the rulebook
/// row's credit, at most all of it: the block's header names that bound.
/// Whether the row is credited, and how it is labelled, both follow
/// `cost.rulebook`.
pub fn where_rows(r: &Inputs, cost: &crate::token_cost::Cost) -> Vec<WhereRow> {
    use crate::token_cost::{net_rows, DEFINITIONS, RULEBOOK, START_HOOK};
    // The label and the credit key on one thing: whether Net credits a
    // measured rulebook (`cost.rulebook`), not whether the brief was
    // measured (a measured brief with no agent in both tables credits none).
    let credited = cost.rulebook.is_some();
    let against_rulebook = match (credited, r.measured) {
        (true, Some(m)) => format!("the rulebook as read before atlas ({} transcripts)", m.transcripts),
        (true, None) => "the rulebook as read before atlas".to_string(),
        (false, _) => "not measured".to_string(),
    };
    let against_rulebook = match cost.rulebook_taken_off() {
        off if off > 0.0 => format!("{against_rulebook}, less {} of recall by key or lane (may read the same records)", compact(off)),
        _ => against_rulebook,
    };
    let mut rows = vec![
        WhereRow {
            source: "lookups",
            credit: 0.0,
            measured: true,
            cost: 0.0,
            against: "grep + read, capped at the pre-atlas p90 episode".into(),
        },
        WhereRow { source: "routing", credit: 0.0, measured: true, cost: 0.0, against: "the tool output it replaced".into() },
        WhereRow { source: RULEBOOK_SOURCE, credit: 0.0, measured: credited, cost: 0.0, against: against_rulebook },
        WhereRow { source: PROMPTS_SOURCE, credit: 0.0, measured: false, cost: 0.0, against: "no pre-atlas equivalent measured".into() },
        WhereRow {
            source: "memory and note reads",
            credit: 0.0,
            measured: true,
            cost: 0.0,
            against: "1x by design: the counterfactual is the same file".into(),
        },
        WhereRow { source: DEFINITIONS, credit: 0.0, measured: false, cost: 0.0, against: "cost only".into() },
        WhereRow {
            source: OTHER_SOURCE,
            credit: 0.0,
            measured: false,
            cost: 0.0,
            against: "cost only: writes, unjudged rows, SubagentStart framing and re-reads".into(),
        },
    ];
    let (lookups, routing, rulebook, prompts, memory, definitions, other) = (0, 1, 2, 3, 4, 5, 6);
    let start = r.matched.map_or(0.0, |m| m.start);
    for n in net_rows(cost, r.by_tool) {
        let key = n.key.as_str();
        let i = match key {
            k if k == DEFINITIONS => definitions,
            k if k == RULEBOOK => rulebook,
            k if k == START_HOOK => {
                // The injections into the agents both tables hold stand in
                // for the rulebook; the rest is unjudged.
                let to_rulebook = start.min(n.cost);
                rows[rulebook].credit += n.credit;
                rows[rulebook].cost += to_rulebook;
                rows[other].cost += n.cost - to_rulebook;
                continue;
            }
            k if PROMPT_HOOKS.contains(&k) => prompts,
            k if k.starts_with("route:") => routing,
            "read_note" => memory,
            k if crate::tokens::CAPPED.contains(&k) => lookups,
            _ => other,
        };
        rows[i].credit += n.credit;
        rows[i].cost += n.cost;
    }
    // A row with no yardstick of its own that carries hit credit (#217)
    // says where the credit comes from.
    if !credited && rows[rulebook].credit != 0.0 {
        rows[rulebook].against = "not measured: the SubagentStart hits, credited by record size".into();
    }
    if rows[prompts].credit != 0.0 {
        rows[prompts].against = "the hits, credited by record size; no pre-atlas equivalent measured".into();
    }
    if rows[other].credit != 0.0 {
        rows[other].against =
            "the hits, credited by record size; the rest cost only: writes, unjudged rows, SubagentStart framing and re-reads".into();
    }
    // Routed Reads are credited only as judged (E4): say so where the
    // credit is summed.
    if r.read_credit.is_some_and(|c| c.judged + c.unjudged + c.unplaced > 0) {
        rows[routing].against =
            "the tool output it replaced; route:Read only what was not read back later (rows before judging or without a place 0)".into();
    }
    rows.retain(|w| w.source == RULEBOOK_SOURCE || w.credit != 0.0 || w.cost != 0.0);
    rows.sort_by(|a, b| b.net().total_cmp(&a.net()).then(a.source.cmp(b.source)));
    rows
}

/// The header line of the "Where the savings live" block: how the rows sum,
/// the single-credit rule, and, with a measured rulebook, the bound on the
/// overlap the ledger cannot measure.
fn where_header(cost: &crate::token_cost::Cost) -> String {
    let mut h = String::from(
        "  Credit, cost and net per source, in tokens, largest saving first; each credit against the yardstick named. The rows sum to Net.",
    );
    match cost.rulebook_net_credit() {
        Some(rest) if rest <= 0.0 => {
            let _ = write!(
                h,
                " A measured rulebook replaces the SubagentStart hit credit, less {} of recall by key or lane, which may read the same records. That is at least its whole credit, so the rulebook row is credited 0 and cannot overlap other lookups.",
                compact(cost.rulebook_taken_off())
            );
        }
        Some(rest) => {
            let _ = write!(
                h,
                " A measured rulebook replaces the SubagentStart hit credit, less {} of recall by key or lane, which may read the same records. Other lookups that open the rulebook (recall by query, search, section) may still overlap it, by at most {} (the rulebook row's credit): the ledger records no agent or file per answer, so that overlap is not measured.",
                compact(cost.rulebook_taken_off()),
                compact(rest)
            );
        }
        None => h.push_str(" No rulebook is credited: the SubagentStart hits, if any, stand on its row."),
    }
    h.push('\n');
    h
}

/// The "Where the savings live" block of the human report.
fn where_block(r: &Inputs, cost: &crate::token_cost::Cost) -> String {
    let rows = where_rows(r, cost);
    let mut out = String::from("Where the savings live\n");
    out.push_str(&where_header(cost));
    let mut cells: Vec<Vec<String>> = rows
        .iter()
        .map(|w| {
            let credit = if w.shown() { compact(w.credit) } else { "--".into() };
            vec![w.source.to_string(), credit, compact(w.cost), compact(w.net()), w.against.clone()]
        })
        .collect();
    let (credit, c) = rows.iter().fold((0.0, 0.0), |(a, b), w| (a + w.credit, b + w.cost));
    cells.push(vec!["total".into(), compact(credit), compact(c), compact(credit - c), String::new()]);
    out.push_str(&table(
        &["source", "credit", "cost", "net", "against"],
        &[Align::Left, Align::Right, Align::Right, Align::Right, Align::Left],
        &cells,
    ));
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
    let mut items = vec![
        ("digests (route:*)", format!("{} answers, {} returned", compact(answers as f64), compact(returned))),
        ("outputs replaced", compact(replaced)),
    ];
    if let Some(line) = r.read_credit.and_then(|c| read_credit_line(c, r.bytes_per_token)) {
        items.push(("route:Read", line));
    }
    items.push((ROUTED, format!("{} answers, {} returned; counted here once, not as cost", compact(post_n as f64), compact(post_tokens))));
    items.push(("net saving", format!("{}{ratio}", compact(net))));
    Some(block(
        "Routing (net)",
        "Routed digests replace larger tool outputs; net = the outputs the route:* rows replaced minus all hook:PostToolUse returned (the digests are logged under both).",
        &items,
    ))
}

/// How the `route:Read` rows were credited (E4), or `None` with none: a
/// digest saves only what the agent does not read back, so each row is
/// credited the output it replaced less the later Reads of the same file in
/// the same transcript, and a row logged before that judgement 0.
pub fn read_credit_line(c: &crate::tokens::ReadCredit, bytes_per_token: f64) -> Option<String> {
    if c.judged + c.unjudged + c.unplaced == 0 {
        return None;
    }
    let t = |b: i64| compact(b as f64 / bytes_per_token);
    let mut s = format!(
        "credited {} = {} replaced less {} read back later (Reads of the same file in the same transcript), over {} judged rows",
        t(c.credit),
        t(c.replaced),
        t(c.reread),
        c.judged - c.missing
    );
    if c.missing > 0 {
        let _ = write!(s, "; {} rows whose transcript is gone credited 0", c.missing);
    }
    if c.unjudged > 0 {
        let _ = write!(s, "; {} rows logged before judging credited 0", c.unjudged);
    }
    if c.unplaced > 0 {
        let _ = write!(s, "; {} rows logged without a place credited 0", c.unplaced);
    }
    Some(s)
}

/// The human report, the default: titled blocks with aligned columns and
/// compact numbers, every figure of [`tsv`] kept but the double-logged
/// `hook` row ([`DOUBLE_LOGGED`], named in one line) and the PostToolUse row
/// ([`ROUTED`], shown as routing's net).
pub fn human(r: &Inputs) -> String {
    let mut out = headline(r);
    out.push('\n');
    if let Some(c) = r.cost {
        out.push_str(&where_block(r, c));
        out.push('\n');
        out.push_str(&crate::token_cost::net_block(c, r.by_tool, r.bytes_per_token));
        out.push('\n');
    }

    let shown = |t: &&ReportRow| t.key != DOUBLE_LOGGED && t.key != ROUTED;
    let head = ["tool", "answers", "returned", "would have cost", "ratio"];
    let lookups = by_returned(r.by_tool.iter().filter(shown).filter(|t| t.counterfactual > 0.0));
    out.push_str("Lookups (compared with pre-atlas grep + read)\n");
    out.push_str("  Tokens returned, against what grep output plus the files read would have cost; the ratio is that rule-based ledger, not a bound.\n");
    out.push_str(&table(&head, &ALIGN, &rows_of(&lookups)));
    if lookups.iter().any(|t| t.key == "read_note") {
        out.push_str(
            "  read_note is 1x by design: its counterfactual is the whole file it returns. section is the cheaper read for one heading.\n",
        );
    }
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

    let explain = "The rulebook as agents read it before atlas, measured per agent over the pre-atlas subagent transcripts, once per agent; not in the ratios above.";
    match r.brief {
        Some(b) => {
            let mut items: Vec<(&str, String)> = vec![("agents", b.agents.to_string())];
            match r.measured {
                Some(m) => {
                    items.push(("tokens", format!("{} (agents x per agent)", compact(m.per_agent * b.agents as f64))));
                    items.push(("per agent", format!("{} (measured: {})", compact(m.per_agent), measured_reads(m))));
                }
                None => items.push(("tokens", "not measured (no pre-atlas read of the rulebook in the calibration)".into())),
            }
            items.push((
                "if read whole",
                format!(
                    "{} ({} per agent: the rulebook's size once per agent, not a measurement)",
                    compact(b.tokens),
                    compact(b.per_agent())
                ),
            ));
            items.push((
                "subagent starts",
                format!("{} ({} resumes: a resumed agent starts again, keeps its context and is counted once)", b.starts, b.resumes()),
            ));
            items.push(("latest rulebook", compact(b.latest)));
            out.push_str(&block("Brief without atlas", explain, &items));
        }
        None => out.push_str(&block("Brief without atlas", explain, &[("subagent starts", "none logged".into())])),
    }
    out.push('\n');

    let explain =
        "What the recall hooks injected as pointers: a main session per prompt (it lives for many prompts), a subagent per agent.";
    match r.inject {
        Some(i) => {
            let s = |p: &crate::token_cost::Spread| format!("p50 {}, p95 {}, max {}", compact(p.p50), compact(p.p95), compact(p.max));
            out.push_str(&block(
                "Hook injections",
                explain,
                &[
                    ("keys", i.keys.to_string()),
                    ("tokens", compact(i.tokens)),
                    ("main sessions", format!("{} sessions, {} prompts; per prompt {}", i.sessions, i.prompts, s(&i.per_prompt))),
                    ("subagents", format!("{} agents; per agent {}", i.subagents, s(&i.per_agent))),
                    ("largest", format!("{} ({})", compact(i.largest), if i.largest_main { "a main session" } else { "a subagent" })),
                ],
            ));
        }
        None => out.push_str(&block("Hook injections", explain, &[("keys", "none logged".into())])),
    }
    out.push('\n');

    let explain = "The agents in both tables: the rulebook as read before atlas against what the hooks injected into them.";
    match r.matched {
        Some(m) => {
            let mut items: Vec<(&str, String)> = vec![("agents", m.agents.to_string())];
            match (m.rulebook, r.measured) {
                (Some(rb), Some(x)) => items
                    .push(("rulebook", format!("{} (measured: {} per agent, as read before atlas)", compact(rb), compact(x.per_agent)))),
                (Some(rb), None) => items.push(("rulebook", compact(rb))),
                (None, _) => items.push(("rulebook", "not measured".into())),
            }
            items.push(("injected", compact(m.injected)));
            if let Some(x) = m.ratio() {
                items.push(("rulebook per injected", format!("{x:.1}x")));
            }
            items.push(("if read whole", format!("{} (the rulebook's size once per agent; not a saving)", compact(m.whole))));
            out.push_str(&block("Brief vs injections", explain, &items));
        }
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
        measure: Option<BriefMeasure>,
        inject: InjectRow,
        matched: MatchRow,
        select: Vec<SelectRow>,
        compact: Totals,
        cost: Option<crate::token_cost::Cost>,
    }

    fn spread(n: usize, p50: f64, p95: f64, max: f64) -> crate::token_cost::Spread {
        crate::token_cost::Spread { n, mean: p50, p50, p95, max }
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
            measure: Some(BriefMeasure {
                per_agent: 1500.0,
                reads: 25,
                readers: 17,
                transcripts: 65,
                cutoff: "2026-10-08T19:20:26.776Z".into(),
            }),
            inject: InjectRow {
                keys: 13828,
                tokens: 2_499_940.0,
                sessions: 3,
                prompts: 900,
                per_prompt: spread(900, 38.0, 1960.0, 15_500.0),
                subagents: 1308,
                per_agent: spread(1308, 3650.0, 5770.0, 15_200.0),
                largest: 200_740.0,
                largest_main: true,
            },
            matched: MatchRow {
                agents: 1250,
                rulebook: Some(1_875_000.0),
                whole: 230_000_000.0,
                injected: 2_000_000.0,
                start: 1_800_000.0,
            },
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
            cost: None,
        }
    }

    /// The fixture with a measured cost side (#227) and the measured
    /// rulebook's net row: every transcript read, so the net is no floor.
    fn with_cost(mut f: Fixture) -> Fixture {
        let mut c = crate::token_cost::Cost { transcripts: true, with_transcript: 4, ..Default::default() };
        c.start_tokens = 100_000.0;
        c.sent.insert("SubagentStart".into(), 2_000_000.0);
        c.sent.insert("UserPromptSubmit".into(), 300_000.0);
        for t in f.tools.iter().filter(|t| !t.key.starts_with("hook")) {
            c.output.insert(t.key.clone(), t.returned);
        }
        c.rulebook = f.measure.as_ref().map(|m| m.per_agent * f.matched.agents as f64);
        f.cost = Some(c);
        f
    }

    /// The fixture as it reads when the brief could not be measured.
    fn not_measured(mut f: Fixture) -> Fixture {
        f.measure = None;
        f.matched.rulebook = None;
        if let Some(c) = f.cost.as_mut() {
            c.rulebook = None;
        }
        f
    }

    /// The block of the human report under a title, up to its blank line.
    fn block_of<'a>(h: &'a str, title: &str) -> &'a str {
        h.split(&format!("\n{title}\n")).nth(1).unwrap_or_else(|| panic!("no {title} block:\n{h}")).split("\n\n").next().unwrap()
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
            measured: f.measure.as_ref(),
            inject: Some(&f.inject),
            matched: Some(&f.matched),
            select: &f.select,
            compact: Some(&f.compact),
            hook_hits: Some((10, 3)),
            read_credit: None,
            cost: f.cost.as_ref(),
        }
    }

    /// E4: the routing block and the "Where the savings live" row say how
    /// route:Read was credited: judged rows less what was read back, and
    /// rows from before judging at 0. Without route:Read rows, no line.
    #[test]
    fn route_read_credit_is_labelled_as_judged() {
        let f = fixture();
        let mut c = crate::tokens::ReadCredit {
            judged: 30,
            unjudged: 1000,
            unplaced: 3,
            missing: 2,
            replaced: 400_000,
            reread: 100_000,
            credit: 300_000,
            ..Default::default()
        };
        let i = Inputs { read_credit: Some(&c), ..inputs(&f) };
        let routing = block_of(&human(&i), "Routing (net)").to_string();
        assert!(
            routing.contains("route:Read         credited 75k = 100k replaced less 25k read back later (Reads of the same file in the same transcript), over 28 judged rows; 2 rows whose transcript is gone credited 0; 1000 rows logged before judging credited 0; 3 rows logged without a place credited 0"),
            "{routing}"
        );
        c = crate::tokens::ReadCredit::default();
        let i = Inputs { read_credit: Some(&c), ..inputs(&f) };
        assert!(!block_of(&human(&i), "Routing (net)").contains("route:Read  "), "no rows, no line");
    }

    /// #219: "Brief without atlas" is agents times the per-agent read
    /// measured before atlas, saying what it was measured from; the file
    /// size is one line, "if read whole", and never a ratio. Not measured:
    /// "not measured", and no ratio at all.
    #[test]
    fn the_brief_is_the_measured_read_and_the_file_size_only_if_read_whole() {
        let f = fixture();
        let (h, t) = (human(&inputs(&f)), tsv(&inputs(&f)));
        let brief = block_of(&h, "Brief without atlas");
        assert!(brief.contains("  tokens           1.95M (agents x per agent)"), "{brief}");
        assert!(
            brief.contains(
                "  per agent        1.5k (measured: 25 reads in 17 of 65 pre-atlas subagent transcripts before 2026-10-08T19:20:26.776Z)"
            ),
            "{brief}"
        );
        assert!(
            brief.contains("  if read whole    240M (185k per agent: the rulebook's size once per agent, not a measurement)"),
            "{brief}"
        );
        let m = block_of(&h, "Brief vs injections");
        assert!(m.contains("  rulebook               1.88M (measured: 1.5k per agent, as read before atlas)"), "{m}");
        assert!(m.contains("  rulebook per injected  0.9x"), "{m}");
        assert!(m.contains("  if read whole          230M (the rulebook's size once per agent; not a saving)"), "{m}");
        assert!(!h.contains("115.0x") && !t.contains("115.0x"), "{h}\n{t}");
        assert!(t.contains("\tagents 1300\ttokens 1950000\tper agent 1500\tmeasured from 25 reads in 17 of 65 pre-atlas subagent transcripts before 2026-10-08T19:20:26.776Z\tif read whole 240000000 (per agent 184615)\t"), "{t}");
        assert!(t.contains("brief vs injections (agents in both)\tagents 1250\trulebook 1875000\tinjected 2000000\trulebook/injected 0.9x\tif read whole 230000000\n"), "{t}");
        // Not measured.
        let g = not_measured(fixture());
        let (h, t) = (human(&inputs(&g)), tsv(&inputs(&g)));
        let brief = block_of(&h, "Brief without atlas");
        assert!(brief.contains("  tokens           not measured (no pre-atlas read of the rulebook in the calibration)"), "{brief}");
        assert!(brief.contains("  if read whole    240M"), "{brief}");
        let m = block_of(&h, "Brief vs injections");
        assert!(m.contains("  rulebook       not measured") && !m.contains("rulebook per injected"), "{m}");
        assert!(t.contains("\tagents 1300\ttokens not measured\tif read whole 240000000 (per agent 184615)\t"), "{t}");
        assert!(
            t.contains(
                "brief vs injections (agents in both)\tagents 1250\trulebook not measured\tinjected 2000000\tif read whole 230000000\n"
            ),
            "{t}"
        );
        assert!(!t.contains("rulebook/injected"), "{t}");
    }

    /// #212: main sessions per prompt and subagents per agent, on their own
    /// lines, and one largest line naming its class; no mixed per-agent line.
    #[test]
    fn hook_injections_print_main_sessions_and_subagents_apart() {
        let f = fixture();
        let (h, t) = (human(&inputs(&f)), tsv(&inputs(&f)));
        let b = block_of(&h, "Hook injections");
        assert!(b.contains("  main sessions  3 sessions, 900 prompts; per prompt p50 38, p95 1.96k, max 15.5k"), "{b}");
        assert!(b.contains("  subagents      1308 agents; per agent p50 3.65k, p95 5.77k, max 15.2k"), "{b}");
        assert!(b.contains("  largest        201k (a main session)"), "{b}");
        assert!(!b.contains("largest agent") && !b.contains("  agents "), "{b}");
        assert!(
            t.contains("\tkeys 13828\ttokens 2499940\tmain sessions 3\tprompts 900\tper prompt p50 38 p95 1960 max 15500\tsubagents 1308\tper agent p50 3650 p95 5770 max 15200\tlargest 200740 (main session)\n"),
            "{t}"
        );
        let mut g = fixture();
        g.inject.largest_main = false;
        assert!(block_of(&human(&inputs(&g)), "Hook injections").contains("  largest        201k (a subagent)"));
    }

    /// The note the Lookups block carries with a read_note row (#213).
    const READ_NOTE_NOTE: &str =
        "  read_note is 1x by design: its counterfactual is the whole file it returns. section is the cheaper read for one heading.\n";

    /// #213: with a read_note row the Lookups block says it is 1x by design
    /// and names the cheaper read; without one it says nothing.
    #[test]
    fn read_note_is_marked_one_x_by_design_only_when_it_has_a_row() {
        let f = fixture();
        assert!(!human(&inputs(&f)).contains("read_note is 1x"));
        let mut g = fixture();
        g.tools.push(row("read_note", 14, 11_100.0, 10_500.0));
        g.tools.sort_by(|a, b| a.key.cmp(&b.key));
        let h = human(&inputs(&g));
        let look = h.split("Lookups (").nth(1).unwrap().split("\n\n").next().unwrap();
        assert!(look.ends_with(READ_NOTE_NOTE.trim_end()), "{look}");
        // Its row stays: dropping a 1x row would inflate the headline.
        assert!(look.lines().any(|l| l.trim_start().starts_with("read_note ")), "{look}");
    }

    /// #213: read_note's description, and the listed `note` tool's that
    /// models see first, steer a one-heading read to `section`.
    #[test]
    fn read_note_descriptions_steer_to_section() {
        let full = crate::tools::full_tool_list();
        let d = full.iter().find(|t| t["name"].as_str() == Some("read_note")).unwrap()["description"].as_str().unwrap().to_string();
        assert!(d.contains("section") && d.contains("whole"), "{d}");
        let listed = crate::tools::tool_list();
        let d = listed.iter().find(|t| t["name"].as_str() == Some("note")).unwrap()["description"].as_str().unwrap().to_string();
        assert!(d.contains("section") && d.contains("whole"), "{d}");
    }

    /// The "Where the savings live" rows of a human report, as (source,
    /// credit, cost, net) cells.
    fn where_cells(h: &str) -> Vec<Vec<String>> {
        block_of(h, "Where the savings live")
            .lines()
            .skip(1)
            .map(|l| l.split("  ").map(str::trim).filter(|c| !c.is_empty()).map(str::to_string).collect())
            .collect()
    }

    /// 0069: one row per source with credit, cost, net and its yardstick,
    /// right after the headline, largest saving first; the rows sum to Net
    /// exactly, and Net's credit holds the measured rulebook.
    #[test]
    fn where_the_savings_live_sums_to_net_and_counts_the_measured_rulebook() {
        let mut f = fixture();
        f.tools.push(row("read_note", 14, 11_100.0, 10_500.0));
        f.tools.sort_by(|a, b| a.key.cmp(&b.key));
        let f = with_cost(f);
        let i = inputs(&f);
        let cost = f.cost.as_ref().unwrap();
        let h = human(&i);
        let (w, n) = (h.find("\nWhere the savings live\n").unwrap(), h.find("\nNet\n").unwrap());
        assert!(h.find("Headline").unwrap() < w && w < n, "{h}");
        let rows = where_rows(&i, cost);
        let net = crate::token_cost::totals(&crate::token_cost::net_rows(cost, i.by_tool));
        let sum = rows.iter().fold((0.0, 0.0), |(c, k), r| (c + r.credit, k + r.cost));
        assert!((sum.0 - net.0).abs() < 1e-6 && (sum.1 - net.1).abs() < 1e-6, "{sum:?} against {net:?}");
        // Net's credit holds the measured rulebook: 1250 agents x 1.5k.
        let ledger: f64 = i.by_tool.iter().filter(|t| t.key != DOUBLE_LOGGED && t.key != ROUTED).map(|t| t.counterfactual).sum();
        assert!((net.0 - ledger - 1_875_000.0).abs() < 1e-6, "{} against {ledger}", net.0);
        let rb = rows.iter().find(|r| r.source == RULEBOOK_SOURCE).unwrap();
        // Its cost: the SubagentStart injections into the agents in both
        // tables (1.8M of the 2M sent); the rest is unjudged.
        assert_eq!((rb.credit, rb.cost), (1_875_000.0, 1_800_000.0));
        assert_eq!(rb.against, "the rulebook as read before atlas (65 transcripts)");
        let mem = rows.iter().find(|r| r.source == "memory and note reads").unwrap();
        assert_eq!((mem.credit, mem.cost), (10_500.0, 11_100.0));
        assert_eq!(mem.against, "1x by design: the counterfactual is the same file");
        // Largest saving first.
        assert!(rows.windows(2).all(|p| p[0].net() >= p[1].net()), "{rows:?}");
        let cells = where_cells(&h);
        assert_eq!(cells[0][0], "source");
        let order: Vec<&str> = cells[1..].iter().map(|c| c[0].as_str()).collect();
        assert_eq!(order.first(), Some(&"routing"), "{h}");
        assert_eq!(order.last(), Some(&"total"), "{h}");
        let total = &cells[cells.len() - 1];
        let net_block = block_of(&h, "Net");
        assert!(net_block.contains(&format!("credit  {}", total[1])) && net_block.contains(&format!("cost    {}", total[2])), "{h}");
        // The Net by tool table has the rulebook as its own row.
        assert!(h.lines().any(|l| l.starts_with("  rulebook (measured)") && l.contains("1.88M")), "{h}");
        let t = tsv(&i);
        assert!(t.contains("where\tsource\tcredit\tcost\tnet\tagainst\n"), "{t}");
        assert!(
            t.contains("where\trulebook (subagent starts)\t1875000\t1800000\t75000\tthe rulebook as read before atlas (65 transcripts)\n"),
            "{t}"
        );
    }

    /// With a memory store the ledger already credits hook injections that
    /// hit (#217). No source is credited twice: a measured rulebook
    /// replaces the SubagentStart hit credit in Net and in the rows, and
    /// without a measurement that hit credit is the rulebook row's; Net's
    /// credit is the sum of the rows either way.
    #[test]
    fn with_injection_hits_no_source_is_credited_twice() {
        let mut f = fixture();
        for (key, credit) in
            [("hook:PreToolUse", 1_720_000.0), ("hook:SubagentStart", 23_900_000.0), ("hook:UserPromptSubmit", 4_210_000.0)]
        {
            f.tools.push(row(key, 100, 1000.0, credit));
        }
        f.tools.sort_by(|a, b| a.key.cmp(&b.key));
        let f = with_cost(f);
        let ledger: f64 = f.tools.iter().filter(|t| t.key != DOUBLE_LOGGED && t.key != ROUTED).map(|t| t.counterfactual).sum();
        for (g, want, rulebook_credit) in [
            // Measured: the rulebook's 1.875M stands in for the 23.9M of hits.
            (f, ledger - 23_900_000.0 + 1_875_000.0, 1_875_000.0),
            // Not measured: the hits stand, on the rulebook row.
            (not_measured(with_cost(fixture_with_hits())), ledger, 23_900_000.0),
        ] {
            let i = inputs(&g);
            let cost = g.cost.as_ref().unwrap();
            let net = crate::token_cost::totals(&crate::token_cost::net_rows(cost, i.by_tool));
            assert!((net.0 - want).abs() < 1e-6, "Net credit {} against {want}", net.0);
            let rows = where_rows(&i, cost);
            let sum: f64 = rows.iter().map(|r| r.credit).sum();
            assert!((sum - net.0).abs() < 1e-6, "{sum} against {}", net.0);
            assert_eq!(rows.iter().find(|r| r.source == RULEBOOK_SOURCE).unwrap().credit, rulebook_credit);
            let prompts = rows.iter().find(|r| r.source == PROMPTS_SOURCE).unwrap();
            assert_eq!(prompts.credit, 1_720_000.0 + 4_210_000.0);
        }
    }

    /// The fixture with the hook rows' hit credit a memory store gives.
    fn fixture_with_hits() -> Fixture {
        let mut f = fixture();
        for (key, credit) in
            [("hook:PreToolUse", 1_720_000.0), ("hook:SubagentStart", 23_900_000.0), ("hook:UserPromptSubmit", 4_210_000.0)]
        {
            f.tools.push(row(key, 100, 1000.0, credit));
        }
        f.tools.sort_by(|a, b| a.key.cmp(&b.key));
        f
    }

    /// Review of #277, finding 1: a subagent's `recall {key}` credits the
    /// rulebook window its record sits in, the same reading the measured
    /// rulebook credits. The rulebook row is reduced by the recall-by-record
    /// credit, so Net counts that reading once; the row and the header say
    /// so, and the header bounds the overlap the ledger cannot measure.
    #[test]
    fn recall_by_key_and_the_rulebook_are_credited_once() {
        let mut f = with_cost(fixture());
        f.cost.as_mut().unwrap().rulebook_overlap = 500_000.0;
        let i = inputs(&f);
        let cost = f.cost.as_ref().unwrap();
        let net = crate::token_cost::totals(&crate::token_cost::net_rows(cost, i.by_tool));
        let ledger: f64 = i.by_tool.iter().filter(|t| t.key != DOUBLE_LOGGED && t.key != ROUTED).map(|t| t.counterfactual).sum();
        let want = ledger + 1_875_000.0 - 500_000.0;
        assert!((net.0 - want).abs() < 1e-6, "Net credit {} against {want}", net.0);
        let rows = where_rows(&i, cost);
        let rb = rows.iter().find(|r| r.source == RULEBOOK_SOURCE).unwrap();
        assert_eq!(rb.credit, 1_375_000.0);
        assert_eq!(
            rb.against,
            "the rulebook as read before atlas (65 transcripts), less 500k of recall by key or lane (may read the same records)"
        );
        let sum: f64 = rows.iter().map(|r| r.credit).sum();
        assert!((sum - net.0).abs() < 1e-6, "{sum} against {}", net.0);
        let h = human(&i);
        let header = block_of(&h, "Where the savings live").lines().next().unwrap();
        assert!(header.contains("less 500k of recall by key or lane") && header.contains("by at most 1.38M"), "{header}");
        // Recall by record past the rulebook's credit takes all of it, never
        // below 0, and recall keeps its own credit.
        f.cost.as_mut().unwrap().rulebook_overlap = 5_000_000.0;
        let i = inputs(&f);
        let rows = where_rows(&i, f.cost.as_ref().unwrap());
        assert_eq!(rows.iter().find(|r| r.source == RULEBOOK_SOURCE).unwrap().credit, 0.0);
        let h = human(&i);
        let header = block_of(&h, "Where the savings live").lines().next().unwrap();
        assert!(header.contains("less 1.88M of recall by key or lane") && header.contains("the rulebook row is credited 0"), "{header}");
    }

    /// Review of #277, finding 3: a row whose yardstick is "no pre-atlas
    /// equivalent" or "cost only" but which carries hit credit says where
    /// that credit comes from.
    #[test]
    fn a_cost_only_row_with_hit_credit_names_the_hits() {
        let mut f = fixture_with_hits();
        f.tools.push(row("hook:Notification", 10, 100.0, 5_000.0));
        f.tools.sort_by(|a, b| a.key.cmp(&b.key));
        let f = with_cost(f);
        let i = inputs(&f);
        let rows = where_rows(&i, f.cost.as_ref().unwrap());
        let prompts = rows.iter().find(|r| r.source == PROMPTS_SOURCE).unwrap();
        assert_eq!(prompts.against, "the hits, credited by record size; no pre-atlas equivalent measured");
        let other = rows.iter().find(|r| r.source == OTHER_SOURCE).unwrap();
        assert_eq!(other.credit, 5_000.0);
        assert_eq!(
            other.against,
            "the hits, credited by record size; the rest cost only: writes, unjudged rows, SubagentStart framing and re-reads"
        );
        // Without hit credit the yardsticks read as before.
        let g = with_cost(fixture());
        let rows = where_rows(&inputs(&g), g.cost.as_ref().unwrap());
        assert_eq!(rows.iter().find(|r| r.source == PROMPTS_SOURCE).unwrap().against, "no pre-atlas equivalent measured");
        assert_eq!(
            rows.iter().find(|r| r.source == OTHER_SOURCE).unwrap().against,
            "cost only: writes, unjudged rows, SubagentStart framing and re-reads"
        );
    }

    /// Review of #277, finding 4: the rulebook row's label and its credit
    /// key on the same thing, whether Net credits a measured rulebook. A
    /// measured brief with no agent in both tables credits no rulebook, so
    /// the SubagentStart hits stay on the row and the row says so.
    #[test]
    fn a_measured_brief_with_no_matched_agent_labels_the_hits() {
        let mut f = with_cost(fixture_with_hits());
        f.cost.as_mut().unwrap().rulebook = None;
        let mut i = inputs(&f);
        i.matched = None;
        assert!(i.measured.is_some());
        let rows = where_rows(&i, f.cost.as_ref().unwrap());
        let rb = rows.iter().find(|r| r.source == RULEBOOK_SOURCE).unwrap();
        assert_eq!(rb.credit, 23_900_000.0);
        assert_eq!(rb.against, "not measured: the SubagentStart hits, credited by record size");
    }

    /// 0069: not measured, the rulebook row says so, its credit is "--" and
    /// Net leaves it out; its cost still counts.
    #[test]
    fn an_unmeasured_rulebook_is_no_credit_and_its_cost_still_counts() {
        let f = not_measured(with_cost(fixture()));
        let i = inputs(&f);
        let cost = f.cost.as_ref().unwrap();
        let rows = where_rows(&i, cost);
        let rb = rows.iter().find(|r| r.source == RULEBOOK_SOURCE).unwrap();
        assert_eq!((rb.credit, rb.cost, rb.against.as_str()), (0.0, 1_800_000.0, "not measured"));
        let net = crate::token_cost::totals(&crate::token_cost::net_rows(cost, i.by_tool));
        let ledger: f64 = i.by_tool.iter().filter(|t| t.key != DOUBLE_LOGGED && t.key != ROUTED).map(|t| t.counterfactual).sum();
        assert!((net.0 - ledger).abs() < 1e-6);
        let h = human(&i);
        let line = block_of(&h, "Where the savings live").lines().find(|l| l.trim_start().starts_with(RULEBOOK_SOURCE)).unwrap();
        assert!(line.contains(" --  ") && line.ends_with("not measured"), "{line}");
        assert!(!h.contains("rulebook (measured)"), "{h}");
        assert!(tsv(&i).contains("where\trulebook (subagent starts)\t--\t1800000\t-1800000\tnot measured\n"));
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
brief without atlas (the rulebook as read before atlas, measured per agent over the pre-atlas subagent transcripts; once per agent, a resume is not a second read; not in the ratios above)\tagents 1300\ttokens 1950000\tper agent 1500\tmeasured from 25 reads in 17 of 65 pre-atlas subagent transcripts before 2026-10-08T19:20:26.776Z\tif read whole 240000000 (per agent 184615)\tstarts 3203 (1903 resumes)\tlatest 189770
hook injections (recall pointers; a main session per prompt, a subagent per agent)\tkeys 13828\ttokens 2499940\tmain sessions 3\tprompts 900\tper prompt p50 38 p95 1960 max 15500\tsubagents 1308\tper agent p50 3650 p95 5770 max 15200\tlargest 200740 (main session)
brief vs injections (agents in both)\tagents 1250\trulebook 1875000\tinjected 2000000\trulebook/injected 0.9x\tif read whole 230000000
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
            measured: None,
            inject: None,
            matched: None,
            select: &[],
            compact: None,
            hook_hits: None,
            read_credit: None,
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
  The rulebook as agents read it before atlas, measured per agent over the pre-atlas subagent transcripts, once per agent; not in the ratios above.
  agents           1300
  tokens           1.95M (agents x per agent)
  per agent        1.5k (measured: 25 reads in 17 of 65 pre-atlas subagent transcripts before 2026-10-08T19:20:26.776Z)
  if read whole    240M (185k per agent: the rulebook's size once per agent, not a measurement)
  subagent starts  3203 (1903 resumes: a resumed agent starts again, keeps its context and is counted once)
  latest rulebook  190k

Hook injections
  What the recall hooks injected as pointers: a main session per prompt (it lives for many prompts), a subagent per agent.
  keys           13828
  tokens         2.5M
  main sessions  3 sessions, 900 prompts; per prompt p50 38, p95 1.96k, max 15.5k
  subagents      1308 agents; per agent p50 3.65k, p95 5.77k, max 15.2k
  largest        201k (a main session)

Brief vs injections
  The agents in both tables: the rulebook as read before atlas against what the hooks injected into them.
  agents                 1250
  rulebook               1.88M (measured: 1.5k per agent, as read before atlas)
  injected               2M
  rulebook per injected  0.9x
  if read whole          230M (the rulebook's size once per agent; not a saving)

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
