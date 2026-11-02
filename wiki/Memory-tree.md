Claude Code loads the memory folder's index (`MEMORY.md`) into every session, one line per memory, and cuts it at
200 lines. `memory.tree` turns that index into a tree that every memory stays reachable through
(`src/memory/tree.rs`):

- **The root** (the index file) lists the top-level branches, then the pinned memories (their line, and their body
  when it is at most `pin_body_chars` long). It is always loaded, so it stays short.
- **A branch** is a hub file in `<memory folder>/<hub_dir>/` that lists its memories, one line each
  (`- slug: hook`, or `- [Title](../file.md) - hook` with `line_format: link`). A branch past `hub_max` lines is split by topic (k-means over the
  embedding model's vectors; hashed word vectors without a model) into hubs of about `hub_target`, and then lists
  those. Every memory is at most three reads from the root: branch, hub, file.
- **Branches are a fixed taxonomy** in the configuration. A memory is placed once, when it first appears: by
  `branch: NAME` in its frontmatter, else by meaning and words together. Each branch scores it by the mean cosine
  with its five nearest members (its listed memories and those its words claim confidently) plus
  `keyword_weight` (0.5) times its word score over the memory's best one (three points per branch word in its file
  name, one per word in its description, two when its `metadata.type` is in the branch's `types`). On a hand-labelled
  set of 227 memories this placed all but 20, against 21 for words first and 36 for meaning alone; the rest were
  judgement calls, fixed with `branch:`. A memory is pinned (in the root as well as its branch) by `pinned: true` or
  by being named in `pins`.
- **A memory's line** is its title (from the line a writer added to the root, or the line its hub holds, or its
  `name`) and its frontmatter `description` as the hook, cut at a whole word to `line_max` with no trailing
  punctuation or dangling word. A file with no description keeps its index line.

Rebuilds are stable, and the files are the tree's only state: a memory stays in the hub that lists it; a split
keeps the hub's file name for its largest part and names only the new parts; a part that falls under `hub_min` is
merged into its nearest sibling; no untouched hub is rewritten. A rebuild with nothing changed writes nothing, and
a new memory changes only the hubs on its path (usually one file). Every rescan that syncs the memory source
rebuilds the tree first, so a memory file that appears is filed on the next rescan; with the tree on, the sync
reads every memory's line from the hubs, so the store's titles and the export are unchanged by the switch.

```json
"memory": {
  "tree": {
    "export": "memory",
    "hub_dir": "index", "hub_max": 24, "hub_target": 20, "hub_min": 8, "line_max": 150, "pin_body_chars": 400,
    "line_format": "slug", "name_stop": ["acme"], "keyword_weight": 0.5, "pins": ["deploy-credentials"],
    "branches": [
      {"name": "game-art", "title": "Game: art", "hook": "models, rigs, textures", "words": ["mesh*", "rig", "texture*"]},
      {"name": "workflow", "title": "Workflow", "hook": "how agents work here", "words": ["agent*", "git"], "types": ["feedback"]}
    ]
  }
}
```

`line_format` is `slug` (the default: `- slug: hook`; the root says once that a memory is `slug.md` beside it and a
hub `index/slug.md`; shorter, and a memory's title is not shown) or `link` (`- [Title](file.md) - hook`, a Markdown
link the harness, Obsidian and people follow). Switching to `slug` gives every memory record one new
version on the next sync, as its title becomes `- [slug](slug.md) - hook`. `name_stop` adds lowercase words that
never name a hub (the user's or the project's name, say) to the built-in ones.

Every key is optional: `export` defaults to the first `memory` export, and without `branches` there is one
branch per memory type (rules, about the user, project facts, references). A branch name is its hub's file name
(lowercase letters, digits and `-`); a word ending in `*` matches any ending. Add `"exclude": ["index/*"]` to the
memory source so the hubs are not indexed as documents.

```sh
atlas --config C --memory-tree [DIR]            # rebuild the tree in DIR (default: the memory source) and check it
atlas --config C --memory-tree [DIR] --check    # write nothing; report what a rebuild would change, and check it
cargo xtask memory-tree-check --config C [--dir DIR]   # the navigation and stability check, as CI would run it
```

The check walks the tree as a reader would: every memory file within three reads of the root, every pinned memory
in the root, no memory listed by two hubs (a pinned one also by the root), no generated hub that no link reaches,
no dangling link, no hub over `hub_max`; and a rebuild of what is on disk would write nothing. It prints the root's
size and the mean size of the root plus the hubs on the way to a memory.

**Switching a live index over.** The tree is written into the memory folder itself, the one Claude Code reads:

1. Back up the folder (`MEMORY.md` at least). The first rebuild also saves a hand-written root to
   `<hub_dir>/MEMORY.md.before-tree` before replacing it.
2. Add `pinned: true` to the frontmatter of each memory to pin, and list them in `memory.tree.pins` too: a pin in
   either place counts, and `--recall-check` reports (as `memory pin` drift) a configured pin whose file has lost
   `pinned: true`, so a memory writer that rewrites the frontmatter is caught.
3. Add `memory.tree` (and the source's `exclude`) to the configuration, then run
   `atlas --config C --memory-tree --check` to see the churn, and `atlas --config C --memory-tree` to build.
   `cargo xtask memory-tree-check --config C` must report no problems.
4. Restart the server (or let the next rescan do it): from then on every rescan rebuilds the tree.

To go back, remove `memory.tree` and restore `MEMORY.md` from the backup; the memory files are never touched.

## Per project

With [[Projects and multi-repo]], each project gets its own tree: its own `memory.tree`, else the user-level one
pointed at the project's memory export. The tree is built in the folder that export syncs from, so hubs live in
each project's own `index/` folder and no two projects share one. A top-level `memory.tree` in a configuration with
projects names no export until a project resolves it. The user layer never gets a tree (a tree naming the user
export is refused); its records reach every project through the sync.

Pins have a scope. A pinned memory of the user layer (the `user.memory` folder, or one marked `scope: user` in
another project's folder) and the user-level `memory.tree.pins` are shared: every project's root lists them
under their own heading, with the file's path (`* ` lines, which no tree reader takes for a memory of the folder).
A project's own pins stay in its own root. `--recall-check` reports a user-level pin found in neither the project
nor the user layer as `memory pin` drift.

A branch may name a repository (`memory.tree.branches[].repo`): a memory filed under it is tagged with that
repository for recall's boost, unless its frontmatter says `repo:` itself.
