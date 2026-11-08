# Required reviews before merge

Reviews lapsed for a week, and about 30 PRs merged with none. atlas holds the line at the merge:

- a reviewer's report is filed as a lane journal entry with its role, the PR, the head sha it read and its
  verdict;
- PreToolUse on `gh pr merge N` denies the merge (enforced) or warns (advisory) and names what is missing:
  `cost review for head abc1234`.

Role names come from the user's configuration and name a job (`simplicity`, `cost`), never a person.

## 1. Configuration

```json
"orchestrate": { "reviews": { "roles": ["simplicity", "cost"], "mode": "enforced" } }
```

- `roles`: the review roles every PR needs. The default is none, which turns the gate off: a public user has no
  reviewers until they name some. Matched without case.
- `mode`: `enforced` denies, `advisory` warns, `off` does nothing. The default is `advisory`.

## 2. The review entry (Q-RV2 (a))

A reviewer, or the orchestrator for it, files:

```text
journal lane=L entry=review text="cost review of PR #99: MERGE WITH FOLLOW-UPS"
meta: role: cost
      pr: 99
      head: 6e828cc
      verdict: merge with follow-ups
      report: Z:/scratch/reviews/cost_pr99.md
```

The `journal` tool refuses a `review` entry without `role:`, `pr:`, `head:` (7 or more hex digits) and
`verdict:`. `role:` may list several roles, comma-separated. Each verdict part must be `pass`, `merge`,
`merge with follow-ups` or `request changes` (any case, `-` or `_` for spaces, an optional `(ROLE)`), so a word
such as `reject` or `do not merge` is refused at filing instead of being read as a pass.

The gate also reads the `note` entries filed before the kind existed, which carry the same lines with
`review: ROLES` for `role:`:

```text
review: simplicity, cost
pr: 99
head: d84126e
verdict: request-changes (simplicity), merge-with-follow-ups (cost)
```

- `pr:` and `head:` may list several. A review reads PR N at head H when N is listed and a listed head is a
  prefix of H.
- `verdict:` is split on commas. A part ending in `(ROLE)` is that role's verdict; a part with no role in
  parentheses (`pass-with-fixes (both)`) applies to every role the review names.
- Every verdict passes except a request for changes (it says "request" and "change"). This loose read is for
  the legacy notes only; `review` entries are checked against the four verdicts when filed.

## 3. The gate

On PreToolUse for Bash and PowerShell, atlas reads each `gh pr merge` in the command: the PR selector (a number,
URL or branch; none means the current branch's PR) and `--repo`/`-R`/`-R=`. Quoted text is one word, so a commit
subject or an `echo 'gh pr merge 3'` is not read as a merge. A redirection (`2>&1`, `> out.txt`) is never the
selector.

1. A numbered PR that some role has no review of at all is reported as `ROLE review` straight from the store,
   without `gh`: that is the likeliest denial, and the head cannot change it.
2. Otherwise the PR's number and head come from `gh pr view SELECTOR --json number,headRefOid`, run in the event's
   folder (Q-RV1 (a)). It costs about half a second per merge attempt (measured 487-639 ms), and only it catches a
   review of a stale head. The gate waits 5 s for it; a `gh` that does not answer is killed and the PR is
   reported as one that cannot be checked.
3. The reviews: one query over active journal entries of kind `review` and notes with `review:` meta.
4. For each role, the reviews that read this PR at this head:
   - any of them requests changes: `ROLE review requested changes at head SHA`. Only a review of a newer
     head that passes clears it (Q-RV3 (a)), since a pass of the same head does not undo the request;
   - none: `ROLE review for head SHA`;
   - else the role is met.
5. Anything missing, or a head `gh` could not read: deny (enforced) or add context (advisory), with one line:
   `atlas reviews: merge denied. PR #99 is missing: cost review for head 6e828cc. File each review as a journal
   entry: ...`.

The gate runs on every session's PreToolUse, the main session's included, since that is where `gh pr merge`
runs. A store error fails open and is logged as a miss.

## 4. Not done here

- The threat model: the gate stops a forgotten review, not an evasive agent. It does not see a merge that lands
  without `gh pr merge` (`git push origin HEAD:main`, `gh api -X PUT repos/O/R/pulls/N/merge`), nor one inside a
  quoted script (`bash -c "gh pr merge 99"`, `pwsh -Command "..."`). A local `git merge` is left alone on purpose,
  since the verify scripts merge onto origin/main locally.
- A push between the gate's `gh pr view` and the merge lands a head nobody reviewed. `--match-head-commit` would
  close that, but the hook cannot rewrite the command; requiring the flag is a possible follow-up.
- In `cd other && gh pr merge 99`, `gh pr view` runs in the event's folder, so it may read another repo's #99. The
  head comparison then gives a false block, not a false allow.
- An unquoted word that starts with `#` is a comment to bash and PowerShell, so `gh pr merge #99` merges the
  current branch's PR; the gate reads it that way and checks that PR, not #99.
- Planned builder items do not get a `review: ROLES` line added by default; the step is enforced at the merge,
  not at the start.
- `gh pr merge --auto` is gated like any merge.
