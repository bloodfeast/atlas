`symbol` answers what a grep and several file reads would, from one pre-sorted table. Code roots are set
under `code` on [[Configuration reference]].

```
symbol {name: "Store"}
defs 1
loc	kind	in	snippet
app:src/lib.rs:3	struct		pub struct Store { pub items: Vec<u32> }
refs 2
loc	in	snippet
app:src/lib.rs:6	Store::new	pub fn new() -> Store { Store { items: Vec::new() } }
app:src/shapes.rs:1	shapes::area	pub fn area() -> u32 { let s = crate::Store::new(); s.open() }
impls 2
loc	impl
app:src/lib.rs:5-7	Store
app:src/lib.rs:8-10	Open for Store
```

- **Rust** comes from `rust-analyzer scip`, the one program atlas starts (`src/code_index.rs`). It is found as
  configured, on `PATH`, or in a rustup toolchain, and must answer `--version`. It runs on a background thread,
  never on the query path: at below-normal priority, its stderr to `code/<root>.ra.log`, killed at the timeout,
  one at a time across every atlas process on the database (a lease row), and only once a changed root has
  been quiet for the debounce time (`.rs`, `.wgsl` and Cargo manifest changes, seen by the same change
  notification as the vaults; a stat-walk fingerprint at startup and every 30 s as the safety net). Its
  configuration enables every Cargo feature and turns build scripts and proc macros off, so no compiler runs.
  Queries read the last good index; while a root is pending, running or failed, the answer says so in a
  `stale` line.
- The `.scip` file is decoded by an owned protobuf wire reader (`src/protobuf.rs`) and SCIP schema decoder
  (`src/scip.rs`) into struct-of-arrays columns with symbols interned as slices of the file. Both range forms
  are read: the packed `range` and `enclosing_range` that rust-analyzer writes, and the typed ranges
  (`single_line_range`, `multi_line_range` and their enclosing pair) that newer indexers such as scip-go write
  instead; the typed form wins when an occurrence has both. rust-analyzer writes no `relationships`, so impl
  edges come from its impl symbols (`impl#[Type][Trait]`), and callers from the innermost enclosing definition
  range of each reference. References into dependencies are not stored.
- **Other languages from a SCIP file**: `atlas --code-index NAME --scip FILE` reads an index written by any SCIP
  indexer (scip-go, scip-typescript, scip-python, scip-java, scip-dotnet and others), run by you at the root's
  folder so its paths are relative to the root. The rows are built the same way for every indexer, from the
  symbol grammar they share: the name is the last descriptor (a constructor, `<constructor>`, `<init>`, `.ctor`
  or `__init__`, is stored under its type's name with kind `ctor`, as is C#'s static `.cctor`; a C# explicit
  interface member, named with its interface, is stored under its own name); the container is the nearest enclosing type,
  else function, else the last namespace segment (Go's package, the TypeScript file's stem, the Python module);
  parameters, type parameters, namespaces and TypeScript's object-literal members are not stored. Kinds come
  from the index (`class`, `interface`, `struct`, `method`, `fn`, `field`, `property`, `const`, `var`, ...);
  when the indexer gives none (scip-python, scip-dotnet), from the keyword before the name on its line and the
  symbol's shape. End lines come from the same brace matcher as Rust's, or from the index's enclosing range
  where the matcher finds no body (Python, and declarations without braces), and callers from those ranges, so
  an indexer that writes no enclosing ranges (scip-dotnet) still gives callers. Every implementation
  relationship (a class implementing an interface, a method implementing an interface method) is an impl edge
  under both names, such as `Shape for Square` or `Shape::Area for Square::Area`. Such rows last until the
  root's next indexing; a language atlas runs the indexer for (Go, below) is then re-indexed by it, any other
  from rust-analyzer and the line scan only.
- **Go** comes from the language table (`src/langs.rs`): one row of data per language says which files and
  folders are its, which files mark a unit, which SCIP indexer to run and where to find it, how the fallback
  scanner reads it and what `--recall-check` greps for, so the next languages are rows, not new code. Each
  `go.mod` is a unit (nested modules too); `vendor`, `testdata` and `_` folders are skipped, as the Go tool
  skips them. atlas runs **scip-go** for each unit when it finds it, on `PATH`, in `GOBIN`, in `GOPATH/bin` or in
  `~/go/bin`, and it answers `--version` with `go` on `PATH` (scip-go runs `go list`). It runs exactly as
  rust-analyzer does (below-normal priority, the timeout, the lease, the debounce, its stderr to
  `code/<root>.scip-go.log`), in the unit's folder, and its paths are rebased to the root. atlas never installs
  scip-go (`go install github.com/scip-code/scip-go/cmd/scip-go@latest` does) and never runs `go` itself;
  scip-go downloads the modules it needs on its first run, as `go build` would. A unit without scip-go, or
  whose run fails, gets the owned scanner, and `symbol` says so (`svc: go definitions only (scan only: scip-go
  not found on PATH, GOBIN, GOPATH/bin, ~/go/bin)`); so does every file scip-go has no document for (a file
  behind a build tag) and every Go file outside a module. The scanner (`src/scanner.rs`, one engine for every
  row) blanks comments and strings, then reads top-level declarations, struct fields, interface methods and
  `type`, `var` and `const` groups with a stack of open blocks, so a local is never taken for a member; it gives
  definitions, kinds, containers and line ranges, no references. A `go.mod`, `go.sum` or `go.work` change
  re-indexes the root like a source change.
- **TypeScript and JavaScript** are two rows of the same table (`.ts`, `.tsx`, `.mts`, `.cts`; `.js`, `.jsx`,
  `.mjs`, `.cjs`) that share units: the shallowest folder with a `tsconfig.json` or a `package.json` is a unit
  and claims its subtree (nested packages, workspace members), and one **scip-typescript** run covers both
  languages. `node_modules`, `bower_components`, `jspm_packages`, `dist`, `build`, `out` and `coverage` are
  skipped. atlas finds scip-typescript beside its `scip-typescript` shim on `PATH` (npm's global folder, or a
  `node_modules/.bin`), else under `npm root -g`, and runs its script with `node` (never the `.cmd` shim, so a
  timeout stops the indexer itself); it needs `node` on `PATH`. A unit whose `package.json` lists dependencies
  and that has no `node_modules` is still run (scip-typescript reads the package's own sources without its
  dependencies), unless one of its `tsconfig.json` files `extends` a package (`"extends": "@scope/tsconfig"`),
  which cannot load without `node_modules`: then the unit is scanned (`scan only: no node_modules: run npm
  install`). A run without `node_modules` that fails says the same after the failure; atlas never runs a
  package manager. A unit's own `tsconfig.json` is used as it is; workspace members (`package.json`
  `workspaces`, `pnpm-workspace.yaml`, `lerna.json`) are passed as projects; a project without a
  `tsconfig.json` gets one inferred (`allowJs`, the folder's files, the skipped folders excluded), written into
  atlas's `code/` folder and never into the project (scip-typescript's own `--infer-tsconfig` writes it into
  the project, which atlas does not do). A unit of JavaScript alone is a `javascript` unit. Files outside the
  `tsconfig.json`'s `include` get the scanner, which reads classes, interfaces, enums, type aliases (their
  object types' members too, written over lines or inline), functions, top-level `const`, `let` and `var`,
  class members, constructors (stored under the class's name, as SCIP stores them) and namespaces; top-level
  definitions take the file's name as their container, as scip-typescript names them. A getter or setter is
  stored under its plain name (scip-typescript writes `<get>name`). A `package.json`, `tsconfig.json`, lock
  file or workspace file change re-indexes the root.
- **C#** (`.cs`) is a row of the same table. A folder with a solution (`*.sln`, else `*.slnx`) is a unit and
  claims its subtree; a project (`*.csproj`) outside every solution is a unit of its own. `obj`, `bin`,
  `packages`, `TestResults` and `artifacts` are skipped. atlas finds **scip-dotnet** on `PATH` or in
  `~/.dotnet/tools` (where `dotnet tool install -g scip-dotnet` puts it; atlas never installs it) and needs
  `dotnet` on `PATH`; it runs `scip-dotnet index --output FILE` with the unit's solution (else its project) in
  the unit's folder, with MSBuild node reuse off so no build server outlives the run. scip-dotnet restores the
  solution's NuGet packages and runs a design-time build itself, as `dotnet build` would: it writes the
  projects' `obj/` folders and fills the NuGet cache, and source generators run, so generated code (gRPC stubs
  under `obj/`) is indexed. A run that fails names the SDK the nearest `global.json` asks for (`scan only:
  scip-dotnet exited with Some(1); global.json asks for .NET SDK 10.0.100`), the usual cause on a machine
  without that SDK. scip-dotnet writes no kinds and no enclosing ranges, so kinds come from the definition's
  line (`class`, `record`, `interface`, `struct`, `enum`, `event`, `const`, a property's accessors or `=>`) and
  item ranges from the brace matcher, else the scanner (the matcher stops at a base list's or a `where`
  clause's comma, the scanner does not). The scanner reads namespaces (block and file-scoped; the last segment
  is the container, as in SCIP), classes, records, structs, interfaces, enums and their members, delegates,
  constructors (stored under the type's name, as SCIP stores `.ctor` and `.cctor`), methods (an explicit
  interface member by its own name), properties, fields, constants and events, with braces on their own lines,
  `where` clauses and `#if` branches (each branch's signature ends with the body they share). Comments,
  strings (verbatim, interpolated and raw) and preprocessor lines are blanked first. Code an `#if` left out of
  scip-dotnet's build has no SCIP rows; the scanner fills its definitions in, marked `(scan)`. A document two
  units' indexes both hold (a project another unit's project references) is read once. A solution, project,
  `.props`, `.targets`, `global.json`, `packages.lock.json` or `NuGet.config` change re-indexes the root.
- **Python** (`.py`) is a row of the same table. A folder with a `pyproject.toml`, `setup.py` or `setup.cfg`, or
  else a `requirements*.txt`, is a unit and claims its subtree; `venv`, `__pycache__`, `__pypackages__`,
  `site-packages`, `build`, `dist` and `*.egg-info` are skipped (dot folders such as `.venv` and `.tox` are
  never walked). atlas finds **scip-python** beside its shim on `PATH`, else under `npm root -g` (where `npm i -g
  @sourcegraph/scip-python` puts it), and runs its script with `node` in the unit's folder (`scip-python index .
  --project-name NAME --project-version 0`, with the unit folder's name as the project name and a fixed version,
  since scip-python reads the git commit and fails outside a checkout). It also asks a Python interpreter for
  the environment's packages; without one the project's own symbols still index. **scip-python does not run on
  Windows** (it crashes at startup), so there atlas does not look for it: a unit is scanned, and its note says
  how to do better (`scan only: scip-python does not run on Windows: write the unit's index.scip with it in WSL
  or Docker`), for example `docker run --rm -v "%cd%:/src" -w /src sourcegraph/scip-python scip-python index .
  --project-name NAME --project-version 0 --output index.scip` in the unit's folder, which atlas then reads as
  a prebuilt index (below). A path given for scip-python in `code.indexers` (a wrapper of your own) is run on
  Windows too. scip-python writes no kinds, so kinds come from the definition's line (`class`, `def`: `fn`, or
  `method` in a class) and its shape (an attribute is a `field`, a module name a `var`); item ranges come from
  the scanner's indentation blocks. The scanner reads blocks by indentation (a logical line ending in `:` opens
  one; brackets and a trailing backslash join lines; comments at any indent are ignored; a docstring is a
  statement): classes, methods, constructors (`__init__`, stored under the class's name, as SCIP stores it),
  class attributes, attributes set through `self` in methods (each where it is first assigned, as SCIP defines
  it), functions nested in functions (named with the function) and in methods (members of the class, as SCIP
  stores them), classes declared in functions, module attributes (assigned, annotated, unpacked, chained, or
  bound by a top-level `with ... as` or `for`), and the bodies of `if`, `try`, `with` and the other compound
  statements as part of the scope around them. Overloads (`@overload` stubs) and property setters, which
  scip-python leaves without a definition, are filled in from the scanner, marked `(scan)`. A
  `pyproject.toml`, `setup.py`, `setup.cfg`, requirements file or lock file (`poetry.lock`, `uv.lock`,
  `Pipfile.lock`, `pdm.lock`) change re-indexes the root.
- **Java** (`.java`) is a row of the same table. A Gradle settings file (`settings.gradle.kts`,
  `settings.gradle`) claims its builds; otherwise a folder with a build file (`build.gradle.kts`,
  `build.gradle`) or a `pom.xml` (a parent `pom.xml` claims its modules) is a unit and claims its subtree;
  `build`, `out` and `bin` are skipped (Maven's `target` is never walked). atlas finds **scip-java** on `PATH`
  or in Coursier's folder (`cs install scip-java`) and runs `scip-java index --output FILE` in the unit's
  folder; scip-java compiles the unit through its own Maven, Gradle or sbt build with a compiler plugin added,
  so the build must work on that machine (and needs `java`). **scip-java does not run on Windows** (it injects
  a POSIX shell `javac` wrapper), so there atlas does not look for it: a unit is scanned, and its note says how
  to do better (`scan only: scip-java does not run on Windows: write the unit's index.scip with it in WSL or
  Docker`), for example `docker run --rm -e JVM_VERSION=17 -v "%cd%:/src" -w /src sourcegraph/scip-java
  scip-java index --output index.scip` in the unit's folder (`JVM_VERSION` picks the JDK the build needs; the
  image defaults to 8), which atlas then reads as a prebuilt index (below). The scanner
  reads packages (the container of a file's top-level types, as a package's last segment), classes,
  interfaces, enums with their constants, records, annotation types, methods, constructors (stored under the
  type's name), fields (several declarators on one line too), with braces on their own lines, `throws` and
  `implements` clauses over several lines, generics and annotations; inside method bodies it reads local
  classes and anonymous class bodies (`new Visitor() {`), whose members scip-java keeps local: those are filled
  in from the scanner, marked `(scan)`. Comments, strings and text blocks are blanked first. A `pom.xml`,
  Gradle build or settings file, `gradle.properties`, version catalog (`libs.versions.toml`) or
  `gradle-wrapper.properties` change re-indexes the root.
- **Kotlin** (`.kt`, `.kts`) is a scanner row: its files belong to the Java units above. scip-java reads
  Kotlin only through semanticdb-kotlinc, which supports Kotlin 1.8 and 1.9 and does not load in the Kotlin 2
  compiler, so a unit of Kotlin alone is never run through scip-java (its note says why) and is indexed by the
  scanner: definitions, kinds, containers and item ranges, no references. In a unit with Java sources the
  Kotlin files scip-java's index holds are read from it, the rest are scanned, and a prebuilt file's Kotlin
  documents are read. The scanner reads packages, classes (with properties declared in the primary
  constructor, and a `private constructor(` on the next line), interfaces (`fun interface` too), enum classes
  with their entries, objects and companion objects, type aliases, functions and properties at the top level
  and in types (with type parameters and receiver types: `fun <T> List<T>.second()`), secondary constructors
  and object expressions' members; string templates (`${...}`, holding strings and running across lines) and
  raw strings are blanked first. Backticked names are not read.
- **Prebuilt indexes**: a unit may be indexed from a `.scip` file written elsewhere, by CI or by you (from WSL
  or Docker on Windows): an `index.scip` in the unit's folder, or the file `code.roots[].scip` names for it (see
  [[Configuration reference]]). Its document paths are relative to the unit's folder. A unit reads it when it
  has no indexer (none found, turned off, or not run on this system), when the file is newer than the
  indexer's last run here and than every source file of the unit (so a fresh index from CI saves a run), or
  when the indexer's run fails (its note then says both: `prebuilt scip-python 0.6.8 (scip-python exited with
  Some(1))`). Otherwise the indexer runs and its output wins. A source file newer than the prebuilt file is
  scanned instead of read from it, so a stale prebuilt index degrades file by file to scanner rows (definitions
  and ranges at the file's current lines, no references) and never gives wrong lines; the note says how many
  (`prebuilt scip-python 0.6.8 (1 file newer, scanned)`), and so does the freshness line of a session's first `symbol` answer
  (these coverage notes are said once per session; state lines such as `stale ...` or `indexing` come every
  time, and a root reads `indexing` until its first walk since atlas started has finished).
  The file is read again whenever it changes: it is part of the root's fingerprint (a configured file outside
  the root too, which the periodic safety-net walk notices).
- `code.indexers` in the configuration turns the language indexers off (`false`) or names each one's binary,
  as `code.rust_analyzer` does for rust-analyzer, and `code.roots[].scip` names prebuilt indexes (see
  [[Configuration reference]]).
- Each root records its **units** in `code_units`: the Rust side (`scip` or `scan only: why`), each unit of the
  language table (`TOOL VERSION` from its indexer, `prebuilt TOOL VERSION` from a prebuilt index, or `scan
  only: why`) and each language of a `--scip` file from another indexer (`prebuilt TOOL VERSION`), with the
  walk fingerprint, time and duration. The per-root note is the Rust unit's alone for a Rust root, as before, else `lang: source` per unit.
  Units in a test-input folder (`fixtures`, `fixture`, `__fixtures__`, `testdata`, `test-data`, `test_data`,
  at any depth) are indexed like the others but left out of the note, which every `symbol` answer's freshness
  lines read; a root whose only units are such folders keeps them in its note.
  A database from v0.1 (`code-5`) is migrated in place on first use: its rows stay, and each Rust root gets
  its unit. A `code-6` database, and so a v0.1 one, keeps its rows and gets `code_packages`; each root is
  re-indexed once on the next walk to record its packages.
- Definitions and impl blocks SCIP lacks (items under an inactive `cfg`, files outside every crate's module
  tree, items written inside a `macro_rules!` body) are added by an owned line scan and marked `(scan)`; with
  no rust-analyzer at all, the scan alone gives definitions and impl edges, and the answer says so. A scanned
  method takes its impl's type as its container, so `symbol {name: "Store::open"}` finds it either way. Impl
  edges carry the block's line range.
- **WGSL** comes from an owned scanner (`src/wgsl.rs`): functions (with their entry-point stage), structs,
  module-scope `var` (address space, `@group`/`@binding`), `const`, `override`, `alias`, the naga_oil
  `#define_import_path` and `#import` lines, and every identifier that names a declared symbol.
- **Protocol Buffers** come from an owned scanner (`src/proto.rs`; no `protoc`): the package, messages (nested
  ones with the outer message as container), enums and their values, services, rpcs (the kind names the
  request and response types, `stream` included) and fields. An rpc's request and response types and a field's
  message type are references, stored under the type's last name, so a message defined in one repository and
  used in another's protos answers in both. Each rpc is linked to the code generated for it and the code that
  implements it, by conventions that are data in each language's row (`Grpc` in `src/langs.rs`: generated-file
  patterns such as `*.pb.go`, `*_pb2_grpc.py`, `*Grpc.cs`, `*Grpc.java`, `*_pb.d.ts`; stub method names such as
  `{Rpc}`, `{rpc}` and `{Rpc}Async`; server base types such as `{S}Server`, `{S}Servicer`, `{S}Base`,
  `{S}ImplBase`). `symbol GetCart` then lists, under impls, each generated stub (`client stub
  CartServiceClient::GetCart`, `server stub CartServiceBase::GetCart`, one per file and side) and each
  implementation (`CartService for CartService::GetCart`: a method of a type that extends, implements or
  embeds a generated server type, found by SCIP impl edges, followed through hand-written base classes, or by
  the type's header or a Go embedded field); under refs, each call through a stub as SCIP resolved it, with
  its caller and kind `rpc` (a stub spelled otherwise, `getCart` or `GetCartAsync`, is stored under the rpc's
  name too). Words no index resolved stay call sites of kind `call (mention)`: in a root's source files, a word
  in code (not in a comment or a string, not a definition's name, not in generated code, not where SCIP has a
  reference) that is a known rpc's name, its lowerCamel or its snake_case, as a client of a `.proto` loaded at
  run time calls it. In each section the `.proto` rows come first and generated code last. The rpc names come
  from every root, so `symbol GetInvoice` (and `lookup` of an unregistered name) answers who defines, calls and
  implements it across a project's repositories (see [[Projects and multi-repo]]). When a root's rpcs change,
  the other roots are re-indexed on the next walk.
- **References across code roots.** A SCIP symbol names its package (manager, name and version:
  `scip-go gomod example.com/lib v1.2.0 ...`, `scip-typescript npm @acme/shared 1.0.0 ...`). Each root records
  the packages its indexes define and the ones its references go into (`code_packages`). A reference whose
  symbol has no definition in its own index is stored, like a reference to the root's own code (with its
  caller), when its package is one another code root defines; any other dependency's references are dropped
  as before. So `symbol Mean` answers the library root's definition with the service root's callers, and a
  call into another root's generated gRPC stub is an rpc call site of the service (`symbol GetCart` lists
  `frontend:rpc.go:58 frontendServer::getCart (rpc)` beside the stubs in the shared root). Packages compare by
  manager and name; the version is never compared (a Go module `replace`d by a sibling folder is version
  `.`, the library's own index says its commit, and a published release says its tag: all one package). How a
  language's names compare is data in its row (`Packages` in `src/langs.rs`: Python's are PEP 503
  normalised, the rest as written). Each root stays indexed on its own: a root reads the other roots'
  recorded packages, never their rows or their indexers; when a root starts or stops defining a package,
  each root whose references go into it is marked stale and re-indexed on the next walk. Packages with no
  name are never linked: scip-dotnet writes `.` for a project's own symbols, so C# references into another
  root are not linked. Linking does not follow `talks_to` (a package reference is the evidence; see
  [[Projects and multi-repo]]).
- Storage: `code_occ` keyed `(name, sect, file, line, col, ord)` so each section of an answer is one `LIMIT`ed
  index range and the answer is one statement; source lines once each in `code_lines`; per-name totals in
  `code_counts`. A re-index replaces one root's rows in one transaction.
- Recall: `--recall-check` also greps every `.rs` and `.wgsl` of the code roots for item definitions (lines in
  comments and string literals excluded) and lists each one the index lacks; each language of the table gets
  its own line from its row's pattern (Go: `func`, `type`, `var` and `const` at a line start; TypeScript and
  JavaScript: `class`, `interface`, `enum`, `type`, `function`, `const`, `let` and `var` at column 0; C#: type
  declarations, and members declared with an access modifier; Python: `def` and `class` at any indent, a
  `def __init__` line held by its class's constructor row; Java: type declarations, and members declared with
  an access modifier; Kotlin: type, object, function and type alias declarations, and properties at column 0
  or declared with a modifier), counting definitions from SCIP rows and from
  the scanner apart.

## Ranges in ordinary reads

The hooks carry the same ranges into the model's own searches and reads, so it reads an item whole instead
of guessing a window:

- each Grep match line ends with its enclosing item and range, such as `[fn Store::report_miss 2089-2113]`;
  impl blocks are items too, so a hit between methods names the impl (`[impl Display for Store 40-58]`);
- a Read window that cuts an item gets one note naming the item and where it ends; the content is untouched,
  so line numbers and Edit strings stay exact;
- shell reads get the same: `grep -n`, `rg -n` and `Select-String` lines get the suffix, and a `sed -n 'A,Bp'`,
  `head`, `tail` or `Get-Content` window gets the Read note.

Files the index does not cover, or that changed since it was built, are left as they are. A file changed by
Edit, Write or MultiEdit is re-indexed as soon as the edit lands (a Rust file by the line scan, a Go file by
the scanner, as is a TypeScript, JavaScript, C#, Python, Java or Kotlin file: definitions and ranges at once, its
references at the root's next full run), so its next read is
annotated again.
`lean.annotate: false` turns the annotations off (see [[Handles and digests]]).

## Language coverage

| Feature | Languages |
|---|---|
| Memory, recall and history, journals and handoffs | any (it is Markdown and records) |
| Search over Markdown notes and sources, `lookup`, `section`, `grep` | any (Markdown and text files) |
| Ranked text search (`search` modes `fts`, `sections`, `hybrid`; `recall {query}`; the recall hook) | words split on spaces and punctuation (SQLite `unicode61`): text with no spaces between words (Chinese, Japanese, Thai) and a word written against an emoji are not found word by word; `search` in `substring` mode (the default, vault notes) and `grep` (vaults and sources) find them |
| Discussions, the work queue (`next_id`, the decision queue) | any |
| Projects and multi-repo, the repository graph | any |
| Hooks: recall, handoffs, the context cap, routing to handles | any |
| `symbol`: definitions, references, callers, impl edges | Rust, WGSL, Protocol Buffers, Go (scip-go when found), TypeScript and JavaScript (scip-typescript when found), C# (scip-dotnet when found), Python (scip-python when found, not on Windows), Java (scip-java when found, not on Windows); any of them from a prebuilt `index.scip`; else definitions only from the scanner, and always for Kotlin; any language with a SCIP indexer, from a `.scip` file given with `--code-index --scip FILE` (tested with scip-go, scip-typescript, scip-dotnet, scip-python and scip-java output) |
| Item ranges on Read and shell reads, Grep annotations | Rust, WGSL, Protocol Buffers, Go, TypeScript, JavaScript, C#, Python, Java, Kotlin; the languages of a `--scip` file until one of their files changes |
| rpcs: generated stubs, exact callers and server implementations (`symbol` of an rpc) | Go, TypeScript, JavaScript, C#, Python, Java, Kotlin (stubs and calls through them from a SCIP index; implementations from SCIP impl edges or the scanner's type headers); word call sites in any language's source files where no index resolved the call |
| References from one code root into another (a package another root defines) | any language whose SCIP indexer names its packages: Go, TypeScript, JavaScript, Python, Java, Rust; calls into another root's generated gRPC stubs as rpc call sites. Not C# (scip-dotnet writes no package name for a project's own symbols) and not scanner-only roots |
| Build and test digests with failures by test | `cargo build`, `cargo test`, `cargo nextest`, `go test` (plain, `-v` and `-json`), jest, vitest, `dotnet test`, pytest, Maven (Surefire) and Gradle test output; other build tools and test runners get the generic digest |
| File outlines in digests | brace-delimited code (Rust, C, C++, Go, JavaScript, TypeScript, Java, C#, WGSL), Python and Markdown |

Each language of the table is measured on a pinned public repository by `cargo xtask langs-bench`
(`docs/design/langs.md`, section 11): the scanner's definitions against the language's SCIP indexer (by path,
line and name), the scanner's item end lines against the indexer's enclosing ranges, and the SCIP rows
against the `--recall-check` grep truth, with the root indexed as atlas indexes it.

gRPC linking is measured on microservices-demo (Go, C#, Python, Java and Node services around one `protos`
folder, one root, a prebuilt SCIP index per service): all 15 rpcs of `protos/demo.proto` are linked to their
stubs (216 links: four Go copies, two Python, Java, and C# for CartService), 14 have exact call sites through
their stubs (32, each with its caller), and 12 have their implementations linked (13: Go, C#, Java and Python
servers). SearchProducts has no caller in the demo; Convert, GetSupportedCurrencies and Charge are served by
Node services that load the `.proto` at run time, so no generated server type names their implementations
and their calls stay `call (mention)` rows (7 in all, down from 1,017).

References across roots are measured on three pairs of roots, each root indexed from its own prebuilt
index (`docs/design/langs.md`, section 14, PR 8):

| Fixture | Roots | Expected references | Stored across roots | False links |
|---|---|---|---|---|
| A Go library and a service using it through `replace ../lib` (scip-go 0.2.9) | 2 | 17 (hand-counted: functions, a method, types, fields, an interface) | 17, each with its caller | 0 |
| microservices-demo's `frontend` and `checkoutservice` with their generated gRPC code moved into a third root, a shared Go module (scip-go 0.2.9) | 3 | 372: the references into `genproto` the same services have when the code is their own (198 and 174) | 372; the 19 calls through stubs are rpc call sites, the same 19 as in the one-root layout | 0 |
| A TypeScript package used by a sibling through a `file:` dependency (scip-typescript 0.5.2) | 2 | 22 (hand-counted; two constructor parameter properties are parameters, which no root stores) | 22 | 0 |

Indexing the library after the service marks the service stale, and its next run stores the references;
indexing the library again with the same packages marks nothing.

| Language | Corpus | Indexer | Scanner recall (precision) | Item ends equal | SCIP rows of the grep truth |
|---|---|---|---|---|---|
| Go | gin-gonic/gin at 0f09c3a (98 files, 24k lines) | scip-go 0.2.9 | 99.7% (100.0%) of 1,987 definitions; types and functions 100% | 100.0% of 1,512 types and functions | 100% (1,519 of 1,519) in the files scip-go indexes; the 34 in files behind build tags come from the scanner, none missing |
| TypeScript | sindresorhus/got at 6b5aafc (92 files, 67k lines) | scip-typescript 0.5.2 | 98.8% (93.5%) of 1,405 definitions; types and callables 100%, terms 98.3% | 99.8% of 426 types and callables | 100% (659 of 659) in the files scip-typescript loads; none from the scanner, none missing |
| JavaScript | GoogleCloudPlatform/microservices-demo `src/currencyservice` at c7cb6d5 (2 files, 266 lines; no `tsconfig.json`, so atlas infers one) | scip-typescript 0.5.2 | 100% (100%) of 27 definitions | 100% of 8 functions | 100% (27 of 27); none from the scanner, none missing |
| C# | serilog/serilog at 43105b3 (217 files, 25k lines) | scip-dotnet 0.2.16 (its index says `0.1.0-SNAPSHOT`) | 99.8% (99.0%) of 2,237 definitions; types and callables 100%, terms 99.2% | scip-dotnet writes no enclosing ranges: 98.3% of 1,749 types and callables equal to the brace matcher's, and in each of the 30 differences the matcher stops at a base list's or a `where` clause's comma | 100% of 1,731 in the files scip-dotnet loads: 1,711 SCIP rows and 20 filled by the scanner (an inactive `#if` branch), none missing |
| JavaScript | expressjs/express at 43fca52 (142 files, 22k lines; no `tsconfig.json` and no `node_modules`: atlas infers the configuration and runs scip-typescript anyway) | scip-typescript 0.5.2 | 100% (100%) of 527 definitions | 100% of 55 functions and methods | 100% (461 of 461); none from the scanner, none missing |
| Python | pallets/flask at 49943b5 (83 files, 18k lines) | scip-python 0.6.8 (its Docker image; it does not run on Windows) | 99.7% (97.6%) of 1,928 definitions; types and callables 100%, terms 98.3% | 100.0% of 1,584 types and callables | 100% of 1,622 in the files scip-python loads: 1,584 SCIP rows and 38 filled by the scanner (overloads, property setters, a class redefined in a function), none missing |
| Java | google/gson at 5773104 (264 files, 57k lines) | scip-java HEAD-20260402-1750 (its Docker image, Maven, the `gson` module; it does not run on Windows) | 99.0% (93.7%) of 4,436 definitions; types 95.8%, callables 99.9%, terms 97.1% | 99.9% of 3,466 types and callables | 100% of 4,017 in the files scip-java's build compiled: 3,749 SCIP rows and 268 filled by the scanner (members of anonymous and local classes, which scip-java keeps local), none missing; the 563 in the modules it did not build come from the scanner, none missing |
| Kotlin | square/kotlinpoet at 4ce5d54 (131 files, 40k lines) | none (semanticdb-kotlinc does not load in Kotlin 2): the scanner against the grep truth | 99.5% of 2,879 grep truth definitions (100% of its rows on the truth's lines are right) | 97.8% of 1,805 types and functions with a brace body equal to the brace matcher's (the brace matcher counts braces inside raw strings, which the scanner blanks) | scanner only: 99.5%, 14 missing |

In Go the scanner's few misses are SCIP's own oddities: the parameters and locals of a function literal assigned
to a package variable, which scip-go stores as package-level symbols. In TypeScript the misses are members
of object types the scanner cannot see from a line (a destructured parameter's type over several lines,
quoted member names), and its extra rows are members SCIP leaves out (those of some object types used in an
intersection). express is the larger JavaScript measure: CommonJS modules with methods assigned to a
constructor's prototype (`View.prototype.render = function render() {`) and declarators continued
comma-first (`var a = require('a')` then `  , b = require('b')`), both read by the scanner. In C# the
scanner's misses are the names of a `const` declaration continued over several lines, and its extra rows are
definitions in `#if` branches scip-dotnet's build left out (which fill the index). In Python the scanner's
misses are attributes of a class declared again in the same function (SCIP defines the first class's), and
its extra rows are the definitions scip-python leaves out: `@overload` stubs, property setters and repeated
definitions, which fill the index. With the flask index read as a prebuilt `index.scip` and
`src/flask/app.py` edited after it (three lines added at the top), that file's 41 definitions come from the
scanner at their new lines and the recall check still finds every definition (1,543 from SCIP rows, 79 from
the scanner, none missing). In Java most of the scanner's misses are record components (`record Point(int a,
int b)`, which SCIP defines as fields; deferred), and its extra rows are the members of anonymous classes
(`new Visitor() { ... }`), which scip-java keeps local and the scanner defines, as a reader looks for them;
the five end differences are local records, whose SCIP range is only the header. Kotlin has no working SCIP
indexer, so it is measured against its grep truth alone: the misses are local functions (a `fun` inside a
function's body), the members of an object expression opened inside a call (`listOf(object : X() {`) and a
class nested in classes with backticked names, and the scanner's 362 definitions outside the truth are what the truth does not count
(properties without a modifier, enum entries, constructors).
