Atlas is authoritative for one kind of data: memories, rules, decisions, lane journals and handoffs. The rest of
atlas is a derived index over files; this part is the reverse, and the Markdown files become generated exports of
it. The design and the measured hook behaviour it rests on are in `docs/design/atlas-c.md`.

- **Its own file**, `memory.store` (default `memory.sqlite` beside the database), `synchronous=FULL`. The index
  database may be deleted and rebuilt; this file never is.
- **Append-only, enforced by SQLite.** Triggers refuse any UPDATE or DELETE of `versions` and any DELETE of
  `records`; a record's head only moves forward. Each version carries the SHA-256 of its text and a per-record hash
  chain, and imported text its provenance (source, file, line, file sha). Every committed batch is also appended to
  `<store>.jsonl`, a plain-text copy.
- **A reversal is a supersede.** The old record stays, recallable, marked `superseded by <key>`, and stays in its
  export with one marker line.

| Tool | Writes | What it does |
|---|---|---|
| `remember` | yes | a memory (key = file name), rule or decision; an existing key gains a version and needs `expected_sha` |
| `recall` | | by `key` (`history: true` for every version), by `query` (FTS5, best first, within `budget_tokens`: the leading records whole (the best, and a second within 0.8 of its score), the rest as pointers, one line each with key, score and title; a body is one `recall {key}` away), or `lane` |
| `supersede` | yes | replaces `old` records with a new one placed right after the first |
| `journal` | yes | a lane entry: `brief`, `decision`, `measurement` (conditions in `meta`), `commit`, `question`, `answered` (`refs: ["answers:<key>"]`), `next`, `note` |
| `handoff` | yes | a lane handoff; also the one tool the context cap lets through |
| `journal_status` | | one row per lane: entries, last entry, last commit, open questions, handoffs |
| `memory_log` | | versions newest first, filtered by key, kind, scope, time, author |
| `hook` | | the hooks' entry point: event fields in, hook JSON (or nothing) out |

`recall {lane}` answers in the order a fresh agent needs: the latest handoff, the brief, open questions, the latest
next step, then decisions, measurements and commits newest first, within `recall.lane_budget_tokens` (2,000).

## Exports

`memory.exports` names each generated file or folder under the private `memory.export_dir`:

| Shape | Layout | Records |
|---|---|---|
| `memory` | a folder: one file per memory (frontmatter then body) and an index file (`index`, default `MEMORY.md`) | `memory`, plus `block` for non-entry index lines |
| `items` | one file of top-level list items between verbatim blocks | `rule`, `block` |
| `headings` | one file of heading sections; with `handoffs: true` new handoffs are added as sections | `section`, `block`, `handoff` |

Every piece is the exact text it was imported or written with (line endings included), so the export of a fresh
import is byte-identical to its source. A file is rewritten only when its bytes change, and never over a file that
differs from what atlas last wrote there: the write is refused and named. `index_cap` cuts each index line to that
many characters (off by default).

`index_topics` (`[{"name": "...", "words": ["..."]}]`) on a `memory` export generates a clustered index instead of
the line-per-memory one: memories whose `metadata.type` is `feedback` or `user` one line each with their description,
its leading attribution trimmed (`Someone 2026-01-02 - `); every other memory by its slug only, project facts grouped
under the first topic with a word in the slug (else in the description, else `Other`), then references and
superseded memories. Every memory is named exactly once (the round trip checks it); recall carries the bodies.

```sh
atlas --config C --memory-import NAME=PATH ...   # into an empty store only; prints counts per kind
atlas --config C --memory-export [--adopt]       # write every export (--adopt: replace files atlas did not write)
atlas --config C --memory-check                  # chains, the .jsonl copy, exports on disk, the round trip
atlas --config C --memory-snapshot               # VACUUM INTO, verified; past snapshot_cap_mb the oldest roll up
atlas --config C --memory-as-of KEY TIME         # a record as it stood at TIME, from the archive, snapshots and store
atlas --config C --recall-scores PROMPTS.tsv     # gate inputs per prompt, and recall timings (dev feature)
```

While the rules file and the memory folder stay hand-written, the store follows them. An `items` or `memory`
export whose name is also a configured source is synced from that source: from the file of the export path's name
under the source root, or from the memory folder that is the root. Every rescan (and every `recall`, which
refreshes those sources first) that finds one of them changed re-imports it; an unchanged source costs a lookup,
and the first rescan in a process compares the indexed hashes with the fingerprint stored at the last sync. History
is never overwritten:

- a new item or memory file becomes a new record, dated and attributed as the item states (`- 2026-11-02 Name:
  ...`; a memory's `metadata.modified`);
- an item whose text changed is superseded by a new record placed after it, so `recall {key}` on the old one
  still answers what the rule was; a changed memory file is a new version of the same key (`recall {key,
  history}`);
- a removed item or memory file retires its record: every version kept, out of the exports and of recall by
  query, still answered by key; a memory file that comes back makes its record active again;
- records with no source item (written through `remember`, `supersede` or the journal) are never touched.

Line endings are not text, and an index line that is a prefix of the stored title is the title cut for length
(as `index_cap` cuts it), not a new title. An export written over its own source is not synced, and `headings`
exports are not. `--recall-check` counts every item or memory file the store does not hold as its source has it
(`memory drift:` lines) as a miss; `--rescan` prints what each sync did under `memory`.

A snapshot is also taken on the first write after `snapshot_hours` (24). `--memory-check` replays every hash chain,
replays the `.jsonl` copy to the same heads, compares each export with the files on disk, and splits each rendered
export back into exactly its records.

Snapshots are kept until they and the archive together pass `memory.snapshot_cap_mb` (1024 MB by default, a
setting). Past it the oldest snapshots are folded, oldest first, into `archive.sqlite` in the snapshot folder and
then removed. The archive keeps every distinct record version once (by key, version number and SHA-256) with its
record, supersede rows and refs; each version's SHA-256 is recomputed before it is copied, and a snapshot is removed
only after every one of its versions is found in the committed archive. The archive is indexed by record and time,
so `--memory-as-of KEY TIME` answers what a record looked like at a time (the newest version written at or before
it, and whether it had been superseded by then) from the archive, the remaining snapshots and the store together;
the answer does not depend on which snapshots have been folded.

## Hooks

`hooks/hooks.json` wires Claude Code `mcp_tool` hooks to the plugin's own server, so no process starts per hook; the
server answers `hook` calls on a worker thread of their own, never behind a slow tool call. `atlas --hook` answers the
same events as a command hook (the event JSON on stdin).

- **Recall** (`UserPromptSubmit` in the main session; `SubagentStart` for subagents, whose brief is taken from the
  parent's `Agent` tool call): a lane the text names gets its journal first; then the gated records, whole, in
  fused order (reciprocal rank over FTS5 and the vectors), within `recall.budget_tokens` (600). A record passes
  lexically when its FTS5 score clears the floor and it contains `recall.min_terms` (3) of the prompt's words; with
  the embedding model (`semantic.model`) loaded the floor is `recall.hybrid_fts_floor` and the record's cosine with
  the prompt's first `recall.query_tokens` (32) tokens must also clear `recall.cos_floor`, and a cosine at or above
  `recall.cos_alone` passes on meaning alone. Without the model the floor is `recall.fts_floor`. Records are
  embedded once per version on a background thread (cached in the store's `rec_vec` table); until a record has a
  vector it is judged lexically. Nothing clears the gate: nothing is injected. A record is injected once per
  session. The floors are set by `cargo xtask calibrate-recall` from `--recall-scores` output, never by hand.
- **Context cap** (`PreToolUse`, subagents only unless `cap.main_session`): the size is the last assistant
  message's input, cache and output tokens in the subagent's own transcript
  (`<parent>/subagents/agent-<id>.jsonl`). At `cap.warn_tokens` (450,000) the agent is told to finish, commit, push
  and write its handoff; at `cap.block_tokens` (500,000) every tool but `handoff` is denied. Both are journal
  entries.
- **Token ledger:** every hook answer is a `token_log` row (`hook:<event>`), empty ones included, with the
  counterfactual of reading the injected records without atlas.

## Rulings and log entries

Two commands write records with explicit fields, so atlas never has to infer what a line defines:

```sh
atlas ruling --by NAME [--defines ID ...] [--ref REF ...] [--date YYYY-MM-DD] TEXT
atlas entry --title TITLE [--ref REF ...] [--date YYYY-MM-DD] [--time HH:MM] TEXT
```

A ruling renders `- DATE BY: TEXT` and, when it defines identifiers, the marker `<!-- atlas defines: ID ... -->`
after it (invisible when the Markdown is rendered); the registry reads the marker as a definition above every
prose position. An entry renders `- DATE TIME TITLE: TEXT`. Each command writes the record to the memory store,
appends the line to its file under a dated heading atlas owns (`## DATE rulings (atlas)`, `## DATE entries
(atlas)`), and indexes the file at once. The files are `lean.records.rulings` and `lean.records.entries`, else
`INTENT.md` and `LOG.md` in the first vault.
