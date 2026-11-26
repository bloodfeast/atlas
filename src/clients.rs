//! Hook clients other than Claude Code: their event JSON into Claude Code's
//! shape, which [`crate::hooks::HookEvent::from_value`] reads, and atlas's
//! answer (Claude Code's hook JSON) back into theirs.
//!
//! `atlas --hook --client <name>` uses it. The hook logic never sees which
//! client asked: only these two edges differ.
//!
//! What each client honours was measured, not read: GitHub Copilot CLI
//! 1.0.114 (`docs/design/clients.md`, section 2). Codex and Gemini CLI follow
//! their documentation only and are marked so there.

use crate::json::{Map, Value};

/// A hook client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Client {
    /// Claude Code.
    #[default]
    Claude,
    /// OpenCode, through the plugin under `clients/opencode`, which speaks
    /// Claude Code's hook JSON.
    OpenCode,
    /// GitHub Copilot: its CLI, VS Code and the app share one hook runtime.
    /// Events in PascalCase, except `userPromptTransformed` and
    /// `subagentStart`, which exist only in camelCase.
    Copilot,
    /// OpenAI Codex.
    Codex,
    /// Gemini CLI.
    Gemini,
}

impl Client {
    /// The client named `name` (`claude`, `opencode`, `copilot`, `codex`,
    /// `gemini`).
    pub fn parse(name: &str) -> Option<Client> {
        Some(match name {
            "claude" => Client::Claude,
            "opencode" => Client::OpenCode,
            "copilot" => Client::Copilot,
            "codex" => Client::Codex,
            "gemini" => Client::Gemini,
            _ => return None,
        })
    }

    /// The client's hook input `v` in Claude Code's shape. `event` is the
    /// event the hooks file named with `--event`, for a client whose input
    /// does not name it (Copilot's camelCase events).
    ///
    /// Another client's `transcript_path` is dropped: atlas reads a transcript
    /// as Claude Code writes it (the context size, the checkpoint), and a
    /// misread size could deny tools past the hard limit.
    pub fn input(self, v: Value, event: &str) -> Value {
        let mut v = match self {
            Client::Claude | Client::OpenCode => return v,
            Client::Codex => v,
            Client::Copilot => copilot_input(v, event),
            Client::Gemini => gemini_input(v),
        };
        if let Some(m) = v.as_object_mut() {
            m.remove("transcript_path");
        }
        v
    }

    /// atlas's answer `out` (Claude Code's hook JSON, PreCompact's plain
    /// instructions, or empty for nothing) to the event `ev`, whose input as
    /// the client sent it is `raw`, in the client's shape.
    pub fn output(self, raw: &Value, ev: &Value, out: &str) -> String {
        if out.is_empty() || matches!(self, Client::Claude | Client::OpenCode) {
            return out.to_string();
        }
        let event = s(ev, "hook_event_name");
        let Ok(a) = crate::json::parse(out) else {
            // PreCompact's plain-text instructions: no other client adds a
            // hook's text to its compaction summary.
            return String::new();
        };
        match self {
            Client::Claude | Client::OpenCode => out.to_string(),
            Client::Copilot => copilot_output(raw, ev, event, &a),
            Client::Codex => codex_output(ev, event, &a, out),
            Client::Gemini => gemini_output(ev, event, &a),
        }
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn obj(v: &Value) -> Map {
    v.as_object().cloned().unwrap_or_default()
}

/// A tool result's text as Claude Code's built-in tool would give it, for
/// the tools whose results atlas digests: `Bash` as stdout, `Read` as a text
/// file read whole. Any other tool keeps `{text}`.
fn claude_response(tool: &str, input: &Value, text: &str) -> Value {
    match tool {
        "Bash" => {
            let mut r = crate::json!({"stdout": text, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false});
            // Copilot keeps an over-long output in a file and passes a
            // preview naming it, as Claude Code's `persistedOutputPath` does.
            if let (Some(p), Some(m)) = (saved_output(text), r.as_object_mut()) {
                let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                m.insert("persistedOutputPath".into(), Value::from(p));
                m.insert("persistedOutputSize".into(), Value::from(size));
            }
            r
        }
        "Read" => {
            let lines = text.lines().count() as u64;
            crate::json!({"type": "text", "file": {"filePath": s(input, "file_path"), "content": text, "numLines": lines, "startLine": 1, "totalLines": lines}})
        }
        _ => crate::json!({"text": text}),
    }
}

/// The file Copilot saved an over-long output to, from the first line of the
/// preview it passes instead: "Output too large to read at once (48.7 KB).
/// Saved to: /tmp/…-copilot-tool-output-….txt" (Copilot CLI 1.0.114).
fn saved_output(text: &str) -> Option<&str> {
    let first = text.lines().next()?;
    if !first.starts_with("Output too large") {
        return None;
    }
    let p = first.split_once("Saved to: ")?.1.trim();
    // The line is the tool's own output, which a command can forge: only a
    // file Copilot names so, directly in the temporary folder, is read.
    let path = std::path::Path::new(p);
    let name = path.file_name()?.to_str()?;
    let ours = name.contains("-copilot-tool-output-") && name.ends_with(".txt") && path.parent() == Some(std::env::temp_dir().as_path());
    // Nor through a link: a planted one would pass the name.
    let ours = ours && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
    ours.then_some(p)
}

/// The text of a replacement tool output atlas wrote in a Claude tool's
/// shape: `Bash`'s stdout, `Read`'s file content, or a plain string.
fn replacement_text(tool: &str, u: &Value) -> Option<String> {
    match (tool, u) {
        (_, Value::String(t)) => Some(t.clone()),
        ("Bash", _) => u.get("stdout").and_then(Value::as_str).map(str::to_string),
        ("Read", _) => u.pointer("/file/content").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// Claude Code's `hookSpecificOutput` of an answer, or an empty object.
fn specific(a: &Value) -> Map {
    a.get("hookSpecificOutput").map(obj).unwrap_or_default()
}

/// Copilot's PascalCase input is Claude Code's already, tool names included
/// (`view` arrives as `Read`, `bash` as `Bash`, `task` as `Agent`); a
/// `Read` names its file `path`, and a result is `tool_result`. The two
/// camelCase events atlas registers are renamed into Claude's.
fn copilot_input(v: Value, event: &str) -> Value {
    let mut m = obj(&v);
    if !m.contains_key("hook_event_name") {
        // A camelCase event: its payload names no event, so the hooks file
        // passes it (`--event`, see clients/copilot/hooks.json).
        let mut c = Map::new();
        let get = |k: &str| Value::from(s(&v, k));
        match event {
            "userPromptTransformed" => {
                c.insert("hook_event_name".into(), Value::from("UserPromptSubmit"));
                c.insert("prompt".into(), get("prompt"));
            }
            "subagentStart" => {
                c.insert("hook_event_name".into(), Value::from("SubagentStart"));
                c.insert("transcript_path".into(), get("transcriptPath"));
            }
            _ => return v,
        }
        c.insert("session_id".into(), get("sessionId"));
        c.insert("cwd".into(), get("cwd"));
        c.insert("agent_id".into(), get("agentId"));
        c.insert("agent_type".into(), get("agentType"));
        return Value::Object(c);
    }
    let tool = s(&v, "tool_name").to_string();
    // Copilot's `task` arrives as `Agent`. atlas's agent caps free a slot at
    // SubagentStop, which Copilot's built-in general-purpose agent never
    // sends: under its own name the launch passes the caps.
    if tool == "Agent" {
        m.insert("tool_name".into(), Value::from("task"));
    }
    if tool == "Read" {
        if let Some(i) = m.get_mut("tool_input").and_then(Value::as_object_mut) {
            if !i.contains_key("file_path") {
                let p = i.get("path").cloned().unwrap_or(Value::Null);
                i.insert("file_path".into(), p);
            }
        }
    }
    if let Some(r) = m.remove("tool_result") {
        let input = m.get("tool_input").cloned().unwrap_or(Value::Null);
        let text = s(&r, "text_result_for_llm");
        m.insert("tool_response".into(), claude_response(&tool, &input, text));
    }
    Value::Object(m)
}

/// atlas's answer for Copilot. Measured: Copilot reads a flat
/// `additionalContext` (SessionStart, SubagentStart, PostToolUse), a flat
/// `modifiedResult`, a nested `permissionDecision`, and
/// `modifiedTransformedPrompt` from `userPromptTransformed`; it ignores
/// Claude's nested `additionalContext`, any input rewrite, and
/// `UserPromptSubmit`'s output.
fn copilot_output(raw: &Value, ev: &Value, event: &str, a: &Value) -> String {
    if a.get("decision").and_then(Value::as_str) == Some("block") {
        // Stop and SubagentStop take the same fields; Copilot has no block
        // for PreCompact.
        return if event == "PreCompact" { String::new() } else { a.to_string() };
    }
    let h = specific(a);
    let context = h.get("additionalContext").and_then(Value::as_str).unwrap_or("");
    let mut out = Map::new();
    match event {
        "SessionStart" | "SubagentStart" if !context.is_empty() => {
            out.insert("additionalContext".into(), Value::from(context));
        }
        "UserPromptSubmit" if !context.is_empty() => {
            let shown = match s(raw, "transformedPrompt") {
                "" => s(raw, "prompt"),
                t => t,
            };
            out.insert("modifiedTransformedPrompt".into(), Value::from(format!("{shown}\n\n{context}")));
        }
        "PreToolUse" => {
            // The nested deny is the shape measured to stop the call.
            if let Some(r) = h.get("permissionDecisionReason").filter(|_| h.get("permissionDecision").and_then(Value::as_str) == Some("deny")) {
                out.insert("hookSpecificOutput".into(), crate::json!({"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": r.clone()}));
            }
        }
        "PostToolUse" => {
            if let Some(t) = h.get("updatedToolOutput").and_then(|u| replacement_text(s(ev, "tool_name"), u)) {
                out.insert("modifiedResult".into(), crate::json!({"resultType": "success", "textResultForLlm": t}));
            }
            if !context.is_empty() {
                out.insert("additionalContext".into(), Value::from(context));
            }
        }
        _ => {}
    }
    if out.is_empty() {
        String::new()
    } else {
        Value::Object(out).to_string()
    }
}

/// atlas's answer for Codex, from its documentation (not run here): Claude
/// Code's JSON, except that a replaced tool result is a PostToolUse
/// `decision: block` whose `reason` takes the result's place
/// (`updatedMCPToolOutput` is not supported).
fn codex_output(ev: &Value, event: &str, a: &Value, out: &str) -> String {
    let mut h = specific(a);
    let Some(u) = h.remove("updatedToolOutput").filter(|_| event == "PostToolUse") else {
        return out.to_string();
    };
    let Some(t) = replacement_text(s(ev, "tool_name"), &u) else {
        return out.to_string();
    };
    let mut m = obj(a);
    m.insert("decision".into(), Value::from("block"));
    m.insert("reason".into(), Value::from(t));
    if h.len() > 1 {
        m.insert("hookSpecificOutput".into(), Value::Object(h));
    } else {
        m.remove("hookSpecificOutput");
    }
    Value::Object(m).to_string()
}

/// Gemini CLI's tool names as Claude Code's, for the tools atlas acts on.
fn gemini_tool(name: &str) -> &str {
    match name {
        "run_shell_command" => "Bash",
        "read_file" => "Read",
        "search_file_content" | "grep" => "Grep",
        "glob" => "Glob",
        "replace" => "Edit",
        "write_file" => "Write",
        n => n,
    }
}

/// Gemini CLI's input, from its documentation (not run here): its event
/// names and tool names as Claude Code's; `read_file`'s `absolute_path` as
/// `file_path`; a result's `llmContent` as the tool's text.
fn gemini_input(v: Value) -> Value {
    let mut m = obj(&v);
    let event = match s(&v, "hook_event_name") {
        "BeforeTool" => "PreToolUse",
        "AfterTool" => "PostToolUse",
        "BeforeAgent" => "UserPromptSubmit",
        "PreCompress" => "PreCompact",
        e => e,
    }
    .to_string();
    m.insert("hook_event_name".into(), Value::from(event));
    let tool = gemini_tool(s(&v, "tool_name")).to_string();
    if !tool.is_empty() {
        m.insert("tool_name".into(), Value::from(tool.as_str()));
    }
    if tool == "Read" {
        if let Some(i) = m.get_mut("tool_input").and_then(Value::as_object_mut) {
            if let Some(p) = i.get("absolute_path").cloned() {
                i.insert("file_path".into(), p);
            }
        }
    }
    if let Some(r) = m.remove("tool_response") {
        let input = m.get("tool_input").cloned().unwrap_or(Value::Null);
        let text = match r.get("llmContent") {
            Some(Value::String(t)) => t.clone(),
            Some(x) => x.to_string(),
            None => String::new(),
        };
        m.insert("tool_response".into(), claude_response(&tool, &input, &text));
    }
    Value::Object(m)
}

/// atlas's answer for Gemini CLI, from its documentation (not run here): a
/// deny is a top-level `decision: deny` with its `reason`; an AfterTool
/// `decision: deny` puts its `reason` in place of the result; context is
/// `hookSpecificOutput.additionalContext`.
fn gemini_output(ev: &Value, event: &str, a: &Value) -> String {
    if a.get("decision").and_then(Value::as_str) == Some("block") {
        return String::new();
    }
    let h = specific(a);
    let mut out = Map::new();
    if let Some(r) = h.get("permissionDecisionReason").filter(|_| h.get("permissionDecision").and_then(Value::as_str) == Some("deny")) {
        out.insert("decision".into(), Value::from("deny"));
        out.insert("reason".into(), r.clone());
    }
    if event == "PostToolUse" {
        if let Some(t) = h.get("updatedToolOutput").and_then(|u| replacement_text(s(ev, "tool_name"), u)) {
            out.insert("decision".into(), Value::from("deny"));
            out.insert("reason".into(), Value::from(t));
        }
    }
    if let Some(c) = h.get("additionalContext").filter(|c| c.as_str().is_some_and(|c| !c.is_empty())) {
        if event != "PreCompact" {
            out.insert("hookSpecificOutput".into(), crate::json!({"additionalContext": c.clone()}));
        }
    }
    if let Some(m) = a.get("systemMessage") {
        out.insert("systemMessage".into(), m.clone());
    }
    if out.is_empty() {
        String::new()
    } else {
        Value::Object(out).to_string()
    }
}

#[cfg(test)]
#[path = "clients_tests.rs"]
mod tests;
