atlas is an owned index for agents, so they stop grepping: a local MCP server and a set of Claude Code hooks
over Markdown folders (vaults and read-only sources), code roots and an append-only memory store. It grew
out of `vault-mcp`, a server written after a third-party Obsidian connector was removed because it let the
model switch on an internet `fetch` and run an unchecked recursive delete. atlas has neither, and is built
so it cannot grow them by accident.

**You do not need Obsidian.** A vault is any folder of Markdown files: atlas reads and writes the files
itself and never touches Obsidian's `.obsidian` settings folder. Obsidian is optional, as a reader for the
same folders. The one Obsidian-specific output is that `make_link` writes `obsidian://open` links between
vaults (see [[Cross-vault links]]); links within a vault are standard `[[wikilinks]]`.

## What it is

- stdio only (newline-delimited JSON-RPC 2.0, MCP `2025-06-18`; also answers `2025-03-26` and `2024-11-05`)
- vaults (writable, additively) and sources (read-only) fixed at launch by a config file or flags; every path
  confined to them; a hard deny-list keeps `.env*`, keys and credentials unread whatever the config says
- an identifier registry: every id the config's patterns describe, where it is defined and every mention
  (see [[Identifiers and freshness]])
- section-level full-text search: a hit is the whole section or ruling, with its heading path and lines
- a code index with exact item ranges for Rust, WGSL and Protocol Buffers (see [[Code index]])
- large tool output kept on disk under a handle, the model given a digest (see [[Handles and digests]])
- an append-only memory store with relevance-gated recall (see [[Memory store and hooks]])
- projects: one install, each project with its own index and memory (see [[Projects and multi-repo]])
- every atlas answer says how fresh it is, and a misses log records what grep found and atlas did not
- no delete, no rename; writes are additive or exact, atomic, serialised across processes and audited
- SQLite (WAL) as a derived index plus the coordination layer; the files stay the source of truth
- no network at query time. Two things reach the network, both through the system `curl` from pinned
  `https` URLs: the embedding model at setup, checked against its SHA-256, and the update check at session
  start, whose manifest is checked against an ed25519 key built into atlas (see [[Security model]])

## Where to go next

- [[Install and setup]] to install the plugin and point it at your notes.
- [[Token savings and method]] for what atlas saves and how it is measured.
- [[Tools reference]] for every tool the server answers.
