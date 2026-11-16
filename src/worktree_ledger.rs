//! The worktree ownership ledger: which lane owns which git worktree, one row
//! per ownership span, in the memory store (`worktree_owners`).
//!
//! The table is a ledger. Rows are never deleted, and after the insert only
//! `last_modified` and the two stamps change; SQLite triggers refuse anything
//! else, whatever the caller:
//!
//! * `removed_at`: the worktree's folder is gone (NULL while it exists).
//! * `ended_at`: another lane took the worktree over while it still exists.
//!
//! A stamp goes from NULL to a time once and never back. A row is *open*
//! while both are NULL ([`OPEN`]); a partial unique index keeps one open row
//! per path. Every write is one `BEGIN IMMEDIATE` transaction, so two
//! launches at once on one path leave one open row.
//!
//! A row is written at an admitted Agent launch from its brief's
//! `Uses: worktree:NAME` line (never from a path the brief only mentions),
//! and later launches bump `last_modified`. A launch of another lane closes
//! the span (`ended_at`) and opens a new one; a review or a `read-only`
//! launch never takes ownership. Nothing here removes a worktree, a branch or
//! a folder: removal reads this ledger and lives elsewhere.
//!
//! ```text
//! atlas worktrees [list]       the open rows: path, lane, age, last_modified
//! atlas worktrees history PATH every row for one path, oldest first
//! atlas worktrees backfill     an open row, lane unknown, per worktree that has none
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rusqlite::{params, Connection, OptionalExtension};

use crate::memory::MemoryStore;
use crate::orchestrate::{Kind, OrchSettings};
use crate::project::Repo;
use crate::Result;

/// The table, its indexes and the triggers that make it a ledger
/// (schema version 3 of the memory store; see [`crate::memory::SCHEMA_VERSION`]).
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS worktree_owners(
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL,
    path_key TEXT NOT NULL,
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    lane TEXT NOT NULL,
    description TEXT NOT NULL,
    tool_use_id TEXT NOT NULL,
    session TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_modified TEXT NOT NULL,
    removed_at TEXT,
    ended_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS worktree_owners_open ON worktree_owners(path_key) WHERE removed_at IS NULL AND ended_at IS NULL;
CREATE INDEX IF NOT EXISTS worktree_owners_path ON worktree_owners(path_key, created_at);
CREATE TRIGGER IF NOT EXISTS worktree_owners_no_delete BEFORE DELETE ON worktree_owners
    BEGIN SELECT RAISE(ABORT, 'worktree_owners is a ledger: rows are never deleted'); END;
CREATE TRIGGER IF NOT EXISTS worktree_owners_fixed BEFORE UPDATE OF id, path, path_key, repo, branch, lane, description, tool_use_id, session, created_at ON worktree_owners
    BEGIN SELECT RAISE(ABORT, 'worktree_owners is a ledger: only last_modified and the stamps change'); END;
CREATE TRIGGER IF NOT EXISTS worktree_owners_removed_once BEFORE UPDATE OF removed_at ON worktree_owners WHEN OLD.removed_at IS NOT NULL
    BEGIN SELECT RAISE(ABORT, 'worktree_owners: removed_at is stamped once'); END;
CREATE TRIGGER IF NOT EXISTS worktree_owners_ended_once BEFORE UPDATE OF ended_at ON worktree_owners WHEN OLD.ended_at IS NOT NULL
    BEGIN SELECT RAISE(ABORT, 'worktree_owners: ended_at is stamped once'); END;
CREATE TRIGGER IF NOT EXISTS worktree_owners_closed_fixed BEFORE UPDATE OF last_modified ON worktree_owners WHEN OLD.removed_at IS NOT NULL OR OLD.ended_at IS NOT NULL
    BEGIN SELECT RAISE(ABORT, 'worktree_owners: a closed span does not change'); END;
";

/// What "open" means, everywhere: the partial index, the handover, the list
/// and the removal code that will read the ledger.
pub const OPEN: &str = "removed_at IS NULL AND ended_at IS NULL";

/// The lane a row gets when the launch declares none (and the backfill).
pub const UNKNOWN: &str = "unknown";

/// The `description` of a backfilled row.
pub const BACKFILL: &str = "backfill";

/// A path normalised for matching: `\\?\` dropped, separators made `/`, a
/// trailing separator dropped, and lowercased on Windows only, where paths
/// do not tell case apart.
pub fn path_key(path: &str) -> String {
    let p = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
    };
    let mut k = p.replace('\\', "/");
    while k.len() > 1 && k.ends_with('/') && !k.ends_with(":/") {
        k.pop();
    }
    if cfg!(windows) {
        k = k.to_lowercase();
    }
    k
}

/// One worktree as `git worktree list` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// The folder, as git prints it.
    pub path: String,
    /// The configured repository's name.
    pub repo: String,
    /// The branch, without `refs/heads/`; empty for a detached HEAD.
    pub branch: String,
}

/// The linked worktrees in `git worktree list --porcelain` output: the
/// first entry (the main checkout) and bare entries are left out.
pub fn parse_porcelain(text: &str, repo: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    for (i, block) in text.replace("\r\n", "\n").split("\n\n").filter(|b| !b.trim().is_empty()).enumerate() {
        let mut path = None;
        let mut branch = String::new();
        let mut bare = false;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(p.to_string());
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
            } else if line == "bare" {
                bare = true;
            }
        }
        if let (Some(path), false, true) = (path, bare, i > 0) {
            out.push(Worktree { path, repo: repo.to_string(), branch });
        }
    }
    out
}

/// Runs `git -C repo worktree list --porcelain`.
fn git_list(repo: &Path) -> std::result::Result<String, String> {
    let mut c = std::process::Command::new("git");
    c.arg("-C").arg(repo).args(["worktree", "list", "--porcelain"]).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let o = c.output().map_err(|e| format!("git worktree list in {}: {e}", repo.display()))?;
    if !o.status.success() {
        return Err(format!("git worktree list in {}: {}", repo.display(), String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Whether `key` (a [`path_key`]) is inside one of `roots` (none: any).
fn under_roots(key: &str, roots: &[String]) -> bool {
    roots.is_empty()
        || roots.iter().any(|r| key == r || key.strip_prefix(r.as_str()).is_some_and(|rest| rest.starts_with('/') || r.ends_with('/')))
}

/// Every linked worktree `git worktree list` gives for the configured
/// repositories, on disk or not, and the errors met (one per repository git
/// could not list). `git` is run once per repository with a checkout, the
/// repositories at once (a process start is most of the cost on Windows).
pub fn listed(repos: &[Repo]) -> (Vec<Worktree>, Vec<String>) {
    let answers: Vec<(&Repo, std::result::Result<String, String>)> = std::thread::scope(|s| {
        let hs: Vec<_> = repos.iter().filter_map(|r| r.path.as_deref().map(|p| (r, s.spawn(move || git_list(p))))).collect();
        hs.into_iter().map(|(r, h)| (r, h.join().unwrap_or_else(|_| Err(format!("git worktree list for {} panicked", r.name))))).collect()
    });
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (r, a) in answers {
        match a {
            Ok(text) => out.extend(parse_porcelain(&text, &r.name)),
            Err(e) => errors.push(e),
        }
    }
    (out, errors)
}

/// [`path_key`] of `p` resolved (symlinks followed, and on Windows 8.3 short
/// names expanded), or of `p` as given when it cannot be resolved. git prints
/// resolved paths: on macOS a temp dir under `/var` comes back under
/// `/private/var`, and on Windows `RUNNER~1` comes back as `runneradmin`.
fn resolved_key(p: &Path) -> String {
    path_key(&std::fs::canonicalize(p).as_deref().unwrap_or(p).to_string_lossy())
}

/// [`listed`], kept to the worktrees whose folder exists, inside `roots`
/// when any are given. Roots and worktrees are compared resolved
/// ([`resolved_key`]); each [`Worktree::path`] stays as git prints it.
pub fn worktrees(repos: &[Repo], roots: &[PathBuf]) -> (Vec<Worktree>, Vec<String>) {
    let roots: Vec<String> = roots.iter().map(|r| resolved_key(r)).collect();
    let (mut all, errors) = listed(repos);
    all.retain(|w| Path::new(&w.path).is_dir() && under_roots(&resolved_key(Path::new(&w.path)), &roots));
    (all, errors)
}

/// The folder name of a worktree path.
fn base_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("")
}

/// What a `worktree:NAME` names among `all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named<'a> {
    /// One worktree.
    One(&'a Worktree),
    /// None.
    Unknown,
    /// More than one (in different repositories or roots): nothing is
    /// recorded, since the owner of each is not known.
    Ambiguous(Vec<&'a Worktree>),
}

/// The worktree whose folder is called `name` (without case: `Uses:` names
/// are lowercased).
pub fn resolve<'a>(name: &str, all: &'a [Worktree]) -> Named<'a> {
    let hits: Vec<&Worktree> = all.iter().filter(|w| base_name(&w.path).eq_ignore_ascii_case(name)).collect();
    match hits.len() {
        0 => Named::Unknown,
        1 => Named::One(hits[0]),
        _ => Named::Ambiguous(hits),
    }
}

/// Who a launch is, for the ledger.
#[derive(Debug, Clone, Copy)]
pub struct Owner<'a> {
    /// Its declared lane, if any.
    pub lane: Option<&'a str>,
    /// It may take ownership: not a review and not `read-only`.
    pub holds: bool,
    /// The Agent tool's `description`.
    pub description: &'a str,
    /// The launching tool call's id (the agent's id does not exist yet).
    pub tool_use_id: &'a str,
    /// The session that launched it.
    pub session: &'a str,
}

/// What [`record`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrote {
    /// A new span (no open row was there).
    Opened(i64),
    /// The open row's `last_modified` was bumped.
    Bumped(i64),
    /// Another lane's span was closed (`ended_at`) and this one opened.
    HandedOver {
        /// The row closed.
        ended: i64,
        /// The row opened.
        opened: i64,
    },
    /// Nothing: a launch that cannot take ownership found no open row.
    Nothing,
}

fn insert(conn: &Connection, w: &Worktree, lane: &str, o: &Owner<'_>, now: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO worktree_owners(path, path_key, repo, branch, lane, description, tool_use_id, session, created_at, last_modified)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
        params![w.path, path_key(&w.path), w.repo, w.branch, lane, o.description, o.tool_use_id, o.session, now],
    )?;
    Ok(conn.last_insert_rowid())
}

fn open_row(conn: &Connection, key: &str) -> rusqlite::Result<Option<(i64, String)>> {
    conn.query_row(&format!("SELECT id, lane FROM worktree_owners WHERE path_key = ?1 AND {OPEN}"), [key], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
}

/// Records a launch on worktree `w`, in one write transaction on `conn`
/// (`store` is the file, for the version check).
///
/// # Errors
///
/// [`crate::Error::StoreTooNew`] or [`crate::Error::Db`], with nothing
/// written.
pub fn record(conn: &Connection, store: &Path, w: &Worktree, o: &Owner<'_>, now: &str) -> Result<Wrote> {
    crate::memory::guarded(conn, store, || {
        let key = path_key(&w.path);
        let bump =
            |id: i64| conn.execute("UPDATE worktree_owners SET last_modified = max(last_modified, ?2) WHERE id = ?1", params![id, now]);
        Ok(match (open_row(conn, &key)?, o.lane) {
            (Some((id, _)), _) if !o.holds => {
                bump(id)?;
                Wrote::Bumped(id)
            }
            (Some((id, _)), None) => {
                bump(id)?;
                Wrote::Bumped(id)
            }
            (Some((id, lane)), Some(l)) if crate::lanes::same_lane(&lane, l) => {
                bump(id)?;
                Wrote::Bumped(id)
            }
            (Some((id, _)), Some(l)) => {
                conn.execute("UPDATE worktree_owners SET ended_at = ?2 WHERE id = ?1", params![id, now])?;
                Wrote::HandedOver { ended: id, opened: insert(conn, w, l, o, now)? }
            }
            (None, _) if !o.holds => Wrote::Nothing,
            (None, l) => Wrote::Opened(insert(conn, w, l.unwrap_or(UNKNOWN), o, now)?),
        })
    })
}

/// One ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Row id.
    pub id: i64,
    /// The folder, as resolved.
    pub path: String,
    /// [`path_key`] of it.
    pub path_key: String,
    /// The repository.
    pub repo: String,
    /// The branch (empty: detached).
    pub branch: String,
    /// The owning lane.
    pub lane: String,
    /// The launch's description.
    pub description: String,
    /// The launch's tool call id.
    pub tool_use_id: String,
    /// The launch's session.
    pub session: String,
    /// When the span began.
    pub created_at: String,
    /// The last launch on it.
    pub last_modified: String,
    /// When the folder was found gone.
    pub removed_at: Option<String>,
    /// When another lane took it over.
    pub ended_at: Option<String>,
}

const COLUMNS: &str =
    "id, path, path_key, repo, branch, lane, description, tool_use_id, session, created_at, last_modified, removed_at, ended_at";

fn rows(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Row>> {
    let mut st = conn.prepare(sql)?;
    let v = st
        .query_map(p, |r| {
            Ok(Row {
                id: r.get(0)?,
                path: r.get(1)?,
                path_key: r.get(2)?,
                repo: r.get(3)?,
                branch: r.get(4)?,
                lane: r.get(5)?,
                description: r.get(6)?,
                tool_use_id: r.get(7)?,
                session: r.get(8)?,
                created_at: r.get(9)?,
                last_modified: r.get(10)?,
                removed_at: r.get(11)?,
                ended_at: r.get(12)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(v)
}

/// The open rows, oldest first.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn open_rows(conn: &Connection) -> Result<Vec<Row>> {
    rows(conn, &format!("SELECT {COLUMNS} FROM worktree_owners WHERE {OPEN} ORDER BY created_at, id"), params![])
}

/// Every row for one path (matched by [`path_key`] of `path` as given or
/// resolved, since rows hold git's resolved path), oldest first.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn history(conn: &Connection, path: &str) -> Result<Vec<Row>> {
    rows(
        conn,
        &format!("SELECT {COLUMNS} FROM worktree_owners WHERE path_key IN (?1, ?2) ORDER BY created_at, id"),
        [path_key(path), resolved_key(Path::new(path))],
    )
}

/// Stamps `removed_at` on every row not yet stamped whose path `gone`
/// says is gone. Reads first and opens a write transaction only when there
/// is something to stamp. Returns the ids stamped.
///
/// # Errors
///
/// [`crate::Error::StoreTooNew`] or [`crate::Error::Db`].
pub fn stamp_gone(conn: &Connection, store: &Path, now: &str, gone: impl Fn(&str, &str) -> bool) -> Result<Vec<i64>> {
    let mut st = conn.prepare("SELECT id, path, path_key FROM worktree_owners WHERE removed_at IS NULL")?;
    let live: Vec<(i64, String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    drop(st);
    let ids: Vec<i64> = live.into_iter().filter(|(_, p, k)| gone(p, k)).map(|(id, _, _)| id).collect();
    if ids.is_empty() {
        return Ok(ids);
    }
    crate::memory::guarded(conn, store, || {
        let mut done = Vec::new();
        for id in &ids {
            if conn.execute("UPDATE worktree_owners SET removed_at = ?2 WHERE id = ?1 AND removed_at IS NULL", params![id, now])? > 0 {
                done.push(*id);
            }
        }
        Ok(done)
    })
}

/// An open row, lane [`UNKNOWN`] and no launch, for each of `wts` that has
/// no open row; rows already open are left alone. Returns how many were
/// written.
///
/// # Errors
///
/// [`crate::Error::StoreTooNew`] or [`crate::Error::Db`], with nothing
/// written.
pub fn backfill(conn: &Connection, store: &Path, wts: &[Worktree], now: &str) -> Result<usize> {
    crate::memory::guarded(conn, store, || {
        let o = Owner { lane: None, holds: true, description: BACKFILL, tool_use_id: "", session: "" };
        let mut n = 0;
        for w in wts {
            if open_row(conn, &path_key(&w.path))?.is_none() {
                insert(conn, w, UNKNOWN, &o, now)?;
                n += 1;
            }
        }
        Ok(n)
    })
}

/// An Agent launch, as the hook saw it.
#[derive(Debug, Clone, Copy)]
pub struct Launch<'a> {
    /// The brief (the launch prompt).
    pub brief: &'a str,
    /// Its kind.
    pub kind: Kind,
    /// The Agent tool's `description`.
    pub description: &'a str,
    /// The tool call's id.
    pub tool_use_id: &'a str,
    /// The session.
    pub session: &'a str,
}

/// What [`at_launch`] did, for the hook's stderr and the tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Done {
    /// Each declared worktree name and what was written for it.
    pub wrote: Vec<(String, Wrote)>,
    /// Rows stamped removed.
    pub removed: Vec<i64>,
    /// Failures, one line each; none fails the launch.
    pub errors: Vec<String>,
}

/// The declared `worktree:NAME` entries of a brief.
pub fn declared_worktrees(brief: &str) -> Vec<String> {
    crate::lanes::declared(brief)
        .uses
        .unwrap_or_default()
        .into_iter()
        .filter_map(|u| u.strip_prefix("worktree:").map(str::to_string))
        .filter(|n| !n.is_empty())
        .collect()
}

/// The ledger's part of an admitted launch: stamps rows whose folder is gone
/// (a stat each, no git), then, only when the brief declares
/// `Uses: worktree:NAME`, lists the repositories' worktrees (one `git` per
/// repository) and records each name. Never fails: errors are returned as
/// lines.
pub fn at_launch(store: &MemoryStore, orch: &OrchSettings, repos: &[Repo], l: &Launch<'_>) -> Done {
    let mut done = Done::default();
    let conn = store.conn();
    let now = crate::memory::now();
    match stamp_gone(conn, store.path(), &now, |p, _| !Path::new(p).exists()) {
        Ok(ids) => done.removed = ids,
        Err(e) => done.errors.push(e.to_string()),
    }
    let names = declared_worktrees(l.brief);
    if names.is_empty() {
        return done;
    }
    let d = crate::lanes::declared(l.brief);
    let holds = !d.read_only && l.kind != Kind::Reviewer;
    let owner = Owner { lane: d.lane.as_deref(), holds, description: l.description, tool_use_id: l.tool_use_id, session: l.session };
    let (all, errors) = worktrees(repos, &orch.worktree_roots);
    done.errors.extend(errors);
    for name in names {
        match resolve(&name, &all) {
            Named::One(w) => match record(conn, store.path(), w, &owner, &now) {
                Ok(x) => done.wrote.push((name, x)),
                Err(e) => done.errors.push(format!("worktree:{name}: {e}")),
            },
            Named::Unknown => {
                done.errors.push(format!("worktree:{name} names no worktree of the configured repositories; nothing recorded"))
            }
            Named::Ambiguous(v) => done.errors.push(format!(
                "worktree:{name} names {} worktrees ({}); nothing recorded (orchestrate.worktree_roots narrows it)",
                v.len(),
                v.iter().map(|w| w.path.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }
    done
}

/// The hook's call at an admitted Agent launch (after the queue and the
/// judge let it through): [`at_launch`] when `orchestrate.enabled` is on,
/// beside the hook's answer and never changing it. A failure is one stderr
/// line.
pub fn hook(store: &MemoryStore, settings: &crate::memory::settings::Settings, ev: &crate::hooks::HookEvent, brief: &str) {
    if !settings.orchestrate.enabled {
        return;
    }
    let f = |k: &str| ev.tool_input.get(k).and_then(crate::json::Value::as_str).unwrap_or("");
    let kind = crate::orchestrate::classify(&settings.orchestrate, brief, f("subagent_type"), f("isolation"));
    let repos = settings.graph.as_deref().map_or(&[][..], |g| &g.repos[..]);
    let l = Launch { brief, kind, description: f("description"), tool_use_id: &ev.tool_use_id, session: &ev.session_id };
    for e in at_launch(store, &settings.orchestrate, repos, &l).errors {
        eprintln!("atlas worktrees: {e}");
    }
}

/// The usage of `atlas worktrees`.
pub const USAGE: &str = "usage: atlas worktrees [list | history PATH | backfill]";

/// `atlas worktrees`: the ledger's list (which first stamps rows whose
/// folder is gone from both the disk and `git worktree list`), one path's
/// history, and the backfill. Nothing on disk or in git is changed.
///
/// # Errors
///
/// A message for the user.
pub fn cli(args: &[String], store: &MemoryStore, orch: &OrchSettings, repos: &[Repo]) -> std::result::Result<ExitCode, String> {
    print!("{}", run(args, store, orch, repos)?);
    Ok(ExitCode::SUCCESS)
}

/// [`cli`]'s work: the text it prints. Git's errors go to stderr.
///
/// # Errors
///
/// A message for the user.
pub fn run(args: &[String], store: &MemoryStore, orch: &OrchSettings, repos: &[Repo]) -> std::result::Result<String, String> {
    use std::fmt::Write as _;
    let mut out = String::new();
    let s = |e: crate::Error| e.to_string();
    let conn = store.conn();
    let now = crate::memory::now();
    match args.first().map_or("list", String::as_str) {
        "list" => {
            let (all, errors) = listed(repos);
            for e in &errors {
                eprintln!("atlas worktrees: {e}");
            }
            // Gone means gone from the disk and from git. With a repository
            // git could not list, git's answer is not whole: nothing is stamped.
            let known: std::collections::HashSet<String> = all.iter().map(|w| path_key(&w.path)).collect();
            let removed = stamp_gone(conn, store.path(), &now, |p, k| errors.is_empty() && !Path::new(p).exists() && !known.contains(k))
                .map_err(s)?;
            if !removed.is_empty() {
                let _ = writeln!(
                    out,
                    "{} row{} stamped removed (folder and worktree gone)",
                    removed.len(),
                    if removed.len() == 1 { "" } else { "s" }
                );
            }
            let open = open_rows(conn).map_err(s)?;
            if open.is_empty() {
                let _ = writeln!(out, "no worktree has an open owner row");
            }
            for r in &open {
                let _ = writeln!(
                    out,
                    "{}  lane {}  {} old  last {}  {}",
                    r.path,
                    r.lane,
                    crate::queue::age(&r.created_at, &now),
                    r.last_modified,
                    r.repo
                );
            }
            Ok(out)
        }
        "history" => {
            let p = args.get(1).ok_or_else(|| format!("name a path\n{USAGE}"))?;
            let h = history(conn, p).map_err(s)?;
            if h.is_empty() {
                let _ = writeln!(out, "no rows for {p}");
            }
            for r in &h {
                let state = match (&r.removed_at, &r.ended_at) {
                    (Some(t), _) => format!("removed {t}"),
                    (None, Some(t)) => format!("ended {t}"),
                    (None, None) => "open".into(),
                };
                let _ = writeln!(
                    out,
                    "{}  {}  lane {}  {}  created {}  last {}  {}",
                    r.id, state, r.lane, r.branch, r.created_at, r.last_modified, r.description
                );
            }
            Ok(out)
        }
        "backfill" => {
            let (all, errors) = worktrees(repos, &orch.worktree_roots);
            for e in &errors {
                eprintln!("atlas worktrees: {e}");
            }
            let n = backfill(conn, store.path(), &all, &now).map_err(s)?;
            let _ = writeln!(
                out,
                "{n} row{} written; {} worktree{} found",
                if n == 1 { "" } else { "s" },
                all.len(),
                if all.len() == 1 { "" } else { "s" }
            );
            Ok(out)
        }
        other => Err(format!("unknown word {other:?}\n{USAGE}")),
    }
}
