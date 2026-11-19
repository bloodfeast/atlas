//! `atlas queue plan --from-jql QUERY` (#305): a JQL search becomes queue
//! items in one command. One item per issue: its lane and `ticket:` line
//! the issue key (or one fixed lane), its title the summary, and its
//! snapshot taken from the same search response. An issue that already
//! has an open item is skipped. Read-only on the tracker: one search.

use std::collections::HashSet;
use std::io::Write;
use std::process::ExitCode;

use super::api::Client;
use super::http::{Curl, Transport};
use super::snapshot::{scrubbed, snap_of, ticket_of, with_snapshot};
use super::{Credential, Ctx};
use crate::json::Value;
use crate::memory::MemoryStore;
use crate::queue::{add_planned, check_planned, open_ticket_items, split_top, Marks, Planned};

/// The usage of `atlas queue plan --from-jql`.
pub const USAGE: &str = "usage: atlas queue plan --from-jql QUERY [--kind K] [--priority P] [--order-from N] [--lane-from key|fixed:LANE] [--limit N] [--uses LIST] [--after LIST] [--scope TEXT]";

/// Where each item's lane comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LaneFrom {
    /// The issue key.
    Key,
    /// One lane for every item.
    Fixed(String),
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// Whether `args` (`plan ...`) ask for a plan from JQL.
pub fn wants(args: &[String]) -> bool {
    args.first().is_some_and(|w| w == "plan") && args.iter().any(|a| a == "--from-jql")
}

/// `atlas queue plan --from-jql ...` with the real transport.
///
/// # Errors
///
/// A message for the user.
pub fn cli(args: &[String], ctx: &Ctx<'_>, served: &[String]) -> Result<ExitCode, String> {
    let mut store = MemoryStore::open(ctx.store).map_err(|e| e.to_string())?;
    let curl = Curl::default();
    run(args, ctx, &mut store, served, &curl, &mut std::io::stdout().lock())
}

/// [`cli`] on a store and a transport.
///
/// # Errors
///
/// A message for the user: a bad flag, or the search failing.
pub fn run(
    args: &[String],
    ctx: &Ctx<'_>,
    store: &mut MemoryStore,
    served: &[String],
    t: &dyn Transport,
    out: &mut dyn Write,
) -> Result<ExitCode, String> {
    let w = |out: &mut dyn Write, s: String| writeln!(out, "{s}").map_err(|e| e.to_string());
    let q = flag(args, "--from-jql").filter(|q| !q.trim().is_empty()).ok_or_else(|| format!("--from-jql needs a query\n{USAGE}"))?;
    for f in ["--lane", "--title", "--ticket", "--order"] {
        if args.iter().any(|a| a == f) {
            return Err(format!("--from-jql sets {f} for each issue; leave it out\n{USAGE}"));
        }
    }
    let kind = flag(args, "--kind").unwrap_or_else(|| "builder".into()).to_ascii_lowercase();
    let priority = match flag(args, "--priority") {
        Some(p) => Some(crate::lanes::Priority::parse(&p).ok_or_else(|| format!("unknown priority {p:?}: urgent, normal or background"))?),
        None => None,
    };
    let order_from =
        flag(args, "--order-from").map(|n| n.parse::<u64>().map_err(|_| format!("--order-from {n:?}: a whole number"))).transpose()?;
    let lane_from = match flag(args, "--lane-from").as_deref() {
        None => LaneFrom::Key,
        Some(f) if f.eq_ignore_ascii_case("key") => LaneFrom::Key,
        Some(f) => match f.get(..6).filter(|p| p.eq_ignore_ascii_case("fixed:")).map(|_| f[6..].trim()) {
            Some(l) if !l.is_empty() => LaneFrom::Fixed(l.to_string()),
            _ => return Err(format!("--lane-from {f:?}: key, or fixed:LANE\n{USAGE}")),
        },
    };
    let limit = flag(args, "--limit")
        .map(|l| l.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| format!("--limit {l:?}: a number, 1 or more")))
        .transpose()?
        .unwrap_or(50);
    let list = |n: &str| flag(args, n).map(|v| split_top(&v)).unwrap_or_default();
    let (uses, after, scope) = (list("--uses"), list("--after"), flag(args, "--scope").unwrap_or_default());
    let Some(s) = ctx.settings.filter(|s| s.env_present(ctx.env)) else {
        w(out, "no direct read configured (project_tracking.atlassian and its variables): run the search with the Atlassian MCP connector and plan each issue with `atlas queue plan --lane KEY --title SUMMARY --ticket KEY`".into())?;
        return Ok(ExitCode::from(2));
    };
    let cred = Credential::resolve(&s.auth, ctx.env).map_err(|m| format!("atlas queue plan: {m}"))?;
    let sleep = |d: std::time::Duration| std::thread::sleep(d);
    let c = Client::new(s, cred, t, &sleep);
    let got = c.jql(&q, limit).map_err(|e| format!("atlas queue plan --from-jql: {e}"))?;
    let issues: &[Value] = got.value.get("issues").and_then(Value::as_array).map_or(&[], Vec::as_slice);
    let done = Marks::read(store).map_err(|e| e.to_string())?.done;
    let mut open: HashSet<String> = open_ticket_items(store, &done)
        .map_err(|e| e.to_string())?
        .iter()
        .filter_map(|h| ticket_of(&h.meta))
        .map(|k| k.to_ascii_uppercase())
        .collect();
    let now = crate::memory::now();
    let (mut made, mut skipped, mut failed) = (0u64, 0usize, 0usize);
    for issue in issues {
        let Some(key) = issue.get("key").and_then(Value::as_str).map(str::to_string) else {
            w(out, "an issue without a key: not planned".into())?;
            failed += 1;
            continue;
        };
        if open.contains(&key.to_ascii_uppercase()) {
            w(out, format!("{key}: skipped, an open item tracks it"))?;
            skipped += 1;
            continue;
        }
        let snap = scrubbed(&c, snap_of(issue, &now));
        let title = if snap.summary.trim().is_empty() { key.clone() } else { snap.summary.clone() };
        let order = match order_from {
            Some(n) => Some(n.saturating_add(made).to_string().parse().map_err(|()| "--order-from: an order out of range".to_string())?),
            None => None,
        };
        let mut extra = vec![format!("ticket: {key}")];
        extra.extend(with_snapshot("", &snap).lines().filter(|l| !l.is_empty()).map(str::to_string));
        let lane = match &lane_from {
            LaneFrom::Key => key.clone(),
            LaneFrom::Fixed(l) => l.clone(),
        };
        let mut p = Planned {
            lane,
            kind: kind.clone(),
            priority,
            uses: uses.clone(),
            after: after.clone(),
            order,
            title,
            scope: scope.clone(),
            extra,
        };
        let filed = check_planned(store, &mut p, served).and_then(|()| add_planned(store, &p).map_err(|e| e.to_string()));
        match filed {
            Ok(item) => {
                w(out, format!("planned {item} for {key} ({})", snap.status))?;
                open.insert(key.to_ascii_uppercase());
                made += 1;
            }
            Err(e) => {
                w(out, format!("{key}: not planned: {e}"))?;
                failed += 1;
            }
        }
    }
    let more = if issues.len() >= limit.min(super::api::MAX_ITEMS) {
        format!("; the search may hold more past --limit {limit} (skipped issues count toward it)")
    } else {
        String::new()
    };
    w(out, format!("from-jql: {made} planned, {skipped} skipped (an open item tracks them), {failed} failed{more}"))?;
    Ok(if failed > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS })
}
