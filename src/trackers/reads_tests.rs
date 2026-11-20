//! Tests for the project tracking reads fixed after the first live run
//! (issues #304 and #308): a Confluence token of its own, the gateway hint,
//! the auth-failed wording, the silent-empty login, the fail-safe Confluence
//! write list, lossless issue handles, `jql --fields` and `slice --out`.

use super::api::tests::{ok, settings, Script};
use super::api::{check_with, read_with, Client, ReadOpts, DEFAULT_JQL_FIELDS};
use super::http::Response;
use super::{base64, AuthRef, Credential, Ctx, Settings};
use crate::json;

fn env(n: &str) -> Option<String> {
    match n {
        "E" => Some("e@example.com".into()),
        "T" => Some("jira-SENTINEL-token".into()),
        "CT" => Some("conf-SENTINEL-token".into()),
        _ => None,
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/atlassian").join(name)).unwrap()
}

fn unused_ctx<'a>(s: &'a Settings, env: &'a dyn Fn(&str) -> Option<String>) -> Ctx<'a> {
    Ctx { settings: Some(s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env }
}

fn check_out(s: &Settings, t: &Script) -> String {
    let mut out = Vec::new();
    check_with(s, &unused_ctx(s, &env), &mut out, t).unwrap();
    String::from_utf8(out).unwrap()
}

fn answer(st: u16, headers: &[(&str, &str)], body: &str) -> Response {
    Response { status: st, headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: body.as_bytes().to_vec() }
}

fn header_of(cfg: &str) -> String {
    cfg.lines().find(|l| l.contains("Authorization:")).unwrap_or("").to_string()
}

#[test]
fn confluence_requests_carry_their_own_token_and_jira_keeps_its_own() {
    let s = Settings {
        confluence_auth: Some(AuthRef::Cloud { email_env: "E".into(), token_env: "CT".into() }),
        ..settings("https://two.example.atlassian.net", true)
    };
    let perms = r#"{"permissions":{"CREATE_ISSUES":{"havePermission":false}}}"#;
    let t = Script::new(vec![Ok(ok(&fixture("jira-myself.json"))), Ok(ok(perms)), Ok(ok(&fixture("confluence-user.json")))]);
    let o = check_out(&s, &t);
    assert!(o.contains("env T: present\nenv CT: present"), "{o}");
    let seen = t.seen.borrow();
    let jira = base64(b"e@example.com:jira-SENTINEL-token");
    let conf = base64(b"e@example.com:conf-SENTINEL-token");
    for (_, url, _, cfg) in seen.iter() {
        let h = header_of(cfg);
        if url.contains("/wiki/") {
            assert!(h.contains(&conf) && !h.contains(&jira), "Confluence {url}: {h}");
        } else {
            assert!(h.contains(&jira) && !h.contains(&conf), "Jira {url}: {h}");
        }
    }
    assert_eq!(seen.iter().filter(|r| r.1.contains("/wiki/")).count(), 1, "one Confluence request");
    // The config: confluence_token_env alone takes email_env's email; an
    // email of its own needs a token beside it; not with a PAT.
    let cfg = |x: &str| Settings::from_config(Some(&json::parse(x).unwrap()));
    let c = cfg(r#"{"atlassian": {"site": "https://x.atlassian.net", "email_env": "E", "token_env": "T", "confluence_token_env": "CT"}}"#)
        .unwrap()
        .unwrap();
    assert_eq!(c.confluence_auth(), &AuthRef::Cloud { email_env: "E".into(), token_env: "CT".into() });
    assert_eq!(c.env_names(), vec!["E", "T", "CT"]);
    let c = cfg(r#"{"atlassian": {"site": "https://x.atlassian.net", "email_env": "E", "token_env": "T"}}"#).unwrap().unwrap();
    assert_eq!(c.confluence_auth(), &c.auth, "falls back to Jira's");
    assert!(cfg(r#"{"atlassian": {"site": "https://x.atlassian.net", "email_env": "E", "token_env": "T", "confluence_email_env": "CE"}}"#)
        .is_err());
    assert!(cfg(r#"{"atlassian": {"site": "https://x.atlassian.net", "pat_env": "P", "confluence_token_env": "CT"}}"#).is_err());
}

#[test]
fn a_401_on_the_site_url_hints_the_gateway_and_writes_are_unknown() {
    let s = settings("https://gw.example.atlassian.net", true);
    let t = Script::new(vec![Ok(answer(401, &[], "{}")), Ok(answer(401, &[], "{}"))]);
    let o = check_out(&s, &t);
    assert!(o.contains("jira auth: failed") && o.contains("https://api.atlassian.com/ex/jira/<cloud id>"), "{o}");
    assert!(o.contains("https://api.atlassian.com/ex/confluence/<cloud id>/wiki"), "{o}");
    assert!(o.contains("jira writes: unknown (auth failed)") && o.contains("confluence writes: unknown (auth failed)"), "{o}");
    assert!(!o.contains("reads only"), "{o}");
    assert_eq!(t.seen.borrow().len(), 2, "no permission read after a failed login");
    // Not a Cloud host: no gateway hint.
    let s = settings("http://127.0.0.1:9", true);
    let t = Script::new(vec![Ok(answer(401, &[], "{}")), Ok(answer(401, &[], "{}"))]);
    let o = check_out(&s, &t);
    assert!(o.contains("jira writes: unknown (auth failed)") && !o.contains("api.atlassian.com/ex/"), "{o}");
}

#[test]
fn a_200_with_a_failed_login_header_is_an_auth_failure_not_an_empty_result() {
    let s = settings("https://seraph.example.atlassian.net", true);
    let empty = r#"{"issues":[],"isLast":true}"#;
    let t = Script::new(vec![Ok(answer(200, &[("x-seraph-loginreason", "AUTHENTICATED_FAILED")], empty))]);
    let sl = |_d: std::time::Duration| {};
    let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
    let e = c.jql("project = PROJ", 10).unwrap_err();
    assert_eq!(e.status, Some(401), "{e}");
    assert!(e.to_string().contains("AUTHENTICATED_FAILED"), "{e}");
    // A login that worked reads as before.
    let t = Script::new(vec![Ok(answer(200, &[("x-seraph-loginreason", "OK")], empty))]);
    let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
    assert!(c.jql("project = PROJ", 10).is_ok());
    // check: a failed-login whoami is a failed login.
    let t = Script::new(vec![Ok(answer(200, &[("x-seraph-loginreason", "AUTHENTICATED_FAILED")], "{}")), Ok(ok("{}"))]);
    let o = check_out(&s, &t);
    assert!(o.contains("jira auth: failed") && o.contains("jira writes: unknown (auth failed)"), "{o}");
}

#[test]
fn an_unknown_confluence_verb_counts_as_a_possible_write() {
    let s = settings("https://verbs2.example.atlassian.net", true);
    let perms = r#"{"permissions":{"CREATE_ISSUES":{"havePermission":false}}}"#;
    let user =
        r#"{"accountId":"x","operations":[{"operation":"read","targetType":"page"},{"operation":"frobnicate","targetType":"space"}]}"#;
    let t = Script::new(vec![Ok(ok(&fixture("jira-myself.json"))), Ok(ok(perms)), Ok(ok(user))]);
    let o = check_out(&s, &t);
    assert!(!o.contains("confluence writes: reads only"), "{o}");
    assert!(o.contains("confluence writes: possibly; operations atlas does not know, counted as writes: frobnicate space"), "{o}");
}

/// A temp folder for a handle store.
fn store_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("atlas-track-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn an_issue_handle_holds_the_raw_response_as_parseable_json() {
    let s = settings("https://lossless.example.atlassian.net", true);
    let dir = store_dir("lossless");
    let db = dir.join("atlas.db");
    let ctx = Ctx { settings: Some(&s), db: &db, lean: None, store: &dir, env: &env };
    // The fixture, with the account's own email as a field value and an
    // echoed request header, as a proxy might add.
    let mut raw = json::parse(&fixture("jira-issue.json")).unwrap();
    let echoed = format!("Basic {}", base64(b"e@example.com:jira-SENTINEL-token"));
    if let Some(m) = raw.as_object_mut() {
        m.insert("echo".into(), json::Value::String(echoed.clone()));
    }
    if let Some(f) = raw.as_object_mut().and_then(|m| m.get_mut("fields")).and_then(json::Value::as_object_mut) {
        f.insert("ownerEmail".into(), json::Value::String("e@example.com".into()));
    }
    let body = json::to_string(&raw);
    let t = Script::new(vec![Ok(ok(&body))]);
    let mut out = Vec::new();
    read_with(&s, &ctx, &mut out, "issue", "PROJ-123", &ReadOpts::default(), &t, crate::handles::HintForm::Recall).unwrap();
    let digest = String::from_utf8(out).unwrap();
    let id = digest.split_whitespace().next().unwrap().to_string();
    assert!(digest.contains("item=json") && digest.contains("item=description-md"), "{digest}");
    let h = crate::handles::Handles::open(&db, crate::handles::HandleCfg::from_lean(None)).unwrap();
    let got = h.extract(&id, &crate::handles::Sel::Item("json".into())).unwrap();
    // The JSON item parses, and equals the raw response with only the auth
    // header's value redacted, byte for byte once pretty-printed.
    let parsed = json::parse(&got).expect("the json item parses");
    let mut want = raw.clone();
    if let Some(m) = want.as_object_mut() {
        m.insert("echo".into(), json::Value::String("Basic [redacted]".into()));
    }
    assert_eq!(got.trim_end(), json::to_string_pretty(&want));
    assert_eq!(parsed, want);
    assert_eq!(parsed.pointer("/fields/ownerEmail").and_then(json::Value::as_str), Some("e@example.com"));
    let whole = h.extract(&id, &crate::handles::Sel::Whole).unwrap();
    for f in ["jira-SENTINEL-token", echoed.as_str(), base64(b"jira-SENTINEL-token").as_str()] {
        assert!(!whole.contains(f), "{f} in the handle");
    }
    let md = h.extract(&id, &crate::handles::Sel::Item("description-md".into())).unwrap();
    assert!(md.contains("## Steps to reproduce") && !md.contains("--- markdown") && json::parse(&md).is_err(), "{md}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn jql_fields_pass_through_and_a_bad_list_is_refused() {
    let s = settings("https://fields.example.atlassian.net", true);
    let sl = |_d: std::time::Duration| {};
    let page = r#"{"issues":[],"isLast":true}"#;
    let t = Script::new(vec![Ok(ok(page)), Ok(ok(page)), Ok(ok(page))]);
    let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
    c.jql_fields("project = PROJ", 5, Some("*all")).unwrap();
    c.jql_fields("project = PROJ", 5, Some("description,labels,customfield_10020")).unwrap();
    c.jql("project = PROJ", 5).unwrap();
    assert!(c.jql_fields("project = PROJ", 5, Some("a b")).is_err());
    let seen = t.seen.borrow();
    assert_eq!(seen.len(), 3, "the bad list sent nothing");
    assert!(seen[0].1.contains("&fields=%2Aall"), "{}", seen[0].1);
    assert!(seen[1].1.contains("&fields=description%2Clabels%2Ccustomfield_10020"), "{}", seen[1].1);
    assert!(seen[2].1.contains(&format!("&fields={DEFAULT_JQL_FIELDS}")), "{}", seen[2].1);
}

#[test]
fn slice_out_writes_the_whole_handle_past_the_cap() {
    let dir = store_dir("sliceout");
    let db = dir.join("atlas.db");
    let mut h = crate::handles::Handles::open(&db, crate::handles::HandleCfg::from_lean(None)).unwrap();
    let text: String = (1..=2000).map(|i| format!("line {i} of a long stored output\n")).collect();
    assert!(text.len() > 8 * 1024 * 4);
    let meta = crate::handles::Meta {
        kind: "run".into(),
        label: "long".into(),
        cmd: "x".into(),
        cwd: String::new(),
        session: String::new(),
        exit_code: None,
        ms: None,
    };
    let row = h.put(text.as_bytes(), &meta).unwrap();
    drop(h);
    let file = dir.join("out.txt");
    let args = vec![row.id.clone(), "--out".into(), file.display().to_string()];
    crate::handles::cli(args, &db, None).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "the whole text, unnumbered");
    let args = vec![row.id.clone(), "--lines".into(), "10-12".into(), "--out".into(), file.display().to_string()];
    crate::handles::cli(args, &db, None).unwrap();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "line 10 of a long stored output\nline 11 of a long stored output\nline 12 of a long stored output\n"
    );
    let args = vec![row.id.clone(), "--grep".into(), "x".into(), "--out".into(), file.display().to_string()];
    assert!(crate::handles::cli(args, &db, None).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A read prints its hint in the form it is given: the `atlas slice`
/// command for a terminal, `recall` otherwise (issue 307).
#[test]
fn a_read_prints_its_hint_in_the_given_form() {
    use crate::handles::HintForm;
    let s = settings("https://hintform.example.atlassian.net", true);
    let dir = store_dir("hintform");
    let db = dir.join("atlas.db");
    let ctx = Ctx { settings: Some(&s), db: &db, lean: None, store: &dir, env: &env };
    let read = |form: HintForm| {
        let t = Script::new(vec![Ok(ok(&fixture("jira-search-1.json")))]);
        let mut out = Vec::new();
        read_with(&s, &ctx, &mut out, "jql", "project = PROJ", &ReadOpts::default(), &t, form).unwrap();
        String::from_utf8(out).unwrap()
    };
    let cli = read(HintForm::Cli);
    let last = cli.trim_end().lines().last().unwrap();
    assert!(last.starts_with("atlas slice h:") && last.contains(" --lines A-B | --grep RE"), "{cli}");
    assert!(!cli.contains("recall h:"), "{cli}");
    let recall = read(HintForm::Recall);
    let last = recall.trim_end().lines().last().unwrap();
    assert!(last.starts_with("recall h:") && last.contains(" lines=A-B | grep=RE"), "{recall}");
    let _ = std::fs::remove_dir_all(&dir);
}
