//! The CI release: the development repository opens a pull request, the
//! public repository builds, and the owner signs and publishes.
//!
//! ```text
//! cargo xtask release --pr --remote URL [--expect vX.Y.Z] [--dry-run]
//!     In the development repository (the release-pr workflow): the early
//!     checks of the release, the export, the leak check of the exported
//!     tree and the wiki pages, then branch release/vX.Y.Z of URL with the
//!     exported tree on top of its main (bin/ kept as main has it), pushed,
//!     and a pull request into main whose body is the release notes. No
//!     tests and no builds: the public CI tests the pull request.
//! cargo xtask release assets --dir DIR --notes FILE [--expect vX.Y.Z]
//!     In the public repository (the release workflow, after the merge):
//!     DIR holds the five binaries; writes DIR/manifest.json (the one
//!     manifest writer, `release::manifest_json`) and the release notes
//!     with each binary's SHA-256 to FILE.
//! cargo xtask release sign vX.Y.Z [--remote URL] [--dry-run]
//!     On the owner's machine: downloads the draft release's manifest and
//!     binaries, checks each SHA-256 and size against the manifest, the
//!     leak denylist against every file and every file for secret-shaped
//!     runs (`secret_scan.rs`, the shapes push protection refuses; in a dry
//!     run as well), commits the binaries to the public
//!     main as bin/ (the plugin install is a git clone), signs the manifest
//!     with ATLAS_SIGN_KEY, uploads manifest.json.sig, pushes main and the
//!     wiki, and publishes the draft on the binaries commit, which creates
//!     the tag as the owner. --dry-run stops before signing and pushing.
//! cargo xtask release check [--force]
//! cargo xtask release gate --dir DIR
//! cargo xtask release accept-hit FILE... [--print]
//!     The binary leak check before anything is public, its CI half, and
//!     the accepted-hits file's writer: see `release_check.rs`.
//! ```
//!
//! Pushing to a remote that is not a local folder needs ATLAS_PUBLISH=vX.Y.Z
//! in both `--pr` and `sign`, as in the `--push` release. Such a run from a
//! local checkout is also refused unless the checkout has no uncommitted
//! change, `git fetch` of origin's main succeeded, and HEAD is that main
//! (`--pr` outside CI, the `--push` release) or a commit on it (`sign`, in
//! a dry run as well); `sign` then refuses unless the checkout's HEAD holds
//! what the draft's commit holds (`checkout_guard`, `same_code`). A local bare
//! repository as `--remote` is the dry path: `--pr` pushes the branch there
//! and opens no pull request; `sign` needs ATLAS_GITHUB_STUB there and reads the draft
//! from the stub folder (`release-<tag>/view.json` and `release-<tag>/assets/`)
//! and writes what it would upload and publish beside it.
//!
//! The leak check of the release assets in `sign` accepts the places the
//! accepted-hits file lists (`accept.rs`), in the binaries only;
//! ATLAS_LEAK_ACCEPTED names the file, else `leak-accepted.txt` beside the
//! denylist is read when it is there.
//!
//! Environment: ATLAS_LEAK_DENYLIST (required), ATLAS_PUBLIC_REPO,
//! ATLAS_PUBLIC_NAME, ATLAS_PUBLIC_EMAIL, ATLAS_RELEASE_STAGE and
//! ATLAS_GITHUB_STUB as for the release (see `release.rs`); ATLAS_SIGN_KEY
//! for `sign`; GH_TOKEN for `gh` (in CI, the release app's installation
//! token).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::release::{
    bin_name, binaries_table, cargo_version, changelog_section, check_public_files, check_section, copy, drop_leading_blank, env, fill_notes, key_const, leak_check, ls, magic_ok, manifest_json, mkdir,
    model_rows, output, plugin_version, read_text, release_date, repo_root, rm_rf, say, sha256_file, sign_manifest, size_of, status, write, Notes, DEFAULT_PUBLIC_REPO, TARGETS,
};
use atlas::json::Value;

type R<T = ()> = Result<T, String>;

/// Usage of the CI modes.
pub const USAGE: &str = "usage: cargo xtask release --pr --remote URL [--expect vX.Y.Z] [--dry-run]
       cargo xtask release assets --dir DIR --notes FILE [--expect vX.Y.Z]
       cargo xtask release sign vX.Y.Z [--remote URL] [--dry-run]
       cargo xtask release ship vX.Y.Z
       cargo xtask release check [--force]
       cargo xtask release gate --dir DIR
       cargo xtask release accept-hit FILE... [--print]";

/// The binaries table row of the pull request's notes: the binaries are
/// built after the merge.
pub const PENDING_BINARIES: &str = "| Every platform | built by the release workflow after this pull request merges | listed in the draft release |\n";

/// Whether `release` arguments ask for a CI mode.
pub fn wants(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("assets" | "sign" | "ship" | "check" | "gate" | "accept-hit")) || args.iter().any(|a| a == "--pr")
}

/// Whether an address may appear in the public history: the public one,
/// and GitHub's own for commits it makes when a pull request is merged
/// (`noreply@github.com` as committer, an app's `[bot]` noreply address).
pub fn address_allowed(addr: &str, public_email: &str) -> bool {
    addr == public_email || addr == "noreply@github.com" || (addr.ends_with("[bot]@users.noreply.github.com") && !addr.contains(['<', '>', ' ']))
}

/// Merges of the public repo whose author address GitHub's web merge took
/// from the merger's account, not the public one; Brandon accepted them in
/// history as they are (2026-11-04), so the check passes over these commits
/// and nothing else.
const ACCEPTED_MERGES: [&str; 2] = [
    "ffd0eb1ee745af6bd89f7ddf0f93f66aef612786", // atlas v0.2.1 (#4)
    "006539d2ee2f170ab9749ddcfc1990266bc661c2", // atlas v0.2.1 (#5)
];

/// The addresses in `git log --format="%H %ae %ce"` output that may not
/// appear in the public history, sorted and once each.
pub fn other_addresses(log: &str, public_email: &str) -> Vec<String> {
    let mut other = Vec::new();
    for line in log.lines() {
        // A line without a sha is checked whole, so it is refused.
        let (sha, addrs) = line.split_once(' ').unwrap_or(("", line));
        if ACCEPTED_MERGES.contains(&sha) {
            continue;
        }
        for a in addrs.split(' ').filter(|a| !address_allowed(a, public_email)) {
            other.push(if a.is_empty() { "<empty>".to_string() } else { a.to_string() });
        }
    }
    other.sort();
    other.dedup();
    other
}

/// The version, from Cargo.toml and plugin.json, which must agree.
pub(crate) fn version_of(root: &Path) -> R<String> {
    let version = cargo_version(&read_text(&root.join("Cargo.toml"))?).ok_or("no version in Cargo.toml")?;
    let pv = plugin_version(&read_text(&root.join(".claude-plugin/plugin.json"))?).unwrap_or_default();
    if pv != version {
        return Err(format!("plugin.json version {pv} is not Cargo.toml's {version}"));
    }
    Ok(version)
}

fn expect_tag(expect: Option<&str>, tag: &str) -> R {
    match expect {
        Some(e) if e != tag => Err(format!("asked to release {e}, but this tree is {tag} (Cargo.toml)")),
        _ => Ok(()),
    }
}

/// The public commit identity.
#[derive(Clone)]
pub struct Ident {
    /// Author and committer name.
    pub name: String,
    /// Author and committer address.
    pub email: String,
}

impl Ident {
    pub(crate) fn from_env() -> Ident {
        Ident {
            name: env("ATLAS_PUBLIC_NAME").unwrap_or_else(|| "Brandon White".into()),
            email: env("ATLAS_PUBLIC_EMAIL").unwrap_or_else(|| "19593371+bloodfeast@users.noreply.github.com".into()),
        }
    }

    fn on(&self, c: &mut Command) {
        c.env("GIT_AUTHOR_NAME", &self.name).env("GIT_AUTHOR_EMAIL", &self.email).env("GIT_COMMITTER_NAME", &self.name).env("GIT_COMMITTER_EMAIL", &self.email);
    }
}

pub(crate) fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0");
    c
}

pub(crate) fn git_out(dir: &Path, args: &[&str]) -> R<String> {
    output(git(dir).args(args)).map(|s| s.trim_end().to_string())
}

/// Clones `remote` into `dir` (fresh); the remote is never printed with
/// credentials because none are put in it (gh supplies them in CI).
pub(crate) fn clone(remote: &str, dir: &Path) -> R {
    rm_rf(dir)?;
    let ok = Command::new("git").args(["clone", "-q", remote]).arg(dir).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).stderr(Stdio::null()).status();
    if !ok.map(|s| s.success()).unwrap_or(false) {
        return Err(format!("cannot clone {remote}"));
    }
    Ok(())
}

fn tag_on_remote(dir: &Path, tag: &str) -> R<bool> {
    Ok(!git_out(dir, &["ls-remote", "--tags", "origin", &format!("refs/tags/{tag}")])?.is_empty())
}

/// Stops a test that reaches a real GitHub call (gh): tests use stubs.
pub(crate) fn no_gh_in_tests() {
    if cfg!(test) {
        panic!("real gh in a test");
    }
}

/// The first 12 characters of a commit id, for messages.
pub(crate) fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Where a checkout's HEAD must stand against the development
/// repository's main for `checkout_guard`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnMain {
    /// HEAD is origin/main itself: the act that starts a release.
    Tip,
    /// HEAD is a commit on origin/main: `sign`, which runs after the
    /// release pull request, when main may have moved on; it then proves
    /// the code is the release's own (`same_code`).
    Reachable,
}

/// The stale-checkout guard: a public act runs from a checkout with no
/// uncommitted change to a tracked file, nothing untracked or ignored where
/// cargo builds the running xtask from (`BUILD_INPUTS`), no file hidden from
/// git status (assume-unchanged, skip-worktree), whose `git fetch` of
/// origin's main succeeded, and whose HEAD stands on that main as `on` says
/// (the reviewed code, not a branch or an old clone). Returns HEAD.
pub(crate) fn checkout_guard(root: &Path, on: OnMain) -> R<String> {
    let dirty = |s: String| Err(format!("uncommitted changes in {}: a release runs from committed code; commit, stash or remove them ({s})", root.display()));
    let tracked = git_out(root, &["status", "--porcelain", "--untracked-files=no"])?;
    if !tracked.is_empty() {
        return dirty(first_paths(&tracked));
    }
    let mut args = vec!["status", "--porcelain", "--untracked-files=all", "--ignored", "--"];
    args.extend(BUILD_INPUTS);
    let extra = git_out(root, &args)?;
    if !extra.is_empty() {
        return dirty(first_paths(&extra));
    }
    let hidden: String = git_out(root, &["ls-files", "-v"])?.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_lowercase() || c == 'S')).map(|l| format!("{l}\n")).collect();
    if !hidden.is_empty() {
        return dirty(format!("hidden from git status: {}", first_paths(&hidden)));
    }
    git_out(root, &["fetch", "-q", "origin", "+refs/heads/main:refs/remotes/origin/main"])
        .map_err(|_| format!("git fetch of origin's main failed in {}: cannot tell whether this checkout is the development repository's main", root.display()))?;
    let head = git_out(root, &["rev-parse", "HEAD"])?;
    let main = git_out(root, &["rev-parse", "refs/remotes/origin/main"])?;
    match on {
        OnMain::Tip if head != main => Err(format!("HEAD is {}, not origin/main ({}): check out the development repository's main, up to date", short(&head), short(&main))),
        OnMain::Reachable if head != main && !status(git(root).args(["merge-base", "--is-ancestor", &head, &main]))? => {
            Err(format!("HEAD {} is not on origin/main ({}): run from a commit of the development repository's main", short(&head), short(&main)))
        }
        _ => Ok(head),
    }
}

/// Where cargo reads the running xtask from, beyond tracked files.
const BUILD_INPUTS: [&str; 6] = ["xtask", "src", "Cargo.toml", "Cargo.lock", ".cargo", "build.rs"];

/// The first few paths of `git status --porcelain` or `ls-files -v` output.
fn first_paths(out: &str) -> String {
    let p: Vec<&str> = out.lines().map(|l| l.get(2..).unwrap_or(l).trim()).collect();
    let more = if p.len() > 5 { format!(" and {} more", p.len() - 5) } else { String::new() };
    format!("{}{more}", p.iter().take(5).copied().collect::<Vec<_>>().join(", "))
}

/// The checkout `sign` guards: `root` for every remote but a local folder.
/// A local folder is the dry path and needs the stub, so no unguarded run
/// signs, uploads to or publishes the real draft.
fn sign_checkout(root: &Path, remote: &str, stub: bool) -> R<Option<PathBuf>> {
    if !Path::new(remote).is_dir() {
        return Ok(Some(root.to_path_buf()));
    }
    if !stub {
        return Err(format!("--remote {remote} is a local folder, the dry path: it needs ATLAS_GITHUB_STUB, or the release on GitHub would be signed from an unguarded run"));
    }
    Ok(None)
}

/// Paths `.gitattributes` at HEAD marks export-ignore: exact paths only, as
/// it has (a pattern would need git's matcher, so it is refused).
fn export_ignored(checkout: &Path) -> R<Vec<String>> {
    let text = git_out(checkout, &["show", "HEAD:.gitattributes"]).unwrap_or_default();
    let mut out = Vec::new();
    for l in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let mut w = l.split_whitespace();
        let Some(path) = w.next() else { continue };
        if w.any(|a| a == "export-ignore") {
            if path.contains(['*', '?', '[', '\\']) || path.ends_with('/') {
                return Err(format!(".gitattributes marks the pattern {path} export-ignore: `release sign` compares exact paths only"));
            }
            out.push(path.trim_start_matches('/').to_string());
        }
    }
    Ok(out)
}

/// `git ls-tree -r` of `rev` as path -> mode, kind and object, without the
/// paths `skip` names.
fn tree_entries(dir: &Path, rev: &str, skip: impl Fn(&str) -> bool) -> R<std::collections::BTreeMap<String, String>> {
    Ok(git_out(dir, &["ls-tree", "-r", "--full-tree", rev])?
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, p)| !skip(p))
        .map(|(m, p)| (p.to_string(), m.to_string()))
        .collect())
}

/// The tree-match half of the guard: the checkout's HEAD holds exactly what
/// the draft's commit holds, export-ignored files and the public bin/ (the
/// last release's binaries) aside. So every file the release is built and
/// signed from (xtask/, src/, Cargo.lock) is the code that checks and signs
/// it: sign from the development commit the release was made from.
pub(crate) fn same_code(checkout: &Path, public: &Path, target: &str) -> R {
    let ignored = export_ignored(checkout)?;
    let here = tree_entries(checkout, "HEAD", |p| ignored.iter().any(|i| i == p))?;
    let there = tree_entries(public, target, |p| p.starts_with("bin/")).map_err(|_| format!("the draft's commit {} is not in the public clone", short(target)))?;
    let differ: Vec<&String> = here.keys().chain(there.keys().filter(|k| !here.contains_key(*k))).filter(|k| here.get(*k) != there.get(*k)).collect();
    if differ.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = differ.iter().take(5).map(|s| s.as_str()).collect();
    Err(format!(
        "this checkout is not the release's source (the draft's commit {}): {} path(s) differ ({}{}): check out the development commit the release was made from and run again",
        short(target),
        differ.len(),
        shown.join(", "),
        if differ.len() > 5 { ", ..." } else { "" }
    ))
}

/// Leak-checks a folder; refuses on a hit, naming places and term numbers only.
fn leak_gate(dir: &Path, denylist: &Path, what: &str) -> R {
    let (n, nfiles, leaks) = leak_check(dir, denylist)?;
    if n == 0 {
        return Err("the denylist holds no terms".into());
    }
    if !leaks.is_empty() {
        for l in &leaks {
            eprintln!("release: leak: {what}: {l}");
        }
        return Err(format!("{} leak(s) in {what} ({})", leaks.len(), dir.display()));
    }
    say(&format!("leak check of {what}: {n} terms, {nfiles} files, no hits"));
    Ok(())
}

/// Leak-checks the release binaries in a folder: as `leak_gate`, but a
/// place in an executable's contents that the accepted-hits file lists
/// (`accept.rs`) passes. Every other place refuses: in a text file, in a
/// path, or not listed. Reports places, term numbers and counts only.
pub(crate) fn binary_leak_gate(dir: &Path, denylist: &Path, accepted: Option<&Path>, what: &str) -> R {
    let mut files = Vec::new();
    crate::release::files_under(dir, Path::new(""), &mut files)?;
    files.sort_by_key(|s| crate::release::collate_key(s));
    let named: Vec<(String, Vec<u8>)> = files.into_iter().map(|f| std::fs::read(dir.join(&f)).map(|b| (f.clone(), b)).map_err(|e| format!("{f}: {e}"))).collect::<R<_>>()?;
    let list = std::fs::read(denylist).map_err(|_| "the denylist is not readable".to_string())?;
    let list = String::from_utf8_lossy(&list).into_owned();
    let db = crate::ucd::Db::load(&crate::pack_embedder::default_ucd())?;
    let m = crate::leak::Matcher::new(&db, &list);
    if m.is_empty() {
        return Err("the denylist holds no terms".into());
    }
    let acc = match accepted {
        Some(p) => crate::accept::Accepted::parse(&read_text(p)?, &m)?,
        None => crate::accept::Accepted::default(),
    };
    let j = crate::accept::judge(&crate::accept::places(&m, &named), &named, &acc);
    if !j.leaks.is_empty() {
        for l in &j.leaks {
            eprintln!("release: leak: {what}: {l}");
        }
        return Err(format!("{} leak(s) in {what} ({}); {} place(s) accepted", j.leaks.len(), dir.display(), j.accepted));
    }
    say(&format!(
        "leak check of {what}: {} terms, {} files, no hits but {} accepted place(s); {} accepted-hits entr(ies), {} unused",
        m.len(),
        named.len(),
        j.accepted,
        acc.0.len(),
        j.unused
    ));
    Ok(())
}

/// The accepted-hits file: ATLAS_LEAK_ACCEPTED when set (it must be
/// readable), else `leak-accepted.txt` beside the denylist when it is
/// there, else none.
pub(crate) fn accepted_file(set: Option<String>, denylist: &Path) -> R<Option<PathBuf>> {
    if let Some(p) = set {
        let p = PathBuf::from(p);
        if std::fs::File::open(&p).is_err() {
            return Err(format!("ATLAS_LEAK_ACCEPTED names {}, which is not readable", p.display()));
        }
        return Ok(Some(p));
    }
    let beside = denylist.with_file_name(crate::accept::DEFAULT_NAME);
    Ok(beside.is_file().then_some(beside))
}

/// Scans a folder's files for secret-shaped runs (`secret_scan.rs`), the
/// shapes push protection refuses; refuses on a hit, naming the file, the
/// byte offset and the rule, never the bytes. Runs before anything is
/// pushed, in a dry run as well.
pub(crate) fn secret_gate(dir: &Path, what: &str) -> R {
    let mut files = Vec::new();
    crate::release::files_under(dir, Path::new(""), &mut files)?;
    files.sort();
    let named: Vec<(String, Vec<u8>)> = files.into_iter().map(|f| std::fs::read(dir.join(&f)).map(|b| (f.clone(), b)).map_err(|e| format!("{f}: {e}"))).collect::<R<_>>()?;
    let hits = crate::secret_scan::scan_files(&named);
    if !hits.is_empty() {
        for h in &hits {
            eprintln!("release: secret scan: {what}: {h}");
        }
        return Err(format!(
            "{} secret-shaped run(s) in {what} ({}): GitHub push protection would refuse the push. Find the literal that forms each run (a key prefix the linker packed against other text, or a real key) and break it up, then rebuild",
            hits.len(),
            dir.display()
        ));
    }
    say(&format!("secret scan of {what}: {} files, no secret-shaped runs", named.len()));
    Ok(())
}

pub(crate) fn stage_root(root: &Path) -> PathBuf {
    match env("ATLAS_RELEASE_STAGE") {
        Some(s) => root.join(s),
        None => root.join(env("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into())).join("release-stage"),
    }
}

pub(crate) fn denylist_from_env() -> R<PathBuf> {
    let d = PathBuf::from(env("ATLAS_LEAK_DENYLIST").ok_or("ATLAS_LEAK_DENYLIST is not set")?);
    if std::fs::File::open(&d).is_err() {
        return Err("the denylist is not readable".into());
    }
    Ok(d)
}

/// Runs a CI mode.
pub fn run(args: Vec<String>) -> R {
    let root = repo_root();
    std::env::set_current_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let repo = env("ATLAS_PUBLIC_REPO").unwrap_or_else(|| DEFAULT_PUBLIC_REPO.into());
    let api = crate::public_repo::api();
    match args.first().map(String::as_str) {
        Some("check" | "gate" | "accept-hit") => crate::release_check::run(&root, args),
        Some("ship") => crate::release_ship::run(&root, args),
        Some("assets") => {
            let (mut dir, mut notes, mut expect) = (None, None, None);
            let mut it = args.into_iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--dir" => dir = it.next().map(PathBuf::from),
                    "--notes" => notes = it.next().map(PathBuf::from),
                    "--expect" => expect = it.next(),
                    _ => return Err(format!("unknown argument {a}\n{USAGE}")),
                }
            }
            let (Some(dir), Some(notes)) = (dir, notes) else { return Err(USAGE.into()) };
            assets(&root, &dir, &notes, expect.as_deref(), &repo)
        }
        Some("sign") => {
            let mut it = args.into_iter().skip(1);
            let tag = it.next().filter(|t| t.starts_with('v')).ok_or(USAGE)?;
            let (mut remote, mut dry_run) = (None, false);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--remote" => remote = it.next(),
                    "--dry-run" => dry_run = true,
                    _ => return Err(format!("unknown argument {a}\n{USAGE}")),
                }
            }
            let update_rs = read_text(&root.join("src/update.rs"))?;
            let (Some(trusted), Some(test_key)) = (key_const(&update_rs, "TRUSTED_KEY_HEX"), key_const(&update_rs, "TEST_KEY_HEX")) else {
                return Err("cannot read TRUSTED_KEY_HEX and TEST_KEY_HEX from src/update.rs".into());
            };
            let rel: Box<dyn Releases> = match env("ATLAS_GITHUB_STUB") {
                Some(d) => Box::new(StubReleases(PathBuf::from(d))),
                None => Box::new(GhReleases { repo: repo.clone() }),
            };
            let remote = remote.unwrap_or_else(|| format!("https://github.com/{repo}.git"));
            let denylist = denylist_from_env()?;
            let s = Sign {
                checkout: sign_checkout(&root, &remote, env("ATLAS_GITHUB_STUB").is_some())?,
                remote,
                stage: stage_root(&root).join(&tag).join("sign"),
                confirm: &crate::release_check::Tty,
                tag,
                repo,
                dry_run,
                accepted: accepted_file(env("ATLAS_LEAK_ACCEPTED"), &denylist)?,
                denylist,
                key: env("ATLAS_SIGN_KEY").map(PathBuf::from),
                trusted,
                test_key,
                ident: Ident::from_env(),
                api: api.as_ref(),
                rel: rel.as_ref(),
            };
            sign(&s).map(|r| say(&format!("{} {}", if r.published { "released on" } else { "dry run; the binaries commit would be" }, r.commit)))
        }
        _ => {
            let (mut remote, mut expect, mut dry_run) = (None, None, false);
            let mut it = args.into_iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--pr" => {}
                    "--remote" => remote = it.next(),
                    "--expect" => expect = it.next(),
                    "--dry-run" => dry_run = true,
                    _ => return Err(format!("unknown argument {a} (--pr takes --remote, --expect and --dry-run)\n{USAGE}")),
                }
            }
            let remote = remote.ok_or("--remote URL is required (a local bare repository for a dry run)")?;
            let version = version_of(&root)?;
            let tag = format!("v{version}");
            // A push to GitHub from a local checkout is guarded; the
            // release-pr job's checkout is fresh, is the dispatched commit,
            // and keeps no credentials to fetch with.
            let in_ci = env("GITHUB_ACTIONS").as_deref() == Some("true") && env("GITHUB_REPOSITORY").as_deref() == Some("bloodfeast/atlas-dev");
            let guard_checkout = !dry_run && !Path::new(&remote).is_dir() && !in_ci;
            let p = Pr {
                guard_checkout,
                stage: stage_root(&root).join(&tag).join("pr"),
                // ATLAS_PUBLISH counts in the release-pr job only; a push to
                // GitHub from anywhere else is typed at a terminal.
                publish_ok: in_ci && env("ATLAS_PUBLISH").as_deref() == Some(tag.as_str()),
                in_ci,
                confirm: &crate::release_check::Tty,
                root: root.clone(),
                remote,
                expect,
                dry_run,
                require_clean: true,
                denylist: denylist_from_env()?,
                repo,
                ident: Ident::from_env(),
                api: api.as_ref(),
                open_pr: true,
            };
            pr(&p).map(|m| say(&format!("{} at {} ({} bytes of release notes)", m.branch, m.commit, m.notes.len())))
        }
    }
}

// ---------------------------------------------------------------- --pr

/// What `--pr` works from.
pub struct Pr<'a> {
    /// The development checkout.
    pub root: PathBuf,
    /// The public repository's remote (a local bare repository for a dry path).
    pub remote: String,
    /// The tag the caller expects (the workflow's input or pushed tag).
    pub expect: Option<String>,
    /// Stop before pushing.
    pub dry_run: bool,
    /// Refuse uncommitted changes (the export is HEAD).
    pub require_clean: bool,
    /// Run `checkout_guard` (HEAD is origin/main): a push from a local
    /// checkout.
    pub guard_checkout: bool,
    /// The leak denylist.
    pub denylist: PathBuf,
    /// The public repository's owner/name.
    pub repo: String,
    /// The public identity.
    pub ident: Ident,
    /// Where the work goes (replaced).
    pub stage: PathBuf,
    /// ATLAS_PUBLISH names this release, in the release-pr job.
    pub publish_ok: bool,
    /// Run by the release-pr job (GITHUB_ACTIONS in the development
    /// repository): ATLAS_PUBLISH stands for the typed prompt.
    pub in_ci: bool,
    /// The typed prompt of a push to GitHub outside the release-pr job.
    pub confirm: &'a dyn crate::release_check::Confirm,
    /// GitHub answers for the guard.
    pub api: &'a dyn crate::public_repo::Api,
    /// Open or update the pull request with gh (GitHub remotes only).
    pub open_pr: bool,
}

/// What `--pr` made.
#[derive(Debug)]
pub struct PrMade {
    /// The branch, `release/vX.Y.Z`.
    pub branch: String,
    /// Its commit.
    pub commit: String,
    /// The pull request body.
    pub notes: String,
}

/// The `--pr` mode (see the module docs).
pub fn pr(p: &Pr) -> R<PrMade> {
    let root = &p.root;
    let version = version_of(root)?;
    let tag = format!("v{version}");
    expect_tag(p.expect.as_deref(), &tag)?;
    if p.guard_checkout {
        checkout_guard(root, OnMain::Tip)?;
    }
    if p.require_clean && !git_out(root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        return Err("uncommitted changes; the release is made from HEAD".into());
    }
    let changes = changelog_section(&read_text(&root.join("CHANGELOG.md"))?, &tag);
    check_section(&changes, &tag)?;
    let named = crate::public_repo::link_mismatches(root, &p.repo)?;
    if !named.is_empty() {
        return Err(format!("ATLAS_PUBLIC_REPO is {}, but {}: change the links or ATLAS_PUBLIC_REPO", p.repo, named.join("; ")));
    }
    let to_github = !Path::new(&p.remote).is_dir();
    let dev = git_out(root, &["remote", "get-url", "origin"]).ok();
    match crate::public_repo::check_unprivileged(p.api, &p.repo, dev.as_deref(), &[&p.remote]) {
        Ok(r) if r.unseen.is_empty() => say(&format!("public repo check: {} passes ({} rulesets)", p.repo, r.rulesets)),
        Ok(r) => say(&format!("public repo check: {} passes; not visible to this token: {} (the owner's `release sign` checks them)", p.repo, r.unseen.join(", "))),
        Err(e) if to_github && !p.dry_run => return Err(format!("public repo check: {e}")),
        Err(e) => say(&format!("public repo check: {e} (a push to GitHub would stop here)")),
    }

    say("export");
    rm_rf(&p.stage)?;
    mkdir(&p.stage)?;
    let src_tar = p.stage.join("src.tar");
    if !status(git(root).args(["archive", "--format=tar", "-o"]).arg(&src_tar).arg("HEAD"))? {
        return Err("git archive failed".into());
    }
    let archive = std::fs::read(&src_tar).map_err(|e| format!("{}: {e}", src_tar.display()))?;
    let tree = p.stage.join("tree");
    crate::tar::extract(&archive, &tree)?;
    check_public_files(&tree)?;
    if tree.join("bin").exists() {
        return Err("the export carries bin/: bin/ in the public repository is the release's binaries, committed by `release sign`".into());
    }
    leak_gate(&tree, &p.denylist, "the exported tree")?;
    let pages = p.stage.join("wiki-pages");
    crate::wiki::stage_pages(&tree.join("wiki"), &read_text(&tree.join("CHANGELOG.md"))?, &pages)?;
    leak_gate(&pages, &p.denylist, "the wiki pages")?;

    let models: String = model_rows(&read_text(&tree.join("config/atlas.template.json"))?).iter().map(|m| m.row()).collect();
    let date = release_date();
    let notes = fill_notes(
        &read_text(&tree.join("scripts/release-notes.md.in"))?,
        &Notes { tag: &tag, date: &date, repo: &p.repo, changes: &changes, binaries: PENDING_BINARIES, missing: "", models: &models },
    );
    let notes_path = p.stage.join("notes.md");
    write(&notes_path, &notes)?;

    say(&format!("branch release/{tag}"));
    let public = p.stage.join("public");
    clone(&p.remote, &public)?;
    if tag_on_remote(&public, &tag)? {
        return Err(format!("{tag} already exists in {}", p.remote));
    }
    let branch = format!("release/{tag}");
    let has_main = git_out(&public, &["rev-parse", "-q", "--verify", "refs/remotes/origin/main"]).is_ok();
    if has_main {
        git_out(&public, &["checkout", "-q", "-B", &branch, "origin/main"])?;
    } else {
        git_out(&public, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])?;
    }
    // The exported tree over main, bin/ excepted: main keeps the binaries
    // of the release before until `release sign` commits this one's.
    let tree_arg = format!("--work-tree={}", tree.display());
    output(git(&public).arg(&tree_arg).args(["add", "-A", "--", ":(top)", ":(top,exclude)bin"]))?;
    if has_main && git_out(&public, &["diff", "--cached", "--quiet", "HEAD"]).is_ok() {
        return Err(format!("the export is what main of {} already holds: nothing to release", p.remote));
    }
    let body = notes.split_once('\n').map_or("", |(_, b)| b);
    let message = p.stage.join("message.txt");
    write(&message, format!("atlas {tag}\n\n{}", drop_leading_blank(body)))?;
    let mut c = git(&public);
    c.args(["commit", "-q", "--cleanup=verbatim", "-F"]).arg(&message);
    p.ident.on(&mut c);
    output(&mut c)?;
    let addrs = git_out(&public, &["log", "-1", "--format=%ae%n%ce"])?;
    if addrs.lines().any(|a| a != p.ident.email) {
        return Err("the release commit carries another address".into());
    }
    if has_main && !git_out(&public, &["diff", "--name-only", "origin/main", "HEAD", "--", "bin"])?.is_empty() {
        return Err("the release branch changes bin/".into());
    }
    let commit = git_out(&public, &["rev-parse", "HEAD"])?;
    let nfiles = git_out(&public, &["ls-files"])?.lines().count();
    say(&format!("commit {} on {branch}: {nfiles} files", &commit[..12]));

    let made = PrMade { branch: branch.clone(), commit, notes: notes.clone() };
    if p.dry_run {
        say(&format!("dry run: not pushed. It would push {branch} to {} and open a pull request into main with {} as its body.", p.remote, notes_path.display()));
        return Ok(made);
    }
    if to_github {
        push_gate(p, &tag, &branch)?;
    }
    // A rerun replaces the branch: release/* belongs to the release.
    if !status(git(&public).args(["push", "-q", "--force", "origin", &format!("HEAD:refs/heads/{branch}")]))? {
        return Err(format!("pushing {branch} failed"));
    }
    say(&format!("pushed {branch} to {}", p.remote));
    if !to_github || !p.open_pr {
        say("a local remote: no pull request opened");
        return Ok(made);
    }
    let title = format!("atlas {tag}");
    let open = output(Command::new("gh").args(["pr", "list", "--repo", &p.repo, "--head", &branch, "--state", "open", "--json", "number", "--jq", ".[0].number"]))?;
    let open = open.trim();
    let mut c = Command::new("gh");
    if open.is_empty() {
        c.args(["pr", "create", "--repo", &p.repo, "--base", "main", "--head", &branch, "--title", &title, "--body-file"]).arg(&notes_path);
    } else {
        c.args(["pr", "edit", open, "--repo", &p.repo, "--title", &title, "--body-file"]).arg(&notes_path);
    }
    let url = output(&mut c)?;
    say(&format!("pull request: {}", if open.is_empty() { url.trim() } else { open }));
    Ok(made)
}

/// What a push of the release branch to GitHub needs: ATLAS_PUBLISH in
/// the release-pr job, the typed `open vX.Y.Z` anywhere else.
fn push_gate(p: &Pr, tag: &str, branch: &str) -> R {
    if p.in_ci {
        if !p.publish_ok {
            return Err(format!("pushing to {} needs ATLAS_PUBLISH={tag}", p.remote));
        }
        return Ok(());
    }
    p.confirm.confirm(&format!("push {branch} to {} and open a pull request into its main: the export becomes PUBLIC", p.remote), &format!("open {tag}"))
}

// ---------------------------------------------------------------- assets

/// `assets` (see the module docs): every target's binary present and what
/// its name says, nothing else, then the manifest and the notes.
pub fn assets(root: &Path, dir: &Path, notes_out: &Path, expect: Option<&str>, repo: &str) -> R {
    let version = version_of(root)?;
    let tag = format!("v{version}");
    expect_tag(expect, &tag)?;
    let want: Vec<String> = TARGETS.iter().map(|t| bin_name(t)).collect();
    let have = ls(dir)?;
    let missing: Vec<&String> = want.iter().filter(|w| !have.contains(w)).collect();
    let extra: Vec<&String> = have.iter().filter(|h| !want.contains(h) && h.as_str() != "manifest.json").collect();
    if !missing.is_empty() || !extra.is_empty() {
        return Err(format!(
            "{}: missing [{}], unexpected [{}]",
            dir.display(),
            missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
            extra.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    for f in &want {
        let b = std::fs::read(dir.join(f)).map_err(|e| format!("{f}: {e}"))?;
        magic_ok(f, &b).map_err(|m| format!("{f} is not the file its name says (starts {m})"))?;
    }
    let _ = std::fs::remove_file(dir.join("manifest.json"));
    let manifest = manifest_json(&version, repo, dir)?;
    let m = atlas::update::parse_manifest(manifest.as_bytes())?;
    if m.files.len() != TARGETS.len() {
        return Err(format!("the manifest lists {} files, not {}", m.files.len(), TARGETS.len()));
    }
    write(&dir.join("manifest.json"), &manifest)?;
    let changes = changelog_section(&read_text(&root.join("CHANGELOG.md"))?, &tag);
    check_section(&changes, &tag)?;
    let models: String = model_rows(&read_text(&root.join("config/atlas.template.json"))?).iter().map(|m| m.row()).collect();
    let date = release_date();
    let notes = fill_notes(
        &read_text(&root.join("scripts/release-notes.md.in"))?,
        &Notes { tag: &tag, date: &date, repo, changes: &changes, binaries: &binaries_table(dir)?, missing: "", models: &models },
    );
    write(notes_out, &notes)?;
    say(&format!("{tag}: manifest.json for {} binaries, notes in {}", m.files.len(), notes_out.display()));
    Ok(())
}

// ---------------------------------------------------------------- sign

/// A release on GitHub as `sign` sees it.
#[derive(Debug, Clone)]
pub struct Draft {
    /// Still a draft.
    pub draft: bool,
    /// The commit it targets.
    pub target: String,
    /// Its assets: name and size.
    pub assets: Vec<(String, u64)>,
}

/// The release calls `sign` makes.
pub trait Releases {
    /// The release of `tag`.
    fn view(&self, tag: &str) -> R<Draft>;
    /// Every asset of it, into `dir`.
    fn download(&self, tag: &str, dir: &Path) -> R;
    /// Uploads `file` as an asset, replacing one of the same name.
    fn upload(&self, tag: &str, file: &Path) -> R;
    /// Publishes the draft on `target` (this creates the tag).
    fn publish(&self, tag: &str, target: &str) -> R;
}

/// `gh release`, as the user gh is logged in as.
pub struct GhReleases {
    /// owner/name.
    pub repo: String,
}

/// Reads `gh release view --json isDraft,targetCommitish,assets` output.
pub fn parse_view(body: &str) -> R<Draft> {
    let v = atlas::json::parse(body).map_err(|e| format!("gh release view: not JSON ({e})"))?;
    let assets = v
        .get("assets")
        .and_then(Value::as_array)
        .ok_or("gh release view: no assets")?
        .iter()
        .map(|a| Some((a.get("name")?.as_str()?.to_string(), a.get("size")?.as_u64()?)))
        .collect::<Option<Vec<_>>>()
        .ok_or("gh release view: an asset without a name or size")?;
    Ok(Draft {
        draft: v.get("isDraft").and_then(Value::as_bool).ok_or("gh release view: no isDraft")?,
        target: v.get("targetCommitish").and_then(Value::as_str).unwrap_or_default().to_string(),
        assets,
    })
}

impl Releases for GhReleases {
    fn view(&self, tag: &str) -> R<Draft> {
        crate::release_ci::no_gh_in_tests();
        parse_view(&output(Command::new("gh").args(["release", "view", tag, "--repo", &self.repo, "--json", "isDraft,targetCommitish,assets"]))?)
    }
    fn download(&self, tag: &str, dir: &Path) -> R {
        crate::release_ci::no_gh_in_tests();
        output(Command::new("gh").args(["release", "download", tag, "--repo", &self.repo, "--clobber", "--dir"]).arg(dir)).map(|_| ())
    }
    fn upload(&self, tag: &str, file: &Path) -> R {
        crate::release_ci::no_gh_in_tests();
        output(Command::new("gh").args(["release", "upload", tag, "--repo", &self.repo, "--clobber"]).arg(file)).map(|_| ())
    }
    fn publish(&self, tag: &str, target: &str) -> R {
        crate::release_ci::no_gh_in_tests();
        output(Command::new("gh").args(["release", "edit", tag, "--repo", &self.repo, "--draft=false", "--latest", "--target", target])).map(|_| ())
    }
}

/// A folder standing in for GitHub (see the module docs).
pub struct StubReleases(pub PathBuf);

impl Releases for StubReleases {
    fn view(&self, tag: &str) -> R<Draft> {
        let f = self.0.join(format!("release-{tag}")).join("view.json");
        let mut d = parse_view(&read_text(&f).map_err(|_| format!("no release {tag} (stub {})", f.display()))?)?;
        if self.0.join(format!("release-{tag}")).join("published.txt").is_file() {
            d.draft = false;
        }
        Ok(d)
    }
    fn download(&self, tag: &str, dir: &Path) -> R {
        let src = self.0.join(format!("release-{tag}")).join("assets");
        for f in ls(&src)? {
            copy(&src.join(&f), &dir.join(&f))?;
        }
        Ok(())
    }
    fn upload(&self, tag: &str, file: &Path) -> R {
        let d = self.0.join(format!("release-{tag}")).join("uploaded");
        mkdir(&d)?;
        copy(file, &d.join(file.file_name().ok_or("no file name")?))
    }
    fn publish(&self, tag: &str, target: &str) -> R {
        write(&self.0.join(format!("release-{tag}")).join("published.txt"), target)
    }
}

/// What `sign` works from.
pub struct Sign<'a> {
    /// vX.Y.Z.
    pub tag: String,
    /// The development checkout this runs from, for `checkout_guard` and
    /// `same_code` (every remote but a local folder: the dry path).
    pub checkout: Option<PathBuf>,
    /// owner/name of the public repository.
    pub repo: String,
    /// Its remote (a local bare repository for the dry path).
    pub remote: String,
    /// Stop before signing and pushing.
    pub dry_run: bool,
    /// The leak denylist.
    pub denylist: PathBuf,
    /// The accepted-hits file (`accept.rs`), for the binaries only.
    pub accepted: Option<PathBuf>,
    /// ATLAS_SIGN_KEY.
    pub key: Option<PathBuf>,
    /// TRUSTED_KEY_HEX and TEST_KEY_HEX of src/update.rs.
    pub trusted: String,
    /// See `trusted`.
    pub test_key: String,
    /// The typed prompt `publish vX.Y.Z`, after every check and the key's
    /// preflight; it covers the signature, the upload, both pushes and the
    /// publish.
    pub confirm: &'a dyn crate::release_check::Confirm,
    /// The public identity.
    pub ident: Ident,
    /// Where the work goes (replaced).
    pub stage: PathBuf,
    /// GitHub answers for the guard.
    pub api: &'a dyn crate::public_repo::Api,
    /// The release calls.
    pub rel: &'a dyn Releases,
}

/// Checks the downloaded assets against the manifest among them: the
/// version, one entry per target with its file name and download URL, and
/// each file's presence, size, SHA-256 and kind; no file the manifest does
/// not list. Every mismatch is named.
pub fn verify_assets(dir: &Path, tag: &str, repo: &str) -> R<atlas::update::Manifest> {
    let bytes = std::fs::read(dir.join("manifest.json")).map_err(|_| "the draft has no manifest.json".to_string())?;
    let m = atlas::update::parse_manifest(&bytes)?;
    let mut errs = Vec::new();
    if format!("v{}", m.version) != tag {
        errs.push(format!("the manifest is for version {}, not {tag}", m.version));
    }
    for t in TARGETS {
        match m.files.iter().filter(|f| f.target == t).count() {
            1 => {}
            0 => errs.push(format!("the manifest lists no {t}")),
            n => errs.push(format!("the manifest lists {t} {n} times")),
        }
    }
    for f in &m.files {
        if !TARGETS.contains(&f.target.as_str()) {
            errs.push(format!("the manifest lists an unknown target {}", f.target));
            continue;
        }
        if f.file != bin_name(&f.target) {
            errs.push(format!("the manifest names {} for {}, not {}", f.file, f.target, bin_name(&f.target)));
        }
        let url = format!("https://github.com/{repo}/releases/download/{tag}/{}", f.file);
        if f.url != url {
            errs.push(format!("{}: the manifest's URL is {}, not {url}", f.file, f.url));
        }
        let p = dir.join(&f.file);
        if !p.is_file() {
            errs.push(format!("{}: listed in the manifest but missing from the draft", f.file));
            continue;
        }
        let size = size_of(&p)?;
        if size != f.size {
            errs.push(format!("{}: {size} bytes, the manifest says {}", f.file, f.size));
            continue;
        }
        let sha = sha256_file(&p)?;
        if sha != f.sha256 {
            errs.push(format!("{}: sha256 {sha}, the manifest says {}", f.file, f.sha256));
        }
        let b = std::fs::read(&p).map_err(|e| format!("{}: {e}", f.file))?;
        if let Err(magic) = magic_ok(&f.file, &b) {
            errs.push(format!("{} is not the file its name says (starts {magic})", f.file));
        }
    }
    for f in ls(dir)? {
        if f != "manifest.json" && f != "manifest.json.sig" && !m.files.iter().any(|x| x.file == f) {
            errs.push(format!("{f}: in the draft but not in the manifest"));
        }
    }
    if errs.is_empty() {
        Ok(m)
    } else {
        Err(format!("the draft {tag} does not match its manifest: {}", errs.join("; ")))
    }
}

/// What `sign` did.
#[derive(Debug)]
pub struct Signed {
    /// The commit the release is published on (the binaries commit).
    pub commit: String,
    /// Whether it was published (false for a dry run).
    pub published: bool,
}

/// The `sign` mode (see the module docs).
pub fn sign(s: &Sign) -> R<Signed> {
    let tag = &s.tag;
    atlas::update::parse_version(tag.trim_start_matches('v')).ok_or(format!("{tag} is not vMAJOR.MINOR.PATCH"))?;
    // In a dry run as well: a dry run says what the publish would meet.
    if let Some(c) = &s.checkout {
        checkout_guard(c, OnMain::Reachable)?;
    }
    if !s.dry_run && s.trusted == s.test_key {
        return Err("the key built into atlas is the test key: put the release key in TRUSTED_KEY_HEX".into());
    }
    let to_github = !Path::new(&s.remote).is_dir();
    match crate::public_repo::check(s.api, &s.repo, None, &[&s.remote, &crate::wiki::wiki_remote(&s.remote)]) {
        Ok(r) => say(&format!("public repo check: {} passes ({} rulesets, {} collaborators)", s.repo, r.rulesets, r.collaborators)),
        Err(e) if to_github && !s.dry_run => return Err(format!("public repo check: {e}")),
        Err(e) => say(&format!("public repo check: {e} (a publish would stop here)")),
    }

    let d = s.rel.view(tag)?;
    if !d.draft {
        return Err(format!("{tag} is already published"));
    }
    if d.target.len() != 40 || !d.target.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("the draft {tag} targets {:?}, not a commit", d.target));
    }
    rm_rf(&s.stage)?;
    let dir = s.stage.join("assets");
    mkdir(&dir)?;
    s.rel.download(tag, &dir)?;
    let mut errs = Vec::new();
    for (name, size) in &d.assets {
        match size_of(&dir.join(name)) {
            Ok(n) if n == *size => {}
            Ok(n) => errs.push(format!("{name}: downloaded {n} bytes, the draft lists {size}")),
            Err(_) => errs.push(format!("{name}: listed on the draft but not downloaded")),
        }
    }
    if !errs.is_empty() {
        return Err(errs.join("; "));
    }
    let m = verify_assets(&dir, tag, &s.repo)?;
    say(&format!("assets: {} binaries match the manifest (sha256 and size)", m.files.len()));
    let _ = std::fs::remove_file(dir.join("manifest.json.sig"));
    binary_leak_gate(&dir, &s.denylist, s.accepted.as_deref(), "the release assets")?;
    secret_gate(&dir, "the release assets")?;

    // The public main must be the draft's commit, or the binaries commit on
    // it from an earlier run of this step.
    let public = s.stage.join("public");
    clone(&s.remote, &public)?;
    // Before anything is committed, signed or pushed.
    if let Some(c) = &s.checkout {
        same_code(c, &public, &d.target)?;
        say("this checkout holds what the draft's commit holds");
    }
    if tag_on_remote(&public, tag)? {
        return Err(format!("{tag} already exists in {}", s.remote));
    }
    let head = git_out(&public, &["rev-parse", "HEAD"])?;
    let subject = format!("atlas {tag} binaries");
    let resumed = head != d.target;
    if resumed {
        let parent = git_out(&public, &["rev-parse", "-q", "--verify", "HEAD^"]).unwrap_or_default();
        let subj = git_out(&public, &["log", "-1", "--format=%s"])?;
        if parent != d.target || subj != subject {
            return Err(format!("main of {} is at {head}, but the draft {tag} targets {}: merge nothing else between the release pull request and this step", s.remote, d.target));
        }
        say("main already has this release's binaries commit (an earlier run); checking it");
    }
    let other = other_addresses(&git_out(&public, &["log", "--format=%H %ae %ce"])?, &s.ident.email);
    if !other.is_empty() {
        return Err(format!("the public history carries another address: {}", other.join(" ")));
    }

    let bin = public.join("bin");
    if !resumed {
        rm_rf(&bin)?;
        mkdir(&bin)?;
        copy(&public.join("scripts/launcher.sh"), &bin.join("atlas"))?;
        for f in &m.files {
            copy(&dir.join(&f.file), &bin.join(&f.file))?;
        }
        output(git(&public).args(["add", "-A", "-f", "bin"]))?;
        for f in ls(&bin)? {
            if !f.ends_with(".exe") {
                output(git(&public).args(["update-index", "--chmod=+x", &format!("bin/{f}")]))?;
            }
        }
        let msg = s.stage.join("bin-message.txt");
        write(&msg, format!("{subject}\n\nThe prebuilt binaries of {tag}, checked against its signed manifest.\n"))?;
        let mut c = git(&public);
        c.args(["commit", "-q", "-F"]).arg(&msg);
        s.ident.on(&mut c);
        output(&mut c)?;
        if git_out(&public, &["log", "-1", "--format=%ae%n%ce"])?.lines().any(|a| a != s.ident.email) {
            return Err("the binaries commit carries another address".into());
        }
    }
    // bin/ in the commit is exactly the verified files and the launcher.
    let mut want: Vec<String> = m.files.iter().map(|f| f.file.clone()).collect();
    want.push("atlas".into());
    want.sort();
    let mut got: Vec<String> = git_out(&public, &["ls-tree", "--name-only", "HEAD", "bin/"])?.lines().map(|l| l.trim_start_matches("bin/").to_string()).collect();
    got.sort();
    if got != want {
        return Err(format!("bin/ in the binaries commit holds [{}], not [{}]", got.join(", "), want.join(", ")));
    }
    for f in &m.files {
        let committed = git_out(&public, &["rev-parse", &format!("HEAD:bin/{}", f.file)])?;
        let verified = output(git(&public).arg("hash-object").arg(dir.join(&f.file)))?;
        if committed != verified.trim() {
            return Err(format!("bin/{} in the binaries commit is not the verified file", f.file));
        }
    }
    let launcher = git_out(&public, &["rev-parse", "HEAD:bin/atlas"])?;
    if launcher != git_out(&public, &["rev-parse", "HEAD:scripts/launcher.sh"])? {
        return Err("bin/atlas is not scripts/launcher.sh".into());
    }
    let commit = git_out(&public, &["rev-parse", "HEAD"])?;
    say(&format!("binaries commit {} on {}", &commit[..12], d.target.get(..12).unwrap_or(&d.target)));

    say("wiki");
    let pages = s.stage.join("wiki-pages");
    let names = crate::wiki::stage_pages(&public.join("wiki"), &read_text(&public.join("CHANGELOG.md"))?, &pages)?;
    leak_gate(&pages, &s.denylist, "the wiki pages")?;
    let ident = crate::wiki::Ident { name: &s.ident.name, email: &s.ident.email };
    let wiki = crate::wiki::prepare(&pages, &crate::wiki::wiki_remote(&s.remote), &s.repo, tag, &ident, &s.stage.join("wiki"))?;
    say(&if wiki.changed { format!("wiki commit: {} pages, {}", names.len(), wiki.stat.lines().last().unwrap_or("").trim()) } else { "wiki: no page changed".into() });

    if s.dry_run {
        say(&format!("dry run: not signed, nothing pushed or published. It would sign manifest.json, upload manifest.json.sig, push main ({}) and the wiki, and publish {tag} on it.", &commit[..12]));
        return Ok(Signed { commit, published: false });
    }
    // The key's preflight, after every other check: openssl on PATH and a
    // file at the key's path (not opened; openssl alone opens it, to sign).
    crate::release::signing_ready(s.key.as_deref().and_then(Path::to_str), crate::release::which("openssl").as_deref())?;
    s.confirm.confirm(
        &format!(
            "sign the manifest of {tag}, upload manifest.json.sig, push main ({}) {}and the wiki to {}, and publish {tag}: the release goes PUBLIC",
            &commit[..12],
            if resumed { "(pushed already) " } else { "" },
            s.remote
        ),
        &format!("publish {tag}"),
    )?;
    let manifest = dir.join("manifest.json");
    sign_manifest(s.key.as_deref().expect("checked above"), &s.trusted, &manifest)?;
    say("manifest signed; signature checked against the built-in key");
    s.rel.upload(tag, &PathBuf::from(format!("{}.sig", manifest.display())))?;
    say("uploaded manifest.json.sig");
    if !resumed {
        if !status(git(&public).args(["push", "-q", "origin", "HEAD:refs/heads/main"]))? {
            return Err("pushing the binaries commit to main failed (nothing is published)".into());
        }
        say(&format!("pushed the binaries commit to main of {}", s.remote));
    }
    crate::wiki::push(&wiki).map_err(|e| format!("the wiki push failed ({e}); run `release sign {tag}` again (it resumes)"))?;
    if wiki.changed {
        say(&format!("pushed the wiki to {}", wiki.remote));
    }
    s.rel.publish(tag, &commit)?;
    if s.rel.view(tag)?.draft {
        return Err(format!("{tag} is still a draft after publishing"));
    }
    say(&format!("published {tag} on {}", &commit[..12]));
    Ok(Signed { commit, published: true })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A person who types what each prompt asks.
    pub(crate) struct Yes;
    impl crate::release_check::Confirm for Yes {
        fn confirm(&self, _prompt: &str, _expect: &str) -> R {
            Ok(())
        }
    }

    /// No one at a terminal: every prompt refused, naming its answer.
    pub(crate) struct No;
    impl crate::release_check::Confirm for No {
        fn confirm(&self, _prompt: &str, expect: &str) -> R {
            Err(format!("refused at `{expect}`; nothing was changed"))
        }
    }

    pub(crate) fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atlas-relci-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub(crate) fn sh(dir: &Path, args: &[&str]) -> String {
        git_out(dir, args).unwrap_or_else(|e| panic!("{e}"))
    }

    pub(crate) const REPO: &str = "acme/tool";

    /// Five small files that pass for the five binaries.
    pub(crate) fn fake_bins(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for t in TARGETS {
            let f = bin_name(t);
            let head: &[u8] = if f.ends_with(".exe") {
                b"MZ\x90\x00"
            } else if f.ends_with("-linux-gnu") {
                b"\x7fELF"
            } else {
                b"\xcf\xfa\xed\xfe"
            };
            std::fs::write(dir.join(&f), [head, format!(" binary for {t}").as_bytes()].concat()).unwrap();
        }
    }

    /// A term no file of the repository holds: the export is leak-checked
    /// against it, and this source carries it only in two halves.
    pub(crate) fn term() -> String {
        ["zqxj", "voolp"].concat()
    }

    pub(crate) fn denylist(d: &Path) -> PathBuf {
        let p = d.join("deny.txt");
        std::fs::write(&p, format!("# test terms\n{}\n", term())).unwrap();
        p
    }

    pub(crate) fn openssl() -> Option<PathBuf> {
        crate::release::which("openssl")
    }

    /// A temp unencrypted ed25519 key and its public half in hex.
    pub(crate) fn temp_key(d: &Path) -> Option<(PathBuf, String)> {
        let o = openssl()?;
        let key = d.join("k.pem");
        let ok = Command::new(&o).args(["genpkey", "-algorithm", "ed25519", "-out"]).arg(&key).stdin(Stdio::null()).output().ok()?.status.success();
        let der = d.join("k.pub.der");
        let ok = ok && Command::new(&o).args(["pkey", "-pubout", "-outform", "DER", "-in"]).arg(&key).arg("-out").arg(&der).stdin(Stdio::null()).output().ok()?.status.success();
        if !ok {
            return None;
        }
        let b = std::fs::read(&der).ok()?;
        Some((key, crate::release::hex(&b[b.len() - 32..])))
    }

    /// The manifest the CI path writes is what the updater reads, and a
    /// signature made as `release sign` makes it verifies with
    /// `atlas::update::verify`.
    #[test]
    fn manifest_round_trips_through_the_updater() {
        let d = tmp("manifest");
        let bins = d.join("bins");
        fake_bins(&bins);
        let text = manifest_json("1.2.3", REPO, &bins).unwrap();
        let m = atlas::update::parse_manifest(text.as_bytes()).unwrap();
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.files.len(), 5);
        for f in &m.files {
            assert_eq!(f.file, bin_name(&f.target));
            assert_eq!(f.url, format!("https://github.com/{REPO}/releases/download/v1.2.3/{}", f.file));
            assert_eq!(f.sha256, sha256_file(&bins.join(&f.file)).unwrap());
            assert_eq!(f.size, size_of(&bins.join(&f.file)).unwrap());
        }
        std::fs::write(bins.join("manifest.json"), &text).unwrap();
        assert_eq!(verify_assets(&bins, "v1.2.3", REPO).unwrap(), m);
        match temp_key(&d) {
            Some((key, pubhex)) => {
                let mp = bins.join("manifest.json");
                sign_manifest(&key, &pubhex, &mp).unwrap();
                let sig = std::fs::read(bins.join("manifest.json.sig")).unwrap();
                let k = atlas::update::key_from_hex(&pubhex).unwrap();
                assert!(atlas::update::verify(text.as_bytes(), &sig, &k));
                assert!(!atlas::update::verify(text.replace("1.2.3", "1.2.4").as_bytes(), &sig, &k));
                // A key that is not the built-in one is refused before signing.
                let e = sign_manifest(&key, &"0".repeat(64), &mp).unwrap_err();
                assert!(e.contains("not the key built into atlas"), "{e}");
            }
            None => eprintln!("openssl not found: the signing half of the round trip skipped"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The jobs of a workflow: `(name, its lines)`, from the two-space
    /// keys under `jobs:`.
    fn jobs(text: &str) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        let mut on = false;
        for l in text.lines() {
            if !l.starts_with(' ') && !l.is_empty() && !l.starts_with('#') {
                on = l == "jobs:";
                continue;
            }
            if !on {
                continue;
            }
            match l.strip_prefix("  ").filter(|r| !r.starts_with(' ') && !r.starts_with('#') && r.ends_with(':')) {
                Some(name) => out.push((name.trim_end_matches(':').to_string(), Vec::new())),
                None => {
                    if let Some(j) = out.last_mut() {
                        j.1.push(l.to_string());
                    }
                }
            }
        }
        out
    }

    /// Whether the tests run in the development repository. The public one
    /// is its export, which leaves out the release-pr workflow
    /// (export-ignore) and carries the release's binaries in bin/.
    fn in_dev_repo(root: &Path) -> bool {
        root.join(".github/workflows/release-pr.yml").is_file()
    }

    /// The release workflows read as the rules say: every action pinned by
    /// a full commit SHA, no `pull_request_target`, no token permission by
    /// default and each job's own, every job gated to its repository (or
    /// behind a job that is), the development one never exported, and the
    /// public build in the release's build environment for every target.
    #[test]
    fn the_release_workflows_are_pinned_gated_and_least_privileged() {
        let root = repo_root();
        let dev = in_dev_repo(&root);
        let dev_only = [".github/workflows/release-pr.yml", ".github/workflows/dev-pr.yml"];
        for (file, gate) in [(".github/workflows/release.yml", "bloodfeast/atlas"), (dev_only[0], "bloodfeast/atlas-dev"), (dev_only[1], "bloodfeast/atlas-dev")] {
            if !dev && dev_only.contains(&file) {
                // Never exported, so the public repository has none to check.
                continue;
            }
            let text = read_text(&root.join(file)).unwrap();
            assert!(!text.contains('\t'), "{file}: a tab");
            assert!(!text.contains("pull_request_target"), "{file}: pull_request_target");
            assert!(text.lines().any(|l| l == "permissions: {}"), "{file}: the token's default must be no permission");
            for l in text.lines().filter(|l| l.trim_start().starts_with("uses:") || l.trim_start().starts_with("- uses:")) {
                let spec = l.split("uses:").nth(1).unwrap().trim();
                let (action, rest) = spec.split_once('@').unwrap_or_else(|| panic!("{file}: {l}"));
                let sha = rest.split_whitespace().next().unwrap();
                assert!(action.starts_with("actions/"), "{file}: only GitHub's own actions: {l}");
                assert!(sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()), "{file}: not pinned by SHA: {l}");
                assert!(rest.contains("# v"), "{file}: the pinned version is named: {l}");
            }
            let js = jobs(&text);
            assert!(!js.is_empty(), "{file}: no jobs");
            for (name, lines) in &js {
                assert!(lines.iter().any(|l| l.starts_with("    permissions:")), "{file}: job {name} sets no permissions");
                let gated = lines.iter().any(|l| l.trim() == format!("if: github.repository == '{gate}'"));
                let behind = lines.iter().any(|l| l.trim_start().starts_with("needs:"));
                assert!(gated || behind, "{file}: job {name} is not gated to {gate}");
                let writes: Vec<&String> = lines.iter().filter(|l| l.trim_end().ends_with(": write") && !l.contains("permission-")).collect();
                assert!(writes.is_empty() || (file.ends_with("release.yml") && name == "draft" && writes.len() == 1), "{file}: job {name} writes: {writes:?}");
            }
        }
        let attrs = output(Command::new("git").arg("-C").arg(&root).args(["check-attr", "export-ignore", "--", dev_only[0], dev_only[1], ".github/workflows/release.yml"])).unwrap();
        for f in dev_only {
            assert!(attrs.contains(&format!("{f}: export-ignore: set")), "{attrs}");
        }
        assert!(attrs.contains(".github/workflows/release.yml: export-ignore: unspecified"), "{attrs}");

        let text = read_text(&root.join(".github/workflows/release.yml")).unwrap();
        assert!(text.contains("  CARGO_PROFILE_RELEASE_STRIP: symbols\n") && text.contains("  ATLAS_RELEASE_BUILD: 1\n"));
        let js = jobs(&text);
        let body = |n: &str| js.iter().find(|(name, _)| name == n).unwrap_or_else(|| panic!("no job {n}")).1.join("\n");
        for job in ["linux", "windows", "macos"] {
            let b = body(job);
            assert!(b.contains("cargo build --release --locked --target \"$TARGET\""), "{job}: the release build line");
            for to in ["=atlas\"", "=cargo\"", "=rust\""] {
                assert!(b.contains(to), "{job}: no remap to {to}");
            }
            assert_eq!(b.contains("/PDBALTPATH:%_PDB%"), job == "windows", "{job}: /PDBALTPATH only on MSVC");
        }
        for t in TARGETS {
            assert!(text.contains(t), "release.yml does not build {t}");
        }
        let linux = body("linux");
        assert!(linux.contains("container: debian:bullseye") && linux.contains("[ \"$max\" -gt 31 ]"), "the glibc floor");
        // The remaps the local release makes, by the same names.
        let ctx = crate::release::Ctx { stage: PathBuf::from("S"), cargo_home: PathBuf::from("C"), sysroot: "R".into() };
        let f = ctx.rustflags("x86_64-pc-windows-msvc");
        for to in ["=atlas", "=cargo", "=rust", "/PDBALTPATH:%_PDB%"] {
            assert!(f.contains(to), "Ctx::rustflags lacks {to}: {f}");
        }
        if dev {
            dev_pr_checks_as_the_release_builds(&root, &linux);
            release_pr_leak_checks_the_release_linux_build(&root, &linux);
        }
    }

    /// The release-pr workflow's Linux build (the release process, Q1): the
    /// public linux job's environment and build step, then the binary gate,
    /// before the pull request job, uploading nothing.
    fn release_pr_leak_checks_the_release_linux_build(root: &Path, release_linux: &str) {
        let text = read_text(&root.join(".github/workflows/release-pr.yml")).unwrap();
        let js = jobs(&text);
        let body = |n: &str| js.iter().find(|(name, _)| name == n).unwrap_or_else(|| panic!("release-pr.yml: no job {n}")).1.join("\n");
        let (leak, pr) = (body("linux-leak"), body("pr"));
        assert!(pr.lines().any(|l| l.trim() == "needs: linux-leak"), "release-pr.yml: the pull request waits on the Linux leak check");
        assert!(leak.contains("container: debian:bullseye") && leak.contains("CARGO_PROFILE_RELEASE_STRIP: symbols") && leak.contains("ATLAS_RELEASE_BUILD: 1"));
        assert!(leak.contains("TARGET: x86_64-unknown-linux-gnu") && !leak.contains("upload-artifact"), "release-pr.yml: the x86_64 build, nothing uploaded");
        // The build step's script is release.yml's, line for line.
        let script = |b: &str| -> String {
            let from = b.find("set -eu").unwrap_or_else(|| panic!("no build script: {b}"));
            b[from..].lines().take_while(|l| !l.trim().is_empty() && !l.trim_start().starts_with("- name:") && !l.trim_start().starts_with('#')).map(str::trim).collect::<Vec<_>>().join("\n")
        };
        assert_eq!(script(&leak), script(release_linux), "release-pr.yml's Linux build is release.yml's");
        let build = leak.find("cargo build --release --locked").unwrap();
        let gate = leak.find("cargo xtask release gate --dir dist").expect("the binary gate");
        let secret = leak.find("secrets.ATLAS_LEAK_DENYLIST").unwrap();
        assert!(build < secret && leak.find("cargo build -q --locked -p xtask").unwrap() < secret && secret < gate, "builds before the denylist is in the job");
        assert!(leak.contains("ATLAS_LEAK_ACCEPTED: ${{ runner.temp }}/leak-accepted.txt"));
        // Ship dispatches a commit it checked and finds the run by a nonce
        // in its name; both jobs build and export that commit, on main only.
        assert!(text.contains("run-name: ${{ github.event_name == 'workflow_dispatch' && format('release-pr v{0} {1}', inputs.version, inputs.nonce)"));
        for (name, b) in [("linux-leak", &leak), ("pr", &pr)] {
            assert!(b.contains("ref: ${{ inputs.commit || github.sha }}") && b.contains("fetch-depth: 0"), "release-pr.yml: {name} checks out the dispatched commit");
            assert!(b.contains(r#"if [ "$EVENT" = workflow_dispatch ] && [ -z "$COMMIT" ]; then"#), "release-pr.yml: {name} refuses a dispatch without a commit");
            let on_main = b.find("git merge-base --is-ancestor HEAD refs/remotes/origin/main").unwrap_or_else(|| panic!("release-pr.yml: {name} refuses a commit off main"));
            assert!(on_main < b.find("secrets.").unwrap(), "release-pr.yml: {name} checks the commit before any secret");
        }
    }

    /// The development repository's pull request check (dev-pr.yml): the
    /// release's Linux build environment and the public CI's two clippy
    /// commands, run whenever a pull request touches the code paths.
    fn dev_pr_checks_as_the_release_builds(root: &Path, release_linux: &str) {
        let text = read_text(&root.join(".github/workflows/dev-pr.yml")).unwrap();
        let js = jobs(&text);
        let body = |n: &str| js.iter().find(|(name, _)| name == n).unwrap_or_else(|| panic!("dev-pr.yml: no job {n}")).1.join("\n");
        let (changes, check) = (body("changes"), body("release-check"));
        assert!(changes.contains(r"grep -Eq '^(Cargo\.[^/]+|rust-toolchain\.toml|build\.rs|(src|xtask|tests|benches|examples|config|hooks|vocab|\.cargo|\.github)/.+)$'") && changes.contains("build=true\n"), "dev-pr.yml: the paths, and build on a failed listing");
        assert!(check.contains("needs.changes.outputs.build != 'false'") && check.contains("!cancelled()"), "dev-pr.yml: release-check runs unless the listing said no");
        assert!(check.contains("container: debian:bullseye") && check.contains("[ \"$max\" -gt 31 ]") && release_linux.contains("container: debian:bullseye"));
        assert!(check.contains("CARGO_PROFILE_RELEASE_STRIP: symbols") && check.contains("ATLAS_RELEASE_BUILD: 1") && check.contains("cargo build --release --locked --target \"$TARGET\""));
        assert!(check.contains("TARGET: x86_64-unknown-linux-gnu") && !check.contains("upload-artifact"), "dev-pr.yml: the x86_64 build, nothing uploaded");
        let ci = read_text(&root.join(".github/workflows/ci.yml")).unwrap();
        let clippy: Vec<&str> = ci.lines().map(str::trim).filter(|l| l.starts_with("run: cargo +") && l.contains(" clippy ")).collect();
        assert_eq!(clippy.len(), 2, "ci.yml's clippy runs");
        for c in clippy {
            assert!(check.lines().any(|l| l.trim() == c), "dev-pr.yml lacks ci.yml's {c}");
        }
    }

    #[test]
    fn addresses_allowed_in_the_public_history() {
        let me = "1+me@users.noreply.github.com";
        assert!(address_allowed(me, me));
        assert!(address_allowed("noreply@github.com", me));
        assert!(address_allowed("123+atlas-release[bot]@users.noreply.github.com", me));
        assert!(!address_allowed("someone@example.com", me));
        assert!(!address_allowed("2+other@users.noreply.github.com", me));
    }

    #[test]
    fn only_the_accepted_merges_may_carry_another_address() {
        let me = "1+me@users.noreply.github.com";
        let log = format!("{} a@example.com noreply@github.com\n{} a@example.com noreply@github.com\nabc123 {me} {me}\n", ACCEPTED_MERGES[0], ACCEPTED_MERGES[1]);
        assert!(other_addresses(&log, me).is_empty());
        let log = format!("{log}def456 b@example.com noreply@github.com\nfed789 b@example.com {me}\n");
        assert_eq!(other_addresses(&log, me), ["b@example.com"]);
        assert_eq!(other_addresses(&format!("abc123  {me}\n"), me), ["<empty>"]);
        assert_eq!(other_addresses(&format!("{}\n", ACCEPTED_MERGES[0]), me), [ACCEPTED_MERGES[0]]);
    }

    #[test]
    fn ci_mode_arguments_are_recognised() {
        let a = |v: &[&str]| wants(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(a(&["sign", "v1.0.0"]) && a(&["assets", "--dir", "d"]) && a(&["--remote", "x", "--pr"]) && a(&["ship", "v1.0.0"]));
        assert!(!a(&["--remote", "x", "--push"]) && !a(&["--build-only"]));
    }

    /// A public repository as the stub and local bare repositories stand
    /// in for it, with the draft of v1.2.3 on its main.
    pub(crate) struct Fixture {
        pub d: PathBuf,
        pub bare: PathBuf,
        pub stub: PathBuf,
        pub target: String,
    }

    pub(crate) fn fixture(name: &str) -> Fixture {
        let d = tmp(name);
        let bare = d.join("pub.git");
        sh(&d, &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        let wiki = d.join("pub.wiki.git");
        sh(&d, &["init", "-q", "--bare", "-b", "master", wiki.to_str().unwrap()]);
        let seed = d.join("seed");
        sh(&d, &["clone", "-q", bare.to_str().unwrap(), seed.to_str().unwrap()]);
        for (f, t) in [("bin/atlas.exe", "MZ the last release's binary\n")].into_iter().chain(SOURCE) {
            std::fs::create_dir_all(seed.join(f).parent().unwrap()).unwrap();
            std::fs::write(seed.join(f), t).unwrap();
        }
        sh(&seed, &["add", "-A"]);
        sh(&seed, &["-c", "user.name=P", "-c", "user.email=pub@example.invalid", "commit", "-q", "-m", "atlas v1.2.3"]);
        sh(&seed, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
        let target = sh(&seed, &["rev-parse", "HEAD"]);
        let stub = d.join("stub");
        let assets = stub.join("release-v1.2.3").join("assets");
        fake_bins(&assets);
        std::fs::write(assets.join("manifest.json"), manifest_json("1.2.3", REPO, &assets).unwrap()).unwrap();
        let list: Vec<String> = ls(&assets).unwrap().iter().map(|f| format!(r#"{{"name": "{f}", "size": {}}}"#, size_of(&assets.join(f)).unwrap())).collect();
        write_view(&stub, &target, &list.join(", "));
        Fixture { d, bare, stub, target }
    }

    pub(crate) fn write_view(stub: &Path, target: &str, assets: &str) {
        std::fs::write(stub.join("release-v1.2.3").join("view.json"), format!(r#"{{"isDraft": true, "targetCommitish": "{target}", "assets": [{assets}]}}"#)).unwrap();
    }

    /// Rewrites the stub's view so its sizes follow the files (as GitHub's
    /// listing would after a bad upload).
    fn relist(fx: &Fixture) {
        let assets = fx.stub.join("release-v1.2.3").join("assets");
        let list: Vec<String> = ls(&assets).unwrap().iter().map(|f| format!(r#"{{"name": "{f}", "size": {}}}"#, size_of(&assets.join(f)).unwrap())).collect();
        write_view(&fx.stub, &fx.target, &list.join(", "));
    }

    pub(crate) struct NoApi;
    impl crate::public_repo::Api for NoApi {
        fn get(&self, path: &str) -> R<String> {
            Err(format!("no network in tests ({path})"))
        }
    }

    pub(crate) fn signer<'a>(fx: &Fixture, rel: &'a StubReleases, dry_run: bool, key: Option<(PathBuf, String)>) -> Sign<'a> {
        let (key, trusted) = match key {
            Some((k, h)) => (Some(k), h),
            None => (None, "1".repeat(64)),
        };
        Sign {
            tag: "v1.2.3".into(),
            checkout: None,
            repo: REPO.into(),
            remote: fx.bare.to_string_lossy().replace('\\', "/"),
            dry_run,
            denylist: denylist(&fx.d),
            accepted: None,
            key,
            trusted,
            test_key: "2".repeat(64),
            confirm: &Yes,
            ident: Ident { name: "P".into(), email: "pub@example.invalid".into() },
            stage: fx.d.join("stage"),
            api: &NoApi,
            rel,
        }
    }

    pub(crate) fn uploaded(fx: &Fixture) -> bool {
        fx.stub.join("release-v1.2.3").join("uploaded").exists() || fx.stub.join("release-v1.2.3").join("published.txt").exists()
    }

    /// The release's source as the fixture's public main holds it at the
    /// draft's commit (beside the last release's bin/), and as the
    /// development checkouts of the guard tests hold it (beside
    /// `DEV_ONLY`, which the export leaves out).
    pub(crate) const SOURCE: [(&str, &str); 9] = [
        ("Cargo.toml", "[package]\nname = \"x\"\nversion = \"1.2.3\"\n"),
        (".claude-plugin/plugin.json", "{\n  \"version\": \"1.2.3\"\n}\n"),
        ("scripts/launcher.sh", "#!/bin/sh\necho launcher\n"),
        ("wiki/Home.md", "# Home\n"),
        ("CHANGELOG.md", "# Changelog\n\n## v1.2.3\n\n- one\n"),
        ("README.md", "readme\n"),
        ("xtask/src/main.rs", "fn main() {}\n"),
        ("src/update.rs", "pub const KEY: &str = \"\";\n"),
        (".gitattributes", "* text=auto eol=lf\n# never exported\n.github/workflows/dev-only.yml export-ignore\n"),
    ];

    /// The development repository's export-ignored file.
    pub(crate) const DEV_ONLY: (&str, &str) = (".github/workflows/dev-only.yml", "on: push\n");

    /// Writes `files` in `repo`, commits them and makes the commit origin's
    /// main (forced: the tests rewind main).
    pub(crate) fn commit_files(repo: &Path, files: &[(&str, &str)], msg: &str) {
        for (f, t) in files {
            std::fs::create_dir_all(repo.join(f).parent().unwrap()).unwrap();
            std::fs::write(repo.join(f), t).unwrap();
        }
        sh(repo, &["add", "-A"]);
        sh(repo, &["-c", "user.name=D", "-c", "user.email=dev@example.invalid", "commit", "-q", "-m", msg]);
        sh(repo, &["push", "-q", "--force", "origin", "HEAD:refs/heads/main"]);
    }

    /// A development repository: a bare origin, and a clean checkout of its
    /// main holding `SOURCE` and `DEV_ONLY`.
    pub(crate) fn dev_checkout(d: &Path) -> PathBuf {
        let origin = d.join("dev.git");
        sh(d, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        let dev = d.join("dev");
        sh(d, &["clone", "-q", origin.to_str().unwrap(), dev.to_str().unwrap()]);
        let mut files = SOURCE.to_vec();
        files.push(DEV_ONLY);
        commit_files(&dev, &files, "dev");
        dev
    }

    /// Steps the checkout back one commit, leaving origin's main ahead.
    pub(crate) fn fall_behind(dev: &Path) {
        commit_files(dev, &[("later.txt", "later\n")], "later");
        sh(dev, &["reset", "-q", "--hard", "HEAD~1"]);
    }

    #[test]
    fn checkout_guard_wants_a_clean_checkout_on_main() {
        let d = tmp("guard");
        let dev = dev_checkout(&d);
        let head = sh(&dev, &["rev-parse", "HEAD"]);
        assert_eq!(checkout_guard(&dev, OnMain::Tip).unwrap(), head);
        assert_eq!(checkout_guard(&dev, OnMain::Reachable).unwrap(), head);
        // An uncommitted change to a tracked file.
        std::fs::write(dev.join("src/update.rs"), "changed\n").unwrap();
        for on in [OnMain::Tip, OnMain::Reachable] {
            let e = checkout_guard(&dev, on).unwrap_err();
            assert!(e.starts_with("uncommitted changes in ") && e.contains("src/update.rs"), "{e}");
        }
        sh(&dev, &["checkout", "-q", "--", "."]);
        // Any tracked file counts, not only the build's.
        std::fs::write(dev.join("README.md"), "changed\n").unwrap();
        assert!(checkout_guard(&dev, OnMain::Tip).unwrap_err().contains("README.md"));
        sh(&dev, &["checkout", "-q", "--", "."]);
        // An untracked or ignored file where cargo builds xtask from; one elsewhere is fine.
        std::fs::write(dev.join("notes.txt"), "mine\n").unwrap();
        checkout_guard(&dev, OnMain::Tip).unwrap();
        for f in ["xtask/build.rs", ".cargo/config.toml", "src/extra.rs"] {
            std::fs::create_dir_all(dev.join(f).parent().unwrap()).unwrap();
            std::fs::write(dev.join(f), "x\n").unwrap();
            let e = checkout_guard(&dev, OnMain::Tip).unwrap_err();
            assert!(e.starts_with("uncommitted changes in ") && e.contains(f.split('/').next().unwrap()), "{f}: {e}");
            std::fs::remove_file(dev.join(f)).unwrap();
        }
        std::fs::write(dev.join(".git/info/exclude"), "src/ignored.rs\n").unwrap();
        std::fs::write(dev.join("src/ignored.rs"), "x\n").unwrap();
        assert!(checkout_guard(&dev, OnMain::Tip).unwrap_err().contains("src/ignored.rs"), "an ignored file");
        std::fs::remove_file(dev.join("src/ignored.rs")).unwrap();
        // A change hidden from git status.
        sh(&dev, &["update-index", "--skip-worktree", "src/update.rs"]);
        std::fs::write(dev.join("src/update.rs"), "hidden\n").unwrap();
        let e = checkout_guard(&dev, OnMain::Tip).unwrap_err();
        assert!(e.contains("hidden from git status: src/update.rs"), "{e}");
        sh(&dev, &["update-index", "--no-skip-worktree", "src/update.rs"]);
        sh(&dev, &["checkout", "-q", "--", "."]);
        // Main moved on since this checkout last fetched: only the fetch shows it.
        fall_behind(&dev);
        assert_ne!(sh(&dev, &["rev-parse", "refs/remotes/origin/main"]), head);
        sh(&dev, &["update-ref", "refs/remotes/origin/main", &head]);
        let e = checkout_guard(&dev, OnMain::Tip).unwrap_err();
        assert!(e.starts_with(&format!("HEAD is {}, not origin/main", short(&head))), "{e}");
        assert_eq!(checkout_guard(&dev, OnMain::Reachable).unwrap(), head, "a commit on main is enough for sign");
        // A commit main does not hold.
        std::fs::write(dev.join("local.txt"), "local\n").unwrap();
        sh(&dev, &["add", "-A"]);
        sh(&dev, &["-c", "user.name=D", "-c", "user.email=dev@example.invalid", "commit", "-q", "-m", "local"]);
        assert!(checkout_guard(&dev, OnMain::Tip).unwrap_err().contains(", not origin/main ("));
        assert!(checkout_guard(&dev, OnMain::Reachable).unwrap_err().contains(" is not on origin/main ("));
        // A fetch that fails.
        sh(&dev, &["reset", "-q", "--hard", &head]);
        sh(&dev, &["remote", "set-url", "origin", d.join("absent.git").to_str().unwrap()]);
        let e = checkout_guard(&dev, OnMain::Reachable).unwrap_err();
        assert!(e.starts_with("git fetch of origin's main failed"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `sign` guards every remote but a local folder, which needs the stub.
    #[test]
    fn sign_guards_every_run_that_can_reach_the_real_draft() {
        let d = tmp("sign-which");
        let url = "https://github.com/acme/tool.git";
        assert_eq!(sign_checkout(&d, url, false).unwrap(), Some(d.clone()));
        assert_eq!(sign_checkout(&d, url, true).unwrap(), Some(d.clone()));
        assert_eq!(sign_checkout(&d, d.to_str().unwrap(), true).unwrap(), None);
        assert!(sign_checkout(&d, d.to_str().unwrap(), false).unwrap_err().contains("needs ATLAS_GITHUB_STUB"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sign_runs_only_from_a_clean_checkout_of_the_release_source() {
        let fx = fixture("sign-checkout");
        let rel = StubReleases(fx.stub.clone());
        let dev = dev_checkout(&fx.d);
        let good = sh(&dev, &["rev-parse", "HEAD"]);
        let with = |dry_run: bool| Sign { checkout: Some(dev.clone()), ..signer(&fx, &rel, dry_run, None) };
        // A dirty checkout stops before anything is downloaded, dry or not.
        std::fs::write(dev.join("xtask/src/main.rs"), "changed\n").unwrap();
        for dry_run in [true, false] {
            let e = sign(&with(dry_run)).unwrap_err();
            assert!(e.starts_with("uncommitted changes in "), "{e}");
        }
        assert!(!fx.d.join("stage").exists(), "it stops before the download");
        sh(&dev, &["checkout", "-q", "--", "."]);
        // The release's source, export-ignored files aside, on main or behind it: the dry run passes.
        sign(&with(true)).unwrap();
        fall_behind(&dev);
        sign(&with(true)).unwrap();
        // A commit not on main.
        sh(&dev, &["checkout", "-q", "-b", "side"]);
        std::fs::write(dev.join("side.txt"), "side\n").unwrap();
        sh(&dev, &["add", "-A"]);
        sh(&dev, &["-c", "user.name=D", "-c", "user.email=dev@example.invalid", "commit", "-q", "-m", "side"]);
        assert!(sign(&with(true)).unwrap_err().contains("is not on origin/main"));
        // A later main, or anything on main that is not the release's: each path is compared.
        sh(&dev, &["reset", "-q", "--hard", "refs/remotes/origin/main"]);
        let e = sign(&with(true)).unwrap_err();
        assert!(e.starts_with(&format!("this checkout is not the release's source (the draft's commit {}): 1 path(s) differ (later.txt)", short(&fx.target))), "{e}");
        for (f, t) in [("xtask/src/main.rs", "fn other() {}\n"), ("src/update.rs", "pub const KEY: &str = \"x\";\n"), ("Cargo.lock", "# new\n"), (DEV_ONLY.0, "changed: never compared\n")] {
            sh(&dev, &["reset", "-q", "--hard", &good]);
            commit_files(&dev, &[(f, t)], "other code");
            let r = sign(&with(true));
            if f == DEV_ONLY.0 {
                r.unwrap();
            } else {
                let e = r.unwrap_err();
                assert!(e.contains(&format!("1 path(s) differ ({f})")), "{e}");
            }
        }
        // An export-ignore pattern cannot be followed.
        sh(&dev, &["reset", "-q", "--hard", &good]);
        commit_files(&dev, &[(".gitattributes", "*.yml export-ignore\n")], "a pattern");
        assert!(sign(&with(true)).unwrap_err().contains("marks the pattern *.yml export-ignore"));
        assert_eq!(sh(&fx.bare, &["rev-parse", "main"]), fx.target);
        assert!(!uploaded(&fx));
        let _ = std::fs::remove_dir_all(&fx.d);
    }

    #[test]
    fn pr_from_a_local_checkout_wants_it_clean_on_main() {
        let d = tmp("pr-guard");
        let files = [("Cargo.toml", "[package]\nname = \"x\"\nversion = \"1.2.3\"\n"), (".claude-plugin/plugin.json", "{\n  \"version\": \"1.2.3\"\n}\n")];
        // The fixture's source carries these two files as they are here.
        let dev = dev_checkout(&d);
        assert_eq!(std::fs::read_to_string(dev.join(files[1].0)).unwrap(), files[1].1);
        let mk = |guard_checkout: bool| Pr {
            root: dev.clone(),
            remote: "https://github.com/acme/tool.git".into(),
            expect: None,
            dry_run: false,
            require_clean: true,
            guard_checkout,
            denylist: denylist(&d),
            repo: REPO.into(),
            ident: Ident { name: "P".into(), email: "pub@example.invalid".into() },
            stage: d.join("stage"),
            publish_ok: true,
            in_ci: false,
            confirm: &No,
            api: &NoApi,
            open_pr: false,
        };
        std::fs::write(dev.join("Cargo.toml"), format!("{}# edited\n", files[0].1)).unwrap();
        assert!(pr(&mk(true)).unwrap_err().starts_with("uncommitted changes in "));
        sh(&dev, &["checkout", "-q", "--", "."]);
        fall_behind(&dev);
        let e = pr(&mk(true)).unwrap_err();
        assert!(e.contains(", not origin/main ("), "{e}");
        // Without the guard (CI's fresh checkout) it goes on, to the changelog here.
        let e = pr(&mk(false)).unwrap_err();
        assert!(!e.contains("origin/main") && !e.contains("uncommitted"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sign_refuses_a_hash_mismatch() {
        let fx = fixture("sign-hash");
        let f = fx.stub.join("release-v1.2.3/assets/atlas-aarch64-apple-darwin");
        let mut b = std::fs::read(&f).unwrap();
        *b.last_mut().unwrap() ^= 1;
        std::fs::write(&f, b).unwrap();
        let rel = StubReleases(fx.stub.clone());
        // The key's preflight comes after every check: without a key, the
        // bad asset is what refuses.
        for dry_run in [false, true] {
            let e = sign(&signer(&fx, &rel, dry_run, None)).unwrap_err();
            assert!(e.contains("atlas-aarch64-apple-darwin: sha256 ") && e.contains(", the manifest says ") && !e.contains("ATLAS_SIGN_KEY"), "{e}");
        }
        assert!(!uploaded(&fx));
        let _ = std::fs::remove_dir_all(&fx.d);
    }

    #[test]
    fn sign_refuses_a_size_mismatch() {
        let fx = fixture("sign-size");
        let f = fx.stub.join("release-v1.2.3/assets/atlas.exe");
        let mut b = std::fs::read(&f).unwrap();
        let want = b.len();
        b.push(0);
        std::fs::write(&f, b).unwrap();
        let rel = StubReleases(fx.stub.clone());
        // The draft's listing disagrees with the download first.
        let e = sign(&signer(&fx, &rel, true, None)).unwrap_err();
        assert!(e.contains(&format!("atlas.exe: downloaded {} bytes, the draft lists {want}", want + 1)), "{e}");
        // An asset uploaded wrong: the listing agrees, the manifest does not.
        relist(&fx);
        let e = sign(&signer(&fx, &rel, true, None)).unwrap_err();
        assert!(e.contains(&format!("atlas.exe: {} bytes, the manifest says {want}", want + 1)), "{e}");
        assert!(!uploaded(&fx));
        let _ = std::fs::remove_dir_all(&fx.d);
    }

    #[test]
    fn sign_refuses_a_missing_or_extra_asset() {
        let fx = fixture("sign-missing");
        let assets = fx.stub.join("release-v1.2.3/assets");
        std::fs::remove_file(assets.join("atlas-x86_64-unknown-linux-gnu")).unwrap();
        relist(&fx);
        let rel = StubReleases(fx.stub.clone());
        let e = sign(&signer(&fx, &rel, true, None)).unwrap_err();
        assert!(e.contains("atlas-x86_64-unknown-linux-gnu: listed in the manifest but missing from the draft"), "{e}");
        std::fs::remove_file(assets.join("manifest.json")).unwrap();
        relist(&fx);
        assert_eq!(sign(&signer(&fx, &rel, true, None)).unwrap_err(), "the draft has no manifest.json");
        fake_bins(&assets);
        std::fs::write(assets.join("manifest.json"), manifest_json("1.2.3", REPO, &assets).unwrap()).unwrap();
        std::fs::write(assets.join("extra.txt"), "x").unwrap();
        relist(&fx);
        let e = sign(&signer(&fx, &rel, true, None)).unwrap_err();
        assert!(e.contains("extra.txt: in the draft but not in the manifest"), "{e}");
        assert!(!uploaded(&fx));
        let _ = std::fs::remove_dir_all(&fx.d);
    }

    #[test]
    fn sign_refuses_a_leak_in_a_binary() {
        let fx = fixture("sign-leak");
        let assets = fx.stub.join("release-v1.2.3/assets");
        let f = assets.join("atlas-aarch64-unknown-linux-gnu");
        let mut b = std::fs::read(&f).unwrap();
        b.extend_from_slice(format!("\0/home/{}/src\0", term().to_uppercase()).as_bytes());
        std::fs::write(&f, b).unwrap();
        std::fs::remove_file(assets.join("manifest.json")).unwrap();
        std::fs::write(assets.join("manifest.json"), manifest_json("1.2.3", REPO, &assets).unwrap()).unwrap();
        relist(&fx);
        let rel = StubReleases(fx.stub.clone());
        let e = sign(&signer(&fx, &rel, true, None)).unwrap_err();
        assert!(e.contains("1 leak(s) in the release assets"), "{e}");
        assert!(!e.to_lowercase().contains(&term()), "the term is never printed: {e}");
        assert!(!uploaded(&fx));

        // The place in the accepted-hits file: the binary passes. The
        // manifest beside it is a text file, never accepted.
        let db = crate::ucd::Db::load(&crate::pack_embedder::default_ucd()).unwrap();
        let m = crate::leak::Matcher::new(&db, &read_text(&denylist(&fx.d)).unwrap());
        let named = vec![("atlas-aarch64-unknown-linux-gnu".to_string(), std::fs::read(&f).unwrap())];
        let acc = crate::accept::Accepted(crate::accept::places(&m, &named).into_iter().map(|p| (p.term, p.digest.unwrap())).collect());
        let acc_file = fx.d.join("accepted.txt");
        std::fs::write(&acc_file, acc.lines()).unwrap();
        let mut s = signer(&fx, &rel, true, None);
        s.accepted = Some(acc_file);
        if let Err(e) = sign(&s) {
            assert!(!e.contains("leak"), "{e}");
        }
        let _ = std::fs::remove_dir_all(&fx.d);
    }

    #[test]
    fn sign_refuses_a_secret_shaped_run_in_a_binary_even_in_a_dry_run() {
        let fx = fixture("sign-secret");
        let assets = fx.stub.join("release-v1.2.3/assets");
        let f = assets.join("atlas-aarch64-unknown-linux-gnu");
        let mut b = std::fs::read(&f).unwrap();
        // v0.2.0's packed prefixes, assembled here so no source holds the run.
        b.extend_from_slice(format!("\0{}{}{}\0", "sk_test", "_rk_live_", "modelssourcequote").as_bytes());
        std::fs::write(&f, b).unwrap();
        std::fs::remove_file(assets.join("manifest.json")).unwrap();
        std::fs::write(assets.join("manifest.json"), manifest_json("1.2.3", REPO, &assets).unwrap()).unwrap();
        relist(&fx);
        let rel = StubReleases(fx.stub.clone());
        let e = sign(&signer(&fx, &rel, true, None)).unwrap_err();
        assert!(e.starts_with("1 secret-shaped run(s) in the release assets") && e.contains("push protection would refuse"), "{e}");
        assert!(!e.contains("modelssourcequote"), "the run is never printed: {e}");
        assert!(!fx.d.join("stage").join("public").exists(), "it stops before the public clone");
        assert!(!uploaded(&fx));
        let _ = std::fs::remove_dir_all(&fx.d);
    }

    #[test]
    fn sign_refuses_when_main_moved_or_the_release_is_out() {
        let fx = fixture("sign-moved");
        write_view(&fx.stub, &"a".repeat(40), "");
        let rel = StubReleases(fx.stub.clone());
        let e = sign(&signer(&fx, &rel, true, None)).unwrap_err();
        assert!(e.contains("but the draft v1.2.3 targets aaaa"), "{e}");
        std::fs::write(fx.stub.join("release-v1.2.3/published.txt"), "x").unwrap();
        assert_eq!(sign(&signer(&fx, &rel, true, None)).unwrap_err(), "v1.2.3 is already published");
        let _ = std::fs::remove_dir_all(&fx.d);
    }

    /// The whole step against the stub and local bare repositories: a dry
    /// run that pushes nothing, then (with openssl) the real one: the
    /// signature uploaded, main carrying the binaries, the wiki pushed and
    /// the draft published on the binaries commit; a second run resumes.
    #[test]
    fn sign_publishes_on_a_binaries_commit() {
        let fx = fixture("sign-ok");
        let rel = StubReleases(fx.stub.clone());
        let r = sign(&signer(&fx, &rel, true, None)).unwrap();
        assert!(!r.published && r.commit != fx.target);
        assert_eq!(sh(&fx.bare, &["rev-parse", "main"]), fx.target, "a dry run pushes nothing");
        assert!(!uploaded(&fx));
        let Some(key) = temp_key(&fx.d) else {
            eprintln!("openssl not found: the publishing half skipped");
            return;
        };
        let r = sign(&signer(&fx, &rel, false, Some(key.clone()))).unwrap();
        assert!(r.published);
        assert_eq!(sh(&fx.bare, &["rev-parse", "main"]), r.commit);
        assert_eq!(sh(&fx.bare, &["rev-parse", "main^"]), fx.target);
        assert_eq!(sh(&fx.bare, &["log", "-1", "--format=%s|%ae|%ce", "main"]), "atlas v1.2.3 binaries|pub@example.invalid|pub@example.invalid");
        let mut files: Vec<String> = sh(&fx.bare, &["ls-tree", "--name-only", "main", "bin/"]).lines().map(str::to_string).collect();
        files.sort();
        assert_eq!(files.len(), 6);
        assert!(sh(&fx.bare, &["ls-tree", "main", "bin/atlas-x86_64-unknown-linux-gnu"]).starts_with("100755"));
        assert!(sh(&fx.bare, &["ls-tree", "main", "bin/atlas.exe"]).starts_with("100644"));
        assert_eq!(std::fs::read_to_string(fx.stub.join("release-v1.2.3/published.txt")).unwrap(), r.commit);
        let sig = std::fs::read(fx.stub.join("release-v1.2.3/uploaded/manifest.json.sig")).unwrap();
        let man = std::fs::read(fx.stub.join("release-v1.2.3/assets/manifest.json")).unwrap();
        assert!(atlas::update::verify(&man, &sig, &atlas::update::key_from_hex(&key.1).unwrap()));
        let wiki = fx.d.join("pub.wiki.git");
        assert_eq!(sh(&wiki, &["log", "-1", "--format=%s", "master"]), "atlas v1.2.3 wiki");
        // Published: a second run refuses. Unpublished again (a failed
        // publish), it resumes on the binaries commit without a new one.
        assert_eq!(sign(&signer(&fx, &rel, false, Some(key.clone()))).unwrap_err(), "v1.2.3 is already published");
        std::fs::remove_file(fx.stub.join("release-v1.2.3/published.txt")).unwrap();
        let again = sign(&signer(&fx, &rel, false, Some(key))).unwrap();
        assert_eq!(again.commit, r.commit);
        assert_eq!(sh(&fx.bare, &["rev-parse", "main"]), r.commit);
        let _ = std::fs::remove_dir_all(&fx.d);
    }

    #[test]
    fn assets_writes_the_manifest_and_notes_or_names_what_is_missing() {
        let d = tmp("assets");
        let dir = d.join("dist");
        fake_bins(&dir);
        let root = repo_root();
        let version = version_of(&root).unwrap();
        let notes = d.join("notes.md");
        assets(&root, &dir, &notes, Some(&format!("v{version}")), REPO).unwrap();
        let m = verify_assets(&dir, &format!("v{version}"), REPO).unwrap();
        assert_eq!(m.files.len(), 5);
        let n = std::fs::read_to_string(&notes).unwrap();
        for f in &m.files {
            assert!(n.contains(&format!("| bin/{} | `{}` |", f.file, f.sha256)), "{} not in the notes", f.file);
        }
        assert!(assets(&root, &dir, &notes, Some("v0.0.0"), REPO).unwrap_err().contains("asked to release v0.0.0"));
        std::fs::remove_file(dir.join("atlas-x86_64-apple-darwin")).unwrap();
        std::fs::write(dir.join("stray"), "x").unwrap();
        let e = assets(&root, &dir, &notes, None, REPO).unwrap_err();
        assert!(e.ends_with("missing [atlas-x86_64-apple-darwin], unexpected [stray]"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `--pr` against a local bare repository standing in for the public
    /// one: the branch carries the export over main, keeps main's bin/,
    /// drops what the export no longer has, and is made under the public
    /// identity; a tag already out stops it.
    #[test]
    fn pr_mode_pushes_a_release_branch_to_a_local_bare_repo() {
        if !in_dev_repo(&repo_root()) {
            // `--pr` exports the development repository; the public one has
            // bin/, which the export refuses.
            eprintln!("skipped: --pr runs only in the development repository");
            return;
        }
        let d = tmp("pr");
        let bare = d.join("pub.git");
        sh(&d, &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        let seed = d.join("seed");
        sh(&d, &["clone", "-q", bare.to_str().unwrap(), seed.to_str().unwrap()]);
        std::fs::create_dir_all(seed.join("bin")).unwrap();
        std::fs::write(seed.join("bin/atlas.exe"), "MZ old binary").unwrap();
        std::fs::write(seed.join("gone.txt"), "a file the export no longer has").unwrap();
        sh(&seed, &["add", "-A"]);
        sh(&seed, &["-c", "user.name=P", "-c", "user.email=pub@example.invalid", "commit", "-q", "-m", "atlas v0.0.1"]);
        sh(&seed, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
        let main = sh(&seed, &["rev-parse", "HEAD"]);
        let remote = bare.to_string_lossy().replace('\\', "/");
        let root = repo_root();
        let tag = format!("v{}", version_of(&root).unwrap());
        let mk = |dry_run: bool| Pr {
            root: root.clone(),
            remote: remote.clone(),
            expect: Some(tag.clone()),
            dry_run,
            require_clean: false,
            guard_checkout: false,
            denylist: denylist(&d),
            repo: DEFAULT_PUBLIC_REPO.into(),
            ident: Ident { name: "P".into(), email: "pub@example.invalid".into() },
            stage: d.join("stage"),
            publish_ok: false,
            in_ci: false,
            confirm: &No,
            api: &NoApi,
            open_pr: false,
        };
        let branch = format!("release/{tag}");
        let made = pr(&mk(true)).unwrap();
        assert!(sh(&bare, &["branch", "--list", &branch]).is_empty(), "a dry run pushes nothing");
        assert!(made.notes.contains(PENDING_BINARIES.trim_end()) && made.notes.contains(&format!("# atlas {tag}")));
        let made = pr(&mk(false)).unwrap();
        assert_eq!(made.branch, branch);
        assert_eq!(sh(&bare, &["rev-parse", &branch]), made.commit);
        assert_eq!(sh(&bare, &["rev-parse", &format!("{branch}^")]), main);
        assert_eq!(sh(&bare, &["rev-parse", "main"]), main, "main is untouched");
        let files = sh(&bare, &["ls-tree", "-r", "--name-only", &branch]);
        assert!(files.lines().any(|l| l == "bin/atlas.exe") && files.lines().any(|l| l == "Cargo.toml") && files.lines().any(|l| l == ".github/workflows/release.yml"));
        assert!(!files.lines().any(|l| l == "gone.txt" || l == ".github/workflows/release-pr.yml"), "{files}");
        assert_eq!(sh(&bare, &["show", &format!("{branch}:bin/atlas.exe")]), "MZ old binary");
        assert_eq!(sh(&bare, &["log", "-1", "--format=%ae|%ce|%s", &branch]), format!("pub@example.invalid|pub@example.invalid|atlas {tag}"));
        // A rerun replaces the branch; an expected tag that is not the tree's stops it.
        pr(&mk(false)).unwrap();
        let mut wrong = mk(true);
        wrong.expect = Some("v9.9.9".into());
        assert!(pr(&wrong).unwrap_err().contains("asked to release v9.9.9"));
        // The tag already out stops it.
        sh(&seed, &["tag", &tag]);
        sh(&seed, &["push", "-q", "origin", &format!("refs/tags/{tag}")]);
        assert!(pr(&mk(true)).unwrap_err().contains("already exists"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A push of the release branch to GitHub: ATLAS_PUBLISH counts in the
    /// release-pr job only; anywhere else the typed `open vX.Y.Z` is asked,
    /// and ATLAS_PUBLISH does not stand for it.
    #[test]
    fn a_push_to_github_outside_ci_is_typed_at_a_terminal() {
        let d = tmp("push-gate");
        let mk = |in_ci: bool, publish_ok: bool, confirm: &'static dyn crate::release_check::Confirm| Pr {
            root: d.clone(),
            remote: "https://github.com/acme/tool.git".into(),
            expect: None,
            dry_run: false,
            require_clean: true,
            guard_checkout: false,
            denylist: d.join("deny.txt"),
            repo: REPO.into(),
            ident: Ident { name: "P".into(), email: "pub@example.invalid".into() },
            stage: d.join("stage"),
            publish_ok,
            in_ci,
            confirm,
            api: &NoApi,
            open_pr: false,
        };
        let gate = |p: &Pr| push_gate(p, "v1.2.3", "release/v1.2.3");
        assert!(gate(&mk(true, true, &No)).is_ok(), "the release-pr job: ATLAS_PUBLISH, no prompt");
        assert!(gate(&mk(true, false, &Yes)).unwrap_err().contains("needs ATLAS_PUBLISH=v1.2.3"));
        assert_eq!(gate(&mk(false, true, &No)).unwrap_err(), "refused at `open v1.2.3`; nothing was changed", "ATLAS_PUBLISH does not stand for the prompt");
        assert!(gate(&mk(false, false, &Yes)).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }
}
