//! The hooks: relevance-gated recall with its budget, the subagent brief
//! hand-over, the context cap, the token log, and the hook worker over the
//! real binary's stdio.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use atlas::hooks::{HookEvent, Hooks};
use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
use atlas::memory::{Draft, MemoryStore};
use common::*;

fn settings(dir: &Path) -> Settings {
    let mut s = Settings::from_config(None, &dir.join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    // The floor is calibrated on a real corpus; bm25 scale grows with corpus size.
    s.recall.fts_floor = 0.0;
    s.recall.hybrid_fts_floor = 0.0;
    s
}

fn seed(s: &Settings) {
    let mut store = MemoryStore::open(&s.store).unwrap();
    let mut w = store.writer().unwrap();
    let mut add = |key: &str, kind: &str, scope: &str, sub: &str, title: &str, body: &str| {
        let d = Draft { title: title.into(), body: body.into(), reason: "seeding hook tests".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
        w.create(key, kind, sub, scope, &d, None).unwrap();
    };
    add("widget-cache", "memory", "", "feedback", "- [Widget cache](widget-cache.md) - bound", "\nThe widget cache evicts least recently used entries past sixty four widgets.\n");
    add("rules:r00001", "rule", "rules", "", "gadget exporter rule", "- the gadget exporter writes with temp files and renames them into place.\n");
    add("rules:r00002", "rule", "rules", "", "unrelated", "- the garden is watered on mondays.\n");
    add("journal:build-7:0001", "journal", "build-7", "brief", "Build the gadget exporter", "");
    add("journal:build-7:0002", "journal", "build-7", "next", "Wire the hooks", "");
    drop(w.commit());
}

fn ev(event: &str) -> HookEvent {
    HookEvent { event: event.into(), session_id: "s1".into(), ..HookEvent::default() }
}

#[test]
fn recall_is_gated_budgeted_and_not_repeated() {
    let t = Temp::new("hook-recall");
    let s = settings(t.path());
    seed(&s);
    let mut h = Hooks::open(s.clone()).unwrap();
    // Unrelated prompt: nothing.
    let a = h.handle(&HookEvent { prompt: "what is the weather like on the coast".into(), ..ev("UserPromptSubmit") });
    assert_eq!(a.to_json("UserPromptSubmit"), "");
    // A prompt about the widget cache gets that memory, not the garden.
    let a = h.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("widget-cache"), "{}", a.context);
    assert!(!a.context.contains("garden"), "{}", a.context);
    assert!(a.context.len() <= s.bytes_for(s.recall.budget_tokens) + 128);
    // Same session, same question: not injected twice.
    let again = h.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), ..ev("UserPromptSubmit") });
    assert!(!again.context.contains("widget-cache"), "{}", again.context);
    // A tiny budget injects nothing whole.
    let mut tight = s.clone();
    tight.recall.budget_tokens = 5;
    let mut h2 = Hooks::open(tight).unwrap();
    let a = h2.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), ..ev("UserPromptSubmit") });
    assert!(!a.context.contains("== widget-cache"), "{}", a.context);
}

#[test]
fn a_subagent_gets_recall_from_the_parents_brief_and_its_lane() {
    let t = Temp::new("hook-brief");
    let s = settings(t.path());
    seed(&s);
    let mut h = Hooks::open(s).unwrap();
    let brief = "You are build-7. Continue the gadget exporter: it writes with temp files and renames them into place.";
    let pre = h.handle(&HookEvent { tool_name: "Agent".into(), tool_prompt: brief.into(), ..ev("PreToolUse") });
    assert_eq!(pre, Default::default());
    let start = h.handle(&HookEvent { agent_id: "a1".into(), agent_type: "general-purpose".into(), ..ev("SubagentStart") });
    assert!(start.context.contains("lane build-7"), "{}", start.context);
    assert!(start.context.contains("# brief") && start.context.contains("# next"), "{}", start.context);
    assert!(start.context.contains("rules:r00001"), "{}", start.context);
    // No stashed brief: the subagent's first tool call reads its own transcript.
    let dir = t.path().join("proj");
    let parent = dir.join("sess.jsonl");
    std::fs::create_dir_all(dir.join("sess").join("subagents")).unwrap();
    std::fs::write(&parent, "").unwrap();
    std::fs::write(
        dir.join("sess").join("subagents").join("agent-a2.jsonl"),
        format!("{}\n", json!({"type": "user", "message": {"role": "user", "content": brief}})),
    )
    .unwrap();
    let start = h.handle(&HookEvent { agent_id: "a2".into(), ..ev("SubagentStart") });
    assert!(start.context.is_empty());
    let first = h.handle(&HookEvent { agent_id: "a2".into(), tool_name: "Bash".into(), transcript_path: parent.to_string_lossy().into_owned(), ..ev("PreToolUse") });
    assert!(first.context.contains("lane build-7"), "{}", first.context);
    let second = h.handle(&HookEvent { agent_id: "a2".into(), tool_name: "Bash".into(), transcript_path: parent.to_string_lossy().into_owned(), ..ev("PreToolUse") });
    assert!(second.context.is_empty());
}

fn usage_line(id: &str, ctx: u64) -> String {
    format!("{}\n", json!({"type": "assistant", "message": {"id": id, "usage": {"input_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": ctx - 2, "output_tokens": 0}}}))
}

#[test]
fn the_cap_warns_then_blocks_all_but_handoff_in_subagents_only() {
    let t = Temp::new("hook-cap");
    let s = settings(t.path());
    let mut h = Hooks::open(s).unwrap();
    let dir = t.path().join("proj");
    let parent = dir.join("sess.jsonl");
    let sub = dir.join("sess").join("subagents").join("agent-a9.jsonl");
    std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
    std::fs::write(&parent, usage_line("p1", 600_000)).unwrap();
    let call = |h: &mut Hooks, agent: &str, tool: &str| {
        h.handle(&HookEvent { agent_id: agent.into(), tool_name: tool.into(), transcript_path: parent.to_string_lossy().into_owned(), ..ev("PreToolUse") })
    };
    // The main session is never capped by default, whatever its size.
    assert_eq!(call(&mut h, "", "Bash"), Default::default());
    std::fs::write(&sub, usage_line("m1", 100_000)).unwrap();
    assert_eq!(call(&mut h, "a9", "Bash"), Default::default());
    std::fs::write(&sub, usage_line("m1", 100_000) + &usage_line("m2", 451_000)).unwrap();
    let w = call(&mut h, "a9", "Bash");
    assert!(w.deny.is_none() && w.context.contains("451000 tokens"), "{w:?}");
    // Not repeated until another 25k tokens.
    std::fs::write(&sub, usage_line("m2", 451_000) + &usage_line("m3", 460_000)).unwrap();
    assert!(call(&mut h, "a9", "Bash").context.is_empty());
    std::fs::write(&sub, usage_line("m3", 460_000) + &usage_line("m4", 501_000) + &usage_line("m4", 501_000)).unwrap();
    let b = call(&mut h, "a9", "Bash");
    assert!(b.deny.as_deref().is_some_and(|r| r.contains("handoff")), "{b:?}");
    assert!(b.to_json("PreToolUse").contains("\"permissionDecision\":\"deny\""));
    let ok = call(&mut h, "a9", "mcp__plugin_atlas_atlas__handoff");
    assert!(ok.deny.is_none(), "{ok:?}");
    // Loading the deferred handoff schema is allowed too; anything else is not.
    assert!(call(&mut h, "a9", "ToolSearch").deny.is_none());
    assert!(call(&mut h, "a9", "Read").deny.is_some());
    // Both crossings are journal entries for the agent.
    let lane = h.store().lane("agent:a9").unwrap();
    let subs: Vec<&str> = lane.iter().map(|x| x.title.as_str()).collect();
    assert_eq!(subs, vec!["context cap warned at 451000 tokens", "context cap blocked at 501000 tokens"]);
}

#[test]
fn every_prompt_answer_is_token_logged_even_when_empty() {
    let t = Temp::new("hook-log");
    let s = settings(t.path());
    seed(&s);
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&HookEvent { prompt: "nothing related at all here".into(), ..ev("UserPromptSubmit") });
        h.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), ..ev("UserPromptSubmit") });
    }
    // The log thread writes after the answer; wait for both rows.
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let mut rows: Vec<(String, i64, Option<i64>)> = Vec::new();
    for _ in 0..200 {
        rows = conn
            .prepare("SELECT tool, returned, read FROM token_log ORDER BY id")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
            .unwrap_or_default();
        if rows.len() == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0], ("hook:UserPromptSubmit".to_string(), 0, None));
    assert!(rows[1].1 > 0 && rows[1].2.unwrap_or(0) > 0, "{rows:?}");
}

/// Each subagent start logs the rulebook's size at that moment to its own
/// table, never to the per-answer ledger.
#[test]
fn a_subagent_start_logs_the_brief_without_atlas() {
    let t = Temp::new("hook-brief");
    let mut s = settings(t.path());
    let rulebook = t.path().join("RULES.md");
    write(&rulebook, &"- a rule\n".repeat(100));
    s.brief_reference = Some(rulebook);
    seed(&s);
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&HookEvent { agent_id: "a1".into(), ..ev("SubagentStart") });
    }
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let mut rows: Vec<(String, String, i64)> = Vec::new();
    for _ in 0..200 {
        rows = conn
            .prepare("SELECT session, agent, bytes FROM brief_log ORDER BY id")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
            .unwrap_or_default();
        if !rows.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(rows, vec![("s1".to_string(), "a1".to_string(), 900)]);
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM token_log WHERE tool = 'hook:SubagentStart'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

fn assistant(time: &str, item: Value) -> String {
    format!("{}\n", json!({"type": "assistant", "timestamp": time, "message": {"role": "assistant", "content": [item]}}))
}

/// Every injected key is logged with its bytes and counterfactual, and the
/// report marks it hit by the fixed rules: the key in a later tool input, or
/// a quoted run of its words; an unused key is not a hit, nor a use before
/// the injection.
#[test]
fn injected_keys_are_logged_and_judged_by_later_use() {
    let t = Temp::new("hook-hits");
    let s = settings(t.path());
    seed(&s);
    let main = t.path().join("main.jsonl");
    {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.handle(&HookEvent { prompt: "why does the widget cache evict entries past sixty four widgets".into(), transcript_path: main.to_string_lossy().into(), ..ev("UserPromptSubmit") });
        h.handle(&HookEvent { prompt: "does the gadget exporter write with temp files and rename them".into(), transcript_path: main.to_string_lossy().into(), ..ev("UserPromptSubmit") });
    }
    let conn = rusqlite::Connection::open(&s.index_db).unwrap();
    let mut rows = Vec::new();
    for _ in 0..200 {
        rows = atlas::hook_hits::logged(&conn, "").unwrap();
        if rows.len() >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    assert!(keys.contains(&"widget-cache") && keys.contains(&"rules:r00001"), "{keys:?}");
    for r in &rows {
        assert_eq!(r.hook, "UserPromptSubmit");
        assert_eq!(r.transcript, main.to_string_lossy());
        assert!(r.bytes > 0 && r.read > 0 && r.fts.is_some(), "{r:?}");
    }
    let late = "2999-01-01T00:00:00.000Z";
    let mut text = assistant("2000-01-01T00:00:00.000Z", json!({"type": "tool_use", "name": "recall", "input": {"key": "rules:r00001"}}));
    text.push_str(&assistant(late, json!({"type": "tool_use", "name": "mcp__atlas__recall", "input": {"key": "widget-cache"}})));
    write(&main, &text);
    let store = MemoryStore::open(&s.store).unwrap();
    let mut judge = atlas::hook_hits::Judge::new(&store, &s.exports);
    let verdict = |judge: &mut atlas::hook_hits::Judge, key: &str| judge.hit(rows.iter().find(|r| r.key == key).unwrap());
    assert_eq!(verdict(&mut judge, "widget-cache"), Some("key"));
    // Recalled only before the injection: not a hit.
    assert_eq!(verdict(&mut judge, "rules:r00001"), None);
    text.push_str(&assistant(late, json!({"type": "text", "text": "Per the rule, the gadget exporter writes with temp files and renames them into place."})));
    write(&main, &text);
    let mut judge = atlas::hook_hits::Judge::new(&store, &s.exports);
    assert_eq!(verdict(&mut judge, "rules:r00001"), Some("quote"));
}

/// A past injection is read back out of the transcript's `hook_success`
/// attachment, with the prompt it answered.
#[test]
fn past_injections_are_found_in_transcripts() {
    let t = Temp::new("hook-found");
    let p = t.path().join("s.jsonl");
    let ctx = "atlas recall (related records; read one with recall key=KEY):\n- widget-cache: Widget cache - bound\n";
    let out = json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": ctx}}).to_string();
    let hook = json!({"type": "attachment", "timestamp": "2026-11-01T00:00:00.000Z", "sessionId": "s9", "attachment": {"type": "hook_success", "hookEvent": "UserPromptSubmit", "stdout": out}});
    let user = json!({"type": "user", "timestamp": "2026-11-01T00:00:00.100Z", "message": {"role": "user", "content": "why does the widget cache evict"}});
    write(&p, &format!("{hook}\n{user}\n"));
    let f = atlas::hook_hits::found_in(&p, "2026-10-01");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].injection.key, "widget-cache");
    assert_eq!(f[0].injection.session, "s9");
    assert_eq!(f[0].injection.bytes, "- widget-cache: Widget cache - bound\n".len());
    assert_eq!(f[0].prompt, "why does the widget cache evict");
    assert!(atlas::hook_hits::found_in(&p, "2026-12-01").is_empty());
}

/// The binary over stdio: a `hook` call is answered by the worker with the
/// hook JSON, and ordinary calls still answer in order.
#[test]
fn the_server_answers_hook_calls_with_hook_json() {
    let t = Temp::new("hook-stdio");
    let vault = t.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let cfg = t.path().join("atlas.json");
    write(&cfg, &format!("{}", json!({"db": "state/index.sqlite", "vaults": [{"name": "v", "path": "vault"}], "watch": false, "recall": {"fts_floor": 0.0}})));
    seed(&settings(t.path()));
    let mut child = Command::new(bin()).arg("--config").arg(&cfg).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    {
        let mut i = child.stdin.take().unwrap();
        let lines = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "t"}}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "hook", "arguments": {"event": "UserPromptSubmit", "session_id": "s", "prompt": "why does the widget cache evict entries past sixty four widgets"}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "recall", "arguments": {"key": "widget-cache"}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "hook", "arguments": {"event": "UserPromptSubmit", "session_id": "s", "prompt": "the weather on the coast"}}}),
        ];
        for l in lines {
            writeln!(i, "{l}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let replies: Vec<Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| json::parse(l).unwrap()).collect();
    let by = |id: i64| replies.iter().find(|r| r["id"].as_i64() == Some(id)).cloned().unwrap();
    let hook = by(2)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let hv = json::parse(&hook).unwrap();
    assert_eq!(hv["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(hv["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("widget-cache"));
    assert!(by(3)["result"]["content"][0]["text"].as_str().unwrap().contains("== widget-cache memory"));
    assert_eq!(by(4)["result"]["content"][0]["text"], "");
    let _: Option<PathBuf> = None;
}

/// With a model file present the gate also scores meaning: records get
/// vectors (journals do not), a cosine floor above every record keeps a
/// lexical match out, and a `cos_alone` below every cosine lets a record in
/// on meaning alone.
#[test]
fn the_gate_scores_meaning_with_a_model() {
    use atlas::memory::vectors::{self, RecVecs};
    let t = Temp::new("hook-sem");
    let mut s = settings(t.path());
    std::fs::create_dir_all(s.model.parent().unwrap()).unwrap();
    std::fs::write(&s.model, atlas::embed::tiny_model_bytes(true, 7)).unwrap();
    seed(&s);
    {
        // Embedded here, as the background thread would, so the test does not wait on it.
        let store = MemoryStore::open(&s.store).unwrap();
        let m = atlas::embed::Model::load(&s.model).unwrap();
        while vectors::embed_pending(&store, &m, 2, 1).unwrap() > 0 {}
        let v = RecVecs::load(&store, &m.id, m.dim).unwrap();
        assert_eq!((v.rids.len(), v.missing), (3, 0), "one memory and two rules; journals are not embedded");
    }
    let widget = "why does the widget cache evict entries past sixty four widgets";
    s.recall.cos_floor = 1.01;
    s.recall.cos_alone = 2.0;
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = h.handle(&HookEvent { prompt: widget.into(), ..ev("UserPromptSubmit") });
    assert!(!a.context.contains("widget-cache"), "a lexical match below the cosine floor passed: {}", a.context);
    s.recall.cos_floor = 0.0;
    let mut h = Hooks::open(s.clone()).unwrap();
    let a = h.handle(&HookEvent { prompt: widget.into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("widget-cache"), "{}", a.context);
    s.recall.cos_alone = -1.01;
    let mut h = Hooks::open(s).unwrap();
    let a = h.handle(&HookEvent { prompt: "what is the weather like on the coast".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("atlas recall"), "nothing passed on meaning alone: {}", a.context);
}

/// R6: recall injects pointers (key and title), within 150 tokens, never a
/// body; and on a new store of ten records the default gate (calibrated on
/// a large store) still recalls the related record and nothing for an
/// unrelated prompt, because the bm25 floor follows the store's size.
#[test]
fn a_ten_record_store_recalls_pointers_with_the_default_gate() {
    use atlas::memory::settings::floor_scale;
    assert!((floor_scale(1400) - 1.0).abs() < 1e-9);
    assert!((floor_scale(10) - 0.27).abs() < 0.01, "{}", floor_scale(10));
    assert_eq!(floor_scale(2), 0.0);
    let t = Temp::new("hook-ten");
    let mut s = Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    assert_eq!(s.recall.budget_tokens, 150);
    {
        let mut store = MemoryStore::open(&s.store).unwrap();
        let mut w = store.writer().unwrap();
        let topics = [
            ("widget-cache", "memory", "- [Widget cache](widget-cache.md) - eviction bound", "The widget cache evicts least recently used entries past sixty four widgets, oldest first."),
            ("rules:r00001", "rule", "", "- the gadget exporter writes temp files and renames them into place."),
            ("rules:r00002", "rule", "", "- the garden is watered on mondays and fridays."),
            ("rules:r00003", "rule", "", "- release builds are signed before upload."),
            ("rules:r00004", "rule", "", "- the parser rejects tabs inside quoted keys."),
            ("rules:r00005", "rule", "", "- backups rotate weekly and keep four copies."),
            ("rules:r00006", "rule", "", "- the scheduler wakes every five minutes."),
            ("rules:r00007", "rule", "", "- error messages name the file and the line."),
            ("rules:r00008", "rule", "", "- the font cache is rebuilt at install."),
            ("rules:r00009", "rule", "", "- logs older than thirty days are compressed."),
        ];
        for (key, kind, title, body) in topics {
            let d = Draft { title: title.into(), body: body.into(), reason: "ten-record test".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
            w.create(key, kind, if kind == "memory" { "feedback" } else { "" }, if kind == "rule" { "rules" } else { "" }, &d, None).unwrap();
        }
        w.commit().unwrap();
    }
    let mut h = Hooks::open(s.clone()).unwrap();
    h.set_offer_paths(None);
    let a = h.handle(&HookEvent { prompt: "why does the widget cache evict the least recently used widgets".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.starts_with(atlas::hooks::POINTER_HEAD), "{:?}", a.context);
    assert!(a.context.contains("- widget-cache: Widget cache - eviction bound\n"), "{}", a.context);
    assert!(!a.context.contains("sixty four"), "a pointer, not the body: {}", a.context);
    assert!(a.context.len() <= s.bytes_for(150), "{}", a.context.len());
    let a = h.handle(&HookEvent { prompt: "what time does the train to the coast leave".into(), ..ev("UserPromptSubmit") });
    assert_eq!(a.context, "", "unrelated: nothing");
    let a = h.handle(&HookEvent { prompt: "the gadget exporter writes temp files, then renames them into place?".into(), ..ev("UserPromptSubmit") });
    assert!(a.context.contains("- rules:r00001: the gadget exporter writes temp files and renames them into place.\n"), "{}", a.context);
    // Already injected in this session: not again.
    let a = h.handle(&HookEvent { prompt: "why does the widget cache evict the least recently used widgets".into(), ..ev("UserPromptSubmit") });
    assert!(!a.context.contains("widget-cache"), "{}", a.context);
}

#[test]
fn an_edit_reindexes_the_file_once_the_answer_is_out() {
    let t = Temp::new("h-r7");
    let s = settings(t.path());
    // A code root, indexed by the line scan, and a vault.
    let root = t.path().join("app");
    write(&root.join("src/a.rs"), "pub struct A;\n\nimpl A {\n    pub fn one(&self) -> u32 {\n        1\n    }\n}\n");
    let notes = t.path().join("notes");
    write(&notes.join("n.md"), "# N\n\nfirst words\n");
    let vaults = || atlas::paths::Vaults::new(&[("notes".to_string(), notes.clone())]).unwrap();
    drop(atlas::store::Store::open(&s.index_db, vaults()).unwrap());
    let spec = atlas::config::CodeSpec { roots: vec![atlas::config::CodeRootSpec { name: "app".into(), root: root.clone(), build_scripts: false }], ..Default::default() };
    let code = atlas::code::CodeSetup::from_spec(&spec, &s.index_db, false);
    let mut conn = rusqlite::Connection::open(&s.index_db).unwrap();
    atlas::code_index::index_root(&code, &mut conn, &code.roots[0], &atlas::code_index::RustSource::ScanOnly("test".into())).unwrap();
    let mut st = atlas::store::Store::open(&s.index_db, vaults()).unwrap();
    st.rescan().unwrap();
    drop(st);
    let sha = |c: &rusqlite::Connection| -> String { c.query_row("SELECT sha256 FROM notes WHERE path = 'n.md'", [], |r| r.get(0)).unwrap() };
    let before = sha(&conn);
    let mut h = Hooks::open(s.clone()).unwrap();
    let db = s.index_db.clone();
    let nv = notes.clone();
    h.set_index_opener(Some(Box::new(move || atlas::store::Store::open(&db, atlas::paths::Vaults::new(&[("notes".to_string(), nv.clone())])?))));
    // The edit moves `one` down two lines and adds `two`.
    let a = root.join("src/a.rs");
    // Past the root's indexing time (milliseconds).
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&a, "pub struct A;\n\n// moved\n// down\nimpl A {\n    pub fn one(&self) -> u32 {\n        1\n    }\n    pub fn two(&self) -> u32 {\n        2\n    }\n}\n").unwrap();
    let path = a.to_string_lossy().to_string();
    assert!(atlas::code::file_items(&conn, &path).unwrap().is_none(), "stale before the edit's re-index");
    let edit = |tool: &str, file: &str| HookEvent {
        event: "PostToolUse".into(),
        session_id: "s1".into(),
        tool_name: tool.into(),
        tool_input: json!({"file_path": file, "old_string": "x", "new_string": "y"}),
        tool_response: json!({"filePath": file}),
        ..HookEvent::default()
    };
    let ans = h.handle(&edit("Edit", &path));
    // The answer does not wait for the re-index, and says nothing.
    assert!(ans.to_json("PostToolUse").len() < 80, "{}", ans.to_json("PostToolUse"));
    h.after();
    let items = atlas::code::file_items(&conn, &path).unwrap().expect("covered again");
    let labels: Vec<String> = items.iter().map(|i| i.label()).collect();
    assert!(labels.contains(&"fn A::one 6-8".to_string()) && labels.contains(&"fn A::two 9-11".to_string()) && labels.contains(&"impl A 5-12".to_string()), "{labels:?}");
    let v = atlas::code::symbol(&conn, "two", atlas::code::Limits { defs: 5, refs: 5, impls: 5 }).unwrap();
    assert_eq!(v.totals[0], 1);
    // A note in a vault: rescanned.
    let n = notes.join("n.md");
    std::fs::write(&n, "# N\n\nfirst words, then more\n").unwrap();
    h.handle(&edit("Write", &n.to_string_lossy()));
    h.after();
    assert_ne!(sha(&conn), before);
    // A deleted code file loses its rows; a file nothing covers is left alone.
    std::fs::remove_file(&a).unwrap();
    assert_eq!(h_reindex(&path, &s), atlas::code::Reindexed::Removed);
    assert_eq!(h_reindex(&t.path().join("elsewhere.rs").to_string_lossy(), &s), atlas::code::Reindexed::NotCovered);
    assert!(atlas::code::symbol(&conn, "two", atlas::code::Limits { defs: 5, refs: 5, impls: 5 }).unwrap().totals == [0, 0, 0]);
}

fn h_reindex(path: &str, s: &Settings) -> atlas::code::Reindexed {
    atlas::hook_rewrite::Router::new(s).reindex_code(path).unwrap()
}