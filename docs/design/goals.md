# Goal check-ins from the work queue

Status: research done, nothing built. This note records what a hook can see of Claude Code's `/goal`, then the
design it allows and the questions it leaves (section 4).

The aim, from the ruling:

1. **Check-ins.** When a `/goal` check-in fires, atlas supplies a digest from the work queue: what moved since the
   last check-in (merges, handoffs, measurements), what each lane is on, and the blockers and questions waiting
   for the user. The orchestrator reasons from that instead of re-reading.
2. **Parts.** A goal's parts map to lanes or work items. When the work queue shows a part done, atlas marks it
   done; when every part is done, the goal is cleared, or offered for clearing.

## 1. What Claude Code documents

From the goal page (https://code.claude.com/docs/en/goal.md) and the hooks page
(https://code.claude.com/docs/en/hooks.md), read 2026-11-04:

| Question | Documented answer |
|---|---|
| What a goal is | "`/goal` is a wrapper around a session-scoped prompt-based Stop hook." One goal per session; a new `/goal` replaces it |
| Who decides it is met | A small fast model, after each turn that ends with no background work running: not yet met, met (the goal is cleared) or impossible (cleared, with a failed entry). It "does not call tools, so it can only judge what Claude has already surfaced in the conversation" |
| Clearing | `/goal clear` (also `stop`, `off`, `reset`, `none`, `cancel`) or `/clear`. Run by the user. No tool the model can call is documented |
| Check-ins | Due once background work has kept the goal waiting for `CLAUDE_CODE_GOAL_CHECKIN_MINUTES` (default 30, doubling up to four times that). Delivered at the end of the next turn, or as a turn Claude Code starts on its own. Check-ins from 2.1.234 |
| Goal fields in hook input | None. No hook event is specific to goals |
| Hook output | `additionalContext` on UserPromptSubmit, Stop and SubagentStop reaches Claude's context. Whether the evaluator sees it is not documented |

## 2. What the transcript shows

Measured on this machine's session transcripts (Claude Code 2.1.316). These are internal formats and may change.

| Event | Records |
|---|---|
| A goal is set | A `queue-operation` enqueue with content `Goal set: <condition>`, then an `attachment` of type `goal_status` with `met: false, sentinel: true, condition`, and an `isMeta` user record: *A session-scoped Stop hook is now active with condition: "..."* |
| An evaluation | An `isMeta` user record `Stop hook feedback:\n[<condition>]: <reason>`, an `attachment` `goal_status` with `met`, `condition` and `reason` (no `sentinel`), and a `system` record of subtype `stop_hook_summary` whose `hookInfos[].promptText` is the condition |
| The goal ends | A `goal_status` with `met: true, sentinel: true` |
| A check-in | A `queue-operation` enqueue, then a `user` record whose content is `<task-notification>\n<summary>Goal check-in: background work still running</summary>\n</task-notification>` followed by a system reminder: *Goal check-in: «<condition>» is still active, and evaluation has been deferred for N min because background work is still running:* and one line per running task (`<id> · shell · <command>` or `<id> · subagent · <description>`), then *Check on their progress ...* |
| Hooks on a check-in | **UserPromptSubmit fires**, with the check-in text as `prompt` (a `hook_success` attachment for UserPromptSubmit follows the record) |

So:

- **A check-in is visible to atlas's UserPromptSubmit hook**, which already parses task-notifications. Its
  summary line `Goal check-in:` marks it, and the `«...»` quote carries the condition. Its `additionalContext`
  goes into the turn that answers the check-in. Section 1 of the ruling can be built on this.
- **The goal's condition is readable** from the transcript (the newest `goal_status` attachment), and so is
  whether it is active (the newest sentinel's `met`) and the evaluator's last reason.
- **atlas cannot clear a goal or mark a part done.** Only the user's `/goal clear` or the evaluator's verdict
  ends it, and the goal is one condition string with no parts. What atlas can do is put the evidence in the
  conversation (the evaluator judges only what is in it), and offer the user the clear when the work queue says
  every part is done.

## 3. Design this allows

1. **The check-in digest.** On a UserPromptSubmit whose prompt is a goal check-in, atlas answers with a digest
   of the work queue since the previous check-in (or since the goal was set): merges, handoffs and measurements
   by lane, each lane's current step, and open questions. Records are cited by key, one recall away. The
   running-task list in the check-in itself is matched against the agents' lanes, so each running item gets its
   lane's latest entry. A budget caps it, like the other hook injections.
2. **Goal parts.** The condition is split into parts (section 4, Q-GO2). Each part maps to lanes or work items.
   When every mapped lane is closed with a handoff and its PR is merged and verified, the part is done.
3. **Clearing.** When every part is done, the digest says so, with the evidence (PRs, merges, verify lines), so
   the evaluator can judge the goal met, and tells the session to ask the user whether to clear it. atlas never
   claims the goal is met by itself.
4. **Measured first.** Like the other hook features, the digest is judged on tokens (the turn that answers a
   check-in, with and without it) and accuracy (the session's next action), on recorded check-ins.

## 4. Rulings

Brandon ruled on 2026-11-04:

- **Q-GO1.** The check-in digest goes in `additionalContext` on the check-in's UserPromptSubmit only, where the
  hook already fires and the text lands in the turn that answers the check-in. Nothing is added on Stop.
- **Q-GO2.** The orchestrator writes a goal's parts: a `goal` journal entry lists the parts and the lane each
  maps to, and atlas tracks them from there. atlas does not split the condition itself.
- **Q-GO3.** When every part is done, the digest says so with the evidence and tells the session to ask the
  user whether to run `/goal clear`. Only the user or the evaluator ends a goal.
