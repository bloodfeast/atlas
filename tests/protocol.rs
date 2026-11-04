//! The real binary over pipes: initialize, tools/list, tools/call; and many
//! processes sharing one database.

mod common;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::*;
use atlas::json::{Value};
use atlas::json;

fn run(specs: &[(String, PathBuf)], db: &Path, lines: &[Value]) -> Vec<Value> {
    let mut cmd = Command::new(bin());
    for (n, p) in specs {
        cmd.arg("--vault").arg(format!("{n}={}", p.display()));
    }
    cmd.arg("--db").arg(db).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn atlas");
    {
        let mut stdin = child.stdin.take().unwrap();
        for l in lines {
            writeln!(stdin, "{l}").unwrap();
        }
        writeln!(stdin, "this is not json").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "exit {:?}: {}", out.status, String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    stdout.lines().map(|l| atlas::json::parse(l).unwrap_or_else(|e| panic!("non-JSON on stdout: {l:?}: {e}"))).collect()
}

#[test]
fn round_trip_through_the_binary() {
    let fx = Fx::new("p-round");
    let msgs = vec![
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pipe-test", "version": "1"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "read_note", "arguments": {"vault": "perf", "path": "Ideas/PI-002 Throttle.md"}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "read_note", "arguments": {"vault": "perf", "path": "../escape.md"}}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "append_history", "arguments": {"vault": "perf", "path": "Index.md", "text": "touched over a pipe", "date": "2026-10-08", "reason": "pipe round-trip test of a write"}}}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "nope"}),
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "delete_note", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 8, "method": "ping"}),
    ];
    let out = run(&fx.specs, &fx.db, &msgs);
    assert_eq!(out.len(), 9, "{out:#?}");
    let init = &out[0]["result"];
    assert_eq!(init["protocolVersion"], json!("2025-06-18"));
    assert_eq!(init["serverInfo"]["name"], json!("atlas"));
    let instr = init["instructions"].as_str().unwrap();
    assert!(!instr.contains('\n') && instr.starts_with("Markdown vaults served: perf (Research Log)"), "{instr}");
    let tools = out[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 14, "the listed tools; the full tools behind their ops stay callable");
    assert!(tools.iter().any(|t| t["name"] == json!("section")) && tools.iter().any(|t| t["name"] == json!("grep")), "the core read verbs");
    assert!(tools.iter().any(|t| t["name"] == json!("symbol")), "the code index answers through symbol");
    // The core verbs are always loaded; every other tool stays deferred.
    let mut always: Vec<&str> = tools.iter().filter(|t| t["_meta"]["anthropic/alwaysLoad"] == json!(true)).map(|t| t["name"].as_str().unwrap()).collect();
    always.sort_unstable();
    assert_eq!(always, ["grep", "lookup", "recall", "search", "section", "symbol"]);
    assert_eq!(tools.iter().filter(|t| t.get("_meta").is_some()).count(), 6);
    for t in tools {
        let d = t["description"].as_str().unwrap();
        assert!(d.len() < 300, "long description for {}", t["name"]);
        assert!(t["inputSchema"]["type"] == json!("object"));
    }
    let read = &out[2]["result"];
    assert_eq!(read["isError"], json!(false));
    // Text by default: one line naming the note and its hash, then the note as written.
    let text = read["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("perf:Ideas/PI-002 Throttle.md sha256=") && text.ends_with(IDEA_2), "{text}");
    assert_eq!(text.lines().count(), IDEA_2.lines().count() + 1);
    assert!(read.get("structuredContent").is_none(), "the answer is sent once");
    assert_eq!(out[3]["result"]["isError"], json!(true));
    assert!(out[3]["result"]["content"][0]["text"].as_str().unwrap().contains("path rejected"));
    assert_eq!(out[4]["result"]["isError"], json!(false));
    assert_eq!(out[5]["error"]["code"], json!(-32601));
    assert_eq!(out[6]["error"]["code"], json!(-32602));
    assert_eq!(out[7]["result"], json!({}));
    assert_eq!(out[8]["error"]["code"], json!(-32700));
    assert_eq!(out[8]["id"], Value::Null);
    // The client name from initialize reached the audit log.
    let conn = rusqlite::Connection::open(&fx.db).unwrap();
    let client: String = conn.query_row("SELECT client FROM writes", [], |r| r.get(0)).unwrap();
    assert_eq!(client, "pipe-test");
}

#[test]
fn twelve_processes_never_share_an_id() {
    let fx = Fx::new("p-twelve");
    const PROCS: usize = 12;
    const EACH: usize = 10;
    let mut handles = Vec::new();
    for p in 0..PROCS {
        let specs = fx.specs.clone();
        let db = fx.db.clone();
        handles.push(std::thread::spawn(move || {
            let mut msgs = vec![json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": format!("proc-{p}")}}})];
            for i in 0..EACH {
                // Mix in reads and other writes so the processes contend on everything.
                msgs.push(json!({"jsonrpc": "2.0", "id": 100 + i, "method": "tools/call", "params": {"name": "next_id", "arguments": {
                    "vault": "perf", "folder": "Ideas", "prefix": "PI-", "name": format!("From {p} {i}"), "content": "---\nid: {{id}}\n---\n", "reason": "concurrent id allocation test", "format": "json"}}}));
                msgs.push(json!({"jsonrpc": "2.0", "id": 200 + i, "method": "tools/call", "params": {"name": "append_history", "arguments": {
                    "vault": "perf", "path": "Index.md", "text": format!("proc {p} step {i}"), "date": "2026-10-08", "reason": "concurrent history append test"}}}));
                msgs.push(json!({"jsonrpc": "2.0", "id": 300 + i, "method": "tools/call", "params": {"name": "search", "arguments": {"query": "PI-"}}}));
            }
            run(&specs, &db, &msgs)
        }));
    }
    let mut ids = Vec::new();
    for h in handles {
        for r in h.join().unwrap() {
            let id = r["id"].as_i64().unwrap_or(-1);
            if (100..200).contains(&id) || (200..300).contains(&id) {
                assert_eq!(r["result"]["isError"], json!(false), "{r}");
            }
            if (100..200).contains(&id) {
                let body: Value = atlas::json::parse(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
                ids.push(body["id"].as_str().unwrap().to_string());
            }
        }
    }
    assert_eq!(ids.len(), PROCS * EACH);
    let unique: HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "duplicate ids handed out");
    let mut nums: Vec<u32> = ids.iter().map(|i| i[3..].parse().unwrap()).collect();
    nums.sort();
    assert_eq!(nums, (3..3 + (PROCS * EACH) as u32).collect::<Vec<_>>(), "ids are not contiguous");
    let files = std::fs::read_dir(fx.perf().join("Ideas")).unwrap().count();
    assert_eq!(files, 2 + PROCS * EACH);
    // Every history append landed: no lost update between processes.
    let index = std::fs::read_to_string(fx.perf().join("Index.md")).unwrap();
    assert_eq!(index.matches(": proc ").count(), PROCS * EACH);
    // The audit mirror has one whole JSON line per write: nothing interleaved or lost.
    let log = std::fs::read_to_string(fx.db.with_file_name(atlas::store::AUDIT_LOG)).unwrap();
    let lines: Vec<Value> = log.lines().map(|l| atlas::json::parse(l).expect("torn audit line")).collect();
    assert_eq!(lines.len(), 2 * PROCS * EACH);
}

#[test]
fn rescan_flag_reports_and_exits() {
    let fx = Fx::new("p-rescan");
    let mut cmd = Command::new(bin());
    for (n, p) in &fx.specs {
        cmd.arg("--vault").arg(format!("{n}={}", p.display()));
    }
    let out = cmd.arg("--db").arg(&fx.db).arg("--rescan").output().unwrap();
    assert!(out.status.success());
    let v: Value = atlas::json::parse_bytes(&out.stdout).unwrap();
    assert_eq!(v["vaults"]["files"], json!(6));
    assert_eq!(v["vaults"]["parsed"], json!(6));
    assert_eq!(v["sources"]["files"], json!(0));
    let bad = Command::new(bin()).arg("--vault").arg("x").output().unwrap();
    assert!(!bad.status.success());
}

/// Property 1: nothing in the dependency graph or the source can reach the
/// network.
#[test]
fn no_network_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    for bad in [
        "reqwest",
        "hyper",
        "ureq",
        "tokio",
        "mio",
        "socket2",
        "curl",
        "native-tls",
        "rustls",
        "h2",
        "async-std",
        "url",
        "trust-dns",
        "hickory",
        "openssl",
        "isahc",
        "attohttpc",
        "tungstenite",
    ] {
        assert!(!lock.contains(&format!("name = \"{bad}\"")), "network crate {bad} in Cargo.lock");
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let deps = manifest.split("[dependencies]").nth(1).unwrap();
    let names: Vec<&str> = deps.lines().filter_map(|l| l.split_once('=').map(|x| x.0.trim())).filter(|n| !n.is_empty()).collect();
    // The two named exceptions: bundled SQLite, and the ed25519 check on
    // release manifests with its default features (random, PEM, X25519) off,
    // so it brings no dependency of its own.
    assert_eq!(names, vec!["rusqlite", "ed25519-compact"]);
    let ed = deps.lines().find(|l| l.starts_with("ed25519-compact")).unwrap();
    assert!(ed.contains("default-features = false") && !ed.contains("features = ["), "{ed}");
    let pkg = |name: &str| lock.split("[[package]]").find(|p| p.contains(&format!("name = \"{name}\""))).unwrap().to_string();
    assert!(!pkg("ed25519-compact").contains("dependencies"), "ed25519-compact pulls in a dependency");
    let mut files = Vec::new();
    let mut dirs = vec![root.join("src")];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.path().is_dir() {
                dirs.push(e.path());
            } else {
                files.push(e.path());
            }
        }
    }
    for path in files {
        let src = std::fs::read_to_string(&path).unwrap();
        // The bench may spawn an older build to compare against; the code
        // indexer spawns rust-analyzer and the language table's SCIP indexers
        // (and asks `npm root -g` where a Node one is)
        // (bounded, one at a time, off the query path); the platform module
        // lists processes (macOS `ps`); the model
        // download runs the system curl (setup and first start only, never
        // at query time); the release check restarts atlas on the binary it
        // just swapped in (session start only); `atlas run` spawns the command
        // the user names, from the command line only (never from a tool or
        // hook); the settings step runs `claude --version` (an interactive
        // offer only, for the min_version gate). Nothing else spawns. Unsafe code lives only in the platform FFI and the
        // SIMD kernels (scan, embedding GEMM).
        let platform = path.parent().is_some_and(|p| p.ends_with("platform"));
        let spawns = path.ends_with("bench.rs") || path.ends_with("code_index.rs") || path.ends_with("models.rs") || path.ends_with("update.rs") || path.ends_with("run.rs") || path.ends_with("claude_settings.rs") || platform;
        let ffi = platform || path.ends_with("scan.rs") || path.ends_with("gemm.rs");
        // Only the server's code: not the test modules, not comments.
        let body = src.split("\n#[cfg(test)]\nmod tests").next().unwrap_or(&src);
        let code: String = body.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
        for bad in ["std::net", "TcpStream", "TcpListener", "UdpSocket", "ToSocketAddrs", "Command::new", "unsafe "] {
            if (spawns && bad == "Command::new") || (ffi && bad == "unsafe ") {
                continue;
            }
            assert!(!code.contains(bad), "{bad} in {}", path.display());
        }
    }
}

/// A Windows client may write a UTF-8 byte order mark before its first
/// message; it is skipped rather than answered with a parse error.
#[test]
fn a_leading_byte_order_mark_is_ignored() {
    let fx = Fx::new("p-bom");
    let mut s = fx.server();
    let mut input = vec![0xEF, 0xBB, 0xBF];
    input.extend_from_slice(br#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#);
    input.push(b'\n');
    let mut out = Vec::new();
    atlas::protocol::serve(&mut s, &input[..], &mut out).unwrap();
    let v: Value = atlas::json::parse_bytes(&out).unwrap();
    assert_eq!(v["id"], json!(7), "{v}");
    assert!(v.get("error").is_none(), "{v}");
}

#[test]
fn help_lists_every_mode_and_subcommand() {
    let out = Command::new(bin()).arg("--help").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for want in ["--setup", "--hook", "--memory-check", "--memory-snapshot", "--token-report", "--code-index", "atlas run ", "atlas slice ", "atlas digest ", "atlas crop ", "atlas ruling ", "atlas entry ", "atlas grammar ", "settings list", "undo [--yes]", "add-project", "migrate-projects", "on|off update"] {
        assert!(text.contains(want), "--help lacks {want:?}:\n{text}");
    }
    assert!(text.lines().count() > 20, "{text}");
}
