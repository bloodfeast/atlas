//! Worktree gc: removes stale worktrees of the ownership ledger
//! ([`crate::worktree_ledger`]), their build output and atlas's own Docker
//! objects ([`crate::worktree_gc_docker`]), and never loses work.
//!
//! A ledger worktree is *stale* when its HEAD is in the remote default branch
//! and is not that branch's tip (a tree with no commits of its own is new),
//! or a merged or closed PR event names its head; and it is not in use
//! ([`in_use`]): no live agent holds its lane or names it, no waiting queue
//! item does, the running agents could all be read, and nothing in it, its
//! git folder or its ledger row changed within the quiet period
//! (`worktrees.quiet_minutes`, 2 h by default) ([`stale`]). The in-use check
//! runs again, with the running agents read again, just before each
//! worktree is touched. It *holds work* when any check of [`wip`] finds
//! something (ignored files outside the build folders included), or any
//! check fails: a
//! worktree that holds work is never removed, whatever the setting, except by
//! `dirty = "commit_delete"`, which first commits, pushes and verifies it.
//! Removal is `git worktree remove` without `--force`: a refusal from git is
//! reported, never overridden.
//!
//! ```text
//! atlas worktrees gc           the plan, with sizes (changes nothing)
//! atlas worktrees gc --apply   the plan, carried out
//! atlas worktrees gc --auto    the hook's run: applies when worktrees.auto_gc is on
//! ```

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::worktree_ledger::{path_key, Row};

/// `worktrees.dirty`: what gc does with a stale worktree that holds work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dirty {
    /// Nothing: it is listed with the kind of work it holds.
    #[default]
    Report,
    /// Only its build output is deleted.
    RemoveTarget,
    /// The work is committed, pushed and verified on the remote, then the
    /// worktree is removed.
    CommitDelete,
}

impl Dirty {
    /// The setting's word.
    pub fn parse(s: &str) -> Option<Dirty> {
        match s {
            "report" => Some(Dirty::Report),
            "remove_target" => Some(Dirty::RemoveTarget),
            "commit_delete" => Some(Dirty::CommitDelete),
            _ => None,
        }
    }

    /// The word for it.
    pub fn name(self) -> &'static str {
        match self {
            Dirty::Report => "report",
            Dirty::RemoveTarget => "remove_target",
            Dirty::CommitDelete => "commit_delete",
        }
    }
}

/// The configuration's `worktrees` section, as written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreesSpec {
    /// `auto_gc`.
    pub auto_gc: Option<bool>,
    /// `dirty`.
    pub dirty: Option<Dirty>,
    /// `notify_gb`.
    pub notify_gb: Option<u64>,
    /// `target_dirs`, made absolute.
    pub target_dirs: Vec<String>,
    /// `quiet_minutes`.
    pub quiet_minutes: Option<u64>,
    /// `disposable`.
    pub disposable: Option<Vec<String>>,
}

/// The default of `worktrees.disposable`: the ignored paths of a worktree
/// that are not work. Only `target/` at its top.
pub const DISPOSABLE: [&str; 1] = ["target/"];

/// The default of `worktrees.notify_gb`.
pub const NOTIFY_GB: u64 = 10;

/// The default of `worktrees.quiet_minutes`: a worktree with a file, its
/// git folder or its ledger row changed this recently is in use.
pub const QUIET_MINUTES: u64 = 120;

/// How long one git or docker call may take before it counts as failed.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The limit of `commit_delete`'s writing calls (add, commit with its
/// hooks, push, ls-remote, fetch): 300 s.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(300);

/// The largest untracked file `commit_delete` commits: 10 MB.
pub const COMMIT_FILE_LIMIT: u64 = 10 << 20;

/// The resolved `worktrees` settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcSettings {
    /// gc acts by itself (from the hook).
    pub auto_gc: bool,
    /// What a stale worktree that holds work gets.
    pub dirty: Dirty,
    /// The hook line's threshold, in bytes.
    pub notify_bytes: u64,
    /// Build folders linked to a worktree, `{name}` its folder name.
    pub target_dirs: Vec<String>,
    /// One git or docker call's limit.
    pub timeout: Duration,
    /// The quiet period: a worktree changed within it is in use.
    pub quiet: Duration,
    /// The largest untracked file `commit_delete` commits.
    pub commit_limit: u64,
    /// Globs of ignored paths that are not work ([`disposable`]).
    pub disposable: Vec<String>,
    /// Whether the plan walks files at all: a candidate's for the quiet
    /// period, and a row it plans to delete for a secret or a checkout
    /// ([`delete_refusal`]). Not configured: off for the hook's dry run
    /// (`--auto` with `auto_gc` off), which deletes nothing. The apply step
    /// always walks ([`guarded_delete`]).
    pub plan_walks: bool,
}

impl Default for GcSettings {
    fn default() -> Self {
        GcSettings {
            auto_gc: false,
            dirty: Dirty::Report,
            notify_bytes: NOTIFY_GB << 30,
            target_dirs: Vec::new(),
            timeout: CALL_TIMEOUT,
            quiet: Duration::from_secs(QUIET_MINUTES * 60),
            commit_limit: COMMIT_FILE_LIMIT,
            disposable: DISPOSABLE.iter().map(|s| s.to_string()).collect(),
            plan_walks: true,
        }
    }
}

impl GcSettings {
    /// The settings from the configuration's section (defaults for what it
    /// leaves out).
    pub fn from_spec(s: Option<&WorktreesSpec>) -> GcSettings {
        let d = GcSettings::default();
        let Some(s) = s else { return d };
        GcSettings {
            auto_gc: s.auto_gc.unwrap_or(d.auto_gc),
            dirty: s.dirty.unwrap_or(d.dirty),
            notify_bytes: s.notify_gb.map_or(d.notify_bytes, |g| g.saturating_mul(1 << 30)),
            target_dirs: s.target_dirs.clone(),
            timeout: d.timeout,
            quiet: s.quiet_minutes.map_or(d.quiet, |m| Duration::from_secs(m.saturating_mul(60))),
            commit_limit: d.commit_limit,
            disposable: s.disposable.clone().unwrap_or(d.disposable),
            plan_walks: d.plan_walks,
        }
    }
}

/// What a finished git or docker call gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// The exit code (`None`: killed by a signal).
    pub code: Option<i32>,
    /// Its standard output.
    pub stdout: String,
    /// Its standard error.
    pub stderr: String,
}

/// Runs `program args` with stdin closed and no window, killed after
/// `timeout`. git never prompts and takes no optional lock (a status beside a
/// running agent leaves its index alone).
///
/// # Errors
///
/// The program could not start, or did not finish in time.
pub fn call(program: &str, dir: Option<&Path>, args: &[&str], timeout: Duration) -> Result<Ran, String> {
    let mut c = std::process::Command::new(program);
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    c.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let what = || format!("{program} {}", args.join(" "));
    let mut child = c.spawn().map_err(|e| format!("{}: {e}", what()))?;
    let pipe = |r: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut v);
            }
            String::from_utf8_lossy(&v).into_owned()
        })
    };
    let out = pipe(child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>));
    let err = pipe(child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>));
    let end = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{}: no answer in {} s", what(), timeout.as_secs_f32()));
            }
            Err(e) => return Err(format!("{}: {e}", what())),
        }
    };
    Ok(Ran { code: status.code(), stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default() })
}

/// `git -C dir args`, which must exit 0; its standard output.
///
/// # Errors
///
/// [`call`]'s, or git's own message on a non-zero exit.
pub fn git(dir: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let r = call("git", Some(dir), args, timeout)?;
    if r.code == Some(0) {
        Ok(r.stdout)
    } else {
        Err(format!("git {} in {}: {}", args.join(" "), dir.display(), r.stderr.trim()))
    }
}

/// One kind of work a worktree can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Work {
    /// A tracked file changed.
    Modified,
    /// A tracked file deleted.
    Deleted,
    /// Something staged (unmerged paths too).
    Staged,
    /// A file git does not track and does not ignore.
    Untracked,
    /// A file git ignores outside the build folders (`target/` at the top
    /// of the tree, and `target_dirs`): `git worktree remove` would delete
    /// it unasked, and a commit leaves it out.
    Ignored,
    /// A commit no remote-tracking ref contains.
    Unpushed,
    /// A merge, rebase, cherry-pick, revert or bisect under way.
    InProgress,
    /// `git worktree lock`.
    Locked,
    /// A stash entry made on its branch.
    Stash,
    /// A check git could not answer: counted as work.
    Unknown,
    /// The path is not a linked worktree of the ledger's repository.
    NotAWorktree,
}

impl Work {
    /// The words for it in the report.
    pub fn name(self) -> &'static str {
        match self {
            Work::Modified => "modified",
            Work::Deleted => "deleted",
            Work::Staged => "staged",
            Work::Untracked => "untracked",
            Work::Ignored => "ignored",
            Work::Unpushed => "unpushed",
            Work::InProgress => "in progress",
            Work::Locked => "locked",
            Work::Stash => "stash",
            Work::Unknown => "unknown",
            Work::NotAWorktree => "not a worktree",
        }
    }

    /// Whether a commit and a push make it durable (`commit_delete`).
    pub fn committable(self) -> bool {
        matches!(self, Work::Modified | Work::Deleted | Work::Staged | Work::Untracked | Work::Unpushed)
    }
}

/// One finding: its kind and what was seen.
pub type Found = (Work, String);

/// What [`wip`] checks: a worktree folder of a repository.
#[derive(Debug, Clone, Copy)]
pub struct Probe<'a> {
    /// The worktree's folder.
    pub path: &'a Path,
    /// The main checkout of the repository the ledger names (`None`: not
    /// configured).
    pub repo: Option<&'a Path>,
    /// One call's limit.
    pub timeout: Duration,
    /// Build folders outside `target/` whose ignored files are not work
    /// (the worktree's `target_dirs`).
    pub build: &'a [PathBuf],
    /// The `worktrees.disposable` globs ([`disposable`]).
    pub disposable: &'a [String],
}

/// The two status letters and the path of each `git status --porcelain=v1
/// -z` entry.
pub fn status_entries(st: &str) -> Vec<(u8, u8, &str)> {
    let mut out = Vec::new();
    let mut it = st.split('\0').filter(|s| !s.is_empty());
    while let Some(e) = it.next() {
        let b = e.as_bytes();
        if b.len() < 3 {
            continue;
        }
        // A rename or copy is followed by its source path.
        if matches!(b[0], b'R' | b'C') || matches!(b[1], b'R' | b'C') {
            it.next();
        }
        out.push((b[0], b[1], &e[3.min(e.len())..]));
    }
    out
}

/// The two status letters of each `git status --porcelain=v1 -z` entry.
pub fn status_codes(st: &str) -> Vec<(u8, u8)> {
    status_entries(st).into_iter().map(|(x, y, _)| (x, y)).collect()
}

/// Whether one path component `s` matches the glob component `g` (`*` any
/// run of characters, `?` one character; case kept).
fn glob_part(g: &[u8], s: &[u8]) -> bool {
    match (g.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => glob_part(&g[1..], s) || (!s.is_empty() && glob_part(g, &s[1..])),
        (Some(b'?'), Some(_)) => glob_part(&g[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => glob_part(&g[1..], &s[1..]),
        _ => false,
    }
}

fn glob_parts(g: &[&str], s: &[&str]) -> bool {
    match g.first() {
        None => s.is_empty(),
        Some(&"**") => glob_parts(&g[1..], s) || (!s.is_empty() && glob_parts(g, &s[1..])),
        Some(p) => !s.is_empty() && glob_part(p.as_bytes(), s[0].as_bytes()) && glob_parts(&g[1..], &s[1..]),
    }
}

/// Whether the ignored path `p` (relative to the worktree's top, `/`
/// separated, a folder ending in `/`, as git status writes it) is disposable
/// under the `worktrees.disposable` globs: a glob matches the path, or one of
/// the folders above it, from the worktree's top. `*` and `?` stay inside one
/// component, `**` is any number of folders, and a glob ending in `/` matches
/// folders only. So `target/` is the top-level build folder and not
/// `crates/x/target/`, `**/__pycache__/` is that folder at any depth, and
/// `*.cache` is a file or folder at the top.
pub fn disposable(globs: &[String], p: &str) -> bool {
    let is_dir = p.ends_with('/');
    let parts: Vec<&str> = p.trim_end_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    // A secret (`.env`, `.env.*`, keys) is never disposable, whatever glob
    // covers it: it stays work, as an unlisted ignored file does.
    let secret = |s: &&str| crate::sources::is_denied_folder(s);
    if parts.iter().any(secret) || (!is_dir && parts.last().is_some_and(|s| secret_str(s))) {
        return false;
    }
    globs.iter().any(|g| {
        let dir_only = g.ends_with('/');
        let gp: Vec<&str> = g.trim_end_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        if gp.is_empty() {
            return false;
        }
        // The path itself, then each folder above it.
        (1..=parts.len()).rev().any(|n| (n < parts.len() || is_dir || !dir_only) && glob_parts(&gp, &parts[..n]))
    })
}

/// Ignored entries (`!!`) of a status of the worktree at `wt` that are work:
/// outside its build folders `build`, and not [`disposable`] under `globs`
/// (by name: no folder is walked here).
/// Untracked entries (`??`) are never looked at here.
pub fn ignored(st: &str, wt: &Path, build: &[PathBuf], globs: &[String]) -> Option<String> {
    let builds: Vec<String> = build.iter().map(|b| path_key(&b.to_string_lossy())).collect();
    let under = |k: &str, b: &str| k == b || k.strip_prefix(b).is_some_and(|r| r.starts_with('/'));
    let paths: Vec<&str> = status_entries(st)
        .into_iter()
        .filter(|(x, y, _)| *x == b'!' && *y == b'!')
        .map(|(_, _, p)| p)
        .filter(|p| {
            let k = path_key(&wt.join(p).to_string_lossy());
            !builds.iter().any(|b| under(&k, b))
        })
        // git lists an ignored folder whole: what a matched folder holds is
        // walked for a secret or a checkout by the plan (rows to remove
        // only) and again by [`guarded_delete`], never here.
        .filter(|p| !disposable(globs, p))
        .collect();
    let mut d = count(paths.len(), "ignored path")?;
    d.push_str(&format!(" outside the build folders: {}", paths.iter().take(5).copied().collect::<Vec<_>>().join(", ")));
    if paths.len() > 5 {
        d.push_str(&format!(" and {} more", paths.len() - 5));
    }
    Some(d)
}

fn count(n: usize, what: &str) -> Option<String> {
    (n > 0).then(|| format!("{n} {what}{}", if n == 1 { "" } else { "s" }))
}

/// Tracked files changed in the tree.
pub fn modified(codes: &[(u8, u8)]) -> Option<String> {
    count(codes.iter().filter(|c| is_modified(**c)).count(), "modified file")
}

fn is_modified((x, y): (u8, u8)) -> bool {
    x != b'?' && x != b'!' && matches!(y, b'M' | b'T')
}

/// Tracked files deleted from the tree.
pub fn deleted(codes: &[(u8, u8)]) -> Option<String> {
    count(codes.iter().filter(|c| is_deleted(**c)).count(), "deleted file")
}

fn is_deleted((x, y): (u8, u8)) -> bool {
    x != b'?' && y == b'D'
}

/// Entries in the index that differ from HEAD (unmerged paths and
/// intent-to-add, ` A`, included).
pub fn staged(codes: &[(u8, u8)]) -> Option<String> {
    count(codes.iter().filter(|c| is_staged(**c)).count(), "staged change")
}

fn is_staged((x, y): (u8, u8)) -> bool {
    !matches!(x, b' ' | b'?' | b'!') || matches!(y, b'U' | b'A')
}

/// Files git neither tracks nor ignores.
pub fn untracked(codes: &[(u8, u8)]) -> Option<String> {
    count(codes.iter().filter(|(x, y)| *x == b'?' && *y == b'?').count(), "untracked file")
}

/// Entries no class above counts (ignored ones aside): every status entry
/// is work, so one of a kind not known here is too.
pub fn other(codes: &[(u8, u8)]) -> Option<String> {
    let known = |c: (u8, u8)| is_modified(c) || is_deleted(c) || is_staged(c) || c == (b'?', b'?') || c == (b'!', b'!');
    count(codes.iter().filter(|c| !known(**c)).count(), "status entry of no known kind")
}

/// Commits reachable from HEAD that no remote-tracking ref contains (a
/// branch never pushed, or with no remote at all, counts all of its own).
///
/// # Errors
///
/// git's.
pub fn unpushed(wt: &Path, t: Duration) -> Result<Option<String>, String> {
    let n: usize = git(wt, &["rev-list", "--count", "HEAD", "--not", "--remotes"], t)?
        .trim()
        .parse()
        .map_err(|e| format!("git rev-list --count in {}: {e}", wt.display()))?;
    Ok(count(n, "unpushed commit"))
}

/// The state files of an operation under way, looked for in the worktree's
/// own git folder (and the bisect ones in the common folder too).
///
/// Not `AUTO_MERGE`: a finished rebase can leave it behind, and `git status`
/// ignores it too. Every real operation that writes it also leaves one of
/// these markers or unmerged index entries, which are work already.
pub const IN_PROGRESS: [&str; 8] =
    ["MERGE_HEAD", "rebase-merge", "rebase-apply", "CHERRY_PICK_HEAD", "REVERT_HEAD", "BISECT_LOG", "BISECT_START", "sequencer"];

/// An operation under way in the worktree: the state files found.
///
/// # Errors
///
/// git's, finding the folders.
pub fn in_progress(wt: &Path, t: Duration) -> Result<Option<String>, String> {
    let dirs = git(wt, &["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"], t)?;
    let mut dirs = dirs.lines().map(str::trim).filter(|l| !l.is_empty());
    let own = dirs.next().ok_or_else(|| format!("git rev-parse --git-dir in {}: no answer", wt.display()))?;
    let common = dirs.next().unwrap_or(own);
    let found: Vec<&str> = IN_PROGRESS
        .iter()
        .copied()
        .filter(|f| Path::new(own).join(f).exists() || (f.starts_with("BISECT") && Path::new(common).join(f).exists()))
        .collect();
    Ok((!found.is_empty()).then(|| found.join(", ")))
}

/// The `git worktree list --porcelain` block of `wt` in `list` (resolved
/// paths compared), never the main checkout's.
pub fn block_of<'a>(list: &'a str, wt: &Path) -> Option<&'a str> {
    let want = resolved(wt);
    list.split("\n\n")
        .map(|b| b.trim_matches(['\r', '\n']))
        .filter(|b| !b.is_empty())
        .skip(1)
        .find(|b| b.lines().any(|l| l.strip_prefix("worktree ").is_some_and(|p| resolved(Path::new(p.trim_end_matches('\r'))) == want)))
}

/// [`path_key`] of `p` resolved, or as given when it cannot be.
pub fn resolved(p: &Path) -> String {
    path_key(&std::fs::canonicalize(p).as_deref().unwrap_or(p).to_string_lossy())
}

/// `git worktree lock`: its reason, or `locked`.
pub fn locked(block: &str) -> Option<String> {
    block.lines().map(|l| l.trim_end_matches('\r')).find_map(|l| match l.strip_prefix("locked") {
        Some("") => Some("locked".to_string()),
        Some(r) if r.starts_with(' ') => Some(format!("locked: {}", r.trim())),
        _ => None,
    })
}

/// The branch a block names (empty: detached).
pub fn block_branch(block: &str) -> String {
    block
        .lines()
        .find_map(|l| l.trim_end_matches('\r').strip_prefix("branch "))
        .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_string())
        .unwrap_or_default()
}

/// Stash entries made on `branch` (`git stash list` subjects `WIP on B:` and
/// `On B:`; a detached HEAD's are `(no branch)`).
///
/// # Errors
///
/// git's.
pub fn stash(wt: &Path, branch: &str, t: Duration) -> Result<Option<String>, String> {
    let list = git(wt, &["stash", "list", "--format=%gs"], t)?;
    let b = if branch.is_empty() { "(no branch)" } else { branch };
    let (a, c) = (format!("WIP on {b}:"), format!("On {b}:"));
    Ok(count(list.lines().filter(|l| l.starts_with(&a) || l.starts_with(&c)).count(), "stash entry"))
}

/// A check git could not answer: work.
pub fn unknown(found: &mut Vec<Found>, e: String) {
    found.push((Work::Unknown, e));
}

fn take(found: &mut Vec<Found>, w: Work, r: Result<Option<String>, String>) {
    match r {
        Ok(Some(d)) => found.push((w, d)),
        Ok(None) => {}
        Err(e) => unknown(found, e),
    }
}

/// Everything the worktree at `p.path` holds that its removal would lose,
/// and every check that could not be answered (empty: nothing), and its
/// branch as git lists it (empty: detached or not listed).
pub fn wip(p: &Probe<'_>) -> (Vec<Found>, String) {
    let mut found = Vec::new();
    let Some(repo) = p.repo else {
        found.push((Work::NotAWorktree, "its repository is not configured".into()));
        return (found, String::new());
    };
    let list = match git(repo, &["worktree", "list", "--porcelain"], p.timeout) {
        Ok(l) => l.replace("\r\n", "\n"),
        Err(e) => {
            unknown(&mut found, e);
            return (found, String::new());
        }
    };
    let Some(block) = block_of(&list, p.path).filter(|_| p.path.is_dir()) else {
        found.push((Work::NotAWorktree, format!("not a linked worktree of {}", repo.display())));
        return (found, String::new());
    };
    let branch = block_branch(block);
    if let Some(l) = locked(block) {
        found.push((Work::Locked, l));
    }
    let status = ["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignored=matching", "--ignore-submodules=none"];
    match git(p.path, &status, p.timeout) {
        Ok(st) => {
            let codes = status_codes(&st);
            let classes = [
                (Work::Modified, modified(&codes)),
                (Work::Deleted, deleted(&codes)),
                (Work::Staged, staged(&codes)),
                (Work::Untracked, untracked(&codes)),
                (Work::Ignored, ignored(&st, p.path, p.build, p.disposable)),
                (Work::Unknown, other(&codes)),
            ];
            found.extend(classes.into_iter().filter_map(|(w, d)| d.map(|d| (w, d))));
        }
        Err(e) => unknown(&mut found, e),
    }
    take(&mut found, Work::Unpushed, unpushed(p.path, p.timeout));
    take(&mut found, Work::InProgress, in_progress(p.path, p.timeout));
    take(&mut found, Work::Stash, stash(p.path, &branch, p.timeout));
    (found, branch)
}

/// The report's words for what a worktree holds.
pub fn describe(found: &[Found]) -> String {
    found.iter().map(|(w, d)| format!("{}: {d}", w.name())).collect::<Vec<_>>().join("; ")
}

/// The remote default branch's ref: `refs/remotes/origin/HEAD`'s target, else
/// `origin/main`, else `origin/master` (`None`: none of them).
///
/// # Errors
///
/// git's.
pub fn default_ref(wt: &Path, t: Duration) -> Result<Option<String>, String> {
    let r = call("git", Some(wt), &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"], t)?;
    if r.code == Some(0) && !r.stdout.trim().is_empty() {
        return Ok(Some(r.stdout.trim().to_string()));
    }
    for c in ["refs/remotes/origin/main", "refs/remotes/origin/master"] {
        if call("git", Some(wt), &["rev-parse", "--verify", "-q", c], t)?.code == Some(0) {
            return Ok(Some(c.to_string()));
        }
    }
    Ok(None)
}

/// Whether HEAD is the remote default branch's tip itself: a worktree with
/// no commits of its own, new rather than done.
///
/// # Errors
///
/// git's.
pub fn at_default_tip(wt: &Path, t: Duration) -> Result<bool, String> {
    let Some(r) = default_ref(wt, t)? else { return Ok(false) };
    let shas = git(wt, &["rev-parse", "HEAD", &r], t)?;
    let mut it = shas.lines().map(str::trim);
    Ok(matches!((it.next(), it.next()), (Some(a), Some(b)) if !a.is_empty() && a == b))
}

/// Whether HEAD is a commit of the worktree's own, merged: committed after
/// `created_at` (the ledger row's time; unparseable: not its own) and
/// contained in the remote default branch (its tip included: a commit of its
/// own landed there). That branch's short name when it is. A worktree
/// with no commits of its own is new, not merged, however far the default
/// branch has moved past it.
///
/// # Errors
///
/// git's.
pub fn merged(wt: &Path, t: Duration, created_at: &str) -> Result<Option<String>, String> {
    let Some(r) = default_ref(wt, t)? else { return Ok(None) };
    if !own_head(wt, t, created_at)? {
        return Ok(None);
    }
    let a = call("git", Some(wt), &["merge-base", "--is-ancestor", "HEAD", &r], t)?;
    match a.code {
        Some(0) => Ok(Some(r.strip_prefix("refs/remotes/").unwrap_or(&r).to_string())),
        Some(1) => Ok(None),
        _ => Err(format!("git merge-base --is-ancestor HEAD {r} in {}: {}", wt.display(), a.stderr.trim())),
    }
}

/// Whether HEAD was committed after `created_at` (committer time): a commit
/// made in the worktree, not the commit it was made from.
///
/// # Errors
///
/// git's.
pub fn own_head(wt: &Path, t: Duration, created_at: &str) -> Result<bool, String> {
    let Some(made) = crate::checkpoint::epoch_secs(created_at) else { return Ok(false) };
    let ct = git(wt, &["log", "-1", "--format=%ct", "HEAD"], t)?;
    let ct: i64 = ct.trim().parse().map_err(|e| format!("git log -1 --format=%ct in {}: {e}", wt.display()))?;
    Ok(ct > made)
}

/// A merged or closed `pull_request` event note (atlas pr-events): its PR,
/// verdict and head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrEnd {
    /// The PR number.
    pub pr: u64,
    /// `merged` or `closed`.
    pub verdict: String,
    /// Its head sha.
    pub head: String,
}

/// The merged or closed PR event notes in `store` newer than record id
/// `after`, and the newest record id read.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn pr_ends(store: &crate::memory::MemoryStore, after: i64) -> crate::Result<(Vec<PrEnd>, i64)> {
    let notes = store.notes_after(after, crate::pr_events::EVENT_PREFIX)?;
    let top = notes.iter().map(|h| h.rid).max().unwrap_or(after);
    let v = notes
        .iter()
        .filter_map(|h| {
            let get = |k: &str| crate::queue::meta_value(&h.meta, k).unwrap_or("").trim().to_string();
            let verdict = get("verdict");
            (get("event") == "pull_request" && (verdict == "merged" || verdict == "closed")).then(|| PrEnd {
                pr: get("pr").parse().unwrap_or(0),
                verdict,
                head: get("head").to_ascii_lowercase(),
            })
        })
        .filter(|e| e.head.len() >= 7)
        .collect();
    Ok((v, top))
}

/// The PR end whose head is `sha`.
pub fn pr_end_of<'a>(ends: &'a [PrEnd], sha: &str) -> Option<&'a PrEnd> {
    let sha = sha.trim().to_ascii_lowercase();
    ends.iter().find(|e| sha.len() >= 7 && (sha.starts_with(&e.head) || e.head.starts_with(&sha)))
}

/// What the stale rule reads besides git.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The open ledger rows.
    pub rows: &'a [Row],
    /// The configured repositories.
    pub repos: &'a [crate::project::Repo],
    /// The running agents of every session (`None`: the view could not be
    /// read, and nothing is stale).
    pub holders: Option<&'a [crate::lanes::Holder]>,
    /// The waiting queue items.
    pub waiting: &'a [crate::lanes::Item],
    /// Merged and closed PR event notes.
    pub pr_ends: &'a [PrEnd],
    /// The settings.
    pub settings: &'a GcSettings,
}

/// The folder name of a path.
pub fn folder_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("")
}

fn names_worktree(uses: &[(String, crate::lanes::Source)], name: &str) -> bool {
    uses.iter().any(|(u, _)| u.strip_prefix("worktree:").is_some_and(|n| n.eq_ignore_ascii_case(name)))
}

fn same_lane(row: &Row, lane: Option<&String>) -> bool {
    row.lane != crate::worktree_ledger::UNKNOWN && lane.is_some_and(|l| crate::lanes::same_lane(&row.lane, l))
}

/// Why `row` must stay whatever git says: a live agent or a waiting item
/// holds its lane or names it. `None`: nothing holds it.
pub fn held(cx: &Context<'_>, row: &Row) -> Option<String> {
    let name = folder_name(&row.path);
    let Some(holders) = cx.holders else { return Some("the running agents could not be read".into()) };
    if let Some(h) = holders.iter().find(|h| same_lane(row, h.class.lane.as_ref()) || names_worktree(&h.class.uses, name)) {
        return Some(format!("live agent {} in lane {}", h.name, h.class.lane.as_deref().unwrap_or("-")));
    }
    if let Some(i) = cx.waiting.iter().find(|i| same_lane(row, i.class.lane.as_ref()) || names_worktree(&i.class.uses, name)) {
        return Some(format!("waiting queue item {}", i.key));
    }
    None
}

/// Whether anything under `p` (links not followed; folders by their own
/// time too) was modified after `cutoff`. A folder that cannot be listed or
/// an entry whose time cannot be read counts as changed: quiet must be
/// shown, never assumed.
pub fn changed_since(p: &Path, cutoff: std::time::SystemTime) -> bool {
    let Ok(md) = std::fs::symlink_metadata(p) else { return true };
    if md.modified().map_or(true, |m| m > cutoff) {
        return true;
    }
    if !md.is_dir() {
        return false;
    }
    match std::fs::read_dir(p) {
        Ok(mut rd) => rd.any(|e| e.map_or(true, |e| changed_since(&e.path(), cutoff))),
        Err(_) => true,
    }
}

/// How deep below a build folder [`build_changed_since`] looks: the folder,
/// its profiles (`debug/`) and their parts (`deps/`, `incremental/`), whose
/// own times move whenever a build adds or replaces a file in them.
pub const BUILD_DEPTH: u32 = 2;

/// [`changed_since`] for a build folder, bounded: the own times of `p` and
/// of the folders and files down to `depth` levels below it, never deeper.
pub fn build_changed_since(p: &Path, cutoff: std::time::SystemTime, depth: u32) -> bool {
    let Ok(md) = std::fs::symlink_metadata(p) else { return true };
    if md.modified().map_or(true, |m| m > cutoff) {
        return true;
    }
    if !md.is_dir() || depth == 0 {
        return false;
    }
    match std::fs::read_dir(p) {
        Ok(mut rd) => rd.any(|e| e.map_or(true, |e| build_changed_since(&e.path(), cutoff, depth - 1))),
        Err(_) => true,
    }
}

/// [`changed_since`] for a worktree, its `target/` at the top bounded
/// ([`build_changed_since`]): the walk stops at the first entry newer than
/// `cutoff` and never goes through the build output's files.
pub fn tree_changed_since(wt: &Path, cutoff: std::time::SystemTime) -> bool {
    let Ok(md) = std::fs::symlink_metadata(wt) else { return true };
    if md.modified().map_or(true, |m| m > cutoff) {
        return true;
    }
    match std::fs::read_dir(wt) {
        Ok(mut rd) => rd.any(|e| {
            e.map_or(true, |e| {
                let build = e.file_name() == "target" && e.file_type().is_ok_and(|t| t.is_dir());
                if build {
                    build_changed_since(&e.path(), cutoff, BUILD_DEPTH)
                } else {
                    changed_since(&e.path(), cutoff)
                }
            })
        }),
        Err(_) => true,
    }
}

/// The quiet period's cutoff and its minutes (`None`: no quiet period).
fn quiet_cutoff(cx: &Context<'_>) -> Option<(std::time::SystemTime, u64)> {
    let quiet = cx.settings.quiet;
    (!quiet.is_zero()).then(|| (std::time::SystemTime::now().checked_sub(quiet).unwrap_or(std::time::UNIX_EPOCH), quiet.as_secs() / 60))
}

/// Why `row` is in use by its ledger row: created or changed within the
/// quiet period (or a time that cannot be read). No file is read.
pub fn row_changed(cx: &Context<'_>, row: &Row) -> Option<String> {
    let (cutoff, mins) = quiet_cutoff(cx)?;
    let floor = cutoff.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let row_times = [&row.created_at, &row.last_modified];
    row_times
        .iter()
        .any(|t| crate::checkpoint::epoch_secs(t).is_none_or(|s| s > floor))
        .then(|| format!("in use: its ledger row changed within {mins} min"))
}

/// Why `row` is in use by its files: its tree ([`tree_changed_since`]), a
/// linked build folder that exists ([`build_changed_since`]) or its own git
/// folder changed within the quiet period.
pub fn files_changed(cx: &Context<'_>, row: &Row) -> Option<String> {
    let (cutoff, mins) = quiet_cutoff(cx)?;
    let wt = Path::new(&row.path);
    if tree_changed_since(wt, cutoff) {
        return Some(format!("in use: a file in it changed within {mins} min"));
    }
    for b in build_dirs(cx, row) {
        let gone = matches!(std::fs::symlink_metadata(&b), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        if !gone && build_changed_since(&b, cutoff, BUILD_DEPTH) {
            return Some(format!("in use: its build folder {} changed within {mins} min", b.display()));
        }
    }
    match git(wt, &["rev-parse", "--path-format=absolute", "--git-dir"], cx.settings.timeout) {
        Ok(d) if !changed_since(Path::new(d.trim()), cutoff) => None,
        Ok(_) => Some(format!("in use: its git folder changed within {mins} min")),
        Err(e) => Some(format!("in use: its git folder could not be read: {e}")),
    }
}

/// Why `row` is in use by the clock: its ledger row ([`row_changed`]) or its
/// files ([`files_changed`]). `None`: quiet that long.
pub fn recently_changed(cx: &Context<'_>, row: &Row) -> Option<String> {
    row_changed(cx, row).or_else(|| files_changed(cx, row))
}

/// Why `row` is in use: a live agent or a waiting item holds it
/// ([`held`]), or it changed within the quiet period ([`recently_changed`]).
pub fn in_use(cx: &Context<'_>, row: &Row) -> Option<String> {
    held(cx, row).or_else(|| recently_changed(cx, row))
}

/// Why `row` is stale (`Ok`), or why it is kept (`Err`), by the cheap tests
/// only: held ([`held`]), its ledger row's times ([`row_changed`]), then
/// merged, PR ended or new. Its files are not walked here: [`plan`] does
/// that last, for candidates only.
pub fn stale(cx: &Context<'_>, row: &Row) -> Result<String, String> {
    if let Some(why) = held(cx, row).or_else(|| row_changed(cx, row)) {
        return Err(why);
    }
    let wt = Path::new(&row.path);
    let t = cx.settings.timeout;
    match merged(wt, t, &row.created_at) {
        Ok(Some(b)) => return Ok(format!("merged (HEAD in {b})")),
        Ok(None) => {}
        Err(e) => return Err(format!("could not tell whether it is merged: {e}")),
    }
    let head = git(wt, &["rev-parse", "HEAD"], t).map_err(|e| format!("could not read HEAD: {e}"))?;
    // A PR end counts only for a tree whose HEAD is its own commit: a tree
    // made at a PR's head (a follow-up before its first commit, a
    // reviewer's tree) stays "new" when that PR ends.
    // Asked only by the arms that need it, once (`None`: git failed).
    let cell = std::cell::OnceCell::new();
    let own = || *cell.get_or_init(|| own_head(wt, t, &row.created_at).ok());
    match pr_end_of(cx.pr_ends, &head) {
        Some(e) if own().unwrap_or(false) => Ok(format!("PR #{} {}", e.pr, e.verdict)),
        _ if at_default_tip(wt, t).unwrap_or(false) => Err("new: HEAD is the default branch's tip, no commits of its own".into()),
        _ if !own().unwrap_or(true) => Err("new: no commits of its own since it was made".into()),
        _ => Err("not merged".into()),
    }
}

/// What gc does with one worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// Stale and clean: removed.
    Remove,
    /// Stale, holds work, left alone (`report`, or work a commit cannot
    /// save).
    Work,
    /// Stale, holds work: only its build output goes (`remove_target`).
    Target,
    /// Stale, holds work: committed, pushed, verified, removed
    /// (`commit_delete`).
    Commit,
    /// Not stale.
    Keep,
}

impl Act {
    /// The report's word.
    pub fn word(self) -> &'static str {
        match self {
            Act::Remove => "remove",
            Act::Work => "work",
            Act::Target => "target",
            Act::Commit => "commit",
            Act::Keep => "keep",
        }
    }
}

/// One worktree in the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its ledger row.
    pub row: Row,
    /// What gc does.
    pub act: Act,
    /// Why (stale or kept).
    pub why: String,
    /// The work it holds.
    pub found: Vec<Found>,
    /// Its branch as git lists it.
    pub branch: String,
    /// The tree's size, when measured.
    pub bytes: Option<u64>,
    /// Its linked build folder, when one exists and no other ledger
    /// worktree links it, with its size when measured.
    pub target: Option<(PathBuf, Option<u64>)>,
    /// What applying it did (`None`: not applied).
    pub done: Option<String>,
}

/// The build folder `template` links to the worktree called `name`.
pub fn target_of(template: &str, name: &str) -> Option<PathBuf> {
    template.contains("{name}").then(|| PathBuf::from(template.replace("{name}", name)))
}

/// The build folders `row`'s worktree links (whether they exist or not).
pub fn build_dirs(cx: &Context<'_>, row: &Row) -> Vec<PathBuf> {
    cx.settings.target_dirs.iter().filter_map(|t| target_of(t, folder_name(&row.path))).collect()
}

/// Why the build folder `p` must not be deleted: it holds `.git` at any depth,
/// or lies inside any folder that does (unless it carries the CACHEDIR.TAG
/// signature and that repository tracks nothing in it), or it is, holds or
/// lies inside a checkout (an open ledger worktree, a configured main
/// checkout, a listed worktree of one), or the walk [`guarded_delete`] makes
/// refuses it ([`delete_refusal`]). `None`: it is only a build folder.
pub fn build_refusal(cx: &Context<'_>, p: &Path) -> Option<String> {
    // Last, as it is the dear one: a checkout or a secret at any depth below
    // (a clone or a `git init` in a scratch folder holds work nobody listed).
    build_refusal_unwalked(cx, p).or_else(|| delete_refusal(&Delete::Folder(p)))
}

/// [`build_refusal`] without its walk: the checks that read no folder.
fn build_refusal_unwalked(cx: &Context<'_>, p: &Path) -> Option<String> {
    if std::fs::symlink_metadata(p.join(".git")).is_ok() {
        return Some(format!("{} holds .git: a checkout, not build output", p.display()));
    }
    // Inside any git checkout at all, configured or not (a `{name}` template
    // one folder too high can land in a repository nobody named).
    let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    // Only a tagged cache folder may sit there: it carries the CACHEDIR.TAG
    // signature (cargo writes it) and the repository tracks nothing under it.
    if let Some(top) = real.ancestors().skip(1).find(|a| std::fs::symlink_metadata(a.join(".git")).is_ok()) {
        if !(cache_tagged(p) && untracked_by_parent(p, cx.settings.timeout)) {
            return Some(format!("{} lies inside the git checkout {}", p.display(), top.display()));
        }
    }
    let k = resolved(p);
    let (listed, _) = crate::worktree_ledger::listed(cx.repos);
    let checkouts = cx
        .rows
        .iter()
        .map(|r| r.path.clone())
        .chain(cx.repos.iter().filter_map(|r| r.path.as_ref().map(|p| p.to_string_lossy().into_owned())))
        .chain(listed.into_iter().map(|w| w.path));
    let under = |a: &str, b: &str| a == b || a.strip_prefix(b).is_some_and(|r| r.starts_with('/'));
    for c in checkouts {
        let ck = resolved(Path::new(&c));
        if under(&k, &ck) || under(&ck, &k) {
            return Some(format!("{} is, holds or lies in the checkout {c}", p.display()));
        }
    }
    None
}

/// Everything gc deletes is one of these, and goes through
/// [`guarded_delete`], the one place that deletes.
#[derive(Debug, Clone, Copy)]
pub enum Delete<'a> {
    /// A build folder, removed whole (never through a link).
    Folder(&'a Path),
    /// A linked worktree, by `git worktree remove` (no `--force`) in the
    /// main checkout `repo`.
    Worktree {
        /// The main checkout git runs in.
        repo: &'a Path,
        /// The worktree's folder.
        wt: &'a Path,
        /// git's limit.
        timeout: Duration,
    },
    /// A file gc wrote itself (a temp file).
    OwnFile(&'a Path),
}

/// Why `d` must not be deleted: the walk of it (every folder below, links
/// and junctions never entered) found a checkout ([`CHECKOUTS`] or a bare
/// repository's shape) or a secret (a denied name, a name that is not
/// UTF-8), or could not be finished. A worktree's walk passes over its own
/// `.git` file and the files git tracks (they are in git, and a change to
/// one is work already), and enters its ignored and build folders too.
/// `None`: it may go (a path that is gone included).
pub fn delete_refusal(d: &Delete<'_>) -> Option<String> {
    let (root, hit) = match *d {
        Delete::Folder(p) => match std::fs::symlink_metadata(p) {
            Ok(md) if md.is_dir() => (p, walk_for(p, true)),
            Ok(_) => return Some(format!("{}: not a folder", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => return Some(format!("{}: {e}", p.display())),
        },
        Delete::Worktree { wt, timeout, .. } => (wt, walk_tree(wt, timeout)),
        Delete::OwnFile(p) => {
            let name = p.file_name().unwrap_or_default();
            return match std::fs::symlink_metadata(p) {
                Ok(md) if md.is_file() && !(is_checkout_name(name) || secret_name(name)) => None,
                Ok(_) => Some(format!("{}: not a plain file gc wrote", p.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => Some(format!("{}: {e}", p.display())),
            };
        }
    };
    match hit {
        Ok(None) => None,
        Ok(Some(g)) => Some(format!("{} holds {} at {}: work, not output", root.display(), hit_kind(&g), g.display())),
        Err(e) => Some(format!("{} could not be walked: {e}", root.display())),
    }
}

/// The one place gc deletes: [`delete_refusal`] runs immediately before,
/// and a refusal deletes nothing.
///
/// # Errors
///
/// The refusal, the I/O error, or git's.
pub fn guarded_delete(d: &Delete<'_>) -> Result<(), String> {
    if let Some(r) = delete_refusal(d) {
        return Err(format!("refused: {r}"));
    }
    match *d {
        Delete::Folder(p) => match std::fs::remove_dir_all(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", p.display())),
            _ => Ok(()),
        },
        Delete::Worktree { repo, wt, timeout } => {
            git(repo, &["worktree", "remove", &wt.to_string_lossy()], timeout).map(|_| ()).map_err(|e| format!("git refused: {e}"))
        }
        Delete::OwnFile(p) => match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", p.display())),
            _ => Ok(()),
        },
    }
}

/// The names of a checkout's own folder: git, Mercurial, Jujutsu,
/// Subversion. A folder holding `HEAD`, `objects/` and `refs/` (a bare git
/// repository) counts too.
pub const CHECKOUTS: [&str; 4] = [".git", ".hg", ".jj", ".svn"];

fn is_checkout_name(n: &std::ffi::OsStr) -> bool {
    CHECKOUTS.iter().any(|c| n.eq_ignore_ascii_case(c))
}

/// A denied name ([`crate::sources::is_denied`]), or one that is not UTF-8
/// and so cannot be checked.
fn secret_name(n: &std::ffi::OsStr) -> bool {
    n.to_str().is_none_or(secret_str)
}

/// A name gc never deletes as output: [`crate::sources::is_denied`], and
/// `.envrc` (direnv's, often secret), which the shared list leaves readable.
fn secret_str(n: &str) -> bool {
    crate::sources::is_denied(n) || n.eq_ignore_ascii_case(".envrc")
}

fn hit_kind(g: &Path) -> String {
    match g.file_name() {
        Some(n) if is_checkout_name(n) => n.to_string_lossy().into_owned(),
        Some(n) if secret_name(n) => "a secret".into(),
        _ => "a bare git repository".into(),
    }
}

std::thread_local! {
    static WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "test-seams"))]
std::thread_local! {
    static WALK_ERROR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// A test seam (#355 S1): on this thread, the walk's read of folder `p`
/// fails with permission denied, on every platform; `None` clears it.
#[cfg(any(test, feature = "test-seams"))]
#[doc(hidden)]
pub fn inject_walk_error(p: Option<&Path>) {
    WALK_ERROR.with(|w| *w.borrow_mut() = p.map(Path::to_path_buf));
}

#[cfg(any(test, feature = "test-seams"))]
fn injected_walk_error(d: &Path) -> Option<std::io::Error> {
    WALK_ERROR.with(|w| w.borrow().as_deref().is_some_and(|p| p == d)).then(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))
}

#[cfg(not(any(test, feature = "test-seams")))]
fn injected_walk_error(_d: &Path) -> Option<std::io::Error> {
    None
}

/// The walks [`walk_for`] has started on this thread (a test seam).
#[doc(hidden)]
pub fn walks() -> usize {
    WALKS.with(std::cell::Cell::get)
}

/// The first checkout (a [`CHECKOUTS`] name, folder, file or link, any case,
/// or a bare repository's folder) at any depth below `p`, and with
/// `secrets` the first secret ([`secret_name`]) too, found by a walk that
/// follows no link or junction. `Err` when a folder cannot be read, as
/// nothing below it is known; a `p` that is gone holds nothing.
fn walk_for(p: &Path, secrets: bool) -> Result<Option<PathBuf>, String> {
    walk(p, secrets, &|_| false)
}

/// The walk of the worktree `wt` before `git worktree remove`: secrets and
/// checkouts, passing over its own `.git` file and the files git tracks
/// (never a folder: a tracked gitlink is entered).
fn walk_tree(wt: &Path, t: Duration) -> Result<Option<PathBuf>, String> {
    let list = git(wt, &["ls-files", "-z", "--full-name"], t)?;
    let tracked: std::collections::HashSet<PathBuf> = list.split('\0').filter(|s| !s.is_empty()).map(|s| wt.join(s)).collect();
    let own = wt.join(".git");
    let own_file = std::fs::symlink_metadata(&own).is_ok_and(|m| m.is_file());
    walk(wt, true, &|q| (own_file && q == own) || tracked.contains(q))
}

fn walk(p: &Path, secrets: bool, pass: &dyn Fn(&Path) -> bool) -> Result<Option<PathBuf>, String> {
    WALKS.with(|w| w.set(w.get() + 1));
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match injected_walk_error(&d).map_or_else(|| std::fs::read_dir(&d), Err) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && d == p => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", d.display())),
        };
        // A bare repository: `HEAD` (a file), `objects/` and `refs/`.
        let mut bare = [false; 3];
        for e in rd {
            let e = e.map_err(|e| format!("{}: {e}", d.display()))?;
            let q = e.path();
            // `file_type` does not follow links: a symlink or junction is
            // not a folder here, so it is never entered.
            let is_dir = e.file_type().map_err(|x| format!("{}: {x}", q.display()))?.is_dir();
            // Only a file is passed over: a folder git lists is a gitlink (a
            // submodule or an embedded repository), walked like any other
            // (#351 F2), so the guard does not lean on git's own refusal.
            if !is_dir && pass(&q) {
                continue;
            }
            let name = e.file_name();
            if is_checkout_name(&name) || (secrets && secret_name(&name)) {
                return Ok(Some(q));
            }
            for (i, n) in ["HEAD", "objects", "refs"].into_iter().enumerate() {
                bare[i] |= name.eq_ignore_ascii_case(n) && is_dir == (i > 0);
            }
            if is_dir {
                stack.push(q);
            }
        }
        if bare == [true; 3] {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

/// The first line of a `CACHEDIR.TAG`: the Cache Directory Tagging spec's
/// generic signature, which cargo (and other tools) write at the top of a
/// cache folder.
pub const CACHEDIR_TAG_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// Whether `p/CACHEDIR.TAG` is a plain file whose first line is
/// [`CACHEDIR_TAG_SIGNATURE`].
fn cache_tagged(p: &Path) -> bool {
    let f = p.join("CACHEDIR.TAG");
    if !std::fs::symlink_metadata(&f).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut head = Vec::new();
    let read = std::fs::File::open(&f).and_then(|h| h.take(CACHEDIR_TAG_SIGNATURE.len() as u64 + 2).read_to_end(&mut head));
    read.is_ok()
        && head.strip_prefix(CACHEDIR_TAG_SIGNATURE.as_bytes()).is_some_and(|r| r.is_empty() || r[0] == b'\n' || r.starts_with(b"\r\n"))
}

/// Whether the repository `p` lies in tracks nothing under `p`
/// (`git ls-files` there is empty; a git failure is "tracked").
fn untracked_by_parent(p: &Path, t: Duration) -> bool {
    git(p, &["ls-files", "-z", "--", "."], t).is_ok_and(|o| o.is_empty())
}

/// The linked build folder of `row` that exists and that no other open
/// ledger row links.
pub fn linked_target(cx: &Context<'_>, row: &Row) -> Option<PathBuf> {
    build_dirs(cx, row).into_iter().find(|p| {
        let k = resolved(p);
        p.is_dir()
            && !cx
                .rows
                .iter()
                .filter(|r| r.id != row.id)
                .any(|r| cx.settings.target_dirs.iter().filter_map(|t| target_of(t, folder_name(&r.path))).any(|q| resolved(&q) == k))
    })
}

/// The bytes under `p` (links not followed; unreadable entries count 0).
pub fn dir_size(p: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(p) else { return 0 };
    if !md.is_dir() {
        return md.len();
    }
    std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| dir_size(&e.path())).sum()).unwrap_or(0)
}

/// The main checkout of the repository called `name`.
pub fn repo_path<'a>(repos: &'a [crate::project::Repo], name: &str) -> Option<&'a Path> {
    repos.iter().find(|r| r.name == name).and_then(|r| r.path.as_deref())
}

/// The plan for every open ledger row, sizes measured when `sizes`.
pub fn plan(cx: &Context<'_>, sizes: bool) -> Vec<Entry> {
    let mut out = Vec::new();
    for row in cx.rows {
        let mut e = Entry {
            row: row.clone(),
            act: Act::Keep,
            why: String::new(),
            found: Vec::new(),
            branch: row.branch.clone(),
            bytes: None,
            target: None,
            done: None,
        };
        if !Path::new(&row.path).is_dir() {
            e.why = "folder gone".into();
            out.push(e);
            continue;
        }
        match stale(cx, row) {
            Err(why) => e.why = why,
            Ok(why) => {
                e.why = why;
                let wt = Path::new(&row.path);
                let build = build_dirs(cx, row);
                let probe = Probe {
                    path: wt,
                    repo: repo_path(cx.repos, &row.repo),
                    timeout: cx.settings.timeout,
                    build: &build,
                    disposable: &cx.settings.disposable,
                };
                let (found, branch) = wip(&probe);
                e.branch = branch;
                // The file walk last, and only for a tree gc would touch.
                let touched = found.is_empty() || cx.settings.dirty != Dirty::Report;
                if let Some(why) = (touched && cx.settings.plan_walks).then(|| files_changed(cx, row)).flatten() {
                    e.why = why;
                    e.found = found;
                    out.push(e);
                    continue;
                }
                e.act = match (found.is_empty(), cx.settings.dirty) {
                    (true, _) => Act::Remove,
                    (false, Dirty::Report) => Act::Work,
                    // Build output goes only from a worktree git answered
                    // for, and never from a locked one.
                    (false, Dirty::RemoveTarget)
                        if !found.iter().any(|(w, _)| matches!(w, Work::Unknown | Work::NotAWorktree | Work::Locked)) =>
                    {
                        Act::Target
                    }
                    (false, Dirty::RemoveTarget) => Act::Work,
                    (false, Dirty::CommitDelete) if found.iter().all(|(w, _)| w.committable()) => {
                        match unsafe_to_commit(wt, cx.settings.commit_limit, cx.settings.timeout) {
                            None => Act::Commit,
                            Some(r) => {
                                e.why.push_str(&format!("; not committed: {r}"));
                                Act::Work
                            }
                        }
                    }
                    (false, Dirty::CommitDelete) => Act::Work,
                };
                e.found = found;
                // Walks only for a row gc plans to delete from, and not in
                // the notify-only run: the walk the removal makes, early.
                let walks = cx.settings.plan_walks && e.act != Act::Work;
                if walks && matches!(e.act, Act::Remove | Act::Commit) {
                    let repo = repo_path(cx.repos, &row.repo).unwrap_or(wt);
                    if let Some(r) = delete_refusal(&Delete::Worktree { repo, wt, timeout: cx.settings.timeout }) {
                        e.found.push((Work::Ignored, r));
                        e.act = Act::Work;
                    }
                }
                if e.act != Act::Work {
                    let linked = linked_target(cx, row);
                    let refusal = |p: &Path| if walks { build_refusal(cx, p) } else { build_refusal_unwalked(cx, p) };
                    match linked.as_deref().and_then(refusal) {
                        Some(r) => e.why.push_str(&format!("; target refused: {r}")),
                        None => e.target = linked.map(|p| (p, None)),
                    }
                }
            }
        }
        out.push(e);
    }
    if sizes {
        measure(&mut out, None);
    }
    out
}

/// Measures the plan's trees to remove and their build folders, in order,
/// until `cap` bytes are counted (`None`: all of them).
pub fn measure(entries: &mut [Entry], cap: Option<u64>) {
    let mut total = 0u64;
    for e in entries.iter_mut().filter(|e| matches!(e.act, Act::Remove | Act::Commit | Act::Target)) {
        if cap.is_some_and(|c| total >= c) {
            return;
        }
        if matches!(e.act, Act::Remove | Act::Commit) && e.bytes.is_none() {
            let n = dir_size(Path::new(&e.row.path));
            total = total.saturating_add(n);
            e.bytes = Some(n);
        }
        if let Some((p, n @ None)) = &mut e.target {
            let m = dir_size(p);
            total = total.saturating_add(m);
            *n = Some(m);
        }
    }
}

/// Removes the worktree at `wt` of the repository at `repo`: the work check
/// again first (anything found: nothing is removed, the findings are
/// returned), then [`guarded_delete`]: the walk of the whole tree, then
/// `git worktree remove` without `--force`, so git's own refusal stands.
///
/// # Errors
///
/// What it holds, the walk's refusal, or git's.
pub fn remove_tree(repo: &Path, wt: &Path, build: &[PathBuf], disposable: &[String], t: Duration) -> Result<(), String> {
    let (found, _) = wip(&Probe { path: wt, repo: Some(repo), timeout: t, build, disposable });
    if !found.is_empty() {
        return Err(format!("holds work now ({})", describe(&found)));
    }
    guarded_delete(&Delete::Worktree { repo, wt, timeout: t })
}

/// The in-tree build folder `target`, when it exists and git ignores it.
pub fn tree_target(wt: &Path, t: Duration) -> Option<PathBuf> {
    let p = wt.join("target");
    let md = std::fs::symlink_metadata(&p).ok()?;
    (md.is_dir() && call("git", Some(wt), &["check-ignore", "-q", "target"], t).ok()?.code == Some(0)).then_some(p)
}

/// Why the changes of the worktree at `wt` must not be committed and pushed:
/// a line its tracked files gain is flagged as below, or those changes are
/// over `limit` bytes; or one of its untracked files is over `limit` bytes,
/// has a denied name
/// ([`crate::sources::is_denied`]: `.env`, keys), or holds a line the
/// redaction rules or the leak denylist flag ([`crate::redact`]); or the
/// denylist is configured and cannot be read, or git cannot list them.
/// `None`: all of them may go.
pub fn unsafe_to_commit(wt: &Path, limit: u64, t: Duration) -> Option<String> {
    if crate::redact::denylist_unreadable() {
        return Some("the leak denylist cannot be read".into());
    }
    // The lines tracked files gain (staged or not) go too.
    let diff = match git(wt, &["diff", "HEAD", "--no-color", "--no-ext-diff", "--no-textconv", "-U0"], t) {
        Ok(d) => d,
        Err(e) => return Some(e),
    };
    if diff.len() as u64 > limit {
        return Some(format!("the changes to tracked files are {}, over the {} limit", size(diff.len() as u64), size(limit)));
    }
    let mut added = diff.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++ ")).map(|l| &l[1..]);
    if added.any(|l| crate::redact::is_secret_line(l, crate::redact::denylist())) {
        return Some("a change to a tracked file adds a line that looks secret or is on the leak denylist".into());
    }
    let list = match git(wt, &["ls-files", "--others", "--exclude-standard", "-z"], t) {
        Ok(l) => l,
        Err(e) => return Some(e),
    };
    for rel in list.split('\0').filter(|s| !s.is_empty()) {
        let p = wt.join(rel);
        let name = folder_name(rel);
        if crate::sources::is_denied(name) {
            return Some(format!("{rel} has a denied name"));
        }
        match std::fs::symlink_metadata(&p) {
            Ok(md) if md.len() > limit => return Some(format!("{rel} is {}, over the {} limit", size(md.len()), size(limit))),
            Ok(md) if md.is_file() => {}
            Ok(_) => continue,
            Err(e) => return Some(format!("{rel}: {e}")),
        }
        let text = match std::fs::read(&p) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => return Some(format!("{rel}: {e}")),
        };
        if text.lines().any(|l| crate::redact::is_secret_line(l, crate::redact::denylist())) {
            return Some(format!("{rel} holds a line that looks secret or is on the leak denylist"));
        }
    }
    None
}

/// The `index.lock` a killed git call can leave in the worktree at `wt`:
/// reported, never deleted.
pub fn index_lock_note(wt: &Path, t: Duration) -> String {
    match git(wt, &["rev-parse", "--path-format=absolute", "--git-path", "index.lock"], t) {
        Ok(p) if Path::new(p.trim()).exists() => format!(" ({} is left; atlas does not delete it)", p.trim()),
        _ => String::new(),
    }
}

/// The `commit_delete` chain on the worktree at `wt` (branch `branch`): its
/// untracked files checked ([`unsafe_to_commit`]), every change committed
/// (untracked non-ignored files included), pushed without force, the remote
/// ref read back and compared with HEAD, the remote-tracking ref fetched,
/// then [`remove_tree`] (which checks for work again). Any step that fails
/// stops the chain with nothing removed. The writing calls get
/// [`WRITE_TIMEOUT`]; one that fails is reported with any `index.lock` it
/// left.
///
/// # Errors
///
/// The step that failed.
pub fn commit_delete(repo: &Path, wt: &Path, branch: &str, build: &[PathBuf], s: &GcSettings) -> Result<String, String> {
    if branch.is_empty() {
        return Err("detached HEAD: no branch to push the work to".into());
    }
    if let Some(r) = unsafe_to_commit(wt, s.commit_limit, s.timeout) {
        return Err(format!("not committed: {r}"));
    }
    let t = s.timeout.max(WRITE_TIMEOUT);
    let msg = commit_push(wt, branch, t).map_err(|e| format!("{e}{}", index_lock_note(wt, s.timeout)))?;
    remove_tree(repo, wt, build, &s.disposable, s.timeout)?;
    Ok(msg)
}

/// [`commit_delete`]'s commit, push, read-back and fetch.
fn commit_push(wt: &Path, branch: &str, t: Duration) -> Result<String, String> {
    git(wt, &["add", "-A"], t)?;
    let staged = call("git", Some(wt), &["diff", "--cached", "--quiet"], t)?;
    match staged.code {
        Some(0) => {}
        Some(1) => {
            git(wt, &["commit", "-q", "-m", "WIP: saved by atlas worktrees gc before removing this worktree"], t)?;
        }
        _ => return Err(format!("git diff --cached in {}: {}", wt.display(), staged.stderr.trim())),
    }
    let remote = git(wt, &["config", "--get", &format!("branch.{branch}.remote")], t).map(|r| r.trim().to_string()).unwrap_or_default();
    let remote = if remote.is_empty() || remote == "." { "origin".to_string() } else { remote };
    let head = git(wt, &["rev-parse", "HEAD"], t)?.trim().to_string();
    let dst = format!("refs/heads/{branch}");
    git(wt, &["push", "-q", &remote, &format!("HEAD:{dst}")], t).map_err(|e| format!("push failed: {e}"))?;
    let ls = git(wt, &["ls-remote", &remote, &dst], t)?;
    let theirs = ls.lines().find_map(|l| l.split_once('\t').filter(|(_, r)| r.trim() == dst).map(|(s, _)| s.trim().to_string()));
    if theirs.as_deref() != Some(head.as_str()) {
        return Err(format!("the remote's {dst} is {} after the push, not {head}", theirs.as_deref().unwrap_or("missing")));
    }
    git(wt, &["fetch", "-q", &remote, &format!("+{dst}:refs/remotes/{remote}/{branch}")], t)?;
    Ok(format!("work pushed to {remote}/{branch} at {}", &head[..head.len().min(12)]))
}

/// Deletes a linked build folder after [`build_refusal`] again (its walk
/// inside [`guarded_delete`]).
fn remove_linked(cx: &Context<'_>, p: &Path) -> Result<(), String> {
    match build_refusal_unwalked(cx, p) {
        Some(r) => Err(format!("refused: {r}")),
        None => guarded_delete(&Delete::Folder(p)),
    }
}

/// Carries out the plan's worktree part: returns the paths removed. Just
/// before each worktree is touched, [`in_use`] runs again with the running
/// agents `live` reads then (`None`: unreadable, so nothing is touched), and
/// the work check again inside [`remove_tree`].
pub fn apply(cx: &Context<'_>, entries: &mut [Entry], live: &dyn Fn() -> Option<Vec<crate::lanes::Holder>>) -> Vec<String> {
    apply_with(cx, entries, live, &|| Some(cx.waiting.to_vec()))
}

/// [`apply`], with the waiting queue items read again through `queue` just
/// before each worktree is touched too (`None`: unreadable, so nothing is
/// touched).
pub fn apply_with(
    cx: &Context<'_>,
    entries: &mut [Entry],
    live: &dyn Fn() -> Option<Vec<crate::lanes::Holder>>,
    queue: &dyn Fn() -> Option<Vec<crate::lanes::Item>>,
) -> Vec<String> {
    let t = cx.settings.timeout;
    let mut removed = Vec::new();
    for e in entries.iter_mut() {
        let wt = PathBuf::from(&e.row.path);
        let Some(repo) = repo_path(cx.repos, &e.row.repo) else {
            if e.act != Act::Keep {
                e.done = Some("left: its repository is not configured".into());
            }
            continue;
        };
        if matches!(e.act, Act::Keep | Act::Work) {
            continue;
        }
        let holders = live();
        let Some(waiting) = queue() else {
            e.done = Some("left: the waiting queue could not be read".into());
            continue;
        };
        let now = Context { holders: holders.as_deref(), waiting: &waiting, ..*cx };
        if let Some(why) = in_use(&now, &e.row) {
            e.done = Some(format!("left: {why}"));
            continue;
        }
        let build = build_dirs(cx, &e.row);
        let r = match e.act {
            Act::Keep | Act::Work => continue,
            Act::Remove => remove_tree(repo, &wt, &build, &cx.settings.disposable, t).map(|()| "removed".to_string()),
            Act::Commit => commit_delete(repo, &wt, &e.branch, &build, cx.settings),
            Act::Target => {
                let mut gone = Vec::new();
                let mut errs = Vec::new();
                let linked = e.target.iter().map(|(p, _)| (p.clone(), true));
                for (p, is_linked) in linked.chain(tree_target(&wt, t).map(|p| (p, false))) {
                    let r = if is_linked { remove_linked(cx, &p) } else { guarded_delete(&Delete::Folder(&p)) };
                    match r {
                        Ok(()) => gone.push(p.display().to_string()),
                        Err(x) => errs.push(x),
                    }
                }
                e.done = Some(match (gone.is_empty(), errs.is_empty()) {
                    (true, true) => "no build output to delete".into(),
                    (_, true) => format!("build output deleted: {}", gone.join(", ")),
                    _ => format!("build output: {}", errs.join("; ")),
                });
                continue;
            }
        };
        match r {
            Ok(msg) => {
                removed.push(e.row.path.clone());
                let mut msg = msg;
                if let Some((p, _)) = &e.target {
                    match remove_linked(cx, p) {
                        Ok(()) => msg.push_str(&format!("; target {} deleted", p.display())),
                        Err(x) => msg.push_str(&format!("; target left: {x}")),
                    }
                }
                e.done = Some(msg);
            }
            Err(why) => e.done = Some(format!("left: {why}")),
        }
    }
    removed
}

/// "1.2 GB" for `n` bytes.
pub fn size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// The bytes the plan would give back: removed trees and build folders.
pub fn reclaimable(entries: &[Entry]) -> u64 {
    entries
        .iter()
        .filter(|e| matches!(e.act, Act::Remove | Act::Commit | Act::Target))
        .map(|e| if e.act == Act::Target { 0 } else { e.bytes.unwrap_or(0) } + e.target.as_ref().and_then(|(_, n)| *n).unwrap_or(0))
        .sum()
}

/// The report: one line per worktree, the Docker lines, the note, the total.
pub fn report(entries: &[Entry], docker: Option<&crate::worktree_gc_docker::Plan>, applied: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if !applied {
        out.push_str("atlas worktrees gc: dry run (--apply carries it out)\n");
    }
    for e in entries {
        let mut l = format!(
            "{:<6} {}  lane {}  {}  {}",
            e.act.word(),
            e.row.path,
            e.row.lane,
            if e.branch.is_empty() { "(detached)" } else { &e.branch },
            e.why
        );
        if !e.found.is_empty() {
            let _ = write!(l, "; holds {}", describe(&e.found));
        }
        if let Some(n) = e.bytes {
            let _ = write!(l, "; {}", size(n));
        }
        if let Some((p, n)) = &e.target {
            let _ = write!(l, "; target {}{}", p.display(), n.map(|n| format!(" {}", size(n))).unwrap_or_default());
        }
        if let Some(d) = &e.done {
            let _ = write!(l, " -> {d}");
        }
        out.push_str(&l);
        out.push('\n');
    }
    if let Some(d) = docker {
        for l in &d.report {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str(crate::worktree_gc_docker::DISK_NOTE);
        out.push('\n');
    }
    let n = entries.iter().filter(|e| matches!(e.act, Act::Remove | Act::Commit)).count();
    let _ = writeln!(out, "reclaimable: {} ({n} worktree{} to remove)", size(reclaimable(entries)), if n == 1 { "" } else { "s" });
    out
}

/// The Docker plan: atlas's objects of checkouts that are gone (`removed`
/// by this run, or stamped removed in the ledger, folder absent) go; any
/// checkout that exists (a listed worktree, a main checkout, an open row)
/// keeps its own.
pub fn docker_plan(
    d: &dyn crate::worktree_gc_docker::Docker,
    conn: &crate::db::Connection,
    repos: &[crate::project::Repo],
    removed: &[String],
) -> Option<crate::worktree_gc_docker::Plan> {
    use crate::worktree_gc_docker as dk;
    let (containers, volumes) = dk::read(d)?;
    let mut gone = std::collections::BTreeMap::new();
    let ledger = crate::worktree_ledger::removed_paths(conn).unwrap_or_default();
    for p in ledger.iter().chain(removed) {
        if !Path::new(p).exists() {
            for h in dk::hashes_of(p) {
                gone.insert(h, p.clone());
            }
        }
    }
    let mut live = std::collections::BTreeSet::new();
    let (listed, _) = crate::worktree_ledger::listed(repos);
    let open = crate::worktree_ledger::open_rows(conn).unwrap_or_default();
    let paths = listed
        .iter()
        .map(|w| w.path.clone())
        .chain(repos.iter().filter_map(|r| r.path.as_ref().map(|p| p.to_string_lossy().into_owned())))
        .chain(open.iter().map(|r| r.path.clone()));
    for p in paths {
        if Path::new(&p).exists() {
            live.extend(dk::hashes_of(&p));
        }
    }
    let now = crate::checkpoint::epoch_secs(&crate::memory::now()).unwrap_or(0);
    Some(dk::plan(&containers, &volumes, &gone, &live, now))
}

/// What one gc run did, for the CLI and the hook's summary.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// The report text.
    pub text: String,
    /// The bytes still reclaimable (a dry run: all the plan's).
    pub reclaimable: u64,
    /// The worktrees still to remove.
    pub stale: usize,
}

/// The running agents of every session with a recent launch entry
/// ([`crate::queue::launch_holders_counted`]); `None` when the view is not
/// whole: the entries could not be read, or any session they name could
/// not be (then nothing is stale).
pub fn running_agents(orch: &crate::orchestrate::OrchSettings, store: &crate::memory::MemoryStore) -> Option<Vec<crate::lanes::Holder>> {
    match crate::queue::launch_holders_counted(orch, store, std::time::SystemTime::now()) {
        Ok((h, read, found)) if read == found => Some(h),
        Ok((_, read, found)) => {
            eprintln!("atlas worktrees gc: running agents: {read} of {found} sessions read");
            None
        }
        Err(e) => {
            eprintln!("atlas worktrees gc: running agents: {e}");
            None
        }
    }
}

/// One gc run over the open ledger rows: the plan, carried out when
/// `apply`, then Docker (through `d`; `None`: not looked at). Sizes are
/// measured up to `size_cap` bytes (`None`: all).
///
/// # Errors
///
/// Store errors reading the ledger.
pub fn run_gc(
    store: &crate::memory::MemoryStore,
    orch: &crate::orchestrate::OrchSettings,
    repos: &[crate::project::Repo],
    settings: &GcSettings,
    d: Option<&dyn crate::worktree_gc_docker::Docker>,
    apply_it: bool,
    size_cap: Option<u64>,
) -> Result<Outcome, String> {
    let conn = store.conn();
    let rows = crate::worktree_ledger::open_rows(conn).map_err(|e| e.to_string())?;
    let live = || running_agents(orch, store);
    let holders = live();
    let waiting = crate::queue::items(store, &crate::memory::now()).map(|(_, i)| i).unwrap_or_else(|e| {
        eprintln!("atlas worktrees gc: queue: {e}");
        Vec::new()
    });
    let (ends, _) = pr_ends(store, 0).map_err(|e| e.to_string())?;
    let cx = Context { rows: &rows, repos, holders: holders.as_deref(), waiting: &waiting, pr_ends: &ends, settings };
    let mut entries = plan(&cx, false);
    measure(&mut entries, size_cap);
    let queue = || match crate::queue::items(store, &crate::memory::now()) {
        Ok((_, i)) => Some(i),
        Err(e) => {
            eprintln!("atlas worktrees gc: queue: {e}");
            None
        }
    };
    let removed = if apply_it { apply_with(&cx, &mut entries, &live, &queue) } else { Vec::new() };
    if !removed.is_empty() {
        let keys: std::collections::HashSet<String> = removed.iter().map(|p| path_key(p)).collect();
        let r = crate::worktree_ledger::stamp_gone(conn, store.path(), &crate::memory::now(), |p, k| {
            keys.contains(k) && !Path::new(p).exists()
        });
        if let Err(e) = r {
            eprintln!("atlas worktrees gc: ledger: {e}");
        }
    }
    let mut dplan = d.and_then(|d| docker_plan(d, conn, repos, &removed));
    if let (true, Some(d), Some(p)) = (apply_it, d, dplan.as_mut()) {
        for e in crate::worktree_gc_docker::apply(d, p) {
            p.report.push(format!("docker: {e}"));
        }
    }
    let left: Vec<&Entry> = entries
        .iter()
        .filter(|e| matches!(e.act, Act::Remove | Act::Commit) && e.done.as_deref().is_none_or(|d| d.starts_with("left")))
        .collect();
    let reclaimable = left.iter().map(|e| e.bytes.unwrap_or(0) + e.target.as_ref().and_then(|(_, n)| *n).unwrap_or(0)).sum();
    Ok(Outcome { text: report(&entries, dplan.as_ref(), apply_it), reclaimable, stale: left.len() })
}

/// The usage of `atlas worktrees gc`.
pub const USAGE: &str = "usage: atlas worktrees gc [--apply | --auto]";

/// `atlas worktrees gc [--apply | --auto]`: the text it prints. `--auto` is
/// the hook's run: it applies when `worktrees.auto_gc` is on, writes the
/// summary the hook line reads and the log beside the store.
///
/// # Errors
///
/// A message for the user.
pub fn cli(
    args: &[String],
    store: &crate::memory::MemoryStore,
    orch: &crate::orchestrate::OrchSettings,
    repos: &[crate::project::Repo],
    settings: &GcSettings,
) -> Result<String, String> {
    let (apply_it, auto) = match args.first().map(String::as_str) {
        None => (false, false),
        Some("--apply") => (true, false),
        Some("--auto") => (settings.auto_gc, true),
        Some(o) => return Err(format!("unknown word {o:?}\n{USAGE}")),
    };
    let _lock = if apply_it || auto {
        match RunLock::take(&lock_path(store.path()), now_secs()) {
            Ok(l) => Some(l),
            Err(why) => return Ok(format!("atlas worktrees gc: another run is under way ({why}); nothing done\n")),
        }
    } else {
        None
    };
    let docker = crate::worktree_gc_docker::Cli { timeout: settings.timeout };
    // The hook's run with auto_gc off measures only as far as the threshold
    // line needs, and makes no quiet-period or guard walk (secrets, checkouts):
    // it removes nothing, and only sums sizes (`dir_size`) up to `notify_gb`.
    // The sum is an upper bound (#351 F3): a tree the removal would refuse is
    // still counted.
    let notify_only = auto && !settings.auto_gc;
    let cap = notify_only.then_some(settings.notify_bytes);
    let s = GcSettings { plan_walks: settings.plan_walks && !notify_only, ..settings.clone() };
    let o = run_gc(store, orch, repos, &s, Some(&docker), apply_it, cap)?;
    if auto {
        let _ = write_atomic(&summary_path(store.path()), &format!("{} {}\n", o.reclaimable, o.stale));
        let _ = std::fs::write(store.path().with_file_name("worktree-gc.log"), &o.text);
    }
    Ok(o.text)
}

/// The run lock beside the store: one `--apply` or `--auto` run at a time.
pub fn lock_path(store: &Path) -> PathBuf {
    store.with_file_name("worktree-gc.lock")
}

fn now_secs() -> i64 {
    crate::checkpoint::epoch_secs(&crate::memory::now()).unwrap_or(0)
}

/// The run lock, held while it lives: the operating system's exclusive lock
/// on the lock file (`File::try_lock`), which only one open file holds at a
/// time and which goes with the process, so a run that died leaves nothing
/// to take over and two runs never both hold it. The file holds the time it
/// was taken and the run's pid and token, for the reader. It is never
/// deleted: a run that deleted it could leave a waiting run locking a file
/// no longer at the path while a third locks a new one.
#[derive(Debug)]
pub struct RunLock {
    file: std::fs::File,
    /// What this run wrote into the file.
    pub token: String,
}

impl RunLock {
    /// Takes the lock at `p` at `now`, at once or not at all.
    ///
    /// # Errors
    ///
    /// Another run holds it (since when, when its file says), or the file
    /// cannot be opened or locked (a file system without locks included:
    /// gc does not run unguarded).
    pub fn take(p: &Path, now: i64) -> Result<RunLock, String> {
        use std::io::{Seek as _, Write as _};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(p)
            .map_err(|e| format!("{}: {e}", p.display()))?;
        match f.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let text = std::fs::read_to_string(p).unwrap_or_default();
                return Err(match text.split_whitespace().next().and_then(|s| s.parse::<i64>().ok()) {
                    Some(s) => format!("{} taken {} s ago", p.display(), now - s),
                    None => format!("{} is held", p.display()),
                });
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(format!("{}: {e}", p.display())),
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let token = format!("{now} {} {nanos:09}", std::process::id());
        let _ = f.set_len(0).and_then(|()| f.rewind()).and_then(|()| writeln!(f, "{token}"));
        Ok(RunLock { file: f, token })
    }

    /// A copy of the open lock file, as a child process holds one between
    /// its fork and its exec (a test seam).
    ///
    /// # Errors
    ///
    /// The I/O error.
    #[doc(hidden)]
    pub fn copy_of_file(&self) -> std::io::Result<std::fs::File> {
        self.file.try_clone()
    }
}

/// Unlocks before the file closes. A flock belongs to the open file, and a
/// process that another thread is starting holds a copy of every open file
/// until it execs, so closing alone could leave the lock held past the drop.
/// An unlock frees it through any copy.
impl Drop for RunLock {
    fn drop(&mut self) {
        // A failed unlock is left to the close, which frees it as before.
        let _ = self.file.unlock();
    }
}

/// Writes `text` to `p` through a temp file and a rename, so a reader sees
/// the old text or the new, never half.
///
/// # Errors
///
/// The I/O error.
pub fn write_atomic(p: &Path, text: &str) -> std::io::Result<()> {
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, p).inspect_err(|_| {
        let _ = guarded_delete(&Delete::OwnFile(&tmp));
    })
}

/// The summary file the hook line reads: `BYTES STALE`.
pub fn summary_path(store: &Path) -> PathBuf {
    store.with_file_name("worktree-gc.summary")
}

/// The stamp file of the last hook-started run: `EPOCH_SECS RID`.
pub fn stamp_path(store: &Path) -> PathBuf {
    store.with_file_name("worktree-gc.stamp")
}

/// How often the hook starts a run at most.
pub const EVERY: i64 = 3_600;

/// Whether the hook starts a run now: an hour since the last (SessionStart
/// and SubagentStop only), or at once after a merged PR event note newer
/// than the last run's record id. `stamp` is the stamp file's text.
pub fn due(stamp: &str, now: i64, event: &str, merged_since: bool) -> bool {
    let last = stamp.split_whitespace().next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    merged_since || (matches!(event, "SessionStart" | "SubagentStop") && now - last >= EVERY)
}

/// The record id the last run read up to, from the stamp's text.
pub fn stamp_rid(stamp: &str) -> i64 {
    stamp.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// The session-start line with `auto_gc` off: when the last summary's
/// reclaimable space is at or over the threshold.
pub fn line(summary: &str, settings: &GcSettings) -> Option<String> {
    if settings.auto_gc {
        return None;
    }
    let mut f = summary.split_whitespace().map(|s| s.parse::<u64>().ok());
    let (bytes, n) = (f.next()??, f.next()??);
    (n > 0 && bytes >= settings.notify_bytes).then(|| {
        format!(
            "atlas worktrees: {n} stale worktree{} hold {} reclaimable; `atlas worktrees gc` lists them (worktrees.auto_gc is off)",
            if n == 1 { "" } else { "s" },
            size(bytes)
        )
    })
}

/// The leading options (`--config PATH`, `--project NAME`) of this process,
/// for the run it starts.
fn leading_options() -> Vec<String> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < a.len() && matches!(a[i].as_str(), "--config" | "--project") {
        out.extend([a[i].clone(), a[i + 1].clone()]);
        i += 2;
    }
    out
}

/// Starts `atlas worktrees gc --auto` detached: the hook never waits on it.
fn spawn_run() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Only atlas itself: a test binary that reaches a hook must not start
    // itself again with these words as its test filter.
    if exe.file_stem().and_then(|s| s.to_str()) != Some("atlas") {
        return Ok(());
    }
    let mk = |breakaway: bool| {
        let mut c = std::process::Command::new(&exe);
        c.args(leading_options()).args(["worktrees", "gc", "--auto"]);
        c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            c.creation_flags(CREATE_NO_WINDOW | if breakaway { CREATE_BREAKAWAY_FROM_JOB } else { 0 });
        }
        #[cfg(not(windows))]
        let _ = breakaway;
        c
    };
    // A job that refuses breakaway refuses the start: then without it.
    mk(true).spawn().or_else(|_| mk(false).spawn()).map(|_| ()).map_err(|e| e.to_string())
}

/// The hook's part (SessionStart, SubagentStop, UserPromptSubmit): starts a
/// detached run when [`due`], after writing the stamp, and returns the
/// session-start line when `auto_gc` is off. Errors are one stderr line.
pub fn tick(store: &crate::memory::MemoryStore, settings: &GcSettings, event: &str) -> Option<String> {
    let sp = stamp_path(store.path());
    let stamp = std::fs::read_to_string(&sp).unwrap_or_default();
    let now = crate::checkpoint::epoch_secs(&crate::memory::now()).unwrap_or(0);
    let (ends, top) = pr_ends(store, stamp_rid(&stamp)).unwrap_or_default();
    let merged_since = ends.iter().any(|e| e.verdict == "merged");
    if due(&stamp, now, event, merged_since) {
        let top = if stamp.is_empty() { store.max_rid().unwrap_or(top) } else { top.max(stamp_rid(&stamp)) };
        if write_atomic(&sp, &format!("{now} {top}\n")).is_ok() {
            if let Err(e) = spawn_run() {
                eprintln!("atlas worktrees gc: could not start: {e}");
            }
        }
    }
    (event == "SessionStart").then(|| session_line(store, settings)).flatten()
}

/// The session-start line (auto_gc off, stale space over `notify_gb`), from
/// the stored summary alone: one small file read, so a spent hook budget
/// that skipped [`tick`] still shows it (#355 N4).
pub fn session_line(store: &crate::memory::MemoryStore, settings: &GcSettings) -> Option<String> {
    line(&std::fs::read_to_string(summary_path(store.path())).unwrap_or_default(), settings)
}
