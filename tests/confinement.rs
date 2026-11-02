//! Every confinement rejection, through the real tool surface.

mod common;

use std::fs;
use std::path::Path;

use common::*;
use atlas::json;

const BAD_PATHS: &[(&str, &str)] = &[
    ("absolute unix", "/etc/passwd.md"),
    ("absolute backslash", "\\Windows\\x.md"),
    ("UNC", "\\\\server\\share\\x.md"),
    ("drive prefix", "C:/Windows/x.md"),
    ("drive relative", "C:x.md"),
    ("parent", "../Field Notes/Missions/M1 Hold.md"),
    ("parent inside", "Ideas/../Index.md"),
    ("dot component", "./Index.md"),
    ("NUL", "Index\0.md"),
    ("NTFS stream", "Index.md:hidden"),
    ("NTFS stream data", "Index.md::$DATA"),
    ("dot folder obsidian", ".obsidian/secret.md"),
    ("dot folder git", ".git/x.md"),
    ("dot folder trash", ".trash/x.md"),
    ("dot file", "Ideas/.hidden.md"),
    ("non-md", ".obsidian/app.json"),
    ("non-md plain", "Index.txt"),
    ("trailing dot", "Index.md."),
    ("trailing space", "Index.md "),
    ("device name", "CON.md"),
    ("device folder", "nul/x.md"),
    ("wildcard char", "Ide*s/x.md"),
    ("empty", ""),
    ("doubled separator", "Ideas//PI-002 Throttle.md"),
];

#[test]
fn reads_refuse_every_bad_path() {
    let fx = Fx::new("conf-read");
    let mut s = fx.server();
    for (why, p) in BAD_PATHS {
        let e = call_err(&mut s, "read_note", json!({"vault": "perf", "path": p}));
        assert!(e.contains("path rejected") || e.contains("invalid"), "{why}: {p:?} gave {e}");
    }
}

#[test]
fn writes_refuse_every_bad_path() {
    let fx = Fx::new("conf-write");
    let mut s = fx.server();
    for (why, p) in BAD_PATHS {
        let e = call_err(&mut s, "create_note", json!({"vault": "perf", "path": p, "content": "x"}));
        assert!(e.contains("path rejected"), "{why}: {p:?} gave {e}");
        let e = call_err(&mut s, "append_to_note", json!({"vault": "perf", "path": p, "text": "x"}));
        assert!(e.contains("path rejected"), "{why}: {p:?} gave {e}");
    }
    // Nothing was created anywhere next to the vaults.
    assert!(!fx.tmp.path().join("x.md").exists());
    assert_eq!(fs::read_to_string(fx.perf().join(".obsidian/secret.md")).unwrap(), "hidden");
}

#[test]
fn folders_and_globs_are_confined() {
    let fx = Fx::new("conf-folder");
    let mut s = fx.server();
    for f in ["..", "../Field Notes", "/", "C:/", ".obsidian", "a/../.."] {
        let e = call_err(&mut s, "list_notes", json!({"vault": "perf", "folder": f}));
        assert!(e.contains("path rejected"), "{f}: {e}");
    }
    let e = call_err(&mut s, "next_id", json!({"vault": "perf", "folder": "../x", "prefix": "PI-"}));
    assert!(e.contains("path rejected"), "{e}");
    let all = call(&mut s, "list_notes", json!({"vault": "perf"}));
    let paths: Vec<&str> = all["notes"].as_array().unwrap().iter().map(|n| n["path"].as_str().unwrap()).collect();
    assert!(!paths.iter().any(|p| p.contains("obsidian")), "{paths:?}");
    let e = call_err(&mut s, "read_note", json!({"vault": "nope", "path": "Index.md"}));
    assert!(e.contains("not found"), "{e}");
}

#[test]
fn no_delete_or_rename_tool_exists() {
    let names: Vec<String> = atlas::tools::tool_list().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    for n in &names {
        for bad in ["delete", "remove", "rename", "move", "trash", "fetch", "http", "url"] {
            assert!(!n.contains(bad), "tool {n} looks like {bad}");
        }
    }
    let fx = Fx::new("conf-tools");
    let mut s = fx.server();
    for t in ["delete_note", "rename_note", "move_note", "fetch"] {
        let e = call_err(&mut s, t, json!({}));
        assert!(e.contains("not found"), "{t}: {e}");
    }
}

#[cfg(windows)]
fn dir_link(link: &Path, target: &Path) -> bool {
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn dir_link(link: &Path, target: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[test]
fn junction_escape_is_refused() {
    let fx = Fx::new("conf-junction");
    let outside = fx.tmp.path().join("outside");
    write(&outside.join("secret.md"), "outside secret");
    let link = fx.perf().join("escape");
    assert!(dir_link(&link, &outside), "could not create a junction or symlink for the test");
    // A link inside the vault that points at a dot folder.
    let dotlink = fx.perf().join("dots");
    assert!(dir_link(&dotlink, &fx.perf().join(".obsidian")));
    let mut s = fx.server();

    for p in ["escape/secret.md", "dots/secret.md"] {
        let e = call_err(&mut s, "read_note", json!({"vault": "perf", "path": p}));
        assert!(e.contains("path rejected"), "{p}: {e}");
        let e = call_err(&mut s, "append_to_note", json!({"vault": "perf", "path": p, "text": "x"}));
        assert!(e.contains("path rejected"), "{p}: {e}");
    }
    for p in ["escape/new.md", "dots/new.md"] {
        let e = call_err(&mut s, "create_note", json!({"vault": "perf", "path": p, "content": "x"}));
        assert!(e.contains("path rejected"), "{p}: {e}");
    }
    let e = call_err(&mut s, "list_notes", json!({"vault": "perf", "folder": "escape"}));
    assert!(e.contains("path rejected"), "{e}");
    // The walker does not follow links, so nothing outside is indexed or searchable.
    let found = call(&mut s, "search", json!({"query": "outside secret"}));
    assert_eq!(found["results"].as_array().unwrap().len(), 0);
    assert!(!outside.join("new.md").exists());
    assert_eq!(fs::read_to_string(outside.join("secret.md")).unwrap(), "outside secret");
}

#[cfg(unix)]
fn file_link(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn file_link(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

/// A note that is a file link is followed and checked where it leads: out of
/// the vault, or into a dot folder, is refused for reads and writes; a link
/// to a note inside the vault is served.
#[test]
fn file_symlink_escape_is_refused() {
    let fx = Fx::new("conf-filelink");
    let outside = fx.tmp.path().join("outside.md");
    write(&outside, "outside");
    write(&fx.perf().join(".obsidian").join("hidden.md"), "hidden");
    write(&fx.perf().join("Ideas").join("inside.md"), "inside");
    let links = [
        (fx.perf().join("link.md"), outside.clone()),
        (fx.perf().join("Ideas").join("deep link.md"), outside.clone()),
        (fx.perf().join("dotlink.md"), fx.perf().join(".obsidian").join("hidden.md")),
        (fx.perf().join("ok link.md"), fx.perf().join("Ideas").join("inside.md")),
    ];
    for (l, t) in &links {
        if let Err(e) = file_link(l, t) {
            // Windows needs Developer Mode or the privilege for file links;
            // without it no such link can exist in a vault either.
            assert!(cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied, "{e}");
            eprintln!("file links unavailable here ({e}); nothing to refuse");
            return;
        }
    }
    let mut s = fx.server();
    for p in ["link.md", "Ideas/deep link.md", "dotlink.md"] {
        let e = call_err(&mut s, "read_note", json!({"vault": "perf", "path": p}));
        assert!(e.contains("path rejected"), "{p}: {e}");
        let e = call_err(&mut s, "append_to_note", json!({"vault": "perf", "path": p, "text": "x"}));
        assert!(e.contains("path rejected"), "{p}: {e}");
    }
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
    let r = call(&mut s, "read_note", json!({"vault": "perf", "path": "ok link.md"}));
    assert!(r.to_string().contains("inside"), "{r}");
}

#[test]
fn size_caps() {
    let fx = Fx::new("conf-size");
    let mut s = fx.server();
    let big = "x".repeat(atlas::fsio::MAX_WRITE + 1);
    let e = call_err(&mut s, "create_note", json!({"vault": "perf", "path": "big.md", "content": big}));
    assert!(e.contains("too large"), "{e}");
    write(&fx.perf().join("huge.md"), &"y".repeat((atlas::fsio::MAX_READ + 10) as usize));
    let e = call_err(&mut s, "read_note", json!({"vault": "perf", "path": "huge.md"}));
    assert!(e.contains("too large"), "{e}");
    let e = call_err(&mut s, "search", json!({"query": "x", "limit": 201}));
    assert!(e.contains("invalid"), "{e}");
}

#[test]
fn unknown_arguments_are_refused() {
    let fx = Fx::new("conf-args");
    let mut s = fx.server();
    let e = call_err(&mut s, "read_note", json!({"vault": "perf", "path": "Index.md", "url": "http://x"}));
    assert!(e.contains("unknown argument"), "{e}");
}
