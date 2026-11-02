//! An owned regular-expression engine for the subset atlas uses, with no
//! dependency: identifier patterns from the configuration, the `ids`
//! filter, regex search, and the fixed patterns of the Markdown parser.
//!
//! # The subset
//!
//! | Syntax | Meaning |
//! |---|---|
//! | `a`, `\.`, `\\`, `\n`, `\t`, `\r` | a literal (any escaped ASCII punctuation is literal) |
//! | `.` | any character but `\n` (any at all with `(?s)`) |
//! | `[abc]`, `[a-z]`, `[^...]` | a class; inside it `\d \w \s \p{L} \p{N}` and escapes work |
//! | `\d \D` | a decimal digit: `char::is_numeric` (Unicode N; the `regex` crate uses Nd only) |
//! | `\w \W` | a word character: `char::is_alphanumeric` or `_` |
//! | `\s \S` | white space: `char::is_whitespace` |
//! | `\p{L} \P{L} \p{N} \P{N}` | Unicode letters (`char::is_alphabetic`) and numbers (`char::is_numeric`) |
//! | `\b \B` | a word boundary (by `\w`), and its negation |
//! | `^ $` | start and end of the text (of a line with `(?m)`) |
//! | `(...)`, `(?:...)`, `(?P<name>...)`, `(?<name>...)` | groups: capturing, plain, named |
//! | `a\|b` | alternation, leftmost first |
//! | `* + ? {n} {n,} {n,m}` | repetition, greedy; a trailing `?` makes it lazy |
//! | `(?i)`, `(?m)`, `(?s)` at the very start | case-insensitive, multi-line, dot matches newline |
//!
//! Anything else (back-references, look-around, `\A`, `\z`, `\x`, `\u`,
//! other Unicode classes, flags anywhere but the start, possessive
//! repetition) is refused by [`Regex::new`] with an error naming it: a
//! pattern is never matched by different rules than it was written for.
//!
//! # The engine
//!
//! A pattern compiles to a small program run by a bounded backtracker: it
//! explores alternatives leftmost first (the semantics of the `regex`
//! crate's `find` and `captures`) and records every (instruction, position)
//! pair it has tried, so a search costs at most program length times text
//! length steps and no pattern can blow up. The record of tried pairs is a
//! bitset of program length times text length bits; a search that would
//! need more than [`MAX_VISITED_BITS`] is refused (it finds nothing, and
//! [`Regex::check_text`] says why first), and a large set is freed after
//! its search. A program with no alternative to explore (a literal, a
//! sequence of classes) never backtracks and needs no record at all. Before that, a prefilter skips
//! text that cannot match: a literal every match must contain is looked for
//! first (with [`crate::scan`]), and a match that must start with a literal
//! is tried only where that literal occurs.

use std::cell::RefCell;
use std::fmt;

/// The most instructions a compiled pattern may hold.
pub const MAX_PROGRAM: usize = 20_000;

/// The largest counted repetition, `{n}` or `{n,m}`.
pub const MAX_REPEAT: u32 = 1000;

/// The most bits one search's visited set may take: program length times
/// (text length + 1). 256 Mibit is 32 MiB, freed after the search. It lets
/// any pattern of up to 256 instructions (every fixed pattern atlas uses,
/// and a typical search query several times over) search a whole note at
/// the 1 MiB read cap, and a 1000-instruction pattern lines of up to
/// 256 KiB, while the worst case before it (a pattern near [`MAX_PROGRAM`]
/// over a 1 MiB line) asked for 2.5 GB.
pub const MAX_VISITED_BITS: usize = 1 << 28;

/// The visited set a thread keeps between searches, in 64-bit words
/// (512 KiB: a 100-instruction pattern over a 40 KiB line). A search that
/// needed more frees its set afterwards.
const KEEP_WORDS: usize = 1 << 16;

/// Why a pattern was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// What was wrong, and where.
    pub msg: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

fn err<T>(at: usize, msg: impl Into<String>) -> Result<T, Error> {
    Err(Error { msg: format!("{} at offset {at}", msg.into()) })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Named {
    Digit,
    Word,
    Space,
    Letter,
    Number,
}

impl Named {
    fn test(self, c: char) -> bool {
        match self {
            Named::Digit => c.is_ascii_digit() || (!c.is_ascii() && c.is_numeric()),
            Named::Word => c.is_alphanumeric() || c == '_',
            Named::Space => c.is_whitespace(),
            Named::Letter => c.is_alphabetic(),
            Named::Number => c.is_numeric(),
        }
    }
}

/// A character class: an ASCII bitmap plus, for other characters, ranges
/// and named sets; negation already applied to the bitmap.
#[derive(Debug, Clone, Default)]
struct Class {
    ascii: [u64; 2],
    ranges: Vec<(char, char)>,
    named: Vec<(Named, bool)>,
    negated: bool,
    fold: bool,
}

impl Class {
    fn add_range(&mut self, lo: char, hi: char) {
        for c in lo..=hi.min('\u{7f}') {
            self.ascii[c as usize / 64] |= 1 << (c as usize % 64);
        }
        if hi > '\u{7f}' {
            self.ranges.push((lo.max('\u{80}'), hi));
        }
    }
    fn add_named(&mut self, n: Named, neg: bool) {
        for b in 0u8..128 {
            if n.test(b as char) != neg {
                self.ascii[b as usize / 64] |= 1 << (b as usize % 64);
            }
        }
        self.named.push((n, neg));
    }
    /// Applies case folding and negation to the ASCII bitmap; called once
    /// the class is complete.
    fn finish(&mut self) {
        if self.fold {
            for b in b'a'..=b'z' {
                let (l, u) = (b as usize, b.to_ascii_uppercase() as usize);
                let has = |a: &[u64; 2], i: usize| a[i / 64] >> (i % 64) & 1 == 1;
                if has(&self.ascii, l) || has(&self.ascii, u) {
                    self.ascii[l / 64] |= 1 << (l % 64);
                    self.ascii[u / 64] |= 1 << (u % 64);
                }
            }
        }
        if self.negated {
            self.ascii = [!self.ascii[0], !self.ascii[1]];
        }
    }
    fn raw(&self, c: char) -> bool {
        self.ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi) || self.named.iter().any(|&(n, neg)| n.test(c) != neg)
    }
    #[inline]
    fn test(&self, c: char) -> bool {
        if c.is_ascii() {
            let i = c as usize;
            return self.ascii[i / 64] >> (i % 64) & 1 == 1;
        }
        let mut hit = self.raw(c);
        if !hit && self.fold {
            hit = c.to_lowercase().any(|l| self.raw(l)) || c.to_uppercase().any(|u| self.raw(u));
        }
        hit != self.negated
    }
    /// True when the class can match a non-ASCII character.
    fn non_ascii(&self) -> bool {
        self.negated || !self.ranges.is_empty() || !self.named.is_empty()
    }
}

#[derive(Debug, Clone)]
enum Node {
    Empty,
    Lit(char),
    Class(Class),
    Any,
    Bol,
    Eol,
    WordB(bool),
    Group(Box<Node>, Option<usize>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat { node: Box<Node>, min: u32, max: Option<u32>, greedy: bool },
}

#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    fold: bool,
    multi: bool,
    dotall: bool,
}

struct Parser<'a> {
    p: &'a str,
    i: usize,
    flags: Flags,
    names: Vec<Option<String>>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.p[self.i..].chars().next()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += c.len_utf8();
        Some(c)
    }
    fn eat(&mut self, s: &str) -> bool {
        if self.p[self.i..].starts_with(s) {
            self.i += s.len();
            true
        } else {
            false
        }
    }

    fn leading_flags(&mut self) -> Result<(), Error> {
        while self.p[self.i..].starts_with("(?") {
            let rest = &self.p[self.i + 2..];
            let Some(end) = rest.find(')') else { return Ok(()) };
            let body = &rest[..end];
            if body.is_empty() || !body.chars().all(|c| matches!(c, 'i' | 'm' | 's')) {
                return Ok(());
            }
            for c in body.chars() {
                match c {
                    'i' => self.flags.fold = true,
                    'm' => self.flags.multi = true,
                    _ => self.flags.dotall = true,
                }
            }
            self.i += 2 + end + 1;
        }
        Ok(())
    }

    fn alt(&mut self, depth: usize) -> Result<Node, Error> {
        if depth > 64 {
            return err(self.i, "groups nested too deeply");
        }
        let mut arms = vec![self.concat(depth)?];
        while self.eat("|") {
            arms.push(self.concat(depth)?);
        }
        Ok(if arms.len() == 1 { arms.pop().unwrap_or(Node::Empty) } else { Node::Alt(arms) })
    }

    fn concat(&mut self, depth: usize) -> Result<Node, Error> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom(depth)?;
            items.push(self.repeat(atom)?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    fn repeat(&mut self, atom: Node) -> Result<Node, Error> {
        let at = self.i;
        let (min, max) = match self.peek() {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            Some('{') => {
                let rest = &self.p[self.i + 1..];
                let Some(end) = rest.find('}') else { return err(at, "unclosed {") };
                let body = &rest[..end];
                let num = |s: &str| -> Result<u32, Error> {
                    match s.parse::<u32>() {
                        Ok(n) if n <= MAX_REPEAT => Ok(n),
                        Ok(_) => err(at, format!("repetition over {MAX_REPEAT}")),
                        Err(_) => err(at, format!("bad repetition {{{body}}}")),
                    }
                };
                let (min, max) = match body.split_once(',') {
                    None => {
                        let n = num(body)?;
                        (n, Some(n))
                    }
                    Some((a, "")) => (num(a)?, None),
                    Some((a, b)) => (num(a)?, Some(num(b)?)),
                };
                if max.is_some_and(|m| m < min) {
                    return err(at, format!("repetition {{{body}}} has max below min"));
                }
                self.i += end + 1; // the brace body; the `{` is bumped below
                (min, max)
            }
            _ => return Ok(atom),
        };
        self.bump();
        if matches!(atom, Node::Bol | Node::Eol | Node::WordB(_) | Node::Empty) {
            return err(at, "repetition of an empty or zero-width item");
        }
        let greedy = !self.eat("?");
        if self.peek() == Some('+') {
            return err(self.i, "unsupported: possessive repetition");
        }
        if matches!(self.peek(), Some('*' | '+' | '?' | '{')) && !self.p[self.i..].starts_with("{") {
            return err(self.i, "repetition of a repetition");
        }
        Ok(Node::Repeat { node: Box::new(atom), min, max, greedy })
    }

    fn atom(&mut self, depth: usize) -> Result<Node, Error> {
        let at = self.i;
        let c = self.bump().ok_or_else(|| Error { msg: "unexpected end".into() })?;
        Ok(match c {
            '.' => Node::Any,
            '^' => Node::Bol,
            '$' => Node::Eol,
            '[' => Node::Class(self.class()?),
            '(' => {
                let mut cap = None;
                if self.eat("?:") {
                } else if self.eat("?P<") || self.eat("?<") {
                    if self.peek() == Some('=') || self.peek() == Some('!') {
                        return err(at, "unsupported: look-behind");
                    }
                    let end = self.p[self.i..].find('>').ok_or_else(|| Error { msg: format!("unclosed group name at offset {at}") })?;
                    let name = &self.p[self.i..self.i + end];
                    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                        return err(at, format!("bad group name {name:?}"));
                    }
                    if self.names.iter().any(|n| n.as_deref() == Some(name)) {
                        return err(at, format!("duplicate group name {name:?}"));
                    }
                    self.i += end + 1;
                    cap = Some(self.names.len());
                    self.names.push(Some(name.to_string()));
                } else if self.peek() == Some('?') {
                    let what = match self.p[self.i..].chars().nth(1) {
                        Some('=') | Some('!') => "look-ahead",
                        Some('i' | 'm' | 's' | '-') => "flags after the start of the pattern",
                        _ => "group syntax",
                    };
                    return err(at, format!("unsupported: {what}"));
                } else {
                    cap = Some(self.names.len());
                    self.names.push(None);
                }
                let inner = self.alt(depth + 1)?;
                if !self.eat(")") {
                    return err(at, "unclosed (");
                }
                Node::Group(Box::new(inner), cap)
            }
            ')' => return err(at, "unmatched )"),
            '*' | '+' | '?' => return err(at, format!("nothing to repeat before {c}")),
            '{' => return err(at, "nothing to repeat before {"),
            '\\' => self.escape(at, false)?,
            c => Node::Lit(c),
        })
    }

    /// An escape after the backslash. In a class (`in_class`), `\b` is not
    /// allowed and the named sets come back as a class node.
    fn escape(&mut self, at: usize, in_class: bool) -> Result<Node, Error> {
        let c = self.bump().ok_or_else(|| Error { msg: format!("trailing backslash at offset {at}") })?;
        let named = |n: Named, neg: bool| {
            let mut k = Class::default();
            k.add_named(n, neg);
            k.negated = false;
            Node::Class(k)
        };
        Ok(match c {
            'n' => Node::Lit('\n'),
            't' => Node::Lit('\t'),
            'r' => Node::Lit('\r'),
            'd' => named(Named::Digit, false),
            'D' => named(Named::Digit, true),
            'w' => named(Named::Word, false),
            'W' => named(Named::Word, true),
            's' => named(Named::Space, false),
            'S' => named(Named::Space, true),
            'b' if !in_class => Node::WordB(true),
            'B' if !in_class => Node::WordB(false),
            'p' | 'P' => {
                let name = if self.eat("{") {
                    let end = self.p[self.i..].find('}').ok_or_else(|| Error { msg: format!("unclosed \\p{{ at offset {at}") })?;
                    let n = self.p[self.i..self.i + end].to_string();
                    self.i += end + 1;
                    n
                } else {
                    self.bump().map(String::from).unwrap_or_default()
                };
                let n = match name.as_str() {
                    "L" | "Letter" => Named::Letter,
                    "N" | "Number" => Named::Number,
                    o => return err(at, format!("unsupported Unicode class \\p{{{o}}}; only L and N")),
                };
                named(n, c == 'P')
            }
            c if c.is_ascii_punctuation() => Node::Lit(c),
            c if c.is_ascii_digit() => return err(at, "unsupported: back-reference"),
            'A' | 'z' | 'Z' => return err(at, format!("unsupported: \\{c}; use ^ or $")),
            c => return err(at, format!("unsupported escape \\{c}")),
        })
    }

    fn class(&mut self) -> Result<Class, Error> {
        let start = self.i - 1;
        let mut k = Class { fold: self.flags.fold, ..Class::default() };
        if self.eat("^") {
            k.negated = true;
        }
        let mut first = true;
        loop {
            let at = self.i;
            let Some(c) = self.bump() else { return err(start, "unclosed [") };
            if c == ']' && !first {
                break;
            }
            first = false;
            if c == '[' && self.peek() == Some(':') {
                return err(at, "unsupported: POSIX class [:name:]");
            }
            let lo = if c == '\\' {
                match self.escape(at, true)? {
                    Node::Lit(l) => l,
                    Node::Class(inner) => {
                        for &(n, neg) in &inner.named {
                            k.add_named(n, neg);
                        }
                        continue;
                    }
                    _ => return err(at, "bad escape in class"),
                }
            } else {
                c
            };
            if self.peek() == Some('-') && !self.p[self.i + 1..].starts_with(']') {
                self.bump();
                let at2 = self.i;
                let hi = match self.bump() {
                    Some('\\') => match self.escape(at2, true)? {
                        Node::Lit(h) => h,
                        _ => return err(at2, "a class range must end in a character"),
                    },
                    Some(h) => h,
                    None => return err(start, "unclosed ["),
                };
                if hi < lo {
                    return err(at, format!("class range {lo}-{hi} is backwards"));
                }
                k.add_range(lo, hi);
            } else {
                k.add_range(lo, lo);
            }
        }
        k.finish();
        Ok(k)
    }
}

#[derive(Debug, Clone)]
enum Inst {
    Byte(u8),
    /// An ASCII letter, either case (the lowercase form stored).
    ByteFold(u8),
    Char(char),
    CharFold(char),
    Class(usize),
    Any,
    AnyNl,
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Bol,
    Eol,
    WordB(bool),
    Match,
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub struct Regex {
    pattern: String,
    prog: Vec<Inst>,
    classes: Vec<Class>,
    names: std::sync::Arc<[Option<String>]>,
    flags: Flags,
    /// A literal (case-sensitive) every match contains.
    required: Vec<u8>,
    /// A literal every match starts with, and whether it is folded.
    prefix: Option<(Vec<u8>, bool)>,
    /// Bytes a match can start with (all 256 when unknown).
    first: [bool; 256],
    /// True when an empty string can match.
    nullable: bool,
    /// Set when the whole pattern is one class repeated one or more times
    /// greedily (`[A-Za-z0-9_-]+`): matched by a direct scan, no program.
    run_of: Option<Class>,
    /// Whether the program can backtrack (holds a split), and so needs the
    /// visited set. Without one a run is a straight line through it.
    memo: bool,
}

/// Builds a [`Regex`] with options.
#[derive(Debug, Clone)]
pub struct RegexBuilder {
    pattern: String,
    fold: bool,
}

impl RegexBuilder {
    /// Starts a builder for a pattern.
    pub fn new(pattern: &str) -> RegexBuilder {
        RegexBuilder { pattern: pattern.to_string(), fold: false }
    }
    /// Matches letters in either case.
    pub fn case_insensitive(&mut self, yes: bool) -> &mut RegexBuilder {
        self.fold = yes;
        self
    }
    /// Compiles the pattern.
    ///
    /// # Errors
    ///
    /// [`Error`] for syntax outside the subset (see the module docs).
    pub fn build(&self) -> Result<Regex, Error> {
        Regex::compile(&self.pattern, self.fold)
    }
}

/// A match: a byte range of the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match<'t> {
    text: &'t str,
    start: usize,
    end: usize,
}

impl<'t> Match<'t> {
    /// Byte offset of the start.
    pub fn start(&self) -> usize {
        self.start
    }
    /// Byte offset past the end.
    pub fn end(&self) -> usize {
        self.end
    }
    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.end - self.start
    }
    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
    /// The matched text.
    pub fn as_str(&self) -> &'t str {
        &self.text[self.start..self.end]
    }
}

/// The groups of one match.
#[derive(Debug, Clone)]
pub struct Captures<'t> {
    text: &'t str,
    slots: Vec<Option<usize>>,
    names: std::sync::Arc<[Option<String>]>,
}

impl<'t> Captures<'t> {
    /// Group `i` (0 is the whole match), if it took part.
    pub fn get(&self, i: usize) -> Option<Match<'t>> {
        match (self.slots.get(2 * i).copied().flatten(), self.slots.get(2 * i + 1).copied().flatten()) {
            (Some(start), Some(end)) => Some(Match { text: self.text, start, end }),
            _ => None,
        }
    }
    /// The named group, if it took part.
    pub fn name(&self, n: &str) -> Option<Match<'t>> {
        let i = self.names.iter().position(|x| x.as_deref() == Some(n))?;
        self.get(i)
    }
    /// How many groups, the whole match included.
    pub fn len(&self) -> usize {
        self.slots.len() / 2
    }
    /// Never true: group 0 always exists.
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl<'t> std::ops::Index<usize> for Captures<'t> {
    type Output = str;
    /// The text of group `i`.
    ///
    /// # Panics
    ///
    /// When group `i` did not take part in the match.
    fn index(&self, i: usize) -> &str {
        self.get(i).map(|m| m.as_str()).unwrap_or_else(|| panic!("no group {i} in this match"))
    }
}

/// The backtracker's visited set: a bitset over (instruction, position),
/// kept all-zero between searches by clearing only the words a search
/// touched, so a search costs what it explores and not the text length.
struct Visited {
    bits: Vec<u64>,
    touched: Vec<usize>,
    /// The backtracking stack, kept between runs to save allocations.
    stack: Vec<Frame>,
}

/// A backtracking stack entry: resume at (instruction, position), or put a
/// capture slot back.
enum Frame {
    Step(usize, usize),
    Restore(usize, Option<usize>),
}

impl Visited {
    #[inline]
    fn test_set(&mut self, k: usize) -> bool {
        let w = &mut self.bits[k / 64];
        let bit = 1u64 << (k % 64);
        if *w & bit != 0 {
            return true;
        }
        if *w == 0 {
            self.touched.push(k / 64);
        }
        *w |= bit;
        false
    }
    fn reset(&mut self) {
        for &w in &self.touched {
            self.bits[w] = 0;
        }
        self.touched.clear();
    }
    /// Frees what a large search grew past [`KEEP_WORDS`].
    fn shrink(&mut self) {
        if self.bits.len() > KEEP_WORDS {
            self.bits = Vec::new();
        }
        if self.touched.capacity() > KEEP_WORDS {
            self.touched = Vec::new();
        }
        if self.stack.capacity() > KEEP_WORDS {
            self.stack = Vec::new();
        }
    }
}

/// The bytes the calling thread's visited set holds between searches.
#[doc(hidden)]
pub fn kept_bytes() -> usize {
    VISITED.with(|v| {
        let v = v.borrow();
        v.bits.capacity() * 8 + v.touched.capacity() * std::mem::size_of::<usize>() + v.stack.capacity() * std::mem::size_of::<Frame>()
    })
}

thread_local! {
    static VISITED: RefCell<Visited> = const { RefCell::new(Visited { bits: Vec::new(), touched: Vec::new(), stack: Vec::new() }) };
}

/// Escapes every character the subset gives a meaning to.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

impl Regex {
    /// Compiles a pattern with the default options.
    ///
    /// # Errors
    ///
    /// [`Error`] for syntax outside the subset (see the module docs).
    pub fn new(pattern: &str) -> Result<Regex, Error> {
        Regex::compile(pattern, false)
    }

    fn compile(pattern: &str, fold: bool) -> Result<Regex, Error> {
        let mut p = Parser { p: pattern, i: 0, flags: Flags { fold, ..Flags::default() }, names: vec![None] };
        p.leading_flags()?;
        let ast = p.alt(0)?;
        if p.i < pattern.len() {
            return err(p.i, "unmatched )");
        }
        let flags = p.flags;
        let names = p.names;
        let mut c = Compiler { prog: Vec::new(), classes: Vec::new(), flags };
        c.push(Inst::Save(0));
        c.node(&ast)?;
        c.push(Inst::Save(1));
        c.push(Inst::Match);
        if c.prog.len() > MAX_PROGRAM {
            return err(0, format!("pattern compiles to over {MAX_PROGRAM} instructions"));
        }
        let mut first = [false; 256];
        let nullable = first_bytes(&ast, &flags, &mut first);
        if nullable {
            first = [true; 256];
        }
        let required = required_literal(&ast, &flags);
        let prefix = prefix_literal(&ast, &flags);
        let run_of = match &ast {
            Node::Repeat { node, min: 1, max: None, greedy: true } => match &**node {
                Node::Class(k) if !flags.fold => Some(k.clone()),
                _ => None,
            },
            _ => None,
        };
        let memo = c.prog.iter().any(|i| matches!(i, Inst::Split(..)));
        Ok(Regex {
            pattern: pattern.to_string(),
            prog: c.prog,
            classes: c.classes,
            names: names.into(),
            flags,
            required,
            prefix,
            first,
            nullable,
            run_of,
            memo,
        })
    }

    /// The visited-set bits a search of `len` bytes of text needs (0 when
    /// the program never backtracks or the pattern is a single-class run).
    pub fn visited_bits(&self, len: usize) -> usize {
        if !self.memo || self.run_of.is_some() {
            return 0;
        }
        self.prog.len().saturating_mul(len.saturating_add(1))
    }

    /// Whether a search of `len` bytes of text fits [`MAX_VISITED_BITS`].
    /// A search over the budget finds nothing, so a caller that takes the
    /// pattern from a user checks first and reports this error.
    ///
    /// # Errors
    ///
    /// [`Error`] naming the program length, the text length and what to
    /// narrow.
    pub fn check_text(&self, len: usize) -> Result<(), Error> {
        let bits = self.visited_bits(len);
        if bits <= MAX_VISITED_BITS {
            return Ok(());
        }
        Err(Error {
            msg: format!(
                "the pattern compiles to {} instructions and the text is {len} bytes: a search would need {} Mibit of backtracking state, over the {} Mibit budget (instructions x bytes); narrow the pattern (fewer or smaller counted repeats, fewer alternatives) or search shorter text",
                self.prog.len(),
                bits.div_ceil(1 << 20),
                MAX_VISITED_BITS >> 20
            ),
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    /// The group names, in order (group 0 and unnamed groups are `None`).
    pub fn capture_names(&self) -> impl Iterator<Item = Option<&str>> {
        self.names.iter().map(|n| n.as_deref())
    }

    /// True when the pattern matches somewhere in `text`. A search over
    /// [`MAX_VISITED_BITS`] (see [`Regex::check_text`]) finds nothing.
    pub fn is_match(&self, text: &str) -> bool {
        self.find_at(text, 0).is_some()
    }

    /// The leftmost-first match.
    pub fn find<'t>(&self, text: &'t str) -> Option<Match<'t>> {
        self.find_at(text, 0)
    }

    /// The leftmost-first match starting at or after byte `from`.
    pub fn find_at<'t>(&self, text: &'t str, from: usize) -> Option<Match<'t>> {
        let mut slots = vec![None; 2];
        self.search(text, from, &mut slots).map(|_| Match { text, start: slots[0].unwrap_or(0), end: slots[1].unwrap_or(0) })
    }

    /// The groups of the leftmost-first match.
    pub fn captures<'t>(&self, text: &'t str) -> Option<Captures<'t>> {
        self.captures_at(text, 0)
    }

    fn captures_at<'t>(&self, text: &'t str, from: usize) -> Option<Captures<'t>> {
        let mut slots = vec![None; self.names.len() * 2];
        self.search(text, from, &mut slots)?;
        Some(Captures { text, slots, names: self.names.clone() })
    }

    /// Every non-overlapping match, left to right.
    pub fn find_iter<'r, 't>(&'r self, text: &'t str) -> impl Iterator<Item = Match<'t>> + 'r
    where
        't: 'r,
    {
        let mut at = 0usize;
        let mut last_empty: Option<usize> = None;
        std::iter::from_fn(move || loop {
            if at > text.len() {
                return None;
            }
            let m = self.find_at(text, at)?;
            if m.is_empty() {
                if last_empty == Some(m.start) {
                    at = next_char(text, m.start);
                    continue;
                }
                last_empty = Some(m.start);
                at = next_char(text, m.end);
            } else {
                at = m.end;
            }
            return Some(m);
        })
    }

    /// The groups of every non-overlapping match, left to right.
    pub fn captures_iter<'r, 't>(&'r self, text: &'t str) -> impl Iterator<Item = Captures<'t>> + 'r
    where
        't: 'r,
    {
        let mut at = 0usize;
        std::iter::from_fn(move || {
            if at > text.len() {
                return None;
            }
            let c = self.captures_at(text, at)?;
            let m = c.get(0)?;
            at = if m.is_empty() { next_char(text, m.end) } else { m.end };
            Some(c)
        })
    }

    /// Finds the leftmost match at or after `from`, filling `slots`.
    fn search(&self, text: &str, from: usize, slots: &mut [Option<usize>]) -> Option<()> {
        let hay = text.as_bytes();
        if from > hay.len() {
            return None;
        }
        if let Some(k) = &self.run_of {
            // One class, greedy, at least once: the leftmost run.
            let mut i = from;
            while i < hay.len() {
                let c = if hay[i] < 0x80 { hay[i] as char } else { text[i..].chars().next().unwrap_or('\0') };
                if k.test(c) {
                    let mut j = i + c.len_utf8();
                    while j < hay.len() {
                        let d = if hay[j] < 0x80 { hay[j] as char } else { text[j..].chars().next().unwrap_or('\0') };
                        if !k.test(d) {
                            break;
                        }
                        j += d.len_utf8();
                    }
                    slots[0] = Some(i);
                    slots[1] = Some(j);
                    return Some(());
                }
                i += c.len_utf8();
            }
            return None;
        }
        if !self.required.is_empty() && crate::scan::find(&hay[from..], &self.required).is_none() {
            return None;
        }
        let width = hay.len() + 1;
        let bits = self.visited_bits(hay.len());
        if bits > MAX_VISITED_BITS {
            return None;
        }
        let need = bits.div_ceil(64);
        VISITED.with(|v| {
            let mut v = v.borrow_mut();
            if v.bits.len() < need {
                // All zero between searches, so a fresh zeroed set (pages the
                // allocator hands over untouched) replaces it, not a copy.
                v.bits = vec![0; need];
            }
            let r = self.scan_from(text, from, slots, &mut v, width);
            v.reset();
            v.shrink();
            r
        })
    }

    fn scan_from(&self, text: &str, from: usize, slots: &mut [Option<usize>], v: &mut Visited, width: usize) -> Option<()> {
        let hay = text.as_bytes();
        let mut at = from;
        {
            loop {
                // The next candidate start.
                let cand = match &self.prefix {
                    Some((lit, false)) => crate::scan::find(&hay[at..], lit).map(|i| at + i)?,
                    Some((lit, true)) => find_fold(&hay[at..], lit).map(|i| at + i)?,
                    None if self.nullable => at,
                    None => (at..hay.len()).find(|&i| self.first[hay[i] as usize] && text.is_char_boundary(i))?,
                };
                if self.run(text, cand, slots, v, width) {
                    return Some(());
                }
                if cand >= hay.len() {
                    return None;
                }
                at = next_char(text, cand);
            }
        }
    }

    /// Runs the program anchored at `start`.
    fn run(&self, text: &str, start: usize, slots: &mut [Option<usize>], visited: &mut Visited, width: usize) -> bool {
        let hay = text.as_bytes();
        let mut stack = std::mem::take(&mut visited.stack);
        stack.clear();
        stack.push(Frame::Step(0, start));
        let found = 'outer: loop {
        let Some(f) = stack.pop() else { break false };
        {
            let (mut pc, mut pos) = match f {
                Frame::Restore(s, v) => {
                    if s < slots.len() {
                        slots[s] = v;
                    }
                    continue;
                }
                Frame::Step(pc, pos) => (pc, pos),
            };
            loop {
                if self.memo && visited.test_set(pc * width + pos) {
                    break;
                }
                match &self.prog[pc] {
                    Inst::Byte(b) => {
                        if hay.get(pos) != Some(b) {
                            break;
                        }
                        pc += 1;
                        pos += 1;
                    }
                    Inst::ByteFold(b) => {
                        if hay.get(pos).map(u8::to_ascii_lowercase) != Some(*b) {
                            break;
                        }
                        pc += 1;
                        pos += 1;
                    }
                    Inst::Char(c) => match text[pos..].chars().next() {
                        Some(x) if x == *c => {
                            pc += 1;
                            pos += x.len_utf8();
                        }
                        _ => break,
                    },
                    Inst::CharFold(c) => match text[pos..].chars().next() {
                        Some(x) if fold_eq(x, *c) => {
                            pc += 1;
                            pos += x.len_utf8();
                        }
                        _ => break,
                    },
                    Inst::Class(i) => {
                        let Some(&b) = hay.get(pos) else { break };
                        if b < 0x80 {
                            if !self.classes[*i].test(b as char) {
                                break;
                            }
                            pos += 1;
                        } else {
                            let x = text[pos..].chars().next().unwrap_or('\0');
                            if !self.classes[*i].test(x) {
                                break;
                            }
                            pos += x.len_utf8();
                        }
                        pc += 1;
                    }
                    Inst::Any | Inst::AnyNl => match text[pos..].chars().next() {
                        Some(x) if x != '\n' || matches!(self.prog[pc], Inst::AnyNl) => {
                            pc += 1;
                            pos += x.len_utf8();
                        }
                        _ => break,
                    },
                    Inst::Split(a, b) => {
                        stack.push(Frame::Step(*b, pos));
                        pc = *a;
                    }
                    Inst::Jmp(a) => pc = *a,
                    Inst::Save(s) => {
                        if *s < slots.len() {
                            stack.push(Frame::Restore(*s, slots[*s]));
                            slots[*s] = Some(pos);
                        }
                        pc += 1;
                    }
                    Inst::Bol => {
                        if !(pos == 0 || self.flags.multi && hay[pos - 1] == b'\n') {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Eol => {
                        if !(pos == hay.len() || self.flags.multi && hay[pos] == b'\n') {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::WordB(want) => {
                        let before = text[..pos].chars().next_back();
                        let after = text[pos..].chars().next();
                        if (is_word(before) != is_word(after)) != *want {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Match => break 'outer true,
                }
            }
        }
        };
        visited.stack = stack;
        found
    }
}

fn next_char(text: &str, i: usize) -> usize {
    text[i..].chars().next().map_or(i + 1, |c| i + c.len_utf8())
}

fn fold_eq(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// ASCII case-insensitive substring search; `lit` is lowercase.
fn find_fold(hay: &[u8], lit: &[u8]) -> Option<usize> {
    if lit.is_empty() {
        return Some(0);
    }
    hay.windows(lit.len()).position(|w| w.iter().zip(lit).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

struct Compiler {
    prog: Vec<Inst>,
    classes: Vec<Class>,
    flags: Flags,
}

impl Compiler {
    fn push(&mut self, i: Inst) -> usize {
        self.prog.push(i);
        self.prog.len() - 1
    }
    fn check(&self) -> Result<(), Error> {
        if self.prog.len() > MAX_PROGRAM {
            return err(0, format!("pattern compiles to over {MAX_PROGRAM} instructions"));
        }
        Ok(())
    }
    fn node(&mut self, n: &Node) -> Result<(), Error> {
        self.check()?;
        match n {
            Node::Empty => {}
            Node::Lit(c) => {
                let fold = self.flags.fold && (c.is_alphabetic());
                let i = match (c.is_ascii(), fold) {
                    (true, true) => Inst::ByteFold(c.to_ascii_lowercase() as u8),
                    (true, false) => Inst::Byte(*c as u8),
                    (false, true) => Inst::CharFold(*c),
                    (false, false) => Inst::Char(*c),
                };
                self.push(i);
            }
            Node::Class(k) => {
                let mut k = k.clone();
                if self.flags.fold && !k.fold {
                    // A named-set class built outside the parser's fold.
                    k.fold = true;
                }
                self.classes.push(k);
                let i = self.classes.len() - 1;
                self.push(Inst::Class(i));
            }
            Node::Any => {
                let i = if self.flags.dotall { Inst::AnyNl } else { Inst::Any };
                self.push(i);
            }
            Node::Bol => {
                self.push(Inst::Bol);
            }
            Node::Eol => {
                self.push(Inst::Eol);
            }
            Node::WordB(w) => {
                self.push(Inst::WordB(*w));
            }
            Node::Group(inner, cap) => {
                if let Some(c) = cap {
                    self.push(Inst::Save(2 * c));
                }
                self.node(inner)?;
                if let Some(c) = cap {
                    self.push(Inst::Save(2 * c + 1));
                }
            }
            Node::Concat(items) => {
                for i in items {
                    self.node(i)?;
                }
            }
            Node::Alt(arms) => {
                let mut jumps = Vec::new();
                for (k, arm) in arms.iter().enumerate() {
                    if k + 1 < arms.len() {
                        let split = self.push(Inst::Split(0, 0));
                        self.node(arm)?;
                        jumps.push(self.push(Inst::Jmp(0)));
                        let next = self.prog.len();
                        self.prog[split] = Inst::Split(split + 1, next);
                    } else {
                        self.node(arm)?;
                    }
                }
                let end = self.prog.len();
                for j in jumps {
                    self.prog[j] = Inst::Jmp(end);
                }
            }
            Node::Repeat { node, min, max, greedy } => {
                for _ in 0..*min {
                    self.node(node)?;
                }
                match max {
                    None => {
                        let split = self.push(Inst::Split(0, 0));
                        self.node(node)?;
                        self.push(Inst::Jmp(split));
                        let end = self.prog.len();
                        self.prog[split] = if *greedy { Inst::Split(split + 1, end) } else { Inst::Split(end, split + 1) };
                    }
                    Some(m) => {
                        let mut splits = Vec::new();
                        for _ in *min..*m {
                            splits.push(self.push(Inst::Split(0, 0)));
                            self.node(node)?;
                        }
                        let end = self.prog.len();
                        for s in splits {
                            self.prog[s] = if *greedy { Inst::Split(s + 1, end) } else { Inst::Split(end, s + 1) };
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Adds the bytes a match of `n` can start with to `set`; returns true
/// when `n` can match the empty string (so what follows can start a match
/// too).
fn first_bytes(n: &Node, f: &Flags, set: &mut [bool; 256]) -> bool {
    match n {
        Node::Empty | Node::Bol | Node::Eol | Node::WordB(_) => true,
        Node::Lit(c) => {
            let mut buf = [0u8; 4];
            let add = |c: char, set: &mut [bool; 256]| {
                let mut b = [0u8; 4];
                set[c.encode_utf8(&mut b).as_bytes()[0] as usize] = true;
            };
            set[c.encode_utf8(&mut buf).as_bytes()[0] as usize] = true;
            if f.fold {
                c.to_lowercase().for_each(|l| add(l, set));
                c.to_uppercase().for_each(|u| add(u, set));
                if !c.is_ascii() {
                    // Folding can reach ASCII (the Kelvin sign) or other
                    // leads: allow every non-ASCII lead and both ASCII cases.
                    (0xC0..=0xFF).for_each(|b| set[b] = true);
                }
            }
            false
        }
        Node::Class(k) => {
            for b in 0u8..128 {
                if k.test(b as char) {
                    set[b as usize] = true;
                }
            }
            if k.non_ascii() || k.fold {
                (0xC0..=0xFF).for_each(|b| set[b] = true);
            }
            false
        }
        Node::Any => {
            for (b, s) in set.iter_mut().enumerate() {
                if b != b'\n' as usize || f.dotall {
                    *s = true;
                }
            }
            false
        }
        Node::Group(inner, _) => first_bytes(inner, f, set),
        Node::Concat(items) => {
            for i in items {
                if !first_bytes(i, f, set) {
                    return false;
                }
            }
            true
        }
        Node::Alt(arms) => {
            let mut any = false;
            for a in arms {
                any |= first_bytes(a, f, set);
            }
            any
        }
        Node::Repeat { node, min, .. } => first_bytes(node, f, set) || *min == 0,
    }
}

/// The literal bytes of a run of literal nodes (case-sensitive only).
fn literal_run(items: &[Node], f: &Flags) -> Vec<Vec<u8>> {
    let mut runs = Vec::new();
    let mut cur = String::new();
    for i in items {
        match i {
            Node::Lit(c) if !(f.fold && c.is_alphabetic()) => cur.push(*c),
            Node::Group(inner, _) if matches!(**inner, Node::Lit(_)) => {
                if let Node::Lit(c) = **inner {
                    if f.fold && c.is_alphabetic() {
                        runs.push(std::mem::take(&mut cur).into_bytes());
                    } else {
                        cur.push(c);
                    }
                }
            }
            Node::Bol | Node::Eol | Node::WordB(_) => {}
            Node::Repeat { node, min, .. } if *min >= 1 => {
                // At least one copy, but what follows may not be adjacent.
                if let Node::Lit(c) = **node {
                    if !(f.fold && c.is_alphabetic()) {
                        cur.push(c);
                    }
                }
                runs.push(std::mem::take(&mut cur).into_bytes());
            }
            _ => runs.push(std::mem::take(&mut cur).into_bytes()),
        }
    }
    runs.push(cur.into_bytes());
    runs
}

/// The longest literal every match must contain, case-sensitively.
fn required_literal(n: &Node, f: &Flags) -> Vec<u8> {
    let best = |runs: Vec<Vec<u8>>| runs.into_iter().max_by_key(Vec::len).unwrap_or_default();
    match n {
        Node::Concat(items) => {
            let mut cands = literal_run(items, f);
            for i in items {
                match i {
                    Node::Group(inner, _) => cands.push(required_literal(inner, f)),
                    Node::Repeat { node, min, .. } if *min >= 1 => cands.push(required_literal(node, f)),
                    _ => {}
                }
            }
            best(cands)
        }
        Node::Lit(c) if !(f.fold && c.is_alphabetic()) => c.to_string().into_bytes(),
        Node::Group(inner, _) => required_literal(inner, f),
        Node::Repeat { node, min, .. } if *min >= 1 => required_literal(node, f),
        _ => Vec::new(),
    }
}

/// A literal every match starts with (after zero-width assertions), and
/// whether it is ASCII-folded. Only patterns whose first consuming items
/// are plain literals have one.
fn prefix_literal(n: &Node, f: &Flags) -> Option<(Vec<u8>, bool)> {
    let items: Vec<&Node> = match n {
        Node::Concat(items) => items.iter().collect(),
        other => vec![other],
    };
    let mut lit = Vec::new();
    let mut started = false;
    for i in items {
        match i {
            Node::Bol | Node::WordB(_) if !started => continue,
            Node::Lit(c) if c.is_ascii() => {
                started = true;
                lit.push(if f.fold { c.to_ascii_lowercase() as u8 } else { *c as u8 });
            }
            _ => break,
        }
    }
    (!lit.is_empty()).then_some((lit, f.fold))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all<'t>(re: &str, text: &'t str) -> Vec<&'t str> {
        Regex::new(re).unwrap().find_iter(text).map(|m| m.as_str()).collect()
    }

    #[test]
    fn conformance_table() {
        // (pattern, text, every match left to right)
        let table: &[(&str, &str, &[&str])] = &[
            ("abc", "xabcabc", &["abc", "abc"]),
            (r"\bD-[0-9]{3,4}\b", "D-001 xD-002 D-12345 D-0042.", &["D-001", "D-0042"]),
            (r"\b[A-Z][A-Z0-9]+(?:-[A-Z][A-Z0-9]+)+\b", "FRAME-SCHED and A-B, X1-Y2-Z3 not Ab-Cd", &["FRAME-SCHED", "X1-Y2-Z3"]),
            (r"\bTASK-[0-9]+[a-z]?\b","TASK-28e TASK-13 TASK-9ab", &["TASK-28e", "TASK-13"]),
            ("a|ab", "ab", &["a"]),
            ("ab|a", "ab", &["ab"]),
            ("a*?b", "aaab", &["aaab"]),
            ("a+?", "aaa", &["a", "a", "a"]),
            ("a{2}", "aaaaa", &["aa", "aa"]),
            ("a{2,}", "aaaaa", &["aaaaa"]),
            ("a{0,2}b", "aaab", &["aab"]),
            ("x*", "ab", &["", "", ""]),
            ("^a", "aa", &["a"]),
            ("a$", "aa", &["a"]),
            (r"[^\[\]\n]+?\]", "x]y]", &["x]", "y]"]),
            (r"[\p{L}\p{N}_/-]+", "tag/é-1 ü", &["tag/é-1", "ü"]),
            (r"\d+", "a12b٣", &["12", "٣"]),
            (r"\s+", "a \t b", &[" \t "]),
            (r"\w+", "héllo wörld_1", &["héllo", "wörld_1"]),
            (r"\.\*\+\?\(\)\[\]\{\}\|\^\$", ".*+?()[]{}|^$", &[".*+?()[]{}|^$"]),
            ("[a-c-]+", "a-b-d", &["a-b-"]),
            ("[]a]+", "]a]b", &["]a]"]),
            (r"\Bb\B", "abc b", &["b"]),
            ("é+", "ééa", &["éé"]),
            (".", "a\nb", &["a", "b"]),
            ("(?s).", "\n", &["\n"]),
            ("(?m)^b$", "a\nb\nc", &["b"]),
            ("(?i)abc", "ABC aBc", &["ABC", "aBc"]),
            ("(?i)[a-c]+", "AbC", &["AbC"]),
            ("(?i)straße", "STRASSE straße STRAßE", &["straße", "STRAßE"]),
        ];
        for (re, text, want) in table {
            assert_eq!(all(re, text), *want, "pattern {re:?} on {text:?}");
        }
    }

    #[test]
    fn captures_and_names() {
        let re = Regex::new(r"(?:^|[^A-Za-z0-9_&/])(?P<id>#[0-9]{1,5})\b").unwrap();
        let got: Vec<&str> = re.captures_iter("PR #12, &#34; a#5 (#7) #123456").filter_map(|c| c.name("id").map(|m| m.as_str())).collect();
        assert_eq!(got, vec!["#12", "#7"]);
        assert_eq!(re.capture_names().collect::<Vec<_>>(), vec![None, Some("id")]);
        let c = Regex::new(r"^### P-(\d{1,6}) (.+) - ([a-z]+)\s*$").unwrap().captures("### P-12 alice - claim ").unwrap();
        assert_eq!((&c[1], &c[2], &c[3]), ("12", "alice", "claim"));
        let c = Regex::new(r"(a)|(b)").unwrap().captures("b").unwrap();
        assert!(c.get(1).is_none() && c.get(2).is_some());
        let m = Regex::new(r"<!-- x ([a-z]+) start (\{.*?\}) -->").unwrap().captures("<!-- x n start {\"a\":{}} --> <!-- x m start {} -->").unwrap();
        assert_eq!(&m[2], "{\"a\":{}}");
    }

    #[test]
    fn refusals_name_what_is_unsupported() {
        for (re, want) in [
            (r"(a)\1", "back-reference"),
            ("(?=a)", "look-ahead"),
            ("(?!a)", "look-ahead"),
            ("(?<=a)b", "look-behind"),
            ("(?<!a)b", "look-behind"),
            (r"\Aa", r"\A"),
            (r"a\z", r"\z"),
            (r"\x41", r"\x"),
            (r"\p{Greek}", "Unicode class"),
            ("a(?i)b", "flags after the start"),
            ("a++", "possessive"),
            ("a**", "repetition of a repetition"),
            ("(a", "unclosed ("),
            ("a)", "unmatched )"),
            ("[a", "unclosed ["),
            ("[z-a]", "backwards"),
            ("*a", "nothing to repeat"),
            ("a{2,1}", "max below min"),
            ("a{5000}", "repetition over"),
            ("[[:alpha:]]", "POSIX"),
            (r"\bx*\b+", "zero-width"),
        ] {
            let e = Regex::new(re).expect_err(re);
            assert!(e.msg.contains(want), "{re:?}: {} lacks {want:?}", e.msg);
        }
    }

    #[test]
    fn no_pattern_blows_up() {
        let re = Regex::new("(a*)*b").unwrap();
        let text = "a".repeat(5000);
        assert!(!re.is_match(&text));
        let re = Regex::new("(x+x+)+y").unwrap();
        assert!(!re.is_match(&"x".repeat(3000)));
    }

    #[test]
    fn escape_round_trips() {
        let s = "a.b*c(d)[e]{f}|g^h$i\\j#k&l-m~";
        let re = Regex::new(&escape(s)).unwrap();
        assert_eq!(re.find(&format!("xx{s}yy")).map(|m| m.as_str()), Some(s));
    }

    #[test]
    fn builder_folds_case() {
        let re = RegexBuilder::new(r"tk-\d+").case_insensitive(true).build().unwrap();
        assert_eq!(re.find("see TK-12").map(|m| m.as_str()), Some("TK-12"));
        assert!(re.is_match("Tk-1"));
    }
}
