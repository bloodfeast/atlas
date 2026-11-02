## Install

atlas is a Claude Code plugin. The plugin carries prebuilt binaries for Windows x86_64, Linux x86_64 and
aarch64, and macOS (Apple Silicon and Intel), as the release notes list them; a platform a release lacks
builds from source (below).

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

   The first session in a folder atlas does not index asks once (through Claude, in one line) whether to
   add that project as a source; `atlas --setup` asks the same in the terminal for the folder it runs in.
   Yes runs `atlas settings add-project`, which adds the folder's Markdown as a source (`**/*.md`, without
   `target` and `node_modules`) and, when it holds Rust or WGSL, the folder as a code root, then indexes the
   source; the code root is indexed when the server next starts. No keeps the empty `notes` vault. Each
   folder is asked once; `atlas settings add-project [DIR]` adds one any time. Each of its configuration
   changes is backed up, and `atlas settings undo` reverts the newest (twice for a project with a code root).
   Backups of atlas's configuration go to `atlas-backups` beside the configuration file; backups of Claude
   Code's `settings.json` go to `atlas-backups` in Claude Code's folder. Undo finds each in its own place.
   A folder that holds several git repositories is offered as a multi-repository project instead; see
   [[Projects and multi-repo]].

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
   `"update": {"pin": "0.1.0"}`. A binary you build from source never updates itself.

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
   configuration folder and the data folder from the table above. Nothing else is written outside them.

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
