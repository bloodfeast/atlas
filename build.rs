//! Builds the vendored SQLite amalgamation for the owned backend
//! (`--features owned-sqlite`): finds a C compiler and an archiver, compiles
//! `vendor/sqlite/sqlite3.c` once with a fixed flag list, archives the one
//! object as `atlas_sqlite3`, and checks `src/db/ffi.rs` against
//! `sqlite3.h` with the same compiler. Standard library only: no `cc`
//! crate, no helper crates.
//!
//! With the rusqlite backend (the default feature set) this script does
//! nothing.
//!
//! Where the compiler comes from, first match wins, each step logged:
//!
//! * `CC_<target>` (underscores, then dashes), `TARGET_CC`, `CC`; the
//!   archiver likewise from `AR_<target>`, `TARGET_AR`, `AR`. An explicit
//!   compiler is used as given: if it cannot run, the build fails and says
//!   so, with no fallback.
//! * Windows MSVC: a Developer prompt (`VCINSTALLDIR`, `INCLUDE` and `LIB`
//!   all set: `cl.exe` and `lib.exe` from `PATH`), else `vswhere.exe` at its
//!   fixed path, the default tools version of the newest install with the
//!   VC tools, and the Windows SDK from the registry (`KitsRoot10`).
//! * Linux: `cc`, `clang`, `gcc`; `ar`, `llvm-ar`.
//! * macOS: `xcrun --sdk macosx --find clang`, then `cc`; `libtool` for the
//!   archive, else `ar`.
//!
//! The archive is the same bytes for a given compiler wherever cargo's
//! target folder is: one fixed flag order, no debug information, `/Brepro`
//! (MSVC) or `-ffile-prefix-map` and `ar D` / `ZERO_AR_DATE` (gcc, clang).
//! `cl.exe` also records the object's own absolute path in the object (the
//! S_OBJNAME record of `.debug$S`), and no flag maps it, so with MSVC the
//! object is compiled and archived in one fixed staging folder,
//! `atlas-sqlite3-stage` under the temp folder, and the archive is copied
//! to `OUT_DIR`. One build at a time holds that folder, through an
//! operating-system lock on `atlas-sqlite3-stage.lock` beside it: a second
//! build waits, and a build that dies drops the lock with its process (the
//! next one clears what it left). The folder is removed when the build is
//! done. The temp folder's own path is therefore part of an MSVC archive:
//! equal on one machine for one user, which is what the check compares.
//!
//! `ATLAS_SQLITE_LEVERS` (read only with the `dev` feature) swaps flags for
//! the lever benches: a comma list of `-NAME` (drop a define), `+NAME` or
//! `+NAME=VALUE` (add or replace one) and `opt:FLAGS` (replace the
//! optimisation flags, space separated).

use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// The compile-time options of SQLite, in the order they are passed. This
/// is the set rusqlite's bundled build (libsqlite3-sys 0.38.2) passes, so
/// the two backends run the same SQLite.
const DEFINES: &[&str] = &[
    "SQLITE_CORE",
    "SQLITE_DEFAULT_FOREIGN_KEYS=1",
    "SQLITE_ENABLE_API_ARMOR",
    "SQLITE_ENABLE_COLUMN_METADATA",
    "SQLITE_ENABLE_DBSTAT_VTAB",
    "SQLITE_ENABLE_FTS3",
    "SQLITE_ENABLE_FTS3_PARENTHESIS",
    "SQLITE_ENABLE_FTS5",
    "SQLITE_ENABLE_JSON1",
    "SQLITE_ENABLE_LOAD_EXTENSION=1",
    "SQLITE_ENABLE_MEMORY_MANAGEMENT",
    "SQLITE_ENABLE_RTREE",
    "SQLITE_ENABLE_STAT4",
    "SQLITE_SOUNDEX",
    "SQLITE_THREADSAFE=1",
    "SQLITE_USE_URI",
    "HAVE_USLEEP=1",
    "HAVE_ISNAN",
    "_POSIX_THREAD_SAFE_FUNCTIONS",
];

/// Added after [`DEFINES`] off Windows.
const DEFINES_UNIX: &[&str] = &["HAVE_LOCALTIME_R"];

const SOURCE: &str = "vendor/sqlite/sqlite3.c";
const HEADER: &str = "vendor/sqlite/sqlite3.h";
const FFI: &str = "src/db/ffi.rs";
const LIB_NAME: &str = "atlas_sqlite3";
/// The staging folder under the temp folder (MSVC), and the stem of its
/// lock file.
const STAGE_NAME: &str = "atlas-sqlite3-stage";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var_os("CARGO_FEATURE_OWNED_SQLITE").is_none() {
        return;
    }
    if let Err(e) = run() {
        eprintln!("error: atlas build.rs: {e}");
        std::process::exit(1);
    }
}

/// An environment variable the build depends on: read, and named to cargo
/// as a rebuild trigger. Empty counts as unset.
fn var(name: &str) -> Option<String> {
    println!("cargo:rerun-if-env-changed={name}");
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// A variable cargo sets for build scripts.
fn cargo_var(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is not set (this script runs under cargo)"))
}

fn log(line: &str) {
    println!("atlas build.rs: {line}");
}

/// A program to run: where it is, the arguments it always takes, and the
/// environment it gets (`None`: this process's; `Some`: exactly these).
struct Tool {
    program: PathBuf,
    args: Vec<String>,
    envs: Option<Vec<(String, OsString)>>,
    /// How it was found, for messages.
    from: String,
}

impl Tool {
    fn found(program: impl Into<PathBuf>, from: impl Into<String>) -> Tool {
        Tool { program: program.into(), args: Vec::new(), envs: None, from: from.into() }
    }

    /// A tool named by an environment variable: a path to a file as is,
    /// otherwise a program followed by arguments, split at white space.
    fn explicit(name: &str, value: &str) -> Tool {
        let from = format!("{name}={value}");
        if Path::new(value).is_file() {
            return Tool::found(value, from);
        }
        let mut words = value.split_whitespace().map(str::to_string);
        let program = words.next().unwrap_or_default();
        Tool { program: program.into(), args: words.collect(), envs: None, from }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        if let Some(envs) = &self.envs {
            c.env_clear();
            c.envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_os_str())));
        }
        c
    }

    /// Lower-case file name without extension: what kind of tool it is.
    fn stem(&self) -> String {
        self.program.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()
    }
}

/// Why a tool gave no result.
enum ToolError {
    /// It could not be started: nothing was compiled or archived.
    NotRun(String),
    /// It ran and exited unsuccessfully; the message carries its whole
    /// output.
    Failed(String),
}

impl ToolError {
    fn text(self) -> String {
        match self {
            ToolError::NotRun(text) | ToolError::Failed(text) => text,
        }
    }
}

/// Runs a prepared command to its end.
///
/// # Errors
///
/// A tool that cannot start, or exits unsuccessfully.
fn run_tool(what: &str, tool: &Tool, mut cmd: Command) -> Result<String, ToolError> {
    let out = cmd
        .output()
        .map_err(|e| ToolError::NotRun(format!("could not run {what} `{}` (from {}): {e}", tool.program.display(), tool.from)))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(ToolError::Failed(format!(
            "{what} failed ({}): `{}` (from {})\n{}",
            out.status,
            tool.program.display(),
            tool.from,
            text.trim_end()
        )));
    }
    Ok(text)
}

/// Whether `program` starts at all (any exit status).
fn starts(program: &str, arg: &str) -> bool {
    Command::new(program).arg(arg).output().is_ok()
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    /// `cl.exe` flags and `lib.exe`.
    Msvc,
    /// gcc and clang flags.
    Gnu,
}

#[derive(Clone, Copy, PartialEq)]
enum Archiver {
    Lib,
    Ar,
    Libtool,
}

struct Toolchain {
    cc: Tool,
    style: Style,
    ar: Tool,
    archiver: Archiver,
    /// Flags the target needs before atlas's own (macOS: target and SDK).
    target_flags: Vec<String>,
}

struct Build {
    target: String,
    host: String,
    os: String,
    env: String,
    arch: String,
    out_dir: PathBuf,
    manifest: PathBuf,
    /// What was tried and did not yield a compiler, for the error message.
    tried: Vec<String>,
}

impl Build {
    /// `BASE_<target>` (underscores, then as written), `TARGET_BASE`, `BASE`.
    fn explicit(&mut self, base: &str) -> Option<(String, String)> {
        let names = [
            format!("{base}_{}", self.target.replace('-', "_")),
            format!("{base}_{}", self.target),
            format!("TARGET_{base}"),
            base.to_string(),
        ];
        let mut found = None;
        for name in names {
            // Every spelling is read, so each is a rebuild trigger.
            let value = var(&name);
            if found.is_none() {
                found = value.map(|v| (name, v));
            }
        }
        if found.is_none() {
            self.tried.push(format!("{base}_{}, TARGET_{base}, {base} (unset)", self.target.replace('-', "_")));
        }
        found
    }

    fn toolchain(&mut self) -> Result<Toolchain, String> {
        match (self.os.as_str(), self.env.as_str()) {
            ("windows", "msvc") => self.msvc(),
            ("linux", _) => self.linux(),
            ("macos", _) => self.macos(),
            _ => Err(format!("unsupported target {}: atlas builds SQLite for Windows (MSVC), Linux and macOS", self.target)),
        }
    }

    fn no_compiler(&self, hint: &str) -> String {
        format!("no C compiler for {}. Tried: {}. {hint}", self.target, self.tried.join("; "))
    }

    // ---- Windows MSVC ----

    fn msvc(&mut self) -> Result<Toolchain, String> {
        let arch = match self.arch.as_str() {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "x86" => "x86",
            other => return Err(format!("unsupported MSVC architecture {other}")),
        };
        let cc = self.explicit("CC");
        let ar = self.explicit("AR");
        let prompt = match (var("VCINSTALLDIR"), var("INCLUDE"), var("LIB")) {
            (Some(_), Some(_), Some(_)) => true,
            (vc, inc, lib) => {
                let unset: Vec<&str> = [("VCINSTALLDIR", vc.is_none()), ("INCLUDE", inc.is_none()), ("LIB", lib.is_none())]
                    .iter()
                    .filter(|x| x.1)
                    .map(|x| x.0)
                    .collect();
                self.tried.push(format!("developer prompt ({} unset)", unset.join(", ")));
                false
            }
        };
        if let Some((name, value)) = cc {
            // An explicit compiler is taken as given. It gets this process's
            // environment; outside a Developer prompt the include and
            // library paths are filled in when an install is found.
            let mut tool = Tool::explicit(&name, &value);
            let style = if matches!(tool.stem().as_str(), "cl" | "clang-cl") { Style::Msvc } else { Style::Gnu };
            let found = if prompt { None } else { self.vswhere(arch) };
            if let (Style::Msvc, Some(vs)) = (style, &found) {
                let mut envs: Vec<(String, OsString)> = env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v)).collect();
                envs.retain(|(k, _)| !k.eq_ignore_ascii_case("INCLUDE") && !k.eq_ignore_ascii_case("LIB"));
                envs.push(("INCLUDE".into(), vs.include.clone()));
                envs.push(("LIB".into(), vs.lib.clone()));
                tool.envs = Some(envs);
            }
            let (ar, archiver) = match ar {
                Some((name, value)) => (Tool::explicit(&name, &value), if style == Style::Msvc { Archiver::Lib } else { Archiver::Ar }),
                None if style == Style::Gnu => (Tool::found("llvm-ar", "PATH"), Archiver::Ar),
                None => match (tool.program.with_file_name("lib.exe"), found) {
                    (beside, _) if beside.is_file() => {
                        (Tool { program: beside, args: Vec::new(), envs: None, from: "beside the compiler".into() }, Archiver::Lib)
                    }
                    (_, Some(vs)) => (vs.lib_exe, Archiver::Lib),
                    _ => (Tool::found("lib.exe", "PATH"), Archiver::Lib),
                },
            };
            log(&format!("C compiler: {} (from {})", tool.program.display(), tool.from));
            return Ok(Toolchain { cc: tool, style, ar, archiver, target_flags: Vec::new() });
        }
        if prompt {
            log("C compiler: cl.exe from PATH (developer prompt: VCINSTALLDIR, INCLUDE and LIB are set)");
            let ar = match ar {
                Some((name, value)) => Tool::explicit(&name, &value),
                None => Tool::found("lib.exe", "PATH (developer prompt)"),
            };
            return Ok(Toolchain {
                cc: Tool::found("cl.exe", "PATH (developer prompt)"),
                style: Style::Msvc,
                ar,
                archiver: Archiver::Lib,
                target_flags: Vec::new(),
            });
        }
        match self.vswhere(arch) {
            Some(vs) => {
                log(&format!("C compiler: {} ({})", vs.cl.program.display(), vs.cl.from));
                let ar = match ar {
                    Some((name, value)) => Tool::explicit(&name, &value),
                    None => vs.lib_exe,
                };
                Ok(Toolchain { cc: vs.cl, style: Style::Msvc, ar, archiver: Archiver::Lib, target_flags: Vec::new() })
            }
            None => Err(self.no_compiler("Install the \"Desktop development with C++\" workload (Visual Studio or its Build Tools), or run from a Developer prompt, or set CC.")),
        }
    }

    /// The newest Visual Studio install with the VC tools, through
    /// `vswhere.exe` at its fixed path, with the Windows SDK.
    fn vswhere(&mut self, arch: &str) -> Option<VsTools> {
        let pf86 = PathBuf::from(env::var_os("ProgramFiles(x86)").unwrap_or_else(|| r"C:\Program Files (x86)".into()));
        let system_root = PathBuf::from(env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()));
        let vswhere = pf86.join(r"Microsoft Visual Studio\Installer\vswhere.exe");
        if !vswhere.is_file() {
            self.tried.push(format!("vswhere at {} (not found)", vswhere.display()));
            return None;
        }
        let out = Command::new(&vswhere)
            .args([
                "-latest",
                "-products",
                "*",
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-property",
                "installationPath",
                "-utf8",
            ])
            .output()
            .ok()?;
        let install = String::from_utf8_lossy(&out.stdout).lines().map(str::trim).find(|l| !l.is_empty()).map(PathBuf::from);
        let Some(install) = install else {
            self.tried.push(format!("vswhere at {} (no install with the VC tools)", vswhere.display()));
            return None;
        };
        let version_file = install.join(r"VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt");
        let Ok(version) = fs::read_to_string(&version_file) else {
            self.tried.push(format!("vswhere: {} (cannot read)", version_file.display()));
            return None;
        };
        let tools = install.join(r"VC\Tools\MSVC").join(version.trim());
        let host = if self.host.starts_with("aarch64") {
            "Hostarm64"
        } else if self.host.starts_with("x86_64") {
            "Hostx64"
        } else {
            "Hostx86"
        };
        let bin = tools.join("bin").join(host).join(arch);
        let cl = bin.join("cl.exe");
        if !cl.is_file() {
            self.tried.push(format!("vswhere: {} (not found)", cl.display()));
            return None;
        }
        let sdk = self.windows_sdk(&pf86, &system_root, arch)?;
        let join = |dirs: &[PathBuf]| env::join_paths(dirs).unwrap_or_default();
        let mut include = vec![tools.join("include")];
        include.extend(sdk.include);
        let mut lib = vec![tools.join("lib").join(arch)];
        lib.extend(sdk.lib);
        let (include, lib) = (join(&include), join(&lib));
        // The compiler's whole environment: nothing else is inherited.
        let temp = env::var_os("TEMP").unwrap_or_else(|| self.out_dir.clone().into());
        let envs = vec![
            ("PATH".to_string(), join(&[bin.clone(), system_root.join("System32")])),
            ("INCLUDE".to_string(), include.clone()),
            ("LIB".to_string(), lib.clone()),
            ("SystemRoot".to_string(), system_root.into_os_string()),
            ("TEMP".to_string(), temp.clone()),
            ("TMP".to_string(), temp),
        ];
        let from = format!("vswhere: {}, tools {}, Windows SDK {}", install.display(), version.trim(), sdk.version);
        Some(VsTools {
            cl: Tool { program: cl, args: Vec::new(), envs: Some(envs.clone()), from: from.clone() },
            lib_exe: Tool { program: bin.join("lib.exe"), args: Vec::new(), envs: Some(envs), from },
            include,
            lib,
        })
    }

    /// The newest Windows 10/11 SDK that has the C runtime and `windows.h`:
    /// its root from the registry (`reg.exe query`), else the default folder.
    fn windows_sdk(&mut self, pf86: &Path, system_root: &Path, arch: &str) -> Option<Sdk> {
        const KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots";
        let reg = system_root.join(r"System32\reg.exe");
        let query = |view: &[&str]| -> Option<PathBuf> {
            let out = Command::new(&reg).args(["query", KEY, "/v", "KitsRoot10"]).args(view).output().ok()?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let line = text.lines().find(|l| l.contains("KitsRoot10") && l.contains("REG_SZ"))?;
            let value = line.split_once("REG_SZ")?.1.trim();
            (!value.is_empty()).then(|| PathBuf::from(value))
        };
        let root = query(&[]).or_else(|| query(&["/reg:32"])).unwrap_or_else(|| pf86.join(r"Windows Kits\10"));
        let numbers = |name: &str| -> Vec<u32> { name.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
        let mut versions: Vec<String> = fs::read_dir(root.join("Include"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|v| {
                root.join("Include").join(v).join(r"um\windows.h").is_file() && root.join("Include").join(v).join(r"ucrt\stdio.h").is_file()
            })
            .collect();
        versions.sort_by_key(|v| numbers(v));
        let Some(version) = versions.pop() else {
            self.tried.push(format!("Windows SDK under {} (none with um\\windows.h and ucrt\\stdio.h)", root.display()));
            return None;
        };
        let inc = root.join("Include").join(&version);
        let lib = root.join("Lib").join(&version);
        Some(Sdk {
            include: vec![inc.join("ucrt"), inc.join("um"), inc.join("shared")],
            lib: vec![lib.join("ucrt").join(arch), lib.join("um").join(arch)],
            version,
        })
    }

    // ---- Linux ----

    fn linux(&mut self) -> Result<Toolchain, String> {
        let cc = match self.explicit("CC") {
            Some((name, value)) => Tool::explicit(&name, &value),
            None => match ["cc", "clang", "gcc"].into_iter().find(|c| starts(c, "--version")) {
                Some(c) => Tool::found(c, "PATH"),
                None => {
                    self.tried.push("cc, clang, gcc on PATH (none runs)".into());
                    return Err(self.no_compiler("Install a C compiler (gcc or clang) or set CC."));
                }
            },
        };
        log(&format!("C compiler: {} (from {})", cc.program.display(), cc.from));
        let ar = self.unix_ar(&cc)?;
        Ok(Toolchain { cc, style: Style::Gnu, ar, archiver: Archiver::Ar, target_flags: Vec::new() })
    }

    /// `AR_<target>`, `TARGET_AR`, `AR`; then `ar`, `llvm-ar`; then, when
    /// the compiler is zig's, `zig ar`.
    fn unix_ar(&mut self, cc: &Tool) -> Result<Tool, String> {
        if let Some((name, value)) = self.explicit("AR") {
            return Ok(Tool::explicit(&name, &value));
        }
        if let Some(a) = ["ar", "llvm-ar"].into_iter().find(|a| starts(a, "--version")) {
            return Ok(Tool::found(a, "PATH"));
        }
        let version = cc.command().arg("--version").output().map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase()).unwrap_or_default();
        if version.contains("zig") && starts("zig", "version") {
            return Ok(Tool { program: "zig".into(), args: vec!["ar".into()], envs: None, from: "PATH (the compiler is zig cc)".into() });
        }
        self.tried.push("ar, llvm-ar on PATH (none runs)".into());
        Err(format!("no archiver for {}. Tried: {}. Install binutils or llvm, or set AR.", self.target, self.tried.join("; ")))
    }

    // ---- macOS ----

    fn macos(&mut self) -> Result<Toolchain, String> {
        let xcrun = |args: &[&str]| -> Option<String> {
            let out = Command::new("xcrun").args(args).output().ok().filter(|o| o.status.success())?;
            let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!line.is_empty()).then_some(line)
        };
        let sdkroot = var("SDKROOT");
        let deployment = var("MACOSX_DEPLOYMENT_TARGET");
        let mut target_flags = Vec::new();
        let cc = match self.explicit("CC") {
            // An explicit compiler (cargo-zigbuild's zig cc among them)
            // carries its own target; only a given SDK is passed on.
            Some((name, value)) => {
                if let Some(sdk) = &sdkroot {
                    target_flags.extend(["-isysroot".to_string(), sdk.clone()]);
                }
                Tool::explicit(&name, &value)
            }
            None => {
                let cc = match xcrun(&["--sdk", "macosx", "--find", "clang"]) {
                    Some(clang) => Tool::found(clang, "xcrun --sdk macosx --find clang"),
                    None if starts("cc", "--version") => Tool::found("cc", "PATH"),
                    None => {
                        self.tried.push("xcrun --sdk macosx --find clang (failed); cc on PATH (does not run)".into());
                        return Err(self.no_compiler("Install the Xcode command line tools (xcode-select --install) or set CC."));
                    }
                };
                // rustc's default deployment targets, unless one is set.
                let (clang_arch, floor) = if self.arch == "aarch64" { ("arm64", "11.0") } else { ("x86_64", "10.12") };
                target_flags.push(format!("--target={clang_arch}-apple-macosx{}", deployment.as_deref().unwrap_or(floor)));
                if let Some(sdk) = sdkroot.clone().or_else(|| xcrun(&["--sdk", "macosx", "--show-sdk-path"])) {
                    target_flags.extend(["-isysroot".to_string(), sdk]);
                }
                cc
            }
        };
        log(&format!("C compiler: {} (from {})", cc.program.display(), cc.from));
        let (ar, archiver) = match self.explicit("AR") {
            Some((name, value)) => (Tool::explicit(&name, &value), Archiver::Ar),
            None => match xcrun(&["--find", "libtool"]) {
                Some(libtool) => (Tool::found(libtool, "xcrun --find libtool"), Archiver::Libtool),
                None => (self.unix_ar(&cc)?, Archiver::Ar),
            },
        };
        Ok(Toolchain { cc, style: Style::Gnu, ar, archiver, target_flags })
    }
}

struct VsTools {
    cl: Tool,
    lib_exe: Tool,
    include: OsString,
    lib: OsString,
}

struct Sdk {
    include: Vec<PathBuf>,
    lib: Vec<PathBuf>,
    version: String,
}

/// The fixed folder an MSVC object is compiled and archived in, held by one
/// build at a time (see the header). Dropped, the folder is removed and the
/// lock released.
struct Stage {
    dir: PathBuf,
    /// The lock file, locked while this value lives. The lock belongs to
    /// the open handle, so it ends when the handle closes, however the
    /// process ends. The file itself is never removed: a waiter holds a
    /// handle to it, and a new file under the same name would be a second
    /// lock.
    _lock: fs::File,
}

impl Stage {
    /// Takes the lock, waiting for a build that holds it, and leaves the
    /// folder empty.
    fn enter() -> Result<Stage, String> {
        let temp = env::temp_dir();
        let dir = temp.join(STAGE_NAME);
        let lock_path = temp.join(format!("{STAGE_NAME}.lock"));
        let at = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
        let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).map_err(|e| at(&lock_path, e))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                log(&format!("another build is compiling SQLite in {}; waiting for it", dir.display()));
                lock.lock().map_err(|e| at(&lock_path, e))?;
            }
            Err(fs::TryLockError::Error(e)) => return Err(at(&lock_path, e)),
        }
        // What a build that was killed left behind.
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(at(&dir, e)),
        }
        fs::create_dir_all(&dir).map_err(|e| at(&dir, e))?;
        log(&format!("staging folder: {}", dir.display()));
        Ok(Stage { dir, _lock: lock })
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        // Runs before the lock field closes; a failure leaves files the
        // next build removes.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The defines and the optimisation flags, after the dev-only levers.
fn flags(b: &Build, style: Style) -> Result<(Vec<String>, Vec<String>), String> {
    let mut defines: Vec<String> = DEFINES.iter().map(|d| d.to_string()).collect();
    if b.os != "windows" {
        defines.extend(DEFINES_UNIX.iter().map(|d| d.to_string()));
    }
    // The profile's level, as the `cc` crate maps it (rusqlite's build).
    let level = env::var("OPT_LEVEL").unwrap_or_else(|_| "3".into());
    let mut opt: Vec<String> = match (style, level.as_str()) {
        (Style::Msvc, "0") => vec!["/Od".into()],
        (Style::Msvc, "1" | "s" | "z") => vec!["/O1".into(), "/Oi".into(), "/Gy".into()],
        (Style::Msvc, _) => vec!["/O2".into(), "/Oi".into(), "/Gy".into()],
        (Style::Gnu, l) => vec![format!("-O{l}")],
    };
    let levers = var("ATLAS_SQLITE_LEVERS");
    if let Some(levers) = levers {
        if env::var_os("CARGO_FEATURE_DEV").is_none() {
            println!("cargo:warning=ATLAS_SQLITE_LEVERS is set and ignored: it is read only with the dev feature");
        } else {
            let name = |d: &str| d.split('=').next().unwrap_or(d).to_string();
            for lever in levers.split(',').map(str::trim).filter(|l| !l.is_empty()) {
                if let Some(drop) = lever.strip_prefix('-') {
                    let before = defines.len();
                    defines.retain(|d| name(d) != drop);
                    if defines.len() == before {
                        return Err(format!("ATLAS_SQLITE_LEVERS: -{drop} names no define in the list"));
                    }
                } else if let Some(add) = lever.strip_prefix('+') {
                    match defines.iter_mut().find(|d| name(d) == name(add)) {
                        Some(d) => *d = add.to_string(),
                        None => defines.push(add.to_string()),
                    }
                } else if let Some(o) = lever.strip_prefix("opt:") {
                    opt = o.split_whitespace().map(str::to_string).collect();
                } else {
                    return Err(format!("ATLAS_SQLITE_LEVERS: {lever:?} is not -NAME, +NAME[=VALUE] or opt:FLAGS"));
                }
            }
            println!("cargo:warning=ATLAS_SQLITE_LEVERS={levers}: SQLite built with {} {}", opt.join(" "), defines.join(" "));
        }
    }
    Ok((defines, opt))
}

fn run() -> Result<(), String> {
    let mut b = Build {
        target: cargo_var("TARGET")?,
        host: cargo_var("HOST")?,
        os: cargo_var("CARGO_CFG_TARGET_OS")?,
        env: env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default(),
        arch: cargo_var("CARGO_CFG_TARGET_ARCH")?,
        out_dir: PathBuf::from(cargo_var("OUT_DIR")?),
        manifest: PathBuf::from(cargo_var("CARGO_MANIFEST_DIR")?),
        tried: Vec::new(),
    };
    for path in [SOURCE, HEADER, FFI] {
        println!("cargo:rerun-if-changed={path}");
    }
    let crt_static = env::var("CARGO_CFG_TARGET_FEATURE").is_ok_and(|f| f.split(',').any(|x| x == "crt-static"));
    let tc = b.toolchain()?;
    let (defines, opt) = flags(&b, tc.style)?;
    let user_cflags = b.explicit("CFLAGS").map(|(_, v)| v).unwrap_or_default();

    ffi_check(&b, &tc)?;

    // MSVC writes the object's path into the object: a fixed folder, held
    // until the archive is copied out. Other compilers work in OUT_DIR.
    let stage = match tc.style {
        Style::Msvc => Some(Stage::enter()?),
        Style::Gnu => None,
    };
    let work: &Path = stage.as_ref().map_or(b.out_dir.as_path(), |s| s.dir.as_path());

    // Compile. The working folder is the crate root and the source path is
    // relative to it, so no absolute source path is spelled on the command
    // line.
    let started = Instant::now();
    let mut cmd = tc.cc.command();
    cmd.current_dir(&b.manifest);
    let object = match tc.style {
        Style::Msvc => {
            let object = work.join("sqlite3.obj");
            cmd.args(["/nologo", "/c"]).args(&opt).args(["/Brepro", if crt_static { "/MT" } else { "/MD" }, "/W0"]);
            cmd.args(defines.iter().map(|d| format!("/D{d}")));
            cmd.args(user_cflags.split_whitespace());
            cmd.arg(format!("/Fo{}", object.display())).arg(SOURCE.replace('/', "\\"));
            object
        }
        Style::Gnu => {
            let object = work.join("sqlite3.o");
            cmd.args(&tc.target_flags).arg("-c").args(&opt);
            cmd.args(["-fPIC", "-ffunction-sections", "-fdata-sections", "-w"]);
            cmd.arg(format!("-ffile-prefix-map={}=atlas", b.manifest.display()));
            cmd.args(defines.iter().map(|d| format!("-D{d}")));
            // After atlas's flags, so a user's -O or a sanitizer wins.
            cmd.args(user_cflags.split_whitespace());
            cmd.arg("-o").arg(&object).arg(SOURCE);
            object
        }
    };
    run_tool("the C compiler", &tc.cc, cmd).map_err(ToolError::text)?;
    log(&format!("compiled {SOURCE} in {:.1} s with {} {}", started.elapsed().as_secs_f64(), opt.join(" "), defines.join(" ")));

    // Archive the one object.
    let mut cmd = tc.ar.command();
    cmd.current_dir(work);
    let member = object.file_name().unwrap_or_default();
    let archive = match tc.archiver {
        Archiver::Lib => format!("{LIB_NAME}.lib"),
        Archiver::Ar | Archiver::Libtool => format!("lib{LIB_NAME}.a"),
    };
    match tc.archiver {
        Archiver::Lib => {
            cmd.args(["/nologo", "/Brepro"]).arg(format!("/OUT:{archive}")).arg(member);
        }
        Archiver::Ar => {
            // `ar` adds to an archive that exists; start from none.
            let _ = fs::remove_file(work.join(&archive));
            // D: zero timestamps, owners and modes. Apple's ar has no D; it
            // reads ZERO_AR_DATE instead.
            let apple = b.os == "macos" && !tc.ar.stem().contains("llvm") && tc.ar.stem() != "zig";
            cmd.arg(if apple { "crs" } else { "crsD" }).arg(&archive).arg(member).env("ZERO_AR_DATE", "1");
        }
        Archiver::Libtool => {
            cmd.args(["-static", "-o"]).arg(&archive).arg(member).env("ZERO_AR_DATE", "1");
        }
    }
    run_tool("the archiver", &tc.ar, cmd).map_err(ToolError::text)?;
    if let Some(stage) = stage {
        let (from, to) = (stage.dir.join(&archive), b.out_dir.join(&archive));
        fs::copy(&from, &to).map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))?;
        // The staging folder is removed and its lock released here.
        drop(stage);
    }
    println!("cargo:rustc-link-search=native={}", b.out_dir.display());
    println!("cargo:rustc-link-lib=static={LIB_NAME}");
    Ok(())
}

/// The C spelling of a type as `src/db/ffi.rs` writes it.
fn c_type(rust: &str) -> Result<String, String> {
    let rust = rust.trim();
    if let Some(inner) = rust.strip_prefix("*const ") {
        return Ok(format!("{} const *", c_type(inner)?));
    }
    if let Some(inner) = rust.strip_prefix("*mut ") {
        return Ok(format!("{} *", c_type(inner)?));
    }
    Ok(match rust {
        "c_int" => "int",
        "c_uint" => "unsigned int",
        "c_char" => "char",
        "c_uchar" => "unsigned char",
        "c_double" => "double",
        "c_void" => "void",
        "i64" => "sqlite3_int64",
        "u64" => "sqlite3_uint64",
        "sqlite3" | "sqlite3_stmt" | "sqlite3_destructor_type" => rust,
        // Not a name of the header's: ffi_check declares it.
        "busy_callback" => "atlas_busy_callback",
        other => return Err(format!("{FFI}: the type `{other}` has no C spelling in build.rs (c_type)")),
    }
    .to_string())
}

/// Checks `src/db/ffi.rs` against `sqlite3.h` with the C compiler: a C file
/// generated from the Rust declarations asserts every `SQLITE_*` constant's
/// value and assigns every declared function to a pointer of the type its
/// Rust signature spells. A mismatch does not compile.
fn ffi_check(b: &Build, tc: &Toolchain) -> Result<(), String> {
    let ffi = fs::read_to_string(b.manifest.join(FFI)).map_err(|e| format!("{FFI}: {e}"))?;
    let mut c = String::from("/* Generated by build.rs from src/db/ffi.rs. */\n#include \"sqlite3.h\"\n");
    // `busy_callback` of ffi.rs: the header gives sqlite3_busy_handler's
    // callback no name of its own.
    c.push_str("typedef int (*atlas_busy_callback)(void *, int);\n");
    let (mut consts, mut fns) = (0, 0);
    for line in ffi.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("pub const SQLITE_") {
            let parsed = rest
                .split_once(": ")
                .and_then(|(name, r)| r.split_once(" = ").map(|(_, v)| (name, v.trim_end_matches(';').replace('_', ""))));
            let Some((name, value)) = parsed else {
                return Err(format!("{FFI}: cannot read the constant in `{line}`"));
            };
            let numeric = value
                .strip_prefix("0x")
                .map_or_else(|| value.bytes().all(|x| x.is_ascii_digit()), |h| h.bytes().all(|x| x.is_ascii_hexdigit()));
            if !numeric || value.is_empty() {
                return Err(format!("{FFI}: the value of SQLITE_{name} is not a plain number: `{line}`"));
            }
            let _ = writeln!(c, "typedef char atlas_const_{name}[(SQLITE_{name} == {value}) ? 1 : -1];");
            consts += 1;
        } else if let Some(rest) = line.strip_prefix("pub fn sqlite3_") {
            let parsed = rest.split_once('(').and_then(|(name, r)| r.rsplit_once(')').map(|(args, ret)| (name, args, ret)));
            let Some((name, args, ret)) = parsed else {
                return Err(format!("{FFI}: cannot read the function in `{line}`"));
            };
            let ret = match ret.trim().trim_end_matches(';').trim().strip_prefix("->") {
                Some(t) => c_type(t)?,
                None => "void".to_string(),
            };
            let mut params = Vec::new();
            for arg in args.split(", ").filter(|a| !a.trim().is_empty()) {
                let Some((_, ty)) = arg.split_once(": ") else {
                    return Err(format!("{FFI}: cannot read the parameter `{arg}` in `{line}`"));
                };
                params.push(c_type(ty)?);
            }
            let params = if params.is_empty() { "void".to_string() } else { params.join(", ") };
            let _ = writeln!(c, "static {ret} (*const atlas_fn_{name})({params}) = sqlite3_{name};");
            fns += 1;
        }
    }
    if consts < 20 || fns < 30 {
        return Err(format!("{FFI}: read only {consts} constants and {fns} functions; the check no longer matches the file's layout"));
    }
    let width = cargo_var("CARGO_CFG_TARGET_POINTER_WIDTH")?;
    for (name, expr) in [
        ("int", "sizeof(int) == 4".to_string()),
        ("int64", "sizeof(sqlite3_int64) == 8 && sizeof(sqlite3_uint64) == 8".to_string()),
        ("double", "sizeof(double) == 8".to_string()),
        ("pointer", format!("sizeof(void *) * 8 == {width}")),
    ] {
        let _ = writeln!(c, "typedef char atlas_size_{name}[({expr}) ? 1 : -1];");
    }
    let source = b.out_dir.join("ffi_check.c");
    fs::write(&source, c).map_err(|e| format!("{}: {e}", source.display()))?;
    let mut cmd = tc.cc.command();
    cmd.current_dir(&b.manifest);
    match tc.style {
        Style::Msvc => {
            // C4133 and C4113 (a pointer of another type) are warnings to cl.
            cmd.args(["/nologo", "/c", "/W3", "/WX", "/we4113", "/we4133", "/we4028", "/we4047", "/Ivendor\\sqlite"]);
            cmd.arg(format!("/Fo{}", b.out_dir.join("ffi_check.obj").display())).arg(&source);
        }
        Style::Gnu => {
            cmd.args(&tc.target_flags).args(["-c", "-Werror", "-Wno-unused", "-Ivendor/sqlite", "-o"]);
            cmd.arg(b.out_dir.join("ffi_check.o")).arg(&source);
        }
    }
    run_tool("the C compiler", &tc.cc, cmd).map_err(|e| match e {
        // A compiler that never ran says nothing about the declarations.
        ToolError::NotRun(text) => text,
        ToolError::Failed(text) => format!("{FFI} does not match {HEADER} ({consts} constants, {fns} functions checked):\n{text}"),
    })?;
    log(&format!("{FFI} matches {HEADER}: {consts} constants, {fns} functions"));
    Ok(())
}
