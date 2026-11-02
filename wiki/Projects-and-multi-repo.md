One install serves several projects, each scoped: a project is a named set of roots (repositories, vaults,
sources, code roots, a memory folder, a rules file) with its own index database and memory store. Recall, the
hooks, lanes and the journal, the decision queue and `next_id` see only the active project's records. A user
layer is shared: the `user.memory` folder, and any memory marked `scope: user` in another project's memory
folder, is imported into every project (a project's own memory of the same name wins). A configuration without
`projects` is one implicit project, as before.

## Quickstart: one repository

Open Claude Code in the repository. The first session asks once, in one line, whether to add the folder; yes
runs `atlas settings add-project`, which adds the folder's Markdown as a source and, when it holds Rust or
WGSL, the folder as a code root. To do it by hand:

```sh
atlas settings add-project .
```

## Quickstart: a folder of repositories

Open Claude Code in the folder that holds the repositories (each one level down, with its own `.git`). The
first session offers, in one line: *new project here with these N repos / add to an existing project / skip*.
New writes `atlas.project.json` in the folder, listing each repository's path and remote, and registers the
project in your user configuration (`{name, file}` under `projects`), so a session opened inside one of its
repositories' checkouts elsewhere resolves to it too. Commit the file so everyone working on the project gets the
same layout. A folder that is itself a git checkout offers only its child repositories; its own checkout is a
member only when listed in `repos` by hand. By hand:

```sh
atlas settings add-project --new .            # a new project from the repositories here
atlas settings add-project --to NAME .        # or add them to project NAME
atlas settings undo                           # one undo removes the file and the registration
```

Then describe how the repositories relate, in `atlas.project.json`, with `role` and `talks_to` (below).

## The configuration

```json
{
  "projects_dir": "state/projects",
  "user": {"memory": "D:/notes/how-i-work"},
  "default_project": "games",
  "checkouts": {"git@git.example:acme/ledger.git": "D:/src/ledger"},
  "projects": [
    {"name": "games", "roots": ["D:/dev"], "db": "state/atlas.sqlite",
     "vaults": [{"name": "notes", "path": "D:/dev/notes"}], "memory_dir": "D:/dev/memory", "intent": "D:/dev/INTENT.md"},
    {"name": "acme", "file": "D:/work/acme/atlas.project.json"}
  ],
  "models": {"files": []}
}
```

| Key | Meaning |
|---|---|
| `projects[]` | `name`; `roots` (folders whose insides make it active); `repos` (as in the project file); `file` (a project file merged in); `memory_dir`, `intent`; and any of `db`, `vaults`, `sources`, `ids`, `code`, `memory`, `wikilinks`, `vault_id_keys` for the project alone. Top-level vaults, sources, ids and code roots are in every project |
| `user.memory` | the user layer's memory folder (with its `MEMORY.md` index) |
| `default_project` | the project active where no project root holds the working directory |
| `checkouts` | remote URL to local checkout, for repositories a project file lists by remote. ssh and https forms of one remote are equal |
| `projects_dir` | where a project without `db` keeps its database and memory store: `<projects_dir>/<name>/` (default `projects/` in the user's data folder) |

Every key is also listed on [[Configuration reference]].

## The project file

`atlas.project.json` at a project root, written to be committed (it may also live in one member repository, or
in a meta repository, and be named by a `projects[].file` entry):

```json
{
  "name": "acme",
  "repos": [
    {"name": "billing", "path": "billing", "remote": "git@git.example:acme/billing.git",
     "role": "invoices and payments", "talks_to": ["auth", "ledger"]},
    {"name": "gateway", "path": "gateway", "role": "public API edge", "talks_to": ["billing"]},
    {"name": "ledger", "remote": "https://git.example/acme/ledger"}
  ],
  "memory_dir": "docs/memory",
  "intent": "docs/INTENT.md"
}
```

A repository is listed by `path` (relative to the file), by `remote`, or both; the remote is identity only,
read from `.git/config` and never fetched. Its checkout is the `checkouts` entry for its remote, else its path; a
repository found neither way is reported once at session start and left out. Each located repository is a
source (its Markdown) and a code root (Rust, WGSL, and `.proto` files, see [[Code index]]).

`role` and `talks_to` are optional: a prompt that names a repository gets one line from the graph (its role,
what it talks to and depends on, what uses it), `lookup NAME` answers the same, and recall puts records about
the session's active repository and its neighbours first. The active repository is the one the session's tool
calls touched most recently and most (file paths in hook events); the working directory is the fallback.
Recall's boost follows a record's tag, not its words: `repo: NAME` in a memory's frontmatter, else the `repo`
its `memory.tree` branch maps to. Records about the active repository come first, then its neighbours'; an
untagged record gets no boost. Moving a memory to another repository is a frontmatter edit, which the sync turns
into a new version of the same record.

`journal_status` and `decision_queue` take a `repo` filter: one repository of the active project and the
repositories it talks to or is used by. A lane is kept when its name or one of its journal or handoff entries
names a covered repository as a whole word; a thread when its file does. An unknown repository is an error that
lists the project's repositories, as is a `repo` filter where no project with repositories is active.

## Which project is active

In order: `--project NAME` (or `ATLAS_PROJECT`); the deepest project root holding the working directory
(configured roots, project file folders, located repositories; a project file at or above the working directory
counts); `default_project`; else none (the user layer alone, in `<projects_dir>/_none/`). The server resolves once
at start; a command hook resolves from its event's folder. Resolving takes one `stat` per parent folder and is
cached per process.

## Moving an existing setup into a project

`atlas settings migrate-projects NAME` moves `db`, `vaults`, `sources`, `ids`, `code` and `memory` into project
NAME (an absent `db` is written as the path it resolved to, so the database and memory store stay where they
are), sets its root to the deepest folder holding its vaults and sources, and makes it `default_project`, so
every session resolves as before. A backup is taken first (`atlas settings undo` restores it), and running it
again changes nothing.

## The user layer

Memories that hold in every project (how you work, your preferences) live in the user layer: the `user.memory`
folder, plus any memory marked `scope: user` in a project's own memory folder. Every project's sync imports them
with scope `user`, so `recall scope=user` finds the whole layer in any project. A project's own memory of the
same name wins.

`remember` with `scope: "user"` writes to the user layer instead of the current project: the file and its index
line go into the `user.memory` folder, and the other projects import it at their next sync. A revision or
supersede of a user-layer record follows it there. Hand-written index lines are kept byte for byte, and a file
the store has not read is never written over. Scope `user` is for memories only, and an error where no user
layer is configured.

Some state is per user, not per project: the asked-once offer state (settings offered, folders asked about,
unmapped repositories reported) and the routing switch live in `projects_dir`, so one answer and one switch serve
every project. They are moved there once from the per-project files at the first start that finds none.

## The memory tree per project

Each project gets its own memory tree, in its own memory folder: its own `memory.tree`, else the user-level one
pointed at the project's memory export. The user layer never gets a tree; its memories reach every project
through the sync. Pins have a scope: the user layer's pins are listed in every project's root, and a project's own
pins only in its root. See [[Memory tree]].
