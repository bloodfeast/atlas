//! Repeat answers within one agent's session.
//!
//! An agent that makes the same read call twice already holds the first
//! answer in its context. When the second answer is the same text, it is
//! one line, `unchanged since your call #N`; when it changed, and the
//! changed lines are under half the answer, only those lines come back
//! (`+ line` new, `- line` gone). `fresh: true` on any call gives the whole
//! answer.
//!
//! The server knows who is calling from the PreToolUse hook, which names
//! the session, the agent, the tool, its input and its tool_use_id right
//! before the call runs. Hooks are noted as they are read (on the reader
//! thread when a hook worker answers them, see [`Announcer`]), so several
//! agents may announce calls before any of them runs; a call sees only the
//! hooks that came before it in the stream, however far ahead the reader
//! has read, so attribution follows stream order. A call takes the
//! announcement whose tool_use_id the client's `_meta` names, else the
//! newest one of the same tool with the same input, else (an announcement
//! that did not carry its input) the newest one of the same tool; answers
//! are then compared per agent. Two agents that announce the same tool with
//! the same input at once may swap callers, which is harmless: each was
//! about to receive the answer to that same call.
//!
//! Without the hooks there is no agent identity, and answers are compared
//! per server process: one server serves a session and all its subagents,
//! so a subagent may be told `unchanged since call #N` of an answer only its
//! parent holds. That reply always names `fresh: true`, which gives the
//! whole answer. Once a PreToolUse hook has been seen, a call it did not
//! announce is not compared at all. A PreCompact hook forgets the session's
//! answers, since compaction drops them from the context. The work-queue
//! tools and the hook itself always answer whole.
//!
//! The answers kept for comparison are bounded: past [`SEEN_BYTES`] the
//! least recently used are forgotten, and a forgotten answer is simply
//! answered whole again.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::json::Value;

/// Read tools that always answer whole: the work queue, a lane's journal
/// (see [`dedupable`]) and the hook.
const WHOLE: &[&str] = &["hook", "decision_queue", "awaiting", "journal_status", "discussion_status", "read_discussion", "list_discussions", "misses", "next_id"];

/// The caller of every call when no hook names one: the server process.
pub const PROCESS: &str = "";

/// The most bytes of answers kept for comparison; past it the least
/// recently used are dropped.
pub const SEEN_BYTES: usize = 8 << 20;

/// The most announcements waiting for their call (a denied call never
/// comes; the oldest are dropped past this).
const PENDING: usize = 256;

/// One PreToolUse announcement.
#[derive(Debug, Clone)]
struct Announcement {
    /// `session/agent`.
    caller: String,
    /// The tool, without its server prefix.
    tool: String,
    /// The tool input, `Null` when the hook did not carry it.
    input: Value,
    /// Claude Code's tool_use_id, empty when the hook did not carry it.
    use_id: String,
    /// Its place in the stream (see [`Announcer::mark`]); 0 when it was
    /// noted in order by the thread that answers calls.
    seq: u64,
}

/// What the hooks told the server, shared between the thread that reads
/// hooks and the thread that answers calls.
#[derive(Debug, Default)]
struct Heard {
    /// Announced calls not yet made, oldest first.
    pending: VecDeque<Announcement>,
    /// The place of the first PreToolUse that named a caller: from then on
    /// only an announced call is compared.
    hooked: Option<u64>,
    /// `session/` prefixes a PreCompact named whose answers are still to be
    /// forgotten, each with the PreCompact's place in the stream.
    forget: Vec<(String, u64)>,
    /// Hooks noted by the reading thread so far.
    read: u64,
}

/// A handle that notes hook calls, for the thread that reads them.
///
/// That thread reads ahead of the thread that answers calls, so it may note
/// a hook that comes after a call still waiting to run. Each hook it notes
/// gets the next place in the stream, and before each other message it
/// takes the [`Announcer::mark`] of what it has noted so far; the call is
/// then answered with that mark as its horizon ([`Dedup::set_horizon`]),
/// and sees exactly the hooks that came before it in the stream.
#[derive(Debug, Clone, Default)]
pub struct Announcer(Arc<Mutex<Heard>>);

impl Announcer {
    /// Notes a hook call read from the stream: PreToolUse announces an
    /// agent's next call; PreCompact forgets the session's answers.
    pub fn hook(&self, args: &Value) {
        self.note(args, true);
    }

    /// The place in the stream of the last hook noted by [`Announcer::hook`]:
    /// the horizon of a message read now.
    pub fn mark(&self) -> u64 {
        self.0.lock().map(|h| h.read).unwrap_or(0)
    }

    /// `stamped`: noted by the reading thread, ahead of the calls; else in
    /// order by the thread that answers calls, and so visible to every
    /// call after it.
    fn note(&self, args: &Value, stamped: bool) {
        // A `${path}` the hook input lacked may arrive unsubstituted; it means empty.
        let get = |k: &str| args.get(k).and_then(Value::as_str).filter(|x| !(x.starts_with("${") && x.ends_with('}'))).unwrap_or("");
        let session = get("session_id");
        if session.is_empty() {
            return;
        }
        let Ok(mut h) = self.0.lock() else { return };
        let event = get("event");
        if !matches!(event, "PreToolUse" | "PreCompact") {
            return;
        }
        let seq = if stamped {
            h.read += 1;
            h.read
        } else {
            0
        };
        match event {
            "PreToolUse" => {
                h.hooked = Some(h.hooked.map_or(seq, |s| s.min(seq)));
                let tool = get("tool_name");
                // Bash, Read and the rest are announced too and never come
                // here; only an MCP tool (or a bare name) can be a call.
                if tool.contains("__") && !tool.starts_with("mcp__") {
                    return;
                }
                if !tool.contains("__") && tool.chars().next().is_some_and(char::is_uppercase) {
                    return;
                }
                let input = match args.get("tool_input") {
                    Some(o @ Value::Object(_)) => o.clone(),
                    // An `mcp_tool` hook substitutes an object as its JSON text.
                    Some(Value::String(t)) if t.starts_with('{') => crate::json::parse(t).ok().filter(|p| p.as_object().is_some()).unwrap_or(Value::Null),
                    _ => Value::Null,
                };
                let a = Announcement { caller: format!("{session}/{}", get("agent_id")), tool: short(tool).to_string(), input, use_id: get("tool_use_id").to_string(), seq };
                if h.pending.len() >= PENDING {
                    h.pending.pop_front();
                }
                h.pending.push_back(a);
            }
            _ => {
                let p = format!("{session}/");
                // In order, it applies now; read ahead, when the first call
                // after it in the stream takes its caller.
                if !stamped {
                    h.pending.retain(|a| !a.caller.starts_with(&p));
                }
                h.forget.push((p, seq));
            }
        }
    }
}

/// A kept answer's key: (caller, tool, arguments).
type Key = (String, String, String);

/// The repeat state of one server.
#[derive(Debug, Default)]
pub struct Dedup {
    /// What the hooks announced.
    heard: Announcer,
    /// Key to (the caller's call number, the answer, its last use).
    seen: HashMap<Key, (usize, String, u64)>,
    /// Last use to key, oldest first.
    order: BTreeMap<u64, Key>,
    /// Bytes of answers in `seen`.
    bytes: usize,
    /// The use counter.
    tick: u64,
    /// The stream place of the message being answered (see
    /// [`Dedup::set_horizon`]).
    horizon: Option<u64>,
    /// Calls per caller.
    calls: HashMap<String, usize>,
}

/// Whether a routed call may answer with a reference to an earlier answer.
pub fn dedupable(tool: &str, read_only: bool, args: &Value) -> bool {
    read_only && !WHOLE.contains(&tool) && !(tool == "recall" && args.get("lane").is_some())
}

/// The tool name a hook reports (`mcp__atlas__lookup`,
/// `mcp__plugin_atlas_atlas__note`) without its server prefix.
fn short(tool: &str) -> &str {
    tool.rsplit("__").next().unwrap_or(tool)
}

impl Dedup {
    /// The handle a hook-reading thread notes hooks through.
    pub fn announcer(&self) -> Announcer {
        self.heard.clone()
    }

    /// Notes a hook call in order, on the thread that answers calls: every
    /// later call sees it (see [`Announcer::hook`]).
    pub fn hook(&mut self, args: &Value) {
        self.heard.note(args, false);
    }

    /// The stream place of the message about to be answered (an
    /// [`Announcer::mark`] taken when it was read): its call sees only the
    /// hooks noted up to it. None, the default, sees every hook noted.
    pub fn set_horizon(&mut self, horizon: Option<u64>) {
        self.horizon = horizon;
    }

    /// The caller of a call to `tool` (as the client named it) with the
    /// arguments `args` as sent: the agent whose announcement matches it
    /// (see the module doc; an announcement is used once, and one read
    /// after the call in the stream is not seen); without the hooks,
    /// [`PROCESS`]; None for a call the hooks did not announce.
    /// `use_id` is the tool_use_id the client sent with the call, if any.
    pub fn caller(&mut self, tool: &str, args: &Value, use_id: Option<&str>) -> Option<String> {
        let horizon = self.horizon.unwrap_or(u64::MAX);
        let (found, hooked, forget) = {
            let Ok(mut h) = self.heard.0.lock() else { return None };
            // The PreCompacts before this call, in stream order: each drops
            // the session's announcements that came before it.
            let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut h.forget).into_iter().partition(|f| f.1 <= horizon);
            h.forget = later;
            for (p, s) in &due {
                if *s > 0 {
                    h.pending.retain(|a| !(a.caller.starts_with(p.as_str()) && a.seq < *s));
                }
            }
            let p = &h.pending;
            let seen = |a: &Announcement| a.seq <= horizon;
            let by_id = use_id.filter(|u| !u.is_empty()).and_then(|u| p.iter().rposition(|a| seen(a) && a.use_id == u));
            let by_input = || p.iter().rposition(|a| seen(a) && a.tool == tool && !a.input.is_null() && a.input == *args);
            let by_tool = || p.iter().rposition(|a| seen(a) && a.tool == tool && a.input.is_null());
            let at = by_id.or_else(by_input).or_else(by_tool);
            (at.and_then(|i| h.pending.remove(i)), h.hooked.is_some_and(|s| s <= horizon), due)
        };
        for (p, _) in forget {
            let gone: Vec<Key> = self.seen.keys().filter(|k| k.0.starts_with(&p)).cloned().collect();
            for k in gone {
                self.drop_seen(&k);
            }
        }
        match found {
            Some(a) => Some(a.caller),
            None if !hooked => Some(PROCESS.to_string()),
            None => None,
        }
    }

    fn drop_seen(&mut self, k: &Key) {
        if let Some((_, text, t)) = self.seen.remove(k) {
            self.bytes -= text.len();
            self.order.remove(&t);
        }
    }

    /// The answer to send: `text` itself, a one-line reference to the same
    /// earlier answer, or the lines that changed since it. Every call of the
    /// caller is counted; only a `reuse` call (see [`dedupable`]) is kept
    /// and compared.
    pub fn answer(&mut self, caller: &str, tool: &str, args: &Value, text: String, reuse: bool) -> String {
        let n = {
            let c = self.calls.entry(caller.to_string()).or_default();
            *c += 1;
            *c
        };
        if !reuse {
            return text;
        }
        let key = (caller.to_string(), tool.to_string(), args.to_string());
        // Per process the earlier call may be another agent's.
        let whose = if caller == PROCESS { "call" } else { "your call" };
        let out = match self.seen.get(&key) {
            Some((k, prev, _)) if *prev == text => format!("unchanged since {whose} #{k} with these arguments; fresh: true gives it whole\n"),
            Some((k, prev, _)) => match changed_lines(prev, &text) {
                Some(d) if d.len() * 2 < text.len() => format!("changed since {whose} #{k} with these arguments (+ new line, - gone; fresh: true gives it whole):\n{d}"),
                _ => text.clone(),
            },
            None => text.clone(),
        };
        self.drop_seen(&key);
        // An answer over the whole budget is not kept: its repeat answers whole.
        if text.len() <= SEEN_BYTES {
            self.tick += 1;
            self.bytes += text.len();
            self.order.insert(self.tick, key.clone());
            self.seen.insert(key, (n, text, self.tick));
            while self.bytes > SEEN_BYTES {
                let Some((_, k)) = self.order.pop_first() else { break };
                if let Some((_, t, _)) = self.seen.remove(&k) {
                    self.bytes -= t.len();
                }
            }
        }
        out
    }

    /// Bytes of answers kept for comparison.
    pub fn kept_bytes(&self) -> usize {
        self.bytes
    }
}

/// The lines of `new` not in `old` (`+ `) and of `old` not in `new` (`- `),
/// each line counted as often as it occurs; None when nothing differs.
fn changed_lines(old: &str, new: &str) -> Option<String> {
    let mut left: HashMap<&str, isize> = HashMap::new();
    for l in old.lines() {
        *left.entry(l).or_default() += 1;
    }
    let mut out = String::new();
    for l in new.lines() {
        match left.get_mut(l) {
            Some(c) if *c > 0 => *c -= 1,
            _ => {
                out.push_str("+ ");
                out.push_str(l);
                out.push('\n');
            }
        }
    }
    for l in old.lines() {
        if let Some(c) = left.get_mut(l) {
            if *c > 0 {
                *c -= 1;
                out.push_str("- ");
                out.push_str(l);
                out.push('\n');
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn pre(d: &mut Dedup, session: &str, agent: &str, tool: &str) {
        d.hook(&json!({"event": "PreToolUse", "session_id": session, "agent_id": agent, "tool_name": format!("mcp__atlas__{tool}")}));
    }

    /// A PreToolUse as `hooks/hooks.json` sends it: the input as JSON text.
    fn pre_with(a: &Announcer, session: &str, agent: &str, tool: &str, input: &Value, use_id: &str) {
        a.hook(&json!({"event": "PreToolUse", "session_id": session, "agent_id": agent, "tool_name": format!("mcp__plugin_atlas_atlas__{tool}"), "tool_input": input.to_string(), "tool_use_id": use_id}));
    }

    #[test]
    fn a_repeat_is_one_line_and_a_change_is_its_lines() {
        let mut d = Dedup::default();
        let a = json!({"id": "X-1"});
        let big = (0..40).map(|i| format!("row {i}\n")).collect::<String>();
        assert_eq!(d.answer("s/", "lookup", &a, big.clone(), true), big);
        let again = d.answer("s/", "lookup", &a, big.clone(), true);
        assert!(again.starts_with("unchanged since your call #1") && again.contains("fresh: true"), "{again}");
        let changed = big.replace("row 7\n", "row seven\n");
        let diff = d.answer("s/", "lookup", &a, changed.clone(), true);
        assert!(diff.contains("changed since your call #2") && diff.contains("fresh: true") && diff.contains("+ row seven\n") && diff.contains("- row 7\n"), "{diff}");
        // A change as big as the answer comes back whole.
        let other = "entirely new\n".to_string();
        assert_eq!(d.answer("s/", "lookup", &a, other.clone(), true), other);
        // Other arguments, or another caller, are a first answer.
        assert_eq!(d.answer("s/", "lookup", &json!({"id": "X-2"}), big.clone(), true), big);
        assert_eq!(d.answer("s/a1", "lookup", &a, other.clone(), true), other);
    }

    #[test]
    fn only_the_announced_call_has_a_caller_and_compaction_forgets() {
        let mut d = Dedup::default();
        let a = json!({"id": "X-1"});
        assert_eq!(d.caller("lookup", &a, None).as_deref(), Some(PROCESS), "no hooks: the process");
        d.answer(PROCESS, "lookup", &a, "x\n".into(), true);
        let again = d.answer(PROCESS, "lookup", &a, "x\n".into(), true);
        assert!(again.starts_with("unchanged since call #1") && again.contains("fresh: true"), "{again}");
        pre(&mut d, "s", "", "lookup");
        assert_eq!(d.caller("search", &a, None), None, "announced another tool");
        pre(&mut d, "s", "a1", "lookup");
        assert_eq!(d.caller("lookup", &a, None).as_deref(), Some("s/a1"), "the newest announcement of the tool");
        assert_eq!(d.caller("lookup", &a, None).as_deref(), Some("s/"), "then the older one");
        assert_eq!(d.caller("lookup", &a, None), None, "used once; with the hooks on, unannounced is no caller");
        d.answer("s/a1", "lookup", &a, "x\n".into(), true);
        d.hook(&json!({"event": "PreCompact", "session_id": "s"}));
        // The forget is applied when the next call takes its caller.
        assert_eq!(d.caller("lookup", &a, None), None);
        assert_eq!(d.answer("s/a1", "lookup", &a, "x\n".into(), true), "x\n");
    }

    /// Review finding 1: with one pending slot, B's PreToolUse overwrote
    /// A's, so A's answer was filed as B's call and B's later identical call
    /// was told `unchanged since your call #N` of an answer B never saw.
    #[test]
    fn interleaved_agents_keep_their_own_calls() {
        let mut d = Dedup::default();
        let ann = d.announcer();
        let x = json!({"id": "X-1"});
        let y = json!({"id": "Y-2"});
        // A and B announce before either call runs.
        pre_with(&ann, "s", "A", "lookup", &x, "toolu_a1");
        pre_with(&ann, "s", "B", "lookup", &y, "toolu_b1");
        // A's call runs first: matched by its input, not by arrival.
        let ca = d.caller("lookup", &x, None).unwrap();
        assert_eq!(ca, "s/A");
        d.answer(&ca, "lookup", &x, "answer x\n".into(), true);
        let cb = d.caller("lookup", &y, None).unwrap();
        assert_eq!(cb, "s/B");
        d.answer(&cb, "lookup", &y, "answer y\n".into(), true);
        // B now asks for X: B never saw it, so it comes whole.
        pre_with(&ann, "s", "B", "lookup", &x, "toolu_b2");
        let cb = d.caller("lookup", &x, None).unwrap();
        assert_eq!(cb, "s/B");
        assert_eq!(d.answer(&cb, "lookup", &x, "answer x\n".into(), true), "answer x\n");
        // A asks for X again: A did see it.
        pre_with(&ann, "s", "A", "lookup", &x, "toolu_a2");
        let ca = d.caller("lookup", &x, None).unwrap();
        assert_eq!(ca, "s/A");
        assert!(d.answer(&ca, "lookup", &x, "answer x\n".into(), true).starts_with("unchanged since your call #1"));
        // Same tool and input from two agents at once: the client's
        // tool_use_id, when it sends one, picks the right agent.
        pre_with(&ann, "s", "A", "lookup", &y, "toolu_a3");
        pre_with(&ann, "s", "B", "lookup", &y, "toolu_b3");
        assert_eq!(d.caller("lookup", &y, Some("toolu_a3")).as_deref(), Some("s/A"));
        assert_eq!(d.caller("lookup", &y, Some("toolu_b3")).as_deref(), Some("s/B"));
        // Other tools' announcements never wait for a call.
        ann.hook(&json!({"event": "PreToolUse", "session_id": "s", "agent_id": "A", "tool_name": "Bash", "tool_input": "{\"command\":\"ls\"}"}));
        assert!(d.heard.0.lock().unwrap().pending.is_empty());
    }

    /// The reader reads ahead of the calls: a hook it noted after a call
    /// in the stream must not be taken by that call, nor a PreCompact after
    /// it forget before it, nor the first PreToolUse after it make it an
    /// unannounced call.
    #[test]
    fn a_call_sees_only_the_hooks_before_it_in_the_stream() {
        let mut d = Dedup::default();
        let ann = d.announcer();
        let x = json!({"id": "X-1"});
        // An unhooked call, read before any hook, then the hooks read ahead.
        let m0 = ann.mark();
        pre_with(&ann, "s", "A", "lookup", &x, "tu1");
        let m1 = ann.mark();
        pre_with(&ann, "s", "B", "lookup", &x, "tu2");
        let m2 = ann.mark();
        d.set_horizon(Some(m0));
        assert_eq!(d.caller("lookup", &x, None).as_deref(), Some(PROCESS), "no hook came before it");
        // Call 1 came after A's hook only: A's, though B's is newer.
        d.set_horizon(Some(m1));
        let c1 = d.caller("lookup", &x, None).unwrap();
        assert_eq!(c1, "s/A");
        d.answer(&c1, "lookup", &x, "x\n".into(), true);
        // A PreCompact and A's repeat are read before call 2 runs.
        ann.hook(&json!({"event": "PreCompact", "session_id": "s"}));
        let m3 = ann.mark();
        pre_with(&ann, "s", "A", "lookup", &x, "tu3");
        let m4 = ann.mark();
        d.set_horizon(Some(m2));
        assert_eq!(d.caller("lookup", &x, None).as_deref(), Some("s/B"));
        // Nothing waits between the PreCompact and the next call.
        d.set_horizon(Some(m3));
        assert_eq!(d.caller("lookup", &x, None), None);
        // After the PreCompact, A's repeat is A's and answers whole.
        d.set_horizon(Some(m4));
        let c3 = d.caller("lookup", &x, None).unwrap();
        assert_eq!(c3, "s/A");
        assert_eq!(d.answer(&c3, "lookup", &x, "x\n".into(), true), "x\n");
    }

    /// Review finding 7: every kept answer stayed for the server's life.
    #[test]
    fn kept_answers_are_bounded_and_least_recent_go_first() {
        let mut d = Dedup::default();
        let chunk = "y".repeat(1 << 20);
        for i in 0..20 {
            d.answer(PROCESS, "lookup", &json!({"id": i}), chunk.clone(), true);
        }
        assert!(d.kept_bytes() <= SEEN_BYTES, "{}", d.kept_bytes());
        assert_eq!(d.kept_bytes(), d.seen.values().map(|v| v.1.len()).sum::<usize>());
        // The newest is still compared; the oldest answers whole.
        assert!(d.answer(PROCESS, "lookup", &json!({"id": 19}), chunk.clone(), true).starts_with("unchanged"));
        assert_eq!(d.answer(PROCESS, "lookup", &json!({"id": 0}), chunk.clone(), true), chunk);
        // An answer over the budget is not kept at all.
        let huge = "z".repeat(SEEN_BYTES + 1);
        d.answer(PROCESS, "lookup", &json!({"id": "huge"}), huge.clone(), true);
        assert_eq!(d.answer(PROCESS, "lookup", &json!({"id": "huge"}), huge.clone(), true), huge);
        assert!(d.kept_bytes() <= SEEN_BYTES);
        // The pending announcements are bounded too.
        for _ in 0..(PENDING + 10) {
            pre(&mut d, "s", "a", "lookup");
        }
        assert_eq!(d.heard.0.lock().unwrap().pending.len(), PENDING);
    }

    #[test]
    fn the_work_queue_and_lanes_answer_whole() {
        for t in ["decision_queue", "awaiting", "journal_status", "next_id", "hook"] {
            assert!(!dedupable(t, true, &json!({})), "{t}");
        }
        assert!(!dedupable("recall", true, &json!({"lane": "L"})));
        assert!(dedupable("recall", true, &json!({"key": "k"})));
        assert!(!dedupable("append_to_note", false, &json!({})));
    }
}
