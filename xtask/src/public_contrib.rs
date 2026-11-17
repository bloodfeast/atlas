//! Outside pull requests on the public repository: brought into the
//! development repository, kept through every release, and credited.
//!
//! ```text
//! cargo xtask release import [--remote URL] [--dry-run]
//!     In the development checkout (clean): clones the public repository
//!     (never pushes to it), finds every change on its main that is not a
//!     release's and that the development repository's main does not hold
//!     yet, and for each one, the squash commit of a merged outside pull
//!     request, makes branch import/public-<n> on the development main with
//!     that change applied and its author kept, pushes the branch to the
//!     development repository (origin) and opens a pull request there whose
//!     body names the public pull request and commit. A changelog fragment
//!     the outside pull request carries, changes/<n>.md, is renamed
//!     changes/public-<n>.md with `author: <login>` added, so it never
//!     collides with a development pull request's and the CHANGELOG line
//!     credits its author. A change the development main holds already is
//!     skipped and named. --dry-run makes the commits and pushes nothing.
//!     Refuses before anything is opened when the public main holds a
//!     commit the release's address check refuses ([`address_guard`]).
//! ```
//!
//! **Held.** A public change is held by a development commit when that
//! commit's history carries a `Public-commit: <sha>` line naming it (an
//! import's commit does; an import strips such lines from the public
//! message it copies), or when the change, `changes/` aside, applies in
//! reverse to its tree (the change is there already, made by hand).
//!
//! **The export guard** ([`export_guard`]): the release puts the
//! development tree on top of the public main, which would revert a public
//! change the development tree lacks. `release --pr` and run 1 of `release
//! ship` refuse before anything is pushed or dispatched while the public
//! main holds a change that is not a release's (`atlas vX.Y.Z`, `atlas
//! vX.Y.Z (#N)` or `atlas vX.Y.Z binaries`, every address allowed) and is
//! not held, naming each one and saying to run the import first.
//!
//! **The address rule** ([`outside_pr`]): the public history's address
//! check lets a commit's author address through when GitHub says the commit
//! is the squash commit of a pull request merged into main whose author is
//! a user other than the repository's owner, and whose commit author is
//! that same GitHub account, committed by GitHub itself (`web-flow`). No
//! address is listed by hand: the owner's own addresses link to the owner's
//! account, so they still fail as an outside commit's author, and every
//! committer and `Co-authored-by:` address is checked as before.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::public_repo::Api;
use crate::release::{mkdir, output, rm_rf, say, write};
use crate::release_ci::{clone, git, git_out, log_addresses, short, ADDRESS_LOG};
use atlas::json::Value;

type R<T = ()> = Result<T, String>;

/// The trailer an import's commit carries, naming the public commit.
pub const TRAILER: &str = "Public-commit";

/// The branch an import of public pull request `n` makes.
pub fn branch_of(n: u64) -> String {
    format!("import/public-{n}")
}

// ---------------------------------------------------------------- release commits

/// Whether a public commit's subject is a release's: `atlas vX.Y.Z`, the
/// release pull request's squash `atlas vX.Y.Z (#N)`, or `atlas vX.Y.Z
/// binaries`.
pub fn release_subject(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("atlas v") else { return false };
    let (ver, tail) = rest.split_once(' ').unwrap_or((rest, ""));
    let pr = |t: &str| {
        t.strip_prefix("(#").and_then(|t| t.strip_suffix(')')).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    atlas::update::parse_version(ver).is_some() && (tail.is_empty() || tail == "binaries" || pr(tail))
}

/// A commit on the public main.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicCommit {
    /// Its id.
    pub sha: String,
    /// Its subject.
    pub subject: String,
    /// How the development commit holds its change, None when it does not.
    pub held: Option<String>,
}

fn git_bytes(dir: &Path, args: &[&str], index: Option<&Path>) -> R<(bool, Vec<u8>)> {
    let mut c = git(dir);
    c.args(args).stdin(Stdio::null());
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    let o = c.output().map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    Ok((o.status.success(), o.stdout))
}

/// The key of a held-mark line, `Public-commit:` or `Public-pr:` (case
/// aside), and its value.
fn held_line(line: &str) -> Option<(&'static str, &str)> {
    let (k, v) = line.split_once(':')?;
    let k = k.trim();
    [TRAILER, "Public-pr"].into_iter().find(|t| k.eq_ignore_ascii_case(t)).map(|t| (t, v.trim()))
}

/// `body` without its held-mark lines ([`held_line`]), each with the folded
/// lines that continue it.
fn strip_held_lines(body: &str) -> String {
    let mut out = Vec::new();
    let mut dropping = false;
    for l in body.lines() {
        if dropping && l.starts_with([' ', '\t']) && !l.trim().is_empty() {
            continue;
        }
        dropping = held_line(l).is_some();
        if !dropping {
            out.push(l);
        }
    }
    out.join("\n")
}

/// The public commits named by a `Public-commit: <sha>` line in `rev`'s
/// history: in a commit's trailer block (an import's commit, rebased in),
/// or anywhere in its body, where a squash with GitHub's default message
/// puts an earlier commit's trailers. An import strips such lines from the
/// public body it copies ([`strip_held_lines`]), so each one is the
/// development history's own.
pub fn trailered(dev: &Path, rev: &str) -> R<BTreeSet<String>> {
    let log = git_out(dev, &["log", "--format=%B", rev])?;
    Ok(log
        .lines()
        .filter_map(held_line)
        .filter(|(k, v)| *k == TRAILER && crate::release_ci::is_full_sha(v))
        .map(|(_, v)| v.to_ascii_lowercase())
        .collect())
}

/// Whether `sha`'s change on the public clone, `changes/` aside, is in
/// `rev`'s tree of the development checkout: it applies in reverse there.
/// A change to `changes/` alone is held.
pub fn applies_in_reverse(dev: &Path, rev: &str, public: &Path, sha: &str, work: &Path) -> R<bool> {
    let patch = change_of(public, sha)?;
    if patch.is_empty() {
        return Ok(true);
    }
    mkdir(work)?;
    let pf = work.join(format!("{}.patch", short(sha)));
    write(&pf, &patch)?;
    let index = fresh_index(dev, rev, work, "held")?;
    let pf = pf.to_string_lossy().into_owned();
    Ok(git_bytes(dev, &["apply", "--cached", "--check", "-R", &pf], Some(&index))?.0)
}

/// `sha`'s change in the clone `public` as a binary patch, `changes/` aside.
fn change_of(public: &Path, sha: &str) -> R<Vec<u8>> {
    let (ok, patch) = git_bytes(
        public,
        &["diff-tree", "-p", "--binary", "--full-index", "--no-commit-id", "--root", "-r", sha, "--", ".", ":(exclude)changes"],
        None,
    )?;
    if !ok {
        return Err(format!("cannot read the change of public commit {}", short(sha)));
    }
    Ok(patch)
}

/// A new index file under `work` holding `rev`'s tree of `dev`.
fn fresh_index(dev: &Path, rev: &str, work: &Path, name: &str) -> R<PathBuf> {
    mkdir(work)?;
    let index = work.join(format!("{name}.index"));
    let _ = std::fs::remove_file(&index);
    if !git_bytes(dev, &["read-tree", rev], Some(&index))?.0 {
        return Err(format!("cannot read the tree of {rev} in {}", dev.display()));
    }
    Ok(index)
}

/// The pull request number a squash commit's subject ends with, `(#N)`.
fn pr_in_subject(s: &str) -> Option<u64> {
    let (_, tail) = s.rsplit_once(" (#")?;
    tail.strip_suffix(')')?.parse().ok()
}

/// How a public commit is named in messages: `public #N <sha>: <subject>`.
fn named(c: &PublicCommit) -> String {
    match pr_in_subject(&c.subject) {
        Some(n) => format!("public #{n} {}: {}", short(&c.sha), c.subject),
        None => format!("{}: {}", short(&c.sha), c.subject),
    }
}

/// The commits of the public main (`public_rev` in the clone `public`) that
/// are not a release's, oldest first, each with whether the development
/// commit `dev_rev` holds it.
pub fn public_changes(dev: &Path, dev_rev: &str, public: &Path, public_rev: &str, public_email: &str, work: &Path) -> R<Vec<PublicCommit>> {
    let addrs: HashMap<String, Vec<String>> = log_addresses(&git_out(public, &["log", ADDRESS_LOG, public_rev])?).into_iter().collect();
    let trailers = trailered(dev, dev_rev)?;
    let allowed = |a: &Vec<String>| a.len() >= 2 && a.iter().all(|x| crate::release_ci::address_allowed(x, public_email));
    let mut out = Vec::new();
    for line in git_out(public, &["log", "--reverse", "--format=%H%x1f%s", public_rev])?.lines() {
        let Some((sha, subject)) = line.split_once('\x1f') else { continue };
        let release =
            release_subject(subject) && (crate::release_ci::ACCEPTED_MERGES.contains(&sha) || addrs.get(sha).is_some_and(allowed));
        if release {
            continue;
        }
        let held = if trailers.contains(&sha.to_ascii_lowercase()) {
            Some(format!("a {TRAILER}: trailer names it"))
        } else if applies_in_reverse(dev, dev_rev, public, sha, work)? {
            Some("its change is in the development tree".to_string())
        } else {
            None
        };
        out.push(PublicCommit { sha: sha.to_string(), subject: subject.to_string(), held });
    }
    Ok(out)
}

/// The export guard (see the module docs).
pub fn export_guard(dev: &Path, dev_rev: &str, public: &Path, public_rev: &str, public_email: &str, work: &Path) -> R {
    let missing: Vec<String> =
        public_changes(dev, dev_rev, public, public_rev, public_email, work)?.iter().filter(|c| c.held.is_none()).map(named).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let s = if missing.len() == 1 { "" } else { "s" };
    Err(format!(
        "the public main holds {} change{s} the development tree lacks, which this export would revert: {}. Run `cargo xtask release import` first, merge its pull request{s}, then release again (wiki/Releasing.md, Outside pull requests)",
        missing.len(),
        missing.join("; ")
    ))
}

// ---------------------------------------------------------------- the address rule

/// A merged outside pull request, as GitHub tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsidePr {
    /// Its number on the public repository.
    pub number: u64,
    /// Its author's login.
    pub login: String,
}

/// What GitHub says about public commit `sha` of `repo`: the merged
/// outside pull request it is the squash commit of, or None (see the
/// module docs for the rule).
pub fn outside_pr(api: &dyn Api, repo: &str, sha: &str) -> R<Option<OutsidePr>> {
    let owner = repo.split('/').next().unwrap_or_default();
    let get = |path: String| -> R<Value> { atlas::json::parse(&api.get(&path)?).map_err(|e| format!("gh api {path}: not JSON ({e})")) };
    let st = |v: &Value, keys: &[&str]| -> String {
        let mut v = Some(v);
        for k in keys {
            v = v.and_then(|x| x.get(k));
        }
        v.and_then(Value::as_str).unwrap_or_default().to_string()
    };
    let pulls = get(format!("repos/{repo}/commits/{sha}/pulls"))?;
    let pr = pulls.as_array().map(Vec::as_slice).unwrap_or_default().iter().find(|p| {
        let login = st(p, &["user", "login"]);
        st(p, &["merge_commit_sha"]).eq_ignore_ascii_case(sha)
            && !st(p, &["merged_at"]).is_empty()
            && st(p, &["base", "ref"]) == "main"
            && st(p, &["base", "repo", "full_name"]).eq_ignore_ascii_case(repo)
            && st(p, &["user", "type"]) == "User"
            && !login.is_empty()
            && !login.eq_ignore_ascii_case(owner)
    });
    let Some(pr) = pr else { return Ok(None) };
    let (Some(number), login) = (pr.get("number").and_then(Value::as_u64), st(pr, &["user", "login"])) else { return Ok(None) };
    let c = get(format!("repos/{repo}/commits/{sha}"))?;
    let parents = c.get("parents").and_then(Value::as_array).map_or(0, Vec::len);
    let ok = st(&c, &["author", "login"]).eq_ignore_ascii_case(&login) && st(&c, &["committer", "login"]) == "web-flow" && parents == 1;
    Ok(ok.then_some(OutsidePr { number, login }))
}

/// What the address check found. Every refused address is counted and
/// never printed; a commit GitHub did not answer for is neither allowed nor
/// counted as a refused address, and the check fails on it all the same.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Checked {
    /// Each refused address, sorted, once each.
    pub other: Vec<String>,
    /// Each commit with a refused address, and its refused slots
    /// (`author`, `committer`, `Co-authored-by`, `malformed`).
    pub commits: Vec<(String, Vec<&'static str>)>,
    /// Each commit GitHub could not be asked about, and why.
    pub unverified: Vec<(String, String)>,
}

impl Checked {
    /// Nothing refused and nothing left unverified.
    pub fn is_clean(&self) -> bool {
        self.other.is_empty() && self.unverified.is_empty()
    }

    /// The unverified commits in words, or None when GitHub answered for
    /// every commit it was asked about.
    pub fn unverified_note(&self) -> Option<String> {
        let (sha, e) = self.unverified.first()?;
        let n = self.unverified.len();
        Some(format!(
            "could not verify {n} commit{} (GitHub error for {}: {e}); that is not a refused address, but nothing passes unverified: run again once GitHub answers",
            if n == 1 { "" } else { "s" },
            short(sha)
        ))
    }

    /// The refusal as an error: `what` says where (it is followed by the
    /// count, "N addresses"), or Ok when clean.
    pub fn refuse(&self, what: &str, tail: &str) -> R {
        if !self.other.is_empty() {
            let also = self.unverified_note().map_or(String::new(), |n| format!("; it also {n}"));
            return Err(format!("{what} {} that may not be there{tail}{also}", crate::release_ci::refused(&self.other)));
        }
        match self.unverified_note() {
            Some(n) => Err(format!("the address check of the public history {n}")),
            None => Ok(()),
        }
    }
}

/// The first line of an error, at most 160 bytes, for a message.
fn first_line(e: &str) -> String {
    let l = e.lines().next().unwrap_or_default().trim();
    let mut end = l.len().min(160);
    while !l.is_char_boundary(end) {
        end -= 1;
    }
    l[..end].to_string()
}

/// The address check of `git log` output in [`ADDRESS_LOG`] format: every
/// address that is not the public one or GitHub's is refused, co-authors
/// included, except that a commit's author address passes when `outside`
/// says the commit is a merged outside pull request's ([`outside_pr`]):
/// Ok(true) passes, Ok(false) refuses, Err leaves the commit unverified.
/// Asked only of commits whose author is refused otherwise.
pub fn other_addresses_with(log: &str, public_email: &str, outside: &dyn Fn(&str) -> R<bool>) -> Checked {
    use crate::release_ci::{address_allowed, is_full_sha, ACCEPTED_MERGES};
    let mut c = Checked::default();
    for (sha, addrs) in log_addresses(log) {
        if ACCEPTED_MERGES.contains(&sha.as_str()) {
            continue;
        }
        let mut slots: Vec<&'static str> = Vec::new();
        // A commit without both addresses is refused: the log is not what
        // was asked for.
        let whole = addrs.len() >= 2 && is_full_sha(&sha);
        if !whole {
            c.other.push("<malformed>".to_string());
            slots.push("malformed");
        }
        let mut asked: Option<R<bool>> = None;
        for (i, a) in addrs.iter().enumerate() {
            if address_allowed(a, public_email) {
                continue;
            }
            // The author's own address, on the squash commit GitHub made of
            // a merged outside pull request: that slot only.
            if i == 0 && whole && !a.is_empty() {
                match asked.get_or_insert_with(|| outside(&sha)) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        c.unverified.push((sha.clone(), first_line(e)));
                        continue;
                    }
                }
            }
            let slot = match i {
                0 => "author",
                1 => "committer",
                _ => "Co-authored-by",
            };
            if !slots.contains(&slot) {
                slots.push(slot);
            }
            c.other.push(if a.is_empty() { "<empty>".to_string() } else { a.clone() });
        }
        if !slots.is_empty() {
            c.commits.push((sha, slots));
        }
    }
    c.other.sort();
    c.other.dedup();
    c
}

/// [`other_addresses_with`], GitHub asked through `api`; a GitHub error
/// leaves the commit unverified (the check fails on it).
pub fn other_addresses_on(log: &str, public_email: &str, api: &dyn Api, repo: &str) -> Checked {
    other_addresses_with(log, public_email, &|sha| outside_pr(api, repo, sha).map(|o| o.is_some()))
}

/// The address check of the whole public main (`public_rev` in the clone
/// `public`), with the outside rule, before anything is opened or
/// dispatched: the same check the release makes after its merge, so a
/// public commit it would refuse stops `release import` and run 1 first,
/// not the release halfway. Names each commit and its refused slots,
/// never an address.
pub fn address_guard(public: &Path, public_rev: &str, public_email: &str, api: &dyn Api, repo: &str) -> R {
    let c = other_addresses_on(&git_out(public, &["log", ADDRESS_LOG, public_rev])?, public_email, api, repo);
    if c.is_clean() {
        return Ok(());
    }
    let subjects: HashMap<String, String> = git_out(public, &["log", "--format=%H%x1f%s", public_rev])?
        .lines()
        .filter_map(|l| l.split_once('\x1f'))
        .map(|(h, s)| (h.to_string(), s.to_string()))
        .collect();
    let name =
        |sha: &str| named(&PublicCommit { sha: sha.to_string(), subject: subjects.get(sha).cloned().unwrap_or_default(), held: None });
    let mut parts: Vec<String> =
        c.commits.iter().map(|(sha, slots)| format!("public commit {} (its {} refused)", name(sha), slots.join(", "))).collect();
    parts.extend(c.unverified.iter().map(|(sha, e)| format!("public commit {} could not be verified (GitHub error: {e})", name(sha))));
    let head = if c.other.is_empty() {
        "the address check of the public history could not finish".to_string()
    } else {
        format!(
            "the public history carries {} the release's address check refuses after its merge, so nothing is opened or dispatched",
            crate::release_ci::refused(&c.other)
        )
    };
    Err(format!(
        "{head}: {}. A GitHub error passes on a run once GitHub answers; a refused address stays, since the public history cannot change: check it by hand, then add the commit to ACCEPTED_MERGES in xtask/src/release_ci.rs (wiki/Releasing.md, Outside pull requests)",
        parts.join("; ")
    ))
}

// ---------------------------------------------------------------- import

/// What an import works from.
pub struct Import<'a> {
    /// The development checkout.
    pub root: PathBuf,
    /// The public repository's remote: cloned, never pushed to.
    pub remote: String,
    /// The public repository's owner/name.
    pub repo: String,
    /// The public address (a release commit's).
    pub public_email: String,
    /// Where the work goes (replaced).
    pub stage: PathBuf,
    /// Make the commits, push nothing and open nothing.
    pub dry_run: bool,
    /// GitHub answers about the public repository.
    pub api: &'a dyn Api,
    /// Opens the development pull request: (branch, title, body file) to its page.
    pub open_pr: &'a dyn Fn(&str, &str, &Path) -> R<String>,
}

/// What an import did.
#[derive(Debug, Default)]
pub struct Imported {
    /// Each pull request opened (or, in a dry run, the commit it would carry).
    pub opened: Vec<String>,
    /// Each public change skipped, and why.
    pub skipped: Vec<String>,
    /// Each public change that could not be imported, and why.
    pub failed: Vec<String>,
}

/// The import (see the module docs).
pub fn import(im: &Import) -> R<Imported> {
    let root = &im.root;
    let dirty = git_out(root, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        return Err(format!(
            "uncommitted changes in {}: import works from a clean checkout ({})",
            root.display(),
            dirty.lines().next().unwrap_or_default().trim()
        ));
    }
    let origin = git_out(root, &["remote", "get-url", "origin"])?;
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').trim_end_matches(".git").to_ascii_lowercase();
    if crate::public_repo::github_slug(&origin).is_some_and(|s| s.eq_ignore_ascii_case(&im.repo)) || norm(&origin) == norm(&im.remote) {
        return Err(format!(
            "origin is the public repository {}: import runs in a checkout of the development repository and pushes only there",
            im.repo
        ));
    }
    git_out(root, &["fetch", "-q", "origin", "+refs/heads/main:refs/remotes/origin/main"])
        .map_err(|e| format!("git fetch of origin's main failed: {e}"))?;
    let base = git_out(root, &["rev-parse", "refs/remotes/origin/main"])?;
    rm_rf(&im.stage)?;
    mkdir(&im.stage)?;
    let public = im.stage.join("public");
    clone(&im.remote, &public)?;
    // A public commit the release's address check would refuse stops the
    // import before any pull request is opened.
    address_guard(&public, "HEAD", &im.public_email, im.api, &im.repo)?;
    let work = im.stage.join("work");
    let mut out = Imported::default();
    for c in public_changes(root, &base, &public, "HEAD", &im.public_email, &work)? {
        let what = named(&c);
        if let Some(h) = &c.held {
            out.skipped.push(format!("{what}: the development main already holds it ({h})"));
            continue;
        }
        let pr = match outside_pr(im.api, &im.repo, &c.sha) {
            Ok(Some(p)) => p,
            Ok(None) => {
                out.failed.push(format!(
                    "{what}: GitHub ties it to no merged outside pull request; bring it in by hand, with a {TRAILER}: {} trailer",
                    c.sha
                ));
                continue;
            }
            Err(e) => {
                out.failed.push(format!("{what}: cannot ask GitHub about it ({e})"));
                continue;
            }
        };
        let branch = branch_of(pr.number);
        let local = git_out(root, &["rev-parse", "-q", "--verify", &format!("refs/heads/{branch}")]).is_ok();
        let remote = !git_out(root, &["ls-remote", "--heads", "origin", &format!("refs/heads/{branch}")])?.is_empty();
        if local || remote {
            let where_ = if remote { "on origin" } else { "here" };
            out.skipped.push(format!(
                "{what}: branch {branch} exists already {where_}; merge its pull request, or delete the branch to import again"
            ));
            continue;
        }
        match import_one(im, &c, &pr, &base, &public, &work)? {
            Err(why) => out.failed.push(format!("{what}: {why}")),
            Ok((commit, title, body)) => {
                if im.dry_run {
                    out.opened.push(format!(
                        "public #{}: dry run, {branch} would be {} ({title}); nothing pushed",
                        pr.number,
                        short(&commit)
                    ));
                    continue;
                }
                git_out(root, &["branch", &branch, &commit])?;
                git_out(root, &["push", "-q", "origin", &format!("refs/heads/{branch}:refs/heads/{branch}")])
                    .map_err(|e| format!("pushing {branch} to origin failed: {e}"))?;
                let url = (im.open_pr)(&branch, &title, &body)?;
                out.opened.push(format!("public #{}: {url}", pr.number));
            }
        }
    }
    Ok(out)
}

/// Fragment `text` with `author: <login>` in its front matter (any author
/// line it had replaced); unchanged when it has no front matter.
fn with_author(text: &str, login: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return text.to_string();
    }
    let end = lines.iter().skip(1).position(|l| l.trim_end() == "---").map_or(lines.len(), |i| i + 1);
    let author = format!("author: {login}\n");
    let mut kept: Vec<&str> = vec![lines[0]];
    kept.extend(lines[1..end].iter().filter(|l| l.split_once(':').map(|(k, _)| k.trim()) != Some("author")));
    kept.push(&author);
    kept.extend(&lines[end..]);
    kept.concat()
}

/// One public change made a development commit on `base`: Ok((commit,
/// pull request title, body file)), or Err(why) when it cannot be.
fn import_one(
    im: &Import,
    c: &PublicCommit,
    pr: &OutsidePr,
    base: &str,
    public: &Path,
    work: &Path,
) -> R<Result<(String, String, PathBuf), String>> {
    let root = &im.root;
    let n = pr.number;
    let index = fresh_index(root, base, work, &format!("import-{n}"))?;
    let patch = change_of(public, &c.sha)?;
    if !patch.is_empty() {
        let pf = work.join(format!("import-{n}.patch"));
        write(&pf, &patch)?;
        let pf = pf.to_string_lossy().into_owned();
        if !git_bytes(root, &["apply", "--cached", &pf], Some(&index))?.0 {
            return Ok(Err("its change does not apply to the development main; bring it in by hand".into()));
        }
    }
    // Its changelog fragments, in their own namespace and credited.
    let mut notes = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for line in
        git_out(public, &["diff-tree", "--no-commit-id", "--name-status", "--diff-filter=A", "--root", "-r", &c.sha, "--", "changes"])?
            .lines()
    {
        let Some(name) = line.split('\t').nth(1).and_then(|p| p.strip_prefix("changes/")) else { continue };
        if name == "README.md" || name.contains('/') {
            continue;
        }
        let Some(stem) = name.strip_suffix(".md") else {
            notes.push(format!("`changes/{name}` is left out"));
            continue;
        };
        let slug = stem.split_once('-').filter(|(num, _)| !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit())).map(|(_, s)| s);
        let mut new = match slug {
            Some(s) => format!("public-{n}-{s}.md"),
            None => format!("public-{n}.md"),
        };
        let mut k = 2;
        while names.contains(&new) {
            new = format!("public-{n}-{k}.md");
            k += 1;
        }
        let (_, bytes) = git_bytes(public, &["show", &format!("{}:changes/{name}", c.sha)], None)?;
        let text = with_author(&String::from_utf8_lossy(&bytes), &pr.login);
        if let Err(e) = crate::version::parse_fragment(&new, &text) {
            notes.push(format!("`changes/{new}` does not parse yet ({e}): fix it on this branch"));
        }
        let tf = work.join(&new);
        write(&tf, &text)?;
        let tf = tf.to_string_lossy().into_owned();
        let blob = git_out(root, &["hash-object", "-w", "--no-filters", &tf])?;
        if !git_bytes(root, &["update-index", "--add", "--cacheinfo", &format!("100644,{blob},changes/{new}")], Some(&index))?.0 {
            return Err(format!("cannot add changes/{new}"));
        }
        notes.push(format!("`changes/{name}` is `changes/{new}` here, with `author: {}`", pr.login));
        names.push(new);
    }
    if names.is_empty() {
        notes.push("none: add one to this branch if users notice the change".into());
    }
    let (ok, tree) = git_bytes(root, &["write-tree"], Some(&index))?;
    let tree = String::from_utf8_lossy(&tree).trim().to_string();
    if !ok || tree.is_empty() {
        return Err("cannot write the import's tree".into());
    }
    // The author, address and date as the public commit has them; the
    // address is never printed.
    let (_, info) = git_bytes(public, &["log", "-1", "--format=%an%x00%ae%x00%aI%x00%B", &c.sha], None)?;
    let info = String::from_utf8_lossy(&info).into_owned();
    let mut f = info.splitn(4, '\0');
    let (an, ae, ad, msg) =
        (f.next().unwrap_or_default(), f.next().unwrap_or_default(), f.next().unwrap_or_default(), f.next().unwrap_or_default());
    let (subject, body) = msg.split_once('\n').unwrap_or((msg, ""));
    let plain = subject.strip_suffix(&format!(" (#{n})")).unwrap_or(subject);
    let repo = &im.repo;
    // Only the import writes the held marks: a contributor's own
    // `Public-commit:` or `Public-pr:` line would mark another public change
    // held.
    let body = strip_held_lines(body);
    let body = body.trim();
    let mut message = format!("{plain} (public #{n})\n\nImported from {repo}#{n}, public commit {}, by {}.\n", c.sha, pr.login);
    if !body.is_empty() {
        message.push_str(&format!("\n{body}\n"));
    }
    let mf = work.join(format!("import-{n}.msg"));
    write(&mf, &message)?;
    let mfs = mf.to_string_lossy().into_owned();
    git_out(
        root,
        &["interpret-trailers", "--in-place", "--trailer", &format!("{TRAILER}: {}", c.sha), "--trailer", &format!("Public-pr: {n}"), &mfs],
    )?;
    let mut cmd = git(root);
    cmd.args(["commit-tree", &tree, "-p", base, "-F", &mfs])
        .env("GIT_AUTHOR_NAME", an)
        .env("GIT_AUTHOR_EMAIL", ae)
        .env("GIT_AUTHOR_DATE", ad);
    let commit = output(&mut cmd).map_err(|_| "git commit-tree failed for the import".to_string())?.trim().to_string();
    let title = format!("Import public #{n}: {plain}");
    let bf = work.join(format!("import-{n}.body.md"));
    write(
        &bf,
        format!(
            "**Merge with \"Rebase and merge\", never squash.** A squash makes whoever merges the commit's author (the outside author only a co-author), and once this branch has a second commit it moves the `{TRAILER}:` line out of the commit's trailer block (wiki/Releasing.md, Outside pull requests).\n\nImports the public pull request {repo}#{n} (https://github.com/{repo}/pull/{n}), merged on the public main as commit {}, by {}.\n\nThe commit keeps its author; the change is applied to this repository's main as it merged, `changes/` aside. Its changelog fragments: {}.\n\nIts `{TRAILER}:` trailer marks the public change as held, so the export guard lets the next release through and the export keeps the change (wiki/Releasing.md, Outside pull requests).\n",
            c.sha,
            pr.login,
            notes.join("; ")
        ),
    )?;
    Ok(Ok((commit, title, bf)))
}

/// `cargo xtask release import` from the command line.
pub fn run(root: &Path, args: Vec<String>, repo: &str, api: &dyn Api) -> R {
    let (mut remote, mut dry_run) = (None, false);
    let mut it = args.into_iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--remote" => remote = it.next(),
            "--dry-run" => dry_run = true,
            _ => return Err(format!("unknown argument {a}\n{}", crate::release_ci::USAGE)),
        }
    }
    crate::release_ship::origin_check(root)?;
    let open = |branch: &str, title: &str, body: &Path| -> R<String> {
        crate::release_ci::no_gh_in_tests();
        let mut c = Command::new("gh");
        c.args([
            "pr",
            "create",
            "--repo",
            crate::release_ship::DEV_REPO,
            "--base",
            "main",
            "--head",
            branch,
            "--title",
            title,
            "--body-file",
        ])
        .arg(body);
        output(&mut c).map(|u| u.trim().to_string())
    };
    let r = import(&Import {
        root: root.to_path_buf(),
        remote: remote.unwrap_or_else(|| format!("https://github.com/{repo}.git")),
        repo: repo.to_string(),
        public_email: crate::release_ci::Ident::from_env().email,
        stage: crate::release_ci::stage_root(root).join("import"),
        dry_run,
        api,
        open_pr: &open,
    })?;
    for o in &r.opened {
        say(&format!("opened {o}"));
    }
    for s in &r.skipped {
        say(&format!("skipped {s}"));
    }
    if r.opened.is_empty() && r.skipped.is_empty() && r.failed.is_empty() {
        say("the public main holds no change the development main lacks");
    }
    if !r.failed.is_empty() {
        return Err(format!("not imported: {}", r.failed.join("; ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_ci::tests::{sh, tmp};

    const REPO: &str = "acme/tool";
    const PUB: &str = "pub@example.invalid";
    const OUT: &str = "out@example.invalid";

    /// GitHub answers by path.
    struct Map(HashMap<String, String>);
    impl Api for Map {
        fn get(&self, path: &str) -> R<String> {
            self.0.get(path).cloned().ok_or(format!("HTTP 404 for {path}"))
        }
    }

    /// GitHub's answers for `sha`, the squash commit of outside pull
    /// request `n` by `login`, committed by GitHub.
    fn answers(m: &mut HashMap<String, String>, sha: &str, n: u64, login: &str) {
        m.insert(
            format!("repos/{REPO}/commits/{sha}/pulls"),
            format!(
                r#"[{{"number": {n}, "state": "closed", "merged_at": "2026-11-16T10:00:00Z", "merge_commit_sha": "{sha}", "user": {{"login": "{login}", "type": "User"}}, "base": {{"ref": "main", "repo": {{"full_name": "{REPO}"}}}}}}]"#
            ),
        );
        m.insert(
            format!("repos/{REPO}/commits/{sha}"),
            format!(r#"{{"sha": "{sha}", "author": {{"login": "{login}", "type": "User"}}, "committer": {{"login": "web-flow", "type": "User"}}, "parents": [{{"sha": "{}"}}]}}"#, "0".repeat(40)),
        );
    }

    /// A commit in `repo` as `who` (name, address), committed by `committer`.
    fn commit_as(repo: &Path, files: &[(&str, &str)], msg: &str, who: (&str, &str), committer: (&str, &str)) -> String {
        for (f, t) in files {
            std::fs::create_dir_all(repo.join(f).parent().unwrap()).unwrap();
            std::fs::write(repo.join(f), t).unwrap();
        }
        sh(repo, &["add", "-A"]);
        let mut c = git(repo);
        c.args(["commit", "-q", "-m", msg])
            .env("GIT_AUTHOR_NAME", who.0)
            .env("GIT_AUTHOR_EMAIL", who.1)
            .env("GIT_COMMITTER_NAME", committer.0)
            .env("GIT_COMMITTER_EMAIL", committer.1);
        output(&mut c).unwrap();
        sh(repo, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
        sh(repo, &["rev-parse", "HEAD"])
    }

    const BASE: [(&str, &str); 3] = [("src/lib.rs", "fn a() {}\n"), ("changes/README.md", "# fragments\n"), ("README.md", "readme\n")];
    const FRAGMENT: &str = "---\nbump: patch\nsection: security\n---\n- **Podman's secrets file is denied.** atlas never indexes it.\n";

    /// A public repository with one release and a development repository
    /// holding the same tree.
    struct World {
        d: PathBuf,
        public: PathBuf,
        seed: PathBuf,
        dev: PathBuf,
    }

    fn world(name: &str) -> World {
        let d = tmp(&format!("contrib-{name}"));
        let public = d.join("pub.git");
        sh(&d, &["init", "-q", "--bare", "-b", "main", public.to_str().unwrap()]);
        let seed = d.join("seed");
        sh(&d, &["clone", "-q", public.to_str().unwrap(), seed.to_str().unwrap()]);
        commit_as(&seed, &BASE, "atlas v1.2.3", ("P", PUB), ("P", PUB));
        let origin = d.join("dev.git");
        sh(&d, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        let dev = d.join("dev");
        sh(&d, &["clone", "-q", origin.to_str().unwrap(), dev.to_str().unwrap()]);
        sh(&dev, &["config", "user.name", "D"]);
        sh(&dev, &["config", "user.email", "dev@example.invalid"]);
        commit_as(&dev, &BASE, "dev", ("D", "dev@example.invalid"), ("D", "dev@example.invalid"));
        World { d, public, seed, dev }
    }

    impl World {
        /// An outside pull request `n` squash-merged on the public main.
        fn outside(&self, n: u64, files: &[(&str, &str)], subject: &str) -> String {
            commit_as(&self.seed, files, &format!("{subject} (#{n})\n\nWhy it matters."), ("Out", OUT), ("GitHub", "noreply@github.com"))
        }
        fn guard(&self) -> R {
            let head = sh(&self.dev, &["rev-parse", "HEAD"]);
            let clone_dir = self.d.join("guard-public");
            clone(&self.public.to_string_lossy(), &clone_dir).unwrap();
            export_guard(&self.dev, &head, &clone_dir, "HEAD", PUB, &self.d.join("guard-work"))
        }
        fn import(&self, api: &dyn Api, dry_run: bool, opened: &std::cell::RefCell<Vec<(String, String, String)>>) -> R<Imported> {
            let open = |branch: &str, title: &str, body: &Path| -> R<String> {
                opened.borrow_mut().push((branch.into(), title.into(), std::fs::read_to_string(body).unwrap()));
                Ok(format!("https://example.invalid/dev/pull/{}", 300 + opened.borrow().len()))
            };
            import(&Import {
                root: self.dev.clone(),
                remote: self.public.to_string_lossy().replace('\\', "/"),
                repo: REPO.into(),
                public_email: PUB.into(),
                stage: self.d.join("stage"),
                dry_run,
                api,
                open_pr: &open,
            })
        }
        fn public_refs(&self) -> String {
            sh(&self.public, &["for-each-ref"])
        }
        fn done(self) {
            let _ = std::fs::remove_dir_all(&self.d);
        }
    }

    /// CONTRIBUTING.md (exported) tells an outside contributor the flow,
    /// and the Releasing page tells the owner.
    #[test]
    fn contributors_and_the_owner_are_told_the_flow() {
        let root = crate::release::repo_root();
        let c = std::fs::read_to_string(root.join("CONTRIBUTING.md")).unwrap_or_default();
        for want in [
            "welcome",
            "public CI",
            "merged on GitHub",
            "cargo xtask release import",
            "next release",
            "author",
            "changes/<number>.md",
            "Co-authored-by",
        ] {
            assert!(c.contains(want), "CONTRIBUTING.md does not say {want:?}");
        }
        assert!(crate::release::PUBLIC_FILES.contains(&"CONTRIBUTING.md"), "the export must carry CONTRIBUTING.md");
        let r = std::fs::read_to_string(root.join("wiki/Releasing.md")).unwrap();
        for want in [
            "cargo xtask release import",
            "The export guard",
            "Public-commit:",
            "web-flow",
            "changes/public-<n>.md",
            "squash dialog",
            "delete every `Co-authored-by:` line",
            "import and run 1 refuse",
            "Rebase and merge",
            "never from another account",
            "not stacked",
        ] {
            assert!(r.contains(want), "wiki/Releasing.md does not say {want:?}");
        }
    }

    #[test]
    fn release_subjects_are_the_releases_own() {
        for s in ["atlas v0.1.0", "atlas v0.6.1 (#12)", "atlas v0.5.0 binaries"] {
            assert!(release_subject(s), "{s}");
        }
        for s in [
            "atlas v0.6.1 (#)",
            "atlas v0.6 (#12)",
            "sources: deny x (#13)",
            "atlas v0.6.1 wiki",
            "atlas v0.6.1 (#12) extra",
            "Atlas v0.6.1",
        ] {
            assert!(!release_subject(s), "{s}");
        }
    }

    // ------------------------------------------------ the export guard

    #[test]
    fn the_export_refuses_while_the_public_main_holds_a_change_dev_lacks() {
        let w = world("guard");
        assert!(w.guard().is_ok(), "only releases on the public main");
        let sha = w.outside(13, &[("src/lib.rs", "fn a() {}\nfn deny() {}\n")], "sources: deny podman's secretsdata.json");
        let e = w.guard().unwrap_err();
        assert!(
            e.contains(short(&sha)) && e.contains("sources: deny podman's secretsdata.json") && e.contains("cargo xtask release import"),
            "{e}"
        );
        assert!(!e.contains(OUT), "no address in the refusal");
        // A release commit after it does not hide it.
        commit_as(&w.seed, &[("bin/atlas.exe", "MZ\n")], "atlas v1.2.4 binaries", ("P", PUB), ("P", PUB));
        assert!(w.guard().unwrap_err().contains(short(&sha)));
        // Held by hand: the change is in the development tree.
        commit_as(
            &w.dev,
            &[("src/lib.rs", "fn a() {}\nfn deny() {}\n")],
            "the same by hand",
            ("D", "dev@example.invalid"),
            ("D", "dev@example.invalid"),
        );
        assert!(w.guard().is_ok(), "the change applies in reverse: held");
        // Then changed again in dev: no longer applies in reverse, so only
        // a trailer holds it.
        commit_as(&w.dev, &[("src/lib.rs", "fn b() {}\n")], "rework", ("D", "dev@example.invalid"), ("D", "dev@example.invalid"));
        assert!(w.guard().is_err());
        commit_as(
            &w.dev,
            &[("x.txt", "x\n")],
            &format!("mark it\n\n{TRAILER}: {sha}"),
            ("D", "dev@example.invalid"),
            ("D", "dev@example.invalid"),
        );
        assert!(w.guard().is_ok(), "the trailer holds it");
        w.done();
    }

    #[test]
    fn a_release_shaped_subject_with_another_address_is_still_a_change() {
        let w = world("guard-shape");
        let sha =
            commit_as(&w.seed, &[("src/lib.rs", "fn evil() {}\n")], "atlas v9.9.9 (#14)", ("Out", OUT), ("GitHub", "noreply@github.com"));
        assert!(w.guard().unwrap_err().contains(short(&sha)));
        w.done();
    }

    // ------------------------------------------------ the address rule

    #[test]
    fn an_outside_pull_request_is_told_by_github_facts() {
        let sha = "a".repeat(40);
        let mut good = HashMap::new();
        answers(&mut good, &sha, 13, "outsider");
        assert_eq!(outside_pr(&Map(good.clone()), REPO, &sha).unwrap(), Some(OutsidePr { number: 13, login: "outsider".into() }));
        let pulls = format!("repos/{REPO}/commits/{sha}/pulls");
        let commit = format!("repos/{REPO}/commits/{sha}");
        let bad = |key: &str, from: &str, to: &str| {
            let mut m = good.clone();
            let v = m[key].replace(from, to);
            assert_ne!(v, m[key], "{from}");
            m.insert(key.into(), v);
            outside_pr(&Map(m), REPO, &sha).unwrap()
        };
        // The owner's own pull request, a bot's, an unmerged one, one into
        // another branch or repository, or whose merge commit is another.
        assert_eq!(bad(&pulls, r#""login": "outsider""#, r#""login": "acme""#), None, "the owner");
        assert_eq!(bad(&pulls, r#""login": "outsider""#, r#""login": "ACME""#), None, "the owner, case aside");
        assert_eq!(bad(&pulls, r#""type": "User""#, r#""type": "Bot""#), None, "a bot");
        assert_eq!(bad(&pulls, r#""2026-11-16T10:00:00Z""#, "null"), None, "not merged");
        assert_eq!(bad(&pulls, r#""ref": "main""#, r#""ref": "dev""#), None, "another branch");
        assert_eq!(bad(&pulls, &format!(r#""full_name": "{REPO}""#), r#""full_name": "fork/tool""#), None, "another repository");
        assert_eq!(
            bad(&pulls, &format!(r#""merge_commit_sha": "{sha}""#), &format!(r#""merge_commit_sha": "{}""#, "b".repeat(40))),
            None,
            "another merge commit"
        );
        // The commit's author is another account (an owner's address links
        // to the owner), or GitHub did not commit it.
        assert_eq!(bad(&commit, r#""author": {"login": "outsider""#, r#""author": {"login": "acme""#), None, "authored as the owner");
        assert_eq!(
            bad(&commit, r#""author": {"login": "outsider", "type": "User"}"#, r#""author": null"#),
            None,
            "an address no account holds"
        );
        assert_eq!(bad(&commit, r#""login": "web-flow""#, r#""login": "outsider""#), None, "not committed by GitHub");
        assert_eq!(
            bad(&commit, r#""parents": [{"sha": ""#, &format!(r#""parents": [{{"sha": "{}"}}, {{"sha": ""#, "1".repeat(40))),
            None,
            "two parents: a merge commit"
        );
        assert_eq!(bad(&pulls, r#"[{"number": 13, "#, "[{"), None, "no number");
        // No answer: not an outside pull request.
        assert!(outside_pr(&Map(HashMap::new()), REPO, &sha).is_err());
    }

    /// One commit in [`ADDRESS_LOG`] format.
    fn entry(sha: &str, author: &str, committer: &str, trailers: &[&str]) -> String {
        format!("{sha}\x1f{author}\x1f{committer}\x1f{}\x1e\n", trailers.join("\x1f"))
    }

    /// The refused addresses only.
    fn other(log: &str, outside: &dyn Fn(&str) -> R<bool>) -> Vec<String> {
        other_addresses_with(log, PUB, outside).other
    }

    #[test]
    fn an_outside_author_passes_only_as_that_commits_author() {
        let (o, r) = ("a".repeat(40), "b".repeat(40));
        let outside = |sha: &str| Ok(sha == o);
        let no = |_: &str| Ok(false);
        let owner_private = "owner@example.invalid";
        // The outside commit's author passes; GitHub's committer too.
        assert!(other_addresses_with(&entry(&o, OUT, "noreply@github.com", &[]), PUB, &outside).is_clean());
        // Without the GitHub facts, refused as before.
        assert_eq!(other(&entry(&o, OUT, "noreply@github.com", &[]), &no), [OUT]);
        // On an outside commit: another committer, or a co-author that is
        // not allowed (an owner address among them), still fails.
        assert_eq!(other(&entry(&o, OUT, OUT, &[]), &outside), [OUT], "its author as committer");
        assert_eq!(other(&entry(&o, OUT, owner_private, &[]), &outside), [owner_private]);
        assert_eq!(other(&entry(&o, OUT, "noreply@github.com", &[&format!("O <{owner_private}>")]), &outside), [owner_private]);
        assert_eq!(other(&entry(&o, OUT, "noreply@github.com", &[&format!("Out <{OUT}>")]), &outside), [OUT], "its author as co-author");
        // An unknown address on a commit GitHub does not tie to a merged
        // outside pull request.
        assert_eq!(other(&entry(&r, OUT, "noreply@github.com", &[]), &outside), [OUT]);
        // Asked only for a refused author.
        let asked = std::cell::Cell::new(0);
        let counting = |_: &str| {
            asked.set(asked.get() + 1);
            Ok(true)
        };
        assert!(other(&(entry(&r, PUB, PUB, &[]) + &entry(&o, OUT, "noreply@github.com", &[])), &counting).is_empty());
        assert_eq!(asked.get(), 1);
        // Through the GitHub answers.
        let mut m = HashMap::new();
        answers(&mut m, &o, 13, "outsider");
        assert!(other_addresses_on(&entry(&o, OUT, "noreply@github.com", &[]), PUB, &Map(m), REPO).is_clean());
    }

    /// Each refused commit is named with its refused slots, never an
    /// address.
    #[test]
    fn a_refused_commit_names_its_slots() {
        let (o, r) = ("a".repeat(40), "b".repeat(40));
        let outside = |sha: &str| Ok(sha == o);
        let log =
            entry(&o, OUT, "noreply@github.com", &["AI <ai@tool.example.invalid>", "X <x@example.invalid>"]) + &entry(&r, OUT, OUT, &[]);
        let c = other_addresses_with(&log, PUB, &outside);
        assert_eq!(c.commits, [(o.clone(), vec!["Co-authored-by"]), (r.clone(), vec!["author", "committer"])]);
        assert_eq!(c.other.len(), 3);
        let e = c.refuse("the public history carries", "").unwrap_err();
        assert!(e.starts_with("the public history carries 3 addresses that may not be there"), "{e}");
        assert!(!e.contains("example.invalid"), "{e}");
    }

    /// A GitHub error (a rate limit, a 5xx) is reported as unverified, not
    /// as a refused address, and still fails.
    #[test]
    fn a_github_error_is_unverified_not_refused() {
        let o = "a".repeat(40);
        let limited = |_: &str| -> R<bool> { Err("gh api: HTTP 403: API rate limit exceeded\nmore".into()) };
        let c = other_addresses_with(&entry(&o, OUT, "noreply@github.com", &[]), PUB, &limited);
        assert!(c.other.is_empty() && c.commits.is_empty(), "{c:?}");
        assert_eq!(c.unverified, [(o.clone(), "gh api: HTTP 403: API rate limit exceeded".to_string())]);
        let e = c.refuse("the public history carries", "").unwrap_err();
        assert!(e.contains("could not verify 1 commit (GitHub error for ") && e.contains("rate limit") && !e.contains(" 1 address"), "{e}");
        assert!(!e.contains(OUT));
        // Through GitHub: no answer is unverified.
        let c = other_addresses_on(&entry(&o, OUT, "noreply@github.com", &[]), PUB, &Map(HashMap::new()), REPO);
        assert_eq!((c.other.len(), c.unverified.len()), (0, 1), "no answer: unverified, and the check fails");
        assert!(!c.is_clean());
        // With a refused address as well: both are said.
        let both = entry(&o, OUT, OUT, &[]);
        let e = other_addresses_with(&both, PUB, &limited).refuse("x carries", "").unwrap_err();
        assert!(e.starts_with("x carries 1 address that may not be there; it also could not verify 1 commit"), "{e}");
    }

    // ------------------------------------------------ import

    #[test]
    fn import_opens_a_dev_pull_request_with_the_author_kept() {
        let w = world("import");
        let sha = w.outside(
            13,
            &[("src/lib.rs", "fn a() {}\nfn deny() {}\n"), ("changes/13.md", FRAGMENT)],
            "sources: deny podman's secretsdata.json",
        );
        let mut m = HashMap::new();
        answers(&mut m, &sha, 13, "outsider");
        let refs = w.public_refs();
        let opened = std::cell::RefCell::new(Vec::new());
        let r = w.import(&Map(m.clone()), false, &opened).unwrap();
        assert_eq!(w.public_refs(), refs, "nothing is pushed to the public repository");
        assert_eq!((r.opened.len(), r.skipped.len(), r.failed.len()), (1, 0, 0), "{r:?}");
        let (branch, title, body) = opened.borrow()[0].clone();
        assert_eq!(branch, "import/public-13");
        assert_eq!(title, "Import public #13: sources: deny podman's secretsdata.json");
        assert!(body.contains(&format!("{REPO}#13")) && body.contains(&sha) && body.contains("outsider") && !body.contains(OUT), "{body}");
        // The branch, on the development origin, on its main.
        let origin = w.d.join("dev.git");
        let tip = sh(&origin, &["rev-parse", "refs/heads/import/public-13"]);
        assert_eq!(sh(&origin, &["rev-parse", &format!("{tip}^")]), sh(&origin, &["rev-parse", "main"]));
        assert_eq!(sh(&origin, &["log", "-1", "--format=%an|%ae", &tip]), format!("Out|{OUT}"), "the author is kept");
        assert_eq!(sh(&origin, &["log", "-1", "--format=%aI", &tip]), sh(&w.public, &["log", "-1", "--format=%aI", &sha]));
        let msg = sh(&origin, &["log", "-1", "--format=%B", &tip]);
        assert!(
            msg.starts_with("sources: deny podman's secretsdata.json (public #13)\n")
                && msg.contains("Why it matters.")
                && msg.contains(&format!("{TRAILER}: {sha}")),
            "{msg}"
        );
        assert_eq!(sh(&origin, &["show", &format!("{tip}:src/lib.rs")]), "fn a() {}\nfn deny() {}");
        // The fragment, renamed into its own namespace, credits its author.
        let files = sh(&origin, &["ls-tree", "-r", "--name-only", &tip, "changes"]);
        assert_eq!(files, "changes/README.md\nchanges/public-13.md");
        let frag = sh(&origin, &["show", &format!("{tip}:changes/public-13.md")]);
        let f = crate::version::parse_fragment("public-13.md", &frag).unwrap();
        assert_eq!(f.author.as_deref(), Some("outsider"));
        // Nothing is opened twice: the branch is there.
        let again = w.import(&Map(m.clone()), false, &opened).unwrap();
        assert!(again.opened.is_empty() && again.skipped.iter().any(|s| s.contains("import/public-13")), "{again:?}");
        // Merged into the development main: held by its trailer, skipped
        // and named.
        sh(&w.dev, &["fetch", "-q", "origin"]);
        sh(&w.dev, &["merge", "-q", "--ff-only", "origin/import/public-13"]);
        sh(&w.dev, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
        sh(&origin, &["branch", "-D", "import/public-13"]);
        let held = w.import(&Map(m), false, &opened).unwrap();
        assert!(held.opened.is_empty() && held.skipped.iter().any(|s| s.contains(short(&sha)) && s.contains("holds")), "{held:?}");
        assert!(w.guard().is_ok(), "the export guard passes once imported");
        w.done();
    }

    #[test]
    fn import_skips_a_change_dev_holds_and_refuses_a_dirty_tree() {
        let w = world("import-held");
        let sha = w.outside(14, &[("README.md", "readme\nmore\n")], "docs: more");
        commit_as(
            &w.dev,
            &[("README.md", "readme\nmore\n")],
            "the same in dev",
            ("D", "dev@example.invalid"),
            ("D", "dev@example.invalid"),
        );
        let mut m = HashMap::new();
        answers(&mut m, &sha, 14, "outsider");
        let opened = std::cell::RefCell::new(Vec::new());
        let r = w.import(&Map(m.clone()), false, &opened).unwrap();
        assert!(r.opened.is_empty() && opened.borrow().is_empty(), "{r:?}");
        assert_eq!(r.skipped.len(), 1);
        assert!(
            r.skipped[0].contains(short(&sha)) && r.skipped[0].contains("public #14") && r.skipped[0].contains("already holds"),
            "{:?}",
            r.skipped
        );
        // A dirty tree: refused before anything.
        std::fs::write(w.dev.join("README.md"), "edited\n").unwrap();
        let e = w.import(&Map(m), false, &opened).unwrap_err();
        assert!(e.starts_with("uncommitted changes in "), "{e}");
        w.done();
    }

    #[test]
    fn import_never_pushes_to_the_public_repository() {
        let w = world("import-origin");
        let sha = w.outside(15, &[("src/lib.rs", "fn c() {}\n")], "fix: c");
        let mut m = HashMap::new();
        answers(&mut m, &sha, 15, "outsider");
        // A checkout whose origin is the public repository.
        sh(&w.dev, &["remote", "set-url", "origin", w.public.to_str().unwrap()]);
        let refs = w.public_refs();
        let e = w.import(&Map(m.clone()), false, &std::cell::RefCell::new(Vec::new())).unwrap_err();
        assert!(e.contains("origin is the public repository"), "{e}");
        assert_eq!(w.public_refs(), refs);
        // A dry run pushes nothing anywhere.
        sh(&w.dev, &["remote", "set-url", "origin", w.d.join("dev.git").to_str().unwrap()]);
        let opened = std::cell::RefCell::new(Vec::new());
        let r = w.import(&Map(m.clone()), true, &opened).unwrap();
        assert_eq!(r.opened.len(), 1);
        assert!(opened.borrow().is_empty() && sh(&w.d.join("dev.git"), &["branch", "--list", "import/*"]).is_empty());
        // A change GitHub ties to no merged outside pull request: its
        // author's address stops the import before anything is opened.
        let mut none = HashMap::new();
        none.insert(format!("repos/{REPO}/commits/{sha}/pulls"), "[]".to_string());
        let e = w.import(&Map(none.clone()), false, &opened).unwrap_err();
        assert!(e.contains(short(&sha)) && e.contains("its author refused") && !e.contains(OUT), "{e}");
        // Under the public address it passes that check, and is still not
        // imported.
        let own = commit_as(&w.seed, &[("README.md", "readme\nown\n")], "docs: own", ("P", PUB), ("P", PUB));
        m.insert(format!("repos/{REPO}/commits/{own}/pulls"), "[]".to_string());
        let r = w.import(&Map(m), false, &opened).unwrap();
        assert!(r.failed.iter().any(|f| f.contains(short(&own)) && f.contains("no merged outside pull request")), "{r:?}");
        w.done();
    }

    /// S1a: a public commit the release's address check refuses (here a
    /// foreign Co-authored-by line the squash kept) stops the import before
    /// any pull request is opened, naming the commit and the slot, never
    /// the address.
    #[test]
    fn import_refuses_a_public_commit_the_address_check_refuses() {
        let w = world("import-addr");
        let sha = commit_as(
            &w.seed,
            &[("src/lib.rs", "fn a() {}\nfn deny() {}\n")],
            "sources: deny x (#13)\n\nWhy.\n\nCo-authored-by: Tool <ai@tool.example.invalid>",
            ("Out", OUT),
            ("GitHub", "noreply@github.com"),
        );
        let mut m = HashMap::new();
        answers(&mut m, &sha, 13, "outsider");
        let opened = std::cell::RefCell::new(Vec::new());
        let e = w.import(&Map(m.clone()), false, &opened).unwrap_err();
        assert!(e.contains(&format!("public #13 {}", short(&sha))) && e.contains("its Co-authored-by refused"), "{e}");
        assert!(e.contains("1 address ") && !e.contains("example.invalid"), "no address printed: {e}");
        assert!(opened.borrow().is_empty() && sh(&w.d.join("dev.git"), &["branch", "--list", "import/*"]).is_empty());
        // A dry run refuses the same way.
        assert!(w.import(&Map(m), true, &opened).is_err());
        // GitHub not answering: unverified, not a refused address.
        let e = w.import(&Map(HashMap::new()), false, &opened).unwrap_err();
        assert!(e.contains("could not be verified (GitHub error"), "{e}");
        w.done();
    }

    /// S2: a contributor's own `Public-commit:` or `Public-pr:` lines are
    /// not copied, so no other public change is marked held.
    #[test]
    fn an_imported_commit_carries_only_the_imports_own_marks() {
        let w = world("import-forged");
        let other = w.outside(14, &[("docs/x.md", "x\n")], "docs: x");
        let sha = commit_as(
            &w.seed,
            &[("README.md", "readme\nmore\n")],
            &format!(
                "docs: more (#15)\n\nWhy.\n\nPublic-commit: {other}\npublic-pr: 14\nPUBLIC-COMMIT:\n  {other}\nSigned-off-by: Out <{OUT}>"
            ),
            ("Out", OUT),
            ("GitHub", "noreply@github.com"),
        );
        let mut m = HashMap::new();
        answers(&mut m, &other, 14, "outsider");
        answers(&mut m, &sha, 15, "outsider");
        let opened = std::cell::RefCell::new(Vec::new());
        let r = w.import(&Map(m), false, &opened).unwrap();
        assert_eq!(r.opened.len(), 2, "{r:?}");
        let origin = w.d.join("dev.git");
        let msg = sh(&origin, &["log", "-1", "--format=%B", "refs/heads/import/public-15"]);
        assert!(!msg.contains(&other) && !msg.to_ascii_lowercase().contains("public-pr: 14"), "{msg}");
        assert_eq!(sh(&origin, &["log", "-1", "--format=%(trailers:key=Public-commit,valueonly)", "refs/heads/import/public-15"]), sha);
        assert!(msg.contains("Why.") && msg.contains("Signed-off-by: Out"), "the rest of the body is kept: {msg}");
        // Only #15's import merged: #14 is still missing, and the guard says so.
        sh(&w.dev, &["fetch", "-q", "origin"]);
        sh(&w.dev, &["merge", "-q", "--ff-only", "origin/import/public-15"]);
        let e = w.guard().unwrap_err();
        assert!(e.contains(short(&other)) && !e.contains(short(&sha)), "{e}");
        w.done();
    }

    /// S3: the import pull request says to rebase, never squash; and a
    /// squash with GitHub's default message, whose `Public-commit:` line
    /// sits in an earlier commit's paragraph, still holds the change.
    #[test]
    fn an_import_squashed_with_a_second_commit_still_holds_its_change() {
        let w = world("import-squash");
        let sha = w.outside(13, &[("src/lib.rs", "fn a() {}\nfn deny() {}\n")], "sources: deny x");
        let mut m = HashMap::new();
        answers(&mut m, &sha, 13, "outsider");
        let opened = std::cell::RefCell::new(Vec::new());
        w.import(&Map(m), false, &opened).unwrap();
        let body = opened.borrow()[0].2.clone();
        assert!(body.starts_with("**Merge with \"Rebase and merge\", never squash.**") && body.contains("author"), "{body}");
        // A second commit on the branch, then a squash as GitHub writes it.
        sh(&w.dev, &["fetch", "-q", "origin"]);
        let first = sh(&w.dev, &["log", "-1", "--format=%B", "origin/import/public-13"]);
        sh(&w.dev, &["checkout", "-q", "-b", "fix", "origin/import/public-13"]);
        std::fs::write(w.dev.join("notes.txt"), "n\n").unwrap();
        sh(&w.dev, &["add", "notes.txt"]);
        sh(&w.dev, &["commit", "-q", "-m", "fix the fragment"]);
        sh(&w.dev, &["checkout", "-q", "main"]);
        sh(&w.dev, &["merge", "-q", "--squash", "fix"]);
        let msg = format!("Import public #13: sources: deny x (#301)\n\n* {first}\n\n* fix the fragment\n\nCo-authored-by: Out <{OUT}>\n");
        let mf = w.d.join("squash.msg");
        std::fs::write(&mf, msg).unwrap();
        sh(&w.dev, &["commit", "-q", "-F", mf.to_str().unwrap()]);
        assert_ne!(sh(&w.dev, &["log", "-1", "--format=%(trailers:key=Public-commit,valueonly)"]), sha, "not in the trailer block");
        // Dev edits the lines later: no longer applies in reverse.
        commit_as(&w.dev, &[("src/lib.rs", "fn b() {}\n")], "rework", ("D", "dev@example.invalid"), ("D", "dev@example.invalid"));
        assert!(w.guard().is_ok(), "the Public-commit: line in the squash body holds it");
        w.done();
    }

    /// N6: the import paths the first round left untested.
    #[test]
    fn an_import_that_does_not_apply_fails_and_a_fragment_is_renumbered() {
        let w = world("import-conflict");
        // Dev changed the same lines: the public change does not apply.
        commit_as(&w.dev, &[("src/lib.rs", "fn z() {}\n")], "rework", ("D", "dev@example.invalid"), ("D", "dev@example.invalid"));
        let sha = w.outside(13, &[("src/lib.rs", "fn a() {}\nfn deny() {}\n")], "sources: deny x");
        let mut m = HashMap::new();
        answers(&mut m, &sha, 13, "outsider");
        let opened = std::cell::RefCell::new(Vec::new());
        let r = w.import(&Map(m), false, &opened).unwrap();
        assert!(r.opened.is_empty() && r.failed.iter().any(|f| f.contains(short(&sha)) && f.contains("does not apply")), "{r:?}");
        // A fragment named for another number, with an author line of its
        // own: renamed for the pull request, its author replaced.
        let w2 = world("import-renumber");
        let spoof = FRAGMENT.replace("section: security\n", "section: security\nauthor: someone-else\n");
        let sha = w2.outside(13, &[("README.md", "readme\nx\n"), ("changes/99-podman.md", &spoof)], "docs: x");
        let mut m = HashMap::new();
        answers(&mut m, &sha, 13, "outsider");
        w2.import(&Map(m), false, &opened).unwrap();
        let origin = w2.d.join("dev.git");
        assert_eq!(
            sh(&origin, &["ls-tree", "-r", "--name-only", "import/public-13", "changes"]),
            "changes/README.md\nchanges/public-13-podman.md"
        );
        let frag = sh(&origin, &["show", "import/public-13:changes/public-13-podman.md"]);
        assert!(frag.contains("author: outsider") && !frag.contains("someone-else"), "{frag}");
        w.done();
        w2.done();
    }

    #[test]
    fn with_author_replaces_a_written_author() {
        assert_eq!(with_author("---\nbump: patch\nauthor: x\n---\n- a\n", "o"), "---\nbump: patch\nauthor: o\n---\n- a\n");
        assert_eq!(with_author("- no front matter\n", "o"), "- no front matter\n");
    }
}
