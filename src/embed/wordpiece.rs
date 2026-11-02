//! BERT's tokenizer, owned: the basic tokenizer (clean, CJK split,
//! lowercase and accent strip, whitespace and punctuation split) then
//! greedy longest-match-first WordPiece.
//!
//! It follows Hugging Face's `BertTokenizer` (`BasicTokenizer` +
//! `WordpieceTokenizer`). The Unicode facts it needs (what each code point
//! lowercases and strips to, and which code points are whitespace, control
//! or punctuation) come from the packed model file, where the one-time
//! packing step wrote them from Python's `unicodedata`; nothing here holds a
//! Unicode table.

use std::collections::HashMap;

/// Character class flags (as packed).
pub const WHITESPACE: u32 = 1;
/// See [`WHITESPACE`].
pub const CONTROL: u32 = 2;
/// See [`WHITESPACE`].
pub const PUNCT: u32 = 4;

/// The longest word WordPiece tries to split; longer is `[UNK]`.
pub const MAX_WORD_CHARS: usize = 100;

/// A WordPiece tokenizer with its vocabulary and Unicode tables.
#[derive(Debug, Clone, Default)]
pub struct Tokenizer {
    vocab: HashMap<String, u32>,
    /// Non-ASCII code point -> its lowercased, accent-stripped text.
    map: HashMap<u32, String>,
    /// Sorted, disjoint `(start, end inclusive, flags)` for non-ASCII.
    classes: Vec<(u32, u32, u32)>,
    lowercase: bool,
    /// `[CLS]`, `[SEP]`, `[UNK]` ids.
    pub cls: u32,
    /// See [`Tokenizer::cls`].
    pub sep: u32,
    /// See [`Tokenizer::cls`].
    pub unk: u32,
}

fn is_cjk(cp: u32) -> bool {
    matches!(cp, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF | 0x2A700..=0x2B73F | 0x2B740..=0x2B81F | 0x2B820..=0x2CEAF | 0xF900..=0xFAFF | 0x2F800..=0x2FA1F)
}

impl Tokenizer {
    /// Builds a tokenizer.
    ///
    /// # Arguments
    ///
    /// * `vocab` - tokens in id order.
    /// * `map` - per non-ASCII code point, its normalised text.
    /// * `classes` - sorted `(start, end, flags)` ranges.
    /// * `lowercase` - whether to lowercase and strip accents.
    ///
    /// # Returns
    ///
    /// `None` when `[CLS]`, `[SEP]` or `[UNK]` is missing from the vocabulary.
    pub fn new(vocab: Vec<String>, map: Vec<(u32, String)>, classes: Vec<(u32, u32, u32)>, lowercase: bool) -> Option<Tokenizer> {
        let vocab: HashMap<String, u32> = vocab.into_iter().enumerate().map(|(i, t)| (t, i as u32)).collect();
        let (cls, sep, unk) = (*vocab.get("[CLS]")?, *vocab.get("[SEP]")?, *vocab.get("[UNK]")?);
        Some(Tokenizer { vocab, map: map.into_iter().collect(), classes, lowercase, cls, sep, unk })
    }

    fn class(&self, c: char) -> u32 {
        let cp = c as u32;
        if cp < 0x80 {
            return match c {
                ' ' | '\t' | '\n' | '\r' => WHITESPACE,
                '\0'..='\x1f' | '\x7f' => CONTROL,
                '!'..='/' | ':'..='@' | '['..='`' | '{'..='~' => PUNCT,
                _ => 0,
            };
        }
        match self.classes.binary_search_by(|r| if r.1 < cp { std::cmp::Ordering::Less } else if r.0 > cp { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal }) {
            Ok(i) => self.classes[i].2,
            Err(_) => 0,
        }
    }

    /// The basic tokenizer: the words and punctuation marks of `text`.
    ///
    /// # Arguments
    ///
    /// * `text` - any text.
    pub fn words(&self, text: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut cur = String::new();
        let flush = |cur: &mut String, words: &mut Vec<String>| {
            if !cur.is_empty() {
                words.push(std::mem::take(cur));
            }
        };
        for c in text.chars() {
            let cp = c as u32;
            // Clean: drop NUL, U+FFFD and control characters; whitespace splits.
            if cp == 0 || cp == 0xFFFD {
                continue;
            }
            let cl = self.class(c);
            if cl & WHITESPACE != 0 {
                flush(&mut cur, &mut words);
                continue;
            }
            if cl & CONTROL != 0 {
                continue;
            }
            if is_cjk(cp) {
                flush(&mut cur, &mut words);
                words.push(c.to_string());
                continue;
            }
            // Lowercase and strip accents (one or more characters, or none).
            let norm: Option<&str> = if !self.lowercase {
                None
            } else if cp < 0x80 {
                None
            } else {
                self.map.get(&cp).map(String::as_str)
            };
            let mut push = |n: char| {
                let n = if self.lowercase && n.is_ascii_uppercase() { n.to_ascii_lowercase() } else { n };
                if self.class(n) & PUNCT != 0 {
                    flush(&mut cur, &mut words);
                    words.push(n.to_string());
                } else {
                    cur.push(n);
                }
            };
            match norm {
                Some(s) => s.chars().for_each(&mut push),
                None => push(c),
            }
        }
        flush(&mut cur, &mut words);
        words
    }

    /// Greedy longest-match-first WordPiece over one word, appending ids.
    fn wordpiece(&self, word: &str, out: &mut Vec<u32>) {
        let chars: Vec<(usize, char)> = word.char_indices().collect();
        if chars.len() > MAX_WORD_CHARS {
            out.push(self.unk);
            return;
        }
        let mark = out.len();
        let mut start = 0;
        let mut buf = String::with_capacity(word.len() + 2);
        while start < chars.len() {
            let mut end = chars.len();
            let mut found = None;
            while start < end {
                buf.clear();
                if start > 0 {
                    buf.push_str("##");
                }
                let a = chars[start].0;
                let b = chars.get(end).map_or(word.len(), |c| c.0);
                buf.push_str(&word[a..b]);
                if let Some(&id) = self.vocab.get(buf.as_str()) {
                    found = Some(id);
                    break;
                }
                end -= 1;
            }
            match found {
                Some(id) => out.push(id),
                None => {
                    out.truncate(mark);
                    out.push(self.unk);
                    return;
                }
            }
            start = end;
        }
    }

    /// Token ids of `text` for the encoder: `[CLS]`, at most `max - 2`
    /// pieces, `[SEP]`.
    ///
    /// # Arguments
    ///
    /// * `text` - any text.
    /// * `max` - the model's sequence length.
    pub fn encode(&self, text: &str, max: usize) -> Vec<u32> {
        // Room for [CLS] and [SEP] at least (`max - 1` underflowed at 0).
        let max = max.max(2);
        let mut ids = vec![self.cls];
        for w in self.words(text) {
            self.wordpiece(&w, &mut ids);
            if ids.len() >= max - 1 {
                break;
            }
        }
        ids.truncate(max - 1);
        ids.push(self.sep);
        ids
    }

    /// Every token id of `text` with no length cut and no special tokens
    /// (for comparing with a reference tokenizer).
    pub fn encode_all(&self, text: &str) -> Vec<u32> {
        let mut ids = Vec::new();
        for w in self.words(text) {
            self.wordpiece(&w, &mut ids);
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn tiny() -> Tokenizer {
        let vocab = ["[PAD]", "[UNK]", "[CLS]", "[SEP]", "the", "un", "##aff", "##able", "cafe", ",", "!", "中", "run", "##ning", "##s", "x"];
        Tokenizer::new(
            vocab.iter().map(|s| s.to_string()).collect(),
            vec![(0xC9, "e".into()), (0xE9, "e".into())],
            vec![(0xA0, 0xA0, WHITESPACE), (0xAD, 0xAD, CONTROL), (0x2014, 0x2014, PUNCT)],
            true,
        )
        .unwrap()
    }

    #[test]
    fn basic_splits_like_bert() {
        let t = tiny();
        assert_eq!(t.words("The  CAFÉ,unaffable!\u{a0}中x\u{ad}y—z"), ["the", "cafe", ",", "unaffable", "!", "中", "xy", "—", "z"]);
    }

    #[test]
    fn a_max_below_two_still_frames_the_ids() {
        let t = tiny();
        for max in [0, 1, 2] {
            let ids = t.encode("the cafe running", max);
            assert_eq!(ids, vec![t.cls, t.sep], "max {max}");
        }
    }

    #[test]
    fn wordpiece_greedy_and_unknown() {
        let t = tiny();
        assert_eq!(t.encode_all("unaffable running runs"), [5, 6, 7, 12, 13, 12, 14]);
        assert_eq!(t.encode_all("unknownword"), [1]);
        assert_eq!(t.encode("the the the the", 4), [2, 4, 4, 3]);
        let long: String = "x".repeat(MAX_WORD_CHARS + 1);
        assert_eq!(t.encode_all(&long), [1]);
    }
}
