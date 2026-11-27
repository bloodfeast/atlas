//! ATLAS-GOAL build step 2 through the journal tool: metric lines checked
//! at write, and `entry=goal` left to the goal writer (journal:ATLAS-GOAL:0037).

mod common;

use atlas::json;
use atlas::memory::settings::Settings;
use atlas::memory::MemoryStore;
use common::*;

fn server(tag: &str) -> (atlas::tools::Server, Fx, Temp, Settings) {
    let tmp = Temp::new(tag);
    let mut settings = Settings::from_config(None, &tmp.path().join("state").join("index.sqlite")).unwrap();
    settings.snapshot_secs = 0;
    let fx = Fx::new(&format!("{tag}-fx"));
    let mut s = fx.server();
    s.set_memory(settings.clone());
    (s, fx, tmp, settings)
}

fn keys(settings: &Settings, lane: &str) -> Vec<String> {
    let store = MemoryStore::open(&settings.store).unwrap();
    store.lane(lane).unwrap().into_iter().map(|h| h.key).collect()
}

#[test]
fn journal_refuses_entry_goal() {
    let (mut s, _fx, _tmp, settings) = server("goal-entry");
    let e = s.answer("journal", &json!({"lane": "GOAL", "entry": "goal", "text": "goal:g v1 spec\ngate G1 a.b < 1 us", "meta": "goal: goal:g\nversion: 1\nsha: abc\n"})).unwrap_err().to_string();
    assert!(e.contains("entry goal") && e.contains("note op=goal"), "{e}");
    assert!(keys(&settings, "GOAL").is_empty(), "nothing written");
    // The tool's entry list does not offer it.
    assert!(!atlas::memory::ENTRIES.contains(&"goal"));
}

#[test]
fn a_malformed_metric_line_is_refused_at_write() {
    let (mut s, _fx, _tmp, settings) = server("metric-write");
    let bad = "frame set @763e4d3a\nmetric: frame.gpu.p99@1080p 4147 us arm b1080 @763e4d3a";
    let e = s.answer("journal", &json!({"lane": "FRAME-SET", "entry": "measurement", "text": bad})).unwrap_err().to_string();
    assert!(e.contains("line 2") && e.contains("quiet or contended"), "{e}");
    assert!(keys(&settings, "FRAME-SET").is_empty(), "nothing written");
    let good = "frame set @763e4d3a\nmetric: frame.gpu.p99@1080p 4147 us arm b1080 @763e4d3a quiet";
    s.answer("journal", &json!({"lane": "FRAME-SET", "entry": "measurement", "text": good})).unwrap();
    assert_eq!(keys(&settings, "FRAME-SET"), ["journal:FRAME-SET:0001"]);
    // An amend that breaks a metric line is refused too.
    let sha = MemoryStore::open(&settings.store).unwrap().head("journal:FRAME-SET:0001").unwrap().unwrap().sha;
    let e = s
        .answer("journal", &json!({"lane": "FRAME-SET", "entry": "measurement", "amend": "journal:FRAME-SET:0001", "expected_sha": sha, "text": bad}))
        .unwrap_err()
        .to_string();
    assert!(e.contains("quiet or contended"), "{e}");
    // Another entry kind is not refused, only warned: it feeds nothing.
    let out = s.answer("journal", &json!({"lane": "FRAME-SET", "entry": "note", "text": good})).unwrap().into_text();
    assert!(out.contains("feed goals only from a measurement entry"), "{out}");
}
