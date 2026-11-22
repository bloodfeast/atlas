---
bump: none
section: upgrade
---
- **Restart running Claude Code sessions after this update.** The plugin's MCP server entry moved into
  `plugin.json`. A session started before the update, on a plugin folder updated in place, loses atlas's tools
  at its next `/clear`; a new session connects as before.
