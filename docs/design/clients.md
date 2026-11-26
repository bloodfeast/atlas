# Hook clients other than Claude Code

How atlas's hooks run under GitHub Copilot, OpenCode, Codex and Gemini CLI. The user's guide is the wiki page
Other clients; this is the design and what it rests on.

## 1. The shape of it

- `atlas::hooks` stays the one place hook logic lives: `Hooks::handle` takes a `HookEvent` and returns a
  `HookAnswer`. It never learns which client asked.
- `atlas::clients` is the edge. `Client::input` turns the client's event JSON into Claude Code's, which
  `HookEvent::from_value` reads; `Client::output` turns atlas's answer (Claude Code's hook JSON, PreCompact's
  plain text, or nothing) into the client's.
- `atlas --hook --client NAME [--event NAME]` applies both around the command hook, including the refusal and
  the overrun answers. `--event` names an event whose input does not (Copilot's camelCase events).
- `Hooks::set_other_client` drops what only Claude Code's own files carry: the Claude Code settings offer, the
  status line repair and the hooks' drift check, and the orchestrated-mode offer.
- Another client's `transcript_path` is dropped on the way in: atlas reads transcripts as Claude Code writes
  them, and a misread context size could deny tools past the hard limit. The context cap is off there.
- Agent caps count Claude Code's launches only. A cap's slot is freed at SubagentStop, which Copilot's built-in
  general-purpose agent never sends; Copilot's `task` keeps its own name, and the OpenCode plugin does not map
  `task`, so the caps pass them.
- A failure of atlas itself exits 0 for another client (`other_client_hook` in `main.rs`): Copilot denies a
  tool call whose PreToolUse command hook exits non-zero.
- The OpenCode plugin spawns atlas without blocking (OpenCode's server runs every session's hooks) and does
  not apply atlas's Bash rewrite, which only appends an exit-code marker OpenCode reports itself.
- OpenCode's hooks are code, not commands: `clients/opencode/atlas/index.ts` builds Claude-shaped events and
  reads Claude-shaped answers, so its `Client` changes no JSON.
- No new transport. Every event is its own `atlas --hook` process, as a Claude Code command hook always could
  be; the one loss documented for that (`Memory-store-and-hooks.md`) is a subagent's lane recall at
  SubagentStart. Whether a server-side transport is worth adding waits on a measured loss: the in-memory hook
  state a fresh process lacks, and the embedding model's load per event against `hooks.budget_ms`.

## 2. What each client honours

Measured on Copilot CLI 1.0.114 and OpenCode 2.4.1 with probe hooks that answer in each shape and ask the model
what reached it. Codex and Gemini CLI follow their documentation only.

| | Copilot | OpenCode | Codex (docs) | Gemini CLI (docs) |
|---|---|---|---|---|
| Input | PascalCase events in Claude Code's shape and tool names (`view` as `Read`, `bash` as `Bash`, `task` as `Agent`); Read names `path`; result in `tool_result.text_result_for_llm` | plugin hooks `session` `prompt`, `context`, `compaction`; `tool` `execute.before`, `execute.after` | Claude Code's | own event and tool names, `tool_response.llmContent` |
| Session context | flat `additionalContext` (nested ignored) | `context` hook: add to the prompt's message in each model call | `additionalContext` | `hookSpecificOutput.additionalContext` |
| Prompt context | UserPromptSubmit output dropped; `userPromptTransformed` `modifiedTransformedPrompt` works | the same (a `prompt` hook's edit is stored and shown as the user's words) | `additionalContext` | BeforeAgent `additionalContext` |
| Deny | nested `permissionDecision` (flat not tried) | throw in `execute.before` | `permissionDecision` | `decision: deny` |
| Input rewrite | ignored (nested `updatedInput` and flat `modifiedArgs`) | replace `input` | `updatedInput` | `hookSpecificOutput.tool_input` |
| Replace a result | flat `modifiedResult` | replace `result.content` | PostToolUse `decision: block`, its reason replaces the result | AfterTool `decision: deny`, its reason replaces the result |
| Compaction | no output | `compaction` hook | stop only | advisory |

Cursor and Grok Build were left at MCP only: neither can replace a built-in tool's result from a hook.

## 3. Long outputs

Both measured clients keep a long shell output in a file and pass the model a part of it:

- **Copilot** passes a preview whose first line reads `Output too large to read at once (48.7 KB). Saved to:
  /tmp/<ms>-copilot-tool-output-<id>.txt`. That line is the command's own output as far as the hook can tell,
  so a command could forge it: `saved_output` reads only a regular file named so, directly in the temporary
  folder, never through a link.
- **OpenCode** cuts the output to its last 2,000 lines and names the whole in `result.output.outputPath`, a
  field the shell tool sets, with `metadata.truncated`.

Either path becomes Claude Code's `persistedOutputPath`, which `hook_rewrite` already reads whole.

## 4. Tests

- `src/clients_tests.rs`: Copilot's captured payloads (`fixtures/clients/copilot`) read as Claude Code's;
  each client's answer shapes; forged saved-output paths are not read; the templates under `clients/` name
  their client and events the adapter maps.
- End to end, by hand, with a scratch `ATLAS_HOME` and `CLAUDE_CONFIG_DIR`: in both Copilot and OpenCode a
  `cat` of a 3,000-line file reached the model as one digest line with its handle, and a decision stored
  beforehand answered a question asked without tools.
