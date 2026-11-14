# Work queue lanes: what can run in parallel

Status: built (classification, conflicts, priority with aging, the proposed batch, admission). The rulings are in
section 9; the eval is in section 8.

atlas's launch queue (`queue.rs`, `orchestrate.md` section 8.1) holds agent launches the agent cap refused,
oldest first, and lists them when a slot frees. This note generalizes it into a work queue that decides what
can run at once. Every item carries a **lane**, a **priority** and the **shared resources** it uses, and when
an agent finishes atlas proposes the next batch that can start together.

The aim, from the ruling:

1. **Classification.** A brief may carry `Lane:`, `Priority:` and `Uses:` lines. atlas infers anything missing
   from the brief. A declaration always wins.
2. **Conflicts.** A lane is a conflict domain: items on the same lane run one at a time, items on different
   lanes run in parallel. Shared resources (a GPU, a heavy compile) serialize across lanes.
3. **Priority.** Three levels, `urgent`, `normal` and `background`, with aging, so background work is never
   starved.
4. **Dispatch.** On each finish atlas proposes the next runnable batch within the caps, and the orchestrator
   launches it. Launching on its own comes later, with unmanned mode.
5. **An eval.** The inference is measured on a labelled set of real briefs before it is trusted.

## 1. Work items

A work item is a brief waiting to start, stored as a `queued` journal entry: the description as its headline,
the brief as its body, and its classification in the meta. Items enter the queue three ways:

| Way | When |
|---|---|
| The PreToolUse hook | A launch the agent cap refuses, or one that conflicts with a running agent (section 6.3) |
| `atlas queue add` | The orchestrator files planned work: `atlas queue add --description TEXT [--kind K] FILE` (`-` reads the brief from stdin) |
| The `journal` tool | `kind: queued`, the lane, the description as the title, the brief as the body |

An item leaves the queue when a launch with the same description and brief starts (Q-QU3), or with `atlas
queue drop KEY`. Because the entries live in the memory store, the queue survives a compaction and a restart.

## 2. Classification

### 2.1 Declarations

Among the first lines of a brief (the same window the `Kind:` line is read from; markdown bullets, quotes and
bold around a line are allowed):

```text
Kind: builder
Lane: parser
Priority: urgent
Uses: gpu, compile, worktree:parser-fix
```

- `Lane:` takes one lane id.
- `Priority:` takes `urgent`, `normal` or `background`.
- `Uses:` takes a list of resource names, or `none`. Only a limit can hold a launch back: `gpu`, `compile`,
  `worktree:NAME` and the names the configuration lists (section 3.2); any other word only adds a note at
  launch. A name with a `:` (`worktree:NAME`, `sqlport:15126`) is an instance of a class, and `sqlport 15126`
  reads as `sqlport:15126` (the parsing rule is in section 3.2). The word `read-only` in the list is not a resource: it says the item does not hold
  its lane (section 3.1).

Each field is declared or inferred on its own: a brief that declares only `Priority:` still has its lane and
resources inferred. A declared `Uses: none` turns off resource inference for that brief.

### 2.2 Inference

When a field is not declared, atlas reads it from the brief. The rules are deliberately cautious: a missed
resource lets two items run together that should not, a spurious one only costs parallelism, so the gate
(section 8) is written around the first.

| Field | Inferred from | When nothing matches |
|---|---|---|
| Lane | The rules the hooks already use to file an agent's journal (`brief_lane`): a lane named after "journal", "under", "your" or "own", else the one lane the brief names | No lane: the item conflicts with nothing on lanes |
| Priority | Words that say so: "urgent", "blocker", "blocking", "hotfix", "right away" (urgent); "low priority", "no rush", "when idle", "background" (background) | `normal` |
| `gpu` | A phrase that means the gpu in any project: a GPU lock, GPU timing, GPU bench, GPU run or capture, quiet GPU, a frame capture, CUDA, Vulkan, OptiX, RenderDoc, Nsight. The bare words capture, screenshot, bench, benchmark, bake and "render it" count only where the project turns them on (below) | Not used |
| `compile` | The brief runs a heavy build or test: `cargo build`, `cargo test`, `cargo nextest`, `cargo xtask`, a release build, or the words "heavy compile" | Not used |
| `worktree:NAME` | The brief points the agent at an existing worktree by path (a path under a folder named `worktrees`), unless it tells the agent to make its own | Not used |
| `file:PATH` | Not inferred in the first cut; declared only (section 9, Q-QL3) | Not used |

**Bare gpu words are per project (atlas-dev #150, journal ATLAS-USES:0002).** In a web repository a
screenshot is a browser PNG and a bench is a unit benchmark, so those words infer no gpu by default. A project
whose screenshots and benches do use the gpu turns them on in its `atlas.project.json`:

```json
{"name": "skyforge", "orchestrate": {"bare_gpu_words": true}}
```

`bare_gpu_words` is the only `orchestrate` key a project file or a `projects` entry may set; the user-level
`orchestrate.bare_gpu_words` is the default for projects that do not. Bare `gpu` is not an inference word: on
the `lanes-eval` corpus it raised the briefs with a spurious gpu from 23 to 31 of 60, since a brief names the gpu
without needing it.

The kind is classified as today (`Kind:` line, `subagent_type`, `isolation: worktree`, the default).

### 2.3 Precedence

A declaration always wins over inference, field by field. Inference never adds a resource to a field that was
declared, and a declared lane is never replaced by an inferred one.

## 3. Conflicts

### 3.1 Lanes

A lane is a conflict domain. An item holds its lane while it runs: a second item on the same lane waits. Items
on different lanes run in parallel. An item with no lane holds none.

An item whose brief declares `Uses: read-only` does not hold its lane: it shares the lane with whatever runs
there (a reviewer reading the branch a builder pushed) and serializes only on resources. Read-only is declared,
never inferred, whatever the item's kind (Q-QL1).

A review (`Kind: reviewer`) never holds or waits on a lane, declared or not: it waits only for a reviewer slot
(journal ATLAS-V03:0095). Two reviews of one PR run together beside the builder that owns the lane, and a
builder is never held back by a running review. Reviews are their own queue in `atlas queue`. A resource a review declares still serializes.

### 3.2 Resources

A resource has a capacity: how many holders it allows at once. Defaults: `gpu` 1, `compile` 1, each
`worktree:NAME` 1. They are set in atlas's configuration:

```json
{"orchestrate": {"resources": {"gpu": 1, "compile": 2, "db:staging": 1}}}
```

An instance name (`worktree:a`, `worktree:b`) is its own resource with its class's capacity (`worktree`),
so two items in different worktrees do not conflict and two in the same one do.

A capacity of 0 means "never serialize this resource". This is how a user turns off a resource atlas infers
(for example `compile: 0` on a machine where parallel builds are fine).

**Only a limit denies or queues (atlas-dev #150).** A resource is a limit when it has its own entry, its class
has one (`sqlport` for `sqlport:15126`), or it is `gpu`, `compile` or a `worktree:NAME`. Any other word on a
`Uses:` line (a repository name such as `db` or `react`) is not a limit: the hook never denies a launch for it
and the proposal never holds an item back for it. When a running agent uses the same word, the launch is let
through with one note naming the word and the holder (`shares db with Build ES1-1663`), whether the word was
declared or inferred. An entry with capacity N >= 1 makes a word a limit; capacity 0 turns it off, with no
note. `Uses:` is for exclusive resources: the gpu, the compile slots, one worktree, one port or one database,
not the repositories an agent touches in its own worktree.

**Parsing.** A `Uses:` value is split on commas into pieces, and each piece on spaces into words. Backticks,
quotes and a trailing `.` are trimmed and every word is lowercased. A word made only of digits joins the word
before it in the same piece as its value, so `sqlport 15126`, `sqlport: 15126`, `sqlport  15126` and
`sqlport:15126` are all `sqlport:15126`. Any other word is its own resource (`Uses: gpu compile` is gpu and
compile), and a number after a comma stays its own resource (`sqlport, 15126` is two). `none` and `and` are
dropped, `read-only` sets read-only, and the list is sorted with duplicates removed. A queued item's `uses:`
meta, written by the hook or by hand in a planned item, is read the same way.

### 3.3 What holds a resource

- **Running agents.** Each running agent's brief is classified like a queued one (its declared and inferred
  lane and resources), read from its transcript once and cached.
- **Proposed items,** while the proposal is built (section 5).
- **Tasks** (later, `tasks.md`): a background command run through `atlas run` holds the resources it declares
  or that the wrap classifier infers, in the same table. A holder whose process is gone releases its resources
  when that is detected; a lock is never left stale.

## 4. Priority and aging

Each item has a class: urgent 0, normal 1, background 2. Its effective rank is

```text
rank = class * aging_minutes - minutes_waited
```

and the queue is ordered by rank, then by a planned item's `order`, then by age (oldest first). Rank counts the
minutes waited, so `order` decides only between items of one rank, such as a batch filed in the same minute.
With the default `aging_minutes` of 60, a
background item that has waited two hours ranks with a fresh urgent one, so nothing waits forever behind a
steady stream of higher-priority work. `orchestrate.aging_minutes` sets the step; 0 turns aging off (strict
classes, then age).

Priority orders what starts next; it never stops a running agent.

## 5. Dispatch: the proposed batch

When an agent finishes, atlas builds a proposal:

1. Count what runs: agents per kind (from the agent cap's own count), lanes held, resource holders.
2. Walk the queue in rank order. For each item, it starts in this batch only if
   - its kind has a free slot, and the total stays within Claude Code's own limit;
   - it does not write on a lane that is held (by a running agent or an item already proposed);
   - each resource it uses has a free unit.
   An item that starts takes its slot, its lane and its resources for the rest of the walk.
3. An item that cannot start does not block items behind it that need other things. The highest-ranked item
   that waits on a lane or a resource reserves them for the rest of the walk: no lower-ranked item takes that
   lane or a unit of those resources (the reason names `KEY (reserved)`). Without this, an item that needs
   `gpu, compile` would lose each freed unit to lower-ranked items that need one, and wait forever while it
   ranks first. With it, aging raises a waiting item to the top and the top item gets what it needs.

The finish notification carries the proposal as one line, and the items that wait with the reason each waits:

```text
atlas queue: start together: journal:parser:0003 (builder, urgent), journal:docs:0001 (research). Waiting: journal:parser:0004 (lane parser, after 0003), journal:render:0002 (gpu held by agent a1). Start each with its brief (`recall KEY`).
```

`atlas queue next` prints the same proposal on demand, counting the running agents of the session whose
transcript it is given (`--transcript PATH`), or none.

The orchestrator launches the batch. atlas never launches anything itself until unmanned mode.

## 6. Admission

### 6.1 The kind cap

Unchanged: a launch past its kind's cap is denied and queued (Q-QU1).

### 6.2 Lane and resource conflicts at launch

A launch whose lane or resources conflict with a running agent is the case the proposal exists to avoid, but
the orchestrator may still try one (Q-QL2):

- **Declared on both sides** (the launch's brief declares the lane or resource, and the holders that declared
  it alone fill it: one for a lane, the capacity for a resource): the launch is denied and queued, like a
  launch past its cap. A declaration is the user's word. An inferred holder beside them does not soften this.
- **Otherwise** (the launch inferred the field, or the holders fill it only with inferred ones counted): the
  launch is let through with a one-line warning naming the holders, since an inference may be wrong. A `Lane:`
  or `Uses:` line corrects it.
- **A word that is not a limit** (section 3.2), shared with running work, declared or inferred: the launch is
  let through with a one-line note naming the word and its holders. It is never denied or queued for it.

A launch that starts takes its waiting entry off the queue: the item its `Queue: KEY` line names, else the
waiting launch with the same description and brief, else (a reworded brief, #150) the oldest waiting launch
with the same declared `Lane:` and the same kind. The hook names the key it took off. A planned item leaves
only by its `Queue:` key, and a launch with an inferred or no lane retires nothing by lane.

Lanes compare without case (`Lane: parser` and `PARSER` are one lane), as resources do.

### 6.3 Classification is recorded

A queued item's meta records each field and where it came from (`lane: parser (declared)`, `uses: gpu
(inferred: "capture")`), so `atlas queue show KEY` explains every decision and a wrong inference can be
corrected by adding a declaration to the brief.

## 7. Configuration

| Setting | Default | Meaning |
|---|---|---|
| `orchestrate.resources` | `{"gpu": 1, "compile": 1}` | Resource capacities (a name or a class); `worktree:*` has 1; other names are not limits and only warn; 0 never serializes |
| `orchestrate.aging_minutes` | 60 | Minutes of waiting that raise an item one class; 0 is no aging |

Everything here applies only in orchestrated mode (`orchestrate.enabled`), like the caps.

## 8. The eval

The inference is measured before it is relied on, as the digests and the memory cuts are.

- **Corpus.** Real briefs from session transcripts (the first user message of each subagent transcript),
  deduplicated, kept private outside the repository. Each brief is labelled by hand, by someone not looking at
  the inference rules: its lane, priority, the resources it needs exclusively, and whether it writes.
- **Scoring.** `cargo xtask lanes-eval --corpus DIR` classifies every brief and compares field by field. It
  reports lane accuracy, priority accuracy, and for resources the missed ones (labelled, not inferred) and the
  spurious ones (inferred, not labelled), each listed by brief. `--rows` and `--gate BASELINE` work as in
  `digest-eval` and `report-eval`: a change to the inference that adds a miss fails the gate.
- **The bar** (Q-QL3): no missed `gpu` or `compile`, lane accuracy at least 95%, spurious resources on at most
  10% of briefs. `file:` is declared only.

### 8.1 First measurement

60 real briefs (52 from one multi-repo workspace, 8 from two others), labelled by a research agent that did not
see the rules: 30 name a lane, 3 urgent and 4 background, resources on 30 (compile), 27 (a worktree), 23 (gpu),
1 (blender); 31 are read-only.

| Rules | Lane | Priority | Missed (gpu or compile) | Briefs with a spurious resource |
|---|---|---|---|---|
| First cut | 34/60 | 53/60 | 20 (17) | 21/60 |
| Plus `package ID`, `the ID lane`, build-environment words, no file names as worktrees | 42/60 | 53/60 | 10 (7) | 24/60 |
| Without `quiet.py`, `gpu.lock` and `timing window` (one workspace's words), one-pass scan | 42/60 | 53/60 | 10 (7) | 23/60 |

The bar is far off. Most lane misses are older briefs that never name their lane (the agent's own numbered name,
or a package the label maps to a work stream), and most spurious resources are reviewers that read a worktree's
branch without working in it. Neither can be read from words alone. So:

- **Inferred fields do not schedule.** The proposal and the admission deny count declared fields only (and the
  lane and resources a planned item was filed with). Inferred ones are shown (`atlas queue show`) and only
  warn at launch. A missed inference therefore loses nothing a declaration-only queue would not; a spurious one
  never makes an item wait. Nothing turns them on: a rule set that meets the bar would come with its own
  change. The inference words are general ones; a workspace's own file names and habits are not inference
  words (a brief that needs them declares `Uses:`). `package ID` is the one left: dropping it loses 8 lanes the
  gate holds, so it goes with inference itself.
- **Its cost.** Inference lowercases the brief once and finds every phrase group in one pass over the word
  starts, with no allocation per line or word, and runs only for the fields a brief does not declare.
- **Briefs declare.** The orchestrate skill tells the orchestrator to write `Lane:`, `Priority:` and `Uses:`
  lines, as it already writes `Kind:`.

## 9. Rulings

- **Q-QL1.** An item holds its lane unless its brief declares `Uses: read-only`. A research agent that declares
  it shares the lane and serializes only on resources; one that does not holds the lane. A reviewer never
  holds a lane (journal ATLAS-V03:0095, below).
- **Q-QL2.** A launch that conflicts with running work is denied and queued when both sides declared the
  conflicting field, and let through with a warning when either side was inferred.
- **Q-QL3.** The inference gate: no missed `gpu` or `compile` on the labelled set, lane accuracy at least 95%,
  spurious resources on at most 10% of briefs; `file:` is declared only.
- **Q-QL4.** Aging raises an item one class per 60 minutes waited, configurable (`orchestrate.aging_minutes`).

## 10. The backlog comb

`atlas backlog` (src/backlog.rs) finds placed work that nobody started. A queue entry is placement, not liveness:
TRACKS had one for six days and still dropped (journal TRACKS:0002). The comb reads store records only, plus
queued vault notes from the index, and never writes the queue.

- **Live** (Brandon 2026-11-10): a `brief` or `commit` entry within 7 days in the item's lane (a lane holding
  only that item, or named after it), or naming its ID as a whole token anywhere; or a running agent whose
  brief declares `Lane: ID` or names it (`--transcript`). Questions, decisions, notes, measurements, reviews,
  `next` entries and handoffs do not count.
- **Done**: a `done:` mark, a retirement, a commit title saying merged, landed, deployed or shipped about it, or
  the newest ruling whose subject is the ID closing it (dropped, killed, merged, folds into) when it is dated on
  or after the item's placement. A ruling deferring it (stays in the X backlog, parked) lists it as waiting with
  that ruling as the reason.
- **Aging** (shown as `aging`; the internal state is still `Overdue` until BC-2): not live, not done, and asked again (Brandon's words with an ask cue, dated on or after the
  placement), or its written-order turn came 2 days ago (every `after:` predecessor started, landed or live, or
  the previous `order:` in its lane), or placed over 14 days ago. Order in prose is not a written order.
  An aging item is shown with its days only; it is not a call to pull it forward.
  This aging is the comb's own state, unrelated to `orchestrate.aging_minutes` (Q-QL4), which raises a
  queued item's priority class while it waits for a slot.
- **Joins are exact**: a whole-token ID (`TRACKS-2` does not name `TRACKS`), a key, or the item's own lane. An
  ask naming several items of one lane chain counts for the chain's head.
- **Rank**: 40 per ask date, 15 when the turn came, 15 for a front-of-queue ruling, 5 per extra dated ruling,
  5 for placed work, plus a point a day of age up to 30. Weights and cue lists are the `backlog` section of the
  configuration.
- `--as-of T` reads each record at its newest version at or before T and ignores what was written, retired or
  marked after it; INTENT items are dated by the date they state.

## 11. Planned work

The queue holds all planned work, not only refused launches. A planned item is a `queued` entry with
`planned: true` in its meta; its body is a short scope, because the real brief is written at launch.

```text
kind: builder            (builder, reviewer, research, none, or orchestrator: done by the orchestrator, no slot)
priority: normal         (written `normal (default)` when none was given, and read back as the default)
uses: gpu, compile       (or read-only, or none)
order: 3                 (its place in the plan, after priority; a decimal such as 9.5 is read exactly)
after: journal:L:0003    (comma-separated: queue keys, or free text)
planned: true
```

- **Filing.** `atlas queue plan --lane L --title T [--kind K] [--priority P] [--uses LIST] [--after LIST]
  [--order N] [--scope TEXT]` writes exactly these lines; hand-written entries in the same format read the
  same. Each value's first word is read, so a note after it (`priority: urgent (declared)`) is kept.
  Over MCP, the `journal` tool's `queued` entry is the same path: the text's first line is the title, the rest
  the scope, and the `meta` lines `kind:`, `priority:`, `uses:`, `after:` and `order:` are checked as `plan`
  checks its flags (a known kind, a priority, a numeric order, `after` keys that name queued items). Other
  meta lines are kept as written (`review: simplicity, cost`). The entry is filed with the meta `plan` writes.
- **Starting.** A launch takes a planned item off the queue only when its brief carries `Queue: KEY`. A launch
  on the same lane without one (a reviewer, say) leaves it waiting. A `Queue:` key that is not waiting is
  reported.
- **`after:`** Each condition is a queue key, met when that item is marked done (`atlas queue done KEY`, which
  also finds an item that already started), or free text, met only when marked (`atlas queue met KEY
  [CONDITION]`). Nothing is read from how a journal entry is worded. `atlas queue plan` refuses a key that names
  no queued item; one filed by hand is shown as `(no such queued item)`, and items that wait on each other as
  `(a cycle)`, so neither waits silently. The proposal never offers an item with an unmet condition, whatever
  its `order`. The marks are read once per proposal (two queries over journal notes, no bodies).
- **Listing.** `atlas queue` lists refused launches oldest first, then planned items by priority and order,
  each condition marked met, waiting, missing or a cycle. A kind atlas does not know is shown with `?` and
  named in a closing line (a tool such as `blender` is a resource: `kind: builder`, `uses: blender`); such an
  item is scheduled as a builder. `atlas queue done KEY` retires a planned item finished without a
  matching launch.
