One JSON file, `atlas.json`, read by hand with every key checked (a misspelt key is an error naming the known
ones). Relative paths are taken from the file's folder. Where the file lives is on [[Install and setup]];
per-project settings are on [[Projects and multi-repo]].

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
| `memory.brief_reference` | the rulebook whose size at each subagent start is logged as the "brief without atlas" (default: the first `items` export) |
| `memory.tree` | the memory index as a tree (off when absent) |
| `memory.transcripts` | index this project's session transcripts at each compaction as an opt-in source: `recall {key: "turn:<session8>:<line>@hash"}` and `recall {query, kind: "turn"}` (visible text only, secret lines redacted, never served to another project; default false). On, `recall` also resolves a compaction card's chain keys (`turn:<session>:b<first>-b<last>@hash`, `turn:<session>:b<offset>#<k>@hash`, `compact:<session>:b<offset>@hash`), read from the transcript and not indexed, without `compact.enabled` |
| `memory.resume_card` | at a new or cleared session (SessionStart `startup` or `clear`), inject the resume card (at most 1,160 bytes): live lanes, their open questions, running agents after a `/clear`, the work queue's head, an unmet goal and other handoffs, as keys, then a multiple-choice question about what to do first (an empty store gives none; default true) |
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
| `recall.inject_floor` | the cosine below which a hook injects a record not at all (records without a vector are not held to it); tuned on hit data by `atlas --hook-report --tune-floor 1` |
| `recall.session_budget_tokens` | the tokens one session's (or one subagent's) recall injections may use in all, lanes and pointers together; past it the hooks inject nothing more, and `recall {query}` still answers. 0 (the default) is no cap. Judge it against the "brief without atlas" row of `--token-report`, which now has a `hook injections` line beside it |
| `recall.prompt_pointers` | the most pointers one main-session prompt gets (2); the rest are counted in the `+N more` line, and a subagent's brief is not capped. 0 is no cap. On the 70 hook questions of the memory eval, 2 against no cap cost 2 informed passes and 1 strict at floor 0 for 22% fewer injected tokens |
| `recall.pointer_anchors` | whether each `recall {query}` pointer ends with `@src:path:line`, the vault or source, file and line its record's text starts at, so a follow-up can `read_note {vault: src, path, lines}` instead of `recall {key}` (default off; see the anchors rows of `cargo xtask memory-eval`) |
| `recall.select` | context selection of what the hooks inject (on by default): near-duplicates sent as refs, pointers and older lane entries trimmed to the clauses that match the prompt, each excerpt the record's own text. Measured by the hook rows of `cargo xtask memory-eval` (each floor with selection off, then on) and the `context selection` line of `--token-report` |
| `recall.select.enabled` | `false` sends every pointer and lane entry whole, as before (the gate and budgets still apply) |
| `recall.select.dup_cos` | the cosine between two records' vectors at which one may be sent as `- KEY: near-duplicate of OTHER` (default 0.82; without the embedding model nothing collapses) |
| `recall.select.dup_overlap` | the share of a record's distinct words the record sent before it must hold for it to collapse (default 0.90). One way, so a record that adds something (a memory saying a rule was reversed) is never hidden behind the ref |
| `recall.select.trim_chars` | the longest excerpt a pointer carries in place of its first line, in characters (default 90, the length of a first-line pointer) |
| `recall.select.lane_chars` | the longest excerpt of an older lane entry (decision, measurement, commit, note) in a hook's lane brief (default 180; 0 sends them whole). The handoff, brief, open questions and next step are always whole |
| `cap` | the context-cap guard's settings |
| `cap.warn_tokens` | the handoff point: an agent past this is told to wrap up and write its handoff at its next clean stopping point. It applies to every agent kind without its own `handoff`, and to the main session (default 285,000 scaled to the context window) |
| `cap.block_tokens` | the hard limit: past this every tool but `handoff` is denied. It applies to every agent kind without its own `limit`, and to the main session (default 450,000 scaled to the context window) |
| `cap.warn_every` | the warning repeats after this many more tokens (default 25,000) |
| `cap.main_session` | apply the cap to the main session too (default false) |
| `orchestrate` | orchestrator mode: caps on how many agents of each kind run at once, and per-kind context limits (see [[Orchestration]], the `orchestrate` skill and `docs/design/orchestrate.md`) |
| `orchestrate.enabled` | turn the agent caps on (default false). Context limits apply without it. `atlas --setup` and `atlas settings orchestrate` ask for it and store the answers |
| `orchestrate.window` | the context window the limits scale to (default 0: `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, else 200,000 when `CLAUDE_CODE_DISABLE_1M_CONTEXT` is set, else 1,000,000) |
| `orchestrate.stale_minutes` | an unfinished agent whose transcript has not changed for this long is not counted (default 360) |
| `orchestrate.resources` | the work queue's shared resources and how many items may hold each at once, e.g. `{"gpu": 1, "compile": 1}` (the default). `worktree:NAME` allows 1; any other name not listed is not a limit and only warns at launch; 0 never serializes |
| `orchestrate.resources.compile` | compile slots (default 1). On yes, setup writes max(1, hardware threads / 16), at most 4, unless you answer `--compile N` or it is set |
| `orchestrate.resources.gpu` | gpu slots (default 1). Setup writes 1: `gpu` is a lock and atlas assigns no device, so nothing is probed |
| `orchestrate.aging_minutes` | minutes a queued item waits before it rises one priority class (default 60; 0 turns aging off) |
| `orchestrate.bare_gpu_words` | the bare words capture, screenshot, bench, benchmark and bake in a brief infer the gpu (default false). A project file (`atlas.project.json`) may set it for its project, and sets no other `orchestrate` key |
| `orchestrate.reviews` | the required-reviews merge gate: a `gh pr merge` in a shell command needs a passing review of the PR's head from each role (see `docs/design/reviews.md`) |
| `orchestrate.reviews.roles` | the review roles every PR needs, named by job, e.g. `["simplicity", "cost"]` (default none: the gate is off) |
| `orchestrate.reviews.mode` | `enforced` denies the merge, `advisory` adds a warning, `off` does nothing (default `advisory`) |
| `orchestrate.judge` | the task judge: a model and effort pick per agent launch from a local cells file (`note` op `judge`; see `src/taskjudge.rs`) |
| `orchestrate.judge.enabled` | at each admitted Agent launch, add one advisory line when the launch's model or its `Judge:` line differs from the pick, and log the pick to `judge-picks.jsonl` beside the memory store; never denies or rewrites a launch (default false) |
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
| `compact` | compaction: the checkpoint card and deferral (see `docs/design/compaction.md`). On by default |
| `compact.enabled` | the compaction features: the PreCompact checkpoint with its chain card and the instructions it returns for the summary, the SessionStart(compact) card and ref report, `atlas settings compact-md`, and `compact.defer` (default true; `false`, or `atlas settings off compact`, turns them off). Off, PreCompact only forgets the session's repeated answers (and, with `memory.transcripts`, indexes the transcript) and returns no text, and `recall` refuses a chain key unless `memory.transcripts` is on |
| `compact.card_tokens` | the budget of the checkpoint PreCompact writes and SessionStart(compact) injects, not counting the chain card appended to it (default 400) |
| `compact.defer` | block a proactive automatic compaction while a tool chain is running, until the next quiet point, never past the ceiling (the window less three times the largest per-request growth, at least 40,000 tokens); a manual `/compact` is never blocked (default false, pending a live check that a blocked compaction is offered again; needs `compact.enabled`) |
| `compact.window` | the context window `compact.defer` measures against (default 0: 1,000,000 once the session has held more than 200,000 tokens, else 200,000) |

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

## Routing, handles and records (`lean`)

`lean` is an object read by the hooks, `atlas run`, `atlas slice` and the record commands. See
[[Handles and digests]].

| Key | Meaning |
|---|---|
| `lean` | the object below |
| `lean.routing` | false turns routing off (default true; `atlas settings off routing` writes a flag with the same effect) |
| `lean.pr_digest` | digests `gh pr diff` and `gh pr view` output per file and per comment, with rounds; false sends it the ordinary route, as before v0.4.0 (default true; see [[Handles and digests]]) |
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
