## Install

atlas is a Claude Code plugin. The plugin carries prebuilt binaries for Windows x86_64, Linux x86_64 and
aarch64, and macOS (Apple Silicon and Intel), as the release notes list them; a platform a release lacks
builds from source (below).

Claude Code version: no minimum is established. The measurements these pages cite were taken on Claude Code
2.1.316 and 2.1.318; the hooks other than SessionStart are `mcp_tool` hooks, and the always-loaded tool schemas need 2.1.285 or later.
An older Claude Code is untested.

1. Add the marketplace and install the plugin, in Claude Code:

   ```
   /plugin marketplace add bloodfeast/atlas
   /plugin install atlas@atlas
   ```

2. Restart Claude Code. On its first start the server:
   - writes a starter configuration, `atlas.json`, in your configuration folder, with one empty vault
     `notes` beside it;
   - keeps its database and memory store in your data folder;
   - downloads the embedding model (about 35 MB) in the background, resuming if interrupted, and checks it
     against the SHA-256 pinned in the configuration. Until it is in place, search's `hybrid` mode answers
     lexically and the recall hook is lexical; both pick the model up when it lands, without a restart.

   | OS | Configuration folder | Data folder (database, memory store, models) |
   |---|---|---|
   | Windows | `%APPDATA%\atlas` | `%LOCALAPPDATA%\atlas` |
   | macOS | `~/Library/Application Support/atlas` | the same folder |
   | Linux | `$XDG_CONFIG_HOME/atlas` (`~/.config/atlas`) | `$XDG_DATA_HOME/atlas` (`~/.local/share/atlas`) |

   `ATLAS_HOME`, when set, is both folders. The plugin's own folder is never used for state: Claude Code
   replaces it on every update.

3. Optionally, run the setup in the foreground to see each step (the plugin's `bin` folder is on the Bash
   tool's `PATH`, so Claude can run it for you):

   ```sh
   atlas --setup
   ```

   It writes the configuration if there is none, creates the data folder, downloads or re-verifies every
   model file, and prints where everything is. Run it again after an interrupted download to resume. It
   needs `curl`, which ships with Windows 10 and later, macOS and practically every Linux distribution.

   A new session in a folder atlas does not index asks, through Claude, in one line: *create a
   vault for this project at `<path>`? yes / other folder / no*, yes being the default. `atlas --setup` asks
   the same in the terminal for the folder it runs in (`[Y]es / [o]ther folder / [n]o`; Enter is yes). Yes
   runs `atlas settings add-project --vault`, which adds the folder's Markdown as a source (`**/*.md`,
   without `target` and `node_modules`) and, when it holds code, the folder as a code root, then creates the
   project's vault and registers it under `vaults`, and indexes the source; the code root is indexed when
   the server next starts. The vault goes to `vaults/<name>-notes` beside atlas's configuration file (where
   the first run's `notes` vault is), so atlas's notes and discussion threads never land in the project's
   repository. It starts with one `README.md` note; open the folder in Obsidian with *Open folder as vault*.
   Other folder runs `atlas settings add-project --vault-at FOLDER` instead: an existing folder (an Obsidian
   vault you already have) is registered as it is, nothing written into it, and a folder inside the project
   is left out of the project's source so no note is indexed twice. No runs `atlas settings add-project --no DIR`, which records the
   answer and changes nothing else. A folder is asked until it is answered: nothing is recorded when the question is
   put, because Claude does not always relay it, and a session with no user runs nothing. So someone who never answers
   hears it at each new session (a start or `/clear`, not a resume or a compaction) in that folder, and the
   orchestrated mode question at each new session in any folder. `atlas settings add-project
   [--vault | --vault-at FOLDER] DIR` adds one any time (`.` for the current folder), and `--vault` on an
   indexed project adds its vault alone. `--dry-run` shows the change, the file and the project it would go
   into, and writes nothing; so does `add-project` with no folder named (pass `.` or `--yes` to write). A
   folder a project already covers (a project root, its project file's folder, a repository) counts as
   indexed, so it is not asked about. Without a terminal, `atlas --setup` only prints the command and records
   nothing, and says so: nothing went wrong, there was no terminal to ask in. In auto mode, Claude Code's
   classifier may block `atlas --setup` and `atlas settings` commands that Claude runs (the reason reads "No
   reason provided") until you tell Claude you allow them.

   In a configuration with `projects`, add-project writes into the project the folder belongs to (its
   `projects[]` entry: `sources`, `code.roots`, `vaults`), never the top level, whose lists every project
   reads; `atlas --project NAME settings add-project DIR` picks the project, else the folder's own, else
   `default_project`. For a folder in no project, with none named, a
   terminal lists the configured projects (even one) and cancel, and writes into the one picked; without a
   terminal, or with `--dry-run`, it is refused before anything is written.
   Every check runs on the text that would be written before it is written: a refused add-project leaves
   the configuration byte for byte as it was.

   The source, the code root and the vault are one configuration change, backed up, and `atlas settings
   undo` reverts the newest change whole. Undo unregisters the vault and says what it did with the folder:
   a folder atlas created that still holds only its `README.md` is removed; a folder holding anything else,
   or one that was there before, is kept. Notes are never deleted.
   Backups of atlas's configuration go to `atlas-backups` beside the configuration file; backups of Claude
   Code's `settings.json` go to `atlas-backups` in Claude Code's folder. Undo finds each in its own place.
   A change holds `atlas.json.lock` beside the configuration file, or `settings.json.lock` in Claude Code's
   folder, so two sessions cannot lose each other's write; both are empty and stay there.
   Undo first shows what it would revert (when the change was made, which command made it, and for each file
   what goes back, with line counts) and asks; without a terminal it reverts only with `--yes`, so a command
   that changed nothing (and made no backup) never leads undo to revert an older change unseen.
   A folder that holds several git repositories is offered as a multi-repository project instead; see
   [[Projects and multi-repo]].

   Setup also asks whether you want to work in **orchestrated mode**, for sessions that delegate their work
   to subagents: atlas caps how many agents of each kind run at once and tells each agent to hand off near its
   context limit (see the `orchestrate` skill and [[Memory store and hooks]]). Nothing is on until you say
   yes. On yes it asks each option, with the value in effect in brackets (Enter keeps it):

   | Question | Stored as | Default |
   |---|---|---|
   | Compile slots (heavy compiles at once) | `orchestrate.resources.compile` | max(1, hardware threads / 16), at most 4 |
   | GPU slots (GPU runs at once) | `orchestrate.resources.gpu` | 1 |
   | Builders at once | `orchestrate.kinds.builder.max` | the compile slots |
   | Reviewers at once | `orchestrate.kinds.reviewer.max` | the builders |
   | Research and design agents at once | `orchestrate.kinds.research.max` | max(1, builders / 2) |
   | Context limits: enforced, advisory or off | `orchestrate.mode` | enforced |
   | Handoff point, in context tokens | `cap.warn_tokens`, only when you type a number | 285,000 on a 1M window, scaled to the window |
   | Hard limit, in context tokens | `cap.block_tokens`, only when you type a number | 450,000 on a 1M window, scaled to the window |
   | After a handoff, start the fresh agent: auto, or ask you first | `orchestrate.fresh` | auto |

   The defaults of the first five are starters: compile from this machine's threads, gpu 1, and the
   caps from the compile slots; a value already in the configuration is kept, and the caps are clamped
   to Claude Code's subagent limit. See [[Orchestration]] for the rules and for declaring lanes. The handoff point and the hard limit are skipped when context limits are off. The answers are one
   configuration change, backed up, which `atlas settings undo` reverts. The first Claude Code session asks
   the same once, through Claude, unless the configuration already sets `orchestrate.enabled`. To answer
   again at any time:

   ```sh
   atlas settings orchestrate                       # asks in a terminal; without one, lists the options
   atlas settings orchestrate --yes --compile 2 --builders 3 --mode advisory   # answers without a terminal
   atlas settings orchestrate --no                  # declines (and turns the caps off if they are on)
   ```

   Setup never asks about [[Memory and rule files]], which is experimental and off by default. Turning it
   on is a deliberate `atlas settings set memory-rules true`.

   `--yes` takes `--builders N`, `--reviewers N`, `--research N`, `--mode enforced|advisory|off`,
   `--handoff TOKENS`, `--limit TOKENS` and `--fresh auto|ask`; an option left out keeps the value in effect,
   and a handoff point above the hard limit is refused. A no writes nothing to the configuration.

4. Updates are on. At each session start atlas asks whether a newer release exists (at most once an hour),
   checks the release manifest's ed25519 signature against the public key built into atlas, downloads the
   binary for your platform, checks its SHA-256 against the signed manifest and swaps it in, then starts it.
   The check waits at most 10 s; past that the installed version starts and the check finishes in the
   background for the next start. Offline, or on any error, the installed version starts and the reason is
   logged as a miss. A newer version is never replaced by an older one. To turn updates off:

   ```sh
   atlas settings off update
   ```

   (`atlas settings on update` turns them back on), or pin a version in `atlas.json` with
   `"update": {"pin": "0.5.0"}`. A binary you build from source never updates itself.

5. Point atlas at your notes: edit `atlas.json` (see [[Configuration reference]]) to add vaults (folders atlas may add
   to) and read-only sources (folders it only indexes), then run `/mcp` and reconnect atlas, or restart
   Claude Code. To keep the model download off entirely, set `"models": {"auto_download": false, ...}`;
   recall then stays lexical.

To build from source instead (any platform with a Rust toolchain):

```sh
cargo build --release --locked
```

and copy `target/release/atlas` (`atlas.exe` on Windows) into the plugin's `bin` folder as
`atlas-<arch>-<os>` (for example `atlas-aarch64-unknown-linux-gnu`), or as `atlas.exe` on Windows.

### Uninstall

1. In Claude Code: `/plugin uninstall atlas@atlas`, then `/plugin marketplace remove atlas`.
2. atlas's own folders are kept on purpose (the memory store is yours). To remove them too, delete the
   configuration folder and the data folder from the table above. In Claude Code's folder atlas leaves only
   what the settings commands wrote there: `atlas-backups` and `settings.json.lock`.

## Build and run

```sh
CARGO_TARGET_DIR=<target dir> cargo test -j 4
CARGO_TARGET_DIR=<target dir> cargo install -j 4 --path . --root . --locked   # -> bin/atlas(.exe)
```

Run the installed copy in `bin/`, not the one in the target folder, so a rebuild never fights a running
server for the locked executable.

```sh
atlas --config atlas.json                 # serve MCP on stdio
atlas --config atlas.json --rescan        # bring the index up to date, print stats, exit
atlas --config atlas.json --recall-check  # compare the index with a plain scan of the same files
atlas --config atlas.json --code-index    # index the code roots now (runs rust-analyzer; see the Code index page)
atlas --config atlas.json --embed 8       # embed every section for semantic search on 8 threads
atlas --vault NAME=PATH [--vault ...] [--db PATH]   # the original flags still work, with or without --config
```

The measuring modes (`--bench`, `--judge`, `--symbol-bench`, `--recall-scores` and the `--runs`, `--modes`,
`--queries` and `--before` options only they read) are built only with the `dev` cargo feature, which is off by
default and in release builds; a release binary does not know them. Build a development binary for them:

```sh
CARGO_TARGET_DIR=<target dir> cargo build --release --features dev
<target dir>/release/atlas --config atlas.json --bench 500   # print the timings below
```

Without `--config` and without `--vault`, the binary reads the config named by `ATLAS_CONFIG`, else
`atlas.json` in the folder above the binary, else `atlas.json` in the user's configuration folder (see
Install), which the first run writes from `config/atlas.template.json` when it does not exist. `--db`
overrides the config's `db`; with neither, the database is `state/atlas.sqlite` in the user's data folder.

Two file tools, `atlas digest` and `atlas crop`, need no configuration; see [[Handles and digests]].

### As a Claude Code plugin

`.claude-plugin/plugin.json` names the plugin, `.claude-plugin/marketplace.json` makes the repository its own
marketplace, and `.mcp.json` launches `${CLAUDE_PLUGIN_ROOT}/bin/atlas` with no arguments, so it finds its
configuration through `ATLAS_CONFIG`, `atlas.json` at the plugin root (gitignored), or the user's configuration
folder. In a release, `bin/` holds `atlas.exe` (Windows resolves `bin/atlas` to it) and, for macOS and Linux, a
small `sh` launcher named `atlas` that starts `atlas-<arch>-<os>` beside it. `cargo xtask release` builds them
(see [[Releasing]]).

`atlas doctor` says what it can check: whether the atlas hooks in Claude Code's `settings.json` match the plugin's
`hooks/hooks.json`, or that there is no such file, no atlas hook in it, or that it could not be read (exit 1). A plugin
install keeps its hooks in the plugin, so it gets its own line and those hooks are not checked. It also names a project
root at or above your home folder, without changing it. It writes nothing.

If the server fails to start once (a missing binary, an invalid `atlas.json`), Claude Code does not try it again for
15 minutes, even after you fix the cause, unless the fix changes the server's command: the next sessions show it as
failed, and its hooks report it as not connected.
This is Claude Code's behaviour, measured at 14.9 minutes still failing and 15.2 connected, and atlas cannot change it.
A session works without atlas in that time: routing, recall and agent caps are off. `atlas doctor` names the wait
when it can see Claude Code's record of it (`mcp-needs-auth-cache.json`).
