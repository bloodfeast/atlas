//! The deny-list (`.env*`, keys, credential files) holds everywhere atlas
//! reads: vault walks, note reads and writes, search and grep, the CLI file
//! tools, and the files Claude Code read that atlas would keep as handles.

mod common;

use std::process::Command;

use atlas::hook_rewrite::Router;
use atlas::hooks::HookEvent;
use atlas::json;
use common::*;

const SECRET: &str = "zebraviolet4417";

/// A link from `link` to `target`, where this machine lets the test make
/// one (a file symlink on Windows needs a privilege CI may lack).
fn file_link(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(target, link).is_ok();
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).is_ok();
}

/// Brandon's ruling on review finding 5: the deny-list's name rules
/// (`credentials*`, `secrets.*`, `id_rsa*`) do not apply to `.md` notes or to
/// folder names; the extension rules, dot names and the canonical-target
/// check still apply everywhere.
#[test]
fn notes_and_folders_named_like_secrets_are_served_and_denied_targets_are_not() {
    let fx = Fx::new("deny-vault");
    let ok = "lanternquill2208";
    for rel in ["Secrets.md", "Credentials.md", "id_rsa rotation.md", "credentials/rotation plan.md", "secrets.d/notes.md"] {
        write(&fx.perf().join(rel), &format!("# Note\n\nword {ok}\n"));
    }
    // Denied by the rules that still hold: a folder named by extension.
    write(&fx.perf().join("keys.gpg").join("inside.md"), &format!("# Inside\n\nkey {SECRET}\n"));
    write(&fx.perf().join("Ops").join("plain.md"), "# Plain\n\nnothing to hide\n");
    // Notes that are links to a denied file: their names pass, their targets do not.
    let outside = fx.tmp.path().join("outside");
    write(&outside.join(".env"), &format!("API_TOKEN={SECRET}\n"));
    write(&outside.join("deploy.key"), &format!("KEY {SECRET}\n"));
    let mut links = Vec::new();
    for (name, target) in [("Secrets link.md", ".env"), ("Credentials link.md", "deploy.key")] {
        if file_link(&outside.join(target), &fx.perf().join("Ops").join(name)) {
            links.push(format!("Ops/{name}"));
        }
    }
    // A folder link (a junction on Windows needs no privilege) into the denied folder.
    #[cfg(windows)]
    let junction = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(fx.perf().join("Linked")).arg(fx.perf().join("keys.gpg")).output().is_ok_and(|o| o.status.success());
    #[cfg(unix)]
    let junction = std::os::unix::fs::symlink(fx.perf().join("keys.gpg"), fx.perf().join("Linked")).is_ok();
    assert!(junction, "a folder link can always be made");
    let mut s = fx.server();
    let e = call_err(&mut s, "read_note", json!({"vault": "perf", "path": "Linked/inside.md"}));
    assert!(e.contains("denied") && !e.contains(SECRET), "{e}");
    let listed = call(&mut s, "list_notes", json!({"vault": "perf"})).to_string();
    for rel in ["Secrets.md", "Credentials.md", "id_rsa rotation.md", "credentials/rotation plan.md", "secrets.d/notes.md"] {
        let out = call(&mut s, "read_note", json!({"vault": "perf", "path": rel})).to_string();
        assert!(out.contains(ok), "{rel}: {out}");
        assert!(listed.contains(rel.rsplit('/').next().unwrap()), "{rel} listed: {listed}");
    }
    let found = call(&mut s, "search", json!({"query": ok, "mode": "fts"})).to_string();
    assert!(found.contains("Secrets.md"), "{found}");
    call(&mut s, "create_note", json!({"vault": "perf", "path": "credentials/new.md", "content": "x"}));
    for (tool, args) in [
        ("search", json!({"query": SECRET})),
        ("search", json!({"query": SECRET, "mode": "fts"})),
        ("grep", json!({"pattern": SECRET})),
        ("list_notes", json!({"vault": "perf"})),
    ] {
        let out = call(&mut s, tool, args).to_string();
        assert!(!out.contains(SECRET) && !out.contains("inside.md"), "{tool}: {out}");
    }
    let e = call_err(&mut s, "read_note", json!({"vault": "perf", "path": "keys.gpg/inside.md"}));
    assert!(e.contains("deny-list"), "{e}");
    let e = call_err(&mut s, "create_note", json!({"vault": "perf", "path": "keys.gpg/new.md", "content": "x"}));
    assert!(e.contains("deny-list"), "{e}");
    let e = call_err(&mut s, "read_note", json!({"vault": "perf", "path": ".env.md"}));
    assert!(e.contains("dot"), "{e}");
    if links.is_empty() {
        eprintln!("no symlink privilege here: the canonical-target cases are checked by the CLI test's junction only");
    }
    for rel in &links {
        let e = call_err(&mut s, "read_note", json!({"vault": "perf", "path": rel}));
        assert!(!e.contains(SECRET), "{rel}: {e}");
    }
}

#[test]
fn cli_file_tools_refuse_denied_files() {
    let t = Temp::new("deny-cli");
    for name in [".env", "server.key", "credentials.json", "id_ed25519"] {
        let f = t.path().join(name);
        write(&f, &format!("API_TOKEN={SECRET}\n"));
        for tool in ["digest", "crop"] {
            let mut cmd = Command::new(bin());
            cmd.arg(tool).arg(&f);
            if tool == "crop" {
                cmd.arg(t.path().join("out.png"));
            }
            let out = cmd.output().unwrap();
            let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(!out.status.success() && !all.contains(SECRET) && all.contains("deny-list"), "{tool} {name}: {all}");
        }
    }
    // A file that is not denied still digests, notes named like secrets too.
    for name in ["notes.txt", "Secrets.md", "id_rsa rotation.md"] {
        let ok = t.path().join(name);
        write(&ok, "just a line\n");
        let out = Command::new(bin()).arg("digest").arg(&ok).output().unwrap();
        assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    }
    // A Secrets.md that is a link to a key file is refused by what it resolves to.
    let link = t.path().join("Secrets link.md");
    if file_link(&t.path().join("server.key"), &link) {
        for tool in ["digest", "crop"] {
            let mut cmd = Command::new(bin());
            cmd.arg(tool).arg(&link);
            if tool == "crop" {
                cmd.arg(t.path().join("out.png"));
            }
            let out = cmd.output().unwrap();
            let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(!out.status.success() && !all.contains(SECRET) && all.contains("deny-list"), "{tool} link: {all}");
        }
    } else {
        eprintln!("no symlink privilege here: path_denied's canonical check is covered by its unit test");
    }
}

#[test]
fn a_denied_file_claude_code_read_is_never_kept_as_a_handle() {
    let t = Temp::new("deny-read");
    let db = t.path().join("index.sqlite");
    let mut r = Router::with_db(&db, None);
    let env = t.path().join(".env");
    let body: String = (0..1000).map(|i| format!("KEY_{i}={SECRET}\n")).collect();
    write(&env, &body);
    let p = env.to_string_lossy().into_owned();
    let read = HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Read".into(),
        tool_input: json!({"file_path": p}),
        tool_response: json!({"type": "text", "file": {"filePath": p, "content": body, "numLines": 1000, "startLine": 1, "totalLines": 1000}}),
        ..HookEvent::default()
    };
    assert_eq!(r.post_tool_use(&read).unwrap(), None);
    let listed = Command::new(bin()).arg("--db").arg(&db).arg("slice").arg("--list").output().unwrap();
    assert!(!String::from_utf8_lossy(&listed.stdout).contains(".env"), "{}", String::from_utf8_lossy(&listed.stdout));
}

/// grep prints a long matched line cut to a window around the match, with
/// its full length named, not the whole line.
#[test]
fn grep_caps_long_lines_around_the_match() {
    let fx = Fx::new("grep-cap");
    let long = format!("{}needle{}", "a".repeat(3000), "b".repeat(3000));
    write(&fx.perf().join("Long.md"), &format!("# Long\n\n{long}\n"));
    let mut s = fx.server();
    let out = call(&mut s, "grep", json!({"pattern": "needle"})).to_string();
    assert!(out.contains("needle") && out.contains("[line of 6006 bytes cut]"), "{out}");
    assert!(out.len() < 1200, "{} bytes", out.len());
}
