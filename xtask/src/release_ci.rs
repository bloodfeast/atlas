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
//!     binaries, checks each SHA-256 and size against the manifest and the
//!     leak denylist against every file, commits the binaries to the public
//!     main as bin/ (the plugin install is a git clone), signs the manifest
//!     with ATLAS_SIGN_KEY, uploads manifest.json.sig, pushes main and the
//!     wiki, and publishes the draft on the binaries commit, which creates
//!     the tag as the owner. --dry-run stops before signing and pushing.
//! ```
//!
//! Pushing to a remote that is not a local folder needs ATLAS_PUBLISH=vX.Y.Z
//! in both `--pr` and `sign`, as in the `--push` release. A local bare
//! repository as `--remote` is the dry path: `--pr` pushes the branch there
//! and opens no pull request; `sign` with ATLAS_GITHUB_STUB reads the draft
//! from the stub folder (`release-<tag>/view.json` and `release-<tag>/assets/`)
//! and writes what it would upload and publish beside it.
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
       cargo xtask release sign vX.Y.Z [--remote URL] [--dry-run]";

/// The binaries table row of the pull request's notes: the binaries are
/// built after the merge.
pub const PENDING_BINARIES: &str = "| Every platform | built by the release workflow after this pull request merges | listed in the draft release |\n";

/// Whether `release` arguments ask for a CI mode.
pub fn wants(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("assets" | "sign")) || args.iter().any(|a| a == "--pr")
}

/// Whether an address may appear in the public history: the public one,
/// and GitHub's own for commits it makes when a pull request is merged
/// (`noreply@github.com` as committer, an app's `[bot]` noreply address).
pub fn address_allowed(addr: &str, public_email: &str) -> bool {
    addr == public_email || addr == "noreply@github.com" || (addr.ends_with("[bot]@users.noreply.github.com") && !addr.contains(['<', '>', ' ']))
}

/// The version, from Cargo.toml and plugin.json, which must agree.
fn version_of(root: &Path) -> R<String> {
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
    fn from_env() -> Ident {
        Ident {
            name: env("ATLAS_PUBLIC_NAME").unwrap_or_else(|| "Brandon White".into()),
            email: env("ATLAS_PUBLIC_EMAIL").unwrap_or_else(|| "19593371+bloodfeast@users.noreply.github.com".into()),
        }
    }

    fn on(&self, c: &mut Command) {
        c.env("GIT_AUTHOR_NAME", &self.name).env("GIT_AUTHOR_EMAIL", &self.email).env("GIT_COMMITTER_NAME", &self.name).env("GIT_COMMITTER_EMAIL", &self.email);
    }
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0");
    c
}

fn git_out(dir: &Path, args: &[&str]) -> R<String> {
    output(git(dir).args(args)).map(|s| s.trim_end().to_string())
}

/// Clones `remote` into `dir` (fresh); the remote is never printed with
/// credentials because none are put in it (gh supplies them in CI).
fn clone(remote: &str, dir: &Path) -> R {
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

fn stage_root(root: &Path) -> PathBuf {
    match env("ATLAS_RELEASE_STAGE") {
        Some(s) => root.join(s),
        None => root.join(env("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into())).join("release-stage"),
    }
}

fn denylist_from_env() -> R<PathBuf> {
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
            let s = Sign {
                remote: remote.unwrap_or_else(|| format!("https://github.com/{repo}.git")),
                stage: stage_root(&root).join(&tag).join("sign"),
                publish_ok: env("ATLAS_PUBLISH").as_deref() == Some(tag.as_str()),
                tag,
                repo,
                dry_run,
                denylist: denylist_from_env()?,
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
            let p = Pr {
                stage: stage_root(&root).join(&tag).join("pr"),
                publish_ok: env("ATLAS_PUBLISH").as_deref() == Some(tag.as_str()),
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
    /// The leak denylist.
    pub denylist: PathBuf,
    /// The public repository's owner/name.
    pub repo: String,
    /// The public identity.
    pub ident: Ident,
    /// Where the work goes (replaced).
    pub stage: PathBuf,
    /// ATLAS_PUBLISH names this release.
    pub publish_ok: bool,
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
    if to_github && !p.publish_ok {
        return Err(format!("pushing to {} needs ATLAS_PUBLISH={tag}", p.remote));
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
        parse_view(&output(Command::new("gh").args(["release", "view", tag, "--repo", &self.repo, "--json", "isDraft,targetCommitish,assets"]))?)
    }
    fn download(&self, tag: &str, dir: &Path) -> R {
        output(Command::new("gh").args(["release", "download", tag, "--repo", &self.repo, "--clobber", "--dir"]).arg(dir)).map(|_| ())
    }
    fn upload(&self, tag: &str, file: &Path) -> R {
        output(Command::new("gh").args(["release", "upload", tag, "--repo", &self.repo, "--clobber"]).arg(file)).map(|_| ())
    }
    fn publish(&self, tag: &str, target: &str) -> R {
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
    /// owner/name of the public repository.
    pub repo: String,
    /// Its remote (a local bare repository for the dry path).
    pub remote: String,
    /// Stop before signing and pushing.
    pub dry_run: bool,
    /// The leak denylist.
    pub denylist: PathBuf,
    /// ATLAS_SIGN_KEY.
    pub key: Option<PathBuf>,
    /// TRUSTED_KEY_HEX and TEST_KEY_HEX of src/update.rs.
    pub trusted: String,
    /// See `trusted`.
    pub test_key: String,
    /// ATLAS_PUBLISH names this release.
    pub publish_ok: bool,
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
    if !s.dry_run {
        // The manual release's preflight: openssl on PATH and a readable
        // key, before anything is downloaded.
        crate::release::signing_ready(s.key.as_deref().and_then(Path::to_str), crate::release::which("openssl").as_deref())?;
        if s.trusted == s.test_key {
            return Err("the key built into atlas is the test key: put the release key in TRUSTED_KEY_HEX".into());
        }
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
    leak_gate(&dir, &s.denylist, "the release assets")?;

    // The public main must be the draft's commit, or the binaries commit on
    // it from an earlier run of this step.
    let public = s.stage.join("public");
    clone(&s.remote, &public)?;
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
    let mut addrs: Vec<String> = git_out(&public, &["log", "--format=%ae%n%ce"])?.lines().map(str::to_string).collect();
    addrs.sort();
    addrs.dedup();
    let other: Vec<&String> = addrs.iter().filter(|a| !address_allowed(a, &s.ident.email)).collect();
    if !other.is_empty() {
        return Err(format!("the public history carries another address: {}", other.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(" ")));
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
    if to_github && !s.publish_ok {
        return Err(format!("publishing to {} needs ATLAS_PUBLISH={tag}", s.remote));
    }
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
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atlas-relci-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        git_out(dir, args).unwrap_or_else(|e| panic!("{e}"))
    }

    const REPO: &str = "acme/tool";

    /// Five small files that pass for the five binaries.
    fn fake_bins(dir: &Path) {
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
    fn term() -> String {
        ["zqxj", "voolp"].concat()
    }

    fn denylist(d: &Path) -> PathBuf {
        let p = d.join("deny.txt");
        std::fs::write(&p, format!("# test terms\n{}\n", term())).unwrap();
        p
    }

    fn openssl() -> Option<PathBuf> {
        crate::release::which("openssl")
    }

    /// A temp unencrypted ed25519 key and its public half in hex.
    fn temp_key(d: &Path) -> Option<(PathBuf, String)> {
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
        for (file, gate) in [(".github/workflows/release.yml", "bloodfeast/atlas"), (".github/workflows/release-pr.yml", "bloodfeast/atlas-dev")] {
            if !dev && file.ends_with("release-pr.yml") {
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
        let attrs = output(Command::new("git").arg("-C").arg(&root).args(["check-attr", "export-ignore", "--", ".github/workflows/release-pr.yml", ".github/workflows/release.yml"])).unwrap();
        assert!(attrs.contains(".github/workflows/release-pr.yml: export-ignore: set"), "{attrs}");
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
    fn ci_mode_arguments_are_recognised() {
        let a = |v: &[&str]| wants(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(a(&["sign", "v1.0.0"]) && a(&["assets", "--dir", "d"]) && a(&["--remote", "x", "--pr"]));
        assert!(!a(&["--remote", "x", "--push"]) && !a(&["--build-only"]));
    }

    /// A public repository as the stub and local bare repositories stand
    /// in for it, with the draft of v1.2.3 on its main.
    struct Fixture {
        d: PathBuf,
        bare: PathBuf,
        stub: PathBuf,
        target: String,
    }

    fn fixture(name: &str) -> Fixture {
        let d = tmp(name);
        let bare = d.join("pub.git");
        sh(&d, &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        let wiki = d.join("pub.wiki.git");
        sh(&d, &["init", "-q", "--bare", "-b", "master", wiki.to_str().unwrap()]);
        let seed = d.join("seed");
        sh(&d, &["clone", "-q", bare.to_str().unwrap(), seed.to_str().unwrap()]);
        for (f, t) in [("scripts/launcher.sh", "#!/bin/sh\necho launcher\n"), ("wiki/Home.md", "# Home\n"), ("CHANGELOG.md", "# Changelog\n\n## v1.2.3\n\n- one\n"), ("README.md", "readme\n")] {
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

    fn write_view(stub: &Path, target: &str, assets: &str) {
        std::fs::write(stub.join("release-v1.2.3").join("view.json"), format!(r#"{{"isDraft": true, "targetCommitish": "{target}", "assets": [{assets}]}}"#)).unwrap();
    }

    /// Rewrites the stub's view so its sizes follow the files (as GitHub's
    /// listing would after a bad upload).
    fn relist(fx: &Fixture) {
        let assets = fx.stub.join("release-v1.2.3").join("assets");
        let list: Vec<String> = ls(&assets).unwrap().iter().map(|f| format!(r#"{{"name": "{f}", "size": {}}}"#, size_of(&assets.join(f)).unwrap())).collect();
        write_view(&fx.stub, &fx.target, &list.join(", "));
    }

    struct NoApi;
    impl crate::public_repo::Api for NoApi {
        fn get(&self, path: &str) -> R<String> {
            Err(format!("no network in tests ({path})"))
        }
    }

    fn signer<'a>(fx: &Fixture, rel: &'a StubReleases, dry_run: bool, key: Option<(PathBuf, String)>) -> Sign<'a> {
        let (key, trusted) = match key {
            Some((k, h)) => (Some(k), h),
            None => (None, "1".repeat(64)),
        };
        Sign {
            tag: "v1.2.3".into(),
            repo: REPO.into(),
            remote: fx.bare.to_string_lossy().replace('\\', "/"),
            dry_run,
            denylist: denylist(&fx.d),
            key,
            trusted,
            test_key: "2".repeat(64),
            publish_ok: false,
            ident: Ident { name: "P".into(), email: "pub@example.invalid".into() },
            stage: fx.d.join("stage"),
            api: &NoApi,
            rel,
        }
    }

    fn uploaded(fx: &Fixture) -> bool {
        fx.stub.join("release-v1.2.3").join("uploaded").exists() || fx.stub.join("release-v1.2.3").join("published.txt").exists()
    }

    #[test]
    fn sign_refuses_a_hash_mismatch() {
        let fx = fixture("sign-hash");
        let f = fx.stub.join("release-v1.2.3/assets/atlas-aarch64-apple-darwin");
        let mut b = std::fs::read(&f).unwrap();
        *b.last_mut().unwrap() ^= 1;
        std::fs::write(&f, b).unwrap();
        let rel = StubReleases(fx.stub.clone());
        let e = sign(&signer(&fx, &rel, false, None)).unwrap_err();
        assert!(e.contains("ATLAS_SIGN_KEY") && !e.contains("sha256"), "the preflight refuses first: {e}");
        let e = sign(&signer(&fx, &rel, true, None)).unwrap_err();
        assert!(e.contains("atlas-aarch64-apple-darwin: sha256 ") && e.contains(", the manifest says "), "{e}");
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
            denylist: denylist(&d),
            repo: DEFAULT_PUBLIC_REPO.into(),
            ident: Ident { name: "P".into(), email: "pub@example.invalid".into() },
            stage: d.join("stage"),
            publish_ok: false,
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
}
