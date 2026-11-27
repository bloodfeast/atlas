//! The prompt hook that says when this session's atlas server is gone
//! (`atlas --server-check`, `atlas::server_check`): whose markers it reads,
//! its verdict on them, and what it prints.

mod common;

use atlas::server_check::{check_in, check_once_in, hook_output, is_claude_code, mark_in, origin, refusal, session_pid, socket_in, socket_of_marker, tell, verdict, Origin, Verdict};
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

/// A hook is Claude Code's child (or a `sh -c`'s that did not exec); the
/// model's Bash tool runs `atlas --hook` under the tool's own shell, which
/// is dropped with a reason. Nothing is judged without a Claude Code above.
#[test]
fn a_hook_run_through_the_bash_tools_shell_is_dropped() {
    // The measured shapes: session (30) -> hook (40); session -> bash -c
    // (35) -> atlas --hook (45), and a pipe's subshell under it (46 -> 47).
    let t = procs(&[(1, 0, "systemd"), (30, 1, "2.1.330"), (35, 30, "bash"), (36, 30, "sh"), (40, 30, "atlas"), (45, 35, "atlas"), (46, 35, "bash"), (47, 46, "atlas"), (48, 36, "atlas")]);
    assert_eq!(origin(&t, 40, |_| None), Origin::Runner);
    assert_eq!(origin(&t, 48, |_| None), Origin::Runner, "a sh -c that did not exec");
    assert_eq!(origin(&t, 45, |_| None), Origin::Through("bash".into()));
    assert_eq!(origin(&t, 47, |_| None), Origin::Through("bash".into()));
    assert!(refusal(&origin(&t, 45, |_| None)).is_some_and(|w| w.contains("`bash`") && w.contains("dropped")));
    for me in [40, 48] {
        assert_eq!(refusal(&origin(&t, me, |_| None)), None, "{me}");
    }
    // A test or a terminal: no Claude Code above, so it runs.
    let term = procs(&[(5, 1, "zsh"), (6, 5, "cargo"), (40, 6, "atlas")]);
    assert_eq!(origin(&term, 40, |_| None), Origin::NoClaudeCode);
    assert_eq!(refusal(&Origin::NoClaudeCode), None);
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
    // A stopped server's socket goes too; a running one's stays.
    let (dead_sock, live_sock) = (socket_in(&dir, &other), socket_in(&dir, &me));
    std::fs::write(&dead_sock, "").unwrap();
    std::fs::write(&live_sock, "").unwrap();
    mark_in(&dir, &me, &me).unwrap();
    assert!(!stale.exists(), "{}", stale.display());
    assert!(!dead_sock.exists(), "{}", dead_sock.display());
    assert!(live_sock.exists(), "{}", live_sock.display());
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

/// The longest data folder the default layout gives, with a 32-byte user
/// name (the longest most systems allow), and the most bytes a socket's
/// path may hold there (`sun_path` less its closing NUL).
const LONGEST: [(&str, &str, usize); 2] = [
    ("linux", "/home/{user}/.local/share/atlas/live-servers", 107),
    ("macos", "/Users/{user}/Library/Application Support/atlas/live-servers", 103),
];

/// The largest pid and a macOS start time (16 digits of microseconds).
const SERVER: &str = "4294967295 1764000000000000";

/// A hook socket's path fits the socket limit on every Unix target, for the
/// longest user name, and the marker of the same server names the same socket.
#[test]
fn the_hook_socket_fits_the_socket_path_limit_for_a_long_user_name() {
    let user = "u".repeat(32);
    for (os, dir, limit) in LONGEST {
        let dir = std::path::PathBuf::from(dir.replace("{user}", &user));
        let sock = socket_in(&dir, SERVER);
        let len = sock.as_os_str().len();
        assert!(len <= limit, "{os}: {} is {len} bytes, over {limit}", sock.display());
        let marker = format!("1-2.{}", SERVER.replace(' ', "-"));
        assert_eq!(socket_of_marker(&dir, &marker), Some(sock), "{os}");
    }
}

/// A socket binds at the longest such path on this host: a folder padded to
/// the length of the longest data folder for this OS.
#[cfg(unix)]
#[test]
fn a_socket_binds_at_the_longest_data_folder() {
    let os = if cfg!(target_os = "macos") { "macos" } else { "linux" };
    let (_, dir, _) = LONGEST.iter().find(|(o, ..)| *o == os).unwrap();
    let want = dir.replace("{user}", &"u".repeat(32)).len();
    let t = Temp::new("sock-len");
    let base = t.path().as_os_str().len();
    assert!(base + 2 <= want, "the temp folder {} is longer than {want} bytes", t.path().display());
    let padded = t.path().join("p".repeat(want - base - 1));
    std::fs::create_dir_all(&padded).unwrap();
    assert_eq!(padded.as_os_str().len(), want);
    let sock = socket_in(&padded, SERVER);
    let l = std::os::unix::net::UnixListener::bind(&sock).unwrap_or_else(|e| panic!("{}: {e}", sock.display()));
    drop(l);
}

/// Where a command hook's event goes, by this session's markers: with none,
/// or a running server with no socket, it runs in the hook's own process; with
/// only stopped servers it is gone; a running server's socket answers it, and
/// one that takes it and says nothing in time is late.
#[cfg(unix)]
#[test]
fn a_hook_goes_to_the_running_servers_socket_or_runs_here() {
    use atlas::hook_socket::{relay_in, Route};
    use std::io::{Read, Write};
    use std::time::Duration;
    let t = Temp::new("relay");
    let dir = t.path().join("live-servers");
    let me = atlas::restart::server_tag();
    let wait = Duration::from_secs(5);
    let input = br#"{"hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
    assert!(matches!(relay_in(&dir, &me, input, wait), Route::InProcess(_)), "no folder");
    mark_in(&dir, &me, &dead_tag()).unwrap();
    assert_eq!(relay_in(&dir, &me, input, wait), Route::Gone);
    mark_in(&dir, &me, &me).unwrap();
    assert!(matches!(relay_in(&dir, &me, input, wait), Route::InProcess(_)), "a running server with no socket");

    // A server that echoes what it read.
    let sock = socket_in(&dir, &me);
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let server = std::thread::spawn(move || {
        let (mut c, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        c.write_all(b"echo:").unwrap();
        c.write_all(&got).unwrap();
        drop(c);
        // The next takes the event and never answers.
        let (mut c, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        c
    });
    let want = format!("echo:{}", String::from_utf8_lossy(input));
    assert_eq!(relay_in(&dir, &me, input, wait), Route::Answered(want));
    assert_eq!(relay_in(&dir, &me, input, Duration::from_millis(200)), Route::Late);
    drop(server.join().unwrap());
}
