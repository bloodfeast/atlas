//! #416 measure: what one subagent tool call's gate check costs with a long
//! parent transcript and several agents at once. Ignored by default (it
//! writes hundreds of MB); run with
//! `cargo test --test gate_timing -- --ignored --nocapture`.
//! Sizes: `GATE_PARENT_MB` (300), `GATE_AGENT_MB` (20), `GATE_AGENTS` (6),
//! `GATE_ROUNDS` (40).

mod common;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use atlas::hooks::{HookEvent, Hooks};
use atlas::json;
use common::*;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// A transcript of about `mb` MB: user, tool use and assistant lines with
/// usage, as Claude Code writes them, the first line a brief.
fn transcript(path: &Path, mb: usize, brief: &str, sidechain: bool) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let side = if sidechain { "true" } else { "false" };
    writeln!(f, "{}", json!({"type": "user", "isSidechain": sidechain, "timestamp": "2026-11-23T12:00:00.000Z", "message": {"role": "user", "content": brief}})).unwrap();
    let pad = "x".repeat(1_800);
    let target = (mb as u64) << 20;
    let mut n = 0u64;
    let mut written = 0u64;
    while written < target {
        n += 1;
        let line = format!(
            "{{\"type\":\"assistant\",\"isSidechain\":{side},\"timestamp\":\"2026-11-23T12:00:00.000Z\",\"message\":{{\"id\":\"m{n}\",\"model\":\"claude-opus-5-5\",\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"t{n}\",\"name\":\"Read\",\"input\":{{\"file_path\":\"/src/a{n}.rs\"}}}}],\"usage\":{{\"input_tokens\":3,\"cache_creation_input_tokens\":500,\"cache_read_input_tokens\":{},\"output_tokens\":40}}}}}}\n{{\"type\":\"user\",\"isSidechain\":{side},\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t{n}\",\"content\":\"{pad}\"}}]}}}}\n",
            20_000 + n % 1000
        );
        written += line.len() as u64;
        f.write_all(line.as_bytes()).unwrap();
    }
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() - 1) as f64 * p).round() as usize]
}

#[test]
#[ignore]
fn subagent_gate_check_cost() {
    let (parent_mb, agent_mb, agents, rounds) = (env("GATE_PARENT_MB", 300), env("GATE_AGENT_MB", 20), env("GATE_AGENTS", 6), env("GATE_ROUNDS", 40));
    let t = Temp::new("gate-timing");
    let mut s = atlas::memory::settings::Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.hook_budget = Duration::from_millis(atlas::hook_budget::MAX_MS);
    let spec = atlas::orchestrate::OrchestrateSpec { enabled: Some(true), ..Default::default() };
    s.orchestrate = atlas::orchestrate::resolve(Some(&spec), None, None, |_| None);
    let parent = t.path().join("proj").join("sess.jsonl");
    let t0 = Instant::now();
    transcript(&parent, parent_mb, "orchestrate the widgets", false);
    let tp = parent.to_string_lossy().into_owned();
    let ids: Vec<String> = (0..agents).map(|i| format!("a{i:015x}")).collect();
    for id in &ids {
        let p = atlas::hooks::subagent_transcript(&tp, id);
        transcript(&p, agent_mb, "Kind: builder\nLane: WIDGETS\nBuild the widget cache.", true);
        std::fs::write(p.with_extension("meta.json"), json!({"agentType": "builder", "toolUseId": format!("toolu_{id}")}).to_string()).unwrap();
    }
    eprintln!("fixture: parent {parent_mb} MB, {agents} agents x {agent_mb} MB, written in {:?}", t0.elapsed());
    let mut h = Hooks::open(s).unwrap();
    let ev = |event: &str, id: &str, tool: &str, n: usize| HookEvent {
        event: event.into(),
        session_id: "S".into(),
        agent_id: id.into(),
        agent_type: "builder".into(),
        transcript_path: tp.clone(),
        tool_name: tool.into(),
        tool_use_id: format!("toolu_{id}_{n}"),
        cwd: t.path().to_string_lossy().into_owned(),
        tool_input: if tool == "Bash" { json!({"command": "echo ok"}) } else { json!({"file_path": "/src/a.rs"}) },
        tool_response: if tool == "Bash" { json!({"stdout": "ok", "stderr": ""}) } else { atlas::json::Value::Null },
        ..HookEvent::default()
    };
    let mut first = Vec::new();
    let mut pre = Vec::new();
    let mut post = Vec::new();
    let mut after = Vec::new();
    let time = |h: &mut Hooks, e: &HookEvent| {
        let t = Instant::now();
        h.handle(e);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        h.after();
        (ms, t.elapsed().as_secs_f64() * 1e3)
    };
    for id in &ids {
        time(&mut h, &ev("SubagentStart", id, "", 0));
    }
    for r in 0..rounds {
        for id in &ids {
            let tool = if r % 2 == 0 { "Read" } else { "Bash" };
            let (ms, a) = time(&mut h, &ev("PreToolUse", id, tool, r));
            if r == 0 { first.push(ms) } else { pre.push(ms) }
            after.push(a);
            // The agent's transcript grows between calls, as a live one does.
            let p = atlas::hooks::subagent_transcript(&tp, id);
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            writeln!(f, "{{\"type\":\"assistant\",\"isSidechain\":true,\"message\":{{\"id\":\"r{r}\",\"model\":\"claude-opus-5-5\",\"usage\":{{\"input_tokens\":3,\"cache_creation_input_tokens\":500,\"cache_read_input_tokens\":30000,\"output_tokens\":40}}}}}}").unwrap();
            let (ms, a) = time(&mut h, &ev("PostToolUse", id, tool, r));
            post.push(ms);
            after.push(a);
        }
    }
    let line = |name: &str, v: &mut Vec<f64>| {
        let n = v.len();
        let (p50, p95) = (pct(v, 0.5), pct(v, 0.95));
        eprintln!("{name}: n={n} p50={p50:.1} ms p95={p95:.1} ms max={:.1} ms", v.last().copied().unwrap_or(0.0));
    };
    line("first PreToolUse", &mut first);
    line("PreToolUse", &mut pre);
    line("PostToolUse", &mut post);
    line("after()", &mut after);
}

/// The same session's events while another server holds the memory store's
/// write lock in bursts (`GATE_HOLD_MS` of every `GATE_HOLD_MS` + 100 ms,
/// default 400), as several sessions' servers writing one store do: each
/// event's time and its slowest phase, the slow ones listed.
#[test]
#[ignore]
fn subagent_gate_check_under_store_contention() {
    let (agents, rounds, hold) = (env("GATE_AGENTS", 6), env("GATE_ROUNDS", 10), env("GATE_HOLD_MS", 400) as u64);
    let t = Temp::new("gate-contention");
    let mut s = atlas::memory::settings::Settings::from_config(None, &t.path().join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.hook_budget = Duration::from_millis(atlas::hook_budget::DEFAULT_MS);
    let spec = atlas::orchestrate::OrchestrateSpec { enabled: Some(true), ..Default::default() };
    s.orchestrate = atlas::orchestrate::resolve(Some(&spec), None, None, |_| None);
    let parent = t.path().join("proj").join("sess.jsonl");
    transcript(&parent, 1, "orchestrate the widgets", false);
    let tp = parent.to_string_lossy().into_owned();
    let store = s.store.clone();
    let mut h = Hooks::open(s).unwrap();
    let ev = |event: &str, id: &str, tool: &str, n: usize, input: atlas::json::Value| HookEvent {
        event: event.into(),
        session_id: "S".into(),
        agent_id: id.into(),
        agent_type: "builder".into(),
        transcript_path: tp.clone(),
        tool_name: tool.into(),
        tool_use_id: format!("toolu_{id}_{n}"),
        tool_prompt: input.get("prompt").and_then(atlas::json::Value::as_str).unwrap_or("").to_string(),
        cwd: t.path().to_string_lossy().into_owned(),
        tool_input: input,
        ..HookEvent::default()
    };
    let brief = "Kind: builder\nLane: WIDGETS\nBuild the widget cache.";
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let holder = {
        let (stop, store) = (stop.clone(), store.clone());
        std::thread::spawn(move || {
            let c = atlas::db::Connection::open(&store).unwrap();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if c.execute_batch("BEGIN IMMEDIATE").is_ok() {
                    std::thread::sleep(Duration::from_millis(hold));
                    let _ = c.execute_batch("COMMIT");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let mut by: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
    let mut slow = Vec::new();
    let mut run = |h: &mut Hooks, name: &str, e: HookEvent| {
        let t0 = Instant::now();
        h.handle(&e);
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        let worst = h.budget_phases().iter().max_by_key(|p| p.1).map(|(p, d)| format!("{p}={:.0}", d.as_secs_f64() * 1e3)).unwrap_or_default();
        let t1 = Instant::now();
        h.after();
        let after = t1.elapsed().as_secs_f64() * 1e3;
        by.entry(name.to_string()).or_default().push(ms);
        by.entry("after()".to_string()).or_default().push(after);
        if ms > 250.0 || after > 250.0 {
            slow.push(format!("{name}: {ms:.0} ms (slowest {worst}), after() {after:.0} ms"));
        }
    };
    for r in 0..rounds {
        for i in 0..agents {
            let id = format!("a{r:03}{i:012x}");
            if r == 0 {
                run(&mut h, "PreToolUse Agent (main)", ev("PreToolUse", "", "Agent", i, json!({"prompt": brief, "subagent_type": "builder", "description": "w"})));
                let p = atlas::hooks::subagent_transcript(&tp, &id);
                transcript(&p, 1, brief, true);
                std::fs::write(p.with_extension("meta.json"), json!({"agentType": "builder", "toolUseId": format!("toolu__{i}")}).to_string()).unwrap();
                run(&mut h, "SubagentStart", ev("SubagentStart", &id, "", 0, atlas::json::Value::Null));
            }
        }
        for i in 0..agents {
            let id = format!("a000{i:012x}");
            run(&mut h, "PreToolUse Read (sub)", ev("PreToolUse", &id, "Read", r, json!({"file_path": "/src/a.rs"})));
            run(&mut h, "PreToolUse Bash (sub)", ev("PreToolUse", &id, "Bash", r, json!({"command": "echo ok"})));
            run(&mut h, "PostToolUse Bash (sub)", ev("PostToolUse", &id, "Bash", r, json!({"command": "echo ok"})));
            if r == rounds - 1 {
                run(&mut h, "PreToolUse journal (sub)", ev("PreToolUse", &id, "mcp__atlas__journal", r, json!({"lane": "WIDGETS", "entry": "note", "text": "x"})));
                run(&mut h, "SubagentStop", ev("SubagentStop", &id, "", r, atlas::json::Value::Null));
            }
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    holder.join().unwrap();
    eprintln!("store lock held {hold} ms of every {} ms", hold + 100);
    for (name, v) in by.iter_mut() {
        let n = v.len();
        let (p50, p95) = (pct(v, 0.5), pct(v, 0.95));
        eprintln!("{name}: n={n} p50={p50:.1} ms p95={p95:.1} ms max={:.1} ms", v.last().copied().unwrap_or(0.0));
    }
    for l in slow.iter().take(30) {
        eprintln!("slow {l}");
    }
}
