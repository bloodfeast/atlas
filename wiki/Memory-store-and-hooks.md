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
| `journal` | yes | a lane entry: `brief`, `decision`, `measurement` (conditions in `meta`), `commit`, `question`, `answered` (`refs: ["answers:<key>"]`), `next`, `note`, `defer` (below). A `decision` or `answered` entry that names an open question of its lane or a numbered sub-lane, by key or by Q-id (a token like `Q-RIVER-3` in the question's text), gets the `answers` ref itself; the reply names each link and lists the lane's questions still open (key, age, title, five at most) |
| `journal` `entry: defer` | yes | parks questions: `refs: ["defers:<question key>"]` and a meta line `until: <UTC time or date>` or `until: next-session` (the next main-session start: SessionStart `startup`, `clear` or `resume`, never a subagent), optionally `proceeding_with: <option>`. A deferred question stays open but leaves the stale flag and the sweep's idle signal; an answer, or a decision naming it, still closes it |
| `recall op=sweep` (`question_sweep`) | | the open questions that look settled, each with its evidence keys: a later `decision` or `answered` entry in the lane family, or a later rule or decision record, that names its key or Q-id (entries also by sharing three of its title's key terms); a later lane `commit` entry recording a merged PR; the lane idle past the stale age. It closes nothing; it prints the one `journal entry=answered` call with an `answers` ref per candidate |
| `recall op=deferred` (`deferred_questions`) | | the deferred questions: those due first, each with its `A) ... B) ...` options and `proceeding_with`, ready to ask as multiple choice; then those still waiting, with their `until` |
| `handoff` | yes | a lane handoff; also the one tool the context cap lets through |
| `journal_status` | | one row per active lane (an open question, a queued entry, or a record in the last 14 days): entries, last entry, last commit, open questions, handoffs; then `+N idle lanes`; then the listed lanes' open questions, one line each with key, age in days, `stale` past `memory.question_stale_days` (default 3), `deferred until ...` or `due`, and title. `all: true` lists every lane |
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

`hooks/hooks.json` runs `atlas --hook` for every event, as a command hook (the event JSON on stdin). That short
process hands the event to the session's own server over a Unix socket (`<pid>.sock` in the data folder's
`live-servers/`, beside the server's marker), and the server answers it on a worker thread of its own, never behind
a slow tool call. What the server keeps between events stays in one process: a subagent's brief (the Agent call's
prompt), held between the `PreToolUse` that carries it and the `SubagentStart` that injects its recall, the
session's caps, and what it already said. SessionStart and SessionEnd run in the hook's own process: they come
before a server is marked and after it is gone. When every server of the session has stopped, the hook does
nothing, and the `UserPromptSubmit` hook says once per loss that atlas is off. On Windows, and where a running
server's socket cannot be reached, the hook runs in its own process: it works, but `SubagentStart` has no brief
there and injects no lane recall. For clients other than Claude Code (Codex runs command hooks), SessionStart and
UserPromptSubmit are a pinned contract, on [[Other MCP clients]]; every other event is not stable outside Claude Code.

A subagent's launch is named by the tool use id in its meta file (`agent-<id>.meta.json`), but Claude Code writes
that file only after `SubagentStart` answers. So the start takes a launch only when that is unambiguous: exactly one
launch waiting in the session, or exactly one waiting launch whose `subagent_type` is the start's `agent_type`
(a launch whose refusal at the permission prompt is in the session transcript is not waiting). The agent's first
tool call checks that launch against the meta file; a wrong guess is told so and given its own launch's context
and sub-lane. An ambiguous start is answered at that first tool call instead, from the launch its meta file names,
else from its own transcript, so parallel launches never trade briefs. `recall.subagent_context`
picks what a subagent is given: `"recall"` (the default, as below), `"binding"` (only the Binding block: the keys its
brief names, key and title, whether as Gherkin tags (also in backticks or brackets), refs, bare keys or short
continuations in the key's sentence on its line (`0507 and 0508 from atlas`, `; the base ruling is 0031`, a range
`0188-0196` or `0188 through 0196`; a zero-padded number of the key's width, never a year, a count or a `#` number),
brief-wide ones before each scenario's, then the handoff, decisions, open questions and next step of the lane it
declares (matched without case), then the newest handoff and decisions of each other lane a named key is filed in,
headed as context when the brief only cites that key in its prose rather than in a tag, a `Given`, `Rulings:` or
`Rules:` line, a `Rulings` section or a `recall KEY` instruction, or names the lane there; compiled when the launch is let through, at most
2,400 bytes, with the lane's handoff and newest 3 decisions always kept however many keys the brief names, each
`+N more` count saying where to recall it, no lane view and no pointers)
or `"off"`.

- **Recall** (`UserPromptSubmit` in the main session; `SubagentStart` for subagents, whose brief is taken from the
  parent's `Agent` tool call): a lane the text names gets its journal first; then the gated records, whole, in
  fused order (reciprocal rank over FTS5 and the vectors), within `recall.budget_tokens` (600; with the 0.53 floor below, chosen by the cos_floor sweep on the off-topic set and the hook replay). A record passes
  lexically when its FTS5 score clears the floor and it contains `recall.min_terms` (3) of the prompt's words; with
  the embedding model (`semantic.model`) loaded the floor is `recall.hybrid_fts_floor` and the record's cosine with
  the prompt's first `recall.query_tokens` (32) tokens must also clear `recall.cos_floor` (0.53), and a cosine at or above
  `recall.cos_alone` passes on meaning alone. Without the model the floor is `recall.fts_floor`. Records are
  embedded once per version on a background thread (cached in the store's `rec_vec` table); until a record has a
  vector it is judged lexically. Nothing clears the gate: nothing is injected (a short prompt with a word the record
lacks gets nothing in every session, fresh or not; that is the gate, not the per-session dedupe). A record is injected once per
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
- **Context cap** (`PreToolUse`; the main session too while `cap.main_session` is on, the default, as advice only): the size is the last assistant
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
  where the work stands as keys, in at most 1,155 bytes (about 605 tokens on the test's card full of keys, at 1,121 bytes, by a public
  vocabulary times a fitted factor, not real tokens; `cargo test -p xtask` holds that card there, the bytes bind at runtime). The lines: the six newest live lanes, each with its open question count and newest
  handoff; the live lanes' open questions (a stale one as `KEY (5d, stale)`; a deferred one not yet due only
  as `+N deferred`); the due deferred questions, each with its options and `proceeding_with`; `N questions
  look settled: recall op=sweep` when the sweep finds any (none of these three adds a byte when nothing is
  stale, due or settled); after a `/clear` only, the agents still running from the previous
  session (a new process has none, so none hold slots); the work queue's count, up to five that can start now
  and the next waiting; the previous session's goal when its newest `goal_status` says not met; handoffs
  the lanes line does not name; and, just before the question and always kept, the handoff the question names
  (the newest live lane's, else the newest other one) with its title and its next step: the first body line
  starting with `Next`, or the line under a `Next` heading, clipped to 60 characters (the title alone when there is none). A lane is live when it has a work record in the last 7 days: an active journal
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
  `--token-report --verbose` ends with the totals; `atlas --compact-report --transcripts DIR` replays recorded sessions and
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
  lines: `since` (the previous checkpoint), `lanes` (each active lane and its current step), `agents` (each live
  agent: see below), `open` (unanswered
  questions), `owed` (`next` entries no commit followed), `rulings` (rules written since), then the in-task state
  read from the transcript since the last compaction: `files` touched, `tools` (the last six calls, each with its
  result's `turn:<session8>:<line>@hash` ref), `pending` (a call still running) and `intent` (the last line of
  visible assistant text). Thinking blocks are never read; every transcript-derived line passes secret redaction
  (`src/redact.rs`: denied file names, key-shaped tokens, `.env`-style assignments, the `ATLAS_LEAK_DENYLIST`
  terms). After the compaction SessionStart injects the newest checkpoint as a card headed by its own ref, within
  `compact.card_tokens` (default 400), before the ref report.
- **A subagent's compaction:** past its handoff point under an enforced limit it is blocked (the agent hands
  off). Otherwise PreCompact writes the agent's own checkpoint on lane `agent:<id>` from its own transcript and
  answers with the same instructions; its card names no live agents. `SessionStart` never fires in a subagent,
  so the card is given on the agent's next tool call, once. When no PreCompact came, a new `compact_boundary`
  record in the agent's transcript writes the checkpoint then (`checkpoint (boundary)`), of the segment that
  boundary closed, without waiting on the tool call's budget; a card the hook watchdog dropped is owed again.
  Every record of a subagent's own transcript is marked `isSidechain`, and there those are read as its own.
  Every subagent PreCompact is a `compact:subagent` hook-log row. The owed card and the boundaries seen are held
  by the serving process only, so this needs the server's hook socket: where a hook runs in its own process
  PreCompact still writes the agent's checkpoint, but no card follows, and a compaction with no PreCompact is
  not seen. Persisting that state would put a store write on every subagent tool call's path.
- **The live agents and their PRs:** the `agents` line names each agent of the session the host lists as running,
  by its lane (`LANE/NAME` when two share a lane), with `PR #N @head7 (ready|draft)` when atlas finds its PR, each
  part only when known. The PR is found without the agent's help, in order: the lane's newest `pr:` journal entry
  since the launch that is the agent's own (`pr:N head:SHA`, as the text's first line or as `pr:`/`head:` meta
  lines); else the launch's `Uses: worktree:NAME`, found in the worktree ledger, whose branch and head are read
  from its `.git` files; a PR event atlas filed for that head; else one `gh pr list --head=BRANCH` per (repo,
  branch), run in the worktree, all by one deadline (1.5 s, or the hook's time left if that is less; once it has
  passed no gh is started). A PR the agent's newer entry (`pr:N merged`) or a PR event says is merged or closed
  is not shown. The word counts as the `state:`/`verdict:` value, or as the first word after the `pr:` value
  that is not `key:value` or a dash (`pr:N head:SHA -- merged as abc`), or as a `pr:` meta entry's first word
  (`Merged.`), and only when it ends its clause: `pr:N head:SHA ... Not merged.` and `-- merged origin/main`
  keep the PR. gh runs in its own process group (a job object on Windows), so a git it starts is ended with it
  at the deadline. When a lookup fails the agent is named without a PR and a `prs:` line says why (`gh not found`,
  `gh not logged in`, `gh over 1.5 s`, `gh skipped: hook budget spent`, `gh: no network`,
  `worktree NAME gone`). At most six agents are named, running ones first, then `+N more`; the line comes after
  `open`, `owed`, `rulings` and `files`, so it cannot push them out of the card's budget. The PreCompact
  instructions end with the same items ("Live agents now ..."), so the summary can copy exact PR numbers. Six
  agents with PRs cost about 280 bytes on the card (about 70 tokens at 4 bytes a token) and about 330 bytes after
  the instructions; more agents add only `, +N more`. The lookup also
  runs for the checkpoint written at a /clear (SessionEnd), so a /clear can take up to 1.5 s longer.
- **An agent's own entries:** an entry is an agent's own when its `agent:` meta names it, or it names the agent's
  worktree or branch (a `worktree:`/`branch:` word or meta line, or the worktree's name as a word or path part of
  its first line; a branch only as `branch:`, since a branch such as `main` is a common word); one naming nobody
  counts only on a lane no other agent of the session shares. A `pr:` entry is read in each form the journal
  takes (`N`, `#N`, `repo#N`, `owner/repo#N`); one naming another repo than the agent's worktree is skipped. When
  a later agent is launched on the same worktree, an entry tied by worktree or branch is the newest agent's
  launched before it, and the line names that worktree once, for its newest launch.
- **Parked agents:** an agent launched in the session that the host no longer lists stays on the `agents` line
  only as `parked (own job running)`, and only while its own newest entry since the launch is a note whose first
  line is `waiting: <job> (<worktree or branch>)`. Any other own entry since (a hand-back, a handoff, more work), or
  none at all, takes it off. This is read from the journal, never from the host's task status. An agent stopped
  mid-wait, or lost to a session restart, stays parked until an entry ties to it or its launch is 3 days old.
  On a lane with one agent, the orchestrator's own entries count as the agent's: entries carry no session or
  agent id to tell them apart, so a builder should name its worktree in each entry.
- **The model's checkpoint entry:** the card's `model:` line names the session's own `checkpoint` journal entry
  written in the segment, in lane `session:<id>`, else in a work lane (an orchestrator's own lane) when this
  session wrote it: its transcript holds the atlas `journal` call whose result names the entry's key. Another
  session's entries and the hook's checkpoints never count.
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

## Context poisoning guard

Atlas returns text other people and other sessions wrote: records, vault notes, source lines. Some of it can hide
text from the person who reviews it while the model that reads it still sees it: hidden-text injection, a way to
poison a session's context. The context poisoning guard removes that hidden channel
(`src/hidden.rs`):

- **Invisible characters are removed on the way out.** One table (`atlas::hidden::HIDDEN_RANGES`: Unicode
  `Default_Ignorable_Code_Point` less the sixteen basic variation selectors emoji use, plus the interlinear
  annotation controls, tested against the committed Unicode 15.1.0 `DerivedCoreProperties.txt`) names them: bidi
  overrides and isolates, zero-width spaces, word joiners, tag characters, Hangul fillers, the combining grapheme
  joiner, Mongolian and supplementary variation selectors, and the unassigned tag plane. A carriage return before a
  line feed is dropped, and every other control character (a terminal escape, a bell, a lone carriage return) is
  written visibly, as `\u{1b}`. A keyword split by zero-width characters therefore reads whole, a tag-character
  message vanishes, and an escape does nothing.
- **Real text keeps what it needs.** A lone zero-width joiner or non-joiner between two non-ASCII characters (emoji
  sequences, Persian, Indic conjuncts), a lone bidi mark (LRM, RLM, ALM), the three RGI subdivision flags (England,
  Scotland, Wales) and the visible Arabic marks stay. A basic variation selector (VS1 to VS16) stays only straight
  after the visible character it selects a glyph for, and only when the pair is a variation sequence Unicode 15.1.0
  defines (`atlas::hidden::VARIATION_PAIRS`: the 1974 pairs in the committed `StandardizedVariants.txt` and
  `emoji-variation-sequences.txt`, so CJK compatibility forms, math variants and emoji text or emoji style); after
  an ASCII base only VS15 or VS16 after a digit, `#` or `*` (keycaps), and never after another selector, a joiner
  or an invisible character, so a joiner never keeps a selector on its right. Every other tag sequence goes (an unknown subdivision draws as the plain black flag
  either way). A run of two or more invisible characters or selectors never stays, since a run can encode bytes.
- **Visible text is left exactly as written.** Nothing is wrapped, labelled or rewritten: `Option<SystemTime>`, a
  `<details>` block and any tag reach the session byte for byte, and JSON answers are bare JSON.
- **What is scrubbed.** Every tool answer (text or JSON, a repeat read's changed-lines delta, the help), every tool
  error (the refused-store server's included), the `initialize` instructions, the protocol's echo of an unknown
  tool, method or resource, every hook's `additionalContext` (records, the compact checkpoint card, task-notification replies, notices),
  its `systemMessage` and its block and deny reasons, and the store-refusal notice. A size cap (the subagent block,
  the prompt's record budget) is measured on the scrubbed text, so shown escapes cannot push it past Claude Code's
  10,000-character cut.
- **What is left as the tool gave it.** `updatedToolOutput`, the Bash, Read and Grep output a hook rewrites:
  removing characters from a file view would make an exact `Edit` match fail. On a Grep that output also carries
  atlas's code-index notes (symbol names from the files the match lines are in), unscrubbed too; they come from
  the same checkout the model can read whole. The PreCompact summary instructions are atlas's
  own constant text.
- **Nothing on disk changes.** The scrub is on the read side: a record, a note and a file keep their bytes, so the
  integrity check, the exports and the history are as written. A write (`journal`, `remember`, `handoff`, a note
  edit) whose text held characters the scrub would remove or show says so in its reply, with the count.
- **The tree is checked.** `cargo xtask hidden` fails on any character the scrub would remove or show, on a
  lone carriage return, on bytes that are not UTF-8 in a text file, and, in code (`.rs`, `.yml`, `.toml`, `.sh`,
  `.json` and the like), on any invisible character at all and on a token that mixes Latin with Cyrillic or Greek
  letters; it names `path:line:column` and the code point. It runs after the tests in the public CI's `test` job,
  as an xtask unit test in every local `cargo test`, in the release pull request beside the leak and secret gates,
  in `cargo xtask release check`, and on the exported tree in `cargo xtask release`. Deliberate cases are listed in
  `xtask/hidden-allow.txt`, one `COUNT path:U+XXXX reason` per line, keyed by the file and the character (never by
  line numbers, which went stale whenever code above a case moved) and bound to how many the file holds: one more (a
  new hidden character slipped into an allowed file) or one fewer fails the gate with the count to set, and an invisible character in code is never listed: it is written as an escape.

What it does not do: anything about visible text. A planted sentence in plain view ("ignore previous instructions")
reaches the session as written, and is out of scope for now: plain filtering cannot tell it apart from a user's own
correction ("stop doing that"), so it is left for the user to catch. Look-alike letters in prose (outside code) are
left alone too. What stays is one choice per place, never a run: a lone bidi mark beside ASCII, or a lone joiner
between two non-ASCII letters, still lets a gap carry a bit or so (mark, other mark, nothing), and a kept selector
picks one of the at most three its base defines (one bit on most of the 1450 bases that have any, nothing on any
other character); no model decodes that without a visible instruction to, which is the visible-text case above.
Variant forms outside VS1 to VS16 are lost: the ideographic variation selectors (U+E0100 to U+E01EF) and the
Mongolian free variation selectors (U+180B to U+180D, U+180F) go wherever they stand, so a Japanese name written
with an ideographic variation sequence and Mongolian text that picks a positional form show their default glyphs
in atlas's output (the characters themselves stay). The update notice's highlights and the leak check drop every format character (Unicode `Cf`,
`atlas::hidden::is_format_char`, tested against the committed `UnicodeData.txt`).

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
