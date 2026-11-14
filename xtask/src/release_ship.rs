//! `cargo xtask release ship vX.Y.Z`: the release as one resumable command,
//! run twice by the owner at a terminal, with his approval on GitHub between
//! the two runs (the release process, Q3 to Q6).
//!
//! ```text
//! cargo xtask release ship vX.Y.Z
//!     Reads what GitHub holds for vX.Y.Z and takes the next step:
//!
//!     run 1 (no pull request yet): refuses unless the checkout is clean at
//!         the development repository's main (`checkout_guard`, OnMain::Tip)
//!         and its origin is github.com/bloodfeast/atlas-dev; runs
//!         `release check` unless a current stamp stands (three release
//!         builds); shows the plan; asks `open vX.Y.Z` at the terminal;
//!         dispatches the release-pr workflow with the version, HEAD and a
//!         nonce; finds that run by the nonce in its name, watches it, and
//!         prints the pull request.
//!     re-export (the pull request open, but the state file names another
//!         commit than HEAD, or its run is missing, still going or failed):
//!         run 1 again through the same steps and its own `open vX.Y.Z`
//!         prompt, from the tip of main only; release-pr replaces the
//!         branch, so the open pull request's head moves and needs a new
//!         approval. A run still going is watched, never dispatched again.
//!     run 2 (the pull request open, and HEAD's export by the state file,
//!         when there is one; else the re-export above): refuses unless
//!         HEAD is on main, the pull request is the release app's, from
//!         release/vX.Y.Z into main, titled `atlas vX.Y.Z`, the owner's
//!         latest review approves its head commit (read from `reviews`), no
//!         one's latest asks for changes, every check is green, and HEAD
//!         holds what that head holds (`same_code`); asks `merge vX.Y.Z`;
//!         squash-merges with the head pinned (GitHub picks the author);
//!         checks every address of the public history up to the merge
//!         commit, co-authors included (here, and on any later run that
//!         finds the pull request merged); watches the release workflow;
//!         then checks the draft (it must target the merge commit) and runs
//!         `release sign`, whose one prompt `publish vX.Y.Z` covers the
//!         signature, the upload, the push of main and the wiki, and the
//!         publish.
//! ```
//!
//! Every state is read again on each run, so a run that stopped anywhere is
//! run again: a release-pr run in progress is watched, not dispatched
//! again; a closed pull request whose branch is gone, or holds a later
//! export, gives way to run 1; a merged pull request is never merged again;
//! a half-signed release resumes on its binaries commit. Each public act is
//! behind its own typed prompt (`release_check::Tty`, the only `Confirm`
//! built outside tests); without a terminal every one is refused, so no agent can open,
//! merge or publish a release. The signing key's path is checked
//! (`is_file`, never opened) only after every other check of the publish;
//! openssl alone opens it, when signing.
//!
//! Configuration: the private file ATLAS_RELEASE_CONFIG names, else
//! `%USERPROFILE%/.atlas-release.json` (`$HOME` elsewhere) when it is there:
//! a JSON object of paths, `denylist`, `accepted` and `key`, and
//! `wsl_distro`; no other key. ATLAS_LEAK_DENYLIST, ATLAS_LEAK_ACCEPTED,
//! ATLAS_SIGN_KEY and ATLAS_WSL_DISTRO win over the file; each run says
//! where each came from (the path, never the contents). The file and every
//! path in it are refused inside the atlas checkout or any clone of the
//! development or public repository, and inside any other git work tree
//! unless git ignores them there.
//!
//! `run 1` writes `<stage>/vX.Y.Z/ship/state.json` (the tag, the commit,
//! the nonce and the run): a convenience for run 2, never trusted alone; the
//! tree check against the pull request's head is the authority.

use std::path::{Path, PathBuf};
use std::process::Command;

use atlas::json::Value;

use crate::release::{env, mkdir, output, read_text, say, status, write};
use crate::release_check::Confirm;
use crate::release_ci::{checkout_guard, clone, git_out, other_addresses, refused, same_code, short, sign, version_of, Draft, Ident, OnMain, Releases, Sign, ADDRESS_LOG};

type R<T = ()> = Result<T, String>;

/// The development repository, owner/name.
pub const DEV_REPO: &str = "bloodfeast/atlas-dev";

/// The checks the public repository's CI runs on a pull request (ci.yml):
/// each must have run and passed before the merge.
pub const REQUIRED_CHECKS: [&str; 5] = ["test (ubuntu-latest)", "test (windows-latest)", "test (macos-latest)", "clippy (windows-latest)", "clippy (ubuntu-latest)"];

/// How often a run that was just started, or a merge, is looked for.
const TRIES: u32 = 20;

/// The keys the private configuration takes.
const CONFIG_KEYS: [&str; 4] = ["denylist", "accepted", "key", "wsl_distro"];

// ---------------------------------------------------------------- config

/// The private configuration file's contents: paths only, and the WSL
/// distribution's name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// The leak denylist.
    pub denylist: Option<PathBuf>,
    /// The accepted-hits file.
    pub accepted: Option<PathBuf>,
    /// The release signing key (never opened here).
    pub key: Option<PathBuf>,
    /// The WSL distribution of the Linux builds.
    pub wsl_distro: Option<String>,
}

/// Reads the configuration: a JSON object whose values are non-empty
/// strings, with no key but [`CONFIG_KEYS`]. Never prints a value.
pub fn parse_config(text: &str) -> R<Config> {
    let v = atlas::json::parse(text).map_err(|_| "the release config is not JSON".to_string())?;
    let m = v.as_object().ok_or("the release config is not a JSON object")?;
    let mut c = Config::default();
    for (k, val) in m.iter() {
        if !CONFIG_KEYS.contains(&k.as_str()) {
            return Err(format!("the release config has an unknown key {k:?} (it takes {})", CONFIG_KEYS.join(", ")));
        }
        let s = val.as_str().filter(|s| !s.trim().is_empty()).ok_or_else(|| format!("the release config's {k} is not a non-empty string"))?;
        match k.as_str() {
            "denylist" => c.denylist = Some(PathBuf::from(s)),
            "accepted" => c.accepted = Some(PathBuf::from(s)),
            "key" => c.key = Some(PathBuf::from(s)),
            _ => c.wsl_distro = Some(s.to_string()),
        }
    }
    Ok(c)
}

/// The default configuration path, `.atlas-release.json` in the home folder.
fn default_config_path() -> PathBuf {
    if cfg!(test) {
        panic!("the default release config path in a test: pass a path");
    }
    crate::release::home().join(".atlas-release.json")
}

/// The configuration file: the one `set` names (ATLAS_RELEASE_CONFIG), which
/// must be there, else the default one when it is there.
pub fn config_file(set: Option<String>) -> R<Option<PathBuf>> {
    match set {
        Some(p) => {
            let p = PathBuf::from(p);
            if !p.is_file() {
                return Err(format!("ATLAS_RELEASE_CONFIG names {}, which is not a file", p.display()));
            }
            Ok(Some(p))
        }
        None => Ok(Some(default_config_path()).filter(|p| p.is_file())),
    }
}

/// What a ship works from, each with where it came from.
#[derive(Debug, Default)]
pub struct Settings {
    /// The leak denylist.
    pub denylist: PathBuf,
    /// The accepted-hits file.
    pub accepted: Option<PathBuf>,
    /// The signing key's path (never opened before the signature).
    pub key: Option<PathBuf>,
    /// The WSL distribution.
    pub wsl_distro: Option<String>,
    /// One line per setting: its name and its source, never its contents.
    pub sources: Vec<String>,
}

/// The environment over the file, for each setting. `envs` reads a
/// variable; `file` is the configuration file's path, for the sources.
pub fn resolve(cfg: &Config, file: Option<&Path>, envs: &dyn Fn(&str) -> Option<String>) -> R<Settings> {
    let from = file.map(|f| format!("the config {}", f.display())).unwrap_or_else(|| "no config".into());
    let mut s = Settings::default();
    let mut pick = |name: &str, var: &str, c: Option<&PathBuf>| -> Option<PathBuf> {
        match (envs(var), c) {
            (Some(v), _) => {
                s.sources.push(format!("{name}: {v} (from {var})"));
                Some(PathBuf::from(v))
            }
            (None, Some(p)) => {
                s.sources.push(format!("{name}: {} (from {from})", p.display()));
                Some(p.clone())
            }
            (None, None) => None,
        }
    };
    let denylist = pick("denylist", "ATLAS_LEAK_DENYLIST", cfg.denylist.as_ref());
    let accepted = pick("accepted hits", "ATLAS_LEAK_ACCEPTED", cfg.accepted.as_ref());
    let key = pick("signing key", "ATLAS_SIGN_KEY", cfg.key.as_ref());
    s.denylist = denylist.ok_or("no denylist: set `denylist` in the release config or ATLAS_LEAK_DENYLIST")?;
    s.accepted = match accepted {
        Some(a) => Some(a),
        None => {
            let beside = s.denylist.with_file_name(crate::accept::DEFAULT_NAME);
            beside.is_file().then(|| {
                s.sources.push(format!("accepted hits: {} (beside the denylist)", beside.display()));
                beside
            })
        }
    };
    s.key = key;
    s.wsl_distro = envs("ATLAS_WSL_DISTRO").or_else(|| cfg.wsl_distro.clone());
    Ok(s)
}

/// The nearest folder at or above `p` that exists.
fn existing_dir(p: &Path) -> Option<PathBuf> {
    let mut d = if p.is_dir() { Some(p) } else { p.parent() };
    while let Some(x) = d {
        if x.is_dir() {
            return Some(x.to_path_buf());
        }
        d = x.parent();
    }
    None
}

/// `p` with its existing part made canonical, for comparing places.
fn canonical(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut rest = Vec::new();
    let mut base = abs.as_path();
    loop {
        if let Ok(c) = base.canonicalize() {
            return rest.iter().rev().fold(c, |acc: PathBuf, r| acc.join(r));
        }
        match (base.file_name(), base.parent()) {
            (Some(n), Some(parent)) => {
                rest.push(n.to_os_string());
                base = parent;
            }
            _ => return abs,
        }
    }
}

/// Where a private file may sit: not in the atlas checkout `checkout`, not
/// in any clone of the development or public repository, and in any other
/// git work tree only when git ignores it there (and does not track it).
/// `what` names the setting; the contents are never read.
pub fn placement(what: &str, p: &Path, checkout: &Path) -> R {
    let shown = p.display();
    if canonical(p).starts_with(canonical(checkout)) {
        return Err(format!("{what} {shown} is inside the atlas checkout {}: keep private files outside it", checkout.display()));
    }
    let Some(dir) = existing_dir(&std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())) else { return Ok(()) };
    let Ok(top) = git_out(&dir, &["rev-parse", "--show-toplevel"]) else { return Ok(()) };
    let top = PathBuf::from(top);
    if let Some(r) = git_out(&top, &["ls-remote", "--get-url", "origin"]).ok().and_then(|u| github_repo(&u)) {
        if r == DEV_REPO || r == crate::release::DEFAULT_PUBLIC_REPO {
            return Err(format!("{what} {shown} is inside a clone of {r} ({}): keep private files outside it", top.display()));
        }
    }
    let abs = std::path::absolute(p).map_err(|e| format!("{shown}: {e}"))?;
    let path = abs.to_string_lossy().into_owned();
    if status(crate::release_ci::git(&top).args(["ls-files", "--error-unmatch", "--", &path]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()))? {
        return Err(format!("{what} {shown} is tracked by the git work tree {}: keep private files out of every repository", top.display()));
    }
    if !status(crate::release_ci::git(&top).args(["check-ignore", "-q", "--", &path]))? {
        return Err(format!("{what} {shown} is inside the git work tree {} and not ignored there: ignore it there, or move it", top.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------- origin

/// owner/name, lower case, of a GitHub URL in one of the forms git takes
/// (`https://github.com/O/N[.git]`, `git@github.com:O/N[.git]`,
/// `ssh://git@github.com/O/N[.git]`); None for anything else.
pub fn github_repo(url: &str) -> Option<String> {
    let u = url.trim().to_ascii_lowercase();
    let rest = ["https://github.com/", "git@github.com:", "ssh://git@github.com/"].iter().find_map(|p| u.strip_prefix(p))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    (ok(owner) && ok(name)).then(|| format!("{owner}/{name}"))
}

/// The checkout's origin, as git fetches from and pushes to it (`insteadOf`
/// and `pushurl` applied), is the development repository.
pub fn origin_check(root: &Path) -> R {
    let fetch = git_out(root, &["ls-remote", "--get-url", "origin"]).unwrap_or_default();
    let push = git_out(root, &["remote", "get-url", "--push", "origin"]).unwrap_or_default();
    for (what, u) in [("fetches from", fetch), ("pushes to", push)] {
        if github_repo(&u).as_deref() != Some(DEV_REPO) {
            return Err(format!("origin {what} {u:?}, not github.com/{DEV_REPO}: ship runs from a checkout of the development repository"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- GitHub

/// A workflow run as `gh run list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Run {
    /// databaseId.
    pub id: u64,
    /// displayTitle (the release-pr run's name carries the nonce).
    pub title: String,
    /// queued, in_progress, completed, ...
    pub status: String,
    /// success, failure, ... (empty until completed).
    pub conclusion: String,
    /// Its page.
    pub url: String,
}

/// A review as `gh pr view --json reviews` shows it. (`latestReviews`
/// comes back with an empty commit oid, so it cannot say which commit an
/// approval was given on.)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Review {
    /// Its author's login.
    pub author: String,
    /// APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED, PENDING.
    pub state: String,
    /// The commit it was given on.
    pub commit: String,
    /// When it was submitted (ISO 8601, UTC, so it sorts as text).
    pub submitted_at: String,
}

/// A check as `statusCheckRollup` shows it (a status context's state is
/// its conclusion).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckRow {
    /// The check's name (a status's context).
    pub name: String,
    /// COMPLETED, IN_PROGRESS, ...
    pub status: String,
    /// SUCCESS, FAILURE, ...
    pub conclusion: String,
}

/// The release pull request as `gh pr view` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrInfo {
    /// Its number.
    pub number: u64,
    /// OPEN, MERGED or CLOSED.
    pub state: String,
    /// Its page.
    pub url: String,
    /// Its title.
    pub title: String,
    /// The branch it merges into.
    pub base: String,
    /// Its branch.
    pub head_ref: String,
    /// Its branch's commit.
    pub head_oid: String,
    /// From a fork.
    pub cross_repo: bool,
    /// Its author's login (`app/NAME` for an app).
    pub author: String,
    /// Its author is a bot (an app).
    pub author_bot: bool,
    /// The merge commit, once merged.
    pub merge_commit: Option<String>,
    /// Every review, as gh lists them; [`latest_reviews`] reduces them.
    pub reviews: Vec<Review>,
    /// Every check on its head.
    pub checks: Vec<CheckRow>,
}

/// What `gh pr merge` is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    /// owner/name of the public repository.
    pub repo: String,
    /// The pull request.
    pub number: u64,
    /// The head commit the approval was given on: GitHub refuses the merge
    /// if the branch moved.
    pub head: String,
    /// The squash commit's subject.
    pub subject: String,
    /// The squash commit's body: the release commit's own message.
    pub body_file: PathBuf,
}

/// `gh pr merge`'s arguments: squash, pinned to the approved head, with the
/// message given; never `--admin` (the ruleset decides), `--auto` or
/// `--delete-branch` (the branch is the record until the release is
/// signed). No `--author-email`: GitHub refused the public address there
/// ("Invalid email address"), so GitHub picks the squash commit's author,
/// and [`merge_addresses`] checks every address after the merge.
pub fn merge_argv(m: &Merge) -> Vec<String> {
    let mut v: Vec<String> = ["pr", "merge"].iter().map(|s| s.to_string()).collect();
    v.push(m.number.to_string());
    for (k, val) in [("--repo", m.repo.as_str()), ("--match-head-commit", &m.head), ("--subject", &m.subject)] {
        v.push(k.into());
        v.push(val.into());
    }
    v.push("--squash".into());
    v.push("--body-file".into());
    v.push(m.body_file.to_string_lossy().into_owned());
    v
}

/// What ship asks GitHub and does there.
pub trait Hub {
    /// The release of `tag` on the public repository, None when there is none.
    fn release(&self, tag: &str) -> R<Option<Draft>>;
    /// The newest pull request of branch `branch` on the public repository, in any state.
    fn pull(&self, branch: &str) -> R<Option<PrInfo>>;
    /// The commit branch `branch` of the public repository points at, None
    /// when there is no such branch.
    fn branch_tip(&self, branch: &str) -> R<Option<String>>;
    /// Starts release-pr on the development repository's main.
    fn dispatch(&self, version: &str, commit: &str, nonce: &str) -> R;
    /// The development repository's dispatched release-pr runs, newest first.
    fn release_pr_runs(&self) -> R<Vec<Run>>;
    /// The public repository's release workflow runs on `commit`, newest first.
    fn release_runs(&self, commit: &str) -> R<Vec<Run>>;
    /// Watches a run to its end: whether it passed.
    fn watch(&self, repo: &str, id: u64) -> R<bool>;
    /// Merges the pull request.
    fn merge(&self, m: &Merge) -> R;
    /// The pause between two looks for a run or a merge.
    fn wait(&self);
}

/// `gh` as the owner is logged in as, every call with `--repo`.
pub struct GhHub {
    /// owner/name of the public repository.
    pub repo: String,
}

fn gh(args: &[&str]) -> R<String> {
    output(Command::new("gh").args(args))
}

impl Hub for GhHub {
    fn release(&self, tag: &str) -> R<Option<Draft>> {
        crate::release_ci::no_gh_in_tests();
        match gh(&["release", "view", tag, "--repo", &self.repo, "--json", "isDraft,targetCommitish,assets"]) {
            Ok(body) => crate::release_ci::parse_view(&body).map(Some),
            Err(e) if e.contains("release not found") => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn pull(&self, branch: &str) -> R<Option<PrInfo>> {
        crate::release_ci::no_gh_in_tests();
        let n = gh(&["pr", "list", "--repo", &self.repo, "--head", branch, "--state", "all", "--limit", "20", "--json", "number,isCrossRepository", "--jq", "[.[] | select(.isCrossRepository | not)][0].number // empty"])?;
        let n = n.trim();
        if n.is_empty() {
            return Ok(None);
        }
        let body = gh(&[
            "pr",
            "view",
            n,
            "--repo",
            &self.repo,
            "--json",
            "number,state,url,title,baseRefName,headRefName,headRefOid,isCrossRepository,author,mergeCommit,reviews,statusCheckRollup",
        ])?;
        parse_pr(&body).map(Some)
    }
    fn branch_tip(&self, branch: &str) -> R<Option<String>> {
        crate::release_ci::no_gh_in_tests();
        match gh(&["api", &format!("repos/{}/branches/{branch}", self.repo), "--jq", ".commit.sha"]) {
            Ok(sha) if is_sha(sha.trim()) => Ok(Some(sha.trim().to_string())),
            Ok(sha) => Err(format!("gh api: branch {branch} of {} names no commit ({:?})", self.repo, sha.trim())),
            Err(e) if e.contains("HTTP 404") => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn dispatch(&self, version: &str, commit: &str, nonce: &str) -> R {
        crate::release_ci::no_gh_in_tests();
        gh(&[
            "workflow",
            "run",
            "release-pr.yml",
            "--repo",
            DEV_REPO,
            "--ref",
            "main",
            "-f",
            &format!("version={version}"),
            "-f",
            &format!("commit={commit}"),
            "-f",
            &format!("nonce={nonce}"),
        ])
        .map(|_| ())
    }
    fn release_pr_runs(&self) -> R<Vec<Run>> {
        crate::release_ci::no_gh_in_tests();
        parse_runs(&gh(&["run", "list", "--repo", DEV_REPO, "--workflow", "release-pr.yml", "-e", "workflow_dispatch", "--limit", "50", "--json", "databaseId,displayTitle,status,conclusion,url"])?)
    }
    fn release_runs(&self, commit: &str) -> R<Vec<Run>> {
        crate::release_ci::no_gh_in_tests();
        parse_runs(&gh(&["run", "list", "--repo", &self.repo, "--workflow", "release.yml", "-c", commit, "--limit", "20", "--json", "databaseId,displayTitle,status,conclusion,url"])?)
    }
    fn watch(&self, repo: &str, id: u64) -> R<bool> {
        crate::release_ci::no_gh_in_tests();
        status(Command::new("gh").args(["run", "watch", &id.to_string(), "--repo", repo, "--exit-status", "--interval", "10"]))
    }
    fn merge(&self, m: &Merge) -> R {
        crate::release_ci::no_gh_in_tests();
        output(Command::new("gh").args(merge_argv(m))).map(|_| ())
    }
    fn wait(&self) {
        crate::release_ci::no_gh_in_tests();
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

fn s_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Reads `gh run list --json databaseId,displayTitle,status,conclusion,url`.
pub fn parse_runs(body: &str) -> R<Vec<Run>> {
    let v = atlas::json::parse(body).map_err(|e| format!("gh run list: not JSON ({e})"))?;
    v.as_array()
        .ok_or("gh run list: not a list")?
        .iter()
        .map(|r| {
            Ok(Run {
                id: r.get("databaseId").and_then(Value::as_u64).ok_or("gh run list: a run without a databaseId")?,
                title: s_of(r, "displayTitle"),
                status: s_of(r, "status"),
                conclusion: s_of(r, "conclusion"),
                url: s_of(r, "url"),
            })
        })
        .collect()
}

/// Reads `gh pr view --json` output with the fields `GhHub::pull` asks for.
pub fn parse_pr(body: &str) -> R<PrInfo> {
    let v = atlas::json::parse(body).map_err(|e| format!("gh pr view: not JSON ({e})"))?;
    let author = v.get("author");
    let reviews = v.get("reviews").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let checks = v.get("statusCheckRollup").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    Ok(PrInfo {
        number: v.get("number").and_then(Value::as_u64).ok_or("gh pr view: no number")?,
        state: s_of(&v, "state"),
        url: s_of(&v, "url"),
        title: s_of(&v, "title"),
        base: s_of(&v, "baseRefName"),
        head_ref: s_of(&v, "headRefName"),
        head_oid: s_of(&v, "headRefOid"),
        cross_repo: v.get("isCrossRepository").and_then(Value::as_bool).ok_or("gh pr view: no isCrossRepository")?,
        author: author.map(|a| s_of(a, "login")).unwrap_or_default(),
        author_bot: author.and_then(|a| a.get("is_bot")).and_then(Value::as_bool).unwrap_or(false),
        merge_commit: v.get("mergeCommit").filter(|m| !m.is_null()).map(|m| s_of(m, "oid")).filter(|s| !s.is_empty()),
        reviews: reviews
            .iter()
            .map(|r| Review {
                author: r.get("author").map(|a| s_of(a, "login")).unwrap_or_default(),
                state: s_of(r, "state"),
                commit: r.get("commit").map(|c| s_of(c, "oid")).unwrap_or_default(),
                submitted_at: s_of(r, "submittedAt"),
            })
            .collect(),
        checks: checks
            .iter()
            .map(|c| {
                if s_of(c, "__typename") == "StatusContext" {
                    let state = s_of(c, "state");
                    CheckRow { name: s_of(c, "context"), status: if state == "PENDING" || state == "EXPECTED" { "PENDING".into() } else { "COMPLETED".into() }, conclusion: state }
                } else {
                    CheckRow { name: s_of(c, "name"), status: s_of(c, "status"), conclusion: s_of(c, "conclusion") }
                }
            })
            .collect(),
    })
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What makes a pull request not the release's: refused outright.
pub fn not_the_release(p: &PrInfo, tag: &str) -> Vec<String> {
    let mut v = Vec::new();
    let branch = format!("release/{tag}");
    if p.base != "main" {
        v.push(format!("it merges into {:?}, not main", p.base));
    }
    if p.head_ref != branch {
        v.push(format!("its branch is {:?}, not {branch}", p.head_ref));
    }
    if p.cross_repo {
        v.push("it comes from a fork".into());
    }
    if !(p.author_bot && p.author.starts_with("app/")) {
        v.push(format!("its author {:?} is not the release app", p.author));
    }
    if p.title != format!("atlas {tag}") {
        v.push(format!("its title is {:?}, not \"atlas {tag}\"", p.title));
    }
    if !is_sha(&p.head_oid) {
        v.push(format!("its head {:?} is not a commit", p.head_oid));
    }
    v
}

/// Each reviewer's latest review, as GitHub counts review state: the
/// latest by `submittedAt` (the later in the list on a tie), COMMENTED and
/// PENDING reviews left out, so a comment never undoes an approval or a
/// request for changes. In the order each reviewer first shows.
pub fn latest_reviews(reviews: &[Review]) -> Vec<&Review> {
    let mut latest: Vec<&Review> = Vec::new();
    for r in reviews.iter().filter(|r| r.state != "COMMENTED" && r.state != "PENDING") {
        match latest.iter_mut().find(|l| l.author.eq_ignore_ascii_case(&r.author)) {
            Some(l) if r.submitted_at >= l.submitted_at => *l = r,
            Some(_) => {}
            None => latest.push(r),
        }
    }
    latest
}

/// What the pull request still lacks before the merge: the owner's latest
/// review an approval of its head commit, no other reviewer's latest a
/// request for changes, and every check run and passed, the required ones
/// among them.
pub fn not_ready(p: &PrInfo, owner: &str) -> Vec<String> {
    let mut v = Vec::new();
    let latest = latest_reviews(&p.reviews);
    match latest.iter().find(|r| r.author.eq_ignore_ascii_case(owner)) {
        None => v.push(format!("no review from {owner}")),
        Some(r) if r.state != "APPROVED" => v.push(format!("{owner}'s latest review is {}", r.state)),
        Some(r) if r.commit != p.head_oid => v.push(format!("{owner} approved {}, not the head {}: approve the head", short(&r.commit), short(&p.head_oid))),
        Some(_) => {}
    }
    for r in latest.iter().filter(|r| r.state == "CHANGES_REQUESTED" && !r.author.eq_ignore_ascii_case(owner)) {
        v.push(format!("{} requested changes", r.author));
    }
    for n in REQUIRED_CHECKS {
        if !p.checks.iter().any(|c| c.name == n) {
            v.push(format!("check {n} has not run"));
        }
    }
    for c in &p.checks {
        if c.status != "COMPLETED" || c.conclusion != "SUCCESS" {
            v.push(format!("check {} is {}", c.name, if c.status == "COMPLETED" { c.conclusion.to_lowercase() } else { c.status.to_lowercase() }));
        }
    }
    v
}

// ---------------------------------------------------------------- state

/// Run 1's record (`<stage>/ship/state.json`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    /// vX.Y.Z.
    pub tag: String,
    /// The commit dispatched.
    pub commit: String,
    /// The nonce in the run's name.
    pub nonce: String,
    /// The run, once found (0 before).
    pub run: u64,
}

impl State {
    fn read(f: &Path) -> R<Option<State>> {
        if !f.is_file() {
            return Ok(None);
        }
        let v = atlas::json::parse(&read_text(f)?).map_err(|_| format!("{} is not JSON: remove it", f.display()))?;
        Ok(Some(State { tag: s_of(&v, "tag"), commit: s_of(&v, "commit"), nonce: s_of(&v, "nonce"), run: v.get("run").and_then(Value::as_u64).unwrap_or(0) }))
    }

    fn write(&self, f: &Path) -> R {
        mkdir(f.parent().ok_or("no folder")?)?;
        write(
            f,
            atlas::json::object(256, |w| {
                w.field("tag", self.tag.as_str()).field("commit", self.commit.as_str()).field("nonce", self.nonce.as_str()).field("run", &self.run);
            }) + "\n",
        )
    }
}

/// A nonce for the run's name: 16 hex digits of the time and process.
fn nonce() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    atlas::sha256::hex(format!("{t} {}", std::process::id()).as_bytes())[..16].to_string()
}

// ---------------------------------------------------------------- ship

/// What a ship works from.
pub struct Ship<'a> {
    /// vX.Y.Z.
    pub tag: String,
    /// The development checkout it runs from.
    pub root: PathBuf,
    /// Check that origin is the development repository (off only in tests
    /// whose origin is a local folder).
    pub check_origin: bool,
    /// owner/name of the public repository.
    pub repo: String,
    /// Its remote.
    pub remote: String,
    /// `<stage root>/vX.Y.Z`: the check's stamp in `check/`, run 1's state
    /// in `ship/`, the sign step's work in `sign/`.
    pub stage: PathBuf,
    /// The leak denylist.
    pub denylist: PathBuf,
    /// The accepted-hits file.
    pub accepted: Option<PathBuf>,
    /// The signing key's path.
    pub key: Option<PathBuf>,
    /// TRUSTED_KEY_HEX of src/update.rs.
    pub trusted: String,
    /// TEST_KEY_HEX of src/update.rs.
    pub test_key: String,
    /// The public identity.
    pub ident: Ident,
    /// GitHub answers for the public-repository guard.
    pub api: &'a dyn crate::public_repo::Api,
    /// The release calls of the sign step.
    pub rel: &'a dyn Releases,
    /// Everything else ship asks GitHub and does there.
    pub hub: &'a dyn Hub,
    /// The typed prompt of each public act.
    pub confirm: &'a dyn Confirm,
    /// `release check` here (run when no current stamp stands).
    pub check: &'a dyn Fn() -> R<bool>,
}

/// Where a ship stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Done {
    /// The release is out already.
    Published,
    /// Run 1 opened the pull request (its page).
    Opened(String),
    /// The pull request waits for the owner (what it lacks).
    Waiting(Vec<String>),
    /// Published on this commit (the binaries commit).
    Released(String),
}

impl Ship<'_> {
    fn state_file(&self) -> PathBuf {
        self.stage.join("ship").join("state.json")
    }

    fn version(&self) -> &str {
        self.tag.trim_start_matches('v')
    }

    fn owner(&self) -> &str {
        self.repo.split('/').next().unwrap_or_default()
    }

    /// Looks for a run or a merge up to [`TRIES`] times.
    fn find<T>(&self, what: &str, look: impl Fn() -> R<Option<T>>) -> R<Option<T>> {
        for i in 0..TRIES {
            if let Some(r) = look()? {
                return Ok(Some(r));
            }
            if i + 1 < TRIES {
                if i == 0 {
                    say(&format!("waiting for {what} to show"));
                }
                self.hub.wait();
            }
        }
        Ok(None)
    }
}

/// The ship command (see the module docs): the next step for what GitHub
/// holds.
pub fn ship(s: &Ship) -> R<Done> {
    let tag = &s.tag;
    atlas::update::parse_version(s.version()).ok_or(format!("{tag} is not vMAJOR.MINOR.PATCH"))?;
    if s.check_origin {
        origin_check(&s.root)?;
    }
    let draft = s.hub.release(tag)?;
    if draft.as_ref().is_some_and(|d| !d.draft) {
        say(&format!("{tag} is published"));
        return Ok(Done::Published);
    }
    let branch = format!("release/{tag}");
    let pr = s.hub.pull(&branch)?;
    match pr {
        Some(p) if p.state == "MERGED" => {
            let wrong = not_the_release(&p, tag);
            if !wrong.is_empty() {
                return Err(format!("{} is not the release pull request: {}", p.url, wrong.join("; ")));
            }
            let commit = p.merge_commit.clone().filter(|c| is_sha(c)).ok_or(format!("{} is merged, but GitHub names no merge commit", p.url))?;
            merge_addresses(s, &commit)?;
            after_merge(s, &p, draft)
        }
        Some(p) if draft.is_some() => Err(draft_before_merge(s, &p.url)),
        None if draft.is_some() => Err(draft_before_merge(s, "no pull request")),
        Some(p) if p.state == "OPEN" => {
            let wrong = not_the_release(&p, tag);
            if !wrong.is_empty() {
                return Err(format!("{} is not the release pull request: {}", p.url, wrong.join("; ")));
            }
            match stale_export(s)? {
                Some(why) => {
                    say(&format!("{}: {why}. Re-export: run 1 again for HEAD, behind its own `open {tag}` prompt", p.url));
                    open(s, Some(&p))
                }
                None => merge_step(s, &p),
            }
        }
        Some(p) if p.state == "CLOSED" => match s.hub.branch_tip(&branch)? {
            Some(tip) if tip == p.head_oid => Err(format!(
                "the release pull request {} was closed without a merge, and branch {branch} still holds its head {}: nothing more happens to it here. To start {tag} again, delete branch {branch} on {} by hand, then run ship again: run 1 starts over after a new `open` answer",
                p.url,
                short(&p.head_oid),
                s.repo
            )),
            tip => {
                let branch_now = tip.map_or("is gone".to_string(), |t| format!("has moved to {} since", short(&t)));
                say(&format!("the release pull request {} was closed without a merge, and branch {branch} {branch_now}: run 1 starts over", p.url));
                open(s, None)
            }
        },
        Some(p) => Err(format!("the release pull request {} is {:?}", p.url, p.state)),
        None => open(s, None),
    }
}

fn draft_before_merge(s: &Ship, pr: &str) -> String {
    format!(
        "a draft release {} exists on {} before the release pull request merged ({pr}): the release workflow would refuse to build over it. Check it and delete it by hand, then run ship again",
        s.tag, s.repo
    )
}

/// HEAD against run 1's record.
fn as_dispatched(s: &Ship, head: &str) -> R {
    match State::read(&s.state_file())? {
        Some(st) if st.tag == s.tag && st.commit != head => Err(format!(
            "HEAD is {}, but run 1 dispatched {} (the open pull request is that commit's export): check out {} and run again",
            short(head),
            short(&st.commit),
            st.commit
        )),
        Some(st) if st.tag != s.tag => Err(format!("{} is for {}, not {}: remove it", s.state_file().display(), st.tag, s.tag)),
        Some(_) => Ok(()),
        None => {
            say("no state file from run 1 in this checkout: the tree check against the pull request's head stands alone");
            Ok(())
        }
    }
}

/// Why the open pull request may not be HEAD's export, None when it is (or
/// when this checkout holds no state file for the tag: then run 2's checks
/// stand alone). The state file is written before the dispatch, so a commit
/// that matches says nothing until the run it names has passed.
fn stale_export(s: &Ship) -> R<Option<String>> {
    let Some(st) = State::read(&s.state_file())?.filter(|st| st.tag == s.tag) else {
        return Ok(None);
    };
    let head = git_out(&s.root, &["rev-parse", "HEAD"])?;
    if st.commit != head {
        return Ok(Some(format!("run 1 last dispatched {}, but HEAD is {}", short(&st.commit), short(&head))));
    }
    let name = format!("release-pr {} {}", s.tag, st.nonce);
    Ok(match s.hub.release_pr_runs()?.into_iter().find(|r| r.title == name) {
        Some(r) if r.status == "completed" && r.conclusion == "success" => None,
        Some(r) => Some(format!(
            "the release-pr run for {} ({}) is {}, so its head may still be an earlier export",
            short(&head),
            r.url,
            if r.status == "completed" { &r.conclusion } else { &r.status }
        )),
        None => Some(format!("no release-pr run \"{name}\" shows for {}, so its head may still be an earlier export", short(&head))),
    })
}

/// Run 1: open the release pull request, or, with the open one (`reexport`),
/// export HEAD again: the release-pr run replaces the branch, which moves
/// that pull request's head.
fn open(s: &Ship, reexport: Option<&PrInfo>) -> R<Done> {
    let tag = &s.tag;
    let prefix = format!("release-pr {tag} ");
    let runs: Vec<Run> = s.hub.release_pr_runs()?.into_iter().filter(|r| r.title.starts_with(&prefix)).collect();
    if let Some(r) = runs.iter().find(|r| r.status != "completed") {
        say(&format!("a release-pr run for {tag} is {} ({}): watching it, nothing dispatched", r.status, r.url));
        return watched_open(s, r, reexport.is_some());
    }
    let failed = runs.first().filter(|r| r.conclusion != "success");
    if let Some(r) = failed {
        say(&format!(
            "the last release-pr run for {tag} ended {} ({}); its branch may already be on {}. Read the run's page; the next step dispatches again",
            r.conclusion, r.url, s.repo
        ));
    }

    let head = checkout_guard(&s.root, OnMain::Tip)?;
    let version = version_of(&s.root)?;
    if format!("v{version}") != *tag {
        return Err(format!("asked to ship {tag}, but this checkout is v{version} (Cargo.toml)"));
    }
    let last = State::read(&s.state_file())?.filter(|st| st.tag == *tag);
    let moved = last.as_ref().filter(|st| st.commit != head);
    let key = crate::release_check::current_key(&s.root, &s.denylist, s.accepted.as_deref())?.1;
    let stamped = crate::release_check::passed(&s.stage.join("check"), &key);

    say(&format!("plan for {tag} from {}:", short(&head)));
    if let Some(st) = moved {
        say(&format!("  HEAD moved since the last dispatch, of {}: this one is from {}", short(&st.commit), short(&head)));
    }
    if let Some(p) = reexport {
        say(&format!("  re-export: pull request #{} ({}) is open at its head {}, an earlier export; it stays open, and its head moves", p.number, p.url, short(&p.head_oid)));
    }
    if stamped {
        say("  the binary leak check passed already for this commit, denylist and accepted-hits file");
    } else {
        say("  1. `release check` first: three release builds here, tens of minutes");
    }
    say(&format!("  2. dispatch release-pr on {DEV_REPO} for {version} at {head}: it leak-checks the export and a Linux build,"));
    say(&format!("     pushes branch release/{tag} to the PUBLIC repository {} and opens the pull request", s.repo));
    say("  3. watch the run and print the pull request; then review and approve it on GitHub, and run ship again");
    if !stamped {
        (s.check)()?;
    }
    let prompt = match reexport {
        Some(p) => format!(
            "dispatch release-pr for {tag} at {}: the export becomes PUBLIC on {} and replaces branch release/{tag}, so pull request #{}, now at its head {}, moves to the export of {}",
            short(&head),
            s.repo,
            p.number,
            short(&p.head_oid),
            short(&head)
        ),
        None => format!("dispatch release-pr for {tag} at {}: the export becomes PUBLIC on {} as branch release/{tag}", short(&head), s.repo),
    };
    s.confirm.confirm(&prompt, &format!("open {tag}"))?;
    let mut st = State { tag: tag.clone(), commit: head.clone(), nonce: nonce(), run: 0 };
    st.write(&s.state_file())?;
    s.hub.dispatch(&version, &head, &st.nonce)?;
    say(&format!("dispatched release-pr for {tag} (nonce {})", st.nonce));
    let name = format!("{prefix}{}", st.nonce);
    let run = s
        .find("the release-pr run", || Ok(s.hub.release_pr_runs()?.into_iter().find(|r| r.title == name)))?
        .ok_or(format!("the dispatched run \"{name}\" did not show: see https://github.com/{DEV_REPO}/actions/workflows/release-pr.yml"))?;
    st.run = run.id;
    st.write(&s.state_file())?;
    watched_open(s, &run, reexport.is_some())
}

/// Watches a release-pr run; on success, the pull request it opened (or
/// moved, after a re-export).
fn watched_open(s: &Ship, r: &Run, reexport: bool) -> R<Done> {
    if !s.hub.watch(DEV_REPO, r.id)? {
        return Err(format!("the release-pr run failed: {} (run ship again to dispatch again)", r.url));
    }
    let branch = format!("release/{}", s.tag);
    match s.hub.pull(&branch)? {
        Some(p) if p.state == "OPEN" => {
            if reexport {
                say(&format!(
                    "pull request: {}, its head now {}. An approval of an earlier head does not count: review it again, approve it on its new head, then run `cargo xtask release ship {}` again",
                    p.url,
                    short(&p.head_oid),
                    s.tag
                ));
            } else {
                say(&format!("pull request: {}. Review it, approve it on its head commit, then run `cargo xtask release ship {}` again", p.url, s.tag));
            }
            Ok(Done::Opened(p.url))
        }
        _ => Err(format!("the release-pr run passed, but {} has no open pull request from {branch}", s.repo)),
    }
}

/// Run 2's first half: the merge.
fn merge_step(s: &Ship, p: &PrInfo) -> R<Done> {
    let tag = &s.tag;
    let wrong = not_the_release(p, tag);
    if !wrong.is_empty() {
        return Err(format!("{} is not the release pull request: {}", p.url, wrong.join("; ")));
    }
    let head = checkout_guard(&s.root, OnMain::Reachable)?;
    as_dispatched(s, &head)?;
    let missing = not_ready(p, s.owner());
    if !missing.is_empty() {
        say(&format!("{} is not ready to merge:", p.url));
        for m in &missing {
            say(&format!("  {m}"));
        }
        say("nothing changed; run ship again when it is");
        return Ok(Done::Waiting(missing));
    }
    let to_github = !Path::new(&s.remote).is_dir();
    match crate::public_repo::check(s.api, &s.repo, None, &[&s.remote]) {
        Ok(r) => say(&format!("public repo check: {} passes ({} rulesets, {} collaborators)", s.repo, r.rulesets, r.collaborators)),
        Err(e) if to_github => return Err(format!("public repo check: {e}")),
        Err(e) => say(&format!("public repo check: {e} (a merge on GitHub would stop here)")),
    }
    let work = s.stage.join("ship");
    let public = work.join("public");
    clone(&s.remote, &public)?;
    same_code(&s.root, &public, &p.head_oid)?;
    say(&format!("this checkout holds what the pull request's head {} holds", short(&p.head_oid)));
    let message = git_out(&public, &["log", "-1", "--format=%B", &p.head_oid])?;
    let body = message.split_once('\n').map_or("", |(_, b)| b).trim_start_matches('\n');
    let body_file = work.join("merge-body.txt");
    write(&body_file, format!("{}\n", body.trim_end()))?;
    let m = Merge { repo: s.repo.clone(), number: p.number, head: p.head_oid.clone(), subject: format!("atlas {tag} (#{})", p.number), body_file };
    s.confirm.confirm(
        &format!("squash-merge {} into main of {} at its head {}: the release's code reaches the PUBLIC main", p.url, s.repo, short(&p.head_oid)),
        &format!("merge {tag}"),
    )?;
    s.hub.merge(&m)?;
    say(&format!("merged {}", p.url));
    let branch = format!("release/{tag}");
    let merged = s
        .find("the merge", || Ok(s.hub.pull(&branch)?.filter(|q| q.number == p.number && q.state == "MERGED" && q.merge_commit.is_some())))?
        .ok_or(format!("{} does not show as merged: run ship again", p.url))?;
    let commit = merged.merge_commit.clone().unwrap_or_default();
    merge_addresses(s, &commit)?;
    after_merge(s, &merged, None)
}

/// The one address check of a merge, in both merge paths (just after the
/// merge, and the MERGED arm of a later run): a fresh clone of the public
/// repository, and every commit up to `commit`, the merge commit: its
/// author, committer and every `Co-authored-by:` trailer, each one
/// [`crate::release_ci::address_allowed`] (the accepted merges aside). With
/// no `--author-email` on the merge, GitHub picks the author, so this is the
/// only guard. A refused address is counted, never printed.
fn merge_addresses(s: &Ship, commit: &str) -> R {
    let public = s.stage.join("ship").join("public");
    clone(&s.remote, &public)?;
    if git_out(&public, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_err() {
        return Err(format!("the merge commit {} is not in {}: run ship again once it is", short(commit), s.remote));
    }
    let other = other_addresses(&git_out(&public, &["log", ADDRESS_LOG, commit])?, &s.ident.email);
    if !other.is_empty() {
        return Err(format!(
            "the merge commit {} left {} in the public history that may not be there (author, committer or Co-authored-by, in {} or before it). Stop: nothing more is done, and nothing is built or signed on it",
            short(commit),
            refused(&other),
            commit
        ));
    }
    say(&format!("merge commit {}: every address in the public history is allowed, co-authors included", short(commit)));
    Ok(())
}

/// Run 2's second half: the release workflow, the draft, the sign step.
fn after_merge(s: &Ship, p: &PrInfo, draft: Option<Draft>) -> R<Done> {
    let tag = &s.tag;
    let commit = p.merge_commit.clone().filter(|c| is_sha(c)).ok_or(format!("{} is merged, but GitHub names no merge commit", p.url))?;
    let runs_page = format!("https://github.com/{}/actions/workflows/release.yml", s.repo);
    let draft = match draft {
        Some(d) => d,
        None => {
            let run = s.find("the release workflow's run", || Ok(s.hub.release_runs(&commit)?.into_iter().next()))?.ok_or(format!("no release workflow run on the merge commit {}: see {runs_page}", short(&commit)))?;
            let ok = if run.status == "completed" {
                run.conclusion == "success"
            } else {
                say(&format!("the release workflow is {} ({}): watching it", run.status, run.url));
                s.hub.watch(&s.repo, run.id)?
            };
            if !ok {
                return Err(format!(
                    "the release workflow failed: {}. A rebuild is the owner's: delete the draft if one was made, start the release workflow by hand on main, then run ship again",
                    run.url
                ));
            }
            s.hub.release(tag)?.ok_or(format!("the release workflow passed ({}) but there is no draft {tag}: see {runs_page}", run.url))?
        }
    };
    if !draft.draft {
        say(&format!("{tag} is published"));
        return Ok(Done::Published);
    }
    if draft.target != commit {
        return Err(format!(
            "the draft {tag} targets {}, not the release pull request's merge commit {}: a stale or foreign draft. Delete it by hand, rebuild, and run ship again",
            short(&draft.target),
            short(&commit)
        ));
    }
    let head = checkout_guard(&s.root, OnMain::Reachable)?;
    as_dispatched(s, &head)?;
    let signed = sign(&Sign {
        tag: tag.clone(),
        checkout: Some(s.root.clone()),
        repo: s.repo.clone(),
        remote: s.remote.clone(),
        dry_run: false,
        denylist: s.denylist.clone(),
        accepted: s.accepted.clone(),
        key: s.key.clone(),
        trusted: s.trusted.clone(),
        test_key: s.test_key.clone(),
        confirm: s.confirm,
        ident: s.ident.clone(),
        stage: s.stage.join("sign"),
        api: s.api,
        rel: s.rel,
    })?;
    Ok(Done::Released(signed.commit))
}

/// `release ship vX.Y.Z` from the command line.
pub fn run(root: &Path, args: Vec<String>) -> R {
    let mut it = args.into_iter().skip(1);
    let tag = it.next().filter(|t| t.starts_with('v')).ok_or(crate::release_ci::USAGE)?;
    if let Some(a) = it.next() {
        return Err(format!("unknown argument {a}\n{}", crate::release_ci::USAGE));
    }
    if env("ATLAS_GITHUB_STUB").is_some() {
        return Err("ship talks to GitHub; the dry paths are `release --pr --remote PATH` and `release sign vX.Y.Z --remote PATH` with ATLAS_GITHUB_STUB".into());
    }
    let file = config_file(env("ATLAS_RELEASE_CONFIG"))?;
    let cfg = match &file {
        Some(f) => parse_config(&read_text(f)?)?,
        None => Config::default(),
    };
    let set = resolve(&cfg, file.as_deref(), &env)?;
    if let Some(f) = &file {
        placement("the release config", f, root)?;
    }
    for (what, p) in [("the denylist", Some(&set.denylist)), ("the accepted-hits file", set.accepted.as_ref()), ("the signing key", set.key.as_ref())] {
        if let Some(p) = p {
            placement(what, p, root)?;
        }
    }
    for l in &set.sources {
        say(l);
    }
    if std::fs::File::open(&set.denylist).is_err() {
        return Err("the denylist is not readable".into());
    }
    if let Some(d) = &set.wsl_distro {
        std::env::set_var("ATLAS_WSL_DISTRO", d);
    }
    let update_rs = read_text(&root.join("src/update.rs"))?;
    let (Some(trusted), Some(test_key)) = (crate::release::key_const(&update_rs, "TRUSTED_KEY_HEX"), crate::release::key_const(&update_rs, "TEST_KEY_HEX")) else {
        return Err("cannot read TRUSTED_KEY_HEX and TEST_KEY_HEX from src/update.rs".into());
    };
    let repo = env("ATLAS_PUBLIC_REPO").unwrap_or_else(|| crate::release::DEFAULT_PUBLIC_REPO.into());
    let stage = crate::release_ci::stage_root(root).join(&tag);
    let (denylist, accepted) = (set.denylist.clone(), set.accepted.clone());
    let check = || crate::release_check::check_here(root, &tag, &denylist, accepted.as_deref(), false);
    let api = crate::public_repo::api();
    let rel = crate::release_ci::GhReleases { repo: repo.clone() };
    let hub = GhHub { repo: repo.clone() };
    let s = Ship {
        tag: tag.clone(),
        root: root.to_path_buf(),
        check_origin: true,
        remote: format!("https://github.com/{repo}.git"),
        repo,
        stage,
        denylist: set.denylist,
        accepted: set.accepted,
        key: set.key,
        trusted,
        test_key,
        ident: Ident::from_env(),
        api: api.as_ref(),
        rel: &rel,
        hub: &hub,
        confirm: &crate::release_check::Tty,
        check: &check,
    };
    match ship(&s)? {
        Done::Published => say(&format!("{tag} is out; nothing to do")),
        Done::Opened(u) => say(&format!("run 1 done: {u}")),
        Done::Waiting(_) => {}
        Done::Released(c) => say(&format!("released {tag} on {c}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_ci::tests::{commit_files, denylist, dev_checkout, fixture, openssl, sh, temp_key, tmp, uploaded, Fixture, No, NoApi, REPO};
    use crate::release_ci::StubReleases;
    use std::cell::{Cell, RefCell};

    const TAG: &str = "v1.2.3";

    /// GitHub as ship sees it, in memory; the draft comes from the stub
    /// folder the sign step reads.
    #[derive(Default)]
    struct FakeHub {
        stub: PathBuf,
        /// The draft is not there until the release workflow is watched.
        hide_draft: Cell<bool>,
        pr: RefCell<Option<PrInfo>>,
        /// The tip of release/v1.2.3 on the public repository (None: no branch).
        branch: RefCell<Option<String>>,
        /// The pull request a watched release-pr run opens.
        opens: RefCell<Option<PrInfo>>,
        pr_runs: RefCell<Vec<Run>>,
        /// Looks at the release-pr runs that see none yet.
        hide_runs: Cell<u32>,
        rel_runs: RefCell<Vec<Run>>,
        watch_fails: Cell<bool>,
        calls: RefCell<Vec<String>>,
        waits: Cell<u32>,
    }

    impl FakeHub {
        fn new(stub: &Path) -> FakeHub {
            FakeHub { stub: stub.to_path_buf(), ..FakeHub::default() }
        }
        fn count(&self, what: &str) -> usize {
            self.calls.borrow().iter().filter(|c| c.starts_with(what)).count()
        }
    }

    impl Hub for FakeHub {
        fn release(&self, tag: &str) -> R<Option<Draft>> {
            if self.hide_draft.get() || !self.stub.join(format!("release-{tag}")).join("view.json").is_file() {
                return Ok(None);
            }
            StubReleases(self.stub.clone()).view(tag).map(Some)
        }
        fn pull(&self, branch: &str) -> R<Option<PrInfo>> {
            assert_eq!(branch, "release/v1.2.3");
            Ok(self.pr.borrow().clone())
        }
        fn branch_tip(&self, branch: &str) -> R<Option<String>> {
            assert_eq!(branch, "release/v1.2.3");
            Ok(self.branch.borrow().clone())
        }
        fn dispatch(&self, version: &str, commit: &str, nonce: &str) -> R {
            self.calls.borrow_mut().push(format!("dispatch {version} {commit} {nonce}"));
            self.pr_runs.borrow_mut().insert(0, Run { id: 7, title: format!("release-pr v{version} {nonce}"), status: "queued".into(), conclusion: String::new(), url: "https://example.invalid/run/7".into() });
            Ok(())
        }
        fn release_pr_runs(&self) -> R<Vec<Run>> {
            if self.hide_runs.get() > 0 {
                self.hide_runs.set(self.hide_runs.get() - 1);
                return Ok(Vec::new());
            }
            Ok(self.pr_runs.borrow().clone())
        }
        fn release_runs(&self, commit: &str) -> R<Vec<Run>> {
            self.calls.borrow_mut().push(format!("release-runs {commit}"));
            Ok(self.rel_runs.borrow().clone())
        }
        fn watch(&self, repo: &str, id: u64) -> R<bool> {
            self.calls.borrow_mut().push(format!("watch {repo} {id}"));
            if repo == DEV_REPO {
                // A watched run ends, as `gh run list` shows it after.
                for r in self.pr_runs.borrow_mut().iter_mut().filter(|r| r.id == id) {
                    r.status = "completed".into();
                    r.conclusion = if self.watch_fails.get() { "failure" } else { "success" }.into();
                }
            }
            if self.watch_fails.get() {
                return Ok(false);
            }
            if repo == DEV_REPO {
                if let Some(p) = self.opens.borrow_mut().take() {
                    *self.pr.borrow_mut() = Some(p);
                }
            } else {
                self.hide_draft.set(false);
            }
            Ok(true)
        }
        fn merge(&self, m: &Merge) -> R {
            self.calls.borrow_mut().push(format!("merge {} {}", m.number, m.head));
            let mut p = self.pr.borrow_mut();
            let p = p.as_mut().unwrap();
            p.state = "MERGED".into();
            p.merge_commit = Some(m.head.clone());
            Ok(())
        }
        fn wait(&self) {
            self.waits.set(self.waits.get() + 1);
        }
    }

    /// A person who types the given answers, one per prompt, in order.
    #[derive(Default)]
    struct Typed {
        answers: RefCell<Vec<&'static str>>,
        asked: RefCell<Vec<String>>,
        prompts: RefCell<Vec<String>>,
    }

    impl Typed {
        fn new(answers: &[&'static str]) -> Typed {
            Typed { answers: RefCell::new(answers.to_vec()), ..Typed::default() }
        }
    }

    impl Confirm for Typed {
        fn confirm(&self, prompt: &str, expect: &str) -> R {
            self.asked.borrow_mut().push(expect.to_string());
            self.prompts.borrow_mut().push(prompt.to_string());
            let mut a = self.answers.borrow_mut();
            let got = if a.is_empty() { "" } else { a.remove(0) };
            if got == expect {
                Ok(())
            } else {
                Err(format!("refused at `{expect}`; nothing was changed"))
            }
        }
    }

    /// A release pull request ready to merge at `head`.
    fn ready(head: &str) -> PrInfo {
        PrInfo {
            number: 5,
            state: "OPEN".into(),
            url: "https://github.com/acme/tool/pull/5".into(),
            title: "atlas v1.2.3".into(),
            base: "main".into(),
            head_ref: "release/v1.2.3".into(),
            head_oid: head.into(),
            cross_repo: false,
            author: "app/atlas-release".into(),
            author_bot: true,
            merge_commit: None,
            reviews: vec![Review { author: "acme".into(), state: "APPROVED".into(), commit: head.into(), submitted_at: "2026-11-10T10:00:00Z".into() }],
            checks: REQUIRED_CHECKS.iter().map(|n| CheckRow { name: n.to_string(), status: "COMPLETED".into(), conclusion: "SUCCESS".into() }).collect(),
        }
    }

    fn merged(head: &str, merge: &str) -> PrInfo {
        PrInfo { state: "MERGED".into(), merge_commit: Some(merge.into()), ..ready(head) }
    }

    fn run_of(status: &str, conclusion: &str) -> Run {
        Run { id: 9, title: "release".into(), status: status.into(), conclusion: conclusion.into(), url: "https://example.invalid/run/9".into() }
    }

    /// A public repository with the draft of v1.2.3 (the fixture) and a
    /// development checkout holding its source.
    struct World {
        fx: Fixture,
        dev: PathBuf,
        rel: StubReleases,
        hub: FakeHub,
        checks: Cell<u32>,
    }

    fn world(name: &str) -> World {
        let fx = fixture(name);
        let dev = dev_checkout(&fx.d);
        let rel = StubReleases(fx.stub.clone());
        let hub = FakeHub::new(&fx.stub);
        World { fx, dev, rel, hub, checks: Cell::new(0) }
    }

    impl World {
        /// No draft, as before the merge.
        fn no_draft(&self) {
            std::fs::rename(self.fx.stub.join("release-v1.2.3"), self.fx.d.join("held-release")).unwrap();
        }
        /// The draft shows once the release workflow is watched.
        fn hide_draft_until_built(&self) {
            self.hub.hide_draft.set(true);
        }
        fn head(&self) -> String {
            sh(&self.dev, &["rev-parse", "HEAD"])
        }
        fn stage(&self) -> PathBuf {
            self.fx.d.join("stage-ship")
        }
        /// A current pass stamp of `release check` for HEAD.
        fn stamp(&self) {
            let key = crate::release_check::current_key(&self.dev, &denylist(&self.fx.d), None).unwrap().1;
            std::fs::create_dir_all(self.stage().join("check")).unwrap();
            std::fs::write(self.stage().join("check/pass.txt"), key).unwrap();
        }
        fn state(&self) -> Option<State> {
            State::read(&self.stage().join("ship/state.json")).unwrap()
        }
        fn ship(&self, confirm: &dyn Confirm, key: Option<(PathBuf, String)>) -> R<Done> {
            let check = || {
                self.checks.set(self.checks.get() + 1);
                Ok(true)
            };
            let (key, trusted) = match key {
                Some((k, h)) => (Some(k), h),
                None => (None, "1".repeat(64)),
            };
            ship(&Ship {
                tag: TAG.into(),
                root: self.dev.clone(),
                check_origin: false,
                repo: REPO.into(),
                remote: self.fx.bare.to_string_lossy().replace('\\', "/"),
                stage: self.stage(),
                denylist: denylist(&self.fx.d),
                accepted: None,
                key,
                trusted,
                test_key: "2".repeat(64),
                ident: Ident { name: "P".into(), email: "pub@example.invalid".into() },
                api: &NoApi,
                rel: &self.rel,
                hub: &self.hub,
                confirm,
                check: &check,
            })
        }
        /// A file standing for the key: the preflight passes on it.
        fn dummy_key(&self) -> (PathBuf, String) {
            let k = self.fx.d.join("dummy.pem");
            std::fs::write(&k, "not a key").unwrap();
            (k, "1".repeat(64))
        }
        fn main(&self) -> String {
            sh(&self.fx.bare, &["rev-parse", "main"])
        }
        fn done(self) {
            let _ = std::fs::remove_dir_all(&self.fx.d);
        }
    }

    #[test]
    fn row_1_a_published_release_is_left_alone() {
        let w = world("ship-r1");
        std::fs::write(w.fx.stub.join("release-v1.2.3/published.txt"), "x").unwrap();
        assert_eq!(w.ship(&No, None).unwrap(), Done::Published);
        assert!(w.hub.calls.borrow().is_empty());
        w.done();
    }

    #[test]
    fn row_2_run_1_checks_asks_once_dispatches_and_watches() {
        let w = world("ship-r2");
        w.no_draft();
        let head = w.head();
        // Refused at the terminal: nothing dispatched, no state written.
        let e = w.ship(&No, None).unwrap_err();
        assert_eq!(e, "refused at `open v1.2.3`; nothing was changed");
        assert_eq!(w.hub.count("dispatch"), 0);
        assert!(w.state().is_none());
        assert_eq!(w.checks.get(), 1, "no stamp: the check runs before the prompt");
        // Another act's answer does not open it.
        assert!(w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err().contains("refused at `open v1.2.3`"));
        assert_eq!(w.hub.count("dispatch"), 0);
        // Typed: one dispatch of HEAD, the run found by its nonce after a
        // look that saw nothing, watched, and the pull request printed.
        w.stamp();
        *w.hub.opens.borrow_mut() = Some(ready(&head));
        // Two blind looks: the state look before the plan, then the first
        // look after the dispatch, so the search waits once.
        w.hub.hide_runs.set(2);
        let t = Typed::new(&["open v1.2.3"]);
        assert_eq!(w.ship(&t, None).unwrap(), Done::Opened("https://github.com/acme/tool/pull/5".into()));
        assert_eq!(w.checks.get(), 2, "a current stamp: no check");
        assert_eq!(*t.asked.borrow(), ["open v1.2.3"]);
        let st = w.state().unwrap();
        assert_eq!((st.tag.as_str(), st.commit.as_str(), st.run), (TAG, head.as_str(), 7));
        assert_eq!(*w.hub.calls.borrow(), [format!("dispatch 1.2.3 {head} {}", st.nonce), format!("watch {DEV_REPO} 7")]);
        assert_eq!(w.hub.waits.get(), 1);
        w.done();
    }

    #[test]
    fn run_1_refuses_a_stale_checkout_or_another_version() {
        let w = world("ship-r2-guard");
        w.no_draft();
        // Behind origin's main: run 1 wants the tip.
        crate::release_ci::tests::fall_behind(&w.dev);
        assert!(w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap_err().contains(", not origin/main ("));
        sh(&w.dev, &["reset", "-q", "--hard", "origin/main"]);
        std::fs::write(w.dev.join("README.md"), "changed\n").unwrap();
        assert!(w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap_err().starts_with("uncommitted changes in "));
        sh(&w.dev, &["checkout", "-q", "--", "."]);
        commit_files(&w.dev, &[("Cargo.toml", "[package]\nname = \"x\"\nversion = \"1.2.4\"\n")], "bump");
        assert!(w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap_err().starts_with("plugin.json version 1.2.3 is not Cargo.toml's 1.2.4"));
        commit_files(&w.dev, &[(".claude-plugin/plugin.json", "{\n  \"version\": \"1.2.4\"\n}\n")], "bump");
        assert_eq!(w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap_err(), "asked to ship v1.2.3, but this checkout is v1.2.4 (Cargo.toml)");
        assert_eq!(w.hub.count("dispatch"), 0);
        w.done();
    }

    #[test]
    fn row_3_a_run_in_progress_is_watched_not_dispatched_again() {
        let w = world("ship-r3");
        w.no_draft();
        w.hub.pr_runs.borrow_mut().push(Run { id: 4, title: "release-pr v1.2.3 abc".into(), status: "in_progress".into(), ..Run::default() });
        *w.hub.opens.borrow_mut() = Some(ready(&w.head()));
        assert!(matches!(w.ship(&No, None).unwrap(), Done::Opened(_)));
        assert_eq!(w.hub.count("dispatch"), 0);
        assert_eq!(*w.hub.calls.borrow(), [format!("watch {DEV_REPO} 4")]);
        w.done();
    }

    #[test]
    fn row_4_a_failed_run_is_dispatched_again_only_after_a_new_prompt() {
        let w = world("ship-r4");
        w.no_draft();
        w.stamp();
        w.hub.pr_runs.borrow_mut().push(Run { id: 4, title: "release-pr v1.2.3 abc".into(), status: "completed".into(), conclusion: "failure".into(), ..Run::default() });
        assert!(w.ship(&No, None).unwrap_err().contains("refused at `open v1.2.3`"));
        assert_eq!(w.hub.count("dispatch"), 0);
        *w.hub.opens.borrow_mut() = Some(ready(&w.head()));
        assert!(matches!(w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap(), Done::Opened(_)));
        assert_eq!(w.hub.count("dispatch"), 1);
        w.done();
        // A run that fails is reported by its page, never its log.
        let w = world("ship-r4-fail");
        w.no_draft();
        w.stamp();
        w.hub.watch_fails.set(true);
        let e = w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap_err();
        assert_eq!(e, "the release-pr run failed: https://example.invalid/run/7 (run ship again to dispatch again)");
        w.done();
    }

    #[test]
    fn row_5_a_pull_request_not_ready_waits_and_changes_nothing() {
        let w = world("ship-r5");
        w.no_draft();
        let head = w.head();
        // The approval says APPROVED, but on an older head.
        let mut p = ready(&head);
        p.reviews[0].commit = "b".repeat(40);
        *w.hub.pr.borrow_mut() = Some(p);
        match w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap() {
            Done::Waiting(m) => assert_eq!(m, [format!("acme approved bbbbbbbbbbbb, not the head {}: approve the head", short(&head))]),
            d => panic!("{d:?}"),
        }
        // A check not green.
        let mut p = ready(&head);
        p.checks[2].conclusion = "FAILURE".into();
        *w.hub.pr.borrow_mut() = Some(p);
        assert_eq!(w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap(), Done::Waiting(vec!["check test (macos-latest) is failure".into()]));
        assert_eq!(w.hub.count("merge"), 0);
        // Not the release app's pull request: refused outright.
        *w.hub.pr.borrow_mut() = Some(PrInfo { author: "someone".into(), author_bot: false, ..ready(&head) });
        assert!(w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err().contains("its author \"someone\" is not the release app"));
        assert_eq!(w.hub.count("merge"), 0);
        w.done();
    }

    #[test]
    fn readiness_names_every_gap() {
        let head = "a".repeat(40);
        assert!(not_ready(&ready(&head), "acme").is_empty());
        assert!(not_ready(&ready(&head), "ACME").is_empty(), "the owner's login, case aside");
        let mut p = ready(&head);
        p.reviews = vec![Review { author: "helper".into(), state: "APPROVED".into(), commit: head.clone(), ..Review::default() }];
        assert_eq!(not_ready(&p, "acme"), ["no review from acme"]);
        p.reviews.push(Review { author: "acme".into(), state: "COMMENTED".into(), commit: head.clone(), ..Review::default() });
        assert_eq!(not_ready(&p, "acme"), ["no review from acme"], "a comment is not a review state");
        let mut p = ready(&head);
        p.reviews.push(Review { author: "helper".into(), state: "CHANGES_REQUESTED".into(), commit: head.clone(), ..Review::default() });
        assert_eq!(not_ready(&p, "acme"), ["helper requested changes"]);
        let mut p = ready(&head);
        p.checks.remove(4);
        p.checks.push(CheckRow { name: "extra".into(), status: "IN_PROGRESS".into(), conclusion: String::new() });
        assert_eq!(not_ready(&p, "acme"), ["check clippy (ubuntu-latest) has not run", "check extra is in_progress"]);
        // Not the release's pull request at all.
        assert!(not_the_release(&ready(&head), TAG).is_empty());
        let wrong = PrInfo { base: "dev".into(), head_ref: "x".into(), cross_repo: true, author: "someone".into(), author_bot: false, title: "atlas".into(), head_oid: "z".into(), ..ready(&head) };
        assert_eq!(not_the_release(&wrong, TAG).len(), 6);
        assert_eq!(not_the_release(&PrInfo { author_bot: false, ..ready(&head) }, TAG).len(), 1, "a person's login under app/ is not the app");
        assert_eq!(not_the_release(&PrInfo { author: "helper[bot]".into(), ..ready(&head) }, TAG).len(), 1, "a bot that is not an app");
    }

    #[test]
    fn row_6_the_checkout_must_hold_the_pull_requests_head() {
        let w = world("ship-r6");
        w.no_draft();
        *w.hub.pr.borrow_mut() = Some(ready(&w.fx.target));
        commit_files(&w.dev, &[("xtask/src/main.rs", "fn other() {}\n")], "other code");
        let e = w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err();
        assert!(e.starts_with("this checkout is not the release's source") && e.contains("(xtask/src/main.rs)"), "{e}");
        assert_eq!(w.hub.count("merge"), 0);
        w.done();
    }

    #[test]
    fn row_7_the_merge_is_typed_pinned_to_the_head_and_never_repeated() {
        let w = world("ship-r7");
        w.hide_draft_until_built();
        *w.hub.pr.borrow_mut() = Some(ready(&w.fx.target));
        w.hub.rel_runs.borrow_mut().push(run_of("in_progress", ""));
        // Refused, or another act's answer: no merge.
        assert_eq!(w.ship(&No, None).unwrap_err(), "refused at `merge v1.2.3`; nothing was changed");
        assert!(w.ship(&Typed::new(&["publish v1.2.3"]), None).unwrap_err().contains("refused at `merge v1.2.3`"));
        assert_eq!(w.hub.count("merge"), 0);
        // Typed: one merge at the head; the publish prompt is its own.
        let t = Typed::new(&["merge v1.2.3", "merge v1.2.3"]);
        let e = w.ship(&t, Some(w.dummy_key())).unwrap_err();
        assert_eq!(*w.hub.calls.borrow(), [format!("merge 5 {}", w.fx.target), format!("release-runs {}", w.fx.target), "watch acme/tool 9".to_string()]);
        if openssl().is_some() {
            assert_eq!(e, "refused at `publish v1.2.3`; nothing was changed");
            assert_eq!(*t.asked.borrow(), ["merge v1.2.3", "publish v1.2.3"]);
        } else {
            assert!(e.contains("openssl"), "{e}");
        }
        assert!(!uploaded(&w.fx));
        assert_eq!(w.main(), w.fx.target);
        // Run again: merged, so never merged again; the build is done.
        w.hub.rel_runs.borrow_mut()[0] = run_of("completed", "success");
        let _ = w.ship(&No, Some(w.dummy_key()));
        assert_eq!(w.hub.count("merge"), 1);
        w.done();
    }

    /// Run 2 wants a commit of main, and stops after the merge on another
    /// address in the public history.
    #[test]
    fn row_7_guards_the_checkout_and_the_merge_commits_addresses() {
        let w = world("ship-r7-guard");
        w.hide_draft_until_built();
        // A commit not on origin's main, with the same code: refused.
        sh(&w.dev, &["-c", "user.name=D", "-c", "user.email=dev@example.invalid", "commit", "-q", "--allow-empty", "-m", "local only"]);
        *w.hub.pr.borrow_mut() = Some(ready(&w.fx.target));
        let e = w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err();
        assert!(e.contains("is not on origin/main"), "{e}");
        assert_eq!(w.hub.count("merge"), 0);
        sh(&w.dev, &["reset", "-q", "--hard", "origin/main"]);
        // A head authored under another address: merged, then stopped,
        // without the address printed.
        let x = w.fx.d.join("other-author");
        sh(&w.fx.d, &["clone", "-q", &w.fx.bare.to_string_lossy(), x.to_str().unwrap()]);
        sh(&x, &["-c", "user.name=P", "-c", "user.email=pub@example.invalid", "commit", "-q", "--allow-empty", "--author", "O <o@other.invalid>", "-m", "atlas v1.2.3"]);
        sh(&x, &["push", "-q", "origin", "HEAD:refs/heads/release/v1.2.3"]);
        let head = sh(&x, &["rev-parse", "HEAD"]);
        *w.hub.pr.borrow_mut() = Some(ready(&head));
        w.hub.rel_runs.borrow_mut().push(run_of("in_progress", ""));
        let e = w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err();
        assert!(e.contains(&format!("merge commit {}", short(&head))) && e.contains(" 1 address ") && !e.contains("o@other.invalid"), "{e}");
        assert_eq!((w.hub.count("merge"), w.hub.count("release-runs")), (1, 0));
        w.done();
    }

    /// A co-author the public history may not hold stops run 2 after the
    /// merge, as an author does.
    #[test]
    fn row_7_a_co_author_trailer_is_checked_after_the_merge() {
        let w = world("ship-r7-trailer");
        w.hide_draft_until_built();
        let x = w.fx.d.join("co-author");
        sh(&w.fx.d, &["clone", "-q", &w.fx.bare.to_string_lossy(), x.to_str().unwrap()]);
        sh(&x, &["-c", "user.name=P", "-c", "user.email=pub@example.invalid", "commit", "-q", "--allow-empty", "-m", "atlas v1.2.3\n\nCo-authored-by: O <o@other.invalid>"]);
        sh(&x, &["push", "-q", "origin", "HEAD:refs/heads/release/v1.2.3"]);
        let head = sh(&x, &["rev-parse", "HEAD"]);
        *w.hub.pr.borrow_mut() = Some(ready(&head));
        w.hub.rel_runs.borrow_mut().push(run_of("in_progress", ""));
        let e = w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err();
        assert!(e.contains(&format!("merge commit {}", short(&head))) && e.contains(" 1 address ") && !e.contains("o@other.invalid"), "{e}");
        assert_eq!((w.hub.count("merge"), w.hub.count("release-runs")), (1, 0));
        w.done();
    }

    #[test]
    fn row_8_a_closed_pull_request_whose_branch_holds_its_head_is_refused() {
        let w = world("ship-r8");
        w.no_draft();
        *w.hub.pr.borrow_mut() = Some(PrInfo { state: "CLOSED".into(), ..ready(&w.fx.target) });
        *w.hub.branch.borrow_mut() = Some(w.fx.target.clone());
        let e = w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap_err();
        assert!(e.contains("was closed without a merge"), "{e}");
        assert!(e.contains("delete branch release/v1.2.3 on acme/tool by hand, then run ship again: run 1 starts over after a new `open` answer"), "{e}");
        assert!(w.hub.calls.borrow().is_empty());
        w.done();
    }

    /// The way out of a closed release pull request: once its branch is
    /// gone, or holds a later export than the closed head, run 1 starts over,
    /// behind its own prompt.
    #[test]
    fn row_8_a_closed_pull_request_without_its_branch_starts_run_1_again() {
        for (name, tip) in [("ship-r8-gone", None), ("ship-r8-moved", Some("d".repeat(40)))] {
            let w = world(name);
            w.no_draft();
            w.stamp();
            *w.hub.pr.borrow_mut() = Some(PrInfo { state: "CLOSED".into(), ..ready(&w.fx.target) });
            *w.hub.branch.borrow_mut() = tip;
            let e = w.ship(&No, None).unwrap_err();
            assert!(e.contains("refused at `open v1.2.3`"), "{name}: {e}");
            assert_eq!(w.hub.count("dispatch"), 0);
            assert!(w.state().is_none());
            *w.hub.opens.borrow_mut() = Some(PrInfo { number: 6, url: "https://github.com/acme/tool/pull/6".into(), ..ready(&w.head()) });
            assert_eq!(w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap(), Done::Opened("https://github.com/acme/tool/pull/6".into()), "{name}");
            assert_eq!(w.hub.count("dispatch"), 1);
            assert_eq!(w.state().unwrap().commit, w.head());
            w.done();
        }
    }

    #[test]
    fn row_9_after_a_merge_the_release_build_is_watched_never_merged_again() {
        let w = world("ship-r9");
        w.no_draft();
        // A merged pull request that is not the release's is refused.
        *w.hub.pr.borrow_mut() = Some(PrInfo { base: "dev".into(), ..merged(&w.fx.target, &w.fx.target) });
        assert!(w.ship(&No, None).unwrap_err().contains("is not the release pull request: it merges into \"dev\""));
        *w.hub.pr.borrow_mut() = Some(merged(&w.fx.target, &w.fx.target));
        w.hub.rel_runs.borrow_mut().push(run_of("in_progress", ""));
        let e = w.ship(&No, None).unwrap_err();
        assert!(e.starts_with("the release workflow passed (https://example.invalid/run/9) but there is no draft v1.2.3"), "{e}");
        assert_eq!((w.hub.count("watch"), w.hub.count("merge"), w.hub.count("dispatch")), (1, 0, 0));
        w.done();
    }

    #[test]
    fn row_10_a_failed_or_missing_release_build_is_refused() {
        let w = world("ship-r10");
        w.no_draft();
        *w.hub.pr.borrow_mut() = Some(merged(&w.fx.target, &w.fx.target));
        w.hub.rel_runs.borrow_mut().push(run_of("completed", "failure"));
        assert!(w.ship(&No, None).unwrap_err().starts_with("the release workflow failed: https://example.invalid/run/9."));
        w.hub.rel_runs.borrow_mut().clear();
        let e = w.ship(&No, None).unwrap_err();
        assert!(e.starts_with("no release workflow run on the merge commit") && e.ends_with("actions/workflows/release.yml"), "{e}");
        assert_eq!(w.hub.waits.get(), TRIES - 1, "a bounded wait");
        assert_eq!((w.hub.count("watch"), w.hub.count("merge")), (0, 0));
        w.done();
    }

    #[test]
    fn row_11_a_draft_not_on_the_merge_commit_is_refused_before_any_download() {
        let w = world("ship-r11");
        // The public main is the draft's commit, so only the merge commit tells.
        let other = public_commit(&w, "other", "pub@example.invalid", "pub@example.invalid", "atlas v1.2.3 (#5)");
        *w.hub.pr.borrow_mut() = Some(merged(&w.fx.target, &other));
        let e = w.ship(&Typed::new(&["publish v1.2.3"]), None).unwrap_err();
        assert!(e.contains("a stale or foreign draft"), "{e}");
        assert!(!w.stage().join("sign").exists());
        w.done();
    }

    #[test]
    fn row_12_and_13_the_draft_on_the_merge_commit_is_signed_after_one_prompt_and_resumes() {
        let w = world("ship-r12");
        *w.hub.pr.borrow_mut() = Some(merged(&w.fx.target, &w.fx.target));
        let Some(key) = temp_key(&w.fx.d) else {
            eprintln!("openssl not found: the publishing half skipped");
            w.done();
            return;
        };
        // Refused at the prompt, after every check: nothing out.
        let e = w.ship(&No, Some(key.clone())).unwrap_err();
        assert_eq!(e, "refused at `publish v1.2.3`; nothing was changed");
        assert!(!uploaded(&w.fx));
        assert_eq!(w.main(), w.fx.target);
        // Typed: signed, pushed and published on the binaries commit.
        let t = Typed::new(&["publish v1.2.3"]);
        let Done::Released(commit) = w.ship(&t, Some(key.clone())).unwrap() else { panic!("not released") };
        assert_eq!(w.main(), commit);
        assert_eq!(std::fs::read_to_string(w.fx.stub.join("release-v1.2.3/published.txt")).unwrap(), commit);
        // Row 13: a publish that failed after the push resumes on the
        // binaries commit, still behind the prompt.
        std::fs::remove_file(w.fx.stub.join("release-v1.2.3/published.txt")).unwrap();
        assert!(w.ship(&No, Some(key.clone())).unwrap_err().contains("refused at `publish v1.2.3`"));
        assert_eq!(w.ship(&Typed::new(&["publish v1.2.3"]), Some(key)).unwrap(), Done::Released(commit.clone()));
        assert_eq!(w.main(), commit);
        assert_eq!((w.hub.count("merge"), w.hub.count("dispatch")), (0, 0));
        w.done();
    }

    #[test]
    fn row_14_a_draft_before_the_merge_is_refused() {
        let w = world("ship-r14");
        let e = w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap_err();
        assert!(e.contains("before the release pull request merged (no pull request)"), "{e}");
        *w.hub.pr.borrow_mut() = Some(ready(&w.fx.target));
        assert!(w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err().contains("before the release pull request merged (https://github.com/acme/tool/pull/5)"));
        assert!(w.hub.calls.borrow().is_empty());
        w.done();
    }

    #[test]
    fn row_15_run_2_wants_the_commit_run_1_dispatched() {
        let w = world("ship-r15");
        w.no_draft();
        let head = w.head();
        let old = "c".repeat(40);
        let new_export = "e".repeat(40);
        // The open pull request is the export of an older commit, approved there.
        *w.hub.pr.borrow_mut() = Some(ready(&w.fx.target));
        let st = State { tag: TAG.into(), commit: old.clone(), nonce: "n".into(), run: 7 };
        st.write(&w.stage().join("ship/state.json")).unwrap();
        w.stamp();
        // Re-export, never a merge: another act's answer changes nothing.
        let e = w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err();
        assert!(e.contains("refused at `open v1.2.3`"), "{e}");
        assert_eq!((w.hub.count("dispatch"), w.hub.count("merge")), (0, 0));
        assert_eq!(w.state().unwrap(), st, "state.json unchanged");
        // Typed: one dispatch of HEAD with a new nonce; the pull request's head moves.
        *w.hub.opens.borrow_mut() = Some(PrInfo { head_oid: new_export.clone(), ..ready(&w.fx.target) });
        let t = Typed::new(&["open v1.2.3"]);
        assert_eq!(w.ship(&t, None).unwrap(), Done::Opened("https://github.com/acme/tool/pull/5".into()));
        assert_eq!(*t.asked.borrow(), ["open v1.2.3"]);
        let prompt = t.prompts.borrow()[0].clone();
        assert!(prompt.contains(&format!("#5, now at its head {}, moves to the export of {}", short(&w.fx.target), short(&head))), "{prompt}");
        assert!(!prompt.contains(short(&old)), "the state's commit is not what the pull request holds: {prompt}");
        let now = w.state().unwrap();
        assert_eq!((now.commit.as_str(), now.run), (head.as_str(), 7));
        assert_ne!(now.nonce, "n");
        assert_eq!(*w.hub.calls.borrow(), [format!("dispatch 1.2.3 {head} {}", now.nonce), format!("watch {DEV_REPO} 7")]);
        assert_eq!(w.hub.pr.borrow().as_ref().unwrap().head_oid, new_export);
        // The owner's approval of the old head does not carry over.
        match w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap() {
            Done::Waiting(m) => assert!(m.len() == 1 && m[0].ends_with("approve the head"), "{m:?}"),
            d => panic!("{d:?}"),
        }
        assert_eq!((w.hub.count("dispatch"), w.hub.count("merge")), (1, 0));
        // A re-export never goes back: HEAD must be origin/main's tip.
        st.write(&w.stage().join("ship/state.json")).unwrap();
        crate::release_ci::tests::fall_behind(&w.dev);
        let e = w.ship(&Typed::new(&["open v1.2.3"]), None).unwrap_err();
        assert!(e.contains(", not origin/main ("), "{e}");
        assert_eq!(w.hub.count("dispatch"), 1);
        sh(&w.dev, &["reset", "-q", "--hard", "origin/main"]);
        // The sign step after the merge still wants the commit run 1 sent.
        std::fs::rename(w.fx.d.join("held-release"), w.fx.stub.join("release-v1.2.3")).unwrap();
        *w.hub.pr.borrow_mut() = Some(merged(&w.fx.target, &w.fx.target));
        let e = w.ship(&Typed::new(&["publish v1.2.3"]), None).unwrap_err();
        assert!(e.starts_with("HEAD is "), "{e}");
        assert!(!w.stage().join("sign").exists());
        std::fs::rename(w.fx.stub.join("release-v1.2.3"), w.fx.d.join("held-release")).unwrap();
        // Run 1 again from a moved HEAD says so in its plan and asks again.
        *w.hub.pr.borrow_mut() = None;
        w.stamp();
        assert!(w.ship(&No, None).unwrap_err().contains("refused at `open v1.2.3`"));
        assert_eq!(w.hub.count("dispatch"), 1);
        w.done();
    }

    /// A re-export run still going is watched, never dispatched again, even
    /// though state.json already names HEAD.
    #[test]
    fn a_reexport_run_in_progress_is_watched_not_dispatched_again() {
        let w = world("ship-r15-progress");
        w.no_draft();
        *w.hub.pr.borrow_mut() = Some(ready(&w.fx.target));
        State { tag: TAG.into(), commit: w.head(), nonce: "n1".into(), run: 4 }.write(&w.stage().join("ship/state.json")).unwrap();
        w.hub.pr_runs.borrow_mut().push(Run { id: 4, title: "release-pr v1.2.3 n1".into(), status: "in_progress".into(), ..Run::default() });
        *w.hub.opens.borrow_mut() = Some(PrInfo { head_oid: "e".repeat(40), ..ready(&w.fx.target) });
        assert_eq!(w.ship(&No, None).unwrap(), Done::Opened("https://github.com/acme/tool/pull/5".into()));
        assert_eq!(*w.hub.calls.borrow(), [format!("watch {DEV_REPO} 4")]);
        w.done();
    }

    /// A re-export whose run failed, or never showed, leaves state.json on
    /// HEAD and the pull request on the old export: ship dispatches again,
    /// only after a new `open` answer, and never merges the old head.
    #[test]
    fn a_failed_or_missing_reexport_run_is_dispatched_again_only_after_a_new_open() {
        let w = world("ship-r15-failed");
        w.no_draft();
        w.stamp();
        *w.hub.pr.borrow_mut() = Some(ready(&w.fx.target));
        let st = State { tag: TAG.into(), commit: w.head(), nonce: "n1".into(), run: 4 };
        st.write(&w.stage().join("ship/state.json")).unwrap();
        w.hub.pr_runs.borrow_mut().push(Run { id: 4, title: "release-pr v1.2.3 n1".into(), status: "completed".into(), conclusion: "failure".into(), ..Run::default() });
        let e = w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err();
        assert!(e.contains("refused at `open v1.2.3`"), "{e}");
        assert_eq!((w.hub.count("dispatch"), w.hub.count("merge")), (0, 0));
        assert_eq!(w.state().unwrap(), st);
        *w.hub.opens.borrow_mut() = Some(PrInfo { head_oid: "e".repeat(40), ..ready(&w.fx.target) });
        let t = Typed::new(&["open v1.2.3"]);
        assert!(matches!(w.ship(&t, None).unwrap(), Done::Opened(_)));
        assert_eq!(w.hub.count("dispatch"), 1);
        // The prompt names the pull request's real head, not HEAD twice
        // (state.json already named HEAD after the failed run).
        let prompt = t.prompts.borrow()[0].clone();
        assert!(prompt.contains(&format!("now at its head {}, moves to the export of {}", short(&w.fx.target), short(&w.head()))), "{prompt}");
        // No run named by state.json's nonce: the same.
        State { nonce: "gone".into(), ..st }.write(&w.stage().join("ship/state.json")).unwrap();
        let e = w.ship(&Typed::new(&["merge v1.2.3"]), None).unwrap_err();
        assert!(e.contains("refused at `open v1.2.3`"), "{e}");
        assert_eq!((w.hub.count("dispatch"), w.hub.count("merge")), (1, 0));
        w.done();
    }

    /// The key's path is checked only by the publish, after every other
    /// check: a key that is not there lets run 1 and the merge through, and
    /// stops the sign step after the binaries commit is made locally,
    /// before anything is signed, uploaded or pushed.
    #[test]
    fn the_key_preflight_comes_last() {
        let w = world("ship-key");
        w.hide_draft_until_built();
        w.stamp();
        let missing = (w.fx.d.join("absent.pem"), "1".repeat(64));
        *w.hub.opens.borrow_mut() = Some(ready(&w.fx.target));
        assert!(matches!(w.ship(&Typed::new(&["open v1.2.3"]), Some(missing.clone())).unwrap(), Done::Opened(_)));
        w.hub.rel_runs.borrow_mut().push(run_of("in_progress", ""));
        let t = Typed::new(&["merge v1.2.3", "publish v1.2.3"]);
        let e = w.ship(&t, Some(missing)).unwrap_err();
        assert_eq!(w.hub.count("merge"), 1);
        assert_eq!(*t.asked.borrow(), ["merge v1.2.3"], "the preflight stops it before the publish prompt");
        assert!(e == "ATLAS_SIGN_KEY is not a file" || e.contains("openssl"), "{e}");
        let public = w.stage().join("sign/public");
        assert_eq!(sh(&public, &["log", "-1", "--format=%s"]), "atlas v1.2.3 binaries", "the preflight comes after the binaries commit");
        assert!(!uploaded(&w.fx));
        assert_eq!(w.main(), w.fx.target);
        w.done();
    }

    #[test]
    fn the_origin_must_be_the_development_repository() {
        let w = world("ship-origin");
        let check = || Ok(true);
        let e = ship(&Ship {
            tag: TAG.into(),
            root: w.dev.clone(),
            check_origin: true,
            repo: REPO.into(),
            remote: String::new(),
            stage: w.stage(),
            denylist: denylist(&w.fx.d),
            accepted: None,
            key: None,
            trusted: String::new(),
            test_key: String::new(),
            ident: Ident { name: "P".into(), email: "pub@example.invalid".into() },
            api: &NoApi,
            rel: &w.rel,
            hub: &w.hub,
            confirm: &No,
            check: &check,
        })
        .unwrap_err();
        assert!(e.starts_with("origin fetches from "), "{e}");
        assert!(w.hub.calls.borrow().is_empty());
        for ok in ["https://github.com/bloodfeast/atlas-dev", "https://github.com/bloodfeast/atlas-dev.git", "https://GitHub.com/Bloodfeast/Atlas-Dev.git/", "git@github.com:bloodfeast/atlas-dev.git", "ssh://git@github.com/bloodfeast/atlas-dev.git"] {
            assert_eq!(github_repo(ok).as_deref(), Some(DEV_REPO), "{ok}");
        }
        for bad in ["https://github.com/bloodfeast/atlas-dev-fork.git", "https://github.com/bloodfeast/atlas.git", "https://github.com/bloodfeast/atlas-dev.evil", "https://gitlab.com/bloodfeast/atlas-dev.git", "https://x@github.com/bloodfeast/atlas-dev.git", "C:/repos/atlas-dev.git"] {
            assert_ne!(github_repo(bad).as_deref(), Some(DEV_REPO), "{bad}");
        }
        // The effective URLs: a push URL elsewhere, an insteadOf rewrite elsewhere.
        sh(&w.dev, &["remote", "set-url", "origin", "https://github.com/bloodfeast/atlas-dev.git"]);
        origin_check(&w.dev).unwrap();
        sh(&w.dev, &["remote", "set-url", "--push", "origin", "https://github.com/someone/else.git"]);
        assert!(origin_check(&w.dev).unwrap_err().starts_with("origin pushes to "));
        sh(&w.dev, &["config", "--unset", "remote.origin.pushurl"]);
        sh(&w.dev, &["config", "url.https://example.invalid/.insteadOf", "https://github.com/"]);
        let e = origin_check(&w.dev).unwrap_err();
        assert!(e.starts_with("origin fetches from \"https://example.invalid/bloodfeast/atlas-dev.git\""), "{e}");
        w.done();
    }

    #[test]
    fn the_merge_is_pinned_squashed_and_never_admin() {
        let m = Merge { repo: "bloodfeast/atlas".into(), number: 9, head: "a".repeat(40), subject: "atlas v1.2.3 (#9)".into(), body_file: PathBuf::from("b.txt") };
        let v = merge_argv(&m);
        let after = |k: &str| v.iter().position(|a| a == k).map(|i| v[i + 1].clone());
        assert_eq!(&v[..3], ["pr", "merge", "9"]);
        assert_eq!(after("--repo").unwrap(), "bloodfeast/atlas");
        assert_eq!(after("--match-head-commit").unwrap(), m.head);
        // GitHub refused the flag ("Invalid email address"): it picks the
        // author, and ship checks every address after the merge.
        assert!(!v.iter().any(|a| a == "--author-email"), "{v:?}");
        assert_eq!(after("--subject").unwrap(), m.subject);
        assert_eq!(after("--body-file").unwrap(), "b.txt");
        assert!(v.iter().any(|a| a == "--squash"));
        for never in ["--admin", "--auto", "--delete-branch", "-d", "--merge", "--rebase"] {
            assert!(!v.iter().any(|a| a == never), "{never}");
        }
    }

    #[test]
    fn gh_answers_are_read() {
        let runs = parse_runs(r#"[{"databaseId": 12, "displayTitle": "release-pr v1.2.3 ab12", "status": "completed", "conclusion": "success", "url": "u"}]"#).unwrap();
        assert_eq!(runs, [Run { id: 12, title: "release-pr v1.2.3 ab12".into(), status: "completed".into(), conclusion: "success".into(), url: "u".into() }]);
        let head = "a".repeat(40);
        let checks: Vec<String> = REQUIRED_CHECKS.iter().map(|n| format!(r#"{{"__typename": "CheckRun", "name": "{n}", "status": "COMPLETED", "conclusion": "SUCCESS"}}"#)).collect();
        let body = format!(
            r#"{{"number": 5, "state": "OPEN", "url": "https://github.com/acme/tool/pull/5", "title": "atlas v1.2.3", "baseRefName": "main", "headRefName": "release/v1.2.3", "headRefOid": "{head}", "isCrossRepository": false,
            "author": {{"login": "app/atlas-release", "is_bot": true}}, "mergeCommit": null,
            "reviews": [{{"author": {{"login": "acme"}}, "state": "APPROVED", "submittedAt": "2026-11-10T21:19:57Z", "commit": {{"oid": "{head}"}}}}],
            "statusCheckRollup": [{}, {{"__typename": "StatusContext", "context": "legacy", "state": "PENDING"}}]}}"#,
            checks.join(", ")
        );
        let p = parse_pr(&body).unwrap();
        assert!(not_the_release(&p, TAG).is_empty());
        assert_eq!(p.merge_commit, None);
        assert_eq!(not_ready(&p, "acme"), ["check legacy is pending"]);
        let merged = parse_pr(&body.replace(r#""mergeCommit": null"#, &format!(r#""mergeCommit": {{"oid": "{}"}}"#, "b".repeat(40)))).unwrap();
        assert_eq!(merged.merge_commit, Some("b".repeat(40)));
    }

    /// bloodfeast/atlas#7's head, and its reviews as gh gave them on
    /// 2026-11-10 (`gh pr view 7 --json reviews,latestReviews,headRefOid`),
    /// trimmed to the fields `parse_pr` reads: logins only. gh leaves the
    /// commit oid of `latestReviews` empty, so only `reviews` names the
    /// commit an approval was given on.
    const PR7_HEAD: &str = "a0ca74d0500930debb1b18dd5f953a02a7452475";
    const PR7_REVIEWS: &str = r#""latestReviews": [{"author": {"login": "bloodfeast"}, "state": "APPROVED", "submittedAt": "2026-11-10T22:21:08Z", "commit": {"oid": ""}}],
        "reviews": [
            {"author": {"login": "bloodfeast"}, "state": "DISMISSED", "submittedAt": "2026-11-10T15:36:29Z", "commit": {"oid": "72fda945d45936f014f4b119fb38e656b9c03edb"}},
            {"author": {"login": "bloodfeast"}, "state": "APPROVED", "submittedAt": "2026-11-10T21:19:57Z", "commit": {"oid": "a0ca74d0500930debb1b18dd5f953a02a7452475"}},
            {"author": {"login": "bloodfeast"}, "state": "APPROVED", "submittedAt": "2026-11-10T22:21:08Z", "commit": {"oid": "a0ca74d0500930debb1b18dd5f953a02a7452475"}}
        ]"#;

    /// `gh pr view` of a release pull request at `head` with every check
    /// green and the review fields given.
    fn pr_json(head: &str, review_fields: &str) -> String {
        let checks: Vec<String> = REQUIRED_CHECKS.iter().map(|n| format!(r#"{{"__typename": "CheckRun", "name": "{n}", "status": "COMPLETED", "conclusion": "SUCCESS"}}"#)).collect();
        format!(
            r#"{{"number": 7, "state": "OPEN", "url": "https://github.com/bloodfeast/atlas/pull/7", "title": "atlas v0.4.0", "baseRefName": "main", "headRefName": "release/v0.4.0", "headRefOid": "{head}", "isCrossRepository": false,
            "author": {{"login": "app/rs-atlas-releases", "is_bot": true}}, "mergeCommit": null, {review_fields},
            "statusCheckRollup": [{}]}}"#,
            checks.join(", ")
        )
    }

    /// One review as gh's `reviews` field shows it.
    fn review_json(login: &str, state: &str, at: &str, oid: &str) -> String {
        format!(r#"{{"author": {{"login": "{login}"}}, "state": "{state}", "submittedAt": "2026-11-10T{at}Z", "commit": {{"oid": "{oid}"}}}}"#)
    }

    #[test]
    fn the_approval_is_read_from_reviews_not_latest_reviews() {
        // #7 as gh gave it: approved on its head, so ready.
        let p = parse_pr(&pr_json(PR7_HEAD, PR7_REVIEWS)).unwrap();
        assert_eq!(not_ready(&p, "bloodfeast"), Vec::<String>::new());
        assert!(not_ready(&p, "BloodFeast").is_empty(), "the owner's login, case aside");
        // The approval was on the head before last: approve the head.
        let p = parse_pr(&pr_json(&"c".repeat(40), PR7_REVIEWS)).unwrap();
        assert_eq!(not_ready(&p, "bloodfeast"), [format!("bloodfeast approved a0ca74d05009, not the head {}: approve the head", short(&"c".repeat(40)))]);
    }

    #[test]
    fn each_reviewers_latest_review_decides() {
        let head = "a".repeat(40);
        let ready_with = |reviews: &[String]| not_ready(&parse_pr(&pr_json(&head, &format!(r#""reviews": [{}]"#, reviews.join(", ")))).unwrap(), "acme");
        let ok = review_json("acme", "APPROVED", "10:00:00", &head);
        assert!(ready_with(std::slice::from_ref(&ok)).is_empty());
        // The owner's approval, then the owner's request for changes.
        assert_eq!(ready_with(&[ok.clone(), review_json("acme", "CHANGES_REQUESTED", "11:00:00", &head)]), ["acme's latest review is CHANGES_REQUESTED"]);
        // The same, listed out of order: the time decides, not the place.
        assert_eq!(ready_with(&[review_json("acme", "CHANGES_REQUESTED", "11:00:00", &head), ok.clone()]), ["acme's latest review is CHANGES_REQUESTED"]);
        // The owner's approval, then dismissed.
        assert_eq!(ready_with(&[ok.clone(), review_json("acme", "DISMISSED", "11:00:00", &head)]), ["acme's latest review is DISMISSED"]);
        // A comment after the approval changes nothing; a comment alone is no review.
        assert!(ready_with(&[ok.clone(), review_json("acme", "COMMENTED", "11:00:00", &head)]).is_empty());
        assert_eq!(ready_with(&[review_json("acme", "COMMENTED", "11:00:00", &head)]), ["no review from acme"]);
        // Another reviewer's request for changes, then their approval: ready.
        assert!(ready_with(&[review_json("helper", "CHANGES_REQUESTED", "09:00:00", &head), ok.clone(), review_json("helper", "APPROVED", "12:00:00", &head)]).is_empty());
        // Their approval, then their request for changes: not ready.
        assert_eq!(ready_with(&[review_json("helper", "APPROVED", "09:00:00", &head), ok, review_json("helper", "CHANGES_REQUESTED", "12:00:00", &head)]), ["helper requested changes"]);
    }

    /// A commit on its own branch `branch` of the public repository, off its
    /// main, with the author, committer and message given: a merge commit as
    /// GitHub makes one.
    fn public_commit(w: &World, branch: &str, author: &str, committer: &str, message: &str) -> String {
        let x = w.fx.d.join(format!("side-{branch}"));
        sh(&w.fx.d, &["clone", "-q", &w.fx.bare.to_string_lossy(), x.to_str().unwrap()]);
        sh(&x, &["-c", "user.name=C", "-c", &format!("user.email={committer}"), "commit", "-q", "--allow-empty", "--author", &format!("A <{author}>"), "-m", message]);
        sh(&x, &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")]);
        sh(&x, &["rev-parse", "HEAD"])
    }

    const BOT: &str = "123+rs-atlas-releases[bot]@users.noreply.github.com";

    /// The MERGED arm checks the merge commit's author, committer and every
    /// co-author before anything else, and never prints a refused address.
    #[test]
    fn a_merged_pull_requests_addresses_are_checked_trailers_included() {
        let w = world("ship-merged-addr");
        w.no_draft();
        // The 117f4dcc shape: the release bot, GitHub's committer, the owner as co-author.
        let good = public_commit(&w, "good", BOT, "noreply@github.com", "atlas v1.2.3 (#5)\n\nCo-authored-by: P <pub@example.invalid>");
        *w.hub.pr.borrow_mut() = Some(merged(&w.fx.target, &good));
        let e = w.ship(&No, None).unwrap_err();
        assert!(e.starts_with("no release workflow run on the merge commit"), "{e}");
        for (name, co) in [("a non-noreply co-author", "o@other.invalid"), ("another user's noreply co-author", "2+other@users.noreply.github.com")] {
            let bad = public_commit(&w, &format!("bad-{}", co.len()), BOT, "noreply@github.com", &format!("atlas v1.2.3 (#5)\n\nCo-authored-by: O <{co}>"));
            *w.hub.pr.borrow_mut() = Some(merged(&w.fx.target, &bad));
            let runs = w.hub.count("release-runs");
            let e = w.ship(&No, None).unwrap_err();
            assert!(e.contains(&format!("merge commit {}", short(&bad))) && e.contains(" 1 address "), "{name}: {e}");
            assert!(!e.contains(co), "{name}: the refused address is printed: {e}");
            assert_eq!(w.hub.count("release-runs"), runs, "{name}: nothing after the check");
        }
        w.done();
    }

    #[test]
    fn the_config_takes_paths_only_and_the_environment_wins() {
        let c = parse_config(r#"{"denylist": "D/deny.txt", "accepted": "D/acc.txt", "key": "K/release.pem", "wsl_distro": "Debian"}"#).unwrap();
        assert_eq!(c.key.as_deref(), Some(Path::new("K/release.pem")));
        assert!(parse_config(r#"{"denylist": "D", "token": "x"}"#).unwrap_err().contains("unknown key \"token\""));
        assert!(parse_config(r#"{"denylist": 3}"#).unwrap_err().contains("denylist is not a non-empty string"));
        assert!(parse_config(r#"{"denylist": ""}"#).is_err());
        assert!(parse_config("[1]").is_err() && parse_config("nope").is_err());
        let none = |_: &str| None;
        let s = resolve(&c, Some(Path::new("CFG.json")), &none).unwrap();
        assert_eq!((s.denylist.as_path(), s.wsl_distro.as_deref()), (Path::new("D/deny.txt"), Some("Debian")));
        assert_eq!(s.sources[0], "denylist: D/deny.txt (from the config CFG.json)");
        let env = |k: &str| (k == "ATLAS_LEAK_DENYLIST").then(|| "E/deny.txt".to_string());
        let s = resolve(&c, Some(Path::new("CFG.json")), &env).unwrap();
        assert_eq!(s.denylist, PathBuf::from("E/deny.txt"));
        assert_eq!(s.sources[0], "denylist: E/deny.txt (from ATLAS_LEAK_DENYLIST)");
        assert!(resolve(&Config::default(), None, &none).unwrap_err().starts_with("no denylist"));
        let d = tmp("ship-config-file");
        assert!(config_file(Some(d.join("absent.json").to_string_lossy().into_owned())).unwrap_err().contains("not a file"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn private_files_stay_out_of_the_checkout_and_out_of_any_repository() {
        let d = tmp("ship-place");
        let checkout = d.join("atlas");
        std::fs::create_dir_all(&checkout).unwrap();
        // Outside any git work tree (when the temp folder is in none).
        if git_out(&d, &["rev-parse", "--show-toplevel"]).is_err() {
            placement("the denylist", &d.join("deny.txt"), &checkout).unwrap();
        }
        // Inside the checkout, even where nothing exists yet.
        assert!(placement("the key", &checkout.join("keys/k.pem"), &checkout).unwrap_err().contains("is inside the atlas checkout"));
        // Another work tree: ignored passes, not ignored or tracked refuses.
        let other = d.join("home");
        sh(&d, &["init", "-q", other.to_str().unwrap()]);
        std::fs::write(other.join(".gitignore"), "private/\n").unwrap();
        std::fs::create_dir_all(other.join("private")).unwrap();
        placement("the denylist", &other.join("private/deny.txt"), &checkout).unwrap();
        placement("the key", &other.join("private/absent/k.pem"), &checkout).unwrap();
        assert!(placement("the denylist", &other.join("deny.txt"), &checkout).unwrap_err().contains("and not ignored there"));
        std::fs::write(other.join("tracked.txt"), "x").unwrap();
        sh(&other, &["add", "-f", "tracked.txt"]);
        std::fs::write(other.join(".gitignore"), "private/\ntracked.txt\n").unwrap();
        assert!(placement("the denylist", &other.join("tracked.txt"), &checkout).unwrap_err().contains("is tracked by"));
        // A clone of the development repository refuses even an ignored file.
        sh(&other, &["remote", "add", "origin", "git@github.com:bloodfeast/atlas-dev.git"]);
        assert!(placement("the denylist", &other.join("private/deny.txt"), &checkout).unwrap_err().contains("inside a clone of bloodfeast/atlas-dev"));
        // And so does a clone of the public repository.
        sh(&other, &["remote", "set-url", "origin", "https://github.com/bloodfeast/atlas.git"]);
        assert!(placement("the denylist", &other.join("private/deny.txt"), &checkout).unwrap_err().contains("inside a clone of bloodfeast/atlas ("));
        let _ = std::fs::remove_dir_all(&d);
    }
}
