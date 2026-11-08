## Fourteen listed tools, every full tool behind them

`tools/list` names 14 tools (about 1,480 tokens; it was 47 tools and 5,930 tokens). `lookup`, `search`,
`section`, `grep`, `symbol`, `recall`, `remember`, `journal`, `handoff` and `hook` keep their names; the rest
of the surface sits behind four tools with an `op` argument:

| Listed tool | `op` | Runs |
|---|---|---|
| `note` (reads) | `read` (default), `list`, `find`, `backlinks`, `outlinks`, `broken`, `link`, `log`, `vaults`, `sources`, `help` | `read_note`, `list_notes`, `find_by_frontmatter`, `backlinks`, `outlinks`, `broken_links`, `make_link`, `write_log`, `list_vaults`, `list_sources`, the help text |
| `edit` (writes) | `create`, `append`, `history`, `frontmatter`, `section`, `link`, `next_id`, `index`, `refresh`, `refresh_all` | `create_note`, `append_to_note`, `append_history`, `set_frontmatter`, `replace_section`, `link_notes` (`vault` and `path` name the note written into), `next_id`, `build_index`, `refresh_index`, `refresh_all_indices` |
| `discuss` | `open`, `post`, `read`, `status`, `awaiting`, `list`, `queue`, `conclude`, `unresolved`, `decide` | `open_discussion`, `post`, `read_discussion`, `discussion_status`, `awaiting`, `list_discussions`, `decision_queue`, `conclude_discussion`, `mark_unresolved`, `record_decision` |
| `miss` | `list` (default), `report`, `resolve` | `misses`, `report_miss`, `resolve_miss` |

`lookup` without `id` runs `ids`; `remember` with `old` runs `supersede`; `recall` with `op: "status"` runs
`journal_status` and with `op: "log"` runs `memory_log`. Each listed schema names the arguments most calls
use, without descriptions; every argument of the full tool an op runs is still accepted and checked. The whole
reference, every tool and argument described, is one fetched text: the MCP resource `atlas://help`, or
`note {op: "help"}` for a client without resources. The full tool names (the table below) still work as tool
names for this release, so existing calls and hooks keep running; the server's instructions point at the help.

## Six are always loaded

`lookup`, `search`, `recall`, `section`, `grep` and `symbol` are marked always-loaded
(`_meta["anthropic/alwaysLoad"]: true` in `tools/list`, Claude Code 2.1.285 and later), so their schemas are
in context from the first turn and the model calls them without a tool search first. Every other tool stays
deferred: only its name is in context until the model searches for it.

## Answers are compact text

Every tool answers in plain text by default; `format: "json"` gives
the JSON instead (for the memory tools: `recall {key}` and `recall {op: "status"}` as fields, every other memory
answer as `{"text": ...}` around its text), and `verbose: true` adds scan times and timings where a tool takes it. `read_note` is one
`vault:path sha256=... bytes=N` line, then the note exactly as written (the JSON answer repeats the frontmatter
parsed beside the content). A tool without a text form of its own writes its JSON as `key: value` lines, a list
of flat records as one table under `key: N`, and every element of every list: the work-queue tools
(`decision_queue`, `awaiting`, `journal_status`) are never cut. The text (`src/text.rs`):

- A list is a header row of field names, then one tab-separated row per record, so names appear once. Inside a
  value a tab is written `\t`, a newline `\n` and a carriage return `\r`; nothing else is escaped.
- A snippet is about 120 characters centred on the matched identifier or word, cut on character boundaries,
  with `…` where it was cut. Repeat mentions in one section are one row with a count (`n`).
- Default limits are small (`lookup` mentions 3, `search` 50 lines or 5 sections, `ids` 50, `misses` 20) and a cut list ends with
  `+N more (limit=L)`. `recall {query}` ends with what it left out: `+N more over budget_tokens`, `+N more scoring near
  zero`, `+more (limit=L)`. A `kind` or `scope` filter searches every word of the query up to its first 48, and a query longer
  than that ends with `+N more words were not searched`; with no filter, a long query is searched by its 12 rarest
  words. The query is not echoed.
- Freshness says nothing when everything is fresh; otherwise one `stale SOURCE:PATH STATE[, reindexed]` line per
  file, up to 8 per source; past that one `stale SOURCE N files (REASON)[, reindexed]` line, where REASON is why the
  source is rebuilt from empty (`schema rebuild`, `index rules changed`, `first index`) or the count per state
  (`12 changed, 3 new`). JSON gives `stale_count` and `reason` in place of the list.
- `lookup` shows its definitions first (`defs`), then the strongest definition's text capped at 600 characters
  (`full: true` for every definition whole), then `mentions`. `search` in sections mode shows snippets
  (`full: true` for whole sections).

Each answer is sent once, as the text content: there is no `structuredContent` copy.

## Token ledger

Every answer is logged to the `token_log` table with the bytes returned and the bytes the same question costs
without atlas: what `grep -rniF` would print for the question's literal over the same files, plus the files a
reader then opens (whole when at most 80 lines, else an 80-line window around each match, windows merged). The
rule is in `src/tokens.rs`; the measurement runs on a background thread after the answer is written.
`atlas --token-report` prints totals by tool and by day: tokens returned, the counterfactual, and the ratio.

The other read tools with a grep equivalent use the same two parts: `section` greps its id or heading like
`lookup`, `grep` is its own pattern, `read_note` reads the whole note, `recall {query}` greps the query over the
export files, and `recall {key}` or `{lane}` reads the record where it is written (a memory's whole file;
another record's text plus 40 lines either side in its export; a lane's every record), the rule the hooks use.
Writes, `journal` among them, log no counterfactual.

Each subagent start also logs the "brief without atlas": the bytes of the rulebook (`memory.brief_reference`)
at that moment, what a brief that says "read it in full" costs. `--token-report` prints it on its own line;
it never enters a per-answer ratio. Beside it, `hook injections` sums what the recall hooks injected (keys,
sessions, tokens, per session and the largest session): the pointers a session gets in place of that brief.
`recall.inject_floor` (a cosine threshold) and `recall.session_budget_tokens` (a cap per session) bound it.
Then a `context selection` line says what `recall.select` did to those injections: how many were sent whole,
trimmed to their matching clauses or collapsed into a ref to a near-duplicate, the tokens each would have taken
whole against what was sent, and the share saved.

`recall {key: "KEY@HASH"}` reads one exact version: `HASH` is 8 to 64 hex digits of the version's SHA-256 (a
ref is written with 12, the start of the `sha` line a recalled record shows). The answer is that version
verbatim, marked `(current)`, or `(changed: the current text is KEY@NEW)` when a later version replaced it;
a hash no version has is an error that says the ref is dangling. Versions are never deleted, so a ref handed
out once always resolves to its text. `recall {key: "SRC:PATH:A-B@HASH"}` reads lines `A` to `B` of a vault or
source file: marked `(current)` when the hash still names them, else the current lines with their new ref (a path
with a space cannot be written as a ref). With `memory.transcripts` on, `recall {key: "turn:<session8>:<line>@hash"}`
returns one earlier transcript turn as stored (visible text, secret lines `[redacted]`) and `recall {query, kind:
"turn"}` lists matching turns with their refs; both see this project's turns only, and with the source off both
are refused. `atlas --compact-report --transcripts DIR [--sweep T,T,...]` adds the
checkpoint card's size and coverage, the hooks' latency, and with `--sweep` the token side of where to compact
(a plain and an atlas-assisted arm at each threshold on the same sessions, each atlas row judged against plain at the
same threshold: compactions, raw and cache-weighted tokens, re-reads; plain at Claude Code's default point is a
labelled reference row only). `--token-report` ends with the compaction line: the ratio of context
before a compaction to its summary, refs and their dangling rate, and post-compaction misses.

`atlas --misses-week [--since TIME]` is the weekly misses pass: the ten open misses (`report_miss`) seen most
since TIME (default seven days ago), by tool and query, then the ten injected keys that cost the most tokens in
the same window, to check whether each earned its place.

Every key a hook injects is logged to `hook_inject` with its scores, bytes and counterfactual.
`atlas --hook-report` marks each one a hit by fixed rules, looking only at the same session's later assistant
messages: the key in a tool input (a `recall key=`, a lookup, a grep), its source read (a memory's file, or a
`Grep` of its export file whose pattern matches it), or a quoted run of 8 of its words; and prints the hit rate
per hook. `--transcripts DIR --since TIME` adds injections reconstructed from past transcripts (rescored by the
hook's own gate), `--rows` lists every key, and `--tune-floor SCALE` prints the relevance-floor curve and the
floor that maximises the bytes the kept hits save net of the bytes injected (SCALE is the share of a hit's
counterfactual credited: 1 for the upper bound).

The counterfactual is an upper bound. `cargo xtask calibrate-baseline --config C --transcripts DIR --before TIME` (a dev-only task) measures the other
side from real Claude Code transcripts below DIR dated before TIME: the bytes of the Grep, Read and shell-read
results agents read when they looked something up in the files atlas serves, grouped into one episode per
question, and saves it. `--token-report` then heads its output with the measured ratio (median pre-atlas
episode over median atlas answer) and the upper bound, labelled as such. The rules are in `xtask/src/calibrate.rs`.

| Tool | Writes | What it does |
|---|---|---|
| `lookup` | | an identifier's definitions (strongest first, each with the defining text) then mentions (newest first), source, path, heading path and line for each, plus `freshness`; `text_hits` when nothing is registered |
| `ids` | | registered identifiers in natural order: kind, definition and occurrence counts, newest date; filters `prefix`, `pattern`, `kind`, `source` |
| `list_sources` | | vaults and sources: root, globs, files and sections, last scan and index times, stale files |
| `report_miss` | misses log | records what grep found and atlas did not: tool, query, what atlas returned, where it was found, reason |
| `misses` | | the misses log, newest first: `open` (default), `resolved` or `all` |
| `resolve_miss` | misses log | marks a miss resolved; the reason is the note. Misses are never deleted |
| `list_vaults` | | short name, folder name, note count |
| `list_notes` | | notes under a folder, optional glob (`*` `?` `**`; without `/` it matches the file name) |
| `read_note` | | a `vault:path sha256=... bytes=N` line, then the note as written; JSON: `content`, `sha256`, size, parsed frontmatter |
| `search` | | `substring` (default), `regex`, or `fts` (SQLite FTS5, ranked) over vault notes only (an empty answer says so when sources are configured; `sections` or `grep` reach sources), returning vault, path, line, snippet; or `sections`: ranked FTS5 over the sections of every vault and source, returning whole sections with heading path and line range, plus `freshness`; `semantic` ranks the same sections by meaning, `hybrid` fuses both rankings (see the Semantic search page) |
| `symbol` | | a name (or `Type::name`) in Rust, WGSL, Protocol Buffers, Go, TypeScript, JavaScript, C#, Python or Java (Kotlin and unindexed files: definitions only; see [[Code index]] for when each needs its SCIP indexer): its definitions, then references with the function each sits in (the caller), then impl edges, each `root:path:line` with a snippet (see the Code index page) |
| `find_by_frontmatter` | | `key = value` (list values match any item, `prefix` for `queued` vs `queued 139`) |
| `backlinks` / `outlinks` | | across all vaults: wikilinks, embeds, Markdown links, `obsidian://` links, each resolved |
| `broken_links` | | links whose target note is missing, cross-vault included (attachments not checked) |
| `make_link` | | `[[wikilink]]` in the same vault (folder dropped when the file name is unique in the vault, as Obsidian's shortest-path links do), `obsidian://open` link across vaults; errors if the note or heading is missing |
| `link_notes` | yes | appends `- <link>` under `## Related` (or another section, created if missing); `reverse` links back; skips a link already present |
| `create_note` | yes | fails if anything exists at the path (`create_folders` to make parents) |
| `append_to_note` | yes | appends at the end |
| `append_history` | yes | `- YYYY-MM-DD: text` as the last line under `## History` (heading created at the end if missing); date defaults to local today |
| `set_frontmatter` | yes | replaces named keys' lines in place; other keys and the body stay byte-identical |
| `replace_section` | yes | replaces one heading's body, or the preamble (text between the frontmatter and the first heading) with `preamble: true` or `heading: ""`; **requires** `expected_sha256` |
| `next_id` | yes | allocates the next `PI-###` (any prefix and width) and optionally creates `<folder>/<id> <name>.md`, `{{id}}` substituted |
| `build_index` | yes | writes a generated index between markers (see Generated indices) |
| `refresh_index` / `refresh_all_indices` | yes | regenerate from the specs stored in the markers; no write when nothing changed |
| `write_log` | | the audit log, newest first: time, tool, vault, path, client, reason, discussion, sha before and after; filters `vault`, `path`, `client`, `discussion`, `since`, `until` (UTC, `2026-10-08` or `2026-10-08T14:30`) |
| `open_discussion` | yes | creates `Discussions/D-### <topic>.md` with subjects, participants and P-1 (see the Discussions page) |
| `post` | yes | appends the next post to a thread |
| `read_discussion` | | posts as JSON (`since_post` for only what is new), integrity check, and `outcomes` |
| `discussion_status` | | each claim, challenge and question: answered, conceded, retracted, contested, unchallenged or open |
| `awaiting` | | challenges and questions to an author's posts that the author has not replied to |
| `conclude_discussion` | yes | verdict post, status `concluded`; gated on contested challenges; returns the verdict's link |
| `mark_unresolved` | yes | ends a thread without agreement for a human decision |
| `record_decision` | yes | settles an unresolved thread with a decision post |
| `list_discussions` | | threads, filtered by `vault`, `status`, `subject`, `needs_decision` |
| `decision_queue` | | every unresolved thread in every vault, oldest first |

`append_to_note` and `set_frontmatter` accept an optional `expected_sha256`. Every write returns
`sha256_before`, `sha256` and `changed`; an edit that produces identical bytes writes and logs nothing.

Line endings: no write normalises a note's line endings. Every edit leaves the bytes outside the edited span
untouched, and inserted text takes the note's own ending (CRLF when its first line break is CRLF, else LF);
new notes are LF. Test `writes_keep_crlf_files_crlf` runs every write tool over a CRLF note.

## Every write cites a reason

Every write tool except `open_discussion` and `post` has a required `reason` argument: why the change is
being made. (`open_discussion` and `post` record their post's kind, author and the start of its body as the
reason.) Every write tool except the thread tools also takes an optional `discussion`
(`perf:D-004` or `perf:Discussions/D-004 ....md`): the thread the change comes from. The server checks the
thread exists, stores it on the audit row and the JSONL line, and a short reason that just points at the
thread (`"per perf:D-004"`) is enough. A thread's `read_discussion` then lists those writes as `outcomes`, and
`write_log {discussion: "perf:D-004"}` finds them. The reason is checked before
anything is read or written, and the call is refused if it is missing, not a string, blank, under 12
characters after trimming, over 500 bytes, more than one line, or a stock placeholder (`update`, `fix`,
`n/a`, `none`, `edit`, `minor changes`, `no reason given`, `not applicable`, ... compared case- and
punctuation-insensitively; the list is `PLACEHOLDERS` in `src/tools.rs`). The trimmed reason is stored in
the `writes` row and the `writes.jsonl` line, and never written into the note (an `append_history` line is
exactly the `text` given). `write_log` answers "why did this note change?":
`write_log {vault: perf, path: "Ideas/PI-014 ....md"}`.

## The perf-vault workflow ("How to add an idea")

1. `next_id {vault: perf, folder: Ideas, prefix: "PI-", name: "Short name", content: "---\nid: {{id}}\n..."}`
   takes the number and creates `Ideas/PI-NNN Short name.md` in one step. Two agents can never get the same
   id: allocation runs under the database write lock, skips any number a file in the folder already uses
   (by hand or by a writer outside this server), and the file is created exclusively, retrying on a clash.
2. Status change: `set_frontmatter {values: {status: "parked", tags: [...]}}` then
   `append_history {text: "parked: measured +0.2 ms (PR #123)"}`. Nothing is removed; parking is a
   frontmatter change.
3. Index and area lines: either `append_to_note` / `replace_section` on the hand-written tables, or (once the
   tables are converted, see [[Generated indices]]) `refresh_all_indices`.
4. Evidence on an existing idea: `append_to_note` or `replace_section` with the sha from `read_note`.

## Repeats, line ranges and anchors

With the hooks on, the PreToolUse hook names the session and agent behind the next atlas call. A read tool
called again by the same agent with the same arguments then answers `unchanged since your call #N with these
arguments`, or, when the answer changed and the changed lines are under half of it, only those lines (`+ line`
new, `- line` gone). `fresh: true` on any call gives the whole answer, and every such reply says so. Without the hooks there is
no agent identity, and repeats are compared per server process: one server serves a session and its
subagents, so a subagent may be told `unchanged since call #N` of an answer only its parent holds, and
follows the reply's `fresh: true` to get it. Once a PreToolUse hook has been seen, a call it did not announce
is never answered by reference. A PreCompact hook forgets the session's answers. The work-queue tools
(`decision_queue`, `awaiting`, `journal_status`, `next_id`, the discussion reads, `misses`) and
`recall {lane}` always answer whole.

`read_note` takes `lines` (`A-B`, `A-` or `A`, from 1) or `budget_tokens` (whole lines up to about that many
tokens, the first line always); the header line names the lines shown, and a cut ends with
`+N more lines (lines=A-B)`. Search hits already carry `vault:path:line`. A record `recall {key}` reads, and a
whole record in a `recall {query}` answer, carries `@L1234` (the line of the export file its key names) or
`@path:line`. Pointers do not by default: with `recall.pointer_anchors` on, each ends with `@src:path:line`,
and `read_note {vault: src, path, lines}` reads that part of the vault or source file. Measured on the memory
evaluation, the anchors cost more tokens a question than the line reads they enable save (see the
configuration reference). `read_note` reads a source's files as well as a vault's notes.
