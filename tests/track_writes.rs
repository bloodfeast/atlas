//! `atlas track comment | transition | edit | page-update` end to end through
//! the binary and the system curl, against a local mock of the Atlassian REST
//! APIs that reads request bodies: a read token and a write token (two
//! sentinels) are used each only where it belongs, a dry-run sends no POST or
//! PUT, an applied write is journaled, and neither sentinel appears in
//! anything atlas printed or wrote.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const READ: &str = "ATATT3xREADSENTINEL0x7f-a1b2c3d4e5f6g7h8i9j0=K9";
const WRITE: &str = "ATATT3xWRITESENTINEL0x7f-z9y8x7w6v5u4t3s2r1q0=K9";
const EMAIL: &str = "leak-sentinel-user@example.invalid";

fn b64(data: &[u8]) -> String {
    atlas::trackers::base64(data)
}

/// Every form of either secret the scan looks for.
fn forms() -> Vec<String> {
    let mut v = Vec::new();
    for t in [READ, WRITE] {
        v.extend([t.to_string(), b64(format!("{EMAIL}:{t}").as_bytes()), b64(t.as_bytes())]);
    }
    v.extend(["READSENTINEL".to_string(), "WRITESENTINEL".to_string()]);
    v
}

/// One request the mock saw: the head and the body.
#[derive(Clone)]
struct Seen {
    head: String,
    body: String,
}

impl Seen {
    fn method(&self) -> &str {
        self.head.split_whitespace().next().unwrap_or("")
    }
    fn path(&self) -> &str {
        self.head.split_whitespace().nth(1).unwrap_or("")
    }
}

fn read_request(s: &mut TcpStream) -> Seen {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") && buf.len() < 65536 {
        match s.read(&mut b) {
            Ok(1) => buf.push(b[0]),
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let len: usize = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    let _ = s.read_exact(&mut body);
    Seen { head, body: String::from_utf8_lossy(&body).into_owned() }
}

/// A mock answering by (method, path with query): (status, body).
type Handler = dyn Fn(&Seen) -> (u16, String) + Send + Sync;

fn mock(h: Box<Handler>) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let h: Arc<Handler> = Arc::from(h);
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let (log, h) = (log.clone(), h.clone());
            std::thread::spawn(move || {
                s.set_read_timeout(Some(Duration::from_secs(10))).ok();
                let req = read_request(&mut s);
                log.lock().unwrap().push(req.clone());
                let (st, body) = h(&req);
                let out = format!(
                    "HTTP/1.1 {st} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(out.as_bytes());
            });
        }
    });
    (port, seen)
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/atlassian").join(name)).unwrap()
}

const PAGE_BODY: &str = "<h1>Intro</h1><p>hello</p><h2>Rollout</h2><p>old plan</p><h2>Other</h2><p>o</p>";

fn page(version: u64) -> String {
    format!(
        r#"{{"id":"123","status":"current","title":"Release","spaceId":"4456","version":{{"number":{version}}},"body":{{"storage":{{"representation":"storage","value":"{PAGE_BODY}"}}}}}}"#
    )
}

/// The site: GETs read, POSTs and PUTs write.
fn site(r: &Seen) -> (u16, String) {
    match (r.method(), r.path().split('?').next().unwrap_or("")) {
        ("GET", "/rest/api/3/issue/PROJ-12") => (200, r#"{"key":"PROJ-12","fields":{"project":{"key":"PROJ"},"status":{"name":"In Progress","id":"3"}}}"#.into()),
        ("GET", "/rest/api/3/issue/PROJ-12/transitions") => (200, fixture("writes/jira-transitions.json")),
        ("POST", "/rest/api/3/issue/PROJ-12/transitions") => (204, String::new()),
        ("POST", "/rest/api/3/issue/PROJ-12/comment") => (201, fixture("writes/jira-comment-created.json")),
        ("PUT", "/rest/api/3/issue/PROJ-12") => (204, String::new()),
        ("GET", "/wiki/api/v2/pages/123") => (200, page(6)),
        ("PUT", "/wiki/api/v2/pages/123") => (200, page(7)),
        _ => (404, r#"{"errorMessages":["no such thing"]}"#.into()),
    }
}

struct Run {
    t: common::Temp,
    cfg: PathBuf,
}

fn setup(tag: &str, port: u16, extra: &str) -> Run {
    let t = common::Temp::new(tag);
    let cfg = t.path().join("atlas.json");
    let p = |n: &str| t.path().join(n).display().to_string();
    std::fs::write(
        &cfg,
        format!(
            "{{\"db\": {:?}, \"vaults\": [], \"memory\": {{\"store\": {:?}}}, \"project_tracking\": {{\"atlassian\": {{\"site\": \"http://127.0.0.1:{port}\", \"email_env\": \"ATL_TEST_EMAIL\", \"token_env\": \"ATL_TEST_TOKEN\", \"timeout_s\": 5, \"min_interval_ms\": 0, \"max_retries\": 1, \"backoff_ms\": 10, \"max_wait_s\": 1{extra}}}}}}}",
            p("state/index.sqlite"),
            p("state/memory.sqlite")
        ),
    )
    .unwrap();
    for d in ["home-atlas", "claude", "tmp"] {
        std::fs::create_dir_all(t.path().join(d)).unwrap();
    }
    Run { t, cfg }
}

const WRITES: &str = r#", "jira_write_token_env": "ATL_TEST_WTOKEN", "confluence_write_token_env": "ATL_TEST_WTOKEN", "writes": {"jira": {"PROJ": ["comment", "transition", "labels"]}, "confluence": {"4456": ["page_update"]}}"#;

fn atlas(r: &Run, args: &[&str]) -> (bool, String, String) {
    let tmp = r.t.path().join("tmp");
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"));
    c.arg("--config").arg(&r.cfg).args(args);
    c.env("ATLAS_HOME", r.t.path().join("home-atlas"))
        .env("CLAUDE_CONFIG_DIR", r.t.path().join("claude"))
        .env("HOME", r.t.path())
        .env("USERPROFILE", r.t.path())
        .env("TMP", &tmp)
        .env("TEMP", &tmp)
        .env("TMPDIR", &tmp)
        .env("ATL_TEST_EMAIL", EMAIL)
        .env("ATL_TEST_TOKEN", READ)
        .env("ATL_TEST_WTOKEN", WRITE)
        .env("ATLAS_TRACK_DEBUG", "1")
        .env("NO_PROXY", "*")
        .env_remove("ATLAS_CONFIG")
        .env_remove("ATLAS_PROJECT");
    for v in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "all_proxy", "CURL_HOME"] {
        c.env_remove(v);
    }
    let out = c.stdin(std::process::Stdio::null()).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Every place the secrets must not be: what atlas printed, and every file of the run.
fn scan(r: &Run, outputs: &[(String, String)]) -> Vec<String> {
    let mut hits = Vec::new();
    let fs = forms();
    for (name, text) in outputs {
        hits.extend(fs.iter().filter(|f| text.contains(f.as_str())).map(|f| format!("{name} holds {f:?}")));
    }
    let mut all = Vec::new();
    files(r.t.path(), &mut all);
    for p in all.into_iter().filter(|p| *p != r.cfg) {
        let bytes = std::fs::read(&p).unwrap_or_default();
        hits.extend(
            fs.iter().filter(|f| bytes.windows(f.len()).any(|w| w == f.as_bytes())).map(|f| format!("{} holds {f:?}", p.display())),
        );
    }
    hits
}

fn auth_of(s: &Seen) -> String {
    s.head.lines().find_map(|l| l.strip_prefix("Authorization: ")).unwrap_or("").to_string()
}

#[test]
fn the_four_writes_end_to_end_use_the_right_token_and_leak_neither() {
    let (port, seen) = mock(Box::new(site));
    let r = setup("trkw-e2e", port, WRITES);
    let mut outputs: Vec<(String, String)> = Vec::new();
    let mut run = |name: &str, args: &[&str]| {
        let res = atlas(&r, args);
        outputs.push((format!("{name} stdout"), res.1.clone()));
        outputs.push((format!("{name} stderr"), res.2.clone()));
        res
    };
    // Dry-runs first: nothing is written.
    for (name, args) in [
        ("dry-comment", vec!["track", "comment", "PROJ-12", "--body", "Deployed."]),
        ("dry-transition", vec!["track", "transition", "PROJ-12", "--to", "Done"]),
        ("dry-edit", vec!["track", "edit", "PROJ-12", "--labels-add", "a"]),
        ("dry-page", vec!["track", "page-update", "123", "--section", "Rollout", "--body", "<p>new</p>"]),
    ] {
        let res = run(name, &args);
        assert!(res.0 && res.1.contains("send: add --apply") && res.1.contains("Authorization: [redacted]"), "{name}: {} {}", res.1, res.2);
    }
    assert!(seen.lock().unwrap().iter().all(|s| s.method() == "GET"), "a dry-run sent a write");
    // Applied.
    let c = run("comment", &["track", "comment", "PROJ-12", "--body", "Deployed.", "--apply"]);
    assert!(c.0 && c.1.trim() == "PROJ-12 comment 10234 added journal:PROJECT-TRACKING:0001", "{} {}", c.1, c.2);
    let t = run("transition", &["track", "transition", "PROJ-12", "--to", "Done", "--apply"]);
    assert!(t.0 && t.1.trim() == "PROJ-12 In Progress -> Done journal:PROJECT-TRACKING:0002", "{} {}", t.1, t.2);
    let e = run("edit", &["track", "edit", "PROJ-12", "--labels-add", "a", "--labels-remove", "b", "--apply"]);
    assert!(e.0 && e.1.trim() == "PROJ-12 labels +a -b journal:PROJECT-TRACKING:0003", "{} {}", e.1, e.2);
    let p = run("page", &["track", "page-update", "123", "--section", "Rollout", "--body", "<p>new</p>", "--apply"]);
    assert!(p.0 && p.1.trim() == "page 123 v6 -> v7 section Rollout (+10/-15 B) journal:PROJECT-TRACKING:0004", "{} {}", p.1, p.2);
    // A refusal: the allow-list has no assignee; exit status is failure and nothing is sent.
    let n_before = seen.lock().unwrap().len();
    let refused = run("refused", &["track", "edit", "PROJ-12", "--assignee", "none", "--apply"]);
    assert!(
        !refused.0 && refused.1.is_empty() && refused.2.contains("assignee is not allowed for Jira project PROJ"),
        "{} {}",
        refused.1,
        refused.2
    );
    assert_eq!(seen.lock().unwrap().len(), n_before);
    // The right header on every request.
    let (rh, wh) =
        (format!("Basic {}", b64(format!("{EMAIL}:{READ}").as_bytes())), format!("Basic {}", b64(format!("{EMAIL}:{WRITE}").as_bytes())));
    for s in seen.lock().unwrap().iter() {
        let want = if s.method() == "GET" { &rh } else { &wh };
        assert_eq!(&auth_of(s), want, "{} {}", s.method(), s.path());
    }
    let sent: Vec<(String, String)> =
        seen.lock().unwrap().iter().filter(|s| s.method() != "GET").map(|s| (s.method().to_string(), s.body.clone())).collect();
    assert_eq!(sent.len(), 4, "{sent:?}");
    assert!(sent[0].1.contains("\"type\":\"doc\"") && sent[0].1.contains("Deployed."), "{}", sent[0].1);
    assert_eq!(sent[1].1, r#"{"transition":{"id":"31"}}"#);
    assert_eq!(sent[2].1, r#"{"update":{"labels":[{"add":"a"},{"remove":"b"}]}}"#);
    assert!(sent[3].1.contains("<h2>Rollout</h2><p>new</p><h2>Other</h2>") && sent[3].1.contains("\"number\":7"), "{}", sent[3].1);
    // The journal is in the store, with no token in it.
    let hits = scan(&r, &outputs);
    assert!(hits.is_empty(), "a secret leaked: {hits:#?}");
}

#[test]
fn failures_and_a_conflict_print_no_secret_and_never_retry_a_page() {
    // The site echoes the write header in a 400 and answers a page PUT with 409.
    let (port, seen) = mock(Box::new(|r: &Seen| match (r.method(), r.path().split('?').next().unwrap_or("")) {
        ("POST", "/rest/api/3/issue/PROJ-12/comment") => (400, format!(r#"{{"errorMessages":["echo {}"]}}"#, auth_of(r))),
        ("PUT", "/wiki/api/v2/pages/123") => {
            (409, r#"{"errors":[{"status":409,"code":"CONFLICT","title":"Version must be incremented"}]}"#.into())
        }
        _ => site(r),
    }));
    let r = setup("trkw-fail", port, WRITES);
    let mut outputs = Vec::new();
    let c = atlas(&r, &["track", "comment", "PROJ-12", "--body", "x", "--apply"]);
    assert!(!c.0 && c.2.contains("HTTP 400") && c.2.contains("Basic [redacted]"), "{} {}", c.1, c.2);
    outputs.extend([("comment stdout".to_string(), c.1), ("comment stderr".to_string(), c.2)]);
    let p = atlas(&r, &["track", "page-update", "123", "--body", "<p>x</p>", "--apply"]);
    assert!(!p.0 && p.2.contains("not written: the page changed since it was read (now version 6, sent 7)"), "{} {}", p.1, p.2);
    outputs.extend([("page stdout".to_string(), p.1), ("page stderr".to_string(), p.2)]);
    assert_eq!(seen.lock().unwrap().iter().filter(|s| s.method() == "PUT").count(), 1, "the conflicting page PUT was not retried");
    let hits = scan(&r, &outputs);
    assert!(hits.is_empty(), "a secret leaked: {hits:#?}");
}

#[test]
fn a_read_only_setup_refuses_before_any_request() {
    let (port, seen) = mock(Box::new(site));
    let r = setup("trkw-ro", port, "");
    let res = atlas(&r, &["track", "comment", "PROJ-12", "--body", "x", "--apply"]);
    assert!(!res.0, "{} {}", res.1, res.2);
    assert!(
        res.2.contains("atlas track comment: no jira_write_token_env in project_tracking.atlassian: this setup is read-only; nothing sent"),
        "{} {}",
        res.1,
        res.2
    );
    assert!(seen.lock().unwrap().is_empty());
}
