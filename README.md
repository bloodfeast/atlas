# atlas

atlas makes long Claude Code sessions cheaper without changing how you work. It is a local MCP server and a
set of Claude Code hooks: large tool output stays on disk under a short handle and the model gets a digest,
code reads get exact item ranges instead of guessed windows, and memory (rules, decisions, journals,
handoffs) is recalled by relevance instead of loaded whole. It runs on your machine, depends on SQLite and
an ed25519 signature check and nothing else, and makes no network calls at query time.

**You do not need Obsidian.** A vault is any folder of Markdown files: atlas reads and writes the files
itself and never touches Obsidian's `.obsidian` settings folder. Obsidian is optional, as a reader for the
same folders. The one Obsidian-specific output is that `make_link` writes `obsidian://open` links between
vaults, which open only in Obsidian (elsewhere they are plain text); links within a vault are standard
`[[wikilinks]]`.

**Languages.** Memory, recall and history, search over Markdown notes and sources, discussions, the work
queue, projects and multi-repo, and the hooks work whatever programming language a project uses, as does `.proto` indexing (rpc call
sites are found in any language's files). The code index (`symbol`, item ranges on reads, grep annotations)
covers Rust, WGSL, `.proto`, Go, TypeScript, JavaScript, C# and Python (through scip-go, scip-typescript,
scip-dotnet and scip-python when they are installed, or a prebuilt `index.scip` from CI, WSL or Docker, else
an owned scanner), and any language with a SCIP indexer from a `.scip` file; build and test digests recognise `cargo test`, `nextest`, `go test` (plain and `-json`), jest,
vitest, `dotnet test` and pytest output, and other
runners get the generic digest. More languages are planned.
Ranked search (`fts`, `sections`, `recall`) splits words on spaces and punctuation, so text written without
spaces (Chinese, Japanese, Thai) is found with `search` in `substring` mode or `grep` instead.

## What it saves

Every assistant turn re-reads the whole context, so a result that enters at turn 10 of a 300-turn session is
processed 290 more times. Measured on real sessions:

| | Without atlas | With atlas |
|---|---|---|
| Large tool output (first measured sessions) | 236 KB into context | 8.2 KB of digests |
| One 87 KB source file, read for review | 87 KB | 3.7 KB outline + 12 KB of targeted windows |
| The tool list, loaded with every session | 47 tools, 5,930 tokens (v0.1) | 14 tools, 1,480 tokens; the full reference is one fetched resource |
| A lookup (median of 2,082 pre-atlas episodes) | 624 tokens of grep and reads | 288 tokens: 2.2x smaller |
| Memory questions (100-question evaluation) | the whole rulebook loaded | 100/100 answered when the agent follows a result to its source, 99/100 reading only the first result; median 504 tokens each |

Shell reads are 36% of what a builder session processes and startup context another 35%; those are the
buckets atlas targets. How each number was measured, and how to measure your own sessions with
`cargo xtask attrib`, is on [Token savings and method](https://github.com/bloodfeast/atlas/wiki/Token-savings-and-method).

## Install

atlas is a Claude Code plugin with prebuilt binaries for Windows x86_64, Linux x86_64 and aarch64, and macOS
(Apple Silicon and Intel). In Claude Code:

```
/plugin marketplace add bloodfeast/atlas
/plugin install atlas@atlas
```

Restart Claude Code. On its first start atlas writes a starter configuration, `atlas.json`, in your
configuration folder (`%APPDATA%\atlas`, `~/Library/Application Support/atlas` or `~/.config/atlas`) and
downloads the embedding model (about 35 MB) in the background, checked against its pinned SHA-256. Until the
model is in place, search and recall are lexical. Updates are signed and on by default (`atlas settings off
update` turns them off). Building from source, uninstalling and the data folders are on
[Install and setup](https://github.com/bloodfeast/atlas/wiki/Install-and-setup).

## Quickstart (5 minutes)

1. Install the plugin (above) and restart Claude Code.
2. Optionally run the setup in the foreground to see each step; Claude can run it for you, since the plugin's
   `bin` folder is on the Bash tool's `PATH`:

   ```sh
   atlas --setup
   ```

3. Open a session in your project. atlas asks once, in one line: *create a vault for this project at
   `<path>`? yes / other folder / no*; say yes. Its Markdown becomes a searchable source, a folder holding
   code becomes a code root, and a new vault (an Obsidian-compatible folder) holds the project's notes and
   discussion threads. `atlas settings undo` reverts it all; it never deletes a note.
4. Work as usual. Large Bash, Read and Grep results now arrive as digests with a handle; Claude pulls what it
   needs with `atlas slice h:ID --item NAME` or `--failures`. Grep and Read results name the enclosing item and
   its line range.
5. Ask Claude to remember a decision (`remember`), and start the next session: relevant records are recalled
   on each prompt, within a 150-token budget (`recall.budget_tokens`).
6. Optionally, `atlas settings offer` proposes the Claude Code settings that cut cost (the 1-hour subagent
   cache, an early compaction point). Nothing is written without a yes, and `atlas settings undo` reverts (it shows the change and asks first;
   `--yes` without a terminal).
   With the experimental compaction features on (`"compact": {"enabled": true}`, off by default),
   `atlas settings compact-md` offers a Compact instructions block for a project's `CLAUDE.md` the same way.

## Projects

One install serves several projects, each with its own index and memory store; recall, the hooks and the
work queue see only the active project. A shared user layer is imported into every project.

**One repository.** Open Claude Code in it and answer yes, or run:

```sh
atlas settings add-project --vault .      # or --vault-at FOLDER; without either, no vault
```

**A folder of repositories.** Open Claude Code in the folder that holds them (each one level down). atlas
offers to make it a project, or run:

```sh
atlas settings add-project --new .
```

That writes `atlas.project.json`, listing each repository by path and remote, and creates the project's vault
beside atlas's configuration (`--vault-at FOLDER` for another folder, `--no-vault` for none); commit the file so the whole team
gets the same layout. Describe how the repositories relate, and a prompt that names one gets its role and
neighbours, while recall puts records about the repository you are working in first:

```json
{
  "name": "acme",
  "repos": [
    {"name": "billing", "path": "billing", "role": "invoices and payments", "talks_to": ["auth", "ledger"]},
    {"name": "gateway", "path": "gateway", "role": "public API edge", "talks_to": ["billing"]},
    {"name": "ledger", "remote": "https://git.example/acme/ledger"}
  ]
}
```

On a fresh install, `add-project --new` first moves the current setup into a project of its own (shown, backed
up, and reversed by the same `atlas settings undo`). In a terminal it asks for that project's name; otherwise the
name is the folder holding `atlas.json`, made a valid project name (`-2` if the new project takes it). An existing
setup moves into a project of another name with `atlas settings migrate-projects NAME`. Everything else is on
[Projects and multi-repo](https://github.com/bloodfeast/atlas/wiki/Projects-and-multi-repo).

## Minimal configuration

`atlas.json`, with relative paths taken from its folder. Every key is checked, so a misspelt key is an error.

```json
{
  "vaults": [{"name": "notes", "path": "../Notes"}],
  "sources": [{"name": "docs", "root": "../my-project", "include": ["**/*.md"], "exclude": ["target/**"]}],
  "code": {"roots": [{"name": "app", "root": "../my-project"}]}
}
```

Vaults are folders atlas may add to; sources are folders it only indexes; code roots feed `symbol`. After an
edit, run `/mcp` and reconnect atlas, or restart Claude Code. Every key is on
[Configuration reference](https://github.com/bloodfeast/atlas/wiki/Configuration-reference).

## Security in brief

- **Local only.** One process over stdio. No socket, HTTP client or DNS in the code or its dependencies;
  nothing reaches the network at query time. The model download at setup and the update check go through
  the system `curl`, to pinned `https` URLs, and are verified (SHA-256, and an ed25519 signature for updates).
- **Confined.** Every path is checked to stay inside a configured folder, lexically and after resolving links;
  dot folders are refused.
- **A fixed deny-list.** `.env*`, keys, keystores and credential files are never read, whatever the
  configuration says: not in a source, not in a vault (nor written there), not by `atlas digest` or `atlas crop`,
  and never kept as a handle when Claude Code reads one.
- **Nothing lost.** No delete or rename tool exists; writes are additive or exact, atomic and audited with a
  stated reason. The memory store is append-only.

The threat model and every defence are on [Security model](https://github.com/bloodfeast/atlas/wiki/Security-model).
Report vulnerabilities privately, as [SECURITY.md](SECURITY.md) describes.

## Documentation

The wiki is the full manual:

- [Overview](https://github.com/bloodfeast/atlas/wiki/Overview): what atlas is and the rules it is built to
- [Install and setup](https://github.com/bloodfeast/atlas/wiki/Install-and-setup): install, updates, uninstall, build from source
- [Configuration reference](https://github.com/bloodfeast/atlas/wiki/Configuration-reference): every key of `atlas.json`
- [Projects and multi-repo](https://github.com/bloodfeast/atlas/wiki/Projects-and-multi-repo): scoped projects and repository graphs
- [Token savings and method](https://github.com/bloodfeast/atlas/wiki/Token-savings-and-method): where the tokens go and how it is measured
- [Handles and digests](https://github.com/bloodfeast/atlas/wiki/Handles-and-digests): routing, `atlas slice`, `atlas run`
- [Tools reference](https://github.com/bloodfeast/atlas/wiki/Tools-reference): every MCP tool and the answer format
- [Code index](https://github.com/bloodfeast/atlas/wiki/Code-index): `symbol`, item ranges and language coverage
- [Semantic search](https://github.com/bloodfeast/atlas/wiki/Semantic-search): the owned embedder and hybrid search
- [Identifiers and freshness](https://github.com/bloodfeast/atlas/wiki/Identifiers-and-freshness): the registry, freshness and misses
- [Memory store and hooks](https://github.com/bloodfeast/atlas/wiki/Memory-store-and-hooks): the store, exports, sync and the hooks
- [Memory tree](https://github.com/bloodfeast/atlas/wiki/Memory-tree): the memory index as a tree
- [Discussions](https://github.com/bloodfeast/atlas/wiki/Discussions): reviewer threads and the decision queue
- [Generated indices](https://github.com/bloodfeast/atlas/wiki/Generated-indices): indices between markers
- [Cross-vault links](https://github.com/bloodfeast/atlas/wiki/Cross-vault-links): links between vaults
- [Security model](https://github.com/bloodfeast/atlas/wiki/Security-model): threat model and defences
- [SQLite store](https://github.com/bloodfeast/atlas/wiki/SQLite-store): the database and the audit log
- [Testing and coverage](https://github.com/bloodfeast/atlas/wiki/Testing-and-coverage): the suite, adversarial tests, coverage
- [Releasing](https://github.com/bloodfeast/atlas/wiki/Releasing): the changelog rule, release notes and the wiki
- [Changelog](https://github.com/bloodfeast/atlas/wiki/Changelog): every release

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
