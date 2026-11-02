# Architecture

One Rust binary, one dependency (SQLite, bundled), no network at query time.

## Processes

- **The MCP server** (`atlas`, started by Claude Code from `.mcp.json`). It serves the tools over
  stdio. It answers `hook` calls on their own worker thread, so a hook never waits behind a slow tool call.
- **Hooks** (`hooks/hooks.json`). Each is an `mcp_tool` hook that calls the running server's `hook`
  tool, so no process starts per hook. Claude Code substitutes `${tool_input}` and `${tool_response}`
  into the call as JSON text. Plugin hooks also fire inside subagents, with `agent_id` set (measured).
  `atlas --hook` is the same logic as a command hook, reading the event on stdin. SessionStart is that
  command hook, because Claude Code runs no `mcp_tool` hook at SessionStart (measured on 2.1.316). Its
  only output is one-time: the settings offer and, once per folder atlas does not index, a line asking the
  model to ask the user whether to add the project as a source (`atlas settings add-project`).
- **The CLI** (`atlas run`, `atlas slice`, `atlas settings`, `atlas grammar`, `atlas --setup`, and the
  `--` maintenance modes). The plugin's `bin/` folder is on the Bash tool's `PATH`, so the model runs
  `atlas slice` as a bare command.

## Two SQLite files

- **The index** (`atlas.sqlite`) is derived and deletable. It holds notes, links, full-text search, the
  identifier registry, sections, the code index, the token ledger and the handle table. Its tables are
  rebuilt from the files when their schema version changes. Two tables are durable and survive a
  rebuild: `misses` (mirrored to a JSONL log) and `handles`.
- **The memory store** (`memory.sqlite`) is authoritative. It holds versioned memory records, journals,
  handoffs and decisions. Markdown files such as `MEMORY.md` are exports of it, not its source.

Both use WAL mode. Writes are serialised across processes by the database write lock.

## Routing and the handle store

Routing is deterministic: hooks decide, not the model. It is on by default; `lean.routing: false` or
`atlas settings off routing` turns it off, and `ATLAS_RAW=1` in a command passes that command through.

- **PostToolUse swaps a large result.** A Bash, PowerShell, Read, Grep or Glob result over the threshold
  is stored whole and replaced by a digest plus a handle. The replacement keeps the tool's own output
  shape, because Claude Code ignores a mismatched `updatedToolOutput` for built-in tools. A Bash output
  over 30 000 characters is read whole from Claude Code's `persistedOutputPath`. A whole-file Read that
  Claude Code cut short is read whole from disk.
- **PreToolUse adds an exit tail, not a rewrite.** A command that exits nonzero fires
  PostToolUseFailure, whose output a hook cannot replace (its error text is middle-truncated). So routed
  one-line Bash commands (readers, searches, `cargo` builds and tests) get `|| echo atlas-exit-nonzero`.
  The command exits 0, PostToolUse sees the whole output, and the router strips the marker (exact final
  line only) and reports "exit nonzero".
  - Rewriting `CMD` to `atlas run -- CMD` would not work. Claude Code checks permission rules against the
    rewritten command, one subcommand at a time, so `atlas run` would need an allow rule that lets any
    command through.
  - The appended `echo` is read-only and needs no rule, and the user's rules still decide `CMD`.
  - Some commands get no tail: multi-line Bash, a line with a comment or a here-document, and background
    commands. A brace group would keep the exit status, but Claude Code refuses it before it runs. These
    commands fail raw rather than with their failure hidden.
  - PowerShell has no tail that keeps a native exit code. Claude Code reports failure from the last
    native command's exit code, 5.1 has no `||`, and the permission check refuses every `$`
    (`$LASTEXITCODE`, `$?`, `"$LASTEXITCODE"`), `exit 0`, `cmd /c`, `Get-Variable` and reads of the
    `variable:` drive (measured on Claude Code 2.1.316, 26 candidates). A trailing read-only native command
    that exits 0 passes the check and turns the call into a PostToolUse, but it loses the status. So on
    Windows a PowerShell `cargo` build or test gets `where.exe /Q where.exe`, which prints nothing: cargo's
    output states its own result, and the digest infers the verdict and keeps the tail verbatim. Every other
    PowerShell command gets the inert `Write-Output ''`, and a nonzero native exit passes raw, so a silent
    failure (`git diff --quiet`, a `findstr` with no match) is never reported as a success. Its digests
    say "exit status unknown (inferred: ...)".
- **The store.** Bytes live in `<data dir>/handles/<sha[0..2]>/<sha>`, written as `.part` then renamed.
  The id is `h:` plus 10 hex characters of a hash over the bytes, the kind and the command. The same
  output is therefore the same handle, and a hook's answer is a pure function of its input.
  - Slices (`atlas slice h:ID --item NAME | --failures | --lines A-B | --grep RE`) are capped at 8 KB and
    say where to continue.
  - Items (tests, failure blocks, compiler messages, files of a grep, items of an outline) are recomputed
    from the stored text on each slice.
  - Eviction is least recently used beyond 512 MB, or after 7 days unused.
- **Annotations.** A Grep, Read or shell read result that passes whole is annotated from the code index.
  Each Grep content match line gets its innermost enclosing item appended (`[fn Type::name 10-20]`, or
  `[impl Trait for Type 5-40]` between methods); context lines and line counts are unchanged. A Read
  window that cuts an item gets one `additionalContext` line naming the cut items with their ranges, and
  its content is untouched, so line numbers and `Edit` strings stay exact.
  - Shell reads are recognised from the command as written. `grep -n`, `rg -n` and PowerShell's
    `Select-String` (lines `path:line:text`, or `line:text` for one named file) get the Grep suffix in
    `stdout`; the rest of the Bash shape is kept. A window (`sed -n 'A,Bp' F`, `cat -n F | sed -n ...`,
    `head`/`tail -n N F`, `Get-Content` with `-TotalCount`, `-Tail`, `| Select-Object -Skip S -First N`
    or `(Get-Content F)[A..B]`) gets the Read note, cut to the lines it printed.
  - A Bash batch of reads (`sed -n 1,80p a.rs; echo ----; git grep -n x -- src`) is read part by part
    when an `echo TEXT` separates every two reads and its exact line is in the output; `cd DIR` parts
    move the folder, and Git Bash paths (`/c/...`) map to drive paths. Any other part (an assignment,
    `||`, a redirect, `$(`) leaves the whole command unread, so an annotation never guesses which output
    is which. `git grep -n` is read like `grep -rn`.
  - A file outside every code root, or written after its root was indexed, is not annotated.
    `lean.annotate: false` turns this off.
- **Fail open.** Any routing error leaves the call untouched and records a miss (`tool = "route"`).
- **Digests** (`src/digest.rs`) have a 600 B budget, or 3.2 KB for an outline. Each digest ends with one
  slice hint line. Without an exit code a verdict says it is inferred and keeps the tail verbatim, so a
  wrong guess cannot hide a failure.

## The code index

A code root is indexed from SCIP (rust-analyzer's `scip` output, parsed by the owned protobuf reader)
plus a line scan that adds what SCIP lacks. WGSL has its own scanner. One pre-sorted table, `code_occ`,
holds definitions, references (with the calling item) and impl edges. Each definition's and impl block's end
line comes from brace matching over its file, so `symbol` answers `path:start-end` and an annotation names the
innermost fn inside an impl, else the impl. The scan finds impl blocks too, and gives a method inside one its
type as container. Reindexing runs on its own thread
when a root changes, never on the query path.

An Edit, Write or MultiEdit re-indexes the file it changed at once (R7). The hook worker queues the path,
sends the hook's answer, then does the work, so the edit never waits:

- A `.rs` or `.wgsl` file under a code root gets its rows rewritten from the line scan in one transaction
  and is stamped indexed, so annotations cover it again on the next call. Its SCIP rows (references, SCIP
  kinds) come back with the root's next full indexing, which the change schedules anyway. Ruled: after a
  per-file re-index the scan rows stand until the root's next full index.
- A Markdown file under a vault or source is rescanned through the index store, which the server hands the
  worker as an opener (`hooks::StoreOpener`).
- A failure is logged as a miss (`tool = "route"`) and changes nothing.

## Settings and backups

`atlas settings` writes two kinds of file, and backs each up before every write into an `atlas-backups`
folder of its own:

- Claude Code's `settings.json`: `settings.STAMP.json` in `<Claude Code folder>/atlas-backups`.
- atlas's own configuration (`atlas settings set`, `add-project`, `atlas grammar accept`):
  `atlas.STAMP.json` in `atlas-backups` beside the configuration file. Ruled: nothing of atlas's own
  goes into Claude Code's folder.

A stamp and its collision number are unique across both folders, so `atlas settings undo` takes the newest
backup of either kind by them alone, each read from its own folder.

Ruled: `add-project` writes the configuration, indexes the new Markdown source at once, and leaves the new
code root to be indexed when the server next starts.

## Owned inference

Semantic search and the recall gate embed with an owned BERT encoder (`src/embed`). It has a WordPiece
tokenizer and GEMM kernels for AVX2, AVX-512 VNNI and NEON over a scalar reference. It reads a packed
little-endian model file; there is no model-format parser or machine-learning crate. JSON, regular
expressions, SHA-256, protobuf and the vector byte scans are owned too.

## The model download

`atlas --setup` downloads the model files listed in the configuration (`models.files`: URL, SHA-256,
size). It uses the system `curl`, started without a shell, resumes a partial download, and checks the hash
before a file is used. The server downloads missing files in the background on start, unless
`models.auto_download` is false. Until the files are in place, recall is lexical.

## The platform module

`src/platform` is the only place with unsafe FFI. It holds change notification for the watcher:
`ReadDirectoryChangesW` on Windows, inotify on Linux, FSEvents on macOS, and a stat walk elsewhere.
Windows, macOS and Linux are all targets.
