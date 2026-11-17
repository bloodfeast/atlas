//! The `tools/list` answer stays small: every byte of it is sent each time a
//! client loads the tools, before any question is asked.

use atlas::json;

/// The ceiling, in tokens at the default ratio of bytes per token. Raised from
/// 1500 to 1515 when the journal description began naming its arguments (#225),
/// and from 1515 to 1530 when the `note` description began steering a
/// one-heading read to `section` (#213: 6,057 to 6,118 bytes, 1,529.5 tokens).
/// Lowered to 1511 when `lookup` and `symbol` lost their always-loaded mark
/// (journal:ATLAS-EFFICIENCY:0004: 6,118 to 6,042 bytes, 1,510.5 tokens).
const BUDGET_TOKENS: f64 = 1511.0;

#[test]
fn tools_list_fits_its_budget() {
    let tools = atlas::tools::tool_list();
    let mut rows: Vec<(usize, String)> = tools.iter().map(|t| (t.to_string().len(), t["name"].as_str().unwrap_or("").to_string())).collect();
    rows.sort();
    for (b, n) in &rows {
        eprintln!("{b:>6} {n}");
    }
    let whole = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": tools}}).to_string().len();
    let tokens = whole as f64 / atlas::tokens::DEFAULT_BYTES_PER_TOKEN;
    eprintln!("tools/list: {whole} bytes, {tokens:.0} tokens, {} tools", rows.len());
    assert!(tokens <= BUDGET_TOKENS, "tools/list is {tokens:.0} tokens; the budget is {BUDGET_TOKENS}");
}

mod common;
use common::*;

/// The fixed bytes an agent start is charged (`token_cost::start_bytes`)
/// are the tools list and the instructions after their per-project head
/// line: the guide and the style rule (#274).
#[test]
fn start_bytes_are_the_tools_and_the_instructions_without_their_head() {
    let fx = Fx::new("start-bytes");
    let s = fx.server();
    let ins = s.instructions();
    let head = ins.lines().next().unwrap_or("").len();
    let tools = json::Value::Array(atlas::tools::tool_list()).to_string().len();
    assert!(ins.ends_with(atlas::tools::STYLE), "{ins}");
    assert_eq!(atlas::token_cost::start_bytes(), tools + ins.len() - head, "{ins}");
}

/// The style rule is paid at every agent start and in each writing
/// context's first journal or handoff answer: it stays short (#274: about
/// 40 words).
#[test]
fn the_style_rule_stays_short() {
    let style = atlas::tools::STYLE;
    let words = style.split_whitespace().count();
    assert!(style.len() <= 260 && words <= 45, "STYLE is {} bytes, {words} words; the cap is 260 bytes, 45 words: {style}", style.len());
    assert!(!style.contains('\n'), "one line of the instructions");
}

#[test]
fn every_listed_argument_is_taken_by_an_op_of_its_tool() {
    let full = atlas::tools::full_tool_list();
    let props_of = |name: &str| -> Vec<String> {
        let t = full.iter().find(|t| t["name"] == json!(name)).unwrap_or_else(|| panic!("no full tool {name}"));
        t["inputSchema"]["properties"].as_object().unwrap().keys().cloned().collect()
    };
    for t in atlas::tools::tool_list() {
        let name = t["name"].as_str().unwrap();
        let mut runs: Vec<&str> = atlas::tools::ops().filter(|o| o.0 == name).map(|o| o.2).collect();
        match name {
            "lookup" => runs.extend(["lookup", "ids"]),
            "remember" => runs.extend(["remember", "supersede"]),
            "recall" | "search" | "section" | "grep" | "symbol" | "journal" | "handoff" | "hook" => runs.push(name),
            _ => {}
        }
        assert!(!runs.is_empty(), "{name} runs nothing");
        for k in t["inputSchema"]["properties"].as_object().unwrap().keys() {
            if k == "op" {
                continue;
            }
            // `edit` link and `note` link take vault and path for their own names.
            let taken = runs.iter().any(|r| props_of(r).contains(k)) || matches!(k.as_str(), "vault" | "path");
            assert!(taken, "{name} lists {k}, which none of {runs:?} takes");
        }
    }
}

#[test]
fn ops_run_their_full_tools_and_the_old_names_still_work() {
    let fx = Fx::new("surface-ops");
    let mut s = fx.server();
    let via_op = s.answer("note", &json!({"vault": "perf", "path": "Index.md"})).unwrap().into_text();
    let via_alias = s.answer("read_note", &json!({"vault": "perf", "path": "Index.md", "fresh": true})).unwrap().into_text();
    assert_eq!(via_op, via_alias);
    assert!(via_op.starts_with("perf:Index.md sha256="), "{via_op}");
    // JSON on request, as before.
    let j = s.answer("note", &json!({"op": "read", "vault": "perf", "path": "Index.md", "format": "json"})).unwrap().into_text();
    assert!(atlas::json::parse(&j).unwrap()["content"].is_string(), "{j}");
    // lookup without an id lists identifiers, as ids does.
    let a = s.answer("lookup", &json!({"prefix": "PI-"})).unwrap().into_text();
    let b = s.answer("ids", &json!({"prefix": "PI-", "fresh": true})).unwrap().into_text();
    assert_eq!(a, b);
    // An unknown op names the ones there are; a merged write tool needs one.
    let e = s.answer("note", &json!({"op": "nope"})).unwrap_err().to_string();
    assert!(e.contains("read") && e.contains("backlinks"), "{e}");
    let e = s.answer("edit", &json!({"vault": "perf"})).unwrap_err().to_string();
    assert!(e.contains("needs op"), "{e}");
    // A write through an op still needs its reason.
    let e = s.answer("edit", &json!({"op": "append", "vault": "perf", "path": "Index.md", "text": "x"})).unwrap_err().to_string();
    assert!(e.contains("reason"), "{e}");
    // The help names every full tool.
    let h = s.answer("note", &json!({"op": "help"})).unwrap().into_text();
    for t in atlas::tools::full_tool_list() {
        assert!(h.contains(&format!("### {}\n", t["name"].as_str().unwrap())), "help misses {}", t["name"]);
    }
}

#[test]
fn the_help_is_a_resource() {
    let fx = Fx::new("surface-help");
    let mut s = fx.server();
    let list = atlas::protocol::handle(&mut s, &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"})).unwrap();
    assert_eq!(list["result"]["resources"][0]["uri"], json!(atlas::tools::HELP_URI));
    let read = atlas::protocol::handle(&mut s, &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": atlas::tools::HELP_URI}})).unwrap();
    assert_eq!(read["result"]["contents"][0]["text"].as_str().unwrap(), atlas::tools::help());
    let missing = atlas::protocol::handle(&mut s, &json!({"jsonrpc": "2.0", "id": 3, "method": "resources/read", "params": {"uri": "atlas://nope"}})).unwrap();
    assert!(missing.get("error").is_some(), "{missing}");
}

#[test]
fn read_note_shows_a_line_range_or_a_budget() {
    let fx = Fx::new("surface-lines");
    let mut s = fx.server();
    let whole = s.answer("note", &json!({"vault": "perf", "path": "Index.md"})).unwrap().into_text();
    let body: Vec<&str> = whole.lines().skip(1).collect();
    assert!(body.len() >= 3, "{whole}");
    let part = s.answer("note", &json!({"vault": "perf", "path": "Index.md", "lines": "2-3"})).unwrap().into_text();
    let mut lines = part.lines();
    assert!(lines.next().unwrap().ends_with(&format!(" lines 2-3 of {}", body.len())), "{part}");
    assert_eq!(lines.collect::<Vec<_>>(), body[1..3].to_vec());
    // A budget of one token still shows the first line, and names the rest.
    let cut = s.answer("note", &json!({"vault": "perf", "path": "Index.md", "budget_tokens": 1})).unwrap().into_text();
    assert!(cut.contains(&format!("\n{}\n", body[0])) && cut.ends_with(&format!("+{} more lines (lines=2-{})\n", body.len() - 1, body.len())), "{cut}");
    let e = s.answer("note", &json!({"vault": "perf", "path": "Index.md", "lines": "3-1"})).unwrap_err().to_string();
    assert!(e.contains("lines"), "{e}");
    let j = call(&mut s, "read_note", json!({"vault": "perf", "path": "Index.md", "lines": "2-3"}));
    assert_eq!(j["lines"], json!("2-3"));
    assert_eq!(j["content"].as_str().unwrap(), format!("{}\n{}\n", body[1], body[2]));
}

#[test]
fn a_repeat_by_the_same_agent_is_a_reference() {
    let fx = Fx::new("surface-dedup");
    let mut s = fx.server();
    s.set_memory(atlas::memory::settings::Settings::from_config(None, &fx.db).unwrap());
    let read = json!({"vault": "perf", "path": "Index.md"});
    let ask = |s: &mut atlas::tools::Server, agent: &str, args: &atlas::json::Value| {
        let hook = json!({"event": "PreToolUse", "session_id": "S", "agent_id": agent, "tool_name": "mcp__atlas__note"});
        s.answer("hook", &hook).unwrap();
        s.answer("note", args).unwrap().into_text()
    };
    let first = ask(&mut s, "a1", &read);
    assert!(first.starts_with("perf:Index.md"), "{first}");
    let again = ask(&mut s, "a1", &read);
    assert!(again.starts_with("unchanged since your call #1"), "{again}");
    // Another agent of the same session never saw it.
    assert_eq!(ask(&mut s, "a2", &read), first);
    // fresh: true, or no hook before the call, gives it whole.
    let mut fresh = read.clone();
    fresh["fresh"] = json!(true);
    assert_eq!(ask(&mut s, "a1", &fresh), first);
    assert_eq!(s.answer("note", &read).unwrap().into_text(), first);
    // Compaction forgets.
    s.answer("hook", &json!({"event": "PreCompact", "session_id": "S"})).unwrap();
    assert_eq!(ask(&mut s, "a1", &read), first);
}

#[test]
fn without_the_hooks_a_repeat_is_per_server_and_names_fresh() {
    let fx = Fx::new("surface-dedup-process");
    let mut s = fx.server();
    let read = json!({"vault": "perf", "path": "Index.md"});
    let first = s.answer("note", &read).unwrap().into_text();
    // No hook names the caller: the server process is the caller, so the
    // earlier call may be another agent's, and the reply says how to get it.
    let again = s.answer("note", &read).unwrap().into_text();
    assert!(again.starts_with("unchanged since call #1") && again.contains("fresh: true"), "{again}");
    let mut fresh = read.clone();
    fresh["fresh"] = json!(true);
    assert_eq!(s.answer("note", &fresh).unwrap().into_text(), first);
}

/// A `Write` the test reads back after the server is done with it.
#[derive(Clone, Default)]
struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Review finding 1, as Claude Code drives the server: hook calls go to the
/// hook worker, two agents announce before either call runs, and each
/// answer must be filed under the agent that made the call. The reader
/// notes hooks as it reads them, so it may have read later announcements
/// (B's and A's second reads of X) before the main thread runs call 1;
/// each call must still see only what came before it in the stream.
fn interleaved_agents_run(fx: &Fx) -> Result<(), String> {
    let mut s = fx.server();
    let settings = atlas::memory::settings::Settings::from_config(None, &fx.db).unwrap();
    s.set_memory(settings.clone());
    let x = json!({"vault": "perf", "path": "Index.md"});
    let y = json!({"vault": "campaign", "path": "Missions/M1 Hold.md"});
    let mut id = 0;
    let mut msgs = Vec::new();
    let mut pre = |agent: &str, args: &atlas::json::Value, use_id: &str| {
        id += 1;
        let input = json!({"event": "PreToolUse", "session_id": "S", "agent_id": agent, "tool_name": "mcp__plugin_atlas_atlas__note", "tool_input": args.to_string(), "tool_use_id": use_id});
        json!({"jsonrpc": "2.0", "id": format!("h{id}"), "method": "tools/call", "params": {"name": "hook", "arguments": input}}).to_string()
    };
    let call = |n: u32, args: &atlas::json::Value, meta: Option<&str>| {
        let mut p = json!({"name": "note", "arguments": args.clone()});
        if let Some(u) = meta {
            p["_meta"] = json!({"claudecode/toolUseId": u});
        }
        json!({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": p}).to_string()
    };
    // A and B announce, then A's call and B's call run.
    msgs.push(pre("A", &x, "tu1"));
    msgs.push(pre("B", &y, "tu2"));
    msgs.push(call(1, &x, None));
    msgs.push(call(2, &y, None));
    // B reads what only A has seen; then A reads it again.
    msgs.push(pre("B", &x, "tu3"));
    msgs.push(call(3, &x, None));
    msgs.push(pre("A", &x, "tu4"));
    msgs.push(call(4, &x, Some("tu4")));
    let input = std::io::Cursor::new((msgs.join("\n") + "\n").into_bytes());
    let out = Shared::default();
    atlas::protocol::serve_with_hooks(&mut s, Some(settings), None, input, out.clone()).map_err(|e| e.to_string())?;
    let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    let answer = |n: u32| -> Result<String, String> {
        let line = text.lines().map(|l| atlas::json::parse(l).unwrap()).find(|v| v["id"] == json!(n)).ok_or_else(|| format!("no answer {n} in {text}"))?;
        Ok(line["result"]["content"][0]["text"].as_str().unwrap_or("").to_string())
    };
    let (a1, a2, a3, a4) = (answer(1)?, answer(2)?, answer(3)?, answer(4)?);
    let head = |t: &str| t.lines().next().unwrap_or("").to_string();
    if !a1.starts_with("perf:Index.md") {
        return Err(format!("answer 1: {}", head(&a1)));
    }
    if !a2.starts_with("campaign:Missions/M1 Hold.md") {
        return Err(format!("answer 2: {}", head(&a2)));
    }
    if a3 != a1 {
        return Err(format!("B never saw X, so it comes whole; answer 3: {}", head(&a3)));
    }
    if !a4.starts_with("unchanged since your call #1") {
        return Err(format!("answer 4: {}", head(&a4)));
    }
    Ok(())
}

#[test]
fn interleaved_agents_through_the_hook_worker_keep_their_own_calls() {
    let fx = Fx::new("surface-dedup-worker");
    interleaved_agents_run(&fx).unwrap();
}

/// The same run many times on many threads at once, so the reader and the
/// main thread interleave every way they can. Run with `--ignored`;
/// `ATLAS_STRESS_RUNS` sets the runs per thread (default 50) and
/// `ATLAS_STRESS_THREADS` the threads (default 16).
#[test]
#[ignore]
fn interleaved_agents_under_load_keep_their_own_calls() {
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (runs, threads) = (env("ATLAS_STRESS_RUNS", 50), env("ATLAS_STRESS_THREADS", 16));
    let fails: Vec<String> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                sc.spawn(move || {
                    let fx = Fx::new(&format!("surface-dedup-stress-{t}"));
                    (0..runs).filter_map(|_| interleaved_agents_run(&fx).err()).collect::<Vec<_>>()
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    eprintln!("stress: {} of {} runs failed", fails.len(), runs * threads);
    for f in fails.iter().take(5) {
        eprintln!("  {f}");
    }
    assert!(fails.is_empty(), "{} of {} runs failed", fails.len(), runs * threads);
}

/// Every `mcp_tool` input the plugin's hooks/hooks.json sends, for every
/// event and every tool its matcher covers, is taken by the `hook` tool's
/// schema through `tools/call` (bloodfeast/atlas-dev#104: an undeclared
/// `tool_use_id` failed every PreToolUse hook). PreToolUse stays on `*`:
/// the context cap blocks every tool but the handoff in a capped agent.
#[test]
fn every_hooks_json_input_is_taken_by_the_hook_tool() {
    let fx = Fx::new("surface-hooks-json");
    let mut s = fx.server();
    s.set_memory(atlas::memory::settings::Settings::from_config(None, &fx.db).unwrap());
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/hooks/hooks.json")).unwrap();
    let v = atlas::json::parse(&text).unwrap();
    let cwd = fx.tmp.path().to_string_lossy().to_string();
    // The PreToolUse consumers: routing (Bash, PowerShell), the review gate
    // (Bash), the agent cap (every tool), the orchestrator (Agent, Task,
    // SendMessage), the report backstop (SubagentHandback), the lane journal
    // and dedup (mcp__atlas__*), and any other server's tools.
    let any = ["Bash", "PowerShell", "Read", "Grep", "Agent", "Task", "SendMessage", "SubagentHandback", "mcp__atlas__journal", "mcp__atlas__note", "mcp__other__tool"];
    let mut sent = 0;
    let mut id = 0;
    for (event, groups) in v["hooks"].as_object().unwrap() {
        for g in groups.as_array().unwrap() {
            let matcher = g.get("matcher").and_then(atlas::json::Value::as_str).unwrap_or("");
            let tools: Vec<&str> = match matcher {
                "" => vec![""],
                "*" => any.to_vec(),
                m => m.split('|').collect(),
            };
            for h in g["hooks"].as_array().unwrap() {
                if h["type"] != json!("mcp_tool") {
                    continue;
                }
                assert_eq!(h["tool"], json!("hook"), "{event}");
                for tool in &tools {
                    let tool_input = match *tool {
                        "Bash" | "PowerShell" => json!({"command": "git status", "description": "d"}).to_string(),
                        "Agent" | "Task" => json!({"prompt": "Lane: X\nbuild it", "description": "d"}).to_string(),
                        _ => json!({"file_path": "a.rs"}).to_string(),
                    };
                    let fill = |k: &str, val: &atlas::json::Value| {
                        let Some(p) = val.as_str().filter(|p| p.starts_with("${")) else { return val.clone() };
                        json!(match p {
                            "${tool_name}" => tool.to_string(),
                            "${tool_input}" => tool_input.clone(),
                            "${tool_input.prompt}" => "build it".to_string(),
                            "${tool_response}" => json!({"stdout": "ok", "stderr": ""}).to_string(),
                            "${stop_hook_active}" => "false".to_string(),
                            "${cwd}" => cwd.clone(),
                            "${permission_mode}" => "default".to_string(),
                            "${transcript_path}" => format!("{cwd}/none.jsonl"),
                            _ => format!("{k}-1"),
                        })
                    };
                    let args: atlas::json::Map = h["input"].as_object().unwrap().iter().map(|(k, val)| (k.clone(), fill(k, val))).collect();
                    let args = atlas::json::Value::Object(args);
                    id += 1;
                    let msg = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hook", "arguments": args}});
                    let r = atlas::protocol::handle(&mut s, &msg).unwrap();
                    assert!(r.get("error").is_none() && r["result"]["isError"] != json!(true), "{event} {tool}: {r}");
                    sent += 1;
                }
            }
        }
    }
    assert!(sent >= 20, "{sent} hook inputs sent");
}

/// N3 of the #188 re-review: the server's hook worker sets BO-1's Binding
/// compiler (protocol.rs), so a binding-mode subagent started through the
/// `hook` tool is given its lane's block.
#[test]
fn the_hook_worker_sets_the_binding_compiler() {
    let fx = Fx::new("surface-binding-wired");
    let mut s = fx.server();
    let mut settings = atlas::memory::settings::Settings::from_config(None, &fx.db).unwrap();
    settings.recall.subagent_context = atlas::memory::settings::SubagentContext::Binding;
    {
        let mut m = atlas::memory::MemoryStore::open(&settings.store).unwrap();
        let mut w = m.writer().unwrap();
        for i in 1..=3 {
            let d = atlas::memory::Draft { title: format!("decision {i}"), body: format!("Decision {i} of ALPHA.\n"), reason: "test".into(), author: "t".into(), client: "t".into(), ..Default::default() };
            w.create(&format!("journal:ALPHA:{i:04}"), "journal", "decision", "ALPHA", &d, None).unwrap();
        }
        w.commit().unwrap();
    }
    s.set_memory(settings.clone());
    let sess = fx.tmp.path().join("proj").join("sess.jsonl");
    std::fs::create_dir_all(fx.tmp.path().join("proj").join("sess").join("subagents")).unwrap();
    std::fs::write(&sess, "").unwrap();
    let tp = sess.to_string_lossy().into_owned();
    let brief = "Lane: ALPHA\nBuild alpha.";
    let hook = |id: &str, args: atlas::json::Value| json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hook", "arguments": args}}).to_string();
    let msgs = [
        hook("h1", json!({"event": "PreToolUse", "session_id": "S", "tool_name": "Agent", "tool_use_id": "tA", "transcript_path": tp, "tool_input": {"description": "d", "subagent_type": "general-purpose", "prompt": brief}})),
        hook("h2", json!({"event": "SubagentStart", "session_id": "S", "agent_id": "x", "agent_type": "general-purpose", "transcript_path": tp})),
    ];
    let input = std::io::Cursor::new((msgs.join("\n") + "\n").into_bytes());
    let out = Shared::default();
    atlas::protocol::serve_with_hooks(&mut s, Some(settings), None, input, out.clone()).unwrap();
    let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    let start = text.lines().map(|l| atlas::json::parse(l).unwrap()).find(|v| v["id"] == json!("h2")).unwrap_or_else(|| panic!("no answer h2 in {text}"));
    let said = start["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
    assert!(said.contains("journal:ALPHA:0003"), "{said}");
}

/// The hook's relevance floor defaults to 0.60 (journal:ATLAS-EFFICIENCY:0004):
/// on 14 days of hook data it drops 3,480 pointer rows (about 37k tokens a
/// week) and leaves the memory eval and its hook replay unchanged. An
/// explicit 0 is honoured, not read as unset.
#[test]
fn the_inject_floor_defaults_to_0_60_and_an_explicit_0_stays_0() {
    let db = std::env::temp_dir().join("atlas-floor-default").join("x.sqlite");
    let s = atlas::memory::settings::Settings::from_config(None, &db).unwrap();
    assert_eq!(s.recall.inject_floor, 0.60);
    let base = std::env::temp_dir().join("atlas-floor-default");
    let c = atlas::config::Config::parse(r#"{"recall": {"inject_floor": 0}}"#, &base).unwrap();
    let s = atlas::memory::settings::Settings::from_config(Some(&c), &db).unwrap();
    assert_eq!(s.recall.inject_floor, 0.0);
}
