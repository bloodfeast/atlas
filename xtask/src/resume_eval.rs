//! `cargo xtask resume-eval`: the cold-start eval of the resume card
//! (`atlas::resume`). A cleared session knows nothing of the work; the card
//! should answer the state questions such a session starts with (which lanes
//! are live, what waits for the user, what the queue starts next, where the
//! newest handoffs are, what the goal was) in a few hundred tokens.
//!
//! The run copies the store, builds the card for `--transcript` at `--now`
//! (as SessionStart `--source`, default `clear`, builds it), and scores each
//! question three ways, by its facts (matched as `memory-eval` matches them,
//! `a | b` accepting either):
//!
//! * card: the card alone;
//! * none: what the same SessionStart injects with `memory.resume_card` off;
//! * calls: the answers of the calls a session would make instead
//!   (`journal_status`, `atlas queue next`, and `recall {lane}` of each live
//!   lane the card names), with their token cost beside the card's.
//!
//! The question file has one block per question: `Qnnn TOPIC`, `ask:`, an
//! optional `line:` (the card's facts are matched on its lines that start
//! with this text only), and `fact:` lines (must appear) and `not:` lines
//! (must not appear). The run fails when a question misses a fact in the
//! card, or [`card_gate`] fails it: over `--max-tokens` (default
//! [`MAX_CARD_TOKENS`]) by [`crate::bpe::tokens`], which is a public vocabulary
//! times a fitted factor, not real tokens.
//!
//! The ceiling also runs without a store or a transcript, under `cargo test -p
//! xtask` and the workspace's `cargo test --workspace` in CI: this file's tests
//! build a card full of keys through `atlas::resume`, and fail one at the old
//! budget of 1,350 bytes. `cargo xtask resume-eval` adds the questions, by hand, on a store.
//! Its none and calls token figures use the card's factor too, so read them as
//! rough.

use std::path::{Path, PathBuf};
use std::time::Instant;

use atlas::json::{self, Value};

/// One question.
#[derive(Debug, Clone)]
pub struct Question {
    /// `Q001`.
    pub id: String,
    /// The words of the question.
    pub ask: String,
    /// The card lines its facts are matched on (by prefix); empty: all.
    pub line: String,
    /// The required facts, each a list of alternatives.
    pub facts: Vec<Vec<String>>,
    /// Facts that must not appear, each a list of alternatives.
    pub nots: Vec<Vec<String>>,
}

/// The card's token ceiling, Brandon's ruling of 2026-11-06: what the test's card
/// full of keys measures at his `CARD_BYTES` of 1,160, so no margin by design. It
/// moves the earlier bar of 600, which 1,110 bytes met by losing facts the eval
/// asks for. Raising this number is his call, not a fix for a failing test.
pub const MAX_CARD_TOKENS: usize = 612;

/// The card's estimated tokens, and whether they are within `max`.
pub fn card_gate(card: &str, max: usize) -> (usize, bool) {
    let n = crate::bpe::tokens(card);
    (n, n <= max)
}

/// Options of one run.
#[derive(Debug, Default)]
pub struct Opts {
    /// The memory store, copied before the run.
    pub store: PathBuf,
    /// The new session's transcript path (the previous one is its newest
    /// sibling).
    pub transcript: String,
    /// The record time the card is built at.
    pub now: String,
    /// The question file.
    pub questions: PathBuf,
    /// The SessionStart source (`clear` or `startup`; empty: `clear`).
    pub source: String,
    /// The card's token ceiling.
    pub max_tokens: Option<usize>,
    /// Print the card and the per-question rows.
    pub rows: bool,
}

/// Parses the question file.
///
/// # Errors
///
/// A block without facts or a line outside a block.
pub fn parse(text: &str) -> Result<Vec<Question>, String> {
    let mut out: Vec<Question> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('Q') {
            let id = format!("Q{}", rest.split_whitespace().next().unwrap_or(""));
            out.push(Question { id, ask: String::new(), line: String::new(), facts: Vec::new(), nots: Vec::new() });
            continue;
        }
        let q = out.last_mut().ok_or_else(|| format!("line {}: outside a question", n + 1))?;
        let alts = |f: &str| f.split(" | ").map(|x| crate::memory_eval::norm(x.trim())).collect();
        if let Some(a) = line.strip_prefix("ask:") {
            q.ask = a.trim().to_string();
        } else if let Some(l) = line.strip_prefix("line:") {
            q.line = l.trim().to_string();
        } else if let Some(f) = line.strip_prefix("fact:") {
            q.facts.push(alts(f));
        } else if let Some(f) = line.strip_prefix("not:") {
            q.nots.push(alts(f));
        } else {
            return Err(format!("line {}: expected ask:, line:, fact: or not:", n + 1));
        }
    }
    if let Some(q) = out.iter().find(|q| q.facts.is_empty()) {
        return Err(format!("{}: no facts", q.id));
    }
    Ok(out)
}

/// How many of a question's checks a text passes: each fact present, each
/// `not` absent. With `scoped`, only the lines the question's `line:` names
/// are read.
fn held(q: &Question, text: &str, scoped: bool) -> usize {
    let text = if scoped && !q.line.is_empty() { text.lines().filter(|l| l.starts_with(q.line.as_str())).collect::<Vec<_>>().join("\n") } else { text.to_string() };
    let t = crate::memory_eval::norm(&text);
    let has = |alts: &Vec<String>| alts.iter().any(|a| t.contains(a.as_str()));
    q.facts.iter().filter(|a| has(a)).count() + q.nots.iter().filter(|a| !has(a)).count()
}

fn text_of(v: &Value) -> String {
    v.get("text").and_then(Value::as_str).map_or_else(|| json::to_string(v), str::to_string)
}

/// Runs the eval and prints the report; `Ok(false)` when the gate fails.
///
/// # Errors
///
/// A message, when the store cannot be copied or opened or the question
/// file is invalid.
pub fn run(o: &Opts) -> Result<bool, String> {
    let questions = parse(&std::fs::read_to_string(&o.questions).map_err(|e| format!("{}: {e}", o.questions.display()))?)?;
    let work = std::env::temp_dir().join(format!("atlas-resume-eval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let store_path = work.join("memory.sqlite");
    std::fs::copy(&o.store, &store_path).map_err(|e| format!("copy {}: {e}", o.store.display()))?;
    let r = run_in(o, &questions, &work, &store_path);
    let _ = std::fs::remove_dir_all(&work);
    r
}

fn run_in(o: &Opts, questions: &[Question], work: &Path, store_path: &Path) -> Result<bool, String> {
    let db = work.join("atlas.sqlite");
    let mut settings = atlas::memory::settings::Settings::from_config(None, &db).map_err(|e| e.to_string())?;
    settings.store = store_path.to_path_buf();
    settings.snapshot_secs = 0;
    let tokens = crate::bpe::tokens;
    let source = if o.source.is_empty() { "clear" } else { o.source.as_str() };

    let store = atlas::memory::MemoryStore::open(store_path).map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    let card = atlas::resume::card(&store, &settings.orchestrate, &o.transcript, &o.now, source == "clear").map_err(|e| e.to_string())?.unwrap_or_default();
    let build = t0.elapsed();
    // The goal search alone (the previous transcript's tail), timed apart.
    let t1 = Instant::now();
    let prev = atlas::resume::previous_transcript(std::path::Path::new(&o.transcript));
    let goal = prev.as_deref().and_then(atlas::resume::goal);
    let goal_time = t1.elapsed();

    // The calls a cleared session makes instead.
    let (heads, items) = atlas::queue::items(&store, &o.now).map_err(|e| e.to_string())?;
    let p = atlas::queue::propose(&settings.orchestrate, &items, &[]);
    let queue_next = atlas::queue::proposal_line(&items, &heads, &p, usize::MAX);
    let live: Vec<String> = card
        .lines()
        .find_map(|l| l.strip_prefix("- live lanes"))
        .and_then(|l| l.split_once(": ").map(|(_, v)| v.to_string()))
        .map(|v| v.split("; ").map(|x| x.split(" (").next().unwrap_or(x).to_string()).collect())
        .unwrap_or_default();
    drop(store);
    // The server needs one vault; an empty one adds nothing to the answers.
    let vault = work.join("vault");
    std::fs::create_dir_all(&vault).map_err(|e| e.to_string())?;
    let index = atlas::paths::Vaults::new(&[("eval".to_string(), vault)])
        .and_then(|v| atlas::store::Store::open_with(&db, v, atlas::store::AtlasSetup::default()))
        .map_err(|e| e.to_string())?;
    let mut server = atlas::tools::Server::new(index);
    server.set_memory(settings.clone());
    let mut calls: Vec<(String, String)> = Vec::new();
    let call = |server: &mut atlas::tools::Server, name: &str, args: Value| server.call(name, &args).map(|v| text_of(&v)).map_err(|e| format!("{name}: {e}"));
    calls.push(("journal_status".into(), call(&mut server, "journal_status", json::parse("{}").map_err(|e| e.to_string())?)?));
    calls.push(("queue next".into(), queue_next));
    for lane in &live {
        let args = json::parse(&format!("{{\"lane\": {}}}", json::to_string(&Value::String(lane.clone())))).map_err(|e| e.to_string())?;
        calls.push((format!("recall lane={lane}"), call(&mut server, "recall", args)?));
    }
    let calls_text: String = calls.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n");
    drop(server);

    // What the same SessionStart gives with the card off.
    let mut off = settings;
    off.resume_card = false;
    let mut hooks = atlas::hooks::Hooks::open(off).map_err(|e| e.to_string())?;
    hooks.set_offer_paths(None);
    let ev = atlas::hooks::HookEvent { event: "SessionStart".into(), session_id: "resume-eval".into(), source: source.into(), transcript_path: o.transcript.clone(), ..Default::default() };
    let none = hooks.handle(&ev).context;
    drop(hooks);

    if o.rows {
        println!("--- card ({} bytes, {} tokens)\n{card}---", card.len(), tokens(&card));
    }
    let (mut card_pass, mut none_pass, mut calls_pass) = (0, 0, 0);
    let (mut facts, mut card_facts, mut none_facts, mut calls_facts) = (0, 0, 0, 0);
    let mut misses = Vec::new();
    for q in questions {
        let (c, z, k, n) = (held(q, &card, true), held(q, &none, false), held(q, &calls_text, false), q.facts.len() + q.nots.len());
        facts += n;
        card_facts += c;
        none_facts += z;
        calls_facts += k;
        card_pass += usize::from(c == n);
        none_pass += usize::from(z == n);
        calls_pass += usize::from(k == n);
        if c < n {
            misses.push(q.id.clone());
        }
        if o.rows {
            println!("row {} card {c}/{n} none {z}/{n} calls {k}/{n} {}", q.id, q.ask);
        }
    }
    let nq = questions.len();
    println!("questions {nq}, checks {facts} (facts and must-not facts), source {source}");
    println!("card:  {card_pass}/{nq} questions, {card_facts}/{facts} checks, {} tokens ({} bytes; {}), built in {:.1} ms (goal search {:.2} ms, goal {})", tokens(&card), card.len(), crate::bpe::WHAT, build.as_secs_f64() * 1e3, goal_time.as_secs_f64() * 1e3, if goal.is_some() { "found" } else { "none" });
    println!("none:  {none_pass}/{nq} questions, {none_facts}/{facts} checks, {} tokens", tokens(&none));
    let calls_tokens: usize = calls.iter().map(|(_, t)| tokens(t)).sum();
    println!("calls: {calls_pass}/{nq} questions, {calls_facts}/{facts} facts, {calls_tokens} tokens in {} calls", calls.len());
    for (name, t) in &calls {
        println!("  {name}: {} tokens", tokens(t));
    }
    if !misses.is_empty() {
        println!("missed from the card: {}", misses.join(", "));
    }
    let max = o.max_tokens.unwrap_or(MAX_CARD_TOKENS);
    let ok = misses.is_empty() && card_gate(&card, max).1;
    println!("gate: {} (every question from the card, card at most {max} tokens)", if ok { "pass" } else { "FAIL" });
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas::memory::{Draft, MemoryStore};
    use atlas::orchestrate::OrchSettings;
    use atlas::resume::{card_within, CARD_BYTES};

    const NOW: &str = "2026-11-04T18:00:00Z";
    /// The budget before the ceiling was counted.
    const OLD_BUDGET: usize = 1_350;

    fn add(store: &mut MemoryStore, key: &str, kind: &str, sub: &str, scope: &str) {
        let d = Draft { title: format!("{key} title"), reason: "t".into(), author: "t".into(), client: "t".into(), time: Some(NOW.into()), ..Default::default() };
        let mut w = store.writer().unwrap();
        w.create(key, kind, sub, scope, &d, None).unwrap();
        w.commit().unwrap();
    }

    /// The card of a store full of keys: more lanes, questions, queue items
    /// and handoffs than any line shows. Invented names, sized like real ones.
    fn dense_card(name: &str, budget: usize) -> String {
        let d = std::env::temp_dir().join(format!("atlas-resume-gate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut s = MemoryStore::open(&d.join("memory.sqlite")).unwrap();
        for i in 0..12 {
            let lane = format!("WORK-STREAM-{i:02}-REVIEW");
            add(&mut s, &format!("journal:{lane}:{:04}", 100 + i), "journal", "question", &lane);
            add(&mut s, &format!("journal:{lane}:{:04}", 200 + i), "journal", "question", &lane);
            add(&mut s, &format!("handoff:{lane}:{:03}", 10 + i), "handoff", "", &lane);
            add(&mut s, &format!("handoff:other-{i:02}:{:03}", 20 + i), "handoff", "", &format!("other-{i:02}"));
            add(&mut s, &format!("journal:{lane}:{:04}", 300 + i), "journal", "queued", &lane);
        }
        let card = card_within(&s, &OrchSettings::default(), "", NOW, false, budget).unwrap().unwrap();
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
        card
    }

    #[test]
    fn the_gate_fails_a_card_at_the_old_budget() {
        let c = &dense_card("old", OLD_BUDGET);
        assert!(c.len() > OLD_BUDGET - 80, "not a full card: {} bytes", c.len());
        let (n, ok) = card_gate(c, MAX_CARD_TOKENS);
        assert!(!ok, "{n} tokens at {} bytes pass the {MAX_CARD_TOKENS} ceiling", c.len());
    }

    #[test]
    fn a_full_card_is_within_the_ceiling() {
        let c = &dense_card("full", CARD_BYTES);
        assert!(c.len() > CARD_BYTES - 80, "not a full card: {} bytes", c.len());
        let (n, ok) = card_gate(c, MAX_CARD_TOKENS);
        assert!(ok, "{n} tokens at {} bytes: lower CARD_BYTES", c.len());
    }
}
