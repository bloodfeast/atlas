`cargo xtask test-gate` (plain `cargo test -j 4` runs the same tests, slower). Every hard property has one: each
confinement rejection for reads, writes and folders, a junction escape (Windows) and symlink escapes (Linux), no
delete or rename tool, byte-identical body after `set_frontmatter` (LF and CRLF), the stale-sha refusal, twelve
real processes allocating 120 ids and appending 120 history lines into one note with no duplicate and no lost
update, the binary round-trip over pipes, database deletion and rebuild, the audit JSONL surviving it, external
edits, the reason rule on every write tool, discussion threads, cross-vault links and every index rule; and for the
atlas layer the definition heuristic, lookup ordering, `ids` filters, section search, the deny-list and globs,
sources being unreachable by every write tool, freshness (`off`, `check`, `auto`, removal, a new name re-reading
every document), misses surviving a deleted database, the recall check (passing, matching an independent regex
scan, and failing when an index row is removed), and the binary serving from a config. Tests build their own corpus
in a temp folder.

The gate before a pull request runs the workspace's tests with cargo-nextest (`cargo install --locked
cargo-nextest`; the gate fails at once, saying so, when it is missing):

```sh
cargo xtask test-gate                        # cargo nextest run --workspace --locked --no-fail-fast --test-threads 16, then cargo test --workspace --locked --doc
cargo xtask test-gate -- -E 'test(name)'     # arguments after -- go to nextest: a filter, --no-capture
```

nextest runs every test in its own process, 16 at a time, so the slowest test binary no longer sets the wall time;
the doc tests, which nextest does not run, follow as a second step. The 16 is a cap, not one per core: the suite
starts servers and processes of its own. Builds take `CARGO_BUILD_JOBS` (4 when unset, never above 16).
`.config/nextest.toml` gives every slot to the two tests a loaded machine would fail, so they run alone: one times
itself against a bar (`admit_at_300_metas_is_cheap`), the other counts lock waits that drop a change after 5 s
(`state_changes_from_many_processes_are_never_lost`). Both steps run even when the first fails, and the last line
sums them up (`test-gate: PASS in 277s; nextest ok 271s; doctest ok 5s`).

On Linux from any machine with Docker (Docker Desktop on Windows and macOS), the public CI's Linux checks on the
checkout:

```sh
cargo xtask linux-test               # the test-gate's two steps in release, then both clippy runs
cargo xtask linux-test --debug       # the tests in the dev profile (a faster build, not what the public CI runs)
cargo xtask linux-test -- NAME       # only the tests matching NAME (clippy still runs; --no-clippy skips it)
cargo xtask linux-test --clean       # remove this checkout's Linux build volume
cargo xtask linux-test --clean --all # and the Cargo home every checkout shares
```

Commit first. It tests the working tree as it is, edits that are not committed included, while the pull request
carries only what is committed; a result quoted for a pull request is a run on its clean head.

It runs in the official `rust:<channel>` image for the toolchain `rust-toolchain.toml` pins, with the pinned clippy
and the pinned cargo-nextest added (a local image built on the first run; nextest is its release's prebuilt binary,
its sha256 checked), at 4 CPUs and `CARGO_BUILD_JOBS=4`. The checkout is mounted
read-only. `CARGO_HOME` is one named Docker volume every checkout shares, Cargo's package-cache lock with it, so two
runs at once from two checkouts take turns on the download as two cargo processes on one machine do; the Linux
build lives in a named volume per checkout, so the second run is incremental and the host's `target` folder is never
touched. A linked git worktree gets its repository's git folder mounted read-only too, so the tests that read the
checkout's git history work. The first run pulls the image and builds everything in release (about 10 minutes at 4
CPUs); later runs rebuild only what changed, and the release test run itself takes about 6.5 minutes. Every step
runs, the tests go on past a failure (`--no-fail-fast`, unlike the public CI, so one run names every failure), the
last line sums them up (`linux-test: PASS in 250s; test ok (1715 passed, 0 failed) 239s; doctest ok (2 passed, 0
failed) 4s; clippy all-targets dev ok 5s; clippy default ok 1s`), and the exit code is non-zero when any failed. It is the gate before
every pull request (see [[Releasing]]): a Windows or macOS machine never compiles the `cfg(target_os = "linux")`
code, and the development repository's CI does not run the tests.

One gap is left: a checkout made on Windows has no executable bits, and Docker Desktop shows every file of the
mount as mode 777, executable, so a script whose mode in git is wrong (a `scripts/` file committed without `+x`,
say) passes here and fails on Linux. `git ls-files -s scripts` shows each file's mode; `100755` is executable.

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
| `linux-test` | the public CI's Linux tests and clippy runs in a Docker container, the gate before a pull request |

The one shell script left is `scripts/launcher.sh`, the release's `bin/atlas` launcher.
