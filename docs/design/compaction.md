# Compaction: checkpoints, content-addressed refs, transcript recall (design)

Status: design for v0.2.0 item 7. Release gate: v0.2 ships only with the compaction ratio, the dangling-ref
rate and post-compaction misses reported by `--token-report` and measured on replayed sessions.

v0.2.0 ships these features experimental and off by default (`compact.enabled`, default false), until they are
tested live: off, PreCompact only forgets the session's repeated answers (and, with `memory.transcripts`, indexes
the transcript), and SessionStart injects no card or ref report.

Goal: higher compaction ratios and more accurate references. A compacted session should carry keys and
deltas, not restated content, and every key it carries should either resolve to the exact text it meant or
be reported as stale.

## 1. What the hooks can and cannot do (measured and documented)

- **PreCompact** gets `trigger` (`manual` or `auto`), `custom_instructions` (what the user typed after
  `/compact`, else null) and `transcript_path`. It can block compaction (exit 2 or `decision: block`).
  `additionalContext` is not acted on for this event. Claude Code writes the summary itself, but what a PreCompact
  hook returns as plain text steers it: a command hook's stdout on exit 0 and an `mcp_tool` hook's tool result
  both land in the summarizer's instructions (the web docs do not say so). That the text comes after any `/compact`
  argument is read from the binary, not observed: no run had both.
  - Evidence: one manual run of each on claude.exe 2.1.318, journal:COMPACT-INPUT:0004 (command hook, body 4,249
    chars), journal:COMPACT-INPUT:0005 (`mcp_tool` stub, body 3,780) and journal:ATLAS-V03:0266 (the real atlas
    `hook` tool, body 3,885). There is no plain control on 2.1.318; the 11,357 baseline is from an earlier version,
    so the summaries' free format is the evidence that does not rest on it.
  - Untested: the automatic trigger (the binary appears to run the same path, not observed).
  - What atlas does: with `compact.enabled` on, once the checkpoint is written, the hook replies with
    `COMPACT_INSTRUCTIONS` (`src/claude_settings.rs`; 1,344 bytes, 336 tokens at 4 bytes per token), the plain text
    and nothing around it, by both `atlas --hook` and the `hook` tool. Off, it replies with nothing, as before. A
    deferred compaction replies `decision: block` as before and gets no text.
  - On failure the reply is empty and Claude Code compacts with its own instructions, as it would without atlas:
    a checkpoint that failed to write, or one with no chain card (a transcript atlas could not read), since the text
    tells the summarizer to leave out what the card lists.
  - What the reply adds to the hook: on the live `mcp_tool` form, nothing waited for. On the command form, one
    token-log row, waited for at exit (main's PreCompact sends no row); that row is unmeasured.
  - The echo: on a manual `/compact` the text also lands in the conversation, on the display line Claude Code writes
    as `local-command-stdout`, about 340 tokens on each later request until the next compaction. The line is on the
    resumed chain (journal:ATLAS-V03:0266's transcript), and a live request body holds that line without text. No run
    measured a turn after it, since all three jobs skipped the scripted turn. The automatic trigger's path is unread.
  - Every byte of the reply is now instructions: nothing else may reach stdout on this path. Diagnostics go to
    stderr.
- **PostCompact** gets `compact_summary` and has no decision control.
- **SessionStart** with `source: compact` runs after the summary is in place. Its `additionalContext` is
  injected at the start of the compacted conversation. Hook strings are capped at 10,000 characters (past
  that, Claude Code writes the text to a file and passes a 2,000-character preview).
- **The transcript records the compaction.** Observed on current Claude Code builds (not documented, so
  parsed defensively): a `type: system, subtype: compact_boundary` record whose `compactMetadata` holds
  `trigger`, `preTokens`, `postTokens` and `durationMs`, followed by a `type: user` record with
  `isCompactSummary: true` whose `message.content` is the summary.

**Blocking and deferral (documented).** The hooks reference says a PreCompact hook blocks with exit code 2
or `decision: block`. For an auto compaction triggered proactively before the context limit, Claude Code
skips it and the conversation continues uncompacted. For one triggered to recover from a context-limit
error the API already returned, the error surfaces and the current request fails. Auto compaction runs
between API calls, so in an agent loop it can land between two tool calls of one task. That makes a "compact
at the next quiet point" option possible (`compact.defer`, off by default):

- PreCompact with `trigger: auto` blocks while the session is mid-task, read from the transcript's tail:
  the last assistant message ended in a `tool_use` (a tool chain is running) rather than a final reply.
- It never blocks past a hard ceiling: the context window minus a margin of three times the largest
  per-request context growth seen in the session, and never below 40,000 tokens of headroom. Past the
  ceiling, or with no usage data, the compaction goes ahead.
- A blocked proactive compaction is re-offered on later requests, so the next quiet point compacts. Each
  deferral is logged (`compact:defer`) with the context it was at, and the sweep measures it as its own arm.
- A manual `/compact` is never blocked.

Whether a deferred compaction is re-offered at every later request is not documented, so the first live
run checks it before the option ships. Either way, the checkpoint carries the in-task working state a
mid-task compaction would cut, as visible records: the current step (the newest `next` entry or brief),
the chain card's keys for every part of the segment (section 8), and the files touched since the last checkpoint.

**No reasoning content.** The checkpoint and the transcript source read only what is visible: user
prompts, assistant text, tool calls and tool results, decisions, files touched and lane state. Thinking
blocks are skipped by type and never stored, indexed or measured. Losing the model's reasoning at a
compaction is accepted.

The summary itself is the one thing atlas cannot write. So the steering levers are:

1. what the session can point at instead of restating (records exist, with refs that resolve verbatim);
2. what SessionStart(compact) injects next to the summary (the checkpoint card and its chain card, 1,165 tokens at the one point measured);
3. the Compact instructions the PreCompact hook returns with `compact.enabled` on (section 1);
4. the same text as a block in the project's `CLAUDE.md`, a manual choice through `atlas settings compact-md` only
   (asked, never written without a yes). Neither SessionStart nor `atlas settings offer` raises it: in `CLAUDE.md`
   alone it showed no reliable effect, and a project with the block gets the text twice and should drop it;
5. `/compact <instructions>` typed by the user (out of atlas's hands).

## 2. The four parts

### (a) PreCompact writes a checkpoint record

Today PreCompact writes one `journal` note (`compaction (trigger) at N context tokens`) in lane
`session:<id>`. It becomes a structured checkpoint: a `journal` record of entry kind `checkpoint` in the
same lane, whose body is lines of refs, never restated text:

```text
lanes: build-7@3f9c2a1b7e40 (next: journal:build-7:0012@...), ...
open: journal:build-7:0009@... (question), ...
owed: journal:build-7:0010@... (next), ...
rulings: rules:r00412@..., rules:r00413@...
since: journal:session:<id>:0003@...   (the previous checkpoint)
```

"Since the last checkpoint" is measured from the previous checkpoint of the same session lane (or the
session's first hook event). Lanes are the lanes with an entry since then; open decisions are unanswered
`question` entries and open decision-queue items; promises owed are `next` entries not yet followed by a
`commit` in their lane; rulings are `rule` records whose head version is newer than the checkpoint.

SessionStart(compact) then injects the card: the checkpoint's own ref and its lines, budgeted (default
600 tokens, well under the 10,000-character hook cap), plus the ref report of (d).

### (b) Content-addressed references

A ref names exact text:

- `key@hash`: a memory record. `hash` is the first 12 hex digits of the version's SHA-256 (the store
  already keeps a per-version `sha`; versions are append-only, so every hash ever handed out stays
  resolvable).
- `src:path:a-b@hash`: lines `a` to `b` of a vault or source file. `hash` is the first 12 hex digits of
  SHA-256 over those lines joined with `\n` (CRLF read as LF).
- `turn:<session8>:<n>@hash` (part c): one transcript turn, by its line `n`; a checkpoint writes
  `turn:<session8>:b<offset>@hash`, the same turn by its record's byte offset (section 8).
- Chain keys (section 8): `turn:<session>:b<first>-b<last>@hash` (a group of records),
  `turn:<session>:b<offset>#<k>@hash` (part `k` of a long record) and `compact:<session>:b<offset>@hash` (an
  older sub-segment's card and digest).

Resolution: the hash matches the head (current), matches an older version (changed: the old text is still
returned verbatim, with the head's ref), or matches nothing (dangling). `recall {key: "k@hash"}` returns that
exact version, never a paraphrase, with a `(changed: the current text is k@new)` note when it is not the
head. A recalled record's ref is its key, `@`, and the first 12 digits of the `sha` line it already prints,
so no answer grows.

A `name@hex` whose name is no stored key and has no stored key's prefix (`rules:`, `journal:`) is someone
else's notation, a package version or a commit, and is not counted as a ref at all.

Twelve hex digits are 48 bits: a collision inside one record's history is out of reach, and a ref costs
about five tokens.

### (c) The session transcript as a source

Opt-in (`memory.transcripts`, off by default), scoped to the project's own transcript folder. Each user
prompt, assistant text and tool result becomes a turn with a stable id (`turn:<session8>:<n>`, n counting
records in file order), indexed for full-text search so `grep`/`search` find exact past errors, numbers and
commands after compaction. Secrets: a line that names a denied file (`sources::is_denied`), holds a
key-shaped token (`sk-`, `ghp_`, `AKIA`, long base64 or hex runs next to `key`, `token`, `secret`,
`password`), or matches a term in the private leak denylist (`ATLAS_LEAK_DENYLIST`, the release leak check's
list) is replaced by `[redacted]` before it is indexed, and transcript turns are never served to another
project's recall.

### (d) SessionStart(compact) validates the summary's refs

At SessionStart with `source: compact`, the hook reads the newest `isCompactSummary` record from the
transcript, extracts every ref, resolves each, and:

- injects one line per dangling or changed ref (`changed: k@old -> k@new`; `dangling: k@hash`), nothing
  when all resolve;
- logs a `compact_log` row: session, trigger, `preTokens`, `postTokens`, summary bytes, refs, changed,
  dangling, bare keys (record keys named without a hash) and bare keys that do not exist.

## 3. Metrics

`--token-report` gains a compaction line from `compact_log` and the token log:

- **ratio** = `preTokens` / summary tokens (summary bytes over the ledger's bytes per token). Also shown:
  `preTokens / postTokens`, Claude Code's own measure of the context right after compaction, which
  includes the system prompt and tool definitions and so has a hard ceiling.
- **dangling-ref rate** = dangling refs / refs (aim 0), with changed refs beside it.
- **post-compaction misses** = lookups after a compaction for something the session had looked up before
  it (same normalized target: a Read of the same file, a Grep of the same pattern and path, an atlas call
  with the same key, query or path), counted by the PreToolUse hook against the lookups since the session's
  previous compaction. Logged as `compact:miss` token-log rows.

## 4. Measuring without spending Claude usage

`atlas --compact-report --transcripts DIR [--since TIME]` replays recorded sessions: for every
`compact_boundary` it reads `preTokens`, `postTokens`, the summary's size and refs (resolved against the
configured store), and counts post-compaction misses over the next 40 tool calls against the lookups since
the previous boundary. It prints numbers only; no transcript text leaves the process, and nothing is copied
into the repository.

The memory fidelity eval (`cargo xtask memory-eval`) gains a compaction pass: each question's answer is
replaced by what survives a simulated compaction (the checkpoint card plus refs only), the refs are resolved
through `recall {key: "k@hash"}`, and the facts must still be found.

## 5. Head-to-head: atlas-assisted against plain compaction

The bar: atlas-assisted compaction must be at least as good as plain Claude Code compaction (atlas off) on
every axis below, on the same sessions at the same compaction points. Any axis that loses blocks the release.

| Axis | Plain arm (atlas off) | Atlas arm | Offline? | Atlas wins when |
|---|---|---|---|---|
| Ratio, pre / summary tokens | recorded `preTokens` over the recorded summary | same over the steered summary, with the card's tokens added to the denominator | plain yes; atlas needs live summaries | atlas >= plain |
| Ratio, pre / post (`postTokens`) | recorded | live | as above | atlas >= plain |
| Fidelity: facts from before the boundary that the session used after it | found in the summary alone | found in summary + card + one `recall` or `search` call by fixed policy | plain yes; atlas card, refs and transcript recall yes, summary live | atlas >= plain |
| Post-compaction misses | replayed re-lookups in the 40 tool calls after the boundary | live count (`compact:miss`); offline bound: re-lookups not reachable from summary + card + refs | plain yes; atlas bound yes | atlas <= plain |
| Dangling or changed refs | bare record keys in the summary that do not resolve | key@hash refs that dangle or changed | recorded yes; steered live | atlas <= plain |
| Resume tokens: tool-result tokens from the boundary to the first edit, write or commit | replayed | live | plain yes | atlas <= plain |
| Hook latency | no hooks: 0 ms | PreCompact + SessionStart(compact) wall time | yes | see below |

Latency cannot equal no hooks at all, so its bar is stated on its own: the hooks' added wall time is at most
1% of Claude Code's own compaction time (`durationMs`, 35 to 150 seconds in the recorded sessions) and under
250 ms at p95 on the recorded stores.

### Fidelity questions, deterministic

Questions are extracted mechanically, never written by a model. For each compaction point, a fact is a
token run that appears in the compacted segment (a tool result or assistant text before the boundary) and
is used again in the 40 tool calls after it: a number, an error line, a command, a path, a record key.
A fact comes from what a call names (its string and number values, read raw), never from the file text it
writes or must match: an edit's `old_string` and `new_string`, a write's `content`, a notebook cell's source.
That text is code the next step types or copies from a fresh read; scoring it would reward a summary for
restating code, which the Compact instructions forbid for the ratio's sake. (Ruled out on 2026-11-04: on the
81a956e5 150k cut, 179 of the 183 facts under the old rule were JavaScript and CSS identifiers inside edits
and writes. Over the 72 recorded compactions in H--dev the rule drops 192 of 2,483 facts and moves plain
fidelity from 49.9% to 52.3%; the numbers in sections 6 and 8 were measured under the old rule.)
Scoring is the eval's substring test over what each arm makes available. The questions are derived per run
from the transcripts and never written inside the repository.
The rule reads a call's own fields only, so a file written through a shell (a Bash heredoc, `python -c`)
still has its body scored as the command's string. Both arms are scored on the same list, so this is
symmetric, but it is the one place restated code still counts.

### Live sample (not run without the maintainer's OK)

The atlas arm's summaries need real compactions. The plan is 12 compaction points from 4 recorded sessions,
mixing auto and manual and spanning 140,000 to 800,000 `preTokens`. Each is truncated at its boundary into a
scratch project, resumed headless, and compacted twice: once with atlas off and once
with atlas on (checkpoint, compact instructions, card). The plain arm is re-run rather than reused, so model
drift since the recording favours neither arm. 24 compactions at about 350,000 input and 4,000 output tokens
each come to roughly 8.5M input and 0.1M output tokens, mostly cache-cold. Resume tokens add one scripted
turn per run (the session's real next prompt, capped at 15 tool calls), roughly another 5M input tokens.
The comparison is paired per point. An axis passes when atlas is at least as good on at least 11 of 12
points and on the mean.

### Threshold sweep: where to compact

The compaction point is measured, not assumed. Each threshold (150,000, 250,000 and 400,000 context tokens)
is a pair, judged on its own: plain compaction at T against atlas-assisted compaction at T, on the same sessions,
scored on tokens and on accuracy (ruling 2026-11-03: "it should be judged on tokens and accuracy per compaction
point w/ or w/o atlas in the loop"). The compaction point `atlas settings` offers is chosen only among the
thresholds where atlas wins its pair, on tokens and on every live axis; with no such threshold, none is offered.

Total tokens per task, from a session's recorded per-request usage (each request re-reads the context):

- every request's context tokens, split into cache reads and cache writes;
- each compaction's own cost: the summarizer reads the whole context cold and writes the summary;
- the cold cache after compaction: the first requests after it write their prefix again;
- the re-reads after resume: the tool-result tokens of post-compaction misses.

Offline, a session's recorded context curve (the per-request growth between requests) is replayed with a
simulated compaction whenever the context crosses the threshold: the context drops to the fixed overhead
plus a summary of the size that arm's ratio predicts, then grows by the recorded deltas again. This is
deterministic and costs nothing. It gives the token side of the sweep for any threshold, with both raw
tokens and a cache-weighted figure (reads at 0.1, writes at 1.25 of input price). Accuracy at each
threshold needs live summaries, so it joins the live sample as a plain and an atlas compaction at each cut
(see the live runner in section 8 for the sizes and the estimate).

## 6. What is honestly achievable

The plain arm, replayed with `--compact-report` on recorded sessions (82 distinct compactions, 74 manual,
2026-09 to 2026-11, `preTokens` 140,000 to 810,000, 4 bytes per token):

| Metric | Plain compaction today |
|---|---|
| Ratio pre/summary | 103.9x pooled, 91.8x median (summaries 2,400 to 6,200 tokens) |
| Ratio pre/post (`postTokens`) | 22.2x median |
| Dangling refs | none written (summaries carry no key@hash refs); 3 bare record keys, all resolve |
| Post-compaction misses | 32 of 359 lookups in the 40 calls after a boundary (8.9%) |
| Fidelity | 41.1% of the facts used after a boundary are in its summary |
| Resume tokens | median 13,023 tool-result tokens before the first edit, write or commit |
| Ref check latency | median 2.0 ms, p95 2.9 ms; Claude Code's own compaction median 49 seconds |

`postTokens` includes 7,000 to 10,000 tokens of system prompt, tools and memory files that no hook can
remove, which is why pre/post sits far below pre/summary.

- **pre/summary of 200x+** means a 250,000-token context compacting to about 1,250 tokens of summary. That
  is reachable only if the summarizer writes keys and deltas, which takes the `CLAUDE.md` compact
  instructions of section 1 plus records worth pointing at. Atlas measures it; it cannot force it.
- **pre/post** is capped near `preTokens / fixed overhead`: about 25x to 35x at 250,000 tokens. It is
  reported, not targeted.
- **dangling-ref rate 0** is achievable: versions are append-only, so a key@hash ref can only dangle if it
  was mistyped or the record never existed, and changed refs still resolve to their exact old text.
- **post-compaction misses** fall as the card and transcript recall cover what the summary drops; the
  floor is the lookups the session would make anyway to see a file's current state after edits.

## 7. Delivery

1. Refs, validation, metrics: `recall k@hash`, the ref report at SessionStart(compact), `compact_log`,
   `compact:miss`, `--compact-report`, the `--token-report` line, this note.
2. Checkpoint records: the structured PreCompact record and the SessionStart(compact) card; the compact
   instructions settings offer (removed since; `atlas settings compact-md` remains); the eval's compaction pass.
3. Transcript source: opt-in indexing with redaction, `turn:` refs.

## 8. Part 2 as built, measured offline

The checkpoint body is the format of section 2(a), with the in-task lines after the store's: `files` (touched
since the last compaction, newest first, at most 12), `pending` (a call with no result yet) and `intent` (the
last line of visible assistant text). Targets and intent are one line each, cut to 100 and 200 characters, and
pass `src/redact.rs` first. The body's budget is `compact.card_tokens` (default 400); items are dropped from a
line's end, then whole lines from the end, so the store's refs survive a tight budget first.

The tool calls are not listed one by one. The PreCompact hook appends the chain card (`src/chain.rs`), outside
that budget: every part of the current sub-segment by a group key, then the links to the older sub-segments. A
card prints the short form, a 4-character session prefix and 6 hash digits; the long
form (8 and 12) resolves too. `recall` resolves a chain key from this project's transcript itself, with
`memory.transcripts` or `compact.enabled` on, and answers an error otherwise. Nothing is stored or indexed for
it, so a key lives as long as its transcript file. A key's hash covers the record's unredacted visible text (a
link's: its line's bytes), so a denylist change does not dangle it. Part numbers are cut from redacted text, so
under a different setting a part key can name another slice or none. Keep the hook and the resolver on one
`ATLAS_LEAK_DENYLIST` setting.

The ref check after a compaction counts a chain key with 8 or more hash digits as unchecked, never dangling; it
does not see the short form a card prints, which is not a plain ref (at least 8 digits), nor a part key. `atlas
--compact-report` prints the checkpoint body only: it has no chain card, so its card size and coverage lines do
not describe the live card.

The ref is by byte offset, not line, so PreCompact reads only the segment since the newest boundary:
finding a line number meant counting every newline of the history before it, 60 ms of a 200 MB transcript
(profiled 2026-11-05). The boundary is found by a raw-byte search back from the end
that decodes only lines holding the needle, and the segment is then streamed once through one buffer. On
the saved compact-live jobs PreCompact went from 190 ms to 90 ms at 400k and from 330 ms to 130 ms on the
largest segment (57 MB). A `turn:<session8>:<line>` ref still parses and resolves.

`compact.defer` reads the window as `compact.window`, else 1,000,000 once the session has held more than
200,000 tokens, else 200,000; with a 200,000 window it never defers near Claude Code's own threshold.

`--compact-report` on the same 82 recorded compactions (store: the eval snapshot; cards built from each
boundary's segment and the store as of the boundary's time):

| Measure | Plain | Atlas arm, offline |
|---|---|---|
| Ratio pre/summary, pooled | 103.9x | 97.6x with the card added to an unsteered summary (card median 226 tokens, p95 387) |
| Misses named in what survives | 24 of 32 (summary) | 25 of 32 (summary or card) |
| Facts in what survives | 1246 of 3031, 41.1% | 1297 of 3031, 42.8% |
| Hook latency | none | median 42 ms, p95 68 ms (transcript read, checkpoint, ref check); at most 0.51% of Claude Code's compaction time, median 0.06% |

The ratio row is the open risk: the card costs about 6% of the plain ratio unless the steered summary is that
much shorter. The Compact instructions tell the summarizer not to repeat what the card lists (files and code
sections are the bulk of a plain summary), so the live sample decides it; atlas cannot force it offline.

Threshold sweep, token side, paired per threshold (`--sweep 150000,250000,400000`, 116 sessions, 18,145
requests; summary 3,660 tokens, card 228, overhead 11,472, re-read 642 tokens per compaction in both arms;
weighted: cache reads 0.1, writes 1.25, the summarizer's read 1.0 and its output 5.0):

| Threshold | Compactions (both arms) | Atlas against plain, input tokens | Atlas against plain, weighted |
|---|---|---|---|
| 150,000 | 247 | +0.27% | +0.32% |
| 250,000 | 137 | -0.06% | +0.02% |
| 400,000 | 81 | +0.02% | +0.05% |

Offline the atlas arm differs from plain only by the card it adds to an unsteered summary, so it cannot win a
pair on tokens here; the steered summary (shorter by at least the card) and the re-reads after resume are live
measures. For reference only, not a comparison: plain at Claude Code's default point (257,876, the median
`preTokens` of recorded automatic compactions) is 132 compactions and 329.7M weighted tokens. `atlas settings
offer` offers no compaction point until the live sweep shows a threshold where atlas wins its pair.

Card budget: at 150, 200, 300 and 400 tokens the misses named stay 25 of 32, and the facts in summary or card
are 1,293, 1,300, 1,309 and 1,311 of 3,058; a smaller card loses facts, so the budget stays 400. The Compact
instructions set a hard limit of 3,000 characters (about 1,250 tokens), which the summarizer has not kept to in
live smokes, spent on identifiers: one line per file with no code
snippets, user messages as requirements and corrections, no restated tool output, refs instead of record
text, and nothing the card lists. A plain summary's bulk is code snippets and the full list of user messages,
which is what this removes.

The eval's (g) pass: each question's answer is replaced by the ref it rests on and recalled once; the facts must
hold (numbers in `cargo xtask memory-eval`).

### The live runner

`cargo xtask compact-live` stages the live sample of section 5 as pairs: every compaction point is a plain and an
atlas compaction of the same transcript cut. The points are the recorded compactions (`rec`: `preTokens`
140,000 to 810,000, the 4 sessions with the most, 3 each, cut just before the boundary) and, for each
threshold T, as many cuts where a segment's context crosses T (or `--per-threshold N`), from the same sessions
first. Each cut is copied into a scratch project under a session id of the job's own, resumed in place and
compacted. Both arms run atlas's hooks and server from a scratch copy of the eval snapshot configuration
(`--setting-sources project`, `--strict-mcp-config`), so the baseline carries the same tool list and session lines:
the plain arm with that configuration's compaction features off, no `CLAUDE.md` and a bare `/compact`, the atlas
arm with them on and the Compact instructions both as the `/compact` argument and as the project's `CLAUDE.md`. The
`CLAUDE.md` block is part of the shipped feature (the steering a user installs); in a resumed session it reaches
only the turn after the boundary, so it is a turn-side difference between the arms, counted in the atlas arm's
tokens. An atlas binary whose hook returns the instructions adds them a further time (after the argument, as read
from the binary, not observed) and adds the echo (section 1) to each scripted turn: the runs recorded here predate
that. By Brandon's ruling the scored run uses a bare `/compact` in the atlas arm, the echo counts against atlas in
bar 3, and its results are not directly comparable with older runs; `compact_prompt` still passes the argument until
that follow-up lands. Each job runs on its own copy of the configuration, index and memory store, so no job's hooks feed another
job's session lines or recall. The memory store copy is pruned to the cut's time (the newest record time in
the kept lines; versions after it go, heads, status and search follow), so no record written after the cut is
there; an undated source item carries its import time and is dropped too, which can only cost the arm that
recalls it. The index is not pruned: its sources are the 2026-11-02 snapshot (as of 15:19:37Z), and every point
the ruling's plan picks (3 sessions, 9 cuts, 2026-10-04 to 2026-11-02T03:45Z) is older than it. The only
session with compactions after the snapshot is the orchestrator's own live one, so refusing older cuts would
leave no plan. The harness therefore refuses them unless `--index-newer-ok`, and the score says how many jobs
ran one. What leaks is symmetric and stays out of the compaction: both arms can search the later index in the
scripted turn, while the summary, the card (bounded by the transcript's newest record) and the ref check never
read it. A `/compact` that leaves no boundary past the cut is retried, never measured. A
cut that leaves a `/goal` active has it cleared first in both arms, since a restored goal's Stop check re-prompts
the turn. One scripted turn follows in plan mode with edits and shells refused; resume is the tool-result bytes from
the boundary to the first edit attempt or `ExitPlanMode`.

Fixes from the 2026-11-03 audit of the first run (voided), besides the symmetric baseline, the goal clear and
the resume measure above: a resumed session attaches `CLAUDE.md` only
after the boundary, so the steering reaches the summarizer only as the `/compact` argument; a `--fork-session`
fork's transcript is written after PreCompact runs, so its checkpoint read nothing (7 of 11), and the copy is now
resumed in place instead, with each job's card source (`own`, `parent`, `unread`) recorded and reported; and
`compact_tokens` was always 0, so each claude result's `usage` and `modelUsage` are counted (the larger), a turn
that hits `--max-turns` keeps its usage, a `/compact` result without usage is counted at the boundary's
`preTokens` and marked so, and `--spend` prints a work folder's totals (input, output, cost).

The run happens once and its result decides. `run.json` in the work folder records the plan and the run id;
launching again resumes it after an infrastructure failure only (a finished compaction or job is never
redone). A smoke run (`--only`) is marked as such, needs its own work folder and is never scored. `--score`
refuses unfinished jobs and results from another run, then judges each compaction point on its own: atlas
against plain at that point on ratio, fidelity, misses, dangling refs, resume tokens and input tokens (at least
as good on all pairs but one and on the mean), and on latency (hooks at most 1% of the compaction's own time,
p95 under 250 ms). Hook latency is each hook process's wall time, spawn to exit, logged by the harness's
`xtask hook-time` wrapper around every hook (PreCompact plus the SessionStart after the compaction); a missing
timing fails the bar. The offered point is chosen among the thresholds where atlas wins its pair.

Sizes (dry run, 2026-11-03): 12 `rec` pairs and 12 pairs at each threshold (10 at 400,000, the crossings the
recordings hold) are 92 jobs, about 47.1M input tokens; with `--per-threshold 6`, 60 jobs and 31.8M; with
`--per-threshold 4`, 48 jobs and 25.8M.

## 9. Part 3: the transcript source, and its threat note

Built as section 2(c) describes, with these choices. `memory.transcripts` (default false) turns it on. The
PreCompact hook queues the session's transcript, and it is indexed once the hook's answer is out (in the
server, after the reply; in a command hook, after the JSON is printed), from where the last pass stopped. A turn
is one record's visible text; its ref hashes that text unredacted, so the checkpoint's `turn:` result refs are
the same refs. `recall {key: "turn:<session8>:<line>@hash"}` returns the stored turn (a checkpoint's
`b<offset>` ref is mapped to its line through the indexed transcript); `recall {query, kind:
"turn"}` searches turns. After a compaction the ref check resolves `turn:` refs against the transcripts beside
the session's own, so a mistyped one counts as dangling.

**Threat note.** A transcript holds whatever passed through the session: pasted keys, `.env` contents a tool
read, tokens in commands, private names. Indexing it copies that into the index database, which lives longer
than the transcript's own folder may and is searched by every later session.

- *Assets*: secrets and private text in tool results, prompts and commands.
- *Who could see them*: a later session of the same project (intended); a session of another project sharing
  the index database (not intended); anyone who reads the database file (the same as reading the transcripts,
  which sit beside it under the user's profile).
- *Mitigations*: off by default; thinking blocks are never read; each stored line passes the redaction of
  `src/redact.rs` (denied file names, provider-shaped keys, PEM headers, JWTs, bearer credentials, `.env`-style
  secret assignments, URLs with a password, long base64 or hex runs beside a secret word, and the leak
  denylist's terms); the whole result of a tool call whose target names a secret is stored as `[redacted]`,
  whatever its lines look like; a turn is cut at 32 KiB; every row carries its project (the transcript's
  folder, Claude Code's encoding of the working folder) and lookups pass the asking server's project, so
  another project's turns are never returned, by ref or by search.
- *Residual risk*: redaction is pattern-based. A secret with no recognizable shape (a password typed as a plain
  word, a private name not on the denylist) is stored. Turning the source on is the user's statement that this
  project's transcripts may be indexed; the database is not exported, synced or sent anywhere.
