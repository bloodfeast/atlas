# atlas

atlas makes long Claude Code sessions cheaper without changing how you work. It is a local MCP server and a set
of Claude Code hooks: large tool output stays on disk behind a short handle and the model gets a digest, code
reads get exact item ranges instead of guessed windows, and project memory (rules, decisions, journals,
handoffs) is recalled by relevance instead of loaded whole.

It is for anyone who runs long Claude Code sessions, or many agents, on one codebase. It runs on your machine
and makes no network calls at query time.

**You do not need Obsidian.** A vault is any folder of Markdown files; Obsidian is an optional reader for it.

Memory, search and the hooks work whatever language a project is written in. The code index covers Rust, Go,
TypeScript, JavaScript, Python, C#, Java, Kotlin, WGSL and Protocol Buffers, and any language with a SCIP
indexer ([Code index](https://github.com/bloodfeast/atlas/wiki/Code-index)).

## Install

Prebuilt for Windows x86_64, Linux x86_64 and aarch64, and macOS (Apple Silicon and Intel). In Claude Code:

```
/plugin marketplace add bloodfeast/atlas
/plugin install atlas@atlas
```

Restart Claude Code and open a session in your project. Claude asks whether to create a vault for it; say yes.
It also asks whether to run in orchestrated mode, and no is fine. Then work as usual.

- The first start writes `atlas.json` in your configuration folder and fetches the embedding model (about
  35 MB, checked against a pinned SHA-256). Search and recall are lexical until it lands.
- Updates are signed and on by default. `atlas settings off update` turns them off.
- `atlas settings undo` reverts any change atlas made to your setup. It never deletes a note.

Setup questions, several repositories in one project, building from source and uninstalling are on
[Install and setup](https://github.com/bloodfeast/atlas/wiki/Install-and-setup) and
[Projects and multi-repo](https://github.com/bloodfeast/atlas/wiki/Projects-and-multi-repo).

## What it saves

Every turn re-reads the whole context, so output that enters early is paid for on every later turn. Before
atlas, shell reads were 36% of what a builder session processed and startup context another 35%; those are
what atlas cuts. Measured:

| What | Measured | Turned on by | Method |
|---|---|---|---|
| Lookups | 6.1x fewer tokens than grep and file reads (ratio of sums over 8,639 answers on one store; 10.3x median), at v0.10.0 | on by default | [Headline figures](https://github.com/bloodfeast/atlas/wiki/Token-savings-and-method#headline-figures) |
| Memory questions | 100 of 100 answered, median 506 tokens each, on the 100-question memory eval, at v0.10.0 | on by default | [Headline figures](https://github.com/bloodfeast/atlas/wiki/Token-savings-and-method#headline-figures) |
| The tool list, in every session | 14 tools in 6,042 bytes (about 1,510 tokens), down from 47 tools and 5,930 tokens; a test holds the budget | always | [Headline figures](https://github.com/bloodfeast/atlas/wiki/Token-savings-and-method#headline-figures) |
| Recall on each prompt | 1 of 30 off-topic prompts gets a record; about 50 tokens injected per prompt over a 303-prompt replay, at v0.10.0 | `recall.budget_tokens` 600, `recall.cos_floor` 0.53 and `recall.inject_floor` 0.60, the defaults | [Headline figures](https://github.com/bloodfeast/atlas/wiki/Token-savings-and-method#headline-figures) |
| Weak injections | 2,739 low-cosine pointers dropped, about 27k tokens a week over 14 days of one store, memory eval unchanged, at v0.9.0 | `recall.inject_floor` 0.60, the default | [Headline figures](https://github.com/bloodfeast/atlas/wiki/Token-savings-and-method#headline-figures) |
| Subagent start | about 210 tokens against 649 for the default, informing 18 needs against 1 on 30 real launches, at v0.9.0 | `"recall": {"subagent_context": "binding"}`, off by default | [Headline figures](https://github.com/bloodfeast/atlas/wiki/Token-savings-and-method#headline-figures) |

`atlas --token-report` measures your own store the same way, against your Claude Code transcripts from before
atlas: a summary in about 15 lines, and the full report with `--verbose`.

## Safe by construction

- **Local only.** One process over stdio, with no HTTP client in the code or its dependencies. The model
  download and the update check go through the system `curl` to pinned `https` URLs and are verified.
- **Confined.** Every path stays inside a configured folder. `.env*`, keys and credential files are never read,
  whatever the configuration says.
- **Nothing lost.** There is no delete or rename tool; writes are additive, atomic and audited, and the memory
  store is append-only.

The threat model is on [Security model](https://github.com/bloodfeast/atlas/wiki/Security-model). Report
vulnerabilities privately, as [SECURITY.md](SECURITY.md) describes.

## Documentation

The wiki is the full manual.

- **Getting started:** [Overview](https://github.com/bloodfeast/atlas/wiki/Overview) ·
  [Install and setup](https://github.com/bloodfeast/atlas/wiki/Install-and-setup) ·
  [Configuration reference](https://github.com/bloodfeast/atlas/wiki/Configuration-reference) ·
  [Reference configuration](https://github.com/bloodfeast/atlas/wiki/Reference-configuration) ·
  [Projects and multi-repo](https://github.com/bloodfeast/atlas/wiki/Projects-and-multi-repo)
- **What it saves:** [Token savings and method](https://github.com/bloodfeast/atlas/wiki/Token-savings-and-method) ·
  [Handles and digests](https://github.com/bloodfeast/atlas/wiki/Handles-and-digests)
- **Reference:** [Tools reference](https://github.com/bloodfeast/atlas/wiki/Tools-reference) ·
  [Code index](https://github.com/bloodfeast/atlas/wiki/Code-index) ·
  [Semantic search](https://github.com/bloodfeast/atlas/wiki/Semantic-search) ·
  [Identifiers and freshness](https://github.com/bloodfeast/atlas/wiki/Identifiers-and-freshness) ·
  [Memory store and hooks](https://github.com/bloodfeast/atlas/wiki/Memory-store-and-hooks) ·
  [Memory tree](https://github.com/bloodfeast/atlas/wiki/Memory-tree) ·
  [Memory and rule files](https://github.com/bloodfeast/atlas/wiki/Memory-and-rule-files) ·
  [Orchestration](https://github.com/bloodfeast/atlas/wiki/Orchestration) ·
  [Discussions](https://github.com/bloodfeast/atlas/wiki/Discussions) ·
  [Project tracking](https://github.com/bloodfeast/atlas/wiki/Project-tracking) ·
  [Generated indices](https://github.com/bloodfeast/atlas/wiki/Generated-indices) ·
  [Cross-vault links](https://github.com/bloodfeast/atlas/wiki/Cross-vault-links)
- **Internals:** [Security model](https://github.com/bloodfeast/atlas/wiki/Security-model) ·
  [SQLite store](https://github.com/bloodfeast/atlas/wiki/SQLite-store) ·
  [Testing and coverage](https://github.com/bloodfeast/atlas/wiki/Testing-and-coverage) ·
  [Releasing](https://github.com/bloodfeast/atlas/wiki/Releasing) ·
  [Changelog](https://github.com/bloodfeast/atlas/wiki/Changelog)

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
