//! The Binding block: what a launched agent is bound by, compiled from the
//! store at launch in a few lines, each a `key@sha12` ref.
//!
//! ```text
//! atlas binding: lane L (recall KEY for full text; these bind this task)
//! named: journal:L:0007@0c1d2e3f4a5b: <title>
//! Scenario: <name>
//! named: journal:M:0003@1d2e3f4a5b6c: <title>
//! handoff: handoff:L:002@9a1c0b2d3e4f: <title>
//! - journal:L:0002@fd529cca502a: "<the owner's own words>"
//! - journal:L:0001@0123456789ab: <headline> [replaces journal:L:0000]
//! open: journal:L:0005@a1b2c3d4e5f6: <question>
//! next: journal:L:0006@b2c3d4e5f6a7: <next step>
//! lane M (the brief names it; recall lane=M for the rest):
//! - journal:M:0009@2e3f4a5b6c7d: <ruling>
//! +3 more decisions: recall lane=L; +2 more named: recall KEY from the brief
//! ```
//!
//! Selection (deterministic: the same store, brief and lane give the same
//! bytes):
//!
//! 1. Keys the brief names ([`declared`]): Gherkin tags ([`tags`]),
//!    `key@hash` refs, bare keys, and a key's abbreviated continuations
//!    (`journal:L:0004 and 0005`, `journal:L:0188-0196`,
//!    `journal:L:0188 through 0196`; [`abbreviated`]). Each is listed
//!    once, key and title, and skipped everywhere else: the brief-wide ones
//!    first, then each scenario's tags under its name.
//! 2. The `lane`'s decisions: its journal `decision` entries and its `rule`
//!    and `decision` records. A superseded one is replaced by its live
//!    successor ([`crate::memory::rules::live_of`]), wherever it is filed,
//!    tagged `replaces KEY`. Newest first, ties by key, at most
//!    [`MAX_DECISIONS`]; a [`Rank`] may reorder the ones inside that cap.
//! 3. Its newest active handoff.
//! 4. Its open questions, newest first, at most [`MAX_OPEN`].
//! 5. Its newest `next` entry, only when it is newer than the handoff.
//! 6. Every other lane a named key is filed in (at most [`MAX_LANES`]): its
//!    newest active handoff and newest [`LANE_DECISIONS`] decisions. A lane
//!    reached only through a key the brief cites in its prose is headed as
//!    context, not as binding; a lane is declared by a tag, or by a key or
//!    its own name on a declaring line (`Given`, `Rulings:`, `Rules:`, the
//!    lines under a `Rulings` heading, a line that says to `recall` a key).
//!
//! The brief's lane is matched without case: a brief that writes it in
//! another case binds the lane as the store spells it.
//!
//! The block is filled in the order named keys, handoff, decisions, open
//! questions, next, other lanes, within [`BUDGET`] bytes and [`LINE_CHARS`]
//! characters a line; what does not fit is counted on one `+N more` line,
//! each count pointing where to recall it. Nothing is cut mid line. The
//! named keys never take the room of the lane's handoff and its newest
//! [`KEPT_DECISIONS`] decisions: those lines are held while the named ones
//! are placed, so a brief with many tags still shows the lane's rulings.
//! With no lane (`""`), only the named keys and their lanes are bound.
//!
//! Pure: it reads the store and writes nothing. The hook that delivers it
//! is wired elsewhere.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use rusqlite::OptionalExtension;

use crate::compact::{self, Target};
use crate::error::Result;
use crate::memory::recall::{newest_handoff, open_in};
use crate::memory::rules;
use crate::memory::{Head, MemoryStore};

/// The most bytes a block takes, its `+N more` line included.
pub const BUDGET: usize = 2400;
/// The most decision lines a block shows for its lane.
pub const MAX_DECISIONS: usize = 6;
/// The most open questions a block shows.
pub const MAX_OPEN: usize = 3;
/// The most named keys a block lists.
pub const MAX_NAMED: usize = 12;
/// The lane's newest decisions that, with its handoff, always keep their
/// room however many keys the brief names.
pub const KEPT_DECISIONS: usize = 3;
/// The most other lanes (of named keys) a block binds.
pub const MAX_LANES: usize = 2;
/// The most decisions a block shows for each other lane.
pub const LANE_DECISIONS: usize = 3;
/// A range of keys longer than this gives its two ends only.
pub const MAX_RANGE: u64 = 12;
/// The most characters one line takes.
pub const LINE_CHARS: usize = 160;
/// The hex digits of a version's sha a line carries.
const SHA_CHARS: usize = 12;

/// A compiled block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Binding {
    /// The text, one entry a line, ending in a newline.
    pub text: String,
    /// The keys shown, in the block's order.
    pub keys: Vec<String>,
    /// Entries selected but not shown (past a cap or the budget), the
    /// count on the `+N more` line.
    pub more: usize,
}

/// A key a brief names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// The key, without a hash.
    pub key: String,
    /// The hash a tag or ref pins it to, lowercase.
    pub hash: Option<String>,
    /// The `Scenario:` or `Spike:` a tag belongs to; `None` brief-wide.
    pub scenario: Option<String>,
    /// Declared: a tag, or a key on a `Given` line. `false` for a key the
    /// brief only cites in its prose.
    pub firm: bool,
}

/// Orders a lane's decisions for a brief. It sees only the decisions that
/// made the cap ([`MAX_DECISIONS`]), newest first, and returns an order of
/// their indices; anything that is not a permutation of `0..cands.len()`
/// is ignored and [`Recency`] stands. It never adds or drops an entry.
pub trait Rank {
    /// The order to show `cands` in, as indices into it.
    fn order(&self, brief: &str, cands: &[Head]) -> Vec<usize>;
}

/// The default [`Rank`]: the order given, newest first.
#[derive(Debug, Clone, Copy, Default)]
pub struct Recency;

impl Rank for Recency {
    fn order(&self, _brief: &str, cands: &[Head]) -> Vec<usize> {
        (0..cands.len()).collect()
    }
}

/// The Binding block for `lane` and a brief, ranked by [`Recency`]. `None`
/// when the lane has nothing to bind that the brief does not already name.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn compile(store: &MemoryStore, brief: &str, lane: &str) -> Result<Option<Binding>> {
    compile_with(store, brief, lane, &Recency)
}

/// [`compile`] with a [`Rank`] for the decisions inside the cap.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn compile_with(store: &MemoryStore, brief: &str, lane: &str, rank: &dyn Rank) -> Result<Option<Binding>> {
    // The brief may write its lane in another case: bind the stored one.
    let lane = &stored_lane(store, lane)?;
    let declared = declared(store, brief)?;
    let named: HashSet<String> = declared.iter().map(|n| n.key.clone()).collect();
    let all = if lane.is_empty() { Vec::new() } else { store.lane_family(lane)? };

    let decisions = decisions(store, lane, &all, &named)?;
    let handoff = newest_handoff(&all).filter(|h| h.status == "active");
    let next = all
        .iter()
        .filter(|h| h.kind == "journal" && h.sub == "next" && h.status == "active")
        .max_by(|a, b| a.time.cmp(&b.time).then_with(|| b.key.cmp(&a.key)))
        .filter(|n| handoff.is_none_or(|h| n.time > h.time))
        .filter(|n| !named.contains(&n.key));
    let open = if lane.is_empty() { HashSet::new() } else { open_in(store, lane)? };
    let mut questions: Vec<&Head> =
        all.iter().filter(|h| h.kind == "journal" && h.sub == "question" && open.contains(&h.key) && !named.contains(&h.key)).collect();
    newest_first(&mut questions, |h| h);

    // The decisions shown: the newest inside the cap, in the ranker's order.
    let cap = decisions.len().min(MAX_DECISIONS);
    let heads: Vec<Head> = decisions[..cap].iter().map(|(h, _)| h.clone()).collect();
    let order = rank.order(brief, &heads);
    let order = if is_permutation(&order, cap) { order } else { (0..cap).collect() };

    // The named keys with their heads, and the other lanes they are filed in.
    // Brief-wide keys first, so none reads as a scenario's; the sort is
    // stable, so each part keeps the brief's order.
    let mut shown_named = Vec::new();
    for n in &declared {
        if let Some(h) = store.head(&n.key)? {
            shown_named.push((n, h));
        }
    }
    shown_named.sort_by_key(|(n, _)| n.scenario.is_some());
    let cue = lane_words(&declaring(brief, &compact::key_prefixes(store)?));
    let lanes = other_lanes(store, lane, &shown_named, &named, &cue)?;

    let handoff_line = handoff.filter(|h| !named.contains(&h.key)).map(|h| (h, line("handoff: ", h, &crate::hooks::pointer_title(h), &[])));
    let decision_lines: Vec<(usize, String)> =
        order.iter().map(|&i| (i, line("- ", &decisions[i].0, &ruling_text(&decisions[i].0), &decisions[i].1))).collect();

    let most = More {
        decisions: decisions.len(),
        open: questions.len(),
        entries: 2,
        named: shown_named.len(),
        lanes: lanes.iter().map(|(l, _, e)| (l.clone(), e.len())).collect(),
    };
    let mut b = Block::new(lane, &most, lanes.iter().map(|(l, _, _)| l.clone()).collect());
    // The lane's handoff and newest decisions keep their room.
    b.held =
        handoff_line.as_ref().map_or(0, |(_, l)| l.len()) + decision_lines.iter().take(KEPT_DECISIONS).map(|(_, l)| l.len()).sum::<usize>();
    let mut scenario: Option<&str> = None;
    for (i, (n, h)) in shown_named.iter().enumerate() {
        if i >= MAX_NAMED {
            b.more.named += 1;
            continue;
        }
        // A scenario's name heads its first line that fits.
        let mut text = String::new();
        if let Some(s) = n.scenario.as_deref().filter(|s| scenario != Some(*s)) {
            text = format!("{}\n", cut(&format!("Scenario: {}", one_line(s)), LINE_CHARS));
        }
        let pin: Vec<String> = n.hash.iter().filter(|p| !h.sha.starts_with(p.as_str())).map(|p| format!("brief pins {p}")).collect();
        text.push_str(&line("named: ", h, &crate::hooks::pointer_title(h), &pin));
        if b.push(Part::Named, &h.key, text) {
            scenario = n.scenario.as_deref();
        }
    }
    b.held = 0;
    if let Some((h, l)) = handoff_line {
        b.push(Part::Other, &h.key, l);
    }
    for (i, l) in decision_lines {
        b.push(Part::Decision, &decisions[i].0.key, l);
    }
    b.more.decisions += decisions.len() - cap;
    for (i, h) in questions.iter().enumerate() {
        if i < MAX_OPEN {
            b.push(Part::Open, &h.key, line("open: ", h, &crate::hooks::pointer_title(h), &[]));
        } else {
            b.more.open += 1;
        }
    }
    if let Some(n) = next {
        b.push(Part::Other, &n.key, line("next: ", n, &crate::hooks::pointer_title(n), &[]));
    }
    for (li, (l, firm, entries)) in lanes.iter().enumerate() {
        // A lane's header goes with its first line that fits. A lane the
        // brief only cites in passing is context, not binding.
        let header = if *firm {
            format!("lane {l} (the brief names it; recall lane={l} for the rest):\n")
        } else {
            format!("lane {l} (cited in passing: context, not binding; recall lane={l} for the rest):\n")
        };
        let mut headed = false;
        for (lead, h, text, tags) in entries {
            let mut t = if headed { String::new() } else { header.clone() };
            t.push_str(&line(lead, h, text, tags));
            headed |= b.push(Part::Lane(li), &h.key, t);
        }
    }
    Ok(b.finish())
}

/// The record keys a brief names, without their hash ([`declared`]).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn named_keys(store: &MemoryStore, brief: &str) -> Result<HashSet<String>> {
    Ok(declared(store, brief)?.into_iter().map(|n| n.key).collect())
}

/// The stored records a brief names, each once, in this order: its
/// Gherkin tags ([`tags`]), its `key@hash` refs ([`compact::refs_in`]), its
/// bare `prefix:rest` words ([`compact::check`]'s bare-key rule), and the
/// abbreviated continuations of a key ([`abbreviated`]). A key the store
/// does not hold is left out.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn declared(store: &MemoryStore, brief: &str) -> Result<Vec<Named>> {
    let prefixes = compact::key_prefixes(store)?;
    let mut cands: Vec<Named> = tags(brief, &prefixes);
    // Keys on a declaring line ([`declaring`]) are declared, like tags;
    // elsewhere, cited.
    let given = declaring(brief, &prefixes);
    let mut firm: HashSet<String> =
        compact::bare_keys(&given, &prefixes).into_iter().map(str::to_string).chain(abbreviated(&given, &prefixes)).collect();
    firm.extend(compact::refs_in(&given).into_iter().filter_map(|r| match r.target {
        Target::Record(k) => Some(k),
        _ => None,
    }));
    for r in compact::refs_in(brief) {
        if let Target::Record(k) = r.target {
            let firm = firm.contains(&k);
            cands.push(Named { key: k, hash: Some(r.hash), scenario: None, firm });
        }
    }
    let bare = compact::bare_keys(brief, &prefixes).into_iter().map(str::to_string).chain(abbreviated(brief, &prefixes));
    cands.extend(bare.map(|key| Named { firm: firm.contains(&key), key, hash: None, scenario: None }));
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for n in cands {
        if seen.contains(&n.key) || store.head(&n.key)?.is_none() {
            continue;
        }
        seen.insert(n.key.clone());
        out.push(n);
    }
    Ok(out)
}

/// The lines of a brief that declare what binds it, joined by newlines:
/// `Given` lines, `Rulings:` and `Rules:` lines, the lines under a heading
/// that starts `Ruling` (to the next heading), and any line that says to
/// `recall` a key (`recall journal:L:0012`, `recall key=journal:L:0012`).
/// A key or a lane written there binds; elsewhere it is cited in passing.
fn declaring(brief: &str, prefixes: &HashSet<String>) -> String {
    let mut out = Vec::new();
    let mut under = false;
    for l in brief.lines() {
        let t = l.trim().trim_start_matches(['-', '*', '>', ' ']);
        if let Some(h) = l.trim_start().strip_prefix('#') {
            under = h.trim_start_matches(['#', ' ', '*']).get(..6).is_some_and(|w| w.eq_ignore_ascii_case("ruling"));
        }
        let words: Vec<&str> = t.split_whitespace().map(unwrapped).collect();
        let recalls = words.windows(2).any(|w| {
            let k = w[1].strip_prefix("key=").unwrap_or(w[1]);
            w[0].eq_ignore_ascii_case("recall") && k.split_once(':').is_some_and(|(p, _)| prefixes.contains(p))
        });
        if under || recalls || ["Given ", "Rulings:", "Ruling:", "Rules:"].iter().any(|p| t.starts_with(p)) {
            out.push(l);
        }
    }
    out.join("\n")
}

/// The words of `text` a lane name could be, uppercase (`lane=M` and
/// `journal:M:0001` both give `M`).
fn lane_words(text: &str) -> HashSet<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_uppercase)
        .collect()
}

/// The lane as the store spells it, when the store holds it in another
/// case (or only its numbered sub-lanes); else `lane` as given.
fn stored_lane(store: &MemoryStore, lane: &str) -> Result<String> {
    if lane.is_empty() {
        return Ok(String::new());
    }
    let found: Option<String> = store
        .conn()
        .query_row(
            "SELECT scope FROM records WHERE scope = ?1 COLLATE NOCASE OR lower(substr(scope, 1, length(?1) + 1)) = lower(?1) || '/'
             ORDER BY scope = ?1 DESC, length(scope), rid LIMIT 1",
            [lane],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found
        .map(|s| crate::lanes::parent_lane(&s).to_string())
        .filter(|s| crate::lanes::same_lane(s, lane))
        .unwrap_or_else(|| lane.to_string()))
}

/// The Gherkin tags of a brief that name a key with one of `prefixes`:
/// whitespace-free words that start with `@` (`@journal:L:0012`), a pin
/// written `@key#hash` or `@key@hash`. Tags on the lines just above a
/// `Scenario:` or `Spike:` line, and tags inside one, belong to it; the
/// rest (above `Feature:`, in its header, after a `Rules:` line or a
/// markdown heading that ends the scenarios) are brief-wide. A tag may sit in
/// backticks, quotes or brackets (`` `@journal:L:0012` ``, `(@key)`). A
/// word with an `@` inside it (an email) is no tag, nor is one whose prefix
/// is not known.
pub fn tags(brief: &str, prefixes: &HashSet<String>) -> Vec<Named> {
    let mut out = Vec::new();
    let mut pending: Vec<(String, Option<String>)> = Vec::new();
    let mut current: Option<String> = None;
    let named =
        |(key, hash): (String, Option<String>), scenario: &Option<String>| Named { key, hash, scenario: scenario.clone(), firm: true };
    for l in brief.lines() {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        let words: Vec<&str> = t.split_whitespace().collect();
        if words.iter().all(|w| unwrapped(w).starts_with('@')) {
            pending.extend(words.iter().filter_map(|w| tag(w, prefixes)));
            continue;
        }
        if let Some(name) = scenario_name(t) {
            current = Some(name);
        } else if t.starts_with('#') || ["Feature:", "Background:", "Rules:"].iter().any(|p| t.starts_with(p)) {
            current = None;
        }
        // Tags just above a scenario, or inside one, are its own; above
        // anything else outside a scenario, brief-wide.
        out.extend(pending.drain(..).map(|p| named(p, &current)));
        out.extend(words.iter().filter_map(|w| tag(w, prefixes)).map(|p| named(p, &current)));
    }
    out.extend(pending.into_iter().map(|p| named(p, &current)));
    out
}

/// A word without the backticks, quotes or brackets around it.
fn unwrapped(word: &str) -> &str {
    word.trim_start_matches(['(', '[', '`', '"', '\'']).trim_end_matches([',', '.', ';', ':', ')', ']', '`', '"', '\''])
}

/// The name of a `Scenario:`, `Scenario Outline:` or `Spike:` line.
fn scenario_name(line: &str) -> Option<String> {
    let t = line.trim_start_matches("- ");
    ["Scenario Outline:", "Scenario:", "Spike:"].iter().find_map(|p| t.strip_prefix(p)).map(|n| n.trim().to_string())
}

/// One tag word: its key and pinned hash, when its prefix is known.
fn tag(word: &str, prefixes: &HashSet<String>) -> Option<(String, Option<String>)> {
    let w = unwrapped(word).strip_prefix('@')?;
    let (key, hash) = match w.split_once('#').or_else(|| w.split_once('@')) {
        Some((k, h)) => (k, Some(h)),
        None => (w, None),
    };
    let hash = hash.filter(|h| h.len() >= 4 && h.chars().all(|c| c.is_ascii_hexdigit())).map(str::to_ascii_lowercase);
    let (p, rest) = key.split_once(':')?;
    (!rest.is_empty() && prefixes.contains(p)).then(|| (key.to_string(), hash))
}

/// Keys a brief writes short after a full one, in the full key's sentence
/// on the same line: `journal:L:0004 and 0005` names `journal:L:0005`, and
/// a range `journal:L:0188-0196` or `journal:L:0188 through 0196` (or
/// `0188-0196` after a key) every key in it, or its two ends past
/// [`MAX_RANGE`]. A number continues the last full key when it has the
/// same width and is no year (1900 to 2099) and no `#` comes before it:
///
/// - zero-padded (`0508`), it names the key anywhere later in the sentence,
///   a plain word or a `;` between them or not (`recall journal:L:0507 and
///   0508 from atlas`, `journal:L:0035; the base ruling is 0031`): a count,
///   a year or a PR number is never written with a leading zero;
/// - without a leading zero (`1003`), it continues only a key written
///   without one (`journal:L:1002`), only in a list (a separator before it:
///   `,`, `/`, `&`, `and`, `or`, `through`, `to`), and not when a plain
///   word follows it (`journal:L:1002 and 1679 tokens` is a count).
///
/// `to` joins a list; only `-` and `through` make a range. An aside in
/// brackets after an item (`0023 (the plan), 0024`) does not break the
/// list; backticks and quotes are ignored. A sentence's end (`.`, `!`, `?`
/// before a space) or the line's end ends it all. The caller keeps the
/// stored keys only.
pub fn abbreviated(brief: &str, prefixes: &HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for line in brief.lines() {
        let mut list = List::default();
        let mut word = String::new();
        let chars: Vec<char> = line.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':') {
                word.push(c);
                continue;
            }
            if !word.is_empty() {
                list.word(&std::mem::take(&mut word), prefixes, &mut out);
            }
            list.mark(c, chars.get(i + 1).copied(), &mut out);
        }
        if !word.is_empty() {
            list.word(&word, prefixes, &mut out);
        }
        list.commit(&mut out);
    }
    out
}

/// Words that join a list's items.
const LIST_WORDS: [&str; 4] = ["and", "or", "through", "to"];

/// A full key's sentence being read along one line ([`abbreviated`]).
#[derive(Default)]
struct List {
    /// The last full key's head (`journal:L:`), its number's width and
    /// whether that number is zero-padded.
    base: Option<(String, usize, bool)>,
    /// The bracket depth the key's sentence runs at, while it runs.
    at: Option<usize>,
    depth: usize,
    /// Still a list: only items, separators and asides since the key.
    strict: bool,
    /// A separator since the last item.
    sep: bool,
    /// The last item's number, while only spaces follow it: a `through`
    /// range starts there.
    last: Option<String>,
    /// A `through` range's start, waiting for its end.
    from: Option<String>,
    /// A number without a leading zero, kept unless a plain word follows it.
    held: Option<(String, String)>,
    /// The next word follows a `#`.
    hash: bool,
}

impl List {
    fn here(&self) -> bool {
        self.at == Some(self.depth)
    }

    fn word(&mut self, word: &str, prefixes: &HashSet<String>, out: &mut Vec<String>) {
        let hash = std::mem::take(&mut self.hash);
        let w = word.trim_end_matches([':', '-']);
        if let Some((head, num)) = w.rsplit_once(':') {
            let (a, b) = num.split_once('-').unwrap_or((num, num));
            if head.split(':').next().is_some_and(|p| prefixes.contains(p)) && digits(a) {
                self.commit(out);
                let head = format!("{head}:");
                if a != b {
                    expand(&head, a, b, out);
                }
                self.base = Some((head, a.len(), padded(a)));
                self.at = Some(self.depth);
                self.strict = true;
                self.sep = false;
                self.last = Some(b.to_string());
                self.from = None;
                return;
            }
        }
        let (Some(at), Some((head, width, base_padded))) = (self.at, self.base.clone()) else { return };
        if w.is_empty() || self.depth < at {
            return;
        }
        let here = self.here();
        let lower = w.to_ascii_lowercase();
        if here && LIST_WORDS.contains(&lower.as_str()) {
            if lower == "through" && self.last.is_some() {
                self.from = self.held.take().map(|(a, _)| a).or(self.last.take());
            } else {
                self.commit(out);
                self.from = None;
            }
            self.last = None;
            self.sep = true;
            return;
        }
        let (a, b) = w.split_once('-').unwrap_or((w, w));
        let fits = !hash && digits(a) && a.len() == width && !year(a) && !year(b);
        if fits && here && self.from.is_some() {
            let from = self.from.take().unwrap_or_default();
            expand(&head, &from, b, out);
            self.last = Some(b.to_string());
            self.sep = false;
            return;
        }
        if fits && padded(a) {
            self.commit(out);
            expand(&head, a, b, out);
            if here {
                self.last = Some(b.to_string());
                self.sep = false;
            }
            return;
        }
        if fits && here && !base_padded && self.strict && self.sep && self.held.is_none() {
            self.held = Some((a.to_string(), b.to_string()));
            self.last = Some(b.to_string());
            self.sep = false;
            return;
        }
        if here {
            // Any other word ends the list, and a number held right before
            // it was a count; the sentence runs on.
            self.held = None;
            self.strict = false;
            self.sep = false;
            self.last = None;
            self.from = None;
        }
    }

    /// A character between words; `next` is the one after it.
    fn mark(&mut self, c: char, next: Option<char>, out: &mut Vec<String>) {
        self.hash = c == '#';
        let here = self.here();
        match c {
            '`' | '"' | '\'' | '*' => {}
            c if c.is_whitespace() => {}
            '(' | '[' => {
                if here {
                    self.commit(out);
                    self.last = None;
                }
                self.depth += 1;
            }
            ')' | ']' => {
                self.depth = self.depth.saturating_sub(1);
                if self.at.is_some_and(|d| d > self.depth) {
                    self.commit(out);
                    self.at = None;
                }
            }
            ',' | '/' | '&' if here => {
                self.commit(out);
                self.sep = true;
                self.last = None;
                self.from = None;
            }
            '.' | '!' | '?' if here && next.is_none_or(char::is_whitespace) => {
                self.commit(out);
                self.at = None;
            }
            _ if here => {
                self.commit(out);
                self.strict = false;
                self.sep = false;
                self.last = None;
                self.from = None;
            }
            _ => {}
        }
    }

    /// Names the held number.
    fn commit(&mut self, out: &mut Vec<String>) {
        if let (Some((a, b)), Some((head, _, _))) = (self.held.take(), &self.base) {
            expand(head, &a, &b, out);
        }
    }
}

/// Zero-padded (`0508`): never how a count, a year or a PR number is
/// written.
fn padded(s: &str) -> bool {
    s.len() > 1 && s.starts_with('0')
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// A four-digit year, 1900 to 2099.
fn year(s: &str) -> bool {
    s.len() == 4 && s.parse::<u32>().is_ok_and(|y| (1900..=2099).contains(&y))
}

/// `head` + every number from `a` to `b` (same width, zero-padded), or the
/// two ends of a range past [`MAX_RANGE`]; nothing for a bad range.
fn expand(head: &str, a: &str, b: &str, out: &mut Vec<String>) {
    if !digits(a) || !digits(b) || a.len() != b.len() {
        return;
    }
    let (Ok(x), Ok(y)) = (a.parse::<u64>(), b.parse::<u64>()) else { return };
    if y < x {
        return;
    }
    let w = a.len();
    if y - x >= MAX_RANGE {
        out.push(format!("{head}{a}"));
        out.push(format!("{head}{b}"));
        return;
    }
    out.extend((x..=y).map(|n| format!("{head}{n:0w$}")));
}

/// A line's lead, head, text and tags.
type Entry = (&'static str, Head, String, Vec<String>);

/// The lanes other than `lane` that named journal entries and handoffs are
/// filed in, in the order first named, at most [`MAX_LANES`], each with
/// whether it is declared (a declared key, [`Named::firm`], is filed in it,
/// or a declaring line names it: `cue`, uppercase words), its newest
/// active handoff and its newest [`LANE_DECISIONS`] decisions not named.
/// Lanes compare without case.
fn other_lanes(
    store: &MemoryStore,
    lane: &str,
    named: &[(&Named, Head)],
    skip: &HashSet<String>,
    cue: &HashSet<String>,
) -> Result<Vec<(String, bool, Vec<Entry>)>> {
    let mut out: Vec<(String, bool, Vec<Entry>)> = Vec::new();
    let lane_of = |h: &Head| crate::lanes::parent_lane(&h.scope).to_string();
    for (_, h) in named {
        let l = lane_of(h);
        if !matches!(h.kind.as_str(), "journal" | "handoff")
            || l.is_empty()
            || crate::lanes::same_lane(&l, lane)
            || out.iter().any(|(x, _, _)| crate::lanes::same_lane(x, &l))
        {
            continue;
        }
        if out.len() == MAX_LANES {
            break;
        }
        let firm = cue.contains(&l.to_ascii_uppercase())
            || named
                .iter()
                .any(|(n, h)| n.firm && matches!(h.kind.as_str(), "journal" | "handoff") && crate::lanes::same_lane(&lane_of(h), &l));
        let all = store.lane_family(&l)?;
        let mut entries: Vec<Entry> = Vec::new();
        if let Some(h) = newest_handoff(&all).filter(|h| h.status == "active" && !skip.contains(&h.key)) {
            entries.push(("handoff: ", h.clone(), crate::hooks::pointer_title(h), Vec::new()));
        }
        for (h, tags) in decisions(store, &l, &all, skip)?.into_iter().take(LANE_DECISIONS) {
            let t = ruling_text(&h);
            entries.push(("- ", h, t, tags));
        }
        if !entries.is_empty() {
            out.push((l, firm, entries));
        }
    }
    Ok(out)
}

/// The lane's live decisions, each with its tags (`replaces KEY`), newest
/// first, ties by key, the named ones dropped.
fn decisions(store: &MemoryStore, lane: &str, all: &[Head], named: &HashSet<String>) -> Result<Vec<(Head, Vec<String>)>> {
    let mut found: Vec<Head> = all.iter().filter(|h| h.kind == "journal" && h.sub == "decision").cloned().collect();
    if !lane.is_empty() {
        found.extend(store.heads(Some("rule"), Some(lane))?);
        found.extend(store.heads(Some("decision"), Some(lane))?);
    }
    let mut live: BTreeMap<String, (Head, BTreeSet<String>)> = BTreeMap::new();
    for h in found {
        let (now, tag) = match h.status.as_str() {
            "active" => (h, None),
            "superseded" => match rules::live_of(store, &h.key)? {
                Some(now) => (now, Some(format!("replaces {}", h.key))),
                None => continue,
            },
            _ => continue,
        };
        let e = live.entry(now.key.clone()).or_insert_with(|| (now, BTreeSet::new()));
        e.1.extend(tag);
    }
    let mut out: Vec<(Head, Vec<String>)> =
        live.into_values().filter(|(h, _)| !named.contains(&h.key)).map(|(h, t)| (h, t.into_iter().collect())).collect();
    newest_first(&mut out, |(h, _)| h);
    Ok(out)
}

/// Sorts by time, newest first, then by key.
fn newest_first<T>(v: &mut [T], head: impl Fn(&T) -> &Head) {
    v.sort_by(|a, b| {
        let (a, b) = (head(a), head(b));
        b.time.cmp(&a.time).then_with(|| a.key.cmp(&b.key))
    });
}

fn is_permutation(order: &[usize], n: usize) -> bool {
    let mut seen = vec![false; n];
    order.len() == n && order.iter().all(|&i| i < n && !std::mem::replace(&mut seen[i], true))
}

/// A ruling's text: a managed record's own headline; otherwise the first
/// quoted span of its body (the owner's own words) when there is one, else
/// its pointer title.
fn ruling_text(h: &Head) -> String {
    if rules::managed_kind(&h.kind) {
        return rules::headline(h);
    }
    first_quote(&h.body).unwrap_or_else(|| crate::hooks::pointer_title(h))
}

/// The first span in straight or curly double quotes of two words or more,
/// on one line, in its quotes.
fn first_quote(body: &str) -> Option<String> {
    let mut rest = body;
    while let Some(open) = rest.find(['"', '\u{201c}']) {
        let after = &rest[open + rest[open..].chars().next().map_or(1, char::len_utf8)..];
        let close = after.find(['"', '\u{201d}'])?;
        let span = one_line(&after[..close]);
        if span.split(' ').count() >= 2 {
            return Some(format!("\"{span}\""));
        }
        rest = &after[close + after[close..].chars().next().map_or(1, char::len_utf8)..];
    }
    None
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One line, `<lead><key>@<sha12>: <text>[ [tag]...]`, the text cut to
/// keep it within [`LINE_CHARS`]; the lead, ref and tags are never cut.
fn line(lead: &str, h: &Head, text: &str, tags: &[String]) -> String {
    let sha = h.sha.get(..SHA_CHARS).unwrap_or(&h.sha);
    let head = format!("{lead}{}@{sha}: ", h.key);
    let tail: String = tags.iter().map(|t| format!(" [{t}]")).collect();
    let room = LINE_CHARS.saturating_sub(head.chars().count() + tail.chars().count());
    format!("{head}{}{tail}\n", cut(&one_line(text), room))
}

/// `text` within `room` characters, ending `...` when cut.
fn cut(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_string();
    }
    let kept: String = text.chars().take(room.saturating_sub(3)).collect();
    format!("{}...", kept.trim_end())
}

/// Which `+N more` count an entry that does not fit goes to.
#[derive(Clone, Copy)]
enum Part {
    /// One of the lane's decisions.
    Decision,
    /// One of the lane's open questions.
    Open,
    /// The lane's handoff or next step.
    Other,
    /// A key the brief names.
    Named,
    /// An entry of the other lane at this index.
    Lane(usize),
}

/// What a block counts as not shown, each where to recall it from.
#[derive(Debug, Clone, Default)]
struct More {
    decisions: usize,
    open: usize,
    /// The lane's handoff and next step.
    entries: usize,
    /// Keys the brief names: the brief holds them.
    named: usize,
    /// Each other lane's entries.
    lanes: Vec<(String, usize)>,
}

impl More {
    fn total(&self) -> usize {
        self.decisions + self.open + self.entries + self.named + self.lanes.iter().map(|(_, n)| n).sum::<usize>()
    }
}

/// A block being filled.
struct Block<'a> {
    lane: &'a str,
    text: String,
    keys: Vec<String>,
    /// Bytes kept free for the `+N more` line at its longest.
    reserve: usize,
    /// Bytes kept free for lines placed later (the lane's handoff and
    /// newest decisions, while the named keys are placed).
    held: usize,
    more: More,
}

impl<'a> Block<'a> {
    /// `most`: every count at its largest, for the reserve; `lanes`: the
    /// other lanes, in [`Part::Lane`]'s order.
    fn new(lane: &'a str, most: &More, lanes: Vec<String>) -> Block<'a> {
        let reserve = more_line(lane, most).len();
        let text = if lane.is_empty() {
            "atlas binding (recall KEY for full text; these bind this task)\n".to_string()
        } else {
            format!("atlas binding: lane {lane} (recall KEY for full text; these bind this task)\n")
        };
        let more = More { lanes: lanes.into_iter().map(|l| (l, 0)).collect(), ..More::default() };
        Block { lane, text, keys: Vec::new(), reserve, held: 0, more }
    }

    /// Adds `line` when it fits; whether it did.
    fn push(&mut self, part: Part, key: &str, line: String) -> bool {
        if self.text.len() + line.len() + self.reserve + self.held <= BUDGET {
            self.text.push_str(&line);
            self.keys.push(key.to_string());
            return true;
        }
        match part {
            Part::Decision => self.more.decisions += 1,
            Part::Open => self.more.open += 1,
            Part::Other => self.more.entries += 1,
            Part::Named => self.more.named += 1,
            Part::Lane(i) => self.more.lanes[i].1 += 1,
        }
        false
    }

    fn finish(mut self) -> Option<Binding> {
        let more = self.more.total();
        if self.keys.is_empty() && more == 0 {
            return None;
        }
        self.text.push_str(&more_line(self.lane, &self.more));
        Some(Binding { text: self.text, keys: self.keys, more })
    }
}

/// The `+N more` line: each count with where to recall it, `; ` between
/// them (`+2 more decisions, +1 more open: recall lane=L; +3 more of lane
/// M: recall lane=M; +4 more named: recall KEY from the brief`), or nothing
/// when every count is 0.
fn more_line(lane: &str, m: &More) -> String {
    let own: Vec<String> = [(m.decisions, "decisions"), (m.open, "open"), (m.entries, "entries")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, w)| format!("+{n} more {w}"))
        .collect();
    let mut parts = Vec::new();
    if !own.is_empty() {
        parts.push(format!("{}: recall lane={lane}", own.join(", ")));
    }
    parts.extend(m.lanes.iter().filter(|(_, n)| *n > 0).map(|(l, n)| format!("+{n} more of lane {l}: recall lane={l}")));
    if m.named > 0 {
        parts.push(format!("+{} more named: recall KEY from the brief", m.named));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{}\n", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_needs_two_words_and_may_be_curly() {
        assert_eq!(first_quote("set \"log\" then \u{201c}bound the  cache\u{201d}"), Some("\"bound the cache\"".into()));
        assert_eq!(first_quote("no quotes"), None);
        assert_eq!(first_quote("unclosed \"half a quote"), None);
    }

    #[test]
    fn permutations() {
        assert!(is_permutation(&[2, 0, 1], 3));
        assert!(!is_permutation(&[0, 0, 1], 3));
        assert!(!is_permutation(&[0, 1], 3));
        assert!(!is_permutation(&[0, 1, 3], 3));
        assert!(is_permutation(&[], 0));
    }

    #[test]
    fn the_more_line_names_only_what_was_counted() {
        let m = |decisions, open, entries| More { decisions, open, entries, ..More::default() };
        assert_eq!(more_line("L", &m(4, 0, 0)), "+4 more decisions: recall lane=L\n");
        assert_eq!(more_line("L", &m(1, 2, 1)), "+1 more decisions, +2 more open, +1 more entries: recall lane=L\n");
        assert_eq!(more_line("L", &m(0, 0, 0)), "");
    }

    #[test]
    fn each_count_points_where_to_recall_it() {
        let m = More { decisions: 2, named: 3, lanes: vec![("M".into(), 1), ("N".into(), 0)], ..More::default() };
        assert_eq!(
            more_line("L", &m),
            "+2 more decisions: recall lane=L; +1 more of lane M: recall lane=M; +3 more named: recall KEY from the brief\n"
        );
        // With no lane, named keys past the cap are not "over the budget".
        let m = More { named: 1, ..More::default() };
        assert_eq!(more_line("", &m), "+1 more named: recall KEY from the brief\n");
    }
}
