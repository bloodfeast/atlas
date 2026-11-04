//! The configuration file: vaults, read-only sources and identifier
//! patterns, given with `--config PATH`.
//!
//! The format is JSON. It is read by hand from a parsed JSON value (no
//! derive), and every object's keys are checked, so a misspelt key is an
//! error rather than silently ignored. Patterns are regexes; `[0-9]` reads
//! better in JSON than `\\d`. Relative paths are taken from the
//! configuration file's folder.
//!
//! ```json
//! {
//!   "db": "state/atlas.sqlite",
//!   "vaults": [{"name": "notes", "path": "../Notes"}],
//!   "sources": [
//!     {"name": "handbook", "root": "../docs", "include": ["*.md"],
//!      "chunk": {"top_level_items": true}},
//!     {"name": "memory", "root": "../memory", "include": ["*.md"],
//!      "id_keys": ["name"], "names_kind": "memory"}
//!   ],
//!   "ids": [
//!     {"name": "ticket", "regex": "\\bTK-[0-9]{3}\\b"},
//!     {"name": "pr", "regex": "(?:^|[^A-Za-z0-9_&/])(?P<id>#[0-9]{1,5})\\b"}
//!   ],
//!   "wikilinks": true,
//!   "refresh_limit": 64
//! }
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use crate::json::{Map, Value};

use crate::chunk::ChunkRule;
use crate::error::{Error, Result};
use crate::registry::PatternSpec;

/// The largest configuration file read, in bytes.
pub const MAX_CONFIG: u64 = 1 << 20;

/// The default for [`Config::refresh_limit`].
pub const DEFAULT_REFRESH_LIMIT: usize = 64;

/// The default for [`Config::rescan_interval_s`]: how often a watched root
/// is walked anyway, as a safety net under change notification.
pub const DEFAULT_RESCAN_INTERVAL_S: u64 = 30;

/// A vault in the configuration.
#[derive(Debug, Clone)]
pub struct VaultSpec {
    /// Short name, as `--vault NAME=PATH`.
    pub name: String,
    /// The vault folder.
    pub path: PathBuf,
}

/// A read-only source in the configuration.
#[derive(Debug, Clone)]
pub struct SourceSpec {
    /// Short name; unique across vaults and sources.
    pub name: String,
    /// The folder the globs are relative to.
    pub root: PathBuf,
    /// Globs of the files to index (see [`crate::sources::Source::include`]).
    pub include: Vec<String>,
    /// Globs that drop matched files.
    pub exclude: Vec<String>,
    /// How files are cut into sections.
    pub chunk: ChunkRule,
    /// Frontmatter keys naming the identifier a file defines (default
    /// `["id"]`).
    pub id_keys: Option<Vec<String>>,
    /// Make every file stem an identifier of this kind.
    pub names_kind: Option<String>,
}

/// A code root: a folder whose Rust (through `rust-analyzer scip`) and
/// WGSL (through [`crate::wgsl`]) are indexed for the `symbol` tool.
#[derive(Debug, Clone)]
pub struct CodeRootSpec {
    /// Short name; unique across vaults, sources and code roots.
    pub name: String,
    /// The folder: a Cargo workspace or package root for Rust, any folder
    /// for WGSL.
    pub root: PathBuf,
    /// Let rust-analyzer run this root's build scripts and load its proc
    /// macros (into a `rust-analyzer` folder of the workspace's target
    /// directory, so the project's own builds are not disturbed). Off by
    /// default: off, no compiler runs and derive-generated items are not
    /// indexed.
    pub build_scripts: bool,
    /// Its prebuilt SCIP files: `true` or absent, `index.scip` in a unit's
    /// folder; `false`, none; a path, the root folder's file; an object
    /// from unit folder (below the root; `""` or `"."` the root) to a path
    /// or `false`, other folders following the convention. Paths are
    /// relative to the configuration file.
    pub scip: crate::code::Prebuilt,
}

/// `code.indexers` as an object: each named indexer's path, or `None` when
/// it is turned off.
pub type IndexerPaths = Vec<(String, Option<PathBuf>)>;

/// The code index settings (see [`crate::code`]).
#[derive(Debug, Clone, Default)]
pub struct CodeSpec {
    /// The roots indexed.
    pub roots: Vec<CodeRootSpec>,
    /// The rust-analyzer binary; found on `PATH` or through `rustup which`
    /// when absent.
    pub rust_analyzer: Option<PathBuf>,
    /// The SCIP indexers of the language table ([`crate::langs`]): `None`
    /// (the key absent, or `true`) finds each as
    /// [`crate::langs::Driver::find`] does; `Some(None)` (`false`) runs
    /// none; `Some(Some(map))` names tools: a path runs that binary (or Node
    /// package folder, or script), `false` turns the tool off, and a tool
    /// not named is found as usual.
    pub indexers: Option<Option<IndexerPaths>>,
    /// The longest a rust-analyzer run may take before it is killed
    /// (default [`crate::code::DEFAULT_TIMEOUT_S`]).
    pub timeout_s: Option<usize>,
    /// How long a root must go without a change before it is re-indexed
    /// (default [`crate::code::DEFAULT_DEBOUNCE_S`]).
    pub debounce_s: Option<usize>,
    /// A lock file (a shared build or GPU lock): while it exists, background
    /// re-indexing waits. Default none. Re-indexing also waits while any
    /// `cargo` process that is not an ancestor of atlas runs.
    pub defer_lock: Option<PathBuf>,
    /// Where `.scip` files and the indexer log go (default: a `code` folder
    /// beside the database).
    pub dir: Option<PathBuf>,
}

/// Semantic search settings (see [`crate::embed`]).
#[derive(Debug, Clone, Default)]
pub struct SemanticSpec {
    /// The packed model file (default: `models/embed.atm` beside the
    /// database). Semantic search is off while the file is absent.
    pub model: Option<PathBuf>,
    /// Encoder threads for embedding (default
    /// [`DEFAULT_EMBED_THREADS`]).
    pub threads: Option<usize>,
    /// The id in `models.files` whose file is the model when `model` is
    /// absent (default [`DEFAULT_MODEL_ID`]).
    pub model_id: Option<String>,
}

/// The model id semantic search and the recall gate use by default.
pub const DEFAULT_MODEL_ID: &str = "embed";

/// One file of the model list (see [`crate::models`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFileSpec {
    /// What `semantic.model_id` names it by.
    pub id: String,
    /// The file name it is stored under in the models folder.
    pub file: String,
    /// Where it is downloaded from: `https`, or `http` to the loopback
    /// address.
    pub url: String,
    /// Its SHA-256, lowercase hex.
    pub sha256: String,
    /// Its size in bytes.
    pub size: u64,
}

/// The model list and download settings.
#[derive(Debug, Clone, Default)]
pub struct ModelsSpec {
    /// The folder the files are kept in (default `models/` in the user's
    /// data folder).
    pub dir: Option<PathBuf>,
    /// Download missing files in the background when the server starts
    /// (default true). `atlas --setup` downloads them either way.
    pub auto_download: Option<bool>,
    /// The `curl` binary (default: the one Windows ships, else `curl` on
    /// `PATH`).
    pub curl: Option<PathBuf>,
    /// The files.
    pub files: Vec<ModelFileSpec>,
}

/// The release check at session start (see `crate::update`).
#[derive(Debug, Clone, Default)]
pub struct UpdateSpec {
    /// Check for a newer signed release at session start (default true;
    /// `atlas settings off update` writes false). Builds from source never
    /// check.
    pub enabled: Option<bool>,
    /// A pinned version: when set, atlas stays on what is installed.
    pub pin: Option<String>,
    /// The manifest URL (default `crate::update::DEFAULT_URL`): `https`, or
    /// plain `http` to the loopback address. The signature check does not
    /// depend on where the manifest comes from.
    pub url: Option<String>,
}

/// One export of the memory store (see `crate::memory::export`).
#[derive(Debug, Clone, Default)]
pub struct ExportCfg {
    /// Its name, unique among exports.
    pub name: String,
    /// `memory`, `items` or `headings`.
    pub shape: String,
    /// The file, or the folder for `memory`, relative to `export_dir`.
    pub path: String,
    /// The index file inside a memory folder (default `MEMORY.md`).
    pub index: Option<String>,
    /// New handoff records are added to this export.
    pub handoffs: bool,
    /// Cut each index line to this many characters (default: no cut).
    pub index_cap: Option<usize>,
    /// Generate the memory index as a clustered summary under these topics
    /// (default: the line-per-memory index, byte-exact to its import).
    pub index_topics: Option<Vec<TopicCfg>>,
}

/// One topic header of a clustered memory index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TopicCfg {
    /// The header.
    pub name: String,
    /// Lowercase words; a memory is listed under the first topic with a word
    /// in its slug, else the first with a word in its description.
    pub words: Vec<String>,
}

/// The memory store's settings.
#[derive(Debug, Clone, Default)]
pub struct MemorySpec {
    /// The store file (default `memory.sqlite` beside the database).
    pub store: Option<PathBuf>,
    /// The private folder exports are written to (none: no exports).
    pub export_dir: Option<PathBuf>,
    /// The exports.
    pub exports: Vec<ExportCfg>,
    /// Hours between automatic snapshots (default 24).
    pub snapshot_hours: Option<usize>,
    /// The snapshot folder (default `snapshots/` beside the store).
    pub snapshot_dir: Option<PathBuf>,
    /// Snapshots are pruned oldest first past this many MB (default 1024).
    pub snapshot_cap_mb: Option<usize>,
    /// The rulebook a brief would tell an agent to read in full without
    /// atlas; its size at each subagent start is the "brief without atlas"
    /// reference (default: the first `items` export).
    pub brief_reference: Option<PathBuf>,
    /// Memory folders whose memories marked `scope: user` the `user` export
    /// also imports (the other projects' folders; set by
    /// [`crate::project::resolve`], not read from the file).
    pub user_scope_from: Vec<PathBuf>,
    /// The user layer's memory folder, where a memory written with `scope:
    /// user` goes (set by [`crate::project::resolve`], not read from the
    /// file).
    pub user_memory: Option<PathBuf>,
    /// The user-level state folder (the asked-once offer state and the
    /// routing switch, shared by every project; set by
    /// [`crate::project::resolve`], not read from the file).
    pub user_state: Option<PathBuf>,
    /// Index the session transcripts as an opt-in source (default false;
    /// see `crate::transcripts`).
    pub transcripts: Option<bool>,
    /// The memory index as a tree (see `crate::memory::tree`); `None` keeps
    /// the index as it is.
    pub tree: Option<crate::memory::tree::TreeSpec>,
}

/// Parses `memory.tree`: every key optional, defaults from
/// [`crate::memory::tree::TreeSpec::new`].
fn parse_tree(tv: &Value, exports: &[ExportCfg], per_project: bool) -> Result<crate::memory::tree::TreeSpec> {
    use crate::memory::tree::{BranchSpec, TreeSpec};
    let f = Fields::new("memory.tree", tv, &["export", "hub_dir", "hub_max", "hub_target", "hub_min", "line_max", "pin_body_chars", "line_format", "name_stop", "keyword_weight", "pins", "branches"])?;
    // A configuration with projects keeps a top-level tree as every
    // project's default: each project points it at its own memory export
    // (see `crate::project::resolve`), so no export is checked here.
    let export = match f.string("export")? {
        Some(e) => e,
        None if per_project => String::new(),
        None => exports.iter().find(|e| e.shape == "memory").map(|e| e.name.clone()).ok_or_else(|| f.bad("export", "a memory export (none is configured)"))?,
    };
    if !per_project && !exports.iter().any(|e| e.name == export && e.shape == "memory") {
        return Err(f.bad("export", "the name of a memory export"));
    }
    if export == crate::project::USER_EXPORT {
        return Err(f.bad("export", "a project's memory export: the user layer has no tree"));
    }
    let mut t = TreeSpec::new(&export);
    if let Some(v) = f.string("hub_dir")? {
        t.hub_dir = v;
    }
    t.hub_max = f.uint("hub_max")?.unwrap_or(t.hub_max);
    t.hub_target = f.uint("hub_target")?.unwrap_or(t.hub_target);
    t.hub_min = f.uint("hub_min")?.unwrap_or(t.hub_min);
    t.line_max = f.uint("line_max")?.unwrap_or(t.line_max);
    t.pin_body_chars = f.uint("pin_body_chars")?.unwrap_or(t.pin_body_chars);
    if let Some(v) = f.get("keyword_weight") {
        let x = v.as_f64().filter(|x| (0.0..=10.0).contains(x)).ok_or_else(|| f.bad("keyword_weight", "a number from 0 to 10"))?;
        t.keyword_weight = (x * 1000.0).round() as u32;
    }
    t.pins = f.strings("pins")?.unwrap_or_default();
    t.name_stop.extend(f.strings("name_stop")?.unwrap_or_default().into_iter().map(|w| w.to_lowercase()));
    if let Some(v) = f.string("line_format")? {
        t.format = crate::memory::tree::LineFormat::parse(&v)?;
    }
    if f.get("branches").is_some() {
        t.branches.clear();
        for (i, bv) in f.list("branches")?.iter().enumerate() {
            let b = Fields::new(&format!("memory.tree.branches[{i}]"), bv, &["name", "title", "hook", "words", "types", "repo"])?;
            let name = b.req_string("name")?;
            t.branches.push(BranchSpec {
                repo: b.string("repo")?,
                title: b.string("title")?.unwrap_or_else(|| name.clone()),
                hook: b.string("hook")?.unwrap_or_default(),
                words: b.strings("words")?.unwrap_or_default().into_iter().map(|w| w.to_lowercase()).collect(),
                types: b.strings("types")?.unwrap_or_default(),
                name,
            });
        }
    }
    t.validate()?;
    Ok(t)
}

/// The recall hook's settings.
#[derive(Debug, Clone, Default)]
pub struct RecallSpec {
    /// Tokens a prompt's recall may inject (default 600).
    pub budget_tokens: Option<usize>,
    /// Tokens a lane recall may take (default 2000).
    pub lane_budget_tokens: Option<usize>,
    /// The least FTS5 score (negated bm25) a record needs.
    pub fts_floor: Option<f64>,
    /// The least number of distinct query words a record must contain.
    pub min_terms: Option<usize>,
    /// The cosine a lexical match also needs (with a model).
    pub cos_floor: Option<f64>,
    /// The cosine at which a record passes on meaning alone (with a model).
    pub cos_alone: Option<f64>,
    /// Tokens of the prompt the gate embeds (with a model).
    pub query_tokens: Option<usize>,
    /// The bm25 floor of a lexical match when a model is loaded.
    pub hybrid_fts_floor: Option<f64>,
    /// The tokens one session's recall injections may use in all (0: no cap).
    pub session_budget_tokens: Option<usize>,
    /// The cosine below which a hook injects nothing (tuned on hit data).
    pub inject_floor: Option<f64>,
    /// Whether `recall {query}` pointers carry their source anchor.
    pub pointer_anchors: Option<bool>,
    /// Context selection of what the hooks inject (`recall.select`).
    pub select: Option<SelectSpec>,
}

/// Context selection (`recall.select`, see [`crate::memory::select`]).
#[derive(Debug, Clone, Default)]
pub struct SelectSpec {
    /// Collapse and trim are on (default true).
    pub enabled: Option<bool>,
    /// The cosine at which two records may collapse (default 0.82).
    pub dup_cos: Option<f64>,
    /// The share of a record's words the one sent before it must hold (default 0.90).
    pub dup_overlap: Option<f64>,
    /// The longest excerpt of a trimmed pointer, in characters (default 90).
    pub trim_chars: Option<usize>,
    /// The longest excerpt of an older lane entry in a lane brief (default 180; 0: whole).
    pub lane_chars: Option<usize>,
}

/// The context-cap hook's settings.
#[derive(Debug, Clone, Default)]
pub struct CapSpec {
    /// Warn at this many context tokens (default 450000).
    pub warn_tokens: Option<usize>,
    /// Block every tool but `handoff` at this many (default 500000).
    pub block_tokens: Option<usize>,
    /// Warn again after this many more tokens (default 25000).
    pub warn_every: Option<usize>,
    /// Apply to the main session too (default false: subagents only).
    pub main_session: Option<bool>,
}

/// The compaction settings (`docs/design/compaction.md`).
#[derive(Debug, Clone, Default)]
pub struct CompactSpec {
    /// Turn on the experimental compaction features: the PreCompact
    /// checkpoint, the SessionStart(compact) card and ref report, the
    /// Compact instructions offer and `defer` (default false).
    pub enabled: Option<bool>,
    /// Block a proactive automatic compaction while a tool chain is running,
    /// up to a hard ceiling (default false).
    pub defer: Option<bool>,
    /// The context window `defer` measures against (default 0: inferred).
    pub window: Option<usize>,
    /// The checkpoint card's budget in tokens (default 400).
    pub card_tokens: Option<usize>,
}

/// The configuration `atlas --setup` writes, and the one used when no
/// configuration file exists yet (`config/atlas.template.json`).
pub const TEMPLATE: &str = include_str!("../config/atlas.template.json");

/// Encoder threads when `semantic.threads` is absent.
pub const DEFAULT_EMBED_THREADS: usize = 8;

/// The whole configuration.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The database file; `--db` overrides it.
    pub db: Option<PathBuf>,
    /// The writable vaults.
    pub vaults: Vec<VaultSpec>,
    /// The read-only sources.
    pub sources: Vec<SourceSpec>,
    /// Identifier patterns, in priority order.
    pub ids: Vec<PatternSpec>,
    /// Record wikilinks and file stems as `note` identifiers (default true).
    pub wikilinks: Option<bool>,
    /// Frontmatter keys naming a vault note's identifier (default `["id"]`).
    pub vault_id_keys: Option<Vec<String>>,
    /// The most stale files a query re-indexes on the spot before answering
    /// (default 64); past it the answer is marked incomplete instead.
    pub refresh_limit: Option<usize>,
    /// Watch the roots for changes (default true; see [`crate::watch`]).
    pub watch: Option<bool>,
    /// Seconds between safety-net walks of a watched root (default 30).
    pub rescan_interval_s: Option<usize>,
    /// Bytes per token for the token ledger (default 4.0; see
    /// [`crate::tokens`]). Recalibrated by a script, never by judgement.
    pub bytes_per_token: Option<f64>,
    /// The code index, when configured.
    pub code: Option<CodeSpec>,
    /// Semantic search settings.
    pub semantic: Option<SemanticSpec>,
    /// The memory store.
    pub memory: Option<MemorySpec>,
    /// The recall hook.
    pub recall: Option<RecallSpec>,
    /// The context-cap hook.
    pub cap: Option<CapSpec>,
    /// Compaction: the checkpoint card and `compact.defer`.
    pub compact: Option<CompactSpec>,
    /// The model files downloaded at setup.
    pub models: Option<ModelsSpec>,
    /// The release check at session start.
    pub update: Option<UpdateSpec>,
    /// The `lean` section, passed through unparsed: each package reads its
    /// own subsection (`handles`, `digests`, `routing`; `setup`, `startup`).
    pub lean: Option<Value>,
    /// The projects, the user-level layer and the checkout map (see
    /// [`crate::project`]). Empty in a configuration without `projects`:
    /// the whole file is then one implicit project, as before projects.
    pub user: crate::project::UserLayer,
}

/// The keys of the configuration file's top level that are not about
/// projects.
pub const TOP_KEYS: &[&str] =
    &["db", "vaults", "sources", "ids", "wikilinks", "vault_id_keys", "refresh_limit", "watch", "rescan_interval_s", "bytes_per_token", "code", "semantic", "memory", "recall", "cap", "compact", "models", "update", "lean"];

pub(crate) fn absolute(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// A JSON object whose keys are checked against a fixed list.
pub(crate) struct Fields<'a> {
    at: String,
    map: &'a Map,
}

impl<'a> Fields<'a> {
    pub(crate) fn new(at: &str, v: &'a Value, known: &[&str]) -> Result<Fields<'a>> {
        let map = v.as_object().ok_or_else(|| Error::InvalidArgument(format!("{at} must be an object")))?;
        if let Some(k) = map.keys().find(|k| !known.contains(&k.as_str())) {
            return Err(Error::InvalidArgument(format!("{at}: unknown field {k:?}; known fields are {}", known.join(", "))));
        }
        Ok(Fields { at: at.to_string(), map })
    }
    pub(crate) fn get(&self, k: &str) -> Option<&'a Value> {
        self.map.get(k).filter(|v| !v.is_null())
    }
    pub(crate) fn bad(&self, k: &str, what: &str) -> Error {
        Error::InvalidArgument(format!("{}.{k} must be {what}", self.at))
    }
    pub(crate) fn string(&self, k: &str) -> Result<Option<String>> {
        match self.get(k) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(self.bad(k, "a string")),
        }
    }
    pub(crate) fn req_string(&self, k: &str) -> Result<String> {
        self.string(k)?.ok_or_else(|| Error::InvalidArgument(format!("{}.{k} is required", self.at)))
    }
    /// A required vault, source or identifier pattern name, checked here so
    /// the error names the file it is in (the index checks it again).
    pub(crate) fn short_name(&self, k: &str) -> Result<String> {
        let n = self.req_string(k)?;
        if n.is_empty() || n.len() > crate::project::MAX_SHORT_NAME || !n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(self.bad(k, &format!("1 to {} characters of A-Z a-z 0-9 _ - (it is {n:?})", crate::project::MAX_SHORT_NAME)));
        }
        Ok(n)
    }
    pub(crate) fn boolean(&self, k: &str) -> Result<Option<bool>> {
        match self.get(k) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(self.bad(k, "true or false")),
        }
    }
    pub(crate) fn uint(&self, k: &str) -> Result<Option<usize>> {
        match self.get(k) {
            None => Ok(None),
            Some(v) => v.as_u64().map(|n| Some(n as usize)).ok_or_else(|| self.bad(k, "a non-negative integer")),
        }
    }
    pub(crate) fn strings(&self, k: &str) -> Result<Option<Vec<String>>> {
        match self.get(k) {
            None => Ok(None),
            Some(Value::Array(a)) => a
                .iter()
                .map(|x| x.as_str().map(str::to_string).ok_or_else(|| self.bad(k, "a list of strings")))
                .collect::<Result<Vec<_>>>()
                .map(Some),
            Some(_) => Err(self.bad(k, "a list of strings")),
        }
    }
    pub(crate) fn list(&self, k: &str) -> Result<&'a [Value]> {
        match self.get(k) {
            None => Ok(&[]),
            Some(Value::Array(a)) => Ok(a),
            Some(_) => Err(self.bad(k, "a list")),
        }
    }
}

/// Reads a code root's `scip`: `true` or absent (the convention), `false`
/// (none), a path (the root folder's file), or an object from unit folder
/// to a path or `false`.
fn prebuilt_spec(f: &Fields<'_>, key: &str, base: &Path) -> Result<crate::code::Prebuilt> {
    use crate::code::Prebuilt;
    let folder = |d: &str| d.replace('\\', "/").trim_start_matches("./").trim_matches('/').to_string();
    match f.get("scip") {
        None | Some(Value::Bool(true)) => Ok(Prebuilt::Convention),
        Some(Value::Bool(false)) => Ok(Prebuilt::Off),
        Some(Value::String(p)) => Ok(Prebuilt::Files(vec![(String::new(), Some(absolute(base, Path::new(p))))])),
        Some(Value::Object(m)) => {
            let mut out = Vec::new();
            for (dir, v) in m.iter() {
                let dir = if dir == "." { String::new() } else { folder(dir) };
                let path = match v {
                    Value::String(p) => Some(absolute(base, Path::new(p))),
                    Value::Bool(false) => None,
                    _ => return Err(Error::InvalidArgument(format!("{key}.{dir} must be a path or false"))),
                };
                out.push((dir, path));
            }
            Ok(Prebuilt::Files(out))
        }
        Some(_) => Err(f.bad("scip", "true, false, a path, or an object from unit folder to a path or false")),
    }
}

/// Reads `code.indexers`: `false` (none run), `true` (the default), or an
/// object from tool name to a path or `false`.
fn indexers_spec(f: &Fields<'_>, base: &Path) -> Result<Option<Option<IndexerPaths>>> {
    let what = "false, true, or an object from indexer name to a path or false";
    match f.get("indexers") {
        None | Some(Value::Bool(true)) => Ok(None),
        Some(Value::Bool(false)) => Ok(Some(None)),
        Some(Value::Object(m)) => {
            let known: Vec<&str> = crate::langs::LANGS.iter().filter_map(|l| l.driver.as_ref().map(|d| d.tool)).collect();
            let mut out = Vec::new();
            for (tool, v) in m.iter() {
                if !known.contains(&tool.as_str()) {
                    return Err(Error::InvalidArgument(format!(
                        "code.indexers: unknown indexer {tool:?}; known indexers are {}",
                        known.join(", ")
                    )));
                }
                let path = match v {
                    Value::String(p) => Some(absolute(base, Path::new(p))),
                    Value::Bool(false) => None,
                    _ => return Err(Error::InvalidArgument(format!("code.indexers.{tool} must be a path or false"))),
                };
                out.push((tool.clone(), path));
            }
            Ok(Some(Some(out)))
        }
        Some(_) => Err(f.bad("indexers", what)),
    }
}

impl Config {
    /// Reads and checks a configuration file.
    ///
    /// # Arguments
    ///
    /// * `path` - the JSON file. Relative paths inside it are resolved
    ///   against its folder.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the file is missing, over
    /// [`MAX_CONFIG`], not valid JSON, has an unknown field or a value of the
    /// wrong type, or names the same short name twice across vaults and
    /// sources.
    pub fn load(path: &Path) -> Result<Config> {
        let meta = fs::metadata(path).map_err(|e| Error::InvalidArgument(format!("config {}: {e}", path.display())))?;
        if meta.len() > MAX_CONFIG {
            return Err(Error::InvalidArgument(format!("config {} is over {MAX_CONFIG} bytes", path.display())));
        }
        let text = fs::read_to_string(path).map_err(|e| Error::InvalidArgument(format!("config {}: {e}", path.display())))?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Config::parse(&text, &base).map_err(|e| match e {
            Error::InvalidArgument(m) => Error::InvalidArgument(format!("config {}: {m}", path.display())),
            other => other,
        })
    }

    /// Parses configuration text.
    ///
    /// # Arguments
    ///
    /// * `text` - the JSON.
    /// * `base` - the folder relative paths are resolved against.
    ///
    /// # Errors
    ///
    /// As [`Config::load`].
    pub fn parse(text: &str, base: &Path) -> Result<Config> {
        let v: Value = crate::json::parse(text).map_err(|e| Error::InvalidArgument(format!("not valid JSON: {e}")))?;
        let known: Vec<&str> = TOP_KEYS.iter().chain(crate::project::USER_KEYS).copied().collect();
        let mut c = Config::parse_object(&v, base, "config", &known)?;
        crate::project::parse_user_keys(&v, base, &mut c)?;
        Ok(c)
    }

    /// Parses one configuration object: the whole file, or one entry of
    /// `projects` (see [`crate::project`]), whose own keys are in `known` and
    /// are left to the caller.
    ///
    /// # Errors
    ///
    /// As [`Config::load`].
    pub fn parse_object(v: &Value, base: &Path, at: &str, known: &[&str]) -> Result<Config> {
        let top = Fields::new(at, v, known)?;
        // Where a nested key is, for a message: `projects[1].sources[0]`.
        let pre = if at == "config" { String::new() } else { format!("{at}.") };
        let mut c = Config {
            db: top.string("db")?.map(|d| absolute(base, Path::new(&d))),
            wikilinks: top.boolean("wikilinks")?,
            vault_id_keys: top.strings("vault_id_keys")?,
            refresh_limit: top.uint("refresh_limit")?,
            watch: top.boolean("watch")?,
            rescan_interval_s: top.uint("rescan_interval_s")?,
            bytes_per_token: match top.get("bytes_per_token") {
                None => None,
                Some(v) => Some(v.as_f64().filter(|f| *f > 0.0).ok_or_else(|| top.bad("bytes_per_token", "a positive number"))?),
            },
            ..Config::default()
        };
        for (i, x) in top.list("vaults")?.iter().enumerate() {
            let f = Fields::new(&format!("{pre}vaults[{i}]"), x, &["name", "path"])?;
            c.vaults.push(VaultSpec { name: f.short_name("name")?, path: absolute(base, Path::new(&f.req_string("path")?)) });
        }
        for (i, x) in top.list("sources")?.iter().enumerate() {
            let at = format!("{pre}sources[{i}]");
            let f = Fields::new(&at, x, &["name", "root", "include", "exclude", "chunk", "id_keys", "names_kind"])?;
            let mut chunk = ChunkRule::default();
            if let Some(cv) = f.get("chunk") {
                let cf = Fields::new(&format!("{at}.chunk"), cv, &["top_level_items", "max_level"])?;
                chunk.top_level_items = cf.boolean("top_level_items")?.unwrap_or(false);
                chunk.max_level = cf.uint("max_level")?.unwrap_or(6);
                if !(1..=6).contains(&chunk.max_level) {
                    return Err(cf.bad("max_level", "1 to 6"));
                }
            }
            c.sources.push(SourceSpec {
                name: f.short_name("name")?,
                root: absolute(base, Path::new(&f.req_string("root")?)),
                include: f.strings("include")?.ok_or_else(|| Error::InvalidArgument(format!("{at}.include is required")))?,
                exclude: f.strings("exclude")?.unwrap_or_default(),
                chunk,
                id_keys: f.strings("id_keys")?,
                names_kind: f.string("names_kind")?,
            });
        }
        for (i, x) in top.list("ids")?.iter().enumerate() {
            let f = Fields::new(&format!("{pre}ids[{i}]"), x, &["name", "regex", "case_insensitive", "exclude"])?;
            c.ids.push(PatternSpec {
                name: f.short_name("name")?,
                regex: f.req_string("regex")?,
                case_insensitive: f.boolean("case_insensitive")?.unwrap_or(false),
                exclude: f.strings("exclude")?.unwrap_or_default(),
            });
        }
        if let Some(cv) = top.get("code") {
            let f = Fields::new("code", cv, &["roots", "rust_analyzer", "indexers", "timeout_s", "debounce_s", "defer_lock", "dir"])?;
            let mut code = CodeSpec {
                rust_analyzer: f.string("rust_analyzer")?.map(|p| absolute(base, Path::new(&p))),
                indexers: indexers_spec(&f, base)?,
                timeout_s: f.uint("timeout_s")?,
                debounce_s: f.uint("debounce_s")?,
                defer_lock: f.string("defer_lock")?.map(|p| absolute(base, Path::new(&p))),
                dir: f.string("dir")?.map(|p| absolute(base, Path::new(&p))),
                ..CodeSpec::default()
            };
            for (i, x) in f.list("roots")?.iter().enumerate() {
                let r = Fields::new(&format!("{pre}code.roots[{i}]"), x, &["name", "root", "build_scripts", "scip"])?;
                code.roots.push(CodeRootSpec {
                    name: r.req_string("name")?,
                    root: absolute(base, Path::new(&r.req_string("root")?)),
                    build_scripts: r.boolean("build_scripts")?.unwrap_or(false),
                    scip: prebuilt_spec(&r, &format!("code.roots[{i}].scip"), base)?,
                });
            }
            c.code = Some(code);
        }
        if let Some(sv) = top.get("semantic") {
            let f = Fields::new("semantic", sv, &["model", "threads", "model_id"])?;
            c.semantic = Some(SemanticSpec {
                model: f.string("model")?.map(|p| absolute(base, Path::new(&p))),
                threads: f.uint("threads")?,
                model_id: f.string("model_id")?,
            });
        }
        if let Some(mv) = top.get("models") {
            let f = Fields::new("models", mv, &["dir", "auto_download", "curl", "files"])?;
            let mut m = ModelsSpec {
                dir: f.string("dir")?.map(|p| absolute(base, Path::new(&p))),
                auto_download: f.boolean("auto_download")?,
                curl: f.string("curl")?.map(PathBuf::from),
                ..ModelsSpec::default()
            };
            for (i, x) in f.list("files")?.iter().enumerate() {
                let e = Fields::new(&format!("models.files[{i}]"), x, &["id", "file", "url", "sha256", "size"])?;
                let file = e.req_string("file")?;
                if file.is_empty() || file.len() > 128 || file.starts_with('.') || file.contains(['/', '\\', ':']) || file.ends_with(".part") {
                    return Err(e.bad("file", "a plain file name (no folder, not starting with a dot, not ending in .part)"));
                }
                let url = e.req_string("url")?;
                if !crate::models::url_allowed(&url) {
                    return Err(e.bad("url", "an https URL (plain http only to 127.0.0.1, localhost or [::1])"));
                }
                let sha256 = e.req_string("sha256")?.to_ascii_lowercase();
                if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(e.bad("sha256", "64 hex characters"));
                }
                let size = match e.get("size") {
                    Some(v) => v.as_u64().filter(|n| *n > 0).ok_or_else(|| e.bad("size", "a positive integer"))?,
                    None => return Err(Error::InvalidArgument(format!("models.files[{i}].size is required"))),
                };
                m.files.push(ModelFileSpec { id: e.req_string("id")?, file, url, sha256, size });
            }
            for (what, by_id) in [("id", true), ("file", false)] {
                let mut v: Vec<&str> = m.files.iter().map(|f| if by_id { f.id.as_str() } else { f.file.as_str() }).collect();
                v.sort_unstable();
                if let Some(w) = v.windows(2).find(|w| w[0] == w[1]) {
                    return Err(Error::InvalidArgument(format!("models.files: the {what} {:?} is used twice", w[0])));
                }
            }
            c.models = Some(m);
        }
        if let Some(uv) = top.get("update") {
            let f = Fields::new("update", uv, &["enabled", "pin", "url"])?;
            let url = f.string("url")?;
            if let Some(u) = &url {
                if !crate::models::url_allowed(u) {
                    return Err(f.bad("url", "an https URL (plain http only to 127.0.0.1, localhost or [::1])"));
                }
            }
            let pin = f.string("pin")?;
            if let Some(p) = &pin {
                if crate::update::parse_version(p).is_none() {
                    return Err(f.bad("pin", "a version, MAJOR.MINOR.PATCH"));
                }
            }
            c.update = Some(UpdateSpec { enabled: f.boolean("enabled")?, pin, url });
        }
        if let Some(mv) = top.get("memory") {
            let f = Fields::new("memory", mv, &["store", "export_dir", "exports", "snapshot_hours", "snapshot_dir", "snapshot_cap_mb", "brief_reference", "tree", "transcripts"])?;
            let mut m = MemorySpec {
                store: f.string("store")?.map(|p| absolute(base, Path::new(&p))),
                export_dir: f.string("export_dir")?.map(|p| absolute(base, Path::new(&p))),
                snapshot_hours: f.uint("snapshot_hours")?,
                snapshot_dir: f.string("snapshot_dir")?.map(|p| absolute(base, Path::new(&p))),
                snapshot_cap_mb: f.uint("snapshot_cap_mb")?,
                brief_reference: f.string("brief_reference")?.map(|p| absolute(base, Path::new(&p))),
                transcripts: f.boolean("transcripts")?,
                ..MemorySpec::default()
            };
            for (i, x) in f.list("exports")?.iter().enumerate() {
                let e = Fields::new(&format!("memory.exports[{i}]"), x, &["name", "shape", "path", "index", "handoffs", "index_cap", "index_topics"])?;
                let shape = e.req_string("shape")?;
                if !["memory", "items", "headings"].contains(&shape.as_str()) {
                    return Err(e.bad("shape", "memory, items or headings"));
                }
                m.exports.push(ExportCfg {
                    name: e.req_string("name")?,
                    shape,
                    path: e.req_string("path")?,
                    index: e.string("index")?,
                    handoffs: e.boolean("handoffs")?.unwrap_or(false),
                    index_cap: e.uint("index_cap")?,
                    index_topics: match e.get("index_topics") {
                        None => None,
                        Some(_) => {
                            let mut ts = Vec::new();
                            for (j, tv) in e.list("index_topics")?.iter().enumerate() {
                                let tf = Fields::new(&format!("memory.exports[{i}].index_topics[{j}]"), tv, &["name", "words"])?;
                                let words = tf.strings("words")?.unwrap_or_default().into_iter().map(|w| w.to_lowercase()).collect();
                                ts.push(TopicCfg { name: tf.req_string("name")?, words });
                            }
                            Some(ts)
                        }
                    },
                });
            }
            let mut en: Vec<&str> = m.exports.iter().map(|e| e.name.as_str()).collect();
            en.sort_unstable();
            if let Some(w) = en.windows(2).find(|w| w[0] == w[1]) {
                return Err(Error::InvalidArgument(format!("the export name {:?} is used twice", w[0])));
            }
            if let Some(tv) = f.get("tree") {
                m.tree = Some(parse_tree(tv, &m.exports, at == "config" && v.get("projects").is_some())?);
            }
            c.memory = Some(m);
        }
        if let Some(rv) = top.get("recall") {
            let f = Fields::new("recall", rv, &["budget_tokens", "lane_budget_tokens", "fts_floor", "min_terms", "cos_floor", "cos_alone", "query_tokens", "hybrid_fts_floor", "inject_floor", "session_budget_tokens", "pointer_anchors", "select"])?;
            let num = |k: &str| -> Result<Option<f64>> {
                match f.get(k) {
                    None => Ok(None),
                    Some(v) => Ok(Some(v.as_f64().ok_or_else(|| f.bad(k, "a number"))?)),
                }
            };
            c.recall = Some(RecallSpec {
                budget_tokens: f.uint("budget_tokens")?,
                lane_budget_tokens: f.uint("lane_budget_tokens")?,
                fts_floor: num("fts_floor")?,
                min_terms: f.uint("min_terms")?,
                cos_floor: num("cos_floor")?,
                cos_alone: num("cos_alone")?,
                query_tokens: f.uint("query_tokens")?,
                hybrid_fts_floor: num("hybrid_fts_floor")?,
                inject_floor: num("inject_floor")?,
                session_budget_tokens: f.uint("session_budget_tokens")?,
                pointer_anchors: f.boolean("pointer_anchors")?,
                select: match f.get("select") {
                    None => None,
                    Some(sv) => {
                        let g = Fields::new("recall.select", sv, &["enabled", "dup_cos", "dup_overlap", "trim_chars", "lane_chars"])?;
                        let unit = |k: &str| -> Result<Option<f64>> {
                            match g.get(k) {
                                None => Ok(None),
                                Some(v) => Ok(Some(v.as_f64().filter(|x| (0.0..=1.0).contains(x)).ok_or_else(|| g.bad(k, "a number from 0 to 1"))?)),
                            }
                        };
                        Some(SelectSpec { enabled: g.boolean("enabled")?, dup_cos: unit("dup_cos")?, dup_overlap: unit("dup_overlap")?, trim_chars: g.uint("trim_chars")?, lane_chars: g.uint("lane_chars")? })
                    }
                },
            });
        }
        if let Some(cv) = top.get("cap") {
            let f = Fields::new("cap", cv, &["warn_tokens", "block_tokens", "warn_every", "main_session"])?;
            c.cap = Some(CapSpec {
                warn_tokens: f.uint("warn_tokens")?,
                block_tokens: f.uint("block_tokens")?,
                warn_every: f.uint("warn_every")?,
                main_session: f.boolean("main_session")?,
            });
        }
        if let Some(cv) = top.get("compact") {
            let f = Fields::new("compact", cv, &["enabled", "defer", "window", "card_tokens"])?;
            c.compact = Some(CompactSpec { enabled: f.boolean("enabled")?, defer: f.boolean("defer")?, window: f.uint("window")?, card_tokens: f.uint("card_tokens")? });
        }
        if let Some(lv) = top.get("lean") {
            if lv.as_object().is_none() {
                return Err(top.bad("lean", "an object"));
            }
            c.lean = Some(lv.clone());
        }
        let mut names: Vec<&str> = c
            .vaults
            .iter()
            .map(|v| v.name.as_str())
            .chain(c.sources.iter().map(|s| s.name.as_str()))
            .chain(c.code.iter().flat_map(|k| k.roots.iter().map(|r| r.name.as_str())))
            .collect();
        names.sort_unstable();
        if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(Error::InvalidArgument(format!("the name {:?} is used twice across vaults and sources", w[0])));
        }
        Ok(c)
    }

    /// Fails when a name is used twice across vaults, sources and code
    /// roots (checked again on a project's merged configuration).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] naming the name.
    pub fn check_names(&self) -> Result<()> {
        let mut names: Vec<&str> = self
            .vaults
            .iter()
            .map(|v| v.name.as_str())
            .chain(self.sources.iter().map(|s| s.name.as_str()))
            .chain(self.code.iter().flat_map(|k| k.roots.iter().map(|r| r.name.as_str())))
            .collect();
        names.sort_unstable();
        match names.windows(2).find(|w| w[0] == w[1]) {
            Some(w) => Err(Error::InvalidArgument(format!("the name {:?} is used twice across vaults and sources", w[0]))),
            None => Ok(()),
        }
    }

    /// The refresh limit, defaulted.
    pub fn refresh_limit(&self) -> usize {
        self.refresh_limit.unwrap_or(DEFAULT_REFRESH_LIMIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_resolves_relative_paths() {
        let c = Config::parse(
            r#"{"db": "s/a.sqlite", "vaults": [{"name": "n", "path": "V"}],
                "sources": [{"name": "d", "root": "/abs", "include": ["*.md"], "chunk": {"top_level_items": true}}],
                "ids": [{"name": "t", "regex": "T-[0-9]+"}]}"#,
            Path::new("base"),
        )
        .unwrap();
        assert_eq!(c.db.unwrap(), Path::new("base").join("s/a.sqlite"));
        assert_eq!(c.vaults[0].path, Path::new("base").join("V"));
        assert!(c.sources[0].chunk.top_level_items);
        assert_eq!(c.ids[0].regex, "T-[0-9]+");
        let c = Config::parse(r#"{"code": {"roots": [{"name": "app", "root": "src"}], "timeout_s": 60}, "semantic": {"model": "m.atm"}}"#, Path::new("b")).unwrap();
        let code = c.code.unwrap();
        assert_eq!(code.roots[0].root, Path::new("b").join("src"));
        assert_eq!(code.timeout_s, Some(60));
        assert_eq!(c.semantic.unwrap().model.unwrap(), Path::new("b").join("m.atm"));
        let c = Config::parse(
            r#"{"models": {"dir": "m", "files": [{"id": "embed", "file": "e.atm", "url": "https://example.com/e.atm", "sha256": "00000000000000000000000000000000000000000000000000000000000000AA", "size": 7}]}}"#,
            Path::new("b"),
        )
        .unwrap();
        let m = c.models.unwrap();
        assert_eq!(m.dir.unwrap(), Path::new("b").join("m"));
        assert_eq!(m.files[0].sha256, "00000000000000000000000000000000000000000000000000000000000000aa");
        assert_eq!(m.files[0].size, 7);
    }

    #[test]
    fn parses_context_selection() {
        let c = Config::parse(r#"{"recall": {"select": {"enabled": false, "dup_cos": 0.9, "dup_overlap": 0.95, "trim_chars": 70, "lane_chars": 0}}}"#, Path::new("b")).unwrap();
        let s = c.recall.clone().unwrap().select.unwrap();
        assert_eq!((s.enabled, s.dup_cos, s.dup_overlap, s.trim_chars, s.lane_chars), (Some(false), Some(0.9), Some(0.95), Some(70), Some(0)));
        let r = crate::memory::settings::Settings::from_config(Some(&c), Path::new("b/i.sqlite")).unwrap().recall.select;
        assert_eq!((r.enabled, r.trim_chars, r.lane_chars), (false, 70, 0));
        let d = crate::memory::settings::Settings::from_config(None, Path::new("b/i.sqlite")).unwrap().recall.select;
        assert_eq!(d, crate::memory::select::Select::default());
        assert!(d.enabled, "on by default");
    }

    #[test]
    fn the_shipped_template_parses() {
        let c = Config::parse(TEMPLATE, Path::new("b")).unwrap();
        let m = c.models.expect("the template lists the models");
        assert!(m.files.iter().any(|f| f.id == DEFAULT_MODEL_ID), "the template names the default model id");
    }

    #[test]
    fn rejects_unknown_fields_bad_types_and_duplicate_names() {
        let base = Path::new(".");
        for bad in [
            r#"{"vault": []}"#,
            r#"{"sources": [{"name": "a", "root": "r", "include": ["*.md"], "chunks": {}}]}"#,
            r#"{"ids": [{"name": "a"}]}"#,
            r#"{"wikilinks": "yes"}"#,
            r#"{"vaults": [{"name": "a", "path": "x"}], "sources": [{"name": "a", "root": "r", "include": ["*.md"]}]}"#,
            r#"{"sources": [{"name": "a", "root": "r", "include": ["*.md"], "chunk": {"max_level": 9}}]}"#,
            "not json",
            r#"{"code": {"roots": [{"name": "a", "path": "x"}]}}"#,
            r#"{"code": {"roots": [{"name": "a", "root": "x"}]}, "vaults": [{"name": "a", "path": "y"}]}"#,
            r#"{"semantic": {"model": 3}}"#,
            r#"{"models": {"files": [{"id": "e", "file": "m.atm", "url": "http://example.com/m", "sha256": "00000000000000000000000000000000000000000000000000000000000000aa", "size": 3}]}}"#,
            r#"{"models": {"files": [{"id": "e", "file": "../m.atm", "url": "https://example.com/m", "sha256": "00000000000000000000000000000000000000000000000000000000000000aa", "size": 3}]}}"#,
            r#"{"models": {"files": [{"id": "e", "file": "m.atm", "url": "https://example.com/m", "sha256": "abc", "size": 3}]}}"#,
            r#"{"models": {"files": [{"id": "e", "file": "m.atm", "url": "https://example.com/m", "sha256": "00000000000000000000000000000000000000000000000000000000000000aa"}]}}"#,
            r#"{"models": {"files": [{"id": "e", "file": "m.atm", "url": "https://x.org/a", "sha256": "00000000000000000000000000000000000000000000000000000000000000aa", "size": 3}, {"id": "e", "file": "n.atm", "url": "https://x.org/b", "sha256": "00000000000000000000000000000000000000000000000000000000000000aa", "size": 3}]}}"#,
            r#"{"recall": {"select": {"dup_cos": 1.5}}}"#,
            r#"{"recall": {"select": {"trim": 90}}}"#,
            r#"{"recall": {"select": true}}"#,
        ] {
            assert!(Config::parse(bad, base).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn a_code_root_scip_is_true_false_a_path_or_a_map_of_unit_folders() {
        use crate::code::Prebuilt;
        let scip = |v: &str| {
            Config::parse(&format!(r#"{{"code": {{"roots": [{{"name": "a", "root": "r", "scip": {v}}}]}}}}"#), Path::new("b"))
                .map(|c| c.code.unwrap().roots[0].scip.clone())
        };
        assert_eq!(scip("true").unwrap(), Prebuilt::Convention);
        assert_eq!(scip("false").unwrap(), Prebuilt::Off);
        assert_eq!(scip(r#""ci/index.scip""#).unwrap(), Prebuilt::Files(vec![(String::new(), Some(Path::new("b").join("ci/index.scip")))]));
        assert_eq!(
            scip(r#"{".": "x.scip", "./svc/api/": false}"#).unwrap(),
            Prebuilt::Files(vec![(String::new(), Some(Path::new("b").join("x.scip"))), ("svc/api".to_string(), None)])
        );
        assert!(scip(r#"{"svc": 1}"#).unwrap_err().to_string().contains("code.roots[0].scip.svc must be a path or false"));
        assert!(scip("3").unwrap_err().to_string().contains("code.roots[0].scip must be"));
        let none = Config::parse(r#"{"code": {"roots": [{"name": "a", "root": "r"}]}}"#, Path::new("b")).unwrap();
        assert_eq!(none.code.unwrap().roots[0].scip, Prebuilt::Convention);
    }

    #[test]
    fn code_indexers_is_false_true_or_a_map_of_known_tools() {
        let code =
            |v: &str| Config::parse(&format!(r#"{{"code": {{"indexers": {v}}}}}"#), Path::new("b")).map(|c| c.code.unwrap().indexers);
        assert_eq!(code("true").unwrap(), None);
        assert_eq!(code("false").unwrap(), Some(None));
        assert_eq!(
            code(r#"{"scip-go": "bin/scip-go", "scip-typescript": false}"#).unwrap(),
            Some(Some(vec![("scip-go".to_string(), Some(Path::new("b").join("bin/scip-go"))), ("scip-typescript".to_string(), None)]))
        );
        let e = code(r#"{"scip-goo": "x"}"#).unwrap_err().to_string();
        assert!(e.contains("unknown indexer \"scip-goo\"") && e.contains("scip-go, scip-typescript"), "{e}");
        assert!(code(r#"{"scip-go": true}"#).unwrap_err().to_string().contains("code.indexers.scip-go must be a path or false"));
        assert!(code("3").unwrap_err().to_string().contains("code.indexers must be"));
        let setup = |v: &str| {
            crate::code::CodeSetup::from_spec(
                &Config::parse(&format!(r#"{{"code": {{"indexers": {v}}}}}"#), Path::new("b")).unwrap().code.unwrap(),
                Path::new("db"),
                false,
            )
            .indexers
        };
        assert_eq!(setup("true"), crate::code::Indexers::Search);
        assert!(matches!(setup("false"), crate::code::Indexers::Off(_)));
        assert_eq!(setup(r#"{"scip-go": false}"#), crate::code::Indexers::Chosen(vec![("scip-go".into(), None)]));
    }
}
