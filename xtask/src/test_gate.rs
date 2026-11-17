//! `cargo xtask test-gate`: the test gate before a pull request.
//!
//! ```text
//! cargo xtask test-gate [-- NEXTEST_ARGS...]
//! ```
//!
//! Runs the workspace's tests the fast way, in two steps:
//!
//! 1. `cargo nextest run --workspace --locked --no-fail-fast --test-threads
//!    16`, every test in its own process, 16 at a time, going on past a
//!    failure so one run names every one (arguments after `--` are added to
//!    it: a filter such as `-E 'test(name)'`, or `--no-capture`);
//! 2. `cargo test --workspace --locked --doc`, the doc tests, which nextest
//!    does not run.
//!
//! The test count is capped at 16 rather than one per core: the suite starts
//! many servers and processes of its own, and `.config/nextest.toml` gives
//! the tests that time themselves the whole run to themselves. Builds take
//! `CARGO_BUILD_JOBS` (4 when unset, never above 16).
//!
//! It needs cargo-nextest and fails at once, with how to install it, when it
//! is missing: it never falls back to `cargo test`, whose single process per
//! binary is the slow path this replaces. Both steps run even when the first
//! failed; the run ends with one summary line and exits non-zero when either
//! failed. `cargo xtask linux-test` runs the same two steps in a Linux
//! container.
//!
//! nextest writes its store (`target/nextest`) under the workspace root
//! whatever `CARGO_TARGET_DIR` says, which would leave a `target` folder in
//! a checkout that builds elsewhere. The gate writes a nextest tool config
//! into the target dir that moves the store to `nextest` there, and passes
//! it with `--tool-config-file` (linux-test has its own, in its image).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Usage line.
pub const USAGE: &str = "usage: cargo xtask test-gate [-- NEXTEST_ARGS...]";

/// Tests run at once.
pub const TEST_THREADS: u32 = 16;

/// Build jobs when `CARGO_BUILD_JOBS` is unset.
pub const DEFAULT_JOBS: u32 = 4;

/// The most build jobs the gate lets cargo run.
pub const MAX_JOBS: u32 = 16;

/// What a run without cargo-nextest prints, on one line.
pub const INSTALL: &str =
    "test-gate: cargo-nextest is not installed; install it with `cargo install --locked cargo-nextest` (or a prebuilt binary from https://nexte.st) and run it again";

/// The tool name the gate's nextest tool config goes by.
pub const TOOL: &str = "atlas-test-gate";

/// The gate's nextest tool config's file name, in the target dir.
pub const TOOL_CONFIG: &str = "atlas-test-gate-nextest.toml";

/// The target dir cargo builds in: `CARGO_TARGET_DIR`'s value (relative to
/// the workspace root, where the gate runs cargo) or `target` under the root
/// when it is unset or empty.
pub fn target_dir(env: Option<&str>, root: &Path) -> PathBuf {
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => root.join(v),
        None => root.join("target"),
    }
}

/// The tool config that moves nextest's store to `nextest` in `target`: a
/// TOML literal string, so a Windows path's backslashes stay as they are.
pub fn store_config(target: &Path) -> Result<String, String> {
    let dir = target.join("nextest");
    let dir = dir.to_str().ok_or_else(|| format!("test-gate: the target dir {} is not UTF-8", target.display()))?;
    if dir.contains(['\'', '\n', '\r']) {
        return Err(format!("test-gate: the target dir {dir:?} cannot be written as a TOML literal string"));
    }
    Ok(format!("[store]\ndir = '{dir}'\n"))
}

/// nextest's arguments naming the tool config at `config` (an absolute path).
pub fn store_args(config: &Path) -> Vec<String> {
    vec!["--tool-config-file".into(), format!("{TOOL}:{}", config.display())]
}

/// The arguments after `test-gate`: only `-- NEXTEST_ARGS...`, which are
/// returned.
pub fn parse(args: Vec<String>) -> Result<Vec<String>, String> {
    let mut it = args.into_iter();
    match it.next().as_deref() {
        None => Ok(Vec::new()),
        Some("--") => Ok(it.collect()),
        Some(_) => Err(USAGE.into()),
    }
}

/// The build jobs from `CARGO_BUILD_JOBS`'s value: [`DEFAULT_JOBS`] when
/// unset or empty, at most [`MAX_JOBS`].
pub fn jobs(env: Option<&str>) -> Result<u32, String> {
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(DEFAULT_JOBS),
        Some(v) => match v.parse::<u32>() {
            Ok(n) if n > 0 => Ok(n.min(MAX_JOBS)),
            _ => Err(format!("test-gate: CARGO_BUILD_JOBS={v:?} is not a positive whole number")),
        },
    }
}

/// The two steps, named: nextest (with `nextest_args` after its own) and
/// the doc tests. `release` builds in the release profile; `test_args`, in
/// libtest's form, go after `--` to both (linux-test's `-- TEST_ARGS`).
pub fn commands(release: bool, nextest_args: &[String], test_args: &[String]) -> Vec<(&'static str, Vec<String>)> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let threads = TEST_THREADS.to_string();
    let mut nextest = s(&["cargo", "nextest", "run", "--workspace", "--locked", "--no-fail-fast", "--test-threads", &threads]);
    let mut doc = s(&["cargo", "test", "--workspace", "--locked", "--doc"]);
    for c in [&mut nextest, &mut doc] {
        if release {
            c.push("--release".into());
        }
    }
    nextest.extend(nextest_args.iter().cloned());
    if !test_args.is_empty() {
        for c in [&mut nextest, &mut doc] {
            c.push("--".into());
            c.extend(test_args.iter().cloned());
        }
    }
    vec![("nextest", nextest), ("doctest", doc)]
}

/// A line without its ANSI colour sequences.
fn plain(l: &str) -> String {
    let mut out = String::with_capacity(l.len());
    let mut it = l.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The passed and failed counts of nextest's `Summary [..] N tests run: P
/// passed (K flaky), F failed (L due to being leaky), T timed out, ...`
/// line ((0, 0) when there is none). Failed counts every test that did not
/// pass: failed (leak-failed among them), timed out, or not run at all
/// (`exec failed`). A flaky test passed on a retry: it is in P, and
/// [`nextest_flaky`] names it.
pub fn nextest_totals(log: &str) -> (u64, u64) {
    let mut out = (0, 0);
    for l in log.lines().map(plain) {
        let Some(rest) = l.trim_start().strip_prefix("Summary [") else { continue };
        let Some((_, counts)) = rest.split_once(" run: ") else { continue };
        let count = |word: &str| -> u64 {
            let n = |c: &str| c.split_once(' ').filter(|(_, w)| w.starts_with(word)).and_then(|(n, _)| n.parse::<u64>().ok());
            counts.split(", ").filter_map(n).sum()
        };
        out = (count("passed"), count("failed") + count("timed out") + count("exec failed"));
    }
    out
}

/// A nextest status line, `STATUS [   0.1s] (3/1391) BINARY TEST`: its
/// status and the test's name (the counter, when there is one, dropped).
fn status_line(l: &str) -> Option<(&str, &str)> {
    let (status, rest) = l.trim_start().split_once(" [")?;
    let (_, name) = rest.split_once("] ")?;
    let name = match name.strip_prefix('(').and_then(|n| n.split_once(") ")) {
        Some((_, n)) => n,
        None => name,
    };
    let name = name.trim();
    (!name.is_empty()).then_some((status, name))
}

/// A status's verdict word and its try: `FAIL` is (`FAIL`, None), `TRY 2
/// FAIL` is (`FAIL`, Some(2)). Anything else (a line of a test's own
/// output) is `None`.
fn verdict(status: &str) -> Option<(&str, Option<u32>)> {
    let ok = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_uppercase() || c == '-');
    match status.split(' ').collect::<Vec<_>>()[..] {
        [w] if ok(w) => Some((w, None)),
        ["TRY", n, w] if ok(w) => Some((w, Some(n.parse().ok()?))),
        _ => None,
    }
}

/// The tests nextest passed only on a retry (`FLAKY 2/2 [..] NAME` in its
/// summary, `TRY 2 PASS [..] NAME` as it runs), each once, in order.
pub fn nextest_flaky(log: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in log.lines().map(plain) {
        let Some((status, name)) = status_line(&l) else { continue };
        let flaky = status.starts_with("FLAKY ") || matches!(verdict(status), Some(("PASS", Some(n))) if n > 1);
        if flaky && !out.iter().any(|x| x == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// The tests nextest reports as failed (`FAIL [..] BINARY TEST`, a leak
/// failure, timed out, or killed by a signal, on the last try when it was
/// retried), each once, in order. A test that failed a try and then passed
/// is flaky, not failed.
pub fn nextest_failed(log: &str) -> Vec<String> {
    let flaky = nextest_flaky(log);
    let mut out: Vec<String> = Vec::new();
    for l in log.lines().map(plain) {
        let Some((status, name)) = status_line(&l) else { continue };
        let Some((w, _)) = verdict(status) else { continue };
        if !matches!(w, "FAIL" | "LEAK-FAIL" | "TIMEOUT" | "SIGSEGV" | "SIGABRT" | "SIGKILL" | "ABORT") {
            continue;
        }
        if !flaky.iter().any(|x| x == name) && !out.iter().any(|x| x == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// What a nextest run came to, for a gate's last line: `P passed, F
/// failed`, with the tests that passed only on a retry (`P passed (1
/// flaky: NAME)`) and those that failed (`: NAME, ...`) named.
pub fn nextest_detail(log: &str) -> String {
    let (passed, failed) = nextest_totals(log);
    let flaky = nextest_flaky(log);
    let names = nextest_failed(log);
    let mut d = format!("{passed} passed");
    if !flaky.is_empty() {
        d.push_str(&format!(" ({} flaky: {})", flaky.len(), flaky.join(", ")));
    }
    d.push_str(&format!(", {failed} failed"));
    if !names.is_empty() {
        d.push_str(&format!(": {}", names.join(", ")));
    }
    d
}

/// Whether `cargo nextest` runs.
fn have_nextest() -> bool {
    Command::new("cargo")
        .args(["nextest", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs `c`, printing its stdout and stderr as they come. Returns whether
/// it passed, the two outputs together, and the error that kept it from
/// running or being read to the end; the child is always waited on.
fn run_read(mut c: Command) -> (bool, String, Option<String>) {
    use std::io::BufReader;
    let mut child = match c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(ch) => ch,
        Err(e) => return (false, String::new(), Some(format!("cargo: {e}"))),
    };
    // stderr on its own thread, so neither pipe fills while the other is read.
    let errs = child.stderr.take().map(|e| std::thread::spawn(move || crate::linux_test::pump(BufReader::new(e), |l| eprintln!("{l}"))));
    let (mut log, mut err) = match child.stdout.take() {
        Some(out) => crate::linux_test::pump(BufReader::new(out), |l| println!("{l}")),
        None => (String::new(), None),
    };
    match errs.map(|h| h.join()) {
        Some(Ok((e_log, e_err))) => {
            log.push_str(&e_log);
            if let Some(e) = e_err {
                err.get_or_insert(e);
            }
        }
        Some(Err(_)) => {
            err.get_or_insert("reading cargo's output: the stderr reader panicked".into());
        }
        None => {}
    }
    let ok = match child.wait() {
        Ok(s) => s.success(),
        Err(e) => {
            err.get_or_insert(format!("cargo: {e}"));
            false
        }
    };
    (ok && err.is_none(), log, err)
}

/// Runs the command.
pub fn run(args: Vec<String>) -> Result<(), String> {
    let extra = parse(args)?;
    let jobs = jobs(std::env::var("CARGO_BUILD_JOBS").ok().as_deref())?;
    if !have_nextest() {
        return Err(INSTALL.into());
    }
    let root = crate::release::repo_root();
    // nextest's store goes in the target dir, not the checkout.
    let target = target_dir(std::env::var("CARGO_TARGET_DIR").ok().as_deref(), &root);
    let config = target.join(TOOL_CONFIG);
    std::fs::create_dir_all(&target).map_err(|e| format!("test-gate: {}: {e}", target.display()))?;
    std::fs::write(&config, store_config(&target)?).map_err(|e| format!("test-gate: {}: {e}", config.display()))?;
    let extra: Vec<String> = store_args(&config).into_iter().chain(extra).collect();
    let start = Instant::now();
    let mut parts = Vec::new();
    let mut pass = true;
    for (name, cmd) in commands(false, &extra, &[]) {
        println!("test-gate: == {name}: {} (CARGO_BUILD_JOBS={jobs})", cmd.join(" "));
        let t = Instant::now();
        let mut c = Command::new(&cmd[0]);
        c.args(&cmd[1..]).current_dir(&root).env("CARGO_BUILD_JOBS", jobs.to_string());
        // nextest's output is read as it is printed, so the last line can
        // count its tests and name a flaky or failed one.
        let (ok, detail) = if name == "nextest" {
            let (ok, log, err) = run_read(c);
            let mut d = nextest_detail(&log);
            if let Some(e) = err {
                d.push_str(&format!(", {e}"));
            }
            (ok, format!(" ({d})"))
        } else {
            match c.status() {
                Ok(s) => (s.success(), String::new()),
                Err(e) => (false, format!(" ({}: {e})", cmd[0])),
            }
        };
        pass &= ok;
        parts.push(format!("{name} {}{detail} {}s", if ok { "ok" } else { "FAILED" }, t.elapsed().as_secs()));
    }
    println!("test-gate: {} in {}s; {}", if pass { "PASS" } else { "FAIL" }, start.elapsed().as_secs(), parts.join("; "));
    if pass {
        Ok(())
    } else {
        Err("test-gate failed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_gate_runs_nextest_at_16_threads_then_the_doc_tests() {
        let c = commands(false, &[], &[]);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].0, "nextest");
        assert_eq!(c[0].1.join(" "), "cargo nextest run --workspace --locked --no-fail-fast --test-threads 16");
        assert_eq!(c[1].0, "doctest");
        assert_eq!(c[1].1.join(" "), "cargo test --workspace --locked --doc");
        // Extra arguments go to nextest only, after its own.
        let c = commands(false, &v(&["-E", "test(x)"]), &[]);
        assert_eq!(c[0].1.join(" "), "cargo nextest run --workspace --locked --no-fail-fast --test-threads 16 -E test(x)");
        assert_eq!(c[1].1.join(" "), "cargo test --workspace --locked --doc");
        // Release, and libtest-style arguments for both (linux-test's `-- TEST_ARGS`).
        let c = commands(true, &[], &v(&["x", "--exact"]));
        assert_eq!(c[0].1.join(" "), "cargo nextest run --workspace --locked --no-fail-fast --test-threads 16 --release -- x --exact");
        assert_eq!(c[1].1.join(" "), "cargo test --workspace --locked --doc --release -- x --exact");
    }

    #[test]
    fn nextest_keeps_its_store_in_the_target_dir_not_the_checkout() {
        // nextest's store is `target/nextest` under the workspace root
        // whatever CARGO_TARGET_DIR says; the gate's tool config moves it to
        // the target dir, and nextest (only) is given it, before any extra
        // arguments.
        let root = Path::new("/w/atlas");
        assert_eq!(target_dir(None, root), root.join("target"));
        assert_eq!(target_dir(Some(""), root), root.join("target"));
        assert_eq!(target_dir(Some("/t/atlas-x"), root), PathBuf::from("/t/atlas-x"));
        assert_eq!(target_dir(Some("t"), root), root.join("t"));
        let target = PathBuf::from("/t/atlas-x");
        let store = target.join("nextest");
        assert_eq!(store_config(&target).unwrap(), format!("[store]\ndir = '{}'\n", store.display()));
        // A Windows path's backslashes are kept as they are (a literal string).
        if cfg!(windows) {
            assert_eq!(store_config(Path::new(r"C:\t\x")).unwrap(), format!("[store]\ndir = '{}'\n", r"C:\t\x\nextest"));
        }
        assert!(store_config(Path::new("/it's")).is_err());
        let config = target.join(TOOL_CONFIG);
        let args = store_args(&config);
        assert_eq!(args, v(&["--tool-config-file", &format!("atlas-test-gate:{}", config.display())]));
        let mut extra = args.clone();
        extra.extend(v(&["-E", "test(x)"]));
        let c = commands(false, &extra, &[]);
        assert_eq!(
            c[0].1.join(" "),
            format!(
                "cargo nextest run --workspace --locked --no-fail-fast --test-threads 16 --tool-config-file atlas-test-gate:{} -E test(x)",
                config.display()
            )
        );
        assert!(!c[1].1.iter().any(|x| x.contains("tool-config")));
    }

    #[test]
    fn args_after_the_separator_pass_through_and_nothing_else_is_taken() {
        assert_eq!(parse(v(&[])).unwrap(), Vec::<String>::new());
        assert_eq!(parse(v(&["--", "-E", "test(=a)", "--no-capture"])).unwrap(), v(&["-E", "test(=a)", "--no-capture"]));
        assert!(parse(v(&["-E"])).is_err());
        assert!(parse(v(&["--release"])).is_err());
    }

    #[test]
    fn build_jobs_default_to_4_and_cap_at_16() {
        assert_eq!(jobs(None), Ok(4));
        assert_eq!(jobs(Some("")), Ok(4));
        assert_eq!(jobs(Some("8")), Ok(8));
        assert_eq!(jobs(Some("16")), Ok(16));
        assert_eq!(jobs(Some("32")), Ok(16));
        assert!(jobs(Some("0")).is_err());
        assert!(jobs(Some("many")).is_err());
    }

    #[test]
    fn a_missing_nextest_is_one_line_naming_the_install() {
        assert!(!INSTALL.contains('\n'));
        assert!(INSTALL.contains("cargo install --locked cargo-nextest"));
    }

    #[test]
    fn nextest_totals_and_failures_read_its_summary_and_fail_lines() {
        let log = "\x1b[32m        PASS\x1b[0m [   0.010s] atlas chain::tests::a\n\
                   \x20       FAIL [   0.004s] atlas::hooks a_lookup_is_logged\n\
                   \x20 stdout ---\n\
                   ------------\n\
                   \x20    Summary [  12.345s] 1391 tests run: 1389 passed, 2 failed, 3 skipped\n\
                   \x20       FAIL [   0.004s] atlas::hooks a_lookup_is_logged\n\
                   \x20       FAIL [   0.200s] atlas chain::tests::b\n\
                   error: test run failed\n";
        assert_eq!(nextest_totals(log), (1389, 2));
        assert_eq!(nextest_failed(log), v(&["atlas::hooks a_lookup_is_logged", "atlas chain::tests::b"]));
        assert_eq!(nextest_totals("     Summary [   1.0s] 7 tests run: 7 passed, 1 skipped\n"), (7, 0));
        assert_eq!(nextest_totals("no summary here"), (0, 0));
    }

    /// Lines as cargo-nextest 0.9.118 prints them for a flaky, a failed, a
    /// leak-failed and a timed-out test (taken from a run of a probe crate).
    const PROBE: &str = "  TRY 1 FAIL [   0.015s] (───) nxprobe tests::flaky_once\n\
        \x20       FAIL [   0.035s] (1/5) nxprobe tests::fails\n\
        \x20       PASS [   0.050s] (2/5) nxprobe tests::ok_one\n\
        \x20 TRY 2 PASS [   0.018s] (3/5) nxprobe tests::flaky_once\n\
        \x20  LEAK-FAIL [   0.170s] (4/5) nxprobe tests::leaks\n\
        \x20 TERMINATING [>  2.000s] (───) nxprobe tests::hangs\n\
        \x20    TIMEOUT [   2.020s] (5/5) nxprobe tests::hangs\n\
        ────────────\n\
        \x20    Summary [   2.021s] 5 tests run: 2 passed (1 flaky), 2 failed (1 due to being leaky), 1 timed out, 0 skipped\n\
        \x20  FLAKY 2/2 [   0.018s] (3/5) nxprobe tests::flaky_once\n\
        \x20       FAIL [   0.035s] (1/5) nxprobe tests::fails\n\
        \x20  LEAK-FAIL [   0.170s] (4/5) nxprobe tests::leaks\n\
        \x20    TIMEOUT [   2.020s] (5/5) nxprobe tests::hangs\n\
        error: test run failed\n";

    #[test]
    fn timed_out_and_leak_failed_tests_count_as_failed_and_a_retried_pass_as_flaky() {
        // 5 run: 2 passed (one on its retry), 3 did not (failed, leak-failed, timed out).
        assert_eq!(nextest_totals(PROBE), (2, 3));
        assert_eq!(nextest_flaky(PROBE), v(&["nxprobe tests::flaky_once"]));
        assert_eq!(nextest_failed(PROBE), v(&["nxprobe tests::fails", "nxprobe tests::leaks", "nxprobe tests::hangs"]));
        assert_eq!(
            nextest_detail(PROBE),
            "2 passed (1 flaky: nxprobe tests::flaky_once), 3 failed: nxprobe tests::fails, nxprobe tests::leaks, nxprobe tests::hangs"
        );
        // A test that fails every try is failed, on its last try.
        let all = "  TRY 1 FAIL [   0.013s] (───) a b\n  TRY 2 FAIL [   0.007s] (1/1) a b\n     Summary [   0.025s] 1 test run: 0 passed, 1 failed, 0 skipped\n  TRY 2 FAIL [   0.007s] (1/1) a b\n";
        assert_eq!(nextest_totals(all), (0, 1));
        assert!(nextest_flaky(all).is_empty());
        assert_eq!(nextest_failed(all), v(&["a b"]));
        assert_eq!(nextest_detail(all), "0 passed, 1 failed: a b");
        // A test's own output that looks like a status is not one.
        assert!(nextest_failed("    thread 'x' FAIL [ not a status ] y\n").is_empty());
        assert_eq!(nextest_detail(""), "0 passed, 0 failed");
    }
}
