//! What came after a lane's newest handoff, for the resume card
//! ([`crate::resume`]) and lane recall ([`super::recall::lane_with`]): the
//! slips of 2026-11-24 (journal:ATLAS-RESUME:0001 to 0004) were all a
//! handoff trusted over the entries written after it.
//!
//! - Entries after the handoff: every active journal entry of the lane newer
//!   than its newest handoff (to the second, then by row), the hook's `cap`
//!   notes aside.
//! - A superseded next step: an entry after the handoff that names the step
//!   done (a review that does not ask for changes, a commit, an entry with
//!   `pr:` meta or "merged" in its title); when the step names PRs (`#501`,
//!   also as `atlas-dev#501`) or keys, the entry must name one of them too.
//! - Orphaned launches (the card's fallback when the previous session's
//!   subagent transcripts are not there): `launch` entries with nothing
//!   after them in the lane but other launches. After a restart their
//!   agents are gone.
//! - Holds: a journal entry whose meta has `held: A, B, C` (what waits) and
//!   `until: TEXT` (the release). It is owed once a later journal entry or
//!   handoff has the key `TEXT`, or names `TEXT` whole, as words, in its
//!   title (any case; a one-word `TEXT` only in the hold's lane), until a
//!   later entry closes it (`closes:` ref, see [`MemoryStore::queue_closed`])
//!   or it is retired.
//! - Queue close: a `note` or `commit` whose title (its first line) is
//!   `queue: <key> done ...`, or any active record with a `closes:<key>`
//!   ref, closes that key: the work queue no longer offers it.
//! - Cross-lane rulings: `decision` and `answered` entries of other lanes
//!   that name the lane in a `lanes:` meta, or as a whole word not followed
//!   by `:digits` in their title or text (a key cites a record, it is no
//!   ruling about the lane), or that supersede, amend, reopen or close one
//!   of its records by ref.
//! - The active goal record: a memory record keyed `active-goal...`, with its
//!   newest `AMENDED` paragraph.

use std::collections::HashSet;

use super::{head_row, Head, MemoryStore, HEAD_SQL, HEAD_SQL_NO_BODY};
use crate::db::params;
use crate::Result;

/// The ref kinds by which a ruling of another lane names a lane's record,
/// as an SQL list.
const RULING_REFS: &str = "('supersedes', 'amends', 'reopens', 'closes')";

/// A record's place in time: its time to the second, then its row (a
/// `...00Z` and a `...00.393Z` of one second compare by row).
pub fn at(h: &Head) -> (&str, i64) {
    (h.time.get(..19).unwrap_or(&h.time), h.rid)
}

/// The lane's newest handoff and the entries after it, oldest first, from
/// the lane's records in row order (as [`MemoryStore::lane_family`] gives
/// them). `None` when the lane has no handoff.
pub fn after_handoff(all: &[Head]) -> Option<(&Head, Vec<&Head>)> {
    let h = super::recall::newest_handoff(all)?;
    let mut after: Vec<&Head> = all.iter().filter(|e| e.kind == "journal" && e.sub != "cap" && e.status == "active" && at(e) > at(h)).collect();
    after.sort_by(|a, b| at(a).cmp(&at(b)));
    Some((h, after))
}

/// The order the card names entries in: decisions, answers, reviews and
/// launches first.
pub fn rank(sub: &str) -> u8 {
    match sub {
        "decision" => 0,
        "answered" => 1,
        "review" => 2,
        "launch" => 3,
        _ => 4,
    }
}

/// A character inside a word: a letter, digit, `-` or `_`.
fn inner(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

/// `word` in `text` with no letter, digit, `-` or `_` on either side; a
/// side where `word` itself starts or ends with a mark (the `#` of `#501`)
/// needs no gap, so `atlas-dev#501` names `#501`.
pub fn names_word(text: &str, word: &str) -> bool {
    word_at(text, word, |_| true)
}

/// `lane` named as a whole word in `text`, not as part of a key: an
/// occurrence followed by `:` and a digit (`GOAL:0034`,
/// `journal:GOAL:0034`) cites a record, not the lane (Brandon 2026-11-24,
/// journal:ATLAS-RESUME:0010).
pub fn names_lane(text: &str, lane: &str) -> bool {
    word_at(text, lane, |rest| !rest.strip_prefix(':').is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit())))
}

/// [`names_word`], and `ok(rest)` for the text after the occurrence.
fn word_at(text: &str, word: &str, ok: impl Fn(&str) -> bool) -> bool {
    if word.is_empty() {
        return false;
    }
    let open = word.chars().next().is_some_and(inner);
    let close = word.chars().next_back().is_some_and(inner);
    let mut from = 0;
    while let Some(i) = text[from..].find(word) {
        let s = from + i;
        let e = s + word.len();
        let before = !open || text[..s].chars().next_back().is_none_or(|c| !inner(c));
        let after = !close || text[e..].chars().next().is_none_or(|c| !inner(c));
        if before && after && ok(&text[e..]) {
            return true;
        }
        from = s + word.len().max(1);
        while !text.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

/// The PRs (`#501`) and keys (`journal:X:0001`, `handoff:X:001`) a step
/// names.
fn named(step: &str) -> Vec<String> {
    step.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '"' | '`'))
        .map(|w| w.trim_end_matches(['.', ':']))
        .filter(|w| {
            w.strip_prefix('#').is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                || ((w.starts_with("journal:") || w.starts_with("handoff:")) && w.matches(':').count() == 2)
        })
        .map(str::to_string)
        .collect()
}

/// Whether `e` names `n` (a `#501` also by its `pr: 501` meta).
fn mentions(e: &Head, n: &str) -> bool {
    if names_word(&e.title, n) || names_word(&e.body, n) {
        return true;
    }
    n.strip_prefix('#').is_some_and(|num| crate::queue::meta_value(&e.meta, "pr").is_some_and(|p| p.rsplit(['#', '/']).next() == Some(num)))
}

/// The newest entry of `after` that names the handoff's next step `next`
/// done (see the module doc), if any.
pub fn superseding<'a>(next: &str, after: &[&'a Head]) -> Option<&'a Head> {
    let names = named(next);
    after
        .iter()
        .rev()
        .find(|e| {
            let done = (e.sub == "review" && !asks_changes(e)) || e.sub == "commit" || crate::queue::meta_value(&e.meta, "pr").is_some() || e.title.to_lowercase().contains("merged");
            done && (names.is_empty() || names.iter().any(|n| mentions(e, n)))
        })
        .copied()
}

/// Whether review `e` asks for changes: a `verdict:` meta, else its title's
/// text after the last `:`, that starts `FIX` or `REQUEST CHANGES` (any
/// case). Such a review leaves the next step owed.
fn asks_changes(e: &Head) -> bool {
    let v = crate::queue::meta_value(&e.meta, "verdict").unwrap_or_else(|| e.title.rsplit(':').next().unwrap_or(""));
    let v = v.trim().to_ascii_uppercase();
    v.starts_with("FIX") || v.starts_with("REQUEST CHANGES") || v.starts_with("CHANGES REQUESTED")
}

/// Words after `done` that make a queue-close title a condition
/// (`queue: K done when X`), which closes nothing.
const CONDITIONS: [&str; 6] = ["when", "once", "after", "if", "until", "unless"];

/// The key a queue-close title names: `queue: <key> done ...` (any case),
/// not followed by a condition ([`CONDITIONS`]).
pub fn done_key(title: &str) -> Option<&str> {
    let rest = title.get(..6).filter(|p| p.eq_ignore_ascii_case("queue:")).and_then(|_| title.get(6..))?;
    let mut w = rest.split_whitespace();
    let k = w.next()?;
    let done = w.next().map(|d| d.trim_end_matches(['.', ',', ';', ':'])).is_some_and(|d| d.eq_ignore_ascii_case("done"));
    let when = w.next().is_some_and(|c| CONDITIONS.iter().any(|x| c.eq_ignore_ascii_case(x)));
    (done && !when).then_some(k)
}

/// A hold whose release came: the hold's key, the release's, and what it
/// held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owed {
    /// The hold entry.
    pub hold: String,
    /// The later entry that met its `until:`.
    pub release: String,
    /// Its `held:` items.
    pub held: Vec<String>,
    /// When the hold was written.
    pub time: String,
}

/// Whether entry `key` (`title`, in `scope`) releases a hold of `lane`
/// whose condition is `until`: it is keyed `until`, or its title names the
/// whole condition as words (any case). A one-word condition ("review")
/// releases only from the hold's own lane: in any lane it fires on anything.
fn releases(until: &str, lane: &str, key: &str, title: &str, scope: &str) -> bool {
    if key == until {
        return true;
    }
    let words = until.split_whitespace().count();
    names_word(&title.to_lowercase(), &until.to_lowercase()) && (words >= 2 || crate::lanes::in_lane(scope, lane))
}

/// The newest paragraph of `body` that starts `AMENDED` (any case, past
/// list and emphasis markers), on one line.
pub fn amendment(body: &str) -> Option<String> {
    body.split("\n\n")
        .map(|p| p.trim().trim_start_matches(['-', '*', '_', '>', ' ']))
        .filter(|p| p.get(..7).is_some_and(|w| w.eq_ignore_ascii_case("amended")))
        .last()
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
}

impl MemoryStore {
    /// The keys closed by a queue-close note or commit, or by a `closes:`
    /// ref (see the module doc).
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn queue_closed(&self) -> Result<HashSet<String>> {
        let sql = "SELECT v.title FROM records r JOIN versions v ON v.vid = r.head
            WHERE r.kind = 'journal' AND r.sub IN ('note', 'commit') AND r.status = 'active' AND v.title LIKE 'queue:%'";
        let titles: Vec<String> = self.conn.prepare_cached(sql)?.query_map([], |r| r.get(0))?.collect::<crate::db::Result<_>>()?;
        let mut out: HashSet<String> = titles.iter().filter_map(|t| done_key(t)).map(str::to_string).collect();
        let sql = "SELECT f.target FROM refs f JOIN records r ON r.rid = f.rid WHERE f.kind = 'closes' AND r.status = 'active'";
        let refs: Vec<String> = self.conn.prepare_cached(sql)?.query_map([], |r| r.get(0))?.collect::<crate::db::Result<_>>()?;
        out.extend(refs);
        Ok(out)
    }

    /// The holds written at or after `since` whose release came and that
    /// nothing closed, oldest first (see the module doc).
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn owed_holds(&self, since: &str) -> Result<Vec<Owed>> {
        let sql = format!(
            "{HEAD_SQL_NO_BODY} WHERE r.kind = 'journal' AND r.status = 'active' AND v.time >= ?1
                AND (substr(v.meta, 1, 5) = 'held:' OR instr(v.meta, char(10) || 'held:') > 0) ORDER BY r.rid"
        );
        let holds: Vec<Head> = self.conn.prepare_cached(&sql)?.query_map([since], head_row)?.collect::<crate::db::Result<_>>()?;
        if holds.is_empty() {
            return Ok(Vec::new());
        }
        let closed = self.queue_closed()?;
        // Candidates by substring; `releases` then asks for the whole
        // condition as words.
        let find = "SELECT r.key, v.title, r.scope FROM records r JOIN versions v ON v.vid = r.head
            WHERE r.kind IN ('journal', 'handoff') AND r.status = 'active' AND r.rid != ?1 AND v.time > ?2
                AND (r.key = ?3 OR instr(lower(v.title), lower(?3)) > 0) ORDER BY v.time, r.rid";
        let mut out = Vec::new();
        for h in holds {
            let until = crate::queue::meta_value(&h.meta, "until").unwrap_or("").trim();
            if until.is_empty() || closed.contains(&h.key) {
                continue;
            }
            let found: Vec<(String, String, String)> =
                self.conn.prepare_cached(find)?.query_map(params![h.rid, h.time, until], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<crate::db::Result<_>>()?;
            let Some((release, ..)) = found.into_iter().find(|(k, t, scope)| releases(until, &h.scope, k, t, scope)) else { continue };
            let held = crate::queue::meta_value(&h.meta, "held").unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
            out.push(Owed { hold: h.key, release, held, time: h.time });
        }
        Ok(out)
    }

    /// Rulings (`decision`, `answered`) of other lanes that name `lane` in a
    /// `lanes:` meta, or as a whole word in their title or text that is not
    /// a key ([`names_lane`]: "GOAL:0034" cites a record, it is no ruling
    /// about GOAL), or that supersede or amend one of its records by ref
    /// ([`RULING_REFS`]), written at or after `since` (the
    /// store's time form; "" for all), oldest first. An `answers` ref is
    /// not one: the open-question count already follows it.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn rulings_naming(&self, lane: &str, since: &str) -> Result<Vec<Head>> {
        let sql = format!(
            "{HEAD_SQL} WHERE r.kind = 'journal' AND r.sub IN ('decision', 'answered') AND r.status = 'active' AND v.time >= ?4
                AND (instr(v.title, ?1) > 0 OR instr(v.body, ?1) > 0 OR instr(v.meta, ?1) > 0
                    OR EXISTS (SELECT 1 FROM refs f WHERE f.rid = r.rid AND f.kind IN {RULING_REFS} AND (substr(f.target, 1, length(?2)) = ?2 OR substr(f.target, 1, length(?3)) = ?3)))
                ORDER BY r.rid"
        );
        let (j, h) = (format!("journal:{lane}:"), format!("handoff:{lane}:"));
        let rows: Vec<Head> = self.conn.prepare_cached(&sql)?.query_map(params![lane, j, h, since], head_row)?.collect::<crate::db::Result<_>>()?;
        let reffed: HashSet<String> = {
            let sql = format!(
                "SELECT r.key FROM refs f JOIN records r ON r.rid = f.rid WHERE f.kind IN {RULING_REFS} AND (substr(f.target, 1, length(?1)) = ?1 OR substr(f.target, 1, length(?2)) = ?2)"
            );
            self.conn.prepare_cached(&sql)?.query_map(params![j, h], |r| r.get(0))?.collect::<crate::db::Result<_>>()?
        };
        Ok(rows
            .into_iter()
            .filter(|r| {
                let meta = crate::queue::meta_value(&r.meta, "lanes").is_some_and(|l| l.split(',').any(|x| x.trim().eq_ignore_ascii_case(lane)));
                !crate::lanes::in_lane(&r.scope, lane) && (reffed.contains(&r.key) || meta || names_lane(&r.title, lane) || names_lane(&r.body, lane))
            })
            .collect())
    }

    /// The `launch` entries written at or after `since` with nothing after
    /// them in their lane but other launches (and the hook's `cap` notes):
    /// a handoff, review, commit, note, answer, brief or decision is an
    /// answer (research and brief-writer agents hand back with a note or an
    /// answer). Oldest first. The card's fallback when the previous
    /// session's transcripts are not there, and it asks for the last day's
    /// only: an older launch with nothing after it is more often an agent
    /// that handed back without a journal entry.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn orphaned_launches(&self, since: &str) -> Result<Vec<Head>> {
        let sql = format!(
            "{HEAD_SQL_NO_BODY} WHERE r.kind = 'journal' AND r.sub = 'launch' AND r.status = 'active' AND v.time >= ?1
                AND NOT EXISTS (SELECT 1 FROM records a JOIN versions w ON w.vid = a.head WHERE a.scope = r.scope
                    AND (a.kind = 'handoff' OR (a.kind = 'journal' AND a.sub NOT IN ('launch', 'cap')))
                    AND (substr(w.time, 1, 19) > substr(v.time, 1, 19) OR (substr(w.time, 1, 19) = substr(v.time, 1, 19) AND a.rid > r.rid)))
                ORDER BY r.rid"
        );
        Ok(self.conn.prepare_cached(&sql)?.query_map([since], head_row)?.collect::<crate::db::Result<_>>()?)
    }

    /// The active goal records, newest first: records of kind `goal` whose
    /// state is active; while the store has no `goal` record at all (before
    /// the seed, journal:ATLAS-GOAL:0031 Q-GS4), memory records keyed
    /// `active-goal...`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn goal_records(&self) -> Result<Vec<Head>> {
        if !self.heads(Some(crate::goal::KIND), None)?.is_empty() {
            return crate::goal::active(self);
        }
        let sql = format!("{HEAD_SQL} WHERE r.kind = 'memory' AND r.status = 'active' AND substr(r.key, 1, 11) = 'active-goal' ORDER BY v.time DESC, r.rid DESC");
        Ok(self.conn.prepare_cached(&sql)?.query_map([], head_row)?.collect::<crate::db::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_steps_and_close_titles_parse() {
        assert!(names_word("HUD-TEXT's rulings", "HUD-TEXT"));
        assert!(names_word("see journal:HUD-TEXT:0003", "HUD-TEXT"));
        assert!(!names_word("HUD-TEXTURE stays", "HUD-TEXT"));
        assert!(!names_word("OLD-HUD-TEXT", "HUD-TEXT"));
        assert_eq!(named("review of #501 fix4, then journal:X:0002."), ["#501", "journal:X:0002"]);
        assert_eq!(done_key("queue: journal:A:0001 done (#465)"), Some("journal:A:0001"));
        assert_eq!(done_key("Queue: journal:A:0001 Done."), Some("journal:A:0001"));
        assert_eq!(done_key("queue: journal:A:0001 waits"), None);
        // A condition is not a close (fix round 1, N1).
        assert_eq!(done_key("queue: journal:A:0001 done when #424 merges"), None);
        // A PR named with its repo (fix round 1, S4).
        assert!(names_word("review atlas-dev#501 fix1", "#501"));
        assert!(!names_word("review #5012", "#501"));
        // A lane named as a key is not the lane named (fix round 1, M2).
        assert!(names_lane("HUD-TEXT's rulings", "HUD-TEXT"));
        assert!(names_lane("GOAL, then SKY", "GOAL"));
        assert!(!names_lane("GOAL:0034 baked spline", "GOAL"));
        assert!(!names_lane("see journal:GOAL:0034.", "GOAL"));
        assert!(names_lane("GOAL: the frame target", "GOAL"));
        assert_eq!(amendment("goal\n\nAMENDED 1: a\n\n**amended 2: b\nc**\n\nrest").as_deref(), Some("amended 2: b c**"));
    }
}
