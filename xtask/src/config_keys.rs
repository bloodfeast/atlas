//! Every configuration key the code accepts, read from the source: the key
//! lists of `Fields::new` calls, the key-list constants and the `lean.*`
//! names. One reader for two checks: `tests/docs.rs` (every key is on the
//! wiki; it includes this file by path) and `cargo xtask skill` (the atlas
//! skill's generated reference lists every key).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every key the source files below `src` accept, as full paths.
pub fn all(src: &Path) -> BTreeSet<String> {
    let mut files = Vec::new();
    rust_files(src, &mut files);
    let mut keys = BTreeSet::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap_or_else(|e| panic!("{}: {e}", f.display())).replace("\r\n", "\n");
        keys.extend(config_keys(&text));
        keys.extend(const_keys(&text));
        keys.extend(lean_keys(&text));
    }
    keys
}

/// The source files below `dir`.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The string a `let NAME = format!("...")` or `let NAME = "..."` before
/// `at` binds.
fn binding(text: &str, at: usize, name: &str) -> Option<String> {
    let head = &text[..at];
    let i = [format!("let {name} = format!(\""), format!("let {name} = \"")].iter().filter_map(|p| head.rfind(p.as_str()).map(|i| i + p.len())).max()?;
    Some(text[i..].split('"').next()?.to_string())
}

/// `sources[{i}].chunk` -> `sources[].chunk`: every `{...}` dropped.
fn unformat(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Every key the configuration parser checks, as its full path:
/// `Fields::new(AT, value, &["k", ...])` gives `AT.k` (`config` is the top).
/// A call whose key list is not written out (the top level and the project
/// entries, which pass a list built from constants) is covered by
/// [`const_keys`]. A label naming `{at}` that is a parameter is expanded
/// for both places a project is read: a `projects` entry (`projects[]`) and
/// the project file `atlas.project.json` (no prefix).
pub fn config_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("Fields::new(") {
        let at = from + i;
        let args = &text[at + "Fields::new(".len()..];
        from = at + 1;
        if args.starts_with("at: &str") {
            continue; // the definition
        }
        let Some(list) = args.find("&[") else { continue };
        let first_line_end = args.find('\n').unwrap_or(args.len());
        if list > first_line_end {
            continue; // the key list is a variable
        }
        let label = if let Some(r) = args.strip_prefix('"') {
            r.split('"').next().unwrap().to_string()
        } else if let Some(r) = args.strip_prefix("&format!(\"") {
            r.split('"').next().unwrap().to_string()
        } else if let Some(r) = args.strip_prefix('&') {
            let name: String = r.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            binding(text, at, &name).unwrap_or_else(|| panic!("Fields::new(&{name}, ...): cannot find what {name} names"))
        } else {
            continue; // a parameter with a variable key list
        };
        let labels: Vec<String> = if !label.contains("{at}") {
            vec![label]
        } else if let Some(b) = binding(text, at, "at") {
            vec![label.replace("{at}", &b)]
        } else {
            vec![label.replace("{at}", "projects[]"), label.replacen("{at}.", "", 1)]
        };
        let keys = &args[list + 2..];
        let keys = &keys[..keys.find(']').unwrap()];
        for label in labels {
            let label = unformat(&label);
            for k in keys.split('"').skip(1).step_by(2) {
                out.push(if label == "config" { k.to_string() } else { format!("{label}.{k}") });
            }
        }
    }
    out
}

/// The key-list constants the variable calls are built from: the top level
/// (`TOP_KEYS`, `USER_KEYS`), a `projects` entry (`ENTRY_KEYS`) and the
/// project file (`FILE_KEYS`).
fn const_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (name, prefix) in [("TOP_KEYS", ""), ("USER_KEYS", ""), ("ENTRY_KEYS", "projects[]."), ("FILE_KEYS", "")] {
        let Some(i) = text.find(&format!("const {name}: &[&str] =")) else { continue };
        let rest = &text[i + format!("const {name}: &[&str] =").len()..];
        let rest = &rest[rest.find("&[").unwrap() + 2..];
        let rest = &rest[..rest.find(']').unwrap()];
        out.extend(rest.split('"').skip(1).step_by(2).map(|k| format!("{prefix}{k}")));
    }
    out
}

/// The `lean.*` keys the code reads, from the backticked names in its docs.
fn lean_keys(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|s| s.starts_with("lean.") && !s.ends_with('.') && s.chars().all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'))
        .map(str::to_string)
        .collect()
}
