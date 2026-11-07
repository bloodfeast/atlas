//! Idea capture through the hook: the write path, the surfaces and the off
//! state, with invented text. Two ignored runs measure, on local files named by
//! environment variables: `shipped_detector_on_tune` and `hook_latency`. They
//! print counts only.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use atlas::hooks::{HookAnswer, HookEvent, Hooks};
use atlas::ideas::{self, Capture, IdeaSettings, Model, State};
use atlas::json::Value;
use atlas::memory::settings::Settings;
use atlas::memory::{Draft, MemoryStore};

const IDEA: &str = "we should keep the cache small because a large one would cost more than it saves";
const REPLY: &str = "ok";

std::thread_local! {
    /// This test's transcript: each test runs on its own thread, in its own dir.
    static TRANSCRIPT: std::cell::RefCell<PathBuf> = const { std::cell::RefCell::new(PathBuf::new()) };
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("atlas-ideas-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TRANSCRIPT.with(|t| *t.borrow_mut() = d.join("transcript.jsonl"));
    d
}

/// Appends one line to this test's transcript and returns its path.
fn record(line: &str) -> String {
    use std::io::Write as _;
    let path = TRANSCRIPT.with(|t| t.borrow().clone());
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
    writeln!(f, "{line}").unwrap();
    path.to_string_lossy().into_owned()
}

/// This test's transcript's path.
fn transcript() -> String {
    TRANSCRIPT.with(|t| t.borrow().to_string_lossy().into_owned())
}

/// What Claude Code writes for a prompt: a person's, or a task notification's.
/// The hook runs before it is written.
fn said(text: &str) -> String {
    let kind = if text.starts_with("<task-notification>") { "task-notification" } else { "human" };
    record(&atlas::json!({"type": "user", "message": {"content": text}, "origin": {"kind": kind}, "entrypoint": "cli"}).to_string())
}

/// A Stop with nothing running, as hooks.json passes it.
fn stop_event(session: &str, active: bool, transcript: &str) -> Value {
    atlas::json!({"hook_event_name": "Stop", "session_id": session, "stop_hook_active": active, "transcript_path": transcript, "background_tasks": "[]", "session_crons": "[]"})
}

fn settings(d: &Path, on: bool) -> Settings {
    let mut s = Settings::from_config(None, &d.join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.recall.fts_floor = 0.0;
    s.recall.hybrid_fts_floor = 0.0;
    s.ideas = IdeaSettings { enabled: on, asks_per_day: 5, ..IdeaSettings::default() };
    s
}

fn event(v: Value) -> HookEvent {
    HookEvent::from_value(&v)
}

fn prompt(h: &mut Hooks, session: &str, text: &str) -> HookAnswer {
    let a = h.handle(&event(atlas::json!({"hook_event_name": "UserPromptSubmit", "session_id": session, "prompt": text, "transcript_path": transcript()})));
    said(text);
    h.after();
    a
}

fn count(s: &MemoryStore) -> i64 {
    s.conn().query_row("SELECT COUNT(*) FROM records WHERE sub = 'idea'", [], |r| r.get(0)).unwrap()
}

#[test]
fn off_writes_nothing_and_says_nothing_on_any_event() {
    let d = dir("off");
    let s = settings(&d, false);
    let spool = ideas::spool_dir(&s.store);
    let mut h = Hooks::open(s).unwrap();
    let mut answers = vec![prompt(&mut h, "s", IDEA)];
    answers.push(h.handle(&event(atlas::json!({"hook_event_name": "Stop", "session_id": "s", "stop_hook_active": false}))));
    answers.push(prompt(&mut h, "s", "<task-notification>\n<summary>Goal check-in: background work still running</summary>\n</task-notification>"));
    answers.push(h.handle(&event(atlas::json!({"hook_event_name": "PostToolUse", "session_id": "s", "tool_name": "AskUserQuestion", "tool_input": {}, "tool_response": {"answers": {"Which?": IDEA}}}))));
    for a in &answers {
        assert!(!a.context.contains("idea") && a.block.is_none(), "{a:?}");
    }
    assert_eq!(count(h.store()), 0);
    assert!(!spool.exists());
}

#[test]
fn a_typed_prompt_files_and_a_notification_never_does() {
    let d = dir("arm");
    let mut h = Hooks::open(settings(&d, true)).unwrap();
    prompt(&mut h, "s", &format!("<task-notification>\n<task-id>abc</task-id>\n{IDEA}\n</task-notification>"));
    assert_eq!(count(h.store()), 0);
    prompt(&mut h, "s", "/compact");
    prompt(&mut h, "s", REPLY);
    assert_eq!(count(h.store()), 0);
    prompt(&mut h, "s", IDEA);
    assert_eq!(count(h.store()), 1);
    let (client, status): (String, String) = h.store().conn().query_row("SELECT v.client, r.status FROM records r JOIN versions v ON v.vid = r.head WHERE r.sub = 'idea'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((client.as_str(), status.as_str()), ("atlas-hook", "active"));
}

#[test]
fn a_prompt_no_person_typed_is_not_filed_or_asked_about() {
    let d = dir("nobody");
    let mut s = settings(&d, true);
    s.ideas.band_below = 100.0;
    s.ideas.band_above = 100.0;
    let spool = ideas::spool_dir(&s.store);
    let mut h = Hooks::open(s).unwrap();
    // A loop's iteration, another session's message, a headless run's prompt.
    let records = [
        atlas::json!({"type": "user", "isMeta": true, "message": {"content": "we should tick the invented loop"}, "entrypoint": "cli"}),
        atlas::json!({"type": "user", "message": {"content": "we should reply to the invented session"}, "entrypoint": "cli"}),
        atlas::json!({"type": "user", "message": {"content": "we should keep the invented headless run"}, "origin": {"kind": "human"}, "entrypoint": "sdk-cli"}),
    ];
    for r in &records {
        let text = r.pointer("/message/content").and_then(Value::as_str).unwrap().to_string();
        let t = transcript();
        h.handle(&event(atlas::json!({"hook_event_name": "UserPromptSubmit", "session_id": "s", "prompt": text, "transcript_path": t.as_str()})));
        record(&r.to_string());
        h.after();
        assert!(h.handle(&event(stop_event("s", false, &t))).block.is_none());
    }
    assert_eq!(count(h.store()), 0);
    assert_eq!(std::fs::read_dir(&spool).map_or(0, Iterator::count), 0, "settled on their own records, not still held");
}

#[test]
fn stop_asks_only_when_nothing_is_waiting() {
    let d = dir("idle");
    let mut s = settings(&d, true);
    s.ideas.band_below = 100.0;
    s.ideas.band_above = 100.0;
    let mut h = Hooks::open(s).unwrap();
    prompt(&mut h, "s", "perhaps a plan worth a look");
    let t = said("perhaps a plan worth a look");
    let with = |tasks: &str, crons: &str| atlas::json!({"hook_event_name": "Stop", "session_id": "s", "stop_hook_active": false, "transcript_path": t.as_str(), "background_tasks": tasks, "session_crons": crons});
    let running = r#"[{"id": "task-1", "type": "subagent", "status": "running"}]"#;
    assert!(h.handle(&event(with(running, "[]"))).block.is_none(), "an agent running");
    assert!(h.handle(&event(with("[]", running))).block.is_none(), "a wakeup scheduled");
    // A substituted path the input lacked, or no field at all, says nothing: no ask.
    assert!(h.handle(&event(with("${background_tasks}", "[]"))).block.is_none());
    assert!(h.handle(&event(atlas::json!({"hook_event_name": "Stop", "session_id": "s", "stop_hook_active": false, "transcript_path": t.as_str()}))).block.is_none());
    // A task notification started this turn.
    prompt(&mut h, "s", "<task-notification>\n<task-id>abc</task-id>\n</task-notification>");
    assert!(h.handle(&event(stop_event("s", false, &t))).block.is_none());
    said("perhaps a plan worth a look");
    assert!(h.handle(&event(stop_event("s", false, &t))).block.is_some());
}

#[test]
fn a_spent_budget_still_files() {
    let d = dir("budget");
    let mut s = settings(&d, true);
    s.recall.session_budget_tokens = 1;
    let mut h = Hooks::open(s).unwrap();
    prompt(&mut h, "s", "spend what there is");
    prompt(&mut h, "s", IDEA);
    assert_eq!(count(h.store()), 1);
}

#[test]
fn a_recall_error_still_files() {
    let d = dir("recall");
    let mut h = Hooks::open(settings(&d, true)).unwrap();
    // Recall's first read needs this table; filing does not.
    h.store().conn().execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE refs;").unwrap();
    let a = prompt(&mut h, "s", IDEA);
    assert!(a.context.is_empty());
    assert_eq!(count(h.store()), 1);
}

#[test]
fn a_lost_lock_answers_at_once_and_the_next_prompt_files() {
    let d = dir("lock");
    let s = settings(&d, true);
    let (path, spool) = (s.store.clone(), ideas::spool_dir(&s.store));
    let mut h = Hooks::open(s).unwrap();
    prompt(&mut h, "s", REPLY);
    let mut other = MemoryStore::open(&path).unwrap();
    let held = other.writer().unwrap();
    let t = Instant::now();
    prompt(&mut h, "s", IDEA);
    assert!(t.elapsed() < Duration::from_secs(5), "well under the store's 30 s wait: {:?}", t.elapsed());
    assert_eq!(std::fs::read_dir(&spool).unwrap().count(), 1);
    drop(held);
    prompt(&mut h, "s", REPLY);
    assert_eq!(count(h.store()), 1);
    assert_eq!(std::fs::read_dir(&spool).unwrap().count(), 0);
}

#[test]
fn a_capture_is_not_recalled_and_the_word_ideas_names_no_lane() {
    let d = dir("recalled");
    let mut h = Hooks::open(settings(&d, true)).unwrap();
    prompt(&mut h, "s", IDEA);
    let a = prompt(&mut h, "t", &format!("ideas about {IDEA}"));
    assert!(!a.context.contains("journal:ideas"), "{}", a.context);
    assert!(!a.context.contains(IDEA), "{}", a.context);
}

#[test]
fn it_survives_a_compaction() {
    let d = dir("compact");
    let mut s = settings(&d, true);
    s.compact.enabled = true;
    let mut h = Hooks::open(s).unwrap();
    prompt(&mut h, "s", IDEA);
    h.handle(&event(atlas::json!({"hook_event_name": "PreCompact", "session_id": "s", "trigger": "auto", "transcript_path": ""})));
    h.handle(&event(atlas::json!({"hook_event_name": "SessionStart", "session_id": "s", "source": "compact", "transcript_path": ""})));
    assert!(ideas::pending_line(h.store()).unwrap().is_some());
}

#[test]
fn surfaces_reach_a_check_in_a_notification_and_a_user_with_neither() {
    let d = dir("surface");
    let mut h = Hooks::open(settings(&d, true)).unwrap();
    // Nothing pending: nothing on any surface.
    let start = atlas::json!({"hook_event_name": "SessionStart", "session_id": "s", "source": "startup", "transcript_path": ""});
    assert!(!h.handle(&event(start.clone())).context.contains("ideas"));
    prompt(&mut h, "s", IDEA);
    // The user with no goal and no agents: the bare line, at session start.
    let a = h.handle(&event(start));
    assert!(a.context.contains("not requests: 1, newest"), "{}", a.context);
    assert!(!a.context.contains("resume the work"), "{}", a.context);
    let checkin = "<task-notification>\n<summary>Goal check-in: background work still running</summary>\n</task-notification>";
    let a = prompt(&mut h, "s", checkin);
    assert!(a.context.contains("not requests: 1 in the last 7 days"), "{}", a.context);
    // Nothing new since: nothing said.
    assert!(!prompt(&mut h, "s", checkin).context.contains("atlas ideas"));
    prompt(&mut h, "s", "another plan we should consider because it is cheaper than this one");
    let done = "<task-notification>\n<task-id>abc</task-id>\n<status>completed</status>\n</task-notification>";
    let a = prompt(&mut h, "s", done);
    assert!(a.context.contains("not requests: 2 in the last 7 days"), "{}", a.context);
}

#[test]
fn stop_blocks_once_and_the_answer_closes_the_capture() {
    let d = dir("stop");
    let mut s = settings(&d, true);
    s.ideas.band_below = 100.0;
    s.ideas.band_above = 100.0;
    let mut h = Hooks::open(s).unwrap();
    prompt(&mut h, "s", "perhaps a plan worth a look");
    let t = said("perhaps a plan worth a look");
    let stop = |active: bool| stop_event("s", active, &t);
    let a = h.handle(&event(stop(false)));
    let reason = a.block.clone().unwrap();
    let json = a.to_json("Stop");
    assert!(json.contains("\"decision\":\"block\""), "{json}");
    let at = reason.find("[idea ").unwrap();
    let q = format!("{} Is this a standalone statement?", &reason[at..at + 17]);
    assert!(h.handle(&event(stop(true))).block.is_none());
    let resp = atlas::json::parse(&format!(r#"{{"hook_event_name": "PostToolUse", "session_id": "s", "tool_name": "AskUserQuestion", "tool_input": {{}}, "tool_response": {{"answers": {{"{q}": "No"}}}}}}"#)).unwrap();
    h.handle(&event(resp));
    h.after();
    assert!(ideas::pending_line(h.store()).unwrap().is_none());
    assert_eq!(count(h.store()), 1);
    // A subagent's stop is not this hook's.
    let a = h.handle(&event(atlas::json!({"hook_event_name": "Stop", "session_id": "s", "agent_id": "x", "stop_hook_active": false})));
    assert!(a.block.is_none());
}

fn sha(text: &str) -> String {
    atlas::sha256::hex(text.as_bytes())
}

struct Row {
    stratum: String,
    repeats: f64,
    text: String,
    label: bool,
}

fn tune_rows() -> Vec<Row> {
    let need = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    let labels = std::fs::read_to_string(need("ATLAS_IDEAS_LABELS")).unwrap();
    let by_id: std::collections::HashMap<&str, bool> = labels
        .lines()
        .map(|l| {
            let mut p = l.split('\t');
            (p.next().unwrap(), p.next() == Some("1"))
        })
        .collect();
    let corpus = need("ATLAS_IDEAS_TUNE");
    assert!(!corpus.contains("heldout"), "the held-out set is spent");
    let text = std::fs::read_to_string(&corpus).unwrap();
    assert_eq!(sha(&text), "7020152bd7352c450892033da94190638962f839d8170a097675c2bb9712fe57", "the frozen tune set");
    assert_eq!(sha(&labels), "b7f77646b8e2839413231d82efc2b31830166bc4e7c1c56cf66e4421d5d6b8df", "the frozen labels");
    text.lines()
        .map(|l| {
            let v = atlas::json::parse(l).unwrap();
            let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap().to_string();
            let label = by_id[s("id").as_str()];
            Row { stratum: s("stratum"), repeats: v.get("repeats").and_then(Value::as_f64).unwrap(), text: s("text"), label }
        })
        .collect()
}

/// The rate of each stratum's stream, per day (journal IDEA-CAPTURE:0014).
fn per_day(st: &str) -> f64 {
    match st {
        "typed" => 46.6,
        "queued" => 5.1,
        "ask" => 3.8,
        _ => 51.7,
    }
}

fn in_stratum(r: &Row, st: &str) -> bool {
    r.stratum == st || (st == "prompt" && r.stratum != "ask")
}

/// The detector through the code a live prompt takes, against the frozen tune
/// numbers of the wide point. The held-out set is spent and is refused.
#[test]
#[ignore = "needs the local tune set: ATLAS_IDEAS_TUNE, ATLAS_IDEAS_LABELS"]
fn shipped_detector_on_tune() {
    assert_eq!(sha(ideas::BASELINE), "eff1f59dd3330642400d345f999010a5f29da4c4e79b1af5b82c4749fbb71940", "the frozen weights");
    assert_eq!(sha(include_str!("../src/ideas/detect.rs")), "065cc8381a1a706e22d4c8265652cf33941a612b7a5d9d63f75a3bd6d330dd87", "the frozen detector");
    let rows = tune_rows();
    let model = Model::load(None).unwrap();
    let deny = atlas::redact::denylist();
    let band_off = IdeaSettings { asks_per_day: 0, ..IdeaSettings::default() };
    let score = |r: &Row| {
        let mut out = f32::MIN;
        Capture::new(&r.text, "eval", "", |t: &str| {
            out = model.score(t);
            None::<(f32, State, bool)>
        }, deny);
        out
    };
    let scores: Vec<f32> = rows.iter().map(score).collect();
    println!("weights {} detector {}", &sha(ideas::BASELINE)[..16], &sha(include_str!("../src/ideas/detect.rs"))[..16]);
    println!("| stratum | rows | P | misses | captures/100 | false/100 | captures/day | false/day |\n|---|---|---|---|---|---|---|---|");
    // Frozen in report/tune-baseline-1.md, point wide.
    let frozen = [("typed", 915, 347, 5, 67.7, 30.3, 29.5, 13.2), ("queued", 86, 29, 2, 66.3, 34.9, 3.4, 1.8), ("prompt", 1001, 376, 7, 67.5, 30.7, 32.9, 14.9), ("ask", 87, 36, 0, 77.0, 35.6, 2.9, 1.3)];
    for (st, n0, p0, m0, c0, f0, cd0, fd0) in frozen {
        let sel: Vec<usize> = (0..rows.len()).filter(|&i| in_stratum(&rows[i], st)).collect();
        let hit = |i: usize| ideas::decide(scores[i], model.threshold, &band_off).is_some();
        let p = sel.iter().filter(|&&i| rows[i].label).count();
        let miss = sel.iter().filter(|&&i| rows[i].label && !hit(i)).count();
        let cap = sel.iter().filter(|&&i| hit(i)).count();
        let fals = sel.iter().filter(|&&i| !rows[i].label && hit(i)).count();
        let weigh = |f: &dyn Fn(usize) -> bool| sel.iter().filter(|&&i| f(i)).map(|&i| 1.0 + rows[i].repeats).sum::<f64>();
        let wt = weigh(&|_| true);
        let (c, f) = (cap as f64 * 100.0 / sel.len() as f64, fals as f64 * 100.0 / sel.len() as f64);
        let (cd, fd) = (weigh(&|i| hit(i)) / wt * per_day(st), weigh(&|i| !rows[i].label && hit(i)) / wt * per_day(st));
        println!("| {st} | {} | {p} | {miss} | {c:.1} | {f:.1} | {cd:.1} | {fd:.1} |", sel.len());
        assert_eq!((sel.len(), p, miss), (n0, p0, m0), "{st}");
        for (a, b) in [(c, c0), (f, f0), (cd, cd0), (fd, fd0)] {
            assert!((a - b).abs() < 0.06, "{st}: {a} against {b}");
        }
    }
    // A bare word count at about the same captures.
    let words = |t: &str| t.split_whitespace().filter(|w| *w != "[redacted]").count();
    let prompt: Vec<&Row> = rows.iter().filter(|r| r.stratum != "ask").collect();
    for k in [8, 9, 10, 11, 12] {
        let cap = prompt.iter().filter(|r| words(&r.text) >= k).count();
        let hit = prompt.iter().filter(|r| r.label && words(&r.text) >= k).count();
        let p = prompt.iter().filter(|r| r.label).count();
        println!("words >= {k}: recall {:.1}, captures/100 {:.1}, false/100 {:.1}", hit as f64 * 100.0 / p as f64, cap as f64 * 100.0 / prompt.len() as f64, (cap - hit) as f64 * 100.0 / prompt.len() as f64);
    }
    // The band: rows per day and the share of ideas on each side of the threshold.
    println!("| side | width | prompts/day | ideas in it |\n|---|---|---|---|");
    let total: f64 = prompt.iter().map(|r| 1.0 + r.repeats).sum();
    let sel: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].stratum != "ask").collect();
    for (side, widths) in [("below", [0.1f32, 0.25, 0.5, 0.75, 1.0]), ("above", [0.1, 0.25, 0.5, 0.75, 1.0])] {
        for w in widths {
            let t = model.threshold;
            let inside = |i: usize| if side == "below" { scores[i] >= t - w && scores[i] < t } else { scores[i] >= t && scores[i] < t + w };
            let day: f64 = sel.iter().filter(|&&i| inside(i)).map(|&i| 1.0 + rows[i].repeats).sum::<f64>() / total * per_day("prompt");
            let (n, pos) = (sel.iter().filter(|&&i| inside(i)).count(), sel.iter().filter(|&&i| inside(i) && rows[i].label).count());
            println!("| {side} | {w} | {day:.1} | {pos} of {n} |");
        }
    }
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p) as usize]
}

fn seeded(d: &Path, on: bool) -> Settings {
    let s = settings(d, on);
    let mut m = MemoryStore::open(&s.store).unwrap();
    let mut w = m.writer().unwrap();
    for i in 0..2000 {
        let d = Draft { title: format!("record {i} about the cache"), body: format!("an invented record number {i} about caches, budgets and plans"), author: "t".into(), client: "t".into(), reason: "seed".into(), ..Draft::default() };
        w.create(&format!("memory-{i}"), "memory", "project", "", &d, None).unwrap();
    }
    w.commit().unwrap();
    s
}

/// Hook time on the tune set's prompts, off against on. `ATLAS_IDEAS_TUNE` and
/// `ATLAS_IDEAS_LABELS` name the rows; only timings and counts are printed.
#[test]
#[ignore = "a measurement; run with --release --nocapture"]
fn hook_latency() {
    let rows = tune_rows();
    let prompts: Vec<&str> = rows.iter().filter(|r| r.stratum != "ask").map(|r| r.text.as_str()).collect();
    let model = Model::load(None).unwrap();
    // The shipped default asks nothing, so the band files nothing either.
    let on_settings = IdeaSettings { enabled: true, ..IdeaSettings::default() };
    let on_asking = IdeaSettings { asks_per_day: 5, ..on_settings.clone() };
    let report = |name: &str, v: &mut Vec<f64>| {
        if !v.is_empty() {
            let n = v.len();
            let (p50, p99, max) = (percentile(v, 0.5), percentile(v, 0.99), percentile(v, 1.0));
            println!("{name}: n={n} p50={p50:.3}ms p99={p99:.3}ms max={max:.3}ms");
        }
    };
    let run = |on: bool, writers: usize, gap_ms: u64, tag: &str| {
        let d = dir(tag);
        let s = seeded(&d, on);
        let mut s = s;
        let (no_tool_call, backlog) = (tag.contains("no-tool"), tag.contains("backlog"));
        s.ideas.asks_per_day = if tag.starts_with("on-cap-5") {
            5
        } else if tag.starts_with("on-asking") {
            // Every Stop reads the transcript and asks: the path's worst case.
            usize::MAX
        } else {
            0
        };
        let asks = s.ideas.asks_per_day > 0;
        let files = |t: &str| ideas::decide(model.score(t), model.threshold, if asks { &on_asking } else { &on_settings }).is_some();
        let (store, spool) = (s.store.clone(), ideas::spool_dir(&s.store));
        let mut h = Hooks::open(s).unwrap();
        // A session's transcript past what a read takes in; `big` is the 90th
        // percentile of 300 real ones, about 243 MB, which an asked Stop's goal
        // search reads whole when the session has no goal.
        let pad = "{\"type\":\"assistant\",\"pad\":\"".to_string() + &"x".repeat(1 << 20) + "\"}";
        for _ in 0..if tag.contains("big") { 232 } else { 5 } {
            record(&pad);
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut threads = Vec::new();
        for w in 0..writers {
            let (stop, store) = (stop.clone(), store.clone());
            threads.push(std::thread::spawn(move || {
                let mut m = MemoryStore::open(&store).unwrap();
                let mut i = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut wr = m.writer().unwrap();
                    let d = Draft { title: format!("entry {i}"), body: "an invented journal entry from a busy agent".into(), author: "t".into(), client: "t".into(), reason: "load".into(), ..Draft::default() };
                    wr.create(&format!("journal:LOAD{w}:{i}"), "journal", "note", &format!("LOAD{w}"), &d, None).unwrap();
                    wr.commit().unwrap();
                    i += 1;
                    std::thread::sleep(Duration::from_millis(gap_ms));
                }
                i
            }));
        }
        let (mut quiet, mut filing, mut after, mut stops, mut left) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0);
        let (mut stops_spooled, mut stops_asked, mut spooled_seen) = (Vec::new(), Vec::new(), 0);
        // Held items whose record never comes: a Stop rereads each of them as long
        // as the transcript grows, up to the read's tail.
        let unmatched = tag.contains("unmatched");
        for (i, p) in prompts.iter().enumerate().take(if unmatched { 200 } else { usize::MAX }) {
            // Paced under writers, so off and on see the same load.
            let began = Instant::now();
            let session = format!("s{}", i / 40);
            let path = transcript();
            let ev = event(atlas::json!({"hook_event_name": "UserPromptSubmit", "session_id": session.as_str(), "prompt": *p, "transcript_path": path.as_str()}));
            let t = Instant::now();
            h.handle(&ev);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            if unmatched {
                record(&pad);
                said(&format!("invented words of another head, number {i}"));
            } else {
                said(p);
            }
            // A turn with no tool call: no event before Stop, the spool still full.
            let t = Instant::now();
            if !no_tool_call {
                h.after();
            }
            let aft = t.elapsed().as_secs_f64() * 1e3;
            if on && files(p) {
                filing.push(ms);
                if !no_tool_call {
                    after.push(aft);
                    if std::fs::read_dir(&spool).map_or(0, Iterator::count) > 0 {
                        left += 1;
                    }
                }
            } else {
                quiet.push(ms);
            }
            // Sixteen prompts spooled before one Stop: the most it drains.
            if backlog && (i + 1) % 16 != 0 {
                continue;
            }
            let spooled = std::fs::read_dir(&spool).map_or(0, Iterator::count);
            let t = Instant::now();
            let asked = h.handle(&event(stop_event(&session, false, &path))).block.is_some();
            let ms = t.elapsed().as_secs_f64() * 1e3;
            stops.push(ms);
            if spooled > 0 {
                stops_spooled.push(ms);
                spooled_seen = spooled_seen.max(spooled);
            }
            if asked {
                stops_asked.push(ms);
            }
            if no_tool_call {
                h.after();
            }
            if writers > 0 {
                std::thread::sleep(Duration::from_millis(20).saturating_sub(began.elapsed()));
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let done: Vec<i32> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        report(&format!("{tag} not filing"), &mut quiet);
        report(&format!("{tag} capture, answer"), &mut filing);
        report(&format!("{tag} capture, drain after the answer"), &mut after);
        report(&format!("{tag} stop"), &mut stops);
        report(&format!("{tag} stop, with the spool not empty (up to {spooled_seen} items)"), &mut stops_spooled);
        report(&format!("{tag} stop, that asked"), &mut stops_asked);
        println!("{tag}: {left} left in the spool after their drain, writer commits {done:?}");
        // Whatever the lock did, nothing is lost: the rest drains and all are on file.
        if on && !unmatched {
            for _ in 0..200 {
                prompt(&mut h, "drain", "ok");
            }
            let distinct: std::collections::HashSet<String> = prompts.iter().filter(|p| files(p)).map(|p| p.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")).collect();
            let filed = count(h.store());
            println!("{tag}: {filed} filed of {} distinct", distinct.len());
            let line = ideas::pending_line(h.store()).unwrap().unwrap().0.len();
            println!("{tag}: surface line {line} bytes");
            assert_eq!(std::fs::read_dir(&spool).map_or(0, Iterator::count), 0);
            assert!(filed as usize >= distinct.len() * 99 / 100, "{filed} of {}", distinct.len());
        }
        let _ = std::fs::remove_dir_all(&d);
    };
    // `ATLAS_IDEAS_TAGS` names the runs to make, by substring; unset, all.
    let only = std::env::var("ATLAS_IDEAS_TAGS").ok();
    let run = |on: bool, writers: usize, gap_ms: u64, tag: &str| {
        if only.as_deref().is_none_or(|o| o.split(',').any(|w| tag.contains(w))) {
            run(on, writers, gap_ms, tag);
        }
    };
    // Twice each, so a warm cache or a noisy minute shows.
    for pass in 1..=2 {
        run(false, 0, 0, &format!("off{pass}"));
        run(true, 0, 0, &format!("on{pass}"));
    }
    // Two agents journaling, each every 50 ms, far more than agents do; then flat out.
    run(false, 2, 50, "off-writers-50ms");
    run(true, 2, 50, "on-writers-50ms");
    run(true, 2, 1, "on-writers-flat-out");
    run(true, 0, 0, "on-cap-5");
    run(true, 0, 0, "on-asking");
    // The drain on Stop's clock: no event between the prompt and Stop. Past its
    // fifth question, a cap-5 run's Stops show the gate, not the drain.
    run(true, 0, 0, "on-cap-5-no-tool");
    run(true, 0, 0, "on-asking-no-tool");
    run(true, 2, 50, "on-asking-no-tool-writers-50ms");
    run(true, 2, 1, "on-asking-no-tool-writers-flat-out");
    run(true, 0, 0, "on-asking-no-tool-backlog");
    run(true, 2, 50, "on-asking-no-tool-backlog-writers-50ms");
    run(true, 2, 1, "on-asking-no-tool-backlog-writers-flat-out");
    for pass in 1..=2 {
        run(true, 0, 0, &format!("on-cap-5-no-tool-big{pass}"));
        run(true, 0, 0, &format!("on-asking-no-tool-big{pass}"));
        run(true, 0, 0, &format!("on-asking-no-tool-unmatched{pass}"));
    }
}
