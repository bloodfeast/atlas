# Agent, build and safety rules

These rules come from the atlas rules template, imported by the project's CLAUDE.md.
Each is one line to follow and one line on why. Edit them to suit the project.

## Agents

- Keep to the per-kind agent caps that `atlas settings orchestrate` sets.
  Why: past the caps, agents contend for the machine and the user's attention.

- Wrap up at the context soft limit, and hand off before the hard limit.
  Why: a long context costs more every turn, and a forced compaction loses detail.

- Hand off to a fresh agent, never a resumed one.
  Why: a resumed agent carries its old context into every turn.

- Keep reviewers read-only: they report findings, and a builder makes the fix.
  Why: a reviewer that edits is reviewing its own work.

- Write briefs with a cheap model, then have a stronger model review each one once.
  Why: drafting is most of a brief's tokens, and one review catches the draft's misses.

## Worktrees and git

- Give each agent a worktree of its own, on a new branch.
  Why: two agents in one tree overwrite and commit each other's work.

- Never reuse a worktree while the agent that had it might be resumed.
  Why: a resumed agent commits into whatever its old tree now holds.

- Use `git -C <worktree>` on every git call, never a bare cd.
  Why: the shell's folder can reset between calls, and git then acts on the wrong repository.

- Commit each new file as soon as it exists.
  Why: an uncommitted file can be lost to another agent's checkout or cleanup.

## Builds

- Run one heavy compile at a time, with a capped job count.
  Why: parallel heavy compiles can exhaust memory and reset the machine.

- Run a long build or test as its own call, and read its log once when it ends.
  Why: polling costs a full turn each time, and a log read once costs it once.

## Safety

- Never read .env files or other secret files.
  Why: whatever enters the context can end up in a log, a commit or a reply.

- Never print credentials, tokens or email addresses.
  Why: printed output is kept in transcripts and can be shared.

- Never touch the Claude config folder under the user profile.
  Why: it holds the user's own settings, and a change there affects every project.

- Never merge as admin, bypass branch protection or skip hooks with --no-verify.
  Why: those checks are the user's gate, not an obstacle to route around.

- Push or merge only where the user has allowed it.
  Why: a push is public and a merge is hard to take back.
