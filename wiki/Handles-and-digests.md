A large tool result costs its size on every later turn of the session, because every turn re-reads the
whole context. atlas keeps such results on disk and gives the model a digest, by hook, not by the model's
choice. Why this matters, and what it saves, is on [[Token savings and method]].

## Routing

A Bash, PowerShell, Read, Grep or Glob result over a threshold (12 KB, or 3 KB for `cargo` build and test
output) is stored whole under a handle such as `h:3f9a2c1b0e`. The model gets a digest instead: test totals
and each failure's location and message, compiler errors and warnings by lint, grep hits by file, or a file's
outline with line ranges. Test digests know `cargo test`, `cargo nextest`, `go test` (plain, `-v` and `-json`: each
failing test or subtest with its `file_test.go:LINE` message, a panic with its first frame outside the
runtime, and package totals), jest and vitest (the runner's own totals, each failing test by its full name
with the first stack frame in the project and its message, jest's `Expected` and `Received` lines, and file
totals; colour codes are read through), and `dotnet test` (each failing test with its `Error Message`, the
first stack frame with a source file, xUnit's `Expected` and `Actual` lines, and the totals of every test
assembly of a solution's run summed), and pytest (its own totals line, each failing test's report under
`FAILURES` by the name its header gives, with the first `E` line, a `+ where` line, and the location after
them; passes and skips are items with `-v`), Maven with Surefire (`mvn`, `mvnw` or `mvnd` running `test`,
`verify` or `install`: the totals of every module's `Tests run:` line, with Failures and Errors both counted
as failed and passed derived from them; each failing test, `<<< FAILURE!` or `<<< ERROR!`, with its exception
and the first frame outside the assertion library; test classes as groups; Surefire lists failures only), and
Gradle (`gradle` or `gradlew`: each `Class > method() FAILED` with its exception and location, in the short or
the full exception format, test tasks as groups, and totals from the `N tests completed, M failed` line a
failing task prints; a passing run prints no count, so its digest says `(Gradle lists failures only)`, and
passes and skips are items when the build's test logging prints them); other runners are rows of the same table in `src/digest.rs`
(`RUNNERS`). A `go test -json` digest reads each package's output whole, though packages interleave their
JSON lines, and its items point at the JSON lines. The
model pulls exactly what it needs:

```sh
atlas slice h:ID --item NAME      # one item of the digest: a test, a file, a function
atlas slice h:ID --failures       # every failure whole
atlas slice h:ID --lines A-B      # a line range
atlas slice h:ID --grep RE        # matching lines
```

Routing is a PostToolUse swap: the tool runs as written, so no permission rule changes, and the result is
replaced by the digest before the model sees it. It is on by default and fails open: any error passes the
call through untouched and is logged as a miss. `atlas settings off routing` (or `lean.routing: false`)
turns it off, and `ATLAS_RAW=1` passes one command through.

One limit: a PowerShell command that exits nonzero is routed only for `cargo` builds and tests on Windows.
Claude Code offers no read-only PowerShell tail that keeps the exit code, so any other failing PowerShell
command reaches the model raw.

In auto mode atlas leaves the command as written. Elsewhere it appends a read-only `|| echo
atlas-exit-nonzero` so a failing read is still digested, but the auto-mode safety classifier judges the
command the model wrote and refused the changed one, which cost a resend per read. So in auto mode a
successful read is digested as usual and a failing one arrives as Claude Code's own error text. Atlas never
approves a call on the classifier's behalf.

A handle id is a pure function of the bytes, the kind and the command, so the same output stored twice is the
same handle. Every slice is capped (8 KB) and says how many lines it cut. The store keeps 512 MB, least
recently used first out, and drops handles unused for 7 days. Each limit is a `lean` key on
[[Configuration reference]].

## Code ranges

Results small enough to pass whole are annotated from the code index instead: Grep lines get their enclosing
item and its range, and a Read window that cuts an item is told where it ends. See [[Code index]].

## atlas run

```sh
atlas run [--digest test|build|grep|listing|source|summary|generic] [--timeout S] [--label TEXT] -- CMD [ARGS...]
```

Runs a command without a shell, keeps its whole output under a handle and prints the digest and the handle
(or the output itself when it is under the threshold), then exits with the command's exit code. A pipeline
goes through `atlas run -- sh -c '...'` (or `cmd /C`).

## File tools

Two file tools need no configuration (no config is looked for or written):

```sh
atlas digest check.log                    # a long check log: its totals tables, failures, panics and PASS/FAIL verdicts only
atlas digest --as test out.txt            # any saved output, digested as atlas run would (test, build, grep, ...)
atlas crop strip.png out.png --frames 2-3 --of 6 --max 640   # frames 2 and 3 of a 6-frame strip, at most 640 wide
atlas crop shot.bmp out.png --rect 100,80,320,200            # a region of a PNG or BMP, written as a small PNG
```

A check log is recognised by its per-package totals table (`package  passed  failed ...` down to `total`);
`atlas run --digest summary` gives the same digest for a command. `crop` splits a strip into equal frames
along its longer side (or `--axis x|y`), then applies `--rect`, then shrinks by the smallest integer factor
that fits `--max W[xH]`; PNG and BMP are decoded and PNG written by atlas itself (`src/picture.rs`).
