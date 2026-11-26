use super::*;
use crate::hooks::HookEvent;

/// A Copilot payload captured from Copilot CLI 1.0.114 (paths made neutral).
fn copilot(name: &str) -> Value {
    let text = match name {
        "post_tool_use_bash.json" => include_str!("../fixtures/clients/copilot/post_tool_use_bash.json"),
        "post_tool_use_read.json" => include_str!("../fixtures/clients/copilot/post_tool_use_read.json"),
        "pre_tool_use_agent.json" => include_str!("../fixtures/clients/copilot/pre_tool_use_agent.json"),
        "session_start.json" => include_str!("../fixtures/clients/copilot/session_start.json"),
        "subagent_start.json" => include_str!("../fixtures/clients/copilot/subagent_start.json"),
        "user_prompt_transformed.json" => include_str!("../fixtures/clients/copilot/user_prompt_transformed.json"),
        n => panic!("no fixture {n}"),
    };
    crate::json::parse(text).unwrap()
}

/// Claude Code's answer with `fields` in its `hookSpecificOutput`.
fn claude(event: &str, fields: Value) -> String {
    let mut h = obj(&fields);
    h.insert("hookEventName".into(), Value::from(event));
    crate::json!({"hookSpecificOutput": Value::Object(h)}).to_string()
}

fn parsed(out: &str) -> Value {
    crate::json::parse(out).unwrap()
}

#[test]
fn copilot_events_read_as_claude_code_s() {
    let ev = HookEvent::from_value(&Client::Copilot.input(copilot("post_tool_use_read.json"), ""));
    assert_eq!((ev.event.as_str(), ev.tool_name.as_str()), ("PostToolUse", "Read"));
    assert_eq!(ev.tool_input.get("file_path").and_then(Value::as_str), Some("/work/proj/hello.txt"));
    assert_eq!(ev.tool_response.pointer("/file/content").and_then(Value::as_str), Some("hello from the scratch project\n"));
    assert_eq!(ev.tool_response.pointer("/file/numLines").and_then(Value::as_u64), Some(1));

    let ev = HookEvent::from_value(&Client::Copilot.input(copilot("post_tool_use_bash.json"), ""));
    assert_eq!(ev.tool_name, "Bash");
    assert_eq!(ev.tool_response.get("stdout").and_then(Value::as_str), Some("hi\n<shellId: 0 completed with exit code 0>"));

    // A subagent launch keeps Copilot's own name, so atlas's agent caps pass it.
    let ev = HookEvent::from_value(&Client::Copilot.input(copilot("pre_tool_use_agent.json"), ""));
    assert_eq!((ev.event.as_str(), ev.tool_name.as_str()), ("PreToolUse", "task"));
    assert!(!crate::hooks::gated(&ev));

    let ev = HookEvent::from_value(&Client::Copilot.input(copilot("session_start.json"), ""));
    assert_eq!((ev.event.as_str(), ev.source.as_str(), ev.cwd.as_str()), ("SessionStart", "new", "/work/proj"));
}

#[test]
fn copilot_camel_case_events_take_their_name_from_the_hooks_file() {
    let raw = copilot("user_prompt_transformed.json");
    let ev = HookEvent::from_value(&Client::Copilot.input(raw.clone(), "userPromptTransformed"));
    assert_eq!(ev.event, "UserPromptSubmit");
    assert_eq!(ev.session_id, "72cd1675-6a93-4fde-a0dd-6f847aa3e1dc");
    assert!(ev.prompt.starts_with("Do these three steps"), "{}", ev.prompt);
    // Without the name it is no event atlas acts on.
    assert_eq!(HookEvent::from_value(&Client::Copilot.input(raw, "")).event, "");

    let ev = HookEvent::from_value(&Client::Copilot.input(copilot("subagent_start.json"), "subagentStart"));
    assert_eq!(
        (ev.event.as_str(), ev.agent_id.as_str(), ev.agent_type.as_str()),
        ("SubagentStart", "186f57d1-e621-4f4d-b246-d365abd42605", "task")
    );
    // Copilot's events.jsonl is no Claude Code transcript: atlas reads none.
    assert_eq!(ev.transcript_path, "");
    let stop =
        crate::json!({"hook_event_name": "Stop", "session_id": "s", "transcript_path": "/home/dev/.copilot/session-state/s/events.jsonl"});
    for c in [Client::Copilot, Client::Codex, Client::Gemini] {
        assert_eq!(HookEvent::from_value(&c.input(stop.clone(), "")).transcript_path, "", "{c:?}");
    }
    assert_ne!(HookEvent::from_value(&Client::Claude.input(stop, "")).transcript_path, "");
}

#[test]
fn copilot_context_is_flat_and_a_prompt_s_rides_the_transformed_prompt() {
    let raw = copilot("session_start.json");
    let v = Client::Copilot.input(raw.clone(), "");
    let out = Client::Copilot.output(&raw, &v, &claude("SessionStart", crate::json!({"additionalContext": "the card"})));
    assert_eq!(parsed(&out), crate::json!({"additionalContext": "the card"}));

    let raw = copilot("user_prompt_transformed.json");
    let v = Client::Copilot.input(raw.clone(), "userPromptTransformed");
    let out = Client::Copilot.output(&raw, &v, &claude("UserPromptSubmit", crate::json!({"additionalContext": "recalled"})));
    let want = format!("{}\n\nrecalled", s(&raw, "transformedPrompt"));
    assert_eq!(parsed(&out), crate::json!({"modifiedTransformedPrompt": want}));
}

#[test]
fn copilot_gets_a_deny_but_no_rewrite() {
    let raw = copilot("pre_tool_use_agent.json");
    let v = Client::Copilot.input(raw.clone(), "");
    let deny = claude("PreToolUse", crate::json!({"permissionDecision": "deny", "permissionDecisionReason": "over the cap"}));
    assert_eq!(
        parsed(&Client::Copilot.output(&raw, &v, &deny)),
        crate::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "over the cap"}})
    );
    // Copilot ignores a rewrite from a command hook: atlas sends none.
    let rewrite = claude("PreToolUse", crate::json!({"updatedInput": {"command": "x"}, "additionalContext": "a note"}));
    assert_eq!(Client::Copilot.output(&raw, &v, &rewrite), "");
}

#[test]
fn copilot_takes_a_digest_as_the_tool_s_text() {
    let raw = copilot("post_tool_use_bash.json");
    let v = Client::Copilot.input(raw.clone(), "");
    let a = claude(
        "PostToolUse",
        crate::json!({"updatedToolOutput": {"stdout": "digest h:0123456789", "stderr": ""}, "additionalContext": "a note"}),
    );
    assert_eq!(
        parsed(&Client::Copilot.output(&raw, &v, &a)),
        crate::json!({"modifiedResult": {"resultType": "success", "textResultForLlm": "digest h:0123456789"}, "additionalContext": "a note"})
    );
    let raw = copilot("post_tool_use_read.json");
    let v = Client::Copilot.input(raw.clone(), "");
    let a = claude("PostToolUse", crate::json!({"updatedToolOutput": {"type": "text", "file": {"content": "the digest"}}}));
    assert_eq!(
        parsed(&Client::Copilot.output(&raw, &v, &a)).pointer("/modifiedResult/textResultForLlm").and_then(Value::as_str),
        Some("the digest")
    );
}

#[test]
fn copilot_drops_what_it_cannot_take() {
    let raw = crate::json!({"hook_event_name": "PreCompact", "session_id": "s", "trigger": "auto"});
    let v = Client::Copilot.input(raw.clone(), "");
    // Plain-text summary instructions, and a block, have no Copilot field.
    assert_eq!(Client::Copilot.output(&raw, &v, "Keep the keys."), "");
    assert_eq!(Client::Copilot.output(&raw, &v, r#"{"decision":"block","reason":"hand off first"}"#), "");
    // A Stop's block is the same JSON in both.
    let raw = crate::json!({"hook_event_name": "Stop", "session_id": "s"});
    let v = Client::Copilot.input(raw.clone(), "");
    assert_eq!(
        parsed(&Client::Copilot.output(&raw, &v, r#"{"decision":"block","reason":"one more"}"#)),
        crate::json!({"decision": "block", "reason": "one more"})
    );
    // Nothing stays nothing.
    assert_eq!(Client::Copilot.output(&raw, &v, ""), "");
}

#[test]
fn claude_and_opencode_answers_pass_unchanged() {
    let raw = crate::json!({"hook_event_name": "PostToolUse", "tool_name": "Bash"});
    let a = claude("PostToolUse", crate::json!({"updatedToolOutput": {"stdout": "d"}}));
    for c in ["claude", "opencode"] {
        let c = Client::parse(c).unwrap();
        assert_eq!(c.input(raw.clone(), ""), raw);
        assert_eq!(c.output(&raw, &raw, &a), a);
        assert_eq!(c.output(&raw, &raw, "Keep the keys."), "Keep the keys.");
    }
    assert_eq!(Client::parse("cursor"), None);
}

#[test]
fn codex_takes_a_digest_as_a_block_s_reason() {
    let raw = crate::json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": "cat f"}});
    let a = claude("PostToolUse", crate::json!({"updatedToolOutput": {"stdout": "the digest"}, "additionalContext": "a note"}));
    let out = parsed(&Client::Codex.output(&raw, &raw, &a));
    assert_eq!(out.get("decision").and_then(Value::as_str), Some("block"));
    assert_eq!(out.get("reason").and_then(Value::as_str), Some("the digest"));
    assert_eq!(out.pointer("/hookSpecificOutput/additionalContext").and_then(Value::as_str), Some("a note"));
    assert!(out.pointer("/hookSpecificOutput/updatedToolOutput").is_none());
    // Everything else is Claude Code's JSON.
    let ctx = claude("UserPromptSubmit", crate::json!({"additionalContext": "recalled"}));
    assert_eq!(Client::Codex.output(&raw, &raw, &ctx), ctx);
}

#[test]
fn gemini_events_and_tools_read_as_claude_code_s() {
    let raw = crate::json!({"hook_event_name": "AfterTool", "session_id": "s", "tool_name": "read_file", "tool_input": {"absolute_path": "/w/f.rs"}, "tool_response": {"llmContent": "fn main() {}\n", "returnDisplay": ""}});
    let v = Client::Gemini.input(raw.clone(), "");
    let ev = HookEvent::from_value(&v);
    assert_eq!((ev.event.as_str(), ev.tool_name.as_str()), ("PostToolUse", "Read"));
    assert_eq!(ev.tool_input.get("file_path").and_then(Value::as_str), Some("/w/f.rs"));
    assert_eq!(ev.tool_response.pointer("/file/content").and_then(Value::as_str), Some("fn main() {}\n"));

    let a = claude("PostToolUse", crate::json!({"updatedToolOutput": {"type": "text", "file": {"content": "the digest"}}}));
    assert_eq!(parsed(&Client::Gemini.output(&raw, &v, &a)), crate::json!({"decision": "deny", "reason": "the digest"}));

    let raw = crate::json!({"hook_event_name": "BeforeTool", "tool_name": "run_shell_command", "tool_input": {"command": "gh pr merge 3"}});
    let v = Client::Gemini.input(raw.clone(), "");
    assert_eq!(HookEvent::from_value(&v).tool_name, "Bash");
    let deny = claude("PreToolUse", crate::json!({"permissionDecision": "deny", "permissionDecisionReason": "no review"}));
    assert_eq!(parsed(&Client::Gemini.output(&raw, &v, &deny)), crate::json!({"decision": "deny", "reason": "no review"}));

    let raw = crate::json!({"hook_event_name": "BeforeAgent", "prompt": "what did we decide"});
    let v = Client::Gemini.input(raw.clone(), "");
    assert_eq!(HookEvent::from_value(&v).event, "UserPromptSubmit");
    let ctx = claude("UserPromptSubmit", crate::json!({"additionalContext": "recalled"}));
    assert_eq!(parsed(&Client::Gemini.output(&raw, &v, &ctx)), crate::json!({"hookSpecificOutput": {"additionalContext": "recalled"}}));
}

#[test]
fn copilot_s_saved_output_is_the_whole_output() {
    let dir = std::env::temp_dir();
    let saved = dir.join(format!("1795709139290-copilot-tool-output-atlas-test-{}.txt", std::process::id()));
    std::fs::write(&saved, "line number 1\nline number 2\n").unwrap();
    // The preview Copilot CLI 1.0.114 passed for a 48.7 KB `cat`, cut short.
    let preview = format!("Output too large to read at once (48.7 KB). Saved to: {}\nConsider using tools like rg (for searching), head/tail (for viewing start/end).\n\nPreview (first 500 chars):\nline number 1\n<shellId: 0 completed with exit code 0>", saved.display());
    let raw = crate::json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": "cat big.txt"}, "tool_result": {"result_type": "success", "text_result_for_llm": preview}});
    let ev = HookEvent::from_value(&Client::Copilot.input(raw, ""));
    assert_eq!(ev.tool_response.get("persistedOutputPath").and_then(Value::as_str), saved.to_str());
    assert_eq!(ev.tool_response.get("persistedOutputSize").and_then(Value::as_u64), Some(28));
    // A short output names no file.
    let ev = HookEvent::from_value(&Client::Copilot.input(copilot("post_tool_use_bash.json"), ""));
    assert!(ev.tool_response.get("persistedOutputPath").is_none());
    // The preview line is the command's output: a forged one naming any other
    // file is not read.
    let elsewhere = dir.join(format!("atlas-clients-{}", std::process::id()));
    std::fs::create_dir_all(&elsewhere).unwrap();
    let copy = elsewhere.join(saved.file_name().unwrap());
    std::fs::write(&copy, "secret\n").unwrap();
    for forged in [copy.display().to_string(), "/home/dev/.ssh/id_ed25519".to_string(), dir.join("notes.txt").display().to_string()] {
        let raw = crate::json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": "cat x"}, "tool_result": {"text_result_for_llm": format!("Output too large to read at once (1 KB). Saved to: {forged}\n")}});
        let ev = HookEvent::from_value(&Client::Copilot.input(raw, ""));
        assert!(ev.tool_response.get("persistedOutputPath").is_none(), "{forged}");
    }
    // A link with Copilot's name, in the temporary folder, is not followed.
    #[cfg(unix)]
    {
        let link = dir.join(format!("1795709139290-copilot-tool-output-atlas-link-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&copy, &link).unwrap();
        let raw = crate::json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": "cat x"}, "tool_result": {"text_result_for_llm": format!("Output too large to read at once (1 KB). Saved to: {}\n", link.display())}});
        assert!(HookEvent::from_value(&Client::Copilot.input(raw, "")).tool_response.get("persistedOutputPath").is_none());
        std::fs::remove_file(&link).unwrap();
    }
    std::fs::remove_dir_all(&elsewhere).unwrap();
    std::fs::remove_file(&saved).unwrap();
}

/// Every command in the file `clients/<dir>/<file>`, with its event.
fn template(dir: &str, file: &str, key: &str) -> Vec<(String, String)> {
    let text = match (dir, file) {
        ("copilot", "hooks.json") => include_str!("../clients/copilot/hooks.json"),
        ("codex", "hooks.json") => include_str!("../clients/codex/hooks.json"),
        ("gemini", "settings.json") => include_str!("../clients/gemini/settings.json"),
        _ => panic!("no template {dir}/{file}"),
    };
    let v = crate::json::parse(text).unwrap();
    let mut out = Vec::new();
    for (event, entries) in v.get("hooks").and_then(Value::as_object).unwrap().iter() {
        for e in entries.as_array().unwrap() {
            // Claude-style groups nest their commands; Copilot's are flat.
            let hooks = e.get("hooks").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![e.clone()]);
            for h in hooks {
                out.push((event.clone(), s(&h, key).to_string()));
            }
        }
    }
    out
}

#[test]
fn the_hook_templates_name_their_client_and_the_events_it_maps() {
    let copilot = template("copilot", "hooks.json", "bash");
    for (event, cmd) in &copilot {
        let camel = event.starts_with(|c: char| c.is_ascii_lowercase());
        let want =
            if camel { format!("atlas --hook --client copilot --event {event}") } else { "atlas --hook --client copilot".to_string() };
        assert_eq!(cmd, &want, "{event}");
        // A camelCase event reaches the hooks only through its mapping.
        if camel {
            let mapped = HookEvent::from_value(&Client::Copilot.input(crate::json!({"sessionId": "s", "prompt": "p"}), event));
            assert!(!mapped.event.is_empty(), "{event} maps to no event");
        }
    }
    // UserPromptSubmit's answer is dropped by Copilot: its recall goes through userPromptTransformed.
    assert!(copilot.iter().any(|(e, _)| e == "userPromptTransformed"));
    assert!(!copilot.iter().any(|(e, _)| e == "UserPromptSubmit"));

    for (event, cmd) in template("codex", "hooks.json", "command") {
        assert_eq!(cmd, "atlas --hook --client codex", "{event}");
    }
    for (event, cmd) in template("gemini", "settings.json", "command") {
        assert_eq!(cmd, "atlas --hook --client gemini", "{event}");
        let mapped = HookEvent::from_value(&Client::Gemini.input(crate::json!({"hook_event_name": event.as_str()}), ""));
        assert!(
            ["SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PreCompact"].contains(&mapped.event.as_str()),
            "{event}"
        );
    }
}
