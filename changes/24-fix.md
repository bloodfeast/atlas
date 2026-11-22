---
bump: patch
section: fix
---
- **Opening the atlas repository in Claude Code no longer warns about `CLAUDE_PLUGIN_ROOT`.** The plugin's MCP
  server entry now lives in `plugin.json`, so the repository's root no longer holds a `.mcp.json` that Claude
  Code also reads as a project server.
