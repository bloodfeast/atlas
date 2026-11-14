# Off-topic hook prompts (v0.5.0 recall defaults)

v0.5.0 changes two recall defaults: `budget_tokens` 150 -> 600 and `cos_floor` 0.57 -> 0.43. The 303-prompt
hook replay that measured them has only on-topic prompts. This set measures what the prompt hook injects for
prompts that should get nothing, or only their own lane's records. Everything here was fixed and committed
before the first run; the commit time is the proof of order.

## The prompts (`prompts.tsv`)

`label<TAB>prompt` lines, the `atlas --recall-scores` format, read by
`cargo xtask memory-eval ... --offtopic prompts.tsv`. 65 prompts:

- (a) 30 `neg` prompts unrelated to any project: 20 everyday questions (cooking, travel, history, health) and
  10 general programming questions (CSS, SQL, git, Python, nginx) that name no project, tool or lane.
- (b) 35 lane-crossed prompts, each about one named lane of the eval store (snap-2026-11-02) and naming it:
  CLIP-SIM 5, UNIT-LIGHTS 5, NEW-MAP 5, RIDGE-MAP 5, LOOK-FOLLOWUPS 4, FILM-WICK 3, TURF 2, QUARRY-FLOW 2,
  DEPOSITS 1, FSE-FALLBACK 1, ATLAS-PACKAGE 1, LEAN-H 1. The label is the prompt's lane; the run fails if the
  store has no such lane.

## The labelling rule (mechanical, `Label::off` in xtask/src/memory_eval.rs)

An injected item is one entry of the hook's injection log (`Hooks::last_injected`): a pointer to a record, a
lane view (`lane:X`), a lane head (`lane-head:X`) or a repository line (`repo:X`). Its lane is X for a lane
view or head; for a record, the record's lane when it is a journal or handoff record (key
`journal:LANE:N` or `handoff:LANE:N`); otherwise it has no lane.

- (a) `neg`: every injected item is off-topic.
- (b) lane prompt: an item is off-topic when it has a lane and that lane is not the prompt's. An item with
  no lane (a rule, memory or section record, a repository line) is not counted off-topic; it is counted as
  injected.
- Control (the 303 recall questions): a question whose source is a journal or handoff record takes that
  record's lane and is labelled by rule (b); the other questions are unlabelled.

## The arms

The eval configuration (`eval-atlas.json`, snap-2026-11-02), every key identical but these:

| arm | `recall.budget_tokens` | `recall.cos_floor` | `recall.prompt_pointers` |
|---|---|---|---|
| old | 150 | 0.57 | default (2) |
| new | 600 | 0.43 | default (2) |
| old-pp0 | 150 | 0.57 | 0 (no cap) |
| new-pp0 | 600 | 0.43 | 0 (no cap) |

old and new are the v0.5.0 behaviour before and after the change. The pp0 pair reproduces the setting the
CHANGELOG's numbers were taken in (DEFAULTS-1 ran before the 2-pointer cap landed), so the 303-prompt control
must give 179/210/61.5 (old-pp0) and 181/214/70.4 (new-pp0) strict/informed/tokens per prompt at floor 0,
select on; if it does not, nothing else is trusted.

## The pass line (fixed before the run)

On the old/new pair (shipped behaviour), select on, floor 0, the new arm passes when both hold:

1. it injects at least one item on no more than 10% of the `neg` prompts (at most 3 of 30); and
2. its off-topic items per lane prompt are at most 0.25 above the old arm's.

The pp0 pair is reported against the same line for information; the verdict is the old/new pair's.

## Widened rule (ATLAS-V05-MEASURE sweep)

Decided before the sweep's code and run (journal:ATLAS-V05-MEASURE:0006, "Yes, widen and rerun"). The pass
line above is unchanged. `prompts.tsv` has 69 lines: 4 header comment lines starting `#`, which the parser
skips, and the 65 labelled lines.

Tokens: a token is a maximal run of the characters `[A-Za-z0-9_-]` in a string. L is the store's lane names
(`lanes()`) without the names that start with `session:`. For an intent or memory record, S is the set of
names in L that equal a token of its key or of its head title, case-sensitive: `TURF` matches, `turf` and
`Turf` do not, and `QUARRY-FLOW-2` is not `QUARRY-FLOW`.

Item lane:

- `lane:X`, `lane-head:X`, journal and handoff records: unchanged (X, or the record's lane).
- intent records (key `intent:...`, kind rule or block) and memory records (kind memory, bare key): S.
- everything else (restore sections, docs, repository lines): no lane, unchanged.

Counting on a lane prompt whose lane is P:

- one lane X (|S| = 1, or a journal, handoff or lane item): off-topic iff X is not P.
- no lane (|S| = 0): not off-topic; counted as injected.
- two or more lanes (|S| >= 2, ambiguous): off-topic iff P is not in S. Ambiguous items are counted in
  their own column (`amb`) and shown in a row as `key[A|B]`.
- `neg` prompts: every item is off-topic (unchanged).
- Control (303): a question whose source is a journal or handoff record is unchanged; a question whose
  source is an intent or memory record takes its lane when |S| = 1, is unlabelled when |S| = 0, and is
  unlabelled and counted in the control's `amb` when |S| >= 2.

The sweep runs eight arms on the same prompts, control and store: old (0.57/150), cos_floor 0.43, 0.47, 0.50,
0.53 and 0.57 at budget 600, all with the shipped 2-pointer cap, and the two pp0 arms as the control.
