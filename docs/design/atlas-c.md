# atlas memory store, lane journal and hooks (design)

Status: design, phase 1. Nothing here is implemented yet. Section 1 is measured; everything after it is a
proposal built on those measurements.

Atlas today is a derived index: the files are the source of truth and the database can be rebuilt from them.
This design adds the first thing atlas is authoritative for: memories, rulings, handoffs and per-lane
journals. Because a bug in it could lose a ruling, durability comes first, and every choice below is
measured against "can a record be lost or silently changed?".

## 1. What the hooks actually do (measured)

Runs: Claude Code 2.1.314, headless (`claude -p`, a small model), in a throwaway project whose
`.claude/settings.json` registered a logging hook on every event, plus a second project whose hooks were
`mcp_tool` hooks against a test MCP server. Each run had the main agent spawn one `general-purpose`
subagent that ran two Bash commands. Payloads were logged verbatim and the transcripts read afterwards.

### 1.1 Which events fire inside a subagent

| Event | Fires for a subagent's own work | `agent_id` in input | `transcript_path` is | Can steer the subagent |
|---|---|---|---|---|
| PreToolUse | yes, every tool call | yes (+ `agent_type`) | the PARENT's transcript | yes: `permissionDecision: deny` blocked the call; `additionalContext` reached the subagent |
| PostToolUse | yes | yes | the parent's | yes: `additionalContext` reached the subagent |
| UserPromptSubmit | no (the subagent's brief does not pass through it) | n/a | n/a | n/a |
| SubagentStart | yes, once, in the parent's process before the subagent's first request | yes | the parent's | yes: `additionalContext` reached the subagent; cannot block. No brief in the input |
| SubagentStop | yes | yes | the parent's; `agent_transcript_path` is the subagent's own | not tested (can block like Stop per the docs) |
| Stop | no (SubagentStop fires instead) | n/a | n/a | n/a |
| PreCompact | not observed in a subagent (a subagent could not be made to compact cheaply); fires in the main session with `trigger`, `custom_instructions` | n/a | the main session's | n/a |
| SessionStart / SessionEnd | main session only | no | the main session's | SessionStart `additionalContext` (main only) |

Further observations that shape the design:

- **The subagent's own transcript is derivable at every tool call.** It is
  `<transcript_path without .jsonl>/subagents/agent-<agent_id>.jsonl`. At the subagent's first PreToolUse
  it already existed and held its brief (first `user` record) and the assistant message that issued the call.
  It does NOT exist yet at SubagentStart. SubagentStop confirmed the path (`agent_transcript_path` equals the
  derived one).
- **Background subagents.** When a subagent runs in the background, the parent's Stop fires before the
  subagent works, and its completion re-enters the parent as a UserPromptSubmit whose `prompt` starts with
  `<task-notification>` and carries `<task-id>` (the agent id).
- **Compaction runs as an unnamed subagent.** `/compact` fired PreCompact (`trigger: manual`), then a
  SubagentStop with an empty `agent_type` whose `last_assistant_message` is the summary, then SessionStart
  with `source: compact`. No SubagentStart.
- **SessionStart on resume carries `context_tokens`.**

### 1.2 Usage records give the real context size

Each assistant record in a transcript has `message.usage`. One API response is written as several records
(one per content block) with the same `message.id` and the same usage, so records are de-duplicated by
`message.id` and the last one is taken.

`input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens` of the last
assistant message equals Claude Code's own `context_tokens` exactly: 32,689 = 32,271 + 418 and
33,583 = 33,267 + 316 on two resumes of the same session. In the subagent transcript the sum rose
monotonically per request (21,836, 24,210, 24,642). So the context size is scripted from the agent's own
transcript, never estimated. The docs warn the transcript is written asynchronously and may lag; at
PreToolUse the record of the message that issued the call was always present, so the figure lags by at most
the tool results not yet sent.

### 1.3 `mcp_tool` hooks reach a running MCP server

Claude Code has a hook type that calls a tool on an already-connected MCP server instead of spawning a
process (`"type": "mcp_tool", "server", "tool", "input"`, with `${path}` substitution from the hook's input).
Measured:

- `${prompt}`, `${transcript_path}`, `${agent_id}`, `${tool_name}` and `${tool_input.command}` were
  substituted intact (quotes and newlines included), and `agent_id` was present for a subagent's tool calls.
- The tool's text is parsed like a command hook's stdout. Text that is hook JSON
  (`{"hookSpecificOutput": {...}}`) took effect: UserPromptSubmit and SubagentStart `additionalContext` reached
  the model, and a PreToolUse `permissionDecision: deny` blocked a subagent's Bash call. Plain text (an atlas
  `search` answer) was logged as success and NOT shown to the model. So hook answers must be hook JSON.
- Round trip, Claude Code's debug log from "calling" to the parsed result: 2 to 11 ms against a Python
  test server (the first call of a session is the slow one). A command hook running Python took 160 ms.
- `mcp_tool` hooks are skipped on SessionStart at launch (no MCP client yet) and on Setup; they run on
  SessionStart after `/clear` or compaction. The plugin-scoped server name is `plugin:<plugin>:<server>`.

Consequence: inside Claude Code, the session's own atlas MCP process is already the resident atlas. The hook
round trip needs no process spawn and no extra transport (section 6 and decision D1).

## 2. The store

### 2.1 A separate, durable database file

Memory lives in its own SQLite file (`memory.store`, default `state/memory.sqlite`), not in the index
database. The index is derived and may be deleted and rebuilt at any time (schema changes already do this);
the memory store never is. The memory file runs `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON`,
and is ATTACHed to the index connection only for joint queries. Snapshots are of this small file alone.

### 2.2 Schema

```sql
-- One row per record. Never deleted. `head` moves forward only.
CREATE TABLE records(
    rid INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,        -- stable name: a memory slug, `rule:000417`, `journal:<lane>:<n>`, ...
    kind TEXT NOT NULL,              -- memory | rule | handoff | journal | decision | block
    scope TEXT NOT NULL DEFAULT '',  -- lane id for journal and handoff records; export name for blocks
    status TEXT NOT NULL,            -- active | superseded
    head INTEGER NOT NULL,           -- vid of the current version
    created TEXT NOT NULL
);
-- Every version of every record, append-only.
CREATE TABLE versions(
    vid INTEGER PRIMARY KEY,
    rid INTEGER NOT NULL REFERENCES records(rid),
    seq INTEGER NOT NULL,            -- 1, 2, ... per record
    time TEXT NOT NULL,              -- UTC
    title TEXT NOT NULL,             -- one line: memory description, ruling first line, journal headline
    meta TEXT NOT NULL,              -- canonical `key: value` lines (frontmatter, measurement conditions)
    body BLOB NOT NULL,              -- exact bytes, line endings untouched
    sha TEXT NOT NULL,               -- SHA-256 of (title, meta, body) framed by lengths
    chain TEXT NOT NULL,             -- SHA-256(previous version's chain || sha): a per-record hash chain
    reason TEXT NOT NULL,            -- same rules as the vault write tools' `reason`
    author TEXT NOT NULL,            -- free text: who asked (the user, a lane)
    client TEXT NOT NULL, pid INTEGER NOT NULL,
    src TEXT, src_path TEXT, src_line INTEGER, src_sha TEXT,   -- provenance: where an imported text came from
    UNIQUE(rid, seq)
);
CREATE TABLE supersedes(new_rid INTEGER NOT NULL, old_rid INTEGER NOT NULL, time TEXT NOT NULL,
                        reason TEXT NOT NULL, PRIMARY KEY(new_rid, old_rid));
CREATE TABLE refs(rid INTEGER NOT NULL, kind TEXT NOT NULL, target TEXT NOT NULL);  -- lane, commit, pr, record
CREATE TABLE placement(export TEXT NOT NULL, ord INTEGER NOT NULL, rid INTEGER NOT NULL,
                       PRIMARY KEY(export, ord));   -- where a record sits in a generated file
CREATE VIRTUAL TABLE records_fts USING fts5(title, body, content='', tokenize='unicode61');  -- head text
CREATE TRIGGER versions_append_only_u BEFORE UPDATE ON versions BEGIN SELECT RAISE(ABORT, 'versions are append-only'); END;
CREATE TRIGGER versions_append_only_d BEFORE DELETE ON versions BEGIN SELECT RAISE(ABORT, 'versions are append-only'); END;
CREATE TRIGGER records_no_delete BEFORE DELETE ON records BEGIN SELECT RAISE(ABORT, 'records are never deleted'); END;
```

Append-only is enforced by SQLite itself (the triggers), not only by the code that writes. Every version is
also appended, in the same commit order, to `memory.jsonl` beside the database (the vault tools'
`writes.jsonl` pattern): a second, plain-text copy that `--memory-check` replays.

### 2.3 Tools

All answers are model-facing text: plain lines, or a header row plus tab-separated rows.

| Tool | Writes | What it does |
|---|---|---|
| `remember` | yes | `kind`, `key` (optional: allocated for rules and journal entries), `title`, `body`, `meta`, `scope`, `refs`, `reason`. A new key creates a record; an existing key appends a version (requires `expected_sha` of the head, as `replace_section` does). Returns key, seq, sha |
| `recall` | | by `key` (the head, `history: true` for every version), by `query` (FTS5 + semantic hybrid over head text, active records, `kind`/`scope` filters, `budget_tokens`), or `lane=<id>` (section 3) |
| `supersede` | yes | `old` key(s), the new record's fields, `reason`. Creates the new record and marks the old one `superseded` in one transaction; the old text stays recallable and stays in the exports, marked |
| `journal` | yes | `lane`, `entry` (brief, decision, measurement, commit, question, answered, next, handoff), `text`, `meta` (measurement conditions, branch, worktree, commit). Sugar over `remember` with kind `journal` or `handoff` |
| `handoff` | yes | a `handoff` record for a lane: `lane`, `text`, `refs` (prior handoff, commits, rules carried). Its own tool name so the context-cap hook can let it through (section 8) |
| `journal_status` | | one row per lane: last entry time, last commit, open questions |
| `memory_log` | | versions newest first, filters `key`, `kind`, `scope`, `since`, `author` |
| `hook` | | the hooks' entry point (section 7, 8): event fields in, hook JSON or nothing out. Not for models |

A reversal is always `supersede`, never an edit that removes the old text. An edit (`remember` on an
existing key) is for corrections and keeps the prior version.

## 3. The lane journal

A lane is a unit of agent work (a branch and a worktree, usually). Its journal is the ordered set of records
with `scope = <lane>`: `journal` entries (brief, decision, measurement with conditions, commit with branch,
worktree and hash, question, answered, next step) and `handoff` records. A handoff record links (`refs`) to
its lane, the prior handoff, its commits and the rules it carries.

`recall {lane: "<id>"}` is the first act of a fresh agent. It answers, in this order and within a budget
(default 2,000 tokens, scripted from bytes):

1. the latest handoff, whole;
2. the brief (latest `brief` entry), whole;
3. open questions (a `question` without a later `answered` that refs it);
4. the latest `next` entry;
5. decisions, newest first;
6. measurements, newest first, each with its conditions;
7. commits, newest first;

then `+N more` lines for anything cut. The orchestrator reads lane state the same way instead of reading
transcripts, and `journal_status {lanes}` (one row per lane: last entry time, last commit, open questions)
shows which journals are behind.

## 4. Exports

The store is authoritative; the Markdown files become generated views of it. Exports are written to a
private data folder named by config (`memory.export_dir`), never inside the atlas repository.

| Export | Shape | Built from |
|---|---|---|
| memory index (`MEMORY.md`) | one line per active memory, `- [title](key.md) - description`, in `placement` order | `memory` records |
| memory files (`<key>.md`) | frontmatter (`name`, `description`, `metadata.type`) then the body bytes | `memory` records, one file each |
| rulings (`RULES.md`) | headings and paragraphs (`block` records) and rulings (`rule` records), each the exact bytes it was imported with, in `placement` order | `block` + `rule` records |
| restore point (`RESTORE-POINT.md`, name from config) | the same block/record interleave; new sections come from `handoff` records | `block` + `handoff` records |

- A superseded record stays in its export with one generated suffix line,
  `  (superseded YYYY-MM-DD by <key>)`, so the history is still readable in the file; the memory index drops
  it (the index is a "what is true now" view) and lists it under a trailing `## Superseded` heading.
- Regenerated after every committed write: only the files the write touched, each written to a temp file
  and renamed into place. A file whose bytes would not change is not written.
- The export folder is a git repository; commits there are decision D2.
- The session loader cuts the memory index at about 200 lines, so a longer index has entries that are never
  seen. The generator enforces a byte cap per line and a line cap per file, and once the
  recall hook injects relevant memories the index can shrink to a summary (decision D4).

## 5. Snapshots and the integrity check

- **Snapshot.** At the first write after `memory.snapshot_hours` (default 24) since the last snapshot, and on
  `atlas --memory-snapshot`, the store is copied with `VACUUM INTO 'snapshots/memory-YYYYMMDD-HHMMSS.sqlite'`
  (a consistent copy while readers and writers continue; no extra rusqlite feature). The snapshot is
  opened read-only and its `PRAGMA integrity_check` and record count are verified before it counts. Retention
  is decision D3.
- **Integrity check** (`atlas --memory-check`, also run at every snapshot and in the test suite on fixtures):
  1. every version's `sha` recomputes and every record's `chain` verifies from seq 1;
  2. `memory.jsonl` replays into an in-memory database whose heads equal the store's heads;
  3. the exports regenerate to bytes equal to the files on disk (a hand edit is named by file and line,
     never overwritten silently: the generator refuses to write over a file whose bytes differ from its last
     export and reports a miss);
  4. **round trip:** the exported files are parsed back by the importer into a scratch database and every
     record's key, kind, order and body bytes compare equal to the store's heads.
- **Never assume complete.** An empty `recall` answer says so and names the export files to grep; a hit grep
  finds there is a `report_miss` like any other.

## 6. The resident atlas protocol

### 6.1 Inside Claude Code: `mcp_tool` hooks

The plugin's hooks call one atlas tool, `hook`, on the session's own atlas server (section 1.3). Its input
is the hook event and the substituted fields; its text answer is hook JSON or empty. No spawn per prompt and
no new transport.

The server today reads and answers stdio requests one at a time, so a hook call would wait behind a slow
tool call in flight (parallel tool calls make this real). The server gets a reader thread that answers
`hook` on its own worker immediately and queues everything else to the existing loop; JSON-RPC allows the
answers out of order.

### 6.2 Outside Claude Code: the pipe (owned proto3)

For clients that are not a Claude Code session with the plugin (a command hook, the CLI, another tool), one
atlas process per database can listen on a local socket: a named pipe on Windows
(`\\.\pipe\atlas-<first 16 hex of SHA-256 of the store path>`, created with a DACL for the current user
only), a Unix domain socket elsewhere (`$XDG_RUNTIME_DIR/atlas-<same>.sock`, else beside the store, mode
0600). Messages are length-prefixed (varint) proto3 encoded and decoded by the owned `protobuf` module:

```proto
message HookRequest  { uint32 version = 1; string event = 2; string session_id = 3; string agent_id = 4;
                       string agent_type = 5; string transcript_path = 6; string prompt = 7;
                       string tool_name = 8; string tool_input = 9; string cwd = 10; }
message HookResponse { string additional_context = 1; Decision decision = 2; string reason = 3;
                       uint32 tokens_injected = 4; uint32 micros = 5; }
enum Decision { NONE = 0; DENY = 1; BLOCK = 2; }
```

`atlas hook <event>` is the thin client: it reads the hook JSON on stdin, sends one request, prints the hook
JSON answer, and if no server answers within 2 ms, answers in-process itself (start to first answer is
measured near 6.5 ms). Whether the pipe is built in this phase is decision D1.

## 7. The recall hook

Events: UserPromptSubmit (main session), SubagentStart (subagents; their prompts never pass through
UserPromptSubmit), and the parent's PreToolUse on the `Agent` tool, which carries the brief
(`${tool_input.prompt}`) that SubagentStart lacks.

- The parent's PreToolUse(`Agent`) stashes the brief keyed by session; the following SubagentStart takes the
  oldest stashed brief for that session (spawns in one message start in call order). If the stash is empty,
  the subagent's first PreToolUse reads the brief from its own transcript (present then, section 1.1) and
  injects the recall as PreToolUse `additionalContext` instead.
- A brief or prompt that names a lane with a journal gets `recall lane=<id>` first.
- **Relevance gate.** The query is the prompt text. Candidates are active records ranked by FTS5 and, when a
  model is present, the semantic score, fused by reciprocal rank. A candidate is injected only if it clears
  a floor on both its FTS5 score and (with a model) its cosine; the floors are config values set by a script
  over a judged set, never by hand. Nothing clears the floor: nothing is injected, the hook answers empty.
- **Budget.** `recall.budget_tokens` (default 600) per prompt, converted with `bytes_per_token`. Records are
  added whole, best first, until the next would pass the budget; the rest is one `+N more: recall {query}`
  line. A record already injected in this session (by key and sha) is not injected again.
- Prompts that start with `<task-notification>` are not searched. Instead the hook checks the finished
  agent's lane journal (by `<task-id>`) and injects one line if no journal entry is newer than the agent's
  start.
- **Token accounting.** Every hook answer is a `token_log` row (tool `hook:<event>`): bytes injected, and the
  counterfactual computed by the existing rule (the literal grep over the export files plus the reads). Gated
  answers are logged with zero bytes so the gate rate is measured. `--token-report` shows them by event.

## 8. The context-cap hook

PreToolUse on every tool (`matcher: "*"`), main session and subagents.

1. The transcript is `${transcript_path}` for the main session; for a subagent (`${agent_id}` non-empty) it
   is the derived `subagents/agent-<id>.jsonl` (section 1.1).
2. Context size = the last assistant message's `input + cache_creation + cache_read + output` tokens
   (section 1.2). The file is read backwards from its end until a record with `usage` is found; the result is
   cached by (path, size), so a call costs one small tail read.
3. At `cap.warn_tokens` (default 450,000): `additionalContext` telling the agent to finish its step,
   commit, push and write its handoff (`journal entry=handoff`). Repeated at most once per 25,000 tokens.
4. At `cap.block_tokens` (default 500,000): `permissionDecision: deny` for every tool except atlas's
   `handoff` tool, matched on `${tool_name}`. (A `journal` call with `entry: handoff` would need the hook to
   see the input's fields, so the handoff write is its own tool name.) The deny reason says exactly that.
5. Each crossing is a `memory_log`-visible journal entry for the lane, so the orchestrator spawns the fresh
   agent from the handoff record.

Unverified and noted: PreCompact inside a subagent was never observed. The cap fires long before a
subagent's own auto-compaction on a large context window, so the design does not depend on it.

## 9. Plugin layout

```
.claude-plugin/plugin.json   name "atlas", author "Brandon White", mcpServers ./.mcp.json, hooks ./hooks/hooks.json
.mcp.json                    ${CLAUDE_PLUGIN_ROOT}/bin/atlas, no arguments
hooks/hooks.json             mcp_tool hooks on server "plugin:atlas:atlas", tool "hook":
                               UserPromptSubmit  {event, prompt, transcript_path, session_id}
                               SubagentStart     {event, agent_id, agent_type, transcript_path, session_id}
                               PreToolUse "*"    {event, tool_name, agent_id, transcript_path, session_id, prompt: ${tool_input.prompt}}
                               PreCompact        {event, trigger, transcript_path}   -> writes an in-flight handoff entry
                               SubagentStop      {event, agent_id, agent_type, agent_transcript_path}
                             (whether `${tool_input.prompt}` substitutes to empty for tools without a prompt is
                              to be checked in the hook milestone)
                             plus one command hook on SessionStart (mcp_tool is skipped at launch):
                               ${CLAUDE_PLUGIN_ROOT}/bin/atlas hook session-start
skills/atlas-memory/SKILL.md     when to remember, recall, supersede; the reason rules
skills/atlas-handoff/SKILL.md    the handoff record shape and the fresh agent's first call
bin/                         prebuilt binaries per platform (gitignored)
atlas.json                   the user's own config (gitignored), or ATLAS_CONFIG
```

New config keys, all optional, checked by hand like the rest:

```json
"memory": {"store": "state/memory.sqlite", "export_dir": "../atlas-data/exports",
           "exports": {"index": "MEMORY.md", "rules": "RULES.md", "restore": "RESTORE-POINT.md"},
           "snapshot_hours": 24, "snapshot_dir": "state/snapshots"},
"recall": {"budget_tokens": 600, "lane_budget_tokens": 2000, "fts_floor": 0.0, "cos_floor": 0.0},
"cap": {"warn_tokens": 450000, "block_tokens": 500000, "main_session": false}
```

## 10. Migration (dry run first)

Before a migration, count every source: files, lines, top-level items, headings, paragraphs, blank runs and line
endings (mixed CRLF and LF must survive). Those counts are the expected record counts per kind.

Steps, each a separate run:

1. Import into a scratch store with provenance (`src`, `src_path`, `src_line`, `src_sha` of the whole
   source file). Print counts per kind and compare with the expected counts; any difference fails the run.
2. Export from the scratch store into a scratch folder; diff byte for byte against the sources. Item and block
   files must be identical; the memory files identical; the index identical, or every difference listed
   (expected: none).
3. Run `--memory-check` on the scratch store (all four checks).
4. Only on the user's word: switch the live files to generated (point the export folder at them, or at a
   private repo the live paths link to). Until then the live files stay hand-written and the store is a copy.

### 10.1 Keeping the copy in step (sync)

The first import ran once (2026-10-31); nothing re-read the sources after it, so items appended to the rules file
and memory files edited since were invisible to `recall` and the hooks. `memory::sync` extends the import rather
than repeating it: the same splitters, the same keys and provenance, applied as a diff.

- **Mapping.** An `items` or `memory` export syncs from the configured source of the same name (the file of the
  export path's name under its root, or the root as the memory folder). Not when the export folder is that
  source (the store is then authoritative), and never a `headings` export (handoffs are written there).
- **Matching.** Source pieces match the export's placed records by text, line endings ignored. An unmatched item
  and an unmatched record this export imported or synced (first version's `src` is the export) pair as an edit
  when their distinct words overlap by at least half (Dice); the rest are new or removed. Memory records match by
  key (the file stem). Records no source produced, and records a tool already superseded, are left alone.
- **Writes.** New: a record dated and attributed as the item states. Changed item: `supersede`, the new record
  placed after the old, which keeps its text and a marker. Changed memory file: a new version of the same key
  (the key is the file's name, so it cannot move). Removed: `retired` (status, a `retirements` row, a `retire`
  line in the `.jsonl` copy, out of `placement`); a memory file that returns is reactivated.
- **Gate.** `mem_meta['sync:<export>']` holds the fingerprint (SHA-256 over `path TAB sha256` of the files read)
  at the last sync. The index store syncs a source after a rescan or refresh re-indexed one of its files, and once
  per process otherwise, comparing the indexed hashes with that fingerprint; with nothing changed the cost is a
  map lookup.
- **Drift.** `--recall-check` and the memory eval plan the sync without writing and count each change as a miss.

## 11. Design decisions

- **D1. Hook transport.** (a) `mcp_tool` hooks to the session's own atlas server, with the pipe built later only
  if a non-Claude client needs it; (b) both now; (c) pipe only, with a command-hook client per event. Measured:
  `mcp_tool` needs no spawn (2 to 11 ms round trip against a slow test server); (c) adds a spawn per hook (about
  6.5 ms for atlas itself) before the pipe answers.
- **D2. Commits in the export folder.** (a) atlas runs `git` there; (b) atlas runs no git, and the exports are
  committed by whoever runs the session; (c) owned versioning only (`memory.jsonl` and snapshots).
- **D3. Snapshot retention.** (a) keep every snapshot; (b) a rolling window.
- **D4. The generated memory index.** (a) all entries, one line each; (b) a capped index with the recall hook
  carrying the rest; (c) only the most recalled entries.
- **D5. Context cap on the main session.** (a) subagents only; (b) the main session too.

## 12. Results (phase 1 implementation)

Decisions taken: D1 (a), with the hook logic in one module (`src/hooks.rs`) so a pipe is an added adapter; D2 (b);
D3 (a) up to a size cap (`snapshot_cap_mb`); D4 the full byte-exact index by default, the capped one behind
`index_cap`; D5 (a) (`cap.main_session` false).

Migration dry run on a real store (scratch store and scratch export folder; the sources only read): every
exported file byte-identical to its source (same SHA-256), and `--memory-check` with 0 problems. Import took
0.4 s and export 0.6 s.

Recall gate calibration (`--recall-scores`, `cargo xtask calibrate-recall`): 40 prompts unrelated to the store and
80 real prompts. Floor 9.72 with 3 matched words: 0 of 40 unrelated prompts inject anything; 64 of 80 real prompts
inject 1 to 3 records (median 1,895 bytes, max 2,415; budget 600 tokens at 4 bytes per token). The floor is an
absolute BM25 score, so it depends on the size of the store it was calibrated on.

Latencies (release, one machine, other builds running):

| What | Median | p99 |
|---|---|---|
| `recall` by key, store read | 8.1 us | 21 us |
| `recall` by key, whole tool answer | 20 us | 43 us |
| gated recall (120 prompts x 20 runs) | 844 us | 2.5 ms |
| `hook` PreToolUse over stdio (Python harness, ~100 us of it the harness) | 113 us | 306 us |
| `hook` UserPromptSubmit over stdio | 1.0 ms | 2.5 ms |
| hook round trip inside Claude Code (its debug log, call to parsed result) | 1 to 13 ms warm | first call 22 ms (before the worker warmed up early) |
| start to first answer (`misses`), same harness, old / new binary, 3 rounds | 15.8-17.1 ms / 16.4-17.9 ms | (bare spawn 5.8 ms; harness overhead ~10 ms) |
| start to first `recall` / first `hook` answer, new binary | 18.1-24.4 ms / 17.7-18.1 ms | the hook worker opens its store after the first answer |

Inside Claude Code (scratch project, a scratch store, lowered cap thresholds): the recall hook injected on a
related prompt; the subagent got the warning from its own transcript's size (22,365 tokens against a 10,000 warn),
repeated after 3,000 more; and a later subagent whose brief named the lane got that lane's handoff at
SubagentStart. The deny path inside a subagent was shown in section 1 with a test server and is covered by the
suite; a live block with atlas itself was not reached (the subagent obeyed the warning and stopped).

Known limits: recall is FTS5 only (semantic hybrid not wired into the gate yet), so relevance is lexical; gated
recall at ~0.8 ms is dominated by FTS5 ranking.

## 13. Results (phase 2: the race, the semantic gate, a live deny)

**The race.** `twelve_processes_post_to_one_thread` failed with "path resolves outside the vault". Cause:
`resolve_existing` canonicalised the note itself, which opens it; when another process renamed a temp file over
the note at that moment (`fsio::replace_atomic`), the handle could land on the replaced file, which NTFS had
already moved to `\$Extend\$Deleted`, so the canonical path fell outside the vault root. A standalone loop showed
4,605 of 200,000 canonicalisations returning a path under `<drive>:\$Extend\$Deleted\` while a second thread
renamed over the file. Fix: the folders are still canonicalised and confined; the last component is followed only
when it is a link (then checked against the root as before), and a regular file's on-disk spelling is read from
its directory entry (`FindFirstFileExW`, owned FFI) without opening it. Stress under a 32-thread CPU load:

| Binary | Runs | Failures |
|---|---|---|
| main (before) | 150 | 5 (all "path resolves outside the vault") |
| fixed | 150 | 0 |

Tests: a note resolved 20,000 times while another thread replaces it; file links out of the vault, into a dot
folder and in a subfolder refused for reads and writes (now on Windows too), a link inside the vault served.

**The semantic gate** (a scratch copy of a real store; bge int8; prompts: 40 unrelated, 80 real). Floors from
`cargo xtask calibrate-recall`:

| Gate | Floors | Unrelated prompts injecting | Real prompts injecting | Bytes per injecting prompt (median / max) | Gate time median / p99 |
|---|---|---|---|---|---|
| lexical (before) | fts 9.72 | 0 of 40 | 64 of 80 | 1,812 / 2,383 | 0.70 ms / 1.96 ms |
| hybrid | fts 8.89, cos 0.43, alone 0.72 | 0 of 40 | 69 of 80 | 1,980 / 2,383 | 54 ms / 71 ms |

Held out: one prompt that, in the phase 1 live run, injected a loosely related record instead of the relevant one.
Lexical: that record alone. Hybrid: the relevant record among the top three; the loosely related one still clears
the gate (cosine 0.64, near the relevant record's 0.68) but ranks fourth and falls to the `+N more` line. Cosine
separates unrelated prompts well (the best unrelated candidate is 0.70, real ones reach 0.92); it does not
separate two records on the same topic.

The time is the query embedding (about 98% of it). Embedding the first 32, 64, 128 or all 512 tokens gave the
same verdicts on these prompts; 32 halves the p99 of 64. Records are embedded once per version on a background
thread (about 7 records per second at 4 threads on a loaded machine).

**Gate latency follow-ups** (lexical gate, 120 prompts x 50 runs, two interleaved rounds):

| Variant | Median | p99 | Real prompts injecting |
|---|---|---|---|
| 12 rarest terms (kept) | 666 / 669 us | 1.89 / 2.00 ms | 64 |
| 8 terms | 664 / 645 us | 3.00 / 3.13 ms | 55 |
| 6 terms | 630 / 593 us | 2.95 / 3.01 ms | 50 |
| `rank` configured (`rank MATCH 'bm25(2.0, 1.0)'`, `ORDER BY rank`) | 830 / 795 us | 2.33 / 2.03 ms | 64 |

Fewer terms lose recall for no median gain; a configured rank is slower. The vocabulary is a cached map hit at
most 48 times per query, so a SoA layout would not move the gate; not built. The encoder's int8 linears (about
70% of a query embedding) walked the whole weight matrix once per token and did one dot product per call; they
now quantise a block of rows, then apply each weight row to it with a 2x4 register-blocked AVX2 kernel
(bit-identical: integer dot products). Linears alone, synthetic, loaded machine: 32 tokens 46 to 43 ms, 64 tokens
108 to 75 ms, 128 tokens 210 to 157 ms. The query is still about 53 ms.

**A live deny inside a subagent** (Claude Code, headless, a small model, cap lowered to warn 6,000 / block 8,000).
The subagent's first Bash call ran: its transcript had no usage record yet when that PreToolUse fired (a replay of
the same event afterwards denied at 27,185). Its second Bash call was denied at 24,628 tokens with the cap reason,
and so was its `ToolSearch`, which it needed to load the deferred `handoff` schema. `ToolSearch` now passes the cap.
Rerun: Bash denied at 24,579, `ToolSearch` and `handoff` passed, the handoff record written.

## 14. Results (phase 3: install, the VNNI kernel, snapshot roll-up, the clustered index)

Decisions taken: the bge int8 model; the hybrid gate stays; snapshots keep 1 GB by default (a setting) and roll up
into an archive; the generated memory index is an index, not a capped line list (D3 and D4 superseded by these).

**Install.** A release build replaced the live binary (the previous one kept beside it), and the live config
gained `memory`, `recall` (the bge int8 floors from section 13) and `cap`. A real import: every exported file
byte-identical to its source; `--memory-check` 0 problems; the first snapshot verified. Import took 2.2 s. The
hand-written files stay the source: the store runs beside them.

**VNNI kernel.** `vpdpbusd` multiplies unsigned by signed bytes; the activations are flipped to `x + 128` (an
exclusive or with 0x80) and `128 * sum(w)` per weight row, computed once at load, is subtracted. Every product fits
16 bits and the 32-bit lane sums do not saturate, so each dot product is the exact integer and the layer output is
bit-identical to the AVX2 and scalar paths (tested on full and masked 64-byte steps, four-row tiles and the
remainder row, and every byte at its extreme). Four input rows against four weight rows per step, 16 accumulators.
Selected at run time when AVX-512 F, BW and VNNI are present; the f32 kernels stay AVX2.

Hybrid gate, 120 prompts x 10 runs, the main and VNNI binaries interleaved in three rounds on a scratch store
(other builds running):

| Round | main median / p99 | VNNI median / p99 |
|---|---|---|
| 1 | 47.9 / 60.6 ms | 41.2 / 54.7 ms |
| 2 | 48.4 / 61.9 ms | 41.6 / 53.8 ms |
| 3 | 50.9 / 86.1 ms | 44.8 / 59.9 ms |

Every candidate line (fts score, matched words, cosine, verdict) is byte-identical between the arms. The gain is
about 13% of the gate: the int8 linears are no longer the whole of the embedding; attention, GELU and LayerNorm in
f32 are the rest.

**Snapshot roll-up.** Past `memory.snapshot_cap_mb` (snapshots plus archive), the oldest snapshots fold, one at a
time and oldest first, into `archive.sqlite` in the snapshot folder, then are removed. The archive keeps each
distinct version once (unique on key, version number and SHA-256) with its record, supersede rows and refs, indexed
by record and time. A fold recomputes every version's SHA-256 before copying (a damaged snapshot is refused and
kept), looks every version up in the archive before committing, and refuses a snapshot not newer than the last one
folded. `--memory-as-of KEY TIME` answers what a record looked like at a time (newest version written at or before
it, and its first supersede by then) from the archive, the snapshots and the store together. Tests: four snapshots
with revisions, a supersede and a new journal record; a zero budget folds three (10, 1 and 2 new versions); the set
of (key, version, SHA-256) and every answer for every key at every write time (and just before it) are equal before
and after, with the archive alone after folding the fourth, and with the store added. Placement and export
bookkeeping are not archived.

**Clustered index.** `index_topics` on the memory export: `feedback` and `user` memories one line each with the
description, its leading attribution trimmed; project facts as slug clusters under the first topic with a word in
the slug (else the description); references and superseded memories as clusters. On a copy of a real store the
clustered index came out at 72% of the hand-written index's lines and 66% of its bytes. Every memory is named
exactly once (checked by the round trip and by a separate count over the folder); `--memory-check` on the copy 0
problems. Not switched over: the week beside comes first.

## 15. Packaging (licence, model download, release)

**Licence.** Apache-2.0: `LICENSE` is the canonical text (SHA-256 `cfc7749b...3d30`), `NOTICE` carries the
attribution and the third-party notices: the model weights (bge-small-en-v1.5, MIT; Qwen3-Embedding,
Apache-2.0, marked as applying once a release's model list names it) and the crates and SQLite compiled into the
binaries. `license = "Apache-2.0"` in `Cargo.toml` and `plugin.json`; `.claude-plugin/marketplace.json` makes
the public repository its own marketplace (`/plugin install atlas@atlas`).

**Where state lives.** Not the plugin folder (Claude Code replaces it on update) and not the plugin data folder
(deleted on uninstall, which would take the memory store with it): the OS configuration and data folders
(`src/dirs.rs`), or `ATLAS_HOME`. The first run with no configuration anywhere writes `config/atlas.template.json`
there with an empty `notes` vault; the default database moved from a build-time path (which also leaked the
build machine's folder into the binary) to `state/atlas.sqlite` in the data folder.

**Model download** (`src/models.rs`). The list is configuration: per file an id, a file name, an `https` URL, a
SHA-256 and a size, so a later model is a configuration change. The transfer is the system `curl`, started with
an argument list: owning TLS is out of proportion, and a TLS crate breaks the one-dependency rule. `.part` file,
`--continue-at -` for resume, exit 33 (server ignores ranges) restarts from zero, `--max-filesize` bounds it, a
lock file keeps two processes apart (broken once neither it nor a part file has changed for 10 minutes: a server stopped with its session can leave it behind). The hash is checked when the part reaches its size; a
mismatch deletes it; only a verified file gets its final name, so anything that loads a model only checks
existence. The recall hook now looks for an absent model on every event (one stat) instead of once per process,
so a background download is picked up without a restart; hybrid search answers lexically with a note until then.
Tests against a loopback server through the real `curl`: a transfer cut at 100,000 of 300,000 bytes resumes with
`Range: bytes=100000-`; a partial file on disk resumes; a server ignoring ranges restarts; a flipped byte is
refused and deleted; an over-long body is refused; a held lock is busy and a stale one broken; the hook state is
lexical until the file lands and scores meaning after; the first run writes the template and `--setup` fetches
and then re-verifies.

**Release.** `cargo xtask release` builds from the `git archive` of HEAD with path prefixes remapped (and
`/PDBALTPATH` on MSVC) so no build-machine path reaches a binary, leak-checks every exported file including the
binaries, and commits one squashed commit per version to a clone of the public remote, author and committer the
public noreply address, verified after the commit. Binaries are committed in `bin/` (the plugin install is a git
clone, so a release asset would need a second download step): `atlas.exe` for Windows, which `bin/atlas` resolves
to there, and an `sh` launcher `bin/atlas` that starts `atlas-<arch>-<os>` on macOS and Linux. This machine
builds Windows x86_64 and Linux x86_64 (WSL); macOS is built on the Mac with `--build-only` and copied in;
aarch64 Linux needs a cross linker this machine does not have.
