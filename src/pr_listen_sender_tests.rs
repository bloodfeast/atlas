//! The sender filter (#385, #390): a review, review comment or PR comment by
//! a bot or by a login in `pr_events.ignore_logins`, and a review comment or
//! PR comment by the authenticated gh user, files nothing, so it neither
//! meets a wait nor surfaces a notice; a person's review, the gh user's
//! included, still meets the wait.

use std::io;
use std::process::Output;

use super::tests::{ctx, fixture, plan, post, sign, store, SECRET};
use super::*;
use crate::queue;

/// Delivers `body` as `event`, signed, and answers the status and the line.
fn deliver(s: &mut MemoryStore, c: &Ctx, event: &str, body: &[u8]) -> Outcome {
    handle(s, c, &post(event, body, Some(sign(SECRET, body))))
}

/// Nothing filed: no wait met and no notice to surface.
fn nothing_filed(s: &MemoryStore) {
    assert!(queue::Marks::read(s).unwrap().met.is_empty());
    assert!(pr_events::notices_after(s, 0).unwrap().0.is_empty());
}

#[test]
fn a_copilot_review_meets_no_wait_and_a_person_s_review_does() {
    let (_d, mut s) = store("copilot");
    let c = ctx();
    let waits = plan(&mut s, "L", "review pr#12");
    let copilot = String::from_utf8(fixture("review_submitted_copilot.json")).unwrap();
    // `Copilot`, and the reviewer login it is also seen under, carry no
    // `[bot]` suffix: the user type says Bot.
    for who in ["Copilot", "copilot-pull-request-reviewer"] {
        let b = copilot.replace("\"login\": \"Copilot\"", &format!("\"login\": \"{who}\""));
        let o = deliver(&mut s, &c, "pull_request_review", b.as_bytes());
        assert_eq!(o.status, 204, "{who}: {}", o.what);
    }
    // Only the sender says Bot.
    let b = copilot.replacen("\"type\": \"Bot\", \"site_admin\"", "\"type\": \"User\", \"site_admin\"", 1);
    assert_eq!(deliver(&mut s, &c, "pull_request_review", b.as_bytes()).status, 204);
    // No type at all: the `[bot]` suffix still drops it.
    let b = copilot.replace("\"type\": \"Bot\"", "\"type\": \"\"").replace("\"login\": \"Copilot\"", "\"login\": \"octo-helper[bot]\"");
    assert_eq!(deliver(&mut s, &c, "pull_request_review", b.as_bytes()).status, 204);
    nothing_filed(&s);
    // A person's review still meets the wait.
    let o = deliver(&mut s, &c, "pull_request_review", &fixture("review_submitted.json"));
    assert_eq!(o.status, 200, "{}", o.what);
    assert_eq!(queue::Marks::read(&s).unwrap().met, vec![(waits, "review pr#12".to_string())]);
}

#[test]
fn a_bot_s_review_comment_and_pr_comment_surface_no_notice() {
    let (_d, mut s) = store("botcomments");
    let c = ctx();
    plan(&mut s, "L", "review pr#12");
    let o = deliver(&mut s, &c, "pull_request_review_comment", &fixture("review_comment_bot.json"));
    assert_eq!(o.status, 204, "{}", o.what);
    let o = deliver(&mut s, &c, "issue_comment", &fixture("issue_comment_created.json"));
    assert_eq!(o.status, 204, "{}", o.what);
    nothing_filed(&s);
    // A person's review comment still files its notice.
    let o = deliver(&mut s, &c, "pull_request_review_comment", &fixture("review_comment_created.json"));
    assert_eq!(o.status, 200, "{}", o.what);
    assert_eq!(pr_events::notices_after(&s, 0).unwrap().0.len(), 1);
}

#[test]
fn the_gh_user_s_own_comments_surface_no_notice() {
    let (_d, mut s) = store("ownuser");
    let c = Ctx { ignore: Ignore::new(&["Octo-Builder".into()], &[]), ..ctx() };
    plan(&mut s, "L", "review pr#12");
    let comment = fixture("issue_comment_by_builder.json");
    let o = deliver(&mut s, &c, "issue_comment", &comment);
    assert_eq!(o.status, 204, "{}", o.what);
    let review_comment = String::from_utf8(fixture("review_comment_created.json")).unwrap().replace("octo-reviewer", "octo-builder");
    assert_eq!(deliver(&mut s, &c, "pull_request_review_comment", review_comment.as_bytes()).status, 204);
    nothing_filed(&s);
    // Another user's listener files the same comment.
    let o = deliver(&mut s, &ctx(), "issue_comment", &comment);
    assert_eq!(o.status, 200, "{}", o.what);
}

#[test]
fn the_gh_user_s_own_review_still_meets_the_wait_and_their_comment_does_not() {
    // The gh user is also the person: their review on a PR an item waits on
    // is the review the wait is for.
    let (_d, mut s) = store("ownreview");
    let c = Ctx { ignore: Ignore::new(&["Octo-Builder".into(), "jdoe_acme".into()], &[]), ..ctx() };
    let waits = plan(&mut s, "L", "review pr#12");
    let comment = String::from_utf8(fixture("issue_comment_by_builder.json")).unwrap();
    for who in ["octo-builder", "JDOE_acme"] {
        let b = comment.replace("octo-builder", who);
        assert_eq!(deliver(&mut s, &c, "issue_comment", b.as_bytes()).status, 204, "{who}");
    }
    nothing_filed(&s);
    let review = String::from_utf8(fixture("review_submitted.json")).unwrap().replace("octo-reviewer", "octo-builder");
    let o = deliver(&mut s, &c, "pull_request_review", review.as_bytes());
    assert_eq!(o.status, 200, "{}", o.what);
    assert_eq!(queue::Marks::read(&s).unwrap().met, vec![(waits, "review pr#12".to_string())]);
}

#[test]
fn a_bot_sender_with_no_user_type_and_no_login_files_nothing() {
    let (_d, mut s) = store("botsender");
    let c = ctx();
    plan(&mut s, "L", "review pr#12");
    let copilot = String::from_utf8(fixture("review_submitted_copilot.json")).unwrap();
    let b = copilot.replacen(
        "\"login\": \"Copilot\", \"id\": 175728472, \"type\": \"Bot\", \"site_admin\"",
        "\"login\": \"\", \"id\": 175728472, \"type\": \"\", \"site_admin\"",
        1,
    );
    assert_ne!(b, copilot, "the fixture's review user changed");
    let o = deliver(&mut s, &c, "pull_request_review", b.as_bytes());
    assert_eq!(o.status, 204, "{}", o.what);
    nothing_filed(&s);
}

#[test]
fn an_ignored_login_files_nothing_and_another_person_still_meets_the_wait() {
    let (_d, mut s) = store("ignorelogins");
    let c = Ctx { ignore: Ignore::new(&[], &["octo-lint-runner".into(), "OCTO-REVIEWER-TWO".into(), "lint_acme".into()]), ..ctx() };
    let waits = plan(&mut s, "L", "review pr#12");
    let review = String::from_utf8(fixture("review_submitted.json")).unwrap();
    let comment = String::from_utf8(fixture("review_comment_created.json")).unwrap();
    for who in ["octo-lint-runner", "octo-reviewer-two", "LINT_ACME"] {
        let b = review.replace("octo-reviewer", who);
        assert_eq!(deliver(&mut s, &c, "pull_request_review", b.as_bytes()).status, 204, "{who}");
        let b = comment.replace("octo-reviewer", who);
        assert_eq!(deliver(&mut s, &c, "pull_request_review_comment", b.as_bytes()).status, 204, "{who}");
    }
    nothing_filed(&s);
    let o = deliver(&mut s, &c, "pull_request_review", review.as_bytes());
    assert_eq!(o.status, 200, "{}", o.what);
    assert_eq!(queue::Marks::read(&s).unwrap().met, vec![(waits, "review pr#12".to_string())]);
}

#[test]
fn ignore_logins_reads_from_the_configuration() {
    // `x_acme`: an Enterprise Managed User's login carries `_`.
    let v = crate::json::parse(r#"{"port": 8765, "ignore_logins": ["octo-lint-runner", "Octo-Bot", "x_acme"]}"#).unwrap();
    let got = Settings::from_config(Some(&v)).unwrap();
    assert_eq!(got.ignore_logins, ["octo-lint-runner", "Octo-Bot", "x_acme"]);
    assert!(Settings::from_config(None).unwrap().ignore_logins.is_empty());
    let bads = [r#"{"ignore_logins": "octo"}"#, r#"{"ignore_logins": [1]}"#, r#"{"ignore_logins": ["a b"]}"#, r#"{"ignore_logins": [""]}"#];
    for bad in bads {
        assert!(Settings::from_config(Some(&crate::json::parse(bad).unwrap())).is_err(), "{bad}");
    }
}

/// A gh that answers `api user` with a set output.
struct UserGh(io::Result<(i32, &'static str)>);

/// The arguments `gh_user` sends on `host`.
fn user_args(host: Option<&str>) -> Vec<String> {
    let mut v = vec!["api".to_string()];
    if let Some(h) = host {
        v.extend(["--hostname".to_string(), h.to_string()]);
    }
    v.extend(["user", "--jq", ".login"].map(String::from));
    v
}

impl pr_supervise::Gh for UserGh {
    fn run(&self, args: &[String]) -> io::Result<Output> {
        let host = (args.get(1).map(String::as_str) == Some("--hostname")).then(|| args[2].as_str());
        assert_eq!(args, user_args(host));
        match &self.0 {
            Ok((code, out)) => Ok(Output { status: exit(*code), stdout: out.as_bytes().to_vec(), stderr: Vec::new() }),
            Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
        }
    }
    fn spawn(&self, _: &[String]) -> io::Result<std::process::Child> {
        unreachable!("gh_user only runs gh")
    }
}

fn exit(code: i32) -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }
}

#[test]
fn the_gh_user_is_read_once_from_gh_and_an_unauthenticated_gh_names_no_one() {
    assert_eq!(gh_user(&UserGh(Ok((0, "octo-builder\n"))), None).unwrap(), "octo-builder");
    // An Enterprise Managed User's login carries `_`.
    assert_eq!(gh_user(&UserGh(Ok((0, "jdoe_acme\n"))), None).unwrap(), "jdoe_acme");
    // Not logged in, gh missing, or an answer that is no login: an error the
    // listener reports, and only the bot and configured rules apply.
    assert!(gh_user(&UserGh(Ok((1, ""))), None).is_err());
    assert!(gh_user(&UserGh(Err(io::Error::from(io::ErrorKind::NotFound))), None).is_err());
    assert!(gh_user(&UserGh(Ok((0, "two words\n"))), None).is_err());
    assert!(gh_user(&UserGh(Ok((0, "\n"))), None).is_err());
}

#[test]
fn the_gh_user_on_an_enterprise_host_is_asked_of_that_host() {
    struct HostGh;
    impl pr_supervise::Gh for HostGh {
        fn run(&self, args: &[String]) -> io::Result<Output> {
            assert_eq!(args, user_args(Some("ghe.corp.example")));
            Ok(Output { status: exit(0), stdout: b"jdoe_corp\n".to_vec(), stderr: Vec::new() })
        }
        fn spawn(&self, _: &[String]) -> io::Result<std::process::Child> {
            unreachable!("gh_user only runs gh")
        }
    }
    assert_eq!(gh_user(&HostGh, Some("ghe.corp.example")).unwrap(), "jdoe_corp");
}
