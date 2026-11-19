# Reference configuration

A complete, working configuration for using atlas as Claude Code's memory: recall on every prompt and at every
subagent start, a versioned copy of the memory folder, a generated `MEMORY.md`, compaction and agent limits. It
is the configuration atlas is developed with, with names, paths and personal values replaced by placeholders.
Start from it when Claude cannot work out how to use atlas as its memory, or when memory looks dead.

The files are in the repository under [`examples/reference/`](https://github.com/bloodfeast/atlas/tree/main/examples/reference).
A test loads them through atlas's own configuration parser with the placeholders filled in, so every key in them
is one the current release accepts. Every key is described in [[Configuration reference]].

## Files

| file | goes to | notes |
| --- | --- | --- |
| `atlas.json` | atlas's configuration file (`ATLAS_CONFIG`, `--config`, or the user configuration folder; see [[Install and setup]]) | fill in the placeholders; keep the `models` block of the configuration your first run wrote |
| `atlas.project.json` | the workspace root, where `projects[].file` names it | one entry per repository in the workspace ([[Projects and multi-repo]]) |
| `.claude/settings.json` | `<WORKSPACE>/.claude/settings.json` | lets the atlas MCP tools run without a permission prompt |

Install atlas as the Claude Code plugin ([[Install and setup]]), not as a bare binary. The plugin brings the
`mcp_tool` hooks (`hooks/hooks.json`) and the `atlas` and `orchestrate` skills. Recall reaches a session only
through those hooks: `UserPromptSubmit` in the main session, `SubagentStart` for subagents
([[Memory store and hooks]]). A command hook (`atlas --hook`) cannot give a subagent its lane recall; use the
plugin's `mcp_tool` hooks.

## Using atlas as Claude's memory

atlas does not replace Claude Code's memory folder. Claude Code still writes the memory files; atlas indexes
them, keeps every version in its store, recalls from them, and (with `memory.tree`) writes the `MEMORY.md` that
Claude Code loads.

Four pieces must line up. With any one missing, memory looks dead.

1. **A source on the memory folder.** In `projects[].sources`, the entry named `memory`:
   - `root`: Claude Code's memory folder for the workspace, `~/.claude/projects/<folder>/memory`, where
     `<folder>` is the workspace's absolute path with every character that is not an ASCII letter or digit
     turned into `-` (separators, `:`, `.`, `_` and spaces alike): `/home/me/work` becomes `-home-me-work`, and
     `C:\work\my_app` becomes `C--work-my-app`. Write the absolute path. List `~/.claude/projects/` to get the folder name
     right: a wrong root is the most common failure, and nothing reports it.
   - `names_kind: "memory"`, so each memory's file name becomes an identifier ([[Identifiers and freshness]]).
   - `id_keys: ["name"]`, since a memory's id is its frontmatter `name:`.
   - `exclude: ["index/*"]`, so the hub files the tree writes are not read back as memories.
2. **An export with the same name.** `memory.exports` holds `{"name": "memory", "shape": "memory", ...}`.
   An export whose name is also a configured source is synced from it: every rescan, and every `recall`, re-imports
   a changed file, and a changed memory file becomes a new version of the same key (`recall {key, history}`), so
   history is kept ([[Memory store and hooks]]).
3. **The tree, optional.** `memory.tree` makes atlas write `MEMORY.md` and the hub files (in `memory.tree.hub_dir`,
   `index` by default) into the memory folder, grouped by `branches[].words`, with pinned memories in the root.
   Back up `MEMORY.md` first; the first rebuild also saves a hand-written root to `index/MEMORY.md.before-tree`.
   Leave `memory.tree` out to keep a hand-written `MEMORY.md` ([[Memory tree]]).
4. **Rules on.** `atlas settings set memory-rules true` (the file already has `"memory": {"rules": true}`). It is
   experimental and off by default. With it on, atlas also reads the project's Claude Code memory folder and the
   first `CLAUDE.md` at the working folder or a project root, and a query that hits a replaced ruling answers
   with the ruling that replaced it ([[Memory and rule files]]).

Then check it:

```sh
atlas --config <atlas.json> --rescan          # index and sync; prints stats, and what each sync did under memory
atlas --config <atlas.json> --memory-check    # hash chains, exports on disk, the round trip
atlas doctor                                  # the atlas hooks in Claude Code's settings match the plugin's
```

In a session, `recall {query: "<words from a memory>"}` should return that memory, and a prompt that uses a
memory's words should get a recall block from the hook. If the tool answers but no prompt ever gets a block, the
hooks are missing: run `atlas doctor`.

For the habits (recall before asking, journal and hand off to lanes), `atlas settings rules-template <WORKSPACE>`
offers a starter `CLAUDE.md` and `.claude/atlas-rules.md`, and writes nothing when the folder already has rules.
Do not add a Compact instructions block while `compact.enabled` is on: atlas's PreCompact hook already returns
that text ([[Memory and rule files]]).

## Placeholders

| placeholder | meaning |
| --- | --- |
| `<ATLAS_HOME>` | the folder for atlas's `state/` and `exports/` (for example atlas's data folder) |
| `<WORKSPACE>` | the folder holding your repositories: the project root |
| `<CLAUDE_MEMORY_DIR>` | Claude Code's memory folder for the workspace, from step 1 above |
| `<project>`, `<Project>` | your project's name |
| `<repo>` | a repository folder name in the workspace |
| `<repo folder, relative to the workspace>` | that repository's path in the project file |
| `<owner>` | the repository's owner on its Git host |
| `<one line: what this repo is>` | the repository's role, one line |
| `<Obsidian vault folder>` | a notes vault folder, if you keep one; else drop `vaults` |
| `<one line: what memories on this branch are about>` | the branch's line in `MEMORY.md` |
| `<keyword>`, `<prefix*>` | words that file a memory on the branch; `*` matches any ending |
| `<your first name>` | in `memory.tree.name_stop`, lowercase: a word that never names a hub; in `backlog.asker`, as you write it: the name that leads your own asks |

## What was left out

- The project's own vaults, sources, identifier patterns and tree branches: each is one example. Keep the generic
  branches (`atlas`, `workflow`, `machine`, `me`) and add your project's own.
- `memory.tree.pins` is empty: pins name your own memory slugs.
- `semantic` and `code.rust_analyzer`: the defaults find the embedding model from the `models` block and
  rust-analyzer on `PATH`.
- Keys that point at tools on one machine only.

## Settings worth knowing

- `recall.*`: floors calibrated on a long-running store. Start with them, and recalibrate on your own store if
  recall is too chatty or too quiet.
- `orchestrate.resources`: the example's `{"gpu": 1, "compile": 1}` are the defaults. Size them to your
  machine: setup writes 1 GPU slot and compile slots from the hardware threads ([[Configuration reference]]).
- `recall.subagent_context: "binding"` gives a subagent only the records its brief names and its lane's newest
  handoff and decisions. The default is `"recall"`.
- `orchestrate.kinds`: at most 6 builders, 4 reviewers and 2 research agents at once; each is told to hand off
  from 285,000 context tokens (`handoff`), and past 500,000 (`limit`) every tool but `handoff` is denied
  ([[Orchestration]]).
- `cap`: the kinds above hand off at their own `handoff` (285,000). `cap.warn_tokens` (450,000) and
  `cap.block_tokens` (500,000) apply to kinds without their own `handoff` and `limit`, which are told to hand
  off at 450,000 and blocked at 500,000, and to the main session, whose compact point is `cap.warn_tokens`
  while `cap.main_warn_tokens` is 0. `cap.main_session: false` turns off the main session's advice to ask for /compact.
