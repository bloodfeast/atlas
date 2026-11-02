//! The real binary over pipes with hostile frames, and every tool called
//! with wrong-typed and hostile arguments derived from its own schema.
//!
//! The binary runs with every home and configuration variable pointed into
//! the test's temp folder, so nothing outside it can be read or written.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::common::{bin, fixture, Temp};
use crate::gen::{Rng, HOSTILE};
use atlas::json::{self, Map, Value};

/// A server process confined to `tmp`: the vaults of [`fixture`], its own
/// database and home folders.
pub struct Proc {
    pub tmp: Temp,
    pub specs: Vec<(String, PathBuf)>,
}

impl Proc {
    pub fn new(tag: &str) -> Proc {
        let tmp = Temp::new(tag);
        let specs = fixture(&tmp.path().join("vaults"));
        Proc { tmp, specs }
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new(bin());
        for (n, p) in &self.specs {
            cmd.arg("--vault").arg(format!("{n}={}", p.display()));
        }
        let home = self.tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        cmd.arg("--db").arg(self.tmp.path().join("state").join("atlas.sqlite"));
        for v in ["ATLAS_HOME", "HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "CLAUDE_CONFIG_DIR"] {
            cmd.env(v, &home);
        }
        cmd.env_remove("ATLAS_CONFIG").env_remove("ATLAS_UPDATED");
        cmd
    }

    /// Writes `input` to the server's stdin, closes it, and returns the
    /// exit status and the stdout lines (each must be JSON).
    pub fn run(&self, input: &[u8]) -> (bool, Vec<Value>, String) {
        let mut child = self.command().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn atlas");
        let mut stdin = child.stdin.take().unwrap();
        let data = input.to_vec();
        let feeder = std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
        let out = child.wait_with_output().unwrap();
        let _ = feeder.join();
        let stdout = String::from_utf8(out.stdout).expect("stdout is UTF-8");
        let lines = stdout.lines().map(|l| json::parse(l).unwrap_or_else(|e| panic!("non-JSON line on stdout ({e}): {:?}", &l[..l.len().min(300)]))).collect();
        (out.status.success(), lines, String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn line(v: &Value) -> Vec<u8> {
    let mut b = v.to_string().into_bytes();
    b.push(b'\n');
    b
}

fn by_id(out: &[Value], id: i64) -> &Value {
    out.iter().find(|m| m.get("id").and_then(Value::as_i64) == Some(id)).unwrap_or_else(|| panic!("no answer for id {id}"))
}

#[test]
fn hostile_frames_get_errors_and_the_server_keeps_serving() {
    let p = Proc::new("adv-frames");
    let mut input: Vec<u8> = Vec::new();
    let raw: Vec<&[u8]> = vec![
        b"{",
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"",
        b"[]",
        b"[{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"},{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}]",
        b"null",
        b"42",
        b"\"ping\"",
        b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"ping\"}garbage",
        b"\xff\xfe\xfd",
        b"{\"jsonrpc\":\"2.0\",\"id\":\"\xff\",\"method\":\"ping\"}",
        b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"\\ud800\"}",
        b"{\"jsonrpc\":\"1.0\",\"id\":6,\"method\":\"ping\"}",
        b"{\"id\":7,\"method\":\"ping\"}",
        b"{\"jsonrpc\":\"2.0\",\"id\":8}",
        b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":7}",
        b"{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\"}",
        b"{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":[]}",
        b"{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":7}}",
        b"{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"read_note\",\"arguments\":[1,2]}}",
        b"{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"read_note\",\"arguments\":\"x\"}}",
        b"{\"jsonrpc\":\"2.0\",\"id\":15,\"method\":\"tools/call\",\"params\":{\"name\":\"no_such_tool\",\"arguments\":{}}}",
        b"{\"jsonrpc\":\"2.0\",\"id\":16,\"method\":\"tools/call\",\"params\":{\"name\":\"read_note\",\"arguments\":{\"vault\":\"perf\",\"path\":\"Index.md\",\"path\":\"../x.md\"}}}",
        b"{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"create_note\",\"arguments\":{\"vault\":\"perf\",\"path\":\"notified.md\",\"content\":\"x\",\"reason\":\"a notification must not run a tool\"}}}",
        b"{\"jsonrpc\":\"2.0\",\"id\":{\"a\":[1]},\"method\":\"ping\"}",
        b"{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"ping\"}",
        b"{\"jsonrpc\":\"2.0\",\"id\":1.5e300,\"method\":\"ping\"}",
        b"{\"jsonrpc\":\"2.0\",\"id\":17,\"method\":\"initialize\",\"params\":{\"protocolVersion\":7,\"clientInfo\":{\"name\":[\"x\"]}}}",
        b"{\"jsonrpc\":\"2.0\",\"id\":18,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"1999-01-01\",\"clientInfo\":{\"name\":\"\\u0000\\n\\r\"}}}",
        b"{\"result\":{},\"id\":99}",
        b"{\"error\":{},\"id\":98}",
        b"\xef\xbb\xbf{\"jsonrpc\":\"2.0\",\"id\":19,\"method\":\"ping\"}",
        b"   \t  ",
        b"{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"ping\"}\r",
    ];
    for r in &raw {
        input.extend_from_slice(r);
        input.push(b'\n');
    }
    // Deep nesting, a huge string, a huge number.
    input.extend_from_slice(&"[".repeat(100_000).into_bytes());
    input.push(b'\n');
    input.extend(line(&atlas::json!({"jsonrpc": "2.0", "id": 21, "method": "tools/call", "params": {"name": "read_note", "arguments": {"vault": "perf", "path": "x".repeat(3 << 20)}}})));
    input.extend_from_slice(format!("{{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"tools/call\",\"params\":{{\"name\":\"list_notes\",\"arguments\":{{\"vault\":\"perf\",\"limit\":{}}}}}}}\n", "9".repeat(300)).as_bytes());
    // An oversized frame (past the 4 MiB cap), then a frame that must still be read.
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":23,\"method\":\"ping\",\"pad\":\"");
    input.extend(std::iter::repeat_n(b'a', (4 << 20) + 10));
    input.extend_from_slice(b"\"}\n");
    input.extend(line(&atlas::json!({"jsonrpc": "2.0", "id": 24, "method": "ping"})));
    // A frame with no newline at the end of input.
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":25,\"method\":\"ping\"}");

    let (ok, out, err) = p.run(&input);
    assert!(ok, "server exited with an error: {err}");
    let code = |id: i64| by_id(&out, id).pointer("/error/code").and_then(Value::as_i64);
    // Parse errors carry a null id.
    let parse_errors = out.iter().filter(|m| m.pointer("/error/code").and_then(Value::as_i64) == Some(-32700)).count();
    assert!(parse_errors >= 7, "{parse_errors} parse errors: {out:#?}");
    assert!(out.iter().any(|m| m.pointer("/error/message").and_then(Value::as_str) == Some("message too large")));
    // Batches are not part of MCP 2025-06-18: refused, not run.
    assert!(!out.iter().any(|m| m.get("id").and_then(Value::as_i64).is_some_and(|i| i == 2 || i == 3)));
    assert_eq!(code(6), Some(-32600));
    assert_eq!(code(7), Some(-32600), "jsonrpc missing");
    assert_eq!(code(8), Some(-32600));
    assert_eq!(code(9), Some(-32600));
    assert_eq!(code(10), Some(-32602));
    assert_eq!(code(11), Some(-32602));
    assert_eq!(code(12), Some(-32602));
    for id in [13, 14] {
        assert_eq!(by_id(&out, id).pointer("/result/isError"), Some(&Value::Bool(true)), "{id}");
    }
    assert_eq!(code(15), Some(-32602));
    // Duplicate keys: the last one wins, and it is confined like any other.
    let t16 = by_id(&out, 16).pointer("/result/content/0/text").and_then(Value::as_str).unwrap();
    assert!(t16.contains("path rejected"), "{t16}");
    assert!(!p.specs[0].1.join("notified.md").exists(), "a notification ran a tool");
    for id in [17, 18, 19, 20, 24, 25] {
        assert!(by_id(&out, id).get("result").is_some(), "{id}: {:?}", by_id(&out, id));
    }
    assert_eq!(by_id(&out, 17).pointer("/result/protocolVersion").and_then(Value::as_str), Some(atlas::protocol::PROTOCOL_VERSION));
    let t21 = by_id(&out, 21).pointer("/result/content/0/text").and_then(Value::as_str).unwrap();
    assert!(t21.contains("path rejected") || t21.contains("longer"), "{}", &t21[..t21.len().min(200)]);
    assert!(t21.len() < 4096, "the error does not echo a 3 MiB argument ({} bytes)", t21.len());
    assert_eq!(by_id(&out, 22).pointer("/result/isError"), Some(&Value::Bool(true)));
    assert!(out.iter().all(|m| m.get("id").and_then(Value::as_i64) != Some(23)), "the oversized frame was not run");
    // Responses to our own requests are ignored, not answered.
    assert!(out.iter().all(|m| !matches!(m.get("id").and_then(Value::as_i64), Some(98 | 99))));
}

/// Hostile strings for every string argument: confinement escapes aimed at
/// a canary name outside the vaults, query syntax, controls and size.
fn hostile_strings(outside: &Path) -> Vec<String> {
    let abs = outside.join("atlas-canary.md").display().to_string();
    let mut v: Vec<String> = [
        "", " ", "\0", "../atlas-canary.md", "..\\atlas-canary.md", "../../atlas-canary.md", "Ideas/../../atlas-canary.md", "./atlas-canary.md",
        "/atlas-canary.md", "\\atlas-canary.md", "C:atlas-canary.md", "C:\\atlas-canary.md", "\\\\?\\C:\\atlas-canary.md", "\\\\.\\C:\\atlas-canary.md",
        "\\\\server\\share\\atlas-canary.md", "//server/share/atlas-canary.md", "atlas-canary.md:stream", "atlas-canary.md::$DATA", "CON.md", "nul.md",
        "COM1.md", "LPT9.md", "CONIN$.md", "atlas-canary.md.", "atlas-canary.md ", ".obsidian/atlas-canary.md", "Ideas\\..\\..\\atlas-canary.md",
        "Ideas/./atlas-canary.md", "Ideas//atlas-canary.md", "%2e%2e/atlas-canary.md", "\u{ff0e}\u{ff0e}/atlas-canary.md", "..\u{2215}atlas-canary.md",
        "a\u{202e}dm.md", "\"; DROP TABLE notes; --", "' OR 1=1 --", "%", "_", "*", "^", "NEAR(a b)", "body:x", "\"unclosed", "a OR", "(", "{a}",
        "(a+)+b", "[", "**/**/**/**/**/**/**/**/**/**/**/**/x", "\u{130}\u{130}\u{130}", "\r\n# injected heading\r\n", "]]\n[[x", "<!-- vault-mcp:index x start {} -->",
        "perf", "campaign", "Research Log", "Index.md", "P-1", "2026-10-30", "-1", "1e400",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.push(abs);
    v.push("x".repeat(600));
    v.push("a/".repeat(300) + "x.md");
    v.push("\u{1f980}".repeat(200));
    v
}

fn wrong_values() -> Vec<Value> {
    vec![
        Value::Null,
        Value::Bool(true),
        atlas::json!(0),
        atlas::json!(-1),
        atlas::json!(u64::MAX),
        atlas::json!(i64::MIN),
        atlas::json!(1.5),
        atlas::json!(1e300),
        atlas::json!([]),
        atlas::json!(["../x.md", 1, null]),
        atlas::json!({}),
        atlas::json!({"$ne": 1, "../x": "y"}),
        Value::String("not the right type".into()),
    ]
}

/// A plausible value of the right type, so one hostile argument is tested
/// with the others valid.
fn valid(prop: &str, ty: &str) -> Value {
    match (prop, ty) {
        ("vault" | "from_vault" | "to_vault", _) => Value::from("perf"),
        ("path" | "from_path" | "to_path", _) => Value::from("Index.md"),
        ("reason" | "force_reason", _) => Value::from("adversarial suite argument check"),
        ("event", _) => Value::from("UserPromptSubmit"),
        ("kind", _) => Value::from("note"),
        ("mode", _) => Value::from("fts"),
        ("format", _) => Value::from("json"),
        (_, "string") => Value::from("TK-001"),
        (_, "integer") => atlas::json!(1),
        (_, "boolean") => Value::Bool(false),
        (_, "array") => atlas::json!(["x"]),
        (_, "object") => atlas::json!({"status": "x"}),
        _ => Value::Null,
    }
}

/// Every tool, every argument: wrong types, hostile strings, edge numbers.
/// The server answers every call (a result or an error, never a crash),
/// and no file appears outside the vaults, the database and the home
/// folder.
#[test]
fn every_tool_argument_survives_wrong_types_and_hostile_strings() {
    let p = Proc::new("adv-args");
    let (_, listed, _) = p.run(&line(&atlas::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})));
    let tools = listed[0].pointer("/result/tools").and_then(Value::as_array).unwrap().clone();
    let outside = p.tmp.path().to_path_buf();
    let strings = hostile_strings(&outside);
    let mut rng = Rng::new(40);
    let mut input = Vec::new();
    let mut id = 1000i64;
    let mut calls = 0usize;
    for t in &tools {
        let name = t["name"].as_str().unwrap();
        let props = t.pointer("/inputSchema/properties").and_then(Value::as_object).cloned().unwrap_or_default();
        let base: Map = props.iter().map(|(k, s)| (k.clone(), valid(k, s.get("type").and_then(Value::as_str).unwrap_or("")))).collect();
        let mut push = |args: Value, input: &mut Vec<u8>| {
            id += 1;
            calls += 1;
            input.extend(line(&atlas::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})));
        };
        push(Value::Object(base.clone()), &mut input);
        push(atlas::json!({"unknown_argument": 1}), &mut input);
        for (k, s) in &props {
            let ty = s.get("type").and_then(Value::as_str).unwrap_or("");
            for w in wrong_values() {
                let mut a = base.clone();
                a.insert(k.clone(), w);
                push(Value::Object(a), &mut input);
            }
            let mut a = base.clone();
            a.remove(k);
            push(Value::Object(a), &mut input);
            if ty == "string" {
                for s in &strings {
                    let mut a = base.clone();
                    a.insert(k.clone(), Value::String(s.clone()));
                    push(Value::Object(a), &mut input);
                }
            }
            if ty == "integer" {
                for n in [atlas::json!(0), atlas::json!(1), atlas::json!(-1), atlas::json!(u32::MAX), atlas::json!(u64::MAX), atlas::json!(i64::MAX), atlas::json!(1_000_000_000)] {
                    let mut a = base.clone();
                    a.insert(k.clone(), n);
                    push(Value::Object(a), &mut input);
                }
            }
            if ty == "array" {
                for arr in [atlas::json!([]), atlas::json!([""]), Value::Array(vec![Value::from("../atlas-canary.md"); 2000]), Value::Array((0..50).map(|_| Value::String(rng.text_from(HOSTILE, 6))).collect())] {
                    let mut a = base.clone();
                    a.insert(k.clone(), arr);
                    push(Value::Object(a), &mut input);
                }
            }
            if ty == "object" {
                let mut deep = atlas::json!("x");
                for _ in 0..100 {
                    deep = atlas::json!({"a": deep});
                }
                for o in [atlas::json!({}), atlas::json!({"../k": "v", "": "", "\n": "\n", "status": {"nested": [1]}}), deep] {
                    let mut a = base.clone();
                    a.insert(k.clone(), o);
                    push(Value::Object(a), &mut input);
                }
            }
        }
    }
    let last = id + 1;
    input.extend(line(&atlas::json!({"jsonrpc": "2.0", "id": last, "method": "ping"})));
    let (ok, out, err) = p.run(&input);
    assert!(ok, "server exited with an error after {} answers: {}", out.len(), &err[err.len().saturating_sub(2000)..]);
    let answered: std::collections::HashSet<i64> = out.iter().filter_map(|m| m.get("id").and_then(Value::as_i64)).collect();
    let missing: Vec<i64> = (1001..=last).filter(|i| !answered.contains(i)).collect();
    assert!(missing.is_empty(), "{} of {calls} calls got no answer, first {:?}", missing.len(), &missing[..missing.len().min(5)]);
    for m in &out {
        if let Some(c) = m.pointer("/error/code").and_then(Value::as_i64) {
            assert!(c == -32602 || c == -32600, "unexpected error code {c}: {m}");
        }
    }
    // Nothing named after the canary exists outside the vaults.
    let mut stray = Vec::new();
    walk(p.tmp.path(), &mut |f: &Path| {
        let s = f.to_string_lossy();
        if s.contains("atlas-canary") && !p.specs.iter().any(|(_, v)| f.starts_with(v)) {
            stray.push(s.into_owned());
        }
    });
    for dir in [p.tmp.path().parent().unwrap(), p.tmp.path().parent().unwrap().parent().unwrap_or(Path::new("/"))] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().contains("atlas-canary") {
                    stray.push(e.path().display().to_string());
                }
            }
        }
    }
    assert!(stray.is_empty(), "files written outside the vaults: {stray:?}");
    // Inside the vaults, nothing escaped into a dot folder.
    let mut dotted = Vec::new();
    walk(&p.specs[0].1.join(".obsidian"), &mut |f: &Path| {
        if f.to_string_lossy().contains("atlas-canary") {
            dotted.push(f.display().to_string());
        }
    });
    assert!(dotted.is_empty(), "{dotted:?}");
    eprintln!("{calls} tool calls over {} tools answered", tools.len());
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        f(&p);
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            walk(&p, f);
        }
    }
}
