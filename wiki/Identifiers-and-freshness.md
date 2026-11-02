## Documents and sections

Every vault note and every source file is a *document* (`docs`), cut into *sections* (`sections`, with
`sections_fts` over the heading path and body). Sections are leaves and never overlap: a section starts at a
heading (up to `max_level`) or, with `top_level_items`, at a list item at column 0, and runs to the next start;
text before the first start (frontmatter included) is section 0. Headings and items inside fenced code or the
frontmatter never start a section. Frontmatter `title`/`name`, `description` and `metadata.type` (or `type`)
are kept per document, and a date (`date`, `created`, `opened`, `updated`, `modified` or `metadata.modified`).

## The identifier registry and the definition heuristic

Every line is scanned, frontmatter and fenced code included, so the registry never finds less than grep:

- each configured pattern, line by line (`^` and `$` mean the line, as in grep); a group named `id` selects
  the identifier; at most one occurrence per identifier per line;
- each *name* (a file stem of a `names_kind` source, `[A-Za-z0-9_-]`, with a letter) wherever it stands as a
  whole word;
- each `[[wikilink]]` target (last path segment, `#heading`, `|alias` and `.md` dropped), and each document's
  own file stem, as `note` identifiers.

An occurrence is a **definition** when one of these holds, strongest first; otherwise it is a mention:

| Rank | `def` | Rule |
|---|---|---|
| 5 | `frontmatter` | a frontmatter `key: value` line whose key is one of the source's `id_keys` and whose value is the identifier |
| 4 | `file` | the file stem is the identifier, or starts with it and a space (`TK-004 Login.md`) |
| 3 | `heading` | the identifier is the *first* identifier on a heading line (a heading is about its first id; later ones are cross-references) |
| 2 | `table_row` | the first identifier on a table row, inside its first cell |
| 1 | `list_item` | the first identifier on a list item, with only markup (`*` `_` `` ` `` `~`) before it, followed by the end of the line, `:`, ` -`, ` (`, ` =` or a dash: `- **TK-3** (2026-02-01): ...` defines, `- see TK-3` and `- [[TK-3 X]]` cite; a leading ISO date (optionally with a time) counts as markup, so `- 2026-10-30 TK-3: ...` defines |

`lookup(id)` matches case-insensitively and returns definitions by rank, then date, then file mtime, each
with its defining text (the section for a heading, the item with its continuation lines for a list item, the
row for a table row, the first two sections for a file or frontmatter definition; at most 8 KiB). Mentions
follow, newest first: an occurrence's date is the first `YYYY-MM-DD` on its line, else of its section's
heading path, else of its document. The heuristic is tested in `registry::tests` and `tests/atlas.rs`.

## Never assume complete

**Freshness.** Atlas tools do not run the blanket per-call rescan. Each answer checks the sources it drew on
(all of them when the answer is empty or ranked across sources). The server watches every root for changes
(`src/watch.rs`) and marks the changed paths dirty, so a check reads only the dirty set: nothing to do when
nothing changed. A root is walked whole (stat only) the first time, when the watch overflowed or failed, when a
folder was created, renamed or removed, and every `rescan_interval_s` as a safety net; without a watch (`watch:
false`, or a platform with no backend) every check walks. The check compares mtime and size with the indexed
row, hashes only files whose stat changed, and reports the `stale` files (`changed`, `new`, `removed`). With `refresh: "auto"` (the
default) up to `refresh_limit` stale files are re-indexed on the spot and the answer recomputed (`refreshed:
true`); a source with nothing indexed yet is indexed whole. `refresh: "check"` reports without re-indexing;
`"off"` skips the walk.

**Misses.** `report_miss` records what atlas failed to return and where grep found it; `misses` lists them;
`resolve_miss` closes one with a note. The `misses` table is never dropped, and every report and resolution is
appended and fsynced to `misses.jsonl` beside the database, which is replayed into an empty table, so deleting
the database loses no miss.

**Recall.** `src/recall.rs` re-reads every indexed file and scans it independently of the index: each pattern
line by line, each name by plain substring search with word boundaries, each wikilink by plain `[[`/`]]`
search, and every heading's text as a phrase. Any occurrence `lookup` does not return, and any document holding
a phrase that section search does not return, is a miss (`stage: index` when the occurrence is not indexed,
`lookup` when it is indexed but not returned). `tests/atlas.rs` runs it over the fixture corpus (and checks it
against a direct regex scan, and that a deleted index row is caught); `atlas --recall-check` runs it over
the configured sources and exits 1 on any miss.

## Performance

`atlas --bench [RUNS] [--queries FILE] [--before EXE CONFIG]` (a `dev` feature build) prints, on the configured sources: a cold index
into a fresh database, the open of a warm one, no-change rescans and freshness checks with and without the
watch, `lookup` and `ids` with the old statements and the new, the JSON encode of an answer, a section search,
whole answers; and for a fixed query set (a file of `lookup ID`, `search TEXT` and `ids PREFIX` lines) the
tokens of each answer as text, as JSON, from an older binary given with `--before`, and the grep counterfactual,
plus the size of `tools/list`. `ids` reads `ident_stats`, a per-identifier summary kept up to date as
documents are indexed. `lookup` reads `lookups`, a table holding every identifier's definitions and mentions
already merged per section and already in answer order (its primary key is the order: folded identifier, rank,
date, file mtime, source, path, line), so an answer is one indexed range scan that stops after the definitions
and the mention limit; `lookup_counts` holds the totals past the limit. Each definition row also holds its
defining text, cut (section, list item, row, or the first two sections) and capped at index time from the
document, so a lookup with text is the same one statement. Both tables are rewritten per document as it is
indexed, touched or removed, and the bench checks on every identifier that the answer, text included, equals
the grouping statement over `idents` with the text read back from the sections. A JSON answer is written once,
straight from the results, and the same text goes to the token ledger and the client. A walk without a watch compares the disk with the indexed path map of
the last walk, kept in memory while the database is unchanged (`PRAGMA data_version` and this connection's
row changes), instead of reading every row back. The server opens the database with one read when nothing
changed (an `open_stamp` in `meta`), and does not scan at startup.

SQLite pragmas: WAL; `synchronous=NORMAL` (a commit survives a process crash; only the last commits can be
lost on power failure, and the derived rows are rebuilt from the files while the audit and misses lines are
fsynced to their JSONL mirrors); `mmap_size` 256 MiB (index reads without a copy); `cache_size` 16 MiB;
`temp_store=MEMORY`; 128 prepared statements cached per connection (the default 16 is fewer than one session
uses, and an evicted statement is compiled again).
