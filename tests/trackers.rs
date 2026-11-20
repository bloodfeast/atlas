//! `atlas track` (project tracking) end to end through the binary and the system curl,
//! against a local mock of the Atlassian REST APIs: the token never leaks on
//! any path (the scan below reads every byte atlas wrote), a redirect to
//! another origin drops the header, and the reads, the handle and the queue
//! snapshot work over a real socket.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The sentinel token: shaped like a Cloud API token, never real.
const TOKEN: &str = "ATATT3xLEAKSENTINEL0x7f-a1b2c3d4e5f6g7h8i9j0=K9";
/// The sentinel account email.
const EMAIL: &str = "leak-sentinel-user@example.invalid";

fn base64(data: &[u8]) -> String {
    atlas::trackers::base64(data)
}

/// Every form of the secret the scan looks for. The account email is not
/// one: it passes through like any user's (journal:ATLAS-TRACKERS:0015).
fn forms() -> Vec<String> {
    vec![TOKEN.to_string(), base64(format!("{EMAIL}:{TOKEN}").as_bytes()), base64(TOKEN.as_bytes()), "LEAKSENTINEL".to_string()]
}

/// What the mock answers for one request (path with query, its index).
type Handler = dyn Fn(&str, usize) -> Answer + Send + Sync;

enum Answer {
    /// A status, extra header lines and a body.
    Http(u16, Vec<(String, String)>, Vec<u8>),
    /// Sleep, then close.
    Hang(Duration),
}

fn json(st: u16, body: &str) -> Answer {
    Answer::Http(st, vec![], body.as_bytes().to_vec())
}

struct Mock {
    port: u16,
    /// Each request's head, as received.
    heads: Arc<Mutex<Vec<String>>>,
}

fn read_head(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") && buf.len() < 65536 {
        match s.read(&mut b) {
            Ok(1) => buf.push(b[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn mock(h: Box<Handler>) -> Mock {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let seen = heads.clone();
    let h: Arc<Handler> = Arc::from(h);
    std::thread::spawn(move || {
        for (n, s) in l.incoming().enumerate() {
            let Ok(mut s) = s else { continue };
            let seen = seen.clone();
            let h = h.clone();
            std::thread::spawn(move || {
                s.set_read_timeout(Some(Duration::from_secs(10))).ok();
                let head = read_head(&mut s);
                let path = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_string();
                seen.lock().unwrap().push(head);
                match h(&path, n) {
                    Answer::Http(st, extra, body) => {
                        let mut out = format!(
                            "HTTP/1.1 {st} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                            body.len()
                        );
                        for (k, v) in extra {
                            out.push_str(&format!("{k}: {v}\r\n"));
                        }
                        out.push_str("\r\n");
                        let _ = s.write_all(out.as_bytes());
                        let _ = s.write_all(&body);
                    }
                    Answer::Hang(d) => std::thread::sleep(d),
                }
            });
        }
    });
    Mock { port, heads }
}

/// A server that answers any connection at once with plain HTTP and
/// closes: a TLS client fails its handshake against it.
fn plain_speaker() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            let _ = s.write_all(
                b"HTTP/1.1 400 Bad Request
Content-Length: 0
Connection: close

",
            );
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    });
    port
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/atlassian").join(name)).unwrap()
}

/// One run's temp tree: the config, the stores, the home folders and the
/// temp folder all live in it, so the scan sees every file atlas writes.
struct Run {
    t: common::Temp,
    cfg: PathBuf,
}

fn setup(tag: &str, site: &str, extra: &str) -> Run {
    let t = common::Temp::new(tag);
    let cfg = t.path().join("atlas.json");
    let p = |n: &str| t.path().join(n).display().to_string();
    std::fs::write(
        &cfg,
        format!(
            "{{\"db\": {:?}, \"vaults\": [], \"memory\": {{\"store\": {:?}}}, \"project_tracking\": {{\"atlassian\": {{\"site\": {site:?}, \"email_env\": \"ATL_TEST_EMAIL\", \"token_env\": \"ATL_TEST_TOKEN\", \"timeout_s\": 2, \"min_interval_ms\": 0, \"max_retries\": 1, \"backoff_ms\": 10, \"max_wait_s\": 1{extra}}}}}}}",
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

/// Runs `atlas --config CFG ARGS` (`track ...` and the other commands): (success, stdout, stderr).
fn atlas(r: &Run, args: &[&str], env: &[(&str, &str)]) -> (bool, String, String) {
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
        .env("ATL_TEST_TOKEN", TOKEN)
        .env("ATLAS_TRACK_DEBUG", "1")
        .env("NO_PROXY", "*")
        .env_remove("ATLAS_CONFIG")
        .env_remove("ATLAS_PROJECT")
        .env_remove("ATLAS_TEST_TRACKER_PANIC");
    for v in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "all_proxy", "CURL_HOME"] {
        c.env_remove(v);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.stdin(std::process::Stdio::null()).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Every file below `dir`, recursively.
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

/// Scans every output and every file of the run for the secret: returns the
/// places it was found, and the count of places scanned.
fn scan(r: &Run, outputs: &[(String, String)]) -> (Vec<String>, usize) {
    let mut hits = Vec::new();
    let mut n = 0;
    let fs = forms();
    for (name, text) in outputs {
        n += 1;
        for f in &fs {
            if text.contains(f.as_str()) {
                hits.push(format!("{name} holds {f:?}"));
            }
        }
    }
    let mut all = Vec::new();
    files(r.t.path(), &mut all);
    for p in all {
        if p == r.cfg {
            continue; // the config is written by the test; checked below
        }
        n += 1;
        let bytes = std::fs::read(&p).unwrap_or_default();
        for f in &fs {
            if bytes.windows(f.len()).any(|w| w == f.as_bytes()) {
                hits.push(format!("{} holds {f:?}", p.display()));
            }
        }
    }
    let cfg = std::fs::read_to_string(&r.cfg).unwrap();
    assert!(!fs.iter().any(|f| cfg.contains(f.as_str())), "the config names variables only");
    (hits, n)
}

/// The key test: every failure path (DNS, TLS, 401, 403, 404, 429 with
/// Retry-After, 5xx, malformed JSON, a timeout, a redirect to another host,
/// a panic in the digest) and every success path (reads, handles, check,
/// smoke, a queue snapshot) run with the sentinel; nothing atlas printed or
/// wrote holds it, its base64 basic-auth form, or the account email.
#[test]
fn the_token_never_leaks_on_any_path() {
    let mut outputs: Vec<(String, String)> = Vec::new();
    let mut scanned = 0;
    let mut record = |r: &Run, name: &str, res: (bool, String, String), outputs: &mut Vec<(String, String)>| {
        outputs.push((format!("{name} stdout"), res.1.clone()));
        outputs.push((format!("{name} stderr"), res.2.clone()));
        let (hits, n) = scan(r, outputs);
        scanned += n;
        assert!(hits.is_empty(), "{name}: the secret leaked: {hits:#?}");
        res
    };

    // DNS failure.
    let r = setup("trk-dns", "https://atlas-leak-test.invalid", "");
    let res = record(&r, "dns", atlas(&r, &["track", "issue", "PROJ-1"], &[]), &mut outputs);
    assert!(
        !res.0
            && res.2.contains("GET /rest/api/3/issue/{key}: ")
            && (res.2.contains("did not resolve") || res.2.contains("could not connect")),
        "{}",
        res.2
    );

    // A TLS error: https to a server speaking plain HTTP.
    let tls = plain_speaker();
    let r = setup("trk-tls", &format!("https://127.0.0.1:{tls}"), "");
    let res = record(&r, "tls", atlas(&r, &["track", "issue", "PROJ-1"], &[]), &mut outputs);
    assert!(!res.0 && (res.2.contains("TLS") || res.2.contains("curl failed") || res.2.contains("could not connect")), "{}", res.2);

    // Statuses: 401, 403, 404, 429 (Retry-After), 503, malformed JSON.
    for (name, st, extra, body) in [
        ("401", 401u16, vec![], "{\"errorMessages\":[\"no\"]}"),
        ("403", 403, vec![], "{}"),
        ("404", 404, vec![], "{\"errorMessages\":[\"Issue does not exist\"]}"),
        ("429", 429, vec![("Retry-After".to_string(), "1".to_string())], "{}"),
        ("5xx", 503, vec![], "<html>down</html>"),
        ("malformed", 200, vec![], "{\"key\": \"PROJ-1\", \"fields\": {"),
    ] {
        let m = mock(Box::new(move |_, _| Answer::Http(st, extra.clone(), body.as_bytes().to_vec())));
        let r = setup(&format!("trk-{name}"), &format!("http://127.0.0.1:{}", m.port), "");
        let res = record(&r, name, atlas(&r, &["track", "issue", "PROJ-1"], &[]), &mut outputs);
        let want = if name == "malformed" { "malformed JSON".to_string() } else { format!("HTTP {st} (") };
        assert!(!res.0 && res.2.contains("GET /rest/api/3/issue/{key}: ") && res.2.contains(&want), "{name}: {}", res.2);
        assert!(
            m.heads
                .lock()
                .unwrap()
                .iter()
                .all(|h| h.contains(&format!("Authorization: Basic {}", base64(format!("{EMAIL}:{TOKEN}").as_bytes())))),
            "{name}: the header was sent"
        );
        if name == "429" {
            assert_eq!(m.heads.lock().unwrap().len(), 2, "one retry after Retry-After");
        }
    }

    // A timeout.
    let m = mock(Box::new(|_, _| Answer::Hang(Duration::from_secs(5))));
    let r = setup("trk-timeout", &format!("http://127.0.0.1:{}", m.port), "");
    let res = record(&r, "timeout", atlas(&r, &["track", "issue", "PROJ-1"], &[]), &mut outputs);
    assert!(!res.0 && res.2.contains("timed out after 2 s"), "{}", res.2);

    // A redirect to another host: followed without the header.
    let other = mock(Box::new(|_, _| json(200, &fixture("jira-myself.json"))));
    let to = format!("http://localhost:{}/elsewhere", other.port);
    let m = mock(Box::new(move |_, _| Answer::Http(302, vec![("Location".into(), to.clone())], Vec::new())));
    let r = setup("trk-redirect", &format!("http://127.0.0.1:{}", m.port), "");
    record(&r, "redirect", atlas(&r, &["track", "issue", "PROJ-1"], &[]), &mut outputs);
    assert!(m.heads.lock().unwrap()[0].contains("Authorization: Basic "), "the site got the header");
    let theirs = other.heads.lock().unwrap();
    assert_eq!(theirs.len(), 1, "the redirect was followed");
    assert!(!theirs[0].to_ascii_lowercase().contains("authorization"), "the other host got the header:\n{}", theirs[0]);
    drop(theirs);

    // Success paths: reads (handles written), check, smoke, a panic in the
    // digest, a queue snapshot.
    let m = mock(Box::new(|path, _| {
        let p = path.split('?').next().unwrap_or("");
        match p {
            "/rest/api/3/issue/PROJ-123" => json(200, &fixture("jira-issue.json")),
            // `track snapshot --all` refetches the items `--from-jql` planned.
            p if p.starts_with("/rest/api/3/issue/PROJ-20") => json(200, &fixture("jira-issue.json")),
            "/rest/api/3/search/jql" if path.contains("nextPageToken") => json(200, &fixture("jira-search-2.json")),
            "/rest/api/3/search/jql" => json(200, &fixture("jira-search-1.json")),
            "/wiki/api/v2/pages/98765" => json(200, &fixture("confluence-page.json")),
            "/wiki/rest/api/search" if path.contains("cursor") => json(200, &fixture("confluence-search-2.json")),
            "/wiki/rest/api/search" => json(200, &fixture("confluence-search-1.json")),
            "/rest/api/3/myself" => json(200, &fixture("jira-myself.json")),
            "/rest/api/3/mypermissions" => json(200, "{\"permissions\":{\"CREATE_ISSUES\":{\"havePermission\":false}}}"),
            "/wiki/rest/api/user/current" => json(200, &fixture("confluence-user.json")),
            _ => json(404, "{}"),
        }
    }));
    let r = setup("trk-ok", &format!("http://127.0.0.1:{}", m.port), "");
    let res = record(&r, "issue", atlas(&r, &["track", "issue", "PROJ-123"], &[]), &mut outputs);
    assert!(res.0 && res.1.starts_with("h:") && res.1.contains("PROJ-123 [Bug] In Progress"), "{} {}", res.1, res.2);
    assert!(res.2.contains("> Authorization: [redacted]"), "debug lines are on: {}", res.2);
    let id = res.1.split_whitespace().next().unwrap().to_string();
    let res = record(&r, "jql", atlas(&r, &["track", "jql", "project = PROJ"], &[]), &mut outputs);
    assert!(res.0 && res.1.contains("5 issues, more pages: no"), "{}", res.1);
    let res = record(&r, "page", atlas(&r, &["track", "page", "98765"], &[]), &mut outputs);
    assert!(res.0 && res.1.contains("Before the freeze"), "{}", res.1);
    let res = record(&r, "cql", atlas(&r, &["track", "cql", "type = page"], &[]), &mut outputs);
    assert!(res.0 && res.1.contains("3 hits of 3"), "{}", res.1);
    // The handle is recallable by key.
    let res = record(&r, "slice", atlas(&r, &["slice", &id, "--grep", "Steps to reproduce"], &[]), &mut outputs);
    assert!(res.0 && res.1.contains("## Steps to reproduce"), "{} {}", res.1, res.2);
    let res = record(&r, "check", atlas(&r, &["track", "check"], &[]), &mut outputs);
    assert!(res.0 && res.1.contains("env ATL_TEST_TOKEN: present") && res.1.contains("jira auth: ok"), "{} {}", res.1, res.2);
    let res = record(
        &r,
        "smoke",
        atlas(&r, &["track", "smoke", "--issue", "PROJ-123", "--page", "98765", "--jql", "project = PROJ", "--cql", "type = page"], &[]),
        &mut outputs,
    );
    assert!(res.0 && res.1.contains("issue: GET /rest/api/3/issue/{key}") && !res.1.contains("Checkout fails"), "{} {}", res.1, res.2);
    let res = record(&r, "panic", atlas(&r, &["track", "issue", "PROJ-123"], &[("ATLAS_TEST_TRACKER_PANIC", "1")]), &mut outputs);
    assert!(!res.0 && res.2.contains("the digest failed; the raw response is kept as h:"), "{} {}", res.1, res.2);
    let res = record(
        &r,
        "plan",
        atlas(&r, &["queue", "plan", "--lane", "LEAK", "--title", "Fix checkout", "--ticket", "PROJ-123"], &[]),
        &mut outputs,
    );
    assert!(res.0, "{} {}", res.1, res.2);
    let res = record(&r, "snapshot", atlas(&r, &["track", "snapshot", "journal:LEAK:0001"], &[]), &mut outputs);
    assert!(res.0 && res.1.contains("PROJ-123 In Progress (Avery Example)"), "{} {}", res.1, res.2);
    let res = record(&r, "show", atlas(&r, &["queue", "show", "journal:LEAK:0001"], &[]), &mut outputs);
    assert!(res.0 && res.1.contains("ticket_status: In Progress"), "{} {}", res.1, res.2);
    // The two paths that write tracker text to the store in bulk (#318 r2 S1).
    let res = record(
        &r,
        "plan-jql",
        atlas(&r, &["queue", "plan", "--from-jql", "project = PROJ", "--lane-from", "fixed:LEAK2"], &[]),
        &mut outputs,
    );
    assert!(res.0 && res.1.contains("planned journal:LEAK2:") && res.1.contains("from-jql: "), "{} {}", res.1, res.2);
    let res = record(&r, "snapshot-all", atlas(&r, &["track", "snapshot", "--all", "--older-than", "0s"], &[]), &mut outputs);
    assert!(res.0 && res.1.contains("snapshots: ") && !res.1.contains("snapshots: 0 refreshed"), "{} {}", res.1, res.2);

    // The scan list, for the record: every stdout and stderr above, and
    // every file each run's tree holds (stores, WAL, handles, logs, homes).
    println!("leak scan: {} places scanned, forms: token, base64(email:token), base64(token)", scanned);
    assert!(scanned > 40, "the scan covered the runs: {scanned}");
}

/// A token pasted into the configuration is refused, and not echoed.
#[test]
fn a_literal_token_in_the_config_is_refused_unechoed() {
    let t = common::Temp::new("trk-literal");
    let cfg = t.path().join("atlas.json");
    std::fs::write(&cfg, format!("{{\"db\": {:?}, \"vaults\": [], \"project_tracking\": {{\"atlassian\": {{\"site\": \"https://x.atlassian.net\", \"email_env\": \"E\", \"token_env\": {TOKEN:?}}}}}}}", t.path().join("i.sqlite").display().to_string())).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"))
        .arg("--config")
        .arg(&cfg)
        .args(["track", "check"])
        .env("ATLAS_HOME", t.path().join("home-atlas"))
        .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
        .env("HOME", t.path())
        .env("USERPROFILE", t.path())
        .env_remove("ATLAS_CONFIG")
        .env_remove("ATLAS_PROJECT")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let (o, e) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success() && e.contains("token_env must name an environment variable"), "{o} {e}");
    assert!(!o.contains("LEAKSENTINEL") && !e.contains("LEAKSENTINEL"), "{o} {e}");
}

/// The feature is project tracking, `atlas track`; the old `trackers`
/// command and config key were never released, so neither is kept
/// (journal:ATLAS-TRACKERS:0012).
#[test]
fn the_old_trackers_command_and_key_are_gone() {
    let r = setup("trk-old", "https://x.atlassian.net", "");
    let (ok, o, e) = atlas(&r, &["trackers", "check"], &[]);
    assert!(!ok && !o.contains("project_tracking.atlassian ok"), "{o} {e}");
    let (ok, o, e) = atlas(&r, &["track", "--help"], &[]);
    assert!(ok && o.contains("atlas track check") && !o.contains("atlas trackers"), "{o} {e}");
    let cfg = r.t.path().join("old.json");
    std::fs::write(&cfg, "{\"vaults\": [], \"trackers\": {\"atlassian\": {\"site\": \"https://x.atlassian.net\", \"pat_env\": \"P\"}}}")
        .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"))
        .arg("--config")
        .arg(&cfg)
        .args(["track", "check"])
        .env("ATLAS_HOME", r.t.path().join("home-atlas"))
        .env("CLAUDE_CONFIG_DIR", r.t.path().join("claude"))
        .env_remove("ATLAS_CONFIG")
        .env_remove("ATLAS_PROJECT")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && e.contains("unknown field \"trackers\"") && e.contains("project_tracking"),
        "the old key is refused as unknown, naming the new one among the known fields: {e}"
    );
}

/// A configured but unset `confluence_token_env` does not stop Jira work:
/// an issue read and a queue snapshot need Jira's variables only, and
/// Confluence's is read on the first Confluence request.
#[test]
fn jira_reads_and_snapshots_work_with_the_confluence_variable_unset() {
    let m = mock(Box::new(|path, _| match path.split('?').next().unwrap_or("") {
        "/rest/api/3/issue/PROJ-123" => json(200, &fixture("jira-issue.json")),
        _ => json(404, "{}"),
    }));
    let r = setup("trk-lazyconf", &format!("http://127.0.0.1:{}", m.port), ", \"confluence_token_env\": \"ATL_TEST_CONF_UNSET\"");
    let unset = [("ATL_TEST_CONF_UNSET", "")];
    let (ok, o, e) = atlas(&r, &["track", "issue", "PROJ-123"], &unset);
    assert!(ok && o.contains("PROJ-123 [Bug] In Progress"), "{o} {e}");
    let (ok, o, e) = atlas(&r, &["queue", "plan", "--lane", "LAZY", "--title", "Fix checkout", "--ticket", "PROJ-123"], &unset);
    assert!(ok, "{o} {e}");
    let (ok, o, e) = atlas(&r, &["track", "snapshot", "journal:LAZY:0001"], &unset);
    assert!(ok && o.contains("PROJ-123 In Progress (Avery Example)") && o.contains("via rest"), "{o} {e}");
    // A Confluence read names the missing variable.
    let (ok, o, e) = atlas(&r, &["track", "page", "98765"], &unset);
    assert!(!ok && e.contains("ATL_TEST_CONF_UNSET is not set"), "{o} {e}");
    assert_eq!(m.heads.lock().unwrap().len(), 2, "the page read sent nothing");
}
