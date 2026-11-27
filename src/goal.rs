//! atlas goals (docs/design/atlas-goal.md): a goal is a record of kind
//! `goal`, key `goal:<slug>`, scope the lane that carries its journal.
//!
//! Build step 1: the record, its body splitter and spec parser, the
//! `note op=goal` actions and the one-shot seed from a memory record.
//! Build step 2 (`goal_eval.rs`): `metric:` lines and [`evaluate`].

use crate::error::{Error, Result};
use crate::memory::{short_title, Draft, Head, MemoryStore, Writer, Written};

/// The record kind.
pub const KIND: &str = "goal";
/// The journal entry kind each spec write appends (Q-GO2).
pub const ENTRY: &str = "goal";

/// Who writes, for the version's audit fields.
#[derive(Debug, Clone, Copy)]
pub struct Who<'a> {
    /// Who asked for it.
    pub author: &'a str,
    /// The MCP client or command.
    pub client: &'a str,
    /// Why.
    pub reason: &'a str,
}

/// A goal body's parts (section 3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Body {
    /// The goal as set, verbatim.
    pub text: String,
    /// Each `AMENDED` paragraph, verbatim, oldest first.
    pub amendments: Vec<String>,
    /// The spec block's lines, verbatim, without its fences.
    pub spec: Option<String>,
    /// Anything after the spec block.
    pub notes: String,
}

/// A goal's state, from its meta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Checked in on.
    Active,
    /// Kept, no check-ins.
    Paused,
    /// Ended met.
    Done,
    /// Ended unmet.
    Dropped,
}

/// A state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Active to paused.
    Pause,
    /// Paused to active.
    Resume,
    /// Ends the goal met.
    Done,
    /// Ends the goal unmet.
    Drop,
}

/// A comparison of a gate or aim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `=`
    Eq,
}

/// How a gate over arms takes its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// The worst arm (the default).
    Worst,
    /// The median arm.
    Median,
}

/// One `gate` or `aim` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    /// `G1`, `A1`.
    pub id: String,
    /// A dotted name with an optional `@case`.
    pub metric: String,
    /// The comparison.
    pub cmp: Cmp,
    /// The bar as written.
    pub bar: String,
    /// The unit.
    pub unit: String,
    /// The arms, empty without `over`.
    pub over: Vec<String>,
    /// How arms are taken.
    pub take: Take,
    /// Contended lines ignored.
    pub quiet: bool,
    /// The lane that feeds it.
    pub feeds: Option<String>,
}

/// One `step` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Its number.
    pub n: u32,
    /// Its label.
    pub label: String,
    /// `done-when <metric> <op> <n>`.
    pub done_when: Option<(String, Cmp, String)>,
}

/// The `cadence` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Every N minutes.
    Minutes(u32),
    /// Every N turns.
    Turns(u32),
}

/// A parsed spec block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spec {
    /// Must hold.
    pub gates: Vec<Bar>,
    /// Reported, never decide met.
    pub aims: Vec<Bar>,
    /// Aims printed together on every check-in.
    pub report: Vec<String>,
    /// Serving lanes, with the step each serves.
    pub lanes: Vec<(String, Option<u32>)>,
    /// The work order.
    pub steps: Vec<Step>,
    /// Per-goal cadence.
    pub cadence: Option<Cadence>,
    /// Per-goal stall hours.
    pub stall_hours: Option<u32>,
}

/// The spec block's opening fence line.
const FENCE_OPEN: &str = "```goal\n";
/// The spec block's closing fence line.
const FENCE_CLOSE: &str = "```";

/// The record key of a goal.
pub fn key(slug: &str) -> String {
    format!("goal:{slug}")
}

/// Where the spec block's opening fence starts, at a line start.
fn fence_at(body: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = body[from..].find(FENCE_OPEN) {
        let at = from + i;
        if at == 0 || body.as_bytes()[at - 1] == b'\n' {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// Splits a goal body into its parts: the paragraphs before the first
/// `AMENDED` paragraph are the text; each `AMENDED` paragraph starts an
/// amendment (a paragraph that follows one, before the next, is part of
/// it); the ```` ```goal ```` block is the spec; what follows it, notes.
pub fn split(body: &str) -> Body {
    // A CRLF body reads as its LF form (journal:ATLAS-GOAL:0036 item 3).
    let lf = body.replace("\r\n", "\n");
    let body = lf.as_str();
    let (before, spec, notes) = match fence_at(body) {
        Some(at) => {
            let inner = &body[at + FENCE_OPEN.len()..];
            // The closing fence: a line that is only ```.
            let mut end = None;
            let mut pos = 0;
            for line in inner.split_inclusive('\n') {
                if line.trim_end() == FENCE_CLOSE {
                    end = Some((pos, pos + line.len()));
                    break;
                }
                pos += line.len();
            }
            match end {
                Some((s, e)) => (&body[..at], Some(inner[..s].to_string()), inner[e..].trim_matches('\n').to_string()),
                None => (&body[..at], Some(inner.to_string()), String::new()),
            }
        }
        None => (body, None, String::new()),
    };
    let mut text: Vec<&str> = Vec::new();
    let mut amendments: Vec<String> = Vec::new();
    for p in before.trim_matches('\n').split("\n\n") {
        if p.starts_with("AMENDED") {
            amendments.push(p.to_string());
        } else if let Some(last) = amendments.last_mut() {
            last.push_str("\n\n");
            last.push_str(p);
        } else {
            text.push(p);
        }
    }
    Body { text: text.join("\n\n").trim().to_string(), amendments, spec, notes }
}

/// A body from its parts, as [`split`] reads it back.
pub fn join(b: &Body) -> String {
    let mut out = format!("{}\n", b.text.trim());
    for a in &b.amendments {
        out.push('\n');
        out.push_str(a.trim_end());
        out.push('\n');
    }
    if let Some(s) = &b.spec {
        out.push('\n');
        out.push_str(FENCE_OPEN);
        out.push_str(s);
        out.push_str(FENCE_CLOSE);
        out.push('\n');
    }
    if !b.notes.trim().is_empty() {
        out.push('\n');
        out.push_str(b.notes.trim_end());
        out.push('\n');
    }
    out
}

/// A spec as the caller sent it, its lines kept: without the fences if it
/// came with them, without blank lines at either end, one newline last.
/// Atlas adds no line and changes none.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an empty spec or one that does not parse.
pub fn normalize_spec(sent: &str) -> Result<String> {
    let mut lines: Vec<&str> = sent.lines().collect();
    if lines.first().is_some_and(|l| l.trim() == FENCE_OPEN.trim_end()) {
        lines.remove(0);
        if let Some(i) = lines.iter().rposition(|l| l.trim() == FENCE_CLOSE) {
            lines.truncate(i);
        }
    }
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return Err(Error::InvalidArgument("spec is empty: send its lines (section 3 of docs/design/atlas-goal.md), or leave spec out".into()));
    }
    let spec = format!("{}\n", lines.join("\n"));
    parse_spec(&spec).map_err(|e| Error::InvalidArgument(format!("spec refused, nothing written: {e}")))?;
    Ok(spec)
}

fn cmp_of(w: &str) -> Option<Cmp> {
    Some(match w {
        "<" => Cmp::Lt,
        "<=" => Cmp::Le,
        ">" => Cmp::Gt,
        ">=" => Cmp::Ge,
        "=" => Cmp::Eq,
        _ => return None,
    })
}

/// A metric name: a letter, then letters, digits and `._-`, with an
/// optional `@case`.
fn is_metric(w: &str) -> bool {
    w.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

fn is_number(w: &str) -> bool {
    w.parse::<f64>().is_ok_and(f64::is_finite)
}

/// `<id> <metric> <op> <bar> <unit> [over <arms>] [take worst|median] [quiet] [feeds <lane>]`.
fn parse_bar(kind: &str, w: &[&str]) -> std::result::Result<Bar, String> {
    let form = format!("`{kind} <id> <metric> <op> <bar> <unit> [over <arms>] [take worst|median] [quiet] [feeds <lane>]`");
    let id = w.first().ok_or_else(|| format!("{kind} has no id: {form}"))?;
    let metric = match w.get(1) {
        Some(m) if is_metric(m) => *m,
        _ => return Err(format!("{kind} {id} has no metric: {form}")),
    };
    let cmp = w.get(2).and_then(|c| cmp_of(c)).ok_or_else(|| format!("{kind} {id} has no comparison (<, <=, >, >=, =): {form}"))?;
    let bar = w.get(3).filter(|b| is_number(b)).ok_or_else(|| format!("{kind} {id} has no numeric bar: {form}"))?;
    let unit = w.get(4).filter(|u| !matches!(**u, "over" | "take" | "quiet" | "feeds")).ok_or_else(|| format!("{kind} {id} has no unit: {form}"))?;
    let mut b = Bar { id: id.to_string(), metric: metric.to_string(), cmp, bar: bar.to_string(), unit: unit.to_string(), over: Vec::new(), take: Take::Worst, quiet: false, feeds: None };
    let mut i = 5;
    while i < w.len() {
        match (w[i], w.get(i + 1)) {
            ("over", Some(a)) => {
                b.over = a.split(',').map(str::to_string).collect();
                if b.over.iter().any(String::is_empty) {
                    return Err(format!("{kind} {id}: over takes arms separated by commas"));
                }
                i += 2;
            }
            ("take", Some(&"worst")) => (b.take, i) = (Take::Worst, i + 2),
            ("take", Some(&"median")) => (b.take, i) = (Take::Median, i + 2),
            ("feeds", Some(l)) => (b.feeds, i) = (Some(l.to_string()), i + 2),
            ("quiet", _) => (b.quiet, i) = (true, i + 1),
            (o, _) => return Err(format!("{kind} {id}: {o:?} is not over, take worst|median, quiet or feeds: {form}")),
        }
    }
    Ok(b)
}

fn parse_u32(w: Option<&&str>, suffix: &str) -> Option<u32> {
    w?.strip_suffix(suffix)?.parse().ok().filter(|n| *n > 0)
}

/// Parses a spec block (its lines, without fences): `gate`, `aim`,
/// `report`, `lane`, `step`, `cadence` and `stall` lines (section 3);
/// blank lines are skipped, anything else is refused.
///
/// # Errors
///
/// `line N: why` for the first line refused.
pub fn parse_spec(text: &str) -> std::result::Result<Spec, String> {
    let mut s = Spec::default();
    let mut report_line = 0;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let w: Vec<&str> = line.split_whitespace().collect();
        let Some(first) = w.first() else { continue };
        let err = |why: String| format!("line {n}: {why}: {}", line.trim());
        match *first {
            "gate" | "aim" => {
                let b = parse_bar(first, &w[1..]).map_err(err)?;
                if s.gates.iter().chain(&s.aims).any(|x| x.id == b.id) {
                    return Err(err(format!("id {} is used twice", b.id)));
                }
                if *first == "gate" {
                    s.gates.push(b);
                } else {
                    s.aims.push(b);
                }
            }
            "report" => {
                if w.len() < 2 {
                    return Err(err("report names no aims: `report <aim id> ...`".into()));
                }
                s.report.extend(w[1..].iter().map(|x| x.to_string()));
                report_line = n;
            }
            "lane" => {
                let lane = w.get(1).ok_or_else(|| err("lane has no name: `lane <LANE> [step <n>]`".into()))?;
                let step = match (w.get(2), w.get(3), w.len()) {
                    (None, _, 2) => None,
                    (Some(&"step"), Some(k), 4) => Some(k.parse().map_err(|_| err(format!("step {k:?} is no number")))?),
                    _ => return Err(err("`lane <LANE> [step <n>]`".into())),
                };
                s.lanes.push((lane.to_string(), step));
            }
            "step" => {
                let num: u32 = w.get(1).and_then(|k| k.parse().ok()).ok_or_else(|| err("`step <n> <label> [done-when <metric> <op> <n>]`".into()))?;
                let dw = w.iter().position(|x| *x == "done-when");
                let label = w[2..dw.unwrap_or(w.len())].join(" ");
                if label.is_empty() {
                    return Err(err(format!("step {num} has no label")));
                }
                let done_when = match dw {
                    None => None,
                    Some(j) => match (w.get(j + 1), w.get(j + 2).and_then(|c| cmp_of(c)), w.get(j + 3), w.len() == j + 4) {
                        (Some(m), Some(c), Some(v), true) if is_metric(m) && is_number(v) => Some((m.to_string(), c, v.to_string())),
                        _ => return Err(err("`done-when <metric> <op> <n>`".into())),
                    },
                };
                s.steps.push(Step { n: num, label, done_when });
            }
            "cadence" => {
                let c = match (w.get(1), w.len()) {
                    (Some(&"every"), 3) => parse_u32(w.get(2), "m").map(Cadence::Minutes).or_else(|| parse_u32(w.get(2), "t").map(Cadence::Turns)),
                    _ => None,
                };
                s.cadence = Some(c.ok_or_else(|| err("`cadence every <minutes>m` or `cadence every <turns>t`".into()))?);
            }
            "stall" => {
                s.stall_hours = Some(parse_u32(w.get(1), "h").filter(|_| w.len() == 2).ok_or_else(|| err("`stall <hours>h`".into()))?);
            }
            o => return Err(err(format!("{o:?} is not gate, aim, report, lane, step, cadence or stall"))),
        }
    }
    for r in &s.report {
        if !s.aims.iter().any(|a| &a.id == r) {
            return Err(format!("line {report_line}: report names {r}, which is no aim of this spec"));
        }
    }
    Ok(s)
}

/// The value of a `name: value` meta line.
fn meta_value<'a>(meta: &'a str, name: &str) -> Option<&'a str> {
    meta.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(':').map(str::trim))
}

/// `meta` with its `name:` line set to `value` (added last if absent).
fn with_meta(meta: &str, name: &str, value: &str) -> String {
    let mut out = String::new();
    let mut found = false;
    for l in meta.lines() {
        if l.strip_prefix(name).is_some_and(|r| r.starts_with(':')) {
            out.push_str(&format!("{name}: {value}\n"));
            found = true;
        } else {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !found {
        out.push_str(&format!("{name}: {value}\n"));
    }
    out
}

/// A goal's state from its meta; a meta without `state:` is active.
pub fn state(meta: &str) -> State {
    match meta_value(meta, "state") {
        Some("paused") => State::Paused,
        Some("done") => State::Done,
        Some("dropped") => State::Dropped,
        _ => State::Active,
    }
}

impl State {
    /// The meta word.
    pub fn word(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Paused => "paused",
            State::Done => "done",
            State::Dropped => "dropped",
        }
    }
}

/// The meta line that holds the met token (section 2.3).
const MET_TOKEN: &str = "met_token";

/// A record's meta as every store read returns it (journal:ATLAS-GOAL:0029
/// item 1): for a `goal` record, an issued `met_token` value reads
/// `issued`, so no read path prints the nonce. [`met_token`] is the one
/// accessor that reads it.
pub fn redact_meta(kind: &str, meta: String) -> String {
    if kind != KIND || meta_value(&meta, MET_TOKEN).is_none_or(|v| v.is_empty() || v == "issued") {
        return meta;
    }
    with_meta(&meta, MET_TOKEN, "issued")
}

/// The head's meta and sha as stored, unredacted.
fn raw_head(store: &MemoryStore, key: &str) -> Result<Option<(String, String)>> {
    use crate::db::OptionalExtension;
    Ok(store
        .conn()
        .prepare_cached("SELECT v.meta, v.sha FROM records r JOIN versions v ON v.vid = r.head WHERE r.key = ?1 AND r.kind = 'goal'")?
        .query_row([key], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// The met token of `goal:<slug>` as issued, the one read of it that is not
/// redacted (for `done`, build step 3). `None` when none was issued.
///
/// # Errors
///
/// [`Error::Db`].
pub fn met_token(store: &MemoryStore, slug: &str) -> Result<Option<String>> {
    Ok(raw_head(store, &key(slug))?.and_then(|(m, _)| meta_value(&m, MET_TOKEN).filter(|v| !v.is_empty()).map(str::to_string)))
}

/// The goal records whose state is active, newest first.
///
/// # Errors
///
/// [`Error::Db`].
pub fn active(store: &MemoryStore) -> Result<Vec<Head>> {
    let mut v: Vec<Head> = store.heads(Some(KIND), None)?.into_iter().filter(|h| h.status == "active" && state(&h.meta) == State::Active).collect();
    v.reverse();
    Ok(v)
}

fn draft(who: &Who<'_>, title: String, meta: String, body: String, reason: String) -> Draft {
    Draft { title, meta, body, reason, author: who.author.to_string(), client: who.client.to_string(), provenance: None, time: None }
}

fn check_slug(slug: &str) -> Result<()> {
    if slug.is_empty() || !slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(Error::InvalidArgument(format!("slug {slug:?}: lower-case letters, digits and -")));
    }
    Ok(())
}

/// Refuses a goal becoming active while another is (Q-AG4, "One active per
/// project"), read under the batch's write lock.
fn check_one_active(w: &Writer<'_>, k: &str) -> Result<()> {
    if let Some(h) = w.heads(Some(KIND), None)?.into_iter().find(|h| h.key != k && h.status == "active" && state(&h.meta) == State::Active) {
        let slug = h.key.strip_prefix("goal:").unwrap_or(&h.key);
        return Err(Error::InvalidArgument(format!(
            "{} is active, and a project has one active goal (Q-AG4): pause it first (note op=goal action=pause slug={slug}), or drop it",
            h.key
        )));
    }
    Ok(())
}

/// The Q-GO2 `goal` journal entry of a spec write, in the goal's lane: the
/// spec as written, who wrote it, and the version it made, with its sha.
fn append_entry(w: &mut Writer<'_>, k: &str, lane: &str, written: &Written, spec: &str, who: &Who<'_>) -> Result<()> {
    let ek = w.next_key(&format!("journal:{lane}:"), 4)?;
    let meta = format!("goal: {k}\nversion: {}\nsha: {}\nby: {}\n", written.seq, written.sha, who.author);
    let d = draft(who, format!("{k} v{} spec", written.seq), meta, spec.to_string(), format!("goal spec of {k} v{}", written.seq));
    w.create(&ek, "journal", ENTRY, lane, &d, None)?;
    w.add_refs(&ek, &[("lane".to_string(), lane.to_string()), ("goal".to_string(), k.to_string())])?;
    Ok(())
}

/// Whether `t` starts in the form `AMENDED <YYYY-MM-DD> (<who>): <text>`
/// (Q-GS5): a date, a non-empty author in parentheses, then words.
fn amended_form(t: &str) -> bool {
    let Some(rest) = t.strip_prefix("AMENDED ") else { return false };
    let (date, rest) = (rest.get(..10).unwrap_or(""), rest.get(10..).unwrap_or(""));
    let date_ok = date.len() == 10 && date.char_indices().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() });
    let Some(rest) = rest.strip_prefix(" (") else { return false };
    match rest.find("): ") {
        Some(i) => date_ok && !rest[..i].trim().is_empty() && !rest[i + 3..].trim().is_empty(),
        None => false,
    }
}

/// Refuses an amendment not in the AMENDED form or holding a fence.
fn check_amendment(t: &str) -> Result<()> {
    if !amended_form(t) || t.contains(FENCE_CLOSE) {
        return Err(Error::InvalidArgument(
            "an amendment is sent as written, in the form `AMENDED <YYYY-MM-DD> (<who>): <text>`, with no ``` fence; atlas adds no words to it".into(),
        ));
    }
    Ok(())
}

/// Refuses a goal text holding a fence or an AMENDED paragraph.
fn check_text(text: &str) -> Result<()> {
    if text.contains(FENCE_CLOSE) || text.lines().any(|l| l.starts_with("AMENDED")) {
        return Err(Error::InvalidArgument("the text may hold no ``` fence and no AMENDED paragraph: send the spec as spec, amendments with amend".into()));
    }
    Ok(())
}

/// A goal's title: its key and the text's first line, cut.
fn title_of(k: &str, text: &str) -> String {
    format!("{k}: {}", short_title(text.lines().next().unwrap_or("")))
}

/// Creates `goal:<slug>` in `lane`, active, from the text and the spec the
/// caller sends (normalized by [`normalize_spec`]); with a spec it appends
/// the `goal` journal entry in the same commit.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for a bad slug, an empty text, a spec that
/// does not parse, or another goal active; [`Error::AlreadyExists`] for a
/// used slug (amend it instead); [`Error::Db`].
pub fn set(store: &mut MemoryStore, slug: &str, lane: &str, text: &str, spec: Option<&str>, who: &Who<'_>) -> Result<Written> {
    check_slug(slug)?;
    let (lane, text) = (lane.trim(), text.trim());
    if lane.is_empty() || text.is_empty() {
        return Err(Error::InvalidArgument("set needs slug, lane and text (the goal verbatim)".into()));
    }
    check_text(text)?;
    let spec = spec.map(normalize_spec).transpose()?;
    let k = key(slug);
    let body = join(&Body { text: text.to_string(), amendments: Vec::new(), spec: spec.clone(), notes: String::new() });
    let mut w = store.writer()?;
    if w.heads(Some(KIND), None)?.iter().any(|h| h.key == k) {
        return Err(Error::AlreadyExists(format!("{k}: amend it (note op=goal action=amend), or pick another slug")));
    }
    check_one_active(&w, &k)?;
    let written = w.create(&k, KIND, "", lane, &draft(who, title_of(&k, text), with_meta("", "state", "active"), body, format!("goal set: {}", who.reason)), None)?;
    w.add_refs(&k, &[("lane".to_string(), lane.to_string())])?;
    if let Some(s) = &spec {
        append_entry(&mut w, &k, lane, &written, s, who)?;
    }
    w.commit()?;
    Ok(written)
}

/// Adds an amendment (an `AMENDED <date> (<who>): <text>` paragraph,
/// verbatim) and/or replaces the spec, as a new version of the record; a
/// new spec appends the `goal` journal entry in the same commit.
///
/// # Errors
///
/// [`Error::NotFound`] for an unknown goal; [`Error::InvalidArgument`] for
/// neither text nor spec, text not in the AMENDED form, a spec that does
/// not parse, or an ended goal; [`Error::StaleRecord`] when the head is
/// not `expected_sha`; [`Error::Db`].
pub fn amend(store: &mut MemoryStore, slug: &str, expected_sha: &str, text: Option<&str>, spec: Option<&str>, who: &Who<'_>) -> Result<Written> {
    let k = key(slug);
    let text = text.map(str::trim).filter(|t| !t.is_empty());
    let spec = spec.filter(|s| !s.trim().is_empty()).map(normalize_spec).transpose()?;
    if text.is_none() && spec.is_none() {
        return Err(Error::InvalidArgument("amend needs text (an AMENDED paragraph), spec, or both".into()));
    }
    if let Some(t) = text {
        check_amendment(t)?;
    }
    let h = store.head(&k)?.filter(|h| h.kind == KIND).ok_or_else(|| Error::NotFound(format!("goal {k}")))?;
    let (meta, _) = raw_head(store, &k)?.ok_or_else(|| Error::NotFound(format!("goal {k}")))?;
    if matches!(state(&meta), State::Done | State::Dropped) {
        return Err(Error::InvalidArgument(format!("{k} is {}: an ended goal is not amended; set a new one", state(&meta).word())));
    }
    let mut b = split(&h.body);
    if let Some(t) = text {
        b.amendments.push(t.to_string());
    }
    if spec.is_some() {
        b.spec.clone_from(&spec);
    }
    let what = match (text.is_some(), spec.is_some()) {
        (true, true) => "amendment and spec",
        (true, false) => "amendment",
        _ => "spec",
    };
    let mut w = store.writer()?;
    let written = w.revise(&k, Some(expected_sha), &draft(who, h.title.clone(), meta, join(&b), format!("goal {what}: {}", who.reason)))?;
    if let (Some(s), true) = (&spec, written.changed) {
        append_entry(&mut w, &k, &h.scope, &written, s, who)?;
    }
    w.commit()?;
    Ok(written)
}

/// Changes a goal's state: a new version with the title and body as they
/// are and the meta's `state:` line changed (journal:ATLAS-GOAL:0031
/// Q-GS1). `done` is refused in build step 1: it needs the met check
/// (`goal::evaluate`, build steps 2 and 3) (Q-GS2).
///
/// # Errors
///
/// [`Error::NotFound`] for an unknown goal; [`Error::InvalidArgument`] for
/// a change its state does not allow, `done`, or a resume while another
/// goal is active; [`Error::Db`].
pub fn change(store: &mut MemoryStore, slug: &str, action: Action, who: &Who<'_>) -> Result<Written> {
    let k = key(slug);
    if action == Action::Done {
        return Err(Error::InvalidArgument(format!(
            "done refused: it needs the met check (goal::evaluate), which lands in atlas-goal build steps 2 and 3; to end {k} unmet, drop it"
        )));
    }
    let h = store.head(&k)?.filter(|h| h.kind == KIND).ok_or_else(|| Error::NotFound(format!("goal {k}")))?;
    let (meta, sha) = raw_head(store, &k)?.ok_or_else(|| Error::NotFound(format!("goal {k}")))?;
    let now = state(&meta);
    let to = match (action, now) {
        (Action::Pause, State::Active) => State::Paused,
        (Action::Resume, State::Paused) => State::Active,
        (Action::Drop, State::Active | State::Paused) => State::Dropped,
        _ => return Err(Error::InvalidArgument(format!("{k} is {}: {action:?} does not apply", now.word()))),
    };
    let mut w = store.writer()?;
    if to == State::Active {
        check_one_active(&w, &k)?;
    }
    let written = w.revise(&k, Some(&sha), &draft(who, h.title.clone(), with_meta(&meta, "state", to.word()), h.body.clone(), format!("goal {}: {}", to.word(), who.reason)))?;
    w.commit()?;
    Ok(written)
}

/// The `show` answer: the goal's head (key, version, state, lane), its
/// body, and its gates, aims and steps by id. With no slug, the active
/// goal, else every goal's key and state. Reads the state, never the met
/// token.
///
/// # Errors
///
/// [`Error::NotFound`] for an unknown goal; [`Error::Db`].
pub fn show(store: &MemoryStore, slug: Option<&str>) -> Result<String> {
    let h = match slug.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => store.head(&key(s))?.filter(|h| h.kind == KIND).ok_or_else(|| Error::NotFound(format!("goal {}", key(s))))?,
        None => match active(store)?.into_iter().next() {
            Some(h) => h,
            None => {
                let all = store.heads(Some(KIND), None)?;
                if all.is_empty() {
                    return Ok("no goal records; set one with note op=goal action=set\n".into());
                }
                let mut out = String::from("no active goal; goals:\n");
                for g in all {
                    out.push_str(&format!("{} v{} {}\n", g.key, g.seq, state(&g.meta).word()));
                }
                return Ok(out);
            }
        },
    };
    let mut out = format!("== {} goal {} v{} lane={} {} sha={}\n", h.key, state(&h.meta).word(), h.seq, h.scope, h.time.get(..16).unwrap_or(&h.time), h.sha.get(..16).unwrap_or(&h.sha));
    out.push_str(&h.body);
    if !h.body.ends_with('\n') {
        out.push('\n');
    }
    let spec = split(&h.body).spec.map(|s| parse_spec(&s));
    match spec {
        Some(Ok(s)) if !s.gates.is_empty() => {
            let e = evaluate(store, &h.key)?;
            out.push_str(&format!("gates {}/{} met; aims {}/{}:", e.gates_met, e.gates, e.aims_met, e.aims));
            for b in &e.bars {
                match &b.taken {
                    Some(t) => {
                        let verdict = if b.verdict == Verdict::Met { "MET".to_string() } else { format!("{:+}", b.over().unwrap_or(f64::NAN)) };
                        let arm = t.arm.as_deref().map(|a| format!(" {a}")).unwrap_or_default();
                        let delta = b.delta.map(|d| format!(" {d:+} since checkpoint")).unwrap_or_default();
                        out.push_str(&format!(" {} {}/{} {} {verdict} (@{}{arm}{delta});", b.id, t.raw, b.bar.bar, b.bar.unit, t.commit));
                    }
                    None => out.push_str(&format!(" {} not measured;", b.id)),
                }
                if let Some(p) = &b.partial {
                    out.push_str(&format!(" [newer @{} lacks {}];", p.commit, p.missing.join(",")));
                }
            }
            out.push('\n');
            if !e.skipped.is_empty() {
                out.push_str(&format!("metric lines not used: {}\n", e.skipped.join("; ")));
            }
            if !s.steps.is_empty() {
                let steps: Vec<String> = s.steps.iter().map(|st| format!("{} {}", st.n, st.label)).collect();
                out.push_str(&format!("steps: {}\n", steps.join("; ")));
            }
        }
        Some(Err(e)) => out.push_str(&format!("spec does not parse ({e}): no gates read\n")),
        _ => out.push_str("no gates: not measured, never met\n"),
    }
    Ok(out)
}

/// The one-shot seed (build step 1): `goal:<slug>` from memory record
/// `memory_key` (`active-goal-<slug>`), version 1 the text alone (the
/// memory's first paragraph wrapped in double quotes, the quotes dropped),
/// version 2 the text, every `AMENDED` paragraph byte for byte, and the
/// caller's spec when one is given (journal:ATLAS-GOAL:0031 Q-GS3). The
/// memory's other paragraphs are not copied. No spec, no gates.
///
/// # Errors
///
/// [`Error::NotFound`] for no such memory; [`Error::InvalidArgument`] for
/// a memory with no quoted paragraph, a spec that does not parse, or
/// another goal active; [`Error::AlreadyExists`] once seeded; [`Error::Db`].
pub fn seed(store: &mut MemoryStore, memory_key: &str, spec: Option<&str>, lane: &str, who: &Who<'_>) -> Result<Vec<Written>> {
    let m = store.head(memory_key)?.filter(|h| h.kind == "memory").ok_or_else(|| Error::NotFound(format!("memory record {memory_key:?}")))?;
    let slug = memory_key.strip_prefix("active-goal-").unwrap_or(memory_key);
    check_slug(slug)?;
    // A CRLF memory reads as its LF form (journal:ATLAS-GOAL:0036 item 3).
    let body = m.body.replace("\r\n", "\n");
    let paras: Vec<&str> = body.split("\n\n").map(|p| p.trim_matches('\n')).collect();
    let text = paras
        .iter()
        .map(|p| p.trim())
        .find_map(|p| p.strip_prefix('"').and_then(|p| p.strip_suffix('"')).filter(|t| !t.trim().is_empty()))
        .ok_or_else(|| Error::InvalidArgument(format!("memory {memory_key} has no paragraph wrapped in double quotes: no goal text to copy")))?;
    let amendments: Vec<String> = paras.iter().filter(|p| p.starts_with("AMENDED")).map(|p| p.to_string()).collect();
    // set's and amend's checks, on what the seed copies (0036 items 4, 5).
    check_text(text).map_err(|e| Error::InvalidArgument(format!("memory {memory_key}: {e}")))?;
    for a in &amendments {
        check_amendment(a).map_err(|e| Error::InvalidArgument(format!("memory {memory_key}: {e}")))?;
    }
    let spec = spec.map(normalize_spec).transpose()?;
    let k = key(slug);
    let from = format!("seed from memory {memory_key} v{} sha {}", m.seq, m.sha.get(..16).unwrap_or(&m.sha));
    let mut w = store.writer()?;
    if w.heads(Some(KIND), None)?.iter().any(|h| h.key == k) {
        return Err(Error::AlreadyExists(format!("{k}: the seed is one-shot")));
    }
    check_one_active(&w, &k)?;
    let title = title_of(&k, text);
    let meta = with_meta("", "state", "active");
    let v1 = Body { text: text.to_string(), ..Body::default() };
    let mut out = vec![w.create(&k, KIND, "", lane, &draft(who, title.clone(), meta.clone(), join(&v1), format!("{from}: the text")), None)?];
    w.add_refs(&k, &[("lane".to_string(), lane.to_string()), ("seeded_from".to_string(), memory_key.to_string())])?;
    if !amendments.is_empty() || spec.is_some() {
        let v2 = Body { amendments, spec: spec.clone(), ..v1 };
        let written = w.revise(&k, Some(&out[0].sha), &draft(who, title, meta, join(&v2), format!("{from}: amendments and spec")))?;
        if let Some(s) = &spec {
            append_entry(&mut w, &k, lane, &written, s, who)?;
        }
        out.push(written);
    }
    w.commit()?;
    Ok(out)
}

/// The usage of `atlas goal`.
pub const USAGE: &str = "usage: atlas goal seed --from-memory KEY [--spec FILE] [--lane LANE]
  one-shot: goal:<slug> from memory KEY (active-goal-<slug>): v1 the quoted text, v2 the AMENDED
  paragraphs and the spec in FILE (gates, aims, lanes, steps; atlas derives none). --lane is the
  goal's journal lane (default GOAL).
";

/// `atlas goal ...`.
///
/// # Errors
///
/// The usage, or the refusal, as text.
pub fn cli(args: &[String], store: &mut MemoryStore) -> std::result::Result<std::process::ExitCode, String> {
    let mut it = args.iter();
    if it.next().map(String::as_str) != Some("seed") {
        return Err(USAGE.to_string());
    }
    let (mut from, mut spec, mut lane) = (None, None, "GOAL".to_string());
    while let Some(a) = it.next() {
        let v = it.next().ok_or_else(|| format!("{a} needs a value\n{USAGE}"))?;
        match a.as_str() {
            "--from-memory" => from = Some(v.clone()),
            "--spec" => spec = Some(std::fs::read_to_string(v).map_err(|e| format!("--spec {v}: {e}"))?),
            "--lane" => lane.clone_from(v),
            _ => return Err(format!("unknown option {a}\n{USAGE}")),
        }
    }
    let from = from.ok_or_else(|| format!("--from-memory is required\n{USAGE}"))?;
    let who = Who { author: "atlas goal seed", client: "cli", reason: "one-shot seed" };
    for w in seed(store, &from, spec.as_deref(), &lane, &who).map_err(|e| e.to_string())? {
        println!("ok {} v{} sha={}", w.key, w.seq, &w.sha[..16]);
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[path = "goal_eval.rs"]
mod eval;
pub use eval::{check_metric_lines, checkpoint_values, evaluate, metric_lines, parse_metric, BarEval, Eval, Metric, Partial, Taken, Verdict, MEASUREMENT, METRIC_FORM};

#[cfg(test)]
#[path = "goal_tests.rs"]
mod tests;
