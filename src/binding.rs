//! The Binding block: what a launched agent is bound by in its lane, compiled
//! from the store at launch in a few lines, each a `key@sha12` ref.
//!
//! ```text
//! atlas binding: lane L (recall KEY for full text; these bind this task)
//! handoff: handoff:L:002@9a1c0b2d3e4f: <title>
//! - journal:L:0002@fd529cca502a: "<the owner's own words>"
//! - journal:L:0001@0123456789ab: <headline> [replaces journal:L:0000]
//! open: journal:L:0005@a1b2c3d4e5f6: <question>
//! next: journal:L:0006@b2c3d4e5f6a7: <next step>
//! +3 more decisions: recall lane=L
//! ```
//!
//! Selection (deterministic: the same store, brief and lane give the same
//! bytes):
//!
//! 1. Keys the brief names ([`named_keys`]) are skipped everywhere: the
//!    brief already carries them.
//! 2. Decisions: the lane's journal `decision` entries and its `rule` and
//!    `decision` records. A superseded one is replaced by its live successor
//!    ([`crate::memory::rules::live_of`]), wherever it is filed, tagged
//!    `replaces KEY`. Newest first, ties by key, at most [`MAX_DECISIONS`];
//!    a [`Rank`] may reorder the ones inside that cap.
//! 3. The newest active handoff.
//! 4. Open questions, newest first, at most [`MAX_OPEN`].
//! 5. The newest `next` entry, only when it is newer than the handoff.
//!
//! The block is filled in the order handoff, decisions, open questions,
//! next, within [`BUDGET`] bytes and [`LINE_CHARS`] characters a line;
//! what does not fit is counted on one `+N more` line. Nothing is cut mid
//! line.
//!
//! Pure: it reads the store and writes nothing. The hook that delivers it
//! is wired elsewhere.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::compact::{self, Target};
use crate::error::Result;
use crate::memory::recall::{newest_handoff, open_in};
use crate::memory::rules;
use crate::memory::{Head, MemoryStore};

/// The most bytes a block takes, its `+N more` line included.
pub const BUDGET: usize = 1600;
/// The most decision lines a block shows.
pub const MAX_DECISIONS: usize = 6;
/// The most open questions a block shows.
pub const MAX_OPEN: usize = 3;
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
    let named = named_keys(store, brief)?;
    let all = store.lane_family(lane)?;

    let decisions = decisions(store, lane, &all, &named)?;
    let handoff = newest_handoff(&all).filter(|h| h.status == "active");
    let next = all
        .iter()
        .filter(|h| h.kind == "journal" && h.sub == "next" && h.status == "active")
        .max_by(|a, b| a.time.cmp(&b.time).then_with(|| b.key.cmp(&a.key)))
        .filter(|n| handoff.is_none_or(|h| n.time > h.time))
        .filter(|n| !named.contains(&n.key));
    let open = open_in(store, lane)?;
    let mut questions: Vec<&Head> = all.iter().filter(|h| h.kind == "journal" && h.sub == "question" && open.contains(&h.key) && !named.contains(&h.key)).collect();
    newest_first(&mut questions, |h| h);

    // The decisions shown: the newest inside the cap, in the ranker's order.
    let cap = decisions.len().min(MAX_DECISIONS);
    let heads: Vec<Head> = decisions[..cap].iter().map(|(h, _)| h.clone()).collect();
    let order = rank.order(brief, &heads);
    let order = if is_permutation(&order, cap) { order } else { (0..cap).collect() };

    let mut b = Block::new(lane, decisions.len(), questions.len());
    if let Some(h) = handoff.filter(|h| !named.contains(&h.key)) {
        b.push(Part::Other, &h.key, line("handoff: ", h, &crate::hooks::pointer_title(h), &[]));
    }
    for i in order {
        let (h, tags) = &decisions[i];
        b.push(Part::Decision, &h.key, line("- ", h, &ruling_text(h), tags));
    }
    b.more_decisions += decisions.len() - cap;
    for (i, h) in questions.iter().enumerate() {
        if i < MAX_OPEN {
            b.push(Part::Open, &h.key, line("open: ", h, &crate::hooks::pointer_title(h), &[]));
        } else {
            b.more_open += 1;
        }
    }
    if let Some(n) = next {
        b.push(Part::Other, &n.key, line("next: ", n, &crate::hooks::pointer_title(n), &[]));
    }
    Ok(b.finish())
}

/// The record keys a brief names, without their hash: every `key@hash` ref
/// ([`compact::refs_in`]) and every bare `prefix:rest` word that names a
/// stored record ([`compact::check`]'s bare-key rule).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn named_keys(store: &MemoryStore, brief: &str) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for r in compact::refs_in(brief) {
        if let Target::Record(k) = r.target {
            if store.head(&k)?.is_some() {
                out.insert(k);
            }
        }
    }
    let prefixes = compact::key_prefixes(store)?;
    for w in compact::bare_keys(brief, &prefixes) {
        if store.head(w)?.is_some() {
            out.insert(w.to_string());
        }
    }
    Ok(out)
}

/// The lane's live decisions, each with its tags (`replaces KEY`), newest
/// first, ties by key, the named ones dropped.
fn decisions(store: &MemoryStore, lane: &str, all: &[Head], named: &HashSet<String>) -> Result<Vec<(Head, Vec<String>)>> {
    let mut found: Vec<Head> = all.iter().filter(|h| h.kind == "journal" && h.sub == "decision").cloned().collect();
    found.extend(store.heads(Some("rule"), Some(lane))?);
    found.extend(store.heads(Some("decision"), Some(lane))?);
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
    let mut out: Vec<(Head, Vec<String>)> = live.into_values().filter(|(h, _)| !named.contains(&h.key)).map(|(h, t)| (h, t.into_iter().collect())).collect();
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
    let text = one_line(text);
    let room = LINE_CHARS.saturating_sub(head.chars().count() + tail.chars().count());
    let text = if text.chars().count() > room {
        let cut: String = text.chars().take(room.saturating_sub(3)).collect();
        format!("{}...", cut.trim_end())
    } else {
        text
    };
    format!("{head}{text}{tail}\n")
}

/// Which `+N more` count an entry that does not fit goes to.
#[derive(Clone, Copy)]
enum Part {
    Decision,
    Open,
    Other,
}

/// A block being filled.
struct Block<'a> {
    lane: &'a str,
    text: String,
    keys: Vec<String>,
    /// Bytes kept free for the `+N more` line at its longest.
    reserve: usize,
    more_decisions: usize,
    more_open: usize,
    more_other: usize,
}

impl<'a> Block<'a> {
    fn new(lane: &'a str, decisions: usize, questions: usize) -> Block<'a> {
        // At its longest every decision and question and both single
        // entries are counted.
        let reserve = more_line(lane, decisions, questions, 2).len();
        let text = format!("atlas binding: lane {lane} (recall KEY for full text; these bind this task)\n");
        Block { lane, text, keys: Vec::new(), reserve, more_decisions: 0, more_open: 0, more_other: 0 }
    }

    fn push(&mut self, part: Part, key: &str, line: String) {
        if self.text.len() + line.len() + self.reserve <= BUDGET {
            self.text.push_str(&line);
            self.keys.push(key.to_string());
            return;
        }
        match part {
            Part::Decision => self.more_decisions += 1,
            Part::Open => self.more_open += 1,
            Part::Other => self.more_other += 1,
        }
    }

    fn finish(mut self) -> Option<Binding> {
        let more = self.more_decisions + self.more_open + self.more_other;
        if self.keys.is_empty() && more == 0 {
            return None;
        }
        self.text.push_str(&more_line(self.lane, self.more_decisions, self.more_open, self.more_other));
        Some(Binding { text: self.text, keys: self.keys, more })
    }
}

/// `+N more decisions, +M more open: recall lane=L`, or nothing when all
/// counts are 0.
fn more_line(lane: &str, decisions: usize, open: usize, other: usize) -> String {
    let parts: Vec<String> = [(decisions, "decisions"), (open, "open"), (other, "entries")].iter().filter(|(n, _)| *n > 0).map(|(n, w)| format!("+{n} more {w}")).collect();
    if parts.is_empty() {
        return String::new();
    }
    format!("{}: recall lane={lane}\n", parts.join(", "))
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
        assert_eq!(more_line("L", 4, 0, 0), "+4 more decisions: recall lane=L\n");
        assert_eq!(more_line("L", 1, 2, 1), "+1 more decisions, +2 more open, +1 more entries: recall lane=L\n");
        assert_eq!(more_line("L", 0, 0, 0), "");
    }
}
