//! `pr_events.repos` (#384), the gh user kept per host (#394), and
//! `pr_events.autostart`'s settings (#386).

use std::io;
use std::process::Output;
use std::sync::Mutex;

use super::tests::{ctx, fixture, plan, post, sign, store, SECRET};
use super::*;

fn settings(json: &str) -> std::result::Result<Settings, String> {
    Settings::from_config(Some(&crate::json::parse(json).unwrap()))
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn repos_limits_what_serve_serves_and_none_serves_all() {
    let served = strings(&["octo-org/widgets", "octo-org/gadgets", "corp/tools", "localonly"]);
    let forwarded = strings(&["octo-org/widgets", "octo-org/gadgets", "ghe.corp.example/corp/tools"]);
    // Absent: every repository with a remote, as before.
    let all = Settings::from_config(None).unwrap();
    assert_eq!(all.allowed(&served, &forwarded), (served.clone(), forwarded.clone(), Vec::new()));
    // Named: those only, matched without case, an Enterprise one by its host.
    let s = settings(r#"{"port": 8765, "repos": ["Octo-Org/Widgets", "ghe.corp.example/corp/tools", "octo-org/missing"]}"#).unwrap();
    let (served2, forwarded2, unknown) = s.allowed(&served, &forwarded);
    assert_eq!(forwarded2, strings(&["octo-org/widgets", "ghe.corp.example/corp/tools"]));
    assert_eq!(served2, strings(&["octo-org/widgets", "corp/tools"]));
    assert_eq!(unknown, strings(&["octo-org/missing"]));
    // `OWNER/NAME` is github.com's: it does not pick a repository of that
    // name on another host.
    let s = settings(r#"{"repos": ["corp/tools"]}"#).unwrap();
    let (_, f, unknown) = s.allowed(&served, &forwarded);
    assert!(f.is_empty());
    assert_eq!(unknown, strings(&["corp/tools"]));
}

#[test]
fn repos_and_autostart_read_from_the_configuration() {
    let s = settings(r#"{"port": 8765, "repos": ["octo-org/widgets", "github.com/octo-org/widgets"], "autostart": true}"#).unwrap();
    assert_eq!(s.repos.len(), 1, "github.com/OWNER/NAME is OWNER/NAME");
    assert!(s.autostart);
    assert!(!Settings::from_config(None).unwrap().autostart, "off by default");
    for bad in [r#"{"repos": "octo-org/widgets"}"#, r#"{"repos": ["widgets"]}"#, r#"{"repos": ["a b/c"]}"#, r#"{"autostart": "yes"}"#] {
        assert!(settings(bad).is_err(), "{bad}");
    }
    // The listener SessionStart starts takes no flags: it needs the port.
    assert!(settings(r#"{"autostart": true}"#).unwrap_err().contains("pr_events.port"));
}

fn deliver(s: &mut MemoryStore, c: &Ctx, event: &str, body: &[u8]) -> Outcome {
    handle(s, c, &post(event, body, Some(sign(SECRET, body))))
}

#[test]
fn the_gh_user_s_login_on_one_host_does_not_drop_another_host_s_person() {
    // `octo-builder` is the gh user on github.com; on the Enterprise host a
    // different person has the same login, and their comments are heard.
    let (_d, mut s) = store("perhost");
    let c =
        Ctx { ignore: Ignore::new(&[(None, "octo-builder".into()), (Some("ghe.corp.example".into()), "jdoe_corp".into())], &[]), ..ctx() };
    plan(&mut s, "L", "review pr#12");
    let comment = String::from_utf8(fixture("issue_comment_by_builder.json")).unwrap();
    assert!(comment.contains("\"html_url\": \"https://github.com/octo-org/widgets\""), "the fixture names its host");
    let o = deliver(&mut s, &c, "issue_comment", comment.as_bytes());
    assert_eq!(o.status, 204, "the gh user on github.com: {}", o.what);
    let on_ghe = comment.replace("https://github.com/octo-org/widgets", "https://GHE.corp.example/octo-org/widgets");
    let o = deliver(&mut s, &c, "issue_comment", on_ghe.as_bytes());
    assert_eq!(o.status, 200, "another person on the Enterprise host: {}", o.what);
    // The Enterprise host's gh user is ignored there.
    let theirs = on_ghe.replace("octo-builder", "jdoe_corp");
    assert_eq!(deliver(&mut s, &c, "issue_comment", theirs.as_bytes()).status, 204);
    // A payload without the URL names no host: the comment is heard.
    let no_host = comment.replace("\"html_url\": \"https://github.com/octo-org/widgets\", ", "");
    assert_ne!(no_host, comment);
    assert_eq!(deliver(&mut s, &c, "issue_comment", no_host.as_bytes()).status, 200);
}

/// A gh that answers `api user` per host, noting the thread each call
/// runs on.
struct HostsGh {
    threads: Mutex<Vec<std::thread::ThreadId>>,
}

impl pr_supervise::Gh for HostsGh {
    fn run(&self, args: &[String]) -> io::Result<Output> {
        self.threads.lock().unwrap().push(std::thread::current().id());
        let host = (args.get(1).map(String::as_str) == Some("--hostname")).then(|| args[2].clone());
        let out = match host.as_deref() {
            None => "octo-builder\n",
            Some("ghe.corp.example") => "jdoe_corp\n",
            Some(_) => return Err(io::Error::from(io::ErrorKind::TimedOut)),
        };
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(0)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(0)
        };
        Ok(Output { status, stdout: out.as_bytes().to_vec(), stderr: Vec::new() })
    }
    fn spawn(&self, _: &[String]) -> io::Result<std::process::Child> {
        unreachable!("gh_users only runs gh")
    }
}

#[test]
fn the_gh_user_is_asked_of_every_host_at_once_and_kept_per_host() {
    let gh = HostsGh { threads: Mutex::new(Vec::new()) };
    let hosts = vec![None, Some("ghe.corp.example".to_string()), Some("down.example".to_string())];
    let got = gh_users(&gh, &hosts);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], (None, Ok("octo-builder".to_string())));
    assert_eq!(got[1], (Some("ghe.corp.example".to_string()), Ok("jdoe_corp".to_string())));
    assert!(got[2].1.is_err(), "an unreachable host names no one");
    // Each host on a thread of its own, none on the caller's: an
    // unreachable host does not hold up the others.
    let mut threads = gh.threads.into_inner().unwrap();
    assert!(!threads.contains(&std::thread::current().id()), "the hosts were asked on the caller's thread, one after another");
    threads.sort_by_key(|t| format!("{t:?}"));
    threads.dedup();
    assert_eq!(threads.len(), 3, "the hosts were asked one after another");
}

/// `atlas pr-events ARGS` on the store in `d`.
fn serve_cli(d: &super::tests::Dir, s: &mut MemoryStore, args: &[&str], forwarded: &[&str], settings: &Settings) -> std::result::Result<ExitCode, String> {
    let forwarded = strings(forwarded);
    cli(&strings(args), s, &d.0.join("memory.sqlite"), &forwarded, &forwarded, settings)
}

#[test]
fn serve_stops_when_repos_names_none_of_the_project_s_repositories() {
    let (d, mut s) = store("reposnone");
    let set = settings(r#"{"port": 8765, "repos": ["octo-org/missing"]}"#).unwrap();
    let e = serve_cli(&d, &mut s, &["serve"], &["octo-org/widgets"], &set).unwrap_err();
    assert!(e.contains("names none of the active project's repositories"), "{e}");
}

#[test]
fn a_hand_started_serve_serves_without_a_lock_it_cannot_take() {
    // #402: the lock path cannot be opened (a folder stands there, as an
    // error other than "held" or "no locks"). By hand: on to the next check;
    // with --auto: the error.
    let (d, mut s) = store("lockerror");
    std::fs::create_dir_all(crate::pr_autostart::Paths::for_store(&d.0.join("memory.sqlite")).lock).unwrap();
    let set = settings(r#"{"port": 8765}"#).unwrap();
    let e = serve_cli(&d, &mut s, &["serve"], &[], &set).unwrap_err();
    assert!(e.contains("no repository to serve"), "the hand start stopped at the lock: {e}");
    let e = serve_cli(&d, &mut s, &["serve", "--auto"], &[], &set).unwrap_err();
    assert!(e.contains(".pr-events.lock"), "{e}");
}
