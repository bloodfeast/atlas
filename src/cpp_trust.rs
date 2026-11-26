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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::CppTrust;
use crate::json::Value;

/// The file, in the code index's state folder, that holds the approved
/// databases: `{"ROOT:UNIT": "sha256"}`.
pub const TRUST_FILE: &str = "cpp-trust.json";

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
    if trust == CppTrust::Ask && approved(state, &key(name, root, unit)).as_deref() != Some(digest(&text).as_str()) {
        return Some(format!("{DATABASE} is not approved since it last changed (`atlas trust cpp {name}`)"));
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
    let mut map = read(state);
    let mut done = Vec::new();
    for unit in units {
        let db = unit.join(DATABASE);
        let text = std::fs::read_to_string(&db).map_err(|e| format!("{}: {e}", db.display()))?;
        compilers_outside(&text, root).map_err(|why| format!("{}: {why}", db.display()))?;
        map.insert(key(name, root, unit), digest(&text));
        done.push(db.display().to_string());
    }
    std::fs::create_dir_all(state).map_err(|e| e.to_string())?;
    let obj: crate::json::Map = map.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
    let tmp = state.join(format!("{TRUST_FILE}.part"));
    std::fs::write(&tmp, format!("{}\n", Value::Object(obj))).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, state.join(TRUST_FILE)).map_err(|e| e.to_string())?;
    Ok(done)
}

/// Checks every compiler a database names: an absolute path outside the
/// root, or a bare name found on `PATH` outside the root.
///
/// # Errors
///
/// The first compiler refused, and why; or a database that is no JSON array.
pub fn compilers_outside(text: &str, root: &Path) -> std::result::Result<(), String> {
    let v = crate::json::parse(text).map_err(|e| format!("{DATABASE} is no JSON: {e}"))?;
    let entries = v.as_array().ok_or_else(|| format!("{DATABASE} is no JSON array"))?;
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for e in entries {
        let Some(c) = compiler(e) else { continue };
        let path = if c.contains('/') || c.contains('\\') {
            let p = PathBuf::from(&c);
            if !p.is_absolute() {
                return Err(format!("{DATABASE} names the compiler {c}, a relative path: not run"));
            }
            p
        } else {
            match crate::langs::on_path(&c) {
                Some(p) => p,
                None => return Err(format!("{DATABASE} names the compiler {c}, not found on PATH: not run")),
            }
        };
        let real = std::fs::canonicalize(&path).unwrap_or(path);
        if real.starts_with(&root) {
            return Err(format!("{DATABASE} names the compiler {}, inside the repository: not run", real.display()));
        }
    }
    Ok(())
}

/// The compiler of one database entry: the first of its `arguments`, or the
/// first word of its `command` (a leading quoted word kept whole).
fn compiler(e: &Value) -> Option<String> {
    if let Some(first) = e.get("arguments").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str) {
        return Some(first.to_string());
    }
    let cmd = e.get("command").and_then(Value::as_str)?.trim_start();
    if let Some(rest) = cmd.strip_prefix('"') {
        return rest.split_once('"').map(|(w, _)| w.to_string());
    }
    cmd.split_whitespace().next().map(str::to_string)
}

fn key(name: &str, root: &Path, unit: &Path) -> String {
    let rel = unit.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    format!("{name}:{rel}")
}

fn digest(text: &str) -> String {
    crate::sha256::hex(text.as_bytes())
}

fn approved(state: &Path, key: &str) -> Option<String> {
    read(state).remove(key)
}

fn read(state: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(state.join(TRUST_FILE)) else { return BTreeMap::new() };
    let Ok(v) = crate::json::parse(&text) else { return BTreeMap::new() };
    v.as_object().map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()).unwrap_or_default()
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
        let db = |c: &str| format!(r#"[{{"directory": ".", "arguments": ["{c}", "-c", "a.cc"], "file": "a.cc"}}]"#);
        assert!(compilers_outside(&db("/usr/bin/c++"), &t).is_ok());
        assert!(compilers_outside(&db(&inside.to_string_lossy()), &t).unwrap_err().contains("inside the repository"));
        assert!(compilers_outside(&db("./tools/cc"), &t).unwrap_err().contains("relative path"));
        assert!(compilers_outside(r#"[{"command": "\"/usr/bin/c++\" -c a.cc", "file": "a.cc"}]"#, &t).is_ok());
        assert!(compilers_outside(r#"[{"command": "tools/cc -c a.cc", "file": "a.cc"}]"#, &t).is_err());
        std::fs::remove_dir_all(&t).ok();
    }
}
