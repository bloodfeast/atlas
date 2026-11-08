//! Whether a person typed a prompt, read from the session's transcript.
//!
//! `UserPromptSubmit` also fires on scheduled tasks, `/loop` iterations and
//! messages from other sessions, and its input does not say which. The
//! transcript does: a prompt's record, or its `queued_command` attachment,
//! carries `origin.kind`. `human` under a non-`sdk-` entrypoint and not
//! `isMeta` is a person; anything else, an absent origin included, is not.
//!
//! The hook runs before its prompt's record is written, so a capture is held
//! ([`super::Hold`]) until the record is there, and only a record past the end
//! the transcript had at the hold can be it: an older prompt may share its head.
//! A record's timestamp cannot say this, as it is stamped before the hook ends.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::json::Value;

/// How much of a transcript's end is read. A prompt further back than this,
/// behind one turn's output, reads as not found.
const TAIL_BYTES: u64 = 4 << 20;
/// How many leading normalised bytes of a prompt identify it: a record may
/// differ from the hook's prompt further on (a paste expanded, an image added).
const HEAD_BYTES: usize = 200;
/// A read's block size.
const BLOCK: u64 = 64 << 10;

/// What a transcript says about a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A person's own prompt.
    Person,
    /// A prompt no person typed.
    Other,
    /// No record of it, or no transcript to read.
    Unknown,
}

/// The hash a prompt is found by, so the spool never holds unredacted words.
pub fn head_hash(text: &str) -> String {
    let norm = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut end = norm.len().min(HEAD_BYTES);
    while !norm.is_char_boundary(end) {
        end -= 1;
    }
    crate::sha256::hex(&norm.as_bytes()[..end])
}

fn text_of(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let mut out = String::new();
    for b in content.as_array()? {
        if b.get("type").and_then(Value::as_str) == Some("text") {
            out.push_str(b.get("text").and_then(Value::as_str).unwrap_or(""));
        }
    }
    Some(out)
}

/// One transcript line read as a prompt.
struct PromptRecord {
    text: String,
    /// A person typed it.
    person: bool,
    /// Claude Code added it inside a running turn (a skill's body, a reminder),
    /// so it starts none. A `queueSkipAttachments` record can start one.
    companion: bool,
}

/// One transcript line as a prompt record. `None` for anything else, a tool
/// result included.
fn prompt_record(line: &str) -> Option<PromptRecord> {
    let user = line.contains("\"type\":\"user\"") && !line.contains("\"type\":\"tool_result\"");
    if !user && !line.contains("\"queued_command\"") {
        return None;
    }
    let v = crate::json::parse(line).ok()?;
    let (holder, text) = match v.get("type").and_then(Value::as_str)? {
        "user" => (&v, text_of(v.get("message")?.get("content")?)?),
        "attachment" => {
            let a = v.get("attachment").filter(|a| a.get("type").and_then(Value::as_str) == Some("queued_command"))?;
            (a, text_of(a.get("prompt")?)?)
        }
        _ => return None,
    };
    let human = holder.get("origin").and_then(|o| o.get("kind")).and_then(Value::as_str) == Some("human");
    let meta = holder.get("isMeta").and_then(Value::as_bool) == Some(true);
    let headless = v.get("entrypoint").and_then(Value::as_str).is_some_and(|e| e.starts_with("sdk-"));
    let companion = meta && holder.get("origin").is_none() && v.get("turnCompanion").and_then(Value::as_bool) == Some(true) && v.get("queueSkipAttachments").is_none();
    Some(PromptRecord { text, person: human && !meta && !headless, companion })
}

/// `f`'s first answer over the transcript's lines that start at or after `from`,
/// newest first, within its last [`TAIL_BYTES`]. Read backward in blocks, since
/// the record sought is usually near the end.
fn newest<T>(transcript: &Path, from: u64, mut f: impl FnMut(&str) -> Option<T>) -> Option<T> {
    let mut file = std::fs::File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    // One byte before `from`, its newline: a line starting at `from` is whole,
    // and a line it falls inside started earlier and is dropped.
    let floor = len.saturating_sub(TAIL_BYTES).max(from.saturating_sub(1));
    let mut pos = len;
    // The start of a line whose beginning is in an earlier block.
    let mut carry: Vec<u8> = Vec::new();
    while pos > floor {
        let start = pos.saturating_sub(BLOCK).max(floor);
        let mut buf = vec![0u8; usize::try_from(pos - start).ok()?];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut buf).ok()?;
        buf.extend_from_slice(&carry);
        pos = start;
        let first = if pos > 0 || from > 0 { buf.iter().position(|b| *b == b'\n').map_or(buf.len(), |i| i + 1) } else { 0 };
        for line in buf[first..].split(|b| *b == b'\n').rev() {
            if let Some(found) = f(String::from_utf8_lossy(line).trim_end_matches('\r')) {
                return Some(found);
            }
        }
        buf.truncate(first);
        carry = buf;
    }
    None
}

/// What says who wrote the prompt with this [`head_hash`], held when the
/// transcript was `from` bytes long: the oldest such record from there on, as a
/// later prompt may share the head before this one's hold is settled.
pub fn of_prompt(transcript: &Path, head: &str, from: u64) -> Origin {
    let mut found = Origin::Unknown;
    newest(transcript, from, |l| {
        if let Some(p) = prompt_record(l).filter(|p| head_hash(&p.text) == head) {
            found = if p.person { Origin::Person } else { Origin::Other };
        }
        None::<()>
    });
    found
}

/// The transcript's newest prompt record: whether a person typed it, and its
/// [`head_hash`], which names the turn.
pub fn last_prompt(transcript: &Path) -> Option<(bool, String)> {
    newest(transcript, 0, |l| prompt_record(l).filter(|p| !p.companion)).map(|p| (p.person, head_hash(&p.text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of_last(transcript: &Path) -> Origin {
        last_prompt(transcript).map_or(Origin::Unknown, |(person, _)| if person { Origin::Person } else { Origin::Other })
    }

    fn write(tag: &str, lines: &[String]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("atlas-ideas-origin-{tag}-{}.jsonl", std::process::id()));
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    fn user(text: &str, tail: &str) -> String {
        format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}{tail}}}"#)
    }

    #[test]
    fn only_a_human_origin_outside_a_headless_run_is_a_person() {
        let t = write(
            "kinds",
            &[
                user("typed words", r#","origin":{"kind":"human"},"entrypoint":"cli""#),
                user("loop words", r#","isMeta":true,"entrypoint":"cli""#),
                user("other session words", r#","entrypoint":"cli""#),
                user("headless words", r#","origin":{"kind":"human"},"entrypoint":"sdk-cli""#),
                user("meta words", r#","origin":{"kind":"human"},"isMeta":true"#),
            ],
        );
        assert_eq!(of_prompt(&t, &head_hash("typed  words"), 0), Origin::Person);
        for w in ["loop words", "other session words", "headless words", "meta words"] {
            assert_eq!(of_prompt(&t, &head_hash(w), 0), Origin::Other, "{w}");
        }
        assert_eq!(of_prompt(&t, &head_hash("never said"), 0), Origin::Unknown);
        assert_eq!(of_prompt(Path::new("Z:/no/such/file.jsonl"), "x", 0), Origin::Unknown);
        assert_eq!(of_last(&t), Origin::Other);
    }

    #[test]
    fn a_queued_prompt_counts_and_a_tool_result_is_not_a_prompt() {
        let queued = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"queued words","origin":{"kind":"human"}},"entrypoint":"cli"}"#.to_string();
        let result = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]},"entrypoint":"cli"}"#.to_string();
        let note = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"<task-notification>","origin":{"kind":"task-notification"}}}"#.to_string();
        let t = write("queued", &[queued.clone(), result.clone()]);
        assert_eq!(of_prompt(&t, &head_hash("queued words"), 0), Origin::Person);
        assert_eq!(of_last(&t), Origin::Person);
        let t = write("note", &[queued, note, result]);
        assert_eq!(of_last(&t), Origin::Other);
    }

    #[test]
    fn a_long_prompt_is_found_by_its_head_and_a_list_content_reads() {
        let long = "word ".repeat(200);
        let line = format!(r#"{{"type":"user","message":{{"content":[{{"type":"text","text":"{long} and an image after"}},{{"type":"image"}}]}},"origin":{{"kind":"human"}}}}"#);
        let t = write("long", &[line]);
        assert_eq!(of_prompt(&t, &head_hash(&long), 0), Origin::Person);
    }

    #[test]
    fn only_whole_lines_are_read_newest_first_across_blocks() {
        let pad = format!(r#"{{"type":"user","message":{{"content":"{}"}},"origin":{{"kind":"human"}}}}"#, "x".repeat(5 << 20));
        let t = write("tail", &[pad, user("end words", r#","origin":{"kind":"human"}"#)]);
        // The pad starts before the read's floor: it is dropped, never half read.
        let mut seen = Vec::new();
        assert_eq!(newest(&t, 0, |l| {
            if !l.is_empty() {
                seen.push(l.len());
            }
            None::<()>
        }), None);
        assert_eq!(seen.len(), 1, "the last line alone");
        let spans = (BLOCK * 3) as usize;
        let t = write("span", &[user("old words", r#","origin":{"kind":"human"}"#), format!(r#"{{"type":"assistant","pad":"{}"}}"#, "x".repeat(spans))]);
        assert_eq!(of_prompt(&t, &head_hash("old words"), 0), Origin::Person, "a line longer than a block is whole");
        assert_eq!(of_last(&t), Origin::Person);
    }

    /// The transcript of `lines`, and where each one starts.
    fn at_offsets(tag: &str, lines: &[String]) -> (std::path::PathBuf, Vec<u64>) {
        let mut at = 0;
        let offsets = lines
            .iter()
            .map(|l| {
                let o = at;
                at += l.len() as u64 + 1;
                o
            })
            .collect();
        (write(tag, lines), offsets)
    }

    #[test]
    fn only_a_record_from_the_hold_on_can_be_the_prompt() {
        let typed = |text: &str| user(text, r#","origin":{"kind":"human"},"entrypoint":"cli""#);
        let peer = |text: &str| user(text, r#","isMeta":true,"origin":{"kind":"peer"},"entrypoint":"cli""#);
        let shared = "h".repeat(HEAD_BYTES);
        let (a, b) = (format!("{shared} one tail"), format!("{shared} other tail"));
        let head = head_hash(&b);
        assert_eq!(head, head_hash(&a), "the two share a head");
        let between = user("between", "");

        // A peer's prompt, its record not yet written, sharing a head with an
        // older typed prompt: unknown while it lags, never a person.
        let (t, o) = at_offsets("older-typed", &[typed(&a), between.clone()]);
        let end = o[1] + between.len() as u64 + 1;
        assert_eq!(of_prompt(&t, &head, 0), Origin::Person, "what matching from the start would say");
        assert_eq!(of_prompt(&t, &head, end), Origin::Unknown);
        let (t, o) = at_offsets("then-peer", &[typed(&a), between.clone(), peer(&b)]);
        assert_eq!(of_prompt(&t, &head, o[2]), Origin::Other, "a line starting at the hold is whole");

        // The mirror: a person's prompt sharing a head with an older non-person
        // record is not dropped.
        let (t, o) = at_offsets("older-peer", &[peer(&a), between.clone()]);
        let end = o[1] + between.len() as u64 + 1;
        assert_eq!(of_prompt(&t, &head, 0), Origin::Other, "what matching from the start would say");
        assert_eq!(of_prompt(&t, &head, end), Origin::Unknown);
        let (t, o) = at_offsets("then-typed", &[peer(&a), between, typed(&b)]);
        assert_eq!(of_prompt(&t, &head, o[2]), Origin::Person);

        // Two prompts held in a row: each is its own, the oldest from its hold.
        let (t, o) = at_offsets("two-held", &[peer(&a), typed(&b)]);
        assert_eq!(of_prompt(&t, &head, o[0]), Origin::Other);
        assert_eq!(of_prompt(&t, &head, o[1]), Origin::Person);

        // A hold inside a line: that line started earlier and is not the prompt.
        let (t, _) = at_offsets("inside", &[typed(&a)]);
        assert_eq!(of_prompt(&t, &head, 5), Origin::Unknown);
        assert_eq!(of_prompt(&t, &head, 1), Origin::Unknown, "the floor is the file's start, and it is still inside");
    }

    #[test]
    fn a_record_added_inside_a_turn_does_not_start_one() {
        let typed = user("typed words", r#","origin":{"kind":"human"},"entrypoint":"cli""#);
        let added = user("added words", r#","isMeta":true,"turnCompanion":true,"entrypoint":"cli""#);
        let t = write("companion", &[typed.clone(), added.clone()]);
        assert_eq!(of_last(&t), Origin::Person);
        assert_eq!(last_prompt(&t).unwrap().1, head_hash("typed words"), "the turn is the person's");

        // Any other meta record still reads as a prompt no person typed.
        for tail in [
            r#","isMeta":true,"entrypoint":"cli""#,
            r#","isMeta":true,"turnCompanion":false,"entrypoint":"cli""#,
            r#","isMeta":true,"turnCompanion":true,"queueSkipAttachments":true,"entrypoint":"cli""#,
            r#","isMeta":true,"turnCompanion":true,"origin":{"kind":"peer"},"entrypoint":"cli""#,
        ] {
            let t = write("not-companion", &[typed.clone(), user("other words", tail)]);
            assert_eq!(of_last(&t), Origin::Other, "{tail}");
        }
        // A held prompt's own record is still read as what it is.
        let t = write("companion-held", &[added]);
        assert_eq!(of_prompt(&t, &head_hash("added words"), 0), Origin::Other);
    }
}
