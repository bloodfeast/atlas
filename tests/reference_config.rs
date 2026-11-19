//! REFERENCE-CONFIG: the published reference configuration
//! (`examples/reference/`, documented on `wiki/Reference-configuration.md`)
//! stays loadable and generic.
//!
//! * Every `<placeholder>` in the example files is listed in the wiki page's
//!   placeholder table.
//! * With the placeholders filled in, `atlas.json` (and the project file it
//!   names) loads through the real configuration parser, which refuses an
//!   unknown key anywhere and reads the `backlog` section with the backlog's
//!   own reader. A key the code drops or renames fails here.
//! * `.claude/settings.json` lets the atlas MCP tools run.
//! * No file names a machine path or a local build-queue key.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use atlas::config::Config;
use common::{write, Temp};

const FILES: &[&str] = &["atlas.json", "atlas.project.json", ".claude/settings.json"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn example(name: &str) -> String {
    let p = root().join("examples").join("reference").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The `<...>` placeholders of a text, leaving out a regex's named group
/// (`(?P<id>...)`).
fn placeholders(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        let named_group = rest[..i].ends_with("?P");
        rest = &rest[i..];
        let Some(j) = rest.find('>') else { break };
        if !named_group && !rest[1..j].contains('<') && !rest[1..j].contains('"') {
            out.insert(rest[..=j].to_string());
        }
        rest = &rest[j + 1..];
    }
    out
}

/// A placeholder's stand-in: a path for the folders, else a slug of its words.
fn fill(p: &str, home: &Path, work: &Path, memory: &Path) -> String {
    let slash = |d: &Path| d.to_string_lossy().replace('\\', "/");
    match p {
        "<ATLAS_HOME>" => slash(home),
        "<WORKSPACE>" => slash(work),
        "<CLAUDE_MEMORY_DIR>" => slash(memory),
        _ => {
            let s: String = p[1..p.len() - 1].chars().map(|c| if c.is_ascii_alphanumeric() || c == '*' { c.to_ascii_lowercase() } else { '-' }).collect();
            s.split('-').filter(|w| !w.is_empty()).collect::<Vec<_>>().join("-")
        }
    }
}

#[test]
fn every_placeholder_is_documented() {
    let page = std::fs::read_to_string(root().join("wiki").join("Reference-configuration.md")).unwrap();
    let table: BTreeSet<String> = page.lines().filter(|l| l.starts_with("| `<")).flat_map(placeholders).collect();
    for f in FILES {
        for p in placeholders(&example(f)) {
            assert!(table.contains(&p), "examples/reference/{f}: {p} is not in the placeholder table of wiki/Reference-configuration.md");
        }
    }
}

#[test]
fn the_example_loads_through_the_real_parser() {
    let t = Temp::new("refconfig");
    let (home, work, memory) = (t.path().join("home"), t.path().join("work"), t.path().join("memory"));
    let filled = |name: &str| {
        let mut text = example(name);
        for p in placeholders(&text) {
            text = text.replace(&p, &fill(&p, &home, &work, &memory));
        }
        assert!(placeholders(&text).is_empty(), "{name}: placeholders left after filling");
        text
    };
    write(&work.join("atlas.project.json"), &filled("atlas.project.json"));
    let config = t.path().join("atlas.json");
    let text = filled("atlas.json");
    write(&config, &text);

    let c = Config::load(&config).unwrap_or_else(|e| panic!("examples/reference/atlas.json does not load: {e}"));
    let projects = &c.user.projects;
    assert_eq!(projects.len(), 1, "one project");
    let p = &projects[0];
    assert_eq!(p.name, "project");
    assert_eq!(p.repos.len(), 1, "the project file's repo is merged in");
    let mem = p.body.sources.iter().find(|s| s.name == "memory").expect("a source named memory");
    assert_eq!(mem.names_kind.as_deref(), Some("memory"));
    assert_eq!(mem.id_keys.as_deref(), Some(&["name".to_string()][..]));
    assert_eq!(mem.exclude, ["index/*"]);
    let exports = &p.body.memory.as_ref().expect("the project's memory section").exports;
    assert!(exports.iter().any(|e| e.name == "memory" && e.shape == "memory"), "an export named as the memory source");
}

#[test]
fn the_claude_settings_allow_the_atlas_tools() {
    let v = atlas::json::parse(&example(".claude/settings.json")).unwrap();
    let allow = v.pointer("/permissions/allow").and_then(|a| a.as_array()).expect("permissions.allow");
    assert!(allow.iter().any(|a| a.as_str() == Some("mcp__atlas")), "permissions.allow names mcp__atlas");
}

#[test]
fn no_machine_paths_or_local_tool_keys() {
    for f in FILES {
        let text = example(f);
        let b = text.as_bytes();
        for i in 1..b.len().saturating_sub(1) {
            let drive = b[i] == b':' && b[i - 1].is_ascii_alphabetic() && (b[i + 1] == b'\\' || (b[i + 1] == b'/' && b.get(i + 2) != Some(&b'/')));
            assert!(!drive, "examples/reference/{f} names a drive path near byte {i}");
        }
        for word in ["/home/", "/Users/", "~/", "\\\\\\\\", "runq", "defer_lock"] {
            assert!(!text.contains(word), "examples/reference/{f} names {word:?}");
        }
    }
}
