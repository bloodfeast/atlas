//! The binary leak check before anything is public (the release process,
//! Q1 and Q2).
//!
//! ```text
//! cargo xtask release check [--force]
//!     On the owner's machine, before the release pull request: builds the
//!     release binaries this host can build from `git archive HEAD`, one
//!     after another (on Windows: x86_64-pc-windows-msvc, then
//!     x86_64-unknown-linux-gnu and aarch64-unknown-linux-gnu inside WSL),
//!     in the release's build environment with at most 16 jobs, and runs the
//!     binary leak gate (places explained against the exported source,
//!     `explain.rs`; accepted hits honoured) and the secret scan on
//!     them. Refuses first a README.md that is out of date for the version
//!     ([`stale_readme`]; `release ship` runs the same before its export),
//!     then a checkout with an uncommitted change to a tracked
//!     file and a host that did not build every target it should. A pass
//!     writes a stamp keyed by the commit, the denylist's SHA-256 and the
//!     accepted-hits file's SHA-256; a later run with the same key says so
//!     and builds nothing (--force runs it anyway), and a change to any of
//!     the three runs it again.
//! cargo xtask release gate --dir DIR [--src TREE] [--export]
//!     The same gates on the binaries in DIR (the release-pr workflow's
//!     Linux build), explained against TREE (default: this checkout's
//!     tracked, not export-ignored files). With --export, also HEAD
//!     exported as `release --pr` exports it, every file and wiki page
//!     against the denylist: the release-pr job's pr job holds no denylist
//!     and exports only the commit this checked.
//! cargo xtask release accept-hit FILE... [--src TREE] [--print]
//!     Lists every place of every term in the release binaries FILE... with
//!     its verdict against TREE (default: this checkout), and appends an
//!     entry for each place a dependency's string holds that is not yet
//!     accepted to the accepted-hits file (`accept.rs`), after the owner
//!     types the confirmation at a terminal. With --print it writes the
//!     entries to standard output instead and changes nothing.
//! ```
//!
//! Every run prints the denylist's term count and SHA-256 (of the file as
//! it is, and of its text with carriage returns removed and one final
//! newline, the form the release-pr job writes from the secret), so the
//! local file and the secret copy can be compared. Terms are never printed.
//!
//! Environment: ATLAS_LEAK_DENYLIST (else the release config's `denylist`,
//! as ship reads it), ATLAS_LEAK_ACCEPTED (the accepted-hits file; else the
//! config's `accepted`; default `leak-accepted.txt` beside the denylist),
//! CARGO_BUILD_JOBS (capped at 16; default 16), ATLAS_WSL_DISTRO and
//! ATLAS_RELEASE_STAGE as for the release.

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::accept::{places, Accepted};
use crate::release::{bin_name, cargo_version, env, mkdir, output, read_text, rm_rf, say, sha256_file, status, write, Ctx};
use crate::release_ci::{accepted_file, binary_leak_gate, denylist_from_env, git, secret_gate, stage_root, USAGE};

type R<T = ()> = Result<T, String>;

/// The most jobs a local release build may run (the machine's build cap).
const MAX_JOBS: u32 = 16;

/// The stamp's file name in the check's stage folder.
const STAMP: &str = "pass.txt";

/// Runs `check`, `gate` or `accept-hit`.
pub fn run(root: &Path, args: Vec<String>) -> R {
    let mode = args.first().cloned().unwrap_or_default();
    let mut it = args.into_iter().skip(1);
    match mode.as_str() {
        "check" => {
            let mut force = false;
            for a in it {
                match a.as_str() {
                    "--force" => force = true,
                    _ => return Err(format!("unknown argument {a}\n{USAGE}")),
                }
            }
            let version = cargo_version(&read_text(&root.join("Cargo.toml"))?).ok_or("no version in Cargo.toml")?;
            refuse_stale_readme(root, &version)?;
            // The hidden-character gate CI runs on every pull request.
            say(&crate::hidden::tracked_gate(root)?);
            let (denylist, accepted) = lists(root)?;
            check_here(root, &format!("v{version}"), &denylist, accepted.as_deref(), force).map(|_| ())
        }
        "gate" => {
            let (mut dir, mut export, mut src) = (None, false, None);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--dir" => dir = it.next().map(PathBuf::from),
                    "--src" => src = Some(src_arg(it.next())?),
                    "--export" => export = true,
                    _ => return Err(format!("unknown argument {a}\n{USAGE}")),
                }
            }
            let dir = dir.ok_or(USAGE)?;
            let (denylist, accepted) = lists(root)?;
            // The source the binaries were built from: this checkout unless
            // --src names an exported tree.
            gate(&dir, &denylist, accepted.as_deref(), src.as_deref().unwrap_or(root))?;
            if export {
                // The release-pr job's pr job holds no denylist: the export
                // is checked here, and that job exports this commit again.
                let version = cargo_version(&read_text(&root.join("Cargo.toml"))?).ok_or("no version in Cargo.toml")?;
                let stage = stage_root(root).join(format!("v{version}")).join("gate-export");
                let commit = crate::release_ci::export_gate(root, &stage, &denylist)?;
                say(&format!("export of {commit}: every file and wiki page checked, no hits"));
            }
            Ok(())
        }
        "accept-hit" => {
            let (mut files, mut print, mut src) = (Vec::new(), false, None);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--print" => print = true,
                    "--src" => src = Some(src_arg(it.next())?),
                    _ if a.starts_with("--") => return Err(format!("unknown argument {a}\n{USAGE}")),
                    _ => files.push(PathBuf::from(a)),
                }
            }
            if files.is_empty() {
                return Err(USAGE.into());
            }
            let (denylist, target) = if env("ATLAS_LEAK_DENYLIST").is_some() {
                let d = denylist_from_env()?;
                let t = env("ATLAS_LEAK_ACCEPTED").map(PathBuf::from).unwrap_or_else(|| d.with_file_name(crate::accept::DEFAULT_NAME));
                (d, t)
            } else {
                let s = crate::release_ship::settings(root)?;
                let t = s.accepted.unwrap_or_else(|| s.denylist.with_file_name(crate::accept::DEFAULT_NAME));
                (s.denylist, t)
            };
            accept_hit(&files, &denylist, &target, src.as_deref().unwrap_or(root), print, &Tty).map(|_| ())
        }
        _ => Err(USAGE.into()),
    }
}

/// The denylist and accepted-hits file for `check` and `gate`:
/// ATLAS_LEAK_DENYLIST (and ATLAS_LEAK_ACCEPTED) when set, else the release
/// config as ship reads it (`release_ship::settings`).
fn lists(root: &Path) -> R<(PathBuf, Option<PathBuf>)> {
    if env("ATLAS_LEAK_DENYLIST").is_some() {
        let d = denylist_from_env()?;
        let a = accepted_file(env("ATLAS_LEAK_ACCEPTED"), &d)?;
        return Ok((d, a));
    }
    let s = crate::release_ship::settings(root)?;
    Ok((s.denylist, s.accepted))
}

/// `check` on this host's targets with the real build, at most 16 jobs; its
/// stage is `<stage root>/<tag>/check`, where `passed` looks.
pub(crate) fn check_here(root: &Path, tag: &str, denylist: &Path, accepted: Option<&Path>, force: bool) -> R<bool> {
    std::env::set_var("CARGO_BUILD_JOBS", jobs(env("CARGO_BUILD_JOBS")).to_string());
    check(&Check {
        root: root.to_path_buf(),
        stage: stage_root(root).join(tag).join("check"),
        accepted: accepted.map(Path::to_path_buf),
        denylist: denylist.to_path_buf(),
        force,
        want: host_targets(),
        build: &build_release,
    })
}

/// The job count for `check`'s builds: CARGO_BUILD_JOBS, capped at
/// [`MAX_JOBS`], which is also the default.
fn jobs(set: Option<String>) -> u32 {
    set.and_then(|j| j.trim().parse::<u32>().ok()).unwrap_or(MAX_JOBS).clamp(1, MAX_JOBS)
}

/// The denylist's term count and SHA-256, of the file and of its text as
/// the release-pr job writes it from the secret. Never a term.
pub(crate) fn denylist_summary(denylist: &Path) -> R<String> {
    let bytes = std::fs::read(denylist).map_err(|_| "the denylist is not readable".to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let db = crate::ucd::Db::load(&crate::pack_embedder::default_ucd())?;
    let n = crate::leak::Matcher::new(&db, &text).len();
    let canon = format!("{}\n", text.replace('\r', "").trim_end_matches('\n'));
    Ok(format!(
        "denylist: {n} terms; sha256 {} (the file), {} (carriage returns removed, one final newline: the release-pr job's form)",
        atlas::sha256::hex(&bytes),
        atlas::sha256::hex(canon.as_bytes())
    ))
}

/// `gate`: the denylist's summary, then the binary leak gate and the secret
/// scan of DIR.
pub(crate) fn gate(dir: &Path, denylist: &Path, accepted: Option<&Path>, src: &Path) -> R {
    say(&denylist_summary(denylist)?);
    let what = format!("the binaries in {}", dir.display());
    binary_leak_gate(dir, denylist, accepted, Some(src), &what)?;
    secret_gate(dir, &what)
}

// ---------------------------------------------------------------- check

/// What `check` works from.
pub(crate) struct Check<'a> {
    /// The development checkout.
    pub root: PathBuf,
    /// The check's stage folder (replaced, but for the stamp).
    pub stage: PathBuf,
    /// The leak denylist.
    pub denylist: PathBuf,
    /// The accepted-hits file, when there is one.
    pub accepted: Option<PathBuf>,
    /// Run even when the stamp matches.
    pub force: bool,
    /// The targets the build must leave in `<stage>/bin`.
    pub want: Vec<&'static str>,
    /// Builds `<stage>/src` (with `<stage>/src.tar`) into `<stage>/bin`.
    pub build: &'a dyn Fn(&Path) -> R,
}

/// The targets a host must build for `check`.
fn host_targets() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]
    } else if cfg!(target_os = "linux") && std::env::consts::ARCH == "x86_64" {
        vec!["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]
    } else if cfg!(target_os = "linux") {
        vec!["aarch64-unknown-linux-gnu"]
    } else if cfg!(target_os = "macos") {
        vec!["aarch64-apple-darwin", "x86_64-apple-darwin"]
    } else {
        Vec::new()
    }
}

/// The real build: the release's own (`Ctx::build_here`), in the stage.
fn build_release(stage: &Path) -> R {
    let ctx = Ctx {
        stage: stage.to_path_buf(),
        cargo_home: env("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| crate::release::home().join(".cargo")),
        sysroot: output(Command::new("rustc").args(["--print", "sysroot"]))?.trim_end_matches(['\r', '\n']).to_string(),
    };
    ctx.build_here()
}

/// The stamp's key: what a pass is valid for.
pub(crate) fn stamp_key(commit: &str, denylist_sha: &str, accepted_sha: &str) -> String {
    format!("commit {commit}\ndenylist {denylist_sha}\naccepted {accepted_sha}\n")
}

/// Whether the stage holds a pass for `key`.
pub(crate) fn passed(stage: &Path, key: &str) -> bool {
    std::fs::read_to_string(stage.join(STAMP)).is_ok_and(|s| s.starts_with(key))
}

/// The commit HEAD names now and the key `check` would stamp for it: the
/// commit, the denylist and the accepted-hits file ("none" without one).
pub(crate) fn current_key(root: &Path, denylist: &Path, accepted: Option<&Path>) -> R<(String, String)> {
    let head = output(Command::new("git").arg("-C").arg(root).args(["rev-parse", "HEAD"]))?.trim().to_string();
    let acc = match accepted {
        Some(p) => sha256_file(p)?,
        None => "none".into(),
    };
    let key = stamp_key(&head, &sha256_file(denylist)?, &acc);
    Ok((head, key))
}

/// `check`: true when it ran, false when a matching stamp stood.
pub(crate) fn check(c: &Check) -> R<bool> {
    let git = |args: &[&str]| output(Command::new("git").arg("-C").arg(&c.root).args(args));
    if !git(&["status", "--porcelain", "--untracked-files=no"])?.trim().is_empty() {
        return Err("uncommitted changes; the check builds HEAD, and its stamp names HEAD".into());
    }
    say(&denylist_summary(&c.denylist)?);
    // The commit is resolved once: the archive is of the commit the stamp names.
    let (head, key) = current_key(&c.root, &c.denylist, c.accepted.as_deref())?;
    if !c.force && passed(&c.stage, &key) {
        say(&format!("check: passed already for this commit, denylist and accepted-hits file ({})", c.stage.join(STAMP).display()));
        return Ok(false);
    }
    let _ = std::fs::remove_file(c.stage.join(STAMP));
    for d in ["src", "bin"] {
        rm_rf(&c.stage.join(d))?;
    }
    mkdir(&c.stage.join("bin"))?;
    let src_tar = c.stage.join("src.tar");
    if !status(Command::new("git").arg("-C").arg(&c.root).args(["archive", "--format=tar", "-o"]).arg(&src_tar).arg(&head))? {
        return Err("git archive failed".into());
    }
    let archive = std::fs::read(&src_tar).map_err(|e| format!("{}: {e}", src_tar.display()))?;
    crate::tar::extract(&archive, &c.stage.join("src"))?;
    (c.build)(&c.stage)?;
    let bin = c.stage.join("bin");
    let missing: Vec<&str> = c.want.iter().copied().filter(|t| !bin.join(bin_name(t)).is_file()).collect();
    if !missing.is_empty() {
        return Err(format!(
            "not built here: {} (inside WSL, aarch64 needs `sudo apt install gcc-aarch64-linux-gnu` and `rustup target add aarch64-unknown-linux-gnu`)",
            missing.join(", ")
        ));
    }
    binary_leak_gate(&bin, &c.denylist, c.accepted.as_deref(), Some(&c.stage.join("src")), "the local release binaries")?;
    secret_gate(&bin, "the local release binaries")?;
    write(&c.stage.join(STAMP), format!("{key}targets {}\n", c.want.join(" ")))?;
    say(&format!("check passed: {} (stamp {})", c.want.join(", "), c.stage.join(STAMP).display()));
    Ok(true)
}

// ---------------------------------------------------------------- preflight

/// What the preflight stamp is keyed by beyond `current_key`: a change to
/// what the preflight runs voids every older stamp.
const PREFLIGHT_V: &str = "preflight v2: hidden, export dry run, xtask tests, CI clippy, CI tests\n";

/// Ship's preflight, before run 1 dispatches anything: every check the
/// release-pr workflow (and the release workflow after it) makes that can
/// run here, so a failure stops at its own line instead of on GitHub.
///
/// 1. `cargo xtask hidden` over the tracked tree (the linux-leak job's
///    context poisoning guard).
/// 2. `release --pr` as a dry run against `public` (ship's fresh clone of
///    the public repository): the changelog section and its highlights,
///    the versions, the links, the export, every exported file and wiki
///    page against the denylist (the linux-leak job's `gate --export`),
///    the release notes, and the export guard on the public main. Nothing
///    is pushed.
/// 3. `cargo test -p xtask --locked`: the release's own code, and the
///    lockfile the workflows build with `--locked`.
/// 4. The public CI's clippy on the exported tree: each `cargo +TOOLCHAIN
///    clippy` line of its ci.yml, run as written ([`ci_clippy_runs`]).
/// 5. The public CI's workspace tests on the exported tree ([`CI_TESTS`]).
///
/// The public `main` ruleset requires the CI's test and clippy checks, so a
/// failure in 4 or 5 would hold the merge after the dispatch.
///
/// The binary leak gate and the secret scan are `release check`, which ship
/// runs after this. A pass is stamped in `stage` for the commit, the
/// denylist and the accepted-hits file, like `check`'s.
pub(crate) fn preflight(root: &Path, stage: &Path, public: &Path, repo: &str, denylist: &Path, accepted: Option<&Path>) -> R {
    let key = format!("{}{PREFLIGHT_V}", current_key(root, denylist, accepted)?.1);
    if passed(stage, &key) {
        say(&format!("preflight: passed already for this commit, denylist and accepted-hits file ({})", stage.join(STAMP).display()));
        return Ok(());
    }
    rm_rf(stage)?;
    mkdir(stage)?;
    say("preflight 1/5: hidden text (cargo xtask hidden)");
    say(&crate::hidden::tracked_gate(root).map_err(|e| format!("hidden text:\n{e}"))?);
    say("preflight 2/5: `release --pr --dry-run` against a clone of the public repository (export, denylist on every file and wiki page, changelog, notes)");
    let api = crate::public_repo::api();
    let p = crate::release_ci::Pr {
        root: root.to_path_buf(),
        remote: public.to_string_lossy().replace('\\', "/"),
        expect: None,
        dry_run: true,
        require_clean: true,
        guard_checkout: false,
        leak: crate::release_ci::Leak::Denylist(denylist.to_path_buf()),
        repo: repo.to_string(),
        ident: crate::release_ci::Ident::from_env(),
        stage: stage.join("pr"),
        publish_ok: false,
        in_ci: false,
        confirm: &Tty,
        api: api.as_ref(),
        open_pr: false,
    };
    crate::release_ci::pr(&p).map_err(|e| format!("the release-pr export (dry run): {e}"))?;
    say("preflight 3/5: cargo test -p xtask --locked");
    let xtask = ["test", "-p", "xtask", "--locked"].map(String::from);
    cargo_logged(root, &xtask, None, &stage.join("xtask-test.log"), "the xtask tests")?;
    // The public CI's required checks, on the exported tree as the public
    // CI sees it, in a target folder of their own so a rebuild reuses it.
    let tree = stage.join("pr").join("tree");
    as_checkout(&tree)?;
    let target = root.join("target").join("release-preflight");
    let ci = std::fs::read_to_string(tree.join(".github/workflows/ci.yml")).map_err(|e| format!("the exported ci.yml: {e}"))?;
    for (i, args) in ci_clippy_runs(&ci)?.iter().enumerate() {
        say(&format!("preflight 4/5: the public CI's clippy, as ci.yml runs it: cargo {}", args.join(" ")));
        cargo_logged(&tree, args, Some(&target), &stage.join(format!("clippy-{}.log", i + 1)), "the public CI's clippy").map_err(|e| format!("{e}\n(ci.yml pins its clippy toolchain: `rustup toolchain install <version> -c clippy` if it is missing)"))?;
    }
    let tests = CI_TESTS.map(String::from);
    say(&format!("preflight 5/5: the public CI's tests (nextest and doc tests there; here one run): cargo {}", tests.join(" ")));
    cargo_logged(&tree, &tests, Some(&target), &stage.join("ci-test.log"), "the public CI's workspace tests")?;
    write(&stage.join(STAMP), key)?;
    say(&format!("preflight passed (stamp {})", stage.join(STAMP).display()));
    Ok(())
}

/// Makes the extracted export `tree` a git checkout of its own, its index
/// `git add -A` of the export, so the public CI's tests that ask git what
/// is tracked (the hidden-text gate, the public-files check) see the
/// export as the public CI's checkout does. The export never carries
/// `bin/` (`export_commit` refuses one); the CI's checkout of the release
/// commit also holds main's previous `bin/`, which no workspace test
/// depends on (the hidden-text gate skips binaries, and the public-files
/// check asks only for its named files). Without this git answers for
/// whatever repository holds the stage folder, or for none. The index is
/// enough for those tests, so no commit (no identity, no signing).
pub(crate) fn as_checkout(tree: &Path) -> R {
    rm_rf(&tree.join(".git"))?;
    output(git(tree).args(["init", "-q"]))?;
    output(git(tree).args(["add", "-A"]))?;
    Ok(())
}

/// The public CI's tests as one `cargo test` run: ci.yml runs `cargo nextest
/// run --workspace --cargo-profile ci --locked --no-fail-fast` and then the
/// doc tests (`cargo test --workspace --profile ci --locked --doc`), which
/// `cargo test` covers in one run without nextest installed.
const CI_TESTS: [&str; 6] = ["test", "--workspace", "--profile", "ci", "--locked", "--no-fail-fast"];

/// The clippy commands of ci.yml (`run: cargo +TOOLCHAIN clippy ...` lines),
/// as cargo's arguments; an error when it holds none, so a reshaped ci.yml
/// stops the preflight rather than skipping the check.
pub(crate) fn ci_clippy_runs(ci: &str) -> R<Vec<Vec<String>>> {
    let runs: Vec<Vec<String>> = ci
        .lines()
        .filter_map(|l| l.trim().strip_prefix("run: cargo "))
        .filter(|l| l.starts_with('+') && l.split_whitespace().nth(1) == Some("clippy"))
        .map(|l| l.split_whitespace().map(String::from).collect())
        .collect();
    if runs.is_empty() {
        return Err("the exported ci.yml has no `run: cargo +TOOLCHAIN clippy` line: the preflight cannot run the public CI's clippy".into());
    }
    Ok(runs)
}

/// `cargo ARGS` in `dir` (with `CARGO_TARGET_DIR` when `target` is given),
/// its output to `log`; on a failure, the failing lines (tests, panics,
/// errors and the result line), quoted from the log.
fn cargo_logged(dir: &Path, args: &[String], target: Option<&Path>, log: &Path, what: &str) -> R {
    let file = std::fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
    let err = file.try_clone().map_err(|e| e.to_string())?;
    let mut c = Command::new("cargo");
    c.args(args).current_dir(dir).stdout(file).stderr(err);
    if let Some(t) = target {
        c.env("CARGO_TARGET_DIR", t);
    }
    let ok = c.status().map_err(|e| format!("cargo {}: {e}", args.join(" ")))?.success();
    if ok {
        return Ok(());
    }
    let text = std::fs::read_to_string(log).unwrap_or_default();
    Err(format!("{what} failed (log {}):\n{}", log.display(), failing_lines(&text)))
}

/// The lines of a `cargo test` log that say what failed: each failing test,
/// each panic and compile error, and the result line.
pub(crate) fn failing_lines(log: &str) -> String {
    let keep = |l: &str| {
        let t = l.trim_start();
        (t.starts_with("test ") && t.ends_with("FAILED")) || t.contains("panicked at") || t.starts_with("error") || t.starts_with("test result:")
    };
    let lines: Vec<&str> = log.lines().filter(|l| keep(l)).take(40).collect();
    if lines.is_empty() {
        log.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
    } else {
        lines.join("\n")
    }
}

// ---------------------------------------------------------------- readme

/// `X.Y` or `X.Y.Z` at the start of `s`: (major, minor) and the bytes read.
fn lead_version(s: &str) -> Option<((u64, u64), usize)> {
    let b = s.as_bytes();
    let digits = |from: usize| from + b[from.min(b.len())..].iter().take_while(|c| c.is_ascii_digit()).count();
    let e1 = digits(0);
    if e1 == 0 || b.get(e1) != Some(&b'.') {
        return None;
    }
    let e2 = digits(e1 + 1);
    if e2 == e1 + 1 {
        return None;
    }
    let mut end = e2;
    if b.get(e2) == Some(&b'.') && digits(e2 + 1) > e2 + 1 {
        end = digits(e2 + 1);
    }
    Some(((s[..e1].parse().ok()?, s[e1 + 1..e2].parse().ok()?), end))
}

/// Every version stamp in `line`, `at vX.Y[.Z]` (`measured at v...` too,
/// any case, `at` a word of its own): its text and its (major, minor).
fn stamps(line: &str) -> Vec<(&str, (u64, u64))> {
    let lower = line.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = lower[from..].find("at v") {
        let at = from + i;
        from = at + 4;
        if at > 0 && lower.as_bytes()[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        if let Some((v, n)) = lead_version(&line[at + 4..]) {
            // "measured at" is named whole in the finding.
            let start = if lower[..at].ends_with("measured ") { at - 9 } else { at };
            out.push((&line[start..at + 4 + n], v));
        }
    }
    out
}

/// Whether `line` promises work: it holds the word `planned` or `coming`.
fn promises(line: &str) -> bool {
    line.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w.eq_ignore_ascii_case("planned") || w.eq_ignore_ascii_case("coming"))
}

/// The `code` spans of `line`: what sits between a pair of backticks.
fn code_spans(line: &str) -> Vec<&str> {
    line.split('`').skip(1).step_by(2).filter(|s| !s.trim().is_empty()).collect()
}

/// The first release section (`## vX.Y.Z`) of `changelog` that names
/// `` `term` ``; the header above the first section ships nothing.
fn shipped_in<'a>(changelog: &'a str, term: &str) -> Option<&'a str> {
    let quoted = format!("`{term}`");
    let mut section = None;
    for l in changelog.lines() {
        if let Some(v) = l.strip_prefix("## ").filter(|v| v.starts_with('v')) {
            section = Some(v.trim());
        } else if section.is_some() && l.contains(&quoted) {
            return section;
        }
    }
    None
}

/// What README.md says that is out of date for the release `version`, one
/// finding per line, each naming its README line:
///
/// * (a) a figure stamped `at vX.Y.Z` from a minor more than one before the
///   release (an older release number counts as older);
/// * (b) a line that says something is `planned` or `coming` and names, in
///   backticks, a term a release section of the CHANGELOG already names in
///   backticks. A promise in prose alone ("more languages are planned") is
///   not caught: there is nothing exact to look up.
pub(crate) fn stale_readme(readme: &str, changelog: &str, version: &str) -> Vec<String> {
    let Some(((major, minor), _)) = lead_version(version) else {
        return vec![format!("README.md: the release version `{version}` is not X.Y.Z")];
    };
    let mut found = Vec::new();
    for (i, l) in readme.lines().enumerate() {
        for (text, (smaj, smin)) in stamps(l) {
            if smaj < major || (smaj == major && smin + 1 < minor) {
                found.push(format!(
                    "README.md:{}: `{text}` is a figure from v{smaj}.{smin}, more than one minor before v{version}: measure it again and stamp it with this version, or take it out",
                    i + 1
                ));
            }
        }
        if promises(l) {
            for term in code_spans(l) {
                if let Some(v) = shipped_in(changelog, term) {
                    found.push(format!(
                        "README.md:{}: says `{term}` is planned or coming, but CHANGELOG.md ships it in {v}: say what it does now",
                        i + 1
                    ));
                }
            }
        }
    }
    found
}

/// Refuses a README.md that [`stale_readme`] finds out of date for
/// `version`; run by `release check` and by `release ship` before export.
pub(crate) fn refuse_stale_readme(root: &Path, version: &str) -> R {
    let found = stale_readme(&read_text(&root.join("README.md"))?, &read_text(&root.join("CHANGELOG.md"))?, version);
    if found.is_empty() {
        return Ok(());
    }
    Err(format!("README.md is out of date for v{version} (wiki/Releasing.md, the README gate):\n{}", found.join("\n")))
}

// ---------------------------------------------------------------- accept-hit

/// A typed confirmation: the terminal one ([`Tty`]), or unattended ship's
/// (`release_ship::Unattended`), which says the act and goes on. `accept-hit`
/// always takes the terminal one: a new leak hit is the owner's call.
pub trait Confirm {
    /// Ok when the person typed `expect` after `prompt`.
    fn confirm(&self, prompt: &str, expect: &str) -> R;
}

/// The confirmation at a terminal: stdin and stdout must both be one.
pub struct Tty;

impl Confirm for Tty {
    fn confirm(&self, prompt: &str, expect: &str) -> R {
        let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        answer(tty, prompt, expect, || {
            let mut l = String::new();
            std::io::stdin().lock().read_line(&mut l).map_err(|e| e.to_string())?;
            Ok(l)
        })
    }
}

/// The terminal confirmation's rule: refuse without a terminal, and accept
/// exactly `expect` (the line ending aside).
fn answer(tty: bool, prompt: &str, expect: &str, read: impl FnOnce() -> R<String>) -> R {
    if !tty {
        return Err(format!("{prompt}: this needs a person at a terminal (stdin and stdout) to type `{expect}`; nothing was changed"));
    }
    print!("{prompt}\ntype `{expect}` to go on: ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    if read()?.trim_end_matches(['\r', '\n']) == expect {
        Ok(())
    } else {
        Err("not confirmed; nothing was changed".into())
    }
}

/// The value of `--src`: a folder that must be given.
fn src_arg(v: Option<String>) -> R<PathBuf> {
    match v {
        Some(s) if !s.is_empty() && !s.starts_with("--") => Ok(PathBuf::from(s)),
        _ => Err(format!("--src needs the folder the binaries were built from\n{USAGE}")),
    }
}

/// `accept-hit`: every place in `files` with its verdict against the source
/// `src` (on standard error), and the entries for the places a dependency's
/// or the toolchain's string holds that are not yet in the file at `target`,
/// printed (`print`) or appended after the confirmation.
pub(crate) fn accept_hit(files: &[PathBuf], denylist: &Path, target: &Path, src: &Path, print: bool, confirm: &dyn Confirm) -> R<Vec<(usize, String)>> {
    let list = std::fs::read(denylist).map_err(|_| "the denylist is not readable".to_string())?;
    let list = String::from_utf8_lossy(&list).into_owned();
    let db = crate::ucd::Db::load(&crate::pack_embedder::default_ucd())?;
    let m = crate::leak::Matcher::new(&db, &list);
    if m.is_empty() {
        return Err("the denylist holds no terms".into());
    }
    let mut named = Vec::new();
    for f in files {
        let b = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
        if !crate::accept::is_executable(&b) {
            return Err(format!("{}: not an executable (ELF, PE or Mach-O): hits are accepted in release binaries only", f.display()));
        }
        // The file name only: the folders above it are not what ships.
        named.push((f.file_name().map_or_else(|| f.display().to_string(), |n| n.to_string_lossy().into_owned()), b));
    }
    let ps = places(&m, &named);
    if let Some(p) = ps.iter().find(|p| p.at.is_none()) {
        return Err(format!("{}: a path is never accepted; rename it", p.line()));
    }
    let corpus = crate::release_ci::source_corpus(&ps, &named, Some(src), "the binaries")?;
    let existing = if target.is_file() { Accepted::parse(&read_text(target)?, &m)? } else { Accepted::default() };
    let (mut new, mut stop) = (BTreeSet::new(), 0);
    for p in &ps {
        // Every place with its verdict, by term number: the audit trail.
        let v = crate::accept::verdict(p, &named, &corpus);
        let k = crate::accept::entry(&m, p, &v);
        let note = match (&v, &k) {
            (Some(crate::explain::Verdict::Split(_)), _) => "passes without an entry",
            (_, Some(k)) if existing.0.contains(k) => "accepted already",
            (Some(crate::explain::Verdict::Join(..)), Some(_)) => "a new entry, only if the owner finds the pieces packed side by side",
            (_, Some(_)) => "a new entry",
            _ => {
                stop += 1;
                "cannot be accepted: fix the source or the build"
            }
        };
        eprintln!("release: place: {}{}: {note}", p.line(), crate::accept::reason(&m, &v));
        if let Some(k) = k.filter(|k| !existing.0.contains(k)) {
            new.insert(k);
        }
    }
    let terms: BTreeSet<usize> = ps.iter().map(|p| p.term).collect();
    // Standard error, so --print leaves only the entries on standard output.
    eprintln!(
        "release: {} place(s) of {} term(s) in {} file(s): {} new entr(ies), {} that cannot be accepted, {} entr(ies) in {}",
        ps.len(),
        terms.len(),
        named.len(),
        new.len(),
        stop,
        existing.0.len(),
        target.display()
    );
    let new: Vec<(usize, String)> = new.into_iter().collect();
    if print {
        print!("{}", Accepted(new.iter().cloned().collect()).lines());
        return Ok(new);
    }
    if new.is_empty() {
        say("nothing to add");
        return Ok(new);
    }
    confirm.confirm(&format!("append {} entr(ies) to {}", new.len(), target.display()), &format!("accept {}", new.len()))?;
    let mut text = if target.is_file() { read_text(target)? } else { String::new() };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&Accepted(new.iter().cloned().collect()).lines());
    Accepted::parse(&text, &m)?;
    write(target, text)?;
    say(&format!("{} entr(ies) appended to {}", new.len(), target.display()));
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atlas-relcheck-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn term() -> String {
        ["zqxj", "voolp"].concat()
    }

    fn elf(body: &str) -> Vec<u8> {
        let mut b = b"\x7fELF\x02\x01\x01\0".to_vec();
        b.extend_from_slice(body.as_bytes());
        b
    }

    /// A stub confirmation: records the prompt, answers `ok`.
    struct Said(bool, Cell<usize>);
    impl Confirm for Said {
        fn confirm(&self, _: &str, expect: &str) -> R {
            self.1.set(self.1.get() + 1);
            answer(true, "stub", expect, || Ok(if self.0 { expect.to_string() } else { "no".into() }))
        }
    }

    #[test]
    fn the_builds_keep_the_job_cap() {
        assert_eq!([jobs(None), jobs(Some("64".into())), jobs(Some("8".into())), jobs(Some("0".into())), jobs(Some("x".into()))], [16, 16, 8, 1, 16]);
        // WSL does not see CARGO_BUILD_JOBS: the script takes it as $2,
        // before either cargo build.
        let w = crate::release::WSL_BUILD;
        let set = w.find("export CARGO_BUILD_JOBS=\"${2:-4}\"").expect("the WSL script sets the job count");
        assert!(w.match_indices("cargo build").all(|(i, _)| i > set), "{w}");
    }

    #[test]
    fn the_confirmation_needs_a_terminal_and_the_exact_text() {
        let e = answer(false, "append", "accept 2", || panic!("never read without a terminal")).unwrap_err();
        assert!(e.contains("needs a person at a terminal"), "{e}");
        assert!(answer(true, "append", "accept 2", || Ok("accept 3\n".into())).is_err());
        assert!(answer(true, "append", "accept 2", || Ok("accept 2 \n".into())).is_err());
        assert!(answer(true, "append", "accept 2", || Ok("accept 2\r\n".into())).is_ok());
    }

    #[test]
    fn src_needs_a_value() {
        for v in [None, Some(String::new()), Some("--print".to_string())] {
            assert!(src_arg(v.clone()).unwrap_err().contains("--src needs"), "{v:?}");
        }
        assert_eq!(src_arg(Some("t".to_string())).unwrap(), PathBuf::from("t"));
    }

    /// A cargo tree at `d/tree` whose registry dependency `quire` (vendored
    /// outside the tree, in place of crates.io) holds `made by TERM` in a
    /// literal.
    fn cargo_tree(d: &Path) -> PathBuf {
        let tree = d.join("tree");
        let vendor = d.join("vendor");
        let dep = vendor.join("quire");
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::create_dir_all(tree.join(".cargo")).unwrap();
        std::fs::create_dir_all(dep.join("src")).unwrap();
        std::fs::write(tree.join("Cargo.toml"), "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nquire = \"1.0.0\"\n\n[workspace]\n").unwrap();
        let v = atlas::json::to_string(&vendor.display().to_string().replace('\\', "/"));
        std::fs::write(tree.join(".cargo/config.toml"), format!("[source.crates-io]\nreplace-with = \"vendored\"\n\n[source.vendored]\ndirectory = {v}\n")).unwrap();
        std::fs::write(tree.join("src/main.rs"), "fn main() {\n    println!(\"{}\", quire::BY);\n}\n").unwrap();
        std::fs::write(dep.join("Cargo.toml"), "[package]\nname = \"quire\"\nversion = \"1.0.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::write(dep.join(".cargo-checksum.json"), "{\"files\":{},\"package\":null}").unwrap();
        std::fs::write(dep.join("src/lib.rs"), format!("/// The author.\npub const BY: &str = \"made by {}\";\n", term())).unwrap();
        assert!(Command::new("cargo").current_dir(&tree).args(["generate-lockfile", "--offline"]).status().unwrap().success());
        tree
    }

    #[test]
    fn accept_hit_appends_only_after_the_confirmation() {
        let d = tmp("accept");
        let deny = d.join("deny.txt");
        std::fs::write(&deny, format!("# t\n{}\n", term())).unwrap();
        let tree = cargo_tree(&d);
        let bin = d.join("atlas-x");
        std::fs::write(&bin, elf(&format!("\0{}made by {}{}\0", "a".repeat(30), term(), "b".repeat(30)))).unwrap();
        let target = d.join(crate::accept::DEFAULT_NAME);

        // Refused: nothing written.
        let no = Said(false, Cell::new(0));
        assert!(accept_hit(std::slice::from_ref(&bin), &deny, &target, &tree, false, &no).is_err());
        assert_eq!((no.1.get(), target.exists()), (1, false));
        // --print: entries out, nothing written, nothing asked.
        let printed = accept_hit(std::slice::from_ref(&bin), &deny, &target, &tree, true, &no).unwrap();
        assert_eq!((printed.len(), no.1.get(), target.exists()), (1, 1, false));
        // Keyed on the dependency's string, not the bytes around it.
        let db = crate::ucd::Db::load(&crate::pack_embedder::default_ucd()).unwrap();
        let m = crate::leak::Matcher::new(&db, &std::fs::read_to_string(&deny).unwrap());
        let term_text = &m.terms()[0].text;
        assert_eq!(printed[0].1, crate::accept::digest(1, term_text, "quire", format!("made by {}", term()).as_bytes()));
        // Confirmed: written, and the gate then passes the binary.
        let yes = Said(true, Cell::new(0));
        assert_eq!(accept_hit(std::slice::from_ref(&bin), &deny, &target, &tree, false, &yes).unwrap(), printed);
        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text, format!("1  {}\n", printed[0].1));
        assert!(!text.contains(&term()));
        let dir = d.join("bins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&bin, dir.join("atlas-x")).unwrap();
        assert!(binary_leak_gate(&dir, &deny, None, Some(&tree), "t").is_err());
        assert!(binary_leak_gate(&dir, &deny, Some(&target), None, "t").is_err(), "nothing explained without the source");
        binary_leak_gate(&dir, &deny, Some(&target), Some(&tree), "t").unwrap();
        // The same bytes as a text file beside it: never accepted.
        std::fs::write(dir.join("notes.txt"), &std::fs::read(&bin).unwrap()[8..]).unwrap();
        let e = binary_leak_gate(&dir, &deny, Some(&target), Some(&tree), "t").unwrap_err();
        assert!(e.starts_with("1 leak(s)"), "{e}");
        std::fs::remove_file(dir.join("notes.txt")).unwrap();
        // Again: nothing new, nothing asked.
        assert!(accept_hit(std::slice::from_ref(&bin), &deny, &target, &tree, false, &yes).unwrap().is_empty());
        assert_eq!(yes.1.get(), 1);
        // A place the build put in: no entry offered, and the gate stops.
        let built = d.join("atlas-y");
        std::fs::write(&built, elf(&format!("\0/home/{}/src\0", term()))).unwrap();
        assert!(accept_hit(std::slice::from_ref(&built), &deny, &target, &tree, true, &no).unwrap().is_empty());
        std::fs::copy(&built, dir.join("atlas-y")).unwrap();
        let e = binary_leak_gate(&dir, &deny, Some(&target), Some(&tree), "t").unwrap_err();
        assert!(e.starts_with("1 leak(s)") && e.contains("atlas-y") && e.contains("not explained by the source") && !e.contains(&term()), "{e}");
        // A text file is refused outright.
        let txt = d.join("notes.md");
        std::fs::write(&txt, term()).unwrap();
        let e = accept_hit(&[txt], &deny, &target, &tree, false, &yes).unwrap_err();
        assert!(e.contains("not an executable"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The preflight's exported tree sits in a stage folder that may be
    /// inside another checkout (or none): `as_checkout` makes it its own,
    /// tracking the export as the release commit does (ignored files left
    /// out), and the hidden-text gate then passes on it.
    #[test]
    fn as_checkout_tracks_the_export_as_the_release_commit_does() {
        let d = tmp("checkout");
        let g = |dir: &Path, args: &[&str]| assert!(Command::new("git").arg("-C").arg(dir).args(args).output().unwrap().status.success(), "git {args:?}");
        g(&d, &["init", "-q"]);
        let tree = d.join("pr").join("tree");
        for (f, body) in [
            (".gitignore", &b"scratch.txt\n"[..]),
            ("a.txt", b"a\n"),
            ("scratch.txt", b"not exported\n"),
            ("fx/log.txt", b"\x1b[31mred\n"),
            ("xtask/hidden-allow.txt", b"1 fx/log.txt:U+001B captured colour codes\n"),
        ] {
            let p = tree.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        assert!(crate::hidden::tracked_gate(&tree).unwrap_err().contains("is not the top of a git checkout"));
        as_checkout(&tree).unwrap();
        let ls = output(git(&tree).args(["ls-files"])).unwrap();
        assert_eq!(ls.lines().collect::<Vec<_>>(), [".gitignore", "a.txt", "fx/log.txt", "xtask/hidden-allow.txt"]);
        crate::hidden::tracked_gate(&tree).unwrap();
        // Run again over the same stage (a rerun): a fresh index.
        as_checkout(&tree).unwrap();
        crate::hidden::tracked_gate(&tree).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A one-commit repository to check.
    fn repo(d: &Path) -> PathBuf {
        let r = d.join("repo");
        std::fs::create_dir_all(&r).unwrap();
        let g = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&r).args(args).output().unwrap().status.success(), "git {args:?}");
        g(&["init", "-q"]);
        std::fs::write(r.join("Cargo.toml"), "[package]\nversion = \"1.2.3\"\n").unwrap();
        g(&["add", "-A"]);
        g(&["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "one"]);
        r
    }

    #[test]
    fn check_stamps_a_pass_and_reruns_when_the_denylist_or_accepted_hits_change() {
        let d = tmp("stamp");
        let root = repo(&d);
        let deny = d.join("deny.txt");
        std::fs::write(&deny, format!("{}\n", term())).unwrap();
        let builds = Cell::new(0);
        let body = RefCell::new(String::from("clean"));
        let build = |stage: &Path| -> R {
            builds.set(builds.get() + 1);
            assert!(stage.join("src/Cargo.toml").is_file(), "built from the archive");
            for t in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
                std::fs::write(stage.join("bin").join(bin_name(t)), elf(&body.borrow())).unwrap();
            }
            Ok(())
        };
        let mut c = Check {
            root: root.clone(),
            stage: d.join("stage"),
            denylist: deny.clone(),
            accepted: None,
            force: false,
            want: vec!["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"],
            build: &build,
        };
        assert!(check(&c).unwrap());
        assert!(!check(&c).unwrap(), "a matching stamp stands");
        assert_eq!(builds.get(), 1);
        // A changed denylist reruns it.
        std::fs::write(&deny, format!("{}\nother\n", term())).unwrap();
        assert!(check(&c).unwrap());
        assert!(!check(&c).unwrap());
        // A new accepted-hits file reruns it.
        let acc = d.join("acc.txt");
        std::fs::write(&acc, "# none yet\n").unwrap();
        c.accepted = Some(acc.clone());
        assert!(check(&c).unwrap());
        assert!(!check(&c).unwrap());
        std::fs::write(&acc, "# changed\n").unwrap();
        assert!(check(&c).unwrap());
        // --force reruns it.
        c.force = true;
        assert!(check(&c).unwrap());
        assert_eq!(builds.get(), 5);
        c.force = false;
        // A leak fails it and leaves no stamp; a missing target fails it.
        *body.borrow_mut() = format!("x{}y", term());
        c.force = true;
        let e = check(&c).unwrap_err();
        assert!(e.contains("leak(s) in the local release binaries"), "{e}");
        c.force = false;
        let key = current_key(&root, &deny, Some(&acc)).unwrap().1;
        assert!(!passed(&c.stage, &key));
        *body.borrow_mut() = "clean".into();
        c.want.push("x86_64-pc-windows-msvc");
        let e = check(&c).unwrap_err();
        assert!(e.contains("not built here: x86_64-pc-windows-msvc"), "{e}");
        assert!(!passed(&c.stage, &key));
        // A dirty checkout is refused before anything is built.
        c.want.pop();
        std::fs::write(root.join("Cargo.toml"), "changed").unwrap();
        let n = builds.get();
        assert!(check(&c).unwrap_err().contains("uncommitted changes"));
        assert_eq!(builds.get(), n);
    }

    #[test]
    fn the_summary_counts_and_hashes_but_never_names_a_term() {
        let d = tmp("summary");
        let a = d.join("a.txt");
        let b = d.join("b.txt");
        std::fs::write(&a, format!("# c\r\n{}\r\n\r\n", term())).unwrap();
        std::fs::write(&b, format!("# c\n{}\n", term())).unwrap();
        let (sa, sb) = (denylist_summary(&a).unwrap(), denylist_summary(&b).unwrap());
        assert!(sa.starts_with("denylist: 1 terms;") && !sa.contains(&term()), "{sa}");
        // The CI form agrees across line endings; the file's own hash does not.
        let canon = |s: &str| s.split(", ").nth(1).unwrap().split(' ').next().unwrap().to_string();
        assert_eq!(canon(&sa), canon(&sb));
        assert_ne!(sa, sb);
    }

    const LOG: &str =
        "# Changelog\n\nHeader: `atlas header-only`.\n\n## v0.5.0\n\n### Highlights\n\n- **`atlas backlog` combs the backlog.**\n";

    #[test]
    fn a_readme_figure_more_than_one_minor_old_is_refused_with_its_line() {
        let readme = "# t\n| a lookup (measured at v0.4.0) | 2x |\n| memory (at v0.2.0) | 99 |\n";
        let found = stale_readme(readme, LOG, "0.6.1");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].starts_with("README.md:2: `measured at v0.4.0`") && found[0].contains("v0.6.1"), "{}", found[0]);
        assert!(found[1].starts_with("README.md:3: `at v0.2.0`"), "{}", found[1]);
    }

    #[test]
    fn a_figure_one_minor_old_or_newer_passes_and_other_versions_are_not_stamps() {
        let readme = "at v0.5.0 and AT V0.6.0, at v0.6, at v0.7.0\n47 tools (v0.1), format v0.1.0, that v0.1.0, at version 0.1\n";
        assert_eq!(stale_readme(readme, LOG, "0.6.1"), Vec::<String>::new());
    }

    #[test]
    fn a_figure_from_an_older_release_number_is_stale() {
        let found = stale_readme("x (at v0.9.0)\n", LOG, "1.0.0");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("README.md:1:"), "{}", found[0]);
        assert_eq!(stale_readme("x (at v1.1.0)\n", LOG, "1.2.0"), Vec::<String>::new());
    }

    #[test]
    fn a_promise_the_changelog_has_shipped_is_refused() {
        let found = stale_readme("Intro.\nA `atlas backlog` command is planned.\n", LOG, "0.6.1");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("README.md:2: ") && found[0].contains("`atlas backlog`") && found[0].contains("v0.5.0"),
            "{}",
            found[0]
        );
        assert_eq!(stale_readme("`atlas backlog` is coming soon.\n", LOG, "0.6.1").len(), 1);
    }

    #[test]
    fn a_promise_of_unshipped_work_or_no_promise_passes() {
        // Not in a release section (the header does not ship anything).
        assert_eq!(stale_readme("`atlas header-only` is planned.\n", LOG, "0.6.1"), Vec::<String>::new());
        assert_eq!(stale_readme("`atlas next` is planned.\n", LOG, "0.6.1"), Vec::<String>::new());
        // Shipped, but not promised: no `planned` or `coming` word.
        assert_eq!(stale_readme("`atlas backlog` is incoming; it was unplanned.\n", LOG, "0.6.1"), Vec::<String>::new());
    }

    #[test]
    fn refuse_stale_readme_names_the_line_and_needs_both_files() {
        let d = tmp("readme");
        assert!(refuse_stale_readme(&d, "0.6.1").unwrap_err().contains("README.md"));
        std::fs::write(d.join("CHANGELOG.md"), LOG).unwrap();
        std::fs::write(d.join("README.md"), "ok (at v0.6.0)\n").unwrap();
        assert_eq!(refuse_stale_readme(&d, "0.6.1"), Ok(()));
        std::fs::write(d.join("README.md"), "ok\nold (at v0.3.0)\n").unwrap();
        let e = refuse_stale_readme(&d, "0.6.1").unwrap_err();
        assert!(e.contains("README.md:2: `at v0.3.0`") && e.contains("v0.6.1"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_repository_readme_passes_for_this_version() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let version = cargo_version(&read_text(&root.join("Cargo.toml")).unwrap()).unwrap();
        assert_eq!(refuse_stale_readme(root, &version), Ok(()));
    }

    /// The preflight runs the public CI's clippy lines as ci.yml writes
    /// them, and stops when it finds none.
    #[test]
    fn the_preflight_takes_the_public_ci_clippy_lines_from_ci_yml() {
        let ci = "      - name: a\n        run: cargo +1.2.3 clippy --workspace --locked --all-targets --features atlas/dev -- -D warnings\n        run: cargo build\n        run: cargo +1.2.3 build\n        run: cargo +1.2.3 clippy --workspace --locked -- -D warnings\n";
        let runs = ci_clippy_runs(ci).unwrap();
        assert_eq!(runs.iter().map(|r| r.join(" ")).collect::<Vec<_>>(), ["+1.2.3 clippy --workspace --locked --all-targets --features atlas/dev -- -D warnings", "+1.2.3 clippy --workspace --locked -- -D warnings"]);
        assert!(ci_clippy_runs("run: cargo clippy\n").unwrap_err().contains("no `run: cargo +TOOLCHAIN clippy` line"));
        // The real ci.yml: both of its clippy runs, with -D warnings.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let real = ci_clippy_runs(&read_text(&root.join(".github/workflows/ci.yml")).unwrap()).unwrap();
        assert_eq!(real.len(), 2);
        assert!(real.iter().all(|r| r.ends_with(&["-D".to_string(), "warnings".to_string()])) && real[0].iter().any(|a| a == "--all-targets"));
        assert!(CI_TESTS.contains(&"--workspace") && CI_TESTS.contains(&"ci") && CI_TESTS.contains(&"--locked"));
    }
}
