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
//! [`mention_rows`] adds call sites: in a root's other source files, a word
//! that is a known rpc's name (as written, in lowerCamel or in snake_case)
//! is a reference of kind `call (mention)`. The rpc names come from every
//! root of the project, so a gateway's `client.GetInvoice(...)` answers
//! "who calls GetInvoice" for the billing service's proto.

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
/// with none adds no row.
pub fn mention_rows(texts: &[(String, String)], rpcs: &HashSet<String>, rows: &mut crate::code::Rows, files: &mut HashMap<String, u32>) {
    let m = spelling_map(rpcs);
    if m.is_empty() {
        return;
    }
    for (p, t) in texts {
        let found = mentions(t, &m);
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
}
