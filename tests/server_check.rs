//! The prompt hook that says when this session's atlas server is gone
//! (`atlas --server-check`, `atlas::server_check`): whose markers it reads,
//! its verdict on them, and what it prints.

mod common;

use atlas::server_check::{check_in, check_once_in, hook_output, is_claude_code, mark_in, session_pid, tell, verdict, Verdict};
use common::*;

fn procs(rows: &[(u32, u32, &str)]) -> Vec<(u32, u32, String)> {
    rows.iter().map(|(a, b, n)| (*a, *b, (*n).to_string())).collect()
}

/// A process that has exited: its tag names no running process.
fn dead_tag() -> String {
    let mut c = std::process::Command::new(std::env::current_exe().unwrap());
    let child = c.arg("--list").stdout(std::process::Stdio::null()).spawn().unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    format!("{pid} -")
}

#[test]
fn claude_code_is_known_by_its_name_or_its_version() {
    for n in ["claude", "Claude.EXE", "node", "bun", "2.1.332", "/home/u/.local/share/claude/versions/2.1.330", "10.0"] {
        assert!(is_claude_code(n), "{n}");
    }
    for n in ["2", "1..2", ".1", "1.", "python3.12", "atlas", "bash", "cargo", ""] {
        assert!(!is_claude_code(n), "{n}");
    }
}

/// The nearest Claude Code above, past shells: a background session's
/// server and its hooks name the session's own process, not the
/// `claude daemon` every background session shares.
#[test]
fn the_session_is_the_nearest_claude_code_above() {
    // daemon (10) -> pty host (20) -> session (30) -> server (40), and the
    // session -> sh -c (35) -> hook (45).
    let bg = procs(&[(1, 0, "systemd"), (10, 1, "claude"), (20, 10, "2.1.332"), (30, 20, "2.1.332"), (35, 30, "sh"), (40, 30, "atlas"), (45, 35, "atlas")]);
    assert_eq!(session_pid(&bg, 40, |_| None), Some(30));
    assert_eq!(session_pid(&bg, 45, |_| None), Some(30));
    // An npm install runs as node; a terminal's native install as claude.
    assert_eq!(session_pid(&procs(&[(5, 1, "zsh"), (30, 5, "node"), (40, 30, "atlas")]), 40, |_| None), Some(30));
    assert_eq!(session_pid(&procs(&[(30, 4, "claude.exe"), (35, 30, "cmd.exe"), (40, 35, "atlas.exe")]), 40, |_| None), Some(30));
    // Something else between (cargo, timeout), nothing above, a loop or an
    // empty table: no session.
    assert_eq!(session_pid(&procs(&[(30, 1, "claude"), (35, 30, "cargo"), (40, 35, "atlas")]), 40, |_| None), None);
    assert_eq!(session_pid(&procs(&[(35, 1, "sh"), (40, 35, "atlas")]), 40, |_| None), None);
    assert_eq!(session_pid(&procs(&[(35, 36, "sh"), (36, 35, "bash"), (40, 35, "atlas")]), 40, |_| None), None);
    assert_eq!(session_pid(&[], 40, |_| None), None);
    // A parent that started after its child took a reused pid.
    let at = |pid: u32| [(30, 50u64), (40, 20)].iter().find(|s| s.0 == pid).map(|s| s.1);
    assert_eq!(session_pid(&procs(&[(30, 1, "claude"), (40, 30, "atlas")]), 40, at), None);
}

#[test]
fn the_verdict_is_gone_only_when_every_marker_names_a_stopped_server() {
    assert!(matches!(verdict(&[]), Verdict::Unknown(_)));
    assert_eq!(verdict(&[Some(false), Some(true)]), Verdict::Running);
    assert_eq!(verdict(&[Some(true), None]), Verdict::Running);
    assert_eq!(verdict(&[Some(false), Some(false)]), Verdict::Gone(2));
    assert!(matches!(verdict(&[Some(false), None]), Verdict::Unknown(_)));
}

/// The markers, on disk: a stopped server's is kept and read as gone, a
/// running one's as running, another session's are not read, and a
/// starting server removes the markers of a Claude Code that has exited.
#[test]
fn markers_tell_a_stopped_server_from_a_running_one() {
    let t = Temp::new("server-check");
    let dir = t.path().join("live-servers");
    let me = atlas::restart::server_tag();
    let (gone, other) = (dead_tag(), dead_tag());
    assert!(matches!(check_in(&dir, &me), Verdict::Unknown(_)), "no folder yet");
    let first = mark_in(&dir, &me, &gone).unwrap();
    assert_eq!(check_in(&dir, &me), Verdict::Gone(1));
    // A reconnect starts a second server for the same Claude Code.
    mark_in(&dir, &me, &me).unwrap();
    assert_eq!(check_in(&dir, &me), Verdict::Running);
    // Another Claude Code's markers are not this one's.
    assert!(matches!(check_in(&dir, "4000000000 -"), Verdict::Unknown(_)));
    // A Claude Code that exited: its marker goes at the next server start;
    // a running Claude Code's stopped server's marker stays.
    let stale = mark_in(&dir, &other, &other).unwrap();
    mark_in(&dir, &me, &me).unwrap();
    assert!(!stale.exists(), "{}", stale.display());
    assert!(first.exists(), "{}", first.display());
}

/// The notice is said once per loss: not again at the next prompt, again
/// when another server of the session has stopped since, never while one
/// runs, and the record of what was told is not read as a marker.
#[test]
fn the_notice_is_said_once_for_each_loss() {
    assert!(tell(None, "a"));
    assert!(!tell(Some("a"), "a"));
    assert!(tell(Some("a"), "a\nb"));
    let t = Temp::new("server-check-once");
    let dir = t.path().join("live-servers");
    let me = atlas::restart::server_tag();
    mark_in(&dir, &me, &dead_tag()).unwrap();
    assert_eq!(check_once_in(&dir, &me), (Verdict::Gone(1), true));
    assert_eq!(check_once_in(&dir, &me), (Verdict::Gone(1), false));
    assert_eq!(check_in(&dir, &me), Verdict::Gone(1), "the record is no marker");
    // A reconnect that runs: nothing said; it stops too: said again.
    mark_in(&dir, &me, &me).unwrap();
    assert_eq!(check_once_in(&dir, &me), (Verdict::Running, false));
    mark_in(&dir, &me, &dead_tag()).unwrap();
    let only_dead: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| std::fs::read_to_string(e.path()).unwrap().trim() == me.trim()).collect();
    for e in only_dead {
        std::fs::remove_file(e.path()).unwrap();
    }
    assert_eq!(check_once_in(&dir, &me), (Verdict::Gone(2), true));
    assert_eq!(check_once_in(&dir, &me), (Verdict::Gone(2), false));
}

#[test]
fn the_hook_prints_the_notice_only_when_the_server_is_gone() {
    let out = hook_output(&Verdict::Gone(1)).unwrap();
    let v = atlas::json::parse(&out).unwrap();
    let user = v["systemMessage"].as_str().unwrap();
    assert!(user.contains("is not running") && user.contains("/mcp") && user.contains("claude --continue"), "{user}");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"].as_str(), Some("UserPromptSubmit"));
    assert!(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("/mcp"), "{out}");
    assert_eq!(hook_output(&Verdict::Running), None);
    assert_eq!(hook_output(&Verdict::Unknown("x".into())), None);
}

/// The hook itself: with no Claude Code above it (a test's process) and no
/// configuration, it reads its input, prints nothing and exits 0.
#[test]
fn the_hook_with_no_session_says_nothing_and_succeeds() {
    use std::io::Write;
    let t = Temp::new("server-check-cli");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"))
        .arg("--server-check")
        .env("ATLAS_HOME", t.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(br#"{"hook_event_name":"UserPromptSubmit","prompt":"hi"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    assert!(!t.path().join("live-servers").exists());
}
