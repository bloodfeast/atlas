//! Projects: one install, scoped projects.
//!
//! A project is a named set of roots: repositories, vaults, sources, code
//! roots, a memory folder and a rules file. Each project has its own index
//! database and memory store, so recall, the hooks, lanes and the journal,
//! the decision queue and `next_id` only ever see that project's records.
//! A user-level layer (a memory folder, plus memories marked `scope: user`
//! in any project's memory folder) is imported into every project.
//!
//! # Where projects come from
//!
//! * The user configuration's `projects` list (see the README for the
//!   shape). An entry may point at a project file with `file`.
//! * A project file, [`PROJECT_FILE`], at a project root: the project's
//!   name and its member repositories, written to be committed. It holds no
//!   machine paths beyond paths relative to itself; a repository listed by
//!   remote URL is found through the user configuration's `checkouts` map.
//!
//! A configuration without `projects` is one implicit project, exactly as
//! before projects existed.
//!
//! # Which project is active
//!
//! In order: `--project NAME` (or `ATLAS_PROJECT`); else the deepest
//! project root containing the working directory, where a project's roots
//! are its configured `roots`, the folder of its project file, and its
//! repositories' local checkouts, and where a project file found walking up
//! from the working directory counts as a root; else `default_project`;
//! else none (the user layer alone, in its own state folder).
//!
//! Resolution is a few path comparisons and one `stat` per parent folder,
//! cached per process by working directory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::config::{absolute, CodeRootSpec, CodeSpec, Config, ExportCfg, Fields, MemorySpec, SourceSpec};
use crate::error::{Error, Result};
use crate::json::Value;

/// The project file's name.
pub const PROJECT_FILE: &str = "atlas.project.json";

/// The largest project file read, in bytes.
pub const MAX_PROJECT_FILE: u64 = 256 * 1024;

/// The top-level keys about projects (see [`UserLayer`]).
pub const USER_KEYS: &[&str] = &["projects", "user", "default_project", "checkouts", "projects_dir"];

/// The keys of one `projects` entry: its own, then the configuration keys a
/// project may set for itself.
const ENTRY_KEYS: &[&str] = &["name", "roots", "repos", "file", "memory_dir", "intent", "db", "vaults", "sources", "ids", "code", "memory", "wikilinks", "vault_id_keys"];

/// The keys of a project file: nothing machine-specific.
const FILE_KEYS: &[&str] = &["name", "repos", "memory_dir", "intent", "vaults", "sources", "ids"];

/// The export (and source) name of the user layer.
pub const USER_EXPORT: &str = "user";

/// The environment variable naming the active project.
pub const PROJECT_VAR: &str = "ATLAS_PROJECT";

/// The state folder of a session that is in no project.
pub const NO_PROJECT: &str = "_none";

/// One repository of a project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoSpec {
    /// Short name, unique in the project.
    pub name: String,
    /// The local folder as listed (absolute, from a relative path).
    pub path: Option<PathBuf>,
    /// The remote URL as listed (identity only; never fetched).
    pub remote: Option<String>,
    /// One line: what the repository is for.
    pub role: String,
    /// The repositories it calls.
    pub talks_to: Vec<String>,
}

/// One project as configured.
#[derive(Debug, Clone, Default)]
pub struct ProjectSpec {
    /// The name.
    pub name: String,
    /// Folders that belong to the project (the working directory inside one
    /// makes it active).
    pub roots: Vec<PathBuf>,
    /// Member repositories.
    pub repos: Vec<RepoSpec>,
    /// The project file it was read from or merged with.
    pub file: Option<PathBuf>,
    /// The project's memory folder (a Claude Code style folder with an index).
    pub memory_dir: Option<PathBuf>,
    /// The project's rules file (an `INTENT.md` of dated items).
    pub intent: Option<PathBuf>,
    /// The configuration keys the project sets for itself: `db`, `vaults`,
    /// `sources`, `ids`, `code`, `memory`, `wikilinks`, `vault_id_keys`.
    pub body: Box<Config>,
}

/// The project side of the user configuration.
#[derive(Debug, Clone, Default)]
pub struct UserLayer {
    /// The `projects` key was given: the configuration is project-scoped.
    pub scoped: bool,
    /// The configured projects.
    pub projects: Vec<ProjectSpec>,
    /// The user memory folder, imported into every project.
    pub memory: Option<PathBuf>,
    /// The project active where no project root contains the working
    /// directory.
    pub default_project: Option<String>,
    /// Normalised remote URL to local checkout.
    pub checkouts: Vec<(String, PathBuf)>,
    /// Where each project's database and memory store go by default (default
    /// `projects/` in the user's data folder).
    pub projects_dir: Option<PathBuf>,
}

fn check_name(f: &Fields<'_>, n: &str) -> Result<()> {
    let ok = !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') && !n.starts_with('_');
    if ok {
        Ok(())
    } else {
        Err(f.bad("name", "1 to 64 characters of a-z A-Z 0-9 - _ (not starting with _)"))
    }
}

fn parse_repos(f: &Fields<'_>, at: &str, base: &Path) -> Result<Vec<RepoSpec>> {
    let mut out: Vec<RepoSpec> = Vec::new();
    for (i, x) in f.list("repos")?.iter().enumerate() {
        let r = Fields::new(&format!("{at}.repos[{i}]"), x, &["name", "path", "remote", "role", "talks_to"])?;
        let path = r.string("path")?.map(|p| absolute(base, Path::new(&p)));
        let remote = r.string("remote")?;
        if path.is_none() && remote.is_none() {
            return Err(Error::InvalidArgument(format!("{at}.repos[{i}] needs a path, a remote or both")));
        }
        let name = match r.string("name")? {
            Some(n) => n,
            None => repo_name(path.as_deref(), remote.as_deref()),
        };
        check_name(&r, &name)?;
        if out.iter().any(|o| o.name.eq_ignore_ascii_case(&name)) {
            return Err(Error::InvalidArgument(format!("{at}.repos: the name {name:?} is used twice")));
        }
        out.push(RepoSpec { name, path, remote, role: r.string("role")?.unwrap_or_default(), talks_to: r.strings("talks_to")?.unwrap_or_default() });
    }
    Ok(out)
}

/// A repository's default name: its folder's name, else the last part of
/// its remote, lowercased.
pub fn repo_name(path: Option<&Path>, remote: Option<&str>) -> String {
    let from_path = path.and_then(|p| p.file_name()).map(|s| s.to_string_lossy().into_owned());
    let from_remote = remote.map(|r| normalize_remote(r).rsplit('/').next().unwrap_or("").to_string());
    let raw = from_path.or(from_remote).unwrap_or_else(|| "repo".into());
    let s: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' }).collect();
    let s = s.trim_matches(|c| c == '-' || c == '_').to_string();
    if s.is_empty() {
        "repo".into()
    } else {
        s
    }
}

/// Parses one project object (a `projects` entry or a project file).
fn parse_project(v: &Value, base: &Path, at: &str, keys: &[&str]) -> Result<ProjectSpec> {
    let body = Config::parse_object(v, base, at, keys)?;
    let f = Fields::new(at, v, keys)?;
    let name = f.req_string("name")?;
    check_name(&f, &name)?;
    Ok(ProjectSpec {
        name,
        roots: f.strings("roots")?.unwrap_or_default().iter().map(|r| absolute(base, Path::new(r))).collect(),
        repos: parse_repos(&f, at, base)?,
        file: f.string("file")?.map(|p| absolute(base, Path::new(&p))),
        memory_dir: f.string("memory_dir")?.map(|p| absolute(base, Path::new(&p))),
        intent: f.string("intent")?.map(|p| absolute(base, Path::new(&p))),
        body: Box::new(body),
    })
}

/// Reads a project file. Its folder is the project's first root.
///
/// # Errors
///
/// [`Error::InvalidArgument`] when the file is missing, too large, not
/// JSON, or holds an unknown key or a bad value.
pub fn load_project_file(path: &Path) -> Result<ProjectSpec> {
    let meta = std::fs::metadata(path).map_err(|e| Error::InvalidArgument(format!("{}: {e}", path.display())))?;
    if meta.len() > MAX_PROJECT_FILE {
        return Err(Error::InvalidArgument(format!("{} is over {MAX_PROJECT_FILE} bytes", path.display())));
    }
    let text = std::fs::read_to_string(path).map_err(|e| Error::InvalidArgument(format!("{}: {e}", path.display())))?;
    let v = crate::json::parse(&text).map_err(|e| Error::InvalidArgument(format!("{}: not valid JSON: {e}", path.display())))?;
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut p = parse_project(&v, &base, "project", FILE_KEYS).map_err(|e| match e {
        Error::InvalidArgument(m) => Error::InvalidArgument(format!("{}: {m}", path.display())),
        other => other,
    })?;
    p.roots = vec![base];
    p.file = Some(path.to_path_buf());
    Ok(p)
}

/// Merges a project file into a configured entry of the same name: the
/// entry's values win, lists are joined, repositories are matched by name.
fn merge(entry: &ProjectSpec, file: ProjectSpec) -> ProjectSpec {
    let mut out = file;
    out.name = entry.name.clone();
    for r in &entry.roots {
        if !out.roots.contains(r) {
            out.roots.push(r.clone());
        }
    }
    for r in &entry.repos {
        match out.repos.iter_mut().find(|o| o.name.eq_ignore_ascii_case(&r.name)) {
            Some(o) => {
                if r.path.is_some() {
                    o.path = r.path.clone();
                }
                if r.remote.is_some() {
                    o.remote = r.remote.clone();
                }
                if !r.role.is_empty() {
                    o.role = r.role.clone();
                }
                if !r.talks_to.is_empty() {
                    o.talks_to = r.talks_to.clone();
                }
            }
            None => out.repos.push(r.clone()),
        }
    }
    if entry.memory_dir.is_some() {
        out.memory_dir = entry.memory_dir.clone();
    }
    if entry.intent.is_some() {
        out.intent = entry.intent.clone();
    }
    let mut body = (*entry.body).clone();
    body.vaults.extend(out.body.vaults.iter().cloned());
    body.sources.extend(out.body.sources.iter().cloned());
    body.ids.extend(out.body.ids.iter().cloned());
    out.body = Box::new(body);
    out
}

/// Reads the top-level project keys into `c.user`.
///
/// # Errors
///
/// As [`Config::load`]; also a project file an entry names that cannot be
/// read, and a project name used twice.
pub fn parse_user_keys(v: &Value, base: &Path, c: &mut Config) -> Result<()> {
    let known: Vec<&str> = crate::config::TOP_KEYS.iter().chain(USER_KEYS).copied().collect();
    let f = Fields::new("config", v, &known)?;
    let mut u = UserLayer { scoped: f.get("projects").is_some(), ..UserLayer::default() };
    for (i, x) in f.list("projects")?.iter().enumerate() {
        let at = format!("projects[{i}]");
        let mut p = parse_project(x, base, &at, ENTRY_KEYS)?;
        if let Some(file) = p.file.clone() {
            p = merge(&p, load_project_file(&file)?);
        }
        if u.projects.iter().any(|o| o.name.eq_ignore_ascii_case(&p.name)) {
            return Err(Error::InvalidArgument(format!("the project name {:?} is used twice", p.name)));
        }
        u.projects.push(p);
    }
    if let Some(uv) = f.get("user") {
        let uf = Fields::new("user", uv, &["memory"])?;
        u.memory = uf.string("memory")?.map(|p| absolute(base, Path::new(&p)));
    }
    u.default_project = f.string("default_project")?;
    if let Some(d) = &u.default_project {
        if !u.projects.iter().any(|p| &p.name == d) {
            return Err(Error::InvalidArgument(format!("default_project {d:?} is not in projects")));
        }
    }
    if let Some(cv) = f.get("checkouts") {
        let m = cv.as_object().ok_or_else(|| f.bad("checkouts", "an object of remote URL to folder"))?;
        for (k, v) in m.iter() {
            let p = v.as_str().ok_or_else(|| f.bad("checkouts", "an object of remote URL to folder"))?;
            u.checkouts.push((normalize_remote(k), absolute(base, Path::new(p))));
        }
    }
    u.projects_dir = f.string("projects_dir")?.map(|p| absolute(base, Path::new(&p)));
    c.user = u;
    Ok(())
}

// ------------------------------------------------------------------ remotes

/// A remote URL as an identity: `host/owner/repo`, lowercased, without the
/// scheme, user, port or `.git`. The ssh (`git@host:owner/repo.git`,
/// `ssh://git@host:22/owner/repo`) and https forms of one repository are
/// equal. Anything else (a local path) is returned with `/` separators.
pub fn normalize_remote(url: &str) -> String {
    let u = url.trim();
    let (host, path) = if let Some((_, rest)) = u.split_once("://") {
        let (hp, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = hp.rsplit('@').next().unwrap_or(hp);
        (host.split(':').next().unwrap_or(host).to_string(), path.to_string())
    } else if let Some((left, right)) = u.split_once(':').filter(|(l, _)| !l.contains('/') && !l.contains('\\') && l.len() > 1) {
        // scp form: [user@]host:owner/repo
        (left.rsplit('@').next().unwrap_or(left).to_string(), right.to_string())
    } else {
        return u.replace('\\', "/").trim_end_matches('/').to_lowercase();
    };
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    format!("{}/{}", host.to_lowercase(), path.to_lowercase())
}

/// The git folder of a repository: `.git` itself, or the folder a `.git`
/// file points at (a worktree or submodule).
fn git_dir(repo: &Path) -> Option<PathBuf> {
    let g = repo.join(".git");
    let meta = std::fs::metadata(&g).ok()?;
    if meta.is_dir() {
        return Some(g);
    }
    let text = std::fs::read_to_string(&g).ok()?;
    let to = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    Some(absolute(repo, Path::new(to)))
}

/// Whether a folder is a git repository's top (holds a `.git` folder or
/// file).
pub fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// A repository's remote URL as `.git/config` states it (`origin`, else the
/// first remote), read from the file: git is never run and nothing is
/// fetched. A worktree's `.git` file is followed to the shared config.
pub fn read_remote(repo: &Path) -> Option<String> {
    let gd = git_dir(repo)?;
    let common = std::fs::read_to_string(gd.join("commondir")).ok().map(|c| absolute(&gd, Path::new(c.trim()))).unwrap_or(gd);
    let text = std::fs::read_to_string(common.join("config")).ok()?;
    let mut section = String::new();
    let mut first: Option<String> = None;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            section = l.to_string();
            continue;
        }
        let Some(url) = l.strip_prefix("url").map(str::trim_start).and_then(|r| r.strip_prefix('=')).map(str::trim) else { continue };
        if !section.starts_with("[remote ") {
            continue;
        }
        if section == "[remote \"origin\"]" {
            return Some(url.to_string());
        }
        first.get_or_insert_with(|| url.to_string());
    }
    first
}

/// The git repositories directly inside a folder (one level down), by name:
/// `(name, folder, remote)`. Dot folders are skipped.
pub fn child_repos(dir: &Path) -> Vec<(String, PathBuf, Option<String>)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf, Option<String>)> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| is_repo(p))
        .map(|p| {
            let remote = read_remote(&p);
            (repo_name(Some(&p), remote.as_deref()), p, remote)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    let mut seen = HashSet::new();
    out.retain(|r| seen.insert(r.0.clone()));
    out
}

// -------------------------------------------------------------------- paths

// The one rule for paths: identity is canonical, values are as given.
//
// * Every comparison (the deepest root holding a working directory, the
//   project file's folder, checkouts, the repository a touched path is in,
//   the offer's asked-once state, the folders already known, export and
//   memory folders) is between two [`path_key`]s, and a key is canonical:
//   symlinks and junctions resolved (see [`crate::paths::canonical`]). So
//   `/var/x` and `/private/var/x` on macOS, or a folder and a link to it
//   anywhere, are one folder.
// * A path atlas reads (the configuration, a project file, a hook event, a
//   tool's input) is kept and returned in the form it was given; atlas never
//   rewrites one.
// * A folder atlas records itself, from a working directory or an argument
//   (the offer state, a project registered by `add-project --new`, a source
//   or repository added by `add-project`), is written canonical: that is its
//   identity, and it reads the same whichever way the folder was reached.

/// A path as resolution compares it: canonical (see the rule above), then
/// `/`-separated, no `\\?\` prefix, no trailing `/`, lowercased on Windows
/// (where paths are case-insensitive). Cached per process.
pub fn path_key(p: &Path) -> String {
    lexical_key(&crate::paths::canonical(p))
}

/// [`path_key`] without touching the file system: the text alone.
fn lexical_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s);
    let s = if s.len() > 1 { s.trim_end_matches('/') } else { s };
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// Whether `inner` (a key) is `root` (a key) or inside it.
pub fn key_within(inner: &str, root: &str) -> bool {
    !root.is_empty() && (inner == root || (inner.len() > root.len() && inner.starts_with(root) && (root.ends_with('/') || inner.as_bytes()[root.len()] == b'/')))
}

fn depth(key: &str) -> usize {
    key.split('/').filter(|s| !s.is_empty()).count()
}

// --------------------------------------------------------------- resolution

/// A repository after its local checkout was found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repo {
    /// Name.
    pub name: String,
    /// Local checkout, when found.
    pub path: Option<PathBuf>,
    /// Normalised remote, when listed.
    pub remote: Option<String>,
    /// One-line role.
    pub role: String,
    /// The repositories it calls.
    pub talks_to: Vec<String>,
}

/// The local checkout of a listed repository: the `checkouts` mapping of
/// its remote, else its listed path when that folder exists.
fn locate(r: &RepoSpec, checkouts: &[(String, PathBuf)]) -> Option<PathBuf> {
    if let Some(remote) = &r.remote {
        let n = normalize_remote(remote);
        if let Some((_, p)) = checkouts.iter().find(|(k, _)| *k == n) {
            return Some(p.clone());
        }
    }
    r.path.clone().filter(|p| p.is_dir())
}

/// A project's repositories located, and the ones that could not be.
pub fn locate_repos(p: &ProjectSpec, checkouts: &[(String, PathBuf)]) -> (Vec<Repo>, Vec<String>) {
    let mut out = Vec::new();
    let mut unmapped = Vec::new();
    for r in &p.repos {
        let path = locate(r, checkouts);
        if path.is_none() {
            unmapped.push(match &r.remote {
                Some(u) => format!("{} ({u})", r.name),
                None => r.name.clone(),
            });
        }
        out.push(Repo { name: r.name.clone(), path, remote: r.remote.as_deref().map(normalize_remote), role: r.role.clone(), talks_to: r.talks_to.clone() });
    }
    (out, unmapped)
}

/// The folders whose insides make a project active.
pub fn project_roots(p: &ProjectSpec, checkouts: &[(String, PathBuf)]) -> Vec<PathBuf> {
    let mut v = p.roots.clone();
    v.extend(p.repos.iter().filter_map(|r| locate(r, checkouts)));
    v
}

/// Why a project is the active one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// `--project` or `ATLAS_PROJECT`.
    Named,
    /// A configured root or repository contains the working directory.
    Root,
    /// A project file in the working directory or a folder above it.
    File,
    /// `default_project`.
    Default,
}

/// The active project and why.
#[derive(Debug, Clone)]
pub struct Active {
    /// The project, a project file merged in.
    pub spec: ProjectSpec,
    /// Why it is active.
    pub why: Why,
}

/// The nearest project file at or above `cwd`. The folders above are the
/// canonical path's (as git finds its repository), so a working directory
/// reached through a link finds the file above the folder it leads to.
pub fn find_project_file(cwd: &Path) -> Option<PathBuf> {
    let cwd = crate::paths::canonical(cwd);
    let mut d = Some(cwd.as_path());
    while let Some(dir) = d {
        let f = dir.join(PROJECT_FILE);
        if f.is_file() {
            return Some(f);
        }
        d = dir.parent();
    }
    None
}

type CacheKey = (String, String);
/// A cached resolution: the project, why, and the project file it came from.
type Resolution = Option<(String, Why, Option<PathBuf>)>;

fn cache() -> &'static Mutex<HashMap<CacheKey, Resolution>> {
    static C: OnceLock<Mutex<HashMap<CacheKey, Resolution>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drops the per-process resolution cache (tests that rewrite project files
/// between resolutions).
pub fn clear_cache() {
    if let Ok(mut c) = cache().lock() {
        c.clear();
    }
    crate::paths::clear_canonical_cache();
}

/// The active project for a working directory (see the module docs).
///
/// # Arguments
///
/// * `c` - the user configuration.
/// * `cwd` - the working directory (relative: from the process's).
/// * `named` - a project named explicitly, which wins.
///
/// # Errors
///
/// [`Error::InvalidArgument`] when `named` is no known project, or a project
/// file found above `cwd` cannot be read.
pub fn active(c: &Config, cwd: &Path, named: Option<&str>) -> Result<Option<Active>> {
    let cwd = if cwd.is_absolute() { cwd.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(cwd) };
    let ck = (path_key(&cwd), named.unwrap_or("").to_string());
    let hit = cache().lock().ok().and_then(|m| m.get(&ck).cloned());
    let found = match hit {
        Some(h) => h,
        None => {
            let h = resolve_uncached(c, &cwd, named)?;
            if let Ok(mut m) = cache().lock() {
                m.insert(ck, h.clone());
            }
            h
        }
    };
    let Some((name, why, file)) = found else { return Ok(None) };
    let spec = match c.user.projects.iter().find(|p| p.name == name) {
        Some(p) => match &file {
            Some(f) if p.file.as_deref().map(path_key) != Some(path_key(f)) => merge(p, load_project_file(f)?),
            _ => p.clone(),
        },
        None => load_project_file(file.as_deref().ok_or_else(|| Error::InvalidArgument(format!("project {name:?} not found")))?)?,
    };
    Ok(Some(Active { spec, why }))
}

fn resolve_uncached(c: &Config, cwd: &Path, named: Option<&str>) -> Result<Resolution> {
    let file = find_project_file(cwd);
    if let Some(n) = named.filter(|n| !n.is_empty()) {
        if c.user.projects.iter().any(|p| p.name == n) {
            return Ok(Some((n.to_string(), Why::Named, None)));
        }
        if let Some(f) = &file {
            if load_project_file(f)?.name == n {
                return Ok(Some((n.to_string(), Why::Named, Some(f.clone()))));
            }
        }
        return Err(Error::InvalidArgument(format!("no project named {n:?}")));
    }
    let key = path_key(cwd);
    let mut best: Option<(usize, String, Why, Option<PathBuf>)> = None;
    for p in &c.user.projects {
        for r in project_roots(p, &c.user.checkouts) {
            let rk = path_key(&r);
            if key_within(&key, &rk) && best.as_ref().is_none_or(|b| depth(&rk) > b.0) {
                best = Some((depth(&rk), p.name.clone(), Why::Root, None));
            }
        }
    }
    if let Some(f) = file {
        let dk = path_key(f.parent().unwrap_or(Path::new("")));
        if best.as_ref().is_none_or(|b| depth(&dk) > b.0) {
            let spec = load_project_file(&f)?;
            best = Some((depth(&dk), spec.name, Why::File, Some(f)));
        }
    }
    if let Some((_, n, w, f)) = best {
        return Ok(Some((n, w, f)));
    }
    Ok(c.user.default_project.clone().map(|d| (d, Why::Default, None)))
}

// ------------------------------------------------------------ the merged config

/// What resolving a project produced.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The configuration the process runs on: the project's own keys over
    /// the user-level ones.
    pub config: Config,
    /// The active project, when there is one.
    pub active: Option<Active>,
    /// Its repositories and their links (empty without a project).
    pub graph: Graph,
}

/// The default state folder of a project's database and memory store.
pub fn state_dir(c: &Config, name: &str) -> PathBuf {
    c.user.projects_dir.clone().unwrap_or_else(|| crate::dirs::data_dir().unwrap_or_default().join("projects")).join(name)
}

/// The user-level state folder of a configuration with projects: the
/// folder holding every project's default state folder (`projects_dir`).
/// The asked-once offer state and the routing switch live there, so one
/// answer and one switch serve every project. `None` without projects: the
/// state stays beside the index database, as before projects.
pub fn user_state_dir(c: &Config) -> Option<PathBuf> {
    c.user.scoped.then(|| c.user.projects_dir.clone().unwrap_or_else(|| crate::dirs::data_dir().unwrap_or_default().join("projects")))
}

/// Moves the per-project offer state and routing switch to the user level,
/// once: when the user-level offer state does not exist yet, every
/// project's (and the no-project folder's) offer state is merged into it
/// (lists joined in order, other keys first come first), and routing is
/// turned off for all when any project had it off. The per-project files
/// are left as they were and no longer read. Returns whether it ran.
///
/// # Errors
///
/// [`Error::Io`] writing the user-level files.
pub fn migrate_user_state(c: &Config) -> Result<bool> {
    let Some(dir) = user_state_dir(c) else { return Ok(false) };
    let target = dir.join(crate::claude_settings::OFFER_STATE);
    if target.exists() {
        return Ok(false);
    }
    let mut old: Vec<PathBuf> = Vec::new();
    for p in &c.user.projects {
        let db = p.body.db.clone().unwrap_or_else(|| state_dir(c, &p.name).join("atlas.sqlite"));
        old.extend(db.parent().map(Path::to_path_buf));
    }
    old.push(state_dir(c, NO_PROJECT));
    let mut seen = HashSet::new();
    old.retain(|d| seen.insert(path_key(d)) && path_key(d) != path_key(&dir));
    let mut merged = crate::json::Map::new();
    let mut routing_off = false;
    for d in &old {
        routing_off |= crate::hook_rewrite::off_flag_in(d).exists();
        let Some(Value::Object(m)) = std::fs::read_to_string(d.join(crate::claude_settings::OFFER_STATE)).ok().and_then(|t| crate::json::parse(&t).ok()) else { continue };
        for (k, v) in m.iter() {
            match (merged.get_mut(k), v) {
                (Some(Value::Array(have)), Value::Array(add)) => {
                    for x in add {
                        if !have.contains(x) {
                            have.push(x.clone());
                        }
                    }
                }
                (Some(_), _) => {}
                (None, _) => {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
    if routing_off {
        crate::hook_rewrite::set_routing_in(&dir, false)?;
    }
    crate::fsio::replace_atomic(&target, (crate::json::to_string_pretty(&Value::Object(merged)) + "\n").as_bytes())?;
    Ok(true)
}

fn unique(name: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(name) {
        return name.to_string();
    }
    (2..).map(|i| format!("{name}-{i}")).find(|n| !taken.contains(n)).unwrap_or_else(|| name.to_string())
}

/// The memory folders of the other configured projects, read for memories
/// marked `scope: user`.
fn other_memory_dirs(c: &Config, active: Option<&str>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for p in c.user.projects.iter().filter(|p| Some(p.name.as_str()) != active) {
        if let Some(m) = &p.memory_dir {
            v.push(m.clone());
        }
        v.extend(p.body.sources.iter().filter(|s| s.names_kind.as_deref() == Some("memory")).map(|s| s.root.clone()));
    }
    v
}

/// The configuration for the active project (see [`Resolved`]).
///
/// A configuration without `projects` comes back unchanged. Otherwise the
/// user-level vaults, sources, identifier patterns and code roots are kept
/// and the project's are added; the project's repositories become sources
/// (their Markdown) and code roots (their Rust, WGSL and `.proto` files);
/// its memory folder and rules file become synced sources; the user memory
/// folder becomes the `user` source in every project; and the database and
/// memory store are the project's own (`db`, else
/// `<projects_dir>/<name>/atlas.sqlite` with the store beside it).
///
/// # Errors
///
/// [`Error::InvalidArgument`] for a name used twice after the merge, or as
/// [`active`].
pub fn resolve(c: &Config, cwd: &Path, named: Option<&str>) -> Result<Resolved> {
    if !c.user.scoped {
        return Ok(Resolved { config: c.clone(), active: None, graph: Graph::default() });
    }
    let active = active(c, cwd, named)?;
    let mut e = c.clone();
    let mut graph = Graph::default();
    let state = state_dir(c, active.as_ref().map_or(NO_PROJECT, |a| a.spec.name.as_str()));
    let mut memory = c.memory.clone().map(|m| MemorySpec { snapshot_hours: m.snapshot_hours, snapshot_cap_mb: m.snapshot_cap_mb, ..MemorySpec::default() }).unwrap_or_default();
    e.db = Some(state.join("atlas.sqlite"));
    if let Some(a) = &active {
        let p = &a.spec;
        let b = &p.body;
        if let Some(d) = &b.db {
            e.db = Some(d.clone());
        }
        e.vaults.extend(b.vaults.iter().cloned());
        e.sources.extend(b.sources.iter().cloned());
        e.ids.extend(b.ids.iter().cloned());
        if b.wikilinks.is_some() {
            e.wikilinks = b.wikilinks;
        }
        if b.vault_id_keys.is_some() {
            e.vault_id_keys = b.vault_id_keys.clone();
        }
        if let Some(pc) = &b.code {
            let mut code = e.code.clone().unwrap_or_default();
            let roots = std::mem::take(&mut code.roots);
            code = CodeSpec { roots, ..pc.clone() };
            code.roots.extend(pc.roots.iter().cloned());
            e.code = Some(code);
        }
        if let Some(m) = &b.memory {
            memory = m.clone();
        }
        let (repos, unmapped) = locate_repos(p, &c.user.checkouts);
        let mut taken: HashSet<String> = e.vaults.iter().map(|v| v.name.clone()).chain(e.sources.iter().map(|s| s.name.clone())).collect();
        taken.extend(e.code.iter().flat_map(|k| k.roots.iter().map(|r| r.name.clone())));
        for r in &repos {
            let Some(path) = &r.path else { continue };
            let sname = unique(&r.name, &taken);
            taken.insert(sname.clone());
            e.sources.push(SourceSpec {
                name: sname,
                root: path.clone(),
                include: vec!["**/*.md".into()],
                exclude: vec!["target/**".into(), "node_modules/**".into(), "vendor/**".into()],
                chunk: Default::default(),
                id_keys: None,
                names_kind: None,
            });
            let cname = unique(&format!("{}-code", r.name), &taken);
            taken.insert(cname.clone());
            e.code.get_or_insert_with(CodeSpec::default).roots.push(CodeRootSpec { name: cname, root: path.clone(), build_scripts: false });
        }
        let has_export = |m: &MemorySpec, n: &str| m.exports.iter().any(|x| x.name == n);
        if let Some(dir) = &p.memory_dir {
            if !taken.contains("memory") {
                e.sources.push(memory_source("memory", dir));
                taken.insert("memory".into());
            }
            if !has_export(&memory, "memory") {
                memory.exports.push(ExportCfg { name: "memory".into(), shape: "memory".into(), path: "memory".into(), index: Some("MEMORY.md".into()), ..ExportCfg::default() });
            }
        }
        if let Some(file) = &p.intent {
            let fname = file.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "INTENT.md".into());
            if !taken.contains("intent") {
                e.sources.push(SourceSpec {
                    name: "intent".into(),
                    root: file.parent().map(Path::to_path_buf).unwrap_or_default(),
                    include: vec![fname.clone()],
                    exclude: Vec::new(),
                    chunk: crate::chunk::ChunkRule { top_level_items: true, ..Default::default() },
                    id_keys: None,
                    names_kind: None,
                });
                taken.insert("intent".into());
            }
            if !has_export(&memory, "intent") {
                memory.exports.push(ExportCfg { name: "intent".into(), shape: "items".into(), path: fname, ..ExportCfg::default() });
            }
        }
        graph = Graph { name: p.name.clone(), roots: project_roots(p, &c.user.checkouts), repos, unmapped };
    }
    if let Some(dir) = &c.user.memory {
        if !e.sources.iter().any(|s| s.name == USER_EXPORT) && !e.vaults.iter().any(|v| v.name == USER_EXPORT) {
            e.sources.push(memory_source(USER_EXPORT, dir));
        }
        if !memory.exports.iter().any(|x| x.name == USER_EXPORT) {
            memory.exports.push(ExportCfg { name: USER_EXPORT.into(), shape: "memory".into(), path: USER_EXPORT.into(), index: Some("MEMORY.md".into()), ..ExportCfg::default() });
        }
        memory.user_scope_from = other_memory_dirs(c, active.as_ref().map(|a| a.spec.name.as_str()));
        memory.user_memory = Some(dir.clone());
    }
    memory.user_state = user_state_dir(c);
    memory.tree = project_tree(c, &memory, active.as_ref().map(|a| &a.spec));
    if e.vaults.is_empty() {
        // The server needs one writable vault (discussions, next_id): a
        // project without one gets its own `notes` beside its database.
        let dir = e.db.as_deref().and_then(Path::parent).unwrap_or(&state).join("notes");
        std::fs::create_dir_all(&dir).map_err(|err| Error::Io(format!("{}: {err}", dir.display())))?;
        if !e.sources.iter().any(|s| s.name == "notes") {
            e.vaults.push(crate::config::VaultSpec { name: "notes".into(), path: dir });
        }
    }
    if memory.export_dir.is_none() && !memory.exports.is_empty() {
        memory.export_dir = Some(e.db.as_deref().and_then(Path::parent).unwrap_or(&state).join("exports"));
    }
    e.memory = Some(memory);
    e.check_names()?;
    Ok(Resolved { config: e, active, graph })
}

/// A project's memory tree: its own `memory.tree`, else the user-level one
/// pointed at the project's memory export (the first memory export that is
/// not the user layer's); none without a memory export. The user-level
/// pins are shared pins, read from the user layer's folders too; the
/// project's own pins stay its own. Hubs go in the folder the export syncs
/// from, so no two projects share a hub.
fn project_tree(c: &Config, memory: &MemorySpec, p: Option<&ProjectSpec>) -> Option<crate::memory::tree::TreeSpec> {
    let top = c.memory.as_ref().and_then(|m| m.tree.clone());
    let mut t = match memory.tree.clone().filter(|t| !t.export.is_empty()) {
        Some(own) => own,
        None => {
            let mut t = top.clone()?;
            t.export = memory.exports.iter().find(|e| e.shape == "memory" && e.name != USER_EXPORT)?.name.clone();
            t.pins = Vec::new();
            t
        }
    };
    if t.export == USER_EXPORT {
        return None;
    }
    t.shared_pins = top.map(|x| x.pins).unwrap_or_default();
    t.shared = c.user.memory.iter().map(|d| (d.clone(), false)).collect();
    t.shared.extend(other_memory_dirs(c, p.map(|p| p.name.as_str())).into_iter().map(|d| (d, true)));
    Some(t)
}

fn memory_source(name: &str, dir: &Path) -> SourceSpec {
    SourceSpec {
        name: name.into(),
        root: dir.to_path_buf(),
        include: vec!["*.md".into()],
        exclude: Vec::new(),
        chunk: Default::default(),
        id_keys: Some(vec!["name".into()]),
        names_kind: Some("memory".into()),
    }
}

// --------------------------------------------------------------- the graph

/// A project's repositories and how they talk: what recall and the tools
/// route by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    /// The project's name (empty without a project).
    pub name: String,
    /// Its root folders.
    pub roots: Vec<PathBuf>,
    /// Its repositories.
    pub repos: Vec<Repo>,
    /// Listed repositories with no local checkout (`name (remote)`).
    pub unmapped: Vec<String>,
}

impl Graph {
    /// A repository by name (case-insensitive).
    pub fn repo(&self, name: &str) -> Option<&Repo> {
        self.repos.iter().find(|r| r.name.eq_ignore_ascii_case(name))
    }

    /// The repositories that list `name` in `talks_to`.
    pub fn used_by(&self, name: &str) -> Vec<&str> {
        self.repos.iter().filter(|r| r.talks_to.iter().any(|t| t.eq_ignore_ascii_case(name))).map(|r| r.name.as_str()).collect()
    }

    /// What `name` talks to and what talks to it.
    pub fn neighbours(&self, name: &str) -> Vec<String> {
        let mut v: Vec<String> = self.repo(name).map(|r| r.talks_to.clone()).unwrap_or_default();
        for u in self.used_by(name) {
            if !v.iter().any(|x| x.eq_ignore_ascii_case(u)) {
                v.push(u.to_string());
            }
        }
        v
    }

    /// The repositories a `repo` filter covers: the repository and what it
    /// talks to or is used by (the neighbours recall puts first), lowercased.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] naming the project's repositories when
    /// `name` is not one of them.
    pub fn coverage(&self, name: &str) -> Result<Vec<String>> {
        let Some(r) = self.repo(name) else {
            let all: Vec<&str> = self.repos.iter().map(|r| r.name.as_str()).collect();
            return Err(Error::InvalidArgument(format!("repo {name:?} is not in project {}; its repos: {}", self.name, all.join(", "))));
        };
        let mut v = vec![r.name.to_lowercase()];
        v.extend(self.neighbours(&r.name).into_iter().map(|n| n.to_lowercase()));
        v.dedup();
        Ok(v)
    }

    /// Whether a text names one of `covered` (see [`Graph::coverage`]) as a
    /// whole word.
    pub fn mentions(text: &str, covered: &[String]) -> bool {
        let lower = text.to_lowercase();
        covered.iter().any(|w| crate::memory::contains_word(&lower, w))
    }

    /// Every repository `name` depends on, directly or through others,
    /// nearest first.
    pub fn depends_on(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut queue: std::collections::VecDeque<String> = self.repo(name).map(|r| r.talks_to.iter().cloned().collect()).unwrap_or_default();
        while let Some(n) = queue.pop_front() {
            if n.eq_ignore_ascii_case(name) || out.iter().any(|o| o.eq_ignore_ascii_case(&n)) {
                continue;
            }
            if let Some(r) = self.repo(&n) {
                queue.extend(r.talks_to.iter().cloned());
            }
            out.push(n);
        }
        out
    }

    /// The repository whose checkout holds a path (the deepest one). A
    /// relative path is in none: its base is the session's, not this
    /// process's working directory.
    pub fn repo_of(&self, path: &str) -> Option<&Repo> {
        if path.is_empty() || !Path::new(path).is_absolute() {
            return None;
        }
        let k = path_key(Path::new(path));
        self.repos
            .iter()
            .filter_map(|r| r.path.as_ref().map(|p| (r, path_key(p))))
            .filter(|(_, rk)| key_within(&k, rk))
            .max_by_key(|(_, rk)| rk.len())
            .map(|(r, _)| r)
    }

    /// The repositories a text names (whole words).
    pub fn named_in(&self, text: &str) -> Vec<&Repo> {
        let lower = text.to_lowercase();
        self.repos.iter().filter(|r| crate::memory::contains_word(&lower, &r.name.to_lowercase())).collect()
    }

    /// One line on a repository: its role, what it talks to and depends on,
    /// and what uses it.
    pub fn describe(&self, name: &str) -> Option<String> {
        let r = self.repo(name)?;
        let mut s = format!("repo {}", r.name);
        if !r.role.is_empty() {
            s.push_str(&format!(": {}", r.role));
        }
        if !r.talks_to.is_empty() {
            s.push_str(&format!("; talks to {}", r.talks_to.join(", ")));
        }
        let deep: Vec<String> = self.depends_on(name).into_iter().filter(|d| !r.talks_to.iter().any(|t| t.eq_ignore_ascii_case(d))).collect();
        if !deep.is_empty() {
            s.push_str(&format!("; through them {}", deep.join(", ")));
        }
        let used = self.used_by(name);
        if !used.is_empty() {
            s.push_str(&format!("; used by {}", used.join(", ")));
        }
        if r.path.is_none() {
            s.push_str("; no local checkout");
        }
        Some(s)
    }
}

/// The repository a session is working in, from the paths its tool calls
/// touch: each touch weighs [`DECAY`] less per later touch, and the
/// heaviest repository wins. The working directory is the fallback.
#[derive(Debug, Default)]
pub struct Tracker {
    sessions: HashMap<String, Vec<(String, u64)>>,
    tick: u64,
}

/// The weight a touch loses per later touch.
pub const DECAY: f64 = 0.8;
/// Touches kept per session.
pub const KEEP: usize = 32;

impl Tracker {
    /// Records that a session touched a path; paths outside every
    /// repository are ignored.
    pub fn touch(&mut self, g: &Graph, session: &str, path: &str) {
        let Some(r) = g.repo_of(path) else { return };
        self.tick += 1;
        let v = self.sessions.entry(session.to_string()).or_default();
        v.push((r.name.clone(), self.tick));
        if v.len() > KEEP {
            v.remove(0);
        }
    }

    /// The paths a tool call names: `file_path`, `path`, `notebook_path`.
    pub fn touch_input(&mut self, g: &Graph, session: &str, input: &Value) {
        for k in ["file_path", "path", "notebook_path"] {
            if let Some(p) = input.get(k).and_then(Value::as_str) {
                self.touch(g, session, p);
            }
        }
    }

    /// The session's active repository: recency-weighted touches, else the
    /// repository holding `cwd`.
    pub fn active(&self, g: &Graph, session: &str, cwd: &str) -> Option<String> {
        let mut score: BTreeMap<&str, f64> = BTreeMap::new();
        if let Some(v) = self.sessions.get(session) {
            let now = v.last().map_or(0, |x| x.1);
            for (r, t) in v {
                *score.entry(r.as_str()).or_default() += DECAY.powi((now - t) as i32);
            }
        }
        match score.into_iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
            Some((r, _)) => Some(r.to_string()),
            None => g.repo_of(cwd).map(|r| r.name.clone()),
        }
    }
}

// ------------------------------------------------------------ the project file

/// The text of a new project file: the name and each repository's path
/// (relative to `dir`) and remote.
pub fn project_file_text(name: &str, dir: &Path, repos: &[(String, PathBuf, Option<String>)]) -> String {
    let list: Vec<Value> = repos
        .iter()
        .map(|(n, p, r)| {
            let rel = p.strip_prefix(dir).map(|x| x.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| p.to_string_lossy().replace('\\', "/"));
            let mut m = crate::json::Map::new();
            m.insert("name".into(), Value::String(n.clone()));
            m.insert("path".into(), Value::String(rel));
            if let Some(r) = r {
                m.insert("remote".into(), Value::String(r.clone()));
            }
            Value::Object(m)
        })
        .collect();
    let mut m = crate::json::Map::new();
    m.insert("name".into(), Value::String(name.to_string()));
    m.insert("repos".into(), Value::Array(list));
    crate::json::to_string_pretty(&Value::Object(m)) + "\n"
}

// ----------------------------------------------------------------- migration

/// The configuration keys a migration moves into the project.
pub const MIGRATED_KEYS: &[&str] = &["db", "vaults", "sources", "ids", "code", "memory"];

/// The deepest folder holding every path, or `None` when that is a drive
/// or file-system root.
fn common_root(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut it = paths.iter();
    let first = it.next()?;
    let mut common: Vec<std::path::Component<'_>> = first.components().collect();
    for p in it {
        let c: Vec<_> = p.components().collect();
        let n = common.iter().zip(&c).take_while(|(a, b)| lexical_key(Path::new(a.as_os_str())) == lexical_key(Path::new(b.as_os_str()))).count();
        common.truncate(n);
    }
    let named = common.iter().filter(|c| matches!(c, std::path::Component::Normal(_))).count();
    (named >= 1).then(|| common.iter().collect())
}

/// The configuration value after moving a pre-project configuration into
/// one project named `name`, or `None` when there is nothing to move (no
/// pre-project keys, or `projects` already present): a migration is
/// idempotent.
///
/// The project keeps the database and memory store it had (an absent `db`
/// is written as `default_db`, the path it resolved to), so no data moves.
/// Its root is the deepest folder holding its vaults, sources and code roots
/// (the Claude Code configuration folder left out), and it becomes
/// `default_project`, so every session that resolved to it before still
/// does.
///
/// # Arguments
///
/// * `v` - the configuration file's value.
/// * `base` - the configuration file's folder (relative paths).
/// * `name` - the project's name.
/// * `default_db` - the database an absent `db` resolved to.
/// * `claude_dir` - Claude Code's folder, left out of the root.
pub fn migrate_value(v: &Value, base: &Path, name: &str, default_db: &Path, claude_dir: Option<&Path>) -> Option<Value> {
    let m = v.as_object()?;
    if m.contains_key("projects") || !MIGRATED_KEYS.iter().any(|k| m.contains_key(k)) {
        return None;
    }
    let mut top = crate::json::Map::new();
    let mut proj = crate::json::Map::new();
    proj.insert("name".into(), Value::String(name.to_string()));
    for (k, x) in m.iter() {
        if MIGRATED_KEYS.contains(&k.as_str()) {
            proj.insert(k.clone(), x.clone());
        } else {
            top.insert(k.clone(), x.clone());
        }
    }
    if !proj.contains_key("db") {
        proj.insert("db".into(), Value::String(default_db.to_string_lossy().into_owned()));
    }
    let c = Config::parse_object(v, base, "config", crate::config::TOP_KEYS).ok()?;
    let claude = claude_dir.map(path_key);
    let mut dirs: Vec<PathBuf> = crate::claude_settings::configured_roots(&c);
    dirs.retain(|d| claude.as_ref().is_none_or(|ck| !key_within(&path_key(d), ck)));
    if let Some(r) = common_root(&dirs) {
        proj.insert("roots".into(), Value::Array(vec![Value::String(r.to_string_lossy().replace('\\', "/"))]));
    }
    // The project's keys in a fixed order: name, roots, then the rest.
    let mut ordered = crate::json::Map::new();
    for k in ["name", "roots"] {
        if let Some(x) = proj.remove(k) {
            ordered.insert(k.into(), x);
        }
    }
    for (k, x) in proj.iter() {
        ordered.insert(k.clone(), x.clone());
    }
    top.insert("projects".into(), Value::Array(vec![Value::Object(ordered)]));
    top.insert("default_project".into(), Value::String(name.to_string()));
    Some(Value::Object(top))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_normalise_across_forms() {
        let want = "github.example/acme/billing";
        for u in [
            "git@github.example:acme/billing.git",
            "git@github.example:acme/billing",
            "ssh://git@github.example/acme/billing.git",
            "ssh://git@github.example:22/acme/billing.git",
            "https://github.example/acme/billing.git",
            "https://user@github.example/acme/billing/",
            "HTTPS://GitHub.Example/Acme/Billing",
            "git://github.example/acme/billing.git",
        ] {
            assert_eq!(normalize_remote(u), want, "{u}");
        }
        assert_ne!(normalize_remote("https://github.example/acme/auth"), want);
    }

    #[test]
    fn keys_compare_at_component_boundaries() {
        assert!(key_within("/w/acme/billing", "/w/acme"));
        assert!(key_within("/w/acme", "/w/acme"));
        assert!(!key_within("/w/acmex", "/w/acme"));
        assert!(!key_within("/w", "/w/acme"));
        assert!(key_within("/w/acme", "/"));
    }

    #[test]
    fn the_graph_answers_dependencies_and_users() {
        let r = |n: &str, t: &[&str]| Repo { name: n.into(), path: None, remote: None, role: format!("{n} role"), talks_to: t.iter().map(|s| s.to_string()).collect() };
        let g = Graph { name: "acme".into(), roots: vec![], repos: vec![r("gateway", &["billing", "auth"]), r("billing", &["auth", "ledger"]), r("auth", &[]), r("ledger", &[])], unmapped: vec![] };
        assert_eq!(g.depends_on("gateway"), vec!["billing", "auth", "ledger"]);
        assert_eq!(g.used_by("auth"), vec!["gateway", "billing"]);
        assert_eq!(g.neighbours("billing"), vec!["auth", "ledger", "gateway"]);
        let d = g.describe("billing").unwrap();
        assert!(d.contains("talks to auth, ledger") && d.contains("used by gateway"), "{d}");
        assert_eq!(g.named_in("what does Billing depend on?").len(), 1);
    }

    #[test]
    fn the_tracker_follows_recent_touches() {
        let base = std::env::temp_dir();
        let r = |n: &str| Repo { name: n.into(), path: Some(base.join("acme").join(n)), ..Repo::default() };
        let g = Graph { repos: vec![r("billing"), r("auth")], ..Graph::default() };
        let mut t = Tracker::default();
        let p = |n: &str| base.join("acme").join(n).join("src").join("x.go").to_string_lossy().into_owned();
        let cwd = base.join("acme").join("auth").to_string_lossy().into_owned();
        assert_eq!(t.active(&g, "s", &cwd).as_deref(), Some("auth"), "cwd is the fallback");
        for _ in 0..3 {
            t.touch(&g, "s", &p("billing"));
        }
        assert_eq!(t.active(&g, "s", &cwd).as_deref(), Some("billing"));
        for _ in 0..3 {
            t.touch(&g, "s", &p("auth"));
        }
        assert_eq!(t.active(&g, "s", &cwd).as_deref(), Some("auth"), "recent touches outweigh older ones");
        assert_eq!(t.active(&g, "other", "").as_deref(), None, "sessions are separate");
    }

    #[test]
    fn migration_moves_the_keys_once() {
        let v = crate::json::parse(r#"{"db": "s/a.sqlite", "vaults": [{"name": "notes", "path": "w/notes"}], "sources": [{"name": "d", "root": "w/docs", "include": ["*.md"]}], "update": {"enabled": true}}"#).unwrap();
        let base = Path::new("base");
        let m = migrate_value(&v, base, "alpha", Path::new("unused.sqlite"), None).unwrap();
        assert!(m.get("vaults").is_none() && m.get("db").is_none());
        assert_eq!(m.get("default_project").and_then(Value::as_str), Some("alpha"));
        assert!(m.get("update").is_some());
        let p = &m.get("projects").unwrap().as_array().unwrap()[0];
        assert_eq!(p.get("db").and_then(Value::as_str), Some("s/a.sqlite"));
        let root = p.get("roots").unwrap().as_array().unwrap()[0].as_str().unwrap().to_string();
        assert_eq!(Path::new(&root), base.join("w"));
        assert!(migrate_value(&m, base, "alpha", Path::new("x"), None).is_none(), "idempotent");
        let c = Config::parse(&crate::json::to_string_pretty(&m), base).unwrap();
        assert_eq!(c.user.projects[0].body.vaults[0].path, base.join("w/notes"));
    }
}
