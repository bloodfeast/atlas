## Six are always loaded

`lookup`, `search`, `recall`, `section`, `grep` and `symbol` are marked always-loaded
(`_meta["anthropic/alwaysLoad"]: true` in `tools/list`, Claude Code 2.1.285 and later), so their schemas are
in context from the first turn and the model calls them without a tool search first. Every other tool stays
deferred: only its name is in context until the model searches for it.

## Answers are compact text

`lookup`, `ids`, `search`, `misses` and `list_sources` answer in plain text by default; `format: "json"` gives
the JSON instead, and `verbose: true` adds scan times and timings. The text (`src/text.rs`):

- A list is a header row of field names, then one tab-separated row per record, so names appear once. Inside a
  value a tab is written `\t`, a newline `\n` and a carriage return `\r`; nothing else is escaped.
- A snippet is about 120 characters centred on the matched identifier or word, cut on character boundaries,
  with `…` where it was cut. Repeat mentions in one section are one row with a count (`n`).
- Default limits are small (`lookup` mentions 3, `search` 50 lines or 5 sections, `ids` 50, `misses` 20) and a cut list ends with
  `+N more (limit=L)`. The query is not echoed.
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
it never enters a per-answer ratio.

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
| `read_note` | | text in the `content` field, `sha256`, size, parsed frontmatter |
| `search` | | `substring` (default), `regex`, or `fts` (SQLite FTS5, ranked) over vault notes, returning vault, path, line, snippet; or `sections`: ranked FTS5 over the sections of every vault and source, returning whole sections with heading path and line range, plus `freshness`; `semantic` ranks the same sections by meaning, `hybrid` fuses both rankings (see the Semantic search page) |
| `symbol` | | a Rust or WGSL name (or `Type::name`): its definitions, then references with the function each sits in (the caller), then impl edges, each `root:path:line` with a snippet (see the Code index page) |
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
