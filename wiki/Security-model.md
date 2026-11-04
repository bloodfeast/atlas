## Threat model

The client is a language model acting on text it read, some of it written by other agents. It may be
confused or prompt-injected. It is not a local attacker with a shell; anything with a shell already has the
files. The server must make the worst instruction it can follow cheap to undo:

| Risk | Defence |
|---|---|
| Data leaves the machine | no network code: no socket, HTTP client or DNS in the source or the dependency graph (test `no_network_code` checks `Cargo.lock` and `src/`). Two things reach the network, both through the system `curl`, started with an argument list (no shell), fetching `https` URLs (`--proto =https`, redirects included; an update URL may also be `http` to loopback, for testing) and sending nothing but the request: the model download at setup and first start, and the update check at session start (off with `atlas settings off update`). Neither runs at query time |
| Reading or writing outside the vaults | roots fixed at launch; paths checked lexically and again after canonicalisation (below) |
| Reaching Obsidian config, git, trash | any path component starting with `.` is refused, before and after canonicalisation |
| Losing notes | no delete or rename tool exists; creates are exclusive; edits need the current sha (`replace_section`) or are additive |
| Torn or interleaved writes | temp file + rename in the same folder; all writers take the SQLite write lock first; a file that changed between read and write is refused |
| Silent changes | every write must state a reason, and lands in the `writes` table and in the append-only `state/writes.jsonl`: time, tool, vault, path, sha before and after, client name, pid, reason; `write_log` reads it back |
| Prompt-shaped output | tool descriptions are short and factual; note text comes back only as a JSON string field; `instructions` is one line naming the vaults |
| Resource exhaustion | reads capped at 1 MiB, write payloads at 256 KiB, search results at 200, listings at 2,000, messages at 4 MiB, regex programs at 20,000 instructions and a search's backtracking state at 256 Mibit (32 MiB, freed after the search; a regex search past it is refused, naming the note, the line and what to narrow), globs at 128 bytes and 8 `*` |

## Path confinement (`src/paths.rs`)

A request names `vault` + a relative path. Refused before touching the disk: absolute paths, leading `/`
or `\`, UNC, any `:` (drive prefixes and NTFS `:stream` names), NUL and control characters, `.` and `..`
components, empty components, any component starting with `.`, components ending in `.` or space (Windows
strips them), `< > " | ? *`, Windows device names (`CON`, `NUL`, `COM1`, ...), paths over 512 bytes, and for
notes anything not ending in `.md`. Then each folder is canonicalised and must stay under the canonical root,
and so must the file, with no dot component in the canonical remainder; so a symlink or junction inside a
vault that points outside it, or into `.obsidian`, is refused (tested with a real junction on Windows and a
symlink on Linux). The rescan walker never follows links at all.

Not defended, by design: a hard link placed inside a vault by someone with a shell (it is indistinguishable
from a file), and a local process racing a symlink swap between the check and the open (that process could
write the target itself).

## Dependencies

Two, each a named exception to the rule that atlas owns its code:

- `rusqlite` (feature `bundled`, so the SQLite C library is compiled in and no system SQLite is loaded): the
  durable store.
- `ed25519-compact`, default features off, so it brings no dependency of its own: the signature check on
  release manifests (`src/update.rs`). Signing and verification are not hand-written curve arithmetic.

No dev-dependencies. Everything else is owned:

- JSON (`src/json.rs`): a strict parser (nesting at most 128, no byte order mark, no lone surrogates, numbers
  that fit), a writer, `json!` and the `ToJson`/`FromJson` traits.
- Regular expressions (`src/regex.rs`): a documented subset (classes, `\b`, `^ $`, groups including
  `(?P<name>)`, alternation, `{m,n}`, `* + ?` and their lazy forms, `(?i) (?m) (?s)` at the start) run by a
  bounded backtracker that records every (instruction, position) it tried, so no pattern can take more than
  program length times text length steps; that record is bounded at 256 Mibit per search (a search past it is
  refused), and a pattern with no alternative to explore needs none. Anything outside the subset is refused with an error naming it. The
  module docs hold the table.
- Change notification (`src/platform/`): `ReadDirectoryChangesW` on Windows, inotify on Linux and FSEvents on
  macOS (per-file events, one stream per root on a thread running its own CFRunLoop; `MustScanSubDirs` and the
  dropped-event flags walk the root), all through owned `extern` declarations.
- Byte scans (`src/scan.rs`): SSE2, AVX2 (detected at run time) and NEON versions of `memchr` and the JSON
  string scan, each tested against its scalar version. `unsafe` is denied crate-wide and allowed only in
  `platform` (including the child-process priority and kill calls of the code indexer), `scan` and
  `embed::gemm`, each block with its invariant written beside it.
- Protocol Buffers and SCIP (`src/protobuf.rs`, `src/scip.rs`), the WGSL scanner (`src/wgsl.rs`) and the
  sentence embedder (`src/embed/`) are owned as well (see [[Code index]] and [[Semantic search]]).

`Cargo.lock` also lists `wasm-bindgen`, `js-sys` and `sqlite-wasm-rs`: they are `rusqlite` dependencies for
the `wasm32` target only and are not compiled on Windows or Linux. SHA-256 is hand-rolled (`src/sha256.rs`,
FIPS 180-4 test vectors) to avoid a hashing dependency.

## Updates

An update is trusted only after two checks: the release manifest's ed25519 signature against the public key
built into atlas (`src/update.rs`), then the downloaded binary's SHA-256 against the signed manifest. A newer
version is never replaced by an older one, a pinned version (`update.pin`) is never replaced, and a binary
built from source never checks. Manifest URLs must be `https`, or `http` to a real loopback address. How
releases are signed is on [[Releasing]].

## Read-only sources

Sources are not vaults: no write tool can name one (each resolves its target through the vault set), and
`src/sources.rs` has no code that opens a file for writing (test `no_write_tool_can_reach_a_source` calls every
write tool against every source and compares bytes and mtimes, and checks the module text). The walker never
follows a symlink or junction, never enters a dot folder, and stays as deep as the globs reach; a file is
opened only after its canonical path is checked to be under the canonical root.

**Deny-list**, fixed in code (`sources::is_denied`), checked before a file is stat'ed and again before it is
opened, and refused in a config's globs. It holds for sources, for vaults (a denied note or folder is not
walked, indexed, read or written, by name or through a link), for the CLI file tools (`atlas digest`, `atlas
crop`), and for files Claude Code reads (never kept as a `slice` handle or read again from disk). The names: `.env`, `.env.*`, `*.env`, `*.pem`, `*.key`, `*.p12`, `*.pfx`,
`*.jks`, `*.keystore`, `*.kdbx`, `*.asc`, `*.gpg`, `id_rsa*`, `id_dsa*`, `id_ecdsa*`, `id_ed25519*`,
`credentials`, `credentials.*`, `*.credentials`, `.netrc`, `_netrc`, `.npmrc`, `.pypirc`, `secrets.*`,
`*.secret`. The name rules (`id_rsa*` and the other key names, `credentials`, `credentials.*`, `_netrc`,
`secrets.*`) do not apply to a `.md` note or to a folder name, so `Secrets.md` and a `credentials/` folder are
served; the extension and `.env` rules, dot names and the check of the name a link resolves to apply everywhere.
Only `.md`, `.markdown` and `.txt` are indexed at all.
