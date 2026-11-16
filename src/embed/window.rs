//! The one window splitter: a section longer than the encoder's cut is
//! embedded as overlapping windows, and scores by its best window.
//!
//! * Window 0 is the section's own text ([`crate::semantic::section_text`]),
//!   cut by the encoder as before, so its vector is the one keyed by
//!   `sections.sha` in `vec_cache` today.
//! * Windows 1..n are the heading path (whole words, at most
//!   [`HEADING_MAX`] tokens), a newline and a byte range of the body cut on
//!   word boundaries, at most [`W`] content tokens with the heading. Each
//!   starts [`S`] body tokens after the one before (at the word boundary at
//!   or before that token, never past the previous window's end, so no text
//!   is skipped); the last is moved back to end at the body's end.
//! * A section that fits in [`W`] tokens has window 0 only.
//! * For a model whose sequence is shorter than 512 (the tests' tiny one),
//!   [`W`], [`S`] and [`HEADING_MAX`] scale down with it.
//!
//! Tokens are the model's own WordPiece pieces ([`Tokenizer`]); the count
//! of a text is the sum of the counts of its runs between word-splitting
//! characters ([`Tokenizer::is_space`]), which is what lets a window be cut
//! between any two runs with its count known exactly.
//!
//! Everything that decides a window is in this file, and its code (tests,
//! comment lines, blank lines and the two `WINDOW_RULES_` lines removed) is
//! hashed by a test against [`WINDOW_RULES_HISTORY`]: a change here that is
//! not recorded there with a new [`WINDOW_RULES_VERSION`] fails the tests,
//! and a new version re-splits every section (windows whose text did not
//! change keep their vectors, which are keyed by the window text's hash).

use std::ops::Range;

use super::wordpiece::Tokenizer;

/// The most content tokens in a window (the encoder's 512 less `[CLS]` and
/// `[SEP]`), heading included.
pub const W: usize = 510;

/// The stride: body tokens from one window's start to the next.
pub const S: usize = 384;

/// The most heading tokens a window 1..n repeats (whole words).
pub const HEADING_MAX: usize = 128;

/// The window rules' version, stamped on every `sec_window` row: a row with
/// an older one is split again.
pub const WINDOW_RULES_VERSION: u32 = 1;

/// Every window rules version with the hash of this file's code (see the module docs).
pub const WINDOW_RULES_HISTORY: &[(u32, &str)] = &[(1, "efb2277324c60e39113cefd28f4139ba22b6dedbe00f3592d5a8785492dfa543")];

/// One window of a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// The text embedded.
    pub text: String,
    /// The body bytes it holds.
    pub body: Range<usize>,
}

/// A run of more tokens than this is cut again before each ASCII
/// punctuation mark (the basic tokenizer splits there too, so counts still
/// add up): minified JSON, a long URL or a path is one run between spaces.
pub const LONG_RUN: usize = 32;

/// The runs of `text` between word-splitting characters: `(start byte, end
/// byte, tokens)`, in order; a run over [`LONG_RUN`] tokens is cut again
/// before each ASCII punctuation mark.
///
/// # Arguments
///
/// * `tok` - the model's tokenizer.
/// * `text` - any text.
pub fn runs(tok: &Tokenizer, text: &str) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        if tok.is_space(c) {
            if let Some(s) = start.take() {
                push_run(tok, text, s, i, &mut out);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        push_run(tok, text, s, text.len(), &mut out);
    }
    out
}

fn push_run(tok: &Tokenizer, text: &str, s: usize, e: usize, out: &mut Vec<(usize, usize, usize)>) {
    let n = tok.encode_all(&text[s..e]).len();
    if n <= LONG_RUN {
        out.push((s, e, n));
        return;
    }
    let mut a = s;
    for (i, c) in text[s..e].char_indices() {
        let i = s + i;
        if i > a && c.is_ascii_punctuation() {
            out.push((a, i, tok.encode_all(&text[a..i]).len()));
            a = i;
        }
    }
    out.push((a, e, tok.encode_all(&text[a..e]).len()));
}

/// Splits a section into its windows.
///
/// # Arguments
///
/// * `tok` - the model's tokenizer.
/// * `max_seq` - the model's sequence length (512: [`W`] is 510).
/// * `heading` - the section's heading path.
/// * `body` - the section's body.
///
/// # Returns
///
/// Window 0 (the whole section text, body `0..body.len()`), then windows
/// 1..n when the section is longer than [`W`] tokens.
pub fn split(tok: &Tokenizer, max_seq: usize, heading: &str, body: &str) -> Vec<Window> {
    let w = W.min(max_seq.saturating_sub(2)).max(1);
    let (stride, head_max) = ((S * w / W).max(1), HEADING_MAX * w / W);
    let mut out = vec![Window { text: crate::semantic::section_text(heading, body), body: 0..body.len() }];
    let heads = runs(tok, heading);
    let h: usize = heads.iter().map(|r| r.2).sum();
    let br = runs(tok, body);
    let mut pre = Vec::with_capacity(br.len() + 1);
    pre.push(0usize);
    for r in &br {
        pre.push(pre[pre.len() - 1] + r.2);
    }
    let total = pre[br.len()];
    if h + total <= w {
        return out;
    }
    let (mut kept, mut head_end) = (0, 0);
    for r in &heads {
        if kept + r.2 > head_max {
            break;
        }
        kept += r.2;
        head_end = r.1;
    }
    let head = &heading[..head_end];
    let budget = w - kept;
    // Window 0 holds the body's first `w - h` tokens (the encoder's cut).
    let (mut prev_start, mut prev_end) = (0, w.saturating_sub(h).min(total));
    let mut prev: Option<(usize, usize)> = None;
    while prev_end < total {
        let target = (prev_start + stride).min(prev_end);
        let mut i = pre.partition_point(|&p| p <= target).saturating_sub(1);
        let mut j;
        if total - pre[i] <= budget {
            // The last window: as far back as the budget allows from the end.
            i = pre.partition_point(|&p| total - p > budget);
            j = br.len();
        } else {
            j = pre.partition_point(|&p| p <= pre[i] + budget).saturating_sub(1);
        }
        if let Some((pi, pj)) = prev {
            i = i.max(pi + 1);
            j = j.max(pj + 1);
        }
        j = j.max(i + 1).min(br.len());
        out.push(Window { text: format!("{head}\n{}", &body[br[i].0..br[j - 1].1]), body: br[i].0..br[j - 1].1 });
        prev = Some((i, j));
        (prev_start, prev_end) = (pre[i], pre[j]);
        if j == br.len() {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every lowercase ASCII word of n letters is n tokens.
    fn letters() -> Tokenizer {
        let mut v: Vec<String> = ["[PAD]", "[UNK]", "[CLS]", "[SEP]"].iter().map(|s| s.to_string()).collect();
        for c in 'a'..='z' {
            v.push(c.to_string());
            v.push(format!("##{c}"));
        }
        v.push(",".into());
        Tokenizer::new(v, Vec::new(), vec![(0xA0, 0xA0, super::super::wordpiece::WHITESPACE)], true).unwrap()
    }

    fn body_of(words: usize, len: usize) -> String {
        (0..words).map(|i| "abcdefgh"[..1 + (i * 7 + len) % 4].to_string()).collect::<Vec<_>>().join(" ")
    }

    fn code_of(src: &str) -> String {
        let body = src.split("\n#[cfg(test)]").next().unwrap_or(src);
        body.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with("pub const WINDOW_RULES_"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_window_rules_version_follows_the_rules() {
        let h = crate::sha256::hex(code_of(include_str!("window.rs")).as_bytes());
        let (v, last) = WINDOW_RULES_HISTORY.last().unwrap();
        assert!(WINDOW_RULES_HISTORY.windows(2).all(|w| w[0].0 < w[1].0), "versions only grow");
        assert_eq!(*v, WINDOW_RULES_VERSION, "WINDOW_RULES_VERSION is the last row of WINDOW_RULES_HISTORY");
        assert_eq!(
            h,
            *last,
            "the window rules changed: append ({}, {h:?}) to WINDOW_RULES_HISTORY and set WINDOW_RULES_VERSION to {}",
            v + 1,
            v + 1
        );
    }

    #[test]
    fn run_counts_add_up_to_the_whole_text() {
        let t = letters();
        for s in ["", "  ab c,d \u{a0}efg\n\nh", &body_of(300, 3)] {
            let r = runs(&t, s);
            assert_eq!(r.iter().map(|x| x.2).sum::<usize>(), t.encode_all(s).len(), "{s:?}");
            assert!(r.iter().all(|x| !s[x.0..x.1].chars().any(|c| t.is_space(c))));
        }
    }

    #[test]
    fn a_section_that_fits_has_window_zero_only() {
        let t = letters();
        let body = "ab ".repeat(254);
        let w = split(&t, 512, "h", body.trim_end());
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].text, format!("h\n{}", body.trim_end()));
        assert_eq!(w[0].body, 0..body.trim_end().len());
        assert_eq!(split(&t, 512, "h", &"ab ".repeat(255)).len(), 2, "511 tokens is one over");
    }

    #[test]
    fn windows_cover_the_body_within_the_budget_and_end_at_its_end() {
        let t = letters();
        for (words, heading) in [(400, "head ing"), (1000, "h"), (3333, "a much longer heading path > with parts"), (700, "")] {
            let body = body_of(words, words);
            let w = split(&t, 512, heading, &body);
            let h = t.encode_all(heading).len();
            let total = t.encode_all(&body).len();
            assert!(w.len() >= 2, "{words}");
            assert_eq!(w[0].text, crate::semantic::section_text(heading, &body));
            // Count: about 1 + ceil((h + total - W) / S); the word-boundary
            // rounding adds at most one.
            let expect = 1 + (h + total - W).div_ceil(S);
            assert!(w.len() == expect || w.len() == expect + 1, "{words}: {} windows, expected {expect}", w.len());
            let mut covered_to = 0;
            for (k, x) in w.iter().enumerate().skip(1) {
                assert!(t.encode_all(&x.text).len() <= W, "window {k} over budget");
                assert!(x.text.starts_with(&format!("{heading}\n")));
                assert_eq!(&x.text[heading.len() + 1..], &body[x.body.clone()]);
                let before = t.encode_all(&body[..x.body.start]).len();
                let window0 = W - h;
                assert!(before <= covered_to.max(window0), "window {k} leaves a gap");
                covered_to = before + t.encode_all(&body[x.body.clone()]).len();
                if k > 1 {
                    assert!(x.body.start > w[k - 1].body.start, "starts only move forward");
                }
            }
            assert_eq!(w.last().unwrap().body.end, body.len(), "the last ends at the body's end");
            let last = &w[w.len() - 1];
            assert!(t.encode_all(&last.text).len() > W - 8, "the last window is full");
        }
    }

    #[test]
    fn the_stride_is_s_body_tokens() {
        let t = letters();
        // One-token words: positions are exact.
        let body = vec!["a"; 2000].join(" ");
        let w = split(&t, 512, "h", &body);
        let starts: Vec<usize> = w[1..].iter().map(|x| t.encode_all(&body[..x.body.start]).len()).collect();
        assert_eq!(&starts[..3], &[S, 2 * S, 3 * S]);
        assert_eq!(*starts.last().unwrap(), 2000 - (W - 1), "the last is aligned to the end");
        assert_eq!(w.len(), 1 + (2001 - W).div_ceil(S));
    }

    #[test]
    fn a_long_heading_is_cut_to_whole_words() {
        let t = letters();
        let heading = vec!["ab"; 100].join(" ");
        let body = vec!["a"; 900].join(" ");
        let w = split(&t, 512, &heading, &body);
        let head = w[1].text.split('\n').next().unwrap();
        assert_eq!(t.encode_all(head).len(), HEADING_MAX);
        assert!(heading.starts_with(head));
        // Window 0 holds W - 200 body tokens; window 1 starts there, not at S.
        assert_eq!(t.encode_all(&body[..w[1].body.start]).len(), W - 200);
    }

    #[test]
    fn a_run_longer_than_the_budget_still_moves_forward() {
        let t = letters();
        let big = vec!["a"; 700].join(",");
        let body = format!("b {big} c d");
        let r = runs(&t, &body);
        assert_eq!(r.iter().map(|x| x.2).sum::<usize>(), t.encode_all(&body).len(), "cut runs still add up");
        assert!(r.iter().all(|x| x.2 <= LONG_RUN), "the blob is cut before its commas");
        let w = split(&t, 512, "h", &body);
        assert!(w.len() >= 3);
        assert_eq!(w.last().unwrap().body.end, body.len());
        let mut covered_to = W - 1;
        for x in &w[1..] {
            assert!(t.encode_all(&x.text).len() <= W, "within the budget");
            let before = t.encode_all(&body[..x.body.start]).len();
            assert!(before <= covered_to, "no gap");
            covered_to = before + t.encode_all(&body[x.body.clone()]).len();
        }
        assert!(w.windows(2).skip(1).all(|p| p[1].body.start > p[0].body.start));
    }
}
