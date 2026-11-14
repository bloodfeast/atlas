Atlas is authoritative for one kind of data: memories, rules, decisions, lane journals and handoffs. The rest of
atlas is a derived index over files; this part is the reverse, and the Markdown files become generated exports of
it. The design and the measured hook behaviour it rests on are in `docs/design/atlas-c.md`.

- **Its own file**, `memory.store` (default `memory.sqlite` beside the database), `synchronous=FULL`. The index
  database may be deleted and rebuilt; this file never is.
- **Append-only, enforced by SQLite.** Triggers refuse any UPDATE or DELETE of `versions` and any DELETE of
  `records`; a record's head only moves forward. Each version carries the SHA-256 of its text and a per-record hash
  chain, and imported text its provenance (source, file, line, file sha). Every committed batch is also appended to
  `<store>.jsonl`, a plain-text copy, under the batch's write lock and before its commit: the store records where
  the committed lines end, so lines a crash left past it (appended, never committed) are cut off by the next
  write, and the copy always holds exactly the committed batches, in commit order.
- **A reversal is a supersede.** The old record stays, recallable, marked `superseded by <key>`, and stays in its
  export with one marker line.

| Tool | Writes | What it does |
|---|---|---|
| `remember` | yes | a memory (key = file name), rule or decision; an existing key gains a version and needs `expected_sha` (the head's sha, or its first 12 or more hex digits, as the ok line prints 16); a rule is one list item (a body without a leading `- ` is written as `- DATE AUTHOR: text`, later lines indented; blank lines or a second item are refused) |
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
export back into exactly its records. A process killed between a write's commit and its export leaves that export
behind the store: `--memory-check` reports `disk: ... differs from its export` and exits 1 until the next write to that
export (or `--memory-export`) rewrites it. No record is lost; the store and its `.jsonl` copy hold the write.

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
same events as a command hook (the event JSON on stdin), one process per event, with one exception: a subagent's
brief (the Agent call's prompt) is held by the server between the `PreToolUse` that carries it and the
`SubagentStart` that injects its recall, so as a command hook `SubagentStart` has no brief and injects no lane
recall. Use the `mcp_tool` hooks for subagent recall.

- **Recall** (`UserPromptSubmit` in the main session; `SubagentStart` for subagents, whose brief is taken from the
  parent's `Agent` tool call): a lane the text names gets its journal first; then the gated records, whole, in
  fused order (reciprocal rank over FTS5 and the vectors), within `recall.budget_tokens` (600; with the 0.53 floor below, chosen by the cos_floor sweep on the off-topic set and the hook replay). A record passes
  lexically when its FTS5 score clears the floor and it contains `recall.min_terms` (3) of the prompt's words; with
  the embedding model (`semantic.model`) loaded the floor is `recall.hybrid_fts_floor` and the record's cosine with
  the prompt's first `recall.query_tokens` (32) tokens must also clear `recall.cos_floor` (0.53), and a cosine at or above
  `recall.cos_alone` passes on meaning alone. Without the model the floor is `recall.fts_floor`. Records are
  embedded once per version on a background thread (cached in the store's `rec_vec` table); until a record has a
  vector it is judged lexically. Nothing clears the gate: nothing is injected. A record is injected once per
  session (or per subagent), across server restarts and compactions: a session's earlier injections are read
  back from the `hook_inject` log, so a command hook's process per event does not repeat them either (a lane's
  journal can come again after a restart). A main-session prompt gets at most `recall.prompt_pointers` (2) pointers, the rest counted in
  a `+N more` line; a subagent whose brief names its own lane gets that lane's journal and no pointers. The floors are set by `cargo xtask calibrate-recall` from `--recall-scores` output, never by hand.
- **Context selection** (`recall.select`, on by default): what clears the gate is cut to what the prompt needs.
  A record whose words the record injected before it holds (`dup_overlap`, 0.90, one way) at a cosine of
  `dup_cos` (0.82) or more is sent as `- KEY: near-duplicate of OTHER`, so the rule kept in INTENT, a memory
  file and a restore point is sent once. A pointer carries the clause of its record that holds the most prompt
  words (after a dated lead such as `2026-10-30 Brandon:`), up to `trim_chars` (90), in place of its first line;
  a lane brief keeps its handoff, brief, open questions and next step whole and cuts older entries to their
  matching clauses (`lane_chars`, 180), with a `+N trimmed above` line. Every piece of an excerpt is an exact
  substring of the record, joined by ` ... `, so a quote of it is the record's own text and `recall {key}` reads
  it whole. A record the lane brief shows is not pointed to again. Clauses are chosen by the prompt's words, not
  by embedding each clause: with the bge int8 model, the cosines did not separate the answer from an off-topic
  clause.
- **Context cap** (`PreToolUse`, subagents only unless `cap.main_session`): the size is the last assistant
  message's input, cache and output tokens in the subagent's own transcript
  (`<parent>/subagents/agent-<id>.jsonl`). Each agent kind (builder, reviewer, research) has its own limits;
  by default, on a 1M window and scaled to the window, at 285,000 the agent is told to wrap up and write its
  handoff at its next clean stopping point, and at 450,000 every tool but `handoff` is denied. Both are journal
  entries. A SendMessage to an agent past its hard limit is refused, and when an agent that passed its handoff
  point finishes, the orchestrator is told to start a fresh agent from the handoff. `orchestrate.mode` makes it
  advisory or off.
- **Agent caps** (`PreToolUse` on `Agent`, with `orchestrate.enabled`): the session's running agents are read
  from Claude Code's `subagents/agent-<id>.meta.json` files and counted by kind; a launch past its kind's cap
  (2 builders, 2 reviewers, 1 research agent without setup; setup derives them from the machine, see [[Orchestration]]) or past Claude Code's own limit is denied with a reason
  to queue it. The hook files the refused launch as a `queued` journal entry in the lane its brief names (else
  lane `queue`), with the brief as its body, and the reason names the key; a retry is not filed twice. A
  finished agent's notification lists the oldest waiting launches, and launching one again with the same
  description and brief retires its entry. `atlas queue [list | show KEY | drop KEY]` lists, prints or removes
  them. The hook only ever denies; on any error it gives no decision. The `orchestrate` skill teaches the
  main session to work this way. `atlas --setup` asks whether to turn it on and its options, and the first
  new session asks the same through Claude until it is answered (`atlas settings orchestrate`; see [[Install and setup]]).
- **Resume card (`memory.resume_card`, on by default):** SessionStart with `source: startup` or `clear` injects
  where the work stands as keys, in at most 1,160 bytes (612 tokens on the test's card full of keys, at 1,119 bytes, by a public
  vocabulary times a fitted factor, not real tokens; `cargo test -p xtask` holds that card there, the bytes bind at runtime). The lines: the six newest live lanes, each with its open question count and newest
  handoff; the live lanes' open questions; after a `/clear` only, the agents still running from the previous
  session (a new process has none, so none hold slots); the work queue's count, up to five that can start now
  and the next waiting; the previous session's goal when its newest `goal_status` says not met; and handoffs
  the lanes line does not name. A lane is live when it has a work record in the last 7 days: an active journal
  entry or handoff that atlas did not write itself (queue entries and notes, the hook's cap and compaction
  notes); a note counts only while no queued entry waits in its lane, since there it adds to a brief. A line past
  the budget is cut at an item, and the card ends by asking Claude to put one multiple-choice question to the
  user: resume, change the plan, or something more pressing first. `recall {key}` reads any key whole. An empty
  store gives no card, and a store error drops only the card. Two limits: the previous session is the newest
  other transcript in the folder, so a second session open in the same project counts as the previous one; and
  a headless `claude -p` run starts with `startup` too, so it gets the card (turn it off in that run's config).
- **Compaction (on by default):** everything in this item and the next three, except the transcript
  source, runs unless `"compact": {"enabled": false}` is in atlas's configuration. Off, PreCompact only
  forgets the session's repeated answers (and, with `memory.transcripts`, indexes the transcript); SessionStart
  injects no card or ref report.
- **Idea capture (`memory.ideas`, off by default):** a person's standalone statements that propose, decide or
  constrain something are filed as pending `idea` journal entries with their exact words, redacted, so an idea said
  in passing is not lost. The detector is a no-model linear score whose weights are data; they come from one author's
  labelled prompts, label error limits them, and generality to other users is untested. Off, it reads nothing, writes
  nothing and adds nothing to context; the Stop and `AskUserQuestion` hooks still make a round trip that does no work.
  A denylist file that cannot be read keeps it off, and with no denylist named only the secret rules redact. UserPromptSubmit spools the capture and answers; the store takes it once the
  answer is out and the transcript's record says `origin.kind` is `human` outside a headless run, so `/loop`
  iterations, scheduled tasks, other sessions' messages and `claude -p` prompts are dropped. Only a record written
  after the hook ran counts, as an older prompt may start with the same words. A prompt whose record
  never turns up in 10 minutes is kept as `origin: unverified`, in `atlas ideas list --unsure` only. Check-ins, task
  notifications and the resume card count and name the last 7 days, framed as records rather than requests, once per
  new capture per session (a line of at most 300 bytes); older ideas stay pending in `atlas ideas list`.
  The question below is unproven in a live session, so `asks_per_day` defaults to 0: nothing asks and the band is off
  until it is set. With it set, scores from 0.25 below to 0.5 above the threshold are unsure, and the band only adds
  to what is captured. At a Stop
  after a person's prompt, with no background tasks, no scheduled wakeups and no active goal, the hook blocks once and
  has the assistant ask, by `AskUserQuestion`, whether the text is a standalone statement; the answer returns through
  PostToolUse and is stored as a label of source `live`, and free text typed there is filed as a capture. At most
  `asks_per_day` a day, on captures of 600 characters or fewer. A question holds the main thread until it is answered,
  and costs at least two more model calls at the session's context. A settings-based install needs the Stop hook and
  `AskUserQuestion` in the PostToolUse matcher; with ideas on, the drift line says when either is missing. CLI:
  `atlas ideas list [--unsure|--all] [--limit N]`, `show ID`, `file TEXT`, `dismiss ID` (source `dismiss`), `answer ID yes|no`
  (source `manual`), `convert ID ruling --by NAME`, `convert ID queue`, and `labels` (tab separated, no text: id, label,
  source, time, asked day, score, weights sha). Asked rows were chosen by band, so they are not a random sample. The
  same words are one record: a dismissed statement said again stays dismissed. Weights change only by an explicit
  later step, scored on rows newer than those it trained on.
- **Compaction** (`PreCompact`, then `SessionStart` with `source: compact`): PreCompact writes a checkpoint
  journal entry in lane `session:<id>` and sets the session's lookups aside. After the compaction the hook reads
  the summary Claude Code wrote (the transcript's `isCompactSummary` record), resolves every content-addressed
  ref in it (`key@hash`, `src:path:a-b@hash`; see [[Tools reference]]), and injects one line per changed or
  dangling ref, nothing when all resolve. Each compaction is a `compact_log` row (`preTokens`, `postTokens`,
  summary size, refs, changed, dangling, the hook's own time). A main-session lookup (a Read, Grep, Glob or MCP
  lookup with the same target) made before the compaction and again after it is a `compact:miss` row.
  `--token-report` ends with the totals; `atlas --compact-report --transcripts DIR` replays recorded sessions and
  prints the same numbers, never transcript text. Design: `docs/design/compaction.md`.
- **Instructions for the summary:** once the checkpoint is written, PreCompact replies with the Compact instructions
  as plain text (1,344 bytes, 336 tokens), by both `atlas --hook` and the `hook` tool; Claude Code adds that text to
  the summarizer's instructions (after any `/compact` argument, as read from Claude Code's binary, not observed). A
  failed checkpoint, or one with no chain card
  (a transcript atlas could not read), replies with nothing, and Claude Code compacts with its own instructions.
  What is known: one manual run each on Claude Code 2.1.318 steered the summary, a command hook's stdout, an
  `mcp_tool` stub's return and the real atlas tool, with no plain control on that version. The automatic trigger is
  untested. On a manual `/compact` the text also lands in the conversation as `local-command-stdout`, about 340 tokens
  on each later request until the next compaction; the automatic path is unread.
- **Checkpoint and card:** the PreCompact checkpoint is a `journal` entry of kind `checkpoint` whose body is ref
  lines: `since` (the previous checkpoint), `lanes` (each active lane and its current step), `open` (unanswered
  questions), `owed` (`next` entries no commit followed), `rulings` (rules written since), then the in-task state
  read from the transcript since the last compaction: `files` touched, `tools` (the last six calls, each with its
  result's `turn:<session8>:<line>@hash` ref), `pending` (a call still running) and `intent` (the last line of
  visible assistant text). Thinking blocks are never read; every transcript-derived line passes secret redaction
  (`src/redact.rs`: denied file names, key-shaped tokens, `.env`-style assignments, the `ATLAS_LEAK_DENYLIST`
  terms). After the compaction SessionStart injects the newest checkpoint as a card headed by its own ref, within
  `compact.card_tokens` (default 400), before the ref report.
- **Which lanes the card lists:** with a project active (a configuration with `projects`), the store is the
  project's own, so the card lists its lanes and rulings. With no project the store may hold every project's
  lanes, so the card keeps to the session's own lane (the previous checkpoint) plus the transcript side. An
  unknown segment start (an empty or unreadable transcript) is the 24 hours before the compaction, never all
  time.
- **Transcript not yet written:** a session forked and compacted at once can fire PreCompact before Claude Code
  has written its transcript. PreCompact re-reads it for up to 1.5 s; if it still holds no turn it reads the
  session it was forked from, the only other transcript in the same folder written in the last ten minutes
  (named in the checkpoint's `tail_from:` meta line). With no such single sibling the checkpoint's title says
  `transcript unread` and its body has an `unread:` line, never a silent `at 0 context tokens`.
- **Transcript source (`memory.transcripts`, off by default):** PreCompact queues the session transcript, indexed
  once the answer is out (a command hook does it after printing): one turn per record's visible text, redacted,
  each row tagged with its project (the transcript's folder) so no other project's server serves it. The ref check
  after a compaction resolves `turn:` refs against the transcripts beside the session's own.
- **Deferral (`compact.defer`, off by default):** PreCompact with `trigger: auto` answers `decision: block` while
  the transcript's last assistant record is a tool call (a chain is running), unless the context is within the
  ceiling's margin of the window (three times the largest per-request growth, at least 40,000 tokens). A manual
  `/compact` is never blocked. Each deferral is a `compact:defer` token-log row. It stays off until a live check
  confirms Claude Code offers a blocked compaction again.
- **Compact instructions:** `atlas settings compact-md [DIR] [--yes]` shows the block for `DIR/CLAUDE.md`
  (default the current folder) and its diff, asks, and writes it with a backup; `atlas settings undo` removes only
  the block (and the file, when atlas created it). This command is the only way in: neither SessionStart nor
  `atlas settings offer` raises it. It is the same text the hook returns, so a project with the block gets the text
  twice and should drop the block (`atlas settings undo`, or delete it by hand).
- **Token ledger:** every hook answer is a `token_log` row (`hook:<event>`), empty ones included, with the
  counterfactual of reading the injected records without atlas.

## Rulings and log entries

Two commands write records with explicit fields, so atlas never has to infer what a line defines:

```sh
atlas ruling --by NAME [--defines ID ...] [--ref REF ...] [--date YYYY-MM-DD] TEXT
atlas entry --title TITLE [--ref REF ...] [--date YYYY-MM-DD] [--time HH:MM] TEXT
```

A `--defines` or `--ref` list takes words until the next flag or a quoted text (a word with a space); when it
runs to the end, its last word is the text. `-- TEXT` always marks the text.

A ruling renders `- DATE BY: TEXT` and, when it defines identifiers, the marker `<!-- atlas defines: ID ... -->`
after it (invisible when the Markdown is rendered); the registry reads the marker as a definition above every
prose position. An entry renders `- DATE TIME TITLE: TEXT`. Each command writes the record to the memory store,
appends the line to its file under a dated heading atlas owns (`## DATE rulings (atlas)`, `## DATE entries
(atlas)`), and indexes the file at once. The files are `lean.records.rulings` and `lean.records.entries`, else
`INTENT.md` and `LOG.md` in the first vault.
