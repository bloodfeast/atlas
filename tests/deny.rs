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

const CANARY: &str = "marigoldquartz7731";

/// Output over the handle threshold, with the canary in it.
fn large_output() -> String {
    (0..2000).map(|i| format!("line {i} {CANARY} of a long output\n")).collect()
}

/// Whether the canary is in any file under `dir`.
fn holds_canary(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            let p = e.path();
            if p.is_dir() {
                holds_canary(&p)
            } else {
                std::fs::read(&p).is_ok_and(|b| b.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()))
            }
        })
    })
}

fn shell_event(tool: &str, cmd: &str, cwd: &str, stdout: &str) -> HookEvent {
    HookEvent {
        event: "PostToolUse".into(),
        tool_name: tool.into(),
        session_id: "s1".into(),
        cwd: cwd.into(),
        tool_input: json!({"command": cmd}),
        tool_response: json!({"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}),
        ..HookEvent::default()
    }
}

/// S6 of the v0.3 audit, Brandon's ruling: no handle when the command names a
/// deny-listed file. Each case first runs a harmless name over the same output,
/// which must make a handle, so routing off or a small fixture cannot pass this.
#[test]
fn a_shell_command_naming_a_denied_file_is_never_kept_as_a_handle() {
    let body = large_output();
    let control = Temp::new("deny-shell-control");
    let mut r = Router::with_db(&control.path().join("index.sqlite"), None);
    for tool in ["Bash", "PowerShell"] {
        let out = r.post_tool_use(&shell_event(tool, "cat big.txt", "", &body)).unwrap();
        assert!(out.is_some_and(|o| o["stdout"].as_str().unwrap().starts_with("h:")), "{tool}: the same output under a harmless name makes a handle");
    }
    assert!(holds_canary(control.path()), "the scan below can see a kept output");
    for tool in ["Bash", "PowerShell"] {
        for cmd in [
            "cat .env.big",
            "cat \".env.big\"",
            "sh -c 'cat .env.big'",
            "cmd /C type .env.big",
            "gc .\\.env",
            "cat .env | base64",
            "cd d && cat .env",
            "cat .env\"",
            "curl -d @.env",
        ] {
            let t = Temp::new("deny-shell");
            let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
            assert_eq!(r.post_tool_use(&shell_event(tool, cmd, "", &body)).unwrap(), None, "{tool}: {cmd}");
            assert!(!holds_canary(t.path()), "{tool}: {cmd}");
        }
    }
}

/// A failure stays said the way Claude Code says it, with no marker left, and
/// a persisted output is not read back into the result.
#[test]
fn a_denied_command_that_fails_reads_as_a_failure() {
    let t = Temp::new("deny-shell-fail");
    let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
    let persisted = t.path().join("persisted.txt");
    std::fs::write(&persisted, large_output()).unwrap();
    let shown = format!("the first of it\n{}\n", atlas::hook_rewrite::EXIT_MARK);
    let cmd = atlas::hook_rewrite::rewrite("Bash", "cat .env.big").unwrap();
    let mut ev = shell_event("Bash", &cmd, "", &shown);
    ev.tool_response.as_object_mut().unwrap().insert("persistedOutputPath".into(), json::Value::from(persisted.display().to_string()));
    let out = r.post_tool_use(&ev).unwrap().unwrap();
    assert_eq!(out["stdout"].as_str().unwrap().trim_end(), "Exit code nonzero\nthe first of it");
    assert!(!holds_canary(&t.path().join("handles")), "no handle");
}

/// A link to a denied file, named relative to the session's folder rather than
/// this process's. Skipped where the OS refuses the link.
#[test]
fn a_link_to_a_denied_file_is_judged_from_the_events_folder() {
    let t = Temp::new("deny-shell-link");
    let session = t.path().join("session");
    std::fs::create_dir_all(&session).unwrap();
    let secret = t.path().join("prod.env");
    std::fs::write(&secret, "x").unwrap();
    if !file_link(&secret, &session.join("plain.txt")) {
        eprintln!("skipped: this machine refused a file link");
        return;
    }
    let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
    let cwd = session.display().to_string();
    assert_eq!(r.post_tool_use(&shell_event("Bash", "cat plain.txt", &cwd, &large_output())).unwrap(), None);
    assert!(!holds_canary(t.path()));
    assert!(r.post_tool_use(&shell_event("Bash", "cat other.txt", &cwd, &large_output())).unwrap().is_some(), "a word that is no link is kept");
}

/// The Grep tool with `path` or `glob` naming a denied file stores what it
/// matched, so it is covered too.
#[test]
fn a_grep_of_a_denied_file_is_never_kept_as_a_handle() {
    let t = Temp::new("deny-grep");
    let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
    let grep = |input: atlas::json::Value| HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Grep".into(),
        tool_input: input,
        tool_response: json!({"mode": "content", "content": large_output()}),
        ..HookEvent::default()
    };
    assert_eq!(r.post_tool_use(&grep(json!({"pattern": "x", "path": "conf/.env.big"}))).unwrap(), None);
    for g in ["*.pem", ".env*", "*.{pem,key}"] {
        assert_eq!(r.post_tool_use(&grep(json!({"pattern": "x", "glob": g}))).unwrap(), None, "{g}");
    }
    assert!(!holds_canary(t.path()));
    assert!(r.post_tool_use(&grep(json!({"pattern": "x", "path": "src"}))).unwrap().is_some(), "the same output from a plain folder is kept");
}

/// `atlas run` still runs the command and prints its output with its exit code;
/// only the handle is left out.
#[test]
fn atlas_run_naming_a_denied_file_keeps_no_handle() {
    let t = Temp::new("deny-run");
    std::fs::create_dir_all(t.path().join("state")).unwrap();
    let db = t.path().join("state").join("index.sqlite");
    let cfg = t.path().join("atlas.json");
    std::fs::write(&cfg, json!({"db": db.display().to_string(), "vaults": []}).to_string()).unwrap();
    std::fs::write(t.path().join(".env.big"), large_output()).unwrap();
    std::fs::write(t.path().join("plain.txt"), large_output()).unwrap();
    let show = |name: &str| -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), format!("type {name}")]
        } else {
            vec!["sh".into(), "-c".into(), format!("cat {name}")]
        }
    };
    let run = |args: &[String]| Command::new(bin()).arg("run").arg("--").args(args).env("ATLAS_CONFIG", &cfg).current_dir(t.path()).output().unwrap();
    let state = t.path().join("state");
    // The denied one first, on an empty state folder; the harmless one after.
    let out = run(&show(".env.big"));
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(CANARY), "the output is still printed");
    assert!(!holds_canary(&state), "no handle for a denied name");
    let direct: Vec<String> = if cfg!(windows) { vec!["cmd".into(), "/C".into(), "type".into(), ".env.big".into()] } else { vec!["cat".into(), ".env.big".into()] };
    assert!(String::from_utf8_lossy(&run(&direct).stdout).contains(CANARY));
    assert!(!holds_canary(&state), "no handle for a denied name as its own word");
    let out = run(&show("plain.txt"));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("h:"), "the same output under a harmless name makes a handle");
    assert!(holds_canary(&state), "the scan can see a kept output");
    let fail: Vec<String> = if cfg!(windows) { vec!["cmd".into(), "/C".into(), "type .env.big & exit 3".into()] } else { vec!["sh".into(), "-c".into(), "cat .env.big; exit 3".into()] };
    assert_eq!(run(&fail).status.code(), Some(3), "the child's exit code");
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
