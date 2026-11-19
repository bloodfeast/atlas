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
                    Some(error(id, -32602, &crate::hidden::scrub(&format!("unknown tool: {}", &m[5..]))))
                }
                // An error can echo an argument or a path: scrubbed as an answer is.
                Err(e) => Some(ok(id, content(crate::hidden::scrub(&e.to_string()).into_owned(), true))),
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

/// The `initialize` result, with `instructions` (vault folder and source
/// names from the file system, so scrubbed).
fn initialize(id: Value, params: &Value, instructions: String) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
    let version = if SUPPORTED.contains(&asked) { asked } else { PROTOCOL_VERSION };
    ok(
        id,
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}, "resources": {}},
            "serverInfo": {"name": "atlas", "version": env!("CARGO_PKG_VERSION")},
            "instructions": crate::hidden::scrub(&instructions).into_owned()
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
            Some(u) => error(id, -32002, &crate::hidden::scrub(&format!("resource not found: {u}"))),
            None => error(id, -32602, "resources/read needs params.uri"),
        },
        // Both echo client input as written: scrubbed.
        _ => error(id, -32601, &crate::hidden::scrub(&format!("method not found: {method}"))),
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
    // The refusal names the store's path, which can hold any character.
    let text = crate::hidden::scrub(&format!("atlas is off: {refusal}")).into_owned();
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
    // Each hook's answer is due by its budget from when it was read: the
    // watchdog answers one the worker has not, and the worker's late answer
    // is dropped (a hook waits behind the one before it).
    let watch = hooks.as_ref().filter(|_| crate::hook_budget::enforced()).map(|s| Watchdog::start(s.hook_budget, out.clone()));
    let watch_reader = watch.as_ref().map(|w| (w.pending.clone(), w.limit));
    let (hook_tx, hook_worker) = match hooks {
        Some(settings) => {
            // `None` is the warm-up signal: the main thread sends it once its
            // first answer is out, so the worker opens its store while the main
            // thread is idle (opening both at start measured +8 ms on the first
            // answer from SQLite busy waits; opening on the first hook, 22 ms
            // on that hook).
            let (tx, rx) = mpsc::channel::<Option<(Value, Value, std::time::Instant)>>();
            let o = out.clone();
            let pending = watch.as_ref().map(|w| w.pending.clone());
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
                        let t_open = std::time::Instant::now();
                        match crate::hooks::Hooks::open(settings.clone()) {
                            Ok(mut h) => {
                                let t_setup = std::time::Instant::now();
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
                                let t_warm = std::time::Instant::now();
                                h.warm();
                                if crate::hook_budget::timings_on() {
                                    let ms = |a: std::time::Instant, b: std::time::Instant| (b - a).as_secs_f64() * 1e3;
                                    eprintln!(
                                        "atlas hook timings open: store={:.1} setup={:.1} warm={:.1}",
                                        ms(t_open, t_setup),
                                        ms(t_setup, t_warm),
                                        crate::hook_budget::ms_since(t_warm)
                                    );
                                }
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
                        let _wait = crate::hook_budget::Active::until(std::time::Instant::now() + crate::hook_budget::AFTER_WAIT);
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
                    let Some((id, args, arrived)) = msg else { continue };
                    let ev = crate::hooks::HookEvent::from_value(&args);
                    // Tests: a hook slower than its budget.
                    #[cfg(test)]
                    if ev.tool_name == tests::SLOW_TOOL {
                        std::thread::sleep(tests::SLOW);
                    }
                    // A panic in hook code (#253) answers empty and drops the
                    // hook state; `open` reopens it on the next message.
                    let (text, panicked) = match (hooks.as_mut(), &failed) {
                        (Some(h), _) => crate::hooks::guard(&settings, &ev, || h.handle_from(&ev, arrived).to_json(&ev.event)),
                        (None, Some(e)) => (notice.answer(e, &ev), false),
                        (None, None) => (String::new(), false),
                    };
                    if panicked {
                        hooks = None;
                    }
                    // Answered already when the watchdog took it.
                    if pending.as_ref().is_none_or(|p| Watchdog::claim(p, &id)) {
                        let r = ok(id, content(text, false));
                        if let Ok(mut w) = o.lock() {
                            let _ = write_line(&mut *w, &r);
                        }
                    } else if let Some(h) = hooks.as_mut() {
                        // Refused by the watchdog: what this answer let in is undone.
                        h.answer_lost(&ev);
                    }
                    // Tests: the worker thread dies (unguarded).
                    #[cfg(test)]
                    if ev.tool_name == tests::KILL_TOOL {
                        tests::KILLED.store(true, std::sync::atomic::Ordering::Release);
                        panic!("test: the hook worker dies");
                    }
                    // Re-indexing an edited file waits until the answer is out.
                    let t_after = std::time::Instant::now();
                    if let Some(h) = hooks.as_mut() {
                        // Its store waits are short: the next hook may be queued.
                        let _wait = crate::hook_budget::Active::until(t_after + crate::hook_budget::AFTER_WAIT);
                        if crate::hooks::guard(&settings, &ev, || h.after()).1 {
                            hooks = None;
                        }
                    }
                    let t_refresh = std::time::Instant::now();
                    if t_refresh >= due {
                        refresh(&mut hooks, &mut due);
                    }
                    if crate::hook_budget::timings_on() {
                        let after_ms = (t_refresh - t_after).as_secs_f64() * 1e3;
                        eprintln!("atlas hook timings after {}: after={after_ms:.1} refresh={:.1}", ev.event, crate::hook_budget::ms_since(t_refresh));
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
                        let arrived = std::time::Instant::now();
                        // Expected before it is sent: a claim never loses to it.
                        if let Some((p, limit)) = &watch_reader {
                            Watchdog::expect(p, &call.0, &call.1, arrived, *limit);
                        }
                        let id = call.0.clone();
                        if tx.send(Some((call.0, call.1, arrived))).is_ok() {
                            announcer.hook(&args);
                            continue;
                        }
                        // The worker is gone: the main thread answers it,
                        // and the watchdog must not (review S3 of #329).
                        if let Some((p, _)) = &watch_reader {
                            Watchdog::claim(p, &id);
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
    drop(watch);
    read
}

/// The hooks read and not yet answered: request id (as JSON) to the id,
/// the answer the watchdog gives it, and its deadline.
type Pending = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (Value, String, std::time::Instant)>>>;

/// The answer to a hook whose budget ran out while the worker was busy (see
/// [`crate::hook_budget`]): a thread that writes [`crate::hooks::overrun_answer`]
/// for each hook still pending a little after its deadline. Stops when
/// dropped.
struct Watchdog {
    pending: Pending,
    limit: std::time::Duration,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// After its deadline, the worker has this long to answer before the
/// watchdog does (the worker answers right after a phase it cut).
const WATCH_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

impl Watchdog {
    fn start<W: Write + Send + 'static>(limit: std::time::Duration, out: std::sync::Arc<std::sync::Mutex<W>>) -> Watchdog {
        use std::sync::atomic::Ordering;
        let pending: Pending = Default::default();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (p, s) = (pending.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("atlas-hook-watch".into())
            .spawn(move || {
                while !s.load(Ordering::Acquire) {
                    let now = std::time::Instant::now();
                    let mut due = Vec::new();
                    let mut next = now + std::time::Duration::from_millis(200);
                    if let Ok(mut m) = p.lock() {
                        m.retain(|_, (id, event, deadline)| {
                            if *deadline + WATCH_GRACE <= now {
                                due.push((id.clone(), std::mem::take(event)));
                                false
                            } else {
                                next = next.min(*deadline + WATCH_GRACE);
                                true
                            }
                        });
                    }
                    for (id, answer) in due {
                        let r = ok(id, content(answer, false));
                        if let Ok(mut w) = out.lock() {
                            let _ = write_line(&mut *w, &r);
                        }
                    }
                    std::thread::sleep(next.saturating_duration_since(std::time::Instant::now()));
                }
            })
            .ok();
        Watchdog { pending, limit, stop, thread }
    }

    /// Notes a hook just read at `arrived`, due `limit` later, with the
    /// answer the watchdog gives it then ([`crate::hooks::overrun_answer`]:
    /// a gate's is its refusal).
    fn expect(p: &Pending, id: &Value, args: &Value, arrived: std::time::Instant, limit: std::time::Duration) {
        let answer = crate::hooks::overrun_answer(&crate::hooks::HookEvent::from_value(args), limit);
        if let Ok(mut m) = p.lock() {
            m.insert(id.to_string(), (id.clone(), answer, arrived + limit));
        }
    }

    /// Takes the hook `id` for the worker to answer: false when the watchdog
    /// answered it already.
    fn claim(p: &Pending, id: &Value) -> bool {
        p.lock().map(|mut m| m.remove(&id.to_string()).is_some()).unwrap_or(true)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
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
        let c = crate::db::Connection::open(db).unwrap();
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

    /// The `initialize` instructions (folder and source names) and the
    /// echoes of a client's method or resource are scrubbed (#299 r3 N3).
    #[test]
    fn instructions_and_protocol_echoes_are_scrubbed() {
        let r = initialize(Value::from(1), &Value::Null, "vault ma\u{3164}in\u{202E}\u{1b}[2J".into());
        assert_eq!(r["result"]["instructions"], Value::from("vault main\\u{1b}[2J"));
        let r = plain(Value::from(2), "to\u{3164}ols/li\u{200B}st", &Value::Null);
        assert_eq!(r["error"]["message"], Value::from("method not found: tools/list"));
        let r = plain(Value::from(3), "resources/read", &crate::json!({"uri": "atlas://he\u{E0041}lp\u{202E}"}));
        assert_eq!(r["error"]["message"], Value::from("resource not found: atlas://help"));
    }

    /// The refused server names the store's path in its `instructions` and
    /// in every tool error: scrubbed, as the hook's notice is (#299 r3 S2).
    #[test]
    fn the_refused_server_scrubs_its_refusal() {
        let e = crate::Error::StoreTooNew {
            store: "memory store".into(),
            path: "C:/st\u{202E}o\u{3164}re\u{1b}[2J.sqlite".into(),
            found: "9".into(),
            supported: "8".into(),
        };
        let input = concat!(
            r#"{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}"#,
            "\n",
            r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "recall", "arguments": {}}}"#,
            "\n"
        );
        let mut out = Vec::new();
        serve_refused(&e, input.as_bytes(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<Value> = out.lines().map(|l| crate::json::parse(l).unwrap()).collect();
        assert_eq!(lines.len(), 2, "{out}");
        let texts = [lines[0]["result"]["instructions"].as_str().unwrap(), lines[1]["result"]["content"][0]["text"].as_str().unwrap()];
        for t in texts {
            assert!(t.contains("C:/store\\u{1b}[2J.sqlite"), "{t}");
            assert_eq!(crate::hidden::hidden_count(t), 0, "{t}");
        }
    }

    /// A hook the worker takes [`SLOW`] over before handling it.
    pub(crate) const SLOW_TOOL: &str = "atlas_test_slow_hook";
    pub(crate) const SLOW: std::time::Duration = std::time::Duration::from_millis(1_000);
    /// A hook after whose answer the worker thread dies.
    pub(crate) const KILL_TOOL: &str = "atlas_test_kill_worker";
    /// Set as the worker dies of [`KILL_TOOL`].
    pub(crate) static KILLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// A `gh pr merge` hook (the review merge gate's).
    fn gate_msg(id: &str) -> String {
        let args = crate::json!({"event": "PreToolUse", "session_id": "S", "tool_name": "Bash", "tool_input": {"command": "gh pr merge 5"}, "tool_use_id": id});
        crate::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hook", "arguments": args}}).to_string()
    }

    /// Input chunks, each read after its pause (once the worker died
    /// first, when flagged); an empty one is the EOF.
    struct Paced(std::collections::VecDeque<(bool, std::time::Duration, Vec<u8>)>);

    impl Read for Paced {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let Some((after_kill, pause, mut b)) = self.0.pop_front() else { return Ok(0) };
            // Not a timing guess: a loaded machine opens the hook state late.
            let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while after_kill && !KILLED.load(std::sync::atomic::Ordering::Acquire) && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            std::thread::sleep(pause);
            let n = out.len().min(b.len());
            let rest = b.split_off(n);
            out[..n].copy_from_slice(&b);
            if !rest.is_empty() {
                self.0.push_front((false, std::time::Duration::ZERO, rest));
            }
            Ok(n)
        }
    }

    /// Review S3 of #329: each hook is answered exactly once, whoever
    /// answers it: the worker in time, the watchdog past the budget, or the
    /// main thread once the worker is gone.
    #[test]
    fn every_hook_is_answered_exactly_once() {
        if !crate::hook_budget::enforced() {
            return;
        }
        let (dir, mut server, mut settings) = served("hook-once");
        settings.hook_budget = std::time::Duration::from_millis(300);
        let ms = std::time::Duration::from_millis;
        let line = |s: String| (s + "\n").into_bytes();
        let input = Paced(
            vec![
                (false, ms(0), line(hook_msg("fast", "TodoWrite"))),
                // Past its budget in the worker: the watchdog answers it,
                // and the one queued behind it.
                (false, ms(0), line(hook_msg("slow", SLOW_TOOL))),
                // A gate queued behind it: refused, never passed (review B1).
                (false, ms(0), line(gate_msg("merge"))),
                (false, ms(0), line(hook_msg("kill", KILL_TOOL))),
                // Read once the worker died (its channel dropped as it
                // unwound): the main thread answers it.
                (true, ms(300), line(hook_msg("after", "TodoWrite"))),
                // Every deadline and the watchdog's grace passed.
                (false, ms(1_200), Vec::new()),
            ]
            .into(),
        );
        let out = Shared::default();
        serve_with_hooks(&mut server, Some(settings), None, io::BufReader::new(input), out.clone()).unwrap();
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let mut n: HashMap<String, usize> = HashMap::new();
        for l in text.lines() {
            if let Some(id) = crate::json::parse(l).unwrap()["id"].as_str() {
                *n.entry(id.to_string()).or_default() += 1;
            }
        }
        for id in ["fast", "slow", "merge", "kill", "after"] {
            assert_eq!(n.get(id), Some(&1), "{id} answered {:?} times: {text}", n.get(id));
        }
        let a = answers(&out);
        assert!(!a["fast"].1 && !a["slow"].1 && !a["kill"].1, "{a:?}");
        assert!(a["merge"].0.contains("\"permissionDecision\":\"deny\"") && a["merge"].0.contains(crate::hooks::GATE_OVERRUN), "{a:?}");
        assert!(!a["kill"].0.contains("deny"), "{a:?}");
        // The main thread's answer (this server has no memory store).
        assert!(a["after"].1, "{a:?}");
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Input chunks, each read once the answer it names is out, then after
    /// its pause; an empty one is the EOF.
    struct AfterAnswer {
        out: Shared,
        steps: std::collections::VecDeque<(Option<&'static str>, std::time::Duration, Vec<u8>)>,
    }

    impl Read for AfterAnswer {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let Some((after, pause, mut b)) = self.steps.pop_front() else { return Ok(0) };
            if let Some(id) = after {
                let want = format!("\"id\":\"{id}\"");
                let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while !String::from_utf8_lossy(&self.out.0.lock().unwrap()).contains(&want) && std::time::Instant::now() < end {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            std::thread::sleep(pause);
            let n = out.len().min(b.len());
            let rest = b.split_off(n);
            out[..n].copy_from_slice(&b);
            if !rest.is_empty() {
                self.steps.push_front((None, std::time::Duration::ZERO, rest));
            }
            Ok(n)
        }
    }

    /// Review S1 of #329: a launch the watchdog refused (the worker was
    /// busy past its budget) holds no cap slot, though the worker admitted
    /// it afterwards: the next launch of its kind at a cap of one is let in.
    #[test]
    fn a_launch_refused_past_its_budget_holds_no_cap_slot() {
        if !crate::hook_budget::enforced() {
            return;
        }
        use crate::orchestrate::{Kind, KindSpec, OrchestrateSpec};
        let (dir, mut server, mut settings) = served("hook-lost-slot");
        let budget = std::time::Duration::from_millis(1_000);
        settings.hook_budget = budget;
        settings.snapshot_secs = 0;
        let one = KindSpec { max: Some(1), ..Default::default() };
        let spec = OrchestrateSpec { enabled: Some(true), kinds: vec![(Kind::Builder, one)], ..Default::default() };
        settings.orchestrate = crate::orchestrate::resolve(Some(&spec), None, None, |_| None);
        let transcript = dir.join("proj").join("sess.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "").unwrap();
        let tp = transcript.to_string_lossy().into_owned();
        let launch = |id: &str| {
            let brief = "Kind: builder\nBuild the widget cache.";
            let input = crate::json!({"description": "d", "subagent_type": "general-purpose", "prompt": brief});
            let args = crate::json!({"event": "PreToolUse", "session_id": "S", "tool_name": "Agent", "tool_use_id": id, "transcript_path": tp, "tool_prompt": brief, "tool_input": input});
            (crate::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hook", "arguments": args}}).to_string() + "\n").into_bytes()
        };
        let line = |s: String| (s + "\n").into_bytes();
        let ms = std::time::Duration::from_millis;
        let out = Shared::default();
        let input = AfterAnswer {
            out: out.clone(),
            steps: vec![
                // The hook state is open before the timed part.
                (None, ms(0), line(hook_msg("warm", "TodoWrite"))),
                // Two slow hooks keep the worker 2 s past the launch's
                // arrival: the watchdog refuses the launch at 1.25 s, and
                // the worker admits it after.
                (Some("warm"), ms(0), line(hook_msg("slow1", SLOW_TOOL))),
                (None, ms(0), line(hook_msg("slow2", SLOW_TOOL))),
                (None, ms(0), launch("first")),
                // Once the worker is done with the refused launch.
                (Some("first"), ms(1_500), launch("second")),
                (Some("second"), ms(0), Vec::new()),
            ]
            .into(),
        };
        serve_with_hooks(&mut server, Some(settings), None, io::BufReader::new(input), out.clone()).unwrap();
        let a = answers(&out);
        assert!(a["first"].0.contains("\"permissionDecision\":\"deny\"") && a["first"].0.contains(crate::hooks::GATE_OVERRUN), "{a:?}");
        assert!(crate::hooks::GATE_OVERRUN.contains("retry the call"));
        assert!(!a["second"].0.contains("deny"), "the refused launch held the builder slot: {a:?}");
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review r3 S1 of #329: a launch the watchdog refused that never
    /// reached `admit` (a subagent past its hard context limit, denied by
    /// the cap) frees no other launch: the builder admitted before it is
    /// still pending, so the next builder at a cap of one is refused.
    #[test]
    fn a_lost_launch_denied_before_admit_frees_no_other_launch() {
        if !crate::hook_budget::enforced() {
            return;
        }
        use crate::orchestrate::{Kind, KindSpec, Mode, OrchestrateSpec};
        let (dir, mut server, mut settings) = served("hook-lost-other");
        settings.hook_budget = std::time::Duration::from_millis(1_000);
        settings.snapshot_secs = 0;
        let one = KindSpec { max: Some(1), ..Default::default() };
        let tight = KindSpec { limit: Some(1_000), handoff: Some(500), mode: Some(Mode::Enforced), ..Default::default() };
        let spec =
            OrchestrateSpec { enabled: Some(true), kinds: vec![(Kind::Builder, one), (Kind::Research, tight)], ..Default::default() };
        settings.orchestrate = crate::orchestrate::resolve(Some(&spec), None, None, |_| None);
        let transcript = dir.join("proj").join("sess.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "").unwrap();
        let tp = transcript.to_string_lossy().into_owned();
        // Subagent y1: a researcher far past its hard limit of 1,000 tokens.
        let sub = crate::hooks::subagent_transcript(&tp, "y1");
        std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
        let usage = "{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":900000,\"cache_read_input_tokens\":0,\"output_tokens\":5}}}";
        std::fs::write(&sub, format!("{{\"type\":\"user\",\"message\":{{\"content\":\"Kind: research\\nRead the cache.\"}}}}\n{usage}\n"))
            .unwrap();
        let launch = |id: &str, agent: &str| {
            let brief = "Kind: builder\nBuild the widget cache.";
            let input = crate::json!({"description": "d", "subagent_type": "general-purpose", "prompt": brief});
            let args = crate::json!({"event": "PreToolUse", "session_id": "S", "agent_id": agent, "tool_name": "Agent", "tool_use_id": id, "transcript_path": tp, "tool_prompt": brief, "tool_input": input});
            (crate::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hook", "arguments": args}}).to_string()
                + "\n")
                .into_bytes()
        };
        let line = |s: String| (s + "\n").into_bytes();
        let ms = std::time::Duration::from_millis;
        let out = Shared::default();
        let input = AfterAnswer {
            out: out.clone(),
            steps: vec![
                (None, ms(0), line(hook_msg("warm", "TodoWrite"))),
                // Builder A is admitted and its answer won: it is pending.
                (Some("warm"), ms(0), launch("a", "")),
                // y1's launch B waits behind two slow hooks: the watchdog
                // refuses it, and the worker's cap denies it before admit.
                (Some("a"), ms(0), line(hook_msg("slow1", SLOW_TOOL))),
                (None, ms(0), line(hook_msg("slow2", SLOW_TOOL))),
                (None, ms(0), launch("b", "y1")),
                // Once the worker is done with B: builder C is over the cap.
                (Some("b"), ms(1_500), launch("c", "")),
                (Some("c"), ms(0), Vec::new()),
            ]
            .into(),
        };
        serve_with_hooks(&mut server, Some(settings), None, io::BufReader::new(input), out.clone()).unwrap();
        let a = answers(&out);
        assert!(!a["a"].0.contains("deny"), "{a:?}");
        assert!(a["b"].0.contains(crate::hooks::GATE_OVERRUN), "{a:?}");
        assert!(a["c"].0.contains("\"permissionDecision\":\"deny\""), "the lost launch B freed builder A's slot: {a:?}");
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
