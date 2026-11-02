//! `cargo xtask coverage`: line and region coverage of the atlas crate from
//! rustc's own instrumentation, with no coverage crate and no cargo plugin.
//!
//! ```text
//! cargo xtask coverage [--debug] [--html] [--no-run] [-- TEST_ARGS...]
//! ```
//!
//! 1. Finds `llvm-profdata` and `llvm-cov` in the sysroot (the `llvm-tools`
//!    rustup component). If they are missing it says so and stops.
//! 2. Builds and runs `cargo test -p atlas` (release unless `--debug`) with
//!    `-C instrument-coverage`, in its own target folder
//!    (`<target>/coverage/build`) so normal builds are not invalidated. Every
//!    process writes a profile under `<target>/coverage/profraw`, including
//!    the atlas binary the integration tests start.
//! 3. Merges the profiles and asks `llvm-cov export -summary-only` for the
//!    per-file counts of every test executable and the atlas binary.
//! 4. Prints line and region coverage per module (a file under `src/`, by its
//!    path without `.rs`), per folder and in total, marks the core modules
//!    below [`CORE_FLOOR`], and writes the table to
//!    `<target>/coverage/summary.txt`. `--html` also writes llvm-cov's
//!    annotated source to `<target>/coverage/html/index.html`.
//!
//! `--no-run` reuses the profiles of the last run (to re-render the report).
//! Arguments after `--` go to the test binaries (a filter, `--ignored`, ...).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Usage line.
pub const USAGE: &str = "usage: cargo xtask coverage [--debug] [--html] [--no-run] [-- TEST_ARGS...]";

/// The line coverage the core modules are held to.
pub const CORE_FLOOR: f64 = 85.0;

/// The modules held to [`CORE_FLOOR`]: a module matches when its path
/// (relative to `src/`, without `.rs`) equals an entry or starts with an
/// entry followed by `/` or `_`.
pub const CORE: &[&str] = &[
    "store", "recall", "protocol", "tools", "json", "regex", "glob", "markdown", "paths", "fsio", "sources", "scan", "index", "chunk", "links", "hooks", "hook_rewrite", "hook_hits", "picture",
    "crop", "update", "sha256", "embed", "memory/recall",
];

/// Whether `module` is one of the [`CORE`] modules.
pub fn is_core(module: &str) -> bool {
    CORE.iter().any(|c| module == *c || module.strip_prefix(c).is_some_and(|r| r.starts_with('/') || r.starts_with('_')))
}

/// Covered and total counts.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Count {
    /// Covered.
    pub covered: u64,
    /// Total.
    pub count: u64,
}

impl Count {
    fn add(&mut self, o: Count) {
        self.covered += o.covered;
        self.count += o.count;
    }
    /// Percent covered; 100 when there is nothing to cover.
    pub fn pct(&self) -> f64 {
        if self.count == 0 {
            100.0
        } else {
            self.covered as f64 * 100.0 / self.count as f64
        }
    }
}

/// One module's counts.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Path under `src/` without `.rs`, with `/` separators.
    pub module: String,
    /// Lines.
    pub lines: Count,
    /// Regions.
    pub regions: Count,
}

/// The module name of a source file, or `None` when it is not under `src/`
/// of the repository at `root`.
pub fn module_of(file: &str, root: &Path) -> Option<String> {
    let norm = |s: &str| s.replace('\\', "/").to_ascii_lowercase();
    let f = file.replace('\\', "/");
    let src = norm(&root.join("src").to_string_lossy());
    let src = src.trim_end_matches('/');
    let lf = f.to_ascii_lowercase();
    let lf = lf.strip_prefix("//?/").unwrap_or(&lf);
    let off = f.len() - lf.len();
    let rest = lf.strip_prefix(src)?.strip_prefix('/')?;
    let start = off + src.len() + 1;
    let rel = &f[start..start + rest.len()];
    Some(rel.strip_suffix(".rs").unwrap_or(rel).to_string())
}

/// Parses `llvm-cov export -summary-only` output into rows for the files
/// under `root/src`, sorted by module.
pub fn rows(export: &str, root: &Path) -> Result<Vec<Row>, String> {
    let v = atlas::json::parse(export).map_err(|e| format!("llvm-cov export output: {e}"))?;
    let count = |s: &atlas::json::Value, k: &str| Count {
        covered: s.pointer(&format!("/{k}/covered")).and_then(|x| x.as_u64()).unwrap_or(0),
        count: s.pointer(&format!("/{k}/count")).and_then(|x| x.as_u64()).unwrap_or(0),
    };
    let mut out: Vec<Row> = Vec::new();
    for d in v.get("data").and_then(|d| d.as_array()).ok_or("llvm-cov export: no data")? {
        for f in d.get("files").and_then(|f| f.as_array()).into_iter().flatten() {
            let (Some(name), Some(s)) = (f.get("filename").and_then(|n| n.as_str()), f.get("summary")) else { continue };
            let Some(module) = module_of(name, root) else { continue };
            let (lines, regions) = (count(s, "lines"), count(s, "regions"));
            match out.iter_mut().find(|r| r.module == module) {
                Some(r) => {
                    r.lines.add(lines);
                    r.regions.add(regions);
                }
                None => out.push(Row { module, lines, regions }),
            }
        }
    }
    out.sort_by(|a, b| a.module.cmp(&b.module));
    Ok(out)
}

/// The report: one line per module, one per folder, the total, and the core
/// modules below [`CORE_FLOOR`].
pub fn render(rows: &[Row]) -> String {
    let mut s = String::new();
    let line = |s: &mut String, name: &str, l: Count, r: Count, mark: &str| {
        s.push_str(&format!("{name:<28} {:>6.1}% {:>6}/{:<6} {:>6.1}% {:>6}/{:<6} {mark}\n", l.pct(), l.covered, l.count, r.pct(), r.covered, r.count));
    };
    s.push_str(&format!("{:<28} {:>7} {:>13} {:>7} {:>13}\n", "module", "lines", "", "regions", ""));
    let (mut tl, mut tr) = (Count::default(), Count::default());
    let mut low = Vec::new();
    for r in rows {
        let core = is_core(&r.module);
        let below = core && r.lines.pct() < CORE_FLOOR;
        if below {
            low.push(format!("{} {:.1}%", r.module, r.lines.pct()));
        }
        line(&mut s, &r.module, r.lines, r.regions, if below { "core <85" } else if core { "core" } else { "" });
        tl.add(r.lines);
        tr.add(r.regions);
    }
    let mut dirs: Vec<(String, Count, Count)> = Vec::new();
    for r in rows {
        let Some((d, _)) = r.module.rsplit_once('/') else { continue };
        match dirs.iter_mut().find(|x| x.0 == d) {
            Some(x) => {
                x.1.add(r.lines);
                x.2.add(r.regions);
            }
            None => dirs.push((d.to_string(), r.lines, r.regions)),
        }
    }
    if !dirs.is_empty() {
        s.push('\n');
        for (d, l, r) in &dirs {
            line(&mut s, &format!("{d}/"), *l, *r, "");
        }
    }
    s.push('\n');
    line(&mut s, "TOTAL", tl, tr, "");
    if low.is_empty() {
        s.push_str(&format!("core modules below {CORE_FLOOR}% lines: none\n"));
    } else {
        s.push_str(&format!("core modules below {CORE_FLOOR}% lines: {}\n", low.join(", ")));
    }
    s
}

struct Tools {
    profdata: PathBuf,
    cov: PathBuf,
}

fn tools() -> Result<Tools, String> {
    let out = Command::new("rustc").args(["--print", "sysroot"]).output().map_err(|e| format!("rustc: {e}"))?;
    let sysroot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let vv = Command::new("rustc").arg("-vV").output().map_err(|e| format!("rustc: {e}"))?;
    let vv = String::from_utf8_lossy(&vv.stdout).into_owned();
    let host = vv.lines().find_map(|l| l.strip_prefix("host: ")).ok_or("rustc -vV printed no host")?.trim().to_string();
    let bin = sysroot.join("lib").join("rustlib").join(host).join("bin");
    let exe = |n: &str| bin.join(format!("{n}{}", std::env::consts::EXE_SUFFIX));
    let t = Tools { profdata: exe("llvm-profdata"), cov: exe("llvm-cov") };
    if !t.profdata.is_file() || !t.cov.is_file() {
        return Err(format!(
            "llvm-profdata and llvm-cov are not in {} (the llvm-tools rustup component is not installed).\ninstall it with: rustup component add llvm-tools",
            bin.display()
        ));
    }
    Ok(t)
}

fn repo_root() -> PathBuf {
    // xtask's manifest folder is <repo>/xtask.
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

fn target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(t) if !t.is_empty() => PathBuf::from(t),
        _ => root.join("target"),
    }
}

/// Runs the whole command.
pub fn run(args: Vec<String>) -> Result<(), String> {
    let (mut release, mut html, mut no_run, mut test_args) = (true, false, false, Vec::new());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--debug" => release = false,
            "--html" => html = true,
            "--no-run" => no_run = true,
            "--" => test_args.extend(it.by_ref()),
            _ => return Err(USAGE.into()),
        }
    }
    let t = tools()?;
    let root = repo_root();
    let out = target_dir(&root).join("coverage");
    let build = out.join("build");
    let raw = out.join("profraw");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let rustflags = {
        let base = std::env::var("RUSTFLAGS").unwrap_or_default();
        format!("{base} -C instrument-coverage").trim().to_string()
    };
    let mut common: Vec<String> = vec!["test".into(), "-p".into(), "atlas".into(), "--tests".into(), "--no-fail-fast".into()];
    if release {
        common.push("--release".into());
    }
    let cmd = |extra: &[&str]| {
        let mut c = Command::new(&cargo);
        c.current_dir(&root).args(&common).args(extra).env("CARGO_TARGET_DIR", &build).env("RUSTFLAGS", &rustflags).env_remove("CARGO_ENCODED_RUSTFLAGS");
        c
    };
    if !no_run {
        let _ = std::fs::remove_dir_all(&raw);
        std::fs::create_dir_all(&raw).map_err(|e| format!("{}: {e}", raw.display()))?;
        let pattern = raw.join("atlas-%p-%m.profraw");
        eprintln!("coverage: building and running the instrumented tests (target {})", build.display());
        let mut c = cmd(&[]);
        c.env("LLVM_PROFILE_FILE", &pattern);
        if !test_args.is_empty() {
            c.arg("--").args(&test_args);
        }
        let st = c.status().map_err(|e| format!("cargo test: {e}"))?;
        if !st.success() {
            eprintln!("coverage: cargo test failed ({st}); reporting what ran");
        }
    }
    let profiles: Vec<PathBuf> = std::fs::read_dir(&raw)
        .map_err(|e| format!("{}: {e} (run without --no-run first)", raw.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "profraw"))
        .collect();
    if profiles.is_empty() {
        return Err(format!("no profiles in {}", raw.display()));
    }
    eprintln!("coverage: merging {} profiles", profiles.len());
    let merged = out.join("atlas.profdata");
    let list = out.join("profraw.list");
    let text: String = profiles.iter().map(|p| format!("{}\n", p.display())).collect();
    std::fs::write(&list, text).map_err(|e| format!("{}: {e}", list.display()))?;
    let st = Command::new(&t.profdata)
        .args(["merge", "-sparse", "-f"])
        .arg(&list)
        .arg("-o")
        .arg(&merged)
        .status()
        .map_err(|e| format!("llvm-profdata: {e}"))?;
    if !st.success() {
        return Err(format!("llvm-profdata merge failed ({st})"));
    }
    // The executables of the same build: cargo prints them as JSON.
    let o = cmd(&["--no-run", "--message-format=json"]).stderr(Stdio::null()).output().map_err(|e| format!("cargo test --no-run: {e}"))?;
    let mut objects: Vec<String> = Vec::new();
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        let Ok(v) = atlas::json::parse(l) else { continue };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        if let Some(e) = v.get("executable").and_then(|e| e.as_str()) {
            if !objects.iter().any(|x| x == e) {
                objects.push(e.to_string());
            }
        }
    }
    if objects.is_empty() {
        return Err("cargo listed no test executables".into());
    }
    let ignore = r"(\.cargo[\\/]registry|[\\/]rustc[\\/]|[\\/]tests[\\/]|xtask)";
    let cov = |sub: &str| {
        let mut c = Command::new(&t.cov);
        c.arg(sub).arg(format!("-instr-profile={}", merged.display())).arg(format!("-ignore-filename-regex={ignore}")).arg(&objects[0]);
        for x in &objects[1..] {
            c.arg("-object").arg(x);
        }
        c
    };
    let ex = cov("export").args(["-format=text", "-summary-only"]).output().map_err(|e| format!("llvm-cov export: {e}"))?;
    if !ex.status.success() {
        return Err(format!("llvm-cov export failed: {}", String::from_utf8_lossy(&ex.stderr)));
    }
    let rows = rows(&String::from_utf8_lossy(&ex.stdout), &root)?;
    let report = render(&rows);
    print!("{report}");
    let summary = out.join("summary.txt");
    std::fs::write(&summary, &report).map_err(|e| format!("{}: {e}", summary.display()))?;
    println!("summary written to {}", summary.display());
    if html {
        let dir = out.join("html");
        let st = cov("show").args(["-format=html", "-show-line-counts-or-regions"]).arg(format!("-output-dir={}", dir.display())).status().map_err(|e| format!("llvm-cov show: {e}"))?;
        if !st.success() {
            return Err(format!("llvm-cov show failed ({st})"));
        }
        println!("html report written to {}", dir.join("index.html").display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_of_maps_files_under_src_only() {
        let root = Path::new("/r/atlas");
        assert_eq!(module_of("/r/atlas/src/store.rs", root).as_deref(), Some("store"));
        assert_eq!(module_of("/r/atlas/src/memory/recall.rs", root).as_deref(), Some("memory/recall"));
        assert_eq!(module_of("/r/atlas/tests/store.rs", root), None);
        assert_eq!(module_of("/r/atlas/srcx/store.rs", root), None);
        assert_eq!(module_of("/home/u/.cargo/registry/x/src/lib.rs", root), None);
    }

    #[test]
    fn module_of_ignores_case_separators_and_verbatim_prefix() {
        let root = Path::new(r"D:\work\proj");
        assert_eq!(module_of(r"d:\work\proj\src\embed\Bert.rs", root).as_deref(), Some("embed/Bert"));
        assert_eq!(module_of(r"\\?\D:\work\proj\src\json.rs", root).as_deref(), Some("json"));
    }

    #[test]
    fn core_matches_prefix_families() {
        assert!(is_core("store"));
        assert!(is_core("store_atlas"));
        assert!(is_core("tools_memory"));
        assert!(is_core("embed/bert"));
        assert!(is_core("memory/recall"));
        assert!(!is_core("memory/export"));
        assert!(!is_core("storehouse"));
        assert!(!is_core("bench"));
    }

    #[test]
    fn rows_sum_duplicate_files_and_render_totals() {
        let root = Path::new("/r");
        let j = r#"{"data":[{"files":[
            {"filename":"/r/src/a.rs","summary":{"lines":{"count":10,"covered":5},"regions":{"count":4,"covered":4}}},
            {"filename":"/r/src/a.rs","summary":{"lines":{"count":10,"covered":5},"regions":{"count":0,"covered":0}}},
            {"filename":"/r/src/m/b.rs","summary":{"lines":{"count":0,"covered":0},"regions":{"count":2,"covered":1}}},
            {"filename":"/elsewhere/c.rs","summary":{"lines":{"count":9,"covered":9},"regions":{"count":9,"covered":9}}}
        ]}]}"#;
        let r = rows(j, root).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], Row { module: "a".into(), lines: Count { covered: 10, count: 20 }, regions: Count { covered: 4, count: 4 } });
        let text = render(&r);
        assert!(text.contains("TOTAL"), "{text}");
        assert!(text.contains("m/"), "{text}");
        assert!(text.contains(" 50.0%"), "{text}");
        assert!(rows("{", root).is_err());
    }
}
