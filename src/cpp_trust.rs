//! When atlas may run scip-clang on a code root's C++.
//!
//! The compilation database scip-clang reads (`compile_commands.json`) is the
//! repository's own file, and it names the compiler of every command, which
//! scip-clang may run. So:
//!
//! - a root's `cpp.trust` must allow it ([`CppTrust`]): off by default;
//! - with `ask`, the database's content must have been approved ([`approve`],
//!   `atlas trust cpp ROOT`), and a change to it (a pull, a rebuild) needs a
//!   new approval;
//! - whatever the setting, a database whose compiler is a relative path or a
//!   file inside the root is refused ([`compilers_outside`]).
//!
//! The approvals are kept in [`TRUST_FILE`] in the code index's state folder.
//! A database the indexer found unapproved is noted in [`PENDING_FILE`], and
//! SessionStart asks the user about it once per content ([`session_line`]),
//! noting it in [`ASKED_FILE`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::CppTrust;
use crate::json::Value;

/// The file, in the code index's state folder, that holds the approved
/// databases: `{"ROOT:UNIT": "sha256"}`.
pub const TRUST_FILE: &str = "cpp-trust.json";

/// The databases the indexer refused as unapproved: `{"ROOT:UNIT": "sha256"}`.
pub const PENDING_FILE: &str = "cpp-pending.json";

/// The databases SessionStart has asked about: `{"ROOT:UNIT": "sha256"}`.
pub const ASKED_FILE: &str = "cpp-asked.json";

/// The database a C++ unit holds.
pub const DATABASE: &str = "compile_commands.json";

/// Why scip-clang may not run on the unit in `unit` (an absolute folder) of
/// the root `name` at `root`, or `None` when it may.
pub fn refusal(name: &str, root: &Path, trust: CppTrust, unit: &Path, state: &Path) -> Option<String> {
    if trust == CppTrust::Off {
        return Some(format!("scip-clang is off for this root (code.roots[].cpp.trust; `atlas trust cpp {name}` approves it)"));
    }
    let db = unit.join(DATABASE);
    let text = match std::fs::read_to_string(&db) {
        Ok(t) => t,
        Err(e) => return Some(format!("{DATABASE} unreadable: {e}")),
    };
    if let Err(why) = compilers_outside(&text, root) {
        return Some(why);
    }
    if trust == CppTrust::Ask {
        let (k, d) = (key(name, root, unit), digest(&text));
        if read(state, TRUST_FILE).get(&k) != Some(&d) {
            // Noted for SessionStart's question; a failed write only loses it.
            let mut pending = read(state, PENDING_FILE);
            if pending.get(&k) != Some(&d) {
                pending.insert(k, d);
                let _ = write(state, PENDING_FILE, pending);
            }
            return Some(format!("{DATABASE} is not approved since it last changed (`atlas trust cpp {name}`)"));
        }
    }
    None
}

/// Approves the databases of every C++ unit folder in `units` (absolute) of
/// the root `name` at `root`: their current content may run.
///
/// # Errors
///
/// A database that cannot be read, whose compilers are refused, or a trust
/// file that cannot be written.
pub fn approve(name: &str, root: &Path, units: &[PathBuf], state: &Path) -> std::result::Result<Vec<String>, String> {
    let mut map = read(state, TRUST_FILE);
    let mut done = Vec::new();
    for unit in units {
        let db = unit.join(DATABASE);
        let text = std::fs::read_to_string(&db).map_err(|e| format!("{}: {e}", db.display()))?;
        compilers_outside(&text, root).map_err(|why| format!("{}: {why}", db.display()))?;
        map.insert(key(name, root, unit), digest(&text));
        done.push(db.display().to_string());
    }
    write(state, TRUST_FILE, map)?;
    Ok(done)
}

/// SessionStart's question about the C++ databases the indexer refused as
/// unapproved, for the roots `(name, folder)` of `roots` that hold the
/// session's folder `cwd` or lie inside it. Each database's content is asked
/// about once: the asked ones are noted, so the next question comes when the
/// file changes. The user approves it, never the agent: the database names the
/// compilers scip-clang runs. `None` when nothing waits.
pub fn session_line(state: &Path, roots: &[(String, PathBuf)], cwd: &Path) -> Option<String> {
    let pending = read(state, PENDING_FILE);
    if pending.is_empty() {
        return None;
    }
    let (trusted, mut asked) = (read(state, TRUST_FILE), read(state, ASKED_FILE));
    let near = |r: &Path| cwd.starts_with(r) || r.starts_with(cwd);
    let mut names: Vec<&str> = Vec::new();
    for (k, d) in &pending {
        let Some((name, _)) = k.split_once(':') else { continue };
        if !roots.iter().any(|(n, r)| n == name && near(r)) || trusted.get(k) == Some(d) || asked.get(k) == Some(d) {
            continue;
        }
        asked.insert(k.clone(), d.clone());
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return None;
    }
    write(state, ASKED_FILE, asked).ok()?;
    let roots = names.join(", ");
    let run = names.iter().map(|n| format!("`atlas trust cpp {n}`")).collect::<Vec<_>>().join(", ");
    Some(format!(
        "atlas: the C++ of {roots} is only scanned (definitions, no references): its {DATABASE} is not approved since it last changed, and scip-clang runs the compilers that file names. Start your first reply, whatever the task, with one line telling the user so, and that, after checking the file, they approve it by running {run} themselves (in Claude Code with a leading `!`); then do the task. Never run it yourself. atlas asks again only when the file changes."
    ))
}

/// Flags whose value is the next argument, all of them safe to index with.
const VALUE_FLAGS: &[&str] = &[
    "-I",
    "-isystem",
    "-iquote",
    "-idirafter",
    "-include",
    "-imacros",
    "-D",
    "-U",
    "-o",
    "-x",
    "-MF",
    "-MT",
    "-MQ",
    "-target",
    "--target",
    "-arch",
];

/// The extensions of a source file a command may compile: C and C++.
const SOURCE_EXTS: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "h", "hh", "hpp", "hxx", "i", "ii", "ipp", "tpp"];

/// Whether one compiler argument is on the allowlist of what indexing
/// needs: include paths, macros, the language and standard, warnings and
/// optimisation and machine options, output and dependency files, and the
/// source file itself. Anything else, such as a toolchain or plugin path, a
/// wrapper, a response file or a pass-through to another tool, is not.
fn allowed(a: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "-I",
        "-isystem",
        "-iquote",
        "-idirafter",
        "-include",
        "-imacros",
        "-D",
        "-U",
        "-std=",
        "--std=",
        "-O",
        "-g",
        "-W",
        "-f",
        "-m",
        "-M",
        "-o",
        "-target",
        "--target=",
        "-arch",
        "-nostdinc",
        "-stdlib=",
        "-pthread",
    ];
    const EXACT: &[&str] =
        &["-c", "-E", "-S", "-pedantic", "-pedantic-errors", "-w", "-pipe", "-shared", "-static", "-ansi", "-v", "-H", "-P", "-C", "-x"];
    // Prefixes above that still reach another program or library.
    const DENIED: &[&str] = &[
        "-Wa,",
        "-Wl,",
        "-Wp,",
        "-fplugin",
        "-fpass-plugin",
        "-fuse-ld",
        "-fprofile-use",
        "-fprofile-sample-use",
        "-fintegrated-cc1",
        "-fno-integrated-cc1",
        "-fno-integrated-as",
        "-fcompare-debug",
        "-mllvm",
        "-gcc-toolchain",
        "-gcc-install-dir",
        "-fopenmp-targets",
        "--offload",
        "--cuda-",
        "-fgpu-",
        "-fsycl",
        "-foffload",
        "-fcuda",
        "-fhip",
        "-fopenmp-target",
    ];
    if !a.starts_with('-') {
        // A source file: a C or C++ one, so no other language's toolchain
        // is chosen by its extension (.cu, .hip, .cl, .s).
        let ext = a.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        return !a.starts_with('@') && SOURCE_EXTS.contains(&ext.as_str());
    }
    if DENIED.iter().any(|d| a.starts_with(d)) {
        return false;
    }
    EXACT.contains(&a) || PREFIXES.iter().any(|p| a.starts_with(p))
}

/// The first argument after the compiler that is not allowed ([`allowed`]);
/// a flag's separate value is taken with it.
fn refused_arg(args: &[String]) -> Option<&str> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if !allowed(a) {
            return Some(a);
        }
        if VALUE_FLAGS.contains(&a) {
            // The value is a path or a word: never a flag or a response file.
            match args.get(i + 1) {
                // A language, for -x: only C and C++ (CUDA, HIP and OpenCL
                // start a device toolchain).
                Some(v)
                    if a == "-x" && !matches!(v.as_str(), "c" | "c++" | "c-header" | "c++-header" | "c-cpp-output" | "c++-cpp-output") =>
                {
                    return Some(v.as_str());
                }
                Some(v) if !v.starts_with('-') && !v.starts_with('@') => i += 1,
                Some(v) => return Some(v.as_str()),
                None => return Some(a),
            }
        }
        i += 1;
    }
    None
}

/// Checks every command of a database. Each must name a compiler (an
/// absolute path, or a bare name found on `PATH`) that resolves outside the
/// root and outside the system's pseudo-file folders, and pass only allowed
/// arguments ([`allowed`]). A command that cannot be
/// read is refused, never skipped.
///
/// # Errors
///
/// The first command refused, and why; or a database that is no JSON array.
pub fn compilers_outside(text: &str, root: &Path) -> std::result::Result<(), String> {
    let v = crate::json::parse(text).map_err(|e| format!("{DATABASE} is no JSON: {e}"))?;
    let entries = v.as_array().ok_or_else(|| format!("{DATABASE} is no JSON array"))?;
    let root = std::fs::canonicalize(root).map_err(|e| format!("the root cannot be resolved: {e}"))?;
    for (i, e) in entries.iter().enumerate() {
        let args = arguments(e).ok_or_else(|| format!("{DATABASE} entry {i} has no readable command: not run"))?;
        let c = args.first().ok_or_else(|| format!("{DATABASE} entry {i} has an empty command: not run"))?;
        if let Some(bad) = refused_arg(&args[1..]) {
            return Err(format!("{DATABASE} passes {bad}, which is not on atlas's list of flags safe to index with: not run"));
        }
        let path = if c.contains('/') || c.contains('\\') {
            let p = PathBuf::from(c);
            if !p.is_absolute() {
                return Err(format!("{DATABASE} names the compiler {c}, a relative path: not run"));
            }
            p
        } else {
            crate::langs::on_path(c).ok_or_else(|| format!("{DATABASE} names the compiler {c}, not found on PATH: not run"))?
        };
        // clang-cl and cl read options spelled like file names (`/clang:`).
        let stem = Path::new(c).file_stem().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if stem == "cl" || stem.ends_with("clang-cl") {
            return Err(format!("{DATABASE} names the compiler {c}, a cl-style driver: not run"));
        }
        // Fail closed: a path that cannot be resolved is not run.
        let real = std::fs::canonicalize(&path)
            .map_err(|e| format!("{DATABASE} names the compiler {}, which cannot be resolved ({e}): not run", path.display()))?;
        if real.starts_with(&root) {
            return Err(format!("{DATABASE} names the compiler {}, inside the repository: not run", real.display()));
        }
        if ["/proc", "/dev", "/sys"].iter().any(|d| real.starts_with(d)) {
            return Err(format!("{DATABASE} names the compiler {}, a system pseudo-file: not run", real.display()));
        }
    }
    Ok(())
}

/// One database entry's command words: its `arguments`, or its `command`
/// split as a shell splits it ([`split_command`]); `None` when neither is
/// readable.
fn arguments(e: &Value) -> Option<Vec<String>> {
    if let Some(a) = e.get("arguments") {
        return a.as_array()?.iter().map(|x| x.as_str().map(str::to_string)).collect();
    }
    split_command(e.get("command")?.as_str()?)
}

/// Splits a command line as clang's compilation database does (POSIX shell
/// rules): words split on unquoted blanks; single quotes keep everything to
/// the next single quote; double quotes keep everything but a backslash
/// before `"`, `\` or `$`; a backslash outside quotes escapes the next
/// character; quoted and unquoted text next to each other join into one
/// word. `None` for an unterminated quote, any backslash, or a control
/// character other than a tab or a line break.
pub fn split_command(cmd: &str) -> Option<Vec<String>> {
    // A backslash, or a control character other than the blanks split on,
    // is where shells and compilation-database readers can disagree: such a
    // command is refused rather than guessed at.
    if cmd.contains('\\') || cmd.chars().any(|c| c.is_control() && !matches!(c, ' ' | '\t' | '\n' | '\r')) {
        return None;
    }
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut it = cmd.chars();
    while let Some(ch) = it.next() {
        match ch {
            ' ' | '\t' | '\n' | '\r' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\\' => {
                cur.push(it.next()?);
                in_word = true;
            }
            '\'' => {
                in_word = true;
                loop {
                    match it.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => match it.next()? {
                            c @ ('"' | '\\' | '$') => cur.push(c),
                            c => {
                                cur.push('\\');
                                cur.push(c);
                            }
                        },
                        c => cur.push(c),
                    }
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

fn key(name: &str, root: &Path, unit: &Path) -> String {
    let rel = unit.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    format!("{name}:{rel}")
}

fn digest(text: &str) -> String {
    crate::sha256::hex(text.as_bytes())
}

fn read(state: &Path, file: &str) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(state.join(file)) else { return BTreeMap::new() };
    let Ok(v) = crate::json::parse(&text) else { return BTreeMap::new() };
    v.as_object().map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()).unwrap_or_default()
}

fn write(state: &Path, file: &str, map: BTreeMap<String, String>) -> std::result::Result<(), String> {
    std::fs::create_dir_all(state).map_err(|e| e.to_string())?;
    let obj: crate::json::Map = map.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
    let tmp = state.join(format!("{file}.part"));
    std::fs::write(&tmp, format!("{}\n", Value::Object(obj))).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, state.join(file)).map_err(|e| e.to_string())
}

/// `atlas trust`'s usage.
pub const USAGE: &str = "usage: atlas trust cpp ROOT

Approves the compile_commands.json of each C++ unit of the code root ROOT,
as it is now: with code.roots[].cpp.trust \"ask\", scip-clang runs on it
until it changes. A database naming a compiler that is a relative path or a
file inside the repository is refused.";

/// `atlas trust cpp ROOT`.
///
/// # Errors
///
/// The usage; no such code root; a database that cannot be approved.
pub fn cli(args: &[String], code: Option<&crate::code::CodeSetup>) -> std::result::Result<String, String> {
    let [what, name] = args else { return Err(USAGE.to_string()) };
    if what != "cpp" {
        return Err(USAGE.to_string());
    }
    let code = code.ok_or("no code roots are configured")?;
    let root = code.roots.iter().find(|r| r.name == *name).ok_or_else(|| format!("no code root named {name}"))?;
    let walk = root.walk().map_err(|e| e.to_string())?;
    let units: Vec<PathBuf> = crate::code::units(&walk).iter().filter(|u| u.lang.name == "cpp").map(|u| root.root.join(&u.dir)).collect();
    if units.is_empty() {
        return Err(format!("{name} holds no {DATABASE} outside a skipped folder"));
    }
    let done = approve(&root.name, &root.root, &units, &code.dir)?;
    let mut out = format!("approved for scip-clang ({name}):\n");
    for d in done {
        out.push_str(&format!("  {d}\n"));
    }
    if root.cpp != CppTrust::Ask {
        out.push_str("note: code.roots[].cpp.trust is not \"ask\", so the approval is not read\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compiler_inside_the_repository_or_relative_is_refused() {
        let t = std::env::temp_dir().join(format!("atlas-cpp-trust-{}", std::process::id()));
        std::fs::create_dir_all(t.join("tools")).unwrap();
        let inside = t.join("tools").join("cc");
        std::fs::write(&inside, "").unwrap();
        // A compiler outside the repository, as /usr/bin/c++ would be.
        let sys = std::env::temp_dir().join(format!("atlas-cpp-trust-sys-{}", std::process::id()));
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(sys.join("c++"), "").unwrap();
        let cxx = sys.join("c++").to_string_lossy().into_owned();
        let db = |c: &str| format!(r#"[{{"directory": ".", "arguments": ["{c}", "-c", "a.cc"], "file": "a.cc"}}]"#);
        assert!(compilers_outside(&db(&cxx), &t).is_ok());
        assert!(compilers_outside(&db(&inside.to_string_lossy()), &t).unwrap_err().contains("inside the repository"));
        assert!(compilers_outside(&db("./tools/cc"), &t).unwrap_err().contains("relative path"));
        assert!(compilers_outside(&format!(r#"[{{"command": "\"{cxx}\" -c a.cc", "file": "a.cc"}}]"#), &t).is_ok());
        assert!(compilers_outside(r#"[{"command": "tools/cc -c a.cc", "file": "a.cc"}]"#, &t).is_err());
        // Shell quoting joins words: `/usr/bin/c""++` is `/usr/bin/c++`, and
        // a quoted relative path is still relative.
        let joined = format!("{}/c\\\"\\\"++", sys.display());
        assert!(compilers_outside(&format!(r#"[{{"command": "{joined} -c a.cc"}}]"#), &t).is_ok(), "{joined}");
        assert!(compilers_outside(r#"[{"command": "'./tools/cc' -c a.cc"}]"#, &t).unwrap_err().contains("relative path"));
        // An argument that loads another program, an unreadable entry, an
        // unterminated quote, an unresolvable compiler: all refused.
        for bad in [
            r#"[{"arguments": ["/usr/bin/c++", "-fplugin=/tmp/x.so", "-c", "a.cc"]}]"#,
            r#"[{"arguments": ["/usr/bin/c++", "-B/tmp/evil", "-c", "a.cc"]}]"#,
            r#"[{"arguments": ["/usr/bin/c++", "-Xclang", "-load", "-Xclang", "/tmp/x.so"]}]"#,
            r#"[{"arguments": ["/usr/bin/c++", "@flags.rsp"]}]"#,
            r#"[{"file": "a.cc"}]"#,
            r#"[{"command": "\"/usr/bin/c++ -c a.cc"}]"#,
            r#"[{"arguments": ["/no/such/compiler", "-c", "a.cc"]}]"#,
        ] {
            assert!(compilers_outside(bad, &t).is_err(), "{bad}");
        }
        assert_eq!(split_command(r#"a "b c" 'f g'h"#).unwrap(), ["a", "b c", "f gh"]);
        assert_eq!(split_command(r#"a d\ e"#), None, "a backslash is refused");
        assert_eq!(split_command("a\u{b}b"), None, "a vertical tab is refused");
        // Only flags safe to index with pass.
        let ok = format!(
            r#"[{{"arguments": ["{cxx}", "-std=c++20", "-O2", "-Iinclude", "-isystem", "/usr/include", "-DX=1", "-Wall", "-fPIC", "-c", "a.cc", "-o", "a.o"]}}]"#
        );
        assert!(compilers_outside(&ok, &t).is_ok(), "{ok}");
        for flag in [
            "-wrapper",
            "-specs=x",
            "--config=x",
            "-Xclang",
            "--rtlib=x",
            "-fplugin-arg-x",
            "-Wp,x",
            "-no-canonical-prefixes",
            "--prefix=x",
            "-gcc-toolchain=/tmp/x",
            "-fno-integrated-cc1",
            "-fcompare-debug",
            "--sysroot=/tmp/x",
            "-fopenmp-targets=nvptx64",
            "-xcuda",
        ] {
            let db = format!(r#"[{{"arguments": ["{cxx}", "{flag}", "-c", "a.cc"]}}]"#);
            assert!(compilers_outside(&db, &t).unwrap_err().contains("not on atlas's list"), "{flag}");
        }
        // -x takes only a C or C++ language; -isysroot is refused.
        for (flag, v) in [("-x", "cuda"), ("-x", "hip"), ("-isysroot", "/tmp/x")] {
            let db = format!(r#"[{{"arguments": ["{cxx}", "{flag}", "{v}", "-c", "a.cc"]}}]"#);
            assert!(compilers_outside(&db, &t).unwrap_err().contains("not on atlas's list"), "{flag} {v}");
        }
        // A source file of another language is refused.
        for src in ["kernel.cu", "a.hip", "k.cl", "start.s"] {
            let db = format!(r#"[{{"arguments": ["{cxx}", "-c", "{src}"]}}]"#);
            assert!(compilers_outside(&db, &t).unwrap_err().contains("not on atlas's list"), "{src}");
        }
        for flag in ["-fsycl", "-foffload=x", "-fcuda-is-device", "-fhip-new-launch-api", "-fopenmp-targets=x"] {
            let db = format!(r#"[{{"arguments": ["{cxx}", "{flag}", "-c", "a.cc"]}}]"#);
            assert!(compilers_outside(&db, &t).is_err(), "{flag}");
        }
        let db = format!(r#"[{{"arguments": ["{cxx}", "-x", "c++", "-c", "a.cc"]}}]"#);
        assert!(compilers_outside(&db, &t).is_ok(), "{db}");
        // A flag's value is never a flag or a response file.
        for v in ["@evil.rsp", "-fplugin=/tmp/x.so"] {
            let db = format!(r#"[{{"arguments": ["{cxx}", "-I", "{v}", "-c", "a.cc"]}}]"#);
            assert!(compilers_outside(&db, &t).unwrap_err().contains("not on atlas's list"), "{v}");
        }
        std::fs::write(sys.join("clang-cl"), "").unwrap();
        let cl = format!(r#"[{{"arguments": ["{}", "a.cc"]}}]"#, sys.join("clang-cl").display());
        assert!(compilers_outside(&cl, &t).unwrap_err().contains("cl-style"));
        std::fs::remove_dir_all(&t).ok();
        std::fs::remove_dir_all(&sys).ok();
    }
}
