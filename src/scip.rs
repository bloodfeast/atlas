//! A decoder for SCIP indexes (the `index.scip` that `rust-analyzer scip`
//! writes), over the owned [`crate::protobuf`] reader.
//!
//! Only the fields the code index uses are read; everything else is
//! skipped as protobuf requires. Field numbers are from `scip.proto`
//! (github.com/sourcegraph/scip):
//!
//! | message | fields read |
//! |---|---|
//! | `Index` | `metadata` 1, `documents` 2, `external_symbols` 3 |
//! | `Metadata` | `tool_info` 2 (`name` 1, `version` 2), `project_root` 3 |
//! | `Document` | `relative_path` 1, `occurrences` 2, `symbols` 3, `language` 4 |
//! | `Occurrence` | `range` 1 (packed int32: 3 or 4 values), `symbol` 2, `symbol_roles` 3, `enclosing_range` 7 |
//! | `SymbolInformation` | `symbol` 1, `relationships` 4, `kind` 5, `display_name` 6, `enclosing_symbol` 8 |
//! | `Relationship` | `symbol` 1, `is_reference` 2, `is_implementation` 3, `is_type_definition` 4, `is_definition` 5 |
//!
//! The result is struct-of-arrays: symbols interned once as `&str` slices
//! of the input (no copies), occurrences as parallel columns. Local symbols
//! (`local N`, scoped to one document) are dropped at decode time unless
//! asked for: nothing outside their function can name them.

use std::collections::HashMap;

use crate::protobuf::{DecodeError, Reader, WireType};

/// `SymbolRole.Definition`.
pub const ROLE_DEFINITION: u32 = 0x1;
/// `SymbolRole.Import`.
pub const ROLE_IMPORT: u32 = 0x2;
/// `SymbolRole.WriteAccess`.
pub const ROLE_WRITE: u32 = 0x4;

/// `Relationship.is_reference`.
pub const REL_REFERENCE: u8 = 1;
/// `Relationship.is_implementation`.
pub const REL_IMPLEMENTATION: u8 = 2;
/// `Relationship.is_type_definition`.
pub const REL_TYPE_DEFINITION: u8 = 4;
/// `Relationship.is_definition`.
pub const REL_DEFINITION: u8 = 8;

/// No value, in the `i32` range columns.
pub const NONE: i32 = -1;

/// One `SymbolInformation`, by interned symbol id.
#[derive(Debug, Clone, Copy, Default)]
pub struct SymInfo<'a> {
    /// `SymbolInformation.Kind` (0 when the indexer left it out).
    pub kind: i32,
    /// The display name, when given.
    pub display_name: &'a str,
    /// The enclosing symbol id, when given.
    pub enclosing: Option<u32>,
    /// True once a `SymbolInformation` for the symbol was read.
    pub known: bool,
}

/// A decoded index.
#[derive(Debug, Default)]
pub struct Index<'a> {
    /// `Metadata.project_root` (a `file://` URI).
    pub project_root: &'a str,
    /// `Metadata.tool_info.name` and `version`.
    pub tool: (&'a str, &'a str),
    /// Each document's `relative_path`.
    pub doc_path: Vec<&'a str>,
    /// Each document's `language`.
    pub doc_lang: Vec<&'a str>,
    /// Interned symbol strings; an id is an index here.
    pub symbols: Vec<&'a str>,
    /// Per symbol id, what its `SymbolInformation` said.
    pub info: Vec<SymInfo<'a>>,
    /// Relationships: from symbol, to symbol, flags (`REL_*`).
    pub rel_from: Vec<u32>,
    /// See [`Index::rel_from`].
    pub rel_to: Vec<u32>,
    /// See [`Index::rel_from`].
    pub rel_flags: Vec<u8>,
    /// Occurrence columns: document index.
    pub occ_doc: Vec<u32>,
    /// Occurrence columns: symbol id.
    pub occ_sym: Vec<u32>,
    /// Occurrence columns: `symbol_roles`.
    pub occ_roles: Vec<u32>,
    /// Occurrence columns: range start line (0-based).
    pub occ_line: Vec<i32>,
    /// Occurrence columns: range start character.
    pub occ_col: Vec<i32>,
    /// Occurrence columns: range end line.
    pub occ_end_line: Vec<i32>,
    /// Occurrence columns: range end character.
    pub occ_end_col: Vec<i32>,
    /// Occurrence columns: `enclosing_range` start line, or [`NONE`].
    pub occ_encl_start: Vec<i32>,
    /// Occurrence columns: `enclosing_range` end line, or [`NONE`].
    pub occ_encl_end: Vec<i32>,
    /// Local-symbol occurrences dropped.
    pub locals_skipped: usize,
    intern: HashMap<&'a str, u32>,
}

/// Decoding options.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Keep `local N` symbols (dropped by default).
    pub keep_locals: bool,
}

fn is_local(s: &str) -> bool {
    s.starts_with("local ")
}

impl<'a> Index<'a> {
    /// The id of a symbol string, interning it if new.
    fn intern(&mut self, s: &'a str) -> u32 {
        if let Some(&id) = self.intern.get(s) {
            return id;
        }
        let id = self.symbols.len() as u32;
        self.symbols.push(s);
        self.info.push(SymInfo::default());
        self.intern.insert(s, id);
        id
    }

    /// The id of a symbol, if it occurs anywhere in the index.
    ///
    /// # Arguments
    ///
    /// * `s` - the full SCIP symbol string.
    pub fn id(&self, s: &str) -> Option<u32> {
        self.intern.get(s).copied()
    }

    /// The number of occurrences.
    pub fn occurrences(&self) -> usize {
        self.occ_sym.len()
    }
}

/// Decodes a whole SCIP file.
///
/// # Arguments
///
/// * `buf` - the file's bytes; the result borrows its strings from it.
/// * `opts` - decoding options.
///
/// # Errors
///
/// [`DecodeError`] for malformed protobuf, a non-UTF-8 string, or an
/// occurrence range that is not 3 or 4 values.
pub fn decode(buf: &[u8], opts: Options) -> Result<Index<'_>, DecodeError> {
    let mut ix = Index::default();
    let mut r = Reader::new(buf);
    while let Some((f, wt)) = r.field()? {
        match (f, wt) {
            (1, WireType::Len) => metadata(&mut ix, r.bytes()?)?,
            (2, WireType::Len) => document(&mut ix, r.bytes()?, opts)?,
            (3, WireType::Len) => symbol_info(&mut ix, r.bytes()?)?,
            _ => r.skip(wt)?,
        }
    }
    Ok(ix)
}

fn metadata<'a>(ix: &mut Index<'a>, buf: &'a [u8]) -> Result<(), DecodeError> {
    let mut r = Reader::new(buf);
    while let Some((f, wt)) = r.field()? {
        match (f, wt) {
            (2, WireType::Len) => {
                let mut t = Reader::new(r.bytes()?);
                while let Some((g, wt2)) = t.field()? {
                    match (g, wt2) {
                        (1, WireType::Len) => ix.tool.0 = t.string()?,
                        (2, WireType::Len) => ix.tool.1 = t.string()?,
                        _ => t.skip(wt2)?,
                    }
                }
            }
            (3, WireType::Len) => ix.project_root = r.string()?,
            _ => r.skip(wt)?,
        }
    }
    Ok(())
}

fn document<'a>(ix: &mut Index<'a>, buf: &'a [u8], opts: Options) -> Result<(), DecodeError> {
    let doc = ix.doc_path.len() as u32;
    ix.doc_path.push("");
    ix.doc_lang.push("");
    let mut r = Reader::new(buf);
    let mut range = Vec::with_capacity(4);
    let mut encl = Vec::with_capacity(4);
    while let Some((f, wt)) = r.field()? {
        match (f, wt) {
            (1, WireType::Len) => ix.doc_path[doc as usize] = r.string()?,
            (4, WireType::Len) => ix.doc_lang[doc as usize] = r.string()?,
            (3, WireType::Len) => symbol_info(ix, r.bytes()?)?,
            (2, WireType::Len) => {
                let at = r.pos();
                let mut o = Reader::new(r.bytes()?);
                range.clear();
                encl.clear();
                let mut sym = "";
                let mut roles = 0u32;
                while let Some((g, wt2)) = o.field()? {
                    match g {
                        1 => o.int32s(wt2, &mut range)?,
                        2 if wt2 == WireType::Len => sym = o.string()?,
                        3 if wt2 == WireType::Varint => roles = o.varint()? as u32,
                        7 => o.int32s(wt2, &mut encl)?,
                        _ => o.skip(wt2)?,
                    }
                }
                if sym.is_empty() {
                    continue;
                }
                if !opts.keep_locals && is_local(sym) {
                    ix.locals_skipped += 1;
                    continue;
                }
                let (l0, c0, l1, c1) = match range[..] {
                    [l, c, e] => (l, c, l, e),
                    [l, c, el, ec] => (l, c, el, ec),
                    _ => return Err(DecodeError { what: "occurrence range is not 3 or 4 values", at }),
                };
                let (e0, e1) = match encl[..] {
                    [l, _, e] => (l, e.min(i32::MAX).max(l)),
                    [l, _, el, _] => (l, el),
                    _ => (NONE, NONE),
                };
                let id = ix.intern(sym);
                ix.occ_doc.push(doc);
                ix.occ_sym.push(id);
                ix.occ_roles.push(roles);
                ix.occ_line.push(l0);
                ix.occ_col.push(c0);
                ix.occ_end_line.push(l1);
                ix.occ_end_col.push(c1);
                ix.occ_encl_start.push(e0);
                // A 3-value enclosing range ends on its start line.
                ix.occ_encl_end.push(if encl.len() == 3 { e0 } else { e1 });
            }
            _ => r.skip(wt)?,
        }
    }
    Ok(())
}

fn symbol_info<'a>(ix: &mut Index<'a>, buf: &'a [u8]) -> Result<(), DecodeError> {
    let mut r = Reader::new(buf);
    let mut sym = "";
    let mut kind = 0;
    let mut display = "";
    let mut enclosing = "";
    let mut rels: Vec<(&'a str, u8)> = Vec::new();
    while let Some((f, wt)) = r.field()? {
        match (f, wt) {
            (1, WireType::Len) => sym = r.string()?,
            (4, WireType::Len) => {
                let mut x = Reader::new(r.bytes()?);
                let mut to = "";
                let mut flags = 0u8;
                while let Some((g, wt2)) = x.field()? {
                    match (g, wt2) {
                        (1, WireType::Len) => to = x.string()?,
                        (2..=5, WireType::Varint) => {
                            if x.varint()? != 0 {
                                flags |= 1 << (g - 2);
                            }
                        }
                        _ => x.skip(wt2)?,
                    }
                }
                if !to.is_empty() {
                    rels.push((to, flags));
                }
            }
            (5, WireType::Varint) => kind = r.int32()?,
            (6, WireType::Len) => display = r.string()?,
            (8, WireType::Len) => enclosing = r.string()?,
            _ => r.skip(wt)?,
        }
    }
    if sym.is_empty() || is_local(sym) {
        return Ok(());
    }
    let id = ix.intern(sym);
    let enc = (!enclosing.is_empty()).then(|| ix.intern(enclosing));
    let i = &mut ix.info[id as usize];
    i.known = true;
    i.kind = kind;
    if !display.is_empty() {
        i.display_name = display;
    }
    if enc.is_some() {
        i.enclosing = enc;
    }
    for (to, flags) in rels {
        if is_local(to) {
            continue;
        }
        let t = ix.intern(to);
        ix.rel_from.push(id);
        ix.rel_to.push(t);
        ix.rel_flags.push(flags);
    }
    Ok(())
}

/// One descriptor of a SCIP symbol: its name and suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor<'a> {
    /// The name, without backtick escaping (a doubled backtick stays
    /// doubled; such names are vanishingly rare in Rust).
    pub name: &'a str,
    /// The suffix: `/` namespace, `#` type, `.` term, `(` method (with
    /// its disambiguator dropped), `[` type parameter, `)` parameter, `:`
    /// meta, `!` macro.
    pub suffix: char,
}

/// An escaped name starting at `start` (just past its opening backtick):
/// returns (start, end), `end` at the closing backtick or the end of input.
/// A doubled backtick inside is an escaped backtick and stays in the slice.
fn escaped(b: &[u8], start: usize) -> (usize, usize) {
    let mut j = start;
    while j < b.len() {
        if b[j] == b'`' {
            if b.get(j + 1) == Some(&b'`') {
                j += 2;
                continue;
            }
            break;
        }
        j += 1;
    }
    (start.min(b.len()), j.min(b.len()))
}

/// Splits a global SCIP symbol into its descriptors (the part after the
/// scheme, manager, package name and version).
///
/// # Arguments
///
/// * `sym` - the symbol string, e.g.
///   `rust-analyzer cargo demo 0.1.0 store/Store#open().`.
///
/// # Returns
///
/// The descriptors in order, or an empty list for a local or malformed
/// symbol.
pub fn descriptors(sym: &str) -> Vec<Descriptor<'_>> {
    // Four space-separated header fields; a doubled space is an escaped space.
    let mut rest = sym;
    for _ in 0..4 {
        let mut i = 0;
        let b = rest.as_bytes();
        loop {
            match b.get(i) {
                None => return Vec::new(),
                Some(b' ') if b.get(i + 1) == Some(&b' ') => i += 2,
                Some(b' ') => break,
                Some(_) => i += 1,
            }
        }
        rest = &rest[i + 1..];
    }
    let mut out = Vec::new();
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'[' | b'(' => {
                // `[name]` type parameter, `(name)` parameter.
                // The name may be escaped in backticks (`[`[u8]`]`, a type
                // holding the close bracket): the escape ends it, not the
                // first bracket.
                let close = if b[i] == b'[' { b']' } else { b')' };
                let (name, after) = if b.get(i + 1) == Some(&b'`') {
                    let (start, end) = escaped(b, i + 2);
                    (&rest[start..end], end + 1)
                } else {
                    let start = i + 1;
                    let end = rest[start..].find(close as char).map(|e| start + e).unwrap_or(b.len());
                    (&rest[start..end], end)
                };
                out.push(Descriptor { name, suffix: if close == b']' { '[' } else { ')' } });
                // Past the close bracket.
                i = rest[after.min(b.len())..].find(close as char).map_or(b.len(), |e| after + e + 1);
            }
            b'`' => {
                let (start, j) = escaped(b, i + 1);
                let name = &rest[start..j];
                i = j + 1;
                i = push_suffix(&mut out, rest, name, i);
            }
            _ => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'+' | b'-' | b'$') || b[i] >= 0x80) {
                    i += 1;
                }
                if i == start {
                    // Not a descriptor character: stop rather than loop.
                    break;
                }
                let name = &rest[start..i];
                i = push_suffix(&mut out, rest, name, i);
            }
        }
    }
    out
}

fn push_suffix<'a>(out: &mut Vec<Descriptor<'a>>, rest: &'a str, name: &'a str, mut i: usize) -> usize {
    let b = rest.as_bytes();
    match b.get(i) {
        Some(b'(') => {
            // Method: `name(disambiguator).`
            let end = rest[i..].find(')').map(|e| i + e).unwrap_or(b.len());
            i = end + 1;
            if b.get(i) == Some(&b'.') {
                i += 1;
            }
            out.push(Descriptor { name, suffix: '(' });
        }
        Some(&c @ (b'/' | b'#' | b'.' | b':' | b'!')) => {
            out.push(Descriptor { name, suffix: c as char });
            i += 1;
        }
        _ => out.push(Descriptor { name, suffix: '.' }),
    }
    i
}

/// A human word for a symbol's shape, from its last descriptor (used when
/// the indexer gave no `kind`).
pub fn suffix_word(d: &Descriptor<'_>) -> &'static str {
    match d.suffix {
        '/' => "mod",
        '#' => "type",
        '(' => "fn",
        '!' => "macro",
        '[' => "type param",
        ')' => "param",
        ':' => "meta",
        _ => "term",
    }
}

/// A human word for `SymbolInformation.Kind`, for the values
/// rust-analyzer emits (read from its output: 8 constant, 11 enum, 12 enum
/// member, 15 field, 17 function, 25 macro, 26 method, 29 module, 49
/// struct, 53 trait, 54 type, 55 type alias, 60 union, 70 trait method, 80
/// static method, 82 static variable); `None` for others.
pub fn kind_word(kind: i32) -> Option<&'static str> {
    Some(match kind {
        3 => "assoc type",
        8 => "const",
        11 => "enum",
        12 => "variant",
        15 => "field",
        17 => "fn",
        25 => "macro",
        26 => "method",
        29 => "mod",
        49 => "struct",
        53 => "trait",
        54 => "type",
        55 => "type alias",
        60 => "union",
        70 => "trait method",
        80 => "assoc fn",
        82 => "static",
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protobuf::Writer;

    /// Builds a small SCIP index: two documents, a struct with a method, a
    /// trait implemented by the struct, a reference from another file, an
    /// external symbol and a local.
    pub(crate) fn sample() -> Vec<u8> {
        let s_struct = "rust-analyzer cargo demo 0.1.0 store/Store#";
        let s_open = "rust-analyzer cargo demo 0.1.0 store/Store#open().";
        let s_trait = "rust-analyzer cargo demo 0.1.0 store/Open#";
        let s_ext = "rust-analyzer cargo std 1.0.0 vec/Vec#";
        let occ = |range: &[i32], sym: &str, roles: i64, encl: &[i32]| {
            let mut o = Writer::default();
            o.packed(1, range).string(2, sym);
            if roles != 0 {
                o.int(3, roles);
            }
            if !encl.is_empty() {
                o.packed(7, encl);
            }
            o
        };
        let info = |sym: &str, kind: i64, name: &str, rel: Option<(&str, i64)>| {
            let mut i = Writer::default();
            i.string(1, sym).int(5, kind).string(6, name);
            if let Some((to, field)) = rel {
                let mut r = Writer::default();
                r.string(1, to).int(field as u32, 1);
                i.message(4, &r);
            }
            i
        };
        let mut d1 = Writer::default();
        d1.string(1, "src/store.rs").string(4, "rust");
        d1.message(2, &occ(&[0, 11, 16], s_struct, 1, &[0, 0, 9, 1]));
        d1.message(2, &occ(&[2, 7, 11], s_open, 1, &[2, 4, 4, 5]));
        d1.message(2, &occ(&[3, 8, 11], s_ext, 0, &[]));
        d1.message(2, &occ(&[3, 4, 5], "local 0", 1, &[]));
        d1.message(2, &occ(&[10, 10, 14], s_trait, 1, &[]));
        d1.message(3, &info(s_struct, 49, "Store", Some((s_trait, 3))));
        d1.message(3, &info(s_open, 26, "open", None));
        d1.message(3, &info(s_trait, 53, "Open", None));
        let mut d2ok = Writer::default();
        d2ok.string(1, "src/main.rs").string(4, "rust");
        d2ok.message(2, &occ(&[5, 4, 9], s_struct, 0, &[]));
        d2ok.message(2, &occ(&[5, 11, 6, 2], s_open, 0x8, &[]));
        let mut meta = Writer::default();
        let mut tool = Writer::default();
        tool.string(1, "rust-analyzer").string(2, "0.0.0");
        meta.message(2, &tool).string(3, "file:///demo/");
        let mut ix = Writer::default();
        ix.message(1, &meta).message(2, &d1).message(2, &d2ok).message(3, &info(s_ext, 49, "Vec", None));
        ix.buf
    }

    #[test]
    fn decodes_documents_occurrences_and_symbols() {
        let buf = sample();
        let ix = decode(&buf, Options::default()).unwrap();
        assert_eq!(ix.tool, ("rust-analyzer", "0.0.0"));
        assert_eq!(ix.project_root, "file:///demo/");
        assert_eq!(ix.doc_path, ["src/store.rs", "src/main.rs"]);
        assert_eq!(ix.occurrences(), 6);
        assert_eq!(ix.locals_skipped, 1);
        let open = ix.id("rust-analyzer cargo demo 0.1.0 store/Store#open().").unwrap();
        let i = ix.occ_sym.iter().rposition(|&s| s == open).unwrap();
        assert_eq!((ix.occ_line[i], ix.occ_col[i], ix.occ_end_line[i], ix.occ_end_col[i]), (5, 11, 6, 2));
        assert_eq!(ix.occ_roles[i], 0x8);
        let d = ix.occ_sym.iter().position(|&s| s == open).unwrap();
        assert_eq!((ix.occ_encl_start[d], ix.occ_encl_end[d]), (2, 4));
        assert_eq!(ix.occ_roles[d] & ROLE_DEFINITION, ROLE_DEFINITION);
        let st = ix.id("rust-analyzer cargo demo 0.1.0 store/Store#").unwrap();
        assert_eq!(ix.info[st as usize].kind, 49);
        assert_eq!(ix.info[st as usize].display_name, "Store");
        assert_eq!(ix.rel_from, [st]);
        assert_eq!(ix.rel_flags, [REL_IMPLEMENTATION]);
        assert_eq!(ix.symbols[ix.rel_to[0] as usize], "rust-analyzer cargo demo 0.1.0 store/Open#");
        let keep = decode(&buf, Options { keep_locals: true }).unwrap();
        assert_eq!(keep.occurrences(), 7);
    }

    #[test]
    fn bad_ranges_and_truncation_are_errors() {
        let mut o = Writer::default();
        o.packed(1, &[1, 2]).string(2, "a b c d e#");
        let mut d = Writer::default();
        d.string(1, "x.rs").message(2, &o);
        let mut ix = Writer::default();
        ix.message(2, &d);
        assert!(decode(&ix.buf, Options::default()).is_err());
        let buf = sample();
        for cut in [1, buf.len() / 3, buf.len() - 1] {
            assert!(decode(&buf[..cut], Options::default()).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn descriptors_of_rust_analyzer_symbols() {
        let d = descriptors("rust-analyzer cargo demo 0.1.0 store/Store#open().");
        assert_eq!(
            d,
            [
                Descriptor { name: "store", suffix: '/' },
                Descriptor { name: "Store", suffix: '#' },
                Descriptor { name: "open", suffix: '(' }
            ]
        );
        let d = descriptors("rust-analyzer cargo demo 0.1.0 impl#[Store][Open]open().");
        assert_eq!(d.iter().map(|x| (x.name, x.suffix)).collect::<Vec<_>>(), [("impl", '#'), ("Store", '['), ("Open", '['), ("open", '(')]);
        let d = descriptors("rust-analyzer cargo demo 0.1.0 m/`r#type`.");
        assert_eq!(d[1].name, "r#type");
        // An escaped type holding the close bracket.
        let d = descriptors("rust-analyzer cargo demo 0.1.0 impl#[`[u8]`][`Ext<T>`]f().");
        assert_eq!(d.iter().map(|x| (x.name, x.suffix)).collect::<Vec<_>>(), [("impl", '#'), ("[u8]", '['), ("Ext<T>", '['), ("f", '(')]);
        let d = descriptors("rust-analyzer cargo demo 0.1.0 vec!");
        assert_eq!(d[0], Descriptor { name: "vec", suffix: '!' });
        assert!(descriptors("local 4").is_empty());
        assert_eq!(descriptors("a  b c d e. f#")[0].name, "f");
    }
}
