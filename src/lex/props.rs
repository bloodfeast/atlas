//! Each hand-written scanner against the regex it replaced: random text
//! built from the pattern's own tokens and hostile characters (Unicode
//! numbers that are not ASCII digits, case-folding traps, every line
//! ending, NUL, wide characters), the empty string, each token alone and
//! repeated, and lines of 64 KiB. Every group span must be the same.
//!
//! `cargo test --release --lib lex::props::timings -- --ignored --nocapture`
//! prints each site's time with the old regex and with the scanner.

use crate::lex::Span;
use crate::regex::Regex;

/// How many random texts each site is checked on.
const CASES: usize = 3000;

/// The size of a huge line.
const HUGE: usize = 64 * 1024;

/// Characters that trip scanners: Unicode numbers that are not ASCII
/// digits (`\d` takes them), letters whose case maps change length or
/// reach ASCII, spaces that are not ASCII, line endings, NUL.
const HOSTILE: &[&str] = &[
    "\u{663}", "\u{216b}", "\u{bd}", "\u{ff13}", "\u{e9}", "\u{130}", "\u{212a}", "\u{df}", "\u{a0}", "\u{2028}", "\u{3000}",
    "\u{85}", "\u{1f980}", "\u{65e5}", "e\u{301}", "\r\n", "\n", "\r", "\t", " ", "  ", "\0", "\u{feff}", "a", "Z", "_", "0",
    "9", "-", ":", ".", "/", "\\", "#", "*", "\"", "'", "`", "(", ")", "[", "]", "{", "}", "<", ">", "|", "!", "@", ",", ";",
];

/// A seeded xorshift64* generator.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    /// A random scalar value, mostly ASCII.
    fn char(&mut self) -> char {
        match self.below(8) {
            0..=4 => (0x20 + self.below(0x5f) as u8) as char,
            5 => char::from_u32(0x80 + self.below(0x780) as u32).unwrap_or('?'),
            6 => char::from_u32(0x800 + self.below(0xD000 - 0x800) as u32).unwrap_or('?'),
            _ => char::from_u32(0x10000 + self.below(0x10000) as u32).unwrap_or('?'),
        }
    }
    /// Up to `max` pieces: the site's tokens, hostile characters, random
    /// characters.
    fn text(&mut self, tokens: &[&str], max: usize) -> String {
        let n = self.below(max + 1);
        let mut s = String::new();
        for _ in 0..n {
            match self.below(10) {
                0..=5 if !tokens.is_empty() => s.push_str(tokens[self.below(tokens.len())]),
                6..=8 => s.push_str(HOSTILE[self.below(HOSTILE.len())]),
                _ => s.push(self.char()),
            }
        }
        s
    }
    /// A char boundary of `s`.
    fn cut(&mut self, s: &str) -> usize {
        let mut i = self.below(s.len() + 1);
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
    /// One of `examples` with up to three edits: a piece inserted, a span
    /// deleted, or a span replaced.
    fn edited(&mut self, examples: &[&str], tokens: &[&str]) -> String {
        let mut s = examples[self.below(examples.len())].to_string();
        for _ in 0..self.below(4) {
            let (a, b) = {
                let a = self.cut(&s);
                let b = self.cut(&s);
                (a.min(b), a.max(b))
            };
            let mut e = b.min(a + 4);
            while !s.is_char_boundary(e) {
                e -= 1;
            }
            let piece = self.text(tokens, 2);
            match self.below(3) {
                0 => s.insert_str(a, &piece),
                1 => s.replace_range(a..e, ""),
                _ => s.replace_range(a..e, &piece),
            }
        }
        s
    }
}

/// What a site's scanner answers, and how the old regex is asked the same.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    /// Every match (`captures_iter`).
    All(fn(&str) -> Vec<Vec<Span>>),
    /// The leftmost match (`captures`).
    One(fn(&str) -> Option<Vec<Span>>),
    /// Whether it matches (`is_match`).
    Bool(fn(&str) -> bool),
}

/// One replaced pattern.
pub(crate) struct Site {
    /// Where it was.
    pub name: &'static str,
    /// The old pattern.
    pub pattern: String,
    /// Pieces its matches are made of, for the generator.
    pub tokens: &'static [&'static str],
    /// Whole lines it matches, which the generator edits.
    pub examples: &'static [&'static str],
    /// The scanner.
    pub mode: Mode,
}

/// Group spans as a vector.
pub(crate) fn v<const N: usize>(c: [Span; N]) -> Vec<Span> {
    c.to_vec()
}

fn old_caps(re: &Regex, c: &crate::regex::Captures<'_>) -> Vec<Span> {
    (0..re.capture_names().count()).map(|i| c.get(i).map(|m| (m.start(), m.end()))).collect()
}

/// The old regex's answer, in the scanner's shape.
fn old(re: &Regex, mode: Mode, s: &str) -> Vec<Vec<Span>> {
    match mode {
        Mode::All(_) => re.captures_iter(s).map(|c| old_caps(re, &c)).collect(),
        Mode::One(_) => re.captures(s).map(|c| old_caps(re, &c)).into_iter().collect(),
        Mode::Bool(_) => if re.is_match(s) { vec![Vec::new()] } else { Vec::new() },
    }
}

/// The scanner's answer.
fn new(mode: Mode, s: &str) -> Vec<Vec<Span>> {
    match mode {
        Mode::All(f) => f(s),
        Mode::One(f) => f(s).into_iter().collect(),
        Mode::Bool(f) => if f(s) { vec![Vec::new()] } else { Vec::new() },
    }
}

/// The inputs a site is checked on.
fn inputs(site: &Site, seed: u64) -> Vec<String> {
    let mut r = Rng::new(seed);
    let mut out = vec![String::new()];
    for t in site.tokens.iter().chain(HOSTILE) {
        out.push(t.to_string());
        out.push(t.repeat(1 + HUGE / t.len().max(1)));
    }
    for e in site.examples {
        out.push(e.to_string());
    }
    for i in 0..CASES {
        let s = if site.examples.is_empty() || i % 2 == 0 { r.text(site.tokens, 4 + i % 40) } else { r.edited(site.examples, site.tokens) };
        out.push(s.replace('\n', "\r\n"));
        out.push(s);
    }
    for _ in 0..4 {
        let mut s = String::new();
        while s.len() < HUGE {
            s.push_str(&r.text(site.tokens, 64));
        }
        out.push(s.replace('\n', " "));
        if let Some(e) = site.examples.first() {
            out.push(format!("{e}{}", s.replace('\n', " ")));
        }
        out.push(s);
    }
    out
}

/// Ordinary lines for the timings: half generated, half edited examples.
fn corpus(site: &Site, seed: u64) -> Vec<String> {
    let mut r = Rng::new(seed);
    (0..20_000)
        .map(|i| if site.examples.is_empty() || i % 2 == 0 { r.text(site.tokens, 4 + i % 40) } else { r.edited(site.examples, site.tokens) })
        .collect()
}

fn check(site: &Site, seed: u64) {
    let re = Regex::new(&site.pattern).unwrap_or_else(|e| panic!("{}: {e}", site.name));
    let mut hits = 0;
    for s in inputs(site, seed) {
        re.check_text(s.len()).unwrap_or_else(|e| panic!("{}: the old regex refuses a {}-byte input: {e}", site.name, s.len()));
        let (a, b) = (old(&re, site.mode, &s), new(site.mode, &s));
        if a != b {
            let shown: String = s.chars().take(300).collect();
            panic!("{}: scanner differs from {:?}\ninput ({} bytes): {shown:?}\nregex:   {a:?}\nscanner: {b:?}", site.name, site.pattern, s.len());
        }
        hits += usize::from(!a.is_empty());
    }
    // The generator must reach matches, not only misses.
    assert!(hits >= CASES / 20, "{}: only {hits} inputs match; give the site better tokens", site.name);
}

/// Every replaced site.
pub(crate) fn sites() -> Vec<Site> {
    let mut v = Vec::new();
    v.extend(crate::markdown::lex_sites());
    v.extend(crate::chunk::lex_sites());
    v.extend(crate::discussion::lex_sites());
    v.extend(crate::index::lex_sites());
    v.extend(crate::registry::lex_sites());
    v.extend(crate::recall::lex_sites());
    v.extend(crate::digest::lex_sites());
    v.extend(crate::langs::lex_sites());
    v
}

#[test]
fn scanners_match_their_old_patterns() {
    for (k, site) in sites().iter().enumerate() {
        check(site, 0xA71A5 + k as u64);
    }
}

/// The recall-check truth scan over this crate's own Rust files (in
/// memory, so only the scan is timed): the old regex, the scanner on one
/// thread, and the scanner across threads ([`crate::lex::par_map`]).
#[test]
#[ignore = "timings: run in release with --nocapture"]
fn truth_scan_timing() {
    use std::time::Instant;
    fn walk(d: &std::path::Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(std::fs::read_to_string(&p).unwrap_or_default());
            }
        }
    }
    let mut files = Vec::new();
    for d in ["src", "tests", "xtask"] {
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(d), &mut files);
    }
    // Several copies: a root the size of a real workspace.
    let files: Vec<String> = (0..8).flat_map(|_| files.iter().cloned()).collect();
    let re = Regex::new(&crate::recall::lex_sites()[0].pattern).unwrap();
    let count = |t: &String, f: &dyn Fn(&str) -> bool| t.lines().filter(|l| f(l)).count();
    let best = |f: &dyn Fn() -> usize| {
        let mut b = f64::MAX;
        let mut n = 0;
        for _ in 0..5 {
            let t = Instant::now();
            n = f();
            b = b.min(t.elapsed().as_secs_f64());
        }
        (b, n)
    };
    let (a, na) = best(&|| files.iter().map(|t| count(t, &|l| re.captures(l).is_some())).sum());
    let (b, nb) = best(&|| files.iter().map(|t| count(t, &|l| crate::recall::rust_def(l).is_some())).sum());
    let (c, nc) = best(&|| crate::lex::par_map(&files, |t| count(t, &|l| crate::recall::rust_def(l).is_some())).into_iter().sum());
    assert!(na == nb && nb == nc, "{na} {nb} {nc}");
    let mb = files.iter().map(String::len).sum::<usize>() as f64 / 1e6;
    println!("truth scan, {} files, {mb:.1} MB, {na} definitions", files.len());
    println!("| old regex | {:.1} ms |\n| scanner, 1 thread | {:.1} ms ({:.1}x) |\n| scanner, par_map | {:.1} ms ({:.1}x) |", a * 1e3, b * 1e3, a / b, c * 1e3, a / c);
}

#[test]
#[ignore = "timings: run in release with --nocapture"]
fn timings() {
    use std::time::Instant;
    println!("| site | old regex | scanner | speedup |");
    println!("|---|---:|---:|---:|");
    for (k, site) in sites().iter().enumerate() {
        let re = Regex::new(&site.pattern).unwrap();
        let corpus = corpus(site, 0x7135 + k as u64);
        let time = |f: &dyn Fn(&str) -> usize| {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                let n: usize = corpus.iter().map(|s| f(s)).sum();
                std::hint::black_box(n);
                best = best.min(t.elapsed().as_secs_f64());
            }
            best
        };
        let (a, b) = match site.mode {
            Mode::Bool(f) => (time(&|s| re.is_match(s) as usize), time(&|s| f(s) as usize)),
            m => (time(&|s| old(&re, m, s).len()), time(&|s| new(m, s).len())),
        };
        println!("| {} | {:.2} ms | {:.2} ms | {:.1}x |", site.name, a * 1e3, b * 1e3, a / b);
    }
}
