# atlas goals: a goal that survives session clears

Status: build steps 1 (atlas-dev #428) and 2 (metric lines and `goal::evaluate`) built; steps 3 to 7 open. Queue key journal:ATLAS-GOAL:0001 (rulings on storage, shape and check-ins),
decision journal:ATLAS-GOAL:0002 (check-ins off the frontier model). Builds on [goals.md](goals.md) (what a hook
can see of Claude Code's `/goal`, rulings Q-GO1 to Q-GO3, intent:r01513). Brandon answered Q-AG1 to Q-AG7 on
2026-11-25, each with the recommended option (journal:ATLAS-GOAL:0014, journal:ATLAS-GOAL:0015); section 8 lists
the answers and where each is folded in. Review journal:ATLAS-GOAL:0013 is addressed in this revision.

The rulings, in Brandon's words (2026-11-24):

- Storage: "Both, atlas owns it". A versioned `goal:<slug>` record is the source of truth; amendments are new
  versions; atlas re-arms Claude Code's `/goal` from it each session; the SessionStart card shows the current
  version.
- Shape: "Text + gates + lanes". Verbatim text, machine-readable gates (metric, threshold, gate or aim), serving
  lanes and work order, so check-ins compute met or not.
- Check-ins: all four. Measured progress, turn and time cadence (per goal), events (merge, handoff, compaction,
  agent finish), stall detection.
- Off-frontier (0002): "it would also be cool if atlas goals didn't burn tokens on the frontier model". Met or not
  is computed with no model, text comes from templates, fuzzy parts go to the local model or Haiku, never the
  session model. Q-AG7 then chose the local embedder only, so this design calls no Haiku.

Earlier rulings this design keeps, from goals.md section 4 (Brandon, 2026-11-04):

- Q-GO1: goal text goes in `additionalContext` on the check-in's UserPromptSubmit only; nothing is added on Stop.
  With Q-AG2 ("Goal + agent notices") the check-in lines of section 4 ride on Claude Code's goal check-ins and on
  agent task notifications, never on Brandon's own prompts (section 4.0).
- Q-GO2: "The orchestrator writes a goal's parts ... atlas does not split the condition itself." Kept in full:
  the gates, aims, lanes and steps of a goal are written by the orchestrator or Brandon, never derived by atlas
  from the goal's text (section 2.1, "Who writes the parts").
- Q-GO3: only the user or the evaluator ends a goal (section 4.5).

## 1. What atlas sees of `/goal` today

| Piece | Where | What it does |
|---|---|---|
| Goal parser | `src/resume.rs:732-840` (`GOAL_CMD`, `GoalEvent`, `goal_event`, `goal_status`, `Goal::apply`) | Reads `/goal TEXT`, `/goal clear` and `goal_status` attachments from a transcript. The one parser the card and resume share |
| Backward search | `src/resume.rs:644-729` (`goal`, `goal_anywhere`, `goal_within`, `GoalBack`) | The previous session's unmet goal, read from the transcript tail |
| Card goal line | `src/resume.rs:331` and `goal_line` at `:493` | "the previous session's goal, not met (restate it with /goal to keep it): ..." |
| Amended-goal line | `src/resume.rs:332-342`, `MemoryStore::goal_records` at `src/memory/since.rs:329`, `amendment` at `:203` | With a goal line only: the newest `AMENDED ...` paragraph of the newest `active-goal*` memory record (#425) |
| Compaction card | `src/checkpoint.rs:47, 734` | The checkpoint card carries the goal read forward through the segment |
| Check-in seen | `src/hooks.rs:3325-3328` (`notification`) | A `<task-notification>` with no task id and `Goal check-in:` gets the ideas line only |

So today the goal lives in the transcript. A `/clear` or a new session drops it (the goal page: "Running `/clear`
... also removes any active goal"), a resume restores it (every resume route since 2.1.239, turn count reset), and
the card can only quote the last session's text plus one amendment paragraph from a memory file whose key
happens to start with `active-goal`. There is no gate, no lane and no progress.

### Hook events and what each can inject

From the hooks reference (read 2026-11-25) and atlas's own use of it (`NOTICE_EVENTS`, `src/hooks.rs:391`).
Review 0013 S5 asked that the Claude Code claims atlas's code does not show be checked. Each was re-read on
2026-11-25 in the Claude Code docs, code.claude.com/docs/en/hooks ("SessionStart decision control", "Stop
decision control") and code.claude.com/docs/en/goal ("Resume with an active goal", "How evaluation works"), and
holds as written here:

| Claim | Docs, 2026-11-25 |
|---|---|
| SessionStart fields are `additionalContext`, `initialUserMessage` (`-p` only), `sessionTitle`, `watchPaths`, `reloadSkills` | Verified: the SessionStart table lists exactly these five; only `initialUserMessage` is `-p` only |
| Stop `hookSpecificOutput.additionalContext` continues the turn | Verified: "The conversation continues so Claude can act on it", labelled `Stop hook feedback`, same loop protections as `decision: "block"` |
| The no-progress cap | Verified, two caps: the goal page stops the loop after "no tool use for several turns in a row"; the hooks page overrides a Stop hook after "eight times in a row" (reset by a tool call, `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP`) |
| Resume restores `/goal` on every route since 2.1.239 | Verified: "Before v2.1.239, Claude Code restored the goal on every route except the `claude --resume` picker" |
| The evaluator's feedback form `Stop hook feedback:\n[<condition>]: <reason>` | Not in the docs; from transcripts on this machine (goals.md section 2, Claude Code 2.1.316), an internal format that may change |

| Event | atlas handler | Reaches the session model | Can start or keep a turn |
|---|---|---|---|
| SessionStart (`startup`, `clear`, `resume`, `compact`) | `src/hooks.rs:2255-2422` | `additionalContext`, at the start | No. `initialUserMessage` starts a first turn in `-p` mode only |
| UserPromptSubmit | `src/hooks.rs:1916-1965` | `additionalContext` with the prompt. Fires on user prompts, task notifications and Claude Code's own goal check-ins | No (it can block a prompt) |
| Stop (main) | `src/hooks.rs:2191` (ideas `on_stop`) | `decision: block` + `reason` keeps Claude working; `hookSpecificOutput.additionalContext` likewise continues; both stop after 8 continuations in a row with no tool call | Yes, it is what `/goal` is built on |
| SubagentStop | `src/hooks.rs:2201` | To the subagent only | The subagent |
| PreCompact | `src/hooks.rs:2430` | Plain stdout becomes summary instructions | No |
| PostCompact, SessionEnd, Notification | SessionEnd at `src/hooks.rs:2427` (clear checkpoint) | Nothing | No |

**No hook output runs a slash command or sets a goal.** SessionStart accepts `additionalContext`,
`initialUserMessage` (`-p` only), `sessionTitle`, `watchPaths` and `reloadSkills`, nothing else. So atlas cannot
re-arm `/goal` by itself. Section 2.3 gives the nearest equivalents.

## 2. Storage

### 2.1 The record

A goal is a record of a new kind `goal` (`goal::KIND`), key `goal:<slug>`, scope
the lane that carries its journal (for engine parity, `GOAL`). Its body is the goal: verbatim text, then the
amendments verbatim, then the spec block (section 3). An amendment is a new version through `Writer::revise`
(`src/memory/mod.rs:1833`, the path `remember` with `expected_sha` already takes, `src/tools_memory.rs:920`), so
`recall key=goal:<slug> op=log` is the goal's history, and the head is always the current goal. The body keeps
every amendment, not only the newest: a goal restated without its amendment is the old goal (ATLAS-RESUME:0001
item 2), and one restated without the original loses its intent.

The record's meta holds the goal's state, which changes without a new version of the text:

| Field | Values | Written by |
|---|---|---|
| `state` | `active`, `paused`, `done`, `dropped` | `note op=goal` (section 6); `done` only after the user says so (Q-GO3) |
| `armed_session` | the session id whose `/goal` was last seen to carry this goal | the UserPromptSubmit hook, from the transcript's newest `goal_status` |
| `snooze_until`, `mute` | a time; a list of check-in kinds | section 6 controls |
| `met_token`, `met_commit` | a 12-hex random nonce and the commit it was issued at; empty until issued | `note op=goal done`, only when every gate is met (section 4.5) |

Each check-in atlas delivers is journaled as one `checkpoint` entry in the goal's lane, holding the gate values it
computed (`meta: goal:engine-parity v2 G1=4147 G2=5578 A1=10493 A2=19675 @763e4d3a`). That entry is the
"since last check-in" baseline, survives any clear, and is what the card's lane line already shows. Only a
delivered line is journaled as a baseline `checkpoint`: a held line (section 4.0) is journaled as a `quiet` entry
that is neither the baseline nor a cadence reset, so a delta is always against values the session actually saw.
A kind configured `quiet` (section 6) never delivers, so for that kind alone its own `quiet` entries are the
baseline and the cadence reset; section 4.0 gives the one-per-window rule that keeps either case from writing a
`quiet` entry on every prompt.

**Who writes the parts (Q-GO2).** Q-GO2 ruled that the orchestrator writes a goal's parts in a `goal` journal
entry and that "atlas does not split the condition itself". This design keeps the rule and names where the parts
now live:

- The spec block (gates, aims, lanes, steps, section 3) is text the orchestrator or Brandon writes, through
  `note op=goal set` or `amend` (section 6). atlas parses it (a grammar check: a malformed line or a gate with no
  metric is refused) and stores it verbatim. atlas never derives a gate, an aim, a lane or a step from the goal's
  prose, and no model is asked to.
- Each `set` and `amend` also appends the `goal` journal entry Q-GO2 names, on the goal's lane: the spec block as
  written, who wrote it, and the record version it made. That entry is the Q-GO2 record of the parts; the
  `goal:<slug>` record is the versioned copy atlas reads, because ruling 0001 made the record the source of truth
  (2026-11-24, after Q-GO2's 2026-11-04). The two never differ: one call writes both, and the entry carries the
  version's sha.
- The seed (build step 1) follows the same rule: `atlas goal seed` copies the text and amendment verbatim from
  the memory record and takes the spec from a file the orchestrator writes (`--spec <file>`); with no `--spec` it
  writes text and amendment only and the goal has no gates ("not measured", never MET). Section 3.1's spec block
  is the design's proposal for that file, built from Brandon's Q-AG5 and Q-AG6 answers, for the orchestrator to
  confirm or change.

So Q-GO2 is not replaced: who writes the parts is unchanged, and the journal entry it names is still written.
The record adds a versioned copy, as ruling 0001 asks. Journal entry kinds today
have no `goal` kind (the `journal` tool's `entry` list); build step 1 adds it.

### 2.2 Which goal is active

At most one goal is `active` per project store at a time (Q-AG4, answered "One active per project"; Claude Code
allows one `/goal` per session too). A second `set` while one is active is refused with the choice to pause the
first; paused goals keep their record and get no check-ins. `MemoryStore::goal_records`
(`src/memory/since.rs:329`) changes from "memory keys starting `active-goal`" to "kind `goal`, state `active`",
and keeps the old query as a fallback until the seed migration (build step 1) has run.

### 2.3 Re-arming `/goal`

Claude Code restores `/goal` on a resume by itself. After a `/clear` or in a new session nothing does, and no hook
can type the command. Three ways to get the goal back:

1. **A pointer condition the user arms in one line (chosen, Q-AG1 "Pointer goal").** The SessionStart card (new
   session or clear) ends its goal line with the exact command:
   `/goal an atlas tool result shows "goal:engine-parity met-token" and a 12-hex nonce`. The condition is 76
   bytes, so the evaluator's feedback after every turn is about 50 tokens instead of about 257 for the verbatim
   text (section 5.2). The idle check-ins, the `◎ /goal active` indicator and the evaluator's waits on background
   work stay Claude Code's.
2. **The verbatim text.** The card offers `/goal <text and amendments>` (904 bytes for engine parity, under the
   4,000-character cap). The directive turn that setting a goal starts is the full goal, but the evaluator judges
   the prose itself, and every evaluated turn carries it back into the session.
3. **No `/goal`: atlas's own Stop hook.** atlas blocks Stop with a template reason while the goal is active and
   unmet and no background work runs (`background_tasks`, already read at `src/hooks.rs:2195`). No evaluator call
   and no typing, but no idle check-ins (only Claude Code can start a turn while the session is idle), no goal
   indicator, and Claude Code's no-progress cap still applies to a Stop hook that keeps blocking.

The card also shows when the armed `/goal` and the record differ: a `/goal TEXT` typed by hand whose text is not
the record's current version (or pointer) gets "the `/goal` in this session is not goal:<slug> v2: re-arm with
...". The hook notes `armed_session` from the transcript's `goal_status`, so it can tell an armed session from an
unarmed one without asking.

`/goal clear` by the user is read (`GoalEvent::Clear`) and leaves the record active: clearing Claude Code's loop is
not dropping the goal. Ending the goal is `note op=goal done` (section 6).

**The met token cannot be produced by text (review 0013 S1).** The evaluator is a model that reads the
conversation, so a pointer condition that quotes a phrase atlas might print ("atlas reports goal:<slug> MET") can
be judged met by a phrase that merely appears: in the card that offers the command, in the `/goal` turn itself,
or near the 4.5 line. The condition therefore asks for something only one atlas call can put in the
conversation:

- The token is `goal:<slug> met-token <nonce>`, the nonce 12 random hex characters drawn when it is issued.
- Only `note op=goal done` issues it, and only when the evaluator (`goal::evaluate`) finds every gate met on a
  quiet full set at that moment; otherwise the call is refused with the unmet gates. The session calls it only
  after the user agrees (Q-GO3, section 4.5). The nonce and its commit go in the record's meta (`met_token`,
  `met_commit`) and the goal's state becomes `done`.
- A well-formed token (`met-token` followed by 12 hex characters) appears in exactly two places: the one tool
  result that issues it, and the record's own meta. The bare word `met-token` with no nonce appears in two more:
  the condition, and the card line that prints the arm command (which contains the condition). Neither has a nonce.
  No template, card line, check-in, compaction line or `show` output prints a nonce (a gate's own "MET" verdict in
  4.1 no longer matters, since the condition does not ask for that word). The test renders every template and
  both cards for a goal with every gate met and asserts none of them matches the regex `met-token [0-9a-f]{12}`.
- Residual risk, stated: atlas cannot check the nonce, so the evaluator passes any well-formed token it sees in a
  tool result. Places one can appear: (a) the issuing tool result (intended); (b) the `done` journal entry
  (the journal entry a `done` writes), which would hold the token if it copied the record's meta; (c)
  the hook budget log, if it logged injected text rather than byte counts; (d) test fixtures and design text read
  through a tool, such as this document if an example token is written in it; (e) the session model's own prose
  (not a tool result, so the condition excludes it, but the evaluator may be fooled). Mitigation, kept small:
  `done` journals the commit and a hash of the token, never the token; the hook budget log records byte counts
  only; fixtures and docs use no 12-hex example after `met-token`. Test:
  `no_store_row_other_than_the_goal_record_contains_a_token_form`. What remains is (a) read back from the record.
  The nonce is redacted once, at the store read (journal:ATLAS-GOAL:0029 item 1, built in step 1): every row
  the store maps into a head (`head_row`), a version (`MemoryStore::log`, so `history` and `key@hash`) or an
  as-of answer (`archive::as_of`) passes its meta through `goal::redact_meta`, which turns an issued
  `met_token` value of a `goal` record into `issued`. `goal::met_token` is the one accessor that reads the raw
  value (for `done`, step 3). So every path in `src/` that can print a record's meta prints `issued`, among
  them:
  - `recall key=goal:<slug>` with `format=json` (`recall_record_json`, `src/tools_memory.rs:194-208`, which puts
    `meta` in its answer);
  - `recall key=goal:<slug>@<hash>` for a version that is no longer the head (`src/tools_memory.rs:389`, which
    frames meta above the body);
  - the CLI `atlas memory as-of <key> <time>` (`src/main.rs:1289-1300`, which prints meta from the store or a
    snapshot in the archive);
  - the new `note op=goal show` and both cards, which read the record's state, never `met_token`.

  Read paths that print no meta today, and stay so: plain `recall key=goal:<slug>` and the head through
  `key@hash` (`recall::render`, `src/memory/recall.rs:71-74`, frames the title and body only), `recall
  history=true` (bodies only, `src/tools_memory.rs:396-400`), `recall op=log` (time, key, kind, version, author,
  title, reason: `memory_log`, `src/tools_memory.rs:640-657`), `recall query` and the atlas `search` and `grep`
  tools (the store's full-text index covers title and body only, `records_fts`, `src/memory/mod.rs:85`, and
  `search`/`grep` read vault and source files, not the store), `recall lane` (journal and handoff records only),
  and the exports (`src/memory/export.rs:132-150`: an items or headings export writes the records placed in it,
  meta and body, and a `goal` record is never placed, since `set` and the seed create it with no place; the memory
  folder export writes kind `memory` only, `:145`; and every export reads heads through the redacting store read
  below, so none could write the nonce). The kind-scoped `show` commands of `atlas queue` and `atlas ideas` print meta but
  reach only their own kinds (`src/queue.rs:1570-1576`, `src/ideas/cli.rs:37-42`). The store file, its `<store>.jsonl`
  copy (the Writer's `journal_line` writes the raw meta) and its archive snapshots hold the nonce as written; they
  are not tool output. Test (build step 3):
  `every_read_path_of_a_done_goal_shows_issued_not_the_nonce` issues a token on a fixture goal, then runs each
  path named above, the redacting ones and the ones that print no meta, and asserts that none of their output
  matches `met-token [0-9a-f]{12}` or contains the nonce, and that the redacting ones show `met_token: issued`.

**How the session reads the goal under a pointer.** Setting a goal starts a turn with the condition as its
directive, so under the pointer that turn carries no goal text. The card's goal line (section 4.0) says
`recall goal:<slug> for the text and gates`, and the session's first act in that turn is `recall key=goal:<slug>`:
the head version, which holds the original text, every amendment and the spec. Every later check-in names the
goal by key and version, so the session re-recalls only when the version changes.

## 3. Shape

A goal body has four parts, in this order:

1. **Text**: the goal as Brandon set it, verbatim, never paraphrased.
2. **Amendments**: each `AMENDED <date> (<who>): <text>` paragraph, verbatim, oldest first (the form
   `amendment`, `src/memory/since.rs:203`, already reads).
3. **Spec**: one fenced block with info string `goal`, line-based, so a builder parses it with no model:
   - `gate <id> <metric> <op> <bar> <unit> [over <arms>] [take worst|median] [quiet] [feeds <lane>]`: must hold.
   - `aim <id> ...` with the same fields: reported every check-in, never decides met (the 4K rule,
     memory 4k-target-is-an-aim-not-a-gate).
   - `report <id> ...`: aims printed together, in this order, on every check-in line that carries gate values
     (4.1, 4.2, 4.5), whether or not they moved. For engine parity this is the amendment's "report the 4K Ultra
     time against it alongside the default-settings 4K time against 4.25 ms".
   - `lane <LANE> [step <n>]`: a serving lane, with the work-order step it serves.
   - `step <n> <label> [done-when <metric> <op> <n>]`: the work order.
   - `cadence every <minutes>m | <turns>t`, `stall <hours>h`: per goal, overriding config (section 6).
4. **Notes**: anything else Brandon said about it (the status at setting, related keys). Read by people only.

A gate is fed by **metric lines** in `measurement` journal entries, one per value:

```
metric: <metric> <value> <unit> [arm <arm>] @<commit> <quiet|contended>
```

`<metric>` is a dotted name with an optional `@<case>`, such as `frame.gpu.p99@1440p`. A gate over arms takes the
worst arm's value by default (a set where one arm fails is a failing set), and `quiet` gates ignore contended
lines (the standing rule that contended numbers are provisional). The newest commit with a full set of the gate's
arms decides; a newer partial set is reported, not used. Measurements are prose today (for example
journal:LOOK-FOLLOWUPS:0020, journal:UNIT-LIGHTS:0002), so nothing is computed until metric lines exist. Per
Q-AG3 ("Frame tool + briefs"): the frame-set tool emits `metric:` lines in the measurement entry it writes, and
briefs require them for every other measurement that feeds a gate or aim (the H2H run, the parity-rows count).
atlas does not parse prose tables. A goal whose gate has never been fed says "G2 not measured" and never MET.

A gate or aim with no `over` clause takes the newest line of its metric (quiet only, if marked `quiet`); `take
worst` then has one value to take. Arms are added by amendment once the lane that measures the metric names its
cases.

### 3.1 Worked example: the seed record for engine parity

From memory active-goal-engine-parity (v4) and its 11-04 amendment, plus memory
4k-target-is-an-aim-not-a-gate. Values from quiet sitting B, 2026-11-12, head 763e4d3a (Z:/scratch/goal/GOAL_STATUS.md
section 1). The original text and the 11-04 amendment are quoted verbatim and in full from memory
active-goal-engine-parity v4 (sha ab623bbe). The amendment's 4K lines are reported aims, not gates: A1 (default
settings, 4.25 ms) and A2 (Ultra, 6.94 ms), printed together on every check-in by the `report` line, because the
amendment says "report" and "never a gate". The spec block is this design's proposal for the orchestrator's
`--spec` file (section 2.1); the orchestrator writes the real one.

````text
== goal:engine-parity goal lane=GOAL v2

Reach parity with the-game-on-Bevy in the new engine with at least 50% better performance. Don't cut corners:
higher fidelity and better performance. The hard number is under 4.25 ms per frame at 4K with ray tracing on,
with traced bodies as the default.

AMENDED 2026-11-04 (Brandon, confirming his 10-22 easing): the hard line is 240 fps (under 4.17 ms) at 1080p and
1440p, RT on, traced bodies default. 4K is reported every time; 4.25 ms at 4K stays the pressed AIM (Brandon 11-04:
"an aim but at the highest settings probably not feasible"), never a gate. See [[4k-target-is-an-aim-not-a-gate]].
Brandon 11-04: "If we can get my machine to run 4k over 144fps that would be great" - so at 4K on his RTX 3090 at
the HIGHEST settings (Ultra), the target is over 144 fps (< 6.94 ms); report the 4K Ultra time against it
alongside the default-settings 4K time against 4.25 ms. When restating the goal, give the original text and this
amendment.

```goal
gate G1 frame.gpu.p99@1080p < 4170 us over r1080,s1080,b1080 take worst quiet feeds FRAME-SET
gate G2 frame.gpu.p99@1440p < 4170 us over r1440,s1440,b1440 take worst quiet feeds FRAME-SET
gate G3 parity.rows.open = 0 rows feeds PARITY-ROWS
gate G4 h2h.ratio.p99 <= 0.667 x take worst quiet feeds H2H
aim  A1 frame.gpu.p99@4k < 4250 us over r4k,s4k,b4k take worst quiet feeds FRAME-SET
aim  A2 frame.gpu.p99@4k-ultra < 6940 us over h4k quiet feeds FRAME-SET
report A1 A2
step 1 PARITY rows done-when parity.rows.open = 0
step 2 Bevy head-to-head done-when h2h.ratio.p99 <= 0.667
step 3 networked 1v1
step 4 campaign vertical slice
lane PARITY-ROWS step 1
lane PARITY-BAR step 1
lane FRAME-SET step 1
lane H2H step 2
cadence every 120m
stall 24h
```

Notes: version 1 is the original text alone (set before 2026-10-22, re-set 2026-11-14); version 2 adds the
11-04 amendment. Related: engine-parity-goal, game-frame-rate-target, intent:r00794, journal:PARITY-AUDIT:0002.
````

Evaluated on sitting B the deterministic part gives:

| Id | Kind | Worst arm | Value / bar (us) | Verdict |
|---|---|---|---|---|
| G1 | gate | b1080 | 4147 / 4170 | MET (by 23; its wall-clock p99 4230 is over, but the gate is the GPU clock) |
| G2 | gate | b1440 | 5578 / 4170 | NOT, +1408 |
| G3 | gate | - | 19 open rows (ledger 11-07) | NOT |
| G4 | gate | - | no Bevy number (intent:r00833) | not measured |
| A1 | aim | b4k | 10493 / 4250 | +6243 |
| A2 | aim | h4k | 19675 / 6940 | +12735 |

So: 1 of 4 gates met, 0 of 2 aims, step 1 of 4. Brandon's answers fix the spec's choices: Q-AG5 ("p99, 1080p and
1440p"): G1 and G2 are both gates on the GPU p99, each taking its worst arm. Q-AG6 ("Gate at 2/3 of Bevy p99"):
G4 is a gate at `<= 0.667` of Bevy's p99, fed by the H2H run, and "not measured" (so never met) until that run
writes its metric lines. G4's arms are not guessed: it has no `over` clause until the H2H lane names its cases
(review 0013 N1), and the orchestrator adds them by amendment.

## 4. Check-ins

Every check-in is one or a few lines from a template, appended to an `additionalContext` atlas already writes.
Per Q-GO1 nothing is added on Stop. Each names the goal by key and version, so the session can `recall` the text
instead of carrying it. All four kinds share one evaluator (`goal::evaluate`, build step 2): gate values, met or
not, the delta against the last `checkpoint` entry, the work-order step, and per serving lane the time since its
last entry. The evaluator reads only the store: no transcript, no model.

### 4.0 Where check-in lines may go (Q-GO1, Q-AG2)

Q-GO1 put goal text on the check-in's UserPromptSubmit only, and Q-AG2 was answered "Goal + agent notices". So
the measured (4.1), cadence (4.2), event (4.3) and stall (4.4) lines, and the met line (4.5), are delivered only
on two kinds of UserPromptSubmit in the main session:

| Prompt | How the hook tells | Goal lines |
|---|---|---|
| Claude Code's goal check-in | `<task-notification>` with no task id and `Goal check-in:` (`notification`, `src/hooks.rs:3325-3328`) | Yes |
| An agent's task notification | `<task-notification>` with a task id (`notification`, `src/hooks.rs:3308`) | Yes |
| Anything else, Brandon's own prompts included | neither form | Never |

A line that comes due while only Brandon's prompts arrive is held, not dropped: it is journaled as a `quiet`
entry and the next goal check-in or agent notification carries the newest line of each kind, so held lines
collapse instead of queueing. A held line never becomes the "since last" baseline and never restarts the cadence
timer: both are read only from the last delivered `checkpoint`, and the delta in the line that finally goes out is
against that delivered baseline. Nothing goes on Stop (Q-GO1).

Each kind keeps two clocks, so neither a long held run nor a kind configured `quiet` stalls or floods:

| Clock | Read from (kind `on`) | Read from (kind configured `quiet`) | Moves when |
|---|---|---|---|
| Delivery baseline: the "since last" delta and the cadence due time | the kind's last delivered `checkpoint` | the kind's last `quiet` entry | a line is delivered (`on`); a `quiet` entry is written (`quiet`) |
| Journal window: whether a `quiet` entry may be written | the later of the kind's last delivered `checkpoint` and its last `quiet` entry | the kind's last `quiet` entry | a `checkpoint` or a `quiet` entry is written |

A `quiet` entry is written only when a line of that kind is due and no `quiet` entry of that kind has been written
in the current journal window: at most one per kind per window. The window is the kind's own period: `cadence`
minutes for cadence, `stall` hours for stall, and one triggering event for measured (a new measurement entry) and
event lines (one merge, handoff or agent finish), so those two are bounded by their triggers, not by prompts. So a
held cadence line over five hours of Brandon's prompts at `cadence every 120m` writes three `quiet` entries (at
the first prompt after the due time and after each further 120 minutes), its delivery baseline stays the last
delivered `checkpoint`, and the line that finally goes out carries the delta from it. A kind configured `quiet`
never delivers, so its delivery baseline moves with its journal window: each `quiet` entry's delta is against the
kind's previous `quiet` entry, and its cadence is due once per window, never forever and never again
(`cadence_is_not_due_twice_in_one_window` holds for both modes).

Cadence lines exist only while something can deliver them. Claude Code's goal check-ins arrive only while
background work holds the goal, so when no agent is running no cadence line arrives at all; nothing covers that gap
except the SessionStart card's goal line, which is on the next session or clear, and a held line then waits for the
next delivery. That is accepted: with no background work there is no progress to report. atlas does not observe
background work for this: the hook payload's `background_tasks` field comes only on Stop and SubagentStop
(`src/hooks.rs:204-206`), and the check-in rule reads only the UserPromptSubmit prompt's form (the table above).

Two goal lines are not check-ins and keep their own channels: the SessionStart card's goal line (the resume card
already carries a goal line, `src/resume.rs:331`), which with a pointer reads `goal:{slug} v{n} active, gates
{m}/{g} met @{commit}; recall goal:{slug} for the text and gates. Arm: /goal {pointer condition}`, and its
compaction form (4.3), which is the checkpoint card's existing goal line with the gate count. Neither carries
stall asks, event lines or deltas.

### 4.1 Measured progress

| | |
|---|---|
| Trigger | A `measurement` journal entry with a metric line that feeds a gate or aim of the active goal (PostToolUse of `journal`, or the rescan that imports one) |
| Inputs | The goal spec; metric lines since the last checkpoint; the last checkpoint's values |
| Deterministic | Full-set selection per gate (arms, quiet), worst-arm take, compare, delta, met count. All of it |
| Fuzzy | None |
| Delivered | The next goal check-in or agent task notification (4.0), usually the notification of the agent that measured; never a prompt of Brandon's |

Template:

```
atlas goal:{slug} v{n}: {source_key} @{commit} {state}. {for each gate and aim: "{id} {case} {stat} {value}/{bar} {unit} {MET | +delta} ({arm})"}. Gates {m}/{g} met; aims {a}/{h}. Since last: {changed ids with signed delta}.
```

Example (275 bytes; A1 and A2 are the `report` aims, present on every such line):

```
atlas goal:engine-parity v2: journal:FRAME-SET:0012 @763e4d3a quiet. G1 1080p p99 4147/4170 us MET (b1080); G2 1440p p99 5578/4170 us +1408 (b1440); A1 4K p99 10493/4250 us +6243 (b4k); A2 4K Ultra p99 19675/6940 us +12735 (h4k). Gates 1/4 met; aims 0/2. Since last: G2 -412.
```

### 4.2 Cadence

| | |
|---|---|
| Trigger | The goal's `cadence` (time since the last checkpoint entry, or main-session turns counted by the hook) is due |
| Inputs | Evaluator output; journal entries since the last checkpoint on serving lanes; commits on the measured branch since the newest gate commit (from the pr-events store, `src/pr_events.rs`) |
| Deterministic | Everything in the line: counts, ages, lanes moving or quiet |
| Fuzzy | None |
| Delivered | Claude Code's own goal check-in (`Goal check-in:` prompt, `src/hooks.rs:3327`, as Q-GO1 ruled) and agent task notifications (Q-AG2), when due; never on Brandon's own prompts (4.0) |

Template:

```
atlas goal:{slug} v{n} check-in ({age} since last): gates {m}/{g} met, aims {a}/{h} ({for each report aim: "{id} {case} {+delta} {unit}"}), newest measure {days} d old @{commit}, {k} commits unmeasured. Step {s}/{S} {label}: {progress}. Serving lanes moving: {lanes}; quiet {hours} h: {lanes}.
```

Example (299 bytes):

```
atlas goal:engine-parity v2 check-in (2 h since last): gates 1/4 met, aims 0/2 (A1 4K +6243 us, A2 4K Ultra +12735 us), newest measure 13 d old @763e4d3a, 13 commits unmeasured. Step 1/4 PARITY rows: 19 open, 2 closed since last. Serving lanes moving: FRAME-SET, PARITY-BAR; quiet 26 h: PARITY-ROWS.
```

The Q-GO1 digest (what moved by lane, open questions) follows this line on a Claude Code check-in, unchanged.

### 4.3 Events

| Event | Trigger already in atlas | Line when the event's lane serves the goal |
|---|---|---|
| Merge | pr-events listener (`src/pr_events.rs`, `src/pr_listen.rs`) | `#{pr} merged ({lane}, step {s} {label}). {k} commits unmeasured since @{commit}.` |
| Handoff | `handoff` write (PostToolUse on the atlas handoff tool) | `handoff:{lane}:{n} ({lane}, step {s}): {title}.` |
| Compaction | SessionStart `compact` (`src/hooks.rs:2405`, beside the checkpoint card) | `goal:{slug} v{n} active, gates {m}/{g} met @{commit}.` (one line, so the summary's loss of the goal is repaired at once). Not a check-in: it is the checkpoint card's goal line (4.0), on the card's channel |
| Agent finish | task notification (`notification`, `src/hooks.rs:3308`) | `{agent} finished on {lane} (step {s}); goal gates unchanged` or the 4.1 line when it measured |

| | |
|---|---|
| Inputs | The event's lane; the spec's `lane` lines; the evaluator |
| Deterministic | Lane membership by exact name; the line |
| Fuzzy | Whether an event on a lane the spec does not list serves the goal anyway (an atlas lane building frame-set tooling). Not decided per event: it feeds stall detection (4.4) |
| Delivered | Merge, handoff and agent-finish lines: the next goal check-in or agent task notification (4.0), never Brandon's own prompt; several held events collapse to one line per lane. Compaction: the SessionStart `compact` card. An event off the serving lanes adds nothing, only a count for 4.4 |

Example (146 bytes): `atlas goal:engine-parity v2: #512 merged (PARITY-BAR, serves step 1 PARITY rows). Gates unchanged until the next frame-set; 14 commits unmeasured.`

### 4.4 Stall detection

| | |
|---|---|
| Trigger | Checked whenever another check-in is computed, and on the first goal check-in or agent task notification after the goal's `stall` hours pass with no serving-lane entry |
| Inputs | Journal entries in the window, by lane; gate values over the last N baseline entries (defined in (c)); the goal text and each serving lane's `brief` entry |
| Deterministic | (a) Quiet: no entry on any serving lane for `stall` hours. (b) Drift: of the last `drift_window` (default 10) main-session lane entries, more than half on lanes the spec does not list. (c) Plateau: a gate's value moved less than its measurement noise (configurable, default 2%) over its last 3 baseline entries while its lane was busy. Baseline entries are delivered `checkpoint` entries, or for a measured kind configured `quiet` its own `quiet` entries (one per new measurement, section 4.0); a held `quiet` entry of a kind that is `on` never counts, so held lines cannot trip a plateau. Each is arithmetic on store rows |
| Fuzzy | For (b) only: does an unlisted lane's work serve the goal? Scored by the local embedder only (Q-AG7, "Local embedder only"; `src/embed`, the one atlas already ships for semantic search): cosine between the lane's latest `brief` or entries and the goal text plus the serving lanes' briefs, against two thresholds tuned on the journal. Above the upper one the lane counts as serving (and the line offers to add it); below the lower one it counts as drift; between them it is counted as unsure and the stall line names the unsure lanes ("unsure: SKY"). No Haiku, never the session model |
| Latency (review 0013 N2) | The hook never runs the encoder. Journal entries are not embedded today (`recallable` excludes `journal`, `src/memory/vectors.rs:37`), so a goal-scoped vector cache (lane entry sha to vector, the `rec_vec` pattern) is filled by the existing background embedder thread (`spawn_embedder`) when an entry on an unlisted lane is written. On the hook path drift is dot products over cached vectors, one per lane in the window (at most `drift_window`, 10); an entry not yet embedded counts as unsure that round. Budget: all goal work on one UserPromptSubmit under 2 ms, and zero encoder calls (a test counts them) |
| Delivered | The next goal check-in or agent task notification (4.0), never Brandon's own prompt; the session puts the ask to the user. At most once per `stall` window per kind, and never twice for the same lanes |

Template (always a multiple-choice ask, as Brandon prefers):

```
atlas goal:{slug} v{n} stall: {reason}. Ask the user: (1) {resume step's next key}, (2) {count the lanes as serving | change the work order}, (3) snooze goal check-ins {stall} h.
```

Example (265 bytes):

```
atlas goal:engine-parity v2 stall: no entry on a serving lane for 26 h; the last 9 entries are on ATLAS-RESUME, ATLAS-GOAL (not serving). Ask the user: (1) resume PARITY rows (journal:PARITY-ROWS:0031), (2) mark the atlas lanes as serving, (3) snooze the goal 24 h.
```

### 4.5 Met

Not a fifth kind but the end of 4.1: when every gate is met on a quiet full set, the line says so with the
evidence and, per Q-GO3, tells the session to ask the user. Only after the user agrees does the session call
`note op=goal done`. That call re-runs the evaluator, and if every gate still holds it returns the met token
(section 2.3) in its tool result and marks the goal `done`. Under a pointer `/goal` the evaluator then sees the
token and returns met, and Claude Code clears its loop by itself. Under the verbatim condition, the session asks
the user to run `/goal clear`. The line itself never prints the token.

```
atlas goal:{slug} v{n}: every gate met at @{commit} quiet ({id} {value}/{bar}, ...). Aims: {report aims with deltas}. Ask the user whether the goal is done; on yes, call note op=goal done.
```

Example (211 bytes): `atlas goal:engine-parity v2: every gate met at @9c1e07a2 quiet (G1 4101/4170, G2 4152/4170, G3 0/0, G4 0.61/0.667). Aims: A1 +402, A2 +1210. Ask the user whether the goal is done; on yes, call note op=goal done.` (made-up values: no run has met them)

## 5. Off the frontier model

### 5.1 The split

| Part | Runs on | Why |
|---|---|---|
| Spec parse, metric-line parse, full-set selection, worst-arm take, met or not, deltas | Rust, no model | Arithmetic on store rows (0002: "computed deterministically") |
| Cadence due, event membership, quiet and plateau stalls | Rust, no model | Times, counts and exact lane names |
| Every line's text | Templates (section 4) | 0002: "rendered from templates" |
| Does an unlisted lane serve the goal (drift, 4.4 b) | Local embedder (`src/embed`) on atlas's background embedder thread, CPU; the hook reads cached vectors only (4.4, Latency) | The only fuzzy question; atlas already ships the model, so no new dependency and no usage (cf. intent:r01338, local only) |
| The embedder's ambiguous band | Nothing: counted as "unsure" and named in the stall line (Q-AG7, "Local embedder only") | No model call of any kind. No Haiku caller is built; if one is ever added it must not reuse the taskjudge path (`src/taskjudge.rs`) while #424 is live (review 0013 N3) |
| Reading the line and acting on it | The session model | Only the rendered line reaches it |

The session model never computes met or not and never writes a check-in. It reads one line and, when a line
asks, asks the user.

### 5.2 Tokens per check-in that reach the session model

Measured with a throwaway script (scratchpad `atlasgoal-fix1-bytes.py`, not committed) that holds this revision's
example lines and counts them two ways: bytes / 4 (typical for this text) and a port of `chain::est`
(`src/chain.rs:120`), atlas's probed upper bound for the real tokenizer. The previous revision's byte counts were
off (4.1 to 4.4: 358, 252, 150 and 271 stated; the example lines were 275, 240, 144 and 265 bytes); these are
recounted, and 4.2, 4.3 and 4.5 changed in this revision.

The evaluator's feedback is modelled on the form goals.md section 2 recorded from transcripts, `Stop hook
feedback:\n[<condition>]: <reason>` (an internal format, not in the docs): 24 bytes of wrapper, the condition,
and the evaluator's reason. The reason is assumed to be one sentence of about 100 bytes whatever the condition
(the example used: "The 1440p gate is still over its bar and no frame set since the last commit shows it met; keep
going.", 101 bytes). The earlier 641 and 96 byte figures stated no reason length; their wrappers implied 129 and
60 bytes, so they assumed different reasons for the two conditions. Here both use the same one.

| Text | Bytes | Tokens, bytes/4 | Tokens, upper bound |
|---|---|---|---|
| 4.1 measured progress | 275 | 69 | 158 |
| 4.2 cadence (with the two 4K report aims) | 299 | 75 | 142 |
| 4.3 event (merge) | 146 | 36 | 62 |
| 4.4 stall | 265 | 66 | 116 |
| 4.5 met | 211 | 53 | 106 |
| SessionStart card goal line, with recall hint and arm command | 192 | 48 | 80 |
| `/goal` evaluator feedback per evaluated turn, verbatim condition (904 bytes: text and full amendment) | 1029 | 257 | 407 |
| `/goal` evaluator feedback per evaluated turn, pointer condition (76 bytes) | 201 | 50 | 78 |

So one check-in is about 35 to 75 tokens (upper bound 60 to 160). A line once injected stays in the context and is
re-read on every later request at the cache-read rate (0.1 of input, `src/token_cost.rs` ratios) until compaction.

A working day, estimated (not measured: the counts are assumptions, to be replaced by the build's own log): 8
cadence check-ins, 20 serving-lane events, 3 measurements and 1 stall come to about 1.6k tokens a day by bytes / 4
(8 x 75 + 20 x 36 + 3 x 69 + 66 = 1,593) and about 3.0k by the upper bound (8 x 142 + 20 x 62 + 3 x 158 + 116 =
2,966). The previous revision's 3.3k upper bound did not follow from its own table (its rows give 3.1k). Held
lines that collapse (4.0) can only lower these counts. The larger cost is Claude Code's evaluator: at an assumed
100 evaluated turns a day, the verbatim condition puts about 25.7k tokens of feedback into the session (40.7k upper
bound) and the pointer condition about 5.0k (7.8k). That is why the pointer is the re-arm (Q-AG1). The
evaluator's own Haiku call reads the whole conversation each evaluated turn whatever the condition; only option 3
of section 2.3 removes it.

The build logs each injected goal line's bytes in the hook budget log (`src/hook_budget.rs`); a PR test checks
the logging (build step 4). After merge, a week of logged bytes per check-in is journaled under ATLAS-GOAL as a
measurement and replaces the day estimate above; that week is a follow-up, not a PR acceptance.

## 6. Controls

Brandon turns check-ins and nags up, down or off at three levels. The narrowest wins.

1. **Config** (`memory.goal` in atlas's settings, `src/config.rs:1133` beside `resume_card`):
   `enabled`; per kind `measured`, `cadence`, `events`, `stall` each `on`, `quiet` (written to the checkpoint
   journal, not injected) or `off`; `cadence_minutes` (default 120); `stall_hours` (default 24); `nag`
   (`ask` puts the multiple-choice question, `line` states the stall only, `off`). No model setting: per Q-AG7
   the fuzzy band uses the local embedder only.
2. **Per goal**: the spec's `cadence` and `stall` lines (section 3), versioned with the goal.
3. **Now, in chat**: one op on the existing `note` tool, `note op=goal` (added to `OPS`, `src/tools_surface.rs:47`,
   so no new tool schema in every session's prompt), with actions:

| Action | Effect |
|---|---|
| `show` (default) | The current version, gate table and step, one recall-sized answer |
| `set slug=... text=...` / `amend slug=... text=...` | A new record / a new version (`expected_sha` as `remember` takes it) |
| `pause` / `resume` | `state` paused: no check-ins, the card says paused |
| `snooze hours=N [kind=...]` | No check-ins (or none of one kind) until then |
| `mute kind=...` / `unmute kind=...` | One kind off for this goal until unmuted |
| `done` / `drop` | Ends the goal (`done` only after the user agreed, Q-GO3). `done` re-runs the evaluator, is refused unless every gate is met, and returns the met token (section 2.3); `drop` ends it unmet, with no token |
| `set` / `amend` with a spec | Also appends the Q-GO2 `goal` journal entry (section 2.1); atlas checks the spec's grammar, never writes its content |

Brandon can say "snooze goal check-ins till tomorrow" and the session makes the call; the CLI `atlas goal ...`
offers the same actions for a shell. Every control change is journaled in the goal's lane, so the next session's
card shows it ("goal:engine-parity v2 snoozed until 11-26 09:00").

## 7. Build plan

In merge order, each sized for one builder (one PR). Each names its red-first acceptance tests; every acceptance
below is a test the PR's own `cargo test` runs, so a reviewer checks it on the PR. Stay off the judge code
throughout (`src/taskjudge.rs` and the hook paths that call it).

1. **Goal record and seed.** `goal` kind in `KINDS` (`src/memory/mod.rs:309`); new `src/goal.rs` with the body
   splitter (text, amendments, spec) and the spec parser; `note op=goal show|set|amend|pause|resume|done|drop`
   (`src/tools_surface.rs`, handler beside `src/tools_memory.rs`); `goal_records` reads kind `goal` with the
   `active-goal*` fallback; the `goal` journal entry kind, appended by every `set` and `amend` that carries a
   spec (Q-GO2, section 2.1); a one-shot `atlas goal seed --from-memory active-goal-engine-parity --spec <file>`
   that writes v1 (text) and v2 (amendment, and the orchestrator's spec when `--spec` is given). Tests in
   `src/goal_tests.rs`: `the_seed_has_both_versions_and_the_spec`,
   `the_seed_amendment_is_the_memory_paragraph_byte_for_byte` (fixture: memory v4's AMENDED paragraph, ending
   "give the original text and this amendment."), `without_a_spec_file_the_seed_has_no_gates`,
   `atlas_never_writes_a_spec_line_the_caller_did_not_send`, `a_spec_write_appends_one_goal_journal_entry_with_the_version_sha`,
   `an_amendment_is_a_new_version_and_the_head_has_every_amendment`, `a_gate_with_no_metric_is_refused`,
   `a_second_active_goal_is_refused`, `report_lists_only_known_aims`. Built with journal:ATLAS-GOAL:0031's
   recommended options: a state change is a new version with the text unchanged (Q-GS1); `done` is refused until
   the met check lands (Q-GS2); the seed copies the memory's quoted paragraph as the text and its `AMENDED`
   paragraphs byte for byte, nothing else (Q-GS3); `goal_records` falls back to `active-goal*` while the store
   has no `goal` record (Q-GS4); an amendment not in the `AMENDED` form is refused (Q-GS5).
2. **Metric lines and the evaluator.** The `metric:` line grammar (section 3) in measurement entries (parsed at
   write and at import); `goal::evaluate(store, goal) -> Eval` with full-set selection, quiet filter, worst-arm,
   met count and deltas against the last checkpoint. Per Q-AG3 the frame-set tool's own `metric:` output is a
   separate game PR on the FRAME-SET lane, and briefs that measure a gate's metric gain a rule to write
   metric lines; atlas parses no prose. Tests: `sitting_b_gives_g1_met_g2_not` (sitting B values as
   a fixture), `a_contended_line_does_not_feed_a_quiet_gate`, `a_partial_set_is_reported_not_used`,
   `an_aim_never_decides_met`, `an_unfed_gate_is_not_measured_never_met`,
   `g4_without_arms_takes_the_newest_quiet_line` (Q-AG6, review 0013 N1), `worst_arm_decides_1080p_and_1440p`
   (Q-AG5). **Built** (`src/goal_eval.rs`, tests `src/goal_eval_tests.rs`, `tests/goal_journal.rs`) with
   journal:ATLAS-GOAL:0039's recommended options: the store has no journal import, so a metric line is checked
   when the journal tool writes or amends a `measurement` entry (a malformed one refused with the reason) and
   parsed again when `evaluate` reads the store, where a malformed line is skipped and named in `Eval.skipped`
   (Q-GS6); a goal checkpoint is a `checkpoint` journal entry in the goal's lane with meta `goal: goal:<slug>` and
   one `value: <id> <value> <unit> @<commit>` line per measured gate and aim, rendered by
   `goal::checkpoint_values` for step 4 to write (Q-GS7); commits are compared as written, lower-cased (Q-GS8); a
   line whose unit is not the gate's is not used and is named in `Eval.skipped` (Q-GS9). A metric line in another
   entry kind feeds nothing, and the journal tool warns. The work-order step is the first whose `done-when` does
   not hold on the newest quiet line of its metric; a step with no `done-when` is never passed. Step 1
   follow-ups (journal:ATLAS-GOAL:0037) ride along: the journal tool refuses `entry=goal`; `split` and the seed
   read CRLF bodies; the seed applies `set`'s fence and `AMENDED` checks; the `AMENDED` form is checked for a
   date and an author; the unread `KINDS` constant is gone.
3. **Card, re-arm and met token.** Depends on steps 1 and 2 (the gate count on the card and the compaction line
   come from `goal::evaluate`). `src/resume.rs:331-342` and `goal_line` read the active record (current version
   and gate count) instead of the transcript's text plus one amendment; the pointer re-arm line with the recall
   hint (Q-AG1, section 4.0); the not-this-goal line when the armed `/goal` differs; the compaction one-liner
   (`src/hooks.rs:2405`, `src/checkpoint.rs`); `note op=goal done` issuing the met token (section 2.3). Tests: the
   ATLAS-RESUME:0001 item 2 fixture (`the_card_gives_the_amended_goal_after_a_clear`),
   `the_pointer_card_line_names_recall_goal_slug`, `a_hand_typed_goal_that_is_not_the_record_is_flagged`,
   `a_user_goal_clear_leaves_the_record_active`, `done_is_refused_while_a_gate_is_unmet`,
   `done_issues_a_fresh_12_hex_token_once_every_gate_is_met`,
   `no_card_or_template_line_contains_the_met_token_form` (every template and both cards rendered for a goal with
   every gate met; none matches `met-token [0-9a-f]{12}`), `the_pointer_condition_contains_no_nonce`,
   `no_store_row_other_than_the_goal_record_contains_a_token_form` (done journal entry and hook budget log),
   `every_read_path_of_a_done_goal_shows_issued_not_the_nonce` (every read path section 2.3 names, through
   `goal::redact_meta`).
4. **Measured progress and cadence check-ins.** The 4.1 and 4.2 templates on the UserPromptSubmit paths
   (`src/hooks.rs:1916`, `notification` at `:3308-3328` for Claude Code's check-in); `checkpoint` journal entry per
   delivery; the prompt classifier of section 4.0 (goal check-in, agent notification, anything else); held lines
   collapse; config `memory.goal` (`src/config.rs:1133`); bytes logged in `src/hook_budget.rs`. Tests:
   `a_goal_checkin_prompt_gets_the_cadence_line_and_the_digest`, `an_agent_notification_gets_the_measured_line`,
   `brandons_own_prompt_gets_no_goal_line_even_when_due`, `a_line_held_over_a_user_prompt_rides_the_next_notification`,
   `cadence_is_not_due_twice_in_one_window`, `every_checkin_line_carries_both_4k_report_aims`,
   `a_held_line_neither_becomes_the_delta_baseline_nor_resets_the_cadence_timer` (a due line held over a user
   prompt, then delivered: its delta equals the value change since the last delivered checkpoint, and cadence due
   time is unchanged by the held `quiet` entry), `with_only_user_prompts_no_cadence_line_is_injected` (input: a
   run of UserPromptSubmit payloads in neither notification form, with the cadence due throughout; the rule reads
   the prompt's form only, never `background_tasks`, section 4.0),
   `a_held_line_over_many_prompts_writes_one_quiet_entry_per_window` (cadence 120 m, due, then five hours of user
   prompts every 10 minutes: exactly three `quiet` entries, the delivery baseline still the last delivered
   `checkpoint`, and the next agent notification's delta measured from it),
   `a_quiet_configured_kind_advances_its_baseline_on_its_own_quiet_entries` (cadence configured `quiet`, agent
   notifications every 10 minutes for five hours: one `quiet` entry per 120 m window, each with its delta against
   the previous `quiet` entry, and the cadence not due again inside a window),
   `a_quiet_kind_journals_but_injects_nothing`, `nothing_is_added_on_stop`,
   `every_injected_goal_line_is_logged_with_its_byte_count` (the logged count equals the injected text's length,
   for each kind), `the_example_lines_render_at_the_documented_sizes` (sitting B fixture: 275 and 299 bytes).
   Post-merge, not acceptance: a week of logged bytes per check-in, journaled as a measurement under ATLAS-GOAL
   (section 5.2).
5. **Event lines.** Merge (pr-events), handoff (PostToolUse), agent finish (compaction is step 3), each only for serving
   lanes, delivered per section 4.0. Tests: `a_merge_on_a_serving_lane_gets_one_line`,
   `an_event_off_the_serving_lanes_adds_nothing`,
   `an_agent_that_measured_gets_the_measured_line_not_the_finish_line`,
   `an_event_during_user_prompts_waits_for_the_next_notification`.
6. **Stall detection.** Quiet, drift and plateau arithmetic; the embedder score for unlisted lanes from a
   goal-scoped vector cache filled by the background embedder (section 4.4, Latency), with thresholds tuned on the
   journal (a local batch, like the ATLAS-DIALS backtest); the band between them counted as unsure and named
   (Q-AG7). No Haiku caller (section 5.1). Tests: `no_serving_entry_for_stall_hours_is_a_stall`,
   `drift_counts_only_unlisted_lanes_below_the_lower_threshold`, `the_unsure_band_is_named_in_the_line`,
   `plateau_counts_baseline_entries_never_held_quiet_entries` (a flat gate with three held `quiet` entries since
   one delivered `checkpoint` is no plateau; the same values on three delivered `checkpoint` entries are, and so
   are three `quiet` entries of a measured kind configured `quiet`),
   `a_stall_is_asked_once_per_window`, `brandons_own_prompt_never_gets_a_stall_line`,
   `the_hook_path_makes_no_encoder_call` (a counting model stub), `an_unembedded_entry_counts_as_unsure`,
   `the_tuned_thresholds_hold_on_the_held_out_lanes` (review 0013 N4: the tuning batch writes its thresholds and a
   labelled fixture of lanes, serving or not, from the journal, split by lane into tune and held-out halves; the
   test asserts that on the held-out half no lane the fixture labels serving is called drift, and that at least
   half the lanes it labels not serving are; the fixture and the numbers are committed with the PR).
7. **Controls.** `snooze`, `mute`, `unmute` actions and the CLI `atlas goal`; card shows the snooze. Tests:
   `a_snoozed_goal_injects_nothing_until_the_time`, `a_muted_kind_stays_off_across_a_clear`.
   (Small enough to fold into step 4 if the builder there has room.)

Order: 1, then 2, then 3. Steps 3 to 6 each depend on step 2 (and so on 1); 3 needs `goal::evaluate` for the
card's gate count and the compaction line, so it cannot run beside 2 (review 0013 S3). Once 2 has merged, 3 and 4
may run side by side; 5 and 6 follow 4 (they use its delivery path and held-line collapse); 7 follows 4.

## 8. Questions, answered

Brandon answered all seven on 2026-11-25, each with the recommended option (journal:ATLAS-GOAL:0014,
journal:ATLAS-GOAL:0015). None is open.

| Question | Answer, in Brandon's words | Folded into |
|---|---|---|
| Q-AG1, re-arm | "Pointer goal": the card offers the pointer `/goal` | 2.3 (with the met token, review 0013 S1), 4.0 card line, step 3 |
| Q-AG2, check-in delivery | "Goal + agent notices": Claude Code's goal check-ins and agent task notifications only | 4.0, 4.1 to 4.4 Delivered rows, steps 4 to 6 |
| Q-AG3, who writes metric lines | "Frame tool + briefs": the frame-set tool emits `metric:` lines, briefs require them elsewhere | 3, step 2 |
| Q-AG4, goals at once | "One active per project", others paused | 2.2, step 1 |
| Q-AG5, the hard line's statistic | "p99, 1080p and 1440p": both gates, worst arm | 3.1 (G1, G2), step 2 |
| Q-AG6, the 50% over Bevy clause | "Gate at 2/3 of Bevy p99", from H2H, unmeasured until then | 3.1 (G4, no guessed arms), step 2 |
| Q-AG7, the fuzzy band | "Local embedder only", unsure band named | 4.4, 5.1, 6, step 6 |

Q-GO2 is kept, not replaced (section 2.1), so no new question was needed for it.

