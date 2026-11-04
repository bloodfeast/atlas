//! An owned `.proto` scanner for the code index: no `protoc`, no crates.
//!
//! What it finds, per file:
//!
//! * the `package`;
//! * definitions: messages (nested ones too, with the outer message as
//!   container), enums and their values, services, rpcs (the kind names the
//!   request and response types, `stream` included) and fields (container:
//!   the message);
//! * references: an rpc's request and response types (within the rpc), a
//!   field's message or enum type (within the message), and `map<K, V>`
//!   types. Scalar types are not references. A qualified name
//!   (`.acme.billing.v1.Invoice`, `billing.v1.Invoice`) is stored under its
//!   last part, as definitions are, so a type defined in one repository and
//!   used from another's protos answers in both.
//!
//! Comments and string literals are blanked first (line numbers kept), so a
//! name in a comment is never a definition. Options (`[deprecated = true]`,
//! `option ...;`), `reserved`, `extensions` and `import` lines define
//! nothing.
//!
//! [`link_rows`] links each rpc to the code generated for it in every
//! language of [`crate::langs`] (stubs, by each row's [`crate::langs::Grpc`]
//! conventions), to the calls made through those stubs (exact, from SCIP
//! references) and to the server types implementing it (by SCIP impl edges
//! or a type's header). [`mention_rows`] adds the call sites no index
//! resolved: in a root's other source files, a word in code (not in a
//! comment or a string, not a definition's name, not where SCIP has a
//! reference, not in generated code) that is a known rpc's name (as
//! written, in lowerCamel or in snake_case) is a reference of kind `call
//! (mention)`: a client of a `.proto` loaded at run time. The rpc names come
//! from every root of the project, so a gateway's `client.GetInvoice(...)`
//! answers "who calls GetInvoice" for the billing service's proto.

use std::collections::{HashMap, HashSet};

/// One definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    /// The short name.
    pub name: String,
    /// `message`, `enum`, `enum value`, `service`, `rpc (Req) returns
    /// (Resp)`, `field Type`.
    pub kind: String,
    /// The package-qualified parent (`acme.billing.v1.Invoice`), or the
    /// package for a top-level item.
    pub container: String,
    /// Line, 1-based.
    pub line: u32,
    /// Column, 0-based bytes.
    pub col: u32,
}

/// One reference to a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// The referenced type's short name.
    pub name: String,
    /// The definition it is written in (an rpc or a message).
    pub within: String,
    /// Line, 1-based.
    pub line: u32,
    /// Column, 0-based bytes.
    pub col: u32,
}

/// What one file holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// `package a.b.c;`, when given.
    pub package: Option<String>,
    /// The files it imports.
    pub imports: Vec<String>,
    /// Definitions.
    pub defs: Vec<Def>,
    /// References.
    pub refs: Vec<Ref>,
}

/// The scalar types: never references.
pub const SCALARS: &[&str] = &[
    "double", "float", "int32", "int64", "uint32", "uint64", "sint32", "sint64", "fixed32", "fixed64", "sfixed32", "sfixed64", "bool", "string", "bytes",
];

/// The text with comments and string contents replaced by spaces (newlines
/// kept), and the string literals in order (for `import` and `package`).
fn blank(text: &str) -> (String, Vec<String>) {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut strings = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            out.extend_from_slice(b"  ");
            i += 2;
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if i < b.len() {
                out.extend_from_slice(b"  ");
                i += 2;
            }
        } else if c == b'"' || c == b'\'' {
            let q = c;
            let start = i + 1;
            out.push(b' ');
            i += 1;
            while i < b.len() && b[i] != q && b[i] != b'\n' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    out.push(b' ');
                    i += 1;
                }
                out.push(b' ');
                i += 1;
            }
            strings.push(String::from_utf8_lossy(&b[start..i.min(b.len())]).into_owned());
            if i < b.len() && b[i] == q {
                out.push(b' ');
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    (String::from_utf8(out).unwrap_or_default(), strings)
}

/// A token: an identifier (dots allowed), or one punctuation byte.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tok {
    text: String,
    line: u32,
    col: u32,
}

fn tokens(text: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    for (ln, line) in text.lines().enumerate() {
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                    i += 1;
                }
                out.push(Tok { text: line[s..i].to_string(), line: ln as u32 + 1, col: s as u32 });
            } else if c.is_ascii_whitespace() {
                i += 1;
            } else {
                out.push(Tok { text: (c as char).to_string(), line: ln as u32 + 1, col: i as u32 });
                i += 1;
            }
        }
    }
    out
}

fn short(t: &str) -> &str {
    t.trim_start_matches('.').rsplit('.').next().unwrap_or(t)
}

fn is_ident(t: &str) -> bool {
    t.bytes().next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_' || c == b'.')
}

/// What a `{` opened.
#[derive(Debug, Clone)]
enum Scope {
    /// A message (its name).
    Message(String),
    /// An enum.
    Enum(String),
    /// A service.
    Service(String),
    /// A oneof inside a message (fields belong to the message).
    Oneof,
    /// Anything else (an rpc's options, an option's aggregate value).
    Other,
}

/// Scans one `.proto` file.
pub fn scan(text: &str) -> Scan {
    let (blanked, strings) = blank(text);
    let toks = tokens(&blanked);
    let mut s = Scan::default();
    let mut stack: Vec<Scope> = Vec::new();
    // Names of the enclosing messages, outermost first.
    let path = |stack: &[Scope], pkg: &Option<String>| -> String {
        let mut parts: Vec<String> = pkg.iter().cloned().collect();
        for sc in stack {
            match sc {
                Scope::Message(n) | Scope::Enum(n) | Scope::Service(n) => parts.push(n.clone()),
                _ => {}
            }
        }
        parts.join(".")
    };
    let mut strings = strings.into_iter();
    let mut i = 0;
    // Statement start: the token after `;`, `{` or `}`.
    let mut at_start = true;
    while i < toks.len() {
        let t = &toks[i];
        let next = |k: usize| toks.get(i + k).map(|x| x.text.as_str()).unwrap_or("");
        match t.text.as_str() {
            "{" => {
                stack.push(Scope::Other);
                at_start = true;
                i += 1;
                continue;
            }
            "}" => {
                stack.pop();
                at_start = true;
                i += 1;
                continue;
            }
            ";" => {
                at_start = true;
                i += 1;
                continue;
            }
            _ => {}
        }
        if !at_start {
            i += 1;
            continue;
        }
        at_start = false;
        let in_enum = matches!(stack.last(), Some(Scope::Enum(_)));
        let in_service = matches!(stack.last(), Some(Scope::Service(_)));
        let message = stack.iter().rev().find_map(|sc| if let Scope::Message(n) = sc { Some(n.clone()) } else { None });
        let in_message = matches!(stack.last(), Some(Scope::Message(_)) | Some(Scope::Oneof));
        match t.text.as_str() {
            "syntax" | "edition" | "option" | "reserved" | "extensions" => {
                // `syntax = "proto3";` and friends: their strings are consumed.
                if t.text == "syntax" || t.text == "edition" {
                    strings.next();
                }
            }
            "package" => {
                s.package = Some(next(1).to_string());
            }
            "import" => {
                if let Some(f) = strings.next() {
                    s.imports.push(f);
                }
            }
            "message" | "enum" | "service" if is_ident(next(1)) && next(2) == "{" => {
                let name = next(1).to_string();
                let n = &toks[i + 1];
                s.defs.push(Def { name: name.clone(), kind: t.text.clone(), container: path(&stack, &s.package), line: n.line, col: n.col });
                stack.push(match t.text.as_str() {
                    "message" => Scope::Message(name),
                    "enum" => Scope::Enum(name),
                    _ => Scope::Service(name),
                });
                i += 3;
                at_start = true;
                continue;
            }
            "oneof" if next(2) == "{" => {
                stack.push(Scope::Oneof);
                i += 3;
                at_start = true;
                continue;
            }
            "rpc" if in_service && is_ident(next(1)) => {
                let name = next(1).to_string();
                let n = &toks[i + 1];
                // rpc Name ( [stream] Req ) returns ( [stream] Resp )
                let mut j = i + 2;
                let mut types: Vec<(String, bool, u32, u32)> = Vec::new();
                while j < toks.len() && toks[j].text != ";" && toks[j].text != "{" && types.len() < 2 {
                    if toks[j].text == "(" {
                        let mut k = j + 1;
                        let mut stream = false;
                        if toks.get(k).is_some_and(|x| x.text == "stream") {
                            stream = true;
                            k += 1;
                        }
                        if let Some(x) = toks.get(k).filter(|x| is_ident(&x.text)) {
                            types.push((x.text.clone(), stream, x.line, x.col));
                        }
                        j = k;
                    }
                    j += 1;
                }
                let show = |k: usize| types.get(k).map(|(t, st, _, _)| format!("{}{}", if *st { "stream " } else { "" }, short(t))).unwrap_or_default();
                let kind = format!("rpc ({}) returns ({})", show(0), show(1));
                s.defs.push(Def { name: name.clone(), kind, container: path(&stack, &s.package), line: n.line, col: n.col });
                for (ty, _, line, col) in &types {
                    if !SCALARS.contains(&ty.as_str()) {
                        s.refs.push(Ref { name: short(ty).to_string(), within: name.clone(), line: *line, col: *col });
                    }
                }
                i = j;
                continue;
            }
            "map" if in_message && next(1) == "<" => {
                // map<K, V> name = N;
                let mut j = i + 2;
                while j < toks.len() && toks[j].text != ">" {
                    let x = &toks[j];
                    if is_ident(&x.text) && !SCALARS.contains(&x.text.as_str()) {
                        s.refs.push(Ref { name: short(&x.text).to_string(), within: message.clone().unwrap_or_default(), line: x.line, col: x.col });
                    }
                    j += 1;
                }
                if let Some(n) = toks.get(j + 1).filter(|x| is_ident(&x.text)) {
                    s.defs.push(Def { name: n.text.clone(), kind: "field map".into(), container: path(&stack, &s.package), line: n.line, col: n.col });
                }
                i = j + 1;
                continue;
            }
            w if in_enum && is_ident(w) && next(1) == "=" => {
                s.defs.push(Def { name: w.to_string(), kind: "enum value".into(), container: path(&stack, &s.package), line: t.line, col: t.col });
            }
            w if in_message && is_ident(w) && w != "extend" => {
                // [repeated|optional|required] Type name = N
                let (ty_at, name_at) = if matches!(w, "repeated" | "optional" | "required") { (i + 1, i + 2) } else { (i, i + 1) };
                if let (Some(ty), Some(n)) = (toks.get(ty_at), toks.get(name_at)) {
                    if is_ident(&ty.text) && is_ident(&n.text) && toks.get(name_at + 1).is_some_and(|x| x.text == "=") {
                        s.defs.push(Def { name: n.text.clone(), kind: format!("field {}", short(&ty.text)), container: path(&stack, &s.package), line: n.line, col: n.col });
                        if !SCALARS.contains(&ty.text.as_str()) {
                            s.refs.push(Ref { name: short(&ty.text).to_string(), within: message.clone().unwrap_or_default(), line: ty.line, col: ty.col });
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    s
}

/// The file extensions searched for rpc call sites.
pub const MENTION_EXTS: &[&str] = &[
    "go", "rs", "ts", "tsx", "js", "jsx", "mjs", "py", "java", "kt", "kts", "cs", "swift", "rb", "php", "scala", "c", "cc", "cpp", "h", "hpp", "dart", "ex", "exs",
];

/// Whether a file name is searched for rpc call sites.
pub fn is_mention_file(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, e)| MENTION_EXTS.contains(&e))
}

/// The spellings an rpc is called by: as written, lowerCamel, snake_case.
pub fn spellings(rpc: &str) -> Vec<String> {
    let mut v = vec![rpc.to_string()];
    let mut c = rpc.chars();
    if let Some(f) = c.next() {
        let lower: String = f.to_lowercase().chain(c).collect();
        v.push(lower);
    }
    let mut snake = String::new();
    for (k, ch) in rpc.chars().enumerate() {
        if ch.is_uppercase() {
            if k > 0 {
                snake.push('_');
            }
            snake.extend(ch.to_lowercase());
        } else {
            snake.push(ch);
        }
    }
    v.push(snake);
    v.sort();
    v.dedup();
    v
}

/// The call sites of known rpcs in one file's text: `(rpc, line, col)` for
/// each word that is one of an rpc's spellings.
pub fn mentions(text: &str, by_spelling: &HashMap<String, String>) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    if by_spelling.is_empty() {
        return out;
    }
    for (ln, line) in text.lines().enumerate() {
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i].is_ascii_alphabetic() || b[i] == b'_' {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                if let Some(rpc) = by_spelling.get(&line[s..i]) {
                    out.push((rpc.clone(), ln as u32 + 1, s as u32));
                }
            } else {
                i += 1;
            }
        }
    }
    out
}

/// The spelling map for a set of rpc names (a spelling shared by two rpcs
/// keeps the first, in name order).
pub fn spelling_map(rpcs: &HashSet<String>) -> HashMap<String, String> {
    let mut names: Vec<&String> = rpcs.iter().collect();
    names.sort();
    let mut m = HashMap::new();
    for n in names {
        for s in spellings(n) {
            m.entry(s).or_insert_with(|| n.clone());
        }
    }
    m
}

/// Adds a root's `.proto` rows: definitions, and type references.
///
/// # Arguments
///
/// * `texts` - each `.proto` file's path and text.
/// * `rows` - where rows go.
/// * `files` - the shared file map.
pub fn proto_rows(texts: &[(String, String)], rows: &mut crate::code::Rows, files: &mut HashMap<String, u32>) {
    use crate::code::{Occ, SECT_DEF, SECT_REF};
    for (p, t) in texts {
        let file = rows.file(files, p, "proto");
        let s = scan(t);
        for d in s.defs {
            rows.occ.push(Occ { name: d.name, sect: SECT_DEF, file, line: d.line, col: d.col, kind: d.kind, container: d.container, within: String::new() });
            rows.stats.proto_defs += 1;
        }
        for r in s.refs {
            rows.occ.push(Occ { name: r.name, sect: SECT_REF, file, line: r.line, col: r.col, kind: "type".into(), container: String::new(), within: r.within });
            rows.stats.proto_refs += 1;
        }
    }
}

/// The kind of an rpc call site row.
pub const MENTION_KIND: &str = "call (mention)";

/// Adds rpc call sites found in source files (see [`mentions`]); a file
/// with none adds no row. Generated files ([`crate::langs::is_generated`])
/// are not searched: their stubs are linked to the rpc instead
/// ([`link_rows`]). A word where `taken` holds a row (a definition, or a
/// SCIP reference: the index says what the word is) is no mention, nor is one
/// in a comment or a string of a language of [`crate::langs`] (its
/// scanner's blanking).
pub fn mention_rows(
    texts: &[(String, String)],
    rpcs: &HashSet<String>,
    taken: &HashSet<(u32, u32, u32)>,
    rows: &mut crate::code::Rows,
    files: &mut HashMap<String, u32>,
) {
    let m = spelling_map(rpcs);
    if m.is_empty() {
        return;
    }
    for (p, t) in texts {
        if crate::langs::is_generated(p) {
            continue;
        }
        let mut found = match crate::langs::of_path(p) {
            Some(l) => mentions(&crate::scanner::of(l).blank(t).0, &m),
            None => mentions(t, &m),
        };
        if let Some(&f) = files.get(p.as_str()) {
            found.retain(|(_, line, col)| !taken.contains(&(f, *line, *col)));
        }
        if found.is_empty() {
            continue;
        }
        let lang = if p.ends_with(".rs") { "rust" } else { "mention" };
        let file = rows.file(files, p, lang);
        for (name, line, col) in found {
            rows.occ.push(crate::code::Occ { name, sect: crate::code::SECT_REF, file, line, col, kind: MENTION_KIND.into(), container: String::new(), within: String::new() });
            rows.stats.mentions += 1;
        }
    }
}

/// The rpc names indexed for code roots: those of `root` (`own`), or of
/// every other root.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn indexed_rpcs(conn: &rusqlite::Connection, root: &str, own: bool) -> crate::Result<HashSet<String>> {
    let sql = format!(
        "SELECT DISTINCT o.name FROM code_occ o JOIN code_files f ON f.id = o.file WHERE o.sect = 0 AND o.kind LIKE 'rpc (%' AND f.root {} ?1",
        if own { "=" } else { "<>" }
    );
    let mut st = conn.prepare(&sql)?;
    let v = st.query_map([root], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<String>>>()?;
    Ok(v)
}

/// The kind of an rpc stub link row ([`link_rows`]).
pub const STUB_KIND: &str = "rpc stub";
/// The kind of an rpc implementation link row ([`link_rows`]).
pub const SERVER_KIND: &str = "rpc server";
/// The kind of a reference to an rpc's stub: an exact call site.
pub const CALL_KIND: &str = "rpc";

/// An rpc's name in one of a [`crate::langs::Grpc`] row's forms: `{S}` the
/// service, `{Rpc}` the rpc as written, `{rpc}` lowerCamel, `{rpc_}`
/// snake_case.
fn fill(template: &str, service: &str, rpc: &str) -> String {
    let (lower, snake) = camel_and_snake(rpc);
    template.replace("{S}", service).replace("{Rpc}", rpc).replace("{rpc_}", &snake).replace("{rpc}", &lower)
}

/// An rpc's lowerCamel and snake_case spellings.
fn camel_and_snake(rpc: &str) -> (String, String) {
    let mut c = rpc.chars();
    let lower: String = c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default();
    let mut snake = String::new();
    for (k, ch) in rpc.chars().enumerate() {
        if ch.is_uppercase() {
            if k > 0 {
                snake.push('_');
            }
            snake.extend(ch.to_lowercase());
        } else {
            snake.push(ch);
        }
    }
    (lower, snake)
}

/// The folder of a `/`-separated path (empty at the root).
fn folder(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |x| x.0)
}

/// The identifier words of a line.
fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty())
}

/// The words of a type's header (its definition line up to the line that
/// opens its body) and of its body lines that are a type name alone (a Go
/// embedded field: `pb.UnimplementedCartServiceServer`, its last part).
///
/// # Arguments
///
/// * `lines` - the file's lines.
/// * `start` - the definition's line, 0-based.
/// * `end` - its last line, 0-based.
fn base_words<'a>(lines: &[&'a str], start: usize, end: usize) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut header = true;
    for l in lines.iter().take(end.max(start) + 1).skip(start).take(400) {
        if header {
            out.extend(words(l));
            let t = l.trim_end();
            header = !(t.contains('{') || t.ends_with(':'));
        } else {
            let t = l.trim().trim_start_matches('*').trim_end_matches(';').trim();
            if !t.is_empty() && t.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                out.push(t.rsplit('.').next().unwrap_or(t));
            }
        }
    }
    out
}

/// One language row's names, each to the rpcs (indices) it names.
#[derive(Default)]
struct Names {
    /// Stub method names.
    stubs: HashMap<String, Vec<usize>>,
    /// Implementation method names.
    servers: HashMap<String, Vec<usize>>,
    /// Server base type names, to services.
    bases: HashMap<String, HashSet<String>>,
}

impl Names {
    fn of(g: &crate::langs::Grpc, rpcs: &[(String, String)]) -> Names {
        let mut t = Names::default();
        for (i, (rpc, svc)) in rpcs.iter().enumerate() {
            for (list, out) in [(g.stubs, &mut t.stubs), (g.servers, &mut t.servers)] {
                for tpl in list {
                    let v = out.entry(fill(tpl, svc, rpc)).or_default();
                    if v.last() != Some(&i) {
                        v.push(i);
                    }
                }
            }
            for tpl in g.bases {
                t.bases.entry(fill(tpl, svc, rpc)).or_default().insert(svc.clone());
            }
        }
        t
    }
}

/// Links each rpc of a `.proto` service to the code generated for it and
/// the code that implements it, by the conventions of each language's row
/// ([`crate::langs::Grpc`]):
///
/// * Stubs: a definition in a generated file (`demo_grpc.pb.go`,
///   `demo_pb2_grpc.py`, `CartGrpc.cs`, `CartServiceGrpc.java`) named as
///   the row names an rpc's stub (`GetCart`, `getCart`, `GetCartAsync`),
///   in a type or a file named after the rpc's service, gets an impl row
///   under the rpc's name, of kind [`STUB_KIND`], one per file, side
///   (client or server) and rpc: `client stub CartServiceClient::GetCart`.
/// * Call sites: a reference outside generated code to such a stub (the
///   same name and container: the symbol SCIP resolved it to) is of kind
///   [`CALL_KIND`]; one spelled otherwise (`getCart`) is also stored under
///   the rpc's name, with its caller.
/// * Implementations: a type that extends, implements or embeds a
///   generated server type (`CartServiceBase`,
///   `UnimplementedCartServiceServer`, `CartServiceServicer`,
///   `CartServiceImplBase`), by a SCIP impl edge (followed through
///   hand-written base classes) or by its header or an embedded field, has
///   its methods named after its service's rpcs linked: an impl row of
///   kind [`SERVER_KIND`] under the rpc's name, `CartService for
///   CartService::GetCart`. Methods are looked for in the type's folder (a
///   Go type's methods are in its package's files).
///
/// # Arguments
///
/// * `rows` - the root's rows, extended here.
/// * `rpcs` - every known rpc: (name, its service's short name).
/// * `texts` - source text by path below the root, for type headers.
/// * `foreign` - definitions other code roots hold that may be stubs
///   ([`foreign_stubs`]): (name, container, path). A reference of this root
///   into one (a cross-root reference, [`crate::code::Rows::linked`]) is a
///   call site as a reference into its own stubs is; no stub row is made
///   here (the defining root has it).
pub fn link_rows(rows: &mut crate::code::Rows, rpcs: &[(String, String)], texts: &HashMap<&str, &str>, foreign: &[(String, String, String)]) {
    use crate::code::{Occ, IMPLEMENTS, SECT_DEF, SECT_IMPL, SECT_REF};
    if rpcs.is_empty() {
        return;
    }
    let mut rpcs: Vec<(String, String)> = rpcs.to_vec();
    rpcs.sort();
    rpcs.dedup();
    let rpcs = rpcs;
    // Per file: its language's names, and whether it is generated.
    let mut names: Vec<Names> = Vec::new();
    let mut of_row: HashMap<&'static str, usize> = HashMap::new();
    let mut file_names: Vec<Option<usize>> = Vec::with_capacity(rows.files.len());
    let mut generated: Vec<bool> = Vec::with_capacity(rows.files.len());
    for (p, _) in &rows.files {
        let lang = crate::langs::of_path(p).filter(|l| l.grpc.is_some());
        file_names.push(lang.map(|l| {
            *of_row.entry(l.name).or_insert_with(|| {
                names.push(Names::of(l.grpc.as_ref().expect("filtered"), &rpcs));
                names.len() - 1
            })
        }));
        generated.push(crate::langs::is_generated(p));
    }
    // Stubs in another root's generated code: (name, container) to the rpc.
    let mut foreign_of: HashMap<(&str, &str), usize> = HashMap::new();
    for (name, container, path) in foreign {
        let Some(lang) = crate::langs::of_path(path).filter(|l| l.grpc.is_some() && crate::langs::is_generated(path)) else { continue };
        let row = *of_row.entry(lang.name).or_insert_with(|| {
            names.push(Names::of(lang.grpc.as_ref().expect("filtered"), &rpcs));
            names.len() - 1
        });
        let Some(cands) = names[row].stubs.get(name) else { continue };
        let stem = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        let cont = container.to_ascii_lowercase();
        if let Some(&i) = cands.iter().find(|&&i| {
            let s = rpcs[i].1.to_ascii_lowercase();
            cont.contains(&s) || stem.contains(&s)
        }) {
            foreign_of.entry((name.as_str(), container.as_str())).or_insert(i);
        }
    }
    let names_of = |file: u32| file_names[file as usize].map(|n| &names[n]);
    let mut new: Vec<Occ> = Vec::new();
    // Stubs: (name, container) to the rpc.
    let mut stub_of: HashMap<(&str, &str), usize> = HashMap::new();
    let mut linked: HashSet<(u32, bool, usize)> = HashSet::new();
    for o in rows.occ.iter().filter(|o| o.sect == SECT_DEF && generated[o.file as usize]) {
        let Some(n) = names_of(o.file) else { continue };
        let Some(cands) = n.stubs.get(&o.name) else { continue };
        let path = rows.files[o.file as usize].0.as_str();
        let stem = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        let cont = o.container.to_ascii_lowercase();
        for &i in cands {
            let svc = &rpcs[i].1;
            let s = svc.to_ascii_lowercase();
            if !(cont.contains(&s) || stem.contains(&s)) {
                continue;
            }
            stub_of.entry((o.name.as_str(), o.container.as_str())).or_insert(i);
            let server = n.bases.get(&o.container).is_some_and(|v| v.contains(svc));
            if linked.insert((o.file, server, i)) {
                let side = if server { "server" } else { "client" };
                let at = if o.container.is_empty() { o.name.clone() } else { format!("{}::{}", o.container, o.name) };
                new.push(Occ {
                    name: rpcs[i].0.clone(),
                    sect: SECT_IMPL,
                    file: o.file,
                    line: o.line,
                    col: o.col,
                    kind: STUB_KIND.into(),
                    container: format!("{side} stub {at}"),
                    within: String::new(),
                });
            }
        }
    }
    rows.stats.rpc_stubs += new.len();
    // Call sites.
    let calls: Vec<(usize, usize)> = rows
        .occ
        .iter()
        .enumerate()
        .filter(|(_, o)| o.sect == SECT_REF && o.kind.is_empty() && !generated[o.file as usize])
        .filter_map(|(k, o)| {
            let key = (o.name.as_str(), o.container.as_str());
            stub_of.get(&key).or_else(|| foreign_of.get(&key)).map(|&i| (k, i))
        })
        .collect();
    for (k, i) in calls {
        let o = &mut rows.occ[k];
        o.kind = CALL_KIND.into();
        rows.stats.rpc_calls += 1;
        if o.name != rpcs[i].0 {
            new.push(Occ { name: rpcs[i].0.clone(), ..o.clone() });
        }
    }
    // Impl edges inside generated code under an rpc's name (a Go client
    // struct's method for its interface's) are the stubs' own plumbing:
    // the stub rows stand for them.
    if rows.stats.rpc_stubs > 0 {
        let rpc_names: HashSet<&str> = rpcs.iter().map(|r| r.0.as_str()).collect();
        rows.occ.retain(|o| !(o.sect == SECT_IMPL && o.kind == IMPLEMENTS && generated[o.file as usize] && rpc_names.contains(o.name.as_str())));
    }
    // Implementing types: (type, folder) to the services it serves, from
    // impl edges and type headers, then through edges from those types.
    let mut serves: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut edges: Vec<(String, String, String)> = Vec::new();
    for o in rows.occ.iter().filter(|o| o.sect == SECT_IMPL && o.kind == IMPLEMENTS && !generated[o.file as usize]) {
        let Some((base, imp)) = o.container.split_once(" for ") else { continue };
        if base.contains("::") || imp != o.name {
            continue;
        }
        let dir = folder(&rows.files[o.file as usize].0).to_string();
        if let Some(v) = names_of(o.file).and_then(|n| n.bases.get(base)) {
            serves.entry((imp.to_string(), dir.clone())).or_default().extend(v.iter().cloned());
        }
        edges.push((base.to_string(), imp.to_string(), dir));
    }
    let mut file_lines: HashMap<u32, Option<Vec<&str>>> = HashMap::new();
    for o in rows.occ.iter().filter(|o| o.sect == SECT_DEF && !generated[o.file as usize]) {
        let kw = o.kind.split(' ').next().unwrap_or("");
        if !matches!(kw, "class" | "struct" | "type" | "record" | "object" | "interface") {
            continue;
        }
        let Some(n) = names_of(o.file).filter(|n| !n.bases.is_empty()) else { continue };
        let path = rows.files[o.file as usize].0.as_str();
        // A file's lines, when a word of it names a server base type.
        let Some(lines) = file_lines
            .entry(o.file)
            .or_insert_with(|| texts.get(path).filter(|t| t.lines().any(|l| words(l).any(|w| n.bases.contains_key(w)))).map(|t| t.lines().collect()))
        else {
            continue;
        };
        let start = o.line.saturating_sub(1) as usize;
        if start >= lines.len() {
            continue;
        }
        let end = rows.ends.get(&(o.file, o.line)).map(|&e| e as usize).or_else(|| crate::digest::brace_body_end(lines, start)).unwrap_or(o.line as usize);
        let found: HashSet<String> = base_words(lines, start, end.saturating_sub(1)).into_iter().filter_map(|w| n.bases.get(w)).flatten().cloned().collect();
        if !found.is_empty() {
            serves.entry((o.name.clone(), folder(path).to_string())).or_default().extend(found);
        }
    }
    // Through edges from implementing types (a servicer's subclasses).
    for _ in 0..edges.len() {
        let mut grew = false;
        for (base, imp, dir) in &edges {
            let from: HashSet<String> = serves.iter().filter(|((t, _), _)| t == base).flat_map(|(_, v)| v.iter().cloned()).collect();
            if from.is_empty() {
                continue;
            }
            let e = serves.entry((imp.clone(), dir.clone())).or_default();
            let had = e.len();
            e.extend(from);
            grew |= e.len() > had;
        }
        if !grew {
            break;
        }
    }
    if !serves.is_empty() {
        let mut members: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (k, o) in rows.occ.iter().enumerate() {
            if o.sect == SECT_DEF && !generated[o.file as usize] && !o.container.is_empty() {
                members.entry((folder(&rows.files[o.file as usize].0), o.container.as_str())).or_default().push(k);
            }
        }
        let mut done: HashSet<(usize, u32, u32)> = HashSet::new();
        let mut keys: Vec<&(String, String)> = serves.keys().collect();
        keys.sort();
        let before = new.len();
        for key in keys {
            let svcs = &serves[key];
            for &k in members.get(&(key.1.as_str(), key.0.as_str())).map_or(&[][..], |v| v.as_slice()) {
                let o = &rows.occ[k];
                let Some(cands) = names_of(o.file).and_then(|n| n.servers.get(&o.name)) else { continue };
                for &i in cands.iter().filter(|&&i| svcs.contains(&rpcs[i].1)) {
                    if done.insert((i, o.file, o.line)) {
                        new.push(Occ {
                            name: rpcs[i].0.clone(),
                            sect: SECT_IMPL,
                            file: o.file,
                            line: o.line,
                            col: o.col,
                            kind: SERVER_KIND.into(),
                            container: format!("{} for {}::{}", rpcs[i].1, o.container, o.name),
                            within: String::new(),
                        });
                    }
                }
            }
        }
        rows.stats.rpc_servers += new.len() - before;
        // An impl edge the index gave on the same line under the rpc's name
        // (a C# override of the base's method) is replaced by the link.
        let mine: HashSet<(&str, u32, u32)> = new[before..].iter().map(|o| (o.name.as_str(), o.file, o.line)).collect();
        rows.occ.retain(|o| !(o.sect == SECT_IMPL && o.kind == IMPLEMENTS && mine.contains(&(o.name.as_str(), o.file, o.line))));
    }
    rows.occ.extend(new);
}

/// The definitions other code roots than `root` hold under a name some
/// language's stub of an rpc has ([`crate::langs::Grpc::stubs`]), in
/// generated files: (name, container, path), for [`link_rows`]'s
/// cross-root call sites.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn foreign_stubs(conn: &rusqlite::Connection, root: &str, rpcs: &[(String, String)]) -> crate::Result<Vec<(String, String, String)>> {
    let mut names: Vec<String> = crate::langs::LANGS.iter().filter_map(|l| l.grpc.as_ref()).flat_map(|g| Names::of(g, rpcs).stubs.into_keys()).collect();
    names.sort();
    names.dedup();
    let mut st = conn.prepare_cached("SELECT o.container, f.path FROM code_occ o JOIN code_files f ON f.id = o.file WHERE o.name = ?1 AND o.sect = 0 AND f.root <> ?2")?;
    let mut out = Vec::new();
    for n in names {
        let rows = st.query_map(rusqlite::params![n, root], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for r in rows {
            let (container, path) = r?;
            if crate::langs::is_generated(&path) {
                out.push((n.clone(), container, path));
            }
        }
    }
    Ok(out)
}

/// The rpcs indexed for code roots other than `root`: (name, the short
/// name of its service).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn indexed_rpc_services(conn: &rusqlite::Connection, root: &str) -> crate::Result<Vec<(String, String)>> {
    let mut st = conn.prepare(
        "SELECT DISTINCT o.name, o.container FROM code_occ o JOIN code_files f ON f.id = o.file WHERE o.sect = 0 AND o.kind LIKE 'rpc (%' AND f.root <> ?1",
    )?;
    let v = st
        .query_map([root], |r| Ok((r.get::<_, String>(0)?, service_of(&r.get::<_, String>(1)?))))?
        .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
    Ok(v)
}

/// The short name of an rpc's service from its container
/// (`hipstershop.CartService`: `CartService`).
pub fn service_of(container: &str) -> String {
    container.rsplit('.').next().unwrap_or(container).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BILLING: &str = r#"syntax = "proto3";
// message Commented { }
package acme.billing.v1;
import "acme/auth/v1/auth.proto";

/* service Hidden {} */
message Invoice {
  string id = 1;
  repeated LineItem items = 2;
  acme.auth.v1.Principal owner = 3 [deprecated = true];
  map<string, Money> totals = 4;
  message LineItem {
    string sku = 1;
    Money price = 2;
  }
  oneof state {
    Paid paid = 5;
  }
  enum Status { STATUS_UNSPECIFIED = 0; STATUS_OPEN = 1; }
}

service InvoiceService {
  rpc GetInvoice(GetInvoiceRequest) returns (Invoice);
  rpc WatchInvoices(stream WatchRequest) returns (stream .acme.billing.v1.Invoice) {
    option (google.api.http) = { get: "/v1/invoices" };
  }
}
"#;

    #[test]
    fn scans_definitions_and_references() {
        let s = scan(BILLING);
        assert_eq!(s.package.as_deref(), Some("acme.billing.v1"));
        assert_eq!(s.imports, vec!["acme/auth/v1/auth.proto"]);
        let def = |n: &str| s.defs.iter().find(|d| d.name == n).unwrap_or_else(|| panic!("no def {n}: {:?}", s.defs));
        assert_eq!(def("Invoice").kind, "message");
        assert_eq!(def("Invoice").line, 7);
        assert_eq!(def("LineItem").container, "acme.billing.v1.Invoice");
        assert_eq!(def("sku").kind, "field string");
        assert_eq!(def("paid").container, "acme.billing.v1.Invoice", "oneof fields belong to the message");
        assert_eq!(def("STATUS_OPEN").kind, "enum value");
        assert_eq!(def("InvoiceService").kind, "service");
        assert_eq!(def("GetInvoice").kind, "rpc (GetInvoiceRequest) returns (Invoice)");
        assert_eq!(def("GetInvoice").container, "acme.billing.v1.InvoiceService");
        assert_eq!(def("WatchInvoices").kind, "rpc (stream WatchRequest) returns (stream Invoice)");
        assert_eq!(def("totals").kind, "field map");
        assert!(!s.defs.iter().any(|d| d.name == "Commented" || d.name == "Hidden"), "comments define nothing");
        let refs: Vec<(&str, &str)> = s.refs.iter().map(|r| (r.name.as_str(), r.within.as_str())).collect();
        for want in [("LineItem", "Invoice"), ("Principal", "Invoice"), ("Money", "Invoice"), ("Money", "LineItem"), ("GetInvoiceRequest", "GetInvoice"), ("Invoice", "GetInvoice"), ("Invoice", "WatchInvoices"), ("Paid", "Invoice")] {
            assert!(refs.contains(&want), "missing {want:?} in {refs:?}");
        }
        assert!(!refs.iter().any(|(n, _)| *n == "string"), "scalars are not references");
    }

    #[test]
    fn finds_rpc_mentions_in_any_spelling() {
        let rpcs: HashSet<String> = ["GetInvoice".to_string()].into_iter().collect();
        let m = spelling_map(&rpcs);
        let go = "func f() {\n  inv, err := client.GetInvoice(ctx, req)\n}\n";
        assert_eq!(mentions(go, &m), vec![("GetInvoice".to_string(), 2, 21)]);
        let ts = "const x = await api.getInvoice({id});";
        assert_eq!(mentions(ts, &m).len(), 1);
        let rs = "let r = client.get_invoice(req).await?;";
        assert_eq!(mentions(rs, &m).len(), 1);
        assert!(mentions("GetInvoices getInvoiceX", &m).is_empty(), "whole words only");
    }

    fn row(name: &str, sect: i64, file: u32, line: u32, kind: &str, container: &str) -> crate::code::Occ {
        crate::code::Occ { name: name.into(), sect, file, line, col: 4, kind: kind.into(), container: container.into(), within: "caller".into() }
    }

    #[test]
    fn links_java_and_csharp_stubs_calls_and_servers_through_base_classes() {
        use crate::code::{Rows, IMPLEMENTS, SECT_DEF, SECT_IMPL, SECT_REF};
        let mut rows = Rows::default();
        for (p, l) in [
            ("build/gen/CartServiceGrpc.java", "java"),
            ("src/Client.java", "java"),
            ("src/CartImpl.java", "java"),
            ("obj/CartGrpc.cs", "csharp"),
            ("Services/CartService.cs", "csharp"),
            ("Tests/CartTests.cs", "csharp"),
        ] {
            rows.files.push((p.into(), l));
        }
        rows.occ = vec![
            // Java: a blocking stub, a call through it, a server two classes
            // down from the generated base.
            row("getCart", SECT_DEF, 0, 10, "method", "CartServiceBlockingStub"),
            row("getCart", SECT_DEF, 0, 20, "method", "CartServiceImplBase"),
            row("getCart", SECT_REF, 1, 5, "", "CartServiceBlockingStub"),
            row("getCart", SECT_REF, 1, 6, "", "OtherStub"),
            row("BaseCart", SECT_IMPL, 2, 3, IMPLEMENTS, "CartServiceImplBase for BaseCart"),
            row("CartImpl", SECT_IMPL, 2, 9, IMPLEMENTS, "BaseCart for CartImpl"),
            row("getCart", SECT_DEF, 2, 11, "method", "CartImpl"),
            row("helper", SECT_DEF, 2, 14, "method", "CartImpl"),
            // C#: an async client method, its base, an override whose impl
            // edge the index already gave.
            row("GetCartAsync", SECT_DEF, 3, 30, "method", "CartServiceClient"),
            row("GetCart", SECT_DEF, 3, 40, "method", "CartServiceBase"),
            row("CartService", SECT_IMPL, 4, 8, IMPLEMENTS, "CartServiceBase for CartService"),
            row("GetCart", SECT_DEF, 4, 12, "method", "CartService"),
            row("GetCart", SECT_IMPL, 4, 12, IMPLEMENTS, "CartServiceBase::GetCart for CartService::GetCart"),
            row("GetCartAsync", SECT_REF, 5, 7, "", "CartServiceClient"),
        ];
        let rpcs = vec![("GetCart".to_string(), "CartService".to_string())];
        link_rows(&mut rows, &rpcs, &HashMap::new(), &[]);
        let got = |kind: &str| -> Vec<String> {
            let mut v: Vec<String> =
                rows.occ.iter().filter(|o| o.name == "GetCart" && o.kind == kind).map(|o| format!("{}:{} {}", o.file, o.line, o.container)).collect();
            v.sort();
            v
        };
        assert_eq!(
            got(STUB_KIND),
            [
                "0:10 client stub CartServiceBlockingStub::getCart",
                "0:20 server stub CartServiceImplBase::getCart",
                "3:30 client stub CartServiceClient::GetCartAsync",
                "3:40 server stub CartServiceBase::GetCart"
            ]
        );
        assert_eq!(got(SERVER_KIND), ["2:11 CartService for CartImpl::getCart", "4:12 CartService for CartService::GetCart"]);
        // Calls: kept under the stub's name and stored under the rpc's.
        assert_eq!(got(CALL_KIND), ["1:5 CartServiceBlockingStub", "5:7 CartServiceClient"]);
        assert!(rows.occ.iter().any(|o| o.name == "getCart" && o.line == 5 && o.kind == CALL_KIND && o.within == "caller"));
        assert!(rows.occ.iter().any(|o| o.name == "getCart" && o.line == 6 && o.kind.is_empty()), "another type's getCart is no call");
        assert!(!rows.occ.iter().any(|o| o.kind == IMPLEMENTS && o.name == "GetCart"), "the override's edge is the link now");
        assert_eq!((rows.stats.rpc_stubs, rows.stats.rpc_calls, rows.stats.rpc_servers), (4, 2, 2));
    }

    #[test]
    fn mentions_skip_generated_files_comments_strings_and_taken_words() {
        use crate::code::Rows;
        let rpcs: HashSet<String> = ["GetCart".to_string()].into_iter().collect();
        let texts = vec![
            ("gen/demo_grpc.pb.go".to_string(), "func GetCart() {}\n".to_string()),
            ("app/main.go".to_string(), "// GetCart here\nx := \"GetCart\"\nc.GetCart()\nd.GetCart()\n".to_string()),
            ("tool.rb".to_string(), "# GetCart\n".to_string()),
        ];
        let mut rows = Rows::default();
        let mut files = HashMap::new();
        let f = rows.file(&mut files, "app/main.go", "go");
        let taken: HashSet<(u32, u32, u32)> = [(f, 4, 2)].into_iter().collect();
        mention_rows(&texts, &rpcs, &taken, &mut rows, &mut files);
        let got: Vec<(String, u32)> = rows.occ.iter().map(|o| (rows.files[o.file as usize].0.clone(), o.line)).collect();
        assert_eq!(got, [("app/main.go".to_string(), 3), ("tool.rb".to_string(), 1)], "a file of no language row is searched as it is");
    }
}
