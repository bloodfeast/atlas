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

An agent counts until it finishes: its meta file says so, its task notification arrives, or its SubagentStop
fires. One that died with none of these (a full disk) is freed by `atlas queue release AGENT`, which records
it finished in atlas's own file beside the meta file and refuses an agent that wrote in the last 5 minutes
unless `--force`. One whose transcript ends in a disk-full tool error stops counting by itself after
`orchestrate.disk_full_minutes` (default 10).

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

## Working lean

Every turn re-reads an agent's whole context, so the `orchestrate` skill's brief has a **Work lean** step
(step 7) that the orchestrator puts in every brief: find with atlas's `symbol`, `lookup`, `grep` and
`section` before reading, then read only the lines around the hit; small edits with the smallest unique
anchor; logs to a file, with the tail and a failure grep read and the path cited; images downscaled or
cropped before reading; a brief or review read once in full; shared scratch files prefixed with the lane or
PR; new work stopped before the handoff point. The `atlas` skill keeps the atlas-specific part beside its
context limits.

The brief's **Journal and limits** step (step 6) asks for `pr:N head:SHA` when a PR opens and on each head
move, and, before an agent ends a turn to wait on its own background job, a `note` whose first line is
`waiting: <job> (<worktree or branch>)`. A completion whose own newest entry is that note, or one without
the brief's report items, is parked: the orchestrator leaves it alone and waits for the next notification on
the same task id. An entry is the agent's own when it names the agent's worktree or branch; on a lane no
other agent shares, any entry since its launch is. The compaction card keeps such an agent on its
`agents:` line as `parked (own job running)` by the same rule.

## Reports and SubagentStop digests

A subagent's report stays in the orchestrator's context for every later turn, so the skill asks for the
digest form: the outcome first, then the numbers, the open questions and the risks, with anything longer
filed as a journal entry and cited by key. In orchestrated mode the SubagentStop hook sends a report over
28,000 bytes back once: atlas files it whole in the agent's lane and asks for the digest. A report within the bound, or a second stop, goes through. When an agent finishes without
having written a journal entry, the task notification says so.

## Worktree gc

The worktree ownership ledger (`atlas worktrees`) knows which lane owns which worktree; `atlas worktrees gc` uses it
to clean up after finished work, never losing any. It looks only at open ledger rows, never at a main checkout.

A worktree is **stale** when its HEAD is a commit of its own (committed after its ledger row was made) that is in
the remote default branch (`origin/HEAD`, else `origin/main` or `origin/master`) and is not that branch's tip, or a
merged or closed PR event note names its head and that head is its own commit (made after the worktree); and it is
not in use. A worktree with no commits of its own is new,
not done, however far the default branch has moved past it. A worktree is **in use** when a running agent holds
its lane or declares `worktree:<name>`, a waiting queue item does, or anything in it, its linked build folders, its
git folder or its ledger row changed within the quiet period (`worktrees.quiet_minutes`, 2 hours by default),
which also covers a main session working in it and a row whose lane is unknown. When the running agents cannot
all be read (the launch records cannot be, or any session they name cannot be), nothing is stale. The cheap tests
go first (held, the ledger row's times, merged or new, git status) and files are walked last, only for a worktree
gc would touch; build output is looked at two folders deep, and the walk stops at the first newer file. The
in-use check runs again, with the running agents and the waiting queue read again, just before each worktree is
touched.

A worktree **holds work**, and is never removed, when any of these is true: a tracked file is modified or deleted;
anything is staged (intent-to-add included); a file is untracked and not ignored; a file is ignored outside the
build folders (the worktree's `target_dirs`) and not listed in `worktrees.disposable` (default `target/` at the
top of the tree only), since `git worktree remove` would delete it unasked; git reports a status entry of any other kind; a commit is in no remote-tracking ref (not pushed, no
upstream, no remote); a merge, rebase, cherry-pick, revert or bisect is under way; the worktree is locked
(`git worktree lock`); a stash entry was made on its branch; any of these checks fails or times out (unknown
counts as work); or the path is not a linked worktree of the repository the ledger names. The checks run again
just before each removal, which is `git worktree remove` without `--force`: when git refuses, the refusal is
reported and nothing is forced.

Every delete gc makes (a worktree, its in-tree `target/`, a linked build folder) goes through one guard, which
walks what it is about to delete immediately before, entering no link or junction, and refuses on a checkout
(`.git`, `.hg`, `.jj` or `.svn`, or a folder shaped as a bare repository, at any depth), a secret (a denied name,
`.envrc`, or a name that is not UTF-8) or a folder it cannot read. A worktree's walk covers the whole tree, its
ignored and build folders included, and passes over its own `.git` file and the files git tracks. The plan makes
the same walk, only for a worktree it would delete from, and lists a refusal as work; the hourly notify-only run
makes no quiet-period or guard walk; it only sums sizes, up to `worktrees.notify_gb`, an upper bound (a tree the
removal would refuse is still counted). `worktrees.dirty` says what a stale worktree that holds work gets
([[Configuration reference]]): reported (the default), its build output deleted, or its work committed, pushed
and verified on the remote before removal. Commit-and-push refuses, and leaves the work reported, when an
untracked file is over 10 MB, has a denied name (`.env`, key files), or holds a line atlas's redaction rules or the
leak denylist flag, or when a line the tracked files gain is flagged so, or those changes are over 10 MB. One
`--apply` or `--auto` run goes at a time: a second finds the run lock beside the store held (the operating
system's file lock, which goes with a run that dies) and does nothing.

With a worktree, its linked build folder (`worktrees.target_dirs`) goes, unless the guard refuses it, or it lies
inside any git checkout (unless it carries a `CACHEDIR.TAG` with the generic signature cargo writes and that repository tracks nothing in it, as with a
build folder in an untracked folder of a workspace that is itself a repository), or is, holds or lies in a checkout (a
ledger worktree, a main checkout or a listed worktree): then it is refused and
reported. Then the Docker objects
`cargo xtask linux-test` made for it: the `atlas-linux-target-<hash>` volume and the stopped
`atlas-linux-test-<hash>` container, `<hash>` from the checkout's path. atlas removes only those, only for a
checkout that is gone, and never while a container runs on them. Every other container and volume is listed,
never touched, with what Docker can verify: a container's state and when it stopped ("exited 5 weeks ago"), a
volume with the containers that use it, and a volume no container uses as "unreferenced, last activity
unknown". Without Docker, or with it stopped, that part is skipped quietly. Docker Desktop's disk image does not
shrink when volumes go: compacting it is your step.

```text
atlas worktrees gc           the plan, with sizes; changes nothing
atlas worktrees gc --apply   the plan, carried out
```

Each line of the report starts with `remove`, `work` (holds work, left alone), `target` (build output only),
`commit` (saved, then removed) or `keep` (not stale), then the path, lane, branch and reason; then the Docker
lines and the reclaimable total. With `worktrees.auto_gc` on, the hook runs it by itself; off, session start says
one line when the stale space passes `worktrees.notify_gb`.

## An example

A project `my-app`, with orchestrated mode on:

1. The orchestrator writes the `brief` entry for `FEATURE-1` with the four lines above, then launches a
   builder whose prompt starts with the same lines.
2. A second builder in `FEATURE-1` is denied and queued while the first holds the lane; a builder in
   `FEATURE-2` that also uses `compile` waits only when the compile slots are full.
3. When the builder opens its PR, two reviewers launch with `Kind: reviewer`, `Lane: FEATURE-1` and
   `Uses: read-only`. They hold no lane, so they run beside the builder, and they wait only for
   reviewer slots.
