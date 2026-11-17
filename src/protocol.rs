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
    let (id, method, params) = match request(msg) {
        Ok(r) => r,
        Err(reply) => return reply,
    };
    let method = method.as_str();
    match method {
        "initialize" => {
            if let Some(name) = params.pointer("/clientInfo/name").and_then(Value::as_str) {
                server.set_client(name);
            }
            Some(initialize(id, &params, server.instructions()))
        }
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
            let use_id = meta_use_id(&params);
            match server.answer_call(name, &args, use_id) {
                Ok(answer) => Some(ok(id, content(answer.into_text(), false))),
                Err(crate::error::Error::NotFound(m)) if m.starts_with("tool ") => {
                    Some(error(id, -32602, &format!("unknown tool: {}", &m[5..])))
                }
                Err(e) => Some(ok(id, content(e.to_string(), true))),
            }
        }
        _ => Some(plain(id, method, &params)),
    }
}

/// A message's request parts (id, method, params), or the reply to send
/// instead (`None` for notifications and stray responses).
fn request(msg: &Value) -> std::result::Result<(Value, String, Value), Option<Value>> {
    let Some(obj) = msg.as_object() else {
        return Err(Some(error(Value::Null, -32600, "message must be a JSON object")));
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // A response to a request we never send, or garbage: ignore
        // responses, reject the rest.
        if obj.contains_key("result") || obj.contains_key("error") {
            return Err(None);
        }
        return Err(Some(error(id.unwrap_or(Value::Null), -32600, "missing method")));
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(id.map(|id| error(id, -32600, "jsonrpc must be \"2.0\"")));
    }
    let Some(id) = id else { return Err(None) };
    Ok((id, method.to_string(), obj.get("params").cloned().unwrap_or(Value::Null)))
}

/// The `initialize` result, with `instructions`.
fn initialize(id: Value, params: &Value, instructions: String) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
    let version = if SUPPORTED.contains(&asked) { asked } else { PROTOCOL_VERSION };
    ok(
        id,
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}, "resources": {}},
            "serverInfo": {"name": "atlas", "version": env!("CARGO_PKG_VERSION")},
            "instructions": instructions
        }),
    )
}

/// The answers that need no tool server: ping, the tool list, the help
/// resource, and "method not found".
fn plain(id: Value, method: &str, params: &Value) -> Value {
    match method {
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tools::tool_list() })),
        // One resource: the tool reference the short tool list leaves out.
        "resources/list" => ok(
            id,
            json!({"resources": [{"uri": tools::HELP_URI, "name": "atlas tools", "description": "Every tool, op and argument.", "mimeType": "text/markdown"}]}),
        ),
        "resources/templates/list" => ok(id, json!({"resourceTemplates": []})),
        "resources/read" => match params.get("uri").and_then(Value::as_str) {
            Some(tools::HELP_URI) => ok(id, json!({"contents": [{"uri": tools::HELP_URI, "mimeType": "text/markdown", "text": tools::help()}]})),
            Some(u) => error(id, -32002, &format!("resource not found: {u}")),
            None => error(id, -32602, "resources/read needs params.uri"),
        },
        _ => error(id, -32601, &format!("method not found: {method}")),
    }
}

/// Serves until `input` closes while the index store is refused (`refusal`, an
/// [`crate::Error::StoreTooNew`] from opening it at start): the server starts
/// and stays up, so Claude Code does not hold it off, but every tool call
/// answers the refusal as its error and the `hook` tool tells it once per
/// session (see [`crate::hooks::RefusalNotice`]). Nothing opens a store.
///
/// # Errors
///
/// An I/O error reading `input` or writing `output`.
pub fn serve_refused(refusal: &crate::Error, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut buf = Vec::new();
    let mut notice = crate::hooks::RefusalNotice::default();
    let text = format!("atlas is off: {refusal}");
    loop {
        let msg = match read_incoming(&mut input, &mut buf).map_err(|e| on("stdin read", e))? {
            Incoming::Eof => return Ok(()),
            Incoming::Reply(r) => {
                write_line(&mut output, &r).map_err(|e| on("stdout write", e))?;
                continue;
            }
            Incoming::Message(m) => m,
        };
        let (id, method, params) = match request(&msg) {
            Ok(r) => r,
            Err(Some(r)) => {
                write_line(&mut output, &r).map_err(|e| on("stdout write", e))?;
                continue;
            }
            Err(None) => continue,
        };
        let r = match method.as_str() {
            "initialize" => initialize(id, &params, text.clone()),
            "tools/call" if params.get("name").and_then(Value::as_str) == Some("hook") => {
                let ev = crate::hooks::HookEvent::from_value(&params.get("arguments").cloned().unwrap_or(Value::Null));
                ok(id, content(notice.answer(refusal, &ev), false))
            }
            "tools/call" => ok(id, content(text.clone(), true)),
            m => plain(id, m, &params),
        };
        write_line(&mut output, &r).map_err(|e| on("stdout write", e))?;
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
    out.flush()?;
    crate::serve_log::responded();
    Ok(())
}

/// `e` with the stream it came from in its text. The text goes to stderr
/// only; the server log takes just the error's kind.
fn on(stream: &str, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{stream}: {e}"))
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
    // A model's call of the tool carries its toolUseId in `_meta`; a hook
    // does not. Marked over whatever the arguments say.
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    Some((msg.get("id")?.clone(), crate::hooks::mark_tool_use(&args, meta_use_id(params))))
}

/// The tool call's id Claude Code names in a request's `_meta` (a key
/// ending in `toolUseId`).
fn meta_use_id(params: &Value) -> Option<&str> {
    params.get("_meta").and_then(|m| m.as_object()).and_then(|m| m.iter().find(|(k, _)| k.ends_with("toolUseId")).and_then(|(_, v)| v.as_str()))
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
    serve_with_hooks_every(server, hooks, index, input, output, crate::restart::REFRESH_EVERY)
}

/// [`serve_with_hooks`] with the restart point refreshed every `every`
/// (tests use a short one).
///
/// # Errors
///
/// As [`serve_with_hooks`].
#[doc(hidden)]
pub fn serve_with_hooks_every(
    server: &mut Server,
    hooks: Option<crate::memory::settings::Settings>,
    index: Option<crate::hooks::StoreOpener>,
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
    every: std::time::Duration,
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
                // Why the last open failed: a store too new for this binary is
                // told once per session (the open is tried again on every
                // hook, so installing a newer atlas recovers without a restart).
                let mut failed: Option<crate::Error> = None;
                let mut notice = crate::hooks::RefusalNotice::default();
                let mut open = |hooks: &mut Option<crate::hooks::Hooks>, failed: &mut Option<crate::Error>| {
                    if hooks.is_none() {
                        match crate::hooks::Hooks::open(settings.clone()) {
                            Ok(mut h) => {
                                h.detect_offer_paths();
                                // A hook over this route may come from anyone's
                                // tools/call, and the server outlives any one
                                // session: nothing says a user is there to answer.
                                h.serve_setup();
                                h.set_index_opener(index.take());
                                // The restart point the shutdown catch writes (#221).
                                h.enable_restart_point();
                                // Its advance warning: a restart pending or scheduled.
                                h.set_restart_warnings(crate::restart_warn::start());
                                if let Some(w) = h.binding_warning() {
                                    eprintln!("{w}");
                                }
                                h.warm();
                                *hooks = Some(h);
                                *failed = None;
                            }
                            Err(e) => {
                                if failed.as_ref() != Some(&e) {
                                    eprintln!("atlas hooks: {e}");
                                }
                                *failed = Some(e);
                            }
                        }
                    }
                };
                // The restart point's refresh is a deadline, not a timeout
                // per message (#221): steady hook traffic still refreshes it,
                // after a hook's answer is out, never on its path.
                let mut due = std::time::Instant::now() + every;
                let refresh = |hooks: &mut Option<crate::hooks::Hooks>, due: &mut std::time::Instant| {
                    if let Some(h) = hooks.as_mut() {
                        h.refresh_restart_point();
                    }
                    *due = std::time::Instant::now() + every;
                };
                loop {
                    let msg = match rx.recv_timeout(due.saturating_duration_since(std::time::Instant::now())) {
                        Ok(m) => m,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            refresh(&mut hooks, &mut due);
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    open(&mut hooks, &mut failed);
                    let Some((id, args)) = msg else { continue };
                    let ev = crate::hooks::HookEvent::from_value(&args);
                    // A panic in hook code (#253) answers empty and drops the
                    // hook state; `open` reopens it on the next message.
                    let (text, panicked) = match (hooks.as_mut(), &failed) {
                        (Some(h), _) => crate::hooks::guard(&settings, &ev, || h.handle(&ev).to_json(&ev.event)),
                        (None, Some(e)) => (notice.answer(e, &ev), false),
                        (None, None) => (String::new(), false),
                    };
                    if panicked {
                        hooks = None;
                    }
                    let r = ok(id, content(text, false));
                    if let Ok(mut w) = o.lock() {
                        let _ = write_line(&mut *w, &r);
                    }
                    // Re-indexing an edited file waits until the answer is out.
                    if let Some(h) = hooks.as_mut() {
                        if crate::hooks::guard(&settings, &ev, || h.after()).1 {
                            hooks = None;
                        }
                    }
                    if std::time::Instant::now() >= due {
                        refresh(&mut hooks, &mut due);
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
            match read_incoming(&mut input, &mut buf).map_err(|e| on("stdin read", e))? {
                Incoming::Eof => {
                    // Its time goes to the shutdown log (#221, Q10).
                    crate::restart::stdin_closed();
                    return Ok(());
                }
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
                result = Err(on("stdout write", e));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn a_hook_call_is_marked_by_its_meta_not_its_arguments() {
        let call = |meta: Value| crate::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "hook", "arguments": {"event": "UserPromptSubmit", "_tool_use": false}, "_meta": meta}});
        let (_, args) = hook_call(&call(crate::json!({"claudecode/toolUseId": "toolu_1"}))).unwrap();
        assert_eq!(args.get(crate::hooks::TOOL_USE_ARG), Some(&Value::Bool(true)), "a model's tool call");
        let (_, args) = hook_call(&call(Value::Null)).unwrap();
        assert_eq!(args.get(crate::hooks::TOOL_USE_ARG), Some(&Value::Bool(false)), "a hook");
    }

    /// A `Write` the test reads back after the server is done with it.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().map_err(|_| io::Error::other("poisoned"))?.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A server over one vault in a fresh temp folder, and the hook
    /// settings over its index. The server has no memory store, so a hook
    /// that falls through to the main thread is answered with an error: an
    /// answer without one came from the `atlas-hooks` worker.
    fn served(tag: &str) -> (std::path::PathBuf, Server, crate::memory::settings::Settings) {
        let dir = std::env::temp_dir().join(format!("atlas-proto-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vault = dir.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(vault.join("A.md"), "# A\n\nalpha\n").unwrap();
        let db = dir.join("state").join("vaults.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let mut store = crate::store::Store::open(&db, crate::paths::Vaults::new(&[("v".into(), vault)]).unwrap()).unwrap();
        store.rescan().unwrap();
        let server = Server::new(store);
        let settings = crate::memory::settings::Settings::from_config(None, &db).unwrap();
        (dir, server, settings)
    }

    fn hook_msg(id: &str, tool: &str) -> String {
        let args =
            crate::json!({"event": "PreToolUse", "session_id": "S", "tool_name": tool, "tool_input": {"todos": []}, "tool_use_id": id});
        crate::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hook", "arguments": args}}).to_string()
    }

    /// Each answer by id: (its text, whether it is an error).
    fn answers(out: &Shared) -> HashMap<String, (String, bool)> {
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let mut m = HashMap::new();
        for l in text.lines() {
            let v = crate::json::parse(l).unwrap();
            let id = match &v["id"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let r = &v["result"];
            let err = v.get("error").is_some() || r["isError"] == Value::Bool(true);
            m.insert(id, (r["content"][0]["text"].as_str().unwrap_or("").to_string(), err));
        }
        m
    }

    fn panic_misses(db: &std::path::Path) -> i64 {
        let c = rusqlite::Connection::open(db).unwrap();
        c.query_row("SELECT count(*) FROM misses WHERE query LIKE 'hook panic %' AND client = 'atlas-hook'", [], |r| r.get(0)).unwrap()
    }

    /// #253: a panic while answering a hook passes that call through
    /// untouched; later hooks and tool calls are answered as before.
    #[test]
    fn a_panicking_hook_answers_empty_and_the_server_goes_on() {
        let (dir, mut server, settings) = served("hook-panic");
        let db = settings.index_db.clone();
        let call = crate::json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "note", "arguments": {"vault": "v", "path": "A.md"}}}).to_string();
        let msgs = [hook_msg("h1", crate::hooks::TEST_PANIC_TOOL), hook_msg("h2", "TodoWrite"), call];
        let input = io::Cursor::new((msgs.join("\n") + "\n").into_bytes());
        let out = Shared::default();
        serve_with_hooks(&mut server, Some(settings), None, input, out.clone()).unwrap();
        let a = answers(&out);
        assert_eq!(a.get("h1"), Some(&(String::new(), false)), "{a:?}");
        assert_eq!(a.get("h2").map(|x| x.1), Some(false), "{a:?}");
        let note = a.get("7").unwrap_or_else(|| panic!("no tools/call answer: {a:?}"));
        assert!(!note.1 && note.0.contains("alpha"), "{note:?}");
        assert_eq!(panic_misses(&db), 1);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Many panicking hooks in one stream: each is answered by the one
    /// worker, which reopens its hook state after each panic.
    #[test]
    fn many_panicking_hooks_are_each_answered_by_the_worker() {
        let (dir, mut server, settings) = served("hook-panic-many");
        let db = settings.index_db.clone();
        let msgs: Vec<String> = (0..200).map(|i| hook_msg(&format!("p{i}"), crate::hooks::TEST_PANIC_TOOL)).collect();
        let input = io::Cursor::new((msgs.join("\n") + "\n").into_bytes());
        let out = Shared::default();
        serve_with_hooks(&mut server, Some(settings), None, input, out.clone()).unwrap();
        let a = answers(&out);
        assert_eq!(a.len(), 200, "{a:?}");
        // No answer came from the main thread's fallback (an error here).
        assert!(a.values().all(|x| x == &(String::new(), false)), "{a:?}");
        assert_eq!(panic_misses(&db), 200);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
