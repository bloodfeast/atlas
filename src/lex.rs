//! Pieces shared by atlas's hand-written line scanners: the fixed patterns
//! of the Markdown parser, the chunker, discussions, index markers, the
//! registry, recall's ground truth and the output digests. Each scanner
//! matches exactly what the [`crate::regex`] pattern it replaced matched
//! (the property tests in `lex/props.rs` hold the old patterns and check
//! that), so the character classes here are that engine's: `\d` is a
//! Unicode number, `\w` a Unicode letter, number or `_`, `\s`
//! `char::is_whitespace`, and `\b` a boundary by `\w`.
//!
//! The owned regex engine stays for what users write: `search mode=regex`,
//! `grep`, `lookup pattern`, and the configuration's identifier and hook
//! patterns.

/// A capture: its byte span, `None` when the group did not take part.
pub type Span = Option<(usize, usize)>;

/// A set of ASCII bytes as a 128-bit bitset: one shift and mask per test.
#[derive(Clone, Copy)]
pub struct Ascii(u128);

impl Ascii {
    /// The set of `bytes` and of the inclusive `ranges`.
    pub const fn new(bytes: &[u8], ranges: &[(u8, u8)]) -> Ascii {
        let mut m = 0u128;
        let mut i = 0;
        while i < bytes.len() {
            m |= 1 << bytes[i];
            i += 1;
        }
        let mut r = 0;
        while r < ranges.len() {
            let mut c = ranges[r].0;
            while c <= ranges[r].1 {
                m |= 1 << c;
                c += 1;
            }
            r += 1;
        }
        Ascii(m)
    }

    /// Whether byte `b` is in the set (never for a non-ASCII byte).
    #[inline(always)]
    pub const fn has(self, b: u8) -> bool {
        b < 128 && (self.0 >> b) & 1 == 1
    }
}

/// ASCII `\w`.
pub const WORD: Ascii = Ascii::new(b"_", &[(b'a', b'z'), (b'A', b'Z'), (b'0', b'9')]);
/// ASCII `\s` (`char::is_whitespace`: tab to carriage return, space).
pub const SPACE: Ascii = Ascii::new(b" ", &[(b'\t', b'\r')]);
/// `[A-Za-z_]`.
pub const IDENT_START: Ascii = Ascii::new(b"_", &[(b'a', b'z'), (b'A', b'Z')]);
/// `[A-Za-z0-9_]`.
pub const IDENT: Ascii = WORD;

/// `\d`: an ASCII digit or any other Unicode number.
#[inline]
pub fn digit(c: char) -> bool {
    c.is_ascii_digit() || (!c.is_ascii() && c.is_numeric())
}

/// `\w`: a Unicode letter or number, or `_`.
#[inline]
pub fn word(c: char) -> bool {
    if c.is_ascii() {
        WORD.has(c as u8)
    } else {
        c.is_alphanumeric()
    }
}

/// `\s`: Unicode white space.
#[inline]
pub fn space(c: char) -> bool {
    if c.is_ascii() {
        SPACE.has(c as u8)
    } else {
        c.is_whitespace()
    }
}

/// `[A-Za-z_]`.
#[inline]
pub fn ident_start(c: char) -> bool {
    c.is_ascii() && IDENT_START.has(c as u8)
}

/// `[A-Za-z0-9_]`.
#[inline]
pub fn ident(c: char) -> bool {
    c.is_ascii() && IDENT.has(c as u8)
}

/// The end of the run of bytes in an ASCII set from byte `i`.
#[inline]
pub fn run_ascii(s: &str, i: usize, set: Ascii) -> usize {
    let b = s.as_bytes();
    let mut j = i;
    while j < b.len() && set.has(b[j]) {
        j += 1;
    }
    j
}

/// The first byte `b` at or after byte `from`.
#[inline]
pub fn find_byte(s: &str, from: usize, b: u8) -> Option<usize> {
    crate::scan::memchr(b, s.as_bytes().get(from..)?).map(|k| from + k)
}

/// The first byte `a` or `b` at or after byte `from`.
#[inline]
pub fn find2(s: &str, from: usize, a: u8, b: u8) -> Option<usize> {
    crate::scan::memchr2(a, b, s.as_bytes().get(from..)?).map(|k| from + k)
}

/// The first `needle` at or after byte `from` (a vector byte search for
/// its first byte, then a compare).
#[inline]
pub fn find_str(s: &str, from: usize, needle: &str) -> Option<usize> {
    crate::scan::find(s.as_bytes().get(from..)?, needle.as_bytes()).map(|k| from + k)
}

/// The end of the line holding byte `from`: its `\n`, or the text's end.
#[inline]
pub fn line_end(s: &str, from: usize) -> usize {
    find_byte(s, from, b'\n').unwrap_or(s.len())
}

/// The character at byte `i`, if any.
#[inline]
pub fn at(s: &str, i: usize) -> Option<char> {
    s.get(i..).and_then(|t| t.chars().next())
}

/// The character just before byte `i`, if any.
#[inline]
pub fn before(s: &str, i: usize) -> Option<char> {
    s.get(..i).and_then(|t| t.chars().next_back())
}

/// `\b` at byte `i`: a `\w` character on exactly one side.
#[inline]
pub fn boundary(s: &str, i: usize) -> bool {
    before(s, i).is_some_and(word) != at(s, i).is_some_and(word)
}

/// The end of the longest run of characters `f` accepts, from byte `i`.
#[inline]
pub fn run(s: &str, i: usize, f: impl Fn(char) -> bool) -> usize {
    // ASCII bytes are tested as they are; only a non-ASCII byte is decoded.
    let b = s.as_bytes();
    let mut j = i;
    while j < b.len() {
        let c = b[j];
        if c < 0x80 {
            if !f(c as char) {
                return j;
            }
            j += 1;
        } else {
            let ch = s[j..].chars().next().expect("a char boundary");
            if !f(ch) {
                return j;
            }
            j += ch.len_utf8();
        }
    }
    b.len()
}

/// [`run`] of at most `max` characters: the end and how many it took.
#[inline]
pub fn run_max(s: &str, i: usize, max: usize, f: impl Fn(char) -> bool) -> (usize, usize) {
    let b = s.as_bytes();
    let (mut j, mut n) = (i, 0);
    while j < b.len() && n < max {
        let ch = if b[j] < 0x80 { b[j] as char } else { s[j..].chars().next().expect("a char boundary") };
        if !f(ch) {
            break;
        }
        n += 1;
        j += ch.len_utf8();
    }
    (j, n)
}

/// `\s*` from byte `i`: where it ends.
#[inline]
pub fn spaces(s: &str, i: usize) -> usize {
    run(s, i, space)
}

/// `\s+` from byte `i`: where it ends, if it took one.
#[inline]
pub fn spaces1(s: &str, i: usize) -> Option<usize> {
    let j = spaces(s, i);
    (j > i).then_some(j)
}

/// The end of literal `lit` at byte `i`, if it is there.
#[inline]
pub fn lit(s: &str, i: usize, lit: &str) -> Option<usize> {
    s.as_bytes().get(i..i + lit.len()).filter(|b| *b == lit.as_bytes()).map(|_| i + lit.len())
}

/// The end of the first of `words` at byte `i`, with the word.
#[inline]
pub fn one_of<'w>(s: &str, i: usize, words: &[&'w str]) -> Option<(usize, &'w str)> {
    words.iter().find_map(|w| lit(s, i, w).map(|e| (e, *w)))
}

/// The end of a run of one or more characters `f` accepts, from byte `i`.
#[inline]
pub fn run1(s: &str, i: usize, f: impl Fn(char) -> bool) -> Option<usize> {
    let j = run(s, i, f);
    (j > i).then_some(j)
}

/// The byte after the character at `i` (`i + 1` at the end), as the regex
/// engine steps past an empty match.
#[inline]
pub fn next_char(s: &str, i: usize) -> usize {
    at(s, i).map_or(i + 1, |c| i + c.len_utf8())
}

/// Every non-overlapping match, left to right, as `captures_iter` finds
/// them: `find(s, from)` is the leftmost match starting at or after `from`,
/// group 0 first.
pub fn all<const N: usize>(s: &str, mut find: impl FnMut(&str, usize) -> Option<[Span; N]>) -> Vec<[Span; N]> {
    let mut out = Vec::new();
    let mut from = 0;
    while from <= s.len() {
        let Some(c) = find(s, from) else { break };
        let Some((a, b)) = c[0] else { break };
        from = if a == b { next_char(s, b) } else { b };
        out.push(c);
    }
    out
}

/// The first character a predicate accepts at or after a byte, with the
/// last answer kept: a scanner that asks again from inside the span it
/// already searched gets it without searching again, so a line of
/// repeated openers is not searched once per opener.
pub struct Next<F> {
    /// The search: the first stop at or after a byte, or the text's length.
    f: F,
    /// The last search: (from, found at). Nothing in `from..at` matched.
    last: (usize, usize),
}

impl<F: Fn(&str, usize) -> usize> Next<F> {
    /// A cached search `f`.
    pub fn new(f: F) -> Next<F> {
        Next { f, last: (usize::MAX, 0) }
    }

    /// The first stop at or after byte `x` (a char boundary), or the
    /// text's length.
    pub fn find(&mut self, s: &str, x: usize) -> usize {
        if !(self.last.0 <= x && x <= self.last.1) {
            self.last = (x, (self.f)(s, x));
        }
        self.last.1
    }
}

/// A [`Next`] search for the first byte `a` or `b` (vectorised).
pub fn stop2(a: u8, b: u8) -> impl Fn(&str, usize) -> usize {
    move |s, x| find2(s, x, a, b).unwrap_or(s.len())
}

/// `f` over `items` on up to 16 scoped threads (one when there are few
/// items), the results in item order: a scan over many files gives the
/// same answer as a serial one.
pub fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let n = std::thread::available_parallelism().map_or(1, |n| n.get()).min(16);
    if n <= 1 || items.len() < 2 * n {
        return items.iter().map(f).collect();
    }
    let f = &f;
    std::thread::scope(|s| {
        let hs: Vec<_> = items.chunks(items.len().div_ceil(n)).map(|c| s.spawn(move || c.iter().map(f).collect::<Vec<R>>())).collect();
        hs.into_iter().flat_map(|h| h.join().expect("a scan thread panicked")).collect()
    })
}

/// The `path:line` of `[^\s]+:\d+:\d+` at byte `i` (no end anchor): the
/// longest path whose `:line:column` follows within its non-space run.
/// Returns where the path ends (its `:`) and where the line ends.
pub fn path_line_col(s: &str, i: usize) -> Option<(usize, usize)> {
    let end = run(s, i, |c| !space(c));
    let b = s.as_bytes();
    let mut x = end;
    while x > i {
        x -= 1;
        if b[x] != b':' || x == i {
            continue;
        }
        if let Some(d) = run1(s, x + 1, digit) {
            if b.get(d) == Some(&b':') && at(s, d + 1).is_some_and(digit) {
                return Some((x, d));
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod props;
