---
name: atlas
description: How to work with atlas, the project memory - recall past rulings and decisions before asking, journal to a lane, write handoffs, plan and start queued work by its key, write lean agent briefs with Kind and Lane lines, and keep state across context limits and compaction. Use when orchestrating or briefing agents, when recalling what was decided earlier, when an atlas cap, handoff or compaction message appears, or when a session is about to compact or clear; load it before your first journal or handoff.
---

# Working with atlas

atlas is the project's memory: rulings, decisions, lane journals, handoffs and the work queue, kept in a local
store across sessions, agents and compactions, beside an index of the project's notes, docs and code. Nothing
in it is lost when an agent ends; what an agent did not write down is.

This guide is the habits and the workflow. `reference.md` beside it lists every tool, op, argument, journal
entry kind, queue command and setting, generated from the same code as the tool schemas and `atlas://help`
(also `note op=help`). Delegating large work is the `orchestrate` skill. `recall`, `search`, `section` and
`grep` are loaded from the start; `lookup` (where an identifier is defined) and `symbol` (code definitions,
references and callers) are deferred like the write tools: load them by a tool search before the first call.

## The habits

1. **Recall before you ask.** Before asking the user something that may have been decided, and before
   starting work in a lane, ask atlas: `recall query=<words>` for rulings and decisions, `recall lane=<id>`
   for a lane's journal and handoff, `recall key=<key>` for one record whole. The hooks also inject what
   clears the relevance gate, so read what arrived before calling again.
2. **Journal as you go.** A decision, a measurement, a commit or an open question goes into your lane with
   `journal` when it happens, not at the end. One entry per fact, the headline on the first line.
3. **Hand off whenever work is left for later.** Write a `handoff` for your lane before your final answer
   whenever work is left for later, and from your handoff point on at the next clean stopping point. It is the
   only write allowed past the hard limit.
4. **Name work by its queue key.** Planned work is a `queued` entry with a key (`journal:LANE:NNNN`). Say the
   key, start it with a `Queue:` line in the brief, and mark it done; never describe it from memory.
5. **Put a `Lane:` line in every brief**, with `Kind:`, so the agent's start gets its lane's recall and its
   kind's limits. Declare each record the agent needs as a Gherkin tag, the full key every time
   (`@journal:LANE:0012`, pinned `@journal:LANE:0012#a1b2c3d4`), above Feature or above the
   Scenario or Spike it serves: the start lists each by key and title. Not `0012, 0013` after a key,
   nor `0140 onward`: name the few that bind.
6. **Write terse.** Journal, handoff and brief text comes back later as recall answers, so every word costs
   again on each read. Style for what you write into atlas and your replies: telegraphic, keys kept. Drop
   filler, preamble and restatement; fragments are fine. Keep every key, number, path, commit, error line and
   quoted ruling exact; a key stands in for its text.

atlas injects no rules of its own. The user's rules stay in their CLAUDE.md and memory files, which atlas
indexes and recalls like any other source.

## Lanes and sub-lanes

- A **lane** is one stream of work with an id, for example `PARSER-2`. It has no record of its own: it exists
  once a `journal` or `handoff` entry is written under it.
- Each agent works in exactly one lane. The next agent in the lane starts from `recall lane=<id>`, which
  gives the lane's journal and its newest handoff.
- A **sub-lane** (`PARSER-2/2`) is given when a second launch goes into a lane that running work holds. The
  agent is told so at its start; it journals and hands off under the sub-lane, and its records show in the
  parent lane's view too.
- **Declare a lane** before its first launch with a `brief` entry carrying the launch lines:

  ```text
  Kind: builder
  Lane: PARSER-2
  Priority: normal
  Uses: compile, worktree:parser-2
  ```

  atlas reads these lines from the first 40 lines of a launch prompt. It schedules on declared lines and only
  warns on what it infers from a prompt's words.

## Journal entries

`journal` takes a `lane`, an `entry` kind and the `text` (first line the headline); `meta` holds `<key>: <value>`
lines and `refs` holds record keys. The kinds:

- `brief`: the lane's work order and launch lines.
- `decision`: what was decided and by whom, in their words where a user ruled.
- `measurement`: a number with the machine state and the command that made it.
- `commit`: a commit, merge or PR, by hash or number.
- `question`: an open question; `answered` closes one, with `answers:<key>` in `refs`.
- `next`: a step owed. A checkpoint lists the `next` entries no `commit` has followed.
- `note`: anything else worth keeping.
- `queued`: planned work (see the queue below).
- `review`: a review for the merge gate, with meta `role:`, `pr:`, `head:`, `verdict:` and `report:`.
- `cap`, `checkpoint` and `idea` are written by atlas: a context-cap warning, a compaction or clear
  checkpoint, and an idea captured from the user's own words.

To correct an entry, `amend` its key with `expected_sha` (its sha, as recall shows it); the old version
stays readable. Durable facts that are not lane work (a user's preference, a standing rule) go to
`remember` with `kind` memory, rule or decision; `old` supersedes earlier records.

## The work queue and handoffs

- **Plan** with `atlas queue plan --lane L --title T` (or a `queued` entry over MCP, with meta lines
  `kind:`, `priority:`, `uses:`, `after:` and `order:`). `after:` lists conditions: a queue key, met when that
  item is done, or free text, met only when marked with `atlas queue met KEY CONDITION`.
- **List** with `atlas queue list`; `atlas queue next` proposes what can start now. An item with an unmet
  condition is never proposed.
- **Start** an item by launching its agent with `Queue: <key>` in the brief; that takes it off the queue.
  A launch refused at its kind's cap is queued too, and the refusal names its key: launch it again later
  with the same brief (`recall key=<key>` reads it) rather than retrying at once.
- **Finish** with `atlas queue done KEY`, or `atlas queue drop KEY` for work no longer wanted.
- A **handoff** holds everything the next agent needs: the state, the commits, where the evidence is, the
  corrected commands, the steps left in order and the open questions. The agent that wrote it hands back
  only the line naming its key; the next one starts fresh from it. Never resume an agent past its handoff.

## Context limits and compaction

- atlas reads each agent's real context from its transcript. The defaults on a 1M window are a handoff point
  at 285k and a hard limit at 450k, scaled to the model's window. Past the handoff point the agent is told to
  wrap up; past the hard limit (enforced mode) every tool but `handoff` is denied. Under an enforced limit a
  subagent past its handoff point is not compacted: atlas blocks its compaction, and it hands off.
- The main session is told when it passes its compact point to ask the user for `/compact`, and to
  recommend `/clear` instead when the last compaction left the session at `compact.clear_floor` or more.
  `atlas settings auto-compact` makes Claude Code's own automatic compaction fire at atlas's hard limit.
- Before a compaction, atlas asks once for a checkpoint and writes a `checkpoint` entry in the session's lane:
  lines of refs (lanes, open questions, owed steps, rulings), never restated text. A `/clear` writes one from
  the ending transcript. The next start injects its card.
- **Refs name exact text:** `key@hash` (a record version), `src:path:a-b@hash` (lines of a file) and the
  `turn:...@hash` and `compact:...@hash` keys the compaction card names (transcript turns and older segments;
  they resolve while `compact.enabled`, on by default, or `memory.transcripts` is on).
  `recall key=<ref>` returns that exact text. After a compaction atlas checks every ref in the summary and
  reports the changed and dangling ones.
- **Write a compaction summary as keys and deltas:** a ruling as one line with its key, the state with its
  journal key, commit or file and line, and only the few exact lines the next step needs. Never restate what
  one recall returns.

## Changing atlas itself

atlas's first job is memory. A change that cuts tokens (an answer's size, a hook floor, trimming,
compaction) ships only after the memory fidelity eval, before and after: `cargo xtask memory-eval` with
recall-check misses at 0, no fact lost on the question set and the work-queue checks matching, and
`cargo xtask rules-eval` for which ruling comes first. A change of behaviour updates this guide's prose;
`cargo xtask skill` regenerates `reference.md`, and a test fails when either drifts from the code.
