//! `cargo xtask verify-macos`: verify atlas on a machine (written for macOS,
//! Apple Silicon or Intel; every step but `sw_vers` runs elsewhere too). The
//! whole test suite with the `dev` feature, including the FSEvents watcher
//! test, then the recall check and the bench on the generic fixture corpus
//! in fixtures/generic, the NEON kernel tests, and the code index over this
//! repository. Nothing outside the repository is read; the databases go to a
//! temporary folder. Paste back everything it prints.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The `test result:` lines' passed and failed counts, summed.
pub fn test_totals(log: &str) -> (u64, u64) {
    let count = |l: &str, word: &str| -> u64 {
        // The number before the last " <word>" (sed 's/.* ([0-9]+) word.*/\1/').
        let Some(i) = l.rfind(&format!(" {word}")) else { return 0 };
        let head = &l[..i];
        let digits = head.rsplit(' ').next().unwrap_or("");
        digits.parse().unwrap_or(0)
    };
    log.lines().filter(|l| l.starts_with("test result")).fold((0, 0), |(p, f), l| (p + count(l, "passed"), f + count(l, "failed")))
}

fn grep<'a>(log: &'a str, pred: impl Fn(&str) -> bool) -> Vec<&'a str> {
    log.lines().filter(|l| pred(l)).collect()
}

/// A test line `test NAME ... OUTCOME`.
fn test_line<'a>(l: &'a str) -> Option<(&'a str, &'a str)> {
    let rest = l.strip_prefix("test ")?;
    let (name, outcome) = rest.split_once(" ... ")?;
    Some((name, outcome))
}

/// Runs the atlas binary; a failure is reported, not fatal.
fn atlas(bin: &Path, args: &[&str], what: &str) {
    match Command::new(bin).args(args).stdin(Stdio::null()).status() {
        Ok(s) if s.success() => {}
        Ok(s) => println!("{what} exited {}", s.code().unwrap_or(-1)),
        Err(e) => println!("{what} exited: {e}"),
    }
}

fn show(c: &mut Command) {
    match c.output() {
        Ok(o) if o.status.success() => print!("{}", String::from_utf8_lossy(&o.stdout)),
        _ => {}
    }
}

fn json_str(s: &str) -> String {
    atlas::json::to_string(&s.to_string())
}

/// A temporary folder, removed when dropped.
struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs the verification; the result is cargo test's exit status.
pub fn run() -> Result<i32, String> {
    let root = crate::release::repo_root();
    std::env::set_current_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = Tmp(std::env::temp_dir().join(format!("atlas-verify-{}-{stamp}", std::process::id())));
    std::fs::create_dir_all(&tmp.0).map_err(|e| format!("{}: {e}", tmp.0.display()))?;
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
    let bin = target.join("release").join(format!("atlas{}", std::env::consts::EXE_SUFFIX));
    let cfg = "fixtures/generic/atlas.json";
    let db = tmp.0.join("atlas.sqlite");
    let db_s = db.to_string_lossy().into_owned();

    println!("== system");
    if Command::new("uname").arg("-mrs").stdin(Stdio::null()).status().is_err() {
        println!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    }
    if let Ok(o) = Command::new("sw_vers").stderr(Stdio::null()).output() {
        print!("{}", String::from_utf8_lossy(&o.stdout).replace('\n', " "));
    }
    println!();
    show(Command::new("rustc").arg("--version"));
    show(Command::new("cargo").arg("--version"));

    println!("== tests (cargo test --release --features dev)");
    let out = Command::new("cargo")
        .args(["test", "--release", "--features", "dev", "--no-fail-fast"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cargo test: {e}"))?;
    let status = out.status.code().unwrap_or(1);
    let log = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    for l in grep(&log, |l| l.starts_with("test result")) {
        println!("{l}");
    }
    for l in grep(&log, |l| test_line(l).is_some_and(|(_, o)| o.starts_with("FAILED") || o.starts_with("ignored"))) {
        println!("{l}");
    }
    println!("watcher tests:");
    let watch = grep(&log, |l| {
        test_line(l).is_some_and(|(n, _)| n == "watch::tests::sees_a_write_under_the_root" || n.starts_with("platform::macos::tests::"))
    });
    if watch.is_empty() {
        println!("  (watcher tests not found)");
    }
    for l in watch {
        println!("{l}");
    }
    if status != 0 {
        println!("cargo test exited {status}; failures:");
        let mut on = false;
        let mut n = 0;
        for l in log.lines() {
            if !on && l == "failures:" {
                on = true;
            }
            if on && n < 150 {
                println!("{l}");
                n += 1;
                if l.starts_with("test result") {
                    on = false;
                }
            }
        }
        for l in grep(&err, |l| l.starts_with("error")).into_iter().take(40) {
            println!("{l}");
        }
    }
    let (passed, failed) = test_totals(&log);
    println!("total: {passed} passed, {failed} failed");

    println!("== recall check (fixture)");
    atlas(&bin, &["--config", cfg, "--db", &db_s, "--recall-check"], "recall check");

    println!("== bench (fixture, 500 runs)");
    atlas(&bin, &["--config", cfg, "--db", &db_s, "--bench", "500", "--queries", "fixtures/generic/queries.txt"], "bench");

    println!("== kernels (NEON against the scalar reference, bit for bit)");
    let kernels = grep(&log, |l| l.starts_with("test embed::gemm::") || l.starts_with("test embed::tests::"));
    if kernels.is_empty() {
        println!("  (kernel tests not found)");
    }
    for l in kernels {
        println!("{l}");
    }

    println!("== code index on this repository (rust-analyzer if installed, else the line scan)");
    let here = root.to_string_lossy().replace('\\', "/");
    let code_cfg = tmp.0.join("code.json");
    let code_db = tmp.0.join("code.sqlite").to_string_lossy().replace('\\', "/");
    let text = format!(
        "{{\"db\": {}, \"vaults\": [{{\"name\": \"notes\", \"path\": {}}}], \"code\": {{\"roots\": [{{\"name\": \"atlas\", \"root\": {}}}]}}}}",
        json_str(&code_db),
        json_str(&format!("{here}/fixtures/generic/Notes")),
        json_str(&here)
    );
    std::fs::write(&code_cfg, text).map_err(|e| format!("{}: {e}", code_cfg.display()))?;
    let code_cfg_s = code_cfg.to_string_lossy().into_owned();
    atlas(&bin, &["--config", &code_cfg_s, "--code-index"], "code index");
    atlas(&bin, &["--config", &code_cfg_s, "--recall-check"], "recall check");
    let names = tmp.0.join("names.txt");
    std::fs::write(&names, "Store\nServer\ndecode\nwindow\nIndexer\n").map_err(|e| format!("{}: {e}", names.display()))?;
    let names_s = names.to_string_lossy().into_owned();
    atlas(&bin, &["--config", &code_cfg_s, "--symbol-bench", &names_s, "--runs", "200"], "symbol bench");
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_sum_every_result_line() {
        let log = "test a ... ok\ntest result: ok. 12 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.1s\n\
                   test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n";
        assert_eq!(test_totals(log), (15, 2));
    }

    #[test]
    fn test_lines_split_name_and_outcome() {
        assert_eq!(test_line("test embed::gemm::x ... ok"), Some(("embed::gemm::x", "ok")));
        assert_eq!(test_line("running 3 tests"), None);
    }
}
