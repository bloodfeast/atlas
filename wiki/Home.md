atlas makes long Claude Code sessions cheaper without changing how you work: a local MCP server and a set of
Claude Code hooks that keep large tool output on disk behind a digest, give code reads exact item ranges, and
recall memory by relevance instead of loading it whole. **You do not need Obsidian**: a vault is any folder of
Markdown files.

The README is the front page; this wiki is the whole manual.

## Getting started

- [[Overview]]: what atlas is, what it is not, and the rules it is built to.
- [[Install and setup]]: the plugin, first run, updates, uninstalling, building from source.
- [[Configuration reference]]: every key of `atlas.json`.
- [[Projects and multi-repo]]: one install, scoped projects, repositories by path or remote.

## What it saves

- [[Token savings and method]]: where a session's tokens go, what atlas changes, how it is measured.
- [[Handles and digests]]: large tool output as a handle and a digest; `atlas slice`, `atlas run`.

## Reference

- [[Tools reference]]: every MCP tool, the compact answer format, the token ledger, write reasons.
- [[Code index]]: `symbol`, item ranges for Rust, WGSL and Protocol Buffers, and language coverage.
- [[Semantic search]]: the owned embedder and hybrid ranking.
- [[Identifiers and freshness]]: documents, sections, the identifier registry, freshness and misses.
- [[Memory store and hooks]]: the append-only store, exports, sync from files, recall and the hooks.
- [[Memory tree]]: the memory index as a tree under Claude Code's 200-line cut.
- [[Memory and rule files]]: experimental, off by default: what atlas does with your rules and memories once you turn it on, replaced rulings, headlines, known limits, and how to turn it off.
- [[Orchestration]]: orchestrated mode: lanes and how to declare them, kinds and caps, starter resources, context limits and report digests.
- [[Discussions]]: reviewer threads with immutable posts and a decision queue.
- [[Generated indices]]: indices written between markers, from frontmatter.
- [[Cross-vault links]]: `obsidian://` links between vaults and how links resolve.

## Internals

- [[Security model]]: threat model, path confinement, the deny-list, updates, dependencies.
- [[SQLite store]]: the index database's tables, rebuilds and the audit log.
- [[Testing and coverage]]: the suite, the adversarial tests, coverage and development tasks.
- [[Releasing]]: the changelog rule, release notes, the wiki publish and the public repository's setup.
- [[Changelog]]: every release, newest first.
