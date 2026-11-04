//! Shared fixtures: synthetic vaults in a fresh temp directory. Tests never
//! touch the live vaults.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use atlas::json::{Value};
use atlas::json;
use atlas::paths::Vaults;
use atlas::store::Store;
use atlas::tools::Server;

static N: AtomicU32 = AtomicU32::new(0);

/// A temp directory removed on drop.
pub struct Temp(pub PathBuf);

impl Temp {
    pub fn new(tag: &str) -> Temp {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let p = std::env::temp_dir().join(format!("atlas-test-{tag}-{}-{n}-{nanos}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        Temp(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn write(p: &Path, text: &str) {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(p, text).unwrap();
}

pub const IDEA_1: &str = "---
id: PI-001
title: \"Cache inside the loader\"
status: \"in flight LOADER-WORK\"
area: \"Frame scheduling and presentation\"
saving_median: \"-0.2 ms\"
measured: true
tags: [perf, area/fs, status/in-flight, measured]
---

# PI-001 Cache inside the loader

> -0.2 ms

## Entry

- **Where:** presenter

## Sources

- Packages: [[LOADER-WORK]]

## Related

- [[PI-002 Throttle]]

## History

Append-only. Add a dated line for every change; never edit or remove a line.

- 2026-10-08: proposed by LOADER-WORK
";

pub const IDEA_2: &str = "---
id: PI-002
title: \"Throttle\"
status: \"parked\"
area: \"GPU passes\"
saving_median: \"12 us\"
measured: true
tags: [perf, area/gp, status/parked, measured]
---

# PI-002 Throttle

## History

- 2026-09-29: measured at 12 us
";

/// Builds three vaults under `root`:
/// `perf` = "Research Log" (seeded), `campaign` = "Field
/// Notes" (one note with a cross-vault link), `duel` = "Archive Box"
/// (empty). Returns the `(name, path)` specs.
pub fn fixture(root: &Path) -> Vec<(String, PathBuf)> {
    let perf = root.join("Research Log");
    let camp = root.join("Field Notes");
    let duel = root.join("Archive Box");
    write(&perf.join("Index.md"), "---\ntags: [perf, index]\n---\n\n# Index\n\nSee [[How to add an idea]].\n");
    write(&perf.join("How to add an idea.md"), "# How to add an idea\n\n```\n[[PACKAGE]]\n```\n");
    write(&perf.join("Ideas/PI-001 Cache inside the loader.md"), IDEA_1);
    write(&perf.join("Ideas/PI-002 Throttle.md"), IDEA_2);
    write(&perf.join("Areas/GPU passes.md"), "# GPU passes\n\nBack to [[Index]].\n");
    write(&perf.join(".obsidian/app.json"), "{}");
    write(&perf.join(".obsidian/secret.md"), "hidden");
    write(
        &camp.join("Missions/M1 Hold.md"),
        "# M1 Hold\n\nPerf: [PI-002](obsidian://open?vault=Research%20Log&file=Ideas%2FPI-002%20Throttle)\nMissing: [[Nowhere]]\n",
    );
    fs::create_dir_all(&duel).unwrap();
    vec![("perf".into(), perf), ("campaign".into(), camp), ("duel".into(), duel)]
}

pub fn server_with(specs: &[(String, PathBuf)], db: &Path) -> Server {
    let vaults = Vaults::new(specs).unwrap();
    let mut store = Store::open(db, vaults).unwrap();
    store.rescan().unwrap();
    let mut s = Server::new(store);
    s.set_client("test-client");
    s
}

pub struct Fx {
    pub tmp: Temp,
    pub specs: Vec<(String, PathBuf)>,
    pub db: PathBuf,
}

impl Fx {
    pub fn new(tag: &str) -> Fx {
        let tmp = Temp::new(tag);
        let specs = fixture(tmp.path());
        let db = tmp.path().join("state").join("vaults.sqlite");
        Fx { tmp, specs, db }
    }
    pub fn server(&self) -> Server {
        server_with(&self.specs, &self.db)
    }
    pub fn perf(&self) -> PathBuf {
        self.specs[0].1.clone()
    }
    pub fn camp(&self) -> PathBuf {
        self.specs[1].1.clone()
    }
}

/// The reason the helpers below add to write calls that do not carry one.
/// The reason rules themselves are tested in `tests/reason.rs` with
/// `Server::call` directly.
pub const TEST_REASON: &str = "integration test fixture write";

pub fn takes_reason(tool: &str) -> bool {
    atlas::tools::full_tool_list().iter().any(|t| t["name"] == tool && t["inputSchema"]["properties"].get("reason").is_some())
}

fn with_reason(tool: &str, mut args: Value) -> Value {
    if takes_reason(tool) {
        if let Some(m) = args.as_object_mut() {
            if !m.contains_key("reason") {
                m.insert("reason".into(), json!(TEST_REASON));
            }
        }
    }
    args
}

pub fn call(s: &mut Server, tool: &str, args: Value) -> Value {
    let args = with_reason(tool, args);
    match s.call(tool, &args) {
        Ok(v) => v,
        Err(e) => panic!("{tool} failed: {e}"),
    }
}

pub fn call_err(s: &mut Server, tool: &str, args: Value) -> String {
    let args = with_reason(tool, args);
    match s.call(tool, &args) {
        Ok(v) => panic!("{tool} unexpectedly succeeded: {v}"),
        Err(e) => e.to_string(),
    }
}

pub fn read(s: &mut Server, vault: &str, path: &str) -> Value {
    call(s, "read_note", json!({"vault": vault, "path": path}))
}

/// The `vault-mcp` binary built for these tests.
pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_atlas"))
}
