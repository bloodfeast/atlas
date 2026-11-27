//! The language-server driver of the code index: a unit no SCIP index
//! serves is indexed by a language server named on its row
//! ([`crate::langs::LspServer`]), when one is found.
//!
//! The server is started at index time only, never on the query path and
//! never on the edit hook's (a file edited between full runs is re-scanned,
//! [`crate::code::reindex_file`]). atlas speaks JSON-RPC over the server's
//! stdin and stdout itself ([`Client`], `Content-Length` frames, no crate):
//! `initialize` (the unit's folder as its root, hierarchical document
//! symbols, UTF-8 or UTF-16 positions), then per file `didOpen`,
//! `textDocument/documentSymbol` and `didClose`, at most [`IN_FLIGHT`]
//! requests at a time, then `shutdown` and `exit`; a server still running
//! [`EXIT_GRACE`] after `exit` is killed.
//!
//! Each file's definitions become rows as a SCIP document's do (a
//! definition at its name, its kind in the words atlas prints, its parent
//! as its container, its item's end line); locals are dropped, as SCIP
//! rows have none, and no reference rows are written. The file is covered:
//! the scanner leaves it alone, and fills it where its row fills.
//!
//! What each file gave is cached in the code folder
//! (`<root>.<tool>[.k].lsp`) by path, modification time and size: a full
//! run asks only for new or changed files; a change to a stale-making file
//! of the row (a manifest) or a new server version empties the cache.
//!
//! The run is bounded as an indexer's is: one at a time under the code
//! lease, at below-normal priority, killed at `code.timeout_s` (the files
//! answered before stay cached and are used).

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::json::Value;
use crate::langs::{Launch, LspServer};

#[cfg(any(test, feature = "test-seams"))]
#[doc(hidden)]
pub mod fake;
#[cfg(test)]
mod refs_spike;
#[cfg(test)]
mod tests;

/// The most requests sent and not yet answered.
pub const IN_FLIGHT: usize = 8;

/// How long a server may run after `exit` before it is killed.
pub const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The largest frame read (a guard against a bad length).
const MAX_FRAME: usize = 64 << 20;

/// The longest header line (with its line end) a frame may have.
const MAX_HEADER: usize = 8 << 10;

/// Writes one JSON-RPC frame: the `Content-Length` header, a blank line,
/// the body.
///
/// # Errors
///
/// The writer's.
pub fn write_frame(w: &mut dyn Write, body: &str) -> io::Result<()> {
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(body.as_bytes())?;
    w.flush()
}

/// Reads one frame's body.
///
/// # Returns
///
/// `Ok(None)` at the end of input before a frame starts; `Err(why)` for a
/// frame that is not one (no `Content-Length`, a header that is not
/// `Name: value`, input ending inside it).
pub fn read_frame(r: &mut dyn BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut len: Option<usize> = None;
    let mut line = Vec::new();
    let mut first = true;
    loop {
        line.clear();
        let n = (&mut *r).take(MAX_HEADER as u64 + 1).read_until(b'\n', &mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return if first { Ok(None) } else { Err("input ended inside a header".into()) };
        }
        if line.len() > MAX_HEADER {
            return Err(format!("a header line over {MAX_HEADER} bytes"));
        }
        first = false;
        while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            line.pop();
        }
        if line.is_empty() {
            break;
        }
        let text = std::str::from_utf8(&line).map_err(|_| "a header is not text".to_string())?;
        let (k, v) = text.split_once(':').ok_or_else(|| format!("a header without a colon: {text:?}"))?;
        if k.trim().eq_ignore_ascii_case("content-length") {
            len = Some(v.trim().parse().map_err(|_| format!("a bad Content-Length: {v:?}"))?);
        }
    }
    let len = len.ok_or_else(|| "no Content-Length".to_string())?;
    if len > MAX_FRAME {
        return Err(format!("a frame of {len} bytes"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(Some(body))
}

/// A path as a `file://` URI: `/`-separated, a Windows drive letter upper
/// case (`file:///C:/work/a.py`), every byte but a letter, digit, `-._~/`
/// and the drive's colon percent-encoded.
pub fn file_uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
    let drive = s.len() >= 2 && s.as_bytes()[1] == b':' && s.as_bytes()[0].is_ascii_alphabetic();
    if drive {
        s = format!("/{}{}", s[..1].to_ascii_uppercase(), &s[1..]);
    }
    let mut out = String::from("file://");
    for (i, b) in s.bytes().enumerate() {
        let keep = b.is_ascii_alphanumeric() || b"-._~/".contains(&b) || (drive && i == 2 && b == b':');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path a `file://` URI names (`file:///c%3A/work/a.py` and
/// `file:///C:/work/a.py` are both `C:/work/a.py`); `None` for another
/// scheme.
pub fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = rest.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                bytes.push(v);
                i += 3;
                continue;
            }
        }
        bytes.push(b[i]);
        i += 1;
    }
    let mut s = String::from_utf8(bytes).ok()?;
    let sb = s.as_bytes();
    if sb.len() >= 3 && sb[0] == b'/' && sb[1].is_ascii_alphabetic() && sb[2] == b':' {
        s = format!("{}{}", s[1..2].to_ascii_uppercase(), &s[2..]);
    }
    Some(PathBuf::from(s))
}

/// Why a server's run ended before its work was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// A frame that is not one, or a body that is not JSON.
    Bad(String),
    /// Its output ended (it exited).
    End,
    /// The deadline passed.
    Timeout,
    /// It answered `initialize` with an error.
    Refused(String),
}

/// What the reader thread passes on.
enum Incoming {
    Msg(Value),
    Bad(String),
    End,
}

/// A JSON-RPC client over a server's output (`R`, read on its own thread)
/// and input (`W`).
pub struct Client<W: Write> {
    out: W,
    rx: mpsc::Receiver<Incoming>,
    next_id: i64,
    /// Requests sent.
    pub requests: usize,
}

impl<W: Write> Client<W> {
    /// A client reading `r` on a thread of its own and writing `out`.
    pub fn new<R: Read + Send + 'static>(r: R, out: W) -> Client<W> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut br = BufReader::new(r);
            loop {
                let m = match read_frame(&mut br) {
                    Ok(None) => Incoming::End,
                    Ok(Some(b)) => match crate::json::parse_bytes(&b) {
                        Ok(v) => Incoming::Msg(v),
                        Err(e) => Incoming::Bad(format!("{e}")),
                    },
                    Err(e) => Incoming::Bad(e),
                };
                let last = !matches!(m, Incoming::Msg(_));
                if tx.send(m).is_err() || last {
                    break;
                }
            }
        });
        Client { out, rx, next_id: 0, requests: 0 }
    }

    fn send(&mut self, v: &Value) -> Result<(), Stop> {
        write_frame(&mut self.out, &v.to_string()).map_err(|_| Stop::End)
    }

    /// Sends a request.
    ///
    /// # Returns
    ///
    /// Its id.
    ///
    /// # Errors
    ///
    /// [`Stop::End`] when the server's input is closed.
    pub fn request(&mut self, method: &str, params: Value) -> Result<i64, Stop> {
        self.next_id += 1;
        let id = self.next_id;
        self.requests += 1;
        self.send(&crate::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    /// Sends a notification.
    ///
    /// # Errors
    ///
    /// [`Stop::End`] when the server's input is closed.
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), Stop> {
        self.send(&crate::json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// The next response, answering the server's own requests meanwhile
    /// (`workspace/configuration` with a `null` per item, any other with
    /// `null`) and dropping its notifications.
    ///
    /// # Returns
    ///
    /// The response's id and its `result` (`Err` holding its `error`).
    ///
    /// # Errors
    ///
    /// [`Stop::Timeout`] at `deadline`, [`Stop::Bad`] for a bad frame,
    /// [`Stop::End`] when the server's output ends.
    pub fn response(&mut self, deadline: Instant) -> Result<(i64, Result<Value, Value>), Stop> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let m = match self.rx.recv_timeout(left) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(Stop::Timeout),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Stop::End),
            };
            let v = match m {
                Incoming::Msg(v) => v,
                Incoming::Bad(why) => return Err(Stop::Bad(why)),
                Incoming::End => return Err(Stop::End),
            };
            match (v.get("method").and_then(Value::as_str), v.get("id")) {
                (Some(method), Some(id)) => {
                    let result = if method == "workspace/configuration" {
                        let n = v.get("params").and_then(|p| p.get("items")).and_then(Value::as_array).map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; n])
                    } else {
                        Value::Null
                    };
                    let id = id.clone();
                    self.send(&crate::json!({"jsonrpc": "2.0", "id": id, "result": result}))?;
                }
                (Some(_), None) => {}
                (None, Some(id)) => {
                    let Some(id) = id.as_i64() else { continue };
                    return Ok(match v.get("error") {
                        Some(e) => (id, Err(e.clone())),
                        None => (id, Ok(v.get("result").cloned().unwrap_or(Value::Null))),
                    });
                }
                (None, None) => return Err(Stop::Bad("a message with neither a method nor an id".into())),
            }
        }
    }
}

/// One definition a server gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sym {
    /// Its name.
    pub name: String,
    /// Its kind, in the words atlas prints ([`kind_word`]).
    pub kind: String,
    /// The line of its name, 1-based.
    pub line: u32,
    /// The byte column of its name, 0-based.
    pub col: u32,
    /// The last line of its item, 1-based.
    pub end: u32,
    /// Its parent's name (a method's class); empty at a file's top.
    pub container: String,
}

/// The word atlas prints for an LSP `SymbolKind` (the words of
/// [`crate::scip::generic_kind_word`]); `None` for a kind that is no
/// definition atlas keeps (a file, a literal, a type parameter).
pub fn kind_word(kind: i64) -> Option<&'static str> {
    Some(match kind {
        2..=4 => "mod",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "ctor",
        10 => "enum",
        11 => "interface",
        12 => "fn",
        13 => "var",
        14 => "const",
        22 => "variant",
        23 => "struct",
        24 => "event",
        _ => return None,
    })
}

/// True for a callable's kind (Method, Constructor, Function): its
/// Variable and Constant children are locals.
fn callable(kind: i64) -> bool {
    matches!(kind, 6 | 9 | 12)
}

/// True for a local's kind, inside a callable (Variable, Constant).
fn local(kind: i64) -> bool {
    matches!(kind, 13 | 14)
}

/// A column the server gave as a byte column of `line`: UTF-16 code units
/// counted when the server chose UTF-16, else bytes (kept on a character
/// boundary).
pub fn byte_col(line: &str, ch: u32, utf16: bool) -> u32 {
    if !utf16 {
        let mut at = (ch as usize).min(line.len());
        while !line.is_char_boundary(at) {
            at -= 1;
        }
        return at as u32;
    }
    let mut units = 0u32;
    for (i, c) in line.char_indices() {
        if units >= ch {
            return i as u32;
        }
        units += c.len_utf16() as u32;
    }
    line.len() as u32
}

/// A Go method's name as gopls gives it (`(*Engine).Use`): the name, and
/// the receiver's type as its container.
fn split_receiver(name: &str) -> (String, Option<String>) {
    if let Some(rest) = name.strip_prefix('(') {
        if let Some((recv, tail)) = rest.split_once(')') {
            let method = tail.trim_start_matches(['.', ' ']);
            let ty = recv.rsplit(' ').next().unwrap_or(recv).trim_start_matches('*');
            let ty = ty.split('[').next().unwrap_or(ty);
            if !method.is_empty() && !ty.is_empty() {
                return (method.to_string(), Some(ty.to_string()));
            }
        }
    }
    (name.to_string(), None)
}

/// A position's (line, character); `None` when it has none.
fn pos(v: Option<&Value>) -> Option<(u32, u32)> {
    let v = v?;
    Some((v.get("line")?.as_u64()? as u32, v.get("character")?.as_u64()? as u32))
}

/// The 1-based last line of a range whose exclusive end is `end`: a range
/// ending at a line's first column ends on the line before.
fn last_line(start: u32, end: (u32, u32)) -> u32 {
    if end.1 == 0 && end.0 > start {
        end.0
    } else {
        end.0 + 1
    }
}

/// The definitions of a `textDocument/documentSymbol` answer: a
/// `DocumentSymbol` tree or a flat `SymbolInformation` list, over the
/// file's `text`.
pub fn symbols(v: &Value, text: &str, utf16: bool) -> Vec<Sym> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(items) = v.as_array() else { return Vec::new() };
    let mut out = Vec::new();
    if items.iter().any(|i| i.get("location").is_some()) {
        flat(items, &lines, utf16, &mut out);
    } else {
        tree(items, "", false, &lines, utf16, &mut out);
    }
    out
}

fn tree(items: &[Value], parent: &str, in_callable: bool, lines: &[&str], utf16: bool, out: &mut Vec<Sym>) {
    for it in items {
        let kind = it.get("kind").and_then(Value::as_i64).unwrap_or(0);
        if in_callable && local(kind) {
            continue;
        }
        let raw = it.get("name").and_then(Value::as_str).unwrap_or("");
        let (name, recv) = split_receiver(raw);
        let range = it.get("range");
        let sel = it.get("selectionRange").or(range);
        let start = pos(sel.and_then(|r| r.get("start")));
        let end = pos(range.and_then(|r| r.get("end")));
        let children = it.get("children").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
        match (kind_word(kind), start) {
            (Some(word), Some((l, c))) if !name.is_empty() => {
                let line = lines.get(l as usize).copied().unwrap_or("");
                let end = end.map_or(l + 1, |e| last_line(l, e)).max(l + 1);
                out.push(Sym {
                    name: name.clone(),
                    kind: word.to_string(),
                    line: l + 1,
                    col: byte_col(line, c, utf16),
                    end,
                    container: recv.unwrap_or_else(|| parent.to_string()),
                });
                tree(children, &name, in_callable || callable(kind), lines, utf16, out);
            }
            _ => tree(children, parent, in_callable, lines, utf16, out),
        }
    }
}

fn flat(items: &[Value], lines: &[&str], utf16: bool, out: &mut Vec<Sym>) {
    let range_of = |i: &Value| {
        let r = i.get("location").and_then(|l| l.get("range"));
        Some((pos(r.and_then(|r| r.get("start")))?, pos(r.and_then(|r| r.get("end")))?))
    };
    // The callables' ranges: a Variable or Constant inside one is a local.
    let callables: Vec<((u32, u32), (u32, u32))> =
        items.iter().filter(|i| callable(i.get("kind").and_then(Value::as_i64).unwrap_or(0))).filter_map(range_of).collect();
    for it in items {
        let kind = it.get("kind").and_then(Value::as_i64).unwrap_or(0);
        let Some(word) = kind_word(kind) else { continue };
        let Some((s, e)) = range_of(it) else { continue };
        if local(kind) && callables.iter().any(|(cs, ce)| *cs <= s && e <= *ce && (*cs, *ce) != (s, e)) {
            continue;
        }
        let raw = it.get("name").and_then(Value::as_str).unwrap_or("");
        let (name, recv) = split_receiver(raw);
        if name.is_empty() {
            continue;
        }
        // The range is the item's: its name is the first occurrence from
        // its start, on that line or the next few.
        let from = byte_col(lines.get(s.0 as usize).copied().unwrap_or(""), s.1, utf16) as usize;
        let (line, col) = (s.0..=e.0.min(s.0 + 3))
            .find_map(|l| {
                let text = lines.get(l as usize)?;
                let at = if l == s.0 { from } else { 0 };
                word_at(text, at, &name).map(|k| (l, k as u32))
            })
            .unwrap_or((s.0, from as u32));
        let container = recv.or_else(|| it.get("containerName").and_then(Value::as_str).map(str::to_string)).unwrap_or_default();
        out.push(Sym { name, kind: word.to_string(), line: line + 1, col, end: last_line(s.0, e).max(line + 1), container });
    }
}

/// The byte offset of the first whole-word `name` in `line` from `from`
/// (`f` in `def f():` is not the `f` of `def`).
fn word_at(line: &str, from: usize, name: &str) -> Option<usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut at = from;
    while let Some(k) = line.get(at..)?.find(name) {
        let s = at + k;
        let e = s + name.len();
        if !line[..s].chars().next_back().is_some_and(ident) && !line[e..].chars().next().is_some_and(ident) {
            return Some(s);
        }
        at = s + line[s..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// What a session is given: the unit's folder and the row's language.
pub struct Ask<'a> {
    /// The folder the server is rooted at.
    pub root: &'a Path,
    /// The `languageId` of each file.
    pub language_id: &'a str,
    /// `initializationOptions`, as JSON text (empty: none).
    pub init_options: &'a str,
    /// When the run is cut off.
    pub deadline: Instant,
}

/// A server after `initialize`.
pub struct Session<W: Write> {
    client: Client<W>,
    utf16: bool,
    /// The server's `serverInfo`: name and version (empty when not given).
    pub info: (String, String),
    language_id: String,
}

impl<W: Write> Session<W> {
    /// Sends `initialize` and `initialized`.
    ///
    /// # Errors
    ///
    /// Why the server did not start: [`Stop`].
    pub fn start(mut client: Client<W>, a: &Ask<'_>) -> Result<Session<W>, Stop> {
        let uri = file_uri(a.root);
        let name = a.root.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let opts = if a.init_options.is_empty() { Value::Null } else { crate::json::parse(a.init_options).unwrap_or(Value::Null) };
        let params = crate::json!({
            "processId": std::process::id(),
            "rootUri": uri.clone(),
            "rootPath": a.root.to_string_lossy().into_owned(),
            "workspaceFolders": [{"uri": uri, "name": name}],
            "initializationOptions": opts,
            "capabilities": {
                "general": {"positionEncodings": ["utf-8", "utf-16"]},
                "textDocument": {"documentSymbol": {"hierarchicalDocumentSymbolSupport": true}},
                "workspace": {"configuration": true, "workspaceFolders": true}
            }
        });
        let id = client.request("initialize", params)?;
        let result = loop {
            match client.response(a.deadline)? {
                (i, Ok(v)) if i == id => break v,
                (i, Err(e)) if i == id => return Err(Stop::Refused(e.get("message").and_then(Value::as_str).unwrap_or("").to_string())),
                _ => {}
            }
        };
        let utf16 = result.get("capabilities").and_then(|c| c.get("positionEncoding")).and_then(Value::as_str) != Some("utf-8");
        let info = result.get("serverInfo");
        let field = |k: &str| info.and_then(|i| i.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
        let info = (field("name"), crate::langs::version_word(&field("version")));
        client.notify("initialized", crate::json!({}))?;
        Ok(Session { client, utf16, info, language_id: a.language_id.to_string() })
    }

    /// Requests sent so far.
    pub fn requests(&self) -> usize {
        self.client.requests
    }

    /// Asks for each file's symbols, [`IN_FLIGHT`] at a time: `didOpen`,
    /// `textDocument/documentSymbol`, and `didClose` once answered. Each
    /// answer is handed to `got` (the file's index in `files`) as it comes;
    /// a file the server could not read, as its error's message.
    ///
    /// # Errors
    ///
    /// Why the run stopped early ([`Stop`]); the files handed on before
    /// stay handed on.
    pub fn document_symbols(
        &mut self,
        files: &[(PathBuf, String)],
        deadline: Instant,
        mut got: impl FnMut(usize, std::result::Result<Vec<Sym>, String>),
    ) -> Result<(), Stop> {
        let mut next = 0;
        let mut waiting: HashMap<i64, usize> = HashMap::new();
        while next < files.len() || !waiting.is_empty() {
            while waiting.len() < IN_FLIGHT && next < files.len() {
                let (path, text) = &files[next];
                let uri = file_uri(path);
                self.client.notify(
                    "textDocument/didOpen",
                    crate::json!({"textDocument": {"uri": uri.clone(), "languageId": self.language_id.clone(), "version": 1, "text": text.clone()}}),
                )?;
                let id = self.client.request("textDocument/documentSymbol", crate::json!({"textDocument": {"uri": uri}}))?;
                waiting.insert(id, next);
                next += 1;
            }
            let (id, result) = self.client.response(deadline)?;
            let Some(k) = waiting.remove(&id) else { continue };
            let (path, text) = &files[k];
            self.client.notify("textDocument/didClose", crate::json!({"textDocument": {"uri": file_uri(path)}}))?;
            got(
                k,
                match result {
                    Ok(v) => Ok(symbols(&v, text, self.utf16)),
                    Err(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("an error").to_string()),
                },
            );
        }
        Ok(())
    }

    /// Sends `shutdown` and waits for its answer (at most [`EXIT_GRACE`]),
    /// then `exit`. A server that does not answer is killed by the caller.
    pub fn finish(mut self) {
        let deadline = Instant::now() + EXIT_GRACE;
        if let Ok(id) = self.client.request("shutdown", Value::Null) {
            while let Ok((i, _)) = self.client.response(deadline) {
                if i == id {
                    break;
                }
            }
        }
        let _ = self.client.notify("exit", Value::Null);
    }
}

/// The server a row's units run under `code.indexers`: a server given a
/// path runs from it (the first so named); a server set to `false` is off;
/// the rest are found in order ([`crate::langs::find_server`]).
///
/// # Errors
///
/// The note a scanned unit carries: which servers are off and which were
/// looked for where; empty for a row without servers.
pub fn choose(indexers: &crate::code::Indexers, servers: &'static [LspServer]) -> Result<(&'static LspServer, Launch), String> {
    use crate::code::Indexers;
    let all: Vec<&'static LspServer> = servers.iter().collect();
    match indexers {
        Indexers::Off(why) => Err(why.clone()),
        Indexers::Search => crate::langs::find_server(&all),
        Indexers::Fixed(list) => all
            .iter()
            .find_map(|s| list.iter().find(|(t, _)| t == s.tool).map(|(_, p)| (*s, s.configured(p))))
            .ok_or_else(|| format!("no {} is configured", servers.iter().map(|s| s.tool).collect::<Vec<_>>().join(" or "))),
        Indexers::Chosen(list) => {
            let named = |s: &LspServer| list.iter().find(|(t, _)| t == s.tool).map(|(_, p)| p.as_deref());
            if let Some((s, p)) = all.iter().find_map(|s| named(s).flatten().map(|p| (*s, p))) {
                return Ok((s, s.configured(p)));
            }
            let off: Vec<String> =
                all.iter().filter(|s| named(s).is_some()).map(|s| format!("{} is off (code.indexers)", s.tool)).collect();
            let rest: Vec<&'static LspServer> = all.iter().copied().filter(|s| named(s).is_none()).collect();
            let found = if rest.is_empty() { Err(String::new()) } else { crate::langs::find_server(&rest) };
            found.map_err(|why| off.into_iter().chain((!why.is_empty()).then_some(why)).collect::<Vec<_>>().join("; "))
        }
    }
}

/// How a server's run went.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    /// Wall time, from the start to the exit.
    pub seconds: f64,
    /// Why it stopped early, if it did.
    pub stop: Option<Stop>,
    /// Its `serverInfo` (name, version).
    pub info: (String, String),
    /// Requests sent.
    pub requests: usize,
    /// True when it was killed: past the deadline, after a bad frame, or
    /// still running [`EXIT_GRACE`] after `exit`.
    pub killed: bool,
}

/// The writer half a session sends on: a server's stdin, or a test's pipe.
pub type Out = Box<dyn Write + Send>;

/// Starts a server in `cwd` at below-normal priority (its stderr to
/// `log`), initializes it, runs `work` and shuts it down; a server that
/// stopped early, or that runs on [`EXIT_GRACE`] after `exit`, is killed.
///
/// # Errors
///
/// When it cannot be started.
pub fn run_server(
    launch: &Launch,
    args: Vec<std::ffi::OsString>,
    cwd: &Path,
    log: &Path,
    a: &Ask<'_>,
    work: impl FnOnce(&mut Session<Out>) -> Result<(), Stop>,
) -> io::Result<Ran> {
    let t0 = Instant::now();
    #[cfg(any(test, feature = "test-seams"))]
    if let Some(name) = launch.program.to_str().and_then(|s| s.strip_prefix(fake::SCHEME)) {
        let (r, w) = fake::start(name)?;
        return Ok(timed(drive(Client::new(r, w), a, work), t0));
    }
    let err = std::fs::File::create(log)?;
    let mut c = std::process::Command::new(&launch.program);
    c.args(launch.args(args))
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(err));
    crate::code_index::below_normal(&mut c);
    let mut child = c.spawn().map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", launch.program.display())))?;
    let pid = child.id();
    crate::platform::process::lower_priority(pid);
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        crate::platform::process::terminate(pid);
        let _ = child.wait();
        return Err(io::Error::other("no pipes to the server"));
    };
    // The waiter owns the child, as run_bounded's does.
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let s = child.wait();
        let _ = tx.send(());
        s
    });
    // The watchdog kills the server at the deadline plus a grace whatever
    // the calling thread is doing: a write to a server that stopped reading
    // blocks until the pipe closes, and the kill closes it.
    let dog = Watchdog::start(a.deadline + EXIT_GRACE, move || crate::platform::process::terminate(pid));
    let mut ran = drive(Client::new(stdout, Box::new(stdin) as Out), a, work);
    if dog.stop() {
        ran.killed = true;
        ran.stop = Some(Stop::Timeout);
    }
    let grace = if ran.stop.is_some() { Duration::ZERO } else { EXIT_GRACE };
    if rx.recv_timeout(grace).is_err() {
        crate::platform::process::terminate(pid);
        ran.killed = true;
    }
    let _ = waiter.join();
    Ok(timed(ran, t0))
}

/// A timer on a thread of its own: runs `kill` at the instant unless
/// stopped first.
pub(crate) struct Watchdog {
    done: mpsc::Sender<()>,
    thread: std::thread::JoinHandle<bool>,
}

impl Watchdog {
    /// Starts the timer.
    pub(crate) fn start(at: Instant, kill: impl FnOnce() + Send + 'static) -> Watchdog {
        let (done, rx) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                kill();
                true
            }
            _ => false,
        });
        Watchdog { done, thread }
    }

    /// Stops the timer.
    ///
    /// # Returns
    ///
    /// True when it had already fired.
    pub(crate) fn stop(self) -> bool {
        drop(self.done);
        self.thread.join().unwrap_or(false)
    }
}

/// `ran` with its wall time since `t0`.
fn timed(mut ran: Ran, t0: Instant) -> Ran {
    ran.seconds = t0.elapsed().as_secs_f64();
    ran
}

/// Initializes, works and finishes; the client (and the server's input)
/// is dropped at the end.
fn drive(client: Client<Out>, a: &Ask<'_>, work: impl FnOnce(&mut Session<Out>) -> Result<(), Stop>) -> Ran {
    let mut ran = Ran { seconds: 0.0, stop: None, info: Default::default(), requests: 1, killed: false };
    match Session::start(client, a) {
        Err(stop) => ran.stop = Some(stop),
        Ok(mut s) => {
            let r = work(&mut s);
            ran.info = s.info.clone();
            match r {
                Ok(()) => {
                    ran.requests = s.requests() + 1;
                    s.finish();
                }
                Err(stop) => {
                    ran.requests = s.requests();
                    ran.stop = Some(stop);
                }
            }
        }
    }
    ran
}

/// What a unit's note says when a run stopped early.
pub fn stop_note(tool: &str, stop: &Stop, timeout: Duration) -> String {
    match stop {
        Stop::Timeout => format!("scan only: {tool} passed the {} s timeout", timeout.as_secs()),
        Stop::Bad(_) => format!("scan only: {tool} sent a bad frame"),
        Stop::End => format!("scan only: {tool} exited"),
        Stop::Refused(m) if m.is_empty() => format!("scan only: {tool} refused initialize"),
        Stop::Refused(m) => format!("scan only: {tool} refused initialize: {m}"),
    }
}

/// One cached file: when it was asked and what it gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its modification time, nanoseconds since the epoch.
    pub mtime: i64,
    /// Its size.
    pub size: u64,
    /// Its definitions.
    pub syms: Vec<Sym>,
}

/// A unit's answers, kept between runs (`<root>.<tool>[.k].lsp`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cache {
    /// The server's version.
    pub version: String,
    /// The signature of the unit's stale-making files ([`stale_sig`]).
    pub stale: String,
    /// Each file's entry, by path below the root.
    pub files: HashMap<String, Entry>,
}

const CACHE_HEAD: &str = "atlas-lsp 1";

/// A field written into a tab-separated cache line.
fn field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

impl Cache {
    /// Reads a cache file; `None` when it is missing or of another format.
    pub fn load(p: &Path) -> Option<Cache> {
        let text = std::fs::read_to_string(p).ok()?;
        let mut lines = text.lines();
        let head: Vec<&str> = lines.next()?.split('\t').collect();
        if head.len() != 3 || head[0] != CACHE_HEAD {
            return None;
        }
        let mut c = Cache { version: head[1].into(), stale: head[2].into(), files: HashMap::new() };
        let mut at: Option<String> = None;
        for l in lines {
            let f: Vec<&str> = l.split('\t').collect();
            match f.as_slice() {
                ["F", path, mtime, size] => {
                    let e = Entry { mtime: mtime.parse().ok()?, size: size.parse().ok()?, syms: Vec::new() };
                    c.files.insert(path.to_string(), e);
                    at = Some(path.to_string());
                }
                ["S", line, col, end, kind, name, container] => {
                    let e = c.files.get_mut(at.as_deref()?)?;
                    e.syms.push(Sym {
                        name: name.to_string(),
                        kind: kind.to_string(),
                        line: line.parse().ok()?,
                        col: col.parse().ok()?,
                        end: end.parse().ok()?,
                        container: container.to_string(),
                    });
                }
                _ => return None,
            }
        }
        Some(c)
    }

    /// Writes the cache file (through a temporary file, renamed).
    ///
    /// # Errors
    ///
    /// The file system's.
    pub fn save(&self, p: &Path) -> io::Result<()> {
        let mut out = format!("{CACHE_HEAD}\t{}\t{}\n", field(&self.version), field(&self.stale));
        let mut paths: Vec<&String> = self.files.keys().collect();
        paths.sort();
        for path in paths {
            let e = &self.files[path];
            out.push_str(&format!("F\t{}\t{}\t{}\n", field(path), e.mtime, e.size));
            for s in &e.syms {
                out.push_str(&format!(
                    "S\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    s.line,
                    s.col,
                    s.end,
                    field(&s.kind),
                    field(&s.name),
                    field(&s.container)
                ));
            }
        }
        let tmp = p.with_extension("lsp.part");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, p)
    }
}

/// The signature of a unit's stale-making files ([`crate::langs::Lang::stale`]
/// in the unit's folder and below): a change to one empties the cache.
pub fn stale_sig(lang: &crate::langs::Lang, walk: &crate::code::Walk, dir: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: &[u8]| {
        for x in b {
            h ^= u64::from(*x);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for f in &walk.markers {
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        let inside = dir.is_empty() || f.path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'));
        if inside && lang.stale.iter().any(|p| crate::langs::name_matches(p, name)) {
            eat(format!("{}\0{}\0{}\n", f.path, f.mtime, f.size).as_bytes());
        }
    }
    format!("{h:016x}")
}

/// What one unit's server run needs from the index run.
pub(crate) struct UnitRun<'a> {
    pub setup: &'a crate::code::CodeSetup,
    pub root: &'a crate::code::CodeRoot,
    pub walk: &'a crate::code::Walk,
    pub unit: &'a crate::code::LangUnit,
    /// The unit's number among its row's units.
    pub k: usize,
    /// Its source files.
    pub files: Vec<&'a crate::code::WalkedFile>,
}

/// Indexes one unit through a server, from its cache where a file is
/// unchanged: rows added, files marked covered, the run appended to
/// `runs`.
///
/// # Returns
///
/// The unit's note: `LABEL VERSION (lsp)`, or `scan only: why` when the
/// server stopped early (the files it answered are still used).
pub(crate) fn index_unit(
    u: &UnitRun<'_>,
    server: &LspServer,
    launch: &Launch,
    rows: &mut crate::code::Rows,
    files: &mut HashMap<String, u32>,
    covered: &mut std::collections::HashSet<String>,
    runs: &mut Vec<(String, crate::code_index::RunReport)>,
) -> String {
    let (setup, root) = (u.setup, u.root);
    let cwd = if u.unit.dir.is_empty() { root.root.clone() } else { root.root.join(&u.unit.dir) };
    if let Err(e) = std::fs::create_dir_all(&setup.dir) {
        return format!("scan only: {e}");
    }
    let stem = if u.k == 0 { format!("{}.{}", root.name, server.tool) } else { format!("{}.{}.{}", root.name, server.tool, u.k) };
    let cache_path = setup.dir.join(format!("{stem}.lsp"));
    let stale = stale_sig(u.unit.lang, u.walk, &u.unit.dir);
    let found = server.version(launch, setup.timeout);
    let mut cache = Cache::load(&cache_path)
        .filter(|c| c.stale == stale && (found.is_empty() || c.version == found))
        .unwrap_or_else(|| Cache { version: found.clone(), stale: stale.clone(), files: HashMap::new() });
    // A removed file leaves the cache.
    let live: std::collections::HashSet<&str> = u.files.iter().map(|f| f.path.as_str()).collect();
    cache.files.retain(|p, _| live.contains(p.as_str()));
    let fresh = |c: &Cache, f: &crate::code::WalkedFile| c.files.get(&f.path).is_some_and(|e| e.mtime == f.mtime && e.size == f.size);
    let todo: Vec<&crate::code::WalkedFile> = u.files.iter().copied().filter(|f| !fresh(&cache, f)).collect();
    let mut note = None;
    // Files the server could not read (asked again next run), with the
    // first message, and how many were asked.
    let (mut failed, mut first_error, mut asked_n) = (0usize, None::<String>, 0usize);
    // Every file is current: no server starts (its version is known).
    if !todo.is_empty() || found.is_empty() {
        let deadline = Instant::now() + setup.timeout;
        let a = Ask { root: &cwd, language_id: server.language_id, init_options: server.init_options, deadline };
        let all = &u.files;
        let ran = run_server(launch, server.args_for(), &cwd, &setup.dir.join(format!("{stem}.log")), &a, |s| {
            // A server whose version only it can tell (a configured path):
            // a new one empties the cache.
            let version = if found.is_empty() { s.info.1.clone() } else { found.clone() };
            let ask: Vec<&crate::code::WalkedFile> = if cache.version == version {
                todo
            } else {
                cache.files.clear();
                cache.version = version;
                all.clone()
            };
            let (asked, texts): (Vec<&crate::code::WalkedFile>, Vec<(PathBuf, String)>) = ask
                .iter()
                .filter_map(|f| crate::code::read_source(&root.root, &f.path).map(|t| (*f, (root.root.join(&f.path), t))))
                .unzip();
            asked_n = texts.len();
            s.document_symbols(&texts, deadline, |i, answer| match answer {
                Ok(syms) => {
                    let f = asked[i];
                    cache.files.insert(f.path.clone(), Entry { mtime: f.mtime, size: f.size, syms });
                }
                Err(e) => {
                    failed += 1;
                    first_error.get_or_insert(e);
                }
            })
        });
        let label = if u.unit.dir.is_empty() { server.tool.to_string() } else { format!("{} {}", server.tool, u.unit.dir) };
        match ran {
            Err(e) => note = Some(format!("scan only: {e}")),
            Ok(r) => {
                note = r.stop.as_ref().map(|s| stop_note(server.tool, s, setup.timeout));
                // A server that read none of the files it was asked for (a
                // module it cannot load) indexed nothing.
                if note.is_none() && asked_n > 0 && failed == asked_n {
                    note = Some(format!("scan only: {} answered no file ({})", server.tool, first_error.as_deref().unwrap_or("")));
                }
                let timed_out = matches!(r.stop, Some(Stop::Timeout));
                runs.push((label, crate::code_index::RunReport { seconds: r.seconds, code: None, timed_out }));
            }
        }
        // The files answered stay, a stopped run's too.
        let _ = cache.save(&cache_path);
    }
    for f in &u.files {
        let Some(e) = cache.files.get(&f.path).filter(|e| e.mtime == f.mtime && e.size == f.size) else { continue };
        let lang = crate::langs::of_path(&f.path).map_or(u.unit.lang.name, |l| l.name);
        let file = rows.file(files, &f.path, lang);
        for s in &e.syms {
            let end = rows.ends.entry((file, s.line)).or_insert(s.end);
            *end = (*end).max(s.end);
            rows.occ.push(crate::code::Occ {
                name: s.name.clone(),
                sect: crate::code::SECT_DEF,
                file,
                line: s.line,
                col: s.col,
                kind: s.kind.clone(),
                container: s.container.clone(),
                within: String::new(),
            });
            rows.stats.lsp_defs += 1;
        }
        covered.insert(f.path.clone());
    }
    note.unwrap_or_else(|| {
        if cache.version.is_empty() {
            format!("{} (lsp)", server.label())
        } else {
            format!("{} {} (lsp)", server.label(), cache.version)
        }
    })
}

/// `atlas doctor`'s code lines: for each language of the language table
/// with files in a configured code root, one line naming what indexes it
/// (its SCIP indexer, else its language server), or, when neither is
/// found, what was looked for and how to install one. Only version probes
/// run: no indexer or server is started.
pub fn doctor_lines(setup: &crate::code::CodeSetup) -> Vec<String> {
    let mut out = Vec::new();
    for root in &setup.roots {
        let Ok(walk) = root.walk() else { continue };
        let mut langs: Vec<&'static crate::langs::Lang> = Vec::new();
        for f in &walk.source {
            let Some(l) = crate::langs::of_path(&f.path) else { continue };
            let owner = l.units_of.and_then(crate::langs::by_name).unwrap_or(l);
            if !langs.iter().any(|x| std::ptr::eq(*x, owner)) {
                langs.push(owner);
            }
        }
        for lang in langs {
            out.push(doctor_line(setup, &root.root, lang));
        }
    }
    out
}

fn doctor_line(setup: &crate::code::CodeSetup, root: &Path, lang: &'static crate::langs::Lang) -> String {
    let at = format!("code: {} in {}", lang.name, root.display());
    let scip = crate::code_index::driver_for(&setup.indexers, lang);
    if let Ok((d, _)) = &scip {
        return format!("{at} is indexed by {}", d.tool);
    }
    if let crate::code::Indexers::Off(why) = &setup.indexers {
        return format!("{at} is scanned: {why}");
    }
    let server = choose(&setup.indexers, lang.lsp);
    if let Ok((s, _)) = &server {
        return format!("{at} is indexed by {} (lsp)", s.label());
    }
    let mut why: Vec<String> = Vec::new();
    match (&lang.driver, scip) {
        (Some(d), _) if d.not_on_windows.is_some() && cfg!(windows) => why.push(format!("no {} (not on Windows)", d.tool)),
        (_, Err(e)) => why.push(e),
        (_, Ok(_)) => {}
    }
    if let Err(e) = server {
        if !e.is_empty() {
            why.push(e);
        }
    }
    let hint = match lang.lsp.first() {
        Some(s) => s.node_package.map_or_else(|| format!("install {}", s.tool), |p| format!("npm i -g {p}")),
        None => lang.driver.as_ref().map_or_else(String::new, |d| format!("install {}", d.tool)),
    };
    format!("{at} is scanned: {}; install one ({hint}) or set code.indexers", why.join(", "))
}
