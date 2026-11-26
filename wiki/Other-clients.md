# Other clients

atlas is built for Claude Code, but it has two parts, and both work elsewhere:

- **The MCP server** (`atlas`, stdio): recall, journal, handoff, lookup, search, the code index and every other
  tool. Any client that loads a local MCP server can use it, with any model.
- **The hooks** (`atlas --hook`): the digest of a long tool result, recall on each prompt, the session card,
  the gates and the compaction work. Each client has its own hook format, so atlas answers each one in its
  own: `atlas --hook --client NAME`.

The model does not matter; the client does. GitHub Copilot, for one, runs the same hooks whichever model you
pick in it.

## What works where

| | Claude Code | GitHub Copilot | OpenCode | Codex | Gemini CLI | Cursor, Grok Build |
|---|---|---|---|---|---|---|
| MCP tools | yes | yes | yes | yes | yes | yes |
| Digest of a long Bash result | yes | yes | yes | yes, as a block | yes, as a deny | no |
| Recall on each prompt | yes | yes | yes | yes | yes | no |
| Session card | yes | yes | yes | yes | yes | no |
| Deny (the merge gate) | yes | yes | yes | yes | yes | no |
| Agent caps, orchestrated mode | yes | no | no | no | no | no |
| Compaction instructions | yes | no | yes | no | no | no |
| Subagent lane recall | yes | at start, no brief | no | at start | no | no |
| Tested | yes | Copilot CLI 1.0.114 | OpenCode 2.4.1 | from its documentation | from its documentation | MCP only |

Cursor and Grok Build cannot replace a built-in tool's result from a hook, which is what the digest needs, so
atlas offers them its MCP server only.

A client other than Claude Code is never offered Claude Code settings, and atlas leaves Claude Code's own
status line and hooks alone in its sessions.

## The MCP server

Each client starts `atlas` with no arguments. `atlas` must be on `PATH`, or give its full path.

| Client | Where |
|---|---|
| GitHub Copilot CLI | `copilot mcp add atlas -- atlas` (user level, `~/.copilot/mcp-config.json`), or a workspace `.mcp.json` |
| VS Code (Copilot) | `.vscode/mcp.json`: `{"servers": {"atlas": {"type": "stdio", "command": "atlas"}}}` |
| OpenCode | `opencode.jsonc`: `{"mcp": {"servers": {"atlas": {"type": "local", "command": ["atlas"]}}}}` |
| Codex | `~/.codex/config.toml`: `[mcp_servers.atlas]` with `command = "atlas"` |
| Gemini CLI | `settings.json`: `{"mcpServers": {"atlas": {"command": "atlas"}}}` |
| Cursor | `.cursor/mcp.json`: `{"mcpServers": {"atlas": {"command": "atlas"}}}` |
| Grok Build | `grok mcp add` |

## The hooks

The files under `clients/` in the repository are ready to copy. Each runs `atlas --hook --client NAME`, one
process per event. They need an `atlas` that knows `--client` on `PATH`: Copilot refuses every tool call whose
PreToolUse hook fails, and an older `atlas` fails on the unknown flag. An `atlas` that knows `--client` lets the
event through whenever it fails itself, with the error on standard error.

- **GitHub Copilot:** copy `clients/copilot/hooks.json` to `~/.copilot/hooks/atlas.json` (every repository), or
  to `.github/hooks/atlas.json` in a trusted repository. VS Code, the GitHub Copilot app and Copilot CLI read
  the same files.
- **OpenCode:** copy the folder `clients/opencode/atlas` to `.opencode/plugins/atlas` in a project, or to
  `~/.config/opencode/plugins/atlas`. The plugin runs `atlas`, or the binary `ATLAS_BIN` names.
- **Codex:** copy `clients/codex/hooks.json` to `~/.codex/hooks.json` or `.codex/hooks.json`, then trust it in
  `/hooks`.
- **Gemini CLI:** merge the `hooks` object of `clients/gemini/settings.json` into `~/.gemini/settings.json` or
  `.gemini/settings.json`.

### How each client differs

- **GitHub Copilot** sends its PascalCase events in Claude Code's shape, tool names included. Two events exist
  only in camelCase and name no event in their input, so the hooks file passes `--event`:
  `userPromptTransformed` carries the prompt's recall (Copilot drops what a command hook answers to
  UserPromptSubmit), and `subagentStart` carries a subagent's start. Copilot ignores a hook's rewrite of a
  tool's input, so atlas sends none. A long output Copilot keeps in a file is digested from that file.
- **OpenCode** runs plugins, not commands. The plugin adds the session card and each prompt's recall to that
  prompt in every model call, but never to the prompt OpenCode stores and shows you. A long shell output OpenCode cut
  short is digested from the file it keeps.
- **Codex** speaks Claude Code's hook JSON; a digest goes back as a PostToolUse block whose reason replaces the
  result. Not yet run against Codex itself.
- **Gemini CLI** has its own event and tool names (`BeforeTool`, `run_shell_command`), mapped to Claude Code's.
  Not yet run against Gemini CLI itself.

### What is lost outside Claude Code

- **Compaction instructions:** only Claude Code and OpenCode let a hook add to the summary's instructions.
  Elsewhere the checkpoint is still written before a compaction, but the summary does not carry its keys.
- **The context cap:** atlas reads the context size from Claude Code's transcript. Other clients' transcripts
  differ, so the cap and its warnings do not apply there.
- **The skills** (`atlas`, `orchestrate`) are Claude Code skills. Other clients get the tools' own
  descriptions only.
- **Agent caps:** atlas frees an agent's slot when the agent stops, and Copilot's built-in general-purpose agent
  sends no stop. Other clients' launches are not counted, and their sessions are not offered orchestrated mode.
