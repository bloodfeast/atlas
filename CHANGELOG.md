# Changelog

Every release has a section here, `## vX.Y.Z`, written for users: what changed for them, not how the work
was organised. It is the source of the release notes (through `scripts/release-notes.md.in`) and of the
wiki's Changelog page. A section has these parts, in this order, and leaves out any that would be empty:

- `### Highlights`: what a user notices, with measured numbers where there are any. Required.
- `### Breaking changes` and `### Upgrade notes`: what a user must do when updating.
- `### Features`, `### Fixes`, `### Security hardening`, `### Known limits`: everything else.

`cargo xtask release` refuses to release a version whose section is missing, empty or has no highlights.

## v0.5.0

### Highlights

- **`atlas backlog` combs the backlog for ruled work that fell out of the queue.** It reads the queue, its marks,
  briefs, commits, rulings and the user's own words from the memory store, and lists the placed items that are not
  live (no brief or commit within 7 days, no running agent) and not done: aging items (asked again, their
  written-order turn came, or placed over 14 days ago, shown with their days), then waiting items, each with its reason
  and the ruling keys behind it. It only proposes: the written order is never changed. `--as-of TIME` replays any
  past day, `--json` prints rows, `--all` adds ruled IDs never placed, and `atlas backlog dismiss ID --because KEY`
  silences a false flag for good.
- **The prompt hook points to less, and never to the same record twice in a session.** A record pointed to once
  is not pointed to again in that session, now also after a server restart or a compaction and under a command
  hook (one process per event): the session's earlier injections are read back from the `hook_inject` log. A
  main-session prompt gets at most 2 pointers (`recall.prompt_pointers`), the rest counted on the `+N more` line,
  and a subagent whose brief names its own lane gets that lane's journal and no pointers beside it. On the memory
  eval's 70 hook questions the injected tokens fall 22% at floor 0 (6,369 to 4,958) and 17% at 0.663 (5,654 to
  4,693), for 2 informed passes and 1 strict pass fewer at floor 0 (61 and 52 of 70) and 1 informed fewer at 0.663;
  recall through the tool is unchanged (100/100, facts 176/176). `"recall": {"prompt_pointers": 0}` removes the cap.

### Upgrade notes

- **A fresh install now recalls within 600 tokens at a 0.53 cosine floor.** A configuration with no
  `recall.budget_tokens` or `recall.cos_floor` now gets 600 tokens and 0.53 (was 150 and 0.57), the lowest floor
  that passes the cos_floor sweep on the off-topic set and the 303-prompt hook replay (same frozen store), both
  arms with `recall.prompt_pointers` at 2, the shipped cap. On the replay that is 1 more strict pass (179 against
  178), 1 more informed (200 against 199) and 50.3 injected tokens per prompt against 49.9; recall through the tool
  is unchanged. On the 30 off-topic prompts, 2 of 30 (6.7%) get a record, 3 records in all, the same as 0.57 at
  150 tokens, and lane off-topic is 0.03 per prompt on both. Off-topic passing is measured only on that 30-prompt
  set. `"recall": {"budget_tokens": 150, "cos_floor": 0.57}` keeps the old behaviour.
- **The compaction features are on by default** (`compact.enabled` was false). Before Claude Code compacts, the
  PreCompact hook checkpoints the segment and returns instructions for the summary (cite atlas keys, write
  deltas), which steers every compaction summary; after it, SessionStart injects a card of about 400 tokens naming
  the segment's parts by key, and `recall` accepts the card's chain keys. Nothing is written into your project's
  files (the `CLAUDE.md` block stays an explicit `atlas settings compact-md`). Measured on a 117 MB recorded session:
  PreCompact 179 ms and the card 56 ms median per compaction (15 ms each with compaction off), and about 8 KB of
  store per checkpoint (16 checkpoints on a fresh store). It also records which lookups are repeated after a
  compaction (`compact_miss`, redacted targets and shell signatures), at one `stat` per Read, Grep and Glob in
  PreToolUse: a median of about 40 us per call in-process (p95 under 65 us), against under 1 us with it off.
  What a user with no `CLAUDE.md` block and a bare `/compact` gets from the steering has not been measured live. `atlas settings off compact`, or `"compact": {"enabled": false}`, turns it off.
- **Building from source needs Rust 1.99.0** (was the latest stable). `rust-toolchain.toml` pins it, and rustup
  installs it on the first `cargo` call.
- **`memory.rules: false` is now the same as unset.** Before, `false` also stopped the sync and query recall of the
  memory folders and rule files your configuration names by hand (`memory_dir`, `intent`); now they keep syncing and
  are recalled as with the key unset. To keep the old behaviour, set `"memory": {"rules": "off"}` (or run
  `atlas settings set memory-rules off`): the store is kept either way. `atlas settings off memory-rules` still
  removes the key.

### Features

- **A task judge for agent launches (experimental, off by default; #156, #161).** The `judge_task` tool (note op
  `judge`) gives the model and effort pick for a launch prompt from a cells file, as a `Judge:` line to paste. With
  `orchestrate.judge.enabled` the PreToolUse hook adds the same pick as one advisory context line at an admitted
  Agent launch, and `orchestrate.judge.classifier.enabled` adds a work classifier's pick. It is advisory only: it
  never denies or rewrites a launch. Both switches default to false; every key is under `orchestrate.judge.*` (see
  the wiki's Configuration reference).
- **A new orchestrated install declares starter resources and agent caps** (#148). On yes, setup (`atlas --setup`,
  `atlas settings orchestrate`, `--yes`) writes the keys you did not answer and the configuration does not set,
  derived from the machine: `orchestrate.resources.compile` max(1, hardware threads / 16), at most 4;
  `orchestrate.resources.gpu` 1 (a lock, no device is assigned or probed); builders the compile slots (at least 1),
  reviewers the builders, research max(1, builders / 2), each clamped to Claude Code's own limit
  (`CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS`, 20 by default). New `--compile` and `--gpu` flags and settings items
  `resources-compile` and `resources-gpu` (0 never serializes) are asked first, so the caps follow them. The
  handoff point and hard limit are not written. An install that never ran setup keeps the code defaults (gpu 1,
  compile 1, caps 2, 2 and 1). With orchestrated mode on, a builder launched with no `Lane:` line is let through
  with one context line asking to declare its lane. See the wiki's Orchestration page.
- **A brief writer is its own kind with its own cap, uncapped by default.** `Kind: brief-writer` (or `brief writer`)
  and the agent type `brief-writer` now classify a launch, so writing briefs no longer falls back to a builder and
  takes a builder slot. `orchestrate.kinds.brief-writer.max` caps it; with none set only Claude Code's own limit
  applies, and setup writes no starter for it.

### Fixes

- **Only real limits on a `Uses:` line hold a launch back** (#150). Before, every word on the line was a
  one-holder lock, so builders that named the same repositories ran one at a time, each in its own worktree.
  Now only `gpu`, `compile`, `worktree:NAME` and the resources your configuration lists can deny or queue a
  launch; any other word lets it through with a note naming what it shares. `sqlport 15126` is read as one
  resource, `sqlport:15126`, so a `sqlport` entry in the configuration limits each port on its own.
- **A queued launch leaves the queue when its lane starts under a reworded brief** (#150). A launch with the same
  `Lane:` and kind takes the oldest such waiting launch off the queue and names it, so `atlas queue next` stops
  proposing work that already runs.
- **A brief that mentions screenshots no longer claims the gpu** (#150). The bare words capture, screenshot,
  bench, benchmark and bake now infer gpu only in a project whose `atlas.project.json` sets
  `"orchestrate": {"bare_gpu_words": true}`. Phrases that always mean the gpu (a GPU lock or timing, a frame
  capture, CUDA, Vulkan) still count everywhere.
- **A prompt after a store write no longer reloads every record vector.** Any commit (a journal entry from another
  agent, the background embedder) made the next prompt re-read all record vectors; now only records whose current
  version changed, or that have no vector yet, are read. No record vector is re-read after a commit that changes no
  record: on a 1,604-record store, a prompt after a journal entry reads none of the 1,604 (`cargo xtask hook-bench`).
- **Every atlas server now loads the record vectors at start.** The background warm-up, which loaded the model, now
  also reads every record vector, so the first prompt does not. Every server pays it, one that never gets a hook
  included: the CPU time of one full vector read, and dim x 4 x N bytes resident (2,463,744 bytes for 1,604 records
  at 384 dimensions).
- **A command hook on a recalling event is said once a session.** `atlas doctor` says when UserPromptSubmit or
  SubagentStart is a command hook (`atlas --hook`): each prompt then starts a new process that reloads the model and
  record vectors. A session hears it once: at its first prompt when UserPromptSubmit is the mcp_tool hook, otherwise
  at SessionStart (a new or cleared session), since with a command hook every prompt is a new process.
- **A subagent's report no longer pulls in whole lane views.** A prompt carrying an `<agent-message` frame (a
  hand-back or another session's message) was recalled like a question you typed: every lane it named got its full
  view, with the handoff, decisions and measurements, plus recall pointers. Now each lane the report itself names (not
  the harness's frame around it) gets one line, `lane X: N entries, handoff NNN <title>, K open; recall lane=X`, and
  a report that names no lane gets nothing; a task notification is unchanged. On a real hand-back that named one
  lane, the hook's added context fell from 10,496 bytes to 122.
- **The report backstop sends back only reports over 28,000 bytes, and without a draft** (0045). It fired past
  14,000 bytes and attached atlas's mechanical draft, which the agent then had in its context as well as its own
  report. Now a report up to twice the 14,000-byte bound passes, and the one sent back gets the rewrite
  instruction alone (the report is still filed whole first).
- **The report digest keeps a section's options and stays within its budget** (0045). A short line ending in a
  colon (`Options:`) inside a `## Questions` section ended the section, so the options under it could be dropped;
  now only a Markdown or bold heading does. Lines the classifier does not recognise were never kept; they now fill
  whatever room is left. The gap marks and closing line now count toward the budget.
- **Calibration counts fewer non-lookups** (0045). A folder above a project root counts as harmless only as a `cd`
  target; `grep -r x R:/work notes.md` no longer counts as a lookup. A Grep glob counts as Markdown-only when every
  alternative of its braces ends in `.md` (`*.{md}` yes; `*.mdx` and `*.{md,rs}` no). `cargo xtask
  calibrate-baseline` needs `--project` for a configuration with projects, instead of choosing by the folder it is
  run from. The lookup rules are now version 3, so atlas recalibrates itself once on the new rules.
- **An agent of type `builder`, `reviewer` or `research` counts as that kind.** A launch with one of these
  subagent_types and no `Kind:` line fell to the worktree rule or `orchestrate.default_kind` (builder); the built-in
  agent-type map now gives each its kind. A `Kind:` line and your own `orchestrate.types` still win.

### Known limits

- The aging marker is shown with its days only; it is not a call to pull work forward (only 2 of 21 (10%) read
  as overdue; 18 read as waiting behind a written order or ruling).

## v0.4.0

### Highlights

- **atlas calibrates itself.** The measured ratio in `atlas --token-report` divides by a baseline taken from your
  own Claude Code transcripts: what agents read on rule and handoff lookups before atlas. The installed binary now
  takes it by itself, in the background at the server's first start (and after an upgrade that changes the lookup
  rules or a configuration change to the vaults, sources or skips), never on a hook's path; no repository or
  cargo needed. It finds its own inputs: the cutoff is the earlier of the store's first atlas answer and the first
  atlas call in the project's transcripts; the transcripts are those whose session ran in one of the project's
  folders; temporary, scratch, eval and replay folders below the project's roots are skipped (`calibrate.skip`),
  and `atlas --calibrate` names each folder and transcript a skip left out. `atlas --calibrate` runs it by hand with
  progress.
- **No made-up ratio.** With fewer than 30 lookup episodes from 5 transcripts before atlas, the report prints the
  upper bound alone and says why and when it will measure again. Otherwise it says when the baseline was taken,
  from how many episodes and transcripts, and with which version of the lookup rules.

### Upgrade notes

- `gh pr diff` and `gh pr view` output is now a PR digest by default; `lean.pr_digest: false` keeps `gh pr`
  output as before.
- A baseline saved before this release has no rules version, so the first server start after the update measures
  it again. The new measurement counts a call that several transcripts hold (an eval replay, a resumed or forked
  session) once, and skips eval and replay folders, so on a machine that replayed sessions the measured ratio
  changes, by a large factor where many replays were counted. The old rows stay in the store; the report reads the
  newest. A baseline measured by a newer atlas on the same store (with newer lookup rules) is left alone, so an
  older and a newer binary sharing a store do not recalibrate in turn.
- The first run reads every transcript that started before the cutoff once (a transcript that starts after it is
  closed after its first line), and keeps what the lookup rules read of each earlier call in the store (tool,
  the input keys the rules read, folder, result size; never the result's text), so a later recalibration does not
  lose history Claude Code has deleted since.
- `cargo xtask calibrate-baseline` stays for development, with explicit inputs; it now drops copies too
  (`--keep-duplicates` keeps them, to reproduce an older run).
- A store that a newer atlas has moved to a newer format is refused, by name, and never written. Before, an older binary opening it
  downgraded or rebuilt it, so two versions sharing a store undid each other on every other open. Now the MCP
  server still starts and says so: with a newer index store every tool answers with the refusal, and with a newer
  memory store the memory tools do, while the vault tools keep working. The CLI and `atlas doctor` print
  `store too new` and exit 1. Every message says to update atlas. v0.3.0 and older binaries do not refuse, so
  update every atlas binary that shares a store together: the plugin install and any build from source.

### Features

- **PR review digests (on by default, `lean.pr_digest`).** A `gh pr diff` over the routing threshold becomes one
  row per file with its own handle and each hunk's line range, lock, generated and vendored files one row each;
  `gh pr view` becomes one row per body, comment and review with a handle to its text. Each diff is a round of its
  PR, kept locally with no network call, so a later round, by the same reviewer or another, lists only the files
  that changed and names the rest as unchanged since round N. See the wiki's Handles and digests page.
  - A file read (`atlas slice h:F`) is the diff as stored, without line numbers or the header lines its row
    already gives, paged between hunks with a `next page` line; `--lines A-B` reads lines as stored.
  - An edited line shows once, as a word diff: `~` with `[-old-]` and `{+new+}`.
  - A deleted file is one `deleted` row; its body stays behind its handle.
  - On a later round, a changed file shows only the hunks no earlier round showed, with the whole file one
    handle away.
  - A file is also generated when its first 12 lines in the working tree carry a generated marker (read only
    when the working directory is the PR's repository); an added marker counts only in the file's first 12 lines.
  - Nothing is lost: every line of the diff is in a read or behind a handle the digest names, and the reads
    decode back to the stored output byte for byte.

  On 12 held-out public PRs (18 review rounds), what a review pulls through the generic digest is 1.24x what it
  pulls through the PR digest at the median (1.31x in tokens), 1.22x on first rounds, and at least 1.17x on every
  round. A 1.5x saving stays a later goal.
  `lean.pr_digest: false` turns it off.

### Fixes

- **A cap refusal says where the launch's kind came from.** The kind is read from the Agent launch prompt's first
  40 lines, never from a brief file the prompt names, so a reviewer whose prompt only named a builder's brief was
  refused as a builder and told to fix the brief. The refusal now names what decided the kind (a `Kind:` line,
  the agent type, `isolation: "worktree"` or `orchestrate.default_kind`) and, when no `Kind:` line did, where one
  goes. A relaunch that adds a `Kind:` line can name its queue entry with `Queue: KEY`, so the entry leaves the
  queue when the agent starts.
- **A PR digest file read has one bound.** All the edited lines of one read share one word-diff step budget; once
  it is spent, the rest are shown as stored, so a large read stays quick and still exact.
- **Every store write checks the store's version under its lock.** Each write to the memory store and the index
  store, side tables and background writers included, now checks the store's version under its write lock, so
  once a newer atlas has upgraded a store, an older atlas (v0.4.0 or later) that opened it earlier stops writing
  to it instead of writing in the old format. Memory exports hold the lock across their file writes.

### Security hardening

- **Releases are made by one command, with a typed answer per public act.** `cargo xtask release ship vX.Y.Z`,
  run twice by the maintainer at a terminal, opens the release pull request from the exact commit it checked,
  merges it only once he approved its head commit and every check passed (pinned to that head, never with the
  admin bypass), and publishes behind one more prompt; the signing key's path is checked last and only openssl
  opens it. The old direct-push release is gone. See the wiki's Releasing page.
  When `main` moves on after the pull request opened, the next run exports the new commit to the same pull
  request, behind a new `open` answer, and a fresh approval of its new head is needed; a closed release pull
  request gives way to a new one once its branch is deleted.

### Known limits

- The rules version is checked by a test that hashes the lookup rules' code and `glob.rs`; a change elsewhere that
  alters a result (the JSON or regex engine, the transcript reader) needs a new version by hand.
- Without `projects` in the configuration every transcript is read; the vault and source roots decide what counts.

## v0.3.0

### Highlights

- **Orchestrator mode.** An `orchestrate` skill teaches the main session to delegate: lanes, lean briefs that
  name their rulings instead of pasting them, a `Kind:` line, handoffs at a context limit, a fresh agent after
  every handoff, and verify-then-merge. In orchestrated mode the PreToolUse hook caps how many agents of each
  kind run at once (2 builders, 2 reviewers, 1 research agent by default, adjustable, clamped to Claude Code's
  own limit) and denies a launch past its cap with a reason to queue it. It never allows a call.
- **Orchestrated mode is asked, not defaulted.** `atlas --setup` and the first session ask whether you want to
  work in orchestrated mode, and on yes ask its options: the caps per kind, how context limits apply
  (enforced, advisory or off), the handoff point and hard limit, and whether a fresh agent after a handoff
  starts on its own or asks you first. The answers go into atlas's configuration in one change that
  `atlas settings undo` reverts. `atlas settings orchestrate` asks again at any time, and `--yes` with flags
  or `--no` answers without a terminal.
- **Context limits per agent kind.** Each kind has its own handoff point and hard limit, scaled to the context
  window, with `enforced`, `advisory` or `off` modes. A SendMessage to an agent past its hard limit is refused,
  and a finished agent that passed its handoff point is followed by a fresh one.

### Upgrade notes

- In orchestrated mode the default context limits are 285,000 (handoff point) and 450,000 (hard limit) on a 1M
  window, scaled to the window, per agent kind; a configuration that sets `cap.warn_tokens` or
  `cap.block_tokens` keeps its values for every kind without its own. With orchestrated mode off (the default)
  nothing changes from v0.2: the cap is `cap.warn_tokens` and `cap.block_tokens` (450,000 and 500,000),
  enforced, for every agent, and a SendMessage is never gated.
- `journal_status` no longer counts a retired question as open, and the checkpoint card counts a question
  answered from another lane as closed: both now share the resume card's open-question test, and so does
  `recall lane=X`.
- One title rule for every record: a title is at most 120 characters, cut at a sentence or clause end. A first
  line past that (a journal entry, a handoff, a decision, `atlas queue plan --title`) stays whole at the top of
  the body, so nothing is lost; before, journal and handoff titles were cut at 200 characters mid-word and other
  titles lost their tail. A memory's or a rule's own `title` (or a memory's `summary`, past 200) is refused
  rather than cut. `journal` with `amend: KEY` now also amends a `review` entry, and a queued launch's
  `brief_sha` follows its amended brief.
- The config lock is atlas against atlas: Claude Code writes `settings.json` itself and takes no lock, so a change
  it makes in the same moment as an atlas one can still lose. The offer state file has the same read and write
  with no lock; its worst case is a question asked twice.
- The shell deny-list check catches named files only, since a shell command can hide a path (`F=.e; cat ${F}nv`, a
  link reached after `cd`). It over-matches on purpose: `grep credentials src` gets no handle, at the cost of one digest.
  Handles stored before this release stay until they expire, up to 7 days; nothing purges them.
- The README's lookup figure, 2.2x (288 tokens per atlas answer against 624 per grep-and-read episode, medians), is
  the v0.1.0 measurement on the maintainer's transcripts. The measured ratio that `atlas --token-report` prints is
  a different measure: your own store's answers against a baseline that `cargo xtask calibrate-baseline` takes from
  your own transcripts. Its lookup rules changed in this release (stricter about what counts as a lookup, with
  `--project`, `--skip` and `--audit`), so a baseline saved before gives a different ratio until it is taken again.
  No figure for it is published here.

### Features

- **A resume card for a new or cleared session.** SessionStart with `source: startup` or `clear` injects where
  the work stands, as keys, in at most 1,160 bytes (612 tokens on the test's card full of keys, at 1,119 bytes, by a
  public vocabulary times a fitted factor, not real tokens): the live lanes (each with its open questions and newest
  handoff), their open questions for you, after a `/clear` the agents still running, the work queue's head and
  the batch that can start now, handoffs from the last seven days, and the previous session's goal when it was
  not met, never cut: a goal too long for the card is named by where it is in the transcript. Claude then asks
  you, as one multiple-choice question, whether to resume, change the plan, or do something more pressing
  first. An empty store gives no card; `memory.resume_card: false` turns it off.
- **A queue for refused launches.** A launch the agent cap refuses is filed as a `queued` journal entry in its
  lane, with its brief, and the deny reason names the key. When an agent finishes, the notification lists what
  waits; launching it again with the same description and brief takes it off the queue. `atlas queue` lists the
  queue, `atlas queue show KEY` prints one, and `atlas queue drop KEY` removes one.
- **Work queue lanes.** Every queued item has a lane, a priority (`urgent`, `normal`, `background`, with aging
  so nothing starves) and the shared resources it uses (`gpu`, `compile`, a worktree, any name you configure),
  each read from `Lane:`, `Priority:` and `Uses:` lines in the brief or inferred from it; a declaration always
  wins. Items on the same lane run one at a time unless declared `Uses: read-only`, and a resource serializes
  across lanes. When an agent finishes, atlas proposes the batch that can start together and says why the rest
  wait; `atlas queue next` shows it on demand and `atlas queue add` files planned work. A launch that conflicts
  with running work is queued when both sides declared the conflict, and warned about when atlas inferred it.
  Planned work waits on other items (`after: KEY`, met when KEY is marked done) and leaves the queue when a
  launch names it with `Queue: KEY`. An agent files planned work over MCP with the `journal` tool's `queued`
  entry, checked as `atlas queue plan` checks it.
- **Required reviews before merge.** With `orchestrate.reviews` naming roles (`["simplicity", "cost"]`), a
  `gh pr merge` waits until each role has a passing review of the PR's current head, read from `gh pr view`.
  Reviews are `review` journal entries (`role:`, `pr:`, `head:`, `verdict:`, `report:`); notes with `review:`
  meta count too. A request for changes blocks its role until a newer head passes. Enforced mode denies the
  merge, advisory (the default) warns, and no roles leaves the gate off.
- **Digest-form reports.** The orchestrate skill asks agents to hand back the outcome first, then the numbers,
  the open questions under one heading and the risks, with longer notes filed in the journal and cited by key.
  In orchestrated mode a report over 14,000 bytes is sent back once, at the agent's stop: atlas files it whole
  in the lane journal and asks for the digest form, with its own draft attached.
- **Experimental, off by default: chain keys on the compaction card.** With `compact.enabled`, the card names
  every part of the segment by a key one `recall` resolves from the transcript. `memory.transcripts` also lets
  `recall` resolve them. Not yet scored live. `atlas --compact-report` no longer lists a `tools:` line, so its
  card size and coverage lines change and do not describe the live card.
- **Experimental, off by default: memory rules.** Nothing asks about it. `atlas settings set memory-rules true`
  has atlas find a project's Claude Code memory folder and root `CLAUDE.md`, and a query that hits a replaced
  ruling answers with the one that holds. `atlas settings off memory-rules` goes back to unset. On a test set, not
  a promise: the ruling that holds came first more often (hook 19 to 33 of 46, query 23 to 43), but the 1.5x token
  bar was missed (hook 1.027x, query 0.921x, so a query costs more), `memory-eval` strict pass fell from 98 to 82,
  tokens per question rose 13%, and the labels are circular, 21 of 46 not reviewed by the maintainer. The known
  limits are on the wiki's Memory and rule files page.
- **Experimental, off by default: idea capture.** With `memory.ideas.enabled`, a person's standalone statements
  that propose, decide or constrain something are filed as pending `idea` journal entries in their exact words,
  redacted, so an idea said in passing is not lost; `atlas ideas` lists, files, dismisses and converts them. The
  detector is a linear score with no model; its weights come from one author's labelled prompts and are untested on
  anyone else. Asking you about an unsure capture is built but unproven in a live session, so
  `memory.ideas.asks_per_day` defaults to 0 and nothing asks. Off, it reads and writes nothing.
- **Compact instructions are no longer offered.** Neither SessionStart nor `atlas settings offer` raises the block,
  which costs about 380 tokens per session and agent and in `CLAUDE.md` alone showed no reliable effect on the
  summary. `atlas settings compact-md` is the only way in, and its prompt now says what was measured.
- **Experimental, off by default: the PreCompact hook returns the Compact instructions.** With `compact.enabled`,
  a compaction's summarizer gets the instructions (336 tokens) without a `CLAUDE.md` block or a `/compact` argument.
  The route steered the summary in one manual run each by command hook, stub and the real atlas tool on Claude Code
  2.1.318, with no plain control on that version; the automatic trigger is untested. On a manual `/compact` the text
  also lands in the conversation as `local-command-stdout`, about 340 tokens on each later request until the next
  compaction; the automatic path is unread. A project with the block gets the text twice and should drop it.

### Fixes

- **The agent cap no longer counts a started or finished agent twice.** A launch's pending record now ends at
  the agent's SubagentStart (or once its meta file is seen, running or finished). Before, with a hook input
  that carried no `tool_use_id`, every agent counted twice for two minutes ("3 of 2 reviewer slots ... 2
  launched moments ago" with one reviewer running), and an agent that finished inside two minutes kept its slot.
- **Reviews wait only for a reviewer slot.** A `Kind: reviewer` launch no longer holds or conflicts on a lane,
  so both reviews of one PR run together beside the lane's builder. `atlas queue` lists waiting reviews as
  their own queue.
- **A fractional queue order is read.** `order: 9.5` read as no order, so it listed bare and after every whole
  number of its priority, and a proposal ignored it. An order is now an exact decimal, compared digit by digit
  and printed as written (`#9.5`), and `atlas queue plan --order` and the `journal` tool file one.
- **`atlas doctor`** checks the atlas hooks in Claude Code's `settings.json` against the plugin's
  `hooks/hooks.json`: each event with no atlas hook, and any missing or extra input field per event. A session's
  first prompt says so too.
- **Concurrent config changes no longer lose a write or fail on the backup.** Every `atlas settings` change,
  `atlas grammar accept` and `atlas settings on|off update` now holds a lock from before its read through its
  write, and takes a backup name no other process can hold: two sessions adding a project at once both keep
  their source, and each change has its own undo. A change waits up to 30 s for another one, saying so once, then
  stops with an error naming the lock file; a process that dies releases its lock. Questions are asked before the
  lock, and a question's answer is applied to the file as it is afterwards. `atlas settings undo` refuses when a
  newer change came after the one it showed.
- **A second project no longer makes your home folder a project root.** Following "One repository" then "A folder
  of repositories" gave the first project a root of the home folder, because the starter vault under atlas's
  configuration folder counted towards it, so no folder under home was ever asked about. A moved setup's root now
  leaves that folder out, and no root is ever the home folder, a folder holding it, or a drive root: each folder
  gets its own root instead. `add-project --new` refuses those folders and says what to do. With projects, a folder
  in no project and holding no repositories is now asked about once, as on a fresh install; before, only folders
  of repositories were. It still reads `default_project`, which is unchanged. A configuration that already holds
  such a root is not rewritten: `atlas doctor` names it.
- **`--memory-check` no longer reports corruption because another session is writing.** It reported `copy:`,
  `disk:` and `round trip:` problems on about 6 in 10 runs beside two writing sessions, and none once they stopped.
  Chains, copy and round trip now read one snapshot of the store; a file that differs from its export is taken again
  under the store's write lock, for up to 6 s, since an export is written just after its commit. A real fault is
  still reported, by the same prefixes and exit code, while writers run. A real disk mismatch makes the check take
  those 6 s.
- **A handoff is no longer refused when two sessions hand off on one lane.** The lane's active handoff is read
  under the write lock. Before, about 1 handoff in 200 under load failed with `already superseded`, and two on an
  empty lane could both stay active.
- **`atlas doctor` says what it checked.** With no `settings.json`, or `{broken` in it, it said the hooks match,
  and a plugin install always got that answer. No file, no atlas hooks in it, a malformed file and a plugin install
  now each have their own line, and "match" is only said of real hooks that match. A malformed or unreadable file
  now exits 1. It also names a project root at or above the home folder, or a project file there, without
  changing it; that is not a failure.
- **A large secret read through the shell is not kept as a handle.** The Read tool's path was denied; Bash,
  PowerShell, `atlas run` and Grep were not, so `cat .env.big` over the threshold sat in plain text under
  `state/handles` for up to 7 days. Output is no longer kept when the command names a deny-listed file, however it
  is quoted or chained, or a word in it is a link to one. The command still runs and its output still shows.
- **The README and wiki name what `atlas settings offer` offers.** They still listed an early compaction point, which
  is no longer offered; it offers the 1-hour subagent cache and goal check-ins.
- **The security page says what holds for prompt-shaped notes.** It said note text comes back only as a JSON string;
  by default it comes back as plain text, as written, and nothing marks it as data. The README's memory evaluation
  row now says it is from v0.2.0.
- **Figures with no source are gone.** The README's 236 KB and 87 KB rows, the about -20% lane cost in the settings
  text and the 48 ms a clause in the wiki had nothing behind them a reader could check. The wiki keeps the first run's
  figures, marked as one unpublished run.
- **`--help` writes nothing.** `atlas settings --help` wrote the first-run configuration before reading its arguments,
  and failed on a malformed `settings.json`; `queue`, `ideas`, `rules` and `ruling` opened a store first, and `rules`
  and `grammar` had no help. Help on any subcommand now touches nothing. `atlas --help` also lists `ideas` and the
  queue's `done`, `met` and `plan`.
- **A Grep of one file no longer reads as no matches.** Over the threshold, a search of a single file prints `12:text`
  with no path, and its digest said `0 hits in 0 files`, which a model took at its word. The digest now counts those
  lines and quotes them. Bash `grep` without file names reads as a plain log, with its line count. A file whose name
  starts with digits (`0001-init.sql:5:x`) is named as a file.
- **A filtered `recall` finds what matches and says when it cut.** `scope` and `kind` were applied after the search's
  window, so records elsewhere could use it up: five matching records could read as `no records match`, and a list cut
  short had no `+more`. The search now reads on until the list is full or the matches run out, and `limit: 0` says when
  its 8 rows cut anything. A filter also keeps all the query's words, up to 48: over 12 known words, the search kept the
  rarest in the whole store, and a scope's only record could hold just the others and read as `no records match`. Past
  48 the answer says how many went unsearched.
- **`memory.rules: false` and `true` no longer answer a false "none".** Under `false`, the hidden rules and memories were
  dropped after the limit, so eight of them at the top read as `no records match` with a journal entry ranked ninth,
  and the hook injected nothing the same way. Under `true`, a replaced record whose successor is gone held a row of the
  limit and showed nothing. Both are now decided before the limit. Unset is unchanged.
- **A word with diacritics is found.** The search folds them and the check for known words did not, so `recall` for
  `Müller` answered `no records match` over a record holding it, and the hook passed it over.
- **The first-session question comes back until it is answered.** It was marked asked when the line was made, and
  Claude relayed it in about 4 of 9 test sessions, so it was often never asked at all. A new session now asks until
  you answer, and no is recorded with `atlas settings add-project --no DIR`, `atlas settings orchestrate --no` or
  `atlas settings offer --no`. The Claude Code settings question is one of these now, at most 60 tokens a session;
  a yes or a no to it is no answer to the orchestrated question. The "on by default" notice is shown once.
  Ignoring it means hearing it again at each new session; a resume or a compaction does not repeat it. Folders already
  marked as asked stay that way. A session with no user (`claude -p`, the SDK) asks nothing, so its stdout stays yours,
  and spends no question: atlas reads `CLAUDE_CODE_SESSION_ATTENDED`, else `CLAUDE_CODE_ENTRYPOINT`, which Claude Code
  sets for its hooks but does not document; with neither it asks. A state file that cannot be written means no
  question at all, rather than one that returns forever with a `--no` that fails. A yes counts once it lands: not
  when the write failed, at a terminal or with `--yes`, and not when the setting needs a newer Claude Code (the command
  then exits 1, and the question stays at each new session, at a terminal's answers too, until the upgrade or `--no`). `offer --yes` names what to turn
  on: bare it is refused, and an atlas switch named there answers nothing.
- **`atlas doctor` names Claude Code's 15-minute wait.** After one failed start of the server, Claude Code does not
  start it again for 15 minutes, even with the cause fixed unless the fix changed its command, which reads as the fix not working. atlas cannot change
  that; the wiki now says so, and the doctor names the wait when it can see it.
- **Compaction's summary is not filed as an agent's report.** Its summariser fires SubagentStop, so in orchestrated mode
  a long summary hit the report backstop, was blocked, and was filed whole as a report. It is now left alone.
- **Test digests name more failures.** A `cargo test --nocapture` failure with no `---- NAME stdout ----` block is
  named from libtest's closing list with its panic; nextest's current `stdout` and `stderr` blocks are a failure's
  block, and `error: test run failed` is not a build error; a check log's summary keeps the line naming its detail
  log; a `NAME: FAILED` verdict is kept like `NAME: FAIL`; and overlapping `--failures` items are shown once.
- **Compaction hooks no longer read the whole transcript.** The checkpoint and PreCompact hooks read only the part
  since the newest compaction, so their time no longer grows with the session's history. A checkpoint's turn refs
  now carry a byte offset (`turn:...:b<offset>@hash`); line refs written before still resolve.
- **`recall {query}` says what it left out.** Records scoring near zero were dropped with no word, which in a small
  store can be real matches (4 records held the words; 2 came back), and the limit cut silently too. The answer now ends
  with `+N more scoring near zero` and `+more (limit=L)`.

### Known limits

- `--memory-export` run beside servers that are writing can make a write answer `export refused: ... present and
  never exported by atlas` for a while; the write itself is stored, and a later export clears it. Seen only under
  heavy load with exports and side commands running.
- Under heavy writes `--memory-snapshot` may print `snapshot skipped ... run it again`. Nothing is lost; run it again
  when the store is quieter.
- The first `add-project` diff can mark unchanged template lines as changed. The change itself is what it says.

## v0.2.1

A patch release with two fixes for the hooks.

### Highlights

- **Every hook call is accepted again.** Claude Code sends `tool_use_id` (and, on SubagentStop,
  `last_assistant_message` and `stop_hook_active`) to the `hook` tool, which did not declare them, so every
  PreToolUse hook call failed its schema check. The schema now declares every input `hooks/hooks.json` sends,
  and a test checks each one through `tools/call`.
- **Git writes are no longer routed.** The routing hook sent any command starting with `git` through its
  digest, so a failing `git push` or `git commit` came back as a tail with exit 0 and the failure was easy to
  miss (bloodfeast/atlas#3). Only read-only git subcommands (`status`, `log`, `diff`, `show` and the like) are
  routed now; a write anywhere in the command (after `&&`, `;`, `sudo`, `env`, inside `$( )`, backticks or
  `bash -c`) runs unrouted. A routed command that fails says so in the context with its exit code.

### Known limits

- A read behind a quoted `-C` value (`git -C "$(git rev-parse --show-toplevel)" log`) also runs unrouted:
  the safe direction, losing only the digest.

## v0.2.0

The code index reaches past Rust to six more languages, the tools and hooks cost fewer tokens for the same
answers, a new project gets its own vault, and the memory store holds up under several sessions at once. The
full list is under Features and Fixes below; the upgrade notes say what an update re-indexes and what changed
in the tool list.

### Highlights

- **Go, TypeScript and JavaScript, C#, Python, Java and Kotlin are indexed.** `symbol`, Read notes and Grep
  annotations work for them out of the box: through each language's SCIP indexer when it is installed (atlas
  never installs one), and through an owned scanner otherwise, which finds 98.8% to 99.8% of the indexer's
  definitions on the measured projects. A prebuilt `index.scip`, written by CI or in WSL, is read too.
- **`symbol` follows gRPC and crosses repositories.** A `.proto` rpc answers with its generated stubs in every
  language above, the calls made through them and the server types that implement it; a library's definition
  answers with the call sites in the code roots that use it as a package.
- **Test digests for more runners:** `go test` (plain and `-json`), pytest, Maven and Gradle, `dotnet test`,
  jest and vitest, each with its totals and each failing test with its message and location.
- **A quarter of the tool list.** `tools/list` is 14 tools and about 1,480 tokens (it was 47 tools and 5,930),
  with every op described in the `atlas://help` resource, and every answer is plain text unless you ask for
  `format: "json"`. The v0.1 tool names still work in this release.
- **The hooks inject less for the same answers.** Context selection (on by default) sends a record that
  restates one already injected as a one-line pointer and trims each pointer to the clause that matches the
  prompt: injected tokens 6,744 -> 6,369 on the memory evaluation at equal or better pass rates, lane briefs
  17,709 -> 14,308. A repeated read answers `unchanged since your call #N`, and the recall hooks can be given a
  per-session budget.
- **Exact references.** Refs are content-addressed (`key@hash`, `src:path:a-b@hash`), so `recall` returns the
  exact text a ref named or says it changed; `read_note` takes line ranges and a token budget.
- **A project gets its own vault.** The first session in a folder atlas does not index offers to create one,
  and `atlas settings add-project --new` works on a fresh install: it moves the current setup into a project
  itself, shown first and reversed with the new project by one undo. add-project writes into the right
  project's entry of a multi-repository configuration.
- **`atlas settings undo` shows what it reverts and asks first** (without a terminal it needs `--yes`).
- **The deny-list holds everywhere.** `.env*`, key and credential files are never indexed, read or written
  through a vault, `atlas digest` and `atlas crop` refuse them, and a denied file is never kept as a handle.
- **The memory store under several sessions.** Two sessions writing at once no longer get errors for
  committed writes, a session killed mid-write no longer leaves `--memory-check` failing, and a note being
  replaced by another program no longer reads as missing.
- **Experimental, off by default: compaction support** and session transcripts as a source. Nothing of it runs
  unless you turn it on (`compact.enabled`, `memory.transcripts`); see Features.

### Upgrade notes

- `atlas settings add-project` with no folder named (and no `--yes`) now only shows what it would do for the
  current folder, and writes nothing; it used to add the current folder. Name the folder (`atlas settings
  add-project .`) or pass `--yes` to write.
- The 47 tool names of v0.1 (`read_note`, `list_notes`, `create_note`, `decision_queue`, `ids`,
  `supersede`, `journal_status` and the rest) still work as tool names in this release but are no longer
  listed; v0.3.0 removes them. Calls that read a JSON answer from `read_note`, a write tool or a discussion
  tool add `format: "json"`. A permission allowlist naming the old tools (`mcp__atlas__read_note`) needs the
  new ones (`mcp__atlas__note`, `mcp__atlas__edit`, `mcp__atlas__discuss`, `mcp__atlas__miss`).
- The code index schema is `code-7`, with new `code_units` and `code_packages` tables. A v0.1 database is
  migrated in place on first use: its rows stay and answer at once, and every code root is re-indexed once in
  the background, to record its packages.
- A code root holding Go files is re-indexed once after the update (its Go files are now indexed as Go); a
  root of Go alone no longer reports a Rust unit.
- A code root holding TypeScript or JavaScript files is re-indexed once after the update, and its `symbol`
  answers name the new units in their note. Units in test-input folders (`fixtures`, `testdata` and their
  kin) are indexed but not named there.
- A code root holding Python files, or an `index.scip` file in a unit's folder, is re-indexed once after the
  update; such a file is now read as that unit's index (`code.roots[].scip: false` turns this off).
- A code root holding C# files is re-indexed once after the update. A C# explicit interface member is now
  stored under its own name (`GetEnumerator`), not its interface-qualified one.
- A code root holding Java or Kotlin files is re-indexed once after the update.
- A code root holding `.proto` files or generated gRPC code is re-indexed once after the update; its rpc call
  sites in comments and strings are gone, and calls through generated stubs are of kind `rpc`.

### Features

- **Refs are content-addressed.** A ref names exact text: `key@hash` (a record version) or `src:path:a-b@hash`
  (lines of a vault or source file). `recall {key: "key@hash"}` returns that version verbatim, or says it changed
  or dangles, and `recall` resolves `src:path:a-b@hash` lines refs.
- **Experimental, off by default: compaction support.** Not yet tested live, so none of it runs unless you add
  `"compact": {"enabled": true}` to atlas's configuration (`atlas.json`); existing setups get nothing new. With it
  on:
  - PreCompact writes a checkpoint (journal entry kind `checkpoint` in lane `session:<id>`): the active lanes and
    their current step, open questions, owed `next` entries and new rulings as `key@hash` refs, plus the in-task
    state a mid-task compaction would cut (files touched, the last tool calls with `turn:` result refs, a pending
    call, the last visible line of intent). Only visible content is read; thinking is never read or stored, and
    every transcript-derived line passes secret redaction. The card lists lanes and rulings only from the active
    project's own store; with no project it keeps to the session's own lane. A transcript Claude Code has not
    written yet is waited for briefly, then read from the session it was forked from, and a checkpoint that still
    has none says `transcript unread` rather than "0 context tokens".
  - After the compaction the SessionStart hook injects the checkpoint as a card (`compact.card_tokens`, default
    400), checks every ref in Claude Code's summary and reports changed or dangling ones. Each compaction is
    logged with its ratio, a lookup repeated after a compaction is logged as a post-compaction miss, and
    `--token-report` ends with the compaction line.
  - `atlas settings compact-md` offers, once per project, a Compact instructions block for the project's
    `CLAUDE.md` (nothing written without a yes; `atlas settings undo` removes just the block). With compaction
    off it changes nothing and says how to turn it on.
  - `compact.defer` (off by default even then) holds a proactive automatic compaction until the running tool
    chain ends, never past a hard ceiling.
  - The offline measurement works either way: `atlas --compact-report --transcripts DIR` replays recorded
    sessions (numbers only) with the card's size and coverage, the hooks' latency and `--sweep T,T,...` (the
    token side of each compaction point, plain and atlas paired per threshold); `cargo xtask memory-eval` gains
    (g), answers that survive a simulated compaction as refs; `cargo xtask compact-live` stages the live
    head-to-head (it spends Claude usage, so it is run by hand). Design: `docs/design/compaction.md`.
- **Session transcripts as an opt-in source.** With `memory.transcripts` on (off by default), each compaction
  indexes the session transcript's visible turns (never thinking), secret lines redacted and a secret file's
  read result kept out whole. `recall {key: "turn:<session8>:<line>@hash"}` returns the exact earlier turn and
  `recall {query, kind: "turn"}` searches turns; both see the asking project's turns only. With `compact.enabled`, the ref check
  after a compaction resolves `turn:` refs too. Threat note: `docs/design/compaction.md`, section 9.
- **The tool list is a quarter of its size.** `tools/list`, sent every time a client loads the tools, is 14
  tools and about 1,480 tokens (it was 47 tools and 5,930). The note, write, discussion and misses tools sit
  behind four tools with an `op` argument (`note`, `edit`, `discuss`, `miss`); `lookup` without an id lists
  identifiers, `remember` with `old` supersedes, and `recall` takes `op: "status"` and `op: "log"`. Listed
  schemas name only the common arguments; every tool, op and argument is described in one fetched text, the
  MCP resource `atlas://help` (or `note {op: "help"}`). Measured on the 100-question memory evaluation: the
  same answers, every fact found.
- **Every answer is text by default.** `read_note` and the other tools that answered only in JSON now answer
  in plain text, and `format: "json"` gives the JSON on any tool. `read_note` is one `vault:path sha256=...`
  line then the note as written, without the parsed frontmatter repeated beside it. Lists keep every item:
  the work-queue tools are never cut.
- **The recall hooks have a session budget, and a weekly misses pass.** `recall.session_budget_tokens` caps
  what one session's (or one subagent's) recall injections may use in all; past it the hooks inject nothing
  more and `recall {query}` still answers. It sits beside the per-prompt `recall.budget_tokens` and the
  confidence threshold `recall.inject_floor`; the defaults leave injection as it was (no session cap, floor 0)
  until both are tuned on hit data. `--token-report` prints a `hook injections` line beside the "brief without
  atlas" row, to judge the pointers against the brief they replace, and `atlas --misses-week [--since TIME]`
  lists the ten open misses seen most in the last seven days and the ten injected keys that cost the most.
- **The hooks inject less for the same answers (context selection, `recall.select`, on by default).** A record
  that restates one already injected (the rule kept in INTENT and in a memory file) is sent as
  `- KEY: near-duplicate of OTHER`, judged by both the records' cosine and their words, one way, so a record that
  adds something is never hidden. A pointer carries the clause of its record that matches the prompt in place of
  its first line, and a lane brief keeps its handoff, brief, open questions and next step whole while older
  entries are cut to their matching clauses. Every excerpt is an exact substring of its record. Measured on the
  memory evaluation's hook replay against the same replay with selection off: injected tokens 6,744 -> 6,369
  (floor 0) and 6,049 -> 5,654 (floor 0.663) at equal or better pass rates (informed 62 -> 63, strict 52 -> 53 at
  floor 0), and lane briefs 17,709 -> 14,308 with every lane question still answered from the injection alone.
  `--token-report` gains a `context selection` line with what trim and collapse saved.
- **A repeated read is one line.** A read tool called again with the same arguments answers
  `unchanged since your call #N`, or only the lines that changed, and the reply names `fresh: true`, which
  gives the whole answer. With the hooks on, the PreToolUse hook names the agent and repeats are per agent,
  and a compaction forgets the session's answers; without them, repeats are per server process, so a
  subagent asking what its parent asked follows the `fresh: true` the reply names (measured: every fact
  reached). The work-queue tools and lane journals always answer whole. Measured on the memory evaluation's 100 first calls made twice by one
  agent: the repeats cost a fraction of the first pass, with every fact the first pass found.
- **Line ranges.** `read_note` takes `lines` (`A-B`) or `budget_tokens` (whole lines up to about that many
  tokens, then a `+N more lines (lines=A-B)` line). A record `recall {key}` reads names the line of its source
  file it came from (`@L1234`, or `@path:line` when the key does not name the file). `read_note` also reads a
  source's files, and `recall.pointer_anchors` (off by default, as measured) puts `@src:path:line` on every
  `recall {query}` pointer.
- **`symbol` for Go, TypeScript, C# and any other language with a SCIP indexer.** `atlas --code-index NAME
  --scip FILE` now reads an index from scip-go, scip-typescript, scip-python, scip-java, scip-dotnet or any other
  SCIP indexer, not only rust-analyzer's: definitions with their kinds and ranges, references with their
  callers, and impl edges from the index's implementation relationships (`Shape for Square`). Indexes without
  kinds or enclosing ranges (scip-dotnet, scip-python) get kinds from the definition's line and ranges from
  the brace matcher. Rust results are unchanged: the same rows, byte for byte, on atlas's own source.
- **Go is indexed out of the box.** A code root's Go modules (each `go.mod`) are indexed by scip-go when it is
  installed (found on `PATH`, `GOBIN`, `GOPATH/bin` or `~/go/bin`; atlas never installs it), in the background
  the way rust-analyzer runs, and by an owned scanner otherwise: definitions, kinds, containers and item
  ranges, so `symbol`, Read notes and Grep annotations work for Go with nothing installed. An edited Go file
  has its ranges right again at once. Measured on gin (98 files, 24k lines) against scip-go 0.2.9: the
  scanner finds 99.7% of scip-go's definitions with no false ones and agrees on 100% of item end lines; the
  scip-go rows hold every `func`, `type`, `var` and `const` a grep finds in the files scip-go indexes, and the
  scanner covers the files behind build tags it skips.
- **`go test` digests.** `go test` output (plain or `-v`) gets a digest like `cargo test`'s: totals by test
  and package, each failing test or subtest with its `file_test.go:LINE` message, a panic with its first frame
  outside the runtime, and `atlas slice --failures` over the failing tests' output.
- **Newer SCIP files decode.** The typed ranges that scip-go 0.2.9 and other newer indexers write instead of
  the deprecated packed ones are read; before, such a file failed with "occurrence range is not 3 or 4
  values".
- **TypeScript and JavaScript are indexed out of the box.** Each package of a code root (the shallowest
  `tsconfig.json` or `package.json`, with its workspace members) is indexed by scip-typescript when it is
  installed (found beside its shim on `PATH` or under `npm root -g`; atlas never installs it and never runs a
  package manager), and by the owned scanner otherwise. A package whose dependencies are not installed is
  still indexed by scip-typescript from its own sources, unless a `tsconfig.json` of it extends a package
  (`scan only: no node_modules: run npm install`, said too when such a run fails). A JavaScript package without a `tsconfig.json` gets one
  inferred into atlas's own folder, so nothing is written into the project. Measured on got (92 files, 67k
  lines) against scip-typescript 0.5.2: the scanner finds 98.8% of its definitions (every type, function and
  method) and agrees on 99.8% of item end lines; the scip-typescript rows hold every top-level declaration a
  grep finds in the files it loads. On microservices-demo's currencyservice and on express (JavaScript, 142
  files, 22k lines, indexed without `node_modules`): 100% on all three.
- **C# is indexed out of the box.** Each solution of a code root (a `*.sln` or `*.slnx` claims its folder's
  projects; a `*.csproj` outside every solution is its own unit) is indexed by scip-dotnet when it is installed
  (found on `PATH` or in `~/.dotnet/tools`; atlas never installs it), and by the owned scanner otherwise.
  scip-dotnet restores the solution's packages and runs a design-time build itself, so generated code such as
  gRPC stubs is indexed; a failed run names the .NET SDK the `global.json` asks for. scip-dotnet writes no kinds
  and no ranges: kinds come from the definition's line and item ranges from the brace matcher or the scanner,
  and code an `#if` left out of the build is filled in by the scanner. Measured on serilog (217 files, 25k
  lines) against scip-dotnet 0.2.16: the scanner finds 99.8% of its definitions (every type and callable) and
  agrees with the brace matcher on 98.3% of item end lines (the rest are the matcher's mistakes at base lists
  and `where` clauses); every type and access-modified member a grep finds in the files scip-dotnet loads is
  in the index, 1,711 from scip-dotnet and 20 from the scanner in inactive `#if` branches.
- **Python is indexed, and prebuilt indexes are read.** Each Python project of a code root (a `pyproject.toml`,
  `setup.py` or `setup.cfg`, else a `requirements*.txt`) is indexed by scip-python when it is installed (found
  beside its shim on `PATH` or under `npm root -g`; atlas never installs it) on Linux and macOS, and by the
  owned scanner otherwise, which now reads indentation blocks: classes, methods, constructors, attributes set
  through `self`, nested functions, module attributes, with item ranges. scip-python does not run on Windows,
  so there a unit's note says to write its `index.scip` with scip-python in WSL or Docker. Any unit of any
  language may be indexed from such a prebuilt file (an `index.scip` in its folder, or the file
  `code.roots[].scip` names), written by CI or by you: it is read when the unit has no indexer, when it is newer
  than the indexer's last run and than the unit's sources, or when the run fails; a source file newer than the
  prebuilt file is scanned instead, so a stale index gives scanner rows at the right lines, never wrong ones.
  Measured on flask (83 files, 18k lines) against scip-python 0.6.8: the scanner finds 99.7% of its
  definitions (every class and function) and agrees on 100% of item end lines; every `def` and `class` a grep
  finds is in the index, 1,584 from scip-python and 38 from the scanner (overloads and property setters that
  scip-python leaves without a definition). With `src/flask/app.py` edited after the prebuilt index was
  written, its 41 definitions came from the scanner at their new lines and none was missing.
- **Java and Kotlin are indexed.** Each Maven or Gradle build of a code root (a Gradle settings file claims its
  builds, a parent `pom.xml` its modules) is indexed by scip-java when it is installed (found on `PATH` or in
  Coursier's folder; atlas never installs it) on Linux and macOS, and by the owned scanner otherwise: types,
  members, enum constants, records, and the local and anonymous classes in method bodies, with item ranges.
  scip-java does not run on Windows, so there a unit's note says to write its `index.scip` with scip-java in
  WSL or Docker, and atlas reads it as a prebuilt index. Kotlin is indexed by the scanner (classes, objects,
  functions, properties, enum entries, type aliases, with item ranges): scip-java's Kotlin plugin does not
  load in Kotlin 2, so a unit of Kotlin alone is never run through scip-java. Measured on gson (264 files, 57k
  lines) against scip-java: the scanner finds 99.0% of its definitions and agrees on 99.9% of item end lines;
  every type and access-modified member a grep finds in the files scip-java's build compiled is in the index,
  3,749 from scip-java and 268 from the scanner (members of anonymous classes, which scip-java keeps local).
  On kotlinpoet (131 files, 40k lines) the scanner finds 99.5% of the declarations a grep finds.
- **gRPC across languages: `symbol` of an rpc names its stubs, its callers and its implementations.** A `.proto`
  rpc is linked to the stubs generated for it in Go, TypeScript, JavaScript, C#, Python, Java and Kotlin
  (`*.pb.go`, `*_pb2_grpc.py`, `*Grpc.cs`, `*Grpc.java` and the like: generated-file patterns and stub naming
  are data in each language's row), to the calls made through those stubs (exact, from the SCIP index, with
  their callers, `getCart` and `GetCartAsync` stored under `GetCart` too) and to the server types that
  implement it (a method of a type that extends, implements or embeds the generated server type, through
  hand-written base classes). Word matches stay only where no index resolved the call (a `.proto` loaded at
  run time), and never in comments, strings, definitions' names or generated code. The `.proto` definition
  answers first and generated code last. Measured on microservices-demo (Go, C#, Python, Java and Node
  services around one `protos` folder): all 15 rpcs, 216 stub links, 32 exact call sites for 14 of them, 13
  implementations of 12 (the other three are Node services that load the `.proto` at run time), and the
  `call (mention)` rows down from 1,017 to 7.
- **`symbol` across repositories: a library's definition with its users' callers.** When one code root uses
  another's code as a package (a Go module, an npm package, a Maven artifact, a Rust crate), its references
  into it are stored with their callers, so `symbol Mean` answers the library root's definition and the
  service root's call sites, and a call into generated gRPC stubs that live in a shared root is an rpc call
  site. Packages compare by name, never by version (a sibling folder, a commit and a release tag are one
  package). Each root is still indexed on its own; one whose packages change marks its users for re-indexing.
  Measured on a Go library and service (17 of 17 references), microservices-demo's Go services with their
  generated code in a shared module (372 of 372, the same 19 rpc call sites as with the code in each service)
  and a TypeScript package used through a `file:` dependency (22 of 22), with no false links. C# is not
  linked (scip-dotnet writes no package name for a project's own code).
- **pytest digests.** pytest's own totals, each failing test's report by the name its header gives, with its
  first `E` line, a `+ where` line and the location; passes and skips are items with `-v`.
- **Maven and Gradle test digests.** `mvn` (Surefire) and `gradle` test output gets the totals (Maven's from
  every module's `Tests run:` line; Gradle's from the line a failing test task prints) and each failing test
  with its exception and the first frame in the project; a passing Gradle run prints no count, and its digest
  says so.
- **`dotnet test` digests.** Each failing test with its message, the first stack frame with a source file and
  xUnit's `Expected` and `Actual` lines; the totals of every test assembly of a solution's run are summed.
- **jest, vitest and `go test -json` digests.** jest and vitest output gets the runner's own totals and each
  failing test by its full name, with the first stack frame in the project and its message (jest's
  `Expected` and `Received` too), read through the colour codes these runners write on Windows. `go test
  -json` gets the `go test` digest, each package's output read whole though packages interleave.
- **The first session offers the project a vault.** Opening Claude Code in a folder atlas does not index
  asks once, in one line: *create a vault for this project at `<path>`? yes / other folder / no*, yes by
  default. Yes adds the project as a source (and a code root) as before and creates an Obsidian-compatible
  vault for its notes in `vaults/` beside atlas's configuration, registered under `vaults`; other folder
  registers a folder you name (an existing vault is used as it is). `atlas settings add-project --vault` and
  `--vault-at FOLDER` do the same by hand, and `atlas --setup` asks it in the terminal. One `atlas settings
  undo` reverts the whole answer and removes the vault folder only when atlas made it and it holds nothing
  but atlas's note.
- **A new multi-repo project gets its vault too.** In a configuration with `projects`, the *new project here*
  answer (and `atlas settings add-project --new`) also creates the project's vault at `vaults/<name>-notes`
  beside atlas's configuration, never inside a repository, and registers it in that project's entry, so only
  that project sees it. `--vault-at FOLDER` puts it elsewhere and `--no-vault` leaves it out; the same one
  undo removes the project file, the registration and a vault folder atlas made.
- `code.indexers` in the configuration: `false` runs no language indexer (every unit is scanned), or an object
  names each one's binary (`{"scip-go": "/opt/bin/scip-go", "scip-typescript": false}`), as
  `code.rust_analyzer` does for rust-analyzer.
- `code.roots[].scip`: prebuilt SCIP indexes for a root's units: `true` (the default) reads an `index.scip` in a
  unit's folder, `false` none, a path the root folder's file, or an object from unit folder to a path or
  `false`.
- `atlas settings add-project` writes the source and the code root in one configuration change, so one
  `atlas settings undo` reverts a project (it took two when the project held code).
- On Windows, a file changed just after the watcher started is no longer missed: the watcher arms its first
  directory read before it reports itself started (under load the gap reached seconds).
- In Claude Code's auto mode, routing no longer adds its exit tail (`|| echo atlas-exit-nonzero`) to a
  read or search command. The auto-mode classifier judged the command as written and refused the changed
  one, so every routed read was sent twice. Successful reads are still digested; a failing read arrives as
  Claude Code's own error text. Other permission modes are unchanged. The PreToolUse hook now passes
  `permission_mode`; a plugin installed before this release picks it up on update.
- `atlas settings add-project --dry-run` shows the change, the file it would go to and the project it would go
  into, and writes nothing.
- atlas's own fixed patterns (links, tags, dates, discussion posts, index markers, output digests, the
  `--recall-check` ground truth) are hand-written scanners instead of regular expressions, with the same
  results: 1.7x to 25x faster per line, and the recall-check ground-truth scan about 14x faster on one thread
  and about 140x faster across 16.
  The regex engine remains for what you write: `search mode=regex`, `grep`, and identifier and hook patterns.

### Fixes

- A repository folder whose name is over 32 characters no longer breaks every atlas run. Its default name
  (and the source made from it) is cut to 32 characters, with `-2`, `-3` added when two cut names meet, and a
  longer name listed by hand is cut the same way for its source. A vault, source or `ids` name over 32
  characters written by hand is refused when the configuration is read, and the message names the file and
  the key (`.../atlas.project.json: project.sources[0].name must be 1 to 32 characters ...`).
- `atlas settings add-project` in a configuration with `projects` writes into the project's own entry
  (`projects[].sources`, `projects[].code.roots`, `projects[].vaults`): the project named with `--project`,
  else the folder's own, else `default_project`; for a folder in no project a terminal lists the projects
  (and cancel) to pick from, and without a terminal or with `--dry-run` it is refused. It wrote the top-level lists, which every project reads, so
  one project's folder was indexed in all of them. A generated name is free against every project's names.
  Every check runs on the text that would be written before anything is written: a refused add-project
  leaves the configuration byte for byte as it was (it used to write and then fail with `at least one --vault
  NAME=PATH is required`, naming a flag that was never passed).
- A folder a project covers (a project root, its project file's folder, a listed repository) counts as
  indexed: the first-session question, `atlas --setup` ("This folder is not indexed") and add-project's
  "already indexed" check no longer treat it as new.
- Without a terminal, `atlas --setup` and `atlas settings offer` say plainly that nothing went wrong: no
  setting was changed because there was no terminal to answer in, and which command to run.
- The end-of-agent check ("agent X finished with no journal entry for lane Y") no longer fires when a subagent
  only paused while its own background work runs; it checks real finishes only. Its lane is the one the agent
  itself wrote under (its own `journal` and `handoff` calls), else the one its brief assigns ("Journal lane:
  X", "You are LANE X"), never a neighbouring lane the brief also mentions; when no single lane resolves it
  says nothing (#79).
- `handoff` without a `title` takes the text's first line as the title, as journal entries do, so the handoff
  is filed and shows in `journal_status` and `recall lane`; a text with no first line is refused with "title is
  required" (#80).
- Two sessions writing memory at once no longer get errors for writes that were committed. The automatic
  snapshot compared its copy with a live record count taken outside the copy, so the other session's commit
  failed the check, the write came back as an error, and a copy was left behind on every later write. The
  count is now compared only when no other session committed during the copy; a copy that raced a commit is
  removed and retried, and skipped until the next write if it keeps racing. A snapshot or export that still
  cannot be made is reported in the answer, never as an error, since the write is already committed.
- Two sessions opening a new memory store at once no longer fail with "database is locked" (the switch to
  WAL is retried, and the schema is made once, under the write lock).
- Exports are rendered and written under the store's write lock, so after two sessions write at once the
  export files show the newest commit (`--memory-check` could find `MEMORY.md` one record behind).
- `atlas settings add-project --new` works on a fresh install. It said "no projects yet: run migrate-projects
  NAME first"; it now moves the current setup into a project itself (the change shown, backed up, and reversed
  with the new project by one `atlas settings undo`). It asks for that project's name in a terminal; without one,
  and on `--dry-run`, it takes the name of the folder holding `atlas.json`, made a valid project name, with `-2`
  on a clash with the new project. The first session in a folder of repositories offers that new project, where
  it used to ask the single-repository vault question.
- A memory session killed mid-write no longer leaves `--memory-check` failing for good. The `.jsonl` copy was
  appended after the commit, so a kill in between left it a batch short; it is now appended under the write lock
  before the commit, the store records where the committed lines end, and the next write cuts off anything a
  crash left past that point. (A copy already left short by an earlier version still reports the gap.) Each
  line past that point is judged against the store: a fresh store records the point when it is opened, so its
  first write killed before the commit is cut too, and a line an older atlas committed is never cut. An
  uncommitted line with committed lines after it is overwritten in place by a `void` line of the same length on
  the next write, and `--memory-check` reports it as a warning until then.
- A rule remembered without a leading `- ` no longer breaks the rules export (`rules: 3 records render to 2
  pieces`): it is written as one list item, `- DATE AUTHOR: text`, with later lines indented; a body with a blank
  line or a second item is refused. `--memory-check` on a fresh store no longer reports exports that were never
  written (and would hold no records) as missing.
- `atlas --hook` (the command hook) no longer loses its token-ledger rows: the thread that writes them is waited
  for before the process exits, so `--token-report` counts command-hook answers, and a first SessionStart no
  longer leaves an empty `atlas.sqlite` beside a hot journal.
- `atlas --setup` and `atlas settings offer` without a terminal no longer record every setting as offered, so a
  later run in a terminal still asks about each one.
- `atlas ruling --by NAME --defines ID "text"`, the documented form, works: it said "the text is empty" because
  the `--defines` list took the text too. A list now ends at a word with a space, and a list that runs to the end
  leaves its last word as the text; `-- TEXT` works as before.
- `atlas settings undo` shows what it would revert (when, which command, and per file what goes back, with line
  counts) and asks first; without a terminal it needs `--yes`. After a command that changed nothing it used to
  restore an older, unrelated backup without a word.
- `remember` accepts the 16-digit sha its ok line prints as `expected_sha` (any prefix of 12 or more hex digits,
  any case); it took only the full 64. A stale one says to recall the record, not to "read the note again".
- `journal`, `handoff` and `remember` refuse an empty or blank `lane`, journal `text` or `body` (they were stored,
  as `journal::0001` for an empty lane).
- `format: "json"` works on the memory tools, as the Tools reference says it does on every tool: `recall {key}`
  and `recall {op: "status"}` answer as fields, every other memory answer as `{"text": ...}`. They answered text
  whatever was asked.
- The README gave the recall hook's budget as 600 tokens; it is 150 (`recall.budget_tokens`).
- `symbol` says `ROOT: indexing` until a root's first walk since atlas started has finished, instead of
  `stale ROOT: changed, reindex pending`. Its coverage notes (`definitions only (scan only: ...)`, about nine
  lines in a three-repository project) come with a session's first `symbol` answer only; state lines still come
  every time. Its description lists the languages it indexes, not "a Rust or WGSL name".
- The `hook` tool is no longer annotated read-only (it writes journal entries).
- `atlas --help` lists every mode, subcommand and flag; `--code-index` prints its row and write counts as
  words and exit codes as numbers, not Rust debug structures.
- `atlas slice --lines 5-1` is refused (the range runs backwards) instead of exiting 0 with nothing selected.
- `grep` cuts a matched line over 400 bytes to a window around the match and names the line's length.
- `recall` no longer prints a title made from the body (a decision's) above the body that starts with it,
  lists no 0.0-score pointers, and with no match says when the store holds no records yet (and does not point
  a setup without exports at its exports).
- `search` in `substring`, `regex` or `fts` mode reads vault notes only; an empty answer now says so and points
  at mode `sections` and `grep` for the sources.
- The server instructions begin "Markdown vaults served" (Obsidian is not needed).
- On a build from source, the settings offer no longer lists updates as on (a source build never updates
  itself); the update pin example names 0.2.0.
- Paths read from the environment use one separator on Windows (`Z:\home\atlas.json`, not
  `Z:/home\atlas.json`), and `migrate-projects` writes the default `db` with `/`.
- The README's "any language" means any programming language; ranked search (`fts`, `sections`, `recall`)
  splits words on spaces and punctuation, so text written without spaces (Chinese, Japanese, Thai) is found
  with `search` in substring mode or `grep`. The Memory store page says that `atlas --hook`, one process per
  event, injects no subagent lane recall at `SubagentStart` (the brief is held by the MCP server).
- A note being replaced by another program (an editor saving by rename, another atlas session's write) no
  longer reads as missing for a moment. On Windows the note's name can vanish mid-replace for as long as the
  writer is held up inside the rename (up to about 32 ms measured on a heavily loaded machine), and a read that
  landed there said "note not found"; atlas now looks again for up to 100 ms before it says so.

### Security hardening

- The deny-list (`.env*`, keys, keystores, credential files) now holds everywhere, as the README promised: a
  vault note such as `credentials.md` is no longer indexed, searched, read or written, `atlas digest` and `atlas
  crop` refuse such files, and a denied file Claude Code reads is never kept as a `slice` handle.

### Known limits

- Compaction support and the transcript source are experimental and not yet tested in live sessions; both are
  off unless turned on.
- scip-python and scip-java do not run on Windows: there their projects are scanned, or read from an
  `index.scip` written in WSL or Docker.
- C# code is not linked across code roots (scip-dotnet writes no package name for a project's own code).

## v0.1.1

A fix release.

### Highlights

- **Each file is indexed once in a multi-repository project.** A repository whose folder is already
  configured as a source or a code root no longer gets a second one, and its Markdown source leaves out the
  files another source already indexes (a `docs` source inside it, the project's `INTENT.md`). Search
  answers no longer list the same section twice, and recall still puts records about the active repository
  first.
- **`atlas --config PATH settings ...` works.** `--config` and `--project` before a subcommand (`settings`,
  `run`, `slice`, `grammar`, `ruling`, `entry`) now apply to it; before, `atlas --config PATH settings
  migrate-projects NAME` failed with "expected on or off".
- **Plain paths in output.** `atlas --memory-tree` and the `list_sources` tool print Windows paths as
  `C:\...`, without the `\\?\` prefix.

### Features

- `cargo xtask release --push` checks that openssl is on PATH and that `ATLAS_SIGN_KEY` names a readable file
  before it runs the tests and builds, so a publish that cannot sign fails in seconds, not at the end.

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
