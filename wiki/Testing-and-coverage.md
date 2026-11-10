`cargo test -j 4`. Every hard property has one: each confinement rejection for reads, writes and folders, a
junction escape (Windows) and symlink escapes (Linux), no delete or rename tool, byte-identical body after
`set_frontmatter` (LF and CRLF), the stale-sha refusal, twelve real processes allocating 120 ids and appending
120 history lines into one note with no duplicate and no lost update, the binary round-trip over pipes,
database deletion and rebuild, the audit JSONL surviving it, external edits, the reason rule on every write
tool, discussion threads, cross-vault links and every index rule; and for the atlas layer the definition
heuristic, lookup ordering, `ids` filters, section search, the deny-list and globs, sources being unreachable by
every write tool, freshness (`off`, `check`, `auto`, removal, a new name re-reading every document), misses
surviving a deleted database, the recall check (passing, matching an independent regex scan, and failing when
an index row is removed), and the binary serving from a config. Tests build their own corpus in a temp folder.

On Linux, in any image with a Rust toolchain:

```sh
docker run --rm -v "$PWD:/src:ro" rust:1 bash -c "cp -r /src /work && cd /work && CARGO_TARGET_DIR=/tmp/t cargo test -j 4"
```

`cargo test --features dev` adds the measuring modes and their tests; the tier reported for a change runs with it.

`cargo test --workspace --release` also runs the `xtask` crate's tests (the release, the wiki publish and the
public-repository guard, against local bare repositories and saved GitHub answers; nothing is pushed
anywhere real). The public repository runs exactly that on Linux, Windows and macOS for every pull request
and push to main (`.github/workflows/ci.yml`). Tests that need something outside the repository (the private
leak denylist, a real configuration, a live vault) skip themselves with a notice when it is absent.

## The adversarial suite

`tests/adversarial` is hostile input on every surface atlas reads, in one test binary: protocol frames over the
real binary, every tool argument with wrong types and hostile strings taken from its schema (with a canary
check that nothing is written outside the vaults), paths and confinement, FTS5 and SQL syntax in queries,
parsers (JSON, regex, glob, Markdown, PNG and BMP), the embedder's model files, hook events, the store and the
update manifest. Property tests use a seeded xorshift generator; `ATLAS_FUZZ_SEED` moves every loop to a new
seed. The default run is fixed-size; deep mode runs 200 times the cases:

```sh
cargo test --release --test adversarial -- --ignored deep_
```

Each finding it made was fixed with its own regression test: quadratic JSON object parsing, exponential glob
matching, a panic on a path split inside a multi-byte character, superscript device names on Windows, a
loopback check fooled by `user@host` URLs, LIKE wildcards in a folder filter, corrupt model files, image
decompression bombs, unbounded regex memory and record framing in recalled context.

## Coverage

```sh
cargo xtask coverage [--debug] [--html] [--no-run] [-- TEST_ARGS...]
```

Line and region coverage of the atlas crate from rustc's own instrumentation (`-C instrument-coverage`), with no
coverage crate and no cargo plugin. It needs the `llvm-tools` rustup component (`rustup component add
llvm-tools`) and says so when it is missing. It builds and runs the tests in release (unless `--debug`) in its own
target folder, so normal builds are not invalidated; merges the profiles of every process (the atlas binary the
integration tests start included); and prints coverage per module, per folder and in total. The core modules
(the store, recall, protocol, tools, parsers, paths, hooks, update and the embedder among them) are held to 85%
lines and named when below. The table is written to `<target>/coverage/summary.txt`; `--html` adds llvm-cov's
annotated source. `--no-run` re-renders the last run's profiles, and arguments after `--` go to the test
binaries.

## Development tasks

Development tasks are `cargo xtask <command>`, in Rust, in the `xtask` workspace crate (never shipped; plain
`cargo build` and `cargo test` build atlas alone). Each is documented at the top of its file in `xtask/src/`:

| Command | What it does |
|---|---|
| `attrib` | attributes the tokens a session processed to what put them in context (see [[Token savings and method]]) |
| `calibrate-baseline` | the calibration with explicit inputs (cutoff, folder, skips, `--keep-duplicates`) and an audit file; the shipped one is `atlas --calibrate` |
| `token-use` | breaks down atlas's answers by tool and mode; `--replay` answers them again on another build |
| `calibrate-recall` | derives the recall floors from `atlas --recall-scores` output |
| `memory-eval` | scores atlas as a memory store on a fixed copy of one: recall-check misses and a question set |
| `memory-tree-check` | checks the memory index tree (see [[Memory tree]]) |
| `pack-embedder` | packs a BERT sentence embedder into atlas's model file |
| `coverage` | the coverage report above |
| `langs-bench` | a code index language's numbers on a pinned public corpus (see [[Code index]]) |
| `release` | the release, its notes and the wiki (see [[Releasing]]) |
| `verify-macos` | the whole suite, the recall check and the bench on macOS, to paste back |

The one shell script left is `scripts/launcher.sh`, the release's `bin/atlas` launcher.
