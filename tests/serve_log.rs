//! The server log (`atlas::serve_log`): what the serve path writes, the
//! strict grammar every line of it follows (an allowlist: no free text), and
//! the adversarial inputs that try to get identifying text into it through
//! every write path that exists.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use atlas::json;
use atlas::serve_log::{self, Exit, Log, Site, HEADER, THREADS};
use common::*;

fn lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p).unwrap_or_default().lines().map(String::from).collect()
}

/// `src/a/b.rs:N` with lowercase, digit and `_` path parts.
fn is_src_location(s: &str) -> bool {
    let Some((file, line)) = s.rsplit_once(':') else { return false };
    let ok = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    !line.is_empty()
        && line.bytes().all(|b| b.is_ascii_digit())
        && file.strip_prefix("src/").and_then(|r| r.strip_suffix(".rs")).is_some_and(|r| r.split('/').all(ok))
}

fn is_location(s: &str) -> bool {
    s == "external" || s == "unknown" || is_src_location(s)
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
fn is_time(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 24
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'.',
            23 => *c == b'Z',
            _ => c.is_ascii_digit(),
        })
}

fn is_thread(s: &str) -> bool {
    s == "other" || THREADS.contains(&s) || s.strip_prefix("atlas-watch-").is_some_and(is_digits)
}

const SITES: [&str; 4] = ["vaults-new", "store-open", "serve-refused", "serve"];

/// One event line against the grammar: `TIME pid=N v=VERSION` and then a
/// start, panic or exit event with only allowlisted keys and value shapes.
fn check_line(l: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("{why}: {l:?}"));
    let mut it = l.splitn(4, ' ');
    let (Some(time), Some(pid), Some(v), Some(event)) = (it.next(), it.next(), it.next(), it.next()) else { return bad("too short") };
    if !is_time(time) {
        return bad("time");
    }
    if !pid.strip_prefix("pid=").is_some_and(is_digits) {
        return bad("pid");
    }
    if v != format!("v={}", env!("CARGO_PKG_VERSION")) {
        return bad("version");
    }
    if let Some(rest) = event.strip_prefix("start ") {
        return if rest == format!("mode=serve os={} arch={}", std::env::consts::OS, std::env::consts::ARCH) {
            Ok(())
        } else {
            bad("start")
        };
    }
    if let Some(rest) = event.strip_prefix("panic ") {
        let (head, msg) = match rest.split_once(" msg=\"") {
            Some((h, m)) => (h, Some(m)),
            None => match rest.strip_suffix(" payload omitted") {
                Some(h) => (h, None),
                None => return bad("panic payload"),
            },
        };
        let mut f = head.split(' ');
        let (Some(t), Some(at), None) = (f.next(), f.next(), f.next()) else { return bad("panic fields") };
        if !t.strip_prefix("thread=").is_some_and(is_thread) {
            return bad("thread");
        }
        let Some(at) = at.strip_prefix("at=").filter(|a| is_location(a)) else { return bad("panic at") };
        if let Some(m) = msg {
            let Some(body) = m.strip_suffix('"') else { return bad("msg close") };
            if !is_src_location(at) || body.len() > 200 || !body.chars().all(|c| (' '..='~').contains(&c) && c != '"' && c != '\\') {
                return bad("msg");
            }
        }
        return Ok(());
    }
    let Some(rest) = event.strip_prefix("exit reason=") else { return bad("event") };
    let f: Vec<&str> = rest.split(' ').collect();
    let n = f.len();
    if n < 3 {
        return bad("exit fields");
    }
    let up = f[n - 2].strip_prefix("uptime=").and_then(|u| u.strip_suffix('s')).and_then(|u| u.split_once('.'));
    if !up.is_some_and(|(a, b)| is_digits(a) && b.len() == 1 && is_digits(b)) {
        return bad("uptime");
    }
    if !f[n - 1].strip_prefix("responses=").is_some_and(is_digits) {
        return bad("responses");
    }
    let site = |s: &str| s.strip_prefix("site=").is_some_and(|s| SITES.contains(&s));
    let at = |s: &str| s.strip_prefix("at=").is_some_and(is_location);
    let kind = |s: &str| s.strip_prefix("kind=").is_some_and(|k| !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphabetic()));
    let ok = match &f[..n - 2] {
        ["eof"] | ["reexec"] | ["refused"] => true,
        ["io", s, k, a] => site(s) && kind(k) && at(a),
        ["failed", s, a] => site(s) && at(a),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        bad("exit")
    }
}

/// The main assertion: the file ends with a newline, its first line is the
/// header, the header appears once, and every other line follows the grammar.
fn assert_grammar(p: &Path) {
    let text = std::fs::read_to_string(p).unwrap();
    assert!(text.ends_with('\n'), "{text}");
    let ls = lines(p);
    assert_eq!(ls[0], HEADER, "{}", p.display());
    assert_eq!(ls.iter().filter(|l| l.as_str() == HEADER).count(), 1, "one header in {}", p.display());
    for l in &ls[1..] {
        if let Err(e) = check_line(l) {
            panic!("{}: {e}", p.display());
        }
    }
}

/// Every log file under `dir`, each checked against the grammar.
fn assert_grammar_all(dir: &Path) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        let n = p.file_name().unwrap().to_string_lossy().into_owned();
        if p.is_file() && (n.ends_with(".log") || n.ends_with(".log.1")) {
            assert_grammar(&p);
        }
    }
}

/// An exit line that passes the grammar, to fill a log with.
fn filler_line() -> String {
    format!("2026-01-01T00:00:00.000Z pid=1 v={} exit reason=eof uptime=0.0s responses=0\n", env!("CARGO_PKG_VERSION"))
}

/// A log of the header and filler lines, `short` bytes under the limit at
/// most. Returns its text.
fn fill(p: &Path, short: u64) -> String {
    let line = filler_line();
    let mut text = format!("{HEADER}\n");
    while (text.len() + line.len()) as u64 <= serve_log::ROTATE_AT - short {
        text.push_str(&line);
    }
    std::fs::write(p, &text).unwrap();
    text
}

fn serve_config(t: &Temp) -> PathBuf {
    write(&t.path().join("vault").join("Index.md"), "# Index\n");
    config(t, r#"[{"name": "secretvault", "path": "vault"}]"#)
}

/// A configuration with these vaults and nothing that reaches the network.
fn config(t: &Temp, vaults: &str) -> PathBuf {
    let cfg = t.path().join("atlas.json");
    write(
        &cfg,
        &format!(
            r#"{{"db": "state/atlas.sqlite", "vaults": {vaults}, "watch": false, "models": {{"auto_download": false, "files": []}}, "update": {{"enabled": false}}}}"#
        ),
    );
    cfg
}

/// The built binary serving with `args`, stdin closed at once.
fn serve(t: &Temp, args: &[&str], stderr: Stdio) -> std::process::Child {
    Command::new(bin())
        .args(args)
        .env("ATLAS_HOME", t.path().join("home-atlas"))
        .current_dir(t.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap()
}

/// The built binary serving one session: an initialize and a tool call
/// whose argument carries a marker, then stdin closes.
#[test]
fn a_served_session_logs_start_and_eof_and_no_request_text() {
    let t = Temp::new("serve-log-e2e");
    let cfg = serve_config(&t);
    let marker = "ArgMarkerQzx";
    let mut child = Command::new(bin())
        .arg("--config")
        .arg(&cfg)
        .env("ATLAS_HOME", t.path().join("home-atlas"))
        .current_dir(t.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let mut i = child.stdin.take().unwrap();
        writeln!(i, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "t"}}})).unwrap();
        writeln!(i, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "read_note", "arguments": {"vault": "secretvault", "path": format!("{marker}.md")}}})).unwrap();
        // Hostile request lines, each answered with one response: an unknown
        // tool, broken JSON, invalid UTF-8, an embedded CR, a hook call.
        writeln!(i, "{}", json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": format!("{marker}tool"), "arguments": {"q": marker}}})).unwrap();
        writeln!(i, "{{\"jsonrpc\": \"2.0\", \"id\": 4, \"method\": \"{marker}").unwrap();
        i.write_all(b"{\"id\": 5, \"q\": \"\xff\xfe ArgMarkerQzx \xc3\"}\n").unwrap();
        writeln!(i, "{{\"id\": 6, \"q\": \"{marker}\r\nexit reason=forged\"}}").unwrap();
        writeln!(i, "{}", json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "hook", "arguments": {"event": {"hook_event_name": "UserPromptSubmit", "prompt": marker, "session_id": "s", "cwd": marker}}}})).unwrap();
    }
    let pid = format!(" pid={} ", child.id());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let log = t.path().join("state").join(serve_log::FILE);
    assert_grammar(&log);
    let ls = lines(&log);
    assert_eq!(ls.len(), 3, "{ls:?}");
    assert!(ls[1].contains(" start mode=serve "), "{}", ls[1]);
    assert!(ls[2].contains(" exit reason=eof uptime="), "{}", ls[2]);
    assert!(ls[1].contains(&pid) && ls[2].contains(&pid), "{ls:?}");
    // The count is of the lines the client got.
    let sent = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).count();
    assert!(sent >= 7, "{sent} responses");
    assert!(ls[2].ends_with(&format!(" responses={sent}")), "{}", ls[2]);
    let text = std::fs::read_to_string(&log).unwrap();
    let tmp = t.path().to_string_lossy().into_owned();
    for needle in [marker, "secretvault", tmp.as_str()] {
        assert!(!text.contains(needle), "{needle} in {text}");
    }
}

/// `atlas --hook` writes no server log.
#[test]
fn hook_mode_writes_nothing() {
    let t = Temp::new("serve-log-hook");
    let cfg = serve_config(&t);
    let mut child = Command::new(bin())
        .arg("--config")
        .arg(&cfg)
        .arg("--hook")
        .env("ATLAS_HOME", t.path().join("home-atlas"))
        .env_remove("CLAUDE_CODE_SESSION_ATTENDED")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .current_dir(t.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Hostile fields: the hook still writes no log, so none of it can leak.
    let w = who();
    let prompt: String = hostile(&w).into_iter().map(|(s, _)| s).filter(|s| s.len() < 1000).collect::<Vec<_>>().join(" ");
    let ev = json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt, "session_id": "s-log", "cwd": t.path().to_string_lossy().into_owned()});
    child.stdin.take().unwrap().write_all(ev.to_string().as_bytes()).unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
    assert!(t.path().join("state").join("atlas.sqlite").exists(), "the hook ran against this state folder");
    assert!(!t.path().join("state").join(serve_log::FILE).exists());
    assert!(!t.path().join("home-atlas").join("state").join(serve_log::FILE).exists());
}

#[test]
fn the_log_rotates_at_the_size_limit() {
    let t = Temp::new("serve-log-rotate");
    let p = t.path().join(serve_log::FILE);
    let old = t.path().join(format!("{}.1", serve_log::FILE));
    std::fs::write(&old, format!("{HEADER}\n")).unwrap();
    let mut full = fill(&p, 0);
    full.push_str(&filler_line());
    std::fs::write(&p, &full).unwrap();
    let log = Log::new(p.clone());
    log.start();
    assert_eq!(std::fs::read_to_string(&old).unwrap(), full, "the full log replaced the older .1");
    let ls = lines(&p);
    assert_eq!(ls.len(), 2);
    assert!(ls[1].contains(" start mode=serve os="), "{}", ls[1]);
    // Under the limit: appended, no header.
    log.exit(Exit::Eof);
    assert_eq!(lines(&p).len(), 3);
    assert_grammar_all(t.path());
    assert!(!t.path().join(format!("{}.lock", serve_log::FILE)).exists(), "the rotation lock is removed");
}

/// A rotation lock held by another appender skips the rotation; one older
/// than the stale bound (left by a killed process) is taken over.
#[test]
fn a_held_rotation_lock_skips_and_a_stale_one_is_taken_over() {
    let t = Temp::new("serve-log-lock");
    let p = t.path().join(serve_log::FILE);
    let old = t.path().join(format!("{}.1", serve_log::FILE));
    let lock = t.path().join(format!("{}.lock", serve_log::FILE));
    let full = fill(&p, 0) + &filler_line();
    std::fs::write(&p, &full).unwrap();
    std::fs::write(&lock, "").unwrap();
    let log = Log::new(p.clone());
    log.exit(Exit::Eof);
    assert!(!old.exists(), "a fresh lock: no rotation");
    assert_eq!(lines(&p).len(), full.lines().count() + 1, "the line is still written");
    let f = std::fs::OpenOptions::new().write(true).open(&lock).unwrap();
    f.set_modified(std::time::SystemTime::now() - serve_log::STALE_LOCK - std::time::Duration::from_secs(5)).unwrap();
    drop(f);
    log.exit(Exit::Eof);
    assert!(old.exists(), "a stale lock is taken over and the log rotated");
    assert!(!lock.exists(), "and removed");
    assert_eq!(lines(&p).len(), 2);
    assert_grammar_all(t.path());
}

/// A lock dated more than the stale bound in the future (the clock stepped
/// back after a crash) is stale too, so rotation does not stop until the
/// clock catches up; one only a little in the future is still held.
#[test]
fn a_lock_dated_far_in_the_future_is_stale() {
    let t = Temp::new("serve-log-future-lock");
    let p = t.path().join(serve_log::FILE);
    let old = t.path().join(format!("{}.1", serve_log::FILE));
    let lock = t.path().join(format!("{}.lock", serve_log::FILE));
    let full = fill(&p, 0) + &filler_line();
    std::fs::write(&p, &full).unwrap();
    std::fs::write(&lock, "").unwrap();
    let date = |ahead: std::time::Duration| {
        let f = std::fs::OpenOptions::new().write(true).open(&lock).unwrap();
        f.set_modified(std::time::SystemTime::now() + ahead).unwrap();
    };
    let log = Log::new(p.clone());
    date(serve_log::STALE_LOCK / 2);
    log.exit(Exit::Eof);
    assert!(!old.exists(), "a lock a little in the future: still held");
    date(serve_log::STALE_LOCK + std::time::Duration::from_secs(3600));
    log.exit(Exit::Eof);
    assert!(old.exists(), "a lock an hour in the future is taken over and the log rotated");
    assert!(!lock.exists(), "and removed");
    assert_eq!(lines(&p).len(), 2);
    assert_grammar_all(t.path());
}

/// Header temporaries (`atlas-server.log.PID.NANOS.new`) left by a process
/// killed mid-create are swept once older than the stale bound; a fresh one
/// (another appender's, in use) and files of any other name are kept.
#[test]
fn leftover_header_temporaries_are_swept() {
    let t = Temp::new("serve-log-temps");
    let p = t.path().join(serve_log::FILE);
    let f = serve_log::FILE;
    let stale = [format!("{f}.4242.1731600000000000000.new"), format!("{f}.1.0.new")];
    let kept = [
        format!("{f}.4243.1731600000000000001.new"),
        format!("{f}.x.1.new"),
        format!("{f}.1.new"),
        format!("{f}.1.2.3.new"),
        format!("{f}.1.2.tmp"),
        "other.log.4242.1731600000000000000.new".to_string(),
    ];
    let old = std::time::SystemTime::now() - serve_log::STALE_LOCK - std::time::Duration::from_secs(5);
    for n in stale.iter().chain(&kept[1..]) {
        let x = std::fs::File::create(t.path().join(n)).unwrap();
        x.set_modified(old).unwrap();
    }
    std::fs::write(t.path().join(&kept[0]), "").unwrap();
    // At start.
    Log::new(p.clone()).start();
    for n in &stale {
        assert!(!t.path().join(n).exists(), "{n} swept at start");
    }
    for n in &kept {
        assert!(t.path().join(n).exists(), "{n} kept");
    }
    // At rotation.
    let x = std::fs::File::create(t.path().join(&stale[0])).unwrap();
    x.set_modified(old).unwrap();
    drop(x);
    let full = fill(&p, 0) + &filler_line();
    std::fs::write(&p, &full).unwrap();
    Log::new(p.clone()).exit(Exit::Eof);
    assert!(t.path().join(format!("{f}.1")).exists(), "rotated");
    assert!(!t.path().join(&stale[0]).exists(), "swept at rotation");
    assert!(t.path().join(&kept[0]).exists());
}

/// Every thread atlas's src spawns with a name has that name on
/// [`THREADS`] (or is `atlas-watch-N`), so its panics name it.
#[test]
fn every_named_thread_in_src_is_on_the_list() {
    let mut files = Vec::new();
    rs_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    let mut found = Vec::new();
    let mut bad = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        for (at, _) in text.match_indices("thread::Builder::new()") {
            let rest = &text[at + "thread::Builder::new()".len()..];
            let chain: String = rest[..rest.find(".spawn").unwrap_or(rest.len())].split_whitespace().collect();
            let Some(arg) = chain.strip_prefix(".name(") else {
                bad.push(format!("{}: a Builder with no .name(...) first: {chain}", f.display()));
                continue;
            };
            let lit = ["\"", "String::from(\""].iter().find_map(|p| arg.strip_prefix(p)).and_then(|a| a.split('"').next());
            match (lit, arg.starts_with("format!(\"atlas-watch-{")) {
                (_, true) => found.push("atlas-watch-N".to_string()),
                (Some(n), false) if THREADS.contains(&n) => found.push(n.to_string()),
                (Some(n), false) => bad.push(format!("{}: {n} is not on THREADS", f.display())),
                (None, false) => bad.push(format!("{}: a name that is not a literal: {arg}", f.display())),
            }
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
    assert!(found.len() >= 12, "{found:?}");
    for n in ["atlas-update", "atlas-reader", "atlas-hooks", "atlas-watch-N"] {
        assert!(found.iter().any(|f| f == n), "{n} in {found:?}");
    }
}

fn rs_files(d: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(d).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The text of `src` with every comment blanked to spaces (line breaks
/// kept), and for each line whether it lies inside a `#[cfg(test)]` item's
/// braces. String and char literals are skipped when counting braces.
fn code_and_test_lines(src: &str) -> (String, Vec<bool>) {
    let b = src.as_bytes();
    let mut code = b.to_vec();
    // Byte offsets of every `{` and `}` outside comments and literals.
    let mut braces: Vec<(usize, u8)> = Vec::new();
    let mut semis: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    code[i] = b' ';
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        code[i] = b' ';
                        code[i + 1] = b' ';
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        code[i] = b' ';
                        code[i + 1] = b' ';
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if b[i] != b'\n' {
                            code[i] = b' ';
                        }
                        i += 1;
                    }
                }
            }
            b'r' if (b.get(i + 1) == Some(&b'"') || b.get(i + 1) == Some(&b'#'))
                && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) =>
            {
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                j += 1;
                let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
                while j < b.len() && !b[j..].starts_with(&close) {
                    j += 1;
                }
                i = j + close.len();
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'\'' => {
                // A char literal ('x', '\n', '\u{..}'), else a lifetime.
                if b.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                } else {
                    let ch = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                    i += if b.get(i + 1 + ch) == Some(&b'\'') { 2 + ch } else { 1 };
                }
            }
            c @ (b'{' | b'}') => {
                braces.push((i, c));
                i += 1;
            }
            b';' => {
                semis.push(i);
                i += 1;
            }
            _ => i += 1,
        }
    }
    let code = String::from_utf8(code).expect("only ASCII bytes were blanked");
    let line_of = |at: usize| code[..at].bytes().filter(|&c| c == b'\n').count();
    let mut test = vec![false; code.lines().count() + 1];
    for (at, _) in code.match_indices("#[cfg(") {
        let attr = &code[at..at + code[at..].find(']').unwrap_or(0)];
        if !attr.contains("test") || attr.contains("not(test") {
            continue;
        }
        let Some(k) = braces.iter().position(|&(p, _)| p > at) else { continue };
        if semis.iter().any(|&s| s > at && s < braces[k].0) {
            continue; // `mod x;` or another item with no block
        }
        let mut depth = 0i32;
        for &(p, c) in &braces[k..] {
            depth += if c == b'{' { 1 } else { -1 };
            if depth == 0 {
                for t in &mut test[line_of(at)..=line_of(p)] {
                    *t = true;
                }
                break;
            }
        }
    }
    (code, test)
}

/// The module files a `#[cfg(test)] mod x;` names, as `src`-relative paths
/// with `/` separators.
fn test_module_files(src: &Path, files: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for f in files {
        let (code, _) = code_and_test_lines(&std::fs::read_to_string(f).unwrap());
        let rel = f.strip_prefix(src).unwrap().to_string_lossy().replace('\\', "/");
        let stem = rel.strip_suffix(".rs").unwrap();
        let dir = match stem.rsplit_once('/') {
            _ if ["lib", "main"].contains(&stem) => String::new(),
            Some((d, "mod")) => format!("{d}/"),
            None if stem == "mod" => String::new(),
            _ => format!("{stem}/"),
        };
        let squashed: String = code.split_whitespace().collect::<Vec<_>>().join(" ");
        for (at, _) in squashed.match_indices("#[cfg(test)]") {
            let rest = squashed[at + "#[cfg(test)]".len()..].trim_start();
            let rest = ["pub(crate) ", "pub(super) ", "pub "].iter().find_map(|p| rest.strip_prefix(p)).unwrap_or(rest);
            if let Some(m) = rest.strip_prefix("mod ").and_then(|r| r.split_once(';')).map(|(m, _)| m.trim()) {
                if m.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
                    out.push(format!("{dir}{m}.rs"));
                    out.push(format!("{dir}{m}/mod.rs"));
                }
            }
        }
    }
    out
}

/// No code that ships names the folder atlas was built in: `env!` (or
/// `option_env!`) of `CARGO_MANIFEST_DIR`, `OUT_DIR` or `CARGO_TARGET_DIR`
/// compiles that absolute path into the binary, where the release's leak
/// check finds it. Tests may use them: `#[cfg(test)]` items and test-only
/// module files are not shipped.
#[test]
fn src_bakes_no_build_folder_into_the_binary() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    let test_only = test_module_files(&src, &files);
    assert!(test_only.iter().any(|f| f == "lex/props.rs"), "{test_only:?}");
    let mut hits = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&src).unwrap().to_string_lossy().replace('\\', "/");
        if test_only.contains(&rel) {
            continue;
        }
        let (code, test) = code_and_test_lines(&std::fs::read_to_string(f).unwrap());
        for (i, l) in code.lines().enumerate() {
            let squashed: String = l.split_whitespace().collect();
            for var in ["CARGO_MANIFEST_DIR", "OUT_DIR", "CARGO_TARGET_DIR"] {
                if squashed.contains(&format!("env!(\"{var}\"")) && !test[i] {
                    hits.push(format!("src/{rel}:{}: {var}", i + 1));
                }
            }
        }
    }
    assert!(hits.is_empty(), "build-folder paths compiled into shipped code: {hits:#?}");
}

#[test]
fn the_cfg_test_scanner_finds_test_items_and_skips_comments() {
    let text = "fn a() { env!(\"OUT_DIR\"); }\n// env!(\"OUT_DIR\")\n#[cfg(test)]\nmod tests {\n    fn b() { let _ = '{'; let _ = \"}\"; }\n}\nfn c() {}\n";
    let (code, test) = code_and_test_lines(text);
    assert!(code.lines().next().unwrap().contains("env!"));
    assert!(!code.lines().nth(1).unwrap().contains("env!"));
    assert_eq!(&test[..7], &[false, false, true, true, true, true, false]);
}

/// `n` threads, released together, each appending `each` exit lines.
fn race(p: &Path, n: usize, each: usize) {
    let go = Arc::new(std::sync::Barrier::new(n));
    let hs: Vec<_> = (0..n)
        .map(|_| {
            let (p, go) = (p.to_path_buf(), go.clone());
            std::thread::spawn(move || {
                let log = Log::new(p);
                go.wait();
                for _ in 0..each {
                    log.exit(Exit::Eof);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
}

/// N threads in this process and two `atlas serve` processes append at once
/// to a log just under the limit: one rotation, the full old log kept in
/// `.1`, one header first in each file, no line lost or split. Then many
/// rounds of threads alone, so the race is met: on a full log, and on a
/// log that does not exist yet.
#[test]
fn concurrent_appenders_rotate_once_and_lose_nothing() {
    const N: usize = 8;
    let t = Temp::new("serve-log-race");
    let cfg = serve_config(&t);
    let state = t.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let p = state.join(serve_log::FILE);
    let old = state.join(format!("{}.1", serve_log::FILE));
    let lock = state.join(format!("{}.lock", serve_log::FILE));
    let cfg_s = cfg.to_string_lossy().into_owned();
    for round in 0..40 {
        let _ = std::fs::remove_file(&old);
        let prefill = fill(&p, 40);
        let kids: Vec<_> = if round == 0 { (0..2).map(|_| serve(&t, &["--config", &cfg_s], Stdio::null())).collect() } else { Vec::new() };
        let each = if round == 0 { 200 } else { 10 };
        race(&p, N, each);
        let procs = kids.len();
        for k in kids {
            k.wait_with_output().unwrap();
        }
        assert_grammar(&old);
        assert_grammar(&p);
        let o = std::fs::read_to_string(&old).unwrap();
        assert!(o.starts_with(&prefill), "round {round}: the .1 holds the full old log");
        let (lo, ln) = (lines(&old), lines(&p));
        assert_eq!(lo.len() + ln.len() - 2, prefill.lines().count() - 1 + N * each + 2 * procs, "round {round}: no line lost");
        let starts = lo.iter().chain(&ln).filter(|l| l.contains(" start mode=serve ")).count();
        assert_eq!(starts, procs, "both servers wrote their start line");
        assert!(!lock.exists());
    }
    for round in 0..100 {
        let _ = std::fs::remove_file(&p);
        race(&p, N, 3);
        assert_grammar(&p);
        assert_eq!(lines(&p).len(), 1 + N * 3, "round {round}");
    }
}

/// A relative `--db`: the log lands beside that database (made absolute),
/// not elsewhere.
#[test]
fn a_relative_db_puts_the_log_beside_it() {
    let t = Temp::new("serve-log-reldb");
    let cfg = serve_config(&t);
    let out = serve(&t, &["--config", &cfg.to_string_lossy(), "--db", "rel/atlas.sqlite"], Stdio::null()).wait_with_output().unwrap();
    assert!(out.status.success());
    let want = std::path::absolute(t.path().join("rel").join("atlas.sqlite")).unwrap().parent().unwrap().join(serve_log::FILE);
    assert_grammar(&want);
    assert_eq!(lines(&want).len(), 3);
    assert!(!t.path().join(serve_log::FILE).exists());
    assert!(!t.path().join("state").join(serve_log::FILE).exists());
}

/// stderr's read end closed and a log that cannot be written (its path is a
/// folder): the server still exits normally on stdin EOF, not by abort.
#[test]
fn a_broken_stderr_and_an_unwritable_log_do_not_stop_the_server() {
    let t = Temp::new("serve-log-broken");
    let cfg = serve_config(&t);
    std::fs::create_dir_all(t.path().join("state").join(serve_log::FILE)).unwrap();
    let mut child = serve(&t, &["--config", &cfg.to_string_lossy()], Stdio::piped());
    drop(child.stderr.take());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{status:?}");
}

/// The two failures reachable today end to end, each with the review's
/// input in the configuration: one exit line with its fixed code, and none
/// of the input's words.
#[test]
fn vault_failures_write_their_code_and_no_input_word() {
    let t = Temp::new("serve-log-vaults");
    let missing = t.path().join("Cases").join("Acme (Falcon merger) 2026").join("notes");
    let one = t.path().join("a").join("Falcon");
    let two = t.path().join("b").join("Falcon");
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    let esc = |p: &Path| p.to_string_lossy().replace('\\', "\\\\");
    for vaults in [
        format!(r#"[{{"name": "notes", "path": "{}"}}]"#, esc(&missing)),
        format!(r#"[{{"name": "one", "path": "{}"}}, {{"name": "two", "path": "{}"}}]"#, esc(&one), esc(&two)),
    ] {
        let cfg = config(&t, &vaults);
        let log = t.path().join("state").join(serve_log::FILE);
        let _ = std::fs::remove_file(&log);
        let out = serve(&t, &["--config", &cfg.to_string_lossy()], Stdio::piped()).wait_with_output().unwrap();
        assert!(!out.status.success(), "{vaults}");
        // The user still sees the error's text, on stderr.
        assert!(String::from_utf8_lossy(&out.stderr).contains("Falcon"), "{}", String::from_utf8_lossy(&out.stderr));
        assert_grammar(&log);
        let ls = lines(&log);
        assert_eq!(ls.len(), 3, "{ls:?}");
        assert!(ls[2].contains(" exit reason=failed site=vaults-new at=src/main.rs:"), "{}", ls[2]);
        assert_no_words(&std::fs::read_to_string(&log).unwrap(), &vaults);
    }
}

/// Atlas's src never makes a runtime string `'static` or panics with a
/// custom payload, so a `&'static str` panic payload in atlas's source is a
/// literal there: the rule the panic line's message relies on.
#[test]
fn src_never_leaks_a_string_or_panics_with_a_custom_payload() {
    let mut files = Vec::new();
    rs_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    assert!(files.len() > 20);
    let mut hits = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        for (i, l) in text.lines().enumerate() {
            // `leak(` catches `.leak()`, `Box::leak(`, `String::leak(`,
            // `Vec::leak(` and `Box::<str>::leak(`; `::leak` a path to it
            // passed as a function; `into_raw` a `Box::into_raw` turned into
            // a reference.
            for bad in ["leak(", "::leak", "into_raw", "panic_any", "resume_unwind"] {
                if l.contains(bad) {
                    hits.push(format!("{}:{}: {bad}", f.display(), i + 1));
                }
            }
        }
    }
    assert!(hits.is_empty(), "{hits:#?}");
}

#[test]
fn the_header_says_it_is_safe_to_attach() {
    assert!(HEADER.starts_with("# atlas-server.log: diagnostic only, safe to attach to a GitHub issue."));
    for field in ["UTC time", "pid", "atlas version", "start", "panic", "exit", "uptime", "Allowlist only", "no free text"] {
        assert!(HEADER.contains(field), "{field}");
    }
    assert!(!HEADER.contains("redact"), "no redactor any more");
}

/// The review's identifying words: none may appear in any log, as a whole
/// word, in any case.
const FORBIDDEN: &[&str] = &[
    "Falcon",
    "merger",
    "Acme",
    "Roe",
    "Doe",
    "Jane",
    "Jo",
    "Smith",
    "Wu",
    "Al",
    "Brandon",
    "Brandons",
    "Smiths",
    "notebook",
    "Project",
    "Notes",
    "kickoff",
    "Cases",
    "custody",
    "Documents",
    "dbg",
    "feedback",
    "4111111111111111",
    "5551234567",
    "hunterSecretPasswordXyz",
];

/// The review's inputs, verbatim (findings 1 to 5 and 7).
const REVIEW: &[&str] = &[
    r#"not found: note "Project Falcon merger plan.md""#,
    "invalid argument: `Falcon merger kickoff` is not a key",
    r#"bad call {"query":"Falcon merger plan with Acme"}"#,
    r#"vault name "Project Falcon Notes" must be ..."#,
    r#"stdin read: "Project Falcon merger plan""#,
    r"vault notes: D:\Cases\Acme (Falcon merger) 2026\notes: not found",
    r"io: D:\Cases\Doe v. Roe custody\notes: Access is denied. (os error 5)",
    r"C:\Users\Jo Smith\Documents\Roe, Jane - Falcon\notes: denied",
    "io: Falcon merger notes.md: not found",
    "invalid argument: two vaults share the folder name Falcon",
    "user Wu and host Al",
    "Brandons notebook and Smiths",
    "account 4111111111111111 phone 5551234567",
    "token hunterSecretPasswordXyz",
    "database disk image is malformed; dbg feedback",
];

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect()
}

fn assert_no_words(text: &str, input: &str) {
    let ws = words(text);
    for f in FORBIDDEN {
        assert!(!ws.contains(&f.to_ascii_lowercase()), "{f:?} from {input:?} in:\n{text}");
    }
}

/// A fake user, host and home, and the real ones from the environment.
struct Who {
    home: String,
    state: String,
    needles: Vec<String>,
}

fn who() -> Who {
    let home = r"C:\Users\Jdoeuser Smithson".to_string();
    let mut needles: Vec<String> = ["Jdoeuser", "Smithson", "secretproj", "fileserver", "corphost"].iter().map(|s| s.to_string()).collect();
    for var in ["USERNAME", "USER", "COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(var) {
            if v.len() >= 3 {
                needles.push(v);
            }
        }
    }
    if let Some(h) = atlas::dirs::home() {
        needles.push(h.to_string_lossy().into_owned());
    }
    Who { state: format!(r"{home}\AppData\Local\atlas\state"), home, needles }
}

/// Hostile inputs: (text, what must not appear besides the common needles).
fn hostile(w: &Who) -> Vec<(String, Vec<&'static str>)> {
    let user = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "nobodyuser".into());
    let host = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "nohost".into());
    let home_json = w.home.replace('\\', "\\\\");
    let long = format!("{} {}\\secretproj\\deep\\file.md {}", "word ".repeat(78), w.home, "tail ".repeat(50));
    vec![
        (format!(r"{}\secretproj\notes\a.md: not found", w.home), vec![]),
        (format!("{}/secretproj/notes/a.md: not found", w.home.replace('\\', "/")), vec![]),
        (r"C:\Wrkzone\secretproj\a.md: denied".into(), vec!["Wrkzone"]),
        ("C:/Wrkzone/secretproj/a.md: denied".into(), vec!["Wrkzone"]),
        (r"\\fileserver\shrzone\secretproj\a.md: unreachable".into(), vec!["shrzone"]),
        (r"C:secretproj\file.txt: drive relative".into(), vec![]),
        (r"\\?\C:\Users\Jdoeuser Smithson\secretproj\a.md: long path".into(), vec![]),
        ("/home/jdoeunix/secretproj/main.rs: no such file".into(), vec!["jdoeunix"]),
        ("~/secretproj/notes.md missing".into(), vec![]),
        (r"%USERPROFILE%\secretproj missing".into(), vec!["USERPROFILE"]),
        ("$HOME/secretproj missing".into(), vec!["HOME"]),
        ("file:///C:/Users/Jdoeuser%20Smithson/secretproj/a.md bad url".into(), vec![]),
        (
            r#"open "D:\My Projects\Sécret Ünicode\o'brien "quoted"\a.md" failed"#.into(),
            vec!["Projects", "Sécret", "nicode", "brien", "quoted"],
        ),
        (format!("{}\\secret\nproj\\a.md split", w.home), vec!["proj"]),
        ("C%3A%5CUsers%5CJdoeuser%5Csecretproj encoded".into(), vec![]),
        (format!(r#"json: {{"path": "{home_json}\\secretproj\\a.md"}}"#), vec![]),
        (format!("user {user} on host {host}"), vec![]),
        ("user corphost here".into(), vec![]),
        ("mail jdoe.mail@example.com now".into(), vec!["example", "jdoe"]),
        ("repo octo-org/secret-repo failed".into(), vec!["octo", "secret-repo"]),
        ("lane ATLAS-USES record journal:ATLAS-USES:0024 handoff:TASK-JUDGE:007".into(), vec!["USES", "JUDGE", "0024"]),
        (
            "token ghp_abcdEFGH1234567890abcdEFGH12345678 and sk-ant-api03-ZZZ9 and AKIAIOSFODNN7EXAMPLE".into(),
            vec!["ghp", "abcdEFGH", "api03", "ZZZ9", "AKIAIOSFODNN"],
        ),
        (long, vec!["word", "tail"]),
        ("\x1b[31mredpart\x1b[0m\r\nnext\tline\u{0007}bell\u{0}nul \u{202e}rtl".into(), vec!["31m", "redpart"]),
        ("outer: middle: inner: C:\\Wrkzone\\secretproj\\x.md: gone".into(), vec!["Wrkzone", "middle", "inner"]),
        // The state folder in each form an error can carry it.
        (format!("{}\\atlas.sqlite: locked", w.state), vec!["locked"]),
        (format!("{}/atlas.sqlite: locked", w.state.replace('\\', "/")), vec!["locked"]),
        (format!(r"\\?\{}\atlas.sqlite: locked", w.state), vec!["locked"]),
        (format!(r"\\localhost\C$\{}\atlas.sqlite: locked", &w.state[3..]), vec!["locked"]),
        (format!("{} locked", percent(&w.state)), vec!["locked"]),
        (format!("{} locked", w.state.to_uppercase()), vec!["locked"]),
        // Request text, as a client sends it.
        (
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"recall","arguments":{"query":"Mergerplanqz"}}}"#.into(),
            vec!["Mergerplanqz", "recall", "jsonrpc"],
        ),
        ("unknown tool \"Toolnameqz\" with args {'q': 'Askqz'}".into(), vec!["Toolnameqz", "Askqz"]),
        // Very long: one word, one path, many words.
        ("Z".repeat(100_000), vec!["ZZZZ"]),
        (format!("{}\\{}a.md", w.home, "deep\\".repeat(20_000)), vec!["deep"]),
        ("plain ".repeat(20_000), vec!["plain"]),
    ]
}

/// `s` percent-encoded byte by byte, as a `file:` URL carries a path.
fn percent(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn assert_clean(text: &str, w: &Who, extra: &[&str], input: &str) {
    for tag in ["<home>", "<state>", "<name>", "<redacted>"] {
        assert!(!text.contains(tag), "no redactor placeholder: {tag}");
    }
    for n in w.needles.iter().map(String::as_str).chain(extra.iter().copied()) {
        // A number is looked for as a whole word: pids and times hold digits.
        let hit = if n.bytes().all(|b| b.is_ascii_digit()) {
            words(text).iter().any(|w| w == n)
        } else {
            text.to_lowercase().contains(&n.to_lowercase())
        };
        assert!(!hit, "{n:?} leaked from {input:?} into:\n{text}");
    }
}

/// Every io error built around hostile text logs its kind only.
fn hostile_exits(input: &str) -> Vec<Exit> {
    let mut bytes = vec![0xff, 0xc3];
    bytes.extend_from_slice(input.as_bytes());
    bytes.extend_from_slice(&[0xe2, 0x80, b'\n', 0x80]);
    let lossy = String::from_utf8_lossy(&bytes).into_owned();
    let parse = json::parse(&format!("{{\"a\": \"{input}\" ")).err().map(|e| e.to_string()).unwrap_or_default();
    vec![
        Exit::io(Site::Serve, std::io::Error::other(input.to_string()).kind()),
        Exit::io(Site::Serve, std::io::Error::new(std::io::ErrorKind::InvalidData, lossy).kind()),
        Exit::io(Site::ServeRefused, std::io::Error::new(std::io::ErrorKind::BrokenPipe, format!("stdout write: {input}")).kind()),
        Exit::io(Site::Serve, std::io::Error::new(std::io::ErrorKind::NotFound, format!("parse error: {parse}")).kind()),
        Exit::failed(Site::VaultsNew),
        Exit::failed(Site::StoreOpen),
        Exit::Eof,
        Exit::Reexec,
        Exit::Refused,
    ]
}

/// The hostile table and the review's inputs through every exit write path.
#[test]
fn hostile_error_text_never_reaches_the_exit_line() {
    let t = Temp::new("serve-log-hostile");
    let w = who();
    let mut cases = hostile(&w);
    cases.extend(REVIEW.iter().map(|s| (s.to_string(), vec![])));
    for (i, (input, extra)) in cases.into_iter().enumerate() {
        let p = t.path().join(format!("case{i}.log"));
        let log = Log::new(p.clone());
        log.start();
        let exits = hostile_exits(&input);
        for e in &exits {
            log.exit(*e);
        }
        let ls = lines(&p);
        assert_eq!(ls.len(), 2 + exits.len(), "one line per event for {input:?}: {ls:?}");
        let text = ls.join("\n");
        assert_clean(&text, &w, &extra, &input);
        assert_no_words(&text, &input);
        assert!(ls[2].contains(" exit reason=io site=serve kind=Other at=external "), "{}", ls[2]);
        assert!(ls[6].contains(" exit reason=failed site=vaults-new at=external "), "{}", ls[6]);
    }
    assert_grammar_all(t.path());
}

/// The hostile table through rotation: both files stay clean, one header
/// each, one line per event.
#[test]
fn hostile_text_through_rotation_stays_out_of_both_files() {
    let t = Temp::new("serve-log-hostile-rotate");
    let w = who();
    let p = t.path().join(serve_log::FILE);
    let old = t.path().join(format!("{}.1", serve_log::FILE));
    for (input, extra) in hostile(&w) {
        let full = fill(&p, 0) + &filler_line();
        std::fs::write(&p, &full).unwrap();
        let log = Log::new(p.clone());
        log.exit(Exit::io(Site::Serve, std::io::Error::other(input.clone()).kind()));
        log.exit(Exit::failed(Site::StoreOpen));
        for f in [&old, &p] {
            assert_grammar(f);
            assert_clean(&std::fs::read_to_string(f).unwrap(), &w, &extra, &input);
        }
        let n = lines(&p);
        assert_eq!(n.len(), 3, "the new log holds the header and both exits: {n:?}");
        assert!(n[1].contains(" exit reason=io site=serve kind=Other "), "{}", n[1]);
    }
}

/// A seeded xorshift generator: no new crate.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A random free text: ASCII, Unicode, control characters, quotes and
/// separators, in words of random length.
fn random_text(r: &mut Rng) -> String {
    // The first 10 are letters and digits.
    const POOL: &str = "abcqxzKMQZ079\u{e9}\u{dc}\u{4e2d}\u{1f600}\u{202e}\u{430}\n\r\t\0\x1b\"'`/\\:;,.(){}[]%$~@-_= ";
    let pool: Vec<char> = POOL.chars().collect();
    let n = 1 + r.below(120);
    (0..n)
        .map(|_| {
            // Letters most of the time, so most texts have long tokens.
            if r.below(3) > 0 {
                pool[r.below(10)]
            } else {
                pool[r.below(pool.len())]
            }
        })
        .collect()
}

/// The text's tokens to look for: alphanumeric runs of 4 or more
/// characters, but not all digits (times, pids and line numbers are).
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4 && !w.chars().all(|c| c.is_ascii_digit()))
        .map(|w| w.to_lowercase())
        .collect()
}

/// Atlas's own words in a log (written with no input at all), lowercased,
/// so a random token that happens to be one is not counted as a leak.
fn vocabulary(text: &str) -> Vec<String> {
    let mut v = words(text);
    v.extend(words(HEADER));
    v.sort();
    v.dedup();
    v
}

/// No token of a random text is found in a log any write path wrote it to.
fn assert_no_tokens(file_words: &[String], vocab: &[String], text: &str) {
    for tok in tokens(text) {
        if vocab.iter().any(|v| v.contains(&tok)) {
            continue;
        }
        assert!(!file_words.iter().any(|w| w.contains(&tok)), "{tok:?} of {text:?} in the log");
    }
}

/// Property-style: 10k random texts through every exit write path.
#[test]
fn random_free_text_never_reaches_the_exit_line() {
    let t = Temp::new("serve-log-prop");
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let control = t.path().join("control.log");
    for e in hostile_exits("x") {
        Log::new(control.clone()).exit(e);
    }
    let vocab = vocabulary(&std::fs::read_to_string(&control).unwrap());
    for batch in 0..100 {
        let p = t.path().join(format!("batch{batch}.log"));
        let log = Log::new(p.clone());
        let texts: Vec<String> = (0..100).map(|_| random_text(&mut r)).collect();
        for text in &texts {
            for e in hostile_exits(text) {
                log.exit(e);
            }
        }
        let mut fw = words(&std::fs::read_to_string(&p).unwrap());
        fw.sort();
        fw.dedup();
        for text in &texts {
            assert_no_tokens(&fw, &vocab, text);
        }
    }
    assert_grammar_all(t.path());
}

/// The panic hook: a formatted payload, a leaked `&'static str` from outside
/// atlas's source, a non-string payload and a thread named with hostile text
/// are never written; the hook it replaced still runs. The hostile table,
/// the review's inputs and 10k random texts go through each. One test, so
/// the process-wide hook is installed once.
#[test]
fn panics_are_logged_without_their_payload_and_chain_to_the_previous_hook() {
    let t = Temp::new("serve-log-panic");
    let p = t.path().join(serve_log::FILE);
    let w = who();
    let ran = Arc::new(AtomicBool::new(false));
    let r = ran.clone();
    let quiet = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |_| r.store(true, Ordering::SeqCst)));
    Log::new(p.clone()).install_panic_hook();
    let on = |name: &str, f: Box<dyn FnOnce() + Send>| {
        let h = std::thread::Builder::new().name(name.into()).spawn(f).unwrap();
        assert!(h.join().is_err());
    };
    // Each text, three ways: formatted, leaked to `'static` (from a test
    // file, so external), and as the name of a thread that panics.
    let three = |input: &str| {
        let i = input.to_string();
        on("atlas-hooks", Box::new(move || panic!("unwrap on {i}")));
        let s: &'static str = String::leak(input.to_string());
        on("atlas-reader", Box::new(move || std::panic::panic_any(s)));
        let named: String = input.chars().filter(|c| *c != '\0').take(200).collect();
        on(&named, Box::new(|| panic!("a literal message")));
    };
    let old = t.path().join(format!("{}.1", serve_log::FILE));
    // Both files (the log rotates along the way), each checked against
    // the grammar, and their event lines.
    let read = || {
        let mut all = String::new();
        for f in [&old, &p] {
            if f.exists() {
                assert_grammar(f);
                all.push_str(&std::fs::read_to_string(f).unwrap());
            }
        }
        all
    };
    on("atlas-worker", Box::new(|| panic!("a literal message")));
    on("atlas-hooks", Box::new(|| std::panic::panic_any(42_u32)));
    on("main", Box::new(|| std::panic::panic_any(String::from("owned"))));
    let first = read();
    let ls: Vec<&str> = first.lines().skip(1).collect();
    assert_eq!(ls.len(), 3, "{ls:?}");
    // A literal from a test file is not atlas's: no message. Names: atlas's
    // own are kept, any other is `other`.
    assert!(ls[0].ends_with(" panic thread=other at=external payload omitted"), "{}", ls[0]);
    assert!(ls[1].ends_with(" panic thread=atlas-hooks at=external payload omitted"), "{}", ls[1]);
    assert!(ls[2].ends_with(" panic thread=main at=external payload omitted"), "{}", ls[2]);
    let mut vocab = vocabulary(&first);
    vocab.extend(THREADS.iter().flat_map(|t| words(t)));
    let mut cases = hostile(&w);
    cases.extend(REVIEW.iter().map(|s| (s.to_string(), vec![])));
    for (input, _) in &cases {
        three(input);
    }
    let all = read();
    let ls: Vec<&str> = all.lines().filter(|l| *l != HEADER).collect();
    assert_eq!(ls.len(), 3 + 3 * cases.len(), "one line per panic");
    // No line carries a message: none of these panics is in atlas's src.
    assert!(ls.iter().all(|l| l.ends_with(" at=external payload omitted")), "{:?}", ls.iter().find(|l| !l.ends_with(" payload omitted")));
    for (input, extra) in &cases {
        assert_clean(&all, &w, extra, input);
        assert_no_words(&all, input);
    }
    // 10k random texts, in batches: each batch's tokens are looked for in
    // both files before a later rotation can drop its lines.
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..10 {
        let texts: Vec<String> = (0..1000).map(|_| random_text(&mut rng)).collect();
        for text in &texts {
            three(text);
        }
        let all = read();
        assert!(all.lines().filter(|l| *l != HEADER).all(|l| l.ends_with(" at=external payload omitted")));
        let mut fw = words(&all);
        fw.sort();
        fw.dedup();
        for text in &texts {
            assert_no_tokens(&fw, &vocab, text);
        }
    }
    std::panic::set_hook(quiet);
    assert!(ran.load(Ordering::SeqCst), "the previous hook ran");
}
