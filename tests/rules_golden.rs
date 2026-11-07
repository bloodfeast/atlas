//! With `memory.rules` unset, query recall and the prompt hook answer as the
//! release before the feature did. `golden/rules_unset.txt` was written by that
//! release's binary (7d0b920), not by this one, so this compares against main
//! and does not pin the build to itself. To write it again from a named binary:
//!
//! ```text
//! ATLAS_GOLDEN_EXE=path/to/atlas ATLAS_GOLDEN_WRITE=1 cargo test --test rules_golden
//! ```
//!
//! The binary is driven only through flags both builds have. All text here is
//! invented. Store times and content hashes are masked, since a store stamps
//! its records when they are written.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use atlas::json;
use common::*;

const RULES: &str = "# Rules\n\n## Storage\n\
- 2026-01-02 Alex: the widget cache is bounded at sixty four entries.\n\
- 2026-01-03 Blake: exports are written atomically through a rename.\n\
- 2026-01-04 Alex: this reverses r00001; the widget cache grows freely.\n\
- 2026-01-05 Blake: the older queue split is scrapped.\n\
- 2026-01-06 Alex: the gamma queue drains oldest first.\n";

/// The rules after an edit: one changed, one removed, one added. A sync
/// supersedes and retires, so the head query reads more than active records.
const RULES_EDITED: &str = "# Rules\n\n## Storage\n\
- 2026-01-02 Alex: the widget cache is bounded at sixty four entries.\n\
- 2026-01-03 Blake: exports are written atomically through a rename and a flush.\n\
- 2026-01-04 Alex: this reverses r00001; the widget cache grows freely.\n\
- 2026-01-06 Alex: the gamma queue drains oldest first.\n\
- 2026-01-07 Blake: the delta lane keeps its own snapshot folder.\n";

const INDEX: &str = "# Memory\n- [Widget note](widget-note.md) - a widget note\n- [Export note](export-note.md) - an export note\n";
const MEM_A: &str = "---\nname: widget-note\ndescription: a widget note\nmetadata:\n  type: feedback\n---\n\nThe old widget cap was withdrawn after the gamma run.\n";
const MEM_B: &str = "---\nname: export-note\ndescription: an export note\nmetadata:\n  type: project\n---\n\nExports use a temp file and a rename.\n";

const ASKS: &[&str] = &[
    "what is the bound on the widget cache entries",
    "how are exports written",
    "gamma queue order",
    "delta lane snapshot folder",
    "older queue split",
    "widget cap withdrawn",
    "a question about nothing stored here zebra quokka",
];

const KEYS: &[&str] = &["rules:r00001", "rules:r00002", "rules:r00004"];

fn exe() -> PathBuf {
    std::env::var_os("ATLAS_GOLDEN_EXE").map_or_else(bin, PathBuf::from)
}

fn config(root: &Path) -> PathBuf {
    let c = root.join("atlas.json");
    write(
        &c,
        r#"{"db": "state/index.sqlite", "watch": false,
  "vaults": [{"name": "notes", "path": "vault"}],
  "sources": [{"name": "rules", "root": "handbook", "include": ["*.md"]}, {"name": "memory", "root": "memory", "include": ["*.md"]}],
  "memory": {"store": "state/memory.sqlite", "export_dir": "out", "snapshot_hours": 100000,
    "exports": [{"name": "memory", "shape": "memory", "path": "memory", "index": "MEMORY.md"}, {"name": "rules", "shape": "items", "path": "RULES.md"}]}}
"#,
    );
    c
}

fn command(root: &Path, cfg: &Path) -> Command {
    let mut c = Command::new(exe());
    c.arg("--config").arg(cfg).current_dir(root).env("ATLAS_HOME", root.join("home")).env("CLAUDE_CONFIG_DIR", root.join("claude")).env("HOME", root).env("USERPROFILE", root).env_remove("ATLAS_CONFIG");
    c
}

fn rescan(root: &Path, cfg: &Path) {
    let out = command(root, cfg).arg("--rescan").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Every answer, in a stable order, through a server's stdin.
fn recalls(root: &Path, cfg: &Path) -> Vec<(String, String)> {
    let mut child = command(root, cfg).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut ask = |o: json::Value| -> json::Value {
        writeln!(input, "{o}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        json::parse(&line).unwrap()
    };
    ask(atlas::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "golden", "version": "0"}}}));
    let mut all = Vec::new();
    let mut id = 1;
    let mut call = |label: String, args: json::Value, ask: &mut dyn FnMut(json::Value) -> json::Value| {
        let r = ask(atlas::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "recall", "arguments": args}}));
        id += 1;
        all.push((label, r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()));
    };
    for q in ASKS {
        call(format!("query: {q}"), atlas::json!({"query": *q}), &mut ask);
    }
    for k in KEYS {
        call(format!("key: {k}"), atlas::json!({"key": *k}), &mut ask);
    }
    drop(input);
    child.wait().unwrap();
    all
}

fn hooks(root: &Path, cfg: &Path) -> Vec<(String, String)> {
    let mut all = Vec::new();
    for (i, q) in ASKS.iter().enumerate() {
        let mut child = command(root, cfg).arg("--hook").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let event = atlas::json!({"hook_event_name": "UserPromptSubmit", "session_id": format!("s{i}"), "prompt": *q, "cwd": root.to_string_lossy().to_string()});
        write!(child.stdin.take().unwrap(), "{event}").unwrap();
        let out = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let context = json::parse(&text).ok().and_then(|v| v["hookSpecificOutput"]["additionalContext"].as_str().map(str::to_string)).unwrap_or_default();
        all.push((format!("hook: {q}"), context));
    }
    all
}

/// Masks what a fresh store writes differently each run: `YYYY-MM-DDTHH:MM`
/// stamps, with any seconds, and 64-digit hashes.
fn mask(text: &str) -> String {
    let b = text.as_bytes();
    let digits = |r: std::ops::Range<usize>| r.end <= b.len() && b[r].iter().all(u8::is_ascii_digit);
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        let stamp = digits(i..i + 4) && b.get(i + 4) == Some(&b'-') && digits(i + 5..i + 7) && b.get(i + 7) == Some(&b'-') && digits(i + 8..i + 10) && b.get(i + 10) == Some(&b'T') && digits(i + 11..i + 13) && b.get(i + 13) == Some(&b':') && digits(i + 14..i + 16);
        if stamp {
            i += 16;
            while i < b.len() && (b[i].is_ascii_digit() || matches!(b[i], b':' | b'.' | b'Z')) {
                i += 1;
            }
            out.push_str("<time>");
            continue;
        }
        let hex = b[i..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
        if hex >= 64 {
            i += hex;
            out.push_str("<hash>");
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden").join("rules_unset.txt")
}

#[test]
fn unset_answers_as_the_release_before_the_feature_did() {
    let t = Temp::new("rules-golden");
    let root = t.path();
    write(&root.join("handbook").join("RULES.md"), RULES);
    write(&root.join("memory").join("MEMORY.md"), INDEX);
    write(&root.join("memory").join("widget-note.md"), MEM_A);
    write(&root.join("memory").join("export-note.md"), MEM_B);
    std::fs::create_dir_all(root.join("vault")).unwrap();
    let cfg = config(root);
    rescan(root, &cfg);
    write(&root.join("handbook").join("RULES.md"), RULES_EDITED);
    rescan(root, &cfg);

    let mut got = String::new();
    for (label, answer) in recalls(root, &cfg).into_iter().chain(hooks(root, &cfg)) {
        got.push_str(&format!("=== {label}\n{}\n", mask(&answer).trim_end()));
    }
    let got = got.replace(&root.to_string_lossy().to_string(), "<root>");
    assert!(got.contains("rules:r00002") && got.contains("widget-note"), "the fixture must reach the store: {got}");

    if std::env::var_os("ATLAS_GOLDEN_WRITE").is_some() {
        write(&golden_path(), &got);
        return;
    }
    let want = std::fs::read_to_string(golden_path()).unwrap().replace("\r\n", "\n");
    assert_eq!(got.replace("\r\n", "\n"), want);
}
