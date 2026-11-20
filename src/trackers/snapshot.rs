//! Ticket snapshots on queue items (journal:queue:0046): an item with a
//! `ticket: KEY` meta line carries its ticket's status, assignee and
//! summary, and when they were fetched.
//!
//! With `project_tracking.atlassian` configured and its variables set,
//! `atlas track snapshot ITEM` reads the issue directly and writes the
//! snapshot (`ticket_via: rest`). Without them, it prints the fallback
//! (fetch the ticket through the Atlassian MCP connector and record it with
//! `--status`, `--assignee` and `--summary`, `ticket_via: mcp`), and the
//! finish notice's queue lines say the same for an item with no snapshot
//! ([`hint_lines`]). Hooks never make a network call.

use std::io::Write;
use std::process::ExitCode;

use super::api::Client;
use super::http::{Curl, Transport};
use super::{Credential, Ctx, Settings};
use crate::json::Value;
use crate::memory::{Draft, Head, MemoryStore};
use crate::queue::meta_value;

/// The meta lines a snapshot writes.
pub const LINES: &[&str] = &["ticket_status", "ticket_category", "ticket_assignee", "ticket_summary", "ticket_fetched", "ticket_via"];

/// A ticket's snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snap {
    /// Its status name.
    pub status: String,
    /// Its status category (`To Do`, `In Progress`, `Done`), or empty when
    /// not known (a snapshot recorded by hand without `--category`).
    pub category: String,
    /// Its assignee's display name, or `unassigned`.
    pub assignee: String,
    /// Its summary, one line.
    pub summary: String,
    /// When it was fetched (UTC).
    pub fetched: String,
    /// `rest` or `mcp`.
    pub via: String,
}

/// The item's ticket key (`ticket:` meta line, first word).
pub fn ticket_of(meta: &str) -> Option<String> {
    meta_value(meta, "ticket")
        .and_then(|v| v.split_whitespace().next())
        .map(|t| t.trim_matches(['`', '"', ',']).to_string())
        .filter(|t| !t.is_empty())
}

/// One line, at most 200 characters.
fn one_line(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(200).collect()
}

/// `meta` with its snapshot lines replaced by `s`.
pub fn with_snapshot(meta: &str, s: &Snap) -> String {
    let mut out: Vec<String> = meta
        .lines()
        .filter(|l| !LINES.iter().any(|k| l.split_once(':').is_some_and(|(x, _)| x.trim() == *k)))
        .map(str::to_string)
        .collect();
    out.push(format!("ticket_status: {}", one_line(&s.status)));
    if !s.category.trim().is_empty() {
        out.push(format!("ticket_category: {}", one_line(&s.category)));
    }
    out.push(format!("ticket_assignee: {}", one_line(&s.assignee)));
    out.push(format!("ticket_summary: {}", one_line(&s.summary)));
    out.push(format!("ticket_fetched: {}", s.fetched));
    out.push(format!("ticket_via: {}", s.via));
    out.join("\n")
}

/// The snapshot of an issue response.
pub fn snap_of(v: &Value, fetched: &str) -> Snap {
    let f = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
    let assignee = f("/fields/assignee/displayName");
    Snap {
        status: f("/fields/status/name"),
        category: f("/fields/status/statusCategory/name"),
        assignee: if assignee.is_empty() { "unassigned".into() } else { assignee },
        summary: f("/fields/summary"),
        fetched: fetched.to_string(),
        via: "rest".into(),
    }
}

/// `s` with its text fields scrubbed of the credential, as the read paths
/// scrub what they print: a token pasted into a ticket is not stored.
pub fn scrubbed(c: &Client<'_>, s: Snap) -> Snap {
    Snap { status: c.scrub(&s.status), category: c.scrub(&s.category), assignee: c.scrub(&s.assignee), summary: c.scrub(&s.summary), ..s }
}

/// The fallback for an item without a direct read: the agent fetches the
/// ticket through the MCP connector and records it.
pub fn fallback(item: &str, ticket: &str) -> String {
    format!("fetch {ticket} with the Atlassian MCP connector (getJiraIssue) and record it: atlas track snapshot {item} --status S --assignee A --summary T")
}

/// The finish notice's lines for waiting items with a ticket and no
/// snapshot (at most three). `direct`: a token is configured and present.
pub fn hint_lines(heads: &[Head], direct: bool) -> String {
    let mut o = String::new();
    for h in heads.iter().filter(|h| meta_value(&h.meta, "ticket_fetched").is_none()).take(3) {
        let Some(t) = ticket_of(&h.meta) else { continue };
        let what = if direct { format!("run atlas track snapshot {}", h.key) } else { fallback(&h.key, &t) };
        o.push_str(&format!("atlas: {} tracks {t}, no snapshot yet: {what}\n", h.key));
    }
    o
}

/// Writes a snapshot onto an item as a new version.
///
/// # Errors
///
/// A store error.
pub fn write(store: &mut MemoryStore, h: &Head, s: &Snap) -> crate::Result<()> {
    let d = Draft {
        title: h.title.clone(),
        meta: with_snapshot(&h.meta, s),
        body: h.body.clone(),
        reason: format!("ticket snapshot ({})", s.via),
        author: "atlas".into(),
        client: "atlas-track".into(),
        provenance: None,
        time: None,
    };
    let mut w = store.writer()?;
    w.revise(&h.key, Some(&h.sha), &d)?;
    w.commit()
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// `atlas track snapshot ITEM [...]`.
///
/// # Errors
///
/// A message for the user.
pub fn cli(args: &[String], ctx: &Ctx<'_>) -> Result<ExitCode, String> {
    let mut store = MemoryStore::open(ctx.store).map_err(|e| e.to_string())?;
    let curl = Curl::default();
    run(args, ctx, &mut store, &curl, &mut std::io::stdout().lock())
}

/// [`cli`] on a store and a transport.
///
/// # Errors
///
/// As [`cli`].
pub fn run(args: &[String], ctx: &Ctx<'_>, store: &mut MemoryStore, t: &dyn Transport, out: &mut dyn Write) -> Result<ExitCode, String> {
    let named = args.first().filter(|a| !a.starts_with("--"));
    if args.iter().any(|a| a == "--all") {
        // Any word that is not a flag or the value of `--older-than` is an item.
        let stray = args.iter().enumerate().find(|&(i, a)| !a.starts_with("--") && (i == 0 || args[i - 1] != "--older-than"));
        if let Some((_, item)) = stray {
            return Err(format!("atlas track snapshot: give ITEM or --all, not both ({item})\n{USAGE}"));
        }
        return run_all(args, ctx, store, t, out, &crate::memory::now());
    }
    if args.iter().any(|a| a == "--older-than") {
        return Err(format!("atlas track snapshot: --older-than goes with --all\n{USAGE}"));
    }
    let item = named.ok_or(USAGE)?;
    let h = store.head(item).map_err(|e| e.to_string())?.ok_or_else(|| format!("atlas track snapshot: no record {item}"))?;
    let ticket = ticket_of(&h.meta).ok_or_else(|| format!("atlas track snapshot: {item} has no `ticket:` meta line"))?;
    let now = crate::memory::now();
    let manual = (flag(args, "--status"), flag(args, "--assignee"), flag(args, "--summary"));
    let snap = if manual.0.is_some() || manual.1.is_some() || manual.2.is_some() {
        let (Some(status), Some(assignee), Some(summary)) = manual else {
            return Err("atlas track snapshot: give --status, --assignee and --summary together".into());
        };
        Snap { status, category: flag(args, "--category").unwrap_or_default(), assignee, summary, fetched: now, via: "mcp".into() }
    } else {
        let direct: Option<&Settings> = ctx.settings.filter(|s| s.env_present(ctx.env));
        let Some(s) = direct else {
            writeln!(out, "no direct read configured (project_tracking.atlassian and its variables): {}", fallback(item, &ticket))
                .map_err(|e| e.to_string())?;
            return Ok(ExitCode::from(2));
        };
        let cred = Credential::resolve(&s.auth, ctx.env).map_err(|m| format!("atlas track snapshot: {m}"))?;
        let sleep = |d: std::time::Duration| std::thread::sleep(d);
        let c = Client::new(s, cred, t, &sleep);
        let got = c.issue(&ticket).map_err(|e| format!("atlas track snapshot: {e}"))?;
        scrubbed(&c, snap_of(&got.value, &now))
    };
    write(store, &h, &snap).map_err(|e| e.to_string())?;
    writeln!(out, "{item}: {ticket} {} ({}), fetched {} via {}", one_line(&snap.status), one_line(&snap.assignee), snap.fetched, snap.via)
        .map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// The usage of `atlas track snapshot`.
pub const USAGE: &str = "usage: atlas track snapshot ITEM [--status S --assignee A --summary T [--category C]] | --all [--older-than DUR]";

/// A duration such as `45s`, `30m`, `6h`, `2d` or `1w`, in seconds.
pub fn parse_dur(s: &str) -> Option<i64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = n.parse().ok()?;
    let per = match unit.to_ascii_lowercase().as_str() {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    };
    n.checked_mul(per)
}

/// `atlas track snapshot --all [--older-than DUR]`: every ticket of
/// [`due_groups`] whose snapshot is older than DUR (default: every one, and
/// an item with none) is fetched again, once per ticket. A failed fetch or
/// write keeps the old snapshot and is reported; the exit code is then 1.
fn run_all(
    args: &[String],
    ctx: &Ctx<'_>,
    store: &mut MemoryStore,
    t: &dyn Transport,
    out: &mut dyn Write,
    now: &str,
) -> Result<ExitCode, String> {
    let w = |out: &mut dyn Write, s: String| writeln!(out, "{s}").map_err(|e| e.to_string());
    let older = match flag(args, "--older-than") {
        Some(d) => Some(parse_dur(&d).ok_or_else(|| format!("--older-than {d:?}: a duration such as 30m, 6h or 2d"))?),
        None if args.iter().any(|a| a == "--older-than") => return Err(format!("--older-than needs a duration\n{USAGE}")),
        None => None,
    };
    let Some(s) = ctx.settings.filter(|s| s.env_present(ctx.env)) else {
        w(out, "no direct read configured (project_tracking.atlassian and its variables): snapshot each item with the Atlassian MCP connector and `atlas track snapshot ITEM --status S --assignee A --summary T`".into())?;
        return Ok(ExitCode::from(2));
    };
    let groups = due_groups(store).map_err(|e| e.to_string())?;
    let now_s = crate::checkpoint::epoch_secs(now);
    let stale = |h: &Head| match (older, meta_value(&h.meta, "ticket_fetched").and_then(crate::checkpoint::epoch_secs), now_s) {
        (Some(d), Some(f), Some(n)) => n - f >= d,
        _ => true,
    };
    // A ticket is fetched when any item it is written to is stale, and the
    // one response is written to each of them.
    let (due, fresh): (Vec<_>, Vec<_>) = groups.into_iter().partition(|(_, hs)| hs.iter().any(stale));
    let cred = Credential::resolve(&s.auth, ctx.env).map_err(|m| format!("atlas track snapshot: {m}"))?;
    let sleep = |d: std::time::Duration| std::thread::sleep(d);
    let c = Client::new(s, cred, t, &sleep);
    let (mut made, mut failed) = (0usize, 0usize);
    for (ticket, items) in &due {
        let got = match c.issue(ticket) {
            Ok(got) => got,
            Err(e) => {
                for h in items {
                    w(out, format!("{}: {ticket} not refreshed, its old snapshot stays: {e}", h.key))?;
                }
                failed += items.len();
                continue;
            }
        };
        let snap = scrubbed(&c, snap_of(&got.value, now));
        for h in items {
            let wrote = store
                .head(&h.key)
                .map_err(|e| e.to_string())
                .and_then(|full| full.ok_or_else(|| "no such record".to_string()))
                .and_then(|full| write(store, &full, &snap).map_err(|e| e.to_string()));
            match wrote {
                Ok(()) => {
                    let (st, who) = (one_line(&snap.status), one_line(&snap.assignee));
                    w(out, format!("{}: {ticket} {st} ({who}), fetched {} via rest", h.key, snap.fetched))?;
                    made += 1;
                }
                Err(e) => {
                    w(out, format!("{}: {ticket} fetched but not written, its old snapshot stays: {e}", h.key))?;
                    failed += 1;
                }
            }
        }
    }
    let fresh: usize = fresh.iter().map(|(_, hs)| hs.len()).sum();
    w(out, format!("snapshots: {made} refreshed, {failed} failed, {fresh} fresh enough"))?;
    Ok(if failed > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

/// The tickets `--all` may fetch, each with the items its snapshot is
/// written to, oldest item first: every open ticket item, and for a ticket
/// that an unmet `after: ticket KEY STATUS` of a waiting item names and no
/// open item tracks, the newest item that tracks it, whatever its status
/// (else that condition could never move). Keys upper-cased.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn due_groups(store: &MemoryStore) -> crate::Result<Vec<(String, Vec<Head>)>> {
    use crate::queue::{open_ticket_items, qmeta, ticket_condition, ticket_items, waiting, Marks};
    let marks = Marks::read(store)?;
    let mut groups: Vec<(String, Vec<Head>)> = Vec::new();
    for h in open_ticket_items(store, &marks.done)? {
        let Some(t) = ticket_of(&h.meta).map(|t| t.to_ascii_uppercase()) else { continue };
        match groups.iter_mut().find(|g| g.0 == t) {
            Some(g) => g.1.push(h),
            None => groups.push((t, vec![h])),
        }
    }
    let mut wanted = std::collections::BTreeSet::new();
    for h in waiting(store)? {
        for cond in qmeta(&h.meta).after {
            let Some((key, status)) = ticket_condition(&cond) else { continue };
            let met = marks.is_met(&h.key, &cond) || marks.tickets.get(&key).and_then(Option::as_ref).is_some_and(|s| s.matches(&status));
            if !met && !groups.iter().any(|g| g.0 == key) {
                wanted.insert(key);
            }
        }
    }
    if !wanted.is_empty() {
        let all = ticket_items(store)?;
        for key in wanted {
            if let Some(h) = all.iter().rev().find(|h| ticket_of(&h.meta).is_some_and(|t| t.eq_ignore_ascii_case(&key))) {
                groups.push((key, vec![h.clone()]));
            }
        }
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trackers::api::tests::{ok, settings, Script};

    fn item(store: &mut MemoryStore, meta: &str) -> String {
        let p = crate::queue::Planned {
            lane: "ES1-1981".into(),
            kind: "builder".into(),
            title: "GeoLocationDal".into(),
            scope: "do it".into(),
            extra: meta.lines().map(str::to_string).collect(),
            ..Default::default()
        };
        crate::queue::add_planned(store, &p).unwrap()
    }

    fn env(n: &str) -> Option<String> {
        matches!(n, "E" | "T").then(|| "v-SENTINEL".to_string())
    }

    #[test]
    fn a_snapshot_is_fetched_directly_and_written_to_the_item() {
        let mut store = MemoryStore::in_memory().unwrap();
        let key = item(&mut store, "ticket: PROJ-123");
        let s = settings("https://snap.example.atlassian.net", true);
        let fx =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/atlassian/jira-issue.json")).unwrap();
        let t = Script::new(vec![Ok(ok(&fx))]);
        let ctx =
            Ctx { settings: Some(&s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env: &env };
        let mut out = Vec::new();
        let code = run(std::slice::from_ref(&key), &ctx, &mut store, &t, &mut out).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(t.seen.borrow()[0].1.ends_with("/rest/api/3/issue/PROJ-123"));
        let h = store.head(&key).unwrap().unwrap();
        assert_eq!(meta_value(&h.meta, "ticket_status"), Some("In Progress"));
        assert_eq!(meta_value(&h.meta, "ticket_assignee"), Some("Avery Example"));
        assert_eq!(meta_value(&h.meta, "ticket_summary"), Some("Checkout fails when the cart holds more than fifty items"));
        assert_eq!(meta_value(&h.meta, "ticket_via"), Some("rest"));
        assert!(meta_value(&h.meta, "ticket_fetched").is_some_and(|f| f.starts_with("20")));
        assert!(
            meta_value(&h.meta, "planned").is_some() && meta_value(&h.meta, "ticket") == Some("PROJ-123"),
            "the item's own lines stay: {}",
            h.meta
        );
        assert!(!h.meta.contains("SENTINEL"));
        assert_eq!(hint_lines(&[h], true), "", "a snapshot item needs no hint");
    }

    /// Review findings 3 and 12 (#318): a snapshot exactly DUR old is due,
    /// one second younger is not; units in any case.
    #[test]
    fn older_than_is_due_at_exactly_dur_and_takes_any_case() {
        let mut store = MemoryStore::in_memory().unwrap();
        let at = item(&mut store, "ticket: B-1\nticket_status: Old\nticket_fetched: 2026-11-17T12:00:00Z");
        let young = item(&mut store, "ticket: B-2\nticket_status: Old\nticket_fetched: 2026-11-17T12:00:01Z");
        let s = settings("https://snap.example.atlassian.net", true);
        let ctx =
            Ctx { settings: Some(&s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env: &env };
        let body = r#"{"key":"B-1","fields":{"summary":"s","status":{"name":"New","statusCategory":{"name":"To Do"}}}}"#;
        let t = Script::new(vec![Ok(ok(body))]);
        let a: Vec<String> = ["--all", "--older-than", "1D"].iter().map(|x| x.to_string()).collect();
        let mut out = Vec::new();
        run_all(&a, &ctx, &mut store, &t, &mut out, "2026-11-18T12:00:00Z").unwrap();
        let o = String::from_utf8(out).unwrap();
        assert!(o.contains("snapshots: 1 refreshed, 0 failed, 1 fresh enough"), "{o}");
        assert_eq!(t.seen.borrow().len(), 1);
        assert_eq!(meta_value(&store.head(&at).unwrap().unwrap().meta, "ticket_status"), Some("New"));
        assert_eq!(meta_value(&store.head(&young).unwrap().unwrap().meta, "ticket_status"), Some("Old"));
        assert_eq!(parse_dur("6H"), Some(21_600));
        assert_eq!(parse_dur("1W"), Some(604_800));
    }

    #[test]
    fn without_a_token_the_agent_is_told_to_use_the_mcp_connector() {
        let mut store = MemoryStore::in_memory().unwrap();
        let key = item(&mut store, "ticket: PROJ-9");
        let none = |_: &str| None;
        let ctx = Ctx { settings: None, db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env: &none };
        let t = Script::new(vec![]);
        let mut out = Vec::new();
        let code = run(std::slice::from_ref(&key), &ctx, &mut store, &t, &mut out).unwrap();
        assert_eq!(code, ExitCode::from(2));
        let o = String::from_utf8(out).unwrap();
        assert!(
            o.contains("fetch PROJ-9 with the Atlassian MCP connector (getJiraIssue)")
                && o.contains(&format!("atlas track snapshot {key} --status S")),
            "{o}"
        );
        assert!(t.seen.borrow().is_empty(), "no request without a token");
        let h = store.head(&key).unwrap().unwrap();
        let hint = hint_lines(std::slice::from_ref(&h), false);
        assert!(hint.contains("tracks PROJ-9, no snapshot yet: fetch PROJ-9 with the Atlassian MCP connector"), "{hint}");
        assert!(hint_lines(std::slice::from_ref(&h), true).contains("run atlas track snapshot"));
        // The agent records what it fetched.
        let args: Vec<String> = [key.as_str(), "--status", "Done", "--assignee", "Robin", "--summary", "Line one\nline two"]
            .iter()
            .map(|x| x.to_string())
            .collect();
        let mut out = Vec::new();
        run(&args, &ctx, &mut store, &t, &mut out).unwrap();
        let h = store.head(&key).unwrap().unwrap();
        assert_eq!(
            (meta_value(&h.meta, "ticket_status"), meta_value(&h.meta, "ticket_summary"), meta_value(&h.meta, "ticket_via")),
            (Some("Done"), Some("Line one line two"), Some("mcp"))
        );
    }
}
