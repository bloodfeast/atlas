//! An owned JSON value, parser and writer: what the protocol, the tools,
//! the configuration and the JSONL logs use, and nothing more.
//!
//! * [`Value`] mirrors the parts of `serde_json::Value` the crate used, so
//!   handlers read the same: `v["key"]`, `v.get("key")`, `as_str`,
//!   `as_u64`, `pointer`.
//! * [`Map`] keeps keys in insertion order (so a struct writes its fields in
//!   declaration order, which the index markers rely on); equality ignores
//!   order.
//! * [`parse`] is strict RFC 8259: no comments, no trailing commas, no
//!   leading byte order mark, no lone surrogates, at most [`MAX_DEPTH`]
//!   nested arrays and objects, and numbers that fit `i64`, `u64` or a
//!   finite `f64`.
//! * The writer escapes `"`, `\` and control characters and writes every
//!   other character as is; a non-finite float is written `null`.
//! * [`ToJson`] replaces `serde::Serialize` (implemented by hand, or with
//!   [`impl_to_json!`](crate::impl_to_json)), and [`json!`](crate::json!)
//!   builds values with the same syntax as `serde_json::json!`.

use std::collections::{BTreeMap, HashMap};
use std::fmt::{self, Write as _};
use std::ops::{Index, IndexMut};

/// The deepest nesting [`parse`] accepts.
pub const MAX_DEPTH: usize = 128;

/// The object size past which the parser indexes keys instead of scanning.
const INDEX_KEYS: usize = 32;

/// A JSON number: an integer when it fits one, else a float.
#[derive(Debug, Clone, Copy)]
pub enum Number {
    /// A negative integer.
    I(i64),
    /// A non-negative integer.
    U(u64),
    /// A float (always finite once parsed).
    F(f64),
}

impl Number {
    /// The number as an `i64`, if it is an integer that fits.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Number::I(i) => Some(i),
            Number::U(u) => i64::try_from(u).ok(),
            Number::F(_) => None,
        }
    }
    /// The number as a `u64`, if it is a non-negative integer.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Number::U(u) => Some(u),
            Number::I(i) => u64::try_from(i).ok(),
            Number::F(_) => None,
        }
    }
    /// The number as an `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        Some(match *self {
            Number::I(i) => i as f64,
            Number::U(u) => u as f64,
            Number::F(f) => f,
        })
    }
    /// True for an integer.
    pub fn is_integer(&self) -> bool {
        !matches!(self, Number::F(_))
    }
}

impl PartialEq for Number {
    fn eq(&self, o: &Number) -> bool {
        match (self.as_i64(), o.as_i64(), self.as_u64(), o.as_u64()) {
            (Some(a), Some(b), _, _) => a == b,
            (_, _, Some(a), Some(b)) => a == b,
            _ if self.is_integer() != o.is_integer() => false,
            _ => self.as_f64() == o.as_f64(),
        }
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Number::I(i) => write!(f, "{i}"),
            Number::U(u) => write!(f, "{u}"),
            Number::F(x) if !x.is_finite() => f.write_str("null"),
            Number::F(x) if x.fract() == 0.0 && x.abs() < 1e16 => write!(f, "{x:.1}"),
            // A whole float this large would print as an integer and read
            // back as one: the exponent keeps it a float.
            Number::F(x) if x.fract() == 0.0 => write!(f, "{x:e}"),
            Number::F(x) => write!(f, "{x}"),
        }
    }
}

/// A JSON object: keys in insertion order, unique.
#[derive(Debug, Clone, Default)]
pub struct Map {
    entries: Vec<(String, Value)>,
}

impl Map {
    /// An empty object.
    pub fn new() -> Map {
        Map { entries: Vec::new() }
    }
    /// The value under `k`.
    pub fn get(&self, k: &str) -> Option<&Value> {
        self.entries.iter().find(|(key, _)| key == k).map(|(_, v)| v)
    }
    /// The value under `k`, mutably.
    pub fn get_mut(&mut self, k: &str) -> Option<&mut Value> {
        self.entries.iter_mut().find(|(key, _)| key == k).map(|(_, v)| v)
    }
    /// True when `k` is present.
    pub fn contains_key(&self, k: &str) -> bool {
        self.get(k).is_some()
    }
    /// Sets `k`, keeping its place if it was present; returns the old value.
    pub fn insert(&mut self, k: String, v: Value) -> Option<Value> {
        match self.get_mut(&k) {
            Some(slot) => Some(std::mem::replace(slot, v)),
            None => {
                self.entries.push((k, v));
                None
            }
        }
    }
    /// Removes `k`, returning its value.
    pub fn remove(&mut self, k: &str) -> Option<Value> {
        let i = self.entries.iter().position(|(key, _)| key == k)?;
        Some(self.entries.remove(i).1)
    }
    /// The keys, in order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.iter().map(|(k, _)| k)
    }
    /// The values, in order.
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|(_, v)| v)
    }
    /// The entries, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }
    /// How many keys.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// The value under `k`, inserted as `null` if missing.
    pub fn entry_or_null(&mut self, k: &str) -> &mut Value {
        let i = match self.entries.iter().position(|(key, _)| key == k) {
            Some(i) => i,
            None => {
                self.entries.push((k.to_string(), Value::Null));
                self.entries.len() - 1
            }
        };
        &mut self.entries[i].1
    }
}

impl PartialEq for Map {
    fn eq(&self, o: &Map) -> bool {
        self.len() == o.len() && self.iter().all(|(k, v)| o.get(k) == Some(v))
    }
}

impl<'a> IntoIterator for &'a Map {
    type Item = (&'a String, &'a Value);
    type IntoIter = Box<dyn Iterator<Item = (&'a String, &'a Value)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl FromIterator<(String, Value)> for Map {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(it: I) -> Map {
        let mut m = Map::new();
        for (k, v) in it {
            m.insert(k, v);
        }
        m
    }
}

/// A JSON value.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    /// `null`.
    #[default]
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number.
    Number(Number),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Value>),
    /// An object.
    Object(Map),
}

static NULL: Value = Value::Null;

/// What [`Value::get`] and indexing accept: a key or a position.
pub trait JsonIndex {
    /// The element of `v` this names.
    fn pick<'a>(&self, v: &'a Value) -> Option<&'a Value>;
}

impl JsonIndex for str {
    fn pick<'a>(&self, v: &'a Value) -> Option<&'a Value> {
        v.as_object()?.get(self)
    }
}
impl JsonIndex for String {
    fn pick<'a>(&self, v: &'a Value) -> Option<&'a Value> {
        v.as_object()?.get(self)
    }
}
impl JsonIndex for usize {
    fn pick<'a>(&self, v: &'a Value) -> Option<&'a Value> {
        v.as_array()?.get(*self)
    }
}
impl<T: JsonIndex + ?Sized> JsonIndex for &T {
    fn pick<'a>(&self, v: &'a Value) -> Option<&'a Value> {
        (**self).pick(v)
    }
}

impl Value {
    /// The element named by a key (objects) or a position (arrays).
    pub fn get<I: JsonIndex>(&self, i: I) -> Option<&Value> {
        i.pick(self)
    }
    /// The string, if this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    /// The bool, if this is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// The number as `i64`, if it is an integer that fits.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) => n.as_i64(),
            _ => None,
        }
    }
    /// The number as `u64`, if it is a non-negative integer.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }
    /// The number as `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => n.as_f64(),
            _ => None,
        }
    }
    /// The array, if this is one.
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
    /// The array, mutably.
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
    /// The object, if this is one.
    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }
    /// The object, mutably.
    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }
    /// True for `null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    /// True for a string.
    pub fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }
    /// True for an object.
    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }
    /// True for an array.
    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }
    /// True for a number.
    pub fn is_number(&self) -> bool {
        matches!(self, Value::Number(_))
    }
    /// True for a bool.
    pub fn is_boolean(&self) -> bool {
        matches!(self, Value::Bool(_))
    }
    /// The element at an RFC 6901 pointer such as `/a/0/b`.
    pub fn pointer(&self, p: &str) -> Option<&Value> {
        if p.is_empty() {
            return Some(self);
        }
        let mut cur = self;
        for raw in p.strip_prefix('/')?.split('/') {
            let k = raw.replace("~1", "/").replace("~0", "~");
            cur = match cur {
                Value::Object(m) => m.get(&k)?,
                Value::Array(a) => a.get(k.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(cur)
    }
    /// Takes the value, leaving `null`.
    pub fn take(&mut self) -> Value {
        std::mem::take(self)
    }
}

impl<I: JsonIndex> Index<I> for Value {
    type Output = Value;
    fn index(&self, i: I) -> &Value {
        i.pick(self).unwrap_or(&NULL)
    }
}

impl Index<&str> for Map {
    type Output = Value;
    /// The value under a key, or `null` when it is missing.
    fn index(&self, k: &str) -> &Value {
        self.get(k).unwrap_or(&NULL)
    }
}

impl IndexMut<&str> for Value {
    /// Indexing a `null` by key makes it an object; a missing key is
    /// inserted as `null`. Indexing anything else by key panics.
    fn index_mut(&mut self, k: &str) -> &mut Value {
        if self.is_null() {
            *self = Value::Object(Map::new());
        }
        match self {
            Value::Object(m) => m.entry_or_null(k),
            _ => panic!("cannot index a non-object JSON value with {k:?}"),
        }
    }
}

impl IndexMut<usize> for Value {
    fn index_mut(&mut self, i: usize) -> &mut Value {
        match self {
            Value::Array(a) => &mut a[i],
            _ => panic!("cannot index a non-array JSON value with {i}"),
        }
    }
}

macro_rules! eq_prim {
    ($($t:ty => $m:ident),*) => {$(
        impl PartialEq<$t> for Value {
            fn eq(&self, o: &$t) -> bool {
                self.$m().is_some_and(|x| x == *o)
            }
        }
    )*};
}
eq_prim!(bool => as_bool, i64 => as_i64, u64 => as_u64, f64 => as_f64);
impl PartialEq<i32> for Value {
    fn eq(&self, o: &i32) -> bool {
        self.as_i64() == Some(i64::from(*o))
    }
}
impl PartialEq<usize> for Value {
    fn eq(&self, o: &usize) -> bool {
        self.as_u64() == Some(*o as u64)
    }
}
impl PartialEq<str> for Value {
    fn eq(&self, o: &str) -> bool {
        self.as_str() == Some(o)
    }
}
impl PartialEq<&str> for Value {
    fn eq(&self, o: &&str) -> bool {
        self.as_str() == Some(*o)
    }
}
impl PartialEq<String> for Value {
    fn eq(&self, o: &String) -> bool {
        self.as_str() == Some(o.as_str())
    }
}

// ---------------------------------------------------------------- writing

/// For each byte, the escape it needs inside a JSON string: 0 for none,
/// `u` for a `\u00XX` escape, else the letter after the backslash.
static ESCAPE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 0x20 {
        t[i] = b'u';
        i += 1;
    }
    t[0x08] = b'b';
    t[0x09] = b't';
    t[0x0a] = b'n';
    t[0x0c] = b'f';
    t[0x0d] = b'r';
    t[b'"' as usize] = b'"';
    t[b'\\' as usize] = b'\\';
    t
};

/// Appends `s` as a JSON string literal.
pub fn write_str(out: &mut String, s: &str) {
    out.reserve(s.len() + 2);
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let e = ESCAPE[b as usize];
        if e == 0 {
            continue;
        }
        out.push_str(&s[start..i]);
        out.push('\\');
        if e == b'u' {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            out.push_str("u00");
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        } else {
            out.push(e as char);
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

/// Appends an unsigned integer in decimal without the formatting machinery.
fn write_u64(out: &mut String, mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for &d in &buf[i..] {
        out.push(d as char);
    }
}

fn write_value(out: &mut String, v: &Value, indent: Option<usize>) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(Number::U(u)) => write_u64(out, *u),
        Value::Number(Number::I(i)) => {
            if *i < 0 {
                out.push('-');
            }
            write_u64(out, i.unsigned_abs());
        }
        Value::Number(n) => {
            let _ = write!(out, "{n}");
        }
        Value::String(s) => write_str(out, s),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent.map(|d| d + 1));
                write_value(out, x, indent.map(|d| d + 1));
            }
            newline(out, indent);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent.map(|d| d + 1));
                write_str(out, k);
                out.push_str(if indent.is_some() { ": " } else { ":" });
                write_value(out, x, indent.map(|d| d + 1));
            }
            newline(out, indent);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, indent: Option<usize>) {
    if let Some(d) = indent {
        out.push('\n');
        for _ in 0..d {
            out.push_str("  ");
        }
    }
}

impl fmt::Display for Value {
    /// Compact JSON.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        write_value(&mut s, self, None);
        f.write_str(&s)
    }
}

impl Value {
    /// The value as compact JSON. (Shadows `ToString`, to write straight
    /// into one buffer instead of through a formatter.)
    #[allow(clippy::inherent_to_string_shadow_display)]
    pub fn to_string(&self) -> String {
        let mut s = String::with_capacity(64);
        write_value(&mut s, self, None);
        s
    }

    /// The value as compact JSON, appended to `out`.
    pub fn write_to(&self, out: &mut String) {
        write_value(out, self, None);
    }
}

/// Writes one JSON object member by member straight into a buffer, for an
/// answer that would otherwise build a [`Value`] only to encode it. The text
/// is what the same members inserted into a [`Map`] in the same order would
/// encode to.
///
/// ```
/// let mut out = String::new();
/// let mut w = atlas::json::ObjectWriter::new(&mut out);
/// w.field("a", &1).field("b", "x");
/// w.end();
/// assert_eq!(out, r#"{"a":1,"b":"x"}"#);
/// ```
#[derive(Debug)]
pub struct ObjectWriter<'a> {
    out: &'a mut String,
    first: bool,
}

impl<'a> ObjectWriter<'a> {
    /// Opens an object at the end of `out`.
    pub fn new(out: &'a mut String) -> ObjectWriter<'a> {
        out.push('{');
        ObjectWriter { out, first: true }
    }

    /// Appends the member `key: value`.
    pub fn field<T: ToJson + ?Sized>(&mut self, key: &str, value: &T) -> &mut ObjectWriter<'a> {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        write_str(self.out, key);
        self.out.push(':');
        value.write_json(self.out);
        self
    }

    /// Closes the object.
    pub fn end(self) {
        self.out.push('}');
    }
}

/// Compact JSON text built by `f` with an [`ObjectWriter`], in a buffer of
/// `capacity` bytes to start.
pub fn object(capacity: usize, f: impl FnOnce(&mut ObjectWriter<'_>)) -> String {
    let mut out = String::with_capacity(capacity);
    let mut w = ObjectWriter::new(&mut out);
    f(&mut w);
    w.end();
    out
}

/// Compact JSON for anything [`ToJson`].
pub fn to_string<T: ToJson + ?Sized>(v: &T) -> String {
    let mut s = String::with_capacity(256);
    v.write_json(&mut s);
    s
}

/// Compact JSON, as a `Result` that cannot fail: the shape of
/// `serde_json::to_string`, for call sites that handle an error.
///
/// # Errors
///
/// Never.
pub fn to_string_ok<T: ToJson + ?Sized>(v: &T) -> Result<String, std::convert::Infallible> {
    Ok(to_string(v))
}

/// Indented JSON (two spaces) for anything [`ToJson`].
pub fn to_string_pretty<T: ToJson + ?Sized>(v: &T) -> String {
    let mut s = String::new();
    write_value(&mut s, &v.to_json(), Some(0));
    s
}

// ---------------------------------------------------------------- parsing

/// Why a text is not JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// What was wrong.
    pub msg: String,
    /// Byte offset where it was found.
    pub at: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.msg, self.at)
    }
}

impl std::error::Error for ParseError {}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, msg: &str) -> Result<T, ParseError> {
        Err(ParseError { msg: msg.to_string(), at: self.i })
    }
    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                break;
            }
            self.i += 1;
        }
    }
    fn lit(&mut self, word: &[u8], v: Value) -> Result<Value, ParseError> {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            self.err("invalid literal")
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.ws();
        match self.b.get(self.i) {
            None => self.err("unexpected end of input"),
            Some(b'n') => self.lit(b"null", Value::Null),
            Some(b't') => self.lit(b"true", Value::Bool(true)),
            Some(b'f') => self.lit(b"false", Value::Bool(false)),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b'[') => {
                if depth >= MAX_DEPTH {
                    return self.err("nesting too deep");
                }
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Value::Array(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::Array(a));
                        }
                        _ => return self.err("expected , or ]"),
                    }
                }
            }
            Some(b'{') => {
                if depth >= MAX_DEPTH {
                    return self.err("nesting too deep");
                }
                self.i += 1;
                let mut m = Map::new();
                // A key index once the object is large, so a hostile object
                // with many keys parses in linear time (`Map::insert` scans).
                let mut seen: HashMap<String, usize> = HashMap::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Value::Object(m));
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return self.err("expected a string key");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return self.err("expected :");
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    if seen.is_empty() && m.entries.len() < INDEX_KEYS {
                        m.insert(k, v);
                        if m.entries.len() == INDEX_KEYS {
                            seen = m.entries.iter().enumerate().map(|(i, (key, _))| (key.clone(), i)).collect();
                        }
                    } else if let Some(&i) = seen.get(&k) {
                        m.entries[i].1 = v;
                    } else {
                        seen.insert(k.clone(), m.entries.len());
                        m.entries.push((k, v));
                    }
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::Object(m));
                        }
                        _ => return self.err("expected , or }"),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(0xEF) if self.i == 0 => self.err("byte order mark before JSON"),
            Some(_) => self.err("unexpected character"),
        }
    }
    fn hex4(&mut self) -> Result<u32, ParseError> {
        let Some(h) = self.b.get(self.i..self.i + 4) else { return self.err("short \\u escape") };
        let s = std::str::from_utf8(h).map_err(|_| ParseError { msg: "bad \\u escape".into(), at: self.i })?;
        let v = u32::from_str_radix(s, 16).map_err(|_| ParseError { msg: "bad \\u escape".into(), at: self.i })?;
        if !h.iter().all(u8::is_ascii_hexdigit) {
            return self.err("bad \\u escape");
        }
        self.i += 4;
        Ok(v)
    }
    fn string(&mut self) -> Result<String, ParseError> {
        self.i += 1; // opening quote
        // The common case: no escape before the closing quote.
        let run = crate::scan::json_stop(&self.b[self.i..]);
        if self.b.get(self.i + run) == Some(&b'"') {
            let s = std::str::from_utf8(&self.b[self.i..self.i + run])
                .map_err(|_| ParseError { msg: "invalid UTF-8 in string".into(), at: self.i })?
                .to_string();
            self.i += run + 1;
            return Ok(s);
        }
        let mut out = Vec::new();
        loop {
            // Copy the run up to the next quote, backslash or control byte.
            let run = crate::scan::json_stop(&self.b[self.i..]);
            out.extend_from_slice(&self.b[self.i..self.i + run]);
            self.i += run;
            match self.b.get(self.i) {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.i += 1;
                    return String::from_utf8(out).map_err(|_| ParseError { msg: "invalid UTF-8 in string".into(), at: self.i });
                }
                Some(b'\\') => {
                    self.i += 1;
                    let Some(&e) = self.b.get(self.i) else { return self.err("unterminated escape") };
                    self.i += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                if self.b.get(self.i..self.i + 2) != Some(b"\\u") {
                                    return self.err("lone leading surrogate");
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return self.err("invalid trailing surrogate");
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return self.err("lone trailing surrogate");
                            } else {
                                hi
                            };
                            char::from_u32(cp).ok_or_else(|| ParseError { msg: "invalid code point".into(), at: self.i })?
                        }
                        _ => return self.err("invalid escape"),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                Some(_) => return self.err("control character in string"),
            }
        }
    }
    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let digits = |p: &mut Parser<'_>| {
            let s = p.i;
            while p.b.get(p.i).is_some_and(u8::is_ascii_digit) {
                p.i += 1;
            }
            p.i - s
        };
        let int_start = self.i;
        let n = digits(self);
        if n == 0 || (n > 1 && self.b[int_start] == b'0') {
            return self.err("invalid number");
        }
        let mut float = false;
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if digits(self) == 0 {
                return self.err("invalid number");
            }
            float = true;
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return self.err("invalid number");
            }
            float = true;
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("");
        if !float {
            if let Ok(u) = text.parse::<u64>() {
                return Ok(Value::Number(Number::U(u)));
            }
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Number(Number::I(i)));
            }
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::Number(Number::F(f))),
            _ => Err(ParseError { msg: "number out of range".into(), at: start }),
        }
    }
}

/// Parses one JSON text.
///
/// # Errors
///
/// [`ParseError`] for anything not strict JSON (see the module docs), or
/// trailing characters after the value.
pub fn parse(text: &str) -> Result<Value, ParseError> {
    parse_bytes(text.as_bytes())
}

/// Parses one JSON text from bytes, which must be UTF-8 inside strings.
///
/// # Errors
///
/// As [`parse`].
pub fn parse_bytes(b: &[u8]) -> Result<Value, ParseError> {
    let mut p = Parser { b, i: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.i != b.len() {
        return p.err("trailing characters");
    }
    Ok(v)
}

/// Parses JSON text into a type that reads itself from a [`Value`].
///
/// # Errors
///
/// A message for bad JSON or a value of the wrong shape.
pub fn from_str<T: FromJson>(text: &str) -> Result<T, String> {
    T::from_json(&parse(text).map_err(|e| e.to_string())?)
}

/// Reads a type from a [`Value`].
///
/// # Errors
///
/// A message for a value of the wrong shape.
pub fn from_value<T: FromJson>(v: Value) -> Result<T, String> {
    T::from_json(&v)
}

// ---------------------------------------------------------------- conversion

/// A type that writes itself as JSON (the owned stand-in for
/// `serde::Serialize`).
pub trait ToJson {
    /// The value.
    fn to_json(&self) -> Value;

    /// Appends the value as compact JSON to `out`: the same text as
    /// `self.to_json().to_string()`. The default builds the [`Value`]; the
    /// structs of [`impl_to_json!`], strings, numbers, options and lists
    /// write themselves without one.
    fn write_json(&self, out: &mut String) {
        write_value(out, &self.to_json(), None);
    }
}

/// A type that reads itself from JSON (the owned stand-in for
/// `serde::Deserialize`).
pub trait FromJson: Sized {
    /// Reads the value.
    ///
    /// # Errors
    ///
    /// A message naming what was wrong.
    fn from_json(v: &Value) -> Result<Self, String>;
}

impl FromJson for Value {
    fn from_json(v: &Value) -> Result<Value, String> {
        Ok(v.clone())
    }
}

/// Converts anything [`ToJson`] into a [`Value`].
pub fn to_value<T: ToJson + ?Sized>(v: &T) -> Value {
    v.to_json()
}

impl ToJson for Value {
    fn to_json(&self) -> Value {
        self.clone()
    }
    fn write_json(&self, out: &mut String) {
        write_value(out, self, None);
    }
}
impl ToJson for Map {
    fn to_json(&self) -> Value {
        Value::Object(self.clone())
    }
}
impl ToJson for bool {
    fn to_json(&self) -> Value {
        Value::Bool(*self)
    }
    fn write_json(&self, out: &mut String) {
        out.push_str(if *self { "true" } else { "false" });
    }
}
macro_rules! to_json_int {
    ($($t:ty),*) => {$(
        impl ToJson for $t {
            #[allow(unused_comparisons)]
            fn to_json(&self) -> Value {
                let v = *self;
                if v < 0 {
                    Value::Number(Number::I(v as i64))
                } else {
                    Value::Number(Number::U(v as u64))
                }
            }
            #[allow(unused_comparisons)]
            fn write_json(&self, out: &mut String) {
                let v = *self;
                if v < 0 {
                    out.push('-');
                    write_u64(out, (v as i64).unsigned_abs());
                } else {
                    write_u64(out, v as u64);
                }
            }
        }
    )*};
}
to_json_int!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
impl ToJson for u128 {
    fn to_json(&self) -> Value {
        match u64::try_from(*self) {
            Ok(u) => Value::Number(Number::U(u)),
            Err(_) => Value::Number(Number::F(*self as f64)),
        }
    }
}
impl ToJson for f64 {
    fn to_json(&self) -> Value {
        if self.is_finite() {
            Value::Number(Number::F(*self))
        } else {
            Value::Null
        }
    }
}
impl ToJson for f32 {
    fn to_json(&self) -> Value {
        f64::from(*self).to_json()
    }
}
impl ToJson for str {
    fn to_json(&self) -> Value {
        Value::String(self.to_string())
    }
    fn write_json(&self, out: &mut String) {
        write_str(out, self);
    }
}
impl ToJson for String {
    fn to_json(&self) -> Value {
        Value::String(self.clone())
    }
    fn write_json(&self, out: &mut String) {
        write_str(out, self);
    }
}
impl ToJson for char {
    fn to_json(&self) -> Value {
        Value::String(self.to_string())
    }
}
impl<T: ToJson + ?Sized> ToJson for &T {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
    fn write_json(&self, out: &mut String) {
        (**self).write_json(out);
    }
}
impl<T: ToJson + ?Sized> ToJson for &mut T {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
}
impl<T: ToJson + ?Sized> ToJson for Box<T> {
    fn to_json(&self) -> Value {
        (**self).to_json()
    }
    fn write_json(&self, out: &mut String) {
        (**self).write_json(out);
    }
}
impl<T: ToJson> ToJson for Option<T> {
    fn to_json(&self) -> Value {
        match self {
            Some(v) => v.to_json(),
            None => Value::Null,
        }
    }
    fn write_json(&self, out: &mut String) {
        match self {
            Some(v) => v.write_json(out),
            None => out.push_str("null"),
        }
    }
}
impl<T: ToJson> ToJson for [T] {
    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(ToJson::to_json).collect())
    }
    fn write_json(&self, out: &mut String) {
        out.push('[');
        for (i, x) in self.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            x.write_json(out);
        }
        out.push(']');
    }
}
impl<T: ToJson, const N: usize> ToJson for [T; N] {
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
}
impl<T: ToJson> ToJson for Vec<T> {
    fn to_json(&self) -> Value {
        self.as_slice().to_json()
    }
    fn write_json(&self, out: &mut String) {
        self.as_slice().write_json(out);
    }
}
impl<A: ToJson, B: ToJson> ToJson for (A, B) {
    fn to_json(&self) -> Value {
        Value::Array(vec![self.0.to_json(), self.1.to_json()])
    }
}
impl<K: AsRef<str>, V: ToJson> ToJson for BTreeMap<K, V> {
    fn to_json(&self) -> Value {
        Value::Object(self.iter().map(|(k, v)| (k.as_ref().to_string(), v.to_json())).collect())
    }
}
impl<K: AsRef<str> + Ord, V: ToJson, S> ToJson for HashMap<K, V, S> {
    /// Keys sorted, so the output is stable.
    fn to_json(&self) -> Value {
        let mut v: Vec<(&K, &V)> = self.iter().collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        Value::Object(v.into_iter().map(|(k, v)| (k.as_ref().to_string(), v.to_json())).collect())
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::String(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::String(s)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}
macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(n: $t) -> Value {
                n.to_json()
            }
        }
    )*};
}
from_int!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
impl From<Map> for Value {
    fn from(m: Map) -> Value {
        Value::Object(m)
    }
}
impl From<Vec<Value>> for Value {
    fn from(a: Vec<Value>) -> Value {
        Value::Array(a)
    }
}

/// Implements [`ToJson`] for a struct as an object of its fields, in the
/// order listed. Fields after `skip_none` are left out when `None`.
///
/// ```
/// struct P { a: i64, b: Option<String> }
/// atlas::impl_to_json!(P { a } skip_none { b });
/// let v = atlas::json::to_value(&P { a: 1, b: None });
/// assert_eq!(v.to_string(), r#"{"a":1}"#);
/// ```
#[macro_export]
macro_rules! impl_to_json {
    ($t:ty { $($f:ident),* $(,)? } $(skip_none { $($o:ident),* $(,)? })?) => {
        impl $crate::json::ToJson for $t {
            fn to_json(&self) -> $crate::json::Value {
                let mut m = $crate::json::Map::new();
                $( m.insert(stringify!($f).to_string(), $crate::json::ToJson::to_json(&self.$f)); )*
                $($( if let Some(v) = &self.$o { m.insert(stringify!($o).to_string(), $crate::json::ToJson::to_json(v)); } )*)?
                $crate::json::Value::Object(m)
            }

            // The same text as `to_json` gives, written field by field.
            #[allow(unused_assignments, unused_mut, unused_variables)]
            fn write_json(&self, out: &mut String) {
                out.push('{');
                let mut first = true;
                $(
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    $crate::json::write_str(out, stringify!($f));
                    out.push(':');
                    $crate::json::ToJson::write_json(&self.$f, out);
                )*
                $($(
                    if let Some(v) = &self.$o {
                        if !first {
                            out.push(',');
                        }
                        first = false;
                        $crate::json::write_str(out, stringify!($o));
                        out.push(':');
                        $crate::json::ToJson::write_json(v, out);
                    }
                )*)?
                out.push('}');
            }
        }
    };
}

/// Builds a [`Value`] with JSON syntax; any other expression goes through
/// [`ToJson`]. The same syntax as `serde_json::json!`.
#[macro_export]
macro_rules! json {
    (null) => { $crate::json::Value::Null };
    (true) => { $crate::json::Value::Bool(true) };
    (false) => { $crate::json::Value::Bool(false) };
    ([]) => { $crate::json::Value::Array(::std::vec::Vec::new()) };
    ([ $($tt:tt)+ ]) => { $crate::json::Value::Array($crate::json_vec!(@elem [] $($tt)+)) };
    ({}) => { $crate::json::Value::Object($crate::json::Map::new()) };
    ({ $($tt:tt)+ }) => {{
        let mut m = $crate::json::Map::new();
        $crate::json_obj!(@key m $($tt)+);
        $crate::json::Value::Object(m)
    }};
    ($e:expr) => { $crate::json::to_value(&$e) };
}

/// Internal to [`json!`]: array elements, split at top-level commas.
#[doc(hidden)]
#[macro_export]
macro_rules! json_vec {
    (@elem [$($done:expr,)*]) => { ::std::vec![$($done,)*] };
    (@elem [$($done:expr,)*] null $(, $($rest:tt)*)?) => {
        $crate::json_vec!(@elem [$($done,)* $crate::json::Value::Null,] $($($rest)*)?)
    };
    (@elem [$($done:expr,)*] [$($a:tt)*] $(, $($rest:tt)*)?) => {
        $crate::json_vec!(@elem [$($done,)* $crate::json!([$($a)*]),] $($($rest)*)?)
    };
    (@elem [$($done:expr,)*] {$($o:tt)*} $(, $($rest:tt)*)?) => {
        $crate::json_vec!(@elem [$($done,)* $crate::json!({$($o)*}),] $($($rest)*)?)
    };
    (@elem [$($done:expr,)*] $e:expr $(, $($rest:tt)*)?) => {
        $crate::json_vec!(@elem [$($done,)* $crate::json::to_value(&$e),] $($($rest)*)?)
    };
}

/// Internal to [`json!`]: object members, `key: value` split at top-level
/// commas. A value is `null`, a JSON array or object literal, or any Rust
/// expression (parsed as one, so `r.get::<_, String>(0)?` keeps its comma).
#[doc(hidden)]
#[macro_export]
macro_rules! json_obj {
    (@key $m:ident) => {};
    (@key $m:ident $key:tt : null $(, $($rest:tt)*)?) => {
        $m.insert(::std::string::String::from($key), $crate::json::Value::Null);
        $crate::json_obj!(@key $m $($($rest)*)?);
    };
    (@key $m:ident $key:tt : [$($a:tt)*] $(, $($rest:tt)*)?) => {
        $m.insert(::std::string::String::from($key), $crate::json!([$($a)*]));
        $crate::json_obj!(@key $m $($($rest)*)?);
    };
    (@key $m:ident $key:tt : {$($o:tt)*} $(, $($rest:tt)*)?) => {
        $m.insert(::std::string::String::from($key), $crate::json!({$($o)*}));
        $crate::json_obj!(@key $m $($($rest)*)?);
    };
    (@key $m:ident $key:tt : $v:expr $(, $($rest:tt)*)?) => {
        $m.insert(::std::string::String::from($key), $crate::json::to_value(&$v));
        $crate::json_obj!(@key $m $($($rest)*)?);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Inner {
        n: i64,
        s: String,
    }
    crate::impl_to_json!(Inner { n, s });

    #[derive(Debug)]
    struct Outer {
        a: Vec<Inner>,
        b: Option<String>,
        c: usize,
        d: bool,
        e: f64,
        f: Value,
        g: Option<i32>,
    }
    crate::impl_to_json!(Outer { a, b, c, d, e, f } skip_none { g });

    #[test]
    fn direct_writing_matches_the_value() {
        let cases = [
            Outer { a: vec![], b: None, c: 0, d: false, e: 0.25, f: Value::Null, g: None },
            Outer {
                a: vec![Inner { n: -7, s: "tab\there \"q\" \\ é\u{1}".into() }, Inner { n: i64::MIN, s: String::new() }],
                b: Some("x\ny".into()),
                c: usize::MAX,
                d: true,
                e: f64::NAN,
                f: crate::json!({"k": [1, "two", null]}),
                g: Some(-3),
            },
        ];
        for o in &cases {
            assert_eq!(to_string(o), o.to_json().to_string(), "{o:?}");
        }
    }

    #[test]
    fn object_writer_matches_a_map_built_in_the_same_order() {
        let inner = vec![Inner { n: 1, s: "a\"b".into() }];
        let written = object(0, |w| {
            w.field("x", &inner).field("y", "line\nbreak").field("z", &crate::json!({"k": null})).field("u", &5u128);
        });
        let mut v = crate::json!({"x": inner, "y": "line\nbreak"});
        v["z"] = crate::json!({"k": null});
        v["u"] = crate::json!(5u128);
        assert_eq!(written, v.to_string());
        assert_eq!(object(0, |_| {}), "{}");
    }

    #[test]
    fn round_trips() {
        for text in [
            r#"{"a":1,"b":[true,false,null],"c":"x\"y\\z\n","d":-5,"e":1.5,"f":{}}"#,
            r#"[]"#,
            r#""é ☃ 𝄞""#,
            r#"18446744073709551615"#,
            r#"-9223372036854775808"#,
            r#"{"z":1,"a":2}"#,
        ] {
            let v = parse(text).unwrap();
            assert_eq!(v.to_string(), text, "{text}");
            assert_eq!(parse(&v.to_string()).unwrap(), v);
        }
        let v = parse(r#" { "k" : [ 1 , 2 ] } "#).unwrap();
        assert_eq!(v.to_string(), r#"{"k":[1,2]}"#);
        assert_eq!(parse(r#""\u00e9\ud834\udd1e\/""#).unwrap(), Value::String("é𝄞/".into()));
        assert_eq!(parse("1e3").unwrap().to_string(), "1000.0");
        assert_eq!(parse("0.1").unwrap().to_string(), "0.1");
    }

    #[test]
    fn whole_floats_stay_floats_through_a_round_trip() {
        for text in ["12202402475002402.61621", "1e16", "-3e17", "1.8e19", "1e300", "123.0", "-0.0"] {
            let v = parse(text).unwrap();
            assert!(!v.as_f64().is_some_and(|_| v.as_u64().is_some() || v.as_i64().is_some()), "{text} parsed as an integer");
            let back = parse(&v.to_string()).unwrap();
            assert_eq!(back, v, "{text} -> {v}");
            assert!(back.as_u64().is_none() && back.as_i64().is_none(), "{text} -> {v} reads back as an integer");
        }
    }

    #[test]
    fn many_keys_parse_in_linear_time_and_duplicates_keep_their_place() {
        // About the most keys a 4 MiB protocol message holds. Quadratic
        // insertion took minutes here.
        let n = 300_000;
        let text = format!("{{{}}}", (0..n).map(|i| format!("\"k{i}\":{i}")).collect::<Vec<_>>().join(","));
        let t = std::time::Instant::now();
        let v = parse(&text).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
        assert_eq!(v.as_object().unwrap().len(), n);
        assert_eq!(v["k299999"], 299_999);
        // Duplicates, below and past the index threshold: the last value,
        // in the first key's place.
        for size in [3, INDEX_KEYS + 5] {
            let mut keys: Vec<String> = (0..size).map(|i| format!("\"k{i}\":{i}")).collect();
            keys.push("\"k1\":\"last\"".into());
            let v = parse(&format!("{{{}}}", keys.join(","))).unwrap();
            let m = v.as_object().unwrap();
            assert_eq!(m.len(), size);
            assert_eq!(m.keys().nth(1).map(String::as_str), Some("k1"));
            assert_eq!(m.get("k1"), Some(&Value::from("last")));
        }
    }

    #[test]
    fn nasty_inputs_are_refused_with_a_reason() {
        let deep_ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&deep_ok).is_ok());
        let cases: Vec<(String, &str)> = vec![
            ("[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1), "nesting too deep"),
            ("\u{feff}{}".into(), "byte order mark"),
            (r#""\ud800""#.into(), "lone leading surrogate"),
            (r#""\udc00""#.into(), "lone trailing surrogate"),
            (r#""\ud800\u0041""#.into(), "invalid trailing surrogate"),
            ("1e400".into(), "number out of range"),
            ("-1e999".into(), "number out of range"),
            ("01".into(), "invalid number"),
            ("1.".into(), "invalid number"),
            ("-".into(), "invalid number"),
            ("[1,]".into(), "unexpected character"),
            ("{\"a\":1,}".into(), "expected a string key"),
            ("{\"a\" 1}".into(), "expected :"),
            ("\"a\nb\"".into(), "control character"),
            ("\"abc".into(), "unterminated string"),
            ("\"\\x\"".into(), "invalid escape"),
            ("\"\\u12\"".into(), "\\u escape"),
            ("nul".into(), "invalid literal"),
            ("{} {}".into(), "trailing characters"),
            ("".into(), "unexpected end"),
            ("// c\n{}".into(), "unexpected character"),
            ("NaN".into(), "unexpected character"),
        ];
        for (text, want) in cases {
            let e = parse(&text).expect_err(&text);
            assert!(e.msg.contains(want), "{text:?}: got {:?}, want {want:?}", e.msg);
        }
        // Invalid UTF-8 inside a string.
        let e = parse_bytes(b"\"\xff\xfe\"").unwrap_err();
        assert!(e.msg.contains("UTF-8"), "{e}");
        // A huge integer past u64 becomes a float; past f64 it is refused.
        assert_eq!(parse("123456789012345678901234567890").unwrap().as_f64(), Some(1.2345678901234568e29));
    }

    #[test]
    fn writer_escapes_controls_and_keeps_unicode() {
        let v = json!({"s": "a\u{1}b\u{8}\u{c}\t\"é"});
        assert_eq!(v.to_string(), r#"{"s":"a\u0001b\b\f\t\"é"}"#);
        assert_eq!(json!(f64::NAN).to_string(), "null");
        assert_eq!(to_string_pretty(&json!({"a": [1, {}], "b": []})), "{\n  \"a\": [\n    1,\n    {}\n  ],\n  \"b\": []\n}");
    }

    #[test]
    fn macro_and_indexing() {
        let name = "x";
        let n: Option<u32> = None;
        let mut v = json!({
            "name": name,
            "list": [1, -2, "three", [4], {"five": 5}],
            "none": n,
            "neg": -1,
            "sum": 1 + 2,
            "nested": {"a": {"b": true}}
        });
        assert_eq!(v["name"], "x");
        assert_eq!(v["list"][1], -2);
        assert_eq!(v["list"][4]["five"], 5);
        assert!(v["none"].is_null() && v["missing"].is_null());
        assert_eq!(v["sum"], 3);
        assert_eq!(v.pointer("/nested/a/b"), Some(&Value::Bool(true)));
        v["added"] = json!([]);
        v["added"].as_array_mut().unwrap().push(json!(1));
        assert_eq!(v["added"], json!([1]));
        assert_eq!(json!({"a": 1, "b": 2}), json!({"b": 2, "a": 1}));
        assert_ne!(json!(1), json!(1.5));
        assert_eq!(json!(1), json!(1u64));
    }
}
