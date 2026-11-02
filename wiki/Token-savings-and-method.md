What atlas saves, and how it is measured. The mechanisms are on [[Handles and digests]], [[Code index]] and
[[Memory store and hooks]].

## Where the tokens go

Every assistant turn re-reads the whole context. A result that enters the context at turn 10 of a 300-turn
session is therefore processed 290 more times. We measured where the processed tokens of real sessions come
from with `cargo xtask attrib` (method below). The sample was 69 builder subagent sessions and the long
orchestrator sessions that ran them, before atlas.

| Source | Share of processed tokens (builder sessions) |
|---|---|
| Startup context: harness prompt, tool list, skill list, memory index, brief | 35% |
| Shell reads (`sed -n` windows are 55% of these, `grep` 21%) | 36% |
| The model's own tool inputs | 16% |
| Build and test output | 6% |
| Read, Grep, Glob and atlas together | under 6% |

The absolute figures:

- A long orchestrator session processes 0.7 to 1.8 G tokens, 82 to 89% of them on turns where the context
  is already above 200k tokens. Orchestrator sessions cost 15 to 35 times what a builder session does, so
  they are where savings matter most.
- A builder session processes about 50 M tokens.
- The startup context is 49.6k tokens per turn: about 31k harness, 5k skill listing, 6k memory index, 2.5k
  deferred tool names, plus hooks.
- Prompt-cache expiry is about 25% of the price-weighted cost of a builder session: with the default
  5-minute cache, a build or test wait longer than that re-writes the whole cached context (130 M of 155 M
  cache-write tokens in the sample were full re-writes).

Each lever has a ceiling. Digesting build output can save at most the 6% build bucket. Exact code ranges
target the 36% shell-read bucket. A 1-hour subagent cache targets the 25% re-write cost.

## What atlas changes

- **Handles and digests, by hook, not by model choice.** A Bash, PowerShell, Read, Grep or Glob result
  over a threshold (12 KB, or 3 KB for `cargo` build and test output) is stored whole under a handle such
  as `h:3f9a2c1b0e`. The model gets a digest instead: test totals and each failure's location and
  message, compiler errors and warnings by lint, grep hits by file, or a file's outline with line ranges.
  It pulls exactly what it needs with `atlas slice h:ID --item NAME | --failures | --lines A-B | --grep RE`.
  Routing is on by default and fails open: any error passes the call through untouched. One limit: a
  PowerShell command that exits nonzero is routed only for `cargo` builds and tests on Windows. Claude
  Code offers no read-only PowerShell tail that keeps the exit code, so any other failing PowerShell command
  reaches the model raw.
- **Exact code ranges.** The code index records where each definition ends, so `symbol` answers
  `src/store.rs:2089-2113` and the model reads that range instead of guessing a window. The hooks carry
  the same ranges into ordinary searches and reads: each Grep match line ends with its enclosing item, such
  as `[fn Store::report_miss 2089-2113]`, and a Read window that cuts an item is told which item and where
  it ends. Impl blocks are items too: a hit inside a method names the method, and a hit between methods
  names the impl (`[impl Display for Store 40-58]`). Shell reads get the same: `grep -n`, `rg -n` and
  `Select-String` lines get the suffix, and a `sed -n 'A,Bp'`, `head`, `tail` or `Get-Content` window gets
  the Read note. Files the index does not cover, or that changed since it was built, are left as they are;
  a file the model edits with Edit or Write is re-indexed as soon as the edit lands, so its next read is
  annotated again.
- **Claude Code settings, offered once.** `atlas settings` offers the settings that cut cost (the 1-hour
  subagent prompt cache, an early compaction point with handoff records), each with what it does and what
  it costs. Nothing is written without a yes, and every change is backed up and reversible.

First measurements, on scratch copies of this repository with Opus sessions doing review, test and edit
tasks:

- Seven results were routed. Raw output over the threshold that was not routed: none.
- 236 KB of output entered context as 8.2 KB of digests. One 87 KB source file became a 3.7 KB outline,
  plus four targeted windows totalling 12 KB.
- Slices per handle: 0 to 1 for build and test output, 4 for a section-by-section file review.

The before/after on full sessions is measured during the release's live week, with the cache effect and
the atlas effect reported separately.

## Method

`cargo xtask attrib` reads Claude Code transcripts (`~/.claude/projects/<project>/*.jsonl` and their
`subagents/` folders). For each assistant turn it notes what entered the context and how many later turns
re-read it. It weights each piece by size times residence and groups it by source. Characters are
divided by 4 for tokens, so absolute figures are estimates; the shares are what this page reports. Run it
on your own sessions:

    cargo xtask attrib ~/.claude/projects/<project>/*.jsonl
