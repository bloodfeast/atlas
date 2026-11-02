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
| `code.rust_analyzer` | the rust-analyzer binary (default: `rust-analyzer` on `PATH`, else a rustup toolchain's `bin`; each must answer `--version`) |
| `code.timeout_s` | a rust-analyzer run is killed past this many seconds (default 1800) |
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

## Recall and the context cap

See the Hooks section of [[Memory store and hooks]]. The floors are set by `cargo xtask calibrate-recall`
and `atlas --hook-report --tune-floor`, never by hand.

| Key | Meaning |
|---|---|
| `recall` | the recall gate's settings |
| `recall.budget_tokens` | the most a recall hook injects per prompt (default 600) |
| `recall.lane_budget_tokens` | the budget of `recall {lane}` (default 2,000) |
| `recall.fts_floor` | the FTS5 score a record must clear without the embedding model |
| `recall.hybrid_fts_floor` | the FTS5 score a record must clear with the embedding model loaded |
| `recall.min_terms` | the prompt words a record must contain to pass lexically (default 3) |
| `recall.cos_floor` | the cosine a record must also clear with the model loaded |
| `recall.cos_alone` | a cosine at or above this passes on meaning alone |
| `recall.query_tokens` | the prompt tokens the cosine is taken over (default 32) |
| `recall.inject_floor` | the cosine below which a hook injects a record not at all (records without a vector are not held to it); tuned on hit data by `atlas --hook-report --tune-floor 1` |
| `cap` | the context-cap guard's settings |
| `cap.warn_tokens` | a subagent past this is told to finish and write its handoff (default 450,000) |
| `cap.block_tokens` | past this every tool but `handoff` is denied (default 500,000) |
| `cap.warn_every` | the warning repeats after this many more tokens (default 25,000) |
| `cap.main_session` | apply the cap to the main session too (default false) |

## Routing, handles and records (`lean`)

`lean` is an object read by the hooks, `atlas run`, `atlas slice` and the record commands. See
[[Handles and digests]].

| Key | Meaning |
|---|---|
| `lean` | the object below |
| `lean.routing` | false turns routing off (default true; `atlas settings off routing` writes a flag with the same effect) |
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
