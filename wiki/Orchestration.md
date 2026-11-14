Orchestrated mode is for a Claude Code session that delegates its work to subagents. The main session plans,
delegates, verifies and merges. Subagents do the work in **lanes**, and atlas keeps the state between them:
lane journals, handoffs and recall. It is off by default. `atlas --setup`, `atlas settings orchestrate` and
the first session ask whether you want it, and nothing is on until you say yes (see [[Install and setup]]).
The `orchestrate` skill teaches the session to work this way. The keys are in [[Configuration reference]].

## Lanes

A lane is one stream of work with an id, for example `FEATURE-1`. A lane has no record of its own: it exists
once a `journal` or `handoff` entry is written under it. Each agent works in exactly one lane, and journals
its brief, decisions, measurements, commits, questions and next step there. The next agent in the lane
starts from `recall lane=FEATURE-1`.

**Declare a lane** before its first launch, with no new tool: a `journal` entry of sub `brief` in the lane,
carrying the same lines the launch prompt carries in its first 40 lines:

```text
Kind: builder
Lane: FEATURE-1
Priority: normal
Uses: compile, worktree:feature-1
```

- `Kind:` is `builder`, `reviewer`, `research` or `brief-writer` (also `brief writer`). The kind belongs to each
  launch, not to the lane: one lane runs a builder, then its read-only reviewers. A brief writer has its own
  cap, uncapped by default, so writing briefs never takes a builder slot.
  A launch with no `Kind:` line and the subagent_type `builder`, `reviewer` or `research` counts as that kind.
- `Priority:` is `urgent`, `normal` or `background`.
- `Uses:` lists the shared resources the work holds (`gpu`, `compile`, `worktree:<name>`, or any name you
  give a capacity), or `read-only` for an agent that shares its lane, such as a reviewer.

atlas schedules on declared lines only. What it infers from a prompt's words (a lane named in passing, a
build or a GPU run) is a warning, never a refusal.

## Kinds and caps

With the caps on, the Agent PreToolUse hook counts the session's running agents by kind and denies a
launch past its kind's cap, with a reason that names the queue entry it was filed as. It also denies a
launch whose lane or resource conflict is declared on both sides, and warns on an inferred one. It never
allows a call, so permission prompts are unchanged.

A builder launched with no `Lane:` line is let through, with one context line telling the orchestrator to
declare the lane: the `Lane:`, `Kind:`, `Priority:` and `Uses:` lines, and the lane's `brief` journal
entry. Reviewer, research and uncounted launches get no line, and with orchestrated mode off nothing is
said.

`atlas queue` lists what waits, `atlas queue next` proposes a batch that keeps lanes and resources apart,
and a task notification lists what waits when an agent finishes.

## Starter resources and caps

On yes, setup writes starter values for the keys you did not answer and the configuration does not set. A
configured value or a flag always wins. They are derived from the machine when the answer is applied:

| Key | Starter | Rule |
|---|---|---|
| `orchestrate.resources.compile` | max(1, hardware threads / 16), at most 4 | a 16-thread machine gets 1, a 32-thread machine 2 |
| `orchestrate.resources.gpu` | 1 | `gpu` is a lock and atlas assigns no device, so a second holder on a two-GPU machine would still land on device 0; nothing is probed |
| `orchestrate.kinds.builder.max` | the compile slots, at least 1 | builders are what compile |
| `orchestrate.kinds.reviewer.max` | the builders | one review beside each build |
| `orchestrate.kinds.research.max` | max(1, builders / 2) | |

Setup writes no starter for `orchestrate.kinds.brief-writer.max`: brief writers are uncapped unless you set one.

Each cap is clamped to Claude Code's own limit (`CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS`, 20 by default). A
machine with 64 threads gets compile 4, gpu 1 and caps 4, 4 and 2. The caps follow the compile slots you
answer: `atlas settings orchestrate --yes --compile 3` gives 3 builders and 3 reviewers. A capacity of 0
never serializes that resource.

The handoff point and the hard limit are not written: they derive from the context window and scale with
it, and a stored number would not. An installation that never ran setup keeps the code defaults (gpu 1,
compile 1, caps 2, 2 and 1). Answering again never rewrites a value you set.

```sh
atlas settings orchestrate --yes                           # the starters for this machine
atlas settings orchestrate --yes --compile 2 --gpu 1 --research 1
```

## Context limits

atlas reads each agent's real context size from its transcript. Past its kind's handoff point an agent is
told to wrap up and write its handoff; past its hard limit (enforced mode) every tool but `handoff` is
denied, and so is a SendMessage to it. When it finishes, the orchestrator is told to start a fresh agent from
the handoff, or to ask the user first (`orchestrate.fresh`).

## Reports and SubagentStop digests

A subagent's report stays in the orchestrator's context for every later turn, so the skill asks for the
digest form: the outcome first, then the numbers, the open questions and the risks, with anything longer
filed as a journal entry and cited by key. In orchestrated mode the SubagentStop hook sends a report over
28,000 bytes back once: atlas files it whole in the agent's lane and asks for the digest. A report within the bound, or a second stop, goes through. When an agent finishes without
having written a journal entry, the task notification says so.

## An example

A project `my-app`, with orchestrated mode on:

1. The orchestrator writes the `brief` entry for `FEATURE-1` with the four lines above, then launches a
   builder whose prompt starts with the same lines.
2. A second builder in `FEATURE-1` is denied and queued while the first holds the lane; a builder in
   `FEATURE-2` that also uses `compile` waits only when the compile slots are full.
3. When the builder opens its PR, two reviewers launch with `Kind: reviewer`, `Lane: FEATURE-1` and
   `Uses: read-only`. They hold no lane, so they run beside the builder, and they wait only for
   reviewer slots.
