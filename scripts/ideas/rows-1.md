# IDEA-CAPTURE eval rows, first shipped detector

Counts only, no prompt text. Reruns from local files named by path:
`ATLAS_IDEAS_TUNE`, `ATLAS_IDEAS_LABELS` and `ATLAS_LEAK_DENYLIST` with
`cargo test --test ideas shipped_detector_on_tune -- --ignored --nocapture`, and
`cargo test --release --test ideas hook_latency -- --ignored --nocapture`.

corpus 7020152bd7352c450892033da94190638962f839d8170a097675c2bb9712fe57 (tune)
labels b7f77646b8e2839413231d82efc2b31830166bc4e7c1c56cf66e4421d5d6b8df
weights eff1f59dd3330642400d345f999010a5f29da4c4e79b1af5b82c4749fbb71940
detector 065cc8381a1a706e22d4c8265652cf33941a612b7a5d9d63f75a3bd6d330dd87

The held-out set is spent and was not rescored; its one score is in the local
report heldout-baseline-1. The weights are one author's prompts with label error
(about 17% wrong on pass 3 rows, about 10% on sure ideas, about 3% on sure
not-ideas, by his own check). Generality to anyone else is untested. The corpus
is span-redacted, as live capture is. Queued prompts are in the rows, but whether
UserPromptSubmit fires for them live is unproven.

## Detector through the shipped path, point wide, threshold -1.658

The test asserts each row against the frozen tune report. `eval score` rebuilt
from this tree matches that report's counts; the only differences are five
per-day cells by 0.1, from the rounded `--per-day` rates passed, and timings.

| stratum | rows | P | misses | captures/100 | false/100 | captures/day | false/day |
|---|---|---|---|---|---|---|---|
| typed | 915 | 347 | 5 | 67.7 | 30.3 | 29.5 | 13.2 |
| queued | 86 | 29 | 2 | 66.3 | 34.9 | 3.4 | 1.8 |
| prompt | 1001 | 376 | 7 | 67.5 | 30.7 | 32.9 | 14.9 |
| ask | 87 | 36 | 0 | 77.0 | 35.6 | 2.9 | 1.3 |

A bare word count near the same captures: 10 words or more has recall 93.9,
captures/100 66.6, false/100 31.4; the detector's prompt row has recall 98.1 at
67.5 and 30.7.

## The ask band, off by default

`asks_per_day` ships at 0: nothing asks and the band files nothing. The rows are
what the ruled band (0.25 below, 0.5 above, cap 5) would see.

| side | width | prompts/day | ideas in it |
|---|---|---|---|
| below | 0.25 | 1.6 | 1 of 34 |
| above | 0.5 | 3.5 | 11 of 72 |

About 5.1 a day before the idle rule. Asked rows are chosen by band, not a random
sample.

## Hook time, release, ms, 2,000 seeded records

Prompt answers on 1,001 tune prompts in a 5 MiB transcript whose record is found
on the first read. Two passes each; the ranges are the two passes. With writers,
prompts are paced at 20 ms, so off and on see the same load (about 760 commits
off, 720 on). Not measured: origin/main, a 13k-row backlog, and vectors on
(a capture commit makes the next recall reload them, as any journal entry does).

| case | not filing p99 | capture answer p99 / max | drain after answer p50 / p99 / max | Stop p99 / max |
|---|---|---|---|---|
| off | 1.54-1.58 | - | - | 0.013 / 0.03 |
| on, shipped default | 1.60-1.61 | 1.19-1.81 / 3.3 | 9.6 / 20.6 / 33 | 0.02 / 0.03 |
| on, cap 5 | 1.62 | 2.18 / 102 | 16 / 167 / 315 | see below |
| on, every Stop asks | 1.60 | 2.20 / 104 | 16 / 166 / 320 | see below |
| off, two writers at 50 ms | 6.01 | - | - | 0.016 / 0.03 |
| on, two writers at 50 ms | 6.01 | 8.34 / 96 | 14 / 119 / 148 | 0.02 / 0.04 |
| on, two writers flat out | 58.0 | 13.3 / 218 | 26 / 136 / 440 | 0.02 / 0.03 |

Filed: 676 of 676 distinct on the shipped default, 710 of 710 with the band on,
under every load. Surface line: 286 bytes. The drain reads the transcript backward
for the person test, and on a prompt it runs after the answer is written: Claude
Code does not wait on it for an `mcp_tool` hook, only the next hook does. Its held
item backs off from 1 s to 30 s and rereads only a grown transcript when its record
is not found; a Stop skips the wait but not the growth test.

These rows were taken before the drain moved onto Stop's clock, on an earlier day.
The drain's shipped-default cell (9.6 / 20.6 / 33) did not reproduce: on 2026-11-06
the unchanged commit f9c0d4b gave 16-17 / 139-300 / 443-451, and this tree
16-18 / 134-336 / 340-548, so that difference is the machine, not a change.

## Stop's time with the drain on its clock, release, ms

A Stop that could ask drains the spool before it answers, so Claude Code waits for
it. Same harness and seeds as above, 2,000 records, a 5 MiB transcript with no goal
in it, so the goal search reads all of it. Spool and store under a temp folder on
Z:. "No tool call" has no event between the prompt and Stop, so the capture is
still spooled at Stop; it is the turn that pays. Backlog is 16 prompts spooled
before one Stop, the most one Stop drains. Two passes of one binary; the ranges are
the two passes. Pass 1 had another lane compiling for at least part of it, pass 2
had no rustc running at its start or end. Not a quiet machine in either: Brandon may
have been using it.

| case | Stops | p50 | p99 | max |
|---|---|---|---|---|
| cap 5, tool call, all Stops (mostly a capped day) | 1001 | 0.21-0.22 | 0.46-0.66 | 54-58 |
| cap 5, no tool call, all Stops | 1001 | 0.23-0.24 | 18-23 | 99-134 |
| cap 5, no tool call, Stops that asked | 5 | 54-68 | 54-86 | 59-134 |
| every Stop asks, tool call, all Stops | 1001 | 0.88-0.92 | 51-108 | 402-410 |
| every Stop asks, no tool call, all Stops | 1001 | 15-16 | 135-319 | 534-681 |
| every Stop asks, no tool call, Stops that asked | 106 | 51-54 | 135-329 | 514-534 |
| same, two writers at 50 ms, all Stops | 1001 | 16 | 133-170 | 687-875 |
| same, two writers flat out, all Stops | 1001 | 25-29 | 182-194 | 261-272 |
| backlog of 16, all Stops | 62 | 155-163 | 626-889 | 718-1011 |
| backlog of 16, two writers at 50 ms | 62 | 183-204 | 463-1080 | 1183-1520 |
| backlog of 16, two writers flat out | 62 | 200-228 | 401-444 | 480-1092 |

The earlier claim that a Stop costs 2.81 (cap 5) and 48.8 (every Stop asks) at p99
no longer stands; those runs drained before each Stop. At the shipped default, asks
0, a Stop returns before all of this: 0.02-0.03 p99. The unchanged commit under the
tool-call cases gave p50 0.67 and p99 1.6 for cap 5, and p50 0.66 and p99 49 for
every Stop asks, in one pass: checking the cap before anything else takes a capped
day's p50 from 0.67 to about 0.2. The p99 of a Stop with work to do is mostly the
filing commit's fsync and the goal search. These are the hook's own time, in
process, not the round trip through Claude Code.

Filed under every case: 710 of 710 distinct with the band on, 676 of 676 without.

### Transcript size, and held items that never match

The table above understates a long session. The 300 newest transcripts run p50
381 KB, p90 243 MB, max 274 MB, 98 of them over 4 MiB, and a Stop about to ask
searches for a goal through the whole file when the session has none. Same harness,
two passes, no rustc running at the start or end of this run, no writers:

| case | Stops | p50 | p99 | max |
|---|---|---|---|---|
| 243 MB, cap 5, Stops that asked | 5 | 1637-1885 | 1701-1971 | 1732-1989 |
| 243 MB, every Stop asks, Stops that asked | 106 | 1553-1594 | 2351-2369 | 2513-2616 |
| 243 MB, cap 5, all Stops (mostly a capped day) | 1001 | 0.23 | 21-28 | 1732-1989 |
| held items that never match, 1 MiB of output a turn, up to 144 held | 200 | 405-507 | 612-937 | 671-2058 |

So on a long session an asked Stop takes about 2 s, and this goal search, not the
drain, is nearly all of it. It predates the drain's move and runs at most about
`asks_per_day` times a day, just before a dialog that holds the session anyway; a
session with a goal record near the end stops there. It stops the hook worker for
that time, and its Stop hook has no `timeout` of its own in hooks.json, so this is
Claude Code's default for an `mcp_tool` hook, not checked here. The last row is the
cost of a Stop rereading held items whenever the transcript grew: up to 16 items at
up to 4 MiB each, on every askable Stop for as long as such items stay held, at
most 10 minutes each. They need a prompt whose record has a different head (a
leading paste), so that row's 144 held is far past any usual day. Not measured: both
together, or the same on a machine with writers.

The `no-tool` tags' `capture, drain` rows are not reported, as no drain runs there.
Backlog groups can straddle two sessions and skip the writers' pacing for 15 of 16
prompts, which shifts those rows slightly.

## Who wrote the newest prompt, at a Stop

Counts over the 300 newest main-session transcripts on 2026-11-06, structure only,
with a turn taken as ending at a `turn_duration` or `stop_hook_summary` record:
19,082 person turns reached a Stop, and on 771 (4.0%) the newest prompt record was
`isMeta`, so no question could be asked. Reading a record with `turnCompanion: true`,
no `origin` and no `queueSkipAttachments` as not a prompt recovers 523 of them,
leaving 248. Of 49,255 non-person turns at a Stop, none then read as a person's.
Of 5,200 companion records 78 started a turn: 77 had `queueSkipAttachments`, which
stay prompts, and one did not. The field is undocumented, so a change to it falls
back to asking nothing.

Prompts sharing their first 200 normalised bytes with an older prompt in the same
transcript and differing in origin: 51 non-person after a person's, 31 the mirror.
`goal_status` records: 57 with `met: true` and `iterations` and no `sentinel`, 120
sentinel `met: true`, 919 sentinel `met: false`, 1,312 `met: false` with no
sentinel. All 112 goal records directly after a `met: true` were sentinel `met: false`.

Whether a prompt's record is always written after its hook ran is not shown by any
of this; a record stamped earlier does not say when it was flushed. One live run
(journal IDEA-CAPTURE 0036) saw it lag the first drain. If one were written first,
that prompt would wait out its 10 minutes and file as `origin: unverified`, never
asked or surfaced. Rising unverified counts would show it.

## Cost in token-turns

A line costs its tokens on every later model call until compaction, so tokens
alone understate it. Denominators are the orchestrator audit of journal entry
ATLAS-V03:0047: 2,800 calls in 20 segments, 469M token-turns over about 5.5 days,
about 85M a day, about 167k context a call, 2.42 chars per context token. A line
lands on average with about 70 calls left in its segment. These are estimates from
that audit, not a measurement of this feature.

| surface | tokens | a day | token-turns a day | share of the orchestrator |
|---|---|---|---|---|
| surface line, one per new capture per session | 118 | at most 36 per session | about 0.30M | about 0.35% |
| question, cap 0 as shipped | - | 0 | 0 | 0% |
| question, cap 5, not proven live | up to about 1,050 of text, at least 2 extra calls | 5 | up to about 2.0M | up to about 2.4% |

The line's count is per session: every session that sees a check-in or notification
pays it, and a server restart forgets what it has shown, as does a dismiss of the
newest id. Off: no context, one hook round trip that does no work for Stop and for
each `AskUserQuestion`, 0.013 ms in the hook.

## Backlog

Captures per day stay about 33 prompts and 3 asks, about 16 of them false. Pending
rows never close by themselves; surfaces show 7 days only; about 13k rows a year.
