# atlas code index for more languages (design)

Status: design, ATLAS-LANGS phase 1 (v0.2). PRs 1 to 8 of section 14 are implemented (typed ranges, the
generic SCIP rows and `code_units`; the language table, the scanner engine and Go; TypeScript and JavaScript;
C#; prebuilt files and Python; Java and Kotlin; protobuf stub and implementation linking; cross-root package
references). Sections 2 to 4 are measured on public repositories with the real indexers; the rest is built
on those numbers. The decisions for Brandon are in section 13 (decided: 1A, 2A, 3A, 4A), the phased plan in
section 14.

Spike code (not for merge): `examples/langs_spike.rs` decodes an indexer's `.scip` with atlas's own
decoder and scores a line-heuristic definition scanner against it. The corpus, the indexers and the run
scripts lived in a scratch folder outside the repository; section 2.1 lists the repositories and pins.

## 1. Where atlas is today

- Memory, search, projects and hooks are language agnostic. Only the code index is language specific:
  Rust from `rust-analyzer scip`, WGSL and `.proto` from owned scanners (`src/wgsl.rs`, `src/proto.rs`).
- `src/scip.rs` already decodes any SCIP file: it reads `Index`, `Document`, `Occurrence` (range,
  symbol, roles, `enclosing_range`) and `SymbolInformation` (kind, display name, enclosing symbol,
  relationships), and `scip::descriptors` parses the descriptor grammar of the SCIP spec, which every
  indexer shares. What is Rust specific sits above it in `src/code.rs`: `parse_symbol` reads
  rust-analyzer's `impl#[Type][Trait]` descriptors for impl edges, `rust_rows` maps kinds with a table
  of the values rust-analyzer emits, and item end lines come from the brace matcher
  (`digest::item_end`), not from SCIP.
- One root is one folder. `code_index::index_root` runs rust-analyzer only when the root has a
  `Cargo.toml` at its top; `--code-index --scip FILE` decodes a prebuilt file instead.
- Freshness: a changed root is re-indexed whole after a 20 s quiet time (one indexer at a time across
  processes, a lease row, deferred while a foreign `cargo` runs). Between full runs, a file changed by
  Edit or Write is re-indexed alone at once by the owned line scan (`code::reindex_file`): its SCIP rows
  are replaced by scan rows (definitions and ranges, no references) until the next full run. So every
  language atlas indexes needs an owned definition scanner anyway, even with an indexer present: it is
  what keeps a just-edited file's ranges right.
- `add-project` makes a folder a code root only when it holds a `Cargo.toml`, `.rs` or `.wgsl` file; a
  project's repositories (`resolve`) all become code roots.
- The code recall check (`recall::code_check`) greps every `.rs` and `.wgsl` file for item definitions
  with one regex per language (comments and strings excluded) and requires a definition row at the same
  line with the same name; rows that only the line scan produced count as `scan_only`.
- Test digests: `cargo test` and `cargo nextest` get totals and per-test failures (`digest::cargo_test`);
  every other runner gets the generic digest (tail and verdict).

## 2. The indexers, measured

### 2.1 Corpus

Medium, permissively licensed public repositories, shallow clones (no private code):

| Language | Repository | Licence | Commit | Files | Lines |
|---|---|---|---|---|---|
| TypeScript | sindresorhus/got | MIT | 6b5aafc | 92 `.ts` | 66,685 |
| JavaScript | GoogleCloudPlatform/microservices-demo `src/currencyservice` | Apache-2.0 | c7cb6d5 | 2 `.js` | 266 |
| JavaScript (added in PR 4) | expressjs/express | MIT | 43fca52 | 142 `.js` | 22,046 |
| Go | gin-gonic/gin | MIT | 0f09c3a | 99 `.go` | 24,556 |
| Python | pallets/flask | BSD-3-Clause | 49943b5 | 83 `.py` | 18,392 |
| Java | google/gson (Maven) | Apache-2.0 | 5773104 | 264 `.java` | 57,131 |
| Kotlin | square/kotlinpoet (Gradle) | Apache-2.0 | 4ce5d54 | 125 `.kt` | 39,929 |
| C# | serilog/serilog | Apache-2.0 | 43105b3 | 217 `.cs` | 24,939 |
| polyglot gRPC | GoogleCloudPlatform/microservices-demo (Go, C#, Python, Node, Java services over one `demo.proto`) | Apache-2.0 | c7cb6d5 | | |

The last one has the shape of the main target (a folder of services that talk over protobuf): Go services
with committed stubs (`genproto/*.pb.go`), Python with committed `demo_pb2*.py`, C# generating its stubs at
build time (`Grpc.Tools`, into `obj/`), Node loading the `.proto` at run time (no stubs).

Machine: Windows 10, 36 logical cores, 64 GB. Times are wall clock of one run; memory is the peak working
set of the whole process tree (sampled every 250 ms), or the container's cgroup `memory.peak` for the
Docker runs.

### 2.2 Getting and running each indexer

| Indexer (version) | How a user gets it | Runtime | Needs before it runs | Windows | Licence |
|---|---|---|---|---|---|
| scip-typescript 0.5.2 | `npm i -g @sourcegraph/scip-typescript` | Node 18+ | `node_modules` installed (a `tsconfig` that `extends` a package fails without it; `--infer-tsconfig` for plain JS) | yes | Apache-2.0 |
| scip-go 0.2.9 | `go install github.com/scip-code/scip-go/cmd/scip-go@latest` (the module moved from `sourcegraph/scip-go`; the old path no longer installs) | Go toolchain (0.2.9 needs Go 1.26; `GOTOOLCHAIN=auto` fetched it) | module download (`go list` does it) | yes | Apache-2.0 |
| scip-python 0.6.8 | `npm i -g @sourcegraph/scip-python` | Node, plus a Python interpreter for the environment | the project's virtualenv for dependency symbols (optional); `--project-version` when the folder is not a git checkout (it crashes otherwise) | **no**: crashes at startup (a `RegExp` built from `path.sep`); ran in the official Docker image | MIT |
| scip-java HEAD-20260402 (Docker image; Java, Kotlin, Scala) | Coursier `cs install scip-java`, or a bootstrap jar, or the Docker image | JVM 11+ and the build tool | a full compile through Maven, Gradle or sbt with the SemanticDB compiler plugin injected; Kotlin through `semanticdb-kotlinc` | **no**: it injects a POSIX shell `javac` wrapper and spawns `mvn` without `.cmd`; ran in the official Docker image | Apache-2.0 |
| scip-dotnet 0.1.0-SNAPSHOT | `dotnet tool install -g scip-dotnet` | .NET SDK 8+ (and whatever SDK the repo's `global.json` asks for: serilog needed 10.0) | NuGet restore and a design-time build (it runs them; source generators such as `Grpc.Tools` run, so generated stubs are indexed) | yes | Apache-2.0 |

Runs (one at a time, nothing else heavy on the machine):

| Run | Wall | Peak memory | `.scip` size | Note |
|---|---|---|---|---|
| got, scip-typescript, no `node_modules` | 1.0 s | 76 MB | none | fails: `@sindresorhus/tsconfig` not found |
| got, `npm install --ignore-scripts` | 31.5 s | 476 MB | | the prerequisite |
| got, scip-typescript | 17.8 s | 861 MB | 9.8 MB | |
| currencyservice (JS), `--infer-tsconfig` | 1.2 s | 140 MB | 29 KB | |
| gin, scip-go, cold (modules downloaded) | 53.8 s | 1,806 MB | 3.8 MB | |
| gin, scip-go, warm | 2.2 s | 96 MB | 4.1 MB | |
| microservices-demo frontend, scip-go, cold | 51.8 s | 986 MB | 1.3 MB | |
| microservices-demo checkoutservice, scip-go, warm cache | 3.2 s | 181 MB | 1.1 MB | |
| flask, scip-python (Docker) | 11.3 s | 470 MB | 2.7 MB | no virtualenv: dependency symbols unresolved |
| emailservice, scip-python (Docker) | 2 s | 168 MB | 142 KB | |
| gson, scip-java + Maven (Docker) | 50 s | 989 MB | 9.4 MB | the `gson` module only (`-pl gson -am`): the repo's `proto` module needs Maven 3.9.6, newer than the image's; the image defaults to JDK 8 (`JVM_VERSION=17` needed); Maven downloads included (a fresh container) |
| kotlinpoet, scip-java + Gradle (Docker) | 145 s | 2,579 MB | none | **fails**: "Plugin semanticdb_kotlinc ... is incompatible with the current version of the compiler"; kotlinpoet is on Kotlin 2, and semanticdb-kotlinc supports Kotlin 1.8 and 1.9 only |
| serilog, scip-dotnet (restore + build) | 51.8 s | 749 MB | 2.8 MB | |
| cartservice, scip-dotnet | 17.4 s | 254 MB | 302 KB | stubs generated into `obj/` and indexed |

atlas's own decoder read every file in 0.2 to 22 ms (got, the largest, 9.8 MB in 21.5 ms).

### 2.3 What each indexer's output holds

From `langs_spike` over each file. "defs" counts definition occurrences of non-local symbols; a
symbol's shape is the suffix of its last descriptor (`#` type, `(` method or function, `.` term, `/`
namespace, `)` parameter, `[` type parameter, `:` meta).

| | scip-typescript (got) | scip-go (gin) | scip-python (flask) | scip-java (gson) | scip-dotnet (serilog) |
|---|---|---|---|---|---|
| defs: types / callables / terms | 202 / 351 / 852 | 170 / 1,308 / 545 | 149 / 1,435 / 344 | 635 / 3,220 / 953 | 267 / 1,483 / 487 |
| other defs | 409 params, 71 type params, 96 file modules, **4,203 `:` object-literal members** | 99 packages | 1,979 params, 83 `__init__:` | 177 type params | 2,232 params |
| `SymbolInformation.kind` | yes | yes | **no** (all 0) | yes | **no** (all 0) |
| `display_name` | no | yes | no | yes | no |
| `enclosing_range` on types and callables | 426 of 553 | 1,466 of 1,478 | all 1,584 | 3,383 of 3,855 | **none** |
| implementation relationships | 112 | 516 | 101 | 1,202 | 637 |
| documentation | yes | yes (and signatures) | yes | yes (and signatures) | yes |
| refs to project symbols / to dependencies | 23,417 / 42,648 | 10,640 / 15,302 | 5,620 / 3,805 | 22,973 / 32,304 | 11,143 / 6,860 |
| refs inside a definition's enclosing range (caller known) | 19% | 85% | 95% | 96% | 0% |
| package in the symbol | npm name and version | module path and pseudo-version | `--project-name`, git commit | Maven coordinates and version | **none** (`nuget . .`) |
| `scip::descriptors` parse failures | 0 of 7,349 | 0 of 2,958 | 0 of 4,424 | 0 of 6,590 | 1 of 5,328 (`dynamic#`, a bare built-in) |

Findings that shape the design:

1. **atlas cannot read scip-go's output today.** SCIP now has typed ranges: `Occurrence` fields 8 and 9
   (`single_line_range`, `multi_line_range`) and 10 and 11 (the enclosing range) replace the packed
   `range` (1) and `enclosing_range` (7), which are deprecated; new producers "SHOULD NOT" set the old
   ones, and typed wins when both are present. scip-go 0.2.9 writes only the typed fields (all 48,118
   occurrences of gin), so `scip::decode` fails with "occurrence range is not 3 or 4 values". The spike
   rewrites typed ranges to packed ones to measure; the real fix is about 30 lines in `src/scip.rs` (a
   proto3 `int32` left out is 0, so a missing `line` is line 0). The other indexers still write packed
   ranges; rust-analyzer will move eventually too. This is PR 1 whatever else is decided.
2. **Two of the five do not run on Windows** (scip-python, scip-java), and the SCIP project itself
   moved from `sourcegraph` to `scip-code` (the scip-go module path changed with it). The main target
   machine runs Windows; which languages its service repositories use decides how much this matters
   (decision 2).
3. **scip-dotnet gives the least**: no kinds, no enclosing ranges, no package names. Kinds and item
   ranges then come from atlas's own line scan of the definition line (section 3 shows the brace matcher
   agrees with SCIP's ranges 96 to 99% where SCIP has them), and callers from those ranges.
4. **scip-typescript's `:` members are noise** for a symbol index (each property of an object literal
   passed to a call is a definition); they and file-module `/` definitions are not stored.
5. **Kind is missing in two indexers**, so kind words must not depend on it: suffix plus the leading
   keyword of the definition line (`class`, `def`, `interface`, `struct`, `record`, `enum`) gives the
   word when `kind` is 0.
6. **Implementation relationships exist in all five** non-Rust indexers measured, so impl edges become
   generic (from relationships), with rust-analyzer's `impl#[T][Trait]` reading kept for Rust only.
7. **Kotlin has no working SCIP indexer for current Kotlin.** scip-java's Kotlin support is the
   semanticdb-kotlinc compiler plugin, which supports Kotlin 1.8 and 1.9; on kotlinpoet (Kotlin 2) the
   compiler refused to load it, and upstream reports the same for Kotlin 2.x. Kotlin is scanner-only for
   now (definitions and ranges, no references) and moves to the generic SCIP path unchanged if that
   plugin catches up.
8. **scip-java is a build, not a scan**: it compiles the project with the SemanticDB plugin through the
   project's own build tool, so the project's build must work in that environment (the gson run needed
   JDK 17 and failed on a module whose plugin wanted a newer Maven). Its output is the richest of all
   (kinds, display names, ranges on 88% of items, 96% of references with a caller).

## 3. The fallback without an indexer: a heuristic scanner, measured

The spike's scanner is one generic engine (about 150 lines) driven by a per-language table: a few
definition regexes (name in the last group), comment syntax, block style (braces, or indentation for
Python), and which block kinds may hold member definitions (a class body, a Go `type (`/`const (`
group, a struct body). Scored against the SCIP definitions of the same files (types, callables and
terms; parameters, type parameters, `:` members and namespaces left out), by `(path, line, name)`:

| Language (repo) | Recall | Precision | Types | Callables | Terms | Scan time |
|---|---|---|---|---|---|---|
| TypeScript (got) | 91.7% (1,288/1,405) | 89.8% | 100% | 91.5% | 89.8% | 220 ms / 67k lines |
| Go (gin) | 96.4% (1,950/2,023) | 84.6% | 100% | 98.2% | 90.8% | 26 ms / 24k lines |
| Python (flask) | 94.2% (1,817/1,928) | 88.7% | 100% | 98.4% | 74.4% | 13 ms / 18k lines |
| Java (gson) | 96.3% (4,629/4,808) | 98.4% | 98.1% | 97.5% | 90.9% | 233 ms / 50k lines |
| Kotlin (kotlinpoet) | not scored: no SCIP truth (see 2.2) | | | | | |
| C# (serilog) | 94.3% (2,109/2,237) | 89.7% | 100% | 93.2% | 94.5% | 153 ms / 25k lines |

Type recall is 100% in four languages and 98.1% in Java (nested classes declared after an annotation on the same line). The misses are what a line regex cannot see: TS `#private`
methods and getters written `get x()` (a fixable pattern gap), Python attributes assigned through
`self.` in odd places and module attributes inside `if` blocks, C# explicit interface members
(`IEnumerable<T>.GetEnumerator`) and constructors named `.ctor` by SCIP. False positives are mostly
locals the regex takes for members (`const x` in a nested block, a Go struct literal's field). These
numbers are a first cut of each pattern row; tuning a row past 95% is short work, and the Rust line scan
sits at the same kind of bar today.

Item ranges, the part annotations depend on: atlas's brace matcher (`digest::item_end`, unchanged) and
an indentation matcher for Python, against SCIP's `enclosing_range` end line, for types and callables:

| | TypeScript | Go | Python | Java | C# |
|---|---|---|---|---|---|
| ends equal | 95.8% (408/426) | 97.5% (1,429/1,466) | 98.8% (1,565/1,584) | 99.0% (3,349/3,383) | SCIP has none |

The disagreements are mostly where SCIP's range is the declaration alone (a Go `type X struct` spec's
name line, a Python module attribute) while atlas's covers the body, which is what a reader wants.

What tree-sitter would add: real parse trees, so locals are never members and nested blocks are exact.
It is a C library plus one C grammar crate per language (a C compiler in the build, and large generated
parser tables); that is several new crates against the no-dependency rule, for a few points of recall over
the owned scanner. Not added; it is option C of decision 3.

## 4. Who runs the indexer, and freshness

Today: atlas starts rust-analyzer itself (found configured, on `PATH`, or in rustup), debounced, one at a
time, at below-normal priority, killed at a timeout; a just-edited file is re-scanned alone at once.

Proposal: the same machinery, driven by a table instead of Rust-only code, plus a prebuilt-file path.

- **Driver table** (data, one row per indexer): marker files, binary names and where else to look (`PATH`,
  `npm root -g`, `GOBIN`/`GOPATH/bin`, `~/.dotnet/tools`, Coursier's bin), the arguments (output file,
  project name and version where needed, `--infer-tsconfig` when there is no `tsconfig.json`), the
  prerequisite check (for example `node_modules` present; without it scip-typescript still runs unless a
  `tsconfig.json` extends a package, and a failed run names `npm install`),
  and the files whose change makes the root stale. atlas never installs an indexer or runs a package
  manager: an indexer that needs `npm install` or a restore it cannot do itself says so in the `stale`
  line, and the scanner covers the root meanwhile. (scip-go and scip-dotnet fetch modules and packages
  themselves; that is their behaviour and is said in the docs.)
- **Prebuilt files**: a root (or unit) may name a `.scip` file, or one is found by convention
  (`<unit>/index.scip`, written by CI or by the user, for example from WSL or Docker on Windows). atlas
  decodes it when its mtime changes; source files newer than it are re-scanned by the scanner, so a
  stale prebuilt index degrades to scan rows file by file, never to wrong line numbers.
- **Both**, with the prebuilt file winning when it is newer than the last run. This is what makes
  scip-python and scip-java usable for a Windows user at all (decision 2).

Freshness per language: a full re-index costs 2 s (Go, warm) to 52 s (C# with restore); every one fits
the existing debounce-and-background model unchanged. The per-file path (`reindex_file`) becomes
"scanner for the file's language", so an edited Go or TS file has exact ranges again at once and its
references return at the next full run, as for Rust today.

## 5. What is per-language and what is generic

The design goal is that a new SCIP language is a table row, not new code. Measured above: the decoder,
the descriptor grammar and the five indexers' symbol strings are already uniform.

| Concern | Generic (one implementation) | Per language (data in a table) |
|---|---|---|
| Decoding `.scip` | `scip.rs`, plus typed ranges | nothing |
| Symbol strings | descriptor grammar (`scip::descriptors`), name = last descriptor, container rule (section 6) | constructor spellings (`<constructor>`, `<init>`, `.ctor`, `__init__`), which suffixes are noise (`:` for TS) |
| Kinds | SCIP `Kind` enum mapped once to atlas words | the definition-line keywords used when `kind` is 0 |
| Item ranges | the brace or indent matcher, as for Rust today (it agrees with SCIP 96 to 99% and is the better of the two where they differ); SCIP's enclosing range only where the matcher finds no body | block style: braces or indentation |
| Callers (`within`) | innermost enclosing definition range (from the item ranges above, so C# gets callers too) | nothing |
| Impl edges | `Relationship.is_implementation` | Rust keeps its `impl#` reading |
| Running the indexer | `run_bounded`, lease, debounce, timeout, priority, prebuilt files | the driver row: markers, binary, arguments, prerequisites, stale files |
| Code roots and units | the marker walk, unit path rebasing | marker files, workspace files, skipped folders (`node_modules`, `vendor`, `obj`, `bin`, `build`, `dist`, `.venv`, `__pycache__`) |
| No-indexer fallback and per-file freshness | the scanner engine | definition regexes, comment syntax, member-holding blocks |
| Recall check | the check | the truth regex per language |
| Test digests | totals, failures by test, verdict, slicing | the runner row: detection, totals and failure patterns |
| Generated-stub detection (protobuf) | the rpc-to-stub linking | generated-file patterns (`*.pb.go`, `*_pb2*.py`, `*Grpc.cs`, `*Grpc.java`, `*_pb.d.ts`) |

A language with a SCIP indexer and no table row still works through `--scip FILE` or a prebuilt
`index.scip`: generic kinds, the matcher's ranges, no fallback scanner. A table row adds
auto-running, add-project detection and the fallback.

## 6. Schema and tools

`code_occ`, `code_lines`, `code_counts` stay as they are: names, kinds and containers are strings, and
`code_files.lang` already exists. Changes (schema `code-6`, a rebuild like every schema bump):

- **`code_units`** (new): `root, dir, lang, source (indexer name, "prebuilt", or "scan only: why"),
  fingerprint, indexed, elapsed_ms`. A root holds units (a Go module, a TS project, a .NET solution, a
  Python project, a Gradle or Maven build); each unit is indexed and timed on its own, and SCIP paths
  (relative to the unit's folder) are rebased to the root. `code_roots.rust` becomes a summary of its
  units' notes.
- **Rows from any SCIP language** (`scip_rows`, the generic sibling of `rust_rows`): name = last
  descriptor (constructors stored under their type's name with kind `ctor`); container = the nearest
  enclosing `#` type, else the nearest callable or term (a Python class inside a function), else the last
  namespace segment (the file stem for TS, the package's last path element for Go, the module's last
  dotted part for Python, the Java package's last segment); parameters, type parameters, locals, `:`
  members, file modules and package namespaces are not stored as definitions.
- **Kinds**: SCIP `Kind` mapped once to the short words atlas prints (`fn`, `method`, `class`, `struct`,
  `interface`, `enum`, `variant`, `field`, `const`, `var`, `type`, `mod`, `ctor`, `property`); `kind` 0
  falls back to suffix plus the line keyword.
- **End lines**: `digest::item_end` (braces) or the indent matcher (Python), chosen by `code_files.lang`,
  as for Rust today; SCIP's enclosing range only where the matcher finds no body. Callers come from these
  ranges, so they no longer depend on the indexer writing enclosing ranges (TS 19% and C# 0% of references
  have a caller from SCIP's ranges alone).
- **References into another configured root**: today dependency references are dropped. A reference whose
  symbol's package (npm name, Go module, Maven coordinates, Python project name) is the package another
  root of the project defines is stored, under that name, so a Go service importing a shared module in a
  sibling repository gets real cross-repo references. Not possible for C# (scip-dotnet writes no package).
  Built in PR 8 as written: packages compare by manager and name (the version is never compared), the
  packages each root defines and uses are recorded in `code_packages` (schema `code-7`), and any configured
  root counts, not only the project's (see the PR 8 row).
- **`symbol`**: `Container::name` also accepts `Container.name` (split at the last `.` only when the whole string is not itself a stored name, since C# names such as `.ctor` hold dots); a `lang` argument filters (optional,
  for names common to several languages); the answer is otherwise unchanged (the path says the language).
- **Grep annotations and Read notes**: unchanged code paths; they work for any file with definition rows
  (`file_items`). `relevant`, `walk` and `reindex_file` take their extension list from the table.
- **`lookup`**: its fallback to `symbol` for unregistered names already covers every language.

## 7. Symbol strings per scheme (measured samples)

All five follow the SCIP grammar (`scheme manager package version descriptors`); `scip::descriptors`
parsed every one of 20,000+ symbols except one bare built-in.

| Indexer | Example | Package part | Namespace descriptors |
|---|---|---|---|
| rust-analyzer | `rust-analyzer cargo demo 0.1.0 store/Store#open().` | crate, version | modules |
| scip-typescript | ``scip-typescript npm got 16.2.0 source/core/`standard-schema.ts`/StandardSchemaV1Issue#`` | npm name, version | folders, then the file (backticked) |
| scip-go | ``scip-go gomod github.com/gin-gonic/gin 0f09c3a9b462 `github.com/gin-gonic/gin`/authPairs#searchCredential().`` | module path, pseudo-version; the standard library is `github.com/golang/go/src .` | the import path, one backticked descriptor |
| scip-python | ``scip-python python flask 49943b5... `examples.tutorial.tests.conftest`/AuthActions#`` | `--project-name`, version (git commit by default) | the dotted module, backticked |
| scip-java | `semanticdb maven maven/com.google.code.gson/gson 2.15.1-SNAPSHOT com/google/gson/JsonNull#deepCopy().` | Maven coordinates | the package, one descriptor per segment |
| scip-dotnet | ``scip-dotnet nuget . . Capturing/PropertyValueConverter#DepthLimiter#`.ctor`().`` | none | namespace segments |

Odd names a generic parser must read whole: C# explicit interface members
(`System.Collections.Generic.IEnumerable<...>.GetEnumerator`, one backticked name; stored since PR 4 under
the member's own name), TS `<get>x` and `<constructor>`, Java `<init>`, C# `.ctor` and `.cctor`.

## 8. add-project: detecting code roots and units

`has_code` becomes "any unit marker or any source file of a known language". Markers per language:

| Language | Unit markers | Workspace (one run covers the members) |
|---|---|---|
| TypeScript / JavaScript | `tsconfig.json`, else `package.json` (JS: `--infer-tsconfig`) | `package.json` `workspaces`, `pnpm-workspace.yaml`, `lerna.json` |
| Go | `go.mod` | `go.work` (scip-go still runs per module) |
| Python | `pyproject.toml`, `setup.py`, `setup.cfg`, else `requirements*.txt` (the shallowest claims its subtree) | none: one unit per project |
| Java / Kotlin / Scala | `build.gradle(.kts)`, `pom.xml`, `build.sbt` | `settings.gradle(.kts)`, a parent `pom.xml` with `<modules>` |
| C# | `*.sln` / `*.slnx`, else `*.csproj` | the solution |
| Rust | `Cargo.toml` | the workspace `Cargo.toml` |

Monorepos: walk the root (skipping the folders above); a workspace marker claims its subtree; otherwise
the shallowest marker of a language claims its subtree, except Go, where every `go.mod` is its own unit.
Several languages in one repository (a Go service with a TS frontend) give several units in one root.
The project folder of ~10 service repositories needs nothing new: each repository is already a code root
(`project::resolve`), and each gets its units.

## 9. Test digests for other runners

The cargo digest's parts are generic (totals, per-test status, failure blocks with a location and a
message, verdict, `slice --failures/--item`); what differs is detection and patterns. Other runners get
`Shape::Test(runner)` with one row per runner (`digest::RUNNERS`; `go test` is the first, PR 2); cargo keeps
its own digest, whose libtest and nextest formats and panic messages fit no simpler row:

| Runner | Detect (command, else text) | Totals | Per-test failure | Location and message |
|---|---|---|---|---|
| `go test` | `go test`; `^(ok\|FAIL)\s+\S+\s+[\d.]+s` | count `--- PASS/FAIL/SKIP` lines; package `ok`/`FAIL` lines | `--- FAIL: TestName (0.00s)`, block until the next `---`/`===` | `file_test.go:42: message` lines in the block; `panic:` plus the first frame |
| `go test -json` | `"Action":` lines | `pass/fail/skip` actions with `Test` | `"Action":"fail"` | the test's `output` events |
| pytest | `pytest`, `py.test`; `=+ .* in [\d.]+s =+` | the final `N failed, M passed, K skipped` line | `_____ test_name _____` blocks under `FAILURES`; the `short test summary info` `FAILED path::test - msg` lines | `path.py:NN: ExceptionType` |
| jest | `jest`; `^Tests:\s+` | `Tests: 1 failed, 5 passed, 6 total` (and `Test Suites:`) | `● Suite › test name` blocks | the `at ... (file:line:col)` frame inside the project, the first `Expected`/`Received` lines |
| vitest | `vitest`; `Test Files\s+` | `Tests  1 failed \| 5 passed (6)` | ` FAIL  file > suite > test` | `❯ file:line:col` |
| Gradle (JUnit) | `gradle`, `gradlew`; `> Task :.*test` | `N tests completed, M failed` | `ClassName > method() FAILED` | the next indented exception line and `at` frame |
| Maven Surefire | `mvn`; `Tests run:.*Failures:` | the last `Tests run: N, Failures: F, Errors: E, Skipped: S` | `[ERROR] method(Class)` / `[ERROR]   Class.method:NN` | the `<<< FAILURE!` line's exception |
| `dotnet test` | `dotnet test`; `^(Passed\|Failed)!\s+-` | `Failed!  - Failed: 1, Passed: 5, Skipped: 0, Total: 6` | `  Failed Name [12 ms]` | `Error Message:` and `Stack Trace:` blocks, the `in file:line N` frame |

Each row is tested against real outputs captured from the corpus (a passing run and one with a failing
test added), kept as fixtures. Build errors per toolchain (tsc `file(l,c): error TS`, `go build`
`file:l:c:`, javac, `dotnet build` `file(l,c): error CS`) are a smaller second table on the same model.

## 10. Cross-repo: protobuf services and their stubs

Today `.proto` files give messages, services and rpcs, and other files' words that match an rpc name (as
written, lowerCamel or snake_case) are `call (mention)` references. Measured on microservices-demo, SCIP
adds exactly what mentions cannot (rpc names from `protos/demo.proto`, references from hand-written
files only):

| Service | rpcs with a SCIP call site | SCIP call sites | mention lines today |
|---|---|---|---|
| frontend (Go, committed stubs) | 11 of 15 | 21 | 27 |
| checkoutservice (Go) | 8 of 15 | 10 | 19 |
| emailservice (Python, committed `demo_pb2_grpc.py`) | 1 of 15 | 1 | 3 |
| cartservice (C#, stubs generated into `obj/`) | 0 of 3 (it is a server) | 0 | 3 |
| currencyservice (Node, `.proto` loaded at run time) | none possible: no stub symbols | 0 | (mentions only) |

- **Stubs**: every rpc's client and server stub definitions are in the SCIP index (`genproto/demo_grpc.pb.go`,
  `demo_pb2_grpc.py`, `obj/.../CartGrpc.cs`), recognisable by generated-file patterns. A stub symbol named
  after an rpc in such a file is linked to that rpc: `symbol GetCart` answers the `.proto` rpc, the stubs
  in each language, and then the real call sites.
- **Server implementations**: the server side is an implementation of the stub's base class or interface
  (C# `CartService : Hipstershop.CartService.CartServiceBase` overrides `GetCart`; a Go server struct
  implements `CartServiceServer`), and every indexer measured emits implementation relationships
  (cartservice 120, serilog 637, gin 516, gson 1,202). So `symbol GetCart` can list "implemented in
  cartservice" with no name guessing; that the override-to-base edge is among cartservice's 120 is the
  first thing PR 7 checks.
- **Precision**: mentions count a word on any line (Go frontend: 27 lines for 21 real calls, including
  the service's own same-named wrapper functions); SCIP call sites are exact and carry their caller.
- **Where SCIP cannot help**: dynamic loading (Node's `@grpc/proto-loader`, Python reflection) leaves only
  mentions, which stay as the fallback, marked as today.
- **Messages**: a message's generated type (Go `demo.pb.go` structs, Python `demo_pb2` classes, C#
  `Cart.cs`) links to the `.proto` message by name the same way, so `symbol CartItem` shows the proto
  definition, each language's type, and every use.

## 11. Measuring it: a per-language recall check and a corpus

Modelled on the Rust check (grep truth per line, the index must have each definition at that line):

- `--recall-check` takes a truth regex per language (one row per language, like `RUST_DEF_RE`, comments
  and strings excluded) and reports, per language, `indexed` (SCIP rows), `scan_only` and misses. With an
  indexer this measures the row building and the indexer's coverage (files outside every `tsconfig`, Go
  files behind build tags, C# files excluded from the project); without one it is the scanner against
  grep.
- A dev-only `cargo xtask langs-bench` reruns the spike's three numbers per language over pinned corpus
  commits: scanner recall and precision against SCIP, item-end agreement, and SCIP-row recall against the
  grep truth. Each language's PR states them; floors (scanner recall 90%, type recall 100%, item ends 95%)
  are tests on small fixtures, not on the corpus.
- CI cannot run the indexers (and its minutes are scarce): the fixtures are tiny projects written for atlas
  (one per language, our own code, Apache-2.0), each with a checked-in `.scip` produced by a pinned
  indexer version and regenerated by an xtask. Unit tests decode them and assert rows, kinds, ranges,
  callers and impl edges.
- Corpus for the benches (public, permissive, pinned): the table in 2.1, plus microservices-demo for the
  protobuf linking.

## 12. The wider indexer ecosystem

Surveyed, not measured. "Generic path" means it works through the generic rows with no new code, given a
`.scip` file.

| Indexer | Languages | Runtime | Build prerequisite | Windows | Maturity |
|---|---|---|---|---|---|
| rust-analyzer `scip` | Rust | none (the binary) | none | yes | in use by atlas |
| scip-typescript | TS, JS | Node | `node_modules` | yes | maintained, widely used (measured) |
| scip-go | Go | Go | module download | yes | maintained, now under `scip-code` (measured) |
| scip-python | Python | Node + Python | optional virtualenv | no (startup crash; Docker/WSL) | maintained (measured) |
| scip-java | Java, Scala (Kotlin 1.8/1.9 only) | JVM + Maven/Gradle/sbt/Bazel | a full compile with SemanticDB | no (Docker/WSL) | maintained; Kotlin plugin lags Kotlin 2 (measured) |
| scip-dotnet | C#, VB | .NET SDK | restore and build | yes | young (0.1 SNAPSHOT); no kinds or ranges (measured) |
| scip-clang | C, C++ | none (binary) | `compile_commands.json` (CMake `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON`, Bear for Make) | no binaries (Linux and macOS only; Windows "being explored") | maintained, used on large codebases |
| scip-ruby | Ruby | none (Sorbet-based binary) | Sorbet-compatible project | no (Linux, macOS) | less active |
| scip-php (davidrjenni) | PHP | PHP 8 + Composer | `composer install` | unverified | community |
| scip-dart (Workiva) | Dart | Dart SDK | `pub get` | likely (Dart is cross-platform), unverified | maintained by Workiva |
| LSIF indexers via `scip convert` | various (older lsif-* tools) | the `scip` CLI | per tool | per tool | legacy route |

Every one writes the same SCIP schema; with typed ranges decoded and the generic rows of section 6, each
slots in with a driver row (markers, binary, arguments) and, optionally, a scanner row for the fallback.
C and C++ on Windows would need `compile_commands.json` plus a Linux run (WSL), the same story as Java.

## 13. Decisions for Brandon

**Decision 1. Who runs the indexers?**

- **A (recommended). atlas runs any indexer it finds, through a driver table, and also reads prebuilt
  `.scip` files** (configured, or `index.scip` by convention; the newer wins). Same safety as rust-analyzer
  today (background, lease, debounce, priority, timeout); atlas never installs tools or runs package
  managers. Works out of the box where the tools are on `PATH`, and covers Windows gaps and CI. Cost: two
  input paths to test.
- B. Prebuilt files only (users or CI produce `.scip`). Simplest and safest (atlas starts nothing new),
  but nothing is indexed out of the box and freshness depends on the user re-running the tool.
- C. atlas runs indexers only (no prebuilt files). Uniform with Rust, but scip-python and scip-java are
  then unavailable on Windows.

**Decision 2. scip-python and scip-java on Windows** (both fail natively).

- **A (recommended). Scanner fallback natively, plus prebuilt files** (from CI, WSL or Docker), and a
  small upstream fix offered for scip-python's startup crash (a `RegExp` from `path.sep`); scip-java's
  shell-wrapper approach is not a small fix. Honest and dependency-free; Java and Python on Windows get
  definitions and ranges, references only with a prebuilt file.
- B. atlas runs those indexers through WSL (`wsl.exe`) when present. Full fidelity on Windows, but atlas
  then drives a second OS: paths translate, the repo must be visible to WSL, and JVM builds there are
  slow over the Windows file share.
- C. Docker images. Full fidelity, but a multi-GB image (scip-java 4.2 GB) and Docker as a dependency.

**Decision 3. The fallback when no indexer is present** (and for per-file freshness, needed regardless).

- **A (recommended). Owned scanners: one generic line engine, one data row per language** (measured 92 to
  96% definition recall, 100% of types, 96 to 99% item-end agreement; no new crates).
- B. Owned hand-written scanner per language (like `wgsl.rs`, `proto.rs`). A few points more recall per
  language, several times the code and tests.
- C. tree-sitter (new crates: the C library and one grammar crate per language, a C compiler in the
  build). Best fallback fidelity, against the no-dependency rule.

**Decision 4. Protobuf across repositories.**

- **A (recommended). Link generated stubs and server implementations to the `.proto` rpc and message,
  and use SCIP call sites; keep `call (mention)` rows only for files without SCIP coverage** (Node with
  run-time loading, scan-only roots). `symbol GetCart` then answers proto, stubs, implementations and
  exact callers across the project's repos.
- B. Keep mentions only (no change). Cheap, but overcounts and cannot say who implements an rpc.
- C. A plus cross-root package references (section 6) in the same slice. More reach (shared Go modules,
  npm packages between repos), more to review at once; better as its own slice.

## 14. Phased plan

Order: **Go first** (native on Windows, fast, the richest output: kinds, display names, ranges on 99% of
items, 85% of references attributable to a caller, and the language of the stubs in the target setup),
then **TypeScript/JavaScript** (native, rich, the `node_modules` prerequisite is the only wrinkle), then
**C#** (native; the generic fallbacks for kinds and ranges get their real test), then **Python** and
**Java/Kotlin** (prebuilt-or-scanner on Windows, per decision 2).

| PR | Content | Measured by |
|---|---|---|
| 1 | `scip.rs`: typed ranges (fields 8 to 11), proto3 zero defaults; generic `scip_rows` (names, containers, kind table, relationships as impl edges, enclosing or matcher end lines); schema `code-6` with `code_units`; `--code-index --scip FILE` for any language | decode tests on tiny fixture `.scip` files from each measured indexer; Rust `--symbol-bench` and recall unchanged |
| 2 | The driver and scanner tables; Go row: marker walk, units, `go.mod` detection in add-project, scip-go driver, Go scanner, per-file re-scan, Go truth regex in `--recall-check`; `go test` digest | Go fixture; gin numbers from `xtask langs-bench` (target: scanner recall 95%+, item ends 97%+, SCIP rows 100% of grep truth). Done: scanner recall 99.7% (precision 100%), item ends 100%, SCIP rows 100% of the truth in the files scip-go indexes (34 more in build-tag files from the scanner, none missing) |
| 3 | TypeScript/JavaScript row (workspaces, `--infer-tsconfig`, the `node_modules` prerequisite note); jest and vitest digests | got and currencyservice numbers. Done: got (scip-typescript 0.5.2): scanner recall 98.8% (precision 93.5%; types and callables 100%), item ends 99.8%, SCIP rows 100% of the truth in the files scip-typescript loads (none from the scanner, none missing); currencyservice: 100% on all three. The inferred configuration is written into atlas's own folder and passed as the project, because scip-typescript's `--infer-tsconfig` writes a `tsconfig.json` into the project. Also `code.indexers`, and the JSON-lines runner mode (`go test -json`). Follow-ups (in PR 4): a unit without `node_modules` is run anyway unless a `tsconfig.json` of it `extends` a package, and the fix is named only when such a run fails; units in test-input folders (`fixtures`, `testdata`, ...) are left out of the root's note; expressjs/express at 43fca52 as a second JavaScript corpus, run without `node_modules`: 100% on all three (527 definitions, 55 item ends, 461 of 461 grep truth) |
| 4 | C# row (`.sln`/`.csproj`, `global.json` SDK note); `dotnet test` digest | serilog numbers; kinds and ranges from the fallbacks. Done: serilog (scip-dotnet 0.2.16): scanner recall 99.8% (precision 99.0%; types and callables 100%); item ends: scip-dotnet writes no enclosing ranges, so the scanner is measured against the brace matcher, 98.3% equal, and each of the 30 differences is the matcher stopping at a base list's or a `where` clause's comma; SCIP rows 100% of the truth in the files scip-dotnet loads (1,711 SCIP rows, 20 filled by the scanner in inactive `#if` branches, none missing). Engine additions: marker and stale-file patterns (`*.csproj`), bodies on the next line (`ScanRow::allman`, with `#if` branches sharing one body), named blocks (`Does::Open` with a `name` group), keywords that are never names, the scanner filling a covered file's definitions (`Lang::fills`), the scanner's end for SCIP rows with neither a brace body nor an enclosing range, driver environment, `{marker}` and failure hints, documents another unit already gave skipped, summed runner totals and a `line` group for frames |
| 5 | Prebuilt `.scip` files (decision 1A) and the Python row; pytest digest | flask numbers from a Docker-made file; stale-file degradation test. Done: flask (scip-python 0.6.8, run in its Docker image): scanner recall 99.7% (precision 97.6%; types and callables 100%, terms 98.3%), item ends 100% (1,584 of 1,584), SCIP rows 100% of the truth in the files scip-python loads (1,622 `def` and `class` lines: 1,584 SCIP rows, 38 filled by the scanner, which are `@overload` stubs, property setters and repeated definitions scip-python leaves without a definition; none missing). Stale-file degradation: with the flask index as a prebuilt `index.scip` and `src/flask/app.py` edited after it, that file's 41 definitions come from the scanner at their new lines (1,543 SCIP rows, 79 scanner, none missing). Prebuilt files: an `index.scip` in a unit's folder, or `code.roots[].scip` (`true`, `false`, a path for the root's folder, or an object from unit folder to a path or `false`, as `code.indexers` is shaped); read when the unit has no indexer, when the file is newer than the indexer's last run and than every source file of the unit, or when the run fails; documents whose source is newer than the file are left to the scanner; the file is in the root's fingerprint. scip-python is not looked for on Windows (a configured path still runs), and the unit's note says to write `index.scip` in WSL or Docker; it runs as `index . --project-name {unit folder} --project-version 0`. Engine additions: indentation blocks (`ScanRow::indent`; a row could not say where a block ends without brackets), blocks that are no scope (`SAME_SCOPE`: an `if` or `try` body holds its scope's definitions), members whose block keeps the class as container (`Does::Member`: a method's `self.x` and nested functions are the class's in SCIP), kinds defined once per container (`ScanRow::once`: SCIP defines an attribute where it is first assigned), chained assignment targets, `*` anywhere in a marker or skipped-folder pattern (`requirements*.txt`, `*.egg-info`), markers that are source files (`setup.py`), `{name}` in driver arguments, drivers that do not run on a platform (`Driver::not_on_windows`), runners without group lines (pytest), and the recall check holding a constructor's own spelling (`def __init__`) by its constructor row |
| 6 | Java row (prebuilt or Docker/WSL on Windows); Kotlin scanner row (scanner-only until semanticdb-kotlinc supports Kotlin 2); Gradle and Maven digests | gson numbers; Kotlin scanner against the Kotlin truth regex. Done: gson (scip-java HEAD-20260402-1750, run in its Docker image on the `gson` module): scanner recall 99.0% (precision 93.7%: the extra rows are anonymous classes' members, which scip-java keeps local and the scanner defines and fills in; types 95.8%, callables 99.9%, terms 97.1%; the misses are mostly record components), item ends 99.9% (3,461 of 3,466; the 5 are local records whose SCIP range is the header), SCIP rows 100% of the truth in the files scip-java's build compiled (3,749 SCIP rows and 268 filled by the scanner of 4,017, none missing), and the 563 in the modules it did not build from the scanner, none missing. kotlinpoet (no indexer: semanticdb-kotlinc does not load in Kotlin 2): scanner recall 99.5% of the Kotlin truth (2,865 of 2,879), every scanner row on a truth line right, item ends 97.8% equal to the brace matcher's (1,766 of 1,805; the matcher counts braces in raw strings). Decisions built (option A each, for Brandon to confirm): scip-java is never run on a unit of Kotlin alone (`Lang::unindexed`), anonymous classes' members are filled in by the scanner, and a passing Gradle run's digest says `(Gradle lists failures only)` rather than inventing a passed count. Engine additions: an opaque scope for rules inside function bodies (local and anonymous classes), Allman bodies opened by a deeper `throws` or `implements` line, a deeper `;` line ending a bodiless declaration, continuation lines read as expressions, `ScanRow::header_continued` (Kotlin's `private constructor(` under its class), string templates running across lines (`Quote::template`), blocks no rule names taking the container around them, `Each` definitions taking the container of the scope their line opens, a language atlas runs no indexer for on its own units (`Lang::unindexed`), runner totals summed over several counts and a passed count derived from a total, a per-runner quiet note, and a scanner-only `langs-bench` for a language without an indexer. Deferred: record components, backticked Kotlin names, local Kotlin functions. Also: the Windows file watcher arms its first directory read before `start` returns (a change made right after start was lost under load) |
| 7 | Protobuf stub and implementation linking (decision 4A) | microservices-demo: rpcs with exact call sites and implementations per service. Done: microservices-demo (11 services, one root; prebuilt indexes from scip-go 0.2.9 for frontend, checkout, shipping and productcatalog, scip-dotnet for cart, scip-python 0.6.8 for email and recommendation, scip-java for ad, scip-typescript 0.5.2 for currency and payment): all 15 rpcs of `protos/demo.proto` (9 services) linked to their stubs, 216 links (Go 4 copies, Python 2, Java 1, C# 1 for CartService; one per file, side and rpc); 32 exact call sites through stubs for 14 rpcs (frontend 11, checkoutservice 8, cartservice's tests 9 through `GetCartAsync` and its kin, recommendationservice 2 (its server calls ListProducts, its test client ListRecommendations), adservice's and emailservice's test clients 1 each); 13 implementations of 12 rpcs (Go by an embedded `Unimplemented{S}Server`, C# by `CartServiceBase`, Java by `AdServiceImplBase`, Python by `{S}Servicer`, emailservice's two through its hand-written base class); no caller: SearchProducts (nothing in the demo calls it; productcatalogservice's test calls the implementation directly); no implementation: Convert, GetSupportedCurrencies and Charge, served by Node services that load `demo.proto` at run time and register handlers in an object (no generated server type), whose calls and registrations stay `call (mention)` rows; `call (mention)` rows down from 1,017 to 7 (the 1,010 dropped were in generated code, comments, strings, definitions' names and words SCIP resolved). Built: per-language `Grpc` rows (generated-file patterns, stub and server method names, server base types), `proto::link_rows`, mentions on blanked code only, file ids putting `.proto` first and generated code last. Deferred: messages' generated types (they answer by name already where spelled alike), stubs in another root (PR 8), Node's `addService` registrations |
| 8 | Cross-root package references | a two-repo fixture (a Go module used by a sibling service). Done: a reference whose SCIP symbol has no definition in its own index is stored, with its caller, when its package (manager and name; the version is never compared, since a `replace`d sibling module is `.`, the library's own index says its commit and a release its tag) is one another code root defines; other dependencies' references are dropped as before. Each root records the packages its indexes define and use (`code_packages`, schema `code-7`, migrated in place from `code-6` with every root re-indexed once); a root reads the other roots' recorded packages, never their rows or indexers, so each root is indexed on its own; when a root starts or stops defining a package, the roots using it are marked stale (fingerprint cleared) and re-indexed on the next walk. Per-language rules are data: `Packages` in each row (managers, and the name fold: PEP 503 for Python, as written for the rest; a manager no row names, rust-analyzer's `cargo`, compares as written); a package named `.` is never linked (scip-dotnet's own symbols, scip-java's symbols outside a build module), so C# is not linked. Calls into another root's generated gRPC stubs are rpc call sites (the PR 7 deferral): the defining root's definitions under a stub's name in generated files are read when the root has cross-root references. Measured: Go library and service through `replace ../lib` (scip-go 0.2.9): 17 of 17 hand-counted references stored, 0 false; microservices-demo's frontend and checkoutservice with `genproto` moved into a third root (a shared module): 198 and 174 references, exactly the references the same services have into `genproto` when it is their own, 0 false, and the same 19 rpc call sites as in the one-root layout; a TypeScript package used by a sibling through a `file:` dependency (scip-typescript 0.5.2): 22 of 22, 0 false (two constructor parameter properties are parameters, stored by no root). Every language bench, Rust `--recall-check`, `--symbol-bench` (104 tokens per answer) and microservices-demo (one root) unchanged: no reference crosses a root there. Decided by the builder, for Brandon to confirm: versions are ignored; linking spans every configured root, not only one project's, and does not follow `talks_to`. Deferred: C# (no package names), a real rust-analyzer measure (tested on a synthetic index), Python across roots (atlas names a scip-python unit after its folder, which matches the distribution name only when they agree) |

Each PR updates `wiki/Code-index.md`'s language table and states its language's three numbers.
