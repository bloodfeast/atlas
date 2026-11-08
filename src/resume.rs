//! The resume card SessionStart injects on a new or cleared session
//! (`memory.resume_card`): where the work stands, as keys, so the first turn
//! starts from the store's state rather than from nothing. The smallest
//! useful layer: each line names keys that `recall` reads whole. An empty
//! store gives no card.
//!
//! The lines, in this order, each left out when empty:
//! - the live lanes (a work record, `LaneStatus::last_work`, within
//!   [`LIVE_DAYS`]; `session:` and `agent:` scopes and the queue lane are not
//!   lanes), each with its open question count and newest handoff;
//! - the live lanes' open questions for the user, as keys;
//! - after a `/clear` only, the agents still running from the previous
//!   session (a new process has none: its agents ended with the old one);
//! - the work queue: how many wait, the batch that can start now and the
//!   next one waiting;
//! - the newest handoffs within [`LIVE_DAYS`] the lanes line does not
//!   already name;
//! - the pending ideas, when `memory.ideas` is on (see [`crate::ideas`]);
//! - the previous session's goal, when it was not met.
//!
//! Then one instruction: ask the user, as multiple choice, whether to resume,
//! change the plan, or do something more pressing first.
//!
//! The whole card fits [`CARD_BYTES`]: a line past the budget is cut at an
//! item boundary, and a line with no room left is dropped. The goal line and
//! the instruction are never cut: their bytes are reserved first, because
//! the card asks for the goal to be restated as written. A goal too long for
//! the card ([`GOAL_CHARS`]) is named by where it is instead of shown cut.
//!
//! The previous session is the newest other transcript in the folder. When
//! two sessions run at once in one project, that can be the other live
//! session, and the card then shows that session's goal: the card says
//! "the previous session's goal", and the user can decline to restate it.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::json::Value;
use crate::memory::MemoryStore;
use crate::orchestrate::OrchSettings;
use crate::Result;

/// A lane is live when its newest work record is at most this many days old.
pub const LIVE_DAYS: i64 = 7;
/// The card's byte budget, Brandon's ruling of 2026-11-06: 1,160 bytes, the lowest
/// budget tried that holds every question of the resume eval. The test's card full
/// of keys reads 612 tokens at 1,119 bytes under it, and that count is the ceiling
/// in `xtask/src/resume_eval.rs`, with no margin for the estimate's error. Only the
/// bytes bind at runtime: a card filling the budget could read a few more tokens.
/// Keys run about 1.83 bytes per token by that estimate, a public vocabulary times
/// a fitted factor and not real tokens, under the 2.42 of mixed session text.
pub const CARD_BYTES: usize = 1_160;
/// Items listed at most per line.
const MAX_LANES: usize = 6;
const MAX_QUESTIONS: usize = 6;
const MAX_AGENTS: usize = 4;
const MAX_START: usize = 5;
const MAX_HANDOFFS: usize = 3;
/// A title is clipped to this many characters.
const TITLE_CHARS: usize = 60;
/// The longest goal the card shows whole; a longer one is named by where it
/// is, never cut.
pub const GOAL_CHARS: usize = 300;
/// How much of the previous transcript's end is searched for its goal, at
/// most.
const GOAL_TAIL_BYTES: u64 = 4 << 20;
/// The goal search reads the transcript backward in blocks of this size.
const GOAL_BLOCK: u64 = 64 << 10;

const HEADER: &str = "atlas resume card (the store at session start; `recall KEY` reads a key whole):\n";

/// The card for a session whose transcript is `transcript` (its folder holds
/// the previous session's), at record time `now`. `cleared` is a `/clear`
/// in a running process, whose previous session's agents may still run.
/// `None` when the store and the previous session have nothing to resume.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn card(store: &MemoryStore, orch: &OrchSettings, transcript: &str, now: &str, cleared: bool) -> Result<Option<String>> {
    card_with(store, orch, transcript, now, cleared, None)
}

/// [`card`] with one more line, the pending ideas'. That line alone makes no
/// card: there is nothing to resume, and the caller shows it bare.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn card_with(store: &MemoryStore, orch: &OrchSettings, transcript: &str, now: &str, cleared: bool, ideas: Option<&str>) -> Result<Option<String>> {
    build(store, orch, transcript, now, cleared, ideas, CARD_BYTES)
}

/// [`card`] in `budget` bytes, for the gate's test in xtask.
///
/// # Errors
///
/// [`crate::Error::Db`].
#[cfg(feature = "dev")]
pub fn card_within(store: &MemoryStore, orch: &OrchSettings, transcript: &str, now: &str, cleared: bool, budget: usize) -> Result<Option<String>> {
    build(store, orch, transcript, now, cleared, None, budget)
}

fn build(store: &MemoryStore, orch: &OrchSettings, transcript: &str, now: &str, cleared: bool, ideas: Option<&str>, budget: usize) -> Result<Option<String>> {
    let prev = previous_transcript(Path::new(transcript));
    let mut lines: Vec<String> = Vec::new();

    let handoffs = store.handoff_keys()?;
    let recent = |time: &str| crate::queue::minutes_between(time, now).is_some_and(|m| m <= LIVE_DAYS * 24 * 60);
    let mut live: Vec<(String, String)> = store
        .lanes()?
        .into_iter()
        .filter(|l| is_lane(&l.lane) && recent(&l.last_work))
        .map(|l| (l.lane, l.last_work))
        .collect();
    live.sort_by(|a, b| b.1.cmp(&a.1));
    let questions: Vec<(String, String, String)> = store.open_questions()?.into_iter().filter(|(l, _, _)| live.iter().any(|(x, _)| x == l)).collect();
    let mut named: Vec<&str> = Vec::new();
    if !live.is_empty() {
        let shown: Vec<String> = live
            .iter()
            .take(MAX_LANES)
            .map(|(lane, _)| {
                let mut parts = Vec::new();
                let open = questions.iter().filter(|(l, _, _)| l == lane).count();
                if open > 0 {
                    parts.push(format!("{open} question{}", if open == 1 { "" } else { "s" }));
                }
                if let Some((_, key, _, _)) = handoffs.iter().find(|(l, ..)| l == lane) {
                    named.push(key);
                    parts.push(key.clone());
                }
                if parts.is_empty() { lane.clone() } else { format!("{lane} ({})", parts.join(", ")) }
            })
            .collect();
        lines.push(format!("- live lanes{}: {}", more(live.len(), MAX_LANES), shown.join("; ")));
    }

    if !questions.is_empty() {
        let shown: Vec<&str> = questions.iter().take(MAX_QUESTIONS).map(|(_, k, _)| k.as_str()).collect();
        lines.push(format!("- open questions for the user{}: {}", more(questions.len(), MAX_QUESTIONS), shown.join(", ")));
    }

    // Only a cleared session's process still runs the previous session's
    // agents; a new process starts with none, and none hold slots.
    let mut running = Vec::new();
    let mut dir = PathBuf::new();
    if let Some(p) = prev.as_ref().filter(|_| cleared) {
        dir = crate::orchestrate::subagents_dir(&p.to_string_lossy());
        let at = crate::queue::system_time(now).unwrap_or_else(SystemTime::now);
        running = crate::orchestrate::Agents::new(None).running(orch, &dir, at).unwrap_or_default();
    }

    if !running.is_empty() {
        let shown: Vec<String> = running
            .iter()
            .take(MAX_AGENTS)
            .map(|r| {
                let id: String = r.id.chars().take(8).collect();
                if r.description.is_empty() { format!("{id} ({})", r.kind.name()) } else { format!("{} ({id}, {})", clip(&r.description), r.kind.name()) }
            })
            .collect();
        lines.push(format!("- running agents (previous session){}: {}", more(running.len(), MAX_AGENTS), shown.join("; ")));
    }

    let (_, items) = crate::queue::items(store, now)?;
    if !items.is_empty() {
        let p = crate::queue::propose(orch, &items, &crate::queue::holders(&running, &dir, &[]));
        let mut q = format!("- work queue: {} waiting", items.len());
        if !p.start.is_empty() {
            let keys: Vec<&str> = p.start.iter().take(MAX_START).map(|&i| items[i].key.as_str()).collect();
            q.push_str(&format!("; can start now{}: {}", more(p.start.len(), MAX_START), keys.join(", ")));
        }
        if let Some((i, why)) = p.wait.first() {
            q.push_str(&format!("; next waiting: {} ({why})", items[*i].key));
        }
        lines.push(q);
    }

    let rest: Vec<String> = handoffs
        .iter()
        .filter(|(_, k, _, time)| !named.contains(&k.as_str()) && recent(time))
        .take(MAX_HANDOFFS)
        .map(|(_, k, t, _)| format!("{k} ({})", clip(t)))
        .collect();
    if !rest.is_empty() {
        lines.push(format!("- other handoffs: {}", rest.join("; ")));
    }

    let goal_line = prev.as_deref().and_then(|p| goal(p).map(|g| goal_line(&g, p))).unwrap_or_default();
    if lines.is_empty() && goal_line.is_empty() {
        return Ok(None);
    }
    lines.extend(ideas.map(str::to_string));
    let first = if items.is_empty() { "resume the work above" } else { "resume the work queue" };
    let ask = format!("{goal_line}Before other work, ask the user one multiple-choice question: (1) {first}, (2) change it first, or (3) do something more pressing.\n");
    Ok(Some(fit(&lines, &ask, budget)))
}

/// The card's goal line, ending in a newline: the goal whole, or, past
/// [`GOAL_CHARS`], where to read it (a cut goal restated with `/goal` would
/// lose its end).
fn goal_line(goal: &str, transcript: &Path) -> String {
    let n = goal.chars().count();
    if n <= GOAL_CHARS {
        return format!("- the previous session's goal, not met (restate it with /goal to keep it): {goal}\n");
    }
    format!(
        "- the previous session's goal, not met, is {n} characters, too long to show here: it is the `condition` of the last `goal_status` record in {}; restate it with /goal to keep it\n",
        transcript.display()
    )
}

/// A scope the card lists as a lane: not a session's or an agent's own, and
/// not the work queue's.
fn is_lane(scope: &str) -> bool {
    !scope.is_empty() && scope != crate::queue::LANE && !scope.starts_with("session:") && !scope.starts_with("agent:")
}

/// The header, `lines` and `tail`, in at most `budget` bytes. `tail` (the
/// goal line and the instruction) is reserved first and never cut. A line
/// past the budget is cut at its last item boundary (`; ` or `, `) with
/// ` ...`, or at a character boundary when it has none; a line with no
/// useful room is dropped, and a later, shorter line may still fit.
fn fit(lines: &[String], tail: &str, budget: usize) -> String {
    const MIN_LINE: usize = 40;
    let mut out = String::from(HEADER);
    let mut left = budget.saturating_sub(HEADER.len() + tail.len());
    for l in lines {
        if l.len() < left {
            out.push_str(l);
            out.push('\n');
            left -= l.len() + 1;
            continue;
        }
        // Room for the cut line, " ..." and its newline.
        let Some(room) = left.checked_sub(5).filter(|r| *r >= MIN_LINE) else { continue };
        let mut end = room;
        while !l.is_char_boundary(end) {
            end -= 1;
        }
        let head = &l[..end];
        let cut = head.rfind("; ").into_iter().chain(head.rfind(", ")).max().filter(|&i| i >= MIN_LINE).unwrap_or(end);
        out.push_str(&l[..cut]);
        out.push_str(" ...\n");
        left -= cut + 5;
    }
    out.push_str(tail);
    out
}

fn more(n: usize, max: usize) -> String {
    if n > max { format!(" ({max} of {n})") } else { String::new() }
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= TITLE_CHARS {
        return s.to_string();
    }
    let mut c: String = s.chars().take(TITLE_CHARS).collect();
    c.push_str("...");
    c
}

/// The previous session's transcript: the most recently written `.jsonl`
/// beside `current`, other than it.
pub fn previous_transcript(current: &Path) -> Option<PathBuf> {
    let dir = current.parent().filter(|d| !d.as_os_str().is_empty())?;
    let name = current.file_name()?;
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name() != name && e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// A transcript's goal, when its newest goal record says it was not met: a
/// `goal_status` attachment (`sentinel: true`) with `met: false` gives its
/// condition; any `met: true` or a later `/goal clear` gives none. The transcript
/// is read backward from its end in [`GOAL_BLOCK`] blocks, up to
/// [`GOAL_TAIL_BYTES`], and the search stops at the newest goal record.
pub fn goal(transcript: &Path) -> Option<String> {
    goal_within(transcript, GOAL_TAIL_BYTES)
}

/// [`goal`] over the whole transcript, for a caller that must not miss one.
pub fn goal_anywhere(transcript: &Path) -> Option<String> {
    goal_within(transcript, u64::MAX)
}

fn goal_within(transcript: &Path, tail: u64) -> Option<String> {
    let mut f = std::fs::File::open(transcript).ok()?;
    let len = f.metadata().ok()?.len();
    let floor = len.saturating_sub(tail);
    let mut pos = len;
    // The bytes after `pos` not yet searched: the start of a line whose
    // beginning is in an earlier block.
    let mut carry: Vec<u8> = Vec::new();
    while pos > floor {
        let start = pos.saturating_sub(GOAL_BLOCK).max(floor);
        let mut buf = vec![0u8; usize::try_from(pos - start).ok()?];
        f.seek(SeekFrom::Start(start)).ok()?;
        f.read_exact(&mut buf).ok()?;
        buf.extend_from_slice(&carry);
        pos = start;
        // Above the read's start, the first line may begin in an earlier
        // block: it waits in `carry`. At the floor, it is dropped unless the
        // floor is the file's start.
        let first = if pos > 0 { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |i| i + 1) } else { 0 };
        for line in buf[first..].split(|&b| b == b'\n').rev() {
            if let Some(decided) = std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)).ok().and_then(goal_record) {
                return decided;
            }
        }
        buf.truncate(first);
        carry = buf;
    }
    None
}

/// What one transcript line says about the goal: `Some(Some(condition))`
/// for an unmet goal, `Some(None)` for a met or cleared one, and `None` when
/// the line is not a goal record.
fn goal_record(line: &str) -> Option<Option<String>> {
    if line.contains("\"type\":\"goal_status\"") {
        let v = crate::json::parse(line).ok()?;
        let a = v.get("attachment").filter(|a| a.get("type").and_then(Value::as_str) == Some("goal_status"))?;
        // The usual completion record has `iterations` and no `sentinel`. A
        // `met: false` with no `sentinel` is one check of a goal still running.
        let met = a.get("met").and_then(Value::as_bool);
        if met == Some(true) {
            return Some(None);
        }
        if a.get("sentinel").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        if met != Some(false) {
            return Some(None);
        }
        return Some(a.get("condition").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty()).map(str::to_string));
    }
    if line.contains("<command-name>/goal</command-name>") {
        let v = crate::json::parse(line).ok()?;
        let typed = v.get("type").and_then(Value::as_str) == Some("user");
        let content = v.get("message").and_then(|m| m.get("content")).and_then(Value::as_str).unwrap_or("");
        if typed && content.contains("<command-name>/goal</command-name>") && content.contains("<command-args>clear</command-args>") {
            return Some(None);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_goal_record_decides() {
        let d = std::env::temp_dir().join(format!("atlas-resume-goal-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("t.jsonl");
        let status = |met: bool| format!(r#"{{"type":"attachment","attachment":{{"type":"goal_status","met":{met},"sentinel":true,"condition":"ship it"}}}}"#);
        let clear = r#"{"type":"user","message":{"content":"<command-name>/goal</command-name>\n<command-args>clear</command-args>"}}"#;
        let quoted = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"<command-name>/goal</command-name> <command-args>clear</command-args>"}]}}"#;
        // As Claude Code writes them: a finished goal, and one failed check.
        let done = r#"{"type":"attachment","attachment":{"type":"goal_status","met":true,"iterations":3,"durationMs":9000,"tokens":1200}}"#;
        let check = r#"{"type":"attachment","attachment":{"type":"goal_status","met":false,"condition":"ship it"}}"#;
        let cases: [(Vec<String>, Option<&str>); 9] = [
            (vec![status(false), done.into()], None),
            (vec![status(false), done.into(), r#"{"type":"user","message":{"content":"hi"}}"#.into()], None),
            (vec![status(false), done.into(), status(false)], Some("ship it")),
            (vec![status(false), check.into()], Some("ship it")),
            (vec![status(false)], Some("ship it")),
            (vec![status(false), status(true)], None),
            (vec![status(false), clear.into()], None),
            (vec![status(false), quoted.into()], Some("ship it")),
            (vec![r#"{"type":"user","message":{"content":"hi"}}"#.into()], None),
        ];
        for (lines, want) in cases {
            std::fs::write(&p, lines.join("\n") + "\n").unwrap();
            assert_eq!(goal(&p).as_deref(), want, "{lines:?}");
            assert_eq!(goal_anywhere(&p).as_deref(), want, "{lines:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    fn store_in(name: &str) -> (PathBuf, MemoryStore) {
        let d = std::env::temp_dir().join(format!("atlas-resume-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let s = MemoryStore::open(&d.join("memory.sqlite")).unwrap();
        (d, s)
    }

    const NOW: &str = "2026-11-04T18:00:00Z";

    /// Writes a record at `time` by `client`.
    fn add(store: &mut MemoryStore, key: &str, kind: &str, sub: &str, scope: &str, time: &str, client: &str) {
        let d = crate::memory::Draft { title: format!("{key} title"), reason: "t".into(), author: "t".into(), client: client.into(), time: Some(time.into()), ..Default::default() };
        let mut w = store.writer().unwrap();
        w.create(key, kind, sub, scope, &d, None).unwrap();
        w.commit().unwrap();
    }

    fn the_card(store: &MemoryStore) -> String {
        card(store, &OrchSettings::default(), "", NOW, false).unwrap().unwrap_or_default()
    }

    fn lanes_line(card: &str) -> String {
        card.lines().find(|l| l.starts_with("- live lanes")).unwrap_or("").to_string()
    }

    #[test]
    fn a_live_lane_has_a_work_record_within_the_cutoff() {
        let (d, mut s) = store_in("live");
        // Six days old: live. Eight days old: not.
        add(&mut s, "journal:FRESH:0001", "journal", "decision", "FRESH", "2026-10-29T18:00:00Z", "t");
        add(&mut s, "journal:STALE:0001", "journal", "decision", "STALE", "2026-10-27T18:00:00Z", "t");
        add(&mut s, "journal:STALE:0002", "journal", "question", "STALE", "2026-10-27T18:00:00Z", "t");
        // A queued entry, the queue's note, and a brief addition while the
        // entry waits: planned, not started.
        add(&mut s, "journal:PLANNED:0001", "journal", crate::queue::ENTRY, "PLANNED", NOW, "claude-code");
        add(&mut s, "journal:PLANNED:0002", "journal", "note", "PLANNED", NOW, "atlas-queue");
        add(&mut s, "journal:PLANNED:0003", "journal", "note", "PLANNED", NOW, "claude-code");
        // The hook's own cap note is not work either.
        add(&mut s, "journal:CAPPED:0001", "journal", "cap", "CAPPED", NOW, "atlas-hook");
        // An agent's note in a lane with nothing queued is work.
        add(&mut s, "journal:NOTED:0001", "journal", "note", "NOTED", NOW, "t");
        // Not lanes.
        add(&mut s, "journal:agent:a1:0001", "journal", "decision", "agent:a1", NOW, "t");
        add(&mut s, "journal:session:s1:0001", "journal", "checkpoint", "session:s1", NOW, "t");
        let c = the_card(&s);
        let lanes = lanes_line(&c);
        assert!(lanes.contains("FRESH") && lanes.contains("NOTED"), "{c}");
        for gone in ["STALE", "PLANNED", "CAPPED", "agent:", "session:"] {
            assert!(!lanes.contains(gone), "{gone}: {c}");
        }
        assert!(!c.contains("journal:STALE:0002"), "a dead lane's question is not listed: {c}");
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn open_questions_agree_everywhere() {
        let (d, mut s) = store_in("open");
        add(&mut s, "journal:L:0001", "journal", "question", "L", NOW, "t");
        add(&mut s, "journal:L:0002", "journal", "question", "L", NOW, "t");
        add(&mut s, "journal:L:0003", "journal", "question", "L", NOW, "t");
        // Answered from another lane, and retired.
        add(&mut s, "journal:M:0001", "journal", "decision", "M", NOW, "t");
        {
            let mut w = s.writer().unwrap();
            w.add_refs("journal:M:0001", &[("answers".into(), "journal:L:0002".into())]).unwrap();
            w.retire("journal:L:0003", "t").unwrap();
            w.commit().unwrap();
        }
        let keys: Vec<String> = s.open_questions().unwrap().into_iter().map(|(_, k, _)| k).collect();
        assert_eq!(keys, ["journal:L:0001"]);
        let l = s.lanes().unwrap().into_iter().find(|l| l.lane == "L").unwrap();
        assert_eq!(l.open_questions, 1);
        let st = crate::checkpoint::state(&s, "x", "2026-11-01T00:00:00Z", "", crate::checkpoint::Lanes::Store).unwrap();
        assert_eq!(st.open.len(), 1, "{:?}", st.open);
        assert!(st.open[0].starts_with("journal:L:0001"), "{:?}", st.open);
        assert!(lanes_line(&the_card(&s)).contains("L (1 question)"));
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_big_store_fits_the_budget() {
        let (d, mut s) = store_in("budget");
        for i in 0..40 {
            let lane = format!("QX{i:02}-PADDINGPADDINGPADDING");
            add(&mut s, &format!("journal:{lane}:0001"), "journal", "question", &lane, NOW, "t");
            add(&mut s, &format!("handoff:{lane}:001"), "handoff", "", &lane, NOW, "t");
            add(&mut s, &format!("handoff:session:{i}:001"), "handoff", "", &format!("session:{i}"), NOW, "t");
        }
        let c = the_card(&s);
        assert!(c.len() <= CARD_BYTES, "{} bytes: {c}", c.len());
        assert!(c.contains("(6 of 40)") && c.ends_with("something more pressing.\n"), "{c}");
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_a_cleared_session_lists_the_previous_agents() {
        let (d, mut s) = store_in("agents");
        add(&mut s, "journal:L:0001", "journal", "decision", "L", NOW, "t");
        let sub = d.join("sessions").join("old").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(d.join("sessions").join("old.jsonl"), "{}
").unwrap();
        std::fs::write(sub.join("agent-aaaa1111.meta.json"), r#"{"description":"the exporter","finished":false}"#).unwrap();
        std::fs::write(sub.join("agent-bbbb2222.meta.json"), r#"{"description":"done already","finished":true}"#).unwrap();
        let new = d.join("sessions").join("new.jsonl");
        let at = |cleared: bool| card(&s, &OrchSettings::default(), &new.to_string_lossy(), &crate::memory::now(), cleared).unwrap().unwrap();
        // The card reads agents at the record time `now`, not the wall clock.
        assert_eq!(crate::queue::system_time("1970-01-01T00:01:00Z"), Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(60)));
        let c = at(true);
        assert!(c.contains("- running agents (previous session): the exporter (aaaa1111, "), "{c}");
        assert!(!c.contains("done already"), "{c}");
        assert!(!at(false).contains("running agents"));
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fit_cuts_at_an_item_boundary_and_keeps_the_ask() {
        let long = format!("- list: {}", (0..100).map(|i| format!("key:{i:03}")).collect::<Vec<_>>().join(", "));
        let out = fit(&[long, "- short line".into()], "ASK\n", 400);
        assert!(out.len() <= 400, "{}", out.len());
        let cut = out.lines().nth(1).unwrap();
        assert!(cut.ends_with(" ...") && !cut.ends_with(", ..."), "{cut}");
        assert!(cut.trim_end_matches(" ...").rsplit(", ").next().unwrap().len() == 7, "{cut}");
        assert!(out.ends_with("ASK\n"));
    }

    /// A goal record written as Claude Code writes it, with `pad` bytes of
    /// condition-free text so the line can span read blocks.
    fn goal_status(met: bool, condition: &str, pad: usize) -> String {
        format!(r#"{{"type":"attachment","pad":"{}","attachment":{{"type":"goal_status","met":{met},"sentinel":true,"condition":"{condition}"}}}}"#, "x".repeat(pad))
    }

    #[test]
    fn the_goal_search_reads_backward_across_blocks() {
        let d = std::env::temp_dir().join(format!("atlas-resume-blocks-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("t.jsonl");
        let block = usize::try_from(GOAL_BLOCK).unwrap();
        let filler = format!("{{\"type\":\"user\",\"pad\":\"{}\"}}", "y".repeat(1000));
        // A goal line longer than a block, with blocks of other lines after
        // it, CRLF endings: found whole.
        let after = vec![filler.as_str(); 3 * block / filler.len()].join("\r\n");
        std::fs::write(&p, format!("{}\r\n{}\r\n{after}\r\n", goal_status(false, "old", 0), goal_status(false, "ship it", 2 * block))).unwrap();
        assert_eq!(goal(&p).as_deref(), Some("ship it"));
        // The newest record decides, though an older one is unmet.
        std::fs::write(&p, format!("{}\n{after}\n{}\n", goal_status(false, "old", 0), goal_status(true, "done", 0))).unwrap();
        assert_eq!(goal(&p), None);
        // A record wholly before the searched tail is not read.
        let far = vec![filler.as_str(); usize::try_from(GOAL_TAIL_BYTES).unwrap() / filler.len() + 2].join("\n");
        std::fs::write(&p, format!("{}\n{far}\n", goal_status(false, "too far back", 0))).unwrap();
        assert_eq!(goal(&p), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_goal_is_never_cut_and_old_handoffs_are_not_listed() {
        let (d, mut s) = store_in("goal");
        for i in 0..40 {
            let lane = format!("QX{i:02}-{}", "PADDING".repeat(6));
            add(&mut s, &format!("journal:{lane}:0001"), "journal", "question", &lane, NOW, "t");
        }
        // The only handoff the lanes line does not name, and past LIVE_DAYS.
        add(&mut s, "handoff:session:old:001", "handoff", "", "session:old", "2026-10-01T00:00:00Z", "t");
        let sessions = d.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let old = sessions.join("old.jsonl");
        let new = sessions.join("new.jsonl");
        let the_card = |condition: &str| {
            std::fs::write(&old, goal_status(false, condition, 0) + "\n").unwrap();
            card(&s, &OrchSettings::default(), &new.to_string_lossy(), NOW, false).unwrap().unwrap()
        };
        // A full card: the goal and the instruction are whole.
        let long_goal = format!("{} end of goal", "every word counts, ".repeat(14));
        assert!(long_goal.chars().count() <= GOAL_CHARS);
        let c = the_card(&long_goal);
        assert!(c.len() <= CARD_BYTES, "{} bytes: {c}", c.len());
        assert!(c.contains(&format!("restate it with /goal to keep it): {long_goal}\n")), "{c}");
        assert!(c.contains(" ...\n") && c.ends_with("something more pressing.\n"), "{c}");
        assert!(!c.contains("handoff:session:old:001"), "{c}");
        // A goal that finished is not offered again, in the shape it finishes in.
        std::fs::write(&old, goal_status(false, "a finished goal", 0) + "\n" + r#"{"type":"attachment","attachment":{"type":"goal_status","met":true,"iterations":3}}"# + "\n").unwrap();
        let c = card(&s, &OrchSettings::default(), &new.to_string_lossy(), NOW, false).unwrap().unwrap();
        assert!(!c.contains("a finished goal") && !c.contains("previous session's goal"), "{c}");
        // A goal too long for the card is named by where it is, not cut.
        let longer = format!("{} the end", "x".repeat(GOAL_CHARS));
        let c = the_card(&longer);
        assert!(!c.contains("xxxxx") && c.contains(&format!("is {} characters", longer.chars().count())) && c.contains(&old.display().to_string()), "{c}");
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }
}
