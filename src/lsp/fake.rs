//! A scripted language server for the tests: it answers
//! `textDocument/documentSymbol` from the opened text itself, reading
//! Python-shaped lines (`class NAME`, `def NAME`, `NAME = ...`) into a
//! `DocumentSymbol` tree (or a flat `SymbolInformation` list), and behaves
//! as its script says (a version, UTF-16 columns, a hang, a bad frame, a
//! server that lingers after `exit`). It runs on a thread over pipes
//! ([`start`], a launch whose program is [`SCHEME`]`N`), or as the `atlas`
//! binary of a test build (the `test-seams` feature, `ATLAS_TEST_LSP_FAKE`
//! holding the script, run with `--stdio`).

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use super::{read_frame, write_frame, Out};
use crate::json::Value;

/// The program-name prefix of a launch served on a thread.
pub const SCHEME: &str = "fake-lsp:";

/// What the fake does.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// The `serverInfo` version.
    pub version: String,
    /// Columns in UTF-16 code units (else bytes).
    pub utf16: bool,
    /// A flat `SymbolInformation` list (else a tree).
    pub flat: bool,
    /// Answers this many symbol requests, then none.
    pub hang_after: Option<usize>,
    /// Sends a bad frame for the first symbol request.
    pub bad_frame: bool,
    /// Keeps running for a minute after `exit`.
    pub linger: bool,
    /// Asks `workspace/configuration` and `window/workDoneProgress/create`
    /// during `initialize`, and logs the replies.
    pub ask_config: bool,
    /// A file each symbol request is logged to (`ask NAME`).
    pub log: Option<PathBuf>,
    /// Names it never gives (a definition the scanner fills).
    pub hide: Vec<String>,
    /// Answers every symbol request with an error (`no view`).
    pub fail: bool,
    /// Stops reading its input after `initialize` (a wedged event loop) and
    /// logs `beat` every 100 ms for a minute, so a test can tell it is gone.
    pub deaf: bool,
}

impl Script {
    /// Reads a script from JSON (`{"version": "1.2", "utf16": true, ...}`).
    pub fn parse(text: &str) -> Script {
        let v = crate::json::parse(text).unwrap_or(Value::Null);
        let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        Script {
            version: v.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
            utf16: b("utf16"),
            flat: b("flat"),
            hang_after: v.get("hang_after").and_then(Value::as_u64).map(|n| n as usize),
            bad_frame: b("bad_frame"),
            linger: b("linger"),
            ask_config: b("ask_config"),
            log: v.get("log").and_then(Value::as_str).map(PathBuf::from),
            fail: b("fail"),
            deaf: b("deaf"),
            hide: v
                .get("hide")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
        }
    }

    fn log(&self, line: &str) {
        if let Some(p) = &self.log {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

static SCRIPTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Registers a script for [`start`].
///
/// # Returns
///
/// The program name a launch runs it by ([`SCHEME`]`N`).
pub fn register(script: &str) -> String {
    let mut s = SCRIPTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    s.push(script.to_string());
    format!("{SCHEME}{}", s.len() - 1)
}

/// Starts a registered script on a thread.
///
/// # Returns
///
/// The server's output and input.
///
/// # Errors
///
/// When the pipes cannot be made or the name is not registered.
pub fn start(name: &str) -> io::Result<(io::PipeReader, Out)> {
    let text = name
        .parse::<usize>()
        .ok()
        .and_then(|i| SCRIPTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(i).cloned())
        .ok_or_else(|| io::Error::other(format!("no fake server {name}")))?;
    let script = Script::parse(&text);
    let (client_r, server_w) = io::pipe()?;
    let (server_r, client_w) = io::pipe()?;
    std::thread::spawn(move || serve(server_r, server_w, &script));
    Ok((client_r, Box::new(client_w)))
}

fn send(w: &mut dyn Write, v: &Value) -> bool {
    write_frame(w, &v.to_string()).is_ok()
}

/// Serves one client until `exit` or the end of its input.
pub fn serve(r: impl Read, mut w: impl Write, s: &Script) {
    let mut br = BufReader::new(r);
    let mut docs: HashMap<String, String> = HashMap::new();
    let mut answered = 0usize;
    loop {
        let Ok(Some(body)) = read_frame(&mut br) else { return };
        let Ok(v) = crate::json::parse_bytes(&body) else { return };
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let reply = |w: &mut dyn Write, result: Value| send(w, &crate::json!({"jsonrpc": "2.0", "id": id.clone(), "result": result}));
        match v.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                if s.ask_config {
                    send(
                        &mut w,
                        &crate::json!({"jsonrpc": "2.0", "id": "c1", "method": "workspace/configuration", "params": {"items": [{"section": "python"}, {"section": "python.analysis"}]}}),
                    );
                    send(
                        &mut w,
                        &crate::json!({"jsonrpc": "2.0", "id": "c2", "method": "window/workDoneProgress/create", "params": {"token": "t"}}),
                    );
                    send(
                        &mut w,
                        &crate::json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 3, "message": "hello"}}),
                    );
                    for _ in 0..2 {
                        let Ok(Some(b)) = read_frame(&mut br) else { return };
                        let r = crate::json::parse_bytes(&b).unwrap_or(Value::Null);
                        let rid = r.get("id").and_then(Value::as_str).unwrap_or("");
                        s.log(&format!("reply {rid} {}", r.get("result").map_or_else(|| "none".to_string(), Value::to_string)));
                    }
                }
                let enc = if s.utf16 { "utf-16" } else { "utf-8" };
                reply(
                    &mut w,
                    crate::json!({"capabilities": {"positionEncoding": enc, "documentSymbolProvider": true}, "serverInfo": {"name": "fake", "version": s.version.clone()}}),
                );
                if s.deaf {
                    for _ in 0..600 {
                        s.log("beat");
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    return;
                }
            }
            Some("textDocument/didOpen") => {
                let doc = v.get("params").and_then(|p| p.get("textDocument"));
                let field = |k: &str| doc.and_then(|d| d.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
                docs.insert(field("uri"), field("text"));
            }
            Some("textDocument/documentSymbol") => {
                if s.bad_frame {
                    let _ = w.write_all(b"Content-Length: nope\r\n\r\n{}");
                    let _ = w.flush();
                    continue;
                }
                if s.hang_after.is_some_and(|n| answered >= n) {
                    continue;
                }
                answered += 1;
                let uri =
                    v.get("params").and_then(|p| p.get("textDocument")).and_then(|d| d.get("uri")).and_then(Value::as_str).unwrap_or("");
                s.log(&format!("ask {}", uri.rsplit('/').next().unwrap_or(uri)));
                if s.fail {
                    send(&mut w, &crate::json!({"jsonrpc": "2.0", "id": id.clone(), "error": {"code": -32603, "message": "no view"}}));
                    continue;
                }
                let text = docs.get(uri).cloned().unwrap_or_default();
                reply(&mut w, symbols(&text, uri, s));
            }
            Some("shutdown") => {
                reply(&mut w, Value::Null);
            }
            Some("exit") => {
                if s.linger {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                }
                return;
            }
            _ => {}
        }
    }
}

/// One item read from a line.
struct Item {
    kind: i64,
    name: String,
    line: usize,
    indent: usize,
    /// The name's column, in the script's units.
    col: usize,
    end: usize,
    children: Vec<Item>,
}

/// The items of a Python-shaped text, nested by indentation.
fn items(text: &str, utf16: bool) -> Vec<Item> {
    let lines: Vec<&str> = text.lines().collect();
    let mut flat: Vec<(usize, Item)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let body = l.trim_start();
        let indent = l.len() - body.len();
        let body2 = body.strip_prefix("async ").unwrap_or(body);
        let (kind, rest) = if let Some(r) = body2.strip_prefix("class ") {
            (5, r)
        } else if let Some(r) = body2.strip_prefix("def ") {
            (12, r)
        } else {
            (13, body)
        };
        let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if name.is_empty()
            || (kind == 13 && !rest[name.len()..].trim_start().starts_with('='))
            || rest[name.len()..].trim_start().starts_with("==")
        {
            continue;
        }
        let at = l.len() - rest.len();
        let col = if utf16 { l[..at].encode_utf16().count() } else { at };
        // The item ends before the next non-blank line indented no deeper.
        let end = (i + 1..lines.len())
            .find(|&j| !lines[j].trim().is_empty() && lines[j].len() - lines[j].trim_start().len() <= indent)
            .map_or(lines.len() - 1, |j| j - 1);
        let end = (i..=end).rev().find(|&j| !lines[j].trim().is_empty()).unwrap_or(i);
        flat.push((indent, Item { kind, name, line: i, indent, col, end, children: Vec::new() }));
    }
    // Nest: each item under the nearest open one indented less.
    let mut roots: Vec<Item> = Vec::new();
    let mut stack: Vec<Item> = Vec::new();
    for (indent, mut it) in flat {
        while stack.last().is_some_and(|p| p.indent >= indent) {
            let done = stack.pop().expect("non-empty");
            attach(&mut stack, &mut roots, done);
        }
        if it.kind == 12 && stack.last().is_some_and(|p| p.kind == 5) {
            it.kind = 6;
        }
        stack.push(it);
    }
    while let Some(done) = stack.pop() {
        attach(&mut stack, &mut roots, done);
    }
    roots
}

fn attach(stack: &mut [Item], roots: &mut Vec<Item>, it: Item) {
    match stack.last_mut() {
        Some(p) => p.children.push(it),
        None => roots.push(it),
    }
}

fn range(it: &Item) -> Value {
    crate::json!({"start": {"line": it.line, "character": it.indent}, "end": {"line": it.end + 1, "character": 0}})
}

fn tree(it: &Item) -> Value {
    let sel =
        crate::json!({"start": {"line": it.line, "character": it.col}, "end": {"line": it.line, "character": it.col + it.name.len()}});
    let children: Vec<Value> = it.children.iter().map(tree).collect();
    crate::json!({"name": it.name.clone(), "kind": it.kind, "range": range(it), "selectionRange": sel, "children": children})
}

fn flatten(it: &Item, uri: &str, parent: &str, out: &mut Vec<Value>) {
    out.push(
        crate::json!({"name": it.name.clone(), "kind": it.kind, "containerName": parent, "location": {"uri": uri, "range": range(it)}}),
    );
    for c in &it.children {
        flatten(c, uri, &it.name, out);
    }
}

/// The answer to a symbol request for `text`.
fn symbols(text: &str, uri: &str, s: &Script) -> Value {
    let mut roots = items(text, s.utf16);
    roots.retain(|r| !s.hide.contains(&r.name));
    if s.flat {
        let mut out = Vec::new();
        for r in &roots {
            flatten(r, uri, "", &mut out);
        }
        Value::Array(out)
    } else {
        Value::Array(roots.iter().map(tree).collect())
    }
}
