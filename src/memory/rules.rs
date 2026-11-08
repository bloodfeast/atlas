//! Which rulings replace which: candidates, the few pairs that can be read
//! from the text, and one-line headlines.
//!
//! Measured on a real 1,582-item rules file: a reversal word finds most
//! reversals (77 of 95 labelled, one reader), but only a key or a quote held by exactly one other item names
//! the record it reverses, about 1 in 12. Everything else is asked. Whether a
//! reversal is whole or partial cannot be read at all, so nothing here changes
//! a record's status: a found pair is a link (see [`AMENDS`]), and `superseded`
//! comes only from an answer or from an explicit `--supersedes`.
//!
//! Topic-with-later-date and "the line above" are not used to join records.
//! Both joined wrongly when measured. They may only suggest an answer.

use std::process::ExitCode;

use crate::error::{Error, Result};

use super::{Head, MemoryStore};

/// Ref kind on the later record: it amends the target key.
pub const AMENDS: &str = "amends";
/// Ref kind on the earlier record: the target key amends it.
pub const AMENDED_BY: &str = "amended_by";
/// Ref kind on a candidate whose question has an answer, so it is not asked
/// again; the target is the answer's name.
pub const CHECKED: &str = "rule_checked";

/// The source and export of a project's root `CLAUDE.md` under a yes.
pub const CLAUDE_MD: &str = "claude-md";

/// What `memory.rules` asks of atlas. Experimental, off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Unset, the default: what the configuration names by hand is synced
    /// and recalled as it always was.
    #[default]
    Today,
    /// `true`: the project's memory folder and root `CLAUDE.md` are found, a hit
    /// on a replaced ruling answers with the live one, and hook pointers on
    /// rules and memories are headlines.
    Live,
    /// `false`, set by hand: no sync of rule files or memory folders and no query recall of
    /// their records. The store is kept, and `recall {key}` still reads it.
    Off,
}

impl Mode {
    /// The mode of a `memory.rules` value.
    pub fn of(v: Option<bool>) -> Mode {
        match v {
            None => Mode::Today,
            Some(true) => Mode::Live,
            Some(false) => Mode::Off,
        }
    }
}

/// Whether a record kind is what `memory.rules` manages. Journal entries and
/// handoffs are not: their status drives the queue.
pub fn managed_kind(kind: &str) -> bool {
    matches!(kind, "rule" | "memory")
}

/// Words that make an item a candidate. Lowercase, whole words.
///
/// A family stays only if the items it alone hits are more often a reversal
/// than not, on a labelled rules file. "Corrected", "overrides" and "replaces"
/// failed: they mostly describe code. The real reversals they held are not
/// asked unless added by hand.
const WORDS: &[&str] = &[
    "reverses",
    "reversed",
    "reversing",
    "reversal",
    "supersedes",
    "superseded",
    "superseding",
    "amends",
    "amended",
    "amending",
    "struck",
    "scraps",
    "scrapped",
    "withdrawn",
    "rescinds",
    "rescinded",
    "refines",
    "refined",
];

/// How far from the word a key or quote may sit, in bytes.
const WINDOW: usize = 70;
/// The shortest quote that can name a record, in bytes.
const MIN_QUOTE: usize = 6;
/// The longest headline text, in characters (the key and date are extra).
pub const HEADLINE_CHARS: usize = 56;

/// A candidate's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Its text names this key.
    Key(String),
    /// A quote beside the word is held by this one other record.
    Quote(String),
    /// Nothing resolves: the owner is asked.
    Ask,
}

/// A record that reads as a reversal or an amendment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The record.
    pub key: String,
    /// The word that made it one.
    pub word: String,
    /// What its text names.
    pub target: Target,
}

/// Byte ranges of the candidate words in `text`, in order.
pub fn word_hits(text: &str) -> Vec<(usize, usize)> {
    let lower = text.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_alphabetic() {
            i += 1;
        }
        // A hyphen or digit joined to the word makes it another word
        // (`self-corrected`, `replaces2`).
        let glued =
            (start > 0 && matches!(b[start - 1], b'-' | b'_' | b'0'..=b'9')) || (i < b.len() && matches!(b[i], b'-' | b'_' | b'0'..=b'9'));
        if !glued && WORDS.contains(&&lower[start..i]) {
            out.push((start, i));
        }
    }
    out
}

fn floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Double-quoted spans of `win`, straight or curly.
fn quotes(win: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = win;
    loop {
        let Some((at, close, len)) = rest.char_indices().find_map(|(i, c)| match c {
            '"' => Some((i, '"', 1)),
            '\u{201c}' => Some((i, '\u{201d}', 3)),
            _ => None,
        }) else {
            return out;
        };
        let after = &rest[at + len..];
        let Some(end) = after.find(close) else { return out };
        out.push(&after[..end]);
        rest = &after[end + close.len_utf8()..];
    }
}

/// The record numbers a text names: `r` and five digits, with or without an
/// export's prefix (`intent:r00191`).
fn rule_keys(win: &str) -> Vec<String> {
    let b = win.as_bytes();
    let mut out = Vec::new();
    for i in 0..b.len() {
        let bounded = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'-');
        if b[i] == b'r'
            && bounded
            && i + 6 <= b.len()
            && b[i + 1..i + 6].iter().all(u8::is_ascii_digit)
            && !(i + 6 < b.len() && b[i + 6].is_ascii_alphanumeric())
        {
            out.push(win[i..i + 6].to_string());
        }
    }
    out
}

/// Whitespace collapsed, ASCII lowercased.
pub fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w.to_ascii_lowercase());
    }
    out
}

fn text_of(h: &Head) -> &str {
    if h.body.is_empty() {
        &h.title
    } else {
        &h.body
    }
}

/// Reads one record against the others.
///
/// A candidate with several words takes the first that names a target.
///
/// # Arguments
///
/// * `h` - the record.
/// * `all` - every rule's key and normalized text.
pub fn read_one(h: &Head, all: &[(String, String)]) -> Option<Candidate> {
    let text = text_of(h);
    let hits = word_hits(text);
    let first = hits.first()?;
    let word_of = |r: &(usize, usize)| text[r.0..r.1].to_ascii_lowercase();
    for r in &hits {
        let (a, b) = (floor(text, r.0.saturating_sub(WINDOW)), ceil(text, (r.1 + WINDOW).min(text.len())));
        let win = &text[a..b];
        // The same number in two exports names neither.
        for n in rule_keys(win) {
            let mut held = all.iter().filter(|(k, _)| k != &h.key && k.rsplit(':').next() == Some(n.as_str()));
            if let (Some((k, _)), None) = (held.next(), held.next()) {
                return Some(Candidate { key: h.key.clone(), word: word_of(r), target: Target::Key(k.clone()) });
            }
        }
        for q in quotes(win) {
            let q = norm(q);
            if q.len() < MIN_QUOTE {
                continue;
            }
            let mut holders = all.iter().filter(|(k, t)| k != &h.key && t.contains(&q));
            if let (Some((k, _)), None) = (holders.next(), holders.next()) {
                return Some(Candidate { key: h.key.clone(), word: word_of(r), target: Target::Quote(k.clone()) });
            }
        }
    }
    Some(Candidate { key: h.key.clone(), word: word_of(first), target: Target::Ask })
}

/// Reads every active rule and memory. Read-only.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn scan(store: &MemoryStore) -> Result<Vec<Candidate>> {
    let mut heads = store.heads(Some("rule"), None)?;
    heads.extend(store.heads(Some("memory"), None)?);
    heads.retain(|h| h.status == "active");
    let rules: Vec<(String, String)> = heads.iter().filter(|h| h.kind == "rule").map(|h| (h.key.clone(), norm(text_of(h)))).collect();
    let mut out = Vec::new();
    for h in &heads {
        // A memory is one file rewritten in place: what it reverses is its
        // own older text or an unnamed record, so it is always asked.
        if h.kind == "memory" {
            let text = format!("{} {}", h.title, h.body);
            if let Some(r) = word_hits(&text).first() {
                out.push(Candidate { key: h.key.clone(), word: text[r.0..r.1].to_ascii_lowercase(), target: Target::Ask });
            }
        } else if let Some(c) = read_one(h, &rules) {
            out.push(c);
        }
    }
    Ok(out)
}

/// What the owner is asked, in two parts: the rules first, then the memories.
/// Each part ends with the items added by hand.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sitting {
    /// Part 1: rules with no resolved target.
    pub rules: Vec<Candidate>,
    /// Part 2: memories, always asked.
    pub memory: Vec<Candidate>,
}

/// Builds the sitting: every candidate with no resolved target and no answer
/// yet, plus `added`, records the word list does not reach. An added record
/// has an empty [`Candidate::word`]. One already asked or already answered
/// adds nothing. Read-only.
///
/// # Errors
///
/// [`Error::NotFound`] for an unknown added key, [`Error::InvalidArgument`]
/// for one that is not an active rule or memory; [`Error::Db`].
pub fn sitting(store: &MemoryStore, added: &[String]) -> Result<Sitting> {
    let checked = store.keys_with_ref(CHECKED)?;
    let mut s = Sitting::default();
    for c in scan(store)? {
        if c.target != Target::Ask || checked.contains(&c.key) {
            continue;
        }
        if store.head(&c.key)?.is_some_and(|h| h.kind == "rule") {
            s.rules.push(c);
        } else {
            s.memory.push(c);
        }
    }
    for key in added {
        let h = store.head(key)?.ok_or_else(|| Error::NotFound(format!("record {key:?}")))?;
        if h.status != "active" || !matches!(h.kind.as_str(), "rule" | "memory") {
            return Err(Error::InvalidArgument(format!(
                "{key} is a {} {} record; a sitting asks about active rules and memories",
                h.status, h.kind
            )));
        }
        let part = if h.kind == "rule" { &mut s.rules } else { &mut s.memory };
        if !checked.contains(key) && !part.iter().any(|c| &c.key == key) {
            part.push(Candidate { key: key.clone(), word: String::new(), target: Target::Ask });
        }
    }
    Ok(s)
}

/// The owner's answer to one question. Both lists empty: not a reversal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Answer {
    /// Records it replaces whole: they become superseded.
    pub whole: Vec<String>,
    /// Records it changes in part: linked, both stay active.
    pub partial: Vec<String>,
}

/// Writes one answer in one transaction and marks the question answered.
///
/// # Errors
///
/// As [`super::Writer::mark_superseded`] and [`super::Writer::link_amends`];
/// nothing is written on an error.
pub fn answer(store: &mut MemoryStore, key: &str, a: &Answer, reason: &str) -> Result<()> {
    let name = match (a.whole.is_empty(), a.partial.is_empty()) {
        (true, true) => "not a reversal",
        (false, true) => "whole",
        (true, false) => "partial",
        (false, false) => "whole and partial",
    };
    let mut w = store.writer()?;
    for old in &a.whole {
        w.mark_superseded(old, key, reason)?;
    }
    for old in &a.partial {
        w.link_amends(old, key)?;
    }
    w.add_refs(key, &[(CHECKED.to_string(), name.to_string())])?;
    w.commit()
}

/// Writes a link for every pair the text names (a key, or a quote one other
/// rule holds). Status stays active. Returns how many were new.
///
/// # Errors
///
/// [`Error::Db`].
pub fn link_found(store: &mut MemoryStore) -> Result<usize> {
    let mut todo = Vec::new();
    for c in scan(store)? {
        if let Target::Key(t) | Target::Quote(t) = c.target {
            if !store.linked_to(AMENDS, &t)?.contains(&c.key) {
                todo.push((t, c.key));
            }
        }
    }
    let mut w = store.writer()?;
    for (old, new) in &todo {
        w.link_amends(old, new)?;
    }
    w.commit()?;
    Ok(todo.len())
}

/// The usage of `atlas rules`.
pub const USAGE: &str = "usage: atlas rules scan
       atlas rules counts [--add KEY ...]
       atlas rules sitting 1|2 [--add KEY ...]
       atlas rules link
       atlas rules answer KEY [--whole OLD ...] [--partial OLD ...] [--reason TEXT]
       atlas rules unmark KEY [--reason TEXT]

A sitting has two parts: 1 rules, 2 memories. --add asks about a record the word
list does not reach. An answer with no --whole or --partial means not a reversal.

Experimental, off by default. Marks written here take effect with memory.rules unset.";

/// What the writing commands print: they work with the flag unset.
const EXPERIMENTAL: &str = "atlas rules is experimental, off by default; marks take effect with memory.rules unset (see the wiki, Memory and rule files)";

/// `atlas rules`: see [`USAGE`].
///
/// # Errors
///
/// A message for bad arguments or a failed store call.
pub fn cli(args: Vec<String>, store: &mut MemoryStore) -> std::result::Result<ExitCode, String> {
    let e = |e: Error| e.to_string();
    let mut it = args.into_iter();
    let word = it.next().unwrap_or_default();
    let (mut flags, mut words) = (Vec::new(), Vec::new());
    while let Some(a) = it.next() {
        match a.strip_prefix("--") {
            Some(n @ ("add" | "whole" | "partial" | "reason")) => {
                flags.push((n.to_string(), it.next().ok_or_else(|| format!("--{n} needs a value\n{USAGE}"))?))
            }
            Some(n) => return Err(format!("unknown flag --{n}\n{USAGE}")),
            None => words.push(a),
        }
    }
    let all = |k: &str| -> Vec<String> { flags.iter().filter(|(n, _)| n == k).map(|(_, v)| v.clone()).collect() };
    let reason = flags.iter().rev().find(|(n, _)| n == "reason").map_or_else(|| format!("atlas rules {word}"), |(_, v)| v.clone());
    match (word.as_str(), words.first().map(String::as_str)) {
        ("scan", None) => {
            for c in scan(store).map_err(e)? {
                let t = match &c.target {
                    Target::Key(k) => format!("key {k}"),
                    Target::Quote(k) => format!("quote {k}"),
                    Target::Ask => "ask".to_string(),
                };
                println!("{}\t{}\t{t}", c.key, c.word);
            }
        }
        ("counts", None) => {
            let s = sitting(store, &all("add")).map_err(e)?;
            println!("part 1, rules: {}\npart 2, memories: {}", s.rules.len(), s.memory.len());
        }
        ("sitting", Some(n @ ("1" | "2"))) => {
            let s = sitting(store, &all("add")).map_err(e)?;
            for (i, c) in if n == "1" { &s.rules } else { &s.memory }.iter().enumerate() {
                let Some(h) = store.head(&c.key).map_err(e)? else { continue };
                let tag = if c.word.is_empty() { "added by hand" } else { c.word.as_str() };
                print!("{}. {}", i + 1, pointer(&h, &[tag.to_string()]));
            }
        }
        ("link", None) => {
            println!("{} links written", link_found(store).map_err(e)?);
            println!("{EXPERIMENTAL}");
        }
        ("answer", Some(k)) => {
            answer(store, k, &Answer { whole: all("whole"), partial: all("partial") }, &reason).map_err(e)?;
            println!("{k}: answered");
            println!("{EXPERIMENTAL}");
        }
        ("unmark", Some(k)) => {
            let mut w = store.writer().map_err(e)?;
            w.unmark_superseded(k, &reason).map_err(e)?;
            w.commit().map_err(e)?;
            println!("{k}: active again");
            println!("{EXPERIMENTAL}");
        }
        _ => return Err(USAGE.to_string()),
    }
    Ok(ExitCode::SUCCESS)
}

/// How many later records one hit may bring along.
const MAX_AMENDERS: usize = 3;
/// The longest chain of successors followed.
const MAX_HOPS: usize = 16;

/// The record that holds now for a superseded one: its successor, followed
/// until an active record. None for a chain that ends elsewhere or loops.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn live_of(store: &MemoryStore, key: &str) -> Result<Option<Head>> {
    let mut at = key.to_string();
    for _ in 0..MAX_HOPS {
        let Some(h) = store.head(&at)? else { return Ok(None) };
        if h.status == "active" {
            return Ok(Some(h));
        }
        match (h.status.as_str(), h.superseded_by) {
            ("superseded", Some(next)) => at = next,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// What one search hit shows, in order, the live ruling first:
///
/// * a superseded record: its live successor, tagged `replaces KEY` (the old
///   text stays one `recall {key}` away);
/// * a record others amend: those later records, then the record, all kept;
/// * anything else: itself.
///
/// A superseded record with no live successor shows nothing.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn live_view(store: &MemoryStore, hit: Head) -> Result<Vec<(Head, Vec<String>)>> {
    if hit.status != "active" {
        return Ok(match live_of(store, &hit.key)? {
            Some(live) => vec![(live, vec![format!("replaces {}", hit.key)])],
            None => Vec::new(),
        });
    }
    let amenders = store.linked_to(AMENDS, &hit.key)?;
    let mut out = Vec::with_capacity(amenders.len().min(MAX_AMENDERS) + 1);
    let mut own = Vec::new();
    for k in amenders.iter().rev().take(MAX_AMENDERS) {
        own.push(format!("amended by {k}"));
        if let Some(h) = store.head(k)?.filter(|h| h.status == "active") {
            out.push((h, vec![format!("amends {}", hit.key)]));
        }
    }
    for k in store.linked_to(AMENDED_BY, &hit.key)? {
        own.push(format!("amends {k}"));
    }
    out.push((hit, own));
    Ok(out)
}

/// One record of a query answer with `memory.rules` on.
#[derive(Debug)]
pub struct LiveHit {
    /// The record.
    pub head: Head,
    /// The score of the hit that brought it.
    pub score: f64,
    /// What it replaces or amends, or what amends it.
    pub tags: Vec<String>,
    /// A ruling the answer may not lose: no score floor or byte budget takes it.
    pub sure: bool,
    /// It comes ahead of everything: a hit about a replaced or linked ruling.
    pub first: bool,
}

/// What a query answers with, from its search rows, the live ruling first. The
/// promise is that a query about a ruling returns the ruling that holds, so the
/// rows are not enough: a record that matches better takes their limit, and a
/// bm25 score near zero (a word in over half of a small store) falls under the
/// pointer floor. [`MemoryStore::search_sure`] finds what may not go, and a hit
/// about a replaced or amended ruling leads, because that is the question the
/// answers were given for. Rows are never dropped for this.
///
/// # Arguments
///
/// * `rows` - the search's rows, best first.
/// * `terms`, `kind`, `scope`, `need` - as in [`MemoryStore::search_sure`].
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn live_hits(store: &MemoryStore, mut rows: Vec<(Head, f64, usize)>, terms: &[String], kind: Option<&str>, scope: Option<&str>, need: usize) -> Result<Vec<LiveHit>> {
    let sure = store.search_sure(terms, kind, scope, need)?;
    let ids: Vec<i64> = sure.iter().map(|(h, _, _)| h.rid).collect();
    for row in sure {
        if !rows.iter().any(|(h, _, _)| h.rid == row.0.rid) {
            rows.push(row);
        }
    }
    let mut out: Vec<LiveHit> = Vec::with_capacity(rows.len());
    for (hit, score, _) in rows {
        let is_sure = ids.contains(&hit.rid);
        for (head, tags) in live_view(store, hit)? {
            let first = is_sure && !tags.is_empty();
            match out.iter_mut().find(|x| x.head.rid == head.rid) {
                Some(x) => {
                    x.sure |= is_sure;
                    x.first |= first;
                    for t in tags {
                        if !x.tags.contains(&t) {
                            x.tags.push(t);
                        }
                    }
                }
                None => out.push(LiveHit { head, score, tags, sure: is_sure, first }),
            }
        }
    }
    out.sort_by_key(|x| !x.first);
    Ok(out)
}

/// The date a ruling states, the first `YYYY-MM-DD` in its text. Store time is
/// not it: a bulk import stamps every record with the import's date.
pub fn stated_date(h: &Head) -> Option<String> {
    let t = text_of(h);
    let b = t.as_bytes();
    (0..b.len().saturating_sub(9)).find_map(|i| {
        let w = &b[i..i + 10];
        let shape = w.iter().enumerate().all(|(j, c)| if j == 4 || j == 7 { *c == b'-' } else { c.is_ascii_digit() });
        let edges = (i == 0 || !b[i - 1].is_ascii_digit()) && (i + 10 == b.len() || !b[i + 10].is_ascii_digit());
        (shape && edges && w[0] == b'2').then(|| t[i..i + 10].to_string())
    })
}

/// A dated item's text after its lead (`2026-10-21 (Brandon, X):`), when the
/// colon is near the start.
fn strip_lead(t: &str) -> &str {
    if t.starts_with(|c: char| c.is_ascii_digit()) {
        let near = floor(t, t.len().min(72));
        if let Some(i) = t[..near].find(": ") {
            return &t[i + 2..];
        }
    }
    t
}

/// A ruling's headline: the bold lead if it has one, else its first clause,
/// on a word boundary, at most [`HEADLINE_CHARS`] characters.
pub fn headline(h: &Head) -> String {
    let raw = if h.title.trim().is_empty() { h.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("") } else { h.title.as_str() };
    let t = raw.trim().trim_start_matches("- ").trim();
    let t = match t.strip_prefix("**").and_then(|r| r.find("**").map(|e| &r[..e])) {
        Some(lead) if lead.len() >= 8 => lead,
        _ => strip_lead(t),
    };
    let t = t.replace("**", "");
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= HEADLINE_CHARS {
        if let Some(i) = t.find(". ").filter(|i| *i >= 24) {
            return t[..i].to_string();
        }
        return t.trim_end_matches(['.', ':', ';', ',']).to_string();
    }
    // The first clause if it fits, else the last whole word that does.
    let cut: String = t.chars().take(HEADLINE_CHARS).collect();
    let clause = cut.find(". ").filter(|i| *i >= 24).or_else(|| cut.rfind(['.', ';', ':']).filter(|i| *i >= 24));
    let end = clause.or_else(|| cut.rfind(' ')).unwrap_or(cut.len());
    cut[..end].trim_end_matches(['.', ':', ';', ',', ' ']).to_string()
}

/// One pointer line: key, `MM-DD` of the stated date when there is one, and
/// the headline. `tags` are appended in brackets (`replaces KEY`).
pub fn pointer(h: &Head, tags: &[String]) -> String {
    let mut s = format!("- {}: ", h.key);
    if let Some(d) = stated_date(h) {
        s.push_str(&d[5..]);
        s.push(' ');
    }
    s.push_str(&headline(h));
    for t in tags {
        s.push_str(" [");
        s.push_str(t);
        s.push(']');
    }
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(key: &str, text: &str) -> Head {
        Head {
            rid: 0,
            key: key.into(),
            kind: "rule".into(),
            sub: String::new(),
            scope: String::new(),
            status: "active".into(),
            seq: 1,
            time: String::new(),
            title: text.chars().take(160).collect(),
            meta: String::new(),
            body: text.into(),
            sha: String::new(),
            chain: String::new(),
            superseded_by: None,
            superseded_time: None,
            anchor: None,
            anchor_src: None,
        }
    }

    fn all(hs: &[&Head]) -> Vec<(String, String)> {
        hs.iter().map(|h| (h.key.clone(), norm(&h.body))).collect()
    }

    #[test]
    fn words_are_whole_and_any_case() {
        assert_eq!(word_hits("This REVERSES it").len(), 1);
        assert!(word_hits("reversible self-corrected replaced").is_empty());
    }

    #[test]
    fn a_key_beside_the_word_resolves() {
        let old = head("intent:r00007", "2026-01-01 the old split");
        let new = head("intent:r00009", "2026-01-02 This reverses the split of r00007.");
        assert_eq!(read_one(&new, &all(&[&old, &new])).unwrap().target, Target::Key("intent:r00007".into()));
    }

    #[test]
    fn a_key_far_from_the_word_is_asked() {
        let old = head("intent:r00007", "old");
        let new = head("intent:r00009", &format!("reverses it. {} see r00007", "x".repeat(100)));
        assert_eq!(read_one(&new, &all(&[&old, &new])).unwrap().target, Target::Ask);
    }

    #[test]
    fn a_quote_held_by_one_record_resolves_and_by_two_is_asked() {
        let a = head("intent:r00001", "all misses are fused protrusions");
        let b = head("intent:r00002", "unrelated");
        let new = head("intent:r00003", "the line \"all misses are fused protrusions\" is reversed");
        assert_eq!(read_one(&new, &all(&[&a, &b, &new])).unwrap().target, Target::Quote("intent:r00001".into()));
        let c = head("intent:r00004", "again all misses are fused protrusions");
        assert_eq!(read_one(&new, &all(&[&a, &c, &new])).unwrap().target, Target::Ask);
    }

    #[test]
    fn a_later_item_on_a_topic_with_no_key_is_never_joined() {
        let old = head("intent:r00001", "2026-09-25 the machine lock is held only while running");
        let new = head("intent:r00002", "2026-09-25 the lock is scrapped. Reverses the lock and queue lines above");
        assert_eq!(read_one(&new, &all(&[&old, &new])).unwrap().target, Target::Ask);
    }

    #[test]
    fn no_word_is_no_candidate() {
        let h = head("intent:r00001", "the sky is lit");
        assert!(read_one(&h, &all(&[&h])).is_none());
    }

    #[test]
    fn the_stated_date_is_read_from_the_text_not_the_store() {
        let mut h = head("intent:r00001", "Ruled 2026-09-25: yes");
        h.time = "2026-10-31T00:00:00Z".into();
        assert_eq!(stated_date(&h).as_deref(), Some("2026-09-25"));
        assert_eq!(stated_date(&head("k", "no date, 12026-01-011 here")), None);
    }

    #[test]
    fn headlines_prefer_the_bold_lead_then_a_clause_and_fit() {
        let h = head("k", "- **The lock is scrapped; concurrency is capped instead.** Brandon said a lot more here");
        assert_eq!(headline(&h), "The lock is scrapped; concurrency is capped instead");
        let h = head("k", "- 2026-10-21 Brandon (AA): the mark moves to the trace pool. Then a long tail of detail that goes on and on");
        assert_eq!(headline(&h), "the mark moves to the trace pool");
        let long = head("k", &"word ".repeat(60));
        assert!(headline(&long).chars().count() <= HEADLINE_CHARS);
    }

    #[test]
    fn a_pointer_carries_key_date_headline_and_tags() {
        let h = head("intent:r00191", "- **The lock is scrapped.** Brandon, 2026-09-25");
        assert_eq!(
            pointer(&h, &["replaces intent:r00007".into()]),
            "- intent:r00191: 09-25 The lock is scrapped [replaces intent:r00007]\n"
        );
    }
}
