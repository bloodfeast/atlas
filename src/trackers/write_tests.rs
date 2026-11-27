//! Tests for project tracking writes (journal:ATLAS-TRACKERS:0013): the
//! Script transport answers from fixtures, so no site is ever called. Every
//! Then of the TRACK-WRITES scenarios has its test here (the socket mock
//! and the sentinel scan are in tests/track_writes.rs).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::api::tests::{ok, Script};
use super::http::{Failure, Response};
use super::write::{run, section_edit, Env, Reply, Verb, WriteArgs, PRE_READS, WRITE_ENDPOINTS};
use super::{base64, Ctx, Settings};
use crate::json;

const READ: &str = "READ-SENTINEL-token";
const WRITE: &str = "WRITE-SENTINEL-token";
const WCONF: &str = "WRITE-CONF-SENTINEL";
const SITE: &str = "https://w.example.atlassian.net";

fn env(n: &str) -> Option<String> {
    match n {
        "E" => Some("e@example.com".into()),
        "T" => Some(READ.into()),
        "WT" => Some(WRITE.into()),
        "WCT" => Some(WCONF.into()),
        "P" => Some("READ-PAT-SENTINEL".into()),
        "WP" => Some("WRITE-PAT-SENTINEL".into()),
        _ => None,
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(name)).unwrap()
}

fn resp(st: u16, headers: &[(&str, &str)], body: &str) -> Response {
    Response { status: st, headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: body.as_bytes().to_vec() }
}

fn r(st: u16, body: &str) -> Result<Response, Failure> {
    Ok(resp(st, &[], body))
}

const CLOUD_WRITES: &str =
    r#""writes": {"jira": {"PROJ": ["comment","transition","assignee","labels","sprint"]}, "confluence": {"4456": ["page_update"]}}"#;

fn cloud_cfg(extra: &str) -> Settings {
    let text = format!(
        r#"{{"atlassian": {{"site": "{SITE}", "email_env": "E", "token_env": "T", "jira_write_token_env": "WT", "confluence_write_token_env": "WCT",
        "min_interval_ms": 0, "max_retries": 2, "max_wait_s": 3, "backoff_ms": 10, {extra}}}}}"#
    );
    Settings::from_config(Some(&json::parse(&text).unwrap())).unwrap().unwrap()
}

fn cloud() -> Settings {
    cloud_cfg(CLOUD_WRITES)
}

fn dc() -> Settings {
    let text = r#"{"atlassian": {"site": "https://jira.example.com", "confluence_site": "https://wiki.example.com", "pat_env": "P", "pat_write_env": "WP",
        "min_interval_ms": 0, "max_retries": 2, "max_wait_s": 3, "backoff_ms": 10,
        "writes": {"jira": {"PROJ": ["comment","transition","labels"]}, "confluence": {"ENG": ["page_update"]}}}}"#;
    Settings::from_config(Some(&json::parse(text).unwrap())).unwrap().unwrap()
}

fn tdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("atlas-trkw-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn a(verb: Verb, target: &str) -> WriteArgs {
    WriteArgs { verb, target: target.into(), ..WriteArgs::default() }
}

fn comment(body: &str) -> WriteArgs {
    WriteArgs { body: Some(body.into()), ..a(Verb::Comment, "PROJ-12") }
}

/// Runs a write over a scripted transport in `dir` (its stores), at `now_s`.
fn exec_at(s: &Settings, w: &WriteArgs, answers: Vec<Result<Response, Failure>>, dir: &Path, now_s: u64) -> (Reply, Script, Vec<Duration>) {
    let t = Script::new(answers);
    let waits = RefCell::new(Vec::new());
    let sleep = |d: Duration| waits.borrow_mut().push(d);
    let db = dir.join("atlas.db");
    let store = dir.join("memory.sqlite");
    let ctx = Ctx { settings: Some(s), db: &db, lean: None, store: &store, env: &env };
    let reply = run(&ctx, s, &Env { t: &t, sleep: &sleep, now_s }, w);
    let waited = waits.borrow().clone();
    (reply, t, waited)
}

fn exec(s: &Settings, w: &WriteArgs, answers: Vec<Result<Response, Failure>>, tag: &str) -> (Reply, Script, PathBuf) {
    let dir = tdir(tag);
    let (reply, t, _) = exec_at(s, w, answers, &dir, 1_000_000);
    (reply, t, dir)
}

/// The requests that were not GETs: (method, url).
fn writes(t: &Script) -> Vec<(String, String)> {
    t.seen.borrow().iter().filter(|x| x.0 != "GET").map(|x| (x.0.clone(), x.1.clone())).collect()
}

fn header_of(cfg: &str) -> String {
    cfg.lines().find(|l| l.contains("Authorization:")).unwrap_or("").to_string()
}

/// The JSON body a recorded curl config sends.
fn body_of(cfg: &str) -> json::Value {
    let line = cfg.lines().find_map(|l| l.strip_prefix("data-binary = ")).expect("a body");
    let inner = &line[1..line.len() - 1];
    let mut out = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        out.push(if c == '\\' { it.next().unwrap_or('\\') } else { c });
    }
    json::parse(&out).unwrap()
}

fn last_body(t: &Script) -> json::Value {
    let seen = t.seen.borrow();
    body_of(&seen.iter().rev().find(|x| x.0 != "GET").expect("a write was sent").3)
}

fn journal(dir: &Path, lane: &str) -> Vec<crate::memory::Head> {
    crate::memory::MemoryStore::open(&dir.join("memory.sqlite")).unwrap().lane(lane).unwrap()
}

fn state(status: &str) -> String {
    format!(r#"{{"key":"PROJ-12","fields":{{"project":{{"key":"PROJ"}},"status":{{"name":"{status}","id":"3"}},"labels":[],"assignee":null}}}}"#)
}

/// The issue read every Jira write makes first (`fields=project,status`).
fn issue() -> Result<Response, Failure> {
    r(200, &state("To Do"))
}

const BODY: &str = "<h1>Intro</h1><p>hello</p><h2>Rollout</h2><p>old plan</p><h3>Detail</h3><p>d</p><h2>Other</h2><p>o</p>";

fn page(version: u64, body: &str) -> String {
    json::to_string(&json!({"id": "123", "status": "current", "title": "Release", "spaceId": "4456",
        "version": {"number": version}, "body": {"storage": {"representation": "storage", "value": body}}}))
}

fn page_update(body: &str) -> WriteArgs {
    WriteArgs { body: Some(body.into()), apply: true, ..a(Verb::PageUpdate, "123") }
}

fn all_verbs() -> Vec<WriteArgs> {
    vec![
        comment("hi"),
        WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") },
        WriteArgs { labels_add: vec!["x".into()], ..a(Verb::Edit, "PROJ-12") },
        page_update("<p>x</p>"),
    ]
}

#[test]
fn a_read_only_config_refuses_every_write() {
    let read_only = Settings::from_config(Some(
        &json::parse(&format!(r#"{{"atlassian": {{"site": "{SITE}", "email_env": "E", "token_env": "T", {CLOUD_WRITES}}}}}"#)).unwrap(),
    ))
    .unwrap()
    .unwrap();
    for apply in [false, true] {
        for w in all_verbs() {
            let w = WriteArgs { apply, ..w };
            let (rep, t, _) = exec(&read_only, &w, vec![], "ro");
            let key = if w.verb == Verb::PageUpdate { "confluence_write_token_env" } else { "jira_write_token_env" };
            assert_eq!(rep.code, 2, "{rep:?}");
            assert_eq!(
                rep.text,
                format!("atlas track {}: no {key} in project_tracking.atlassian: this setup is read-only; nothing sent", w.verb.word())
            );
            assert!(t.seen.borrow().is_empty(), "no request of any kind: {:?}", t.seen.borrow());
        }
    }
    let (rep, _, _) = exec(&dc_without_write(), &comment("x"), vec![], "ro-dc");
    assert!(rep.text.contains("no pat_write_env in project_tracking.atlassian"), "{}", rep.text);
}

fn dc_without_write() -> Settings {
    Settings::from_config(Some(&json::parse(r#"{"atlassian": {"site": "https://jira.example.com", "pat_env": "P"}}"#).unwrap()))
        .unwrap()
        .unwrap()
}

#[test]
fn the_write_configuration_is_checked_at_load() {
    let cfg = |extra: &str| {
        Settings::from_config(Some(
            &json::parse(&format!(r#"{{"atlassian": {{"site": "{SITE}", "email_env": "E", "token_env": "T", {extra}}}}}"#)).unwrap(),
        ))
    };
    // A write variable may name a read variable: a legacy unscoped token
    // does both (journal:ATLAS-TRACKERS:0054, TW-Q5 b).
    let same = cfg(r#""jira_write_token_env": "T""#).unwrap().unwrap();
    assert_eq!(same.write_auth(false).unwrap(), same.auth);
    let e = cfg(r#""writes": {"jira": {"PROJ": ["comment", "delete"]}}"#).unwrap_err();
    assert!(e.contains("unknown verb \"delete\"") && e.contains("comment, transition, assignee, labels, sprint"), "{e}");
    let e = cfg(r#""writes": {"confluence": {"ENG": ["comment"]}}"#).unwrap_err();
    assert!(e.contains("page_update"), "{e}");
    assert!(cfg(r#""pat_write_env": "WP""#).unwrap_err().contains("Data Center"));
    assert!(cfg(r#""jira_write_token_env": "not a name""#).unwrap_err().contains("not hold a credential"));
    assert!(cfg(r#""writes": {"jira": {"PROJ": ["comment"]}, "nope": 1}"#).is_err());
    let pat = r#"{"atlassian": {"site": "https://j.example.com", "pat_env": "P", "jira_write_token_env": "WT"}}"#;
    assert!(Settings::from_config(Some(&json::parse(pat).unwrap())).unwrap_err().contains("for Cloud"));
    // Confluence's write email follows confluence_email_env, else email_env.
    let c = cfg(r#""confluence_email_env": "CE", "confluence_token_env": "CT", "confluence_write_token_env": "WCT""#).unwrap().unwrap();
    assert_eq!(c.write_auth(true).unwrap(), super::AuthRef::Cloud { email_env: "CE".into(), token_env: "WCT".into() });
    assert_eq!(c.write_auth(false).unwrap_err(), "jira_write_token_env");
}

#[test]
fn a_read_token_is_never_used_for_a_write() {
    let s = cloud();
    let reads = [base64(format!("e@example.com:{READ}").as_bytes()), READ.to_string()];
    let jira_w = base64(format!("e@example.com:{WRITE}").as_bytes());
    let conf_w = base64(format!("e@example.com:{WCONF}").as_bytes());
    let to = WriteArgs { to: Some("Done".into()), apply: true, ..a(Verb::Transition, "PROJ-12") };
    let (rep, t, _) = exec(
        &s,
        &to,
        vec![r(200, &state("In Progress")), r(200, &fixture("atlassian/writes/jira-transitions.json")), r(204, "")],
        "tok-jira",
    );
    assert_eq!(rep.code, 0, "{rep:?}");
    for (m, url, _, cfg) in t.seen.borrow().iter() {
        let h = header_of(cfg);
        if m == "GET" {
            assert!(h.contains(&reads[0]) && !h.contains(&jira_w), "pre-read {url} carries the read header only: {h}");
        } else {
            assert!(h.contains(&jira_w), "{url}: {h}");
            assert!(reads.iter().all(|x| !cfg.contains(x.as_str())), "no form of the read token in a write: {cfg}");
        }
    }
    let up = page_update("<p>x</p>");
    let (rep, t, _) = exec(&s, &up, vec![r(200, &page(6, BODY)), r(200, &page(7, "<p>x</p>"))], "tok-conf");
    assert_eq!(rep.code, 0, "{rep:?}");
    for (m, url, _, cfg) in t.seen.borrow().iter() {
        let h = header_of(cfg);
        if m == "GET" {
            assert!(h.contains(&reads[0]) && !h.contains(&conf_w), "{url}: {h}");
        } else {
            assert!(h.contains(&conf_w) && !h.contains(&jira_w), "Confluence writes use their own token: {h}");
            assert!(!cfg.contains(&reads[0]) && !cfg.contains(READ) && !cfg.contains(WRITE), "{cfg}");
        }
    }
    // DC: the PAT.
    let (rep, t, _) = exec(&dc(), &WriteArgs { apply: true, ..comment("x") }, vec![issue(), r(201, "{\"id\":\"9\"}")], "tok-dc");
    assert_eq!(rep.code, 0, "{rep:?}");
    let cfg = t.seen.borrow()[1].3.clone();
    assert!(cfg.contains("Authorization: Bearer WRITE-PAT-SENTINEL") && !cfg.contains("READ-PAT-SENTINEL"), "{cfg}");
}

#[test]
fn dry_run_sends_nothing_and_redacts_the_header() {
    let s = cloud();
    let long = "word ".repeat(400);
    let (rep, t, dir) = exec(&s, &comment(&long), vec![issue()], "dry");
    assert_eq!(rep.code, 0, "{rep:?}");
    assert!(writes(&t).is_empty());
    assert_eq!(t.seen.borrow().len(), 1, "a comment reads only the issue's project");
    let o = &rep.text;
    assert!(o.contains("POST https://w.example.atlassian.net/rest/api/3/issue/PROJ-12/comment"), "{o}");
    assert!(o.contains("Authorization: [redacted]") && o.contains("send: add --apply"), "{o}");
    assert!(o.contains("WT: set"), "the write variable is named, never its value: {o}");
    assert!(o.contains(" B, h:"), "the byte count and a handle: {o}");
    assert!(o.len() <= 1200, "{} bytes", o.len());
    for f in [READ, WRITE, &base64(format!("e@example.com:{WRITE}").as_bytes())] {
        assert!(!o.contains(f));
    }
    assert!(journal(&dir, "PROJECT-TRACKING").is_empty(), "a dry-run writes no journal entry");
    // The handle holds the whole body.
    let id = o.split("h:").nth(1).unwrap().split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap();
    let h = crate::handles::Handles::open(&dir.join("atlas.db"), crate::handles::HandleCfg::from_lean(None)).unwrap();
    let all = h.list(5).unwrap();
    assert!(all.iter().any(|x| x.id.ends_with(id) && x.bytes > 2000), "{id}: {all:?}");
    // A page dry-run names the versions and the section's sizes.
    let up = WriteArgs { section: Some("Rollout".into()), apply: false, ..page_update("<p>new</p>") };
    let (rep, t, _) = exec(&s, &up, vec![r(200, &page(6, BODY))], "dry-page");
    assert!(writes(&t).is_empty() && t.seen.borrow().len() == 1);
    assert!(
        rep.text.contains("version 6 -> 7") && rep.text.contains("section Rollout") && rep.text.contains("38 B -> 10 B"),
        "{}",
        rep.text
    );
}

#[test]
fn a_verb_outside_the_allow_list_is_refused() {
    let s = cloud_cfg(r#""writes": {"jira": {"PROJ": ["transition"]}}"#);
    for apply in [false, true] {
        let (rep, t, dir) = exec(&s, &WriteArgs { apply, ..comment("x") }, vec![], "allow");
        assert_eq!(rep.code, 2);
        assert_eq!(
            rep.text,
            "atlas track comment PROJ-12: refused: comment is not allowed for Jira project PROJ (project_tracking.atlassian.writes.jira.PROJ allows: transition); nothing sent"
        );
        assert!(t.seen.borrow().is_empty() && journal(&dir, "PROJECT-TRACKING").is_empty());
    }
    // A project not listed, and no writes section at all.
    let other = WriteArgs { target: "OTHER-1".into(), ..comment("x") };
    let (rep, _, _) = exec(&s, &other, vec![], "allow2");
    assert!(rep.code == 2 && rep.text.contains("Jira project OTHER") && rep.text.contains("has no OTHER"), "{}", rep.text);
    let none = cloud_cfg(r#""timeout_s": 5"#);
    let (rep, t, _) = exec(&none, &comment("x"), vec![], "allow3");
    assert!(rep.code == 2 && rep.text.contains("nothing sent") && t.seen.borrow().is_empty(), "{}", rep.text);
    // Confluence: the space (the page GET names it) must be listed.
    let spaces = cloud_cfg(r#""writes": {"confluence": {"9999": ["page_update"]}}"#);
    let (rep, t, _) = exec(
        &spaces,
        &page_update("<p>x</p>"),
        vec![r(200, &page(6, BODY)), r(200, &fixture("atlassian/writes/confluence-space.json"))],
        "allow4",
    );
    assert!(rep.code == 2 && rep.text.contains("Confluence space 4456 (ENG)") && writes(&t).is_empty(), "{}", rep.text);
    // A space key resolves through the space read.
    let keyed = cloud_cfg(r#""writes": {"confluence": {"ENG": ["page_update"]}}"#);
    let (rep, t, _) = exec(
        &keyed,
        &page_update("<p>x</p>"),
        vec![r(200, &page(6, BODY)), r(200, &fixture("atlassian/writes/confluence-space.json")), r(200, &page(7, "<p>x</p>"))],
        "allow5",
    );
    assert_eq!(rep.code, 0, "{rep:?}");
    assert_eq!(writes(&t).len(), 1);
}

#[test]
fn an_edit_with_one_disallowed_field_sends_nothing() {
    let s = cloud_cfg(r#""writes": {"jira": {"PROJ": ["labels"]}}"#);
    let w = WriteArgs { labels_add: vec!["a".into()], assignee: Some("me".into()), apply: true, ..a(Verb::Edit, "PROJ-12") };
    let (rep, t, _) = exec(&s, &w, vec![], "edit-mixed");
    assert_eq!(rep.code, 2);
    assert!(rep.text.contains("assignee is not allowed for Jira project PROJ") && rep.text.contains("allows: labels"), "{}", rep.text);
    assert!(t.seen.borrow().is_empty(), "not even a pre-read");
    let none = WriteArgs { apply: true, ..a(Verb::Edit, "PROJ-12") };
    let (rep, t, _) = exec(&cloud(), &none, vec![], "edit-none");
    assert!(rep.code == 2 && rep.text.contains("nothing to change") && t.seen.borrow().is_empty(), "{}", rep.text);
}

#[test]
fn comments_are_plain_text_in_adf_on_cloud_and_a_string_on_dc() {
    let (rep, t, _) = exec(
        &cloud(),
        &WriteArgs { apply: true, ..comment("first line\nsecond line\n\nnew paragraph") },
        vec![issue(), r(201, &fixture("atlassian/writes/jira-comment-created.json"))],
        "adf",
    );
    assert_eq!(rep.code, 0, "{rep:?}");
    let b = last_body(&t);
    assert_eq!(b.pointer("/body/type").and_then(|v| v.as_str()), Some("doc"));
    assert_eq!(b.pointer("/body/content/0/content/0/text").and_then(|v| v.as_str()), Some("first line"));
    assert_eq!(b.pointer("/body/content/0/content/1/type").and_then(|v| v.as_str()), Some("hardBreak"));
    assert_eq!(b.pointer("/body/content/0/content/2/text").and_then(|v| v.as_str()), Some("second line"));
    assert_eq!(b.pointer("/body/content/1/content/0/text").and_then(|v| v.as_str()), Some("new paragraph"));
    assert_eq!(writes(&t)[0].1, "https://w.example.atlassian.net/rest/api/3/issue/PROJ-12/comment");
    let (_, t, _) = exec(&dc(), &WriteArgs { apply: true, ..comment("**not converted**") }, vec![issue(), r(201, "{\"id\":\"7\"}")], "dc-comment");
    assert_eq!(last_body(&t).get("body").and_then(|v| v.as_str()), Some("**not converted**"));
    assert_eq!(writes(&t)[0].1, "https://jira.example.com/rest/api/2/issue/PROJ-12/comment");
}

#[test]
fn edits_use_the_update_form_and_resolve_people_and_the_sprint_field() {
    let s = cloud();
    // Labels: add and remove, never the replacing fields.labels.
    let w =
        WriteArgs { labels_add: vec!["a".into(), "b".into()], labels_remove: vec!["c".into()], apply: true, ..a(Verb::Edit, "PROJ-12") };
    let (rep, t, _) = exec(&s, &w, vec![issue(), r(204, "")], "edit-labels");
    assert_eq!(rep.code, 0, "{rep:?}");
    let b = last_body(&t);
    assert_eq!(json::to_string(b.pointer("/update/labels").unwrap()), r#"[{"add":"a"},{"add":"b"},{"remove":"c"}]"#);
    assert!(b.pointer("/fields/labels").is_none());
    assert_eq!(writes(&t)[0], ("PUT".to_string(), "https://w.example.atlassian.net/rest/api/3/issue/PROJ-12".to_string()));
    // me, and an account id taken as is.
    let w = WriteArgs { assignee: Some("me".into()), apply: true, ..a(Verb::Edit, "PROJ-12") };
    let (_, t, _) = exec(&s, &w, vec![issue(), r(200, &fixture("atlassian/jira-myself.json")), r(204, "")], "edit-me");
    assert_eq!(last_body(&t).pointer("/fields/assignee/accountId").and_then(|v| v.as_str()), Some("5b10ac8d82e05b22cc7d4ef5"));
    assert_eq!(t.seen.borrow()[1].1, "https://w.example.atlassian.net/rest/api/3/myself");
    let w = WriteArgs { assignee: Some("none".into()), apply: true, ..a(Verb::Edit, "PROJ-12") };
    let (_, t, _) = exec(&s, &w, vec![issue(), r(204, "")], "edit-none");
    assert!(matches!(last_body(&t).pointer("/fields/assignee/accountId"), Some(json::Value::Null)));
    let id = WriteArgs { assignee: Some("5b10a2844c20165700ede21g".into()), apply: true, ..a(Verb::Edit, "PROJ-12") };
    let (_, t, _) = exec(&s, &id, vec![issue(), r(204, "")], "edit-id");
    assert_eq!(t.seen.borrow().len(), 2, "an account id needs no lookup: the issue read and the PUT");
    // A name: exactly one account, or refused naming the count.
    let q = WriteArgs { assignee: Some("avery".into()), apply: true, ..a(Verb::Edit, "PROJ-12") };
    let (rep, t, _) = exec(&s, &q, vec![issue(), r(200, &fixture("atlassian/writes/jira-users-two.json"))], "edit-two");
    assert!(rep.code == 2 && rep.text.contains("2 accounts match") && writes(&t).is_empty(), "{}", rep.text);
    assert!(t.seen.borrow()[1].1.ends_with("/rest/api/3/user/assignable/search?issueKey=PROJ-12&query=avery"));
    let (rep, _, _) = exec(&s, &q, vec![issue(), r(200, "[]")], "edit-zero");
    assert!(rep.code == 2 && rep.text.contains("0 accounts match"), "{}", rep.text);
    let (_, t, _) = exec(&s, &q, vec![issue(), r(200, "[{\"accountId\":\"abc123\"}]"), r(204, "")], "edit-one");
    assert_eq!(last_body(&t).pointer("/fields/assignee/accountId").and_then(|v| v.as_str()), Some("abc123"));
    // The sprint: the field id comes from the field list, the value is the numeric id.
    let sp = WriteArgs { sprint: Some("42".into()), apply: true, ..a(Verb::Edit, "PROJ-12") };
    let (rep, t, _) = exec(&s, &sp, vec![issue(), r(200, &fixture("atlassian/writes/jira-fields.json")), r(204, "")], "edit-sprint");
    assert_eq!(rep.code, 0, "{rep:?}");
    assert!(matches!(last_body(&t).pointer("/fields/customfield_10020").and_then(|v| v.as_u64()), Some(42)));
    let off = WriteArgs { sprint: Some("none".into()), ..sp.clone() };
    let (_, t, _) = exec(&s, &off, vec![issue(), r(200, &fixture("atlassian/writes/jira-fields.json")), r(204, "")], "edit-sprint-none");
    assert!(matches!(last_body(&t).pointer("/fields/customfield_10020"), Some(json::Value::Null)));
    let bad = WriteArgs { sprint: Some("Sprint 4".into()), ..sp };
    let (rep, t, _) = exec(&s, &bad, vec![], "edit-sprint-name");
    assert!(rep.code == 2 && rep.text.contains("numeric sprint id") && t.seen.borrow().is_empty(), "{}", rep.text);
    let (rep, _, _) = exec(&s, &WriteArgs { sprint: Some("42".into()), ..bad }, vec![issue(), r(200, "[]")], "edit-nofield");
    assert!(rep.code == 2 && rep.text.contains("no Sprint field"), "{}", rep.text);
    // The combined edit is one PUT.
    let all = WriteArgs {
        labels_add: vec!["a".into()],
        sprint: Some("7".into()),
        assignee: Some("none".into()),
        apply: true,
        ..a(Verb::Edit, "PROJ-12")
    };
    let (rep, t, _) = exec(&s, &all, vec![issue(), r(200, &fixture("atlassian/writes/jira-fields.json")), r(204, "")], "edit-all");
    assert_eq!(writes(&t).len(), 1);
    assert!(rep.text.starts_with("PROJ-12 labels +a, assignee none, sprint 7 journal:"), "{}", rep.text);
}

#[test]
fn a_transition_to_the_current_status_sends_nothing() {
    let s = cloud();
    let to = |n: &str| WriteArgs { to: Some(n.into()), apply: true, ..a(Verb::Transition, "PROJ-12") };
    let (rep, t, dir) = exec(&s, &to("done"), vec![r(200, &state("Done"))], "tr-noop");
    assert_eq!((rep.code, rep.text.as_str()), (0, "PROJ-12: already Done; nothing sent"));
    assert!(writes(&t).is_empty() && journal(&dir, "PROJECT-TRACKING").is_empty());
    // A transition name, a target status name and an id all select one.
    for name in ["Finish", "done", "31"] {
        let (rep, t, _) = exec(
            &s,
            &to(name),
            vec![r(200, &state("In Progress")), r(200, &fixture("atlassian/writes/jira-transitions.json")), r(204, "")],
            "tr-pick",
        );
        assert!(rep.code == 0 && rep.text.starts_with("PROJ-12 In Progress -> Done journal:"), "{name}: {}", rep.text);
        assert_eq!(json::to_string(&last_body(&t)), r#"{"transition":{"id":"31"}}"#);
    }
    // Not reachable, and nothing visible at all.
    let (rep, t, _) = exec(
        &s,
        &to("Blocked"),
        vec![r(200, &state("In Progress")), r(200, &fixture("atlassian/writes/jira-transitions.json"))],
        "tr-none",
    );
    assert!(
        rep.code == 2 && rep.text.contains("no transition to \"Blocked\"") && rep.text.contains("To Do, Done") && writes(&t).is_empty(),
        "{}",
        rep.text
    );
    let (rep, _, _) = exec(&s, &to("Done"), vec![r(200, &state("In Progress")), r(200, "{\"transitions\":[]}")], "tr-empty");
    assert!(rep.text.contains("no transitions visible to this token's user"), "{}", rep.text);
}

#[test]
fn a_duplicate_comment_needs_again() {
    let s = cloud();
    let dir = tdir("dup");
    let w = WriteArgs { apply: true, ..comment("Deployed.") };
    let created = || vec![issue(), r(201, &fixture("atlassian/writes/jira-comment-created.json"))];
    let (first, _, _) = exec_at(&s, &w, created(), &dir, 1_000);
    assert_eq!(first.code, 0, "{first:?}");
    let (again, t, _) = exec_at(&s, &w, created(), &dir, 1_300);
    assert_eq!(again.code, 2);
    assert!(
        again.text.contains("the same comment was added")
            && again.text.contains("--again")
            && again.text.contains("journal:PROJECT-TRACKING:0001"),
        "{}",
        again.text
    );
    assert!(writes(&t).is_empty() && t.seen.borrow().len() == 1, "only the issue read");
    let (done, _, _) = exec_at(&s, &WriteArgs { again: true, ..w.clone() }, created(), &dir, 1_300);
    assert_eq!(done.code, 0, "{done:?}");
    let (other, _, _) = exec_at(&s, &WriteArgs { body: Some("Different.".into()), ..w.clone() }, created(), &dir, 1_301);
    assert_eq!(other.code, 0, "another text is not a duplicate");
    let (later, _, _) = exec_at(&s, &WriteArgs { body: Some("Deployed.".into()), ..w.clone() }, created(), &dir, 1_000 + 601 + 400);
    assert_eq!(later.code, 0, "after ten minutes it is not a duplicate: {later:?}");
    assert_eq!(journal(&dir, "PROJECT-TRACKING").len(), 4);
    // A dry-run of the duplicate is refused too (the newest copy counts).
    let (dry, _, _) = exec_at(&s, &WriteArgs { apply: false, ..w }, vec![issue()], &dir, 1_000 + 601 + 401);
    assert_eq!(dry.code, 2, "{dry:?}");
}

fn created() -> Result<Response, Failure> {
    r(201, &fixture("atlassian/writes/jira-comment-created.json"))
}

fn limited() -> Result<Response, Failure> {
    Ok(resp(429, &[("retry-after", "1")], "{}"))
}

fn transitions() -> Result<Response, Failure> {
    r(200, &fixture("atlassian/writes/jira-transitions.json"))
}

fn applied(w: WriteArgs) -> WriteArgs {
    WriteArgs { apply: true, ..w }
}

#[test]
fn a_429_is_retried_for_every_write() {
    let s = cloud();
    let to = WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") };
    let labels = WriteArgs { labels_add: vec!["x".into()], ..a(Verb::Edit, "PROJ-12") };
    let cases = vec![
        (comment("hi"), vec![issue(), limited(), created()]),
        (to, vec![r(200, &state("In Progress")), transitions(), limited(), r(204, "")]),
        (labels, vec![issue(), limited(), r(204, "")]),
        (page_update("<p>x</p>"), vec![r(200, &page(6, BODY)), limited(), r(200, &page(7, "<p>x</p>"))]),
    ];
    for (w, answers) in cases {
        let (rep, t, waits) = exec_at(&s, &applied(w.clone()), answers, &tdir("429"), 1_000_000);
        assert_eq!(rep.code, 0, "{:?}: {rep:?}", w.verb);
        assert_eq!(writes(&t).len(), 2, "{:?}: one retry", w.verb);
        // Site pacing is shared by every test on this origin (some space it), so only the long waits count here.
        let long: Vec<Duration> = waits.into_iter().filter(|d| *d >= Duration::from_millis(500)).collect();
        assert_eq!(long, vec![Duration::from_secs(1)], "{:?}: Retry-After is honoured", w.verb);
    }
    // Past max_retries it gives up with the status named, and journals nothing.
    let (rep, t, dir) = exec(&s, &applied(comment("hi")), vec![issue(), limited(), limited(), limited(), created()], "429-out");
    assert!(rep.code == 1 && rep.text.contains("HTTP 429") && writes(&t).len() == 3, "{}", rep.text);
    assert!(journal(&dir, "PROJECT-TRACKING").is_empty());
}

#[test]
fn comment_is_sent_once_after_an_uncertain_failure() {
    for (n, first) in [r(503, "{}"), Err(Failure::Timeout), Err(Failure::Connect), r(500, "{}")].into_iter().enumerate() {
        let (rep, t, dir) = exec(&cloud(), &applied(comment("hi")), vec![issue(), first, created()], &format!("unknown{n}"));
        assert_eq!(writes(&t).len(), 1, "never sent twice");
        assert_eq!(rep.code, 1);
        assert!(rep.text.contains("outcome unknown: check with atlas track issue PROJ-12"), "{}", rep.text);
        let j = journal(&dir, "PROJECT-TRACKING");
        assert!(j.len() == 1 && j[0].meta.contains("result: unknown"), "an unknown outcome is journaled as such: {j:?}");
    }
}

#[test]
fn a_transition_retries_only_after_a_re_read() {
    let s = cloud();
    let to = applied(WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") });
    let run3 = |tag: &str, tail: Vec<Result<Response, Failure>>| {
        let mut v = vec![r(200, &state("In Progress")), transitions()];
        v.extend(tail);
        exec(&s, &to, v, tag)
    };
    let (rep, t, _) = run3("tr-old", vec![r(503, "{}"), r(200, &state("In Progress")), r(204, "")]);
    assert!(rep.code == 0 && writes(&t).len() == 2, "{}", rep.text);
    let (rep, t, _) = run3("tr-new", vec![r(503, "{}"), r(200, &state("Done"))]);
    assert!(rep.code == 0 && writes(&t).len() == 1 && rep.text.starts_with("PROJ-12 In Progress -> Done journal:"), "{}", rep.text);
    let (rep, t, dir) = run3("tr-other", vec![r(503, "{}"), r(200, &state("Blocked"))]);
    assert!(rep.code == 1 && rep.text.contains("outcome unknown") && writes(&t).len() == 1, "{}", rep.text);
    assert!(journal(&dir, "PROJECT-TRACKING")[0].meta.contains("result: unknown"));
    // A 409 is re-read and retried once.
    let (rep, t, _) = run3("tr-409", vec![r(409, "{}"), r(200, &state("In Progress")), r(204, "")]);
    assert!(rep.code == 0 && writes(&t).len() == 2, "{}", rep.text);
}

#[test]
fn edits_retry_as_reads_and_a_409_once() {
    let s = cloud();
    let w = applied(WriteArgs { labels_add: vec!["x".into()], ..a(Verb::Edit, "PROJ-12") });
    let (rep, t, _) = exec(&s, &w, vec![issue(), r(503, "{}"), r(204, "")], "ed-503");
    assert!(rep.code == 0 && writes(&t).len() == 2, "{}", rep.text);
    let (rep, t, _) = exec(&s, &w, vec![issue(), r(409, "{}"), r(200, &state("In Progress")), r(204, "")], "ed-409");
    assert!(rep.code == 0 && writes(&t).len() == 2, "{}", rep.text);
    let (rep, t, _) = exec(&s, &w, vec![issue(), r(409, "{}"), r(200, &state("In Progress")), r(409, "{}"), r(204, "")], "ed-409x2");
    assert!(rep.code == 1 && writes(&t).len() == 2 && rep.text.contains("conflicting update"), "{}", rep.text);
}

#[test]
fn a_page_409_refuses_and_never_overwrites() {
    let conflict = r#"{"errors":[{"status":409,"code":"CONFLICT","title":"Version must be incremented on update. Current version is: 8","detail":null}]}"#;
    let (rep, t, dir) = exec(
        &cloud(),
        &page_update("<p>x</p>"),
        vec![r(200, &page(6, BODY)), r(409, conflict), r(200, &page(8, BODY)), r(200, "{}")],
        "p409",
    );
    assert_eq!(writes(&t).len(), 1, "no second PUT, with N+1 or any other version");
    assert_eq!(last_body(&t).pointer("/version/number").and_then(|v| v.as_u64()), Some(7));
    assert_eq!(rep.code, 1);
    assert_eq!(
        rep.text,
        "page 123: not written: the page changed since it was read (now version 8, sent 7) or the space needs approval before publishing; re-run to work from version 8"
    );
    assert!(journal(&dir, "PROJECT-TRACKING").is_empty());
}

#[test]
fn a_page_put_retries_only_while_the_version_is_unchanged() {
    let s = cloud();
    let (rep, t, _) = exec(
        &s,
        &page_update("<p>x</p>"),
        vec![r(200, &page(6, BODY)), r(503, "{}"), r(200, &page(6, BODY)), r(200, &page(7, "<p>x</p>"))],
        "p503",
    );
    assert!(rep.code == 0 && writes(&t).len() == 2, "{}", rep.text);
    let (rep, t, _) =
        exec(&s, &page_update("<p>x</p>"), vec![r(200, &page(6, BODY)), r(503, "{}"), r(200, &page(7, "<p>other</p>"))], "p503b");
    assert!(
        rep.code == 1 && writes(&t).len() == 1 && rep.text.contains("outcome unknown") && rep.text.contains("version 7"),
        "{}",
        rep.text
    );
}

#[test]
fn a_stale_expect_version_refuses_before_sending() {
    let s = cloud();
    let w = WriteArgs { expect_version: Some(5), ..page_update("<p>x</p>") };
    let (rep, t, _) = exec(&s, &w, vec![r(200, &page(6, BODY))], "expect");
    assert_eq!(rep.code, 2);
    assert!(
        rep.text.contains("expected version 5") && rep.text.contains("at version 6") && rep.text.contains("nothing sent"),
        "{}",
        rep.text
    );
    assert!(writes(&t).is_empty());
    let w = WriteArgs { expect_version: Some(6), ..page_update("<p>x</p>") };
    let (rep, _, _) = exec(&s, &w, vec![r(200, &page(6, BODY)), r(200, &page(7, "<p>x</p>"))], "expect-ok");
    assert_eq!(rep.code, 0, "{rep:?}");
}

#[test]
fn section_replace_changes_only_that_section() {
    let e = section_edit(BODY, "Rollout", "<p>new plan</p>", false).unwrap();
    assert_eq!(e.body, "<h1>Intro</h1><p>hello</p><h2>Rollout</h2><p>new plan</p><h2>Other</h2><p>o</p>");
    assert_eq!((e.old_len, e.new_len), ("<p>old plan</p><h3>Detail</h3><p>d</p>".len(), "<p>new plan</p>".len()));
    let e = section_edit(BODY, "Rollout", "<p>more</p>", true).unwrap();
    assert_eq!(e.body, "<h1>Intro</h1><p>hello</p><h2>Rollout</h2><p>old plan</p><h3>Detail</h3><p>d</p><p>more</p><h2>Other</h2><p>o</p>");
    // The last section runs to the end; a heading's level ends it, not a deeper one.
    assert_eq!(
        section_edit(BODY, "Other", "<p>z</p>", false).unwrap().body,
        "<h1>Intro</h1><p>hello</p><h2>Rollout</h2><p>old plan</p><h3>Detail</h3><p>d</p><h2>Other</h2><p>z</p>"
    );
    assert_eq!(section_edit(BODY, "Detail", "<p>q</p>", false).unwrap().body, BODY.replace("<p>d</p>", "<p>q</p>"));
    assert_eq!(section_edit(BODY, "Intro", "", false).unwrap().body, "<h1>Intro</h1>");
    // Missing and duplicated headings are refused naming the count.
    assert!(section_edit(BODY, "Nope", "x", false).unwrap_err().contains("0 headings"));
    let twice = format!("{BODY}<h2>Rollout</h2><p>again</p>");
    let err = section_edit(&twice, "Rollout", "x", false).unwrap_err();
    assert!(err.contains("2 headings") && err.contains("Rollout"), "{err}");
    // Entities and inline markup in a heading, as the editor stores them.
    let rich = "<h2><strong>Owners &amp; dates</strong></h2><p>a</p><h2 class=\"x\">Next</h2><p>b</p>";
    assert_eq!(
        section_edit(rich, "Owners & dates", "<p>z</p>", false).unwrap().body,
        "<h2><strong>Owners &amp; dates</strong></h2><p>z</p><h2 class=\"x\">Next</h2><p>b</p>"
    );
    // The fixture page: a round trip with a section's own content is byte-identical.
    let v: json::Value = json::parse(&fixture("atlassian/confluence-page.json")).unwrap();
    let body = v.pointer("/body/storage/value").and_then(|x| x.as_str()).unwrap().to_string();
    let e = section_edit(&body, "Build", "", false).unwrap();
    let own = &body[body.find("<h2>Build</h2>").unwrap() + "<h2>Build</h2>".len()..][..e.old_len];
    assert_eq!(section_edit(&body, "Build", own, false).unwrap().body, body);
    // Through the command: the PUT carries the edited body, the next version and the title.
    let w = WriteArgs { section: Some("Rollout".into()), message: Some("note".into()), ..page_update("<p>new plan</p>") };
    let (rep, t, _) = exec(&cloud(), &w, vec![r(200, &page(6, BODY)), r(200, &page(7, "x"))], "section-run");
    assert!(rep.text.starts_with("page 123 v6 -> v7 section Rollout (+15/-38 B) journal:"), "{}", rep.text);
    let b = last_body(&t);
    assert_eq!(
        b.pointer("/body/value").and_then(|x| x.as_str()),
        Some("<h1>Intro</h1><p>hello</p><h2>Rollout</h2><p>new plan</p><h2>Other</h2><p>o</p>")
    );
    assert_eq!(b.pointer("/body/representation").and_then(|x| x.as_str()), Some("storage"));
    assert_eq!(
        (b.pointer("/title").and_then(|x| x.as_str()), b.pointer("/status").and_then(|x| x.as_str())),
        (Some("Release"), Some("current"))
    );
    assert_eq!(b.pointer("/version/message").and_then(|x| x.as_str()), Some("note"));
    assert_eq!(writes(&t)[0].1, "https://w.example.atlassian.net/wiki/api/v2/pages/123");
    // --append alone adds to the end of the page; --title renames.
    let w = WriteArgs { append: true, title: Some("New".into()), ..page_update("<p>tail</p>") };
    let (_, t, _) = exec(&cloud(), &w, vec![r(200, &page(6, BODY)), r(200, &page(7, "x"))], "append-run");
    assert_eq!(last_body(&t).pointer("/body/value").and_then(|x| x.as_str()), Some(format!("{BODY}<p>tail</p>").as_str()));
    assert_eq!(last_body(&t).pointer("/title").and_then(|x| x.as_str()), Some("New"));
    // DC: the content API's shape.
    let dcpage = r#"{"id":"123","title":"Release","space":{"key":"ENG"},"version":{"number":3},"body":{"storage":{"value":"<p>a</p>","representation":"storage"}}}"#;
    let (rep, t, _) = exec(&dc(), &page_update("<p>b</p>"), vec![r(200, dcpage), r(200, dcpage)], "dc-page");
    assert_eq!(rep.code, 0, "{rep:?}");
    let b = last_body(&t);
    assert_eq!(
        (b.pointer("/type").and_then(|x| x.as_str()), b.pointer("/version/number").and_then(|x| x.as_u64())),
        (Some("page"), Some(4))
    );
    assert_eq!(b.pointer("/body/storage/value").and_then(|x| x.as_str()), Some("<p>b</p>"));
    assert_eq!(writes(&t)[0].1, "https://wiki.example.com/rest/api/content/123");
}

#[test]
fn errors_are_worded_without_assuming_a_403() {
    let s = cloud();
    let fail = |st: u16, body: &str, tag: &str| exec(&s, &applied(comment("hi")), vec![issue(), r(st, body)], tag).0;
    let rep = fail(400, r#"{"errorMessages":["Comment is too long"],"errors":{"body":"bad field"}}"#, "e400");
    assert!(
        rep.code == 1 && rep.text.contains("HTTP 400") && rep.text.contains("Comment is too long") && rep.text.contains("body: bad field"),
        "{}",
        rep.text
    );
    assert!(rep.text.contains("a missing permission shows as 400 or 404"), "{}", rep.text);
    assert!(fail(404, "{}", "e404").text.contains("not found, or this account may not view or edit it"));
    assert!(fail(401, "{}", "e401").text.contains("api.atlassian.com gateway"), "a Cloud site URL gets the gateway hint");
    assert!(fail(413, "{}", "e413").text.contains("limit"));
    assert!(fail(422, "{}", "e422").text.contains("configuration"));
    let page = exec(&s, &page_update("<p>x</p>"), vec![r(200, &page(6, BODY)), r(404, "{}")], "e404p").0;
    assert!(page.text.contains("not found, or this account may not view or edit it"), "{}", page.text);
    // A failed write never echoes the credential, even when the site does.
    let echo = format!(r#"{{"errorMessages":["Basic {} {WRITE}"]}}"#, base64(format!("e@example.com:{WRITE}").as_bytes()));
    let rep = fail(400, &echo, "echo");
    assert!(!rep.text.contains(WRITE) && !rep.text.contains(&base64(format!("e@example.com:{WRITE}").as_bytes())), "{}", rep.text);
}

#[test]
fn each_applied_write_journals_once() {
    let s = cloud();
    let dir = tdir("journal");
    let labels = WriteArgs { labels_add: vec!["a".into()], ..a(Verb::Edit, "PROJ-12") };
    let to = WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") };
    let runs: Vec<(WriteArgs, Vec<Result<Response, Failure>>)> = vec![
        (comment("Deployed."), vec![issue(), created()]),
        (to, vec![r(200, &state("In Progress")), transitions(), r(204, "")]),
        (labels, vec![issue(), r(204, "")]),
        (page_update("<p>x</p>"), vec![r(200, &page(6, BODY)), r(200, &page(7, "<p>x</p>"))]),
    ];
    for (n, (w, answers)) in runs.into_iter().enumerate() {
        let (rep, _, _) = exec_at(&s, &applied(w.clone()), answers, &dir, 5_000);
        assert_eq!(rep.code, 0, "{rep:?}");
        let j = journal(&dir, "PROJECT-TRACKING");
        assert_eq!(j.len(), n + 1, "exactly one new entry per applied write");
        let e = &j[n];
        assert!(rep.text.ends_with(&e.key), "the reply names the entry: {} / {}", rep.text, e.key);
        assert!(e.title.starts_with(&format!("track {} ", w.verb.word())), "{}", e.title);
        for k in ["verb:", "target:", "site: https://w.example.atlassian.net\n", "status:", "result:", "body_sha:", "handle:"] {
            assert!(e.meta.contains(k) || e.meta.ends_with(k.trim_end()), "{k} in {}", e.meta);
        }
        let all = format!("{} {} {}", e.title, e.meta, e.body);
        assert!(![READ, WRITE, WCONF, "Authorization"].iter().any(|x| all.contains(x)), "no token in the entry: {all}");
    }
    assert!(journal(&dir, "PROJECT-TRACKING")[0].title.contains("comment 10234 added"));
    // A dry-run, a refusal, a no-op and a failure write nothing.
    let quiet = tdir("journal-quiet");
    let noop = applied(WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") });
    for (w, answers) in [
        (comment("x"), vec![issue()]),
        (applied(WriteArgs { target: "OTHER-1".into(), ..comment("x") }), vec![]),
        (noop, vec![r(200, &state("Done"))]),
        (applied(comment("x")), vec![issue(), r(400, "{}")]),
    ] {
        let _ = exec_at(&s, &w, answers, &quiet, 5_000);
    }
    assert!(journal(&quiet, "PROJECT-TRACKING").is_empty());
    // The lane: --lane, else writes.journal_lane, else PROJECT-TRACKING.
    let lane = tdir("journal-lane");
    let w = applied(WriteArgs { lane: Some("MINE".into()), ..comment("a") });
    let _ = exec_at(&s, &w, vec![issue(), created()], &lane, 5_000);
    let cfg = cloud_cfg(r#""writes": {"journal_lane": "CONFIGURED", "jira": {"PROJ": ["comment"]}}"#);
    let _ = exec_at(&cfg, &applied(comment("b")), vec![issue(), created()], &lane, 5_000);
    let _ = exec_at(&s, &applied(comment("c")), vec![issue(), created()], &lane, 5_000);
    assert_eq!((journal(&lane, "MINE").len(), journal(&lane, "CONFIGURED").len(), journal(&lane, "PROJECT-TRACKING").len()), (1, 1, 1));
}

#[test]
fn an_unset_write_variable_is_named_never_valued() {
    let s =
        cloud_cfg(&format!(r#""jira_write_token_env": "NOPE_TOKEN", {CLOUD_WRITES}"#).replace("\"jira_write_token_env\": \"WT\", ", ""));
    let (rep, t, _) = exec(&s, &comment("x"), vec![issue()], "unset");
    assert!(rep.code == 0 && rep.text.contains("NOPE_TOKEN: unset") && t.seen.borrow().len() == 1, "{}", rep.text);
    let (rep, t, _) = exec(&s, &applied(comment("x")), vec![], "unset-apply");
    assert!(
        rep.code == 2 && rep.text.contains("the environment variable NOPE_TOKEN is not set") && rep.text.contains("nothing sent"),
        "{}",
        rep.text
    );
    assert!(t.seen.borrow().is_empty());
}

#[test]
fn write_replies_stay_compact() {
    let s = cloud();
    let dir = tdir("compact");
    let labels = WriteArgs {
        labels_add: vec!["a".into()],
        labels_remove: vec!["b".into()],
        sprint: Some("42".into()),
        assignee: Some("me".into()),
        ..a(Verb::Edit, "PROJ-12")
    };
    let to = WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") };
    let sec = WriteArgs { section: Some("Rollout".into()), ..page_update(&"<p>new</p>".repeat(500)) };
    let runs: Vec<(WriteArgs, Vec<Result<Response, Failure>>)> = vec![
        (comment(&"long text ".repeat(5000)), vec![issue(), created()]),
        (to, vec![r(200, &state("In Progress")), transitions(), r(204, "")]),
        (labels, vec![issue(), r(200, &fixture("atlassian/jira-myself.json")), r(200, &fixture("atlassian/writes/jira-fields.json")), r(204, "")]),
        (sec, vec![r(200, &page(6, BODY)), r(200, &page(7, "x"))]),
    ];
    for (w, answers) in runs {
        let (rep, _, _) = exec_at(&s, &applied(w.clone()), answers, &dir, 7_000);
        assert!(rep.code == 0 && rep.text.len() <= 200 && !rep.text.contains('\n'), "{} bytes: {}", rep.text.len(), rep.text);
        assert!(!rep.text.contains('{') && !rep.text.contains("<p>"), "no JSON or page echoed: {}", rep.text);
        let (dry, _, _) = exec_at(
            &s,
            &w,
            vec![
                r(200, &state("In Progress")),
                transitions(),
                r(200, &page(6, BODY)),
                r(200, &fixture("atlassian/jira-myself.json")),
                r(200, &fixture("atlassian/writes/jira-fields.json")),
            ],
            &tdir("compact-dry"),
            7_000,
        );
        assert!(dry.text.len() <= 1200, "{:?}: a dry-run is {} bytes", w.verb, dry.text.len());
    }
    // The shape of each reply.
    assert!(journal(&dir, "PROJECT-TRACKING")[2].title.contains("labels +a -b, assignee me, sprint 42"));
}

#[test]
fn the_write_calls_are_listed_and_the_reads_stay_reads() {
    let s = cloud();
    let labels =
        WriteArgs { labels_add: vec!["a".into()], assignee: Some("me".into()), sprint: Some("1".into()), ..a(Verb::Edit, "PROJ-12") };
    let to = WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") };
    let runs: Vec<(WriteArgs, Vec<Result<Response, Failure>>)> = vec![
        (comment("x"), vec![issue(), created()]),
        (to, vec![r(200, &state("In Progress")), transitions(), r(204, "")]),
        (labels, vec![issue(), r(200, &fixture("atlassian/jira-myself.json")), r(200, &fixture("atlassian/writes/jira-fields.json")), r(204, "")]),
        (page_update("<p>x</p>"), vec![r(200, &page(6, BODY)), r(200, &page(7, "x"))]),
    ];
    for (w, answers) in runs {
        let (_, t, _) = exec(&s, &applied(w), answers, "listed");
        for (m, url, _, cfg) in t.seen.borrow().iter() {
            let path = url.trim_start_matches(SITE).trim_start_matches("/wiki").split('?').next().unwrap();
            let matches = |tpl: &str| {
                let pre = tpl.split('{').next().unwrap();
                if tpl.contains('{') {
                    path.starts_with(pre)
                        && path.trim_start_matches(pre).split('/').count() <= tpl.matches('/').count() - pre.matches('/').count() + 1
                } else {
                    path == tpl
                }
            };
            if m == "GET" {
                assert!(
                    PRE_READS.iter().any(|e| matches(e.cloud)) || crate::trackers::api::ENDPOINTS.iter().any(|e| matches(e.cloud)),
                    "{path}"
                );
                assert!(!cfg.contains("data-binary"), "{cfg}");
            } else {
                assert!(WRITE_ENDPOINTS.iter().any(|e| e.method.name() == m && matches(e.cloud)), "{m} {path} is not a listed write");
                assert!(cfg.contains(&format!("request = {m}\n")) && cfg.contains("data-binary = \"{"), "{cfg}");
            }
        }
    }
    assert!(WRITE_ENDPOINTS.iter().all(|e| !e.cloud.contains("delete") && !e.dc.contains("delete")));
    // The body goes in the config on stdin, quoted, never on the command line.
    let body = r#"{"a":"b\\c\"d"}"#;
    let req = super::http::Request {
        method: super::http::Method::Get,
        write: Some(super::http::WriteBody { method: super::http::WriteMethod::Put, body }),
        url: "https://x.atlassian.net/rest/api/3/issue/P-1",
        auth: None,
        timeout: Duration::from_secs(5),
        max_bytes: 10,
    };
    let c = super::http::curl_config(&req);
    assert!(c.contains("request = PUT\n") && c.contains(r#"data-binary = "{\"a\":\"b\\\\c\\\"d\"}""#), "{c}");
    let file = super::http::Request {
        write: Some(super::http::WriteBody { method: super::http::WriteMethod::Post, body: "@/etc/passwd" }),
        ..req
    };
    assert!(!super::http::curl_config(&file).contains("data-binary"), "a body that is not a JSON object is never sent");
}

/// `text` with each handle id (`h:` and the characters after it) made `h:ID`.
fn no_handle(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("h:") {
        out.push_str(&rest[..i + 2]);
        rest = rest[i + 2..].trim_start_matches(|c: char| c.is_ascii_alphanumeric());
        out.push_str("ID");
    }
    out + rest
}

#[test]
fn the_mcp_op_matches_the_cli() {
    let dir = tdir("mcp");
    let file = dir.join("body.txt");
    std::fs::write(&file, "from a file").unwrap();
    let argv = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let map = |t: &str| match json::parse(t).unwrap() {
        json::Value::Object(m) => m,
        _ => panic!(),
    };
    let f = file.display().to_string();
    let cases: Vec<(Verb, Vec<String>, String)> = vec![
        (Verb::Comment, argv(&["PROJ-12", "--body-file", &f, "--again", "--apply", "--lane", "L"]), format!(r#"{{"verb":"comment","target":"PROJ-12","body_file":{f:?},"again":true,"apply":true,"lane":"L"}}"#)),
        (Verb::Transition, argv(&["PROJ-12", "--to", "Done"]), r#"{"verb":"transition","target":"PROJ-12","to":"Done"}"#.into()),
        (
            Verb::Edit,
            argv(&["PROJ-12", "--assignee", "me", "--labels-add", "a,b", "--labels-remove", "c", "--sprint", "42", "--apply"]),
            r#"{"verb":"edit","target":"PROJ-12","assignee":"me","labels_add":["a","b"],"labels_remove":"c","sprint":"42","apply":true}"#.into(),
        ),
        (
            Verb::PageUpdate,
            argv(&["123", "--section", "Rollout", "--append", "--body", "x", "--title", "T", "--expect-version", "6", "--message", "m"]),
            r#"{"verb":"page_update","target":"123","section":"Rollout","append":true,"body":"x","title":"T","expect_version":6,"message":"m"}"#.into(),
        ),
    ];
    for (verb, args, op) in cases {
        let from_cli = WriteArgs::from_argv(verb, &args).unwrap();
        let from_op = WriteArgs::from_map(&map(&op)).unwrap();
        assert_eq!(from_cli, from_op, "{verb:?}");
        // The same arguments, the same function, the same reply.
        let s = cloud();
        let (a1, a2) = (WriteArgs { apply: false, ..from_cli }, WriteArgs { apply: false, ..from_op });
        let answers = || vec![r(200, &page(6, BODY)), r(200, &state("In Progress")), transitions()];
        let (r1, _, _) = exec(&s, &a1, answers(), "mcp1");
        let (r2, _, _) = exec(&s, &a2, answers(), "mcp2");
        assert_eq!(no_handle(&r1.text), no_handle(&r2.text), "{verb:?}");
    }
    // Dry-run by default, unknown verbs and stray flags refused.
    assert!(!WriteArgs::from_map(&map(r#"{"verb":"comment","target":"P-1","body":"x"}"#)).unwrap().apply);
    assert!(WriteArgs::from_map(&map(r#"{"verb":"delete","target":"P-1"}"#)).is_err());
    assert!(WriteArgs::from_map(&map(r#"{"verb":"comment","target":"P-1","body":"x","body_file":"f"}"#)).is_err());
    assert!(WriteArgs::from_argv(Verb::Comment, &argv(&["P-1", "--force"])).unwrap_err().contains("unknown argument"));
    assert!(WriteArgs::from_argv(Verb::Comment, &argv(&["--body", "x"])).unwrap_err().contains("missing its issue key"));
    assert!(WriteArgs::from_argv(Verb::Transition, &argv(&["P-1", "--to"])).unwrap_err().contains("needs a value"));
}

#[test]
fn check_names_the_write_variables_and_the_allowed_verbs() {
    let s = cloud();
    let t = Script::new(vec![
        Ok(ok(&fixture("atlassian/jira-myself.json"))),
        Ok(ok(r#"{"permissions":{}}"#)),
        Ok(ok(&fixture("atlassian/confluence-user.json"))),
    ]);
    let dir = tdir("check");
    let (db, store) = (dir.join("a.db"), dir.join("m.sqlite"));
    let ctx = Ctx { settings: Some(&s), db: &db, lean: None, store: &store, env: &env };
    let mut out = Vec::new();
    super::api::check_with(&s, &ctx, &mut out, &t).unwrap();
    let o = String::from_utf8(out).unwrap();
    assert!(o.contains("jira write token: WT set; allowed verbs 5 (1 project)"), "{o}");
    assert!(o.contains("confluence write token: WCT set; allowed verbs 1 (1 space)"), "{o}");
    assert!(!o.contains(WRITE) && !o.contains("atlas itself sends GET only"), "{o}");
    let ro = Settings::from_config(Some(
        &json::parse(&format!(r#"{{"atlassian": {{"site": "{SITE}", "email_env": "E", "token_env": "T"}}}}"#)).unwrap(),
    ))
    .unwrap()
    .unwrap();
    let t = Script::new(vec![]);
    let mut out = Vec::new();
    super::api::check_with(&ro, &Ctx { settings: Some(&ro), ..ctx }, &mut out, &t).unwrap();
    let o = String::from_utf8(out).unwrap();
    assert!(o.contains("jira write token: not configured (jira_write_token_env); writes refused"), "{o}");
}

// ---- the size table against the Atlassian MCP (xtask write-cost reads the golden) ----

const BIG_SECTION: &str = "Rollout";

/// A 6 KB storage-format page whose "Rollout" section is about 600 bytes.
fn big_body(rollout: &str) -> String {
    let filler = |s: &str, n: usize| format!("<p>{}</p>", s.repeat(n));
    format!(
        "<h1>Release plan</h1>{}<h2>{BIG_SECTION}</h2>{rollout}<h2>Owners</h2>{}<h2>Risks</h2>{}",
        filler("This page tracks the quarterly platform release across every team. ", 28),
        filler("Owners and escalation contacts are listed per service below. ", 30),
        filler("Known risks, their mitigations and the people who watch them. ", 25),
    )
}

fn text_of(n: usize, seed: &str) -> String {
    let mut s = seed.repeat(n / seed.len() + 1);
    s.truncate(n);
    s
}

fn size_cases() -> json::Value {
    let s = cloud();
    let old = format!("<p>{}</p>", text_of(593, "Stage one rolls to the canary hosts first. "));
    let new = format!("<p>{}</p>", text_of(593, "Stage two moves to every region in waves. "));
    assert_eq!((old.len(), new.len()), (600, 600));
    let (before, after) = (big_body(&old), big_body(&new));
    assert!((5800..6600).contains(&before.len()), "{}", before.len());
    let page_json = |v: u64, body: &str| page(v, body);
    let fx = |n: &str| fixture(&format!("atlassian/mcp-replies/{n}"));
    let text = "Deployed build 4471 to staging. Smoke tests pass; the rollout to production waits on the change window Thursday.";
    let cloud_id = "0b6d4c1e-3f52-4c0a-9a58-1d2e7f6a9b10";
    let dir = tdir("size");
    let mut out = Vec::new();
    // (name, atlas args, dry answers, apply answers, atlas command, MCP inputs, MCP replies)
    let edit = WriteArgs {
        labels_add: vec!["release".into()],
        labels_remove: vec!["draft".into()],
        sprint: Some("42".into()),
        assignee: Some("me".into()),
        ..a(Verb::Edit, "PROJ-12")
    };
    let sec = WriteArgs { section: Some(BIG_SECTION.into()), ..page_update(&new) };
    type Answers = Vec<Result<Response, Failure>>;
    type Case = (&'static str, WriteArgs, Answers, Answers, String, Vec<String>, Vec<String>);
    let cases: Vec<Case> = vec![
        (
            "comment",
            comment(text),
            vec![issue()],
            vec![issue(), created()],
            format!("atlas track comment PROJ-12 --body \"{text}\" --apply"),
            vec![json::to_string(&json!({"cloudId": cloud_id, "issueIdOrKey": "PROJ-12", "commentBody": text}))],
            vec![fx("jira-add-comment.json")],
        ),
        (
            "transition",
            WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") },
            vec![r(200, &state("In Progress")), transitions()],
            vec![r(200, &state("In Progress")), transitions(), r(204, "")],
            "atlas track transition PROJ-12 --to Done --apply".into(),
            vec![
                json::to_string(&json!({"cloudId": cloud_id, "issueIdOrKey": "PROJ-12"})),
                json::to_string(&json!({"cloudId": cloud_id, "issueIdOrKey": "PROJ-12", "transition": {"id": "31"}})),
            ],
            vec![fx("jira-transitions.json"), fx("jira-issue.json")],
        ),
        (
            "edit",
            edit,
            vec![issue(), r(200, &fixture("atlassian/jira-myself.json")), r(200, &fixture("atlassian/writes/jira-fields.json"))],
            vec![issue(), r(200, &fixture("atlassian/jira-myself.json")), r(200, &fixture("atlassian/writes/jira-fields.json")), r(204, "")],
            "atlas track edit PROJ-12 --assignee me --labels-add release --labels-remove draft --sprint 42 --apply".into(),
            vec![json::to_string(
                &json!({"cloudId": cloud_id, "issueIdOrKey": "PROJ-12", "fields": {"assignee": {"accountId": "5b10ac8d82e05b22cc7d4ef5"}, "labels": ["release", "staging"], "customfield_10020": 42}}),
            )],
            vec![fx("jira-issue.json")],
        ),
        (
            "page_update",
            sec,
            vec![r(200, &page_json(6, &before))],
            vec![r(200, &page_json(6, &before)), r(200, &page_json(7, &after))],
            format!("atlas track page-update 123 --section {BIG_SECTION} --body \"{new}\" --apply"),
            vec![json::to_string(
                &json!({"cloudId": cloud_id, "pageId": "123", "title": "Release", "body": after, "version": {"number": 7}, "contentFormat": "storage"}),
            )],
            vec![page_json(7, &after)],
        ),
    ];
    for (name, w, dry_answers, apply_answers, cmd, mcp_in, mcp_out) in cases {
        let mut dry_w = w.clone();
        dry_w.apply = false;
        let dry = if dry_answers.is_empty() {
            exec_at(&s, &dry_w, vec![], &dir, 7_000).0
        } else {
            exec_at(&s, &dry_w, dry_answers, &dir, 7_000).0
        };
        let mut ap = w.clone();
        ap.apply = true;
        let apply = exec_at(&s, &ap, apply_answers, &dir, 9_000).0;
        assert_eq!((dry.code, apply.code), (0, 0), "{name}: {dry:?} {apply:?}");
        out.push(json!({
            "name": name,
            "atlas_in": cmd,
            "atlas_dry": dry.text,
            "atlas_apply": apply.text,
            "mcp_in": mcp_in,
            "mcp_out": mcp_out,
        }));
    }
    json!({"basis": "documented response shapes as synthetic fixtures; swap in recorded replies from a live run when Brandon has them", "cases": out})
}

#[test]
fn the_size_table_golden_matches_the_code() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/atlassian/mcp-replies/size-cases.json");
    let now = json::to_string_pretty(&size_cases());
    if std::env::var("ATLAS_BLESS").is_ok() {
        std::fs::write(&path, &now).unwrap();
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(have.trim(), now.trim(), "size-cases.json is stale: ATLAS_BLESS=1 cargo test --lib --features dev the_size_table");
}

// ---- fix round 1 (review journal:ATLAS-TRACKERS:0061)

/// The four Jira writes, applied, with the answers each needs after the
/// issue read.
fn jira_writes() -> Vec<(WriteArgs, Vec<Result<Response, Failure>>)> {
    vec![
        (applied(comment("x")), vec![created()]),
        (applied(WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") }), vec![transitions(), r(204, "")]),
        (applied(WriteArgs { labels_add: vec!["a".into()], ..a(Verb::Edit, "PROJ-12") }), vec![r(204, "")]),
        (applied(WriteArgs { sprint: Some("7".into()), ..a(Verb::Edit, "PROJ-12") }), vec![r(200, &fixture("atlassian/writes/jira-fields.json")), r(204, "")]),
    ]
}

/// M1: PROJ-12 was moved to SEC (now SEC-3) and Jira still answers to the
/// old key. The allow-list names PROJ only, so every write is refused by the
/// issue's real project, and nothing is sent.
#[test]
fn a_moved_issue_is_refused_by_its_real_project() {
    let moved = r#"{"key":"SEC-3","fields":{"project":{"key":"SEC"},"status":{"name":"In Progress","id":"3"}}}"#;
    for (w, _) in jira_writes() {
        for apply in [false, true] {
            let w = WriteArgs { apply, ..w.clone() };
            let (rep, t, dir) = exec(&cloud(), &w, vec![r(200, moved)], "moved");
            assert_eq!(rep.code, 2, "{:?}: {rep:?}", w.verb);
            assert!(rep.text.contains("not allowed for Jira project SEC") && rep.text.contains("has no SEC"), "{}", rep.text);
            assert!(rep.text.contains("PROJ-12 is now in SEC") && rep.text.ends_with("nothing sent"), "{}", rep.text);
            assert!(writes(&t).is_empty(), "{:?}: nothing sent", w.verb);
            assert_eq!(t.seen.borrow().len(), 1, "{:?}: only the issue read", w.verb);
            assert!(journal(&dir, "PROJECT-TRACKING").is_empty());
        }
    }
    // A reply without a project is refused too: the check never passes on an empty key.
    let (rep, t, _) = exec(&cloud(), &applied(comment("x")), vec![r(200, r#"{"key":"PROJ-12","fields":{}}"#)], "moved-none");
    assert!(rep.code == 2 && rep.text.contains("project could not be read") && writes(&t).is_empty(), "{}", rep.text);
    // A failed issue read sends nothing.
    let (rep, t, _) = exec(&cloud(), &applied(comment("x")), vec![r(404, r#"{"errorMessages":["Issue does not exist"]}"#)], "moved-404");
    assert!(rep.code != 0 && writes(&t).is_empty(), "{}", rep.text);
}

/// M1: before each Jira write, exactly one GET of the issue with
/// `fields=project`, with the read token; the dry-run shows the project.
#[test]
fn every_jira_write_reads_the_project_first() {
    for (i, (w, rest)) in jira_writes().into_iter().enumerate() {
        let mut answers = vec![issue()];
        answers.extend(rest);
        let (rep, t, _) = exec(&cloud(), &w, answers, "reads-project");
        assert_eq!(rep.code, 0, "{:?}: {rep:?}", w.verb);
        let seen = t.seen.borrow();
        let first = &seen[0];
        assert_eq!((first.0.as_str(), first.1.as_str()), ("GET", "https://w.example.atlassian.net/rest/api/3/issue/PROJ-12?fields=project,status"));
        assert!(first.3.contains(&base64(format!("e@example.com:{READ}").as_bytes())), "the read token");
        let write_at = seen.iter().position(|x| x.0 != "GET").expect("a write");
        let project_reads = seen[..write_at].iter().filter(|x| x.1.contains("fields=project")).count();
        assert_eq!(project_reads, 1, "{:?}: one project read before the write", w.verb);
        assert_eq!(seen.iter().filter(|x| x.0 != "GET").count(), 1);
        drop(seen);
        // The dry-run: the same reads, without the write's answer.
        let mut dry = vec![issue()];
        dry.extend(jira_writes().swap_remove(i).1);
        dry.pop();
        let (rep, _, _) = exec(&cloud(), &WriteArgs { apply: false, ..w.clone() }, dry, "reads-dry");
        assert!(rep.text.contains("project: PROJ (read from the issue)"), "{:?}: {}", w.verb, rep.text);
    }
    // The transition takes the status from that same read: no second issue GET.
    let w = applied(WriteArgs { to: Some("Done".into()), ..a(Verb::Transition, "PROJ-12") });
    let (_, t, _) = exec(&cloud(), &w, vec![r(200, &state("Done"))], "reads-noop");
    assert_eq!(t.seen.borrow().len(), 1, "already Done: one read, nothing sent");
}

const LAYOUT: &str = "<ac:layout><ac:layout-section ac:type=\"two_equal\"><ac:layout-cell><h2>A</h2><p>a</p></ac:layout-cell><ac:layout-cell><h2>B</h2><p>b<br>c<br/></p></ac:layout-cell></ac:layout-section></ac:layout>";

/// S2: a heading inside a layout cell, a table cell or a macro body scopes
/// its section to that container; what cannot be scoped is refused.
#[test]
fn a_nested_section_is_refused_or_scoped() {
    // Replace and append stay inside the first cell.
    assert_eq!(section_edit(LAYOUT, "A", "<p>z</p>", false).unwrap().body, LAYOUT.replace("<p>a</p>", "<p>z</p>"));
    assert_eq!(section_edit(LAYOUT, "A", "<p>z</p>", true).unwrap().body, LAYOUT.replace("<p>a</p>", "<p>a</p><p>z</p>"));
    // The last heading of the last cell keeps the layout's closing tags.
    let e = section_edit(LAYOUT, "B", "<p>z</p>", false).unwrap();
    assert_eq!(e.body, LAYOUT.replace("<p>b<br>c<br/></p>", "<p>z</p>"));
    assert_eq!(e.old_len, "<p>b<br>c<br/></p>".len());
    // A table cell, with a later top-level heading of the same level.
    let table = "<table><tbody><tr><td><h3>X</h3><p>x</p></td><td><p>y</p></td></tr></tbody></table><h3>After</h3><p>t</p>";
    assert_eq!(section_edit(table, "X", "<p>q</p>", false).unwrap().body, table.replace("<p>x</p>", "<p>q</p>"));
    assert_eq!(section_edit(table, "After", "<p>q</p>", true).unwrap().body, format!("{table}<p>q</p>"));
    // A top-level section holds the containers below it, and a deeper heading of the same level stays inside.
    let outer = "<h2>Top</h2><div><h2>Inner</h2><p>i</p></div><p>p</p><h2>Next</h2><p>n</p>";
    assert_eq!(section_edit(outer, "Top", "", false).unwrap().body, "<h2>Top</h2><h2>Next</h2><p>n</p>");
    assert_eq!(section_edit(outer, "Inner", "<p>j</p>", false).unwrap().body, outer.replace("<p>i</p>", "<p>j</p>"));
    // A heading in a code macro's CDATA or in a comment is not a heading.
    let code = "<h2>Code</h2><ac:structured-macro ac:name=\"code\"><ac:plain-text-body><![CDATA[<h2>Fake</h2> a < b]]></ac:plain-text-body></ac:structured-macro><!-- <h2>Gone</h2> --><h2>Next</h2><p>n</p>";
    for fake in ["Fake", "Gone"] {
        assert!(section_edit(code, fake, "x", false).unwrap_err().contains("0 headings"), "{fake}");
    }
    let cut = &code[..code.find("<h2>Next").unwrap()];
    assert_eq!(section_edit(code, "Code", "<p>c</p>", false).unwrap().body, format!("<h2>Code</h2><p>c</p>{}", &code[cut.len()..]));
    // A body that does not nest cannot be scoped: refused with the reason.
    for bad in ["<ac:layout-cell><h2>A</h2><p>a</p>", "<h2>A</h2><p>a</div>", "<h2>A</h2></p>", "<h2>A</h2><p>a <!-- x", "<h2>A</h2><p>a < b</p>", "<h2>A</h2><p"] {
        let err = section_edit(bad, "A", "<p>z</p>", false).unwrap_err();
        assert!(err.contains("does not nest") && err.contains("replace the whole page"), "{bad}: {err}");
    }
    // New content that would close the cell is refused, for a replace and an append.
    for append in [false, true] {
        let err = section_edit(LAYOUT, "A", "<p>z</p></ac:layout-cell><ac:layout-cell>", append).unwrap_err();
        assert!(err.contains("would cross its container"), "{err}");
    }
    // Through the command: refused before the PUT.
    let w = WriteArgs { section: Some("A".into()), ..page_update("<p>z</p></ac:layout-cell>") };
    let (rep, t, _) = exec(&cloud(), &w, vec![r(200, &page(6, LAYOUT)), r(200, &page(7, LAYOUT))], "nested-cmd");
    assert!(rep.code == 2 && rep.text.contains("would cross its container") && writes(&t).is_empty(), "{}", rep.text);
}

/// An append's sizes: the section grows by the new content (mutants: the sum).
#[test]
fn an_append_reports_the_section_growing_by_the_new_content() {
    let e = section_edit(LAYOUT, "A", "<p>zz</p>", true).unwrap();
    assert_eq!((e.old_len, e.new_len), ("<p>a</p>".len(), "<p>a</p>".len() + "<p>zz</p>".len()));
}

/// A declaration or processing instruction is not an element (mutants: tags).
#[test]
fn a_declaration_is_not_an_element() {
    for head in ["<?xml version=\"1.0\"?>", "<!DOCTYPE html>"] {
        let body = format!("{head}<h2>A</h2><p>a</p>");
        assert_eq!(section_edit(&body, "A", "<p>z</p>", false).unwrap().body, format!("{head}<h2>A</h2><p>z</p>"), "{head}");
    }
}

/// Targets and lanes are checked before any request (mutants: valid_target, key_ok).
#[test]
fn a_malformed_target_or_lane_is_refused_before_any_request() {
    let long = format!("P-{}", "1".repeat(70));
    for target in ["PROJ12", "-12", "PROJ-", "PROJ-1a", "PR OJ-1", "PROJ/../X-1", long.as_str()] {
        let (rep, t, _) = exec(&cloud(), &WriteArgs { target: target.into(), ..comment("x") }, vec![issue()], "bad-target");
        assert!(rep.code == 2 && rep.text.contains("an issue key is PROJECT-NUMBER") && t.seen.borrow().is_empty(), "{target}: {}", rep.text);
    }
    for page in ["", "12a", "../1", &"1".repeat(33)] {
        let (rep, t, _) = exec(&cloud(), &WriteArgs { target: page.into(), ..page_update("<p>x</p>") }, vec![], "bad-page");
        assert!(rep.code == 2 && rep.text.contains("a page id is digits") && t.seen.borrow().is_empty(), "{page:?}: {}", rep.text);
    }
    let (rep, _, _) = exec(&cloud(), &WriteArgs { target: "123".into(), ..page_update("<p>x</p>") }, vec![r(200, &page(6, BODY))], "good-page");
    assert!(!rep.text.contains("a page id is digits"), "{}", rep.text);
    assert!(super::key_ok("A-b_9~") && super::key_ok(&"a".repeat(64)));
    let (rep, _, _) = exec(&cloud(), &WriteArgs { target: "PRO_J-12".into(), ..comment("x") }, vec![issue()], "good-target");
    assert!(!rep.text.contains("PROJECT-NUMBER"), "an underscore is a key character: {}", rep.text);
    for lane in ["", "a b", "a/b", &"a".repeat(65)] {
        assert!(!super::key_ok(lane), "{lane:?}");
        let (rep, t, _) = exec(&cloud(), &WriteArgs { lane: Some(lane.into()), ..comment("x") }, vec![issue()], "bad-lane");
        assert!(rep.code == 2 && rep.text.contains("--lane is a lane name") && t.seen.borrow().is_empty(), "{lane:?}: {}", rep.text);
    }
}

/// Each verb's own input checks, an empty allow-list entry, and the 429
/// backoff doubling (mutants: check_input, allowed, send).
#[test]
fn inputs_an_empty_entry_and_the_backoff_are_checked() {
    let s = cloud();
    let cases = [
        (WriteArgs { body: Some("  ".into()), ..a(Verb::Comment, "PROJ-12") }, "a comment needs --body"),
        (a(Verb::Transition, "PROJ-12"), "a transition needs --to"),
        (WriteArgs { to: Some(" ".into()), ..a(Verb::Transition, "PROJ-12") }, "a transition needs --to"),
        (a(Verb::PageUpdate, "123"), "a page update needs --body-file"),
    ];
    for (w, want) in cases {
        let (rep, t, _) = exec(&s, &w, vec![issue()], "input");
        assert!(rep.code == 2 && rep.text.contains(want) && t.seen.borrow().is_empty(), "{want}: {}", rep.text);
    }
    let none = cloud_cfg(r#""writes": {"jira": {"PROJ": []}}"#);
    let (rep, _, _) = exec(&none, &comment("x"), vec![], "empty-entry");
    assert!(rep.text.contains("writes.jira.PROJ allows: nothing"), "{}", rep.text);
    let spaces = cloud_cfg(r#""writes": {"confluence": {"4456": []}}"#);
    let (rep, _, _) = exec(&spaces, &page_update("<p>x</p>"), vec![r(200, &page(6, BODY))], "empty-space");
    assert!(rep.code == 2 && rep.text.contains("allows: nothing"), "{}", rep.text);
    let listed = cloud_cfg(r#""writes": {"confluence": {"4456": ["page_update"]}, "jira": {"PROJ": ["comment"]}}"#);
    let (rep, _, _) = exec(&listed, &page_update("<p>x</p>"), vec![r(200, &page(6, BODY)), r(200, &page(7, "<p>x</p>"))], "listed-space");
    assert!(!rep.text.contains("allows: nothing"), "{}", rep.text);
    // The duplicate window is ten minutes, exclusive: at 600 s the same comment goes.
    let dir = tdir("dup-edge");
    let w = applied(comment("Same."));
    assert_eq!(exec_at(&s, &w, vec![issue(), created()], &dir, 1_000).0.code, 0);
    assert_eq!(exec_at(&s, &w, vec![issue(), created()], &dir, 1_599).0.code, 2, "599 s: a duplicate");
    assert_eq!(exec_at(&s, &w, vec![issue(), created()], &dir, 1_600).0.code, 0, "600 s: not a duplicate");
    // A 429 without Retry-After backs off from backoff_ms (10), doubling.
    let (rep, _, waits) = exec_at(&s, &applied(comment("hi")), vec![issue(), r(429, "{}"), r(429, "{}"), created()], &tdir("backoff"), 1_000_000);
    assert_eq!(rep.code, 0, "{rep:?}");
    for want in [10, 20] {
        assert!(waits.contains(&Duration::from_millis(want)), "{want} ms in {waits:?}");
    }
}

/// Every write's curl config caps the reply at 4 MiB (`max_bytes`), Jira and
/// Confluence alike; reads keep their own cap.
#[test]
fn a_write_request_caps_its_reply_at_4_mib() {
    let mut runs: Vec<(WriteArgs, Vec<Result<Response, Failure>>)> = Vec::new();
    for (w, rest) in jira_writes() {
        let mut answers = vec![issue()];
        answers.extend(rest);
        runs.push((w, answers));
    }
    runs.push((page_update("<p>x</p>"), vec![r(200, &page(6, BODY)), r(200, &page(7, "<p>x</p>"))]));
    for (w, answers) in runs {
        let (rep, t, _) = exec(&cloud(), &w, answers, "cap-4mib");
        assert_eq!(rep.code, 0, "{:?}: {rep:?}", w.verb);
        let seen = t.seen.borrow();
        let sent: Vec<_> = seen.iter().filter(|x| x.0 != "GET").collect();
        assert_eq!(sent.len(), 1, "{:?}: one write", w.verb);
        assert!(sent[0].3.contains("max-filesize = 4194304\n"), "{:?}: the write's reply is capped at 4 MiB", w.verb);
    }
}
