//! `cargo xtask linux-test`: the public CI's Linux checks on this checkout,
//! inside a Docker Linux container, before a pull request is opened.
//!
//! ```text
//! cargo xtask linux-test [--debug] [--no-clippy] [--image IMAGE] [-- TEST_ARGS...]
//! cargo xtask linux-test --clean [--all]
//! ```
//!
//! The development repository's CI does not run the tests, and a Windows or
//! macOS machine never builds the `cfg(target_os = "linux")` code, so a
//! Linux-only failure used to surface first on the public repository's
//! release pull request. This runs, in the official `rust:<channel>` image
//! (`<channel>` read from `rust-toolchain.toml`, the toolchain the public CI
//! installs) with the pinned clippy and the pinned cargo-nextest added (a
//! local image,
//! `atlas-linux-test:rust-<channel>-clippy-<channel>-nextest-<version>`,
//! built on the first run; nextest is the release's prebuilt binary, its
//! sha256 checked before it is unpacked):
//!
//! 1. the tests as `cargo xtask test-gate` runs them, in release: `cargo
//!    nextest run --workspace --locked --no-fail-fast --test-threads 16
//!    --release`, going on past a failing test so one run reports every
//!    failure (`--debug` drops `--release`; arguments after `--` go to the
//!    tests in libtest's form, a filter for example);
//! 2. `cargo test --workspace --locked --doc --release`, the doc tests,
//!    which nextest does not run (the same arguments after `--`);
//! 3. `cargo +<channel> clippy --workspace --locked --all-targets --features
//!    atlas/dev -- -D warnings` and
//! 4. `cargo +<channel> clippy --workspace --locked -- -D warnings`, the
//!    public CI's `clippy` job (`--no-clippy` skips both).
//!
//! The checkout is mounted read-only at `/src`; nothing in it is written
//! (nextest's store, `target/nextest` under the workspace root whatever
//! `CARGO_TARGET_DIR` says, is moved to the build volume by a tool config
//! the image carries).
//! `CARGO_HOME` is the named volume `atlas-linux-cargo-home`, shared by every
//! checkout: the registry, the git dependencies and Cargo's package-cache
//! lock all live on it, so two runs at once (two checkouts) take turns on
//! the download as two cargo processes on one machine do. The build lives
//! in a named volume per checkout, `atlas-linux-target-<hash of the checkout
//! path>`, so a second run is incremental and the host's `target` folder is
//! never touched; each step's container is named
//! `atlas-linux-test-<the same hash>`. The container gets 4 CPUs and
//! `CARGO_BUILD_JOBS=4`. `--clean` removes this checkout's build volume and
//! any temporary folder a killed run left; `--clean --all` also removes the
//! shared `CARGO_HOME` volume (the image stays: `docker image rm` it).
//!
//! Every step runs even when an earlier one failed, and a step that could
//! not be run or read counts as failed; the run ends with one summary line
//! and exits non-zero when any step failed. Needs a running Docker engine
//! that runs Linux containers (Docker Desktop on Windows and macOS, plain
//! Docker on Linux).

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::verify_macos::{test_line, test_totals};

/// Usage line.
pub const USAGE: &str = "usage: cargo xtask linux-test [--debug] [--no-clippy] [--image IMAGE] [-- TEST_ARGS...]
       cargo xtask linux-test --clean [--all]";

/// The CPUs the container may use and the jobs cargo runs in it.
pub const CPUS: u32 = 4;

/// The named volume holding `CARGO_HOME`, shared by every checkout.
pub const CARGO_HOME_VOLUME: &str = "atlas-linux-cargo-home";

/// Where that volume is mounted, and `CARGO_HOME` in the container.
pub const CARGO_HOME: &str = "/cargo-home";

/// The registry-only volume of the first version, which mounted it at
/// `/usr/local/cargo/registry` without Cargo's lock; `--clean --all`
/// removes it too.
pub const OLD_REGISTRY_VOLUME: &str = "atlas-linux-cargo";

/// How long `docker version` may take to answer.
const DOCKER_VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// The parsed command line.
#[derive(Debug, Default, PartialEq)]
pub struct Args {
    /// Tests in the dev profile instead of release.
    pub debug: bool,
    /// Skip both clippy runs.
    pub no_clippy: bool,
    /// Remove this checkout's build volume and stop.
    pub clean: bool,
    /// With `clean`: the shared `CARGO_HOME` volume too.
    pub all: bool,
    /// The image instead of `rust:<channel>`.
    pub image: Option<String>,
    /// Arguments for the test binaries.
    pub test_args: Vec<String>,
}

impl Args {
    /// Parses the arguments after `linux-test`.
    pub fn parse(args: Vec<String>) -> Result<Args, String> {
        let mut a = Args::default();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--debug" => a.debug = true,
                "--no-clippy" => a.no_clippy = true,
                "--clean" => a.clean = true,
                "--all" => a.all = true,
                "--image" => a.image = Some(it.next().ok_or(USAGE)?),
                "--" => a.test_args.extend(it.by_ref()),
                _ => return Err(USAGE.into()),
            }
        }
        if a.all && !a.clean {
            return Err(USAGE.into());
        }
        if a.clean && (a.debug || a.no_clippy || a.image.is_some() || !a.test_args.is_empty()) {
            return Err(USAGE.into());
        }
        Ok(a)
    }
}

/// The `channel` of a `rust-toolchain.toml`, without quotes or a trailing
/// comment.
pub fn channel(toml: &str) -> Option<String> {
    toml.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        if k.trim() != "channel" {
            return None;
        }
        let v = v.trim();
        let v = match v.strip_prefix('"') {
            Some(rest) => rest.split_once('"')?.0,
            None => v.split('#').next()?,
        };
        let v = v.trim();
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// A channel goes into the Dockerfile's `RUN` line and the image tag, so it
/// must be a plain toolchain name.
pub fn check_channel(c: &str) -> Result<(), String> {
    if !c.is_empty() && c.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) {
        Ok(())
    } else {
        Err(format!("linux-test: the channel {c:?} in rust-toolchain.toml is not [A-Za-z0-9._-]+"))
    }
}

/// An image goes into the Dockerfile's `FROM` line, so it must be a plain
/// image reference.
pub fn check_image(i: &str) -> Result<(), String> {
    let ok_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b':' | b'@');
    if !i.is_empty() && !i.starts_with('-') && i.bytes().all(ok_byte) {
        Ok(())
    } else {
        Err(format!("linux-test: {i:?} is not an image reference"))
    }
}

/// A hash of the checkout's path naming its build volume and container, so
/// two checkouts never share (and keep invalidating) one build. Windows
/// paths are case-blind and take either slash, so there the path is
/// normalised first; elsewhere it is taken as it is.
pub fn checkout_hash(path: &str, windows: bool) -> String {
    let path = if windows { path.replace('\\', "/").to_lowercase() } else { path.to_string() };
    atlas::sha256::hex(path.as_bytes())[..12].to_string()
}

/// This checkout's build volume.
pub fn target_volume(hash: &str) -> String {
    format!("atlas-linux-target-{hash}")
}

/// This checkout's container, one step at a time.
pub fn container_name(hash: &str) -> String {
    format!("atlas-linux-test-{hash}")
}

/// The volumes `--clean` removes (`--all`: the shared ones too).
pub fn clean_volumes(hash: &str, all: bool) -> Vec<String> {
    let mut v = vec![target_volume(hash)];
    if all {
        v.push(CARGO_HOME_VOLUME.into());
        v.push(OLD_REGISTRY_VOLUME.into());
    }
    v
}

/// One step: its name and the cargo arguments.
pub fn steps(a: &Args, channel: &str) -> Vec<(&'static str, Vec<String>)> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let store = ["--tool-config-file".to_string(), format!("atlas-linux-test:{NEXTEST_TOOL_CONFIG}")];
    let mut out: Vec<(&'static str, Vec<String>)> = crate::test_gate::commands(!a.debug, &store, &a.test_args)
        .into_iter()
        .map(|(name, cmd)| (if name == "nextest" { "test" } else { name }, cmd))
        .collect();
    if !a.no_clippy {
        let plus = format!("+{channel}");
        out.push((
            "clippy all-targets dev",
            s(&["cargo", &plus, "clippy", "--workspace", "--locked", "--all-targets", "--features", "atlas/dev", "--", "-D", "warnings"]),
        ));
        out.push(("clippy default", s(&["cargo", &plus, "clippy", "--workspace", "--locked", "--", "-D", "warnings"])));
    }
    out
}

/// The names of the tests a log reports as failed.
pub fn failed_tests(log: &str) -> Vec<String> {
    log.lines().filter_map(test_line).filter(|(_, outcome)| outcome.trim_end().starts_with("FAILED")).map(|(n, _)| n.to_string()).collect()
}

/// One `--mount` value. Docker reads it as a CSV record, so a field holding
/// a comma or a quote is quoted.
pub fn mount(kind: &str, src: &str, dst: &str, readonly: bool) -> String {
    let field = |k: &str, v: &str| {
        let f = format!("{k}={v}");
        if f.contains([',', '"']) {
            format!("\"{}\"", f.replace('"', "\"\""))
        } else {
            f
        }
    };
    let mut m = format!("type={kind},{},{}", field("src", src), field("dst", dst));
    if readonly {
        m.push_str(",readonly");
    }
    m
}

/// The `docker run` arguments for one step.
fn docker_args(root: &Path, image: &str, name: &str, volume: &str, mounts: &[String], cmd: &[String]) -> Vec<String> {
    let mut v: Vec<String> = ["run", "--rm", "--init", "--name", name, "--cpus"].iter().map(|s| s.to_string()).collect();
    v.push(CPUS.to_string());
    let own = [
        mount("bind", &root.to_string_lossy(), "/src", true),
        mount("volume", CARGO_HOME_VOLUME, CARGO_HOME, false),
        mount("volume", volume, "/target", false),
    ];
    for m in own.iter().chain(mounts) {
        v.push("--mount".into());
        v.push(m.clone());
    }
    // The checkout's files belong to another user on a Linux host; git
    // would call the repository dubious and refuse it.
    let env = [
        format!("CARGO_HOME={CARGO_HOME}"),
        "CARGO_TARGET_DIR=/target".to_string(),
        format!("CARGO_BUILD_JOBS={CPUS}"),
        "CARGO_TERM_COLOR=always".into(),
        "GIT_CONFIG_COUNT=1".into(),
        "GIT_CONFIG_KEY_0=safe.directory".into(),
        "GIT_CONFIG_VALUE_0=*".into(),
    ];
    for e in env {
        v.push("-e".into());
        v.push(e);
    }
    v.push("-w".into());
    v.push("/src".into());
    v.push(image.into());
    v.extend(cmd.iter().cloned());
    v
}

/// Copies `r` to `sink` a line at a time, decoded lossily (a test may print
/// bytes that are not UTF-8), and returns the whole log and the read error
/// that stopped it, if one did.
pub fn pump(mut r: impl BufRead, mut sink: impl FnMut(&str)) -> (String, Option<String>) {
    let mut log = String::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) => return (log, None),
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\n', '\r']);
                sink(line);
                log.push_str(line);
                log.push('\n');
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return (log, Some(format!("reading the output: {e}"))),
        }
    }
}

/// What one step came to.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The step's name.
    pub name: &'static str,
    /// Whether it ran and passed.
    pub ok: bool,
    /// The test counts and failed tests, and any error running it.
    pub detail: String,
    /// Its wall time.
    pub secs: u64,
}

/// The detail of a step from its log and the error that kept it from
/// running or being read.
pub fn step_detail(name: &str, log: &str, err: Option<&str>) -> String {
    let mut d = String::new();
    if name == "test" {
        d = crate::test_gate::nextest_detail(log);
    } else if name == "doctest" {
        let ((passed, failed), names) = (test_totals(log), failed_tests(log));
        d = format!("{passed} passed, {failed} failed");
        if !names.is_empty() {
            d.push_str(&format!(": {}", names.join(", ")));
        }
    }
    if let Some(e) = err {
        if !d.is_empty() {
            d.push_str(", ");
        }
        d.push_str(e);
    }
    d
}

/// The run's verdict and its last line.
pub fn summary(steps: &[Step], secs: u64) -> (bool, String) {
    let pass = !steps.is_empty() && steps.iter().all(|s| s.ok);
    let parts: Vec<String> = steps
        .iter()
        .map(|s| {
            let verdict = if s.ok { "ok" } else { "FAILED" };
            if s.detail.is_empty() {
                format!("{} {verdict} {}s", s.name, s.secs)
            } else {
                format!("{} {verdict} ({}) {}s", s.name, s.detail, s.secs)
            }
        })
        .collect();
    (pass, format!("linux-test: {} in {secs}s; {}", if pass { "PASS" } else { "FAIL" }, parts.join("; ")))
}

/// Runs a step, printing its output as it comes. Returns whether it passed,
/// its output, and the error that kept it from running or being read; the
/// child is always waited on, and its container removed when the output
/// could not be read to the end.
fn run_step(args: &[String], name: &str) -> (bool, String, Option<String>) {
    let mut child = match Command::new("docker").args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return (false, String::new(), Some(format!("docker: {e}"))),
    };
    // nextest reports on stderr, libtest on stdout: both are read (stderr
    // on its own thread, so neither pipe fills while the other is read)
    // and the step's log is the two together.
    let errs = child.stderr.take().map(|e| std::thread::spawn(move || pump(BufReader::new(e), |l| eprintln!("{l}"))));
    let (mut log, mut err) = match child.stdout.take() {
        Some(out) => pump(BufReader::new(out), |l| println!("{l}")),
        None => (String::new(), None),
    };
    if err.is_some() {
        // The client may be blocked writing to a pipe nobody reads; the
        // container would outlive it (and the stderr reader with it).
        let _ = Command::new("docker").args(["rm", "-f", name]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    match errs.map(|h| h.join()) {
        Some(Ok((e_log, e_err))) => {
            log.push_str(&e_log);
            if let Some(e) = e_err {
                err.get_or_insert(e);
            }
        }
        Some(Err(_)) => {
            err.get_or_insert("reading docker's output: the stderr reader panicked".into());
        }
        None => {}
    }
    let ok = match child.wait() {
        Ok(s) => s.success(),
        Err(e) => {
            err.get_or_insert(format!("docker: {e}"));
            false
        }
    };
    (ok && err.is_none(), log, err)
}

/// The cargo-nextest the image installs, pinned.
pub const NEXTEST_VERSION: &str = "0.9.118";

/// The sha256 of each Linux build of that release's prebuilt archive
/// (`cargo-nextest-<version>-<arch>-unknown-linux-gnu.tar.gz`), as the
/// release publishes it beside the archive; the image build refuses an
/// archive that hashes otherwise.
pub const NEXTEST_SHA256: [(&str, &str); 2] = [
    ("x86_64", "1e60aac5de62391d2419bb6c5bfa467f4b27586b3f1b3ec07aa79f4632583450"),
    ("aarch64", "1b89b3cbac2e27095168f8edaa5165a344a83743b42dfb90a95b8896187807e8"),
];

/// A nextest tool config the image carries, given to the test step with
/// `--tool-config-file`. nextest writes its store under the workspace root
/// (`target/nextest`), not under `CARGO_TARGET_DIR`, and the workspace root
/// is the read-only checkout; this moves the store to the build volume.
pub const NEXTEST_TOOL_CONFIG: &str = "/usr/local/share/atlas-linux-test/nextest.toml";

/// The revision of [`dockerfile`], in the image's tag: an image built
/// before a change to what the image carries (2: the
/// [`NEXTEST_TOOL_CONFIG`]) is not reused.
pub const IMAGE_REVISION: u32 = 2;

/// The local image the steps run in: `base` with the pinned clippy added,
/// as the public CI's clippy job installs it (the official `rust` images
/// carry no clippy), the pinned cargo-nextest and its tool config. Named
/// after all three and [`IMAGE_REVISION`], so another channel or nextest on
/// the same base is another image. Built
/// once and then reused from Docker's image store.
pub fn image_tag(base: &str, channel: &str) -> String {
    let safe: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '-' }).collect();
    format!("atlas-linux-test:{safe}-clippy-{channel}-nextest-{NEXTEST_VERSION}-r{IMAGE_REVISION}")
}

/// The Dockerfile of that image. nextest is the release's prebuilt binary
/// for the image's architecture, checked against [`NEXTEST_SHA256`] before
/// it is unpacked into `/usr/local/bin` (`CARGO_HOME` is a volume at run
/// time, so its `bin` would not hold it). The tool config sets nextest's
/// store to `/target/nextest`, on the build volume.
pub fn dockerfile(base: &str, channel: &str) -> String {
    let v = NEXTEST_VERSION;
    let cases: String = NEXTEST_SHA256.iter().map(|(arch, sum)| format!("{arch}) sum={sum} ;; ")).collect();
    format!(
        "FROM {base}\n\
         RUN rustup toolchain install {channel} --profile minimal -c clippy\n\
         RUN set -eu; arch=\"$(uname -m)\"; case \"$arch\" in {cases}*) echo \"no cargo-nextest build for $arch\" >&2; exit 1 ;; esac; \
         f=cargo-nextest-{v}-$arch-unknown-linux-gnu.tar.gz; \
         curl -fsSL -o /tmp/$f https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-{v}/$f; \
         echo \"$sum  /tmp/$f\" | sha256sum -c -; tar -xzf /tmp/$f -C /usr/local/bin; rm /tmp/$f; cargo-nextest nextest --version\n\
         RUN mkdir -p {dir} && \
         printf '[store]\\ndir = \"/target/nextest\"\\n' > {NEXTEST_TOOL_CONFIG}\n",
        dir = NEXTEST_TOOL_CONFIG.rsplit_once('/').map_or("/", |(d, _)| d)
    )
}

fn ensure_image(base: &str, channel: &str) -> Result<String, String> {
    check_image(base)?;
    check_channel(channel)?;
    let tag = image_tag(base, channel);
    let have =
        Command::new("docker").args(["image", "inspect", &tag]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    if have.is_ok_and(|s| s.success()) {
        return Ok(tag);
    }
    println!("linux-test: building {tag} from {base}");
    let mut child =
        Command::new("docker").args(["build", "-q", "-t", &tag, "-"]).stdin(Stdio::piped()).spawn().map_err(|e| format!("docker: {e}"))?;
    // Write and close stdin, then wait whatever the write did.
    let wrote = child.stdin.take().map(|mut stdin| stdin.write_all(dockerfile(base, channel).as_bytes()));
    let s = child.wait().map_err(|e| format!("docker: {e}"))?;
    if let Some(Err(e)) = wrote {
        return Err(format!("docker build: {e}"));
    }
    if !s.success() {
        return Err(format!("linux-test: could not build {tag} from {base}"));
    }
    Ok(tag)
}

/// Where a linked worktree's git directory sits inside its repository's
/// common directory (`worktrees/NAME`), or `None` for an ordinary checkout,
/// whose `.git` folder is inside the checkout and mounted with it.
pub fn worktree_git_dir(root: &Path, git_dir: &Path, common: &Path) -> Option<PathBuf> {
    if git_dir.starts_with(root) {
        return None;
    }
    git_dir.strip_prefix(common).ok().map(Path::to_path_buf)
}

/// The `.git` file naming a worktree's git directory under `/git-common`.
pub fn gitfile(rel: &Path) -> String {
    format!("gitdir: /git-common/{}\n", rel.to_string_lossy().replace('\\', "/"))
}

/// A private folder for one run's files, removed when dropped (and by
/// `--clean` when a killed run left it).
struct RunDir(PathBuf);

impl RunDir {
    fn create(prefix: &str) -> Result<RunDir, String> {
        let base = std::env::temp_dir();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let mut last = String::new();
        for n in 0..16 {
            let p = base.join(format!("{prefix}{}-{nanos}-{n}", std::process::id()));
            #[cfg(unix)]
            let b = {
                use std::os::unix::fs::DirBuilderExt;
                let mut b = std::fs::DirBuilder::new();
                b.mode(0o700);
                b
            };
            #[cfg(not(unix))]
            let b = std::fs::DirBuilder::new();
            match b.create(&p) {
                Ok(()) => return Ok(RunDir(p)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => last = format!("{}: {e}", p.display()),
                Err(e) => return Err(format!("{}: {e}", p.display())),
            }
        }
        Err(last)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The prefix of this checkout's run folders in the temp folder.
fn run_dir_prefix(hash: &str) -> String {
    format!("{}-", container_name(hash))
}

/// The mounts that give the container a working git repository at `/src`.
/// A linked worktree's `.git` file names its git directory by a host path
/// the container cannot reach, and git refuses to run anywhere below a
/// broken `.git` file (in the tests' temporary repositories too, when the
/// test runs from the checkout). So the repository's common directory is
/// mounted read-only at `/git-common`, and a `.git` file naming the
/// worktree's directory under it, written to a private folder for this
/// run, is mounted over the checkout's own.
fn git_mounts(root: &Path, hash: &str) -> Result<(Vec<String>, Option<RunDir>), String> {
    let o = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !o.status.success() {
        // Not a git checkout (an export, say): nothing to mount.
        return Ok((Vec::new(), None));
    }
    let out = String::from_utf8_lossy(&o.stdout);
    let mut lines = out.lines().map(|l| strip_verbatim(PathBuf::from(l.trim())));
    let (Some(git_dir), Some(common)) = (lines.next(), lines.next()) else { return Ok((Vec::new(), None)) };
    let canon = |p: &Path| std::fs::canonicalize(p).map(strip_verbatim).unwrap_or_else(|_| p.to_path_buf());
    let (git_dir, common) = (canon(&git_dir), canon(&common));
    let Some(rel) = worktree_git_dir(root, &git_dir, &common) else { return Ok((Vec::new(), None)) };
    let dir = RunDir::create(&run_dir_prefix(hash))?;
    let file = dir.0.join("gitfile");
    std::fs::write(&file, gitfile(&rel)).map_err(|e| format!("{}: {e}", file.display()))?;
    let mounts =
        vec![mount("bind", &common.to_string_lossy(), "/git-common", true), mount("bind", &file.to_string_lossy(), "/src/.git", true)];
    Ok((mounts, Some(dir)))
}

fn root() -> PathBuf {
    let r = crate::release::repo_root();
    std::fs::canonicalize(&r).map(strip_verbatim).unwrap_or(r)
}

/// `\\?\H:\x` to `H:\x`: Docker does not take the verbatim form. A
/// verbatim UNC path is left as it is (Docker cannot mount a share).
pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(s) if !s.starts_with("UNC\\") => PathBuf::from(s),
        _ => p,
    }
}

/// A command's output, or an error when it has not finished within `limit`
/// (it is then killed).
fn output_within(cmd: &mut Command, limit: Duration) -> Result<Output, String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|e| e.to_string()),
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer in {}s", limit.as_secs()));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn clean(hash: &str, all: bool) -> Result<(), String> {
    let vols = clean_volumes(hash, all);
    let s = Command::new("docker").args(["volume", "rm", "-f"]).args(&vols).status().map_err(|e| format!("docker: {e}"))?;
    if !s.success() {
        return Err(format!("linux-test: could not remove {} (is a run still using it?)", vols.join(", ")));
    }
    println!("linux-test: removed {}", vols.join(", "));
    let prefix = run_dir_prefix(hash);
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_dir_all(e.path());
                println!("linux-test: removed {}", e.path().display());
            }
        }
    }
    Ok(())
}

/// Runs the command.
pub fn run(args: Vec<String>) -> Result<(), String> {
    let a = Args::parse(args)?;
    let root = root();
    let hash = checkout_hash(&root.to_string_lossy(), cfg!(windows));
    let volume = target_volume(&hash);
    let name = container_name(&hash);
    match output_within(Command::new("docker").args(["version", "--format", "{{.Server.Os}}"]), DOCKER_VERSION_TIMEOUT) {
        Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "linux" => {}
        Ok(o) if o.status.success() => return Err("linux-test: the Docker engine does not run Linux containers".into()),
        Ok(_) => return Err("linux-test: no Docker engine answered `docker version`; start Docker and run it again".into()),
        Err(e) => return Err(format!("linux-test: `docker version`: {e}; start Docker and run it again")),
    }
    if a.clean {
        return clean(&hash, a.all);
    }
    let toml = std::fs::read_to_string(root.join("rust-toolchain.toml")).map_err(|e| format!("rust-toolchain.toml: {e}"))?;
    let channel = channel(&toml).ok_or("rust-toolchain.toml names no channel")?;
    check_channel(&channel)?;
    let base = a.image.clone().unwrap_or_else(|| format!("rust:{channel}"));
    let image = ensure_image(&base, &channel)?;
    // Dropped at the end of the run, removing the run's folder.
    let (mounts, _run_dir) = git_mounts(&root, &hash)?;
    println!("linux-test: {} in {image} (from {base}), {CPUS} CPUs, build volume {volume}", root.display());
    let start = Instant::now();
    let mut results = Vec::new();
    for (step, cmd) in steps(&a, &channel) {
        println!("linux-test: == {step}: {}", cmd.join(" "));
        let t = Instant::now();
        let (ok, log, err) = run_step(&docker_args(&root, &image, &name, &volume, &mounts, &cmd), &name);
        results.push(Step { name: step, ok, detail: step_detail(step, &log, err.as_deref()), secs: t.elapsed().as_secs() });
    }
    let (pass, line) = summary(&results, start.elapsed().as_secs());
    println!("{line}");
    if pass {
        Ok(())
    } else {
        Err("linux-test failed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_flags_and_test_args() {
        let a = Args::parse(v(&["--debug", "--image", "rust:x", "--", "some_test", "--exact"])).unwrap();
        assert!(a.debug && !a.no_clippy && !a.clean && !a.all);
        assert_eq!(a.image.as_deref(), Some("rust:x"));
        assert_eq!(a.test_args, v(&["some_test", "--exact"]));
        assert!(Args::parse(v(&["--bogus"])).is_err());
        assert!(Args::parse(v(&["--release"])).is_err(), "release is the default; --debug opts out");
        assert!(Args::parse(v(&["--image"])).is_err());
        assert!(Args::parse(v(&["--clean", "--debug"])).is_err());
        assert!(Args::parse(v(&["--all"])).is_err(), "--all only goes with --clean");
        assert!(Args::parse(v(&["--clean"])).unwrap().clean);
        let a = Args::parse(v(&["--clean", "--all"])).unwrap();
        assert!(a.clean && a.all);
    }

    #[test]
    fn reads_the_channel() {
        assert_eq!(channel("[toolchain]\nchannel = \"1.99.0\"\n").as_deref(), Some("1.99.0"));
        assert_eq!(channel("[toolchain]\ncomponents = [\"clippy\"]\n"), None);
        assert_eq!(channel("[toolchain]\nchannel = \"1.99.0\" # the public CI's\n").as_deref(), Some("1.99.0"));
        assert_eq!(channel("[toolchain]\nchannel = 1.99.0 # unquoted\n").as_deref(), Some("1.99.0"));
        assert_eq!(channel("# channel = \"nightly\"\n[toolchain]\nchannel = \"stable\"\n").as_deref(), Some("stable"));
    }

    #[test]
    fn only_a_plain_channel_and_image_reach_the_dockerfile() {
        for ok in ["1.99.0", "stable", "nightly-2026-01-01", "1.99.0-x86_64-unknown-linux-gnu"] {
            assert!(check_channel(ok).is_ok(), "{ok}");
        }
        for bad in ["", "1.99.0 && curl evil | sh", "1.99.0\nRUN x", "a;b", "$(x)", "a/b"] {
            assert!(check_channel(bad).is_err(), "{bad:?}");
        }
        for ok in ["rust:1.99.0", "ghcr.io/x/y:1", "rust@sha256:abc123"] {
            assert!(check_image(ok).is_ok(), "{ok}");
        }
        for bad in ["", "rust:1\nRUN x", "rust 1", "--privileged"] {
            assert!(check_image(bad).is_err(), "{bad:?}");
        }
        let d = dockerfile("rust:1.99.0", "1.99.0");
        assert!(d.starts_with("FROM rust:1.99.0\nRUN rustup toolchain install 1.99.0 --profile minimal -c clippy\nRUN "), "{d}");
        assert_eq!(d.lines().count(), 4, "{d}");
        assert!(d.ends_with('\n'));
    }

    #[test]
    fn the_image_installs_the_pinned_nextest_and_checks_its_sha256() {
        let d = dockerfile("rust:1", "1.99.0");
        let run = d.lines().nth(2).unwrap();
        assert!(run.starts_with("RUN set -eu; "), "{run}");
        assert!(run.contains("https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.118/$f"), "{run}");
        assert!(run.contains("f=cargo-nextest-0.9.118-$arch-unknown-linux-gnu.tar.gz"), "{run}");
        for (arch, sum) in NEXTEST_SHA256 {
            assert_eq!(sum.len(), 64);
            assert!(run.contains(&format!("{arch}) sum={sum} ;;")), "{run}");
        }
        // The check comes before the unpack, and an unknown arch stops the build.
        let (check, unpack) = (run.find("sha256sum -c -").unwrap(), run.find("tar -xzf").unwrap());
        assert!(check < unpack, "{run}");
        assert!(run.contains("*) echo \"no cargo-nextest build for $arch\" >&2; exit 1 ;;"), "{run}");
    }

    #[test]
    fn nextest_keeps_its_store_off_the_read_only_checkout() {
        // nextest's store dir is `target/nextest` under the workspace root,
        // whatever CARGO_TARGET_DIR says: in the container that is
        // /src/target/nextest, on the read-only checkout, and the test step
        // failed before it ran a test. The image carries a tool config that
        // moves it to the writable build volume, and the step names it.
        let d = dockerfile("rust:1", "1.99.0");
        let run = d.lines().nth(3).unwrap();
        assert_eq!(
            run,
            "RUN mkdir -p /usr/local/share/atlas-linux-test && \
             printf '[store]\\ndir = \"/target/nextest\"\\n' > /usr/local/share/atlas-linux-test/nextest.toml"
        );
        assert!(NEXTEST_TOOL_CONFIG.starts_with("/usr/local/share/atlas-linux-test/"));
        let s = steps(&Args::default(), "1.99.0");
        let i = s[0].1.iter().position(|x| x == "--tool-config-file").unwrap();
        assert_eq!(s[0].1[i + 1], format!("atlas-linux-test:{NEXTEST_TOOL_CONFIG}"));
        // The doc tests and clippy are cargo's, which take no such flag.
        assert!(s[1..].iter().all(|(_, c)| !c.iter().any(|x| x.contains("tool-config"))));
    }

    #[test]
    fn steps_match_the_public_ci_in_release_and_never_stop_early() {
        let a = Args::default();
        let s = steps(&a, "1.99.0");
        assert_eq!(s.len(), 4);
        assert_eq!(s.iter().map(|x| x.0).collect::<Vec<_>>(), ["test", "doctest", "clippy all-targets dev", "clippy default"]);
        assert_eq!(
            s[0].1.join(" "),
            "cargo nextest run --workspace --locked --no-fail-fast --test-threads 16 --release \
             --tool-config-file atlas-linux-test:/usr/local/share/atlas-linux-test/nextest.toml"
        );
        assert_eq!(s[1].1.join(" "), "cargo test --workspace --locked --doc --release");
        assert_eq!(s[2].1.join(" "), "cargo +1.99.0 clippy --workspace --locked --all-targets --features atlas/dev -- -D warnings");
        assert_eq!(s[3].1.join(" "), "cargo +1.99.0 clippy --workspace --locked -- -D warnings");
        let a = Args { debug: true, no_clippy: true, test_args: v(&["x"]), ..Args::default() };
        let s = steps(&a, "1.99.0");
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0].1.join(" "),
            "cargo nextest run --workspace --locked --no-fail-fast --test-threads 16 \
             --tool-config-file atlas-linux-test:/usr/local/share/atlas-linux-test/nextest.toml -- x"
        );
        assert_eq!(s[1].1.join(" "), "cargo test --workspace --locked --doc -- x");
    }

    #[test]
    fn hash_is_per_checkout_and_case_blind_only_on_windows() {
        let a = checkout_hash(r"H:\work\one", true);
        assert_eq!(a, checkout_hash("h:/work/one", true));
        assert_ne!(a, checkout_hash(r"H:\work\two", true));
        assert_eq!(a.len(), 12);
        assert_ne!(checkout_hash("/home/u/Atlas", false), checkout_hash("/home/u/atlas", false));
        assert_ne!(checkout_hash(r"/home/u/a\b", false), checkout_hash("/home/u/a/b", false));
        assert_eq!(target_volume(&a), format!("atlas-linux-target-{a}"));
        assert_eq!(container_name(&a), format!("atlas-linux-test-{a}"));
    }

    #[test]
    fn clean_all_removes_the_shared_cargo_home() {
        assert_eq!(clean_volumes("abc", false), v(&["atlas-linux-target-abc"]));
        assert_eq!(clean_volumes("abc", true), v(&["atlas-linux-target-abc", "atlas-linux-cargo-home", "atlas-linux-cargo"]));
    }

    #[test]
    fn image_tag_is_a_valid_tag_and_names_the_channel() {
        assert_eq!(image_tag("rust:1.99.0", "1.99.0"), "atlas-linux-test:rust-1.99.0-clippy-1.99.0-nextest-0.9.118-r2");
        assert_eq!(image_tag("ghcr.io/x/y:1", "nightly"), "atlas-linux-test:ghcr.io-x-y-1-clippy-nightly-nextest-0.9.118-r2");
        assert_ne!(image_tag("rust:1", "1.99.0"), image_tag("rust:1", "1.98.0"));
    }

    #[test]
    fn only_a_linked_worktree_needs_its_git_dir_mounted() {
        let root = Path::new("/w/tree");
        assert_eq!(worktree_git_dir(root, Path::new("/w/tree/.git"), Path::new("/w/tree/.git")), None);
        assert_eq!(
            worktree_git_dir(root, Path::new("/r/.git/worktrees/tree"), Path::new("/r/.git")),
            Some(PathBuf::from("worktrees/tree"))
        );
    }

    #[test]
    fn gitfile_names_the_worktree_under_git_common() {
        assert_eq!(gitfile(Path::new("worktrees/tree")), "gitdir: /git-common/worktrees/tree\n");
        assert_eq!(gitfile(Path::new(r"worktrees\tree")), "gitdir: /git-common/worktrees/tree\n");
    }

    #[test]
    fn strips_only_a_local_verbatim_prefix() {
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\D:\src\atlas")), PathBuf::from(r"D:\src\atlas"));
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\UNC\server\share\x")), PathBuf::from(r"\\?\UNC\server\share\x"));
        assert_eq!(strip_verbatim(PathBuf::from(r"D:\src\atlas")), PathBuf::from(r"D:\src\atlas"));
        assert_eq!(strip_verbatim(PathBuf::from("/home/u/atlas")), PathBuf::from("/home/u/atlas"));
    }

    #[test]
    fn mount_strings_quote_what_docker_would_split() {
        assert_eq!(mount("bind", "/w", "/src", true), "type=bind,src=/w,dst=/src,readonly");
        assert_eq!(mount("volume", "vol", "/target", false), "type=volume,src=vol,dst=/target");
        assert_eq!(mount("bind", r"C:\My Work\atlas", "/src", true), r"type=bind,src=C:\My Work\atlas,dst=/src,readonly");
        assert_eq!(mount("bind", "/a,b", "/src", false), "type=bind,\"src=/a,b\",dst=/src");
        assert_eq!(mount("bind", "/a\"b", "/src", false), "type=bind,\"src=/a\"\"b\",dst=/src");
    }

    #[test]
    fn finds_failed_tests() {
        let log = "test a::b ... ok\ntest c::d ... FAILED\ntest e ... ignored\nfailures:\n";
        assert_eq!(failed_tests(log), v(&["c::d"]));
    }

    #[test]
    fn docker_args_mount_read_only_share_cargo_home_and_cap_cpus() {
        let d = docker_args(
            Path::new("/w"),
            "rust:1",
            "atlas-linux-test-abc",
            "vol",
            &v(&["type=bind,src=/g,dst=/git-common,readonly"]),
            &v(&["cargo", "nextest", "run"]),
        );
        let j = d.join(" ");
        assert!(j.starts_with("run --rm --init --name atlas-linux-test-abc --cpus 4 "), "{j}");
        assert!(j.contains("--mount type=bind,src=/w,dst=/src,readonly"));
        assert!(j.contains("--mount type=volume,src=atlas-linux-cargo-home,dst=/cargo-home "));
        assert!(j.contains("-e CARGO_HOME=/cargo-home"));
        assert!(j.contains("--mount type=volume,src=vol,dst=/target "));
        assert!(j.contains("--mount type=bind,src=/g,dst=/git-common,readonly"));
        assert!(j.contains("-e CARGO_BUILD_JOBS=4"));
        assert!(!j.contains(" -v "), "--mount only");
        assert!(j.ends_with("rust:1 cargo nextest run"));
    }

    #[test]
    fn docker_args_keep_a_windows_path_with_spaces_in_one_argument() {
        let d = docker_args(Path::new(r"C:\My Work\atlas"), "rust:1", "n", "vol", &[], &v(&["cargo", "nextest", "run"]));
        let i = d.iter().position(|x| x.contains("dst=/src")).unwrap();
        assert_eq!(d[i], r"type=bind,src=C:\My Work\atlas,dst=/src,readonly");
        assert_eq!(d[i - 1], "--mount");
    }

    #[test]
    fn pump_reads_bytes_that_are_not_utf8_to_the_end() {
        let input: &[u8] = b"test a ... ok\nbad \xff\xfe bytes\r\ntest b ... FAILED\nno newline";
        let mut seen = Vec::new();
        let (log, err) = pump(input, |l| seen.push(l.to_string()));
        assert_eq!(err, None);
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[1], "bad \u{fffd}\u{fffd} bytes");
        assert_eq!(seen[3], "no newline");
        assert_eq!(failed_tests(&log), v(&["b"]));
    }

    #[test]
    fn pump_keeps_what_it_read_before_an_error() {
        struct Broken(bool);
        impl std::io::Read for Broken {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    return Err(std::io::Error::other("pipe broke"));
                }
                self.0 = true;
                buf[..6].copy_from_slice(b"line1\n");
                Ok(6)
            }
        }
        let (log, err) = pump(BufReader::new(Broken(false)), |_| {});
        assert_eq!(log, "line1\n");
        assert!(err.unwrap().contains("pipe broke"));
    }

    #[test]
    fn step_detail_counts_names_failures_and_carries_an_error() {
        let log = "test a ... ok\ntest b ... FAILED\ntest result: FAILED. 1 passed; 1 failed; 0 ignored\n";
        // The test step is nextest's: a libtest log holds no summary of its.
        assert_eq!(step_detail("test", log, None), "0 passed, 0 failed");
        assert_eq!(step_detail("clippy default", "", None), "");
        assert_eq!(step_detail("clippy default", "", Some("docker: not found")), "docker: not found");
        assert_eq!(step_detail("doctest", log, None), "1 passed, 1 failed: b");
        let nx = "        FAIL [   0.004s] atlas::hooks a\n     Summary [   1.000s] 3 tests run: 2 passed, 1 failed, 0 skipped\n";
        assert_eq!(step_detail("test", nx, None), "2 passed, 1 failed: atlas::hooks a");
        // A pass on a retry is shown as one.
        let nx = "  TRY 1 FAIL [   0.3s] (───) atlas::orchestrate admit\n  TRY 2 PASS [   0.2s] (2/2) atlas::orchestrate admit\n     Summary [   1.000s] 2 tests run: 2 passed (1 flaky), 0 failed, 0 skipped\n   FLAKY 2/2 [   0.2s] (2/2) atlas::orchestrate admit\n";
        assert_eq!(step_detail("test", nx, None), "2 passed (1 flaky: atlas::orchestrate admit), 0 failed");
        assert_eq!(step_detail("test", "", Some("reading docker's output: x")), "0 passed, 0 failed, reading docker's output: x");
    }

    #[test]
    fn summary_passes_only_when_every_step_passed() {
        let st = |name, ok, detail: &str, secs| Step { name, ok, detail: detail.into(), secs };
        let pass =
            [st("test", true, "1391 passed, 0 failed", 578), st("clippy all-targets dev", true, "", 3), st("clippy default", true, "", 0)];
        assert_eq!(
            summary(&pass, 582),
            (
                true,
                "linux-test: PASS in 582s; test ok (1391 passed, 0 failed) 578s; clippy all-targets dev ok 3s; clippy default ok 0s".into()
            )
        );
        let fail = [st("test", false, "0 passed, 0 failed, docker: not found", 0), st("clippy default", true, "", 1)];
        assert_eq!(
            summary(&fail, 1),
            (false, "linux-test: FAIL in 1s; test FAILED (0 passed, 0 failed, docker: not found) 0s; clippy default ok 1s".into())
        );
        assert!(!summary(&[], 0).0, "no step is no pass");
    }
}
