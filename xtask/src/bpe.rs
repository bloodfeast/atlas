//! A token count for the development tasks: byte-level BPE over a small
//! vocabulary of our own, times one fitted factor. It is an estimate of Claude's
//! context tokens, not a count of them: Claude's vocabulary is not published.
//!
//! The vocabulary (`xtask/vocab/vocab-500.txt`) is 500 merges trained by
//! `xtask/vocab/train.py` on the permissively licensed public text
//! `xtask/vocab/manifest.json` names, and on paths and ids generated from it: one
//! merged token per line in rank order, `\xNN` for anything but printable ASCII
//! and for a backslash. `xtask/vocab/DECLARED.md` holds the declarations it was
//! scored under. It lives in xtask and nothing ships.
//!
//! The split is [`piece_len`], on bytes, every byte of 0x80 and up counted as a
//! letter, so it needs no Unicode table. [`form_suffix`] is one rule beside it:
//! the numbers ending an absolute `path:line:col` enter no merge.
//!
//! [`FACTOR`] is the median real count over this count on six card-shaped texts
//! (Claude Opus 5.5, counted once each with `claude -p`). The other classes'
//! factors are in the manifest, all of them for that model alone. Median and
//! worst absolute error held out, in percent, against a flat bytes-per-token
//! ratio per class (journal ATLAS-V03:0304):
//!
//! | Class, texts | this count | flat |
//! |---|---|---|
//! | prose, 92 transcript rows (worst: p90) | 5.58, 10.75 | 20.75, 28.07 |
//! | code, 116 transcript rows (worst: p90) | 6.22, 10.68 | 15.46, 22.67 |
//! | ids, 12 | 4.08, 21.84 | 8.59, 37.72 |
//! | paths, 20 | 5.46, 16.99 | 8.63, 21.29 |
//! | card, 6 | 1.04, 1.97 | 0.88, 3.16 |
//!
//! Paths miss the declared limit (16.99 against 14.19, two thirds of flat's worst
//! at flat's refit factor) and pass it at flat's first factor (23.90); the project
//! owner accepted that after seeing it. The card is judged at 10% median only, also
//! by his ruling after seeing the result, and flat is lower on its median.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Real tokens per vocabulary token, on text shaped like the resume card.
pub const FACTOR: f64 = 0.7277;

/// What the count is, for anything that prints one.
pub const WHAT: &str = "a public vocabulary times a fitted factor, not real tokens";

/// A piece is merged in blocks of this many bytes: the merge is quadratic, and
/// no token of the vocabulary comes near this long.
const BLOCK: usize = 256;

const VOCAB: &str = include_str!("../vocab/vocab-500.txt");

fn unescape(line: &str) -> Vec<u8> {
    let b = line.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            let hex = line.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok());
            out.push(hex.expect("vocabulary: a bad escape"));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// The merged tokens by rank, built on the first count.
fn ranks() -> &'static HashMap<Vec<u8>, u32> {
    static R: OnceLock<HashMap<Vec<u8>, u32>> = OnceLock::new();
    R.get_or_init(|| VOCAB.lines().enumerate().map(|(i, l)| (unescape(l), i as u32)).collect())
}

/// Tokens of one block: merge the lowest-ranked adjacent pair until none is in
/// the vocabulary.
fn merge(piece: &[u8], ranks: &HashMap<Vec<u8>, u32>, starts: &mut Vec<usize>) -> usize {
    starts.clear();
    starts.extend(0..=piece.len());
    while starts.len() > 2 {
        let best = (0..starts.len() - 2).filter_map(|i| ranks.get(&piece[starts[i]..starts[i + 2]]).map(|&r| (r, i))).min();
        let Some((_, i)) = best else { break };
        starts.remove(i + 1);
    }
    starts.len() - 1
}

fn is_letter(b: u8) -> bool {
    b.is_ascii_alphabetic() || b >= 0x80
}

fn is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_punct(b: u8) -> bool {
    !is_letter(b) && !is_digit(b) && !is_space(b)
}

/// The length of the piece at the start of `s` (not empty). `SPLITS["lead"]` in
/// `train.py` is the pattern, and the trainer's strings test that the two agree:
/// a run of one class with one leading space, digits three at a time, and the
/// punctuation just before letters leading them (`/src`, `.rs`).
fn piece_len(s: &[u8]) -> usize {
    let run = |from: usize, f: fn(u8) -> bool| from + s[from..].iter().take_while(|&&b| f(b)).count();
    let from = usize::from(s[0] == b' ');
    let letters = run(from, is_letter);
    if letters > from {
        return letters;
    }
    if is_punct(s[0]) {
        let led = run(1, is_letter);
        if led > 1 {
            return led;
        }
    }
    let digits = s[from..].iter().take(3).take_while(|&&b| is_digit(b)).count();
    if digits > 0 {
        return from + digits;
    }
    let punct = run(from, is_punct);
    if punct > from {
        // The run's last byte leads the letters after it, if the run has another.
        let leads = punct - from > 1 && s.get(punct).copied().is_some_and(is_letter);
        return punct - usize::from(leads);
    }
    let space = run(0, is_space);
    if let Some(i) = s[..space].iter().rposition(|&b| b == b'\r' || b == b'\n') {
        return i + 1;
    }
    // Spaces before text leave the last one to lead it.
    let leaves = space > 1 && space < s.len() && s[space - 1] == b' ';
    space - usize::from(leaves)
}

/// The start and the digit lengths of the suffix when `word` (a maximal run with
/// no whitespace) has the `abs:line:col` form: it starts at a root that needs no
/// working directory and ends in `:line:col`. `FORM` in `train.py`.
fn form_suffix(word: &[u8]) -> Option<(usize, usize, usize)> {
    let root = match word {
        [b'/', ..] => 1,
        [b'~', b'/' | b'\\', ..] => 2,
        [d, b':', b'/' | b'\\', ..] if d.is_ascii_alphabetic() => 3,
        _ => return None,
    };
    let digits_before = |end: usize| word[..end].iter().rev().take_while(|b| b.is_ascii_digit()).count();
    let col = digits_before(word.len());
    let col_colon = word.len().checked_sub(col + 1).filter(|&i| col > 0 && word[i] == b':')?;
    let line = digits_before(col_colon);
    let line_colon = col_colon.checked_sub(line + 1).filter(|&i| line > 0 && word[i] == b':')?;
    (line_colon >= root).then_some((line_colon, line, col))
}

/// The tokens of a stretch outside every form suffix.
fn count_split(text: &[u8], ranks: &HashMap<Vec<u8>, u32>, starts: &mut Vec<usize>) -> usize {
    let (mut n, mut at) = (0, 0);
    while at < text.len() {
        let len = piece_len(&text[at..]);
        n += text[at..at + len].chunks(BLOCK).map(|b| if b.len() == 1 || ranks.contains_key(b) { 1 } else { merge(b, ranks, starts) }).sum::<usize>();
        at += len;
    }
    n
}

/// The vocabulary's count of `text`, before [`FACTOR`].
pub fn count(text: &[u8]) -> usize {
    let ranks = ranks();
    let mut starts = Vec::new();
    let (mut n, mut done, mut word) = (0, 0, 0);
    while word < text.len() {
        if is_space(text[word]) {
            word += 1;
            continue;
        }
        let end = text[word..].iter().position(|&b| is_space(b)).map_or(text.len(), |i| word + i);
        if let Some((colon, line, col)) = form_suffix(&text[word..end]) {
            // Each colon is one token, each group of up to three digits one.
            n += count_split(&text[done..word + colon], ranks, &mut starts) + 2 + line.div_ceil(3) + col.div_ceil(3);
            done = end;
        }
        word = end;
    }
    n + count_split(&text[done..], ranks, &mut starts)
}

/// The estimate of `text`'s context tokens: [`count`] times [`FACTOR`].
pub fn tokens(text: &str) -> usize {
    (count(text.as_bytes()) as f64 * FACTOR).round() as usize
}

/// What `cargo xtask token-ratio` prints: bytes per token of the sample files,
/// and the same with the encoder's timing under `bench`. It uses the card's
/// factor, so other text reads a little off. It writes nothing:
/// `bytes_per_token` in the configuration stays whatever a person sets.
///
/// # Errors
///
/// A file that cannot be read.
pub fn token_ratio(files: &[String], bench: bool) -> Result<(), String> {
    let t0 = std::time::Instant::now();
    let _ = ranks();
    let load = t0.elapsed();
    let (mut bytes, mut est) = (0usize, 0usize);
    for f in files {
        let text = std::fs::read(f).map_err(|e| format!("{f}: {e}"))?;
        let n = count(&text);
        let t = (n as f64 * FACTOR).round() as usize;
        println!("{f}: {} bytes, count {n}, {t} tokens", text.len());
        bytes += text.len();
        est += t;
        if bench {
            let mut us: Vec<f64> = (0..if text.len() > 100_000 { 21 } else { 2001 })
                .map(|_| {
                    let t = std::time::Instant::now();
                    std::hint::black_box(count(std::hint::black_box(&text[..])));
                    t.elapsed().as_secs_f64() * 1e6
                })
                .collect();
            us.sort_by(f64::total_cmp);
            println!("  encode median {:.1} us, p99 {:.1} us", us[us.len() / 2], us[us.len() * 99 / 100]);
        }
    }
    if bench {
        println!("load {:.1} us ({} tokens, {} bytes of vocabulary)", load.as_secs_f64() * 1e6, ranks().len(), VOCAB.len());
    }
    if est > 0 {
        println!("sample: {bytes} bytes, {est} tokens, {:.2} bytes per token ({WHAT})", bytes as f64 / est as f64);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(s: &str) -> Vec<&str> {
        let (mut out, mut at) = (Vec::new(), 0);
        while at < s.len() {
            let n = piece_len(&s.as_bytes()[at..]);
            out.push(&s[at..at + n]);
            at += n;
        }
        out
    }

    /// The pieces the trainer's pattern gave on the same strings.
    #[test]
    fn pieces_follow_the_pattern() {
        assert_eq!(pieces("journal:ALPHA-V03:0271@abc12345, x"), ["journal", ":ALPHA", "-V", "03", ":", "027", "1", "@abc", "123", "45", ",", " x"]);
        assert_eq!(pieces("/src/a.rs ../b  c\t  d;;\n\n  x"), ["/src", "/a", ".rs", " ..", "/b", " ", " c", "\t ", " d", ";;", "\n\n", " ", " x"]);
        assert_eq!(pieces("    println!(\"{x:?}\");\n"), ["   ", " println", "!(\"", "{x", ":?}\");", "\n"]);
        assert_eq!(pieces("caf\u{e9} \u{4e2d}\u{6587} \u{1f600} it's"), ["caf\u{e9}", " \u{4e2d}\u{6587}", " \u{1f600}", " it", "'s"]);
    }

    /// The counts the trainer's own `Bpe` gave on the same strings.
    #[test]
    fn counts_agree_with_the_script() {
        for (text, want) in COUNTS {
            assert_eq!(count(text.as_bytes()), *want, "{text:?}");
        }
        assert_eq!(count(b""), 0);
    }

    /// The same strings `golden_strings` in `train.py` builds: its `frozen` command
    /// prints this total and hash.
    #[test]
    fn golden_strings_agree_with_the_trainer() {
        let atoms = ["the", " ", "a", "/", "\n", "  ", "\u{e9}", "1", "22", ".rs", ":", "_", "(", ")", "\t", "\u{4e2d}", "X", "'", "\r\n", "\u{2014}", "fn", "4e7", "..", " = ", "    ", ";"];
        let step = |x: u64| (x * 1_103_515_245 + 12_345) % (1 << 31);
        let mut x = 12_345u64;
        let mut all: Vec<Vec<u8>> = Vec::new();
        for _ in 0..400 {
            x = step(x);
            let mut s = String::new();
            for _ in 0..x % 60 {
                x = step(x);
                s.push_str(atoms[((x >> 8) % atoms.len() as u64) as usize]);
            }
            all.push(s.into_bytes());
        }
        all.extend(FORM_EDGES.iter().map(|s| s.as_bytes().to_vec()));
        let (mut total, mut hash) = (0u64, 0u64);
        for s in &all {
            let n = count(s) as u64;
            total += n;
            hash = (hash * 31 + n) % (1 << 32);
        }
        assert_eq!((total, hash), (16_803, 2_285_938_215));
    }

    /// In the form, then just outside it each way.
    const FORM_EDGES: &[&str] = &[
        "/a/b.rs:12:3",
        "x /a/b.rs:1234:5678\n/c:1:2 y",
        "C:/a.rs:1:2",
        "D:\\a\\b.rs:7:88",
        "~/a.rs:9:9",
        "~\\a.rs:9:9",
        "a/b.rs:12:3",
        "/a/b.rs:12",
        "/a/b.rs:12:3:",
        "/a/b.rs:12:3x",
        "/a/b.rs:x:3",
        "(/a/b.rs:12:3)",
        "/a:1:2:3:4",
        "C:a.rs:1:2",
    ];

    #[test]
    fn the_form_is_exactly_the_declared_one() {
        assert_eq!(form_suffix(b"/a/b.rs:1234:5"), Some((7, 4, 1)));
        assert_eq!(form_suffix(b"C:\\a.rs:1:22"), Some((7, 1, 2)));
        assert_eq!(form_suffix(b"/:1:2"), Some((1, 1, 1)));
        assert_eq!(form_suffix(b"/a:1:2:3:4"), Some((6, 1, 1)));
        for not in ["a/b.rs:1:2", "/a.rs:1", "/a.rs:1:2:", "/a.rs:1:x", "(/a.rs:1:2)", "C:a.rs:1:2", ":1:2", "/a.rs::2", "/a.rs:1:", "~a:1:2", ""] {
            assert_eq!(form_suffix(not.as_bytes()), None, "{not}");
        }
    }

    /// The script's counts on the counted sets, which are not in the repository:
    /// `ATLAS_VOCAB_COUNTS` names a JSON object of text file to count. Without the
    /// variable the test is skipped with a notice, as the leak check is.
    #[test]
    fn counted_sets_agree_with_the_script() {
        let Some(file) = std::env::var_os("ATLAS_VOCAB_COUNTS").filter(|v| !v.is_empty()) else {
            eprintln!("counted sets skipped: ATLAS_VOCAB_COUNTS is not set");
            return;
        };
        let want = atlas::json::parse(&std::fs::read_to_string(file).unwrap()).unwrap();
        let want = want.as_object().unwrap();
        assert!(!want.is_empty());
        for (path, n) in want.iter() {
            assert_eq!(count(&std::fs::read(path).unwrap()) as u64, n.as_u64().unwrap(), "{path}");
        }
        eprintln!("counted sets: {} texts agree", want.len());
    }

    /// A long run with no merges stays fast and counts every byte.
    #[test]
    fn a_long_piece_is_cut_into_blocks() {
        let s = "\u{7f}".repeat(1 << 20);
        assert_eq!(count(s.as_bytes()), 1 << 20);
    }

    /// A long run of digits is three at a time, and each piece looks at no more.
    #[test]
    fn a_long_number_stays_linear() {
        let s = "7".repeat(1 << 20);
        assert_eq!(count(s.as_bytes()), 1 << 20);
    }

    #[test]
    fn the_tree_holds_what_the_manifest_froze() {
        let m = atlas::json::parse(include_str!("../vocab/manifest.json")).unwrap();
        let frozen = m.get("frozen").unwrap();
        assert_eq!(ranks().len() as u64, frozen.get("size").unwrap().as_u64().unwrap());
        assert_eq!(VOCAB.lines().count(), ranks().len());
        assert!(ranks().keys().all(|t| t.len() > 1));
        assert_eq!(FACTOR, frozen.get("factors").unwrap().get("card").unwrap().as_f64().unwrap());
    }

    const COUNTS: &[(&str, usize)] = &[
        ("the quick brown fox jumps over the lazy dog", 27),
        ("journal:ALPHA-V03:0271@abc12345, handoff:BRAVO-TK1:010", 45),
        ("C:/work/proj/src/lib.rs:120:4\nD:\\src\\repo\\main.rs", 25),
        ("/a/b.rs:1234:5678 x", 12),
        ("fn main() {\n    let x = vec![1, 2, 3];\n    println!(\"{x:?}\");\n}\n", 45),
        ("4e71bbe0c078468e00fefbf94b39849389f346e5", 38),
        ("caf\u{e9} \u{4e2d}\u{6587} \u{1f600} it's", 18),
        ("/src/a.rs ../b  c\t  d;;\n\n  x", 17),
    ];
}
