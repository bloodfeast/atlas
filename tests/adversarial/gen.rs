//! A deterministic generator for property and fuzz tests: xorshift64*, and
//! builders for hostile text and bytes. No external crates.
//!
//! Every fuzz loop runs a fixed number of cases in `cargo test`; the
//! `--ignored` deep tests run [`DEEP`] times as many. `ATLAS_FUZZ_SEED`
//! moves every loop to another seed (a deep run with a fresh seed explores
//! new inputs); a failure prints the seed and the case so it reproduces.
#![allow(dead_code)]

use std::panic::{catch_unwind, AssertUnwindSafe};

/// How many times longer a deep (`--ignored`) loop runs.
pub const DEEP: usize = 200;

/// A seeded xorshift64* generator.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator for `stream`, offset by `ATLAS_FUZZ_SEED` when set.
    pub fn new(stream: u64) -> Rng {
        let base = std::env::var("ATLAS_FUZZ_SEED").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0x9E37_79B9_7F4A_7C15);
        let mut r = Rng(base ^ stream.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
        for _ in 0..4 {
            r.next();
        }
        r
    }
    /// The next 64 random bits.
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// A number below `n` (0 when `n` is 0).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    /// True with probability `num/den`.
    pub fn chance(&mut self, num: usize, den: usize) -> bool {
        self.below(den) < num
    }
    /// One element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
    /// `n` random bytes.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
    /// A random `char` (any scalar value, weighted to ASCII).
    pub fn char(&mut self) -> char {
        match self.below(10) {
            0..=5 => (0x20 + self.below(0x5f) as u8) as char,
            6 => self.below(0x20) as u8 as char,
            7 => char::from_u32(0x80 + self.below(0x780) as u32).unwrap_or('?'),
            8 => char::from_u32(0x800 + self.below(0xD000 - 0x800) as u32).unwrap_or('?'),
            _ => char::from_u32(0x10000 + self.below(0x10FFFF - 0x10000) as u32).unwrap_or('?'),
        }
    }
    /// Text built from `tokens` and random characters, up to `max` pieces.
    pub fn text_from(&mut self, tokens: &[&str], max: usize) -> String {
        let n = self.below(max + 1);
        let mut s = String::new();
        for _ in 0..n {
            if self.chance(3, 4) {
                s.push_str(self.pick(tokens));
            } else {
                s.push(self.char());
            }
        }
        s
    }
    /// Mutates bytes in place: flips, inserts, deletes, truncates, repeats.
    pub fn mutate(&mut self, b: &mut Vec<u8>) {
        let k = 1 + self.below(4);
        for _ in 0..k {
            match self.below(6) {
                0 if !b.is_empty() => {
                    let i = self.below(b.len());
                    b[i] ^= 1 << self.below(8);
                }
                1 if !b.is_empty() => {
                    let i = self.below(b.len());
                    b[i] = self.next() as u8;
                }
                2 => {
                    let i = self.below(b.len() + 1);
                    let v = self.next() as u8;
                    b.insert(i, v);
                }
                3 if !b.is_empty() => {
                    let i = self.below(b.len());
                    b.remove(i);
                }
                4 => {
                    let n = self.below(b.len() + 1);
                    b.truncate(n);
                }
                _ if !b.is_empty() => {
                    let i = self.below(b.len());
                    let j = (i + 1 + self.below(16)).min(b.len());
                    let piece = b[i..j].to_vec();
                    let at = self.below(b.len() + 1);
                    b.splice(at..at, piece);
                }
                _ => {}
            }
        }
    }
}

/// Text that is hostile to hand-written parsers: multi-byte characters
/// whose lowercase has a different length, combining marks, every line
/// ending, NUL, a byte order mark, bidi controls, and Markdown, YAML,
/// regex, glob, FTS5 and SQL syntax.
pub const HOSTILE: &[&str] = &[
    "\n", "\r\n", "\r", "\n\n", "\t", " ", "  ", "\0", "\u{feff}", "\u{200b}", "\u{202e}", "\u{2028}", "\u{85}",
    "\u{130}", "\u{212a}", "\u{1e9e}", "\u{df}", "\u{3a3}", "e\u{301}", "\u{1f980}", "\u{65e5}\u{672c}", "\u{fffd}",
    "#", "## ", "###### ", "####### ", "---", "---\n", "...", "```", "~~~", "    ", "- ", "* ", "1. ", "> ", "|", "| a | b |",
    "[[", "]]", "[[a|b]]", "[[a#b]]", "![[x.png]]", "[t](u)", "[t](<u v>)", "(", ")", "[", "]", "{", "}", "<", ">",
    "<!-- ", " -->", "<!-- vault-mcp:index x start {} -->", "key: value", "key:", ": ", "- [a, b]", "tags: [x, y]", "\"", "'", "`",
    "\\", "/", "..", ".", "*", "**", "?", "+", "^", "$", "(?i)", "(?:", "\\b", "\\d", "\\p{L}", "{2,5}", "{1000}",
    "AND", "OR", "NOT", "NEAR(", "col:", "'; DROP TABLE notes; --", "%", "_", "obsidian://open?vault=a&file=b%2Fc", "%ZZ", "%",
    "TK-001", "PI-002", "2026-10-30", "9999-99-99", "### P-1 a - claim", "*2026-01-01T00:00:00Z | sha: 0123456789abcdef*",
    "fn x() {", "impl A for B {", "}", "/*", "*/", "//", "r#\"", "\"#", "'a", "b'x'", "@group(0)", "fn main() -> u32 {",
];

/// Runs `f` on `n` generated cases; on a panic, fails naming the case.
pub fn each<T: std::fmt::Debug>(stream: u64, n: usize, mut make: impl FnMut(&mut Rng) -> T, mut f: impl FnMut(&T)) {
    let mut r = Rng::new(stream);
    for i in 0..n {
        let case = make(&mut r);
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| f(&case))) {
            let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
            let mut shown = format!("{case:?}");
            if shown.len() > 2000 {
                let mut cut = 2000;
                while !shown.is_char_boundary(cut) {
                    cut -= 1;
                }
                shown.truncate(cut);
                shown.push_str("...");
            }
            panic!("stream {stream} case {i} panicked: {msg}\ninput: {shown}");
        }
    }
}

/// True when `i` is a char boundary of `s` (an index the parser returned
/// must be one).
pub fn boundary(s: &str, i: usize) -> bool {
    i <= s.len() && s.is_char_boundary(i)
}
