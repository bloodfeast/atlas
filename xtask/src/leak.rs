//! The leak check's matcher: denylist terms against file contents and
//! paths, after normalising both. Used by `cargo xtask release` on every
//! exported file (binaries included) and by `tests/leak_check.rs` on every
//! tracked file; the test includes this file and `ucd.rs` by path, so it
//! names its sibling as `super::ucd` and holds no other `crate::` path.
//!
//! # Matching
//!
//! A term is a substring of the text once both are normalised, per
//! character:
//!
//! 1. Decoding: UTF-8 (a BOM is skipped, an invalid byte is U+FFFD), or
//!    UTF-16 by its BOM. A file without a BOM whose bytes are at least a
//!    quarter NUL is also read as UTF-16 (little- or big-endian by where the
//!    NULs sit), beside the UTF-8 reading.
//! 2. Confusables (see [`confusable`]): Cyrillic and Greek letters that look
//!    like Latin ones, and full-width ASCII, become that Latin letter.
//! 3. Unicode lowercase (`UnicodeData.txt` and `SpecialCasing.txt` from the
//!    committed `xtask/ucd/15.1.0`, read by `ucd.rs`), then canonical
//!    decomposition (NFD) with nonspacing marks (`Mn`) dropped, so
//!    precomposed and combining spellings, and accented look-alikes, match.
//! 4. Format characters (`Cf`: zero-width space and joiners, soft hyphen,
//!    BOM, bidi controls) are dropped.
//! 5. Every run of white space and line breaks is one space, so a
//!    multi-word term matches across a line break or a double space (a
//!    term written as one word still does not match two words).
//! 6. String concatenation is joined: a quote, an optional space, an
//!    optional `+ . .. & ~`, an optional space and a quote are dropped, so
//!    `"quill" + "feather"` reads `"quillfeather"`.
//!
//! # Whole-word terms
//!
//! A term matches anywhere, inside longer words too, unless the line right
//! before it is exactly `#[word]` (white space around it trimmed): then it
//! matches only where the characters on both sides of it, in the
//! normalised text, are not word characters (or are the start or end).
//! Word characters are letters and digits (Unicode) and `_`; `-` is not
//! one, so a whole-word term is still caught in `x-term`, but not in
//! `terms` or `x_term`. A term's own first and last characters may be
//! anything: a whole-word term ending in `-` matches before a space or a
//! quote, not before a letter.
//!
//! The marker is a comment line, so it can never be a term, every other
//! reader of the list (the CI log masking, `src/redact.rs`, an older
//! release) still reads the term below it as a plain term, and term numbers
//! do not change. It applies to the very next line only: a blank or
//! comment line after it spends it, leaving the term plain (the side that
//! catches more), and `#[word] term` on one line is a comment.
//!
//! A hit is reported as `path:line:column (byte N): denylist term #K`, the
//! line, column (in characters) and byte offset in the original file, or
//! `path (path): denylist term #K` for a path; the term itself is never
//! printed, so the report cannot leak it. `K` counts the list's terms
//! (comments and blank lines skipped).

use super::ucd::Db;

/// Look-alikes folded to the Latin letter they imitate: the Cyrillic and
/// Greek letters (both cases) that read as a Latin letter in common fonts,
/// and full-width ASCII (U+FF01 to U+FF5E). Applied before lowercasing, so
/// a capital that looks like one Latin letter (Greek Eta, H) and its small
/// form that looks like another (eta, n) each fold to their own. Not the
/// whole of Unicode's confusables.txt: other scripts, mathematical
/// alphanumerics and look-alike digits are out of scope.
pub fn confusable(c: char) -> char {
    let u = c as u32;
    if (0xFF01..=0xFF5E).contains(&u) {
        return char::from_u32(u - 0xFF01 + 0x21).unwrap_or(c);
    }
    match c {
        // Cyrillic capitals.
        '\u{410}' => 'A',
        '\u{412}' => 'B',
        '\u{415}' => 'E',
        '\u{41a}' => 'K',
        '\u{41c}' => 'M',
        '\u{41d}' => 'H',
        '\u{41e}' => 'O',
        '\u{420}' => 'P',
        '\u{421}' => 'C',
        '\u{422}' => 'T',
        '\u{423}' => 'Y',
        '\u{425}' => 'X',
        '\u{405}' => 'S',
        '\u{406}' => 'I',
        '\u{408}' => 'J',
        '\u{4c0}' => 'I',
        '\u{51a}' => 'Q',
        '\u{51c}' => 'W',
        // Cyrillic small letters.
        '\u{430}' => 'a',
        '\u{435}' => 'e',
        '\u{43a}' => 'k',
        '\u{43e}' => 'o',
        '\u{440}' => 'p',
        '\u{441}' => 'c',
        '\u{443}' => 'y',
        '\u{445}' => 'x',
        '\u{455}' => 's',
        '\u{456}' => 'i',
        '\u{458}' => 'j',
        '\u{4bb}' => 'h',
        '\u{4cf}' => 'l',
        '\u{501}' => 'd',
        '\u{51b}' => 'q',
        '\u{51d}' => 'w',
        // Greek capitals.
        '\u{391}' => 'A',
        '\u{392}' => 'B',
        '\u{395}' => 'E',
        '\u{396}' => 'Z',
        '\u{397}' => 'H',
        '\u{399}' => 'I',
        '\u{39a}' => 'K',
        '\u{39c}' => 'M',
        '\u{39d}' => 'N',
        '\u{39f}' => 'O',
        '\u{3a1}' => 'P',
        '\u{3a4}' => 'T',
        '\u{3a5}' => 'Y',
        '\u{3a7}' => 'X',
        // Greek small letters.
        '\u{3b1}' => 'a',
        '\u{3b7}' => 'n',
        '\u{3b9}' => 'i',
        '\u{3ba}' => 'k',
        '\u{3bd}' => 'v',
        '\u{3bf}' => 'o',
        '\u{3c1}' => 'p',
        '\u{3c4}' => 't',
        '\u{3c5}' => 'u',
        '\u{3c7}' => 'x',
        '\u{3f2}' => 'c',
        '\u{3f3}' => 'j',
        _ => c,
    }
}

/// Normalised text and, per byte of it, the byte offset in the original
/// file of the character it came from.
pub struct Norm {
    /// The normalised text.
    pub text: String,
    /// Per byte of `text`, the original byte offset.
    pub at: Vec<usize>,
}

/// The line that marks the next line's term whole-word (see the module
/// docs). A comment to every other reader of the denylist.
pub const WHOLE_WORD: &str = "#[word]";

/// A denylist term, normalised.
pub struct Term {
    /// Its number: the count of terms up to it, from 1.
    pub num: usize,
    /// The normalised term.
    pub text: String,
    /// Matches only between non-word characters ([`is_word`]).
    pub whole_word: bool,
}

impl Term {
    /// The first place of the term in normalised text at or after byte
    /// `from` (a character boundary): any place for a plain term, the first
    /// with a non-word character (or an end) on both sides for a
    /// whole-word one.
    pub fn find(&self, hay: &str, mut from: usize) -> Option<usize> {
        while let Some(i) = hay[from..].find(self.text.as_str()) {
            let p = from + i;
            let end = p + self.text.len();
            if !self.whole_word || (!hay[..p].chars().next_back().is_some_and(is_word) && !hay[end..].chars().next().is_some_and(is_word)) {
                return Some(p);
            }
            from = p + hay[p..].chars().next().map_or(1, char::len_utf8);
        }
        None
    }
}

/// A word character, for whole-word terms: a letter or digit (Unicode,
/// after normalisation) or `_`. `-` is not one, so a whole-word term is
/// still caught in a hyphenated compound (`x-term`), but not in an
/// identifier (`x_term`) or a longer word.
pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The matcher: the Unicode tables and the normalised terms.
pub struct Matcher<'a> {
    db: &'a Db,
    terms: Vec<Term>,
}

impl<'a> Matcher<'a> {
    /// The matcher for a denylist's text: one term per line, trimmed;
    /// blank lines and lines starting with `#` skipped, and a [`WHOLE_WORD`]
    /// line makes the term on the line right after it whole-word. A term
    /// that normalises to nothing keeps its number but matches nothing.
    pub fn new(db: &'a Db, list: &str) -> Matcher<'a> {
        let mut m = Matcher { db, terms: Vec::new() };
        let mut n = 0;
        let mut whole_word = false;
        for l in list.lines().map(|l| l.trim_matches(|c: char| c.is_whitespace())) {
            let marked = std::mem::replace(&mut whole_word, l == WHOLE_WORD);
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            n += 1;
            let chars: Vec<(usize, char)> = l.char_indices().collect();
            let t = m.normalise(&chars).text.trim().to_string();
            if !t.is_empty() {
                m.terms.push(Term { num: n, text: t, whole_word: marked });
            }
        }
        m.terms.shrink_to_fit();
        m
    }

    /// The number of terms that can match.
    pub fn len(&self) -> usize {
        self.terms.len()
    }

    /// The terms that can match. For the accepted-hits digests
    /// (`accept.rs`), which hash a term and never print it.
    #[allow(dead_code, reason = "tests/leak_check.rs includes this file and does not use it")]
    pub fn terms(&self) -> &[Term] {
        &self.terms
    }

    /// Whether no term can match.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Steps 2 to 6 over decoded characters.
    pub fn normalise(&self, chars: &[(usize, char)]) -> Norm {
        let mut n = Norm { text: String::with_capacity(chars.len()), at: Vec::with_capacity(chars.len()) };
        for &(at, c) in chars {
            self.fold(&mut n, c, at);
        }
        join_concatenation(n)
    }

    /// Steps 2 to 5 for one decoded character at byte `at`.
    fn fold(&self, n: &mut Norm, c: char, at: usize) {
        let c = confusable(c);
        if c.is_ascii() {
            if c.is_ascii_whitespace() || c == '\u{b}' {
                space(n, at);
            } else {
                push(n, c.to_ascii_lowercase(), at);
            }
            return;
        }
        if c == '\u{fffd}' {
            push(n, c, at);
            return;
        }
        let u = self.db.get(c as u32);
        if &u.cat == b"Cf" {
            return;
        }
        if u.cat[0] == b'Z' || c == '\u{85}' {
            space(n, at);
            return;
        }
        for x in self.db.nfd(&self.db.lower(c as u32)) {
            let d = self.db.get(x);
            if &d.cat == b"Mn" || &d.cat == b"Cf" {
                continue;
            }
            push(n, char::from_u32(x).map(confusable).unwrap_or('\u{fffd}'), at);
        }
    }

    /// One line per (file, term) hit, at its first place in the file (or
    /// the path, when the contents do not hold it).
    pub fn hits(&self, files: &[(String, Vec<u8>)]) -> Vec<String> {
        let mut out = Vec::new();
        for (rel, body) in files {
            let name_chars: Vec<(usize, char)> = rel.char_indices().collect();
            let name = self.normalise(&name_chars);
            let reads: Vec<(Vec<(usize, char)>, Norm)> = readings(body)
                .into_iter()
                .map(|r| {
                    let n = self.normalise(&r);
                    (r, n)
                })
                .collect();
            for t in &self.terms {
                let k = t.num;
                let first = reads.iter().filter_map(|(r, n)| t.find(&n.text, 0).map(|p| (n.at[p], r))).min_by_key(|(at, _)| *at);
                if let Some((at, r)) = first {
                    let (line, col) = line_col(r, at);
                    out.push(format!("{rel}:{line}:{col} (byte {at}): denylist term #{k}"));
                } else if t.find(&name.text, 0).is_some() {
                    out.push(format!("{rel} (path): denylist term #{k}"));
                }
            }
        }
        out
    }
}

fn push(n: &mut Norm, c: char, at: usize) {
    n.text.push(c);
    n.at.resize(n.text.len(), at);
}

fn space(n: &mut Norm, at: usize) {
    if !n.text.is_empty() && !n.text.ends_with(' ') {
        push(n, ' ', at);
    }
}

fn is_quote(c: u8) -> bool {
    matches!(c, b'"' | b'\'' | b'`')
}

/// Step 6: drops each quote, optional space, optional concatenation
/// operator, optional space, quote.
fn join_concatenation(n: Norm) -> Norm {
    let b = n.text.as_bytes();
    if !b.iter().any(|&c| is_quote(c)) {
        return n;
    }
    let mut out = Norm { text: String::with_capacity(b.len()), at: Vec::with_capacity(b.len()) };
    let mut keep_from = 0;
    let mut i = 0;
    while i < b.len() {
        if is_quote(b[i]) {
            let mut j = i + 1;
            if b.get(j) == Some(&b' ') {
                j += 1;
            }
            match b.get(j) {
                Some(b'.') if b.get(j + 1) == Some(&b'.') => j += 2,
                Some(b'+' | b'.' | b'&' | b'~') => j += 1,
                _ => {}
            }
            if b.get(j) == Some(&b' ') {
                j += 1;
            }
            if b.get(j).is_some_and(|&q| is_quote(q)) {
                out.text.push_str(&n.text[keep_from..i]);
                out.at.extend_from_slice(&n.at[keep_from..i]);
                keep_from = j + 1;
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out.text.push_str(&n.text[keep_from..]);
    out.at.extend_from_slice(&n.at[keep_from..]);
    out
}

/// The readings of a file (step 1): each character with its byte offset.
pub fn readings(bytes: &[u8]) -> Vec<Vec<(usize, char)>> {
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => vec![utf8(rest, 3)],
        [0xFF, 0xFE, rest @ ..] => vec![utf16(rest, 2, false)],
        [0xFE, 0xFF, rest @ ..] => vec![utf16(rest, 2, true)],
        _ => {
            let mut out = vec![utf8(bytes, 0)];
            let nul = |parity: usize| bytes.iter().skip(parity).step_by(2).filter(|&&b| b == 0).count();
            let (even, odd) = (nul(0), nul(1));
            if bytes.len() >= 4 && (even + odd) * 4 >= bytes.len() {
                out.push(utf16(bytes, 0, even > odd));
            }
            out
        }
    }
}

fn utf8(bytes: &[u8], base: usize) -> Vec<(usize, char)> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = base;
    for chunk in bytes.utf8_chunks() {
        for (i, c) in chunk.valid().char_indices() {
            out.push((at + i, c));
        }
        at += chunk.valid().len();
        if !chunk.invalid().is_empty() {
            out.push((at, '\u{fffd}'));
            at += chunk.invalid().len();
        }
    }
    out
}

fn utf16(bytes: &[u8], base: usize, big_endian: bool) -> Vec<(usize, char)> {
    let units = bytes.as_chunks::<2>().0.iter().map(|p| if big_endian { u16::from_be_bytes(*p) } else { u16::from_le_bytes(*p) });
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut at = base;
    for r in char::decode_utf16(units) {
        let (c, n) = match r {
            Ok(c) => (c, c.len_utf16()),
            Err(_) => ('\u{fffd}', 1),
        };
        out.push((at, c));
        at += 2 * n;
    }
    out
}

/// `(line, column)` of byte `at` in a reading, both from 1.
pub fn line_col(reading: &[(usize, char)], at: usize) -> (usize, usize) {
    let (mut line, mut col) = (1, 1);
    for &(o, c) in reading {
        if o >= at {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}
