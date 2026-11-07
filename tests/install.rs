//! The out-of-box install gate, through the binary on this OS: a fresh
//! home (atlas's folder, Claude Code's folder, HOME), no configuration and
//! no edits. The server starts as Claude Code starts it (stdio, no
//! arguments), answers every hook `hooks/hooks.json` wires, offers the
//! settings once, remembers and recalls; then `atlas settings` and
//! `atlas --setup` run against the same home. The network is closed (a
//! dead proxy), so the background model download fails fast and the
//! server stays lexical, as it would offline.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use atlas::json;
use atlas::json::Value;
use common::*;

fn fresh(c: &mut Command, root: &Path) {
    c.env("ATLAS_HOME", root.join("atlas"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env_remove("ATLAS_CONFIG")
        .env_remove("CLAUDE_CODE_SESSION_ATTENDED")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    for k in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        c.env(k, "http://127.0.0.1:9");
    }
    c.env_remove("NO_PROXY").env_remove("no_proxy");
}

/// `${name}` placeholders of a hook's input, filled as Claude Code fills
/// them (objects arrive as their JSON text).
fn fill(input: &Value, vals: &Value) -> Value {
    let mut out = json::Map::new();
    for (k, v) in input.as_object().unwrap().iter() {
        let x = match v.as_str().and_then(|s| s.strip_prefix("${")).and_then(|s| s.strip_suffix('}')) {
            Some(name) => match vals.get(name) {
                Some(Value::String(s)) => Value::String(s.clone()),
                Some(o) => Value::String(o.to_string()),
                None => Value::String(String::new()),
            },
            None => v.clone(),
        };
        out.insert(k.clone(), x);
    }
    Value::Object(out)
}

#[test]
fn a_fresh_home_serves_every_hook_and_offers_the_settings() {
    let t = Temp::new("install");
    let root = t.path();
    std::fs::create_dir_all(root.join("claude")).unwrap();
    let transcript = root.join("transcript.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let hooks = json::parse(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("hooks").join("hooks.json")).unwrap()).unwrap();
    let vals = json!({
        "session_id": "s1", "prompt": "how does the cache evict entries", "transcript_path": transcript.to_string_lossy().to_string(),
        "agent_id": "", "agent_type": "general-purpose", "tool_name": "Bash", "tool_input.prompt": "", "trigger": "manual",
        "source": "startup", "permission_mode": "default", "cwd": root.to_string_lossy().to_string(),
        "tool_input": {"command": "ls"},
        "tool_response": {"stdout": "a\nb\n", "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}
    });
    let mut lines = vec![
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "install-test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ];
    let mut hook_ids = Vec::new();
    let mut commands = Vec::new();
    let mut id = 10;
    for (event, entries) in hooks["hooks"].as_object().unwrap().iter() {
        for e in entries.as_array().unwrap() {
            for h in e["hooks"].as_array().unwrap() {
                if h["type"] == "command" {
                    // Claude Code runs no mcp_tool hook at SessionStart (measured:
                    // "no MCP client context"), so that one is `atlas --hook`.
                    assert_eq!(h["command"], "\"${CLAUDE_PLUGIN_ROOT}/bin/atlas\" --hook", "{event}");
                    commands.push(event.clone());
                    continue;
                }
                assert_eq!(h["type"], "mcp_tool", "{event}");
                assert_eq!(h["server"], "plugin:atlas:atlas", "{event}");
                id += 1;
                hook_ids.push((id, event.clone()));
                lines.push(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": h["tool"].as_str().unwrap(), "arguments": fill(&h["input"], &vals)}}));
            }
        }
    }
    let events: Vec<&str> = hook_ids.iter().map(|(_, e)| e.as_str()).chain(commands.iter().map(String::as_str)).collect();
    assert_eq!(commands, ["SessionStart"]);
    for e in ["SessionStart", "UserPromptSubmit", "SubagentStart", "SubagentStop", "PreToolUse", "PostToolUse", "PreCompact"] {
        assert!(events.contains(&e), "hooks.json wires {e}");
    }
    lines.push(json!({"jsonrpc": "2.0", "id": 90, "method": "tools/call", "params": {"name": "remember", "arguments": {"kind": "rule", "title": "Cache eviction", "body": "The cache evicts entries by age, oldest first, when it is full.", "reason": "install test"}}}));
    lines.push(json!({"jsonrpc": "2.0", "id": 91, "method": "tools/call", "params": {"name": "recall", "arguments": {"query": "cache eviction"}}}));
    lines.push(json!({"jsonrpc": "2.0", "id": 92, "method": "tools/call", "params": {"name": "hook", "arguments": {"event": "SessionStart", "session_id": "s2", "source": "startup"}}}));

    // SessionStart first, as Claude Code runs it: a command hook with the
    // event JSON on stdin. It is also the first run, so it writes the template.
    let mut c = Command::new(bin());
    fresh(&mut c, root);
    let mut hook = c.arg("--hook").current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let input = json!({"hook_event_name": "SessionStart", "session_id": "s1", "source": "startup", "transcript_path": transcript.to_string_lossy().to_string(), "cwd": root.to_string_lossy().to_string()});
    write!(hook.stdin.take().unwrap(), "{input}").unwrap();
    let ho = hook.wait_with_output().unwrap();
    let herr = String::from_utf8_lossy(&ho.stderr).to_string();
    assert!(ho.status.success() && herr.contains("first run"), "{herr}");
    let ctx = json::parse(&String::from_utf8_lossy(&ho.stdout)).unwrap()["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
    assert!(ctx.starts_with("atlas: 2 Claude Code settings are offered"), "{ctx}");
    // The settings line stays within 60 tokens; the orchestrated-mode
    // question follows it once, within its own budget (about 200 tokens).
    let (settings, orchestrate) = ctx.split_once(" atlas: orchestrated mode is offered").unwrap_or((ctx.as_str(), ""));
    assert!(settings.len() <= 240, "{ctx}");
    assert!(!orchestrate.is_empty() && orchestrate.len() <= 800, "{} bytes: {ctx}", orchestrate.len());
    // Experimental and off by default: a session never hears of memory rules.
    assert!(!ctx.contains("memory folder") && !ctx.contains("memory-rules"), "{ctx}");

    let mut c = Command::new(bin());
    fresh(&mut c, root);
    let mut child = c.current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    {
        let mut i = child.stdin.take().unwrap();
        for l in &lines {
            writeln!(i, "{l}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "{err}");
    let replies: Vec<Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| json::parse(l).unwrap()).collect();
    let by = |id: i64| replies.iter().find(|r| r["id"].as_i64() == Some(id)).cloned().unwrap_or_else(|| panic!("no answer to {id}: {err}"));
    assert!(by(1).get("result").is_some(), "{}", by(1));
    let names: Vec<String> = by(2)["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    for n in ["hook", "recall", "remember", "lookup"] {
        assert!(names.contains(&n.to_string()), "{n} in {names:?}");
    }
    let text = |r: &Value| r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
    for (id, event) in &hook_ids {
        let r = by(*id);
        assert!(r.get("result").is_some() && r["result"]["isError"] != true, "{event}: {r}");
        let t = text(&r);
        if !t.is_empty() {
            let h = json::parse(&t).unwrap_or_else(|e| panic!("{event} answered non-JSON {t:?}: {e:?}"));
            assert_eq!(h["hookSpecificOutput"]["hookEventName"], event.as_str());
        }
    }
    // A recall query answers the best record whole.
    assert!(text(&by(91)).contains("evicts entries by age"), "{}", by(91));
    assert_eq!(text(&by(92)), "", "the first-session questions are the command hook's: a tool call does not say a user is there, so it asks nothing");
    // What the first run wrote: the template, its vault, the state.
    let home = root.join("atlas");
    assert_eq!(std::fs::read_to_string(home.join("atlas.json")).unwrap(), atlas::config::TEMPLATE);
    assert!(home.join("notes").is_dir());
    assert!(home.join("state").join("atlas.sqlite").is_file());
    assert!(home.join("state").join(atlas::claude_settings::OFFER_STATE).is_file());
    assert!(!root.join("claude").join("settings.json").exists(), "nothing written without a yes");

    // The settings step, from the terminal.
    let run = |args: &[&str]| {
        let mut c = Command::new(bin());
        fresh(&mut c, root);
        let o = c.args(args).stdin(Stdio::null()).output().unwrap();
        (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    };
    let (ok, o) = run(&["settings", "list"]);
    assert!(ok && o.contains("[subagent-cache-1h]") && o.contains("[routing]"), "{o}");
    let (ok, o) = run(&["--setup"]);
    // The model download fails (no network here); the settings step still runs and writes nothing.
    assert!(o.contains("Claude Code settings atlas offers") && o.contains("Nothing went wrong"), "{ok} {o}");
    assert!(!root.join("claude").join("settings.json").exists());
    let (ok, o) = run(&["settings", "offer", "--yes", "subagent-cache-1h", "goal-checkin-120"]);
    assert!(ok, "{o}");
    let s = json::parse(&std::fs::read_to_string(root.join("claude").join("settings.json")).unwrap()).unwrap();
    assert_eq!(s["subagentPromptCacheTtl"], "1h");
    assert_eq!(s["env"]["CLAUDE_CODE_GOAL_CHECKIN_MINUTES"], "120");
}
