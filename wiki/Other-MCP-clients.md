# Other MCP clients

atlas is built and measured in Claude Code, but it is a plain MCP server over stdio, so any client that runs local
MCP servers can use its tools. This page says how to set it up in Codex, Zed and Claude Desktop, what each one
gets, and the hook contract a client with command hooks can rely on.

## What works without hooks

Without hooks atlas is project memory; the measured token savings need hooks (Claude Code today, Codex planned).
The tools (`recall`, `search`, `grep`, `section`, `lookup`, `symbol`, `journal`, `handoff` and the rest) work in
every client below, and what one session writes the next one recalls, whatever client wrote it.

What hooks add is everything atlas does without being asked: recall injected on each prompt, the compaction card,
the context cap, subagent briefs, and the output swaps behind routing (shell output into handles), which carry
about two thirds of the net saving measured in Claude Code. A client with no hooks at all keeps at most the share
that comes from the model calling atlas's lookups itself, less what the tool definitions cost: about 40% of the
hooked net, and that is an **estimate**, never measured outside Claude Code. It assumes the model calls atlas as
often as Claude does; a model that calls it rarely saves close to nothing. See
[[Token savings and method]] for how the hooked numbers are measured.

## Codex

Verified with Codex CLI 0.172.0 on Windows, 2026-11-25: the MCP server, the SessionStart and UserPromptSubmit
command hooks, and their context reaching the model.

### The server

Project `.codex/config.toml` (or the same table in your user `config.toml`):

```toml
[mcp_servers.atlas]
command = 'C:\path\to\atlas.exe'
args = []
startup_timeout_sec = 30
env = { ATLAS_UNATTENDED = '1' }

# Only if shell commands call atlas themselves (`atlas run`):
[shell_environment_policy]
set = { ATLAS_UNATTENDED = '1' }
```

`args` takes the same flags as anywhere else, for example `['--config', 'C:\path\to\atlas.json']`.
`ATLAS_UNATTENDED=1` tells atlas no user is there to answer its SessionStart offers (see
[Sessions with no user](Configuration-reference#sessions-with-no-user-atlas_unattended)); Codex never asks them.
The `env` key is Codex's documented one; the verification run set the variable another way, so `env` itself is
not yet run against atlas.

### The hooks

Project `.codex/hooks.json`. The command form is recommended for both events: it is the one verified end to end.
Forward slashes avoid JSON escape mistakes on Windows.

```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "C:/path/to/atlas.exe --hook", "timeout": 30}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "C:/path/to/atlas.exe --hook", "timeout": 30}]}]
  }
}
```

A hook process does not inherit the server's `env`, so set `ATLAS_UNATTENDED=1` in the environment Codex runs in,
or add `--config` to the command the same way as `args` above.

The no-spawn alternative for UserPromptSubmit is Codex's `mcp_tool` handler calling atlas's `hook` tool on the
server that is already running:

```json
{"type": "mcp_tool", "server": "atlas", "tool": "hook",
 "input": {"event": "UserPromptSubmit", "session_id": "${session_id}", "prompt": "${prompt}", "cwd": "${cwd}"}}
```

Its reply is the same hook JSON as text (see [The hook contract](#the-hook-contract)); whether Codex unwraps
`hookSpecificOutput` from a tool's text is untested, so it may reach the model as the JSON itself. In the
verification run Codex filled `${session_id}` with the turn's id rather than the session's, so on this form a
record can be pointed to again on every turn.

Use one transport per event. Both share the per-session dedupe through the store: a `--hook` process reads what
was injected in the session before it started, and the server reads it again at every prompt, so a record either
one pointed to by its own key is not pointed to again by the other. A record shown only inside a lane view is not
covered: the view is logged under its lane key alone, so the other transport can still point to that record. Rows
are written after the answer: a `--hook` process writes its rows before it exits, and the server's rows come from
its log thread, so a hook that answers while that thread lags behind the server's last answer can point to a record
the server has just pointed to.

### Trust

- The project must be trusted: `codex exec --trust`, or accept the trust prompt in interactive `codex`.
- Each hook must be trusted too, in `/hooks` or Settings > Hooks. `codex exec` skips an untrusted hook **without a
  warning**, so a missing answer in exec is first a trust question.
- On Windows the trust keys are lowercased paths: a project trusted under one casing is the same project.

### Shell commands

Under PowerShell, `atlas run -- echo X` fails (`echo` is a PowerShell alias, not a program). Run a shell
built-in through its shell: `atlas run -- cmd /c echo X`, or `atlas run -- powershell -NoProfile -Command "..."`.

## Zed

Not yet run against atlas: set up from Zed's own documentation.

The Zed Agent (Zed's native agent) reaches MCP servers through `context_servers` in Zed's settings. It supports
MCP's tools and prompts, nothing else, and it has no hooks: no event of any kind reaches a server, so atlas there is
the tools alone.

```json
{
  "context_servers": {
    "atlas": {
      "command": "C:\\path\\to\\atlas.exe",
      "args": [],
      "env": { "ATLAS_UNATTENDED": "1" }
    }
  }
}
```

- **Tool approval.** Zed asks before every tool call until you allow it; atlas's tools are keyed
  `mcp:atlas:<tool>` in `agent.tool_permissions`.
- **Rules.** Zed reads the **first match only** of `.rules`, `.cursorrules`, `.windsurfrules`, `.clinerules`,
  `.github/copilot-instructions.md`, `AGENT.md`, `AGENTS.md`, `CLAUDE.md` and `GEMINI.md` at the project root, so
  an atlas block in a project's `AGENTS.md` is silently unread where a `.rules` file exists. The personal file is
  read for every project and is the safer carrier: `%APPDATA%\Zed\AGENTS.md` on Windows, `~/.config/zed/AGENTS.md`
  elsewhere. A rule block to paste there is planned as `atlas settings agents-md`; atlas prints it and never writes
  Zed's files.
- **External agents.** Zed forwards its `context_servers` to external agents (Claude Agent, Codex and the others
  it runs through ACP), and each agent reads its own MCP configuration as well. An agent that already has atlas
  in its own configuration then gets two atlas servers: two tool lists to pay for, and hooks that talk to one
  server while the model calls the other. Put atlas in Zed's settings only for the Zed Agent, or configure it in
  one place only.

## Claude Desktop

Not yet run against atlas: set up from Anthropic's documentation.

`claude_desktop_config.json` (Settings > Developer > Edit Config):

```json
{
  "mcpServers": {
    "atlas": {
      "command": "C:\\path\\to\\atlas.exe",
      "args": [],
      "env": { "ATLAS_UNATTENDED": "1" }
    }
  }
}
```

- **Chat** has no hooks and drops a server's instructions, so the model learns atlas from its tool descriptions
  alone. Recall works when it is called; nothing is injected.
- **The Code tab is Claude Code.** Use the atlas plugin there, as in [[Install and setup]]; everything on the other
  pages applies.

## The hook contract

`atlas --hook` reads one hook event as JSON on stdin and prints its answer on stdout, a process per event; the
`hook` MCP tool takes the same event as its arguments and replies with the same text. Only SessionStart and
UserPromptSubmit are pinned for clients other than Claude Code, by the tests
`the_hook_contract_for_other_clients_is_pinned_command` and `the_hook_contract_for_other_clients_is_pinned_tool`;
every other event `--hook` accepts is not stable. (PreCompact, for one, does run on a Codex rollout: it writes a
checkpoint whose card, at the next SessionStart `compact`, says the transcript was unread.)

**Input.** The fields atlas reads for these two events:

| Field | Required | Meaning |
|---|---|---|
| `hook_event_name` (or `event`) | yes | `SessionStart` or `UserPromptSubmit`; the `hook` tool's name for it is `event` |
| `session_id` | yes | the conversation; dedupe and the session budget are per session |
| `prompt` | UserPromptSubmit | the user's prompt, what recall is matched against |
| `cwd` | yes | the working folder, which names the project |
| `source` | no | SessionStart: `startup`, `resume`, `clear` or `compact` |
| `transcript_path` | no | a Claude Code transcript; null, missing, or a file that is not Claude Code's (a Codex rollout) is ignored, never parsed |

Any other field (Codex sends `turn_id`, `model` and `permission_mode`) is accepted and ignored. A `${...}` value
the client did not substitute counts as empty.

**Output.** A UserPromptSubmit that recalls something prints exactly one JSON object:

```json
{"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "atlas recall ..."}}
```

That is `hookSpecificOutput` holding exactly `hookEventName` and `additionalContext`, nothing else, on both
transports. `additionalContext` names each record by its key, one `recall key=` away. Nothing to say is empty stdout with
exit 0. A SessionStart answer has the same shape with `"hookEventName": "SessionStart"`; with `ATLAS_UNATTENDED=1`
it never asks an offer, and on a fresh store it prints nothing, where the same start attended (`0`, or unset)
prints the first-session offers on the command form. Exit is 0 in every case atlas answers, including a
time budget overrun (the answer is then one line saying what was skipped).

**Per session.** A record pointed to by its own key is pointed to once per session, across processes and across
both transports: a second `--hook` process of the same session reads what the first injected from the store, and
the server reads it again at every prompt. Records shown inside a lane view are deduped within one process only
(the view is logged under its lane key). The session budget (`recall.session_budget_tokens`) counts from the same rows: each holds every
byte its answer sent, the pointer header and the "+N more" line included, so a `--hook` process and the server
charge a prompt alike. Rows not yet written (the other process is still answering) are not counted.

**A prompt that gets nothing** is the relevance gate, not the session: a record must hold `recall.min_terms` (3)
of the prompt's words, so "what is the widget colour decision?" gets nothing from a record that says "widget" and
"colour" but never "decision", in any session, fresh or not.

**Cost.** Each command-hook call is a process start, a store open and, with the embedding model, a model load.
Measured 2026-11-26 at commit 2e045dde, release build, Windows, a temp store of 1,604 records, 20 UserPromptSubmit
runs per arm timed from process spawn to exit (one warm-up run first); the machine had another build running, so
treat these as provisional:

| Arm | Median | p95 | What the hook answered |
|---|---|---|---|
| no model (lexical recall) | 34 ms | 36 ms | nothing, on every run (random prompts cleared no gate) |
| embedding model (bge int8) | 338 ms | 353 ms | a pointer, on every run |

About 270 ms of the model arm is loading the model, once per process. The `hook` tool through the running server
pays it once per server, not per prompt, which is the case for `mcp_tool` where a client offers it. The Codex
verification run measured 34 to 105 ms per call on a fresh store, through a Python logging wrapper.
