//! A chain key's gate in `recall`: off unless the transcript source or the
//! compaction work is on. Its own test binary, because the resolver finds
//! the transcript through `CLAUDE_CONFIG_DIR`, which is process-wide.

mod common;

use atlas::json;
use atlas::memory::settings::Settings;
use common::*;

/// Synthesized records only.
fn transcript() -> String {
    let user = |t: &str, ts: &str| format!(r#"{{"parentUuid":null,"isSidechain":false,"type":"user","message":{{"role":"user","content":"{t}"}},"timestamp":"{ts}"}}"#);
    [user("start: src/gate_start.rs", "2026-11-05T10:00:00.000Z"), user("then src/gate_more.rs", "2026-11-05T10:01:00.000Z")].join("\n") + "\n"
}

#[test]
fn a_chain_key_is_off_unless_transcripts_or_compaction_is_on() {
    let tmp = Temp::new("chain-keys");
    std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());
    let here = atlas::transcripts::project_of_cwd(&std::env::current_dir().unwrap());
    let p = tmp.path().join("projects").join(&here).join("abcdef12-0000.jsonl");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, transcript()).unwrap();
    let card = atlas::chain::card(&p).unwrap();
    let key = atlas::chain::written_keys(&card).into_iter().next().unwrap_or_else(|| panic!("no key in {card}"));

    let off = Settings::from_config(None, &tmp.path().join("state").join("index.sqlite")).unwrap();
    assert!(!off.transcripts && !off.compact.enabled, "both are off by default");
    let fx = Fx::new("chain-keys-server");
    let mut s = fx.server();
    s.set_memory(off.clone());
    let e = s.answer("recall", &json!({"key": key})).unwrap_err().to_string();
    assert!(e.contains("which is off (memory.transcripts and compact.enabled)"), "{e}");

    // A plain turn ref is no chain key: it keeps its own gate.
    let e = s.answer("recall", &json!({"key": "turn:abcdef12:1@0123456789ab"})).unwrap_err().to_string();
    assert!(e.contains("transcript source is off"), "{e}");

    let mut transcripts = off.clone();
    transcripts.transcripts = true;
    s.set_memory(transcripts);
    let t = s.answer("recall", &json!({"key": key})).unwrap().into_text();
    assert!(t.contains("gate_start.rs"), "{t}");

    let mut compact = off;
    compact.compact.enabled = true;
    s.set_memory(compact);
    let t = s.answer("recall", &json!({"key": key, "fresh": true})).unwrap().into_text();
    assert!(t.contains("gate_start.rs"), "{t}");
}
