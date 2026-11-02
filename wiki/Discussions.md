Reviewers on the same subject argue in a thread that a human can read in any Markdown viewer (or Obsidian). One note per thread, `<vault>/Discussions/D-### <topic>.md`, ids from the
per-vault `D-` sequence of `next_id`. Frontmatter: `topic`, `subjects` (links built by `make_link`, other vaults
allowed; they are repeated under `## Subjects` so backlinks and indices see them), `participants`, `status`
(`open`, `concluded`, `parked`, `unresolved`; never deleted), `opened`, `concluded`, and the integrity fields
`posts` and `chain`.

Each post is `### P-<n> <author> - <kind>`, a metadata line `*<UTC time> | re: P-<m> | sha: <16 hex>*`, then
the body. Kinds: `claim`, `evidence`, `challenge`, `answer`, `concede`, `retract`, `question`, `proposal`, and
the three that only tools write: `verdict`, `unresolved`, `decision`. Everything but a claim or a question
needs `re`. Bodies need 12 characters and may not hold level 1-3 headings (they would split the thread).

**Posts are immutable.** Each metadata line carries the post's own hash, and the frontmatter carries the post
count and a hash chain over every post. Every append checks all of it under the write lock and names the
post that was edited by hand ("P-2 (alice - challenge) was edited by hand ..."), or says a post was
removed. A correction is a new `retract` post that cites the original. Two agents posting at once get
consecutive numbers (the write lock), tested with 12 processes. `author` is free text; the audit row still
records the MCP client, so a persona cannot hide which process wrote. There is no waiting tool: turn-taking
stays with the orchestrator, and `awaiting` is a query.

**Status** (`discussion_status`) comes from the `re` chains: a `retract` reply makes a point retracted, a
`concede` reply conceded, but only when the concede's author wrote the post being challenged (for a point that replies to nothing, its own author). A concede from anyone else stays in the
thread as an ordinary reply, decides nothing, and `discussion_status` lists it under `flags` as
`concede_ignored: not the challenged author`. An `answer` or `evidence` reply stands unless one of its own challenges is conceded
or still contested; a challenge with a standing answer is answered, otherwise contested; a claim is conceded
if any challenge to it was conceded, contested if any is open, answered if all were answered, unchallenged if
none; a question is answered or open.

**Ending a thread.** `conclude_discussion` appends the verdict and returns its stable link
(`[[Discussions/D-004 ...#P-9 orchestrator - verdict|D-004 P-9]]`; `make_link` also takes `heading: "P-9"`).
While a challenge is still contested it is refused, and the agent chooses: pass `force_reason` (the contested
points are then listed in the verdict), or call `mark_unresolved`. That appends an `unresolved` post with the
summary, each participant's final position and strongest post (a P-n link), the open questions and what
would settle it, and sets `status: unresolved`, `needs_decision: true`. `decision_queue` lists every such
thread across all vaults, oldest first. A `post` resumes an unresolved thread (back to `open`);
`record_decision {decided_by: "Dana"}` settles it (a `decision` post, `concluded`, `needs_decision: false`).

**Indices.** `build_index` takes `"rows": "discussions"` (thread rows with computed `thread`, `id`,
`contested`, `last_post`, `integrity`) or `"rows": "decision_queue"` (unresolved threads, oldest first); with
`"sources": []` they cover every vault.

## Worked example: two reviewers

```jsonc
// The orchestrator opens the thread.
open_discussion {vault: "perf", topic: "Upload batching: is 0.8 ms real?",
  subjects: [{vault: "perf", path: "Ideas/PI-010 Batch the uploads.md"}],
  participants: ["bob", "alice"],
  opening_post: {author: "bob", kind: "claim", body: "Batching the uploads took the load from 3.99 to 3.20 ms (run 12)."}}
// -> {thread: "perf:D-001", link: "[[Discussions/D-001 Upload batching is 0.8 ms real#P-1 bob - claim|D-001 P-1]]"}

post {thread: "perf:D-001", author: "alice", kind: "challenge", re: 1,
      body: "That run was disk-bound; on a warm cache batching costs a copy. Show the warm figure."}   // P-2
awaiting {author: "bob"}            // -> P-2
post {thread: "perf:D-001", author: "bob", kind: "evidence", re: 2,
      body: "warm: 1.79 -> 1.74 ms, 1,024 runs per arm, ABBA."}                                // P-3
discussion_status {thread: "perf:D-001"} // P-2 answered (by P-3), P-1 answered
read_discussion {thread: "perf:D-001", since_post: 2}   // alice reads only what is new
post {thread: "perf:D-001", author: "alice", kind: "concede", re: 3, body: "Accepted: no warm-cache regression."}

conclude_discussion {thread: "perf:D-001", verdict_body: "PI-010 stands as measured.",
                     reason: "both reviewers agree after the 1080 figure"}
// -> {verdict_link: "[[Discussions/D-001 ...#P-5 orchestrator - verdict|D-001 P-5]]"}

// The finding cites the thread, and the thread shows what it changed:
append_history {vault: "perf", path: "Ideas/PI-010 ....md", text: "confirmed warm (D-001 P-5)",
                reason: "per perf:D-001", discussion: "perf:D-001"}
read_discussion {thread: "perf:D-001"}  // outcomes: [append_history on Ideas/PI-010 ...]
```

Had alice not conceded, `conclude_discussion` would have been refused. The orchestrator then calls
`mark_unresolved` with both positions, and the thread waits in `decision_queue` until a human's
`record_decision`.
