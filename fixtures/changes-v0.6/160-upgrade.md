---
section: upgrade
---
- **The memory store moves to schema 3 the first time this build opens it.** The migration only adds the worktree
  ledger's table, index and triggers, and changes no existing row. Before it migrates it writes a copy of the
  schema 2 store to `memory.sqlite.v2.bak` beside it; if the copy fails, nothing migrates. An older atlas refuses
  a schema 3 store ("Update atlas"), so install this build for every session at once.
