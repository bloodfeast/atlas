//! MCP over stdio: newline-delimited JSON-RPC 2.0 (MCP 2025-06-18).
//!
//! The server only answers requests; it never sends requests of its own and
//! never opens anything but stdin, stdout and stderr.

use std::io::{self, BufRead, Read, Write};

use crate::json::{Map, Value};
use crate::json;

use crate::tools::{self, Server};

/// The protocol version this server speaks natively.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The longest message accepted, in bytes (4 MiB).
pub const MAX_MESSAGE: u64 = 4 << 20;

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn ok(id: Value, result: Value) -> Value {
    // Moved in, not copied (`json!` copies an expression's value).
    let mut m = Map::new();
    m.insert("jsonrpc".into(), Value::from("2.0"));
    m.insert("id".into(), id);
    m.insert("result".into(), result);
    Value::Object(m)
}

/// A `tools/call` result: the answer text moved into one text content item.
fn content(text: String, is_error: bool) -> Value {
    let mut c = Map::new();
    c.insert("type".into(), Value::from("text"));
    c.insert("text".into(), Value::String(text));
    let mut m = Map::new();
    m.insert("content".into(), Value::Array(vec![Value::Object(c)]));
    m.insert("isError".into(), Value::Bool(is_error));
    Value::Object(m)
}

/// Handles one parsed message.
///
/// # Arguments
///
/// * `server` - the tool server.
/// * `msg` - the JSON-RPC message.
///
/// # Returns
///
/// The response, or `None` for notifications and stray responses.
pub fn handle(server: &mut Server, msg: &Value) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(error(Value::Null, -32600, "message must be a JSON object"));
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // A response to a request we never send, or garbage: ignore
        // responses, reject the rest.
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        return Some(error(id.unwrap_or(Value::Null), -32600, "missing method"));
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return id.map(|id| error(id, -32600, "jsonrpc must be \"2.0\""));
    }
    let id = id?;
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
            let version = if SUPPORTED.contains(&asked) { asked } else { PROTOCOL_VERSION };
            if let Some(name) = params.pointer("/clientInfo/name").and_then(Value::as_str) {
                server.set_client(name);
            }
            Some(ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}, "resources": {}},
                    "serverInfo": {"name": "atlas", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": server.instructions()
                }),
            ))
        }
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(id, json!({ "tools": tools::tool_list() }))),
        // One resource: the tool reference the short tool list leaves out.
        "resources/list" => Some(ok(
            id,
            json!({"resources": [{"uri": tools::HELP_URI, "name": "atlas tools", "description": "Every tool, op and argument.", "mimeType": "text/markdown"}]}),
        )),
        "resources/templates/list" => Some(ok(id, json!({"resourceTemplates": []}))),
        "resources/read" => match params.get("uri").and_then(Value::as_str) {
            Some(tools::HELP_URI) => {
                Some(ok(id, json!({"contents": [{"uri": tools::HELP_URI, "mimeType": "text/markdown", "text": tools::help()}]})))
            }
            Some(u) => Some(error(id, -32002, &format!("resource not found: {u}"))),
            None => Some(error(id, -32602, "resources/read needs params.uri")),
        },
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error(id, -32602, "tools/call needs params.name"));
            };
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            // The answer goes out once, as text: no structuredContent copy
            // (no tool declares an output schema), so a client reads each
            // byte once.
            // Claude Code names the tool_use_id of a call in `_meta`; the
            // PreToolUse hook names the same id with the agent.
            let use_id = params.get("_meta").and_then(|m| m.as_object()).and_then(|m| m.iter().find(|(k, _)| k.ends_with("toolUseId")).and_then(|(_, v)| v.as_str()));
            match server.answer_call(name, &args, use_id) {
                Ok(answer) => Some(ok(id, content(answer.into_text(), false))),
                Err(crate::error::Error::NotFound(m)) if m.starts_with("tool ") => {
                    Some(error(id, -32602, &format!("unknown tool: {}", &m[5..])))
                }
                Err(e) => Some(ok(id, content(e.to_string(), true))),
            }
        }
        _ => Some(error(id, -32601, &format!("method not found: {method}"))),
    }
}

/// Serves until `input` closes.
///
/// # Arguments
///
/// * `server` - the tool server.
/// * `input` - newline-delimited JSON-RPC messages (stdin).
/// * `output` - where responses go, one per line (stdout).
///
/// # Errors
///
/// An I/O error reading `input` or writing `output`.
pub fn serve(server: &mut Server, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = (&mut input).take(MAX_MESSAGE + 1).read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        if buf.len() as u64 > MAX_MESSAGE && !buf.ends_with(b"\n") {
            // Discard the rest of the oversized line.
            let mut sink = Vec::new();
            loop {
                sink.clear();
                let m = (&mut input).take(MAX_MESSAGE).read_until(b'\n', &mut sink)?;
                if m == 0 || sink.ends_with(b"\n") {
                    break;
                }
            }
            write_line(&mut output, &error(Value::Null, -32700, "message too large"))?;
            continue;
        }
        let line = String::from_utf8_lossy(&buf);
        // A Windows client may open the pipe with a UTF-8 byte order mark.
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        let response = match crate::json::parse(line) {
            Ok(msg) => handle(server, &msg),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(r) = response {
            write_line(&mut output, &r)?;
        }
    }
}

fn write_line(out: &mut impl Write, v: &Value) -> io::Result<()> {
    let mut s = v.to_string();
    s.push('\n');
    out.write_all(s.as_bytes())?;
    out.flush()
}

/// One line read from the client: a parsed message, an error response to
/// send back, or the end of input.
enum Incoming {
    Message(Value),
    Reply(Value),
    Eof,
}

fn read_incoming(input: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<Incoming> {
    loop {
        buf.clear();
        let n = (&mut *input).take(MAX_MESSAGE + 1).read_until(b'\n', buf)?;
        if n == 0 {
            return Ok(Incoming::Eof);
        }
        if buf.len() as u64 > MAX_MESSAGE && !buf.ends_with(b"\n") {
            let mut sink = Vec::new();
            loop {
                sink.clear();
                let m = (&mut *input).take(MAX_MESSAGE).read_until(b'\n', &mut sink)?;
                if m == 0 || sink.ends_with(b"\n") {
                    break;
                }
            }
            return Ok(Incoming::Reply(error(Value::Null, -32700, "message too large")));
        }
        let line = String::from_utf8_lossy(buf);
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        return Ok(match crate::json::parse(line) {
            Ok(msg) => Incoming::Message(msg),
            Err(e) => Incoming::Reply(error(Value::Null, -32700, &format!("parse error: {e}"))),
        });
    }
}

/// A `tools/call` of the `hook` tool: its id and arguments.
fn hook_call(msg: &Value) -> Option<(Value, Value)> {
    if msg.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let params = msg.get("params")?;
    if params.get("name").and_then(Value::as_str) != Some("hook") {
        return None;
    }
    Some((msg.get("id")?.clone(), params.get("arguments").cloned().unwrap_or(Value::Null)))
}

/// Serves until `input` closes, answering `hook` calls on their own worker
/// thread so a hook never waits behind a slow tool call (answers may go out
/// of order; JSON-RPC matches them by id). Everything else is answered in
/// order on the calling thread, as [`serve`] does.
///
/// # Arguments
///
/// * `server` - the tool server.
/// * `hooks` - the hook settings; the worker opens its hook state on its own
///   thread at start. `None` answers `hook` through `server` like any tool.
/// * `index` - opens the index store on the worker's thread, so a note or
///   source file an Edit or Write changed is re-indexed at once (code files
///   need only the database). `None` re-indexes code only.
/// * `input` - newline-delimited JSON-RPC messages (stdin).
/// * `output` - where responses go, one per line (stdout).
///
/// # Errors
///
/// An I/O error reading `input` or writing `output`.
pub fn serve_with_hooks(
    server: &mut Server,
    hooks: Option<crate::memory::settings::Settings>,
    index: Option<crate::hooks::StoreOpener>,
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
) -> io::Result<()> {
    use std::sync::{mpsc, Arc, Mutex};
    let out = Arc::new(Mutex::new(output));
    let (main_tx, main_rx) = mpsc::channel::<(Incoming, u64)>();
    let (hook_tx, hook_worker) = match hooks {
        Some(settings) => {
            // `None` is the warm-up signal: the main thread sends it once its
            // first answer is out, so the worker opens its store while the main
            // thread is idle (opening both at start measured +8 ms on the first
            // answer from SQLite busy waits; opening on the first hook, 22 ms
            // on that hook).
            let (tx, rx) = mpsc::channel::<Option<(Value, Value)>>();
            let o = out.clone();
            let worker = std::thread::Builder::new().name("atlas-hooks".into()).spawn(move || {
                let mut hooks: Option<crate::hooks::Hooks> = None;
                let mut index = index;
                let mut open = |hooks: &mut Option<crate::hooks::Hooks>| {
                    if hooks.is_none() {
                        match crate::hooks::Hooks::open(settings.clone()) {
                            Ok(mut h) => {
                                // A hook over this route may come from anyone's
                                // tools/call, and the server outlives any one
                                // session: nothing says a user is there to answer.
                                h.set_unattended(true);
                                h.set_index_opener(index.take());
                                h.warm();
                                *hooks = Some(h);
                            }
                            Err(e) => eprintln!("atlas hooks: {e}"),
                        }
                    }
                };
                for msg in rx {
                    open(&mut hooks);
                    let Some((id, args)) = msg else { continue };
                    let ev = crate::hooks::HookEvent::from_value(&args);
                    let text = hooks.as_mut().map(|h| h.handle(&ev).to_json(&ev.event)).unwrap_or_default();
                    let r = ok(id, content(text, false));
                    if let Ok(mut w) = o.lock() {
                        let _ = write_line(&mut *w, &r);
                    }
                    // Re-indexing an edited file waits until the answer is out.
                    if let Some(h) = hooks.as_mut() {
                        h.after();
                    }
                }
            })?;
            (Some(tx), Some(worker))
        }
        None => (None, None),
    };
    let mut warm_tx = hook_tx.clone();
    // The reader notes each hook as it reads it, before any call it
    // announces can reach the main thread, and sends every other message
    // with the mark of the hooks read before it: a call sees exactly the
    // hooks that came before it in the stream, however far ahead the
    // reader has read (see [`crate::tools::dedup::Announcer`]).
    let announcer = server.announcer();
    let reader = std::thread::Builder::new().name("atlas-reader".into()).spawn(move || -> io::Result<()> {
        let mut input = input;
        let mut buf = Vec::new();
        loop {
            match read_incoming(&mut input, &mut buf)? {
                Incoming::Eof => return Ok(()),
                Incoming::Message(m) => {
                    if let (Some(tx), Some(call)) = (&hook_tx, hook_call(&m)) {
                        let args = call.1.clone();
                        // Noted once: by the reader when the worker takes
                        // it, else by the main thread as it answers it.
                        if tx.send(Some(call)).is_ok() {
                            announcer.hook(&args);
                            continue;
                        }
                    }
                    if main_tx.send((Incoming::Message(m), announcer.mark())).is_err() {
                        return Ok(());
                    }
                }
                r @ Incoming::Reply(_) => {
                    if main_tx.send((r, announcer.mark())).is_err() {
                        return Ok(());
                    }
                }
            }
        }
    })?;
    let mut result = Ok(());
    for (inc, mark) in main_rx {
        let response = match inc {
            Incoming::Message(m) => {
                server.set_horizon(Some(mark));
                handle(server, &m)
            }
            Incoming::Reply(r) => Some(r),
            Incoming::Eof => None,
        };
        if let Some(r) = response {
            let mut w = out.lock().map_err(|_| io::Error::other("output lock poisoned"))?;
            if let Err(e) = write_line(&mut *w, &r) {
                result = Err(e);
                break;
            }
            drop(w);
            if let Some(tx) = &warm_tx {
                let _ = tx.send(None);
            }
            warm_tx = None;
        }
    }
    server.set_horizon(None);
    // On an error the reader may be blocked on input; it ends with the process.
    result?;
    drop(warm_tx);
    let read = reader.join().unwrap_or_else(|_| Err(io::Error::other("reader thread panicked")));
    if let Some(w) = hook_worker {
        let _ = w.join();
    }
    read
}
