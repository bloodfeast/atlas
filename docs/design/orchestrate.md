# Orchestrator mode

Status: design, first cut built (agent caps, per-kind context limits, the delegation skill). The SubagentStop
digest, the queue command and the unmanned mode are later phases (section 8).

Orchestrator mode is for a main Claude Code session that delegates its work to subagents. It has three parts:

1. **A skill** (`skills/orchestrate/SKILL.md`, shipped in the plugin). It teaches the session to delegate:
   - work runs in lanes;
   - briefs stay lean;
   - each agent states its kind;
   - agents hand off at a context limit;
   - the orchestrator verifies before it merges;
   - the next agent is always a fresh one.
2. **Agent caps.** The PreToolUse hook on the Agent tool enforces hard limits per kind on how many agents run at
   once: builders, reviewers and research. The limits are adjustable and default to 2 / 2 / 1. They are clamped
   to Claude Code's own limit on concurrent subagents.
3. **Context limits per kind.** These are enforced, not only taught:
   - a soft point, where the agent is told to wrap up and hand off at its next clean stopping point;
   - a hard limit, past which every tool except the handoff tool is denied.

   Both scale with the model's context window. Resuming an agent that is past its hard limit is refused too.

The hook never allows a call. It either denies, with a reason, or says nothing, so Claude Code's permission
mode and classifier still see every call.

## 1. What the hook can see

Facts measured in `atlas-c.md` section 1, plus Claude Code's documentation (hooks, sub-agents and context-window
pages, read 2026-11-04):

| Need | Source |
|---|---|
| A launch, before it happens | PreToolUse with `tool_name` `Agent` (or `Task` on older versions). It carries `tool_use_id`, and its `tool_input` holds `subagent_type`, `description`, `prompt`, `model`, `isolation` and `name` |
| The agents running now | `<session transcript without .jsonl>/subagents/agent-<id>.meta.json`, which Claude Code writes when an agent launches. It holds `agentType`, `description`, `toolUseId` and `model`, and gets `"finished": true` once the agent is done. Nested agents write theirs to the same folder, so one count covers the whole session |
| An agent's brief | The first `user` record of `subagents/agent-<id>.jsonl` |
| Which agent a tool call belongs to | `agent_id` and `agent_type` in every PreToolUse a subagent makes. In those calls, `transcript_path` is the parent's transcript |
| An agent's live context size | The last assistant `usage` in its own transcript: input + cache creation + cache read + output. This equals Claude Code's own figure (`atlas-c.md` 1.2), and the existing cap already reads it |
| A finish | The main session's `<task-notification>` (UserPromptSubmit) with the agent's `<task-id>`. `is_interim` already tells a real finish from a stop that waits on background work |
| Claude Code's concurrent-subagent limit | 20 per session by default. Past it the spawn fails. The `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS` environment variable changes it (2.1.217+); the MCP server inherits Claude Code's environment |
| The model's window | No hook field carries it. The signals are `CLAUDE_CODE_DISABLE_1M_CONTEXT=1` (which means 200k) and `CLAUDE_CODE_AUTO_COMPACT_WINDOW` or `autoCompactWindow` (where the agent will compact) |

The meta file is an undocumented internal format. Section 4 covers what happens if it changes.

### 1.1 Counting

On an Agent PreToolUse, while caps are on:

1. **Classify the launch** into a kind (section 2).
2. **Count the agents of each kind that are running.** An agent counts when all of these hold:
   - its meta file has no `"finished": true`;
   - no real finish notification has been seen for it since its transcript last changed;
   - its transcript or meta changed after this server process started. Background agents do not outlive the
     Claude Code process that started them, so an unfinished meta from an earlier process is dead;
   - its transcript or meta changed within `stale_minutes`. This is the only liveness rule left for the command
     hook transport, which has no resident start time.
3. **Add admitted launches whose agent has not started yet.** Each admitted launch leaves a pending record, so
   two Agent calls in one message cannot both slip under the cap. The record ends when its agent starts: the
   SubagentStart event pairs it (by the meta's `toolUseId` when the record has the launch's `tool_use_id`, else
   the oldest record), and so does the agent's meta file once seen in any state, running or finished, should
   the start event be lost. A record whose agent never starts (a launch refused after atlas answered, by a
   permission prompt or another hook) counts for 2 minutes. Before 0.3.0 the record was matched only by
   `tool_use_id` against unfinished agents: a hook input without the id counted every agent twice, and an agent
   that finished inside the 2 minutes kept its slot (journal ATLAS-V03:0137). `atlas doctor` and a line at a
   session's first prompt flag a live atlas hook missing from, or whose input fields differ from,
   `hooks/hooks.json`.
4. **Deny the launch** when the count for its kind is at its cap, or when all running agents together are at
   Claude Code's limit. The reason names the running agents of that kind (description and age) and tells the
   session to queue this launch and start it when one of them finishes. It also names the setting that changes
   the cap.

## 2. Kinds

Each launch gets one kind: `builder`, `reviewer`, `research` or `none` (not counted). Rules, first match wins:

1. **A `Kind:` line in the brief.** It must be one of the first 40 lines, for example `Kind: reviewer`. The
   skill makes the orchestrator write it.
2. **`orchestrate.types`**, a map from `subagent_type` to kind. The default maps `Explore`, `Plan` and
   `claude-code-guide` to research and `statusline-setup` to none. Users add their own agent types, such as a
   reviewer persona type.
3. **`isolation: "worktree"`** makes the agent a builder.
4. **`orchestrate.default_kind`** applies otherwise. The default is builder: an unmarked general agent is
   counted against the tightest slot that compiles.

The same rules classify a running agent from its meta file (`agentType`, `description`) and its brief. The
result is cached by agent id.

## 3. Settings

Everything lives in atlas's configuration (`orchestrate`, plus the existing `cap` section). The main levers are
also in `atlas settings`.

```json
"orchestrate": {
  "enabled": false,
  "window": 0,
  "stale_minutes": 360,
  "default_kind": "builder",
  "mode": "enforced",
  "fresh": "auto",
  "types": {"Explore": "research", "Plan": "research"},
  "kinds": {
    "builder":  {"max": 2, "limit": 450000, "handoff": 285000, "mode": "enforced", "fresh": "auto"},
    "reviewer": {"max": 2},
    "research": {"max": 1, "handoff_pct": 60}
  },
  "resources": {"gpu": 1, "compile": 1},
  "aging_minutes": 60,
  "reviews": {"roles": ["simplicity", "cost"], "mode": "enforced"}
}
```

- **`resources`** and **`aging_minutes`** belong to the work queue's lanes: resource capacities (0 never
  serializes) and the minutes of waiting that raise a queued item one priority class. See `queue-lanes.md`.
- **`reviews`** is the required-reviews merge gate: a `gh pr merge` needs a passing review of the PR's head from
  each role. No roles (the default) turns it off; the default mode is advisory. See `reviews.md`.

- **`enabled`** turns the agent caps on. Context limits do not depend on it: the context cap already ran by
  default before this mode existed.
- **`max`** is clamped to Claude Code's limit (`CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS`, else 20). A value of 0
  means no agent of that kind is launched.
- **`window`**: 0 infers the window, in this order:
  1. `CLAUDE_CODE_AUTO_COMPACT_WINDOW`;
  2. 200,000 when `CLAUDE_CODE_DISABLE_1M_CONTEXT` is set;
  3. otherwise 1,000,000.

  It can be set per kind (`kinds.X.window`).
- **`limit`** is the hard limit. In order: `kinds.X.limit`, else `cap.block_tokens`, else 450,000 scaled by the
  window (450,000 x window / 1,000,000).
- **`handoff`** is the soft point. In order: `kinds.X.handoff`, else `limit x handoff_pct / 100`, else
  `cap.warn_tokens`, else 285,000 scaled by the window. It is clamped to at most `limit`.
- **`mode`** (per kind, else `orchestrate.mode`, default `enforced`):
  - `enforced`: past the hard limit every tool except `handoff` and `ToolSearch` is denied, and so is a
    SendMessage to the agent;
  - `advisory`: the agent and the orchestrator are told, and nothing is denied;
  - `off`: nothing.
- **`fresh`** (per kind, else `orchestrate.fresh`, default `auto`): `auto` or `ask`. When an agent that passed its soft point finishes, the orchestrator is told
  either to start a fresh agent from the lane's handoff (`auto`) or to ask the user first (`ask`).
- **`cap.warn_every`** (default 25,000) still spaces repeated soft warnings. `cap.main_session` still applies the
  `cap` limits to the main session.

### 3.1 Asking

Orchestrated mode is never turned on silently, and never defaulted without a question (Q-OR1). It is asked
in three places, the way the other setup options are:

- **`atlas --setup`** runs `atlas settings offer`, which asks after the Claude Code settings: *Work in
  orchestrated mode? [y/N]*. On yes it asks each option in turn, with the value in effect in brackets (Enter
  keeps it): builders, reviewers and research agents at once; context limits enforced, advisory or off; the
  handoff point and the hard limit (skipped when limits are off); and whether the fresh agent after a handoff
  starts on its own (`auto`) or after asking the user (`ask`). A value that fails its check is asked again.
- **The first session** gets one SessionStart line, each attended new session until answered and only while `orchestrate.enabled` is
  unset, telling Claude to ask the user the same questions as multiple choice and to run
  `atlas settings orchestrate --yes --builders N ...` or `atlas settings orchestrate --no`.
- **`atlas settings orchestrate`** asks again at any time. Without a terminal it lists the options and writes
  nothing; `--yes` with flags applies answers, checked as the questions check them; `--no` declines.

A yes stores `orchestrate.enabled`, the three caps, `orchestrate.mode` and `orchestrate.fresh` in one
configuration change (one backup, one `atlas settings undo`). The handoff point and the hard limit go to
`cap.warn_tokens` and `cap.block_tokens` only when a number is given, so their defaults keep scaling to the
window (Q-OS2). A no writes nothing and is recorded in the offer state; a no while the caps are on turns them
off (Q-OS1).

The defaults change from 450k / 500k to 285k / 450k on a 1M window. A configuration that sets
`cap.warn_tokens` or `cap.block_tokens` keeps its values, because they still apply to every kind without a
per-kind value. The CHANGELOG says so.

## 4. Failing safe

**The caps fail open.** The hook only ever denies, and only when it has positive evidence: a readable meta
folder and a count at or over the cap. In every other case it gives no decision and the call goes through, and
an error is written to stderr. Those cases are:

- an I/O error;
- a missing `transcript_path`;
- a meta folder that does not exist yet;
- a malformed meta file, which is skipped;
- an unknown kind name in the configuration, which the parser refuses at load.

A cap that wrongly blocks every launch would stop the user's whole session, while a cap that wrongly lets one
through costs one extra agent.

Guards against a stuck count:

- the server-start rule and `stale_minutes`, so dead agents expire;
- a real finish notification counts as finished, even if the meta file never says so;
- the deny reason lists what it counted, so a wrong count shows at once;
- `orchestrate.enabled: false` is the off switch.

**Context limits** read the same usage record the existing cap reads. With no usage record they do nothing.

## 5. How it composes with hooks.json

It needs no new hook entries:

- **Agent caps and the SendMessage refusal** ride the existing `PreToolUse` `*` entry, which already passes
  `tool_use_id`, `agent_id`, `transcript_path` and `tool_input`.
- **The fresh-agent line** rides the existing UserPromptSubmit notification path.

Order inside `Hooks::try_handle`, for PreToolUse:

1. **The context limit** for the calling agent. It may deny.
2. **The agent cap**, for Agent or Task calls. It may deny. A denied launch does not queue a brief, so a recall
   is not handed to the wrong agent at the next SubagentStart.
3. **Routing**, only when nothing denied.

Hooks the user adds in their own settings run beside atlas's, and Claude Code applies the most restrictive
decision. A user hook that denies an Agent call also wins.

## 6. The skill

The skill is `skills/orchestrate/SKILL.md`, which a plugin loads automatically. It is short (under 2k tokens)
and covers:

- when to delegate;
- lanes and journals;
- the brief shape: goal, the rulings by key or line, scope and owned files, a `Kind:` line, the journal lane,
  the context-limit sentence, and a verification command;
- never pasting a rulebook;
- caps, and what to do on a cap denial (queue, do not retry);
- handing off at the context limit, then a fresh agent;
- verify-then-merge;
- purging finished work.

It uses no project names.

## 7. Tests

- **Cap refusal:** at the cap the launch is denied; under it nothing is said; a finished meta, a stale meta and
  a meta older than the server do not count; a pending launch counts until its agent starts (SubagentStart or
  its meta in any state), with and without `tool_use_id`; the total limit is applied.
- **Classification precedence.**
- **Soft and hard context points:** the soft warning, the hard deny, handoff passing, and advisory and off
  modes; window scaling; SendMessage refused past the hard limit; the fresh-agent line on finish.
- **Settings resolution and clamping:** a max above Claude Code's limit, a handoff above the limit,
  `handoff_pct`, legacy `cap.*` fallbacks, unknown kinds refused.

## 8. Later phases

These are in the ruling but not in this cut:

- **The SubagentStop digest.** It files the full report into the lane journal and hands the orchestrator a
  digest: the outcome, open questions and risks. The ruling gates it on an eval, and it needs a SubagentStop
  hook entry. The eval comes first: `cargo xtask report-eval` scores `atlas::report::digest` on a private
  corpus of real subagent reports whose needed facts (outcome, numbers, questions, risks) are labelled by hand,
  and gates later changes on the kept-fact count, as `digest-eval` gates run digests. A hook cannot rewrite the
  report the orchestrator receives (the task-notification or the hand-back text arrives whole), so a digest
  saves tokens only if the agent hands back the digest itself. Rulings Q-SD1 and Q-SD2 in section 9.

  The first measurement (30 reports: 15 builder, 8 reviewer, 7 research; 284 facts: 80 outcome, 56 number,
  51 question, 97 risk). The pieces that hold a fact are 15.9% of the report tokens, so a perfect picker would
  save 84%. The mechanical digest is far from that:

  | Budget | Facts kept | Whole cases | Digest tokens / report |
  |---|---|---|---|
  | a third, 700 to 2,400 B (first cut) | 139/284 | 0/30 | 27.1% |
  | the same, tiered (opening, questions first) | 158/284 | 0/30 | 27.7% |
  | 4,000 B | 231/284 | 14/30 | 48.2% |
  | 8,000 B | 273/284 | 25/30 | 75.9% |
  | 12,000 B | 282/284 | 29/30 | 87.2% |
  | whole up to 14,000 B, else max(14,000, len/8) | 284/284 | 30/30 | 91.5% |

  Under Q-SD2 only the last row ships: reports up to 14,000 bytes pass whole, and the saving (8.5%) comes from
  the one longer report. The tokens are saved by the skill's digest-form report, which the agent writes knowing
  what matters; the hook's mechanical digest is a backstop for long reports only. The gate rows are
  `baseline-51fee57.rows` beside the corpus.

  The backstop is wired (Q-SD3). In orchestrated mode, a report over 14,000 bytes is sent back once: at the
  SubagentStop hook (`decision: block`, unless `stop_hook_active`), or, when the report is handed back
  through a hand-back tool, at that tool's PreToolUse (a deny). atlas files the report whole as a journal note
  in the agent's lane (`reports` when no lane is known), and the reason asks for the digest form, cites the
  key, and attaches the mechanical draft (`digest(report, budget(len))`, so the draft keeps every labelled fact
  on the corpus). A report within the bound, a second stop, the caps off, or a filing error lets it through.
- **A queue command** for what comes next. Built; section 8.1.
- **The unmanned mode,** with questions queued for the user's return.
- **REFS-BRIEF:** briefs written as refs. It ships only if a paired test passes. Until then the skill teaches
  lean prose briefs that name rulings by key.
- **ATLAS-GOALS:** goal check-ins from the work queue. The research into what a hook can see of `/goal` is in
  `goals.md`: a check-in reaches atlas's UserPromptSubmit hook, and only the user or Claude Code's evaluator
  can end a goal.

### 8.1 The launch queue

A launch the agent cap refuses is kept, so the orchestrator does not have to hold it in its own context, and a
command hook (a fresh process per event) or a compaction loses nothing:

- **Filed by the hook.** On the deny, the PreToolUse hook writes a `queued` journal entry in the lane the brief
  names (the same rule the SubagentStart recall uses), else in lane `queue`. The headline is the description;
  the meta holds the kind, the agent type, the session and a hash of the brief; the body is the brief. The deny
  reason names the key. A retry of the same launch (same description and brief) is not filed twice.
- **Listed on a finish.** The task-notification of a finished agent carries one line while launches wait: how
  many, and the oldest three with key, kind and description.
- **Left on start.** An admitted launch with the same description and brief retires its entry (reason
  `started`). `atlas queue drop KEY` retires one without starting it (reason `dropped from the queue`).
- **The command.** `atlas queue` lists what waits, oldest first (key, kind, lane, age, description);
  `atlas queue show KEY` prints one with its brief; `recall {key}` reads it too.
- **Off with the caps.** With `orchestrate.enabled` false nothing is filed and no line is added. Every queue
  error is written to stderr and changes no decision.

atlas never starts a queued launch itself: a hook cannot launch an agent, and the orchestrator decides what
runs next (Q-QU2).

## 9. Rulings

- **Q-OR1.** Not a silent default. `atlas --setup` (through `atlas settings offer`) and the first session ask the
  user whether to work in orchestrated mode, and on yes ask its options. Section 3.1.
- **Q-OR2.** The window comes from the `window` setting, else `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, else 200k when
  `CLAUDE_CODE_DISABLE_1M_CONTEXT` is set, else 1M.
- **Q-OR3.** An unclassified agent is a builder.
- **Q-OR4.** Past the handoff point a SendMessage passes with a wrap-up-only note; past the hard limit it is
  refused.
- **Q-OR5.** `atlas settings` shows the 7 global levers; per-kind values live in the file only.
- **Q-OS1.** A no writes nothing in the configuration; the offer state records it, so it is not asked again. A
  no while the caps are on turns them off.
- **Q-OS2.** Enter stores nothing for the handoff point and the hard limit, so the defaults keep scaling to the
  window; only a typed number is stored.
- **Q-QU1.** The hook files a refused launch in the queue, at the deny, with the brief whole.
- **Q-QU2.** When a slot frees, the finish notification lists what waits and the orchestrator picks the next.
- **Q-QU3.** A started launch is matched to its queue entry by the same description and brief; a changed brief
  is a new launch.
- **Q-SD1.** Both. The orchestrate skill teaches agents to hand back the digest form (outcome, numbers,
  questions, risks; the full report filed one recall away), and the SubagentStop hook blocks the stop once, as
  a backstop, only when the report is not already a digest.
- **Q-SD2.** The digest ships only if every labelled fact is kept on the whole corpus (284/284).
- **Q-SD3.** Wire the backstop now. It fires only for reports over 14,000 bytes, blocks the stop once, and
  asks the agent to hand back its own digest form with atlas's draft attached. It must pass the report gate.
