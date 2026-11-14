---
name: orchestrate
description: Run a large piece of work as an orchestrator - split it into lanes, delegate each lane to a subagent with a lean brief, keep within atlas's per-kind agent caps and context limits, verify before merging, and start a fresh agent from the handoff whenever one reaches its limit. Use when work is bigger than one agent's context, when the user asks to delegate, orchestrate, parallelise or "have agents do it", or when an atlas agent-cap or context-limit message appears.
---

# Orchestrating with atlas

The main session plans, delegates, verifies and merges. Subagents do the work. atlas holds the state between
them: lane journals, handoffs and recall. No agent has to carry another agent's context.

## Lanes

- A **lane** is one stream of work with an id, for example `PARSER-2`. Each agent works in exactly one lane, and
  journals under it with the atlas `journal` tool: its brief, its decisions, its measurements, its commits, any
  questions, and what is next.
- An agent that stops writes a **handoff** for its lane with the atlas `handoff` tool. The next agent starts from
  `recall lane=<id>`, so nothing is lost when an agent ends.
- **Never resume an agent past its handoff.** Start a fresh one from the handoff. A short reply to an agent
  that is mid-task and below its handoff point is fine.
- **Declare a lane** before its first launch: a `brief` journal entry in the lane with Kind, Lane, Priority and
  Uses lines, the same lines the launch prompt carries (see the brief, step 2). For example:

  ```text
  Kind: builder
  Lane: FEATURE-1
  Priority: normal
  Uses: compile, worktree:feature-1
  ```

  Kind stays a property of each launch: the same lane later runs reviewers with `Kind: reviewer` and
  `Uses: read-only`. A builder launched with no `Lane:` line is let through, and atlas reminds you to declare
  its lane.

## Kinds and caps

Every agent has a kind:

- **builder** writes and compiles code;
- **reviewer** reads and reviews, and does not build;
- **research** researches or designs, and does not build.

With agent caps on (`atlas settings orchestrate` asks for them), atlas denies a launch past its kind's cap. Setup
derives the caps from the machine unless you answer them: compile slots of one per 16 hardware threads (at least
1, at most 4) and 1 gpu slot, then builders equal to the compile slots, as many reviewers, and half as many
research agents (at least 1). A configured value always wins. When a launch is denied, do not retry it: atlas queues it (the
reason names its key), and a task-notification lists what waits when an agent finishes. Then launch the next one
whose kind has a free slot, with the same description and brief (`recall KEY` reads it); that removes it from
the queue. `atlas queue` lists the queue, and `atlas queue drop KEY` removes a launch you no longer want.

The kind comes from a `Kind:` line in the first 40 lines of the Agent launch prompt itself; a brief file the
prompt names is not read. Without one, Explore and Plan agents count as research, an agent in its own worktree
counts as a builder, and anything else gets the configured default kind (builder). So write the line.

## The brief

Keep it lean. A brief is a work order, not a copy of the rulebook.

1. **Who and what:** the lane id, the goal in one or two sentences, and the deliverable (a branch, a PR, or a
   report).
2. **`Kind: builder`** (or `reviewer`, or `research`), and the work queue's lines: `Lane: <id>`, `Priority: urgent`,
   `normal` or `background`, and `Uses:` with the exclusive resources it needs (`gpu`, `compile`,
   `worktree:<name>`, one port or database such as `sqlport 15126`) or `read-only` for an agent that shares its
   lane. Never list repositories: each agent works in its own worktree. Only `gpu`, `compile`, `worktree:*` and
   the resources the configuration lists can hold a launch back; any other word (`blender`, unless it is
   configured) only adds a note when a running agent uses it too. atlas schedules on declared lines;
   it only warns on what it infers. A planned item's brief adds `Queue: <key>` so its launch takes it off the queue.
3. **The rulings that apply,** named by their recall key, a heading or a line hint, never pasted whole. "Read
   the whole rulebook" costs tens of thousands of tokens before any work starts. The agent pulls exactly what it
   needs with `recall`, `lookup` or `search`.
4. **Scope:** the files or modules it owns, what it must not touch, and what it is safe to do without asking.
5. **Verification:** the exact command that proves the work, and the numbers it must match.
6. **Journal and limits:** "Journal to lane `<id>`. From your handoff point, wrap up at the next clean stopping
   point and write your handoff; you must hand off by your hard limit."
7. **The report:** hand back the digest form. Lead with the outcome (PR, branch, merge, verdict) in the
   first lines, then the numbers, every open question under a `## Questions` heading, and every risk (a denied
   action, an unverified claim, a failure left open). File anything longer (the full notes, tables, logs) as a
   journal entry and cite its key. The report stays in the orchestrator's context for every later turn, so a
   line that changes nothing it does is a cost. A report over 28,000 bytes is sent back once: atlas files it
   whole and asks for the digest form.

## Context limits

atlas reads each agent's real context size from its transcript and applies its kind's limits. The defaults on
a 1M window are a handoff point at 285k and a hard limit at 450k, and both scale with the window.

- **Past the handoff point,** the agent is told to wrap up: finish the step, commit, push, and write the
  handoff.
- **Past the hard limit** (in enforced mode), every tool except `handoff` is denied, and so is a SendMessage
  to that agent.

When such an agent finishes, atlas tells you to start a fresh agent from its handoff. Depending on the
`fresh` setting, it tells you to start it or to ask the user first. The fresh agent's brief names the lane and
says to start from `recall lane=<id>`.

The main session follows the same rule. Near your own limit, journal and hand off, and offer the user a
compaction point.

## Verify, then merge

- **Do not take a report's word for a result.** Run the verification yourself, or read its output: test
  counts, eval lines, the diff.
- **Review independently when it matters.** Give the reviewer the code and the claim, not your conclusion.
  Name reviewers by job (simplicity, cost), never by person.
- **File each review** as a `journal` entry `review` with meta `role:`, `pr:`, `head:` (the sha it read),
  `verdict:` and `report:`. With `orchestrate.reviews` set, `gh pr merge` waits for every role's review of the
  PR's current head.
- **Merge only what passed.** Then record the merge commit in the lane journal.

## Finish cleanly

When a lane completes:

- remove its worktree and its build target;
- close the lane with its handoff;
- say what moved, which questions wait on the user, and what is next.

Ask the user design questions as multiple choice, with the recommended option first.
