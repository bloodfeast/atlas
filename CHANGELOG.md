# Changelog

Every release has a section here, `## vX.Y.Z`, written for users: what changed for them, not how the work
was organised. It is the source of the release notes (through `scripts/release-notes.md.in`) and of the
wiki's Changelog page. A section has these parts, in this order, and leaves out any that would be empty:

- `### Highlights`: what a user notices, with measured numbers where there are any. Required.
- `### Breaking changes` and `### Upgrade notes`: what a user must do when updating.
- `### Features`, `### Security hardening`, `### Known limits`: everything else.

`cargo xtask release` refuses to release a version whose section is missing, empty or has no highlights.

## v0.1.0

The first public release.

### Highlights

- **Large tool output stays out of the context.** A Bash, PowerShell, Read, Grep or Glob result over the
  threshold is kept on disk under a short handle, and the model gets a digest: test totals and failures,
  compiler errors by lint, grep hits by file, a file's outline. In the first measured sessions, 236 KB of
  such output entered the context as 8.2 KB of digests, and an 87 KB source file became a 3.7 KB outline
  plus the four windows the model asked for. It is on by default and fails open.
- **Exact code ranges.** `symbol` answers with each definition's line range (`src/store.rs:2089-2113`), and
  the hooks add the enclosing item and its range to ordinary Grep, Read and shell-read output, so the model
  reads an item whole instead of guessing windows.
- **Answers half the size of grep.** On real transcripts, an atlas lookup answer has a median of 288 tokens
  against 624 for the grep-and-read episode it replaces: 2.2 times smaller per answer, measured with
  `cargo xtask calibrate-baseline` and `token-use`, not estimated.
- **Memory recalled by relevance, not loaded whole.** An append-only memory store with lane journals,
  handoffs and relevance-gated recall. On a 100-question evaluation answered from tool output alone, it
  answered all 100 for an agent that follows a result to its source and 99 for a strict agent that reads only
  the first result, finding all 176 facts at a median of 504 tokens per question, with no recall misses and
  no drift between the store and its files.
- **Projects.** One install serves several projects, each with its own index and memory store, including
  multi-repository projects described by a committed `atlas.project.json`.
- **Local only.** One MCP server over stdio. No network at query time; the only downloads are the
  embedding model at setup and signed updates.

### Features

- **The plugin.** Install with `/plugin marketplace add bloodfeast/atlas` and `/plugin install
  atlas@atlas`. Prebuilt binaries for Windows x86_64, Linux x86_64 and aarch64, and macOS (Apple Silicon
  and Intel).
- **Setup.** `atlas --setup` writes a starter configuration and downloads the embedding model, resuming an
  interrupted download and checking its SHA-256 before use. Until the model is in place, search's
  `hybrid` mode and recall are lexical. The first session in a folder asks once whether to add it as a
  source (its Markdown, plus a code root when it holds Rust or WGSL); `atlas settings add-project [DIR]`
  adds one any time.
- **Updates.** At session start atlas installs a newer signed release: checked at most hourly, capped at
  10 s, and failing open. `atlas settings off update` or `update.pin` turns it off. A newer version is
  never replaced by an older one.
- **Settings.** `atlas settings` offers the Claude Code settings that cut cost (the 1-hour subagent prompt
  cache, an early compaction point), each with what it does and costs. Nothing is written without a yes;
  every write is backed up and `atlas settings undo` reverts the newest.
- **Handles and digests.** `atlas slice h:ID --item NAME | --failures | --lines A-B | --grep RE` pulls
  exactly what the model needs from a stored output. `atlas run -- CMD` does the same for a command and
  exits with its exit code. `atlas settings off routing` turns routing off; `ATLAS_RAW=1` passes one
  command through.
- **Code index.** Rust from rust-analyzer's SCIP output (build scripts and proc macros off: nothing is
  compiled), WGSL and Protocol Buffers from owned scanners. Definitions, references with their callers,
  and impl edges, each with its line range. A file changed by Edit, Write or MultiEdit is re-indexed at once.
  Across a project's repositories, `symbol` of an rpc names who defines it and who calls it.
- **Identifiers and sections.** An identifier registry and section-level full-text search over vaults and
  read-only Markdown sources. Every answer states its freshness; a misses log records what search did not
  find, and `atlas --recall-check` compares the index with a plain scan.
- **Core verbs always loaded.** `lookup`, `search`, `recall`, `section`, `grep` and `symbol` are in context
  from the first turn; the other tools stay deferred until searched for. Answers are compact text with
  small default limits (`lookup` 3 mentions, `search` 5 sections).
- **Semantic and hybrid search** with an owned int8 sentence-embedding encoder (AVX2, AVX-512 VNNI and NEON
  kernels, tested bit-identical against a scalar reference).
- **Memory store.** Append-only and hash-chained, enforced by SQLite triggers. A reversal is a supersede;
  snapshots roll up into an archive, and `--memory-as-of KEY TIME` answers what a record said at any time.
  Lane journals, handoffs, and `atlas ruling` and `atlas entry` for rulings and log entries with explicit
  fields. `recall {query}` returns the best record whole and the rest as one-line pointers.
- **Memory stays in sync with its files.** While the rules file and the memory folder stay hand-written,
  every rescan re-imports what changed: a changed item is superseded, a removed one retired, and history is
  never overwritten. `--recall-check` reports drift between the store and its source files.
- **The memory index as a tree.** Claude Code cuts `MEMORY.md` at 200 lines; `memory.tree` turns it into a
  short root over topic hubs, with every memory at most three reads from the root and pinned memories in the
  root. Rebuilds are stable. On a hand-labelled set of 227 memories, automatic placement agreed with the
  labels on all but 20, which were judgement calls.
- **Hooks.** Relevance-gated recall on each prompt and subagent start, pre-compaction handoffs, and a
  context-cap guard for subagents. `atlas --hook-report` prints each hook's hit rate and tunes the relevance
  floor from that data.
- **Projects.** A project is a named set of roots with its own database and memory store; recall, the hooks,
  lanes and the decision queue see only the active one. A shared user layer is imported into every project.
  `atlas.project.json` lists a project's repositories by path or by remote, with optional `role` and
  `talks_to`: a prompt that names a repository gets one line from that graph, and recall puts records about
  the active repository and its neighbours first. The first session in a folder of repositories offers to
  make it a project; `atlas settings migrate-projects NAME` moves an existing setup into one.
- **Discussions.** Reviewer threads in Markdown with immutable, hash-chained posts, computed status, and an
  unresolved queue for human decisions.
- **Vault writes.** Additive or exact, atomic, serialised across processes, and audited with a required
  reason. No delete or rename tool exists.
- **The token ledger.** Every answer is logged against its grep-and-read equivalent; `atlas --token-report`
  leads with the measured ratio, the upper bound beside it.
- **File tools.** `atlas digest FILE` cuts a long check log to its totals, failures and verdicts; `atlas crop`
  cuts frames or a region out of a picture strip into a small PNG. Neither needs a configuration.
- **Platforms.** Windows, Linux and macOS, with native change notification on each. Two dependencies: SQLite
  (compiled in) and an ed25519 signature check.

### Security hardening

- Every vault path is confined lexically and again after canonicalisation; dot folders, device names
  (including `COM¹`, `CONIN$` and the like), alternate data streams and links that leave a vault are refused.
- A fixed deny-list keeps `.env*`, keys and credential files unread whatever the configuration says.
- Updates are verified against an ed25519 key built into atlas, and each binary against the signed
  SHA-256. Download URLs must be `https`, or `http` to a real loopback address.
- Regular expressions run on an owned bounded backtracker: no pattern takes more than program length times
  text length steps, and the memory for that bound is capped at 32 MiB per search. A search past it is
  refused with what to narrow.
- Recalled records are framed so a line inside a record's body cannot pass for the header of another record.
- Glob matching and JSON parsing are linear time on hostile input; image, model and protocol parsers refuse
  corrupt or oversized input instead of panicking.
- An adversarial test suite covers every surface atlas reads: protocol frames over the real binary, every
  tool argument with hostile values, paths, queries, parsers, hooks, the store and updates.
- Releases are checked against a private leak denylist after Unicode normalisation (case, confusable
  letters, invisible characters, line breaks and UTF-16), so nothing private ships.

### Known limits

- The before-and-after cost of whole sessions is measured during the release's first live week; the
  numbers above are per answer and per tool result.
- A PowerShell command that exits nonzero is routed only for `cargo` builds and tests on Windows. Claude
  Code offers no read-only PowerShell tail that keeps the exit code, so other failing PowerShell commands
  reach the model raw.
- The code index covers Rust, WGSL and Protocol Buffers. Other languages are seen only as rpc call sites.
- `make_link` writes `obsidian://open` links between vaults, which open only in Obsidian.
- Only the latest release gets fixes.

### Upgrade notes

- First install: nothing to migrate. Your notes stay the source of truth; atlas builds its index from the
  files on first start.
- To move an existing single configuration into a project, run `atlas settings migrate-projects NAME`. It
  takes a backup first, and every session resolves as before.
