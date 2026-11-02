//! An owned WGSL scanner for the code index: declarations (functions,
//! structs, module-scope `var`, `const`, `override` and `alias`, with their
//! `@group`/`@binding` and entry-point attributes), identifier references,
//! and the naga_oil preprocessor lines `#define_import_path` and `#import`.
//!
//! It is a lexer plus a declaration matcher, not a parser: line comments,
//! nested block comments and preprocessor lines are skipped; both arms of an
//! `#ifdef` are scanned (so a symbol defined in either is found); braces are
//! counted so `var`, `const` and `let` inside a function body are not taken
//! for module declarations. An identifier token that is not a declaration's
//! own name is a reference candidate; the code index keeps the ones that
//! name a declared symbol.
//!
//! `#import a::b::{c, d}`, `#import a::b::c` and `#import a::b as e` are
//! read (braces may span lines); each imported item is a reference with its
//! module path.

/// One declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decl {
    /// The declared name.
    pub name: String,
    /// `fn`, `struct`, `var`, `const`, `override` or `alias`.
    pub kind: &'static str,
    /// Extra facts: `vertex`, `fragment` or `compute` for an entry point,
    /// `group G binding B` for a resource, the address space of a `var`.
    pub detail: String,
    /// Line, 1-based.
    pub line: u32,
    /// Column of the name, in bytes, 0-based.
    pub col: u32,
}

/// One reference candidate: an identifier that is not a declaration name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// The identifier.
    pub name: String,
    /// Line, 1-based.
    pub line: u32,
    /// Column, in bytes, 0-based.
    pub col: u32,
    /// The innermost function the reference sits in, if any.
    pub within: Option<String>,
    /// True when it comes from an `#import` line.
    pub import: bool,
}

/// What a file holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// The file's `#define_import_path`, if any.
    pub module: Option<String>,
    /// Declarations in file order.
    pub decls: Vec<Decl>,
    /// Reference candidates in file order.
    pub refs: Vec<Ref>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum T {
    Ident,
    Punct(u8),
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Tok {
    t: T,
    start: usize,
    end: usize,
    line: u32,
    col: u32,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// WGSL keywords and builtin type and function names: never references to
/// a user declaration, so they are not reported.
const RESERVED: &[&str] = &[
    "alias", "bitcast", "bool", "break", "case", "const", "const_assert", "continue", "continuing", "default", "diagnostic", "discard",
    "else", "enable", "f16", "f32", "false", "fn", "for", "i32", "if", "let", "loop", "override", "requires", "return", "struct",
    "switch", "true", "u32", "var", "while", "array", "atomic", "mat2x2", "mat2x3", "mat2x4", "mat3x2", "mat3x3", "mat3x4", "mat4x2",
    "mat4x3", "mat4x4", "ptr", "sampler", "sampler_comparison", "texture_1d", "texture_2d", "texture_2d_array", "texture_3d",
    "texture_cube", "texture_cube_array", "texture_multisampled_2d", "texture_storage_1d", "texture_storage_2d",
    "texture_storage_2d_array", "texture_storage_3d", "texture_depth_2d", "texture_depth_2d_array", "texture_depth_cube",
    "texture_depth_cube_array", "texture_depth_multisampled_2d", "vec2", "vec3", "vec4", "function", "private", "workgroup",
    "uniform", "storage", "read", "write", "read_write",
];

/// Scans one WGSL file.
///
/// # Arguments
///
/// * `text` - the file's text.
///
/// # Returns
///
/// Its import path, declarations and reference candidates.
pub fn scan(text: &str) -> Scan {
    let mut out = Scan::default();
    let toks = lex(text, &mut out);
    let src = text.as_bytes();
    let word = |t: &Tok| &text[t.start..t.end];
    let mut depth = 0usize;
    // Function bodies open at a depth: (name, depth inside the body).
    let mut fns: Vec<(String, usize)> = Vec::new();
    let mut pending_fn: Option<String> = None;
    // Attributes seen since the last declaration or `;`.
    let mut stage = "";
    let mut group: Option<String> = None;
    let mut binding: Option<String> = None;
    let mut decl_at: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        match t.t {
            T::Punct(b'{') => {
                depth += 1;
                if let Some(f) = pending_fn.take() {
                    fns.push((f, depth));
                }
            }
            T::Punct(b'}') => {
                if fns.last().is_some_and(|f| f.1 == depth) {
                    fns.pop();
                }
                depth = depth.saturating_sub(1);
            }
            T::Punct(b';') => {
                stage = "";
                group = None;
                binding = None;
            }
            T::Punct(b'@') => {
                if let Some(a) = toks.get(i + 1).filter(|a| a.t == T::Ident) {
                    let name = word(a);
                    let arg = if toks.get(i + 2).map(|p| p.t) == Some(T::Punct(b'(')) {
                        toks.get(i + 3).map(|v| text[v.start..v.end].to_string())
                    } else {
                        None
                    };
                    match name {
                        "vertex" | "fragment" | "compute" => stage = if name == "vertex" { "vertex" } else if name == "fragment" { "fragment" } else { "compute" },
                        "group" => group = arg,
                        "binding" => binding = arg,
                        _ => {}
                    }
                    i += 2;
                    continue;
                }
            }
            T::Ident => {
                let w = word(&t);
                let next_ident = |from: usize| -> Option<(usize, Tok)> {
                    // Skips a `<...>` template list (`var<storage, read>`).
                    let mut j = from;
                    if toks.get(j).map(|x| x.t) == Some(T::Punct(b'<')) {
                        let mut d = 0;
                        while let Some(x) = toks.get(j) {
                            match x.t {
                                T::Punct(b'<') => d += 1,
                                T::Punct(b'>') => {
                                    d -= 1;
                                    if d == 0 {
                                        j += 1;
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            j += 1;
                        }
                    }
                    toks.get(j).filter(|x| x.t == T::Ident).map(|x| (j, *x))
                };
                let kind = match w {
                    "fn" => Some("fn"),
                    "struct" => Some("struct"),
                    "var" | "const" | "override" | "alias" if depth == 0 => Some(match w {
                        "var" => "var",
                        "const" => "const",
                        "override" => "override",
                        _ => "alias",
                    }),
                    _ => None,
                };
                if let Some(kind) = kind {
                    if let Some((j, n)) = next_ident(i + 1) {
                        let mut detail = String::new();
                        if kind == "fn" && !stage.is_empty() {
                            detail.push_str(stage);
                        }
                        if kind == "var" {
                            if let Some(tl) = toks.get(i + 1).filter(|x| x.t == T::Punct(b'<')) {
                                let close = toks[i + 1..j].iter().rposition(|x| x.t == T::Punct(b'>')).map(|p| toks[i + 1 + p].end);
                                if let Some(c) = close {
                                    detail.push_str(&String::from_utf8_lossy(&src[tl.start..c]).replace(' ', ""));
                                }
                            }
                            if let (Some(g), Some(b)) = (&group, &binding) {
                                if !detail.is_empty() {
                                    detail.push(' ');
                                }
                                detail.push_str(&format!("group {g} binding {b}"));
                            }
                        }
                        let name = word(&n).to_string();
                        if kind == "fn" {
                            pending_fn = Some(name.clone());
                        }
                        out.decls.push(Decl { name, kind, detail, line: n.line, col: n.col });
                        decl_at.push(j);
                        stage = "";
                        group = None;
                        binding = None;
                        i = j + 1;
                        continue;
                    }
                }
                if !RESERVED.contains(&w) {
                    // A struct member or parameter name followed by `:` is a
                    // declaration of its own (not indexed), not a reference.
                    let is_member = toks.get(i + 1).map(|x| x.t) == Some(T::Punct(b':'))
                        && toks.get(i + 2).map(|x| x.t) != Some(T::Punct(b':'))
                        && i > 0
                        && matches!(toks[i - 1].t, T::Punct(b'{') | T::Punct(b',') | T::Punct(b'(') | T::Punct(b')'));
                    // `let x` and `var x` inside a body declare locals.
                    let is_local = i > 0 && toks[i - 1].t == T::Ident && matches!(word(&toks[i - 1]), "let" | "var");
                    if !is_member && !is_local {
                        out.refs.push(Ref { name: w.to_string(), line: t.line, col: t.col, within: fns.last().map(|f| f.0.clone()), import: false });
                    }
                }
            }
            T::Other | T::Punct(_) => {}
        }
        i += 1;
    }
    out
}

/// Tokenises, skipping comments and handling preprocessor lines (which add
/// to `out.module` and `out.refs`).
fn lex(text: &str, out: &mut Scan) -> Vec<Tok> {
    let b = text.as_bytes();
    let mut toks = Vec::new();
    let (mut i, mut line, mut line_start) = (0usize, 1u32, 0usize);
    let mut at_line_start = true;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            i += 1;
            line += 1;
            line_start = i;
            at_line_start = true;
            continue;
        }
        if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut d = 0;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    d += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    d -= 1;
                    i += 2;
                    if d == 0 {
                        break;
                    }
                } else {
                    if b[i] == b'\n' {
                        line += 1;
                        line_start = i + 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        if c == b'#' && at_line_start {
            let (end, lines) = preprocessor(text, i, line, line_start, out);
            line += lines;
            if lines > 0 {
                line_start = text[..end].rfind('\n').map(|p| p + 1).unwrap_or(0);
            }
            i = end;
            continue;
        }
        at_line_start = false;
        let start = i;
        let col = (i - line_start) as u32;
        if is_ident_start(c) {
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            toks.push(Tok { t: T::Ident, start, end: i, line, col });
        } else if c.is_ascii_digit() {
            while i < b.len() && (is_ident(b[i]) || b[i] == b'.') {
                i += 1;
            }
            toks.push(Tok { t: T::Other, start, end: i, line, col });
        } else if c.is_ascii() {
            i += 1;
            toks.push(Tok { t: T::Punct(c), start, end: i, line, col });
        } else {
            // Skip a whole non-ASCII character.
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    toks
}

/// Reads one preprocessor directive starting at `#`. Returns where it ends
/// (the newline after it, not consumed) and how many newlines it spanned.
fn preprocessor(text: &str, at: usize, line: u32, line_start: usize, out: &mut Scan) -> (usize, u32) {
    let b = text.as_bytes();
    let eol = |from: usize| text[from..].find('\n').map(|p| from + p).unwrap_or(b.len());
    let first_end = eol(at);
    let head = &text[at..first_end];
    let directive: String = head[1..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    match directive.as_str() {
        "define_import_path" => {
            let p = head[1 + directive.len()..].trim().trim_end_matches(';').trim();
            if !p.is_empty() {
                out.module = Some(p.to_string());
            }
            (first_end, 0)
        }
        "import" => {
            // The import may span lines while a `{` is open.
            let mut end = first_end;
            let mut depth: i32 = head.matches('{').count() as i32 - head.matches('}').count() as i32;
            let mut lines = 0;
            while depth > 0 && end < b.len() {
                let next = eol(end + 1);
                let l = &text[end + 1..next];
                depth += l.matches('{').count() as i32 - l.matches('}').count() as i32;
                end = next;
                lines += 1;
            }
            let body_start = at + 1 + directive.len();
            import_refs(text, body_start, end, line, line_start, out);
            (end, lines)
        }
        _ => (first_end, 0),
    }
}

/// Emits each imported item of `#import` text `[body_start, end)` as a
/// reference: the last segment of every path, with its position.
fn import_refs(text: &str, body_start: usize, end: usize, mut line: u32, mut line_start: usize, out: &mut Scan) {
    let b = text.as_bytes();
    let mut i = body_start;
    // Path segments of the current item, and the prefix stack for braces.
    let mut cur: Vec<(usize, usize, u32, u32)> = Vec::new();
    let mut alias = false;
    let flush = |cur: &mut Vec<(usize, usize, u32, u32)>, out: &mut Scan| {
        if let Some(&(s, e, l, c)) = cur.last() {
            out.refs.push(Ref { name: text[s..e].to_string(), line: l, col: c, within: None, import: true });
        }
        cur.clear();
    };
    while i < end {
        let c = b[i];
        if c == b'\n' {
            line += 1;
            line_start = i + 1;
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < end && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if is_ident_start(c) || c == b'"' {
            let s = i;
            if c == b'"' {
                i += 1;
                while i < end && b[i] != b'"' {
                    i += 1;
                }
                i += 1;
                cur.push((s + 1, i - 1, line, (s + 1 - line_start) as u32));
                continue;
            }
            while i < end && is_ident(b[i]) {
                i += 1;
            }
            if &text[s..i] == "as" {
                alias = true;
                continue;
            }
            if alias {
                alias = false;
                continue;
            }
            cur.push((s, i, line, (s - line_start) as u32));
            continue;
        }
        match c {
            b',' | b'}' | b';' => flush(&mut cur, out),
            b'{' => cur.clear(),
            _ => {}
        }
        i += 1;
    }
    flush(&mut cur, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHADER: &str = "#define_import_path demo::lighting

#import demo::common::{Light, shade}
#import demo::noise::hash as h
#import demo::{
    util::clamp01,
    // a comment inside
    util::Wide,
}

struct Material { albedo: vec3<f32>, rough: f32 };

@group(0) @binding(1) var<storage, read> lights: array<Light>;
@group(1) @binding(0) var<uniform> material: Material;
const PI: f32 = 3.14159;
override samples: u32 = 4u;
alias Color = vec4<f32>;

/* a block /* nested */ comment with fn hidden() */
fn helper(x: f32) -> f32 {
    let y = clamp01(x);
    var z = y * PI;
    return z;
}

#ifdef FANCY
fn fancy() -> f32 { return helper(1.0); }
#else
fn fancy() -> f32 { return 0.0; }
#endif

@fragment
fn main(@location(0) uv: vec2<f32>) -> @location(0) Color {
    let m = material;
    return Color(shade(lights[0], m.albedo) * helper(uv.x), 1.0);
}
";

    #[test]
    fn declarations_with_attributes() {
        let s = scan(SHADER);
        assert_eq!(s.module.as_deref(), Some("demo::lighting"));
        let d: Vec<(&str, &str, &str, u32)> = s.decls.iter().map(|d| (d.name.as_str(), d.kind, d.detail.as_str(), d.line)).collect();
        assert_eq!(
            d,
            [
                ("Material", "struct", "", 11),
                ("lights", "var", "<storage,read> group 0 binding 1", 13),
                ("material", "var", "<uniform> group 1 binding 0", 14),
                ("PI", "const", "", 15),
                ("samples", "override", "", 16),
                ("Color", "alias", "", 17),
                ("helper", "fn", "", 20),
                ("fancy", "fn", "", 27),
                ("fancy", "fn", "", 29),
                ("main", "fn", "fragment", 33),
            ]
        );
        assert_eq!(s.decls[6].col, 3);
    }

    #[test]
    fn references_imports_and_scopes() {
        let s = scan(SHADER);
        let imports: Vec<(&str, u32)> = s.refs.iter().filter(|r| r.import).map(|r| (r.name.as_str(), r.line)).collect();
        assert_eq!(imports, [("Light", 3), ("shade", 3), ("hash", 4), ("clamp01", 6), ("Wide", 8)]);
        let names = |n: &str| s.refs.iter().filter(|r| r.name == n && !r.import).map(|r| (r.line, r.within.clone())).collect::<Vec<_>>();
        assert_eq!(names("helper"), [(27, Some("fancy".to_string())), (35, Some("main".to_string()))]);
        assert_eq!(names("PI"), [(22, Some("helper".to_string()))]);
        assert_eq!(names("Color"), [(33, None), (35, Some("main".to_string()))]);
        assert_eq!(names("Light"), [(13, None)]);
        // Locals, members, keywords and commented text are not references.
        for gone in ["hidden", "f32", "let", "rough"] {
            assert!(names(gone).is_empty(), "{gone}");
        }
        assert!(names("z").iter().all(|r| r.0 == 23), "z declared by var is a local");
        assert_eq!(names("albedo").len(), 1, "member access is a reference; the member declaration is not");
    }
}
