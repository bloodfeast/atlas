One JSON file, `atlas.json`, read by hand with every key checked (a misspelt key is an error naming the known
ones). Relative paths are taken from the file's folder. Where the file lives is on [[Install and setup]];
per-project settings are on [[Projects and multi-repo]].

## Minimal configuration

```json
{
  "vaults": [{"name": "notes", "path": "../Notes"}],
  "sources": [{"name": "docs", "root": "../my-project", "include": ["**/*.md"], "exclude": ["target/**"]}],
  "code": {"roots": [{"name": "app", "root": "../my-project"}]}
}
```

Vaults are folders atlas may add to; sources are folders it only indexes; code roots feed `symbol`. After an
edit, run `/mcp` and reconnect atlas, or restart Claude Code.

## A full example

```json
{
  "db": "state/atlas.sqlite",
  "vaults": [{"name": "notes", "path": "../Notes"}],
  "sources": [
    {"name": "handbook", "root": "../handbook", "include": ["*.md", "guides/**/*.md"], "exclude": ["drafts/*"],
     "chunk": {"top_level_items": true, "max_level": 6}},
    {"name": "memory", "root": "../memory", "include": ["*.md"], "id_keys": ["name"], "names_kind": "memory"}
  ],
  "ids": [
    {"name": "ticket", "regex": "\\bTK-[0-9]{3}\\b"},
    {"name": "pr", "regex": "(?:^|[^A-Za-z0-9_&/])(?P<id>#[0-9]{1,5})\\b"},
    {"name": "work", "regex": "\\b[A-Z][A-Z0-9]+(?:-[A-Z][A-Z0-9]+)+\\b", "exclude": ["NOT-AN-ID"]}
  ],
  "wikilinks": true,
  "vault_id_keys": ["id"],
  "refresh_limit": 64,
  "watch": true,
  "rescan_interval_s": 30,
  "bytes_per_token": 4.0,
  "code": {"roots": [{"name": "app", "root": "../app"}], "debounce_s": 20, "timeout_s": 1800},
  "semantic": {"model": "state/models/embed.atm"}
}
```

Names are unique across vaults, sources and code roots. Changing any of the index rules (or the set of names)
rebuilds the atlas rows on the next open or scan; a fingerprint of the rules is kept in `meta`.

Every key the configuration accepts is listed below. A test (`tests/docs.rs`) fails when the code accepts a key
this wiki does not document.

## Index

| Key | Meaning |
|---|---|
| `db` | the index database (default `state/atlas.sqlite` in the user's data folder); `--db` overrides it |
| `vaults` | writable vaults, as `--vault NAME=PATH` |
| `vaults[].name` | the vault's short name, used in every tool call |
| `vaults[].path` | the vault's folder |
| `sources` | read-only sources |
| `sources[].name` | the source's short name |
| `sources[].root` | the source's folder |
| `sources[].include` | globs below the root (`*` within a folder, `**` across, `?`; a glob without `/` matches the file name). The walk goes only as deep as the globs reach. Only `.md`, `.markdown` and `.txt` files are read |
| `sources[].exclude` | globs left out |
| `sources[].chunk` | how files are cut into sections |
| `sources[].chunk.top_level_items` | also start a section at every list item at column 0 (for files that are long bullet lists under few headings) |
| `sources[].chunk.max_level` | the deepest heading that starts a section |
| `sources[].id_keys` | frontmatter keys whose value is the identifier the file defines (default `["id"]`) |
| `sources[].names_kind` | every file stem in the source becomes an identifier of this kind, mentioned wherever the stem appears as a whole word |
| `ids` | identifier patterns in priority order |
| `ids[].name` | the identifier kind |
| `ids[].regex` | the pattern (atlas's own regex subset, see [[Security model]]); a group named `id` selects the identifier from a wider match |
| `ids[].case_insensitive` | match ignoring case |
| `ids[].exclude` | matches that are never identifiers |
| `wikilinks` | record `[[targets]]` and file stems as `note` identifiers (default true) |
| `vault_id_keys` | frontmatter keys whose value is the identifier a vault note defines (default `["id"]`) |
| `refresh_limit` | the most stale files a query re-indexes on the spot (default 64) |
| `watch` | watch the roots for changes so a query checks only what changed (default true; see [[Identifiers and freshness]]) |
| `rescan_interval_s` | seconds between safety-net walks of a watched root (default 30) |
| `bytes_per_token` | the ratio the token ledger converts bytes with (default 4.0); recalibrated by a script that tokenizes sample answers, never by judgement |

## Code index

See [[Code index]].

| Key | Meaning |
|---|---|
| `code` | the code index's settings |
| `code.roots` | code roots for `symbol` |
| `code.roots[].name` | the root's short name |
| `code.roots[].root` | a Cargo workspace or package for Rust; any folder for WGSL and Protocol Buffers |
| `code.roots[].build_scripts` | let rust-analyzer run this root's build scripts and proc macros (default false: nothing is compiled) |
| `code.roots[].cpp` | this root's C++ settings: an object with `trust` (below) |
| `code.roots[].cpp.trust` | whether scip-clang may index this root's C++: `"off"` (the default: C++ is only scanned), `"ask"` (only a `compile_commands.json` approved since it last changed, with `atlas trust cpp ROOT`), or `"always"`. The database is the repository's own file and names the compiler scip-clang may run, so one naming a relative path or a file inside the repository is never run |
| `code.roots[].exclude` | globs, relative to the root with `/` separators, of folders and files the code index leaves out (default none). A glob without `/` names a folder or file anywhere below the root (`OctoPrint-Slow`), one with `/` a path from the root (`plugins/slow`, `**/generated`); matching ignores case, and a leading `./` or trailing `/` is dropped. Nothing below a match is walked, so a package there is no unit of its own: its indexer (scip-python, scip-typescript and the others) never runs and nothing in it is scanned. An indexer whose unit or workspace holds the folder still reads it (rust-analyzer over a Cargo workspace, or a unit whose project file sits above it); the rows it gives for excluded files are dropped. Example: `["OctoPrint-Slow", "tools/gen"]` |
| `code.roots[].scip` | prebuilt SCIP indexes for this root's units, written elsewhere (CI, WSL, Docker): `true` or absent reads an `index.scip` in a unit's folder; `false` reads none; a path is the file for the root's own folder; an object maps a unit folder (below the root, `"."` for the root) to a path, or to `false` to read none there, other folders following the `index.scip` convention. Paths are relative to this file. A source file newer than the prebuilt file is scanned instead (see [[Code index]]). Example: `{"services/api": "ci/api.scip", "tools": false}` |
| `code.rust_analyzer` | the rust-analyzer binary (default: `rust-analyzer` on `PATH`, else a rustup toolchain's `bin`; each must answer `--version`) |
| `code.indexers` | the SCIP indexers of the other languages (scip-go, scip-typescript, scip-dotnet, scip-python): `false` runs none (every unit is scanned), `true` or absent finds each one (see [[Code index]]), or an object from indexer name to a path, which runs that binary (for scip-typescript and scip-python also its npm package folder or its `.js` script, run with `node`; a path given for scip-python is run on Windows too, so a wrapper script that runs it in WSL or Docker works), or to `false`, which turns that one off; an indexer not named is found as usual. Example: `{"scip-go": "/opt/go/bin/scip-go", "scip-typescript": false}` |
| `code.timeout_s` | a rust-analyzer or other indexer run is killed past this many seconds (default 1800) |
| `code.debounce_s` | a changed root is re-indexed once quiet this long (default 20) |
| `code.defer_lock` | a lock file; background re-indexing waits while it exists (default none). It also waits while a `cargo` process other than atlas's own ancestors runs |
| `code.dir` | where `.scip` files and the indexer log go (default `code/` beside the database) |

## Semantic search and models

See [[Semantic search]].

| Key | Meaning |
|---|---|
| `semantic` | semantic search's settings |
| `semantic.model` | the packed embedding model; when absent, the `models.files` entry whose id is `semantic.model_id`, else `models/embed.atm` beside the database. Semantic search is off and recall is lexical while the file is absent |
| `semantic.model_id` | which `models.files` entry is the embedding model (default `embed`) |
| `semantic.threads` | encoder threads for embedding (default 8) |
| `models` | the model files and their download |
| `models.files` | the model files downloaded at setup. A file is used only after its hash matched; changing the model is a change here, not in code |
| `models.files[].id` | the file's id |
| `models.files[].file` | the name it is stored under |
| `models.files[].url` | where it is downloaded from (`https`) |
| `models.files[].sha256` | its pinned SHA-256 |
| `models.files[].size` | its size in bytes |
| `models.dir` | where model files are kept (default `models/` in the user's data folder) |
| `models.auto_download` | download missing model files in the background when the server starts (default true); `atlas --setup` downloads them either way |
| `models.curl` | the `curl` binary (default: the one Windows ships in `System32`, else `curl` on `PATH`) |

## Updates

| Key | Meaning |
|---|---|
| `update` | the update check's settings |
| `update.enabled` | check for a newer signed release at session start (default true; `atlas settings off update` writes false). Builds from source never check |
| `update.pin` | a version (`MAJOR.MINOR.PATCH`): atlas stays on what is installed |
| `update.notify` | tell you about updates (default true): a line at session start, once per version, and `atlas --statusline` for a status line badge while one is pending. Pinned or off, atlas still reads the signed manifest (at most hourly, never the binary) for it; false turns the notice off and, for a pinned or off setup, every update network call |
| `update.url` | where the release manifest is read (default the public repository's latest release; `https`, or `http` to loopback). The signature is checked against the built-in key wherever it comes from |

## Calibration

The baseline `--token-report`'s measured ratio divides by: what agents read on lookups before atlas, measured
from your own Claude Code transcripts (see [[Tools reference]]). It runs by itself; these keys only adjust it.

| Key | Meaning |
|---|---|
| `calibrate` | the self-calibration's settings |
| `calibrate.auto` | calibrate in the background when the server starts and the stored baseline is missing or stale (default true). `ATLAS_CALIBRATE=off` in the environment turns it off too; `atlas --calibrate` runs it by hand either way |
| `calibrate.skip` | what leaves a transcript out, matched against the folder its session ran in and the Claude Code folder its transcript is filed under (a replay keeps the folder of the session it copies): an entry with `/`, `\` or `:` is a folder (it and the folders below it), any other is a word that must be a whole word of the part of the folder's path below the project root that holds it (split at every character but a letter or digit; all of the path when no root holds it or there are no projects), so a project kept under a `temp` or `eval` folder still counts. `atlas --calibrate` names each folder and transcript a skip left out. It replaces the default, `["tmp", "temp", "scratch", "eval", "evals", "replay", "replays"]`. A changed list recalibrates |
| `calibrate.handoff` | the handoff file-name pattern, a regular expression matched case-insensitively against a file's name (default `handoff[^/]*\.md$`). A changed pattern recalibrates |

## Memory store

See [[Memory store and hooks]] and [[Memory tree]].

| Key | Meaning |
|---|---|
| `memory` | the memory store's settings |
| `memory.store` | the store file (default `memory.sqlite` beside the database) |
| `memory.export_dir` | the folder exports are written under |
| `memory.exports` | the generated files and folders |
| `memory.exports[].name` | the export's name, unique among exports; an export whose name is also a source is synced from it |
| `memory.exports[].shape` | `memory`, `items` or `headings` |
| `memory.exports[].path` | the file, or the folder for `memory`, relative to `export_dir` |
| `memory.exports[].index` | the index file inside a memory folder (default `MEMORY.md`) |
| `memory.exports[].handoffs` | add new handoff records to this export (`headings` only) |
| `memory.exports[].index_cap` | cut each index line to this many characters (default: no cut) |
| `memory.exports[].index_topics` | generate a clustered index under these topics instead of one line per memory |
| `memory.exports[].index_topics[].name` | the topic's header |
| `memory.exports[].index_topics[].words` | lowercase words; a memory is listed under the first topic with a word in its slug, else in its description |
| `memory.snapshot_hours` | a snapshot is taken on the first write after this many hours (default 24) |
| `memory.snapshot_dir` | where snapshots and the archive go (default `snapshots/` beside the store) |
| `memory.snapshot_cap_mb` | past this, the oldest snapshots are folded into the archive (default 1024) |
| `memory.brief_reference` | the rulebook: `--token-report` measures the "brief without atlas" from the pre-atlas transcripts' reads of this path (matched as text, slashes and case folded), and logs its size at each subagent start as the "if read whole" line (default: the first `items` export) |
| `memory.tree` | the memory index as a tree (off when absent) |
| `memory.transcripts` | index this project's session transcripts at each compaction as an opt-in source: `recall {key: "turn:<session8>:<line>@hash"}` and `recall {query, kind: "turn"}` (visible text only, secret lines redacted, never served to another project; default false). On, `recall` also resolves a compaction card's chain keys (`turn:<session>:b<first>-b<last>@hash`, `turn:<session>:b<offset>#<k>@hash`, `compact:<session>:b<offset>@hash`), read from the transcript and not indexed, without `compact.enabled` |
| `memory.resume_card` | at a new or cleared session (SessionStart `startup` or `clear`), inject the resume card (at most 1,155 bytes): live lanes, their open questions, running agents after a `/clear`, the work queue's head, an unmet goal and other handoffs, as keys, then the title and next step of the handoff the question names and a multiple-choice question about what to do first (an empty store gives none; default true) |
| `memory.question_stale_days` | days after which an open question that is not deferred is flagged `stale` on the resume card and in `recall op=status`, and after which an idle lane counts as a sweep signal (default 3; at least 1) |
| `memory.rules` | experimental, off by default: whether atlas manages the memory folder and rule files. Unset keeps what is named by hand, `true` finds them and answers with the ruling that holds, `false` is the same as unset, and `"off"` also stops sync and recall of what is named by hand and keeps the store. See [[Memory and rule files]] |
| `memory.ideas.enabled` | idea capture, off by default: the one switch for capture, surfaces and questions. See [[Memory store and hooks]] |
| `memory.ideas.weights` | a weights file in place of the built-in frozen ones |
| `memory.ideas.band_below` | how far below the threshold a score is still unsure (default 0.25) |
| `memory.ideas.band_above` | how far above the threshold a score may still be asked about (default 0.5) |
| `memory.ideas.asks_per_day` | questions per UTC day (default 0 until the route is proven live; 0 asks nothing and turns the band off) |
| `memory.tree.export` | the `memory` export the tree is built for (default the first) |
| `memory.tree.hub_dir` | the hub folder inside the memory folder (default `index`) |
| `memory.tree.hub_max` | a hub past this many lines is split (default 24) |
| `memory.tree.hub_target` | the size a split aims for (default 20) |
| `memory.tree.hub_min` | a part under this is merged into its nearest sibling (default 8) |
| `memory.tree.line_max` | a memory's line is cut at a whole word to this many characters (default 150) |
| `memory.tree.pin_body_chars` | a pinned memory's body is shown in the root when at most this long (default 400) |
| `memory.tree.line_format` | `slug` (default) or `link` |
| `memory.tree.name_stop` | lowercase words that never name a hub |
| `memory.tree.keyword_weight` | the weight of a branch's word score in placement (default 0.5) |
| `memory.tree.pins` | memories pinned in the root |
| `memory.tree.branches` | the fixed taxonomy (default one branch per memory type) |
| `memory.tree.branches[].name` | the branch's hub file name (lowercase letters, digits and `-`) |
| `memory.tree.branches[].title` | its title in the root |
| `memory.tree.branches[].hook` | its one-line description in the root |
| `memory.tree.branches[].words` | words that claim a memory (`*` at the end matches any ending) |
| `memory.tree.branches[].types` | `metadata.type` values that count toward the branch |
| `memory.tree.branches[].repo` | the repository a memory filed under this branch is about, for recall's boost (see [[Projects and multi-repo]]) |

## Recall, the context cap and orchestrator mode

See the Hooks section of [[Memory store and hooks]]. The floors are set by `cargo xtask calibrate-recall`
and `atlas --hook-report --tune-floor`, never by hand.

| Key | Meaning |
|---|---|
| `recall` | the recall gate's settings |
| `recall.budget_tokens` | the most a recall hook injects per prompt (default 600; with `recall.cos_floor` 0.53, chosen by the cos_floor sweep on the off-topic set and the 303-prompt hook replay) |
| `recall.lane_budget_tokens` | the budget of `recall {lane}` (default 2,000) |
| `recall.fts_floor` | the FTS5 score a record must clear without the embedding model |
| `recall.hybrid_fts_floor` | the FTS5 score a record must clear with the embedding model loaded |
| `recall.min_terms` | the prompt words a record must contain to pass lexically (default 3) |
| `recall.cos_floor` | the cosine a record must also clear with the model loaded (default 0.53, the lowest floor that passes the cos_floor sweep at 600 tokens with 2 prompt pointers: no more off-topic records than 0.57 at 150 tokens on the 30-prompt off-topic set; set 0.57 for a stricter floor) |
| `recall.cos_alone` | a cosine at or above this passes on meaning alone |
| `recall.query_tokens` | the prompt tokens the cosine is taken over (default 32) |
| `recall.inject_floor` | the cosine below which a hook injects a record not at all (records without a vector are not held to it). Default 0.60: on 14 days of hook data it dropped 3,480 pointer rows (about 37k tokens a week) for 8.3% of the hits, with the memory eval and its hook replay unchanged (61/70 informed, 52/70 strict); 0.66 lost 2 informed and 1 strict replay answers. 0 holds no record to it. Tuned on hit data by `atlas --hook-report --tune-floor 1`, which never goes below the active floor (the hook logs nothing below it) |
| `recall.session_budget_tokens` | the tokens one session's (or one subagent's) recall injections may use in all, lanes and pointers together; past it the hooks inject nothing more, and `recall {query}` still answers. 0 (the default) is no cap. Judge it against the `hook injections` block of `--token-report --verbose`, which gives a main session's injections per prompt and a subagent's per agent, and the measured "brief without atlas" beside it |
| `recall.prompt_pointers` | the most pointers one main-session prompt gets (2); the rest are counted in the `+N more` line, and a subagent's brief is not capped. 0 is no cap. On the 70 hook questions of the memory eval, 2 against no cap cost 2 informed passes and 1 strict at floor 0 for 22% fewer injected tokens |
| `recall.subagent_context` | what a subagent is given at its start: `"recall"` (the default, today's lane views and pointers), `"binding"` (only the Binding block: the keys its brief names, key and title, and the lane its brief declares by a `Lane:` line or an exact upper-case id after "lane", with each other lane a named key is filed in, matched to the agent by its launch's tool use id, no lane view and no pointers, the whole answer at most 9,000 characters) or `"off"` (nothing from the store; orchestration lines stay). The AB-2 toggle of the brief-only design |
| `recall.main_lane_bytes` | the bytes the one lane view a main-session prompt gets may take: the prompt's `Lane:` lane, else the first lane it names; every other named lane is a one-line head. 0 (the default) is no cap: every named lane's view, each up to `recall.lane_budget_tokens` |
| `recall.pointer_anchors` | whether each `recall {query}` pointer ends with `@src:path:line`, the vault or source, file and line its record's text starts at, so a follow-up can `read_note {vault: src, path, lines}` instead of `recall {key}` (default off; see the anchors rows of `cargo xtask memory-eval`) |
| `recall.select` | context selection of what the hooks inject (on by default): near-duplicates sent as refs, pointers and older lane entries trimmed to the clauses that match the prompt, each excerpt the record's own text. Measured by the hook rows of `cargo xtask memory-eval` (each floor with selection off, then on) and the `context selection` line of `--token-report --verbose` |
| `recall.select.enabled` | `false` sends every pointer and lane entry whole, as before (the gate and budgets still apply) |
| `recall.select.dup_cos` | the cosine between two records' vectors at which one may be sent as `- KEY: near-duplicate of OTHER` (default 0.82; without the embedding model nothing collapses) |
| `recall.select.dup_overlap` | the share of a record's distinct words the record sent before it must hold for it to collapse (default 0.90). One way, so a record that adds something (a memory saying a rule was reversed) is never hidden behind the ref |
| `recall.select.trim_chars` | the longest excerpt a pointer carries in place of its first line, in characters (default 90, the length of a first-line pointer) |
| `recall.select.lane_chars` | the longest excerpt of an older lane entry (decision, measurement, commit, note) in a hook's lane brief (default 180; 0 sends them whole). The handoff, brief, open questions and next step are always whole |
| `cap` | the context-cap guard's settings |
| `cap.warn_tokens` | the handoff point: an agent past this is told to wrap up and write its handoff at its next clean stopping point. It applies to every agent kind without its own `handoff`, and to the main session (default 285,000 scaled to the context window) |
| `cap.block_tokens` | the hard limit: past this every tool but `handoff` is denied. It applies to every agent kind without its own `limit`, and to the main session (default 450,000 scaled to the context window) |
| `cap.warn_every` | the warning repeats after this many more tokens (default 25,000) |
| `cap.main_session` | tell the main session to ask the user for /compact (or /clear) past its compact point; advice only, never blocked (default true) |
| `cap.main_warn_tokens` | the main session's compact point: from this many context tokens it is told to ask the user for /compact while `cap.main_session` is on (default 0: `cap.warn_tokens`) |
| `hooks` | the hooks' own settings |
| `hooks.budget_ms` | the time budget of every hook event, in milliseconds, counted from when the event arrived (default 3,000; at least 1; at most 10,000, Claude Code's shortest hook timeout in the shipped hooks). PreCompact, SessionStart and SessionEnd get at least 8,000 ms, whatever is set here: their work is a store's first open and a checkpoint, and a cut one loses the compaction instructions or the session card. A hook works in phases (for a prompt: session, recall, ideas, main_context, restart_warn, drift; for a session start: the clear card, resume card, restart offer and compaction card) and starts none once the budget is spent: it answers with what it has and one line, `atlas: hook time budget (N ms) spent; skipped: ...`, and exits 0. A wait on another writer's lock of the memory store ends at the budget too. A resident server answers a hook still running 250 ms past its budget itself (a hook waiting behind a slow one included) and drops the late answer; `atlas --hook` does the same and exits. That answer is the line alone for UserPromptSubmit, SessionStart and SubagentStart; for a launch or a merge it is a refusal, `atlas: gate check did not finish within its budget (N ms; slow: WHAT); refused, retry the call`, WHAT naming the slow phase and its milliseconds, or the wait in the queue and the hook ahead of it (it fails closed: a PreToolUse of an `Agent`/`Task` launch, of a `SendMessage`, or of a shell command with a `gh pr merge`, a `gh api -X PUT .../pulls/N/merge`, or either inside a shell's `-c` string); any other subagent tool call fails open: it goes ahead with the context line `atlas: gate check skipped, its budget spent (N ms; slow: WHAT); this call went ahead unchecked, and the next one is checked again`, so a backed-up hook queue cannot refuse every call of an agent. A resident server does not work a tool call the watchdog already answered. For any other event the answer is empty and the call passes through. `atlas --hook-report` lists each event's time from arrival (p50, p95, max), its queue wait (p95), and its refusals and overruns. The merge gate's `gh pr view` waits at most the budget left. Work after an answer waits 50 ms at most on another writer's lock, and what the lock refused (a session start, a filed idea, an edited file's re-index) is retried after a later answer. `ATLAS_HOOK_TIMINGS=1` prints each hook's phases and milliseconds on stderr; `ATLAS_HOOK_BUDGET_OFF=1` turns the budget off, for measuring without it; it turns off the fail-closed refusal too, so a gate that runs past Claude Code's own timeout is not refused by atlas (every gate still runs to the end and refuses on its own checks) |
| `orchestrate` | orchestrator mode: caps on how many agents of each kind run at once, and per-kind context limits (see [[Orchestration]], the `orchestrate` skill and `docs/design/orchestrate.md`) |
| `orchestrate.enabled` | turn the agent caps on (default false). Context limits apply without it. `atlas --setup` and `atlas settings orchestrate` ask for it and store the answers |
| `orchestrate.window` | the context window the limits scale to (default 0: 200,000 when `CLAUDE_CODE_DISABLE_1M_CONTEXT` is set, else 1,000,000; `CLAUDE_CODE_AUTO_COMPACT_WINDOW` is where Claude Code compacts, never read as the window) |
| `orchestrate.stale_minutes` | an unfinished agent whose transcript has not changed for this long is not counted (default 360) |
| `orchestrate.disk_full_minutes` | an unfinished agent whose transcript ends in a disk-full tool error (`ENOSPC`, "no space left", "not enough space on the disk", "os error 112") and has not changed for this long is not counted, so it stops holding its lane, slot and resources (default 10; 0 turns it off). `atlas queue release AGENT` frees one at once |
| `orchestrate.resources` | the work queue's shared resources and how many items may hold each at once, e.g. `{"gpu": 1, "compile": 1}` (the default). `worktree:NAME` allows 1; any other name not listed is not a limit and only warns at launch; 0 never serializes |
| `orchestrate.resources.compile` | compile slots (default 1). On yes, setup writes max(1, hardware threads / 16), at most 4, unless you answer `--compile N` or it is set |
| `orchestrate.resources.gpu` | gpu slots (default 1). Setup writes 1: `gpu` is a lock and atlas assigns no device, so nothing is probed |
| `orchestrate.aging_minutes` | minutes a queued item waits before it rises one priority class (default 60; 0 turns aging off) |
| `orchestrate.bare_gpu_words` | the bare words capture, screenshot, bench, benchmark and bake in a brief infer the gpu (default false). A project file (`atlas.project.json`) may set it for its project, and sets no other `orchestrate` key |
| `orchestrate.worktree_roots` | folders, made absolute against the configuration's folder, that a `Uses: worktree:NAME` resolves under for the worktree ownership ledger (`atlas worktrees`). Unset: every linked worktree that `git worktree list` gives for the project's repositories. A name that matches no worktree, or more than one, writes no row. A user configuration key: a project file does not set it |
| `orchestrate.runq_root` | Optional: runq's state folder, read for the restart point's runq holders (#221). The `RUNQ_ROOT` environment variable comes first (as runq itself reads it), then this key (made absolute against the configuration's folder); with neither set, no holders are read and no folder is assumed. Only holders under the server's own Claude Code process are recorded; with that process unknown, none are |
| `orchestrate.reviews` | the required-reviews merge gate: a `gh pr merge` in a shell command needs a passing review of the PR's head from each role (see `docs/design/reviews.md`) |
| `orchestrate.reviews.roles` | the review roles every PR needs, named by job, e.g. `["simplicity", "cost"]` (default none: the gate is off) |
| `orchestrate.reviews.mode` | `enforced` denies the merge, `advisory` adds a warning, `off` does nothing (default `advisory`) |
| `orchestrate.judge` | the task judge: a model and effort pick per agent launch from a local cells file (`note` op `judge`; see `src/taskjudge.rs`) |
| `orchestrate.judge.enabled` | at each admitted Agent launch, add one advisory line when the launch's model or its `Judge:` line differs from the pick, and log the pick to `judge-picks.jsonl` beside the memory store; never denies or rewrites a launch (default false) |
| `orchestrate.judge.mode` | `"off"`, `"advisory"` or `"enforced"`; a stated mode wins over `enabled` (absent: `enabled: true` is advisory, else off). Enforced: at an admitted builder launch the hook scores the brief with the classifier (`orchestrate.judge.classifier`, the scoring `note op=judge` does; both share a score cache, `judge-scores.jsonl` beside the memory store) and denies a launch whose `model` or `effort` differs from the pick or that passes no `effort`, naming the pick. Exceptions, each a prompt line: `Judge: brandon-named <model>/<effort>` allows that pair (logged with `brandon_named` true, never counted); `Judge: explore` allows a launch at exactly one rung below the pick (the rungs are the map's picks, by model then effort) only when the explore launch is due. Every 5th routed builder launch is the explore launch; while it is due, a launch at the pick is denied. The counter is `judge-explore.json` beside the memory store, seeded from `log` when missing. A builder launch with no scorable brief is denied. The advisory line still runs (default off) |
| `orchestrate.judge.log` | the dogfood log (JSONL): in enforced mode, one row per admitted builder launch (`ts`, `lane`, `brief`, `score`, `source`, `pick`, `launched`, `explore`, `brandon_named`, `judged_before_launch`, `outcome` null, `agent_id`, `tool_use_id`); the agent id is filled in at the agent's start, and a row whose `outcome` is null is an owed step on the compaction card until it is filled (default none) |
| `orchestrate.judge.cells` | the cells file: `{"cells": [{"key": {"kind", "has_brief"}, "scope", "unk"}]}`, looked up by (kind, names a brief), then kind, then the global cell (default none: no pick) |
| `orchestrate.judge.classifier` | the work classifier (rule C): an ordinal demand score 0-5 for a launch that names a brief, from a small head over the brief's input vector (bge-int8 encoder, model id `746bdb58bc259433-int8`) or from a kind-only table, turned into a model and effort by an editable rules map. Advisory only: one `Classifier:` context line at an admitted launch (kind source only: the head's embed is too slow for the launch hook) and a `classifier` field in the `judge_task` answer; never denies or rewrites a launch. Absent or off: no change. |
| `orchestrate.judge.classifier.enabled` | run the classifier (default false); it runs only for a launch that names a `BRIEF*.md` file |
| `orchestrate.judge.classifier.weights` | the head's `weights.f32` (f32 little-endian: 388 weights, then 5 thresholds; 1,572 bytes); a missing file, wrong size or wrong sha256 gives no pick and no error (default none). Not shipped; local only |
| `orchestrate.judge.classifier.weights_sha256` | the weights file's sha256, checked when set (default none) |
| `orchestrate.judge.classifier.source` | `"head"` (the trained head; needs `weights` and the bge-int8 `semantic.model`) or `"kind"` (the kind-only table: reviewer 0, builder 5, research 2, other 0) (default `"kind"`) |
| `orchestrate.judge.classifier.map` | the pick per score, `{"0": "opus/medium", ..., "5": "opus/high"}`; a score left out keeps its default (scores 0-4 opus/medium, 5 opus/high) |
| `orchestrate.judge.classifier.overrides` | per-kind overrides, each `{"enabled", "max_score", "pick"}` or null: `research` defaults to `{"enabled": false, "max_score": 1, "pick": "sonnet/medium"}`; `builder` defaults to null (set later by the builder replays). Reviewers have none |
| `orchestrate.judge.classifier.overrides.research` | the research override, or null for none (default `{"enabled": false, "max_score": 1, "pick": "sonnet/medium"}`) |
| `orchestrate.judge.classifier.overrides.research.enabled` | apply the research override (default false) |
| `orchestrate.judge.classifier.overrides.research.max_score` | the highest score the research override applies to (required) |
| `orchestrate.judge.classifier.overrides.research.pick` | the research override's `"model/effort"` (required) |
| `orchestrate.judge.classifier.overrides.builder` | the builder override, or null for none (default null: no override) |
| `orchestrate.judge.classifier.overrides.builder.enabled` | apply the builder override (default false) |
| `orchestrate.judge.classifier.overrides.builder.max_score` | the highest score the builder override applies to (required) |
| `orchestrate.judge.classifier.overrides.builder.pick` | the builder override's `"model/effort"` (required) |
| `orchestrate.default_kind` | the kind of an agent no rule classifies: `builder`, `reviewer`, `research`, `brief-writer` or `none` (default `builder`). Rules, in order: a `Kind:` line in the first 40 lines of the Agent launch prompt (not a brief file it names), `orchestrate.types`, an agent with `isolation: "worktree"` is a builder |
| `orchestrate.types` | agent type to kind, over the built-in map (`Explore`, `Plan`, `claude-code-guide` and `research` are research; `builder` is builder; `reviewer` is reviewer; `statusline-setup` is none; `brief-writer` is brief-writer). A kind is `builder`, `reviewer`, `research`, `brief-writer` (also `brief writer`) or `none` |
| `orchestrate.mode` | how context limits apply to a kind that sets none: `enforced` (past the hard limit every tool but the handoff is denied, and so is a SendMessage to the agent), `advisory` (told only) or `off` (default `enforced`) |
| `orchestrate.fresh` | when an agent that passed its handoff point finishes, the orchestrator is told to start a fresh agent from its handoff (`auto`) or to ask the user first (`ask`) (default `auto`) |
| `orchestrate.kinds` | per-kind settings: `orchestrate.kinds.builder`, `orchestrate.kinds.reviewer`, `orchestrate.kinds.research` and `orchestrate.kinds.brief-writer`, each with the keys below |
| `orchestrate.kinds.builder.max`, `orchestrate.kinds.reviewer.max`, `orchestrate.kinds.research.max` | agents of this kind at once (defaults 2, 2 and 1 for an install that never ran setup); clamped to Claude Code's `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS` (20 by default); 0 launches none. On yes, setup writes starters for the caps not answered or set: builders = the compile slots (at least 1), reviewers = builders, research = max(1, builders / 2) |
| `orchestrate.kinds.brief-writer` | brief writers: agents launched with `Kind: brief-writer` (or `brief writer`) or `subagent_type` `brief-writer`. Their own cap, never the builders' |
| `orchestrate.kinds.brief-writer.max` | brief writers at once (default uncapped: only Claude Code's `CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS` applies); a value, 0 included, is enforced and clamped like the others. Setup writes no starter for it |
| `orchestrate.kinds.brief-writer.window`, `orchestrate.kinds.brief-writer.limit`, `orchestrate.kinds.brief-writer.handoff`, `orchestrate.kinds.brief-writer.handoff_pct`, `orchestrate.kinds.brief-writer.mode`, `orchestrate.kinds.brief-writer.fresh` | as for the other kinds below |
| `orchestrate.kinds.builder.window`, `orchestrate.kinds.reviewer.window`, `orchestrate.kinds.research.window` | the context window this kind's limits scale to (default `orchestrate.window`) |
| `orchestrate.kinds.builder.limit`, `orchestrate.kinds.reviewer.limit`, `orchestrate.kinds.research.limit` | the hard limit in context tokens (default `cap.block_tokens`, else 450,000 scaled to the window) |
| `orchestrate.kinds.builder.handoff`, `orchestrate.kinds.reviewer.handoff`, `orchestrate.kinds.research.handoff` | the handoff point in context tokens, at most the limit (default `handoff_pct` of the limit, else `cap.warn_tokens`, else 285,000 scaled to the window) |
| `orchestrate.kinds.builder.handoff_pct`, `orchestrate.kinds.reviewer.handoff_pct`, `orchestrate.kinds.research.handoff_pct` | the handoff point as 1 to 100 percent of the limit |
| `orchestrate.kinds.builder.mode`, `orchestrate.kinds.reviewer.mode`, `orchestrate.kinds.research.mode` | `enforced`, `advisory` or `off` (default `orchestrate.mode`) |
| `orchestrate.kinds.builder.fresh`, `orchestrate.kinds.reviewer.fresh`, `orchestrate.kinds.research.fresh` | `auto` or `ask` (default `orchestrate.fresh`) |
| `orchestrate.kinds.builder.models`, `orchestrate.kinds.reviewer.models`, `orchestrate.kinds.research.models`, `orchestrate.kinds.brief-writer.models` | per-model points for this kind: an object of model ids or family aliases to `{window, limit, handoff, handoff_pct}`, for example `{"haiku": {"limit": 180000, "handoff": 90000}}`, so a Haiku builder hands off earlier than an Opus one. The subagent's model and the matching are as for `compact.model_windows` (its transcript's last real model, else its meta file's `model` alias; lowercased, `[1m]` stripped; an exact id key beats a family alias key). A key the entry leaves unset is the kind's, then the global default; an entry's `handoff` or `handoff_pct` replaces the kind's pair as a whole. A model with no entry, or not yet known, has the kind's points. The hook text names the entry that applied, for example `haiku builder: 90,000 (orchestrate.kinds.builder.models.haiku)`. Keys must be non-empty. Default empty |
| `orchestrate.kinds.builder.models[].window`, `orchestrate.kinds.reviewer.models[].window`, `orchestrate.kinds.research.models[].window`, `orchestrate.kinds.brief-writer.models[].window` | the context window this model's limits scale to (default the kind's `window`) |
| `orchestrate.kinds.builder.models[].limit`, `orchestrate.kinds.reviewer.models[].limit`, `orchestrate.kinds.research.models[].limit`, `orchestrate.kinds.brief-writer.models[].limit` | this model's hard limit in context tokens (default the kind's `limit`, then as for the kind) |
| `orchestrate.kinds.builder.models[].handoff`, `orchestrate.kinds.reviewer.models[].handoff`, `orchestrate.kinds.research.models[].handoff`, `orchestrate.kinds.brief-writer.models[].handoff` | this model's handoff point in context tokens; with `limit` in the same entry it must be at most that limit (a configuration error otherwise) |
| `orchestrate.kinds.builder.models[].handoff_pct`, `orchestrate.kinds.reviewer.models[].handoff_pct`, `orchestrate.kinds.research.models[].handoff_pct`, `orchestrate.kinds.brief-writer.models[].handoff_pct` | this model's handoff point as 1 to 100 percent of its limit |
| `compact` | compaction: the checkpoint card and deferral (see `docs/design/compaction.md`). On by default |
| `compact.enabled` | the compaction features: the PreCompact checkpoint with its chain card and the instructions it returns for the summary, the SessionStart(compact) card and ref report, `atlas settings compact-md`, and `compact.defer` (default true; `false`, or `atlas settings off compact`, turns them off). Off, PreCompact only forgets the session's repeated answers (and, with `memory.transcripts`, indexes the transcript) and returns no text, and `recall` refuses a chain key unless `memory.transcripts` is on |
| `compact.card_tokens` | the budget of the checkpoint PreCompact writes and SessionStart(compact) injects, not counting the chain card appended to it (default 400) |
| `compact.checkpoint_prompt` | predict where Claude Code compacts the main session on its own (its window, clamped by `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, less 33,000) and ask the session once, `compact.checkpoint_margin` tokens before it, for its checkpoint journal entry; an automatic compaction that comes first waits once for it (default true; needs `compact.enabled`) |
| `compact.checkpoint_margin` | how many tokens before the predicted automatic compaction the checkpoint is asked for (default 40,000) |
| `compact.clear_floor` | past this many context tokens on the first request after a compaction, the main-session prompts recommend /clear over /compact; the checkpoint is written at SessionEnd (reason clear) from the ending transcript and its card given at the cleared session's start (default 80,000; 0 never recommends a clear) |
| `compact.defer` | block a proactive automatic compaction while a tool chain is running, until the next quiet point, never past the ceiling (the window less three times the largest per-request growth, at least 40,000 tokens); a manual `/compact` is never blocked (default false, pending a live check that a blocked compaction is offered again; needs `compact.enabled`) |
| `compact.window` | the context window `compact.defer` measures against (default 0: 1,000,000 once the session has held more than 200,000 tokens, else 200,000). For a subagent's automatic compaction it is only the fallback for an unknown or unrecorded model: a known model's window (below) beats it |
| `compact.model_windows` | an object of model ids or family aliases to context windows in tokens, for example `{"claude-opus-5-5": 200000, "haiku": 200000}`. When a subagent past its handoff point compacts on its own, atlas blocks it (so it hands off) only if its hard limit comes before Claude Code's own limit on the subagent's window. The window comes from the subagent's model: its transcript's last real assistant `message.model` (skipping `<synthetic>` and empty ones), else its meta file's `model` alias (`haiku`, `sonnet`, `opus`). The meta file's `pickedEffortModel` is not read: Claude Code writes the parent's model there. Keys and ids are matched lowercased with a `[1m]` suffix stripped. An exact id key wins, then a family alias key (`haiku`, `opus`, `sonnet` or `fable`), which matches any id of that family, such as `us.anthropic.claude-opus-5-5-v1:0`; else a Haiku of generation 4 or older (`claude-haiku-4-5`, `claude-3-5-haiku-...`, `claude-3-haiku-...`) is 200,000, and any other model id containing `haiku`, `opus`, `sonnet` or `fable` (Haiku 5.5 and the bare `haiku` alias included) is 1,000,000 (200,000 when `CLAUDE_CODE_DISABLE_1M_CONTEXT` is set). Older ids that only have a 200,000 window, such as `claude-sonnet-4-5` or `claude-sonnet-4-6` without `[1m]` and `claude-opus-4-1`, classify as 1,000,000 by family: list them here to set their window. A key must be non-empty and a window above 20,000 (Claude Code's output reserve). An unknown or unrecorded model uses `compact.window`, else that same inferred window (default empty) |

## The backlog comb (`backlog`)

`atlas backlog [--all] [--as-of YYYY-MM-DDTHH:MM] [--json] [--transcript PATH]` lists placed work with no live
brief, commit or agent: aging items, then waiting; `atlas backlog dismiss ID --because KEY` silences an ID for good. It
proposes only and never changes the queue's order. The section is optional; every key has a default.

| Key | Meaning |
|---|---|
| `backlog.w_ask`, `w_turn`, `w_front`, `w_ruling`, `w_placed` | rank weights: per ask date (40), turn came (15), front-of-queue ruling (15), per dated ruling past the first (5), placed (5) |
| `backlog.age_cap_days` | age adds a point a day up to this (default 30) |
| `backlog.live_days` | a brief or commit keeps its lane live this long (default 7) |
| `backlog.turn_days` | an item whose written-order turn came this long ago is shown as aging (default 2) |
| `backlog.overdue_days` | a placed item older than this is shown as aging (default 14) |
| `backlog.stale_days` | an unplaced, never-asked ID older than this is listed as stale under `--all` (default 30) |
| `backlog.near_duplicate` | token Jaccard at which an ask repeats an earlier one (default 0.5) |
| `backlog.ask`, `front`, `place`, `fold`, `include`, `defer`, `close`, `done` | the cue phrases, lowercase |
| `backlog.stop` | ID-shaped words that are never work |
| `backlog.asker` | the name that leads a line in the user's own words (`- 2026-11-04 Name:` or `Name 2026-11-10:`); only such lines (and ideas) count as asks. Default none: no lead counts, so set it to your own name to rank asks |

## Worktree gc (`worktrees`)

`atlas worktrees gc` lists the stale worktrees of the ownership ledger with what it would remove and the sizes;
`--apply` carries it out. A worktree that holds any work is never removed (see [[Orchestration]], Worktree gc). A
user configuration key: a project file does not set it. Older builds refuse the unknown key.

| Key | Meaning |
|---|---|
| `worktrees` | the section, optional |
| `worktrees.auto_gc` | gc acts by itself (default false): from the hook, at most once an hour (SessionStart, SubagentStop) and at once after a merged PR event note, a detached `atlas worktrees gc --auto` that the hook never waits on. Off, that run only measures, and session start says one line when the stale space reaches `notify_gb` |
| `worktrees.dirty` | what a stale worktree that holds work gets: `report` (default; left alone and listed with the kind of work), `remove_target` (only its build output is deleted: the linked `target_dirs` folder and an ignored in-tree `target/`), or `commit_delete` (every change committed as a WIP commit on its branch, untracked files that are not ignored included, pushed without force, the remote ref read back and compared with HEAD, then removed; any failure leaves everything and is reported, with any `index.lock` a failed call left (never deleted); the writing git calls get 300 s; work a commit cannot save, such as a lock, a stash, a rebase under way or an ignored file outside the build folders, is only reported, and so is a tree with an untracked file over 10 MB, with a denied name such as `.env`, or holding a line the redaction rules or the leak denylist flag). `remove_target` skips a locked worktree |
| `worktrees.disposable` | globs of ignored paths in a worktree that are not work (default `["target/"]`). Each is matched from the worktree's top: `*` and `?` stay inside one folder name, `**` is any number of folders, and a trailing `/` matches folders only. So `target/` is the top-level build folder (not `crates/x/target/`), `*.cache` a file or folder at the top, and `**/__pycache__/` that folder at any depth; e.g. `["target/", "*.cache", "**/__pycache__/"]`. It applies to ignored paths only: an untracked file is always work, and so is a secret (`.env`, `.env.*`, keys and the other denied names) whatever glob covers it. git lists an ignored folder whole, so before a worktree is removed its whole tree is walked (no link or junction followed, stopping at the first hit; at the plan for a tree gc would delete, and again just before the delete): a matched folder that holds a secret (`.envrc` included) or a checkout (`.git`, `.hg`, `.jj`, `.svn`, a bare repository) at any depth, or cannot be read, keeps the tree. Absolute globs, `..`, backslashes and globs made only of `*`, `?` and `/` (such as `**`, `?*` or `**/?*`) are refused. List only caches: an ignored file it leaves out (captures, notes) keeps the worktree |
| `worktrees.notify_gb` | the session-start line's threshold with `auto_gc` off, in GB (default 10); that hourly run removes nothing and makes no quiet-period or guard walk: it only sums the sizes of the trees it would remove, up to the threshold. The sum is an upper bound, since a tree the removal would refuse (a secret or a checkout in a build folder) is still counted |
| `worktrees.quiet_minutes` | the quiet period, in minutes (default 120): a worktree whose files, linked build folders, git folder or ledger row changed within it is in use and never stale. Build output is looked at two folders deep (`target/debug/deps` itself, not its files), and the walk stops at the first newer file; files are walked last, only for a worktree gc would touch. It covers a main session working in a worktree and a ledger row whose lane is unknown |
| `worktrees.target_dirs` | build folders outside the tree that belong to a worktree, each holding `{name}` (the worktree's folder name), made absolute against the configuration's folder, e.g. `["D:/targets/{name}"]`. One is deleted with its worktree, never when another ledger worktree links the same folder, and never when it holds a checkout or a secret at any depth (a clone or `git init` anywhere inside it; the walk follows no link or junction and runs at plan and again just before the delete), lies inside any git checkout (configured or not) unless it carries a `CACHEDIR.TAG` with the generic Cache Directory Tagging signature (cargo writes one) and that repository tracks nothing in it, or is, holds or lies in a checkout (refused and reported). So an untagged folder, or one with a tracked file, under a parent folder that is itself a git repository is refused; a tagged, untracked one there is deleted like any other. Ignored files inside one are build output, not work, but a folder that lies inside its own worktree is walked with the tree (a checkout or a secret in it keeps the tree). A folder without `{name}` is refused |

## PR events (`pr_events`)

`atlas pr-events serve [--port N] [--lane LANE] [--auto]` listens on `127.0.0.1` only for the deliveries `gh webhook forward`
posts, checks each `X-Hub-Signature-256` against its repository's secret (one per repository in the project's
`repos` list, or those `pr_events.repos` names, generated at start and held only in memory), and files one journal note per event in the lane of the
item that waits on the PR (`atlas queue plan --after "review OWNER/NAME#N"`). A bare `review pr#N` is accepted only
while the project lists exactly one repository, and keeps that repository (`pr_repo:`) for good. It also runs one
`gh webhook forward` (the gh-webhook extension) per repository, with a new secret at each start, restarted with
backoff; Ctrl-C stops them and deletes only the `cli` hooks atlas saw them make. A flag wins over the section.
Older builds refuse the unknown key, so add it only once a build with it is installed.

| Key | Meaning |
|---|---|
| `pr_events` | the section, optional |
| `pr_events.port` | the port to listen on (1 to 65535; no default) |
| `pr_events.lane` | the lane for an event no item waits on (default `pr-events`) |
| `pr_events.ignore_logins` | GitHub logins whose reviews, review comments and PR comments file nothing, compared without case (default none) |
| `pr_events.repos` | the repositories to serve, each `OWNER/NAME` (github.com) or `HOST/OWNER/NAME` (another host), compared without case (default none: every repository in the project's `repos` list that has a `remote`). An entry that names no such repository is reported at start and skipped; one that names none of them stops the start |
| `pr_events.autostart` | `true` to have each session's start run the listener when none runs for the memory store (default `false`); needs `pr_events.port` |

**Auto-start.** With `pr_events.autostart` on, SessionStart starts `atlas pr-events serve --auto` in the
background when no listener runs for the memory store, and says so in one line with the listener's pid and log
file. No OS service is installed; the listener runs apart from the session (its own session on Unix, no console
on Windows), so it outlives the session that started it, and the next session to start after it ends starts it
again. Only one runs: a listener holds an exclusive lock on `<store>.pr-events.lock` for its whole life, and the
lock is let go however the process ends. `<store>.pr-events.pid` names the listener (`PID START`); a dead pid
is no listener, nor, on Windows, Linux and macOS, is a pid whose start time differs (the OS gave it to another
process; other Unix systems read no start time). A session that finds the lock held starts nothing, nor does one
that finds it free while a pid file written in the last 10 seconds (twice the time a listener waits for the lock)
names a live listener (one another session started a moment ago, which has not taken the lock yet); an older pid
file with the lock free names no listener, on every platform; otherwise it starts the listener while it holds
the lock and names it in the pid file before letting go, so two sessions starting at once start one. A listener
started with `--auto` waits a few seconds for that lock and leaves when another listener holds it; one started
by hand serves either way, as before, and also when the lock cannot be taken at all (it says so). On a file
system without locks the pid file alone decides, and that is best-effort: two sessions starting at the same
moment can start two. The listener's output is appended to `<store>.pr-events.log` (cut at a start when over
1 MiB); when the last listener ended on an error, the next session's start line quotes it, so a listener that
fails at every start does not pass for a clean one. A `pr_events` section that is not valid (`autostart` with no
`port`, say) stops the configuration from loading, with the reason, so it never leaves sessions without a
listener in silence.

The auto-started listener serves the project of the session that started it (that session's folder and
configuration), and one runs per memory store: with several projects on one store, only the first project to
start a session is served, and the others get no events until that listener ends. Give each such project its own
store.

To stop it, end the process the pid file names; on Unix `kill PID` stops it as Ctrl-C does, deleting the hooks
it made. Turn `autostart` off first, or the next session starts it again.

A review, review comment or PR comment files nothing, so it neither meets a wait nor surfaces a notice, when it
comes from a bot (the user or sender type `Bot`, such as `Copilot`, or a login ending in `[bot]`) or from a login
in `ignore_logins`; a review by the PR's own author files nothing either. A review comment or PR comment from the
user gh is logged in as files nothing too (read once with `gh api user` at start, of every host served at once,
and kept per host: the host is read from the delivery's `repository.html_url`, so the same login on another host
is another person and is heard; agents replying as that user wake no one), but that user's reviews still meet waits: the gh user is also the person
reviewing. Logins may carry `_` (Enterprise Managed Users). When gh is not logged in, the listener says so at
start and only the other rules apply. Merges and closes are filed whoever made them.

## Project tracking: Jira and Confluence (`project_tracking`)

`atlas track` reads Jira and Confluence over REST, read-only, with a credential from environment variables the
section names; atlas never holds the credential. Setup, token scopes, commands and how the token is kept:
[[Project tracking]]. Older builds refuse the unknown key, so add it only once a build with it is installed.

| Key | Meaning |
|---|---|
| `project_tracking` | the section, optional |
| `project_tracking.atlassian` | the Atlassian site, optional |
| `project_tracking.atlassian.site` | the site, `https://example.atlassian.net` (Data Center: Jira's URL); required |
| `project_tracking.atlassian.confluence_site` | Confluence's own URL (default `<site>/wiki` on Cloud, `site` on Data Center) |
| `project_tracking.atlassian.email_env` | Cloud: the variable holding the account email |
| `project_tracking.atlassian.token_env` | Cloud: the variable holding the API token |
| `project_tracking.atlassian.confluence_token_env` | Cloud: the variable holding Confluence's own API token, for a scoped token per product (default: `token_env`'s) |
| `project_tracking.atlassian.confluence_email_env` | Cloud: the variable holding the email for `confluence_token_env` (default: `email_env`'s); only with `confluence_token_env` |
| `project_tracking.atlassian.pat_env` | Data Center or Server: the variable holding a personal access token (bearer); not with `email_env` and `token_env` |
| `project_tracking.atlassian.timeout_s` | seconds one request may take (default 30) |
| `project_tracking.atlassian.min_interval_ms` | the least time between two requests to the site (default 200) |
| `project_tracking.atlassian.max_retries` | retries after a 429 or a 5xx (default 3) |
| `project_tracking.atlassian.max_wait_s` | the longest wait for one retry; a longer `Retry-After` is capped (default 60) |
| `project_tracking.atlassian.backoff_ms` | the first backoff without a `Retry-After`, doubled per try (default 500) |

A variable name is upper case letters, digits and `_`, at most 64 characters; anything else (a pasted token) is
refused without being repeated.

## Routing, handles and records (`lean`)

`lean` is an object read by the hooks, `atlas run`, `atlas slice` and the record commands. See
[[Handles and digests]].

| Key | Meaning |
|---|---|
| `lean` | the object below |
| `lean.routing` | false turns routing off (default true; `atlas settings off routing` writes a flag with the same effect) |
| `lean.pr_digest` | digests `gh pr diff` and `gh pr view` output per file and per comment, with rounds; false sends it the ordinary route, as before v0.4.0 (default true; see [[Handles and digests]]) |
| `lean.route_read` | which whole-file Reads become a digest: `outputs` (the default) digests only a tool's or a background task's saved output (a path under `tool-results/`, or a `.output` file under `tasks/`) from `lean.handles.threshold_bytes`; `tuned` digests any file from `lean.route_read_min_kb`; `all` digests any file from `lean.handles.threshold_bytes`, as before v0.5.0; `off` passes every Read whole. In every mode a ranged Read (`offset` or `limit`) and a second whole Read of a file the session already holds a read handle for pass whole. The old boolean still works: true is `tuned`, false is `off` |
| `lean.route_read_min_kb` | the smallest file `tuned` digests, in KB (default 32; used by `tuned` only) |
| `lean.annotate` | false stops the code-range annotations on Grep, Read and shell reads (default true) |
| `lean.handles` | the handle store's settings, below |
| `lean.digests` | the digests' settings, below |
| `lean.handles.threshold_bytes` | a tool result over this becomes a handle and a digest (default 12 KB) |
| `lean.handles.build_threshold_bytes` | the same for `cargo` build and test output (default 3 KB) |
| `lean.handles.slice_bytes` | the most one slice returns (default 8 KB) |
| `lean.handles.cap_mb` | the handle store's size; least recently used beyond it are evicted (default 512) |
| `lean.handles.max_age_days` | handles unused this long are evicted (default 7) |
| `lean.digests.max_bytes` | the digest budget for a run or a search (default 600) |
| `lean.records.rulings` | the file `atlas ruling` appends to (default `INTENT.md` in the first vault) |
| `lean.records.entries` | the file `atlas entry` appends to (default `LOG.md` in the first vault) |

## Projects

See [[Projects and multi-repo]] for the whole shape.

| Key | Meaning |
|---|---|
| `projects` | the projects; without it the configuration is one implicit project |
| `projects[].name` | the project's name |
| `projects[].roots` | folders whose insides make it active |
| `projects[].repos` | repositories, as in the project file |
| `projects[].file` | a project file merged in |
| `projects[].memory_dir` | the project's memory folder |
| `projects[].intent` | the project's rules file |
| `projects[].db` | the project's index database |
| `projects[].vaults` | vaults for this project alone |
| `projects[].sources` | sources for this project alone |
| `projects[].ids` | identifier patterns for this project alone |
| `projects[].code` | code roots for this project alone |
| `projects[].memory` | memory settings for this project alone |
| `projects[].wikilinks` | `wikilinks` for this project alone |
| `projects[].vault_id_keys` | `vault_id_keys` for this project alone |
| `projects[].orchestrate` | `{"bare_gpu_words": true}` for this project alone; no other `orchestrate` key is allowed here (a project file takes the same) |
| `user` | the user layer, shared by every project |
| `user.memory` | the user layer's memory folder (with its `MEMORY.md` index) |
| `default_project` | the project active where no project root holds the working directory |
| `checkouts` | remote URL to local checkout, for repositories a project file lists by remote |
| `projects_dir` | where a project without `db` keeps its database and memory store: `<projects_dir>/<name>/` (default `projects/` in the user's data folder) |
| `projects[].repos[].name` | a repository's name in the project |
| `projects[].repos[].path` | its checkout, relative to the configuration |
| `projects[].repos[].remote` | its remote URL (identity only, never fetched) |
| `projects[].repos[].role` | one line on what it is |
| `projects[].repos[].talks_to` | the repositories it calls |

### The project file

`atlas.project.json` at a project root, written to be committed. Paths are relative to the file.

| Key | Meaning |
|---|---|
| `name` | the project's name (letters, digits, `-` and `_`, at most 64) |
| `repos` | the project's repositories |
| `repos[].name` | a repository's name |
| `repos[].path` | its checkout, relative to the file |
| `repos[].remote` | its remote URL; with `checkouts`, a repository found by remote |
| `repos[].role` | one line on what it is, shown when a prompt names it |
| `repos[].talks_to` | the repositories it calls; the graph recall and the `repo` filters follow |
| `memory_dir` | the project's memory folder |
| `intent` | the project's rules file |
| `vaults` | vaults for this project |
| `sources` | sources for this project |
| `ids` | identifier patterns for this project |

## Sessions with no user (`ATLAS_UNATTENDED`)

An environment variable, not a key of `atlas.json`. It tells atlas's hooks whether a user is there to answer, for a
client that is not Claude Code (a wrapper or an SDK harness), which sets neither of Claude Code's own variables.

| Variable | Meaning |
|---|---|
| `ATLAS_UNATTENDED` | `1`: no user is there, so no SessionStart offer is asked (the Claude Code settings, orchestrated mode, adding the folder or its repositories as a project, the unmapped-folder notice, the restart point and the update notice); `0`: a user is there. It covers every offer atlas makes now and any added later, since all of them read the one flag it sets. Surrounding spaces are ignored, so cmd's `set ATLAS_UNATTENDED=1 && client` works. Empty or unset leaves the decision to Claude Code; any other value is ignored, with one stderr line naming it at SessionStart. Precedence: `ATLAS_UNATTENDED`, then `CLAUDE_CODE_SESSION_ATTENDED` (`1` attended, `0` not), then `CLAUDE_CODE_ENTRYPOINT` (`sdk-cli`, `sdk-ts` and `sdk-py` are unattended); with none of them set, a user is assumed |
