//! Tests for the review fixes on #313: a Jira gateway site with no
//! `confluence_site`, Confluence's credential read lazily, `operations`
//! that is not a list, `use` read only for the application, `slice --out`
//! replacing and refusing a flag, the wider scrub, the gateway hint only
//! for Cloud, the host class by enum, and `--fields` outside jql.

use super::api::tests::{ok, settings, Script};
use super::api::{check_with, Client};
use super::http::Response;
use super::{base64, AuthRef, Credential, Ctx, HostClass, Settings, CONFLUENCE_SITE_NEEDED};

fn env(n: &str) -> Option<String> {
    match n {
        "E" => Some("e@example.com".into()),
        "T" => Some("jira-SENTINEL-token".into()),
        "CT" => Some("conf-SENTINEL-token".into()),
        _ => None,
    }
}

fn ctx<'a>(s: &'a Settings, env: &'a dyn Fn(&str) -> Option<String>) -> Ctx<'a> {
    Ctx { settings: Some(s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env }
}

fn check_out(s: &Settings, t: &Script) -> String {
    let mut out = Vec::new();
    check_with(s, &ctx(s, &env), &mut out, t).unwrap();
    String::from_utf8(out).unwrap()
}

fn status(st: u16) -> Response {
    Response { status: st, headers: vec![], body: b"{}".to_vec() }
}

const PERMS: &str = r#"{"permissions":{"CREATE_ISSUES":{"havePermission":false}}}"#;
const MYSELF: &str = r#"{"accountId":"x","displayName":"Someone"}"#;

#[test]
fn a_jira_gateway_site_without_confluence_site_never_sends_confluence_to_the_jira_gateway() {
    let s = settings("https://api.atlassian.com/ex/jira/11111111-2222", true);
    assert!(s.site_is_jira_gateway());
    assert_eq!(s.confluence_base().unwrap_err(), CONFLUENCE_SITE_NEEDED);
    let t = Script::new(vec![Ok(ok(MYSELF)), Ok(ok(PERMS))]);
    let o = check_out(&s, &t);
    assert!(o.contains("jira auth: ok") && o.contains("confluence: not tried: site is Jira's gateway"), "{o}");
    assert!(o.contains("confluence_site to https://api.atlassian.com/ex/confluence/<cloud id>/wiki"), "{o}");
    assert!(t.seen.borrow().iter().all(|r| !r.1.contains("/wiki")), "no request to <gateway>/wiki");
    // A read is refused before anything is sent.
    let t = Script::new(vec![]);
    let sl = |_d: std::time::Duration| {};
    let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
    let e = c.page("98765").unwrap_err();
    assert!(e.to_string().contains("confluence_site"), "{e}");
    assert!(c.cql("type = page", 5).is_err());
    assert!(t.seen.borrow().is_empty(), "nothing sent");
    // With confluence_site set, Confluence goes there.
    let s = Settings { confluence_site: Some("https://api.atlassian.com/ex/confluence/11111111-2222/wiki".into()), ..s };
    assert_eq!(s.confluence_base().unwrap(), "https://api.atlassian.com/ex/confluence/11111111-2222/wiki");
}

#[test]
fn the_jira_401_hint_names_confluence_site_and_its_token() {
    let s = settings("https://hint.example.atlassian.net", true);
    let t = Script::new(vec![Ok(status(401)), Ok(status(401))]);
    let o = check_out(&s, &t);
    let hint = o.lines().find(|l| l.starts_with("hint:") && l.contains("/ex/jira/")).unwrap_or_else(|| panic!("{o}"));
    assert!(hint.contains("set site to https://api.atlassian.com/ex/jira/<cloud id>"), "{hint}");
    assert!(hint.contains("confluence_site to https://api.atlassian.com/ex/confluence/<cloud id>/wiki"), "{hint}");
    assert!(hint.contains("confluence_token_env"), "{hint}");
}

#[test]
fn confluence_credential_is_read_on_the_first_confluence_request_only() {
    let s = Settings {
        confluence_auth: Some(AuthRef::Cloud { email_env: "E".into(), token_env: "CU".into() }),
        ..settings("https://lazy.example.atlassian.net", true)
    };
    let no_cu = |n: &str| if n == "CU" { None } else { env(n) };
    assert!(s.env_present(&no_cu), "Jira's variables are enough for a Jira read or a snapshot");
    let t = Script::new(vec![Ok(ok(r#"{"key":"PROJ-1","fields":{}}"#))]);
    let sl = |_d: std::time::Duration| {};
    let c = Client::new(&s, Credential::resolve(&s.auth, &no_cu).unwrap(), &t, &sl).with_confluence_env(s.confluence_auth(), &no_cu);
    assert!(c.issue("PROJ-1").is_ok());
    let e = c.page("98765").unwrap_err();
    assert!(e.to_string().contains("CU is not set"), "{e}");
    assert_eq!(t.seen.borrow().len(), 1, "the page read sent nothing");
}

#[test]
fn operations_that_is_not_a_list_is_not_known_and_use_counts_only_for_the_application() {
    let s = settings("https://ops.example.atlassian.net", true);
    let t = Script::new(vec![Ok(ok(MYSELF)), Ok(ok(PERMS)), Ok(ok(r#"{"accountId":"x","operations":{"results":[]}}"#))]);
    let o = check_out(&s, &t);
    assert!(o.contains("confluence writes: not known") && !o.contains("confluence writes: reads only"), "{o}");
    let user =
        r#"{"accountId":"x","operations":[{"operation":"use","targetType":"application"},{"operation":"use","targetType":"space"}]}"#;
    let t = Script::new(vec![Ok(ok(MYSELF)), Ok(ok(PERMS)), Ok(ok(user))]);
    let o = check_out(&s, &t);
    assert!(o.contains("counted as writes: use space") && o.contains("not counted as writes): use application"), "{o}");
    assert!(!o.contains("confluence writes: reads only"), "{o}");
}

#[test]
fn slice_out_says_replaced_and_refuses_a_flag_for_a_file() {
    let dir = std::env::temp_dir().join(format!("atlas-track-outfix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("out.txt").display().to_string();
    let first = crate::handles::write_out("h:1", &file, "a\nb\n").unwrap();
    assert!(first.contains("wrote") && !first.contains("replaced"), "{first}");
    let again = crate::handles::write_out("h:1", &file, "c\n").unwrap();
    assert!(again.contains("replaced"), "{again}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "c\n");
    for bad in ["--lines", "-x"] {
        assert!(crate::handles::write_out("h:1", bad, "z").is_err(), "{bad}");
        assert!(!std::path::Path::new(bad).exists());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scrub_matches_unpadded_and_url_safe_base64() {
    // A token whose base64 forms hold + and / and padding.
    let tok = "???>>>x";
    let env = |n: &str| match n {
        "E" => Some("e@example.com".to_string()),
        "T" => Some(tok.to_string()),
        _ => None,
    };
    let c = Credential::resolve(&AuthRef::Cloud { email_env: "E".into(), token_env: "T".into() }, &env).unwrap();
    let std64 = base64(tok.as_bytes());
    assert_eq!(std64, "Pz8/Pj4+eA==", "the vector holds +, / and padding");
    let url64 = std64.replace('+', "-").replace('/', "_");
    let pair64 = base64(format!("e@example.com:{tok}").as_bytes());
    for form in [std64.trim_end_matches('='), url64.as_str(), url64.trim_end_matches('='), pair64.trim_end_matches('=')] {
        let clean = c.scrub(&format!("echo <{form}>"));
        assert_eq!(clean, "echo <[redacted]>", "{form}");
    }
    let p = Credential::resolve(&AuthRef::Pat { pat_env: "T".into() }, &env).unwrap();
    assert_eq!(p.scrub(&format!("x {} y", url64.trim_end_matches('='))), "x [redacted] y");
}

#[test]
fn the_401_gateway_text_is_for_cloud_auth_on_a_cloud_site_only() {
    let sl = |_d: std::time::Duration| {};
    for (site, cloud, want) in [
        ("https://c.example.atlassian.net", true, true),
        ("https://c.example.atlassian.net", false, false),
        ("http://127.0.0.1:9", true, false),
        ("https://api.atlassian.com/ex/jira/abc", true, false),
    ] {
        let s = settings(site, cloud);
        let e = |n: &str| if n == "P" { Some("pat-SENTINEL".to_string()) } else { env(n) };
        let t = Script::new(vec![Ok(status(401))]);
        let c = Client::new(&s, Credential::resolve(&s.auth, &e).unwrap(), &t, &sl);
        let err = c.myself().unwrap_err().to_string();
        assert!(err.contains("HTTP 401 (unauthorized"), "{err}");
        assert_eq!(err.contains("gateway URL"), want, "{site} cloud={cloud}: {err}");
    }
}

#[test]
fn the_host_class_is_an_enum() {
    assert_eq!(HostClass::of("https://x.atlassian.net/jira"), HostClass::Cloud);
    assert_eq!(HostClass::of("https://X.Atlassian.NET"), HostClass::Cloud);
    assert_eq!(HostClass::of("http://127.0.0.1:8080"), HostClass::Loopback);
    assert_eq!(HostClass::of("https://api.atlassian.com/ex/jira/abc"), HostClass::Custom);
    assert!(settings("https://x.atlassian.net", true).is_cloud_site());
    assert!(!settings("https://jira.example.com", false).is_cloud_site());
}

#[test]
fn fields_is_refused_outside_jql() {
    // Loopback on a closed port: a refusal that failed would not reach a real site.
    let s = settings("http://127.0.0.1:9", true);
    for word in ["issue", "page", "cql"] {
        let args: Vec<String> = [word, "X-1", "--fields", "*all"].iter().map(ToString::to_string).collect();
        let e = super::cli(&args, &ctx(&s, &env)).unwrap_err();
        assert!(e.contains("--fields is for jql"), "{word}: {e}");
    }
}
