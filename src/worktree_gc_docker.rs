//! The Docker part of worktree gc ([`crate::worktree_gc`]): atlas removes
//! only what `cargo xtask linux-test` made for a checkout
//! (`atlas-linux-target-<hash>` volumes and `atlas-linux-test-<hash>`
//! containers, the hash of the checkout's path), and only when that checkout
//! is gone and no running container uses them. Every other container and
//! volume is reported, never touched, with the activity Docker can verify.
//! Docker missing or not running is a quiet skip.

use std::collections::{BTreeMap, BTreeSet};

use crate::worktree_ledger::path_key;

/// The prefix of a checkout's build volume.
pub const TARGET_PREFIX: &str = "atlas-linux-target-";

/// The prefix of a checkout's container.
pub const CONTAINER_PREFIX: &str = "atlas-linux-test-";

/// The note every Docker report ends with.
pub const DISK_NOTE: &str =
    "note: Docker Desktop's disk image does not shrink by itself when volumes go; compacting it is your step (Docker Desktop: Troubleshoot, Clean / Purge data, or compact the WSL vhdx)";

/// The hash `cargo xtask linux-test` names a checkout's volume and container
/// by: the first 12 hex digits of the SHA-256 of its path, which on Windows
/// is taken with `/` separators and lowercased first.
pub fn checkout_hash(path: &str, windows: bool) -> String {
    let p = if windows { path.replace('\\', "/").to_lowercase() } else { path.to_string() };
    crate::sha256::hex(p.as_bytes())[..12].to_string()
}

/// The hashes a path may have been taken under: as given and resolved, each
/// as a [`path_key`] (git's spelling and the canonical one agree there).
pub fn hashes_of(path: &str) -> BTreeSet<String> {
    let w = cfg!(windows);
    let mut v = BTreeSet::new();
    v.insert(checkout_hash(&path_key(path), w));
    v.insert(checkout_hash(&crate::worktree_gc::resolved(std::path::Path::new(path)), w));
    v
}

/// What runs docker: the real CLI or a test's fake.
pub trait Docker {
    /// `docker args`: its standard output, or why it failed.
    ///
    /// # Errors
    ///
    /// Docker missing, not running, or the command failing.
    fn run(&self, args: &[&str]) -> Result<String, String>;
}

/// The `docker` on the PATH, each call limited to `timeout`.
pub struct Cli {
    /// One call's limit.
    pub timeout: std::time::Duration,
}

impl Docker for Cli {
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let r = crate::worktree_gc::call("docker", None, args, self.timeout)?;
        if r.code == Some(0) {
            Ok(r.stdout)
        } else {
            Err(format!("docker {}: {}", args.join(" "), r.stderr.trim()))
        }
    }
}

/// One container, as `docker inspect` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// Its name, without the leading `/`.
    pub name: String,
    /// `State.Status`: created, running, paused, restarting, removing,
    /// exited or dead.
    pub state: String,
    /// `State.FinishedAt` (Docker's zero time when it never ran).
    pub finished_at: String,
    /// The named volumes it mounts.
    pub volumes: Vec<String>,
}

impl Container {
    /// Running, or about to: never touched, and its volumes neither.
    pub fn live(&self) -> bool {
        matches!(self.state.as_str(), "running" | "paused" | "restarting" | "removing")
    }
}

/// The `docker inspect` format [`parse_inspect`] reads.
pub const INSPECT_FORMAT: &str =
    "{{.Name}}\t{{.State.Status}}\t{{.State.FinishedAt}}\t{{range .Mounts}}{{if .Name}}{{.Name}},{{end}}{{end}}";

/// Containers from `docker inspect --format INSPECT_FORMAT` output.
pub fn parse_inspect(text: &str) -> Vec<Container> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.trim_end_matches('\r').split('\t');
            let name = f.next()?.trim_start_matches('/').to_string();
            if name.is_empty() {
                return None;
            }
            let state = f.next()?.to_string();
            let finished_at = f.next().unwrap_or("").to_string();
            let volumes = f.next().unwrap_or("").split(',').filter(|v| !v.is_empty()).map(str::to_string).collect();
            Some(Container { name, state, finished_at, volumes })
        })
        .collect()
}

/// What Docker holds: containers and volume names. `None`: Docker is
/// missing or not running (a quiet skip), or it could not be read.
pub fn read(d: &dyn Docker) -> Option<(Vec<Container>, Vec<String>)> {
    let ids = d.run(&["ps", "-a", "-q", "--no-trunc"]).ok()?;
    let ids: Vec<&str> = ids.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let containers = if ids.is_empty() {
        Vec::new()
    } else {
        let mut args = vec!["inspect", "--format", INSPECT_FORMAT];
        args.extend(ids.iter().copied());
        parse_inspect(&d.run(&args).ok()?)
    };
    let volumes = d.run(&["volume", "ls", "-q"]).ok()?.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    Some((containers, volumes))
}

/// "5 weeks ago" for `secs` seconds.
pub fn ago(secs: i64) -> String {
    let (n, u) = match secs.max(0) {
        s if s < 3_600 => (s / 60, "minute"),
        s if s < 86_400 => (s / 3_600, "hour"),
        s if s < 14 * 86_400 => (s / 86_400, "day"),
        s if s < 60 * 86_400 => (s / (7 * 86_400), "week"),
        s => (s / (30 * 86_400), "month"),
    };
    format!("{n} {u}{} ago", if n == 1 { "" } else { "s" })
}

/// A container's verified activity: its state, and when it stopped.
pub fn activity(c: &Container, now: i64) -> String {
    if c.live() {
        return c.state.clone();
    }
    match crate::checkpoint::epoch_secs(&c.finished_at).filter(|t| *t > 0 && !c.finished_at.starts_with("0001-")) {
        Some(t) => format!("{} {}", c.state, ago(now - t)),
        None => format!("{}, never ran", c.state),
    }
}

/// What gc does with Docker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// atlas's containers to remove (stopped, checkout gone).
    pub remove_containers: Vec<String>,
    /// atlas's volumes to remove (checkout gone, no running container).
    pub remove_volumes: Vec<String>,
    /// One line per object reported and left alone.
    pub report: Vec<String>,
}

/// The plan. `gone` maps a hash to the checkout path that is gone (or
/// removed by this run); `live` holds every hash of a checkout that exists.
/// A hash in both is live. `now` is the epoch seconds.
pub fn plan(containers: &[Container], volumes: &[String], gone: &BTreeMap<String, String>, live: &BTreeSet<String>, now: i64) -> Plan {
    let mut p = Plan::default();
    let gone_hash = |h: &str| gone.get(h).filter(|_| !live.contains(h));
    let mut users: BTreeMap<&str, Vec<&Container>> = BTreeMap::new();
    for c in containers {
        for v in &c.volumes {
            users.entry(v.as_str()).or_default().push(c);
        }
    }
    let mut taken: BTreeSet<&str> = BTreeSet::new();
    for c in containers {
        let Some(h) = c.name.strip_prefix(CONTAINER_PREFIX) else {
            p.report.push(format!("docker container {}: {}", c.name, activity(c, now)));
            continue;
        };
        match gone_hash(h) {
            Some(path) if !c.live() => {
                p.remove_containers.push(c.name.clone());
                taken.insert(c.name.as_str());
                p.report.push(format!("docker remove container {} ({}; checkout {path} gone)", c.name, activity(c, now)));
            }
            Some(_) => p.report.push(format!("docker container {}: {}; kept while it runs", c.name, activity(c, now))),
            None => p.report.push(format!("docker container {}: {}; its checkout exists or is not known", c.name, activity(c, now))),
        }
    }
    for v in volumes {
        let used: Vec<&Container> = users.get(v.as_str()).cloned().unwrap_or_default();
        let by = used.iter().map(|c| format!("{} ({})", c.name, activity(c, now))).collect::<Vec<_>>().join(", ");
        let Some(h) = v.strip_prefix(TARGET_PREFIX) else {
            p.report.push(if used.is_empty() {
                format!("docker volume {v}: unreferenced, last activity unknown")
            } else {
                format!("docker volume {v}: used by {by}")
            });
            continue;
        };
        // Every user must be one of atlas's stopped containers removed first.
        let free = used.iter().all(|c| !c.live() && taken.contains(c.name.as_str()));
        match gone_hash(h) {
            Some(path) if free => {
                p.remove_volumes.push(v.clone());
                p.report.push(format!("docker remove volume {v} (checkout {path} gone)"));
            }
            Some(_) => p.report.push(format!("docker volume {v}: kept, used by {by}")),
            None => p.report.push(format!("docker volume {v}: its checkout exists or is not known")),
        }
    }
    p
}

/// Carries out `p`: containers first, then volumes. Returns one line per
/// failure (Docker refuses a volume in use; that is reported).
pub fn apply(d: &dyn Docker, p: &Plan) -> Vec<String> {
    let mut errors = Vec::new();
    for c in &p.remove_containers {
        if let Err(e) = d.run(&["rm", c]) {
            errors.push(e);
        }
    }
    for v in &p.remove_volumes {
        if let Err(e) = d.run(&["volume", "rm", v]) {
            errors.push(e);
        }
    }
    errors
}
