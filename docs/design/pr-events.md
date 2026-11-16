# PR events: a review reaches atlas as an event

Status: Part 1 built (the waiting form, the wake, the notice), Part 2 built (the listener, `src/pr_listen.rs`) and
Part 3 built (the supervisor, `src/pr_supervise.rs`, tested against a fake gh only). Part 4 (the live check)
follows. Release: v0.5.0.

When work waits on a PR review, atlas reacts to the review when it lands, instead of an agent or the orchestrator
polling GitHub (Brandon 2026-11-12, journal:ATLAS-USES:0008; route (a) confirmed, journal:ATLAS-USES:0009 Q1).
Reviews already have their own queue (intent r01584): a planned item that waits on a review is the waiting side,
and the event is what ends the wait.

## 1. The event path

```text
GitHub --(websocket, GitHub's host)--> gh webhook forward --(HTTP POST, 127.0.0.1)--> atlas listener
    --> one journal note per event, a met mark per waiting condition --> a notice at the session's next turn
```

- **gh webhook forward** (the `cli/gh-webhook` extension, v0.2.0, preview) creates a webhook on the repo through
  the API, receives the events over a websocket and POSTs each one, body and headers unchanged, to `--url`.
- **The listener** (`atlas pr-events serve`, Part 2) binds 127.0.0.1 only, checks the signature, parses the
  payload and calls the event path below.
- **The record** (Part 1, `src/pr_events.rs`): marks the conditions the event meets and files one note.
- **The notice** (Part 1, `hooks.rs`): the session sees one line at its next turn. Nothing polls.

Route (b), Claude Code's in-session PR notifications, reaches only a live session that subscribed; atlas does not
depend on it. Rejected: smee.io and any other public relay (review content would pass through a third party), and
an Actions workflow (CI minutes are scarce).

## 2. Which events count

| GitHub event | Action | Marks a wait met | Notice |
|---|---|---|---|
| `pull_request_review` | `submitted` | yes | `PR #N reviewed: VERDICT, by LOGIN, ...` |
| `pull_request_review_comment` | `created` | no | `PR #N review comment, by LOGIN, ...` |
| `issue_comment` on a PR | `created` | no | `PR #N comment, by LOGIN, ...` |
| `pull_request` | `closed` (merged or not) | no | `PR #N merged` or `PR #N closed` |

- A submitted review of any state meets the wait: `APPROVED`, `CHANGES_REQUESTED` and `COMMENTED` are all a
  review that came back. The verdict is in the notice, so the orchestrator reads a request for changes as one.
- A review comment always arrives inside a submitted review (a single inline comment is a `COMMENTED` review), so
  it adds nothing to the wait. A PR comment can be a bot's or the author's own and is not a review. A closed PR
  will never be reviewed: the item stays waiting and the notice says why, so the orchestrator drops it or files
  new work.
- Other actions (`edited`, `dismissed`, `deleted`) and other events are ignored and not filed.
- An event no item waits on is filed in the default lane (`pr-events`, configurable) and gets no notice: the
  journal keeps it, and nothing interrupts a session over a PR nobody waits on.

## 3. Who is waiting: the existing form

No new meta is needed. A planned item already waits on free text:

```text
atlas queue plan --lane L --title "merge PR #12" --after "review pr#12"
```

- `check_planned` accepts free text that is not a `journal:LANE:N` key; `dep_states` reads it as waiting until a
  met mark with the same text exists (`Marks::is_met`); `mark_met(item, text)` writes that mark, the same mark
  `atlas queue met KEY "review pr#12"` writes by hand.
- The event path reads two forms of the text: `review pr#N` and `review OWNER/NAME#N` (that repository only).
  Words and the `pr` prefix are matched without case; nothing else around the text is allowed, so
  `review pr#12 and pr#13` is not read.
- The bare form names no repository, so it is accepted only while atlas serves exactly one (the project's `repos`
  list; Q7 (B), journal:ATLAS-USES:0018). With none (Q8) or several, filing the wait (`check_planned`) fails and
  says to use `review OWNER/NAME#N`. The qualified form is accepted whatever is served.
- An accepted bare wait is bound to the one repository served when it is filed (Q9, journal:ATLAS-USES:0021):
  `check_planned` writes it into the item's meta as `pr_repo: owner/name` (a `pr_repo:` line the author wrote is
  dropped; atlas writes it). An event from repository R meets a bare `review pr#N` only when R is its stored
  repository, so a repository served later neither makes the wait ambiguous nor drops it; a qualified wait is met
  only when it names R. A bare wait with no `pr_repo:` line (filed before this release) is met only while R is the
  one repository served, the rule as Part 2 built it.
- For each waiting `queued` item and each such condition naming this PR, a submitted review writes a met mark with
  the item's own text, exactly as written, never `*` (`*` meets every condition on the item). A pair already met
  is not marked again, however many events name it.
- So `waiting: pr#N` meta is not added: the existing form holds every case found.

## 4. The record

One `note` per event, in the lane of the first item that waits on the PR, else the default lane:

```text
title: PR #12 reviewed (changes requested) by octo-reviewer
meta:  event: pull_request_review
       repo: octo-org/widgets
       pr: 12
       head: 6e828cc...
       verdict: changes requested
       author: octo-reviewer
       wakes: journal:L:0007 (review pr#12)
body:  a short excerpt of the review text (at most 280 characters, redacted)
```

- **Not a role review.** A GitHub review has no role, and its states are not the four role verdicts, so the
  `journal` tool would refuse it as a `review` entry. It files as kind `note` with no `review:` line: the merge
  gate reads notes with `review:` as role reviews, and an event note must not pass a role it did not earn
  (journal:ATLAS-USES:0009 Q5).
- `event:` is the first meta line: the notice finds event notes by it in one query, and a note never starts with
  `met:` or `done:`, which the queue reads as marks.
- Every value in the meta and title is reduced to a safe alphabet before it is written: logins and repository
  names to letters, digits and `-_./[]`, the head to hex digits, the verdict to the known words.
- The excerpt is untrusted text from a GitHub user. It is stored as data in the body, run through the leak
  redaction, and never put in a notice, a title or any prompt.

## 5. The notice

At a main session's next `SessionStart`, `UserPromptSubmit` or `PostToolUse` (the events whose context reaches
the session's own model, `hooks.rs` `NOTICE_EVENTS`), one line per new event note that woke something:

```text
atlas pr-events: PR #12 reviewed: changes requested, by octo-reviewer, lane L, waiting: journal:L:0007 (review pr#12, now met); event journal:L:0021
```

- The text is built from the meta alone. The review body is never in it.
- Each atlas server serves one session (an `mcp_tool` hook), so the hook keeps, per session, the newest record id
  it has looked past. A session starts from the store's newest record at its first event: it is told what lands
  while it lives, not the history. The store's `data_version` stamp skips the query when nothing was committed.
- Subagent events carry no notice: their context goes to the subagent.
- A command hook (one process per event) never sees a second event, so it tells nothing; the met mark and the
  queue proposal still carry the wake.

## 6. The listener (Part 2)

- `atlas pr-events serve` binds `127.0.0.1:PORT` (a configured port), never another address.
- It checks `X-Hub-Signature-256`: `sha256=` and the hex HMAC-SHA256 of the raw body under the secret, compared in
  constant time. atlas has no HMAC today; it is built over `sha256.rs` `Hasher` (RFC 2104: 64-byte block, key
  hashed when longer). A missing or bad signature is a 401 and is logged; nothing is filed.
- The payload is parsed with `json.rs`. Only the fields in section 4 are read.
- Configuration: a new top-level `pr_events` section (`port`, `lane`; `--port` and `--lane` win). A build before
  this release refuses an unknown top-level key, so the key is added only once the release is installed.
- Secrets: one per repository with a remote in the project's `repos` list, 32 bytes from the system random source
  (`platform::random`: `BCryptGenRandom` on Windows, `/dev/urandom` elsewhere) as 64 hex digits, held only in the
  process's memory (`Secrets`, whose `Debug` names repositories only). Generated at start; Part 3 rotates a
  repository's secret at each forwarder start (`Secrets::rotate`) and hands it to gh as `--secret`.
- The signature is tried against every repository's secret before the body is parsed, so nothing unsigned is read.
  A delivery signed with one repository's secret that names another is a 401.
- Answers: 200 filed; 204 an event or action section 2 does not count (`ping`, an edit, a comment on an issue);
  400 a body that does not read; 401 signature; 404 or 405 not `POST /github`; 411 no `Content-Length` (chunked is
  refused); 408 the connection's deadline ran out; 413 over 5 MB; 500 the store failed. Each answer is one stderr line with no payload text and no secret.
- One connection at a time, under one 10 s deadline per connection (each read waits only for the time left, then
  408), so a client that trickles bytes cannot hold the listener past it; the forwarders are few and the store has
  one writer, so a thread per connection buys nothing.
- A review by the pull request's author, or by a login ending in `[bot]`, is answered 204 and meets no wait.
- `tests/protocol.rs` `no_network_code` allows this one file a socket, and checks it binds only
  `Ipv4Addr::LOCALHOST` and never connects out.

## 7. The supervisor (Part 3)

- `atlas pr-events serve` runs the listener and the supervisor in one process: one `gh webhook forward` child per
  repository the listener holds a secret for (the project's `repos` list entries with a remote, `project.rs`
  `RepoSpec`, the same list as `served`); there is no `orchestrate.repos`. A name that is not `OWNER/NAME` of
  `[A-Za-z0-9-_.]` (and not `.` or `..`) gets no forwarder and one log line.
- The command line is an argument vector, never a shell string: `gh webhook forward --repo OWNER/NAME --events
  pull_request_review,pull_request_review_comment,issue_comment,pull_request --url http://127.0.0.1:PORT/github
  --secret SECRET`. The hook calls are `gh api --method GET --paginate --slurp repos/OWNER/NAME/hooks?per_page=100`,
  `gh api --method POST repos/OWNER/NAME/hooks/ID/pings`, `gh api --method GET repos/OWNER/NAME/hooks/ID` and
  `gh api --method DELETE repos/OWNER/NAME/hooks/ID`, each bounded to 30 s.
- A remote on a host other than github.com carries its host through: `--github-host HOST` to the forwarder and
  `--hostname HOST` to every `gh api`. Its stored line is `HOST/OWNER/NAME ids`; a github.com line stays
  `OWNER/NAME ids`.
- A child that exits is restarted with backoff (1 s doubling to 5 min, reset after a run of 10 min), each restart
  logged. gh-webhook itself retries a websocket drop (code 1006) every 5 s.
- Log: every supervisor line goes to stderr, written so a closed stderr never panics or stops a shutdown. State
  changes, not repeats, are also filed as a `note`: the first restart of a run, then one summary every 10 restarts
  or every hour, whichever is first; a found line only when something was deleted or a foreign `cli` hook was seen
  for the first time; each delete and each fatal; filed in the listener's lane (default `pr-events`) with meta `forwarder: OWNER/NAME` and
  `what: restart|found|deleted|fatal|error`. atlas has no separate events log; the journal is it. These notes never
  start with `event:`, `met:` or `done:`, so the notice and the queue do not read them.
- Not restarted (one `fatal` line): gh not on `PATH`; gh-webhook not installed (gh prints `unknown command
  "webhook"`); GitHub's 403 (`you do not have access to this feature`). The forwarder's output is shown on stderr a
  line at a time, cut to 300 characters, with the secret masked should it ever appear.
- Listener and supervisor run in one process, so the secrets live only in its memory: never in a file, a repo,
  `.env` or a log. A new secret is generated for each forwarder start (journal:ATLAS-USES:0009 Q2), one per
  repository, and the listener accepts each repository's current secret.
- Each child starts in a process group of its own (`setpgid` on Unix, `CREATE_NEW_PROCESS_GROUP` on Windows), so
  a Ctrl-C at the terminal reaches atlas alone. gh runs an extension as its own child, so killing gh alone would
  leave gh-webhook running: stop ends the whole tree, the process group on Unix (`kill(-pgid, SIGKILL)`) and the
  child's descendants read from one process snapshot on Windows (`TerminateProcess` on each, the child last).
- The forwarder's life is tied to atlas's. Windows: a Job object with `KILL_ON_JOB_CLOSE`, held by atlas, the
  child assigned right after spawn (a process the child starts before it is assigned escapes; the tree kill stays
  the fallback, and each candidate is opened once, its creation time checked and terminated on that one handle).
  Linux: `PR_SET_PDEATHSIG` (`SIGKILL`) in `pre_exec`, then a `getppid` check; this covers gh only, so the
  gh-webhook process gh starts can outlive a killed atlas, and its hook is swept on the next start. macOS: no tie;
  a killed atlas leaks its forwarders (a documented limit; the macOS target is check-only). A forwarder that exits
  on its own has its group or tree ended too.
- One forwarding `serve` per store: a lease row in `mem_meta`, key `pr_events_lease`, value `pid start beat_ms`
  (start is the process creation time, else 0; empty means no lease). Only the holder lists, starts, pings or
  deletes hooks; it beats every 15 s with a compare-and-set in one `guarded` transaction and checks the lease
  before each spawn, ping and delete. A lease is stale when its pid is dead, its start differs or its beat is over
  60 s old, and is then taken over. Other `serve`s run the listener without forwarders, say so once, and try to
  take the lease every 15 s. A holder that loses the lease ends its forwarders without cleanup (the new holder
  sweeps). A clean stop writes the empty value. No schema change.
- Stop: Ctrl-C (Ctrl-Break on Windows; `SIGINT`, `SIGTERM` or `SIGHUP` on Unix, an inherited ignored `SIGHUP` kept, so `nohup` holds) sets a flag; the supervisor ends
  every child with its tree, runs the hook cleanup, the store is taken once the request in hand is filed, and the
  process exits.
- Cross-platform: Windows, Linux and macOS (intent r01220, journal:ATLAS-USES:0013). The signal handler, the process
  group and the tree kill are in `src/platform/mod.rs`, behind `cfg`; no shell command lines. `cargo-zigbuild
  check -p atlas --all-targets` runs for `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`.

### 7.1 gh-webhook facts (source read 2026-11-12, cli/gh-webhook v0.2.0, released 2025-10-21)

- Flags: `-E/--events`, `-R/--repo` or `-O/--org` (one required), `-U/--url`, `-S/--secret`, `-H/--github-host`.
- `createHook` POSTs `repos/OWNER/NAME/hooks` with `name: "cli"`, the events, `active: false` and config
  `{content_type: json, insecure_ssl: "0", secret}`. **The config has no URL**: the local `--url` never reaches
  GitHub. The response's `ws_url` is the websocket it dials; the hook is then PATCHed active. A 403 is reported as
  "you do not have access to this feature".
- **The relay host** is whatever GitHub's API returns in `ws_url`: GitHub's own host, not a third party.
- **Hook cleanup:** nothing in the source deletes the hook, so every forwarder start leaves a `cli` hook on the
  repo. The forwarder prints no hook id.
- **The secret** goes on gh's command line (`--secret`), visible to same-user processes while gh runs, and is
  sent to GitHub in the hook config. Accepted (Q2): a single-user machine, a short-lived local process, rotated on
  each start, listener on 127.0.0.1 only.
- "One forwarder per repo per user" (journal:ATLAS-USES:0008) is not in the source: unverified.
- Live check (journal:ATLAS-USES:0058): GitHub pings a hook when it is made, and the hook is gone once its
  forwarder ends (a DELETE then answers 404). A forwarder killed before its websocket closes may still leave one.

### 7.2 Stale hooks: the proof rule

A forwarder's hook carries no URL (section 7.1), so Q3's URL test matches nothing, and no field of the hook tells
atlas's apart from another `gh webhook forward`'s: the secret is masked, the forwarder prints no id, and
`created_at` and `events` only narrow it. Q6's before/after diff (journal:ATLAS-USES:0018) could record a `cli` hook
atlas did not make, so it is replaced by a proof (review H1, journal:ATLAS-USES:0043):

- After a start the list is read at 1, 3, 7 and 15 s. A candidate is a `cli` hook not listed before the start,
  whose `events` equal ours as a set and whose `created_at` falls in [spawn - 120 s, spawn + 26 s + 120 s]. While
  the child runs, atlas asks GitHub to ping each candidate (`POST repos/OWNER/NAME/hooks/ID/pings`). A candidate is
  never recorded on its own.
- The proof: a delivery that reaches atlas's listener signed under the secret atlas made for this start of this
  repository names its hook: a ping by `/hook_id` in the body, which the HMAC covers; any other event by the
  `X-GitHub-Hook-ID` header of a delivery that verified. The listener hands each proof (repository, hook id) to the
  supervisor through a shared set; the supervisor records the id only if it was a candidate of the running start.
  A proof may arrive at any time in the forwarder's life.
- A start with no proof records nothing and logs once: "no proven cli hook for this start; left in place". A
  leaked hook of atlas's own is acceptable; deleting anyone else's is not.
- The recorded ids persist (Q10, journal:ATLAS-USES:0023) in `mem_meta`, key `pr_events_hooks`, one line per
  repository (`OWNER/NAME ID ID ...`, or `HOST/OWNER/NAME ...` off github.com): hook ids only, never a secret. Each
  change is a read and write in one `guarded` transaction. In `serve` the store is held by the listener's log
  thread and the supervisor's reads and writes go to it and wait for the answer. If the load fails, nothing is
  persisted for that run (the ids are kept in memory and the failure logged once), so a crashed run's ids survive.
- A lease holder that finds stored ids sweeps them for every repository that has some, served or not. Cleanup and
  sweep list first (all pages) and report; a recorded id is deleted only if its listed hook is still named `cli`
  with our events. A recorded id missing from the list is dropped only after `GET repos/OWNER/NAME/hooks/ID`
  answers 404; any other failure keeps it. A failed list deletes nothing; a failed delete keeps its own id. A `cli`
  hook atlas did not record is reported, never deleted. A clean stop leaves the store empty.
- A stop raised during the cleanup or the list before a start means no rotate and no spawn.
- The tests drive a fake gh (`Gh` trait): `api` answers from an in-memory hook list, `webhook forward` adds a `cli`
  hook there and starts a copy of the test binary as the forwarder, and a ping makes the forwarder that owns that
  hook post a signed ping to the listener; a foreign hook's ping reaches nothing. No real `gh webhook`, no API
  write, no network.

## 8. Failure modes

| Failure | What happens |
|---|---|
| gh-webhook missing, or the 403 "no access to this feature" | The supervisor logs it once and does not restart that repository's forwarder; waits stay open and `atlas queue met` still works by hand |
| A forwarder dies | Restart with backoff, logged; events in the gap are lost (GitHub's delivery log keeps them for the hook) |
| The listener is down while a forwarder runs | gh logs `warning: error forwarding event` and drops the event |
| A bad or missing signature | 401, one stderr line, nothing filed |
| A payload that does not parse, or an event not in section 2 | 400 or 204, nothing filed |
| The store is busy or fails | 500, logged; GitHub does not redeliver over the websocket, so the event is lost and the wait stays open |
| The same review delivered twice | Two notes; the met mark is written once |
| atlas stops without cleanup | The recorded ids stay in the store (section 7.2); the next start sweeps them, deleting only the recorded hooks still there, each delete logged |
| atlas is killed outright (not Ctrl-C) | Windows: the Job object ends the forwarders. Linux: gh dies with atlas, the gh-webhook it started may outlive it. macOS: the forwarders keep running (a documented leak). Recorded hooks still there are swept by the next lease holder; an unproven hook stays |
| A forwarder's hook is never proved | Nothing is recorded for that start, so nothing of it is deleted; logged once |
| A second `serve` on the same store | It does not hold the lease: it serves the listener without forwarders and takes the lease once the holder's is stale |
| A forwarder flaps or is offline | The first restart is a note, then one summary every 10 restarts or every hour; the rest goes to stderr |
| A local client trickles its request | 408 when the connection's 10 s deadline runs out |

## 9. Rulings and questions

Answered by Brandon 2026-11-12 (journal:ATLAS-USES:0009): Q1 route (a); Q2 secret on argv, rotated each start,
listener 127.0.0.1 only; Q3 delete only the atlas listener's hooks, after listing, each delete logged; Q4 v0.5.0;
Q5 kind `note`, no `review:` meta, the merge gate does not count it. The first live forwarder start needs
Brandon's say-so at that moment.

Answered by Brandon 2026-11-12 (journal:ATLAS-USES:0018):

- Q6 (A), "Only ones it made": atlas diffs the repository's hook list before and after starting a forwarder,
  records the new `cli` hook ids, and deletes only those (section 7.2). This replaces Q3's URL test.
- Q7 (B), "Bare only if one repo": `review pr#N` is accepted only while atlas serves exactly one repository; with
  several, filing the wait fails and says to use `review OWNER/NAME#N` (section 3).
- The cross-OS gate runs locally with zig and cargo-zigbuild (no CI).

Answered by Brandon 2026-11-12 (journal:ATLAS-USES:0021):

- Q8 "Refuse it": a bare `review pr#N` is refused when no repository is served (section 3).
- Q9 "Record repo at filing": a bare wait stores the one repository served when it is filed, so adding a
  repository later neither makes it ambiguous nor drops it (section 3, `pr_repo:`).

Answered by Brandon 2026-11-12 (journal:ATLAS-USES:0023):

- Q10 "Persist the ids": atlas stores the `cli` hook ids its forwarders create (as the Q6 diff records them). A
  supervisor that starts and finds ids left by a run that did not stop cleanly lists the repository's hooks,
  deletes only the stored ids still there, logs each delete, and clears them from the store (section 7.2). A clean
  stop deletes and clears as before.
