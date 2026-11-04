//! The language table: one row of data per language the code index reads
//! beyond Rust, WGSL and Protocol Buffers (which have their own code).
//!
//! A row says everything that differs between languages; the code that
//! uses it is shared:
//!
//! * which files are the language's ([`Lang::exts`]), which folders hold
//!   none of them ([`Lang::skip_dirs`]), and which files mark a unit (a Go
//!   module) or make a root stale without a source change
//!   ([`Lang::markers`], [`Lang::stale`]): the walk in [`crate::code::walk`]
//!   and the unit walk in [`crate::code::units`];
//! * the SCIP indexer atlas runs when it finds one ([`Driver`]): where to
//!   look for the binary, what else must be on `PATH`, its arguments
//!   ([`crate::code_index`] runs it as it runs rust-analyzer);
//! * the definition scanner used when there is no indexer, and for a file
//!   edited since the last full run ([`ScanRow`], run by
//!   [`crate::scanner`]);
//! * the grep truth `--recall-check` measures the index against
//!   ([`Lang::truth`]).
//!
//! The test runners a digest recognises are the same kind of table, in
//! [`crate::digest`] (`RUNNERS`).
//!
//! Adding a language is adding a row to [`LANGS`]; a language whose blocks
//! are indentation (Python) says so in its row ([`ScanRow::indent`]).
//! Marker and stale-file names may be patterns (`*.csproj`,
//! `requirements*.txt`, [`name_matches`]).

use std::path::{Path, PathBuf};

/// One language's row.
#[derive(Debug)]
pub struct Lang {
    /// The word `code_files.lang` and `code_units.lang` store (the same
    /// word [`crate::code::lang_of`] gives a SCIP document).
    pub name: &'static str,
    /// File extensions, without the dot.
    pub exts: &'static [&'static str],
    /// Folder names whose files of this language are not indexed (`vendor`;
    /// a `*` matches any run of characters: `_*`, the folders the Go tool
    /// ignores, or `*.egg-info`). Other languages' files in them still are.
    pub skip_dirs: &'static [&'static str],
    /// File names that make their folder a unit (a Go module's `go.mod`), in
    /// order of preference (a C# solution before a project; Python's
    /// `requirements*.txt` last). A marker may be a source file of the
    /// language (Python's `setup.py`).
    pub markers: &'static [&'static str],
    /// Whether a marker below another unit's folder makes a unit of its
    /// own (every Go module is one), or the shallowest unit claims its
    /// whole subtree (a TypeScript package with nested `package.json`
    /// files, a workspace and its members).
    pub nested_units: bool,
    /// Another row whose units hold this language's files and whose
    /// indexer reads them (JavaScript files are TypeScript's: one
    /// scip-typescript run covers both). Such a row has no markers and no
    /// driver of its own.
    pub units_of: Option<&'static str>,
    /// File names whose change makes the root stale although no source
    /// file changed (manifests, lock files, workspace files); the markers
    /// are among them.
    pub stale: &'static [&'static str],
    /// The definition scanner's row.
    pub scanner: ScanRow,
    /// The `--recall-check` truth: a regex over one source line with groups
    /// `kw` (the keyword) and `name`. A line inside a block comment or a
    /// multi-line string is not counted.
    pub truth: &'static str,
    /// The SCIP indexer atlas runs for a unit, when one is found.
    pub driver: Option<Driver>,
    /// The scanner also adds its definitions on lines of a file the index
    /// covers where the index has none: code the indexer's build left out
    /// (C#'s inactive `#if` branches, as Rust's line scan does for an
    /// inactive `cfg`). Off where whole files are left out instead (Go's
    /// build tags) and where the scanner's extra rows would be noise.
    pub fills: bool,
    /// For a row whose files are another row's units' ([`Lang::units_of`]):
    /// why that row's indexer is not run for a unit holding only this
    /// language's files (scip-java cannot read Kotlin 2). The unit is
    /// scanned, and a prebuilt file of it is still read. Its files in a
    /// unit with the owner's own files are indexed when the indexer covers
    /// them, else scanned.
    pub unindexed: Option<&'static str>,
    /// How its SCIP indexer names packages, for references into another
    /// code root ([`package_key`]); `None`: another row's (JavaScript's
    /// are TypeScript's npm packages, Kotlin's Java's Maven ones).
    pub packages: Option<Packages>,
    /// How gRPC code generators write this language's stubs for a `.proto`
    /// service (see [`Grpc`]); `None`: no stubs are recognised.
    pub grpc: Option<Grpc>,
}

/// A language's gRPC conventions: which files a generator writes and how
/// it names an rpc's stub methods and the server type an implementation
/// extends. [`crate::proto::link_rows`] links each rpc of a `.proto`
/// service to them. In a name, `{S}` is the service's name, `{Rpc}` the
/// rpc's as written (`GetCart`), `{rpc}` its lowerCamel spelling
/// (`getCart`) and `{rpc_}` its snake_case one (`get_cart`).
#[derive(Debug)]
pub struct Grpc {
    /// File name patterns of generated code (`*.pb.go`; a `*` stands for
    /// any run of characters, as in [`name_matches`]). A generated file's
    /// definitions are stubs, never implementations, and its words are not
    /// rpc call sites.
    pub generated: &'static [&'static str],
    /// The names a stub method of an rpc has (`{Rpc}`, C#'s `{Rpc}Async`).
    pub stubs: &'static [&'static str],
    /// The name an implementation's method of an rpc has.
    pub servers: &'static [&'static str],
    /// The names of the generated server types an implementation extends,
    /// implements or embeds (`{S}Server`, `Unimplemented{S}Server`); a stub
    /// in a type so named is the server side's, any other a client's.
    pub bases: &'static [&'static str],
}

/// How a language's SCIP indexer names packages: the managers its symbols
/// carry (`gomod`, `npm`, `maven`) and how a package name compares. A
/// reference in one code root to a package another root defines is stored
/// with that root's definitions ([`crate::code::scip_rows`]); the version
/// is never compared (a sibling module's replaced copy, a workspace
/// package and a published release are the same package).
#[derive(Debug)]
pub struct Packages {
    /// The manager words (the symbol's second header field).
    pub managers: &'static [&'static str],
    /// How names compare.
    pub fold: Fold,
}

/// How package names compare ([`Packages::fold`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fold {
    /// As written (Go module paths, npm names, Maven coordinates).
    Exact,
    /// Python's normalised project names (PEP 503): ASCII case ignored, and
    /// runs of `-`, `_` and `.` the same (`Foo_Bar` is `foo-bar`).
    Pep503,
}

/// The key a SCIP symbol's package compares by: `manager name`, the name
/// folded as the language row with that manager says ([`Packages`]; as
/// written for a manager no row names, rust-analyzer's `cargo` among them).
///
/// # Arguments
///
/// * `manager` - the symbol's manager field.
/// * `name` - its package name field.
///
/// # Returns
///
/// `None` for a package with no name (`.`, as scip-dotnet writes for a
/// project's own symbols and scip-java for symbols of no build module):
/// such a package is never linked.
pub fn package_key(manager: &str, name: &str) -> Option<String> {
    if name.is_empty() || name == "." || manager.is_empty() || manager == "." {
        return None;
    }
    let fold = LANGS.iter().filter_map(|l| l.packages.as_ref()).find(|p| p.managers.contains(&manager)).map_or(Fold::Exact, |p| p.fold);
    let name = match fold {
        Fold::Exact => name.to_string(),
        Fold::Pep503 => {
            let mut out = String::with_capacity(name.len());
            for c in name.chars() {
                if matches!(c, '-' | '_' | '.') {
                    if !out.ends_with('-') {
                        out.push('-');
                    }
                } else {
                    out.push(c.to_ascii_lowercase());
                }
            }
            out
        }
    };
    Some(format!("{manager} {name}"))
}

/// A string or comment form of a language, blanked before rules match.
#[derive(Debug)]
pub struct Quote {
    /// What opens it.
    pub open: &'static str,
    /// What closes it (empty: the end of the line).
    pub close: &'static str,
    /// A backslash escapes the next character inside it.
    pub escape: bool,
    /// It may run across lines (a raw string, a block comment); otherwise
    /// it ends at the end of its line however it is written.
    pub multiline: bool,
    /// What opens a template expression inside it (`${` in a Kotlin
    /// string): code up to its matching `}`, blanked with the string, which
    /// does not end inside it (not at a `"` of a string the expression holds,
    /// nor at the end of a line). Empty: none.
    pub template: &'static str,
}

/// The scanner's row: how comments and strings are written, and the rules
/// that find definitions.
#[derive(Debug)]
pub struct ScanRow {
    /// Comments and string literals: their text (delimiters included) is
    /// blanked to spaces before any rule matches, so neither a brace nor a
    /// keyword inside one counts.
    pub quotes: &'static [Quote],
    /// The rules, tried in order on each line; the first that matches in
    /// the line's scope wins.
    pub rules: &'static [Rule],
    /// Top-level definitions take the file's name without its extension as
    /// their container (a TypeScript or JavaScript module, as
    /// scip-typescript names it); a [`Does::Container`] rule replaces it.
    pub file_container: bool,
    /// A line that continues the definition before it (a type alias written
    /// over several lines: `^s*[|&]`); a deeper-indented line continues it
    /// too, and a `;` ends it. Empty: a definition ends with its line or
    /// block (Go).
    pub continued_by: &'static str,
    /// With [`ScanRow::allman`]: a line that continues a declaration's
    /// header at any indent (Kotlin's `private constructor(` under `class
    /// X`); its first bracket, of any kind, is the declaration's (its
    /// parameter list is the class's scope). In such a row a parameter list
    /// closed on a line hands over to a body on a later line however the
    /// line ends (`) : Base,` then `Other {`). Empty: none.
    pub header_continued: &'static str,
    /// Bodies may open on a later line (C#'s braces on their own line): a
    /// definition whose line leaves no block open and does not end in `;`
    /// owns the block a following line starting with `{` opens, past blank
    /// lines and lines indented deeper than it (a base list, `where`
    /// constraints, a constructor initializer), or that a deeper line ending
    /// in `{` opens (a Java `throws` or `implements` clause). Those lines
    /// are no declarations of their own: rules read them as the inside of an
    /// opaque block ([`OPAQUE`]), and a deeper one ending in `;` ends the
    /// declaration.
    pub allman: bool,
    /// Words that are never a definition's name (C#'s keywords): a rule
    /// that would define one defines nothing (`const string` followed by
    /// its names on the next lines).
    pub not_names: &'static [&'static str],
    /// Blocks are indentation, not brackets (Python): a logical line ending
    /// in `:` opens a block that holds the lines indented deeper than it;
    /// brackets only join lines into one logical line. See
    /// [`crate::scanner`].
    pub indent: bool,
    /// Kinds defined once per container in a file: a later definition of
    /// the same name and container is dropped (a Python attribute assigned
    /// again in another method, which SCIP defines where it is first
    /// assigned).
    pub once: &'static [&'static str],
}

/// What a rule does when it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Does {
    /// Defines the names in group `name` (a comma-separated list is several
    /// names) with this kind word.
    Def(&'static str),
    /// Opens a block of kind [`Rule::opens`] and defines nothing (a Go
    /// `const (` group).
    Open,
    /// Names the file's default container from group `name` (a Go
    /// `package` clause): top-level definitions are its members.
    Container,
    /// Defines the enclosing block's name with this kind word (a
    /// constructor, which SCIP stores under its class's name).
    DefOwner(&'static str),
    /// Defines the `name` of every match on the line with this kind word,
    /// when another rule defined something on the line first (the members
    /// of an object type written inline in a declaration,
    /// `type T = Merge<A, {x: X; y: Y}>`).
    Each(&'static str),
    /// Defines as [`Does::Def`] does, but the block it opens keeps the
    /// enclosing container: a Python method, whose `self.x` attributes and
    /// nested functions SCIP stores as its class's members.
    Member(&'static str),
}

/// The [`Rule::opens`] kind of a block that is no scope of its own: rules
/// apply inside it as in the block around it, and its definitions take that
/// block's container (a Python `if`, `try` or `with` body, whose `def` at
/// the top level is a top-level function). Indentation rows only.
pub const SAME_SCOPE: &str = "^";

/// The [`Rule::scope`] of an opaque block (a function body, a value's
/// braces): a rule of this scope finds what such a block may declare that
/// the index keeps (a Java local class, an anonymous class's body).
/// Bracket rows only.
pub const OPAQUE: &str = "~";

/// One scanner rule.
#[derive(Debug)]
pub struct Rule {
    /// Where it applies: `""` at the top level (no block open), else inside
    /// a block of this kind (the innermost open block's); several scopes
    /// are `|`-separated (`"|namespace"`: the top level and a namespace
    /// body).
    pub scope: &'static str,
    /// The pattern, over the blanked line ([`crate::regex`] syntax): group
    /// `name` names the definition, optional group `container` its
    /// container (a Go method's receiver type).
    pub re: &'static str,
    /// What a match does.
    pub does: Does,
    /// The kind of the block the line leaves open, if it leaves one: rules
    /// of that scope apply inside it, and its definitions take the
    /// definition named here as their container. `""`: an opaque block (a
    /// function body), where no rule applies.
    pub opens: &'static str,
}

/// Where a driver looks for its binary, in order.
#[derive(Debug, Clone, Copy)]
pub enum Find {
    /// Every folder of `PATH`.
    Path,
    /// The folder an environment variable names (`GOBIN`).
    Env(&'static str),
    /// A sub-folder of each folder an environment variable lists
    /// (`GOPATH` / `bin`).
    EnvJoin(&'static str, &'static str),
    /// A folder below the user's home (`go/bin`, the default `GOPATH`).
    Home(&'static str),
    /// npm's global package folder, as `npm root -g` prints it (asked only
    /// when every place before it in the list came up empty; npm is asked,
    /// never told to install anything).
    NpmRoot,
}

/// A SCIP indexer atlas runs itself. atlas never installs one: it is used
/// when found, and the root is scanned meanwhile.
#[derive(Debug)]
pub struct Driver {
    /// The tool's name, and its binary's (`.exe` added on Windows).
    pub tool: &'static str,
    /// The npm package of a tool written for Node (`@sourcegraph/scip-typescript`):
    /// atlas finds the package's folder and runs its `bin` script with
    /// `node` (never the `.cmd` shim npm writes on Windows, which a timeout
    /// could kill while leaving its `node` child running).
    pub node_package: Option<&'static str>,
    /// Where to look for it.
    pub find: &'static [Find],
    /// The argument it must answer with success to count as found.
    pub version_arg: &'static str,
    /// Programs that must be on `PATH` for it to run (scip-go runs `go`);
    /// when one is missing the unit is scanned and the note says why.
    pub needs: &'static [&'static str],
    /// Its arguments, run in the unit's folder; `{out}` is the output file,
    /// `{marker}` the unit's marker file (a C# solution or project; left out
    /// when the unit has none), `{name}` the unit folder's name (a Python
    /// project's name).
    pub args: &'static [&'static str],
    /// What a unit should hold before the tool runs (installed
    /// dependencies); see [`Prereq`] for when its absence stops the run.
    pub prereq: Option<Prereq>,
    /// How the unit's projects are passed (TypeScript's `tsconfig.json`
    /// folders and workspace members); `None`: the tool reads the unit's
    /// folder by itself.
    pub projects: Option<Projects>,
    /// Environment variables set for its runs (`MSBUILDDISABLENODEREUSE`:
    /// no MSBuild node is left running after a .NET indexer exits).
    pub env: &'static [(&'static str, &'static str)],
    /// When the tool does not run on Windows (scip-python): what a scanned
    /// unit's note says instead of looking for it there. A path configured
    /// in `code.indexers` (a wrapper that runs it in WSL or Docker) is still
    /// run.
    pub not_on_windows: Option<&'static str>,
    /// A value a failed run's note quotes, when the unit has the file (a
    /// `global.json` pinning a .NET SDK the machine may not have).
    pub fail_hint: Option<Hint>,
}

/// A value from a JSON file in a unit's folder or a folder above it (up to
/// the root), quoted in the note of a failed run.
#[derive(Debug)]
pub struct Hint {
    /// The file name.
    pub file: &'static str,
    /// The key path of the value (`sdk`, `version`).
    pub key: &'static [&'static str],
    /// The note, with `{}` for the value.
    pub note: &'static str,
}

impl Hint {
    /// The note for the nearest file from `unit` up to `root` that holds
    /// the value, if any.
    pub fn for_unit(&self, root: &Path, unit: &Path) -> Option<String> {
        let mut dir = Some(unit);
        while let Some(d) = dir {
            if let Ok(text) = std::fs::read_to_string(d.join(self.file)) {
                let v = crate::json::parse(&text).ok()?;
                let mut at = Some(&v);
                for k in self.key {
                    at = at.and_then(|x| x.get(*k));
                }
                return at.and_then(|x| x.as_str()).map(|val| self.note.replace("{}", val));
            }
            if d == root {
                break;
            }
            dir = d.parent().filter(|p| p.starts_with(root));
        }
        None
    }
}

/// A folder a unit should hold before its indexer runs, when its manifest
/// declares anything to put there (`node_modules`, when `package.json`
/// lists dependencies).
///
/// Without it the indexer still runs (scip-typescript reads a package's own
/// sources without its dependencies), unless a configuration file of the
/// unit needs a package from the folder ([`Prereq::required_by`]); the note
/// names [`Prereq::fix`] when the unit is not run, or when the run fails.
#[derive(Debug)]
pub struct Prereq {
    /// The folder, in the unit's folder.
    pub dir: &'static str,
    /// The JSON manifest that says whether it is needed.
    pub manifest: &'static str,
    /// Its keys that list what goes into the folder; the folder is needed
    /// when one of them is a non-empty object.
    pub keys: &'static [&'static str],
    /// The command the note tells the user to run.
    pub fix: &'static str,
    /// A configuration file name and its key that, when a file of that
    /// name in the unit names a package there (not a relative or absolute
    /// path), means the tool cannot run without the folder: a
    /// `tsconfig.json` whose `extends` is `@scope/tsconfig` fails to load
    /// without `node_modules`.
    pub required_by: Option<(&'static str, &'static str)>,
}

/// How a driver is told which projects of a unit to read.
#[derive(Debug)]
pub struct Projects {
    /// The file that makes a folder a project the tool reads as it is
    /// (`tsconfig.json`).
    pub config: &'static str,
    /// The configuration atlas writes, into its own folder and never the
    /// user's, for a project folder that has no `config`; `{dir}` is the
    /// folder, `/`-separated and escaped for a JSON string. Its path is passed in place of the
    /// folder's. (scip-typescript's own `--infer-tsconfig` writes a
    /// `tsconfig.json` into the project, which atlas does not do.)
    pub inferred: &'static str,
    /// Where a workspace lists its members (folder globs).
    pub workspaces: &'static [Workspace],
}

/// A file listing a workspace's member folders as globs.
#[derive(Debug, Clone, Copy)]
pub enum Workspace {
    /// A JSON file and the key path of the list (`package.json`
    /// `workspaces`, or its `workspaces.packages` form; `lerna.json`
    /// `packages`).
    Json(&'static str, &'static [&'static str]),
    /// A YAML file whose top-level key holds a plain list of strings
    /// (`pnpm-workspace.yaml` `packages`).
    YamlList(&'static str, &'static str),
}

/// How a found tool is started: a program and the arguments that go
/// before the driver's own (a Node tool's script).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The program.
    pub program: PathBuf,
    /// Arguments before the driver's.
    pub pre: Vec<std::ffi::OsString>,
}

impl Launch {
    /// A program run as it is.
    pub fn program(p: PathBuf) -> Launch {
        Launch { program: p, pre: Vec::new() }
    }

    /// A configured path: a Node package's folder or a `.js` script is run
    /// with `node` from `PATH`; anything else is run as it is.
    pub fn configured(p: &Path) -> Launch {
        if p.is_dir() {
            if let Some(script) = package_bin(p) {
                return node_launch(script);
            }
        }
        if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("js") || e.eq_ignore_ascii_case("mjs") || e.eq_ignore_ascii_case("cjs")) {
            return node_launch(p.to_path_buf());
        }
        Launch::program(p.to_path_buf())
    }

    /// The whole argument list: [`Launch::pre`], then `args`.
    pub fn args(&self, args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
        let mut v = self.pre.clone();
        v.extend(args);
        v
    }
}

fn node_launch(script: PathBuf) -> Launch {
    Launch { program: on_path("node").unwrap_or_else(|| PathBuf::from(exe("node"))), pre: vec![script.into_os_string()] }
}

/// The script a Node package's `package.json` names in `bin` (its only
/// entry, or the one named after the package's last segment).
fn package_bin(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v = crate::json::parse(&text).ok()?;
    let bin = v.get("bin")?;
    let rel = match bin.as_str() {
        Some(s) => s.to_string(),
        None => {
            let o = bin.as_object()?;
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let last = name.rsplit('/').next().unwrap_or(name);
            o.get(last).or_else(|| o.iter().next().map(|(_, v)| v)).and_then(|s| s.as_str())?.to_string()
        }
    };
    let p = dir.join(rel);
    p.is_file().then_some(p)
}

const GO_QUOTES: &[Quote] = &[
    Quote { open: "//", close: "", escape: false, multiline: false, template: "" },
    Quote { open: "/*", close: "*/", escape: false, multiline: true, template: "" },
    Quote { open: "\"", close: "\"", escape: true, multiline: false, template: "" },
    Quote { open: "'", close: "'", escape: true, multiline: false, template: "" },
    // Raw strings run across lines and know no escapes.
    Quote { open: "`", close: "`", escape: false, multiline: true, template: "" },
];

/// Go identifiers, one or a comma-separated list.
macro_rules! go_names {
    () => {
        r"[A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*"
    };
}

const GO_RULES: &[Rule] = &[
    // The top level (gofmt puts every top-level declaration at column 0).
    Rule { scope: "", re: r"^package\s+(?P<name>[A-Za-z_]\w*)", does: Does::Container, opens: "" },
    Rule {
        scope: "",
        re: r"^func\s*\(\s*(?:[A-Za-z_]\w*\s+)?\*?\s*(?P<container>[A-Za-z_]\w*)(?:\[[^\]]*\])?\s*\)\s*(?P<name>[A-Za-z_]\w*)",
        does: Does::Def("method"),
        opens: "",
    },
    Rule { scope: "", re: r"^func\s+(?P<name>[A-Za-z_]\w*)", does: Does::Def("fn"), opens: "" },
    Rule { scope: "", re: r"^type\s+(?P<name>[A-Za-z_]\w*)(?:\[[^\]]*\])?\s+struct\b", does: Does::Def("struct"), opens: "struct" },
    Rule {
        scope: "",
        re: r"^type\s+(?P<name>[A-Za-z_]\w*)(?:\[[^\]]*\])?\s+interface\b",
        does: Does::Def("interface"),
        opens: "interface",
    },
    Rule { scope: "", re: r"^type\s*\(", does: Does::Open, opens: "type" },
    Rule { scope: "", re: r"^type\s+(?P<name>[A-Za-z_]\w*)", does: Does::Def("type"), opens: "" },
    Rule { scope: "", re: r"^var\s*\(", does: Does::Open, opens: "var" },
    Rule { scope: "", re: r"^const\s*\(", does: Does::Open, opens: "const" },
    Rule { scope: "", re: concat!(r"^var\s+(?P<name>", go_names!(), r")"), does: Does::Def("var"), opens: "" },
    Rule { scope: "", re: concat!(r"^const\s+(?P<name>", go_names!(), r")"), does: Does::Def("const"), opens: "" },
    // A `type (` group: the same forms without the keyword.
    Rule { scope: "type", re: r"^\s+(?P<name>[A-Za-z_]\w*)(?:\[[^\]]*\])?\s+struct\b", does: Does::Def("struct"), opens: "struct" },
    Rule {
        scope: "type",
        re: r"^\s+(?P<name>[A-Za-z_]\w*)(?:\[[^\]]*\])?\s+interface\b",
        does: Does::Def("interface"),
        opens: "interface",
    },
    Rule { scope: "type", re: r"^\s+(?P<name>[A-Za-z_]\w*)", does: Does::Def("type"), opens: "" },
    // `var (` and `const (` groups: a name list, then a type, a value or
    // nothing (an `iota` continuation); not an operator (a continued
    // expression).
    Rule {
        scope: "var",
        re: concat!(r"^\s+(?P<name>", go_names!(), r")(?:\s*$|\s*=[^=]|\s*=$|\s+[A-Za-z_*\[(<])"),
        does: Does::Def("var"),
        opens: "",
    },
    Rule {
        scope: "const",
        re: concat!(r"^\s+(?P<name>", go_names!(), r")(?:\s*$|\s*=[^=]|\s*=$|\s+[A-Za-z_*\[(<])"),
        does: Does::Def("const"),
        opens: "",
    },
    // A struct body: fields (a nested struct type opens its own body), and
    // embedded types, which define a field named after the type.
    Rule { scope: "struct", re: r"^\s+(?P<name>[A-Za-z_]\w*)\s+(?:\*|\[\w*\])*struct\s*\{", does: Does::Def("field"), opens: "struct" },
    Rule { scope: "struct", re: concat!(r"^\s+(?P<name>", go_names!(), r")\s+[A-Za-z_*\[(<]"), does: Does::Def("field"), opens: "" },
    Rule {
        scope: "struct",
        re: r"^\s+\*?(?:[A-Za-z_]\w*\.)?(?P<name>[A-Za-z_]\w*)(?:\[[^\]]*\])?\s*$",
        does: Does::Def("field"),
        opens: "",
    },
    // An interface body: methods (embedded interfaces and type unions
    // define nothing).
    Rule { scope: "interface", re: r"^\s+(?P<name>[A-Za-z_]\w*)\s*\(", does: Does::Def("method"), opens: "" },
];

const TS_QUOTES: &[Quote] = &[
    Quote { open: "//", close: "", escape: false, multiline: false, template: "" },
    Quote { open: "/*", close: "*/", escape: false, multiline: true, template: "" },
    Quote { open: "\"", close: "\"", escape: true, multiline: false, template: "" },
    Quote { open: "'", close: "'", escape: true, multiline: false, template: "" },
    // Template literals run across lines; their `${...}` parts are blanked
    // with them (their braces balance).
    Quote { open: "`", close: "`", escape: true, multiline: true, template: "" },
];

/// A TypeScript or JavaScript identifier.
macro_rules! ts_id {
    () => {
        r"[A-Za-z_$][\w$]*"
    };
}

/// What may come before a top-level declaration's keyword.
macro_rules! ts_lead {
    () => {
        r"^\s*(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?"
    };
}

/// A class member's modifiers.
macro_rules! ts_mods {
    () => {
        r"^\s*(?:(?:public|private|protected|static|readonly|abstract|override|declare|accessor|async)\s+)*"
    };
}

const TS_RULES: &[Rule] = &[
    // The top level and a namespace body (whose members SCIP stores as the
    // top level's, under the namespace).
    Rule { scope: "|namespace", re: concat!(ts_lead!(), r"class\s+(?P<name>", ts_id!(), ")"), does: Does::Def("class"), opens: "class" },
    Rule {
        scope: "|namespace",
        re: concat!(ts_lead!(), r"interface\s+(?P<name>", ts_id!(), ")"),
        does: Does::Def("interface"),
        opens: "interface",
    },
    Rule {
        scope: "|namespace",
        re: concat!(ts_lead!(), r"(?:const\s+)?enum\s+(?P<name>", ts_id!(), ")"),
        does: Does::Def("enum"),
        opens: "enum",
    },
    // A type alias whose line opens an object type (`= {`, `= A & {`,
    // `= Merge<A, {`): its members are defined as an interface's.
    Rule {
        scope: "|namespace",
        re: concat!(ts_lead!(), r"type\s+(?P<name>", ts_id!(), r")\s*(?:<.*>)?\s*=\s*(?:.*[&|<,]\s*)?\{\s*$"),
        does: Does::Def("type"),
        opens: "interface",
    },
    Rule {
        scope: "|namespace",
        re: concat!(ts_lead!(), r"type\s+(?P<name>", ts_id!(), r")\s*(?:<|=|$)"),
        does: Does::Def("type"),
        opens: "",
    },
    Rule {
        scope: "|namespace",
        re: concat!(ts_lead!(), r"(?:async\s+)?function\s*\*?\s*(?P<name>", ts_id!(), ")"),
        does: Does::Def("fn"),
        opens: "",
    },
    // `namespace N {`, `module N {`, `declare module "x" {` (its name is
    // blanked with the quotes), `declare global {`.
    Rule {
        scope: "|namespace",
        re: concat!(ts_lead!(), r"(?:(?:namespace|module)\s|global\s*\{)[^{]*\{"),
        does: Does::Open,
        opens: "namespace",
    },
    Rule {
        scope: "|namespace",
        re: concat!(ts_lead!(), r"(?:const|let|var|using)\s+(?P<name>", ts_id!(), ")"),
        does: Does::Def("var"),
        opens: "",
    },
    // A method assigned to a constructor's prototype (`View.prototype.render
    // = function render() {`), and a declarator continued comma-first on the
    // next line (`var a = require('a')` then `  , b = require('b')`).
    Rule {
        scope: "|namespace",
        re: concat!(r"^\s*(?P<container>", ts_id!(), r")\.prototype\.(?P<name>", ts_id!(), r")\s*=[^=]"),
        does: Does::Def("method"),
        opens: "",
    },
    Rule { scope: "|namespace", re: concat!(r"^\s*,\s*(?P<name>", ts_id!(), r")\s*(?:=[^=]|,|;|$)"), does: Does::Def("var"), opens: "" },
    // A class body.
    Rule { scope: "class", re: concat!(ts_mods!(), r"constructor\s*\("), does: Does::DefOwner("ctor"), opens: "" },
    Rule {
        scope: "class",
        re: concat!(ts_mods!(), r"(?:(?:get|set)\s+)?\*?\s*(?P<name>#?", ts_id!(), r")\s*\??\s*(?:<.*>)?\s*\("),
        does: Does::Def("method"),
        opens: "",
    },
    // A property whose type is an object type opens it.
    Rule {
        scope: "class",
        re: concat!(ts_mods!(), r"(?P<name>#?", ts_id!(), r")\s*[?!]?\s*:\s*(?:.*[&|<,]\s*)?\{\s*$"),
        does: Does::Def("property"),
        opens: "interface",
    },
    Rule {
        scope: "class",
        re: concat!(ts_mods!(), r"(?P<name>#?", ts_id!(), r")\s*[?!]?\s*(?::|=|;|$)"),
        does: Does::Def("property"),
        opens: "",
    },
    // An interface or object type body: methods (accessors included) and
    // properties (an index or call signature defines nothing).
    Rule {
        scope: "interface",
        re: concat!(r"^\s*(?:readonly\s+)?(?:(?:get|set)\s+)?(?P<name>", ts_id!(), r")\s*\??\s*(?:<.*>)?\s*\("),
        does: Does::Def("method"),
        opens: "",
    },
    Rule {
        scope: "interface",
        re: concat!(r"^\s*(?:readonly\s+)?(?P<name>", ts_id!(), r")\s*\??\s*:\s*(?:.*[&|<,]\s*)?\{\s*$"),
        does: Does::Def("property"),
        opens: "interface",
    },
    Rule {
        scope: "interface",
        re: concat!(r"^\s*(?:readonly\s+)?(?P<name>", ts_id!(), r")\s*\??\s*:"),
        does: Does::Def("property"),
        opens: "",
    },
    // An enum body: its members.
    Rule { scope: "enum", re: concat!(r"^\s*(?P<name>", ts_id!(), r")\s*(?:=|,|$)"), does: Does::Def("variant"), opens: "" },
    // An object type written inline on a line of any of those scopes
    // (`Merge<A, {x: T; y: U}>`, `opts: {a?: T}`): the member after its
    // opening brace and each one after a `;`.
    Rule {
        scope: "|namespace|class|interface",
        re: concat!(r"(?:(?:[:<&|,]|=>)\s*\{|;)\s*(?:readonly\s+)?(?P<name>", ts_id!(), r")\s*\??\s*:"),
        does: Does::Each("property"),
        opens: "",
    },
];

/// A line that continues a TypeScript definition over several lines: a
/// union or intersection member, a conditional type's branch, a chained
/// call.
const TS_CONTINUED: &str = r"^\s*(?:[|&?:.]|=>)";

/// TypeScript and JavaScript keep their dependencies, build output and
/// coverage reports in these folders.
const TS_SKIP: &[&str] = &["node_modules", "bower_components", "jspm_packages", "dist", "build", "out", "coverage"];

/// The files whose change makes a TypeScript or JavaScript root stale.
const TS_STALE: &[&str] = &[
    "package.json",
    "tsconfig.json",
    "jsconfig.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "lerna.json",
];

/// The `--recall-check` truth for TypeScript and JavaScript: a top-level
/// declaration (column 0, so locals and members are not counted).
const TS_TRUTH: &str = concat!(
    r"^(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?",
    r"(?P<kw>class|interface|(?:const\s+)?enum|type|function\s*\*?|const|let|var)\s+(?P<name>",
    ts_id!(),
    r")"
);

const CS_QUOTES: &[Quote] = &[
    Quote { open: "//", close: "", escape: false, multiline: false, template: "" },
    Quote { open: "/*", close: "*/", escape: false, multiline: true, template: "" },
    // A preprocessor directive (`#if`, `#region`) is no code.
    Quote { open: "#", close: "", escape: false, multiline: false, template: "" },
    // Raw and verbatim strings run across lines and know no escapes (a
    // verbatim string's `""` closes and reopens it, which blanks the same).
    Quote { open: "\"\"\"", close: "\"\"\"", escape: false, multiline: true, template: "" },
    Quote { open: "@$\"", close: "\"", escape: false, multiline: true, template: "" },
    Quote { open: "$@\"", close: "\"", escape: false, multiline: true, template: "" },
    Quote { open: "@\"", close: "\"", escape: false, multiline: true, template: "" },
    Quote { open: "\"", close: "\"", escape: true, multiline: false, template: "" },
    Quote { open: "'", close: "'", escape: true, multiline: false, template: "" },
];

/// A C# identifier.
macro_rules! cs_id {
    () => {
        r"@?[A-Za-z_]\w*"
    };
}

/// Attributes and modifiers before a C# declaration's keyword or type.
macro_rules! cs_lead {
    () => {
        concat!(
            r"^\s*(?:\[[^\]]*\]\s*)*",
            r"(?:(?:public|private|protected|internal|static|readonly|sealed|abstract|virtual|override|new|extern|unsafe|async|partial|volatile|required|file|ref|fixed)\s+)*"
        )
    };
}

/// A C# type: a tuple, or a dotted name with type arguments, then array,
/// nullable and pointer suffixes.
macro_rules! cs_type {
    () => {
        r"(?:\([^()]*\)|[A-Za-z_][\w.:]*(?:\s*<[\w\s,.:<>?\[\]()]*?>)?)(?:\s*\[[,\s]*\]|\?|\*)*"
    };
}

const CS_RULES: &[Rule] = &[
    // A namespace: a block (its body is where types are) or, file-scoped,
    // the container of the file's types. Its last segment names it, as
    // SCIP's namespace descriptors do.
    Rule { scope: "", re: r"^\s*namespace\s+(?:[\w.]+\.)?(?P<name>[A-Za-z_]\w*)\s*;", does: Does::Container, opens: "" },
    Rule {
        scope: "|namespace",
        re: r"^\s*namespace\s+(?:[\w.]+\.)?(?P<name>[A-Za-z_]\w*)\s*(?:\{|$)",
        does: Does::Open,
        opens: "namespace",
    },
    // Types, at the top level, in a namespace or nested in a type.
    Rule {
        scope: "|namespace|class",
        re: concat!(cs_lead!(), r"(?:record\s+)?struct\s+(?P<name>", cs_id!(), ")"),
        does: Does::Def("struct"),
        opens: "class",
    },
    Rule {
        scope: "|namespace|class",
        re: concat!(cs_lead!(), r"interface\s+(?P<name>", cs_id!(), ")"),
        does: Does::Def("interface"),
        opens: "class",
    },
    Rule {
        scope: "|namespace|class",
        re: concat!(cs_lead!(), r"(?:class|record(?:\s+class)?)\s+(?P<name>", cs_id!(), ")"),
        does: Does::Def("class"),
        opens: "class",
    },
    Rule { scope: "|namespace|class", re: concat!(cs_lead!(), r"enum\s+(?P<name>", cs_id!(), ")"), does: Does::Def("enum"), opens: "enum" },
    Rule {
        scope: "|namespace|class",
        re: concat!(cs_lead!(), r"delegate\s+", cs_type!(), r"\s+(?P<name>", cs_id!(), r")\s*(?:<[\w\s,.<>?\[\]]*>)?\s*\("),
        does: Does::Def("type"),
        opens: "",
    },
    // A type's members. A constructor is stored under its type's name, as
    // SCIP stores `.ctor`; a call continuing an expression-bodied member
    // on the next line is no constructor (the name must be the type's).
    Rule { scope: "class", re: concat!(cs_lead!(), r"(?P<name>", cs_id!(), r")\s*\("), does: Does::DefOwner("ctor"), opens: "" },
    Rule {
        scope: "class",
        re: concat!(cs_lead!(), r"event\s+", cs_type!(), r"\s+(?P<name>", cs_id!(), r"(?:\s*,\s*", cs_id!(), r")*)"),
        does: Does::Def("event"),
        opens: "",
    },
    Rule {
        scope: "class",
        re: concat!(cs_lead!(), r"const\s+", cs_type!(), r"\s+(?P<name>", cs_id!(), ")"),
        does: Does::Def("const"),
        opens: "",
    },
    // Methods (an explicit interface member, `IEnumerable<T>.GetEnumerator`,
    // by its own name), then properties (accessors, an expression body, or
    // accessors on the next lines), then fields.
    Rule {
        scope: "class",
        re: concat!(cs_lead!(), cs_type!(), r"\s+(?:", cs_type!(), r"\.)?(?P<name>", cs_id!(), r")\s*(?:<[\w\s,.<>?\[\]]*>)?\s*\("),
        does: Does::Def("method"),
        opens: "",
    },
    Rule {
        scope: "class",
        re: concat!(cs_lead!(), cs_type!(), r"\s+(?:", cs_type!(), r"\.)?(?P<name>", cs_id!(), r")\s*(?:\{|=>|$)"),
        does: Does::Def("property"),
        opens: "",
    },
    Rule {
        scope: "class",
        re: concat!(cs_lead!(), cs_type!(), r"\s+(?P<name>", cs_id!(), r"(?:\s*,\s*", cs_id!(), r")*)\s*(?:=[^=>]|=$|;)"),
        does: Does::Def("field"),
        opens: "",
    },
    // A field line's further declarators (`int _a = 1, _b = 2;`).
    Rule { scope: "class", re: concat!(r",\s*(?P<name>", cs_id!(), r")\s*=[^=>]"), does: Does::Each("field"), opens: "" },
    // An enum's members (fields, as SCIP's shape gives them).
    Rule {
        scope: "enum",
        re: concat!(r"^\s*(?:\[[^\]]*\]\s*)*(?P<name>", cs_id!(), r")\s*(?:=[^,]*)?,?\s*$"),
        does: Does::Def("field"),
        opens: "",
    },
];

/// A line that continues a C# declaration with no body of its own: an
/// expression body or the `;` written on a later line (after an `#if`
/// branch, `void F()` / `#if X` / `=> G()` / `#endif` / `;`).
const CS_CONTINUED: &str = r"^\s*(?:=>|;)";

/// C# keywords and built-in type names: never a definition's name.
const CS_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "base",
    "bool",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "checked",
    "class",
    "const",
    "continue",
    "decimal",
    "default",
    "delegate",
    "do",
    "double",
    "else",
    "enum",
    "event",
    "explicit",
    "extern",
    "false",
    "finally",
    "fixed",
    "float",
    "for",
    "foreach",
    "goto",
    "if",
    "implicit",
    "in",
    "int",
    "interface",
    "internal",
    "is",
    "lock",
    "long",
    "namespace",
    "new",
    "null",
    "object",
    "operator",
    "out",
    "override",
    "params",
    "private",
    "protected",
    "public",
    "readonly",
    "ref",
    "return",
    "sbyte",
    "sealed",
    "short",
    "sizeof",
    "stackalloc",
    "static",
    "string",
    "struct",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "uint",
    "ulong",
    "unchecked",
    "unsafe",
    "ushort",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
    "var",
    "dynamic",
    "nint",
    "nuint",
];

/// C# build output, restored packages and tool folders.
const CS_SKIP: &[&str] = &["obj", "bin", "packages", "TestResults", "artifacts", "node_modules"];

/// The files whose change makes a C# root stale.
const CS_STALE: &[&str] =
    &["*.sln", "*.slnx", "*.csproj", "*.props", "*.targets", "global.json", "packages.lock.json", "NuGet.config", "nuget.config"];

/// The `--recall-check` truth for C#: a type declaration, or a member
/// declared with an access modifier (fields, properties, methods,
/// constructors, events, delegates). Members without one (interface
/// members, enum members, explicit interface implementations) and
/// operators are not counted.
const CS_TRUTH: &str = concat!(
    r"^\s*(?:\[[^\]]*\]\s*)*(?:(?:new|static|sealed|abstract|partial|readonly|unsafe|file|virtual|override|async|extern|volatile|required)\s+)*",
    r"(?P<kw>public|protected|internal|private|class|interface|struct|enum|record)\b",
    r"(?:\s+(?:public|protected|internal|private|new|static|sealed|abstract|partial|readonly|unsafe|virtual|override|async|extern|const|volatile|required|class|interface|struct|enum|record|event|delegate))*",
    r"\s+(?:[A-Za-z_][\w.]*(?:<[^;=(){}]*>)?(?:\[[,\s]*\])*\??\s+)?(?P<name>[A-Za-z_]\w*)\s*(?:<[^>]*>\s*)?(?:\(|\{|=>|=|;|:|$)"
);

const PY_QUOTES: &[Quote] = &[
    Quote { open: "#", close: "", escape: false, multiline: false, template: "" },
    // Triple-quoted strings (docstrings) run across lines. A prefix
    // (`r`, `b`, `f`) stays: it is a letter before the quote.
    Quote { open: "\"\"\"", close: "\"\"\"", escape: true, multiline: true, template: "" },
    Quote { open: "'''", close: "'''", escape: true, multiline: true, template: "" },
    Quote { open: "\"", close: "\"", escape: true, multiline: false, template: "" },
    Quote { open: "'", close: "'", escape: true, multiline: false, template: "" },
];

/// Python assignment targets: one name, a comma-separated list (`a, b = 1,
/// 2`) or a chain (`a = b = 0`); [`Does::Def`] defines each.
macro_rules! py_names {
    () => {
        r"[A-Za-z_]\w*(?:\s*[,=]\s*[A-Za-z_]\w*)*"
    };
}

/// A compound statement's header: its body belongs to the scope around it
/// (a `def` under `if TYPE_CHECKING:` is a top-level function).
const PY_COMPOUND: &str = r"^\s*(?:(?:if|elif|else|try|except\*?|finally|with|for|while|match|case)\b|async\s+(?:with|for)\b)";

const PY_RULES: &[Rule] = &[
    // The top level, and a compound statement's body there.
    Rule { scope: "", re: r"^\s*(?:async\s+)?def\s+(?P<name>[A-Za-z_]\w*)", does: Does::Def("fn"), opens: "fn" },
    Rule { scope: "", re: r"^\s*class\s+(?P<name>[A-Za-z_]\w*)", does: Does::Def("class"), opens: "class" },
    // Module attributes: assigned (annotated or not, a name list), or
    // annotated alone.
    Rule { scope: "", re: concat!(r"^\s*(?P<name>", py_names!(), r")\s*(?::[^=]*)?=[^=]"), does: Does::Def("var"), opens: "" },
    Rule { scope: "", re: r"^\s*(?P<name>[A-Za-z_]\w*)\s*:\s*[^=\s][^=]*$", does: Does::Def("var"), opens: "" },
    // Names a `with` or `for` statement binds at the top level are module
    // attributes too.
    Rule { scope: "", re: r"^\s*(?:async\s+)?with\b.*\bas\s+(?P<name>[A-Za-z_]\w*)\s*:\s*$", does: Does::Def("var"), opens: SAME_SCOPE },
    Rule {
        scope: "",
        re: r"^\s*(?:async\s+)?for\s+(?P<name>[A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)\s+in\b",
        does: Does::Def("var"),
        opens: SAME_SCOPE,
    },
    // A class body: the constructor (stored under the class's name, as SCIP
    // stores `__init__`), methods, nested classes and class attributes. A
    // method's block keeps the class as container.
    Rule { scope: "class", re: r"^\s*(?:async\s+)?def\s+__init__\s*\(", does: Does::DefOwner("ctor"), opens: "method" },
    Rule { scope: "class", re: r"^\s*(?:async\s+)?def\s+(?P<name>[A-Za-z_]\w*)", does: Does::Member("method"), opens: "method" },
    Rule { scope: "class", re: r"^\s*class\s+(?P<name>[A-Za-z_]\w*)", does: Does::Def("class"), opens: "class" },
    Rule { scope: "class", re: concat!(r"^\s*(?P<name>", py_names!(), r")\s*(?::[^=]*)?=[^=]"), does: Does::Def("field"), opens: "" },
    Rule { scope: "class", re: r"^\s*(?P<name>[A-Za-z_]\w*)\s*:\s*[^=\s][^=]*$", does: Does::Def("field"), opens: "" },
    // A method's body: attributes set (or declared) through `self`, and
    // nested functions and classes (SCIP stores a function nested in a
    // method as a member of the class). Locals are not definitions.
    Rule { scope: "method", re: r"^\s*self\.(?P<name>[A-Za-z_]\w*)\s*(?::|=[^=])", does: Does::Def("field"), opens: "" },
    Rule { scope: "method", re: r"^\s*(?:async\s+)?def\s+(?P<name>[A-Za-z_]\w*)", does: Does::Member("method"), opens: "method" },
    Rule { scope: "method", re: r"^\s*class\s+(?P<name>[A-Za-z_]\w*)", does: Does::Def("class"), opens: "class" },
    // A function's body: nested functions and classes.
    Rule { scope: "fn", re: r"^\s*(?:async\s+)?def\s+(?P<name>[A-Za-z_]\w*)", does: Does::Def("fn"), opens: "fn" },
    Rule { scope: "fn", re: r"^\s*class\s+(?P<name>[A-Za-z_]\w*)", does: Does::Def("class"), opens: "class" },
    // A compound statement in any of those: no scope of its own.
    Rule { scope: "|class|method|fn", re: PY_COMPOUND, does: Does::Open, opens: SAME_SCOPE },
];

/// Python keywords and soft keywords: never a definition's name (`print`
/// and other built-ins may be).
const PY_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try",
    "while", "with", "yield",
];

/// Virtual environments, caches, build output and installed packages.
/// (Dot folders, `.venv` and `.tox` among them, are never walked.)
const PY_SKIP: &[&str] = &["venv", "__pycache__", "__pypackages__", "site-packages", "build", "dist", "*.egg-info", "node_modules"];

/// The files whose change makes a Python root stale.
const PY_STALE: &[&str] =
    &["pyproject.toml", "setup.py", "setup.cfg", "requirements*.txt", "Pipfile", "Pipfile.lock", "poetry.lock", "uv.lock", "pdm.lock"];

const JAVA_QUOTES: &[Quote] = &[
    Quote { open: "//", close: "", escape: false, multiline: false, template: "" },
    Quote { open: "/*", close: "*/", escape: false, multiline: true, template: "" },
    // Text blocks run across lines.
    Quote { open: "\"\"\"", close: "\"\"\"", escape: true, multiline: true, template: "" },
    Quote { open: "\"", close: "\"", escape: true, multiline: false, template: "" },
    Quote { open: "'", close: "'", escape: true, multiline: false, template: "" },
];

/// A Java identifier.
macro_rules! java_id {
    () => {
        r"[A-Za-z_$][\w$]*"
    };
}

/// Annotations (`@Override`, `@SuppressWarnings("unchecked")`, one level
/// of nested parentheses) and modifiers before a Java declaration.
macro_rules! java_lead {
    () => {
        concat!(
            r"^\s*(?:(?:@[\w$.]+(?:\s*\([^()]*(?:\([^()]*\)[^()]*)*\))?|public|private|protected|static|final|abstract|native|synchronized|transient|volatile|strictfp|default|sealed|non-sealed)\s+)*"
        )
    };
}

/// Java type arguments or parameters, nested up to three deep, each list
/// balanced (`<K, List<Map<K, V>>>`), so `<T> List<T>` is two lists.
macro_rules! java_targs {
    () => {
        r"<(?:[\w$\s,.?&\[\]@]|<(?:[\w$\s,.?&\[\]@]|<[\w$\s,.?&\[\]@]*>)*>)*>"
    };
}

/// A Java type: annotations on it, a dotted name with type arguments, then
/// array brackets or varargs.
macro_rules! java_type {
    () => {
        concat!(r"(?:@[\w$.]+\s+)*[A-Za-z_$][\w$.]*(?:\s*", java_targs!(), r")?(?:\s*\[\s*\])*(?:\.\.\.)?")
    };
}

/// A generic method's or constructor's type parameters (`<T extends
/// Comparable<T>>`).
macro_rules! java_tparams {
    () => {
        concat!(r"(?:", java_targs!(), r"\s*)?")
    };
}

/// The end of a line that opens an anonymous class's body: `new
/// Type<A>(args) {`.
macro_rules! java_new_body {
    () => {
        r"\bnew\s+[\w$.]+\s*(?:<[^;]*>)?\s*\([^;]*\)\s*\{\s*$"
    };
}

const JAVA_RULES: &[Rule] = &[
    // The package names the container of the file's top-level types, as
    // the last namespace segment of scip-java's symbols does.
    Rule { scope: "", re: concat!(r"^\s*package\s+(?:[\w$.]+\.)?(?P<name>", java_id!(), r")\s*;"), does: Does::Container, opens: "" },
    // Types, at the top level or nested in a type.
    Rule {
        scope: "|class|enum",
        re: concat!(java_lead!(), r"@\s*interface\s+(?P<name>", java_id!(), ")"),
        does: Does::Def("interface"),
        opens: "class",
    },
    Rule {
        scope: "|class|enum",
        re: concat!(java_lead!(), r"interface\s+(?P<name>", java_id!(), ")"),
        does: Does::Def("interface"),
        opens: "class",
    },
    Rule { scope: "|class|enum", re: concat!(java_lead!(), r"enum\s+(?P<name>", java_id!(), ")"), does: Does::Def("enum"), opens: "enum" },
    Rule {
        scope: "|class|enum",
        re: concat!(java_lead!(), r"(?:class|record)\s+(?P<name>", java_id!(), ")"),
        does: Does::Def("class"),
        opens: "class",
    },
    // A type's members. A constructor is stored under its type's name, as
    // SCIP stores `<init>` (the name must be the type's: a call continuing
    // a field's initializer is none).
    Rule {
        scope: "class|enum",
        re: concat!(java_lead!(), java_tparams!(), r"(?P<name>", java_id!(), r")\s*\("),
        does: Does::DefOwner("ctor"),
        opens: "",
    },
    Rule {
        scope: "class|enum",
        re: concat!(java_lead!(), java_tparams!(), java_type!(), r"\s+(?P<name>", java_id!(), r")\s*\("),
        does: Does::Def("method"),
        opens: "",
    },
    // Fields: a field whose value is an anonymous class (its methods are
    // defined, as the class's members), then one or several declarators
    // (`int a, b;`), a value, or a value on the next line.
    Rule {
        scope: "class|enum",
        re: concat!(java_lead!(), java_type!(), r"\s+(?P<name>", java_id!(), r")\s*=\s*", java_new_body!()),
        does: Does::Def("field"),
        opens: "class",
    },
    Rule {
        scope: "class|enum",
        re: concat!(java_lead!(), java_type!(), r"\s+(?P<name>", java_id!(), r"(?:\s*,\s*", java_id!(), r")*)\s*(?:\[\s*\]\s*)*(?:=[^=]|=$|;|,\s*$)"),
        does: Does::Def("field"),
        opens: "",
    },
    // A field line's further declarators (`int a = 1, b = 2;`).
    Rule { scope: "class|enum", re: concat!(r",\s*(?P<name>", java_id!(), r")\s*=[^=]"), does: Does::Each("field"), opens: "" },
    // An enum's constants, before its members: a name list, or one constant
    // with arguments or a body of its own (whose methods are defined, as the
    // constant's).
    Rule {
        scope: "enum",
        re: concat!(r"^\s*(?:@[\w$.]+(?:\s*\([^()]*\))?\s+)*(?P<name>", java_id!(), r"(?:\s*,\s*", java_id!(), r")*)\s*(?:\(|\{|,|;|$)"),
        does: Does::Def("field"),
        opens: "class",
    },
    // In a method's body: local types, which SCIP keeps (an anonymous
    // class's members it keeps local, but the scanner defines them, as a
    // reader looks for them: `fills`).
    Rule {
        scope: OPAQUE,
        re: concat!(r"^\s*(?:@[\w$.]+(?:\s*\([^()]*\))?\s+)*(?:(?:final|abstract|static|strictfp|sealed|non-sealed)\s+)*interface\s+(?P<name>", java_id!(), ")"),
        does: Does::Def("interface"),
        opens: "class",
    },
    Rule {
        scope: OPAQUE,
        re: concat!(r"^\s*(?:@[\w$.]+(?:\s*\([^()]*\))?\s+)*(?:(?:final|abstract|static|strictfp|sealed|non-sealed)\s+)*enum\s+(?P<name>", java_id!(), ")"),
        does: Does::Def("enum"),
        opens: "enum",
    },
    Rule {
        scope: OPAQUE,
        re: concat!(r"^\s*(?:@[\w$.]+(?:\s*\([^()]*\))?\s+)*(?:(?:final|abstract|static|strictfp|sealed|non-sealed)\s+)*(?:class|record)\s+(?P<name>", java_id!(), ")"),
        does: Does::Def("class"),
        opens: "class",
    },
    Rule { scope: OPAQUE, re: java_new_body!(), does: Does::Open, opens: "class" },
];

/// Java keywords: never a definition's name.
const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
    "true",
    "false",
    "null",
];

/// Build output of Maven (`target`, never walked), Gradle and the IDEs.
const JAVA_SKIP: &[&str] = &["build", "out", "bin", "node_modules"];

/// The files whose change makes a Java or Kotlin root stale: Maven and
/// Gradle builds, the Gradle version catalog and wrapper.
const JAVA_STALE: &[&str] = &[
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradle.properties",
    "libs.versions.toml",
    "gradle-wrapper.properties",
];

/// The `--recall-check` truth for Java: a type declaration (with any
/// modifiers), or a member declared with an access modifier (fields,
/// methods, constructors). Members without one (package-private members,
/// interface members, enum constants) are not counted. `kw` is what comes
/// before the name.
const JAVA_TRUTH: &str = concat!(
    r"^\s*(?:@[\w$.]+(?:\s*\([^()]*(?:\([^()]*\)[^()]*)*\))?\s+)*(?P<kw>",
    r"(?:(?:public|protected|private|static|final|abstract|sealed|non-sealed|strictfp)\s+)*(?:class|interface|enum|record|@interface)\s+",
    r"|(?:(?:static|final|abstract|synchronized|native|transient|volatile|default)\s+)*(?:public|protected|private)",
    r"(?:\s+(?:static|final|abstract|synchronized|native|transient|volatile|default|strictfp))*",
    r"\s+(?:<[^;=(){}]*>\s+)?(?:(?:@[\w$.]+\s+)*[A-Za-z_$][\w$.]*(?:\s*<[^;=(){}]*>)?(?:\s*\[\s*\])*(?:\.\.\.)?\s+)?)",
    r"(?P<name>[A-Za-z_$][\w$]*)\s*(?:<[^;=(){}]*>\s*)?(?:\(|\{|=|;|,|\s|$)"
);

const KT_QUOTES: &[Quote] = &[
    Quote { open: "//", close: "", escape: false, multiline: false, template: "" },
    Quote { open: "/*", close: "*/", escape: false, multiline: true, template: "" },
    // Strings run on while a template expression in them is open (`${`
    // to its `}`, which may hold strings and run across lines); raw strings
    // run across lines and know no escapes.
    Quote { open: "\"\"\"", close: "\"\"\"", escape: false, multiline: true, template: "${" },
    Quote { open: "\"", close: "\"", escape: true, multiline: false, template: "${" },
    Quote { open: "'", close: "'", escape: true, multiline: false, template: "" },
];

/// A Kotlin identifier (a backticked name is not read).
macro_rules! kt_id {
    () => {
        r"[A-Za-z_]\w*"
    };
}

/// Annotations (`@Suppress("x")`, `@file:JvmName("x")`, one level of
/// nested parentheses) and modifiers before a Kotlin declaration.
macro_rules! kt_lead {
    () => {
        concat!(
            r"^\s*(?:(?:@[\w.:]+(?:\s*\([^()]*(?:\([^()]*\)[^()]*)*\))?|public|private|protected|internal|open|abstract|final|sealed|data|enum|annotation|inline|value|inner|override|suspend|tailrec|operator|infix|external|expect|actual|lateinit|const|crossinline|noinline)\s+)*"
        )
    };
}

/// What may come between `fun`, `val` or `var` and the name: type
/// parameters, then a receiver type (`fun <T> List<T>.second()`).
macro_rules! kt_pre_name {
    () => {
        r"(?:<[^=(){}]*>\s*)?(?:[\w.?<>, *]+\.)?"
    };
}

const KT_RULES: &[Rule] = &[
    // The package names the container of the file's top-level
    // definitions, as a Java package's last segment does.
    Rule { scope: "", re: concat!(r"^\s*package\s+(?:[\w.]+\.)?(?P<name>", kt_id!(), r")"), does: Does::Container, opens: "" },
    // Types, at the top level or nested in a type. A class's primary
    // constructor is in its scope (its `val` parameters are properties).
    Rule { scope: "|class|enum", re: concat!(kt_lead!(), r"enum\s+class\s+(?P<name>", kt_id!(), ")"), does: Does::Def("enum"), opens: "enum" },
    Rule {
        scope: "|class|enum",
        re: concat!(kt_lead!(), r"(?:fun\s+)?interface\s+(?P<name>", kt_id!(), ")"),
        does: Does::Def("interface"),
        opens: "class",
    },
    Rule { scope: "|class|enum", re: concat!(kt_lead!(), r"class\s+(?P<name>", kt_id!(), ")"), does: Does::Def("class"), opens: "class" },
    // A named object, and a companion object, whose members are called
    // through its class (an unnamed one keeps the class as container).
    Rule {
        scope: "|class|enum",
        re: concat!(kt_lead!(), r"(?:companion\s+)?object\s+(?P<name>", kt_id!(), ")"),
        does: Does::Def("object"),
        opens: "class",
    },
    Rule { scope: "class|enum", re: concat!(kt_lead!(), r"companion\s+object\b"), does: Does::Open, opens: "class" },
    Rule { scope: "|class|enum", re: concat!(kt_lead!(), r"typealias\s+(?P<name>", kt_id!(), ")"), does: Does::Def("type"), opens: "" },
    // Functions and properties: top-level ones, then members.
    Rule { scope: "", re: concat!(kt_lead!(), r"fun\s+", kt_pre_name!(), r"(?P<name>", kt_id!(), r")\s*[(<]"), does: Does::Def("fn"), opens: "" },
    Rule {
        scope: "class|enum",
        re: concat!(kt_lead!(), r"fun\s+", kt_pre_name!(), r"(?P<name>", kt_id!(), r")\s*[(<]"),
        does: Does::Def("method"),
        opens: "",
    },
    Rule { scope: "class|enum", re: concat!(kt_lead!(), r"constructor\s*\("), does: Does::DefOwner("ctor"), opens: "" },
    // A property whose value is an object expression: its members are the
    // property's.
    Rule {
        scope: "|class|enum",
        re: concat!(kt_lead!(), r"(?:val|var)\s+", kt_pre_name!(), r"(?P<name>", kt_id!(), r")\b[^=]*=\s*object\s*:[^{]*\{\s*$"),
        does: Does::Def("property"),
        opens: "class",
    },
    Rule {
        scope: "|class|enum",
        re: concat!(kt_lead!(), r"(?:val|var)\s+", kt_pre_name!(), r"(?P<name>", kt_id!(), r")\s*(?::|=|$|\bby\b)"),
        does: Does::Def("property"),
        opens: "",
    },
    // A primary constructor's further properties on its line
    // (`class P(val x: Int, val y: Int)`).
    Rule {
        scope: "|class|enum",
        re: concat!(r"[(,]\s*(?:@[\w.:]+\s+)*(?:(?:public|private|protected|internal|override|open|final)\s+)*(?:val|var)\s+(?P<name>", kt_id!(), r")\s*:"),
        does: Does::Each("property"),
        opens: "",
    },
    // An enum's entries, before its members: a name list, or one entry
    // with arguments or a body of its own (whose members are the entry's).
    Rule {
        scope: "enum",
        re: concat!(r"^\s*(?:@[\w.:]+(?:\s*\([^()]*\))?\s+)*(?P<name>", kt_id!(), r"(?:\s*,\s*", kt_id!(), r")*)\s*(?:\(|\{|,|;|$)"),
        does: Does::Def("variant"),
        opens: "class",
    },
    // An object expression's body (`object : Visitor {`) in a function or
    // a value: its members, as a Java anonymous class's.
    Rule { scope: OPAQUE, re: r"\bobject\s*:[^{]*\{\s*$", does: Does::Open, opens: "class" },
];

/// A line continuing a Kotlin class's header at its own indent: its
/// primary constructor with modifiers (`class X` then `private
/// constructor(`), whose parameter list is the class's.
const KT_HEADER: &str = r"^\s*(?:(?:@[\w.]+(?:\([^()]*\))?|private|internal|protected|public)\s+)*constructor\s*\(";

/// Kotlin keywords that may begin a line in a declaration's scope: never
/// a definition's name.
const KT_KEYWORDS: &[&str] = &[
    "as", "break", "class", "continue", "do", "else", "false", "for", "fun", "if", "in", "interface", "is", "null", "object", "package", "return",
    "super", "this", "throw", "true", "try", "typealias", "typeof", "val", "var", "when", "while", "init", "constructor", "companion",
];

/// The `--recall-check` truth for Kotlin: a type, object, function or type
/// alias declaration anywhere, and a property declared at the top level
/// (column 0) or with a modifier a local cannot have. Properties of a class
/// without one (most, Kotlin's default is public) and enum entries are not
/// counted.
const KT_TRUTH: &str = concat!(
    r"^(?P<kw>\s*(?:@[\w.:]+(?:\s*\([^()]*\))?\s+)*",
    r"(?:(?:public|private|protected|internal|open|abstract|final|sealed|data|enum|annotation|inline|value|inner|override|suspend|tailrec|operator|infix|external|expect|actual|companion|fun)\s+)*",
    r"(?:class|interface|object|fun|typealias)",
    r"|(?:@[\w.:]+(?:\s*\([^()]*\))?\s+)*(?:(?:public|private|protected|internal|override|const|lateinit|open|abstract|final|expect|actual|inline)\s+)*(?:val|var)",
    r"|\s*(?:@[\w.:]+(?:\s*\([^()]*\))?\s+)*(?:(?:open|abstract|final|expect|actual|inline)\s+)*(?:public|private|protected|internal|override|const|lateinit)\s+",
    r"(?:(?:public|private|protected|internal|override|const|lateinit|open|abstract|final|expect|actual|inline)\s+)*(?:val|var))",
    r"\s+(?:<[^=(){}]*>\s*)?(?:[\w.?<>, *]+\.)?(?P<name>[A-Za-z_]\w*)"
);

/// The languages, by row. See the module documentation.
pub static LANGS: &[Lang] = &[
    Lang {
        name: "go",
        exts: &["go"],
        skip_dirs: &["vendor", "testdata", "_*"],
        markers: &["go.mod"],
        nested_units: true,
        units_of: None,
        stale: &["go.mod", "go.sum", "go.work", "go.work.sum"],
        scanner: ScanRow {
            quotes: GO_QUOTES,
            rules: GO_RULES,
            file_container: false,
            continued_by: "",
            header_continued: "",
            allman: false,
            not_names: &[],
            indent: false,
            once: &[],
        },
        truth: r"^(?P<kw>func|type|var|const)\b\s*(?:\([^)]*\)\s*)?(?P<name>[A-Za-z_]\w*)",
        fills: false,
        unindexed: None,
        packages: Some(Packages { managers: &["gomod"], fold: Fold::Exact }),
        grpc: Some(Grpc {
            generated: &["*.pb.go"],
            stubs: &["{Rpc}"],
            servers: &["{Rpc}"],
            bases: &["{S}Server", "Unimplemented{S}Server"],
        }),
        driver: Some(Driver {
            tool: "scip-go",
            node_package: None,
            find: &[Find::Path, Find::Env("GOBIN"), Find::EnvJoin("GOPATH", "bin"), Find::Home("go/bin")],
            version_arg: "--version",
            needs: &["go"],
            args: &["--output", "{out}"],
            prereq: None,
            projects: None,
            env: &[],
            fail_hint: None,
            not_on_windows: None,
        }),
    },
    Lang {
        name: "typescript",
        exts: &["ts", "tsx", "mts", "cts"],
        skip_dirs: TS_SKIP,
        markers: &["tsconfig.json", "package.json"],
        nested_units: false,
        units_of: None,
        stale: TS_STALE,
        scanner: ScanRow {
            quotes: TS_QUOTES,
            rules: TS_RULES,
            file_container: true,
            continued_by: TS_CONTINUED,
            header_continued: "",
            allman: false,
            not_names: &[],
            indent: false,
            once: &[],
        },
        truth: TS_TRUTH,
        fills: false,
        unindexed: None,
        packages: Some(Packages { managers: &["npm"], fold: Fold::Exact }),
        grpc: Some(Grpc {
            generated: &["*_pb.d.ts", "*_pb.ts", "*_grpc_pb.d.ts", "*_grpc_web_pb.d.ts"],
            stubs: &["{rpc}"],
            servers: &["{rpc}"],
            bases: &["I{S}Server", "{S}Server"],
        }),
        driver: Some(Driver {
            tool: "scip-typescript",
            node_package: Some("@sourcegraph/scip-typescript"),
            find: &[Find::Path, Find::NpmRoot],
            version_arg: "--version",
            needs: &["node"],
            args: &["index", "--no-progress-bar", "--output", "{out}"],
            prereq: Some(Prereq {
                dir: "node_modules",
                manifest: "package.json",
                keys: &["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"],
                fix: "npm install",
                required_by: Some(("tsconfig.json", "extends")),
            }),
            projects: Some(Projects {
                config: "tsconfig.json",
                inferred: concat!(
                    r#"{"compilerOptions": {"allowJs": true}, "include": ["{dir}/**/*"], "exclude": ["#,
                    r#""{dir}/**/node_modules", "{dir}/**/bower_components", "{dir}/**/jspm_packages", "{dir}/**/dist", "#,
                    r#""{dir}/**/build", "{dir}/**/out", "{dir}/**/coverage"]}"#
                ),
                workspaces: &[
                    Workspace::Json("package.json", &["workspaces"]),
                    Workspace::Json("package.json", &["workspaces", "packages"]),
                    Workspace::YamlList("pnpm-workspace.yaml", "packages"),
                    Workspace::Json("lerna.json", &["packages"]),
                ],
            }),
            env: &[],
            fail_hint: None,
            not_on_windows: None,
        }),
    },
    Lang {
        name: "javascript",
        exts: &["js", "jsx", "mjs", "cjs"],
        skip_dirs: TS_SKIP,
        markers: &[],
        nested_units: false,
        units_of: Some("typescript"),
        stale: &[],
        scanner: ScanRow {
            quotes: TS_QUOTES,
            rules: TS_RULES,
            file_container: true,
            continued_by: TS_CONTINUED,
            header_continued: "",
            allman: false,
            not_names: &[],
            indent: false,
            once: &[],
        },
        truth: TS_TRUTH,
        fills: false,
        unindexed: None,
        packages: None,
        grpc: Some(Grpc {
            generated: &["*_pb.js", "*_grpc_pb.js", "*_grpc_web_pb.js"],
            stubs: &["{rpc}"],
            servers: &["{rpc}"],
            bases: &[],
        }),
        driver: None,
    },
    Lang {
        name: "csharp",
        exts: &["cs"],
        skip_dirs: CS_SKIP,
        // A solution claims its folder's projects; a project without one
        // is its own unit.
        markers: &["*.sln", "*.slnx", "*.csproj"],
        nested_units: false,
        units_of: None,
        stale: CS_STALE,
        scanner: ScanRow {
            quotes: CS_QUOTES,
            rules: CS_RULES,
            file_container: false,
            continued_by: CS_CONTINUED,
            header_continued: "",
            allman: true,
            not_names: CS_KEYWORDS,
            indent: false,
            once: &[],
        },
        truth: CS_TRUTH,
        fills: true,
        unindexed: None,
        packages: Some(Packages { managers: &["nuget"], fold: Fold::Exact }),
        grpc: Some(Grpc {
            generated: &["*Grpc.cs"],
            stubs: &["{Rpc}", "{Rpc}Async"],
            servers: &["{Rpc}"],
            bases: &["{S}Base"],
        }),
        driver: Some(Driver {
            tool: "scip-dotnet",
            node_package: None,
            find: &[Find::Path, Find::Home(".dotnet/tools")],
            version_arg: "--version",
            needs: &["dotnet"],
            // scip-dotnet restores the solution's packages and runs a
            // design-time build itself.
            args: &["index", "--output", "{out}", "{marker}"],
            env: &[("MSBUILDDISABLENODEREUSE", "1"), ("DOTNET_CLI_TELEMETRY_OPTOUT", "1"), ("DOTNET_NOLOGO", "1")],
            fail_hint: Some(Hint { file: "global.json", key: &["sdk", "version"], note: "global.json asks for .NET SDK {}" }),
            prereq: None,
            projects: None,
            not_on_windows: None,
        }),
    },
    Lang {
        name: "python",
        exts: &["py"],
        skip_dirs: PY_SKIP,
        // The last is the weakest: a folder of scripts with a requirements
        // file is a unit only when no project file claims it.
        markers: &["pyproject.toml", "setup.py", "setup.cfg", "requirements*.txt"],
        nested_units: false,
        units_of: None,
        stale: PY_STALE,
        scanner: ScanRow {
            quotes: PY_QUOTES,
            rules: PY_RULES,
            file_container: true,
            continued_by: "",
            header_continued: "",
            allman: false,
            not_names: PY_KEYWORDS,
            indent: true,
            once: &["field", "var"],
        },
        truth: r"^\s*(?:async\s+)?(?P<kw>def|class)\s+(?P<name>[A-Za-z_]\w*)",
        // Overloads and property setters, which SCIP leaves without a
        // definition, are filled in from the scanner.
        fills: true,
        unindexed: None,
        packages: Some(Packages { managers: &["python"], fold: Fold::Pep503 }),
        grpc: Some(Grpc {
            generated: &["*_pb2.py", "*_pb2_grpc.py", "*_pb2.pyi", "*_pb2_grpc.pyi"],
            stubs: &["{Rpc}"],
            servers: &["{Rpc}"],
            bases: &["{S}Servicer"],
        }),
        driver: Some(Driver {
            tool: "scip-python",
            node_package: Some("@sourcegraph/scip-python"),
            find: &[Find::Path, Find::NpmRoot],
            version_arg: "--version",
            // It also asks a Python interpreter for the environment's
            // packages; without one the project's own symbols still index.
            needs: &["node"],
            // A fixed version: scip-python takes the git commit by default
            // and fails outside a git checkout.
            args: &["index", ".", "--project-name", "{name}", "--project-version", "0", "--output", "{out}"],
            prereq: None,
            projects: None,
            env: &[],
            fail_hint: None,
            not_on_windows: Some("scip-python does not run on Windows: write the unit's index.scip with it in WSL or Docker"),
        }),
    },
    Lang {
        name: "java",
        exts: &["java"],
        skip_dirs: JAVA_SKIP,
        // A Gradle settings file claims its builds; a lone build file or a
        // Maven project (a parent `pom.xml` claims its modules) is a unit.
        markers: &["settings.gradle.kts", "settings.gradle", "build.gradle.kts", "build.gradle", "pom.xml"],
        nested_units: false,
        units_of: None,
        stale: JAVA_STALE,
        scanner: ScanRow {
            quotes: JAVA_QUOTES,
            rules: JAVA_RULES,
            file_container: false,
            continued_by: "",
            header_continued: "",
            allman: true,
            not_names: JAVA_KEYWORDS,
            indent: false,
            once: &[],
        },
        truth: JAVA_TRUTH,
        // Members of anonymous classes, which SCIP keeps local.
        fills: true,
        unindexed: None,
        packages: Some(Packages { managers: &["maven"], fold: Fold::Exact }),
        grpc: Some(Grpc {
            generated: &["*Grpc.java"],
            stubs: &["{rpc}"],
            servers: &["{rpc}"],
            bases: &["{S}ImplBase", "AsyncService"],
        }),
        driver: Some(Driver {
            tool: "scip-java",
            node_package: None,
            // Coursier's `cs install scip-java` folder (Linux, macOS).
            find: &[Find::Path, Find::Home(".local/share/coursier/bin"), Find::Home("Library/Application Support/Coursier/bin")],
            version_arg: "--version",
            // It compiles the unit through its own build (Maven, Gradle or
            // sbt, found by their files) with a compiler plugin added.
            needs: &["java"],
            args: &["index", "--output", "{out}"],
            prereq: None,
            projects: None,
            env: &[],
            fail_hint: None,
            not_on_windows: Some("scip-java does not run on Windows: write the unit's index.scip with it in WSL or Docker"),
        }),
    },
    Lang {
        name: "kotlin",
        exts: &["kt", "kts"],
        skip_dirs: JAVA_SKIP,
        // A Gradle or Maven unit holds Kotlin beside Java; scip-java reads
        // Kotlin only through semanticdb-kotlinc, which does not load in
        // the Kotlin 2 compiler, so a unit of Kotlin alone is scanned
        // ([`Lang::unindexed`]); a prebuilt file's Kotlin documents are
        // still read.
        markers: &[],
        nested_units: false,
        units_of: Some("java"),
        stale: &[],
        scanner: ScanRow {
            quotes: KT_QUOTES,
            rules: KT_RULES,
            file_container: false,
            continued_by: "",
            header_continued: KT_HEADER,
            allman: true,
            not_names: KT_KEYWORDS,
            indent: false,
            once: &[],
        },
        truth: KT_TRUTH,
        fills: false,
        unindexed: Some("scip-java reads Kotlin through semanticdb-kotlinc, which supports Kotlin 1.8 and 1.9 only"),
        packages: None,
        grpc: Some(Grpc {
            generated: &["*GrpcKt.kt"],
            stubs: &["{rpc}"],
            servers: &["{rpc}"],
            bases: &["{S}CoroutineImplBase"],
        }),
        driver: None,
    },
];

/// The row of a language by its name.
pub fn by_name(name: &str) -> Option<&'static Lang> {
    LANGS.iter().find(|l| l.name == name)
}

/// The row whose extension a file name or path has.
pub fn of_path(path: &str) -> Option<&'static Lang> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (_, ext) = name.rsplit_once('.')?;
    LANGS.iter().find(|l| l.exts.iter().any(|e| e.eq_ignore_ascii_case(ext)))
}

/// True when a `/`-separated path is a file a gRPC code generator writes
/// for its language ([`Grpc::generated`]).
pub fn is_generated(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    of_path(path).and_then(|l| l.grpc.as_ref()).is_some_and(|g| g.generated.iter().any(|p| name_matches(p, name)))
}

/// The row a stale-making file name (a marker, a manifest) belongs to.
pub fn of_stale_file(name: &str) -> Option<&'static Lang> {
    LANGS.iter().find(|l| l.stale.iter().any(|p| name_matches(p, name)))
}

/// True when a file name matches a marker or stale-file pattern: the name
/// itself, or a pattern with one `*` standing for any run of characters
/// (`requirements*.txt`), at least one when the pattern starts with it
/// (`*.csproj`: `.csproj` alone names no project).
pub fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((pre, post)) => {
            name.len() >= pre.len() + post.len() + usize::from(pre.is_empty()) && name.starts_with(pre) && name.ends_with(post)
        }
        None => name == pattern,
    }
}

impl Lang {
    /// Which of [`Lang::markers`] a file name is, by its index (the order
    /// is a preference: a C# solution before a project).
    pub fn marker_rank(&self, name: &str) -> Option<usize> {
        self.markers.iter().position(|p| name_matches(p, name))
    }
}

impl Lang {
    /// True when a folder name holds none of this language's files: a
    /// [`Lang::skip_dirs`] name, or a pattern whose `*` stands for any run
    /// of characters, none included (`_*` skips `_`).
    pub fn skips(&self, dir: &str) -> bool {
        self.skip_dirs.iter().any(|s| match s.split_once('*') {
            Some((pre, post)) => dir.len() >= pre.len() + post.len() && dir.starts_with(pre) && dir.ends_with(post),
            None => dir == *s,
        })
    }

    /// True when a `/`-separated path below a root is in a folder this
    /// language skips (its last component, the file, is not looked at).
    pub fn skips_path(&self, rel: &str) -> bool {
        let mut parts: Vec<&str> = rel.split('/').collect();
        parts.pop();
        parts.into_iter().any(|d| self.skips(d))
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The file names a program may have: on Windows its `.exe`, then the
/// `.cmd` shim a package manager writes (`npm.cmd`).
fn program_names(name: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    } else {
        vec![name.to_string()]
    }
}

/// The home folder.
fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// The first `PATH` folder holding a program (on Windows its `.exe`, else
/// its `.cmd`).
pub fn on_path(name: &str) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH")?).collect();
    program_names(name).iter().find_map(|n| dirs.iter().map(|d| d.join(n)).find(|p| p.is_file()))
}

impl Find {
    /// How the note names it.
    fn label(&self) -> String {
        match self {
            Find::Path => "PATH".to_string(),
            Find::Env(v) => v.to_string(),
            Find::EnvJoin(v, sub) => format!("{v}/{sub}"),
            Find::Home(sub) => format!("~/{sub}"),
            Find::NpmRoot => "npm root -g".to_string(),
        }
    }

    /// The folders it names.
    fn dirs(&self) -> Vec<PathBuf> {
        match self {
            Find::Path => std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default(),
            Find::Env(v) => std::env::var_os(v).filter(|d| !d.is_empty()).map(PathBuf::from).into_iter().collect(),
            Find::EnvJoin(v, sub) => std::env::var_os(v)
                .map(|p| std::env::split_paths(&p).filter(|d| !d.as_os_str().is_empty()).map(|d| d.join(sub)).collect())
                .unwrap_or_default(),
            Find::Home(sub) => home().map(|h| h.join(sub)).into_iter().collect(),
            Find::NpmRoot => crate::code_index::npm_root().into_iter().collect(),
        }
    }
}

impl Driver {
    /// Every place this driver's binary (or, for a Node tool, its package
    /// folder) may be, in search order. `npm root -g` is asked only when
    /// no place before it holds anything.
    pub fn candidates(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for f in self.find {
            if matches!(f, Find::NpmRoot) && out.iter().any(|p| p.exists()) {
                break;
            }
            out.extend(self.candidates_in(f));
        }
        out
    }

    fn candidates_in(&self, f: &Find) -> Vec<PathBuf> {
        let dirs = f.dirs();
        let Some(pkg) = self.node_package else {
            return dirs.iter().map(|d| d.join(exe(self.tool))).collect();
        };
        if matches!(f, Find::NpmRoot) {
            return dirs.iter().map(|d| d.join(pkg)).collect();
        }
        // A folder holding the tool's shim: npm's global prefix on Windows
        // (packages in `node_modules` beside it), its `bin` elsewhere
        // (`../lib/node_modules`), or a project's `node_modules/.bin`.
        let shims = [self.tool.to_string(), format!("{}.cmd", self.tool)];
        let mut out = Vec::new();
        for d in dirs.iter().filter(|d| shims.iter().any(|s| d.join(s).is_file())) {
            out.push(d.join("node_modules").join(pkg));
            if let Some(up) = d.parent() {
                out.push(up.join("lib").join("node_modules").join(pkg));
                if d.file_name().is_some_and(|n| n == ".bin") {
                    out.push(up.join(pkg));
                }
            }
        }
        out
    }

    /// Finds the tool: the first candidate that exists and answers
    /// [`Driver::version_arg`], with every program it needs on `PATH`.
    ///
    /// # Returns
    ///
    /// How to start it, or the note a scanned unit carries (`scip-go not
    /// found on PATH, GOBIN, GOPATH/bin, ~/go/bin`).
    pub fn find(&self) -> Result<Launch, String> {
        if let Some(why) = self.not_on_windows.filter(|_| cfg!(windows)) {
            return Err(why.to_string());
        }
        if let Some(missing) = self.needs.iter().find(|n| on_path(n).is_none()) {
            return Err(format!("{} needs {missing} on PATH", self.tool));
        }
        for p in self.candidates() {
            let launch = match self.node_package {
                Some(_) if p.is_dir() => match package_bin(&p) {
                    Some(script) => node_launch(script),
                    None => continue,
                },
                None if p.is_file() => Launch::program(p),
                _ => continue,
            };
            if crate::code_index::answers(&launch, self.version_arg) {
                return Ok(launch);
            }
        }
        let places: Vec<String> = self.find.iter().map(Find::label).collect();
        Err(format!("{} not found on {}", self.tool, places.join(", ")))
    }

    /// The arguments for one run, writing to `out`, of the unit whose
    /// marker file is `marker` and whose folder is named `name`.
    pub fn args_for(&self, out: &Path, marker: Option<&str>, name: &str) -> Vec<std::ffi::OsString> {
        self.args
            .iter()
            .filter_map(|a| match *a {
                "{out}" => Some(out.as_os_str().to_os_string()),
                "{marker}" => marker.map(std::ffi::OsString::from),
                "{name}" => Some(std::ffi::OsString::from(name)),
                _ => Some(std::ffi::OsString::from(a)),
            })
            .collect()
    }
}

impl Prereq {
    /// Why a unit's folder cannot be indexed yet, if it cannot: its
    /// manifest lists entries for [`Prereq::dir`] and the folder is absent.
    pub fn unmet(&self, unit: &Path) -> Option<String> {
        if unit.join(self.dir).is_dir() {
            return None;
        }
        let text = std::fs::read_to_string(unit.join(self.manifest)).ok()?;
        let v = crate::json::parse(&text).ok()?;
        let needed = self.keys.iter().any(|k| v.get(*k).and_then(|x| x.as_object()).is_some_and(|o| !o.is_empty()));
        needed.then(|| format!("no {}: run {}", self.dir, self.fix))
    }

    /// True when one of `configs` (the unit's files named as
    /// [`Prereq::required_by`] says) names a package under its key, so the
    /// tool cannot run while [`Prereq::unmet`] holds.
    pub fn blocks(&self, configs: &[PathBuf]) -> bool {
        let Some((_, key)) = self.required_by else { return false };
        configs.iter().any(|f| std::fs::read_to_string(f).is_ok_and(|t| names_package(&t, key)))
    }
}

/// The first `"key": VALUE` whose value is a string or an array (up to its
/// first `]`), as a span of `text` (was the pattern
/// `"{key}"\s*:\s*(\[[^\]]*\]|"[^"]*")`, `key` literal).
pub(crate) fn key_value(text: &str, key: &str) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let mut q = 0;
    // The key between quotes. Occurrences can overlap (`"a"a"`): step one
    // byte, not one match.
    while let Some(k) = crate::lex::find_str(text, q, key) {
        q = k + 1;
        if k == 0 || b[k - 1] != b'"' || b.get(k + key.len()) != Some(&b'"') {
            continue;
        }
        let Some(c) = crate::lex::lit(text, crate::lex::spaces(text, k + key.len() + 1), ":") else { continue };
        let v = crate::lex::spaces(text, c);
        let close = match text.as_bytes().get(v) {
            Some(b'[') => ']',
            Some(b'"') => '"',
            _ => continue,
        };
        if let Some(k) = crate::lex::find_byte(text, v + 1, close as u8) {
            return Some((v, k + 1));
        }
    }
    None
}

#[cfg(test)]
pub(crate) fn lex_sites() -> Vec<crate::lex::props::Site> {
    use crate::lex::props::{Mode, Site};
    vec![Site {
        name: "langs package key",
        pattern: r#""extends"\s*:\s*(?P<v>\[[^\]]*\]|"[^"]*")"#.into(),
        tokens: &["\"extends\"", "\"", "extends", ":", " ", "\n", "[", "]", ",", "@a/b", "./x.json", "{", "}", "//"],
        examples: &["{\n  \"extends\": \"@tsconfig/node18\",\n}", "{\"extends\" : [\"./a.json\", \"b\"]}"],
        mode: Mode::One(|s| key_value(s, "extends").map(|(a, b)| {
            // Group 0 starts at the key's opening quote.
            let k = s[..a].rfind("\"extends\"").unwrap_or(0);
            vec![Some((k, b)), Some((a, b))]
        })),
    }]
}

/// True when a JSON (or JSON with comments) text's `key` holds a string, or
/// an array of strings, one of which names a package: not `./`, `../`, an
/// absolute path or a drive path. Read by pattern, not parsed, since a
/// `tsconfig.json` may hold comments and trailing commas.
pub fn names_package(text: &str, key: &str) -> bool {
    let Some((a, b)) = key_value(text, key) else { return false };
    let v = &text[a..b];
    v.split('"').skip(1).step_by(2).any(|s| {
        let s = s.trim();
        !s.is_empty() && !s.starts_with('.') && !s.starts_with('/') && !s.starts_with('\\') && !(s.len() > 1 && s.as_bytes()[1] == b':')
    })
}

impl Workspace {
    /// The member globs a unit folder's file lists, if it has the file.
    pub fn globs(&self, unit: &Path) -> Vec<String> {
        match self {
            Workspace::Json(file, keys) => {
                let Ok(text) = std::fs::read_to_string(unit.join(file)) else { return Vec::new() };
                let Ok(v) = crate::json::parse(&text) else { return Vec::new() };
                let mut at = Some(&v);
                for k in *keys {
                    at = at.and_then(|x| x.get(*k));
                }
                at.and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
            }
            Workspace::YamlList(file, key) => {
                let Ok(text) = std::fs::read_to_string(unit.join(file)) else { return Vec::new() };
                yaml_list(&text, key)
            }
        }
    }
}

/// The strings of a top-level YAML key holding a block list (`key:` then
/// `- item` lines) or a flow list (`key: [a, b]`); quotes are taken off.
pub fn yaml_list(text: &str, key: &str) -> Vec<String> {
    let unquote = |s: &str| s.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let body = line.split(" #").next().unwrap_or(line);
        if body.trim().is_empty() || body.trim_start().starts_with('#') {
            continue;
        }
        if !body.starts_with([' ', '\t', '-']) {
            inside = false;
            if let Some(rest) = body.strip_prefix(key).and_then(|r| r.trim_start().strip_prefix(':')) {
                let rest = rest.trim();
                if let Some(flow) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    out.extend(flow.split(',').map(unquote).filter(|s| !s.is_empty()));
                } else {
                    inside = rest.is_empty();
                }
            }
            continue;
        }
        if inside {
            if let Some(item) = body.trim_start().strip_prefix('-') {
                out.push(unquote(item));
            }
        }
    }
    out
}

impl Projects {
    /// The projects of a unit, relative to its folder (`.` for the folder
    /// itself): the folder when it has [`Projects::config`], and the
    /// workspace members its files declare; the folder alone when it
    /// declares none. A member is a folder below the unit holding a
    /// `package.json` that a member glob matches (a `!` glob takes folders
    /// out).
    ///
    /// # Arguments
    ///
    /// * `unit` - the unit's folder.
    /// * `manifests` - the folders below it holding a `package.json`,
    ///   `/`-separated and relative to it.
    pub fn of_unit(&self, unit: &Path, manifests: &[String]) -> Vec<String> {
        let globs: Vec<String> = self.workspaces.iter().flat_map(|w| w.globs(unit)).collect();
        if globs.is_empty() {
            return vec![".".to_string()];
        }
        let norm = |g: &str| g.trim().trim_start_matches("./").trim_end_matches('/').to_string();
        let (neg, pos): (Vec<&String>, Vec<&String>) = globs.iter().partition(|g| g.trim_start().starts_with('!'));
        let neg: Vec<String> = neg.iter().map(|g| norm(g.trim_start().trim_start_matches('!'))).collect();
        let pos: Vec<String> = pos.iter().map(|g| norm(g)).collect();
        // A glob without a `/` names a folder of the unit itself (the glob
        // module would match it against any path's last folder).
        let hit = |g: &str, d: &str| (g.contains('/') || !d.contains('/')) && crate::glob::matches(g, d);
        let mut out: Vec<String> = Vec::new();
        if unit.join(self.config).is_file() {
            out.push(".".to_string());
        }
        for d in manifests {
            if !d.is_empty() && pos.iter().any(|g| hit(g, d)) && !neg.iter().any(|g| hit(g, d)) && !out.contains(d) {
                out.push(d.clone());
            }
        }
        if out.is_empty() {
            out.push(".".to_string());
        }
        out
    }

    /// The inferred configuration for a project folder ([`Projects::inferred`]).
    pub fn inferred_for(&self, dir: &Path) -> String {
        let d = dir.to_string_lossy().replace('\\', "/").replace('"', "\\\"");
        self.inferred.replace("{dir}", &d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_keys_fold_as_their_row_says() {
        assert_eq!(package_key("gomod", "example.com/lib").as_deref(), Some("gomod example.com/lib"));
        assert_eq!(package_key("npm", "@demo/shared").as_deref(), Some("npm @demo/shared"));
        // Python names are PEP 503 normalised; a manager no row names
        // compares as written.
        assert_eq!(package_key("python", "Foo_Bar..baz").as_deref(), Some("python foo-bar-baz"));
        assert_eq!(package_key("cargo", "My_Crate").as_deref(), Some("cargo My_Crate"));
        // No name: never linked.
        assert_eq!(package_key("nuget", "."), None);
        assert_eq!(package_key("maven", ""), None);
    }

    #[test]
    fn rows_resolve_by_name_extension_and_marker() {
        assert_eq!(by_name("go").map(|l| l.name), Some("go"));
        assert_eq!(of_path("a/b/main.go").map(|l| l.name), Some("go"));
        assert_eq!(of_path("a\\B.GO").map(|l| l.name), Some("go"));
        assert!(of_path("lib.rs").is_none() && of_path("Makefile").is_none());
        assert_eq!(of_stale_file("go.sum").map(|l| l.name), Some("go"));
        assert!(of_stale_file("Cargo.toml").is_none());
    }

    #[test]
    fn skipped_folders_take_prefixes() {
        let go = by_name("go").unwrap();
        assert!(go.skips("vendor") && go.skips("testdata") && go.skips("_old"));
        assert!(!go.skips("vendors") && !go.skips("src"));
        assert!(go.skips_path("x/vendor/y/a.go") && !go.skips_path("vendor.go") && !go.skips_path("a/b.go"));
    }

    #[test]
    fn every_pattern_compiles() {
        for l in LANGS {
            crate::regex::Regex::new(l.truth).unwrap();
            for r in l.scanner.rules {
                crate::regex::Regex::new(r.re).unwrap_or_else(|e| panic!("{}: {}: {e}", l.name, r.re));
            }
        }
    }

    #[test]
    fn a_missing_driver_says_where_it_looked() {
        let d = Driver {
            tool: "atlas-no-such-indexer",
            node_package: None,
            find: &[Find::Path, Find::EnvJoin("GOPATH", "bin")],
            version_arg: "--version",
            needs: &[],
            args: &["-o", "{out}"],
            prereq: None,
            projects: None,
            env: &[],
            fail_hint: None,
            not_on_windows: None,
        };
        assert_eq!(d.find().unwrap_err(), "atlas-no-such-indexer not found on PATH, GOPATH/bin");
        assert_eq!(d.args_for(Path::new("x.scip"), Some("a.sln"), "a"), vec![std::ffi::OsString::from("-o"), "x.scip".into()]);
        let cs = by_name("csharp").unwrap().driver.as_ref().unwrap();
        let args = |m| cs.args_for(Path::new("o.scip"), m, "o").iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(args(Some("App.sln")), ["index", "--output", "o.scip", "App.sln"]);
        assert_eq!(args(None), ["index", "--output", "o.scip"]);
    }

    #[test]
    fn typescript_and_javascript_share_typescript_units() {
        assert_eq!(of_path("src/App.TSX").map(|l| l.name), Some("typescript"));
        assert_eq!(of_path("server.cjs").map(|l| l.name), Some("javascript"));
        let js = by_name("javascript").unwrap();
        assert_eq!((js.units_of, js.markers.len(), js.driver.is_none()), (Some("typescript"), 0, true));
        assert_eq!(of_stale_file("pnpm-lock.yaml").map(|l| l.name), Some("typescript"));
        let ts = by_name("typescript").unwrap();
        assert!(ts.skips_path("dist/a.ts") && ts.skips_path("x/coverage/b.ts") && !ts.skips_path("src/a.ts"));
    }

    #[test]
    fn csharp_markers_are_patterns_and_a_solution_comes_first() {
        let cs = by_name("csharp").unwrap();
        assert_eq!(of_path("src/App/Program.CS").map(|l| l.name), Some("csharp"));
        for f in ["App.csproj", "App.sln", "Directory.Build.props", "global.json"] {
            assert_eq!(of_stale_file(f).map(|l| l.name), Some("csharp"), "{f}");
        }
        assert!(of_stale_file(".csproj").is_none(), "a pattern needs a name before its suffix");
        assert!(name_matches("go.mod", "go.mod") && !name_matches("go.mod", "xgo.mod"));
        let ranks: Vec<Option<usize>> = ["App.sln", "App.slnx", "App.csproj", "App.cs"].iter().map(|n| cs.marker_rank(n)).collect();
        assert_eq!(ranks, [Some(0), Some(1), Some(2), None]);
        assert!(cs.skips_path("src/App/obj/Debug/x.cs") && cs.skips_path("bin/x.cs") && !cs.skips_path("src/App/x.cs"));
        // The SDK a global.json at or above the unit asks for, up to the root.
        let t = std::env::temp_dir().join(format!("atlas-langs-cs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/App")).unwrap();
        std::fs::write(t.join("global.json"), r#"{"sdk": {"version": "8.0.100"}}"#).unwrap();
        let h = cs.driver.as_ref().unwrap().fail_hint.as_ref().unwrap();
        assert_eq!(h.for_unit(&t, &t.join("src/App")).as_deref(), Some("global.json asks for .NET SDK 8.0.100"));
        assert_eq!(h.for_unit(&t.join("src"), &t.join("src/App")), None, "not above the root");
        std::fs::write(t.join("src/global.json"), r#"{"msbuild-sdks": {}}"#).unwrap();
        assert_eq!(h.for_unit(&t, &t.join("src/App")), None, "the nearest file decides");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn workspace_lists_are_read_from_json_and_yaml() {
        let t = std::env::temp_dir().join(format!("atlas-langs-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("package.json"), r#"{"workspaces": {"packages": ["apps/*", "!apps/old"]}}"#).unwrap();
        std::fs::write(t.join("pnpm-workspace.yaml"), "# members\npackages:\n  - 'libs/**'\n  - \"tools\" # one\ncatalog:\n  - nope\n")
            .unwrap();
        let p = by_name("typescript").unwrap().driver.as_ref().unwrap().projects.as_ref().unwrap();
        let manifests: Vec<String> =
            ["", "apps/web", "apps/old", "libs/a/b", "tools", "tools/x", "docs"].iter().map(|s| s.to_string()).collect();
        assert_eq!(p.of_unit(&t, &manifests), ["apps/web", "libs/a/b", "tools"]);
        std::fs::write(t.join("tsconfig.json"), "{}").unwrap();
        assert_eq!(p.of_unit(&t, &manifests)[0], ".", "the root's own project comes first");
        assert_eq!(yaml_list("packages: [a, 'b/*']\n", "packages"), ["a", "b/*"]);
        assert!(p.inferred_for(Path::new(r"C:\w\cart")).contains(r#""include": ["C:/w/cart/**/*"]"#));
        // The prerequisite: a declared dependency and no node_modules.
        let pre = by_name("typescript").unwrap().driver.as_ref().unwrap().prereq.as_ref().unwrap();
        std::fs::write(t.join("package.json"), r#"{"dependencies": {}, "devDependencies": {"x": "1"}}"#).unwrap();
        assert_eq!(pre.unmet(&t).as_deref(), Some("no node_modules: run npm install"));
        std::fs::write(t.join("package.json"), r#"{"dependencies": {}}"#).unwrap();
        assert_eq!(pre.unmet(&t), None, "nothing to install");
        std::fs::write(t.join("package.json"), r#"{"dependencies": {"x": "1"}}"#).unwrap();
        std::fs::create_dir_all(t.join("node_modules")).unwrap();
        assert_eq!(pre.unmet(&t), None);
        // Only a configuration extending a package needs the folder.
        assert!(names_package("{\n  // base\n  \"extends\": \"@sindresorhus/tsconfig\",\n}", "extends"));
        assert!(names_package(r#"{"extends": ["./base.json", "@tsconfig/node18"]}"#, "extends"));
        assert!(!names_package(r#"{"extends": "./tsconfig.base.json"}"#, "extends"));
        assert!(!names_package(r#"{"extends": ["../base.json", "/abs/x.json", "C:/x.json"]}"#, "extends"));
        assert!(!names_package(r#"{"compilerOptions": {"strict": true}}"#, "extends"));
        std::fs::write(t.join("tsconfig.json"), r#"{"extends": "@tsconfig/node18/tsconfig.json"}"#).unwrap();
        std::fs::write(t.join("rel.json"), r#"{"extends": "./tsconfig.json"}"#).unwrap();
        assert!(pre.blocks(&[t.join("rel.json"), t.join("tsconfig.json")]));
        assert!(!pre.blocks(&[t.join("rel.json"), t.join("missing.json")]));
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn a_node_package_is_run_through_its_bin_script() {
        let t = std::env::temp_dir().join(format!("atlas-langs-node-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let pkg = t.join("node_modules").join("@sourcegraph").join("scip-typescript");
        std::fs::create_dir_all(pkg.join("dist")).unwrap();
        std::fs::write(pkg.join("package.json"), r#"{"name": "@sourcegraph/scip-typescript", "bin": "dist/main.js"}"#).unwrap();
        std::fs::write(pkg.join("dist/main.js"), "").unwrap();
        let l = Launch::configured(&pkg);
        assert_eq!(l.pre, vec![pkg.join("dist/main.js").into_os_string()]);
        assert!(l.program.to_string_lossy().contains("node"));
        assert_eq!(Launch::configured(&pkg.join("dist/main.js")).pre.len(), 1);
        assert_eq!(Launch::configured(Path::new("tools/scip-go")), Launch::program(PathBuf::from("tools/scip-go")));
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn python_markers_skipped_folders_and_driver() {
        let py = by_name("python").unwrap();
        assert_eq!(of_path("src/app/views.py").map(|l| l.name), Some("python"));
        for f in ["pyproject.toml", "setup.cfg", "requirements.txt", "requirements-dev.txt", "poetry.lock", "uv.lock"] {
            assert_eq!(of_stale_file(f).map(|l| l.name), Some("python"), "{f}");
        }
        assert!(!name_matches("requirements*.txt", "requirements.txt.bak") && !name_matches("requirements*.txt", "requirementstxt"));
        let ranks: Vec<Option<usize>> =
            ["pyproject.toml", "setup.py", "setup.cfg", "requirements-test.txt", "app.py"].iter().map(|n| py.marker_rank(n)).collect();
        assert_eq!(ranks, [Some(0), Some(1), Some(2), Some(3), None], "a requirements file is the weakest");
        assert!(py.skips_path("venv/lib/x.py") && py.skips_path("a/__pycache__/x.py") && py.skips_path("src/demo.egg-info/x.py"));
        assert!(py.skips_path("build/lib/x.py") && !py.skips_path("src/builder/x.py") && !py.skips_path("egg-info/x.py"));
        let d = py.driver.as_ref().unwrap();
        let args: Vec<String> = d.args_for(Path::new("o.scip"), None, "shop").iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["index", ".", "--project-name", "shop", "--project-version", "0", "--output", "o.scip"]);
        if cfg!(windows) {
            assert_eq!(d.find().unwrap_err(), d.not_on_windows.unwrap(), "not looked for on Windows");
        }
    }

    #[test]
    fn java_and_kotlin_share_java_units() {
        let java = by_name("java").unwrap();
        let kt = by_name("kotlin").unwrap();
        assert_eq!(of_path("src/main/java/a/App.java").map(|l| l.name), Some("java"));
        assert_eq!(of_path("src/main/kotlin/App.kt").map(|l| l.name), Some("kotlin"));
        assert_eq!(of_path("build.gradle.kts").map(|l| l.name), Some("kotlin"), "a Kotlin build script is Kotlin source");
        for f in ["pom.xml", "build.gradle", "settings.gradle.kts", "gradle.properties", "libs.versions.toml"] {
            assert_eq!(of_stale_file(f).map(|l| l.name), Some("java"), "{f}");
        }
        let ranks: Vec<Option<usize>> =
            ["settings.gradle.kts", "settings.gradle", "build.gradle.kts", "build.gradle", "pom.xml", "App.java"].iter().map(|n| java.marker_rank(n)).collect();
        assert_eq!(ranks, [Some(0), Some(1), Some(2), Some(3), Some(4), None]);
        assert!(java.skips_path("build/generated/A.java") && java.skips_path("app/out/A.java") && !java.skips_path("src/builder/A.java"));
        assert_eq!((kt.units_of, kt.markers.len(), kt.driver.is_none(), kt.unindexed.is_some()), (Some("java"), 0, true, true));
        let d = java.driver.as_ref().unwrap();
        let args: Vec<String> = d.args_for(Path::new("o.scip"), Some("pom.xml"), "app").iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["index", "--output", "o.scip"]);
        if cfg!(windows) {
            assert_eq!(d.find().unwrap_err(), d.not_on_windows.unwrap(), "not looked for on Windows");
        }
    }

    #[test]
    fn jvm_truth_counts_declarations_not_locals() {
        let truth = |lang: &str, line: &str| {
            let re = crate::regex::Regex::new(by_name(lang).unwrap().truth).unwrap();
            re.captures(line).and_then(|c| c.name("name").map(|m| m.as_str().to_string()))
        };
        assert_eq!(truth("java", "public final class Gson implements Foo {").as_deref(), Some("Gson"));
        assert_eq!(truth("java", "  @Override public <T> TypeAdapter<T> create(Gson gson) {").as_deref(), Some("create"));
        assert_eq!(truth("java", "  private static final long serialVersionUID = 1L;").as_deref(), Some("serialVersionUID"));
        assert_eq!(truth("java", "  public Gson() {").as_deref(), Some("Gson"));
        assert_eq!(truth("java", "  @interface Since {").as_deref(), Some("Since"));
        assert_eq!(truth("java", "    int count = 0;"), None, "no modifier: not counted");
        assert_eq!(truth("kotlin", "public data class Point(val x: Int)").as_deref(), Some("Point"));
        assert_eq!(truth("kotlin", "  override fun <T> List<T>.second(): T = this[1]").as_deref(), Some("second"));
        assert_eq!(truth("kotlin", "val TOP = 1").as_deref(), Some("TOP"));
        assert_eq!(truth("kotlin", "  private val cache = mutableMapOf<String, Int>()").as_deref(), Some("cache"));
        assert_eq!(truth("kotlin", "    val local = 1"), None, "a local looks like an unmodified member");
    }
}
