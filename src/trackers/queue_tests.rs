//! #305: a JQL search planned as queue items, snapshots refreshed in bulk,
//! and `after: ticket KEY STATUS` met from a snapshot.

use std::collections::HashSet;
use std::process::ExitCode;

use super::api::tests::{ok, settings, Script};
use super::http::Response;
use super::{plan, snapshot, Ctx};
use crate::memory::MemoryStore;
use crate::queue::{self, meta_value, DepState, Marks};

fn env(n: &str) -> Option<String> {
    matches!(n, "E" | "T").then(|| "v-SENTINEL".to_string())
}

fn item(store: &mut MemoryStore, lane: &str, meta: &[&str]) -> String {
    let p = queue::Planned {
        lane: lane.into(),
        kind: "builder".into(),
        title: format!("work on {lane}"),
        scope: "do it".into(),
        extra: meta.iter().map(|l| l.to_string()).collect(),
        ..Default::default()
    };
    queue::add_planned(store, &p).unwrap()
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|x| x.to_string()).collect()
}

fn issue(key: &str, summary: &str, status: &str, category: &str) -> String {
    format!(
        r#"{{"key":"{key}","fields":{{"summary":"{summary}","status":{{"name":"{status}","statusCategory":{{"name":"{category}"}}}},"assignee":{{"displayName":"Robin"}}}}}}"#
    )
}

#[test]
fn a_jql_search_becomes_one_item_per_issue_with_its_snapshot() {
    let mut store = MemoryStore::in_memory().unwrap();
    let before = item(&mut store, "ES1-2", &["ticket: ES1-2"]);
    let body = format!(
        r#"{{"issues":[{},{},{}],"isLast":true}}"#,
        issue("ES1-1", "Add the geo lookup", "Ready For Dev", "To Do"),
        issue("ES1-2", "Already planned", "Ready For Dev", "To Do"),
        issue("ES1-3", "Fix the cart total", "Dev In Progress", "In Progress"),
    );
    let t = Script::new(vec![Ok(ok(&body))]);
    let s = settings("https://plan.example.atlassian.net", true);
    let ctx = Ctx { settings: Some(&s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env: &env };
    let mut out = Vec::new();
    let a = args(&["plan", "--from-jql", "sprint = 7", "--order-from", "5", "--priority", "urgent", "--limit", "3"]);
    let code = plan::run(&a, &ctx, &mut store, &[], &t, &mut out).unwrap();
    let o = String::from_utf8(out).unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "{o}");
    assert!(o.contains("from-jql: 2 planned, 1 skipped"), "{o}");
    let seen = t.seen.borrow();
    assert_eq!(seen.len(), 1, "one search, no request per issue");
    assert!(seen[0].1.contains("maxResults=3"), "--limit pages the search: {}", seen[0].1);
    let heads = queue::waiting(&store).unwrap();
    assert_eq!(heads.len(), 3, "the planned one stays and two are new");
    assert_eq!(heads[0].key, before);
    let made: Vec<_> = heads.iter().filter(|h| h.key != before).collect();
    let got: Vec<_> = made
        .iter()
        .map(|h| {
            (
                h.scope.as_str(),
                h.title.as_str(),
                meta_value(&h.meta, "ticket"),
                meta_value(&h.meta, "ticket_status"),
                meta_value(&h.meta, "ticket_category"),
                meta_value(&h.meta, "order"),
                meta_value(&h.meta, "priority"),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("ES1-1", "Add the geo lookup", Some("ES1-1"), Some("Ready For Dev"), Some("To Do"), Some("5"), Some("urgent")),
            ("ES1-3", "Fix the cart total", Some("ES1-3"), Some("Dev In Progress"), Some("In Progress"), Some("6"), Some("urgent")),
        ]
    );
    assert!(made.iter().all(|h| meta_value(&h.meta, "ticket_fetched").is_some() && meta_value(&h.meta, "ticket_via") == Some("rest")));
    assert!(!o.contains("SENTINEL") && !heads.iter().any(|h| h.meta.contains("SENTINEL")));
    // Run again: every issue now has an open item.
    let t = Script::new(vec![Ok(ok(&body))]);
    let mut out = Vec::new();
    plan::run(&a, &ctx, &mut store, &[], &t, &mut out).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("from-jql: 0 planned, 3 skipped"));
    // A fixed lane, and a flag the search sets is refused.
    assert!(plan::run(&args(&["plan", "--from-jql", "x", "--lane", "L"]), &ctx, &mut store, &[], &t, &mut Vec::new()).is_err());
    assert!(plan::run(&args(&["plan", "--from-jql", "x", "--lane-from", "fixed:"]), &ctx, &mut store, &[], &t, &mut Vec::new()).is_err());
}

#[test]
fn snapshot_all_refreshes_the_stale_ones_and_keeps_a_failed_one() {
    let mut store = MemoryStore::in_memory().unwrap();
    let old = [
        "ticket_status: Old",
        "ticket_assignee: unassigned",
        "ticket_summary: s",
        "ticket_fetched: 2020-01-01T00:00:00Z",
        "ticket_via: rest",
    ];
    let stale = item(&mut store, "P-1", &[&["ticket: P-1"][..], &old[..]].concat());
    let now = crate::memory::now();
    let fresh_line = format!("ticket_fetched: {now}");
    let fresh = item(&mut store, "P-2", &["ticket: P-2", "ticket_status: Fresh", &fresh_line]);
    let failing = item(&mut store, "P-3", &[&["ticket: P-3"][..], &old[..]].concat());
    let none = item(&mut store, "P-4", &[]);
    let missing = Response { status: 404, headers: vec![], body: br#"{"errorMessages":["Issue does not exist"]}"#.to_vec() };
    let t = Script::new(vec![Ok(ok(&issue("P-1", "Now done", "Closed", "Done"))), Ok(missing)]);
    let s = settings("https://snap.example.atlassian.net", true);
    let ctx = Ctx { settings: Some(&s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env: &env };
    let mut out = Vec::new();
    let code = snapshot::run(&args(&["--all", "--older-than", "1d"]), &ctx, &mut store, &t, &mut out).unwrap();
    let o = String::from_utf8(out).unwrap();
    assert_eq!(code, ExitCode::from(1), "a failed fetch is reported in the exit code: {o}");
    assert!(o.contains("snapshots: 1 refreshed, 1 failed, 1 fresh enough"), "{o}");
    assert!(o.contains(&format!("{failing}: P-3 not refreshed, its old snapshot stays")), "{o}");
    let seen: Vec<String> = t.seen.borrow().iter().map(|r| r.1.clone()).collect();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen[0].ends_with("/issue/P-1") && seen[1].ends_with("/issue/P-3"), "{seen:?}");
    let m = |k: &str, l: &str| store.head(k).unwrap().unwrap().meta.lines().find(|x| x.starts_with(&format!("{l}:"))).map(str::to_string);
    assert_eq!(m(&stale, "ticket_status").as_deref(), Some("ticket_status: Closed"));
    assert_eq!(m(&stale, "ticket_category").as_deref(), Some("ticket_category: Done"));
    assert!(m(&stale, "ticket_fetched").is_some_and(|f| !f.contains("2020")), "the new fetch time is recorded");
    assert_eq!(m(&failing, "ticket_status").as_deref(), Some("ticket_status: Old"));
    assert_eq!(m(&failing, "ticket_fetched").as_deref(), Some("ticket_fetched: 2020-01-01T00:00:00Z"));
    assert_eq!(m(&fresh, "ticket_status").as_deref(), Some("ticket_status: Fresh"));
    assert_eq!(m(&none, "ticket_status"), None);
    // Without --older-than every open ticket item is fetched again.
    let t = Script::new(vec![]);
    snapshot::run(&args(&["--all"]), &ctx, &mut store, &t, &mut Vec::new()).unwrap();
    assert_eq!(t.seen.borrow().len(), 3);
    assert!(snapshot::run(&args(&["--all", "--older-than", "soon"]), &ctx, &mut store, &t, &mut Vec::new()).is_err());
    assert_eq!(snapshot::parse_dur("90m"), Some(5_400));
    assert_eq!(snapshot::parse_dur("2d"), Some(172_800));
}

#[test]
fn a_ticket_condition_is_met_from_the_latest_snapshot() {
    let mut store = MemoryStore::in_memory().unwrap();
    item(
        &mut store,
        "P-1",
        &["ticket: P-1", "ticket_status: Ready For Dev", "ticket_category: To Do", "ticket_fetched: 2026-11-01T00:00:00Z"],
    );
    // A newer snapshot of the same ticket on another item wins.
    item(
        &mut store,
        "P-1b",
        &["ticket: P-1", "ticket_status: Dev In Progress", "ticket_category: In Progress", "ticket_fetched: 2026-11-02T00:00:00Z"],
    );
    item(&mut store, "P-2", &["ticket: P-2"]);
    let after: Vec<String> = [
        "ticket P-1 In Progress",
        "ticket p-1 dev in progress",
        "ticket P-1 Done",
        "ticket P-1 Ready For Dev",
        "ticket P-2 Done",
        "ticket P-9 Done",
        "P-1 PRs merged",
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    let mut p = queue::Planned { lane: "W".into(), kind: "builder".into(), title: "w".into(), after: after.clone(), ..Default::default() };
    queue::check_planned(&store, &mut p, &[]).unwrap();
    let w = queue::add_planned(&mut store, &p).unwrap();
    let marks = Marks::read(&store).unwrap();
    let st: Vec<DepState> = queue::dep_states(&store, &w, &after, &marks, &HashSet::new()).into_iter().map(|(_, s)| s).collect();
    assert_eq!(
        st,
        [DepState::Met, DepState::Met, DepState::Waiting, DepState::Waiting, DepState::Waiting, DepState::Untracked, DepState::Waiting]
    );
    assert_eq!(queue::ticket_condition("ticket ES1-1557 Done"), Some(("ES1-1557".into(), "Done".into())));
    assert_eq!(queue::ticket_condition("ticket ES1-1557"), None);
    assert_eq!(queue::ticket_condition("tickets ES1-1 Done"), None);
    // The scheduler sees the same: only the met ones leave the unmet list.
    let (_, items) = queue::items(&store, "2026-11-18T00:00:00Z").unwrap();
    let it = items.iter().find(|i| i.key == w).unwrap();
    assert_eq!(it.unmet.len(), 5, "{:?}", it.unmet);
    assert!(it.unmet.iter().any(|u| u == "ticket P-9 Done (no queue item tracks this ticket)"), "{:?}", it.unmet);
    // A key is letters, digits, `-` and `_`, at most 64 bytes.
    assert_eq!(queue::ticket_condition("ticket ES1/5 Done"), None);
    assert_eq!(queue::ticket_condition(&format!("ticket {} Done", "K".repeat(65))), None);
    assert_eq!(queue::ticket_condition(&format!("ticket {} Done", "k".repeat(64))), Some(("K".repeat(64), "Done".into())));
    assert_eq!(queue::ticket_condition("TICKET a_b-1 In Review"), Some(("A_B-1".into(), "In Review".into())));
}

fn ctx_of(s: &super::Settings) -> Ctx<'_> {
    Ctx { settings: Some(s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env: &env }
}

fn queue_cli(store: &mut MemoryStore, a: &[&str]) {
    queue::cli(&args(a), store, &crate::orchestrate::OrchSettings::default(), &[]).unwrap();
}

/// Review finding 1 and 4 (#318): the ticket only a done item tracks is
/// still fetched for a waiting condition, and two items on one ticket cost
/// one fetch.
#[test]
fn snapshot_all_moves_a_condition_on_a_ticket_only_a_done_item_tracks() {
    let mut store = MemoryStore::in_memory().unwrap();
    let a = item(
        &mut store,
        "ES1-5",
        &["ticket: ES1-5", "ticket_status: In Review", "ticket_category: In Progress", "ticket_fetched: 2026-11-01T00:00:00Z"],
    );
    queue_cli(&mut store, &["done", &a]);
    let after = vec!["ticket ES1-5 Done".to_string()];
    let mut p =
        queue::Planned { lane: "W".into(), kind: "builder".into(), title: "after QA".into(), after: after.clone(), ..Default::default() };
    queue::check_planned(&store, &mut p, &[]).unwrap();
    let b = queue::add_planned(&mut store, &p).unwrap();
    let c1 = item(&mut store, "ES1-7a", &["ticket: ES1-7"]);
    let c2 = item(&mut store, "ES1-7b", &["ticket: es1-7"]);
    let t = Script::new(vec![
        Ok(ok(&issue("ES1-7", "pasted v-SENTINEL here", "Ready", "To Do"))),
        Ok(ok(&issue("ES1-5", "Ship it", "Done", "Done"))),
    ]);
    let s = settings("https://snap.example.atlassian.net", true);
    let ctx = ctx_of(&s);
    let mut out = Vec::new();
    let code = snapshot::run(&args(&["--all"]), &ctx, &mut store, &t, &mut out).unwrap();
    let o = String::from_utf8(out).unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "{o}");
    let seen: Vec<String> = t.seen.borrow().iter().map(|r| r.1.clone()).collect();
    assert_eq!(seen.len(), 2, "one fetch per ticket: {seen:?}");
    assert!(seen[0].ends_with("/issue/ES1-7") && seen[1].ends_with("/issue/ES1-5"), "{seen:?}");
    assert!(o.contains("snapshots: 3 refreshed, 0 failed, 0 fresh enough"), "{o}");
    let meta = |k: &str| store.head(k).unwrap().unwrap().meta;
    assert_eq!(meta_value(&meta(&a), "ticket_status"), Some("Done"));
    for k in [&c1, &c2] {
        assert_eq!(meta_value(&meta(k), "ticket_status"), Some("Ready"));
        assert!(!meta(k).contains("SENTINEL"), "the summary is scrubbed: {}", meta(k));
    }
    let marks = Marks::read(&store).unwrap();
    let st: Vec<DepState> = queue::dep_states(&store, &b, &after, &marks, &HashSet::new()).into_iter().map(|(_, s)| s).collect();
    assert_eq!(st, [DepState::Met]);
    // Met now, so the done item's ticket is left alone.
    let t = Script::new(vec![Ok(ok(&issue("ES1-7", "s", "Ready", "To Do")))]);
    snapshot::run(&args(&["--all"]), &ctx, &mut store, &t, &mut Vec::new()).unwrap();
    assert_eq!(t.seen.borrow().len(), 1);
}

/// Review finding 10 (#318): a store write that fails is counted and
/// reported per item, and the rest still run.
#[test]
fn snapshot_all_reports_a_failed_write_and_carries_on() {
    let mut store = MemoryStore::in_memory().unwrap();
    let bad = item(&mut store, "Q-1", &["ticket: Q-1"]);
    let good = item(&mut store, "Q-2", &["ticket: Q-2"]);
    store
        .conn()
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER refuse BEFORE INSERT ON versions WHEN NEW.rid = (SELECT rid FROM records WHERE key = '{bad}') BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;"
        ))
        .unwrap();
    let t = Script::new(vec![Ok(ok(&issue("Q-1", "a", "Done", "Done"))), Ok(ok(&issue("Q-2", "b", "Done", "Done")))]);
    let s = settings("https://snap.example.atlassian.net", true);
    let mut out = Vec::new();
    let code = snapshot::run(&args(&["--all"]), &ctx_of(&s), &mut store, &t, &mut out).unwrap();
    let o = String::from_utf8(out).unwrap();
    assert_eq!(code, ExitCode::from(1), "{o}");
    assert!(o.contains(&format!("{bad}: Q-1 fetched but not written")) && o.contains("refused by the test"), "{o}");
    assert!(o.contains("snapshots: 1 refreshed, 1 failed, 0 fresh enough"), "{o}");
    assert_eq!(meta_value(&store.head(&good).unwrap().unwrap().meta, "ticket_status"), Some("Done"));
    assert_eq!(meta_value(&store.head(&bad).unwrap().unwrap().meta, "ticket_status"), None);
}

/// Review finding 9 (#318).
#[test]
fn snapshot_refuses_an_item_with_all_and_older_than_without_all() {
    let mut store = MemoryStore::in_memory().unwrap();
    let k = item(&mut store, "R-1", &["ticket: R-1"]);
    let s = settings("https://snap.example.atlassian.net", true);
    let t = Script::new(vec![]);
    let e = snapshot::run(&args(&[&k, "--all"]), &ctx_of(&s), &mut store, &t, &mut Vec::new()).unwrap_err();
    assert!(e.contains("not both"), "{e}");
    let e = snapshot::run(&args(&[&k, "--older-than", "1d"]), &ctx_of(&s), &mut store, &t, &mut Vec::new()).unwrap_err();
    assert!(e.contains("goes with --all"), "{e}");
    let e = snapshot::run(&args(&["--all", "--older-than"]), &ctx_of(&s), &mut store, &t, &mut Vec::new()).unwrap_err();
    assert!(e.contains("--older-than needs a duration"), "{e}");
    assert_eq!(snapshot::parse_dur("45s"), Some(45));
    assert!(t.seen.borrow().is_empty());
}

/// Review finding 3 (#318): the skip rule's other branches, a fixed lane,
/// `--limit` over pages, and `--limit 0`.
#[test]
fn from_jql_skips_started_items_and_plans_done_and_dropped_ones_again() {
    let mut store = MemoryStore::in_memory().unwrap();
    let started = item(&mut store, "ES1-4", &["ticket: ES1-4"]);
    {
        let mut w = store.writer().unwrap();
        w.retire(&started, "started").unwrap();
        w.commit().unwrap();
    }
    let finished = item(&mut store, "ES1-5", &["ticket: ES1-5"]);
    queue_cli(&mut store, &["done", &finished]);
    let dropped = item(&mut store, "ES1-6", &["ticket: ES1-6"]);
    queue_cli(&mut store, &["drop", &dropped]);
    let body = format!(
        r#"{{"issues":[{},{},{}],"isLast":true}}"#,
        issue("ES1-4", "started", "Dev In Progress", "In Progress"),
        issue("ES1-5", "done", "Done", "Done"),
        issue("ES1-6", "dropped", "To Do", "To Do"),
    );
    let t = Script::new(vec![Ok(ok(&body))]);
    let s = settings("https://plan.example.atlassian.net", true);
    let mut out = Vec::new();
    let a = args(&["plan", "--from-jql", "q", "--lane-from", "FIXED:Sprint 7"]);
    let code = plan::run(&a, &ctx_of(&s), &mut store, &[], &t, &mut out).unwrap();
    let o = String::from_utf8(out).unwrap();
    assert_eq!(code, ExitCode::SUCCESS, "{o}");
    assert!(o.contains("ES1-4: skipped") && o.contains("from-jql: 2 planned, 1 skipped"), "{o}");
    let made: Vec<_> = queue::waiting(&store).unwrap().into_iter().filter(|h| h.key != finished && h.key != dropped).collect();
    let got: Vec<_> = made.iter().map(|h| (h.scope.as_str(), meta_value(&h.meta, "ticket"))).collect();
    assert_eq!(got, [("Sprint 7", Some("ES1-5")), ("Sprint 7", Some("ES1-6"))]);
    assert!(!o.contains("may hold more"), "{o}");
    let e = plan::run(&args(&["plan", "--from-jql", "q", "--limit", "0"]), &ctx_of(&s), &mut store, &[], &t, &mut Vec::new()).unwrap_err();
    assert!(e.contains("1 or more"), "{e}");
    let e = plan::run(&args(&["plan", "--from-jql", "q", "--lane-from", "lane"]), &ctx_of(&s), &mut store, &[], &t, &mut Vec::new())
        .unwrap_err();
    assert!(e.contains("key, or fixed:LANE"), "{e}");
}

#[test]
fn from_jql_limit_counts_issues_across_pages_skipped_ones_too() {
    let mut store = MemoryStore::in_memory().unwrap();
    let page1 = format!(
        r#"{{"issues":[{},{}],"nextPageToken":"T2","isLast":false}}"#,
        issue("L-1", "a", "To Do", "To Do"),
        issue("L-2", "b", "To Do", "To Do")
    );
    // The site answers the second page's maxResults=1 with one issue.
    let page2a = format!(r#"{{"issues":[{}],"nextPageToken":"T3","isLast":false}}"#, issue("L-3", "c", "To Do", "To Do"));
    let page2 = format!(r#"{{"issues":[{},{}],"isLast":true}}"#, issue("L-3", "c", "To Do", "To Do"), issue("L-4", "d", "To Do", "To Do"));
    let s = settings("https://plan.example.atlassian.net", true);
    let t = Script::new(vec![Ok(ok(&page1)), Ok(ok(&page2a))]);
    let mut out = Vec::new();
    plan::run(&args(&["plan", "--from-jql", "q", "--limit", "3", "--lane-from", "Key"]), &ctx_of(&s), &mut store, &[], &t, &mut out)
        .unwrap();
    let o = String::from_utf8(out).unwrap();
    let seen: Vec<String> = t.seen.borrow().iter().map(|r| r.1.clone()).collect();
    assert_eq!(seen.len(), 2, "the second page is read: {seen:?}");
    assert!(seen[1].contains("maxResults=1") && seen[1].contains("nextPageToken=T2"), "{seen:?}");
    assert!(
        o.contains("from-jql: 3 planned, 0 skipped") && o.contains("may hold more past --limit 3 (skipped issues count toward it)"),
        "{o}"
    );
    assert!(queue::waiting(&store).unwrap().iter().all(|h| meta_value(&h.meta, "ticket") == Some(h.scope.as_str())), "lane from the key");
    // Skipped issues count toward the limit; all four fit under 10.
    let t = Script::new(vec![Ok(ok(&page1)), Ok(ok(&page2))]);
    let mut out = Vec::new();
    plan::run(&args(&["plan", "--from-jql", "q", "--limit", "10"]), &ctx_of(&s), &mut store, &[], &t, &mut out).unwrap();
    let o = String::from_utf8(out).unwrap();
    assert!(o.contains("from-jql: 1 planned, 3 skipped") && !o.contains("may hold more"), "{o}");
}

/// An issue without a key and an item the queue refuses are counted as
/// failed, and the exit code says so.
#[test]
fn from_jql_counts_an_issue_without_a_key_and_a_refused_item_as_failed() {
    let mut store = MemoryStore::in_memory().unwrap();
    let body = format!(r#"{{"issues":[{{"fields":{{}}}},{}],"isLast":true}}"#, issue("F-1", "a", "To Do", "To Do"));
    let s = settings("https://plan.example.atlassian.net", true);
    let t = Script::new(vec![Ok(ok(&body))]);
    let mut out = Vec::new();
    let code = plan::run(&args(&["plan", "--from-jql", "q", "--kind", "bogus"]), &ctx_of(&s), &mut store, &[], &t, &mut out).unwrap();
    let o = String::from_utf8(out).unwrap();
    assert_eq!(code, ExitCode::from(1), "{o}");
    assert!(o.contains("an issue without a key: not planned") && o.contains("F-1: not planned: unknown kind"), "{o}");
    assert!(o.contains("from-jql: 0 planned, 0 skipped (an open item tracks them), 2 failed"), "{o}");
    assert!(queue::waiting(&store).unwrap().is_empty());
}

/// Review finding 8 (#318): every stored text field is scrubbed, on the
/// single-item path too.
#[test]
fn a_token_pasted_into_any_ticket_field_is_not_stored() {
    let mut store = MemoryStore::in_memory().unwrap();
    let k = item(&mut store, "S-1", &["ticket: S-1"]);
    let x = "v-SENTINEL";
    let body = format!(
        r#"{{"key":"S-1","fields":{{"summary":"s {x}","status":{{"name":"st {x}","statusCategory":{{"name":"cat {x}"}}}},"assignee":{{"displayName":"who {x}"}}}}}}"#
    );
    let s = settings("https://snap.example.atlassian.net", true);
    let t = Script::new(vec![Ok(ok(&body))]);
    let mut out = Vec::new();
    snapshot::run(&args(&[&k]), &ctx_of(&s), &mut store, &t, &mut out).unwrap();
    let meta = store.head(&k).unwrap().unwrap().meta;
    for f in ["ticket_status", "ticket_category", "ticket_assignee", "ticket_summary"] {
        let v = meta_value(&meta, f).unwrap_or_default();
        assert!(!v.is_empty() && !v.contains("SENTINEL"), "{f}: {v}");
    }
    assert!(!String::from_utf8(out).unwrap().contains("SENTINEL"));
}

/// The router's question: only `plan ... --from-jql` goes to the tracker.
#[test]
fn only_plan_with_from_jql_is_a_plan_from_jql() {
    assert!(plan::wants(&args(&["plan", "--from-jql", "q"])));
    assert!(!plan::wants(&args(&["plan", "--lane", "L", "--title", "T"])));
    assert!(!plan::wants(&args(&["list", "--from-jql", "q"])));
    assert!(!plan::wants(&[]));
}

/// Review r2 N1 (#318): an item after `--all` is refused too; the value of
/// `--older-than` is not an item.
#[test]
fn snapshot_refuses_an_item_after_all() {
    let mut store = MemoryStore::in_memory().unwrap();
    let k = item(&mut store, "R-1", &["ticket: R-1"]);
    let s = settings("https://snap.example.atlassian.net", true);
    let t = Script::new(vec![]);
    for a in [vec!["--all", k.as_str()], vec!["--all", "--older-than", "1d", k.as_str()], vec!["--older-than", "1d", "--all", k.as_str()]] {
        let e = snapshot::run(&args(&a), &ctx_of(&s), &mut store, &t, &mut Vec::new()).unwrap_err();
        assert!(e.contains("not both") && e.contains(&k), "{a:?}: {e}");
    }
    assert!(t.seen.borrow().is_empty());
}
