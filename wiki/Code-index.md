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
  (`src/scip.rs`) into struct-of-arrays columns with symbols interned as slices of the file. rust-analyzer
  writes no `relationships`, so impl edges come from its impl symbols (`impl#[Type][Trait]`), and callers from
  the innermost enclosing definition range of each reference. References into dependencies are not stored.
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
  used in another's protos answers in both. Call sites count too: in a root's other source files (Go, Rust,
  TypeScript, Python, Java and others), a word that is a known rpc's name, its lowerCamel or its snake_case is a
  reference of kind `call (mention)`. The rpc names come from every root, so `symbol GetInvoice` (and `lookup`
  of an unregistered name) answers who defines it and who calls it across a project's repositories (see
  [[Projects and multi-repo]]). When a root's rpcs change, the other roots are re-indexed on the next walk.
- Storage: `code_occ` keyed `(name, sect, file, line, col, ord)` so each section of an answer is one `LIMIT`ed
  index range and the answer is one statement; source lines once each in `code_lines`; per-name totals in
  `code_counts`. A re-index replaces one root's rows in one transaction.
- Recall: `--recall-check` also greps every `.rs` and `.wgsl` of the code roots for item definitions (lines in
  comments and string literals excluded) and lists each one the index lacks.

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
Edit, Write or MultiEdit is re-indexed as soon as the edit lands, so its next read is annotated again.
`lean.annotate: false` turns the annotations off (see [[Handles and digests]]).

## Language coverage

| Feature | Languages |
|---|---|
| Memory, recall and history, journals and handoffs | any (it is Markdown and records) |
| Search over Markdown notes and sources, `lookup`, `section`, `grep` | any (Markdown and text files) |
| Discussions, the work queue (`next_id`, the decision queue) | any |
| Projects and multi-repo, the repository graph | any |
| Hooks: recall, handoffs, the context cap, routing to handles | any |
| `symbol`: definitions, references, callers, impl edges | Rust, WGSL, Protocol Buffers |
| Item ranges on Read and shell reads, Grep annotations | Rust, WGSL, Protocol Buffers |
| rpc call sites (`symbol` of an rpc names its callers) | any language's source files in a code root |
| Build and test digests with failures by test | `cargo build`, `cargo test` and `cargo nextest` output; other build tools and test runners get the generic digest |
| File outlines in digests | brace-delimited code (Rust, C, C++, Go, JavaScript, TypeScript, Java, C#, WGSL), Python and Markdown |

More languages for the code index are planned.
