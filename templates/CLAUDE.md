# Project rules

These rules come from the atlas rules template. Each is one line to follow and one
line on why. Edit them to suit the project; atlas never rewrites this file.

## Token-saving rules

Every token in the context is paid for again on every later turn. These rules keep
the context small.

- Write compaction summaries as keys and deltas, never restated content, under a hard character cap.
  Why: restated output costs its length every later turn; a key is one recall away.

- Recall from atlas before asking the user or re-reading a file.
  Why: a recorded answer costs one recall, not a question or a full read.

- Keep briefs lean: name the record keys an agent needs, and never tell it to read the rulebook.
  Why: a rulebook read in full is charged to every agent that reads it.

- Have a cheap model write each brief, and a stronger model review it once before launch.
  Why: drafting is most of a brief's tokens, and one review catches the draft's misses.

- Make fewer, larger shell calls: batch independent reads and searches into one call.
  Why: every call re-reads the whole context.

- Never sleep-poll: run a long job as its own call, or in the background, and read its output once.
  Why: every poll is a full turn.

- Start a fresh agent at each handoff; never resume one.
  Why: a resumed agent carries its old context into every turn.

- Purge finished worktrees and build folders without asking.
  Why: stale copies cost disk, and get searched and read by mistake.

<!-- atlas:compact-block -->

## Habits

- Journal decisions and measurements to the lane as they happen.
  Why: a journal entry is one recall for the next agent; a memory of it is nothing.

- Write a handoff before stopping, naming the state, the evidence and the steps left.
  Why: the next agent starts from the handoff, not from a rebuilt context.

- Ask the user design questions as multiple choice, with a recommended default.
  Why: a choice is answered in seconds, and the default shows your reasoning.

## Agent, build and safety rules

The rules for agents, builds and safety are in the file imported on the last line.

@.claude/atlas-rules.md
