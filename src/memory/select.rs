//! Context selection: what the hooks inject, cut to what the prompt needs
//! (Brandon, 2026-11-02; `recall.select` in the configuration).
//!
//! Three steps, each measured by the memory eval and logged per injected key
//! for `--token-report` (`inject_select`):
//!
//! 1. **Gate** (the relevance gate and token budget of [`super::recall`]
//!    and the hooks): nothing is injected when nothing clears it. This
//!    module adds nothing to it.
//! 2. **Collapse**: a record that is a near-duplicate of one already
//!    injected (the same rule kept in INTENT, a memory file and a restore
//!    point) is sent as a ref to it, `- KEY: near-duplicate of OTHER`. Near means
//!    both a cosine at or above [`Select::dup_cos`] between the two records'
//!    vectors and an exact-text check: at least [`Select::dup_overlap`] of
//!    its distinct words are in the record sent ([`overlap`], one way, so
//!    what it adds is never hidden behind the ref). Without a model nothing
//!    collapses.
//! 3. **Trim**: a pointer carries the sentence (or clause) of the record that
//!    best matches the prompt rather than its first line ([`excerpt`]). Every
//!    piece of an excerpt is an exact byte range of the record's title or
//!    body, so a quote of it is the record's own text; pieces are joined by
//!    ` ... ` and a cut edge is marked `... ` or ` ...`.
//!
//! The scanners are hand-written (no regex).

use std::collections::HashSet;

use super::recall::terms;
use super::Head;

/// Context-selection settings (`recall.select`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Select {
    /// Collapse and trim are on. Off, pointers are as before (the gate and
    /// budget still apply).
    pub enabled: bool,
    /// The cosine between two records' vectors at which they may collapse.
    pub dup_cos: f32,
    /// The share of a record's distinct words the one sent before it must
    /// hold for it to collapse into that one.
    pub dup_overlap: f32,
    /// The longest excerpt a trimmed pointer carries, in characters.
    pub trim_chars: usize,
    /// The longest excerpt of an older lane entry in a hook's lane brief,
    /// in characters; 0 sends every entry whole.
    pub lane_chars: usize,
}

/// The default [`Select::dup_cos`].
pub const DEFAULT_DUP_COS: f32 = 0.82;
/// The default [`Select::dup_overlap`].
pub const DEFAULT_DUP_OVERLAP: f32 = 0.90;
/// The default [`Select::trim_chars`].
pub const DEFAULT_TRIM_CHARS: usize = 90;
/// The default [`Select::lane_chars`].
pub const DEFAULT_LANE_CHARS: usize = 180;

impl Default for Select {
    fn default() -> Self {
        Select { enabled: true, dup_cos: DEFAULT_DUP_COS, dup_overlap: DEFAULT_DUP_OVERLAP, trim_chars: DEFAULT_TRIM_CHARS, lane_chars: DEFAULT_LANE_CHARS }
    }
}

/// Which text of a record a span is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The title.
    Title,
    /// The body.
    Body,
}

/// An excerpt: byte ranges of one part of a record, ascending and
/// disjoint, each on char boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    /// The part the spans are in.
    pub part: Part,
    /// The ranges.
    pub spans: Vec<(usize, usize)>,
}

impl Excerpt {
    /// The text of the part this excerpt is cut from.
    pub fn source<'a>(&self, h: &'a Head) -> &'a str {
        match self.part {
            Part::Title => &h.title,
            Part::Body => &h.body,
        }
    }

    /// The excerpt as one line: the spans in order, ` ... ` between two
    /// that do not touch, and `... ` or ` ...` at a cut edge (text before the first
    /// span or after the last, other than white space).
    pub fn render(&self, h: &Head) -> String {
        let src = self.source(h);
        let mut out = String::new();
        let mut last = None;
        for &(s, e) in &self.spans {
            match last {
                None if !only_markers(&src[..s]) => out.push_str("... "),
                Some(p) if !src[p..s].trim().is_empty() => out.push_str(" ... "),
                Some(_) => out.push(' '),
                None => {}
            }
            out.push_str(&src[s..e]);
            last = Some(e);
        }
        if let Some(e) = last {
            if !src[e..].trim().is_empty() {
                out.push_str(" ...");
            }
        }
        out
    }
}

/// Whether text before an excerpt is only white space and list markers
/// (`- `, `* `, `+ `, `> `): no text is cut there.
fn only_markers(t: &str) -> bool {
    t.split_whitespace().all(|w| matches!(w, "-" | "*" | "+" | ">"))
}

/// Whether a char ends a line for the scanners (a pointer is one line).
fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// The shortest clause [`segments`] cuts at a sentence mark: shorter, it
/// stays with the next one (so `e.g.` or `No. 3` is no clause of its own).
const MIN_CLAUSE: usize = 24;

/// The sentences and clauses of a text, as byte ranges with the outer white
/// space and a leading list marker (`- `, `* `, `+ `, `> `) left out. A
/// line break always ends one; `.`, `?`, `!` and `;` end one when followed
/// by white space or the end, and the clause so far is at least
/// [`MIN_CLAUSE`] bytes. The mark stays in the clause it ends.
pub fn segments(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut push = |s: usize, e: usize| {
        let piece = &text[s..e];
        let lead = piece.len() - piece.trim_start().len();
        let mut s = s + lead;
        let rest = &text[s..e];
        for m in ["- ", "* ", "+ ", "> "] {
            if rest.starts_with(m) {
                s += m.len();
                break;
            }
        }
        let piece = &text[s..e];
        let e = s + piece.trim_end().len();
        let s = e - text[s..e].trim_start().len();
        if e > s {
            out.push((s, e));
        }
    };
    let mut start = 0;
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if is_break(c) {
            push(start, i);
            start = i + c.len_utf8();
            continue;
        }
        if matches!(c, '.' | '?' | '!' | ';') {
            let end = i + 1;
            let next_space = it.peek().is_none_or(|&(_, n)| n.is_whitespace());
            if next_space && text[start..end].trim().len() >= MIN_CLAUSE {
                push(start, end);
                start = end;
            }
        }
    }
    push(start, text.len());
    out
}

/// The distinct words of a text worth comparing: [`terms`] without its cap.
fn words(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for w in text.split(|c: char| !c.is_alphanumeric()) {
        if w.chars().count() < 3 || w.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        out.insert(w.to_lowercase());
    }
    out
}

/// The exact-text check of a collapse: the share of `part`'s distinct
/// words (see [`words`]) that `whole` holds; 0 when `part` has none. One
/// way on purpose: a record is sent as a ref only to one that holds its
/// words, so what it adds (a memory saying a rule was REVERSED, say) is
/// never hidden behind the ref. On the eval store's injections every pair
/// at 0.90 or more (with a cosine of 0.82 or more) is an INTENT rule and
/// the memory file that restates it whole.
pub fn overlap(whole: &str, part: &str) -> f32 {
    let (big, small) = (words(whole), words(part));
    if small.is_empty() {
        return 0.0;
    }
    small.iter().filter(|w| big.contains(*w)).count() as f32 / small.len() as f32
}

/// The text of a record the collapse check compares: title and body.
pub fn record_words_text(h: &Head) -> String {
    format!("{}\n{}", h.title, h.body)
}

/// A dated lead, `2026-10-30 Brandon:`: the text starts (after white space
/// and a list marker) with a digit and has `: ` (or a final `:`) within its
/// first 48 bytes, before any sentence mark or line break. Returns the range up to and including the colon.
fn lead(text: &str) -> Option<(usize, usize)> {
    let mut t = text.trim_start();
    for m in ["- ", "* ", "+ ", "> "] {
        if let Some(r) = t.strip_prefix(m) {
            t = r.trim_start();
            break;
        }
    }
    let off = text.len() - t.len();
    if !t.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    for (i, c) in t.char_indices() {
        if i > 48 || is_break(c) || matches!(c, '.' | '?' | '!' | ';') && t[i + 1..].starts_with(' ') {
            return None;
        }
        if c == ':' && (t[i + 1..].is_empty() || t[i + 1..].starts_with(' ')) {
            return Some((off, off + i + 1));
        }
    }
    None
}

/// The distinct prompt terms a piece of text holds as words.
fn matched(piece: &str, ts: &[String]) -> usize {
    let l = piece.to_lowercase();
    ts.iter().filter(|t| super::contains_word(&l, t)).count()
}

/// Cuts a span of more than `max` chars to a window of at most `max` chars
/// on word boundaries, starting at the word before the first prompt term it
/// holds (or at its start).
fn window(text: &str, (s, e): (usize, usize), ts: &[String], max: usize) -> (usize, usize) {
    let piece = &text[s..e];
    if piece.chars().count() <= max {
        return (s, e);
    }
    // `max` chars from byte `from` of the piece, as a byte end.
    let end_of = |from: usize| piece[from..].char_indices().nth(max).map_or(piece.len(), |(i, _)| from + i);
    // The piece lowercased char by char, and for each of its bytes the
    // piece's byte it came from: lowercasing can grow one char and shrink
    // another, so offsets in `l` are mapped back, never reused.
    let mut l = String::with_capacity(piece.len());
    let mut back = Vec::with_capacity(piece.len() + 1);
    for (i, c) in piece.char_indices() {
        for lc in c.to_lowercase() {
            l.push(lc);
            back.resize(l.len(), i);
        }
    }
    back.push(piece.len());
    // The first term's offset in the piece.
    let first = ts.iter().filter_map(|t| find_word(&l, t)).min().map_or(0, |o| back[o]);
    // One word of context before it.
    let from = match piece[..first].trim_end().rfind(' ') {
        Some(sp) => sp + 1,
        None => 0,
    };
    let mut to = end_of(from);
    if to < piece.len() {
        // Back to the last space, so no word is cut.
        if let Some(sp) = piece[from..to].rfind(' ').filter(|&sp| sp > 0) {
            to = from + sp;
        }
    }
    let w = piece[from..to].trim();
    if w.is_empty() {
        // Only white space there: the head of the span instead.
        let to = end_of(0);
        return (s, s + piece[..to].trim_end().len());
    }
    let w_start = from + (piece[from..to].len() - piece[from..to].trim_start().len());
    (s + w_start, s + w_start + w.len())
}

/// The byte offset of `word` as a whole word in `hay`.
fn find_word(hay: &str, word: &str) -> Option<usize> {
    let b = hay.as_bytes();
    let mut from = 0;
    while let Some(i) = hay[from..].find(word) {
        let s = from + i;
        let e = s + word.len();
        if (s == 0 || !b[s - 1].is_ascii_alphanumeric()) && (e >= b.len() || !b[e].is_ascii_alphanumeric()) {
            return Some(s);
        }
        from = s + 1;
        while from < hay.len() && !hay.is_char_boundary(from) {
            from += 1;
        }
    }
    None
}

/// The excerpt a trimmed pointer carries for `query`, or None when the
/// record's first line serves as well (no clause holds a prompt term, or the
/// best one is the start of the first line): [`clauses`], less that case.
pub fn excerpt(h: &Head, query: &str, max: usize) -> Option<Excerpt> {
    clauses(h, query, max, 0).filter(|(_, head)| !*head).map(|(x, _)| x)
}

/// The clauses of a record that match `query` best, and whether they are
/// just the start of its first line; None when no clause holds a prompt
/// term (or the record is a memory, whose title is its summary).
///
/// The best clause is the one holding the most distinct prompt terms, ties
/// to the earlier; a dated lead (see [`lead`]) is kept before it; a second
/// clause is added when it also holds a term and fits. At most `max` chars
/// of record text in all. A clause that starts before byte `from` of the
/// text is not a candidate (the lane dump passes the end of a title the
/// body opens with, which is already shown).
pub fn clauses(h: &Head, query: &str, max: usize, from: usize) -> Option<(Excerpt, bool)> {
    let ts = terms(query);
    if ts.is_empty() {
        return None;
    }
    // Memory titles are hand-written one-line summaries: kept.
    if h.kind == "memory" {
        return None;
    }
    let (part, text) = if h.body.trim().is_empty() { (Part::Title, h.title.as_str()) } else { (Part::Body, h.body.as_str()) };
    let segs = segments(text);
    let scored: Vec<(usize, (usize, usize))> = segs.iter().map(|&sp| (if sp.0 < from { 0 } else { matched(&text[sp.0..sp.1], &ts) }, sp)).collect();
    let best = scored.iter().enumerate().max_by(|a, b| a.1 .0.cmp(&b.1 .0).then(b.0.cmp(&a.0)))?;
    let (best_i, &(best_n, best_sp)) = best;
    if best_n == 0 {
        return None;
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut left = max;
    if best_i > 0 {
        if let Some(l) = lead(text) {
            let n = text[l.0..l.1].chars().count();
            if n + 24 < left && l.0 >= from && l.1 <= best_sp.0 {
                spans.push(l);
                left -= n + 1;
            }
        }
    }
    let b = window(text, best_sp, &ts, left);
    left = left.saturating_sub(text[b.0..b.1].chars().count());
    spans.push(b);
    // A second clause holding a term, when it fits whole.
    if let Some(&(_, sp)) = scored.iter().enumerate().filter(|(i, (n, _))| *i != best_i && *n > 0).map(|(_, x)| x).max_by(|a, b| a.0.cmp(&b.0).then(b.1 .0.cmp(&a.1 .0))) {
        let n = text[sp.0..sp.1].chars().count();
        if n + 5 <= left && !spans.iter().any(|&(s, e)| s < sp.1 && sp.0 < e) {
            spans.push(sp);
        }
    }
    spans.sort_unstable();
    let x = Excerpt { part, spans };
    let head = x.spans.len() == 1 && segs.first().is_some_and(|f| f.0 == x.spans[0].0);
    Some((x, head))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review finding 6: `İ` grows by a byte when lowercased and the Kelvin
    /// sign shrinks by two, so the totals agree while the offsets before the
    /// term shift; the offset then landed inside `é` and the slice panicked.
    #[test]
    fn a_window_survives_lowercasing_that_moves_offsets() {
        let piece = format!("\u{212A} x\u{e9} term \u{130}\u{130} {}", "pad ".repeat(40));
        assert_eq!(piece.len(), piece.to_lowercase().len(), "the totals agree");
        let ts = vec!["term".to_string()];
        let (s, e) = window(&piece, (0, piece.len()), &ts, 12);
        let w = &piece[s..e];
        assert!(w.contains("term"), "{w:?}");
        assert!(w.starts_with("x\u{e9}"), "one word of context before the term: {w:?}");
        // A term after a char that lowercases to more bytes.
        let piece = format!("\u{130}\u{130}\u{130} alpha term {}", "pad ".repeat(40));
        let (s, e) = window(&piece, (0, piece.len()), &ts, 12);
        assert!(piece[s..e].starts_with("alpha term"), "{:?}", &piece[s..e]);
    }

    fn head(kind: &str, title: &str, body: &str) -> Head {
        Head {
            rid: 1,
            key: "intent:r00001".into(),
            kind: kind.into(),
            sub: String::new(),
            scope: String::new(),
            status: "active".into(),
            seq: 1,
            time: "2026-11-02T10:11:12Z".into(),
            title: title.into(),
            meta: String::new(),
            body: body.into(),
            sha: "0".repeat(64),
            chain: String::new(),
            superseded_by: None,
            superseded_time: None,
            anchor: None,
            anchor_src: None,
        }
    }

    #[test]
    fn segments_cut_at_sentences_and_lines() {
        let t = "- 2026-10-30 Lead: first clause is long enough here. Second sentence follows it; a clause after a semicolon.\nNext line, e.g. this one.";
        let s: Vec<&str> = segments(t).iter().map(|&(a, b)| &t[a..b]).collect();
        assert_eq!(s, vec!["2026-10-30 Lead: first clause is long enough here.", "Second sentence follows it;", "a clause after a semicolon.", "Next line, e.g. this one."]);
        assert!(segments("").is_empty());
        assert!(segments(" \n\r\n ").is_empty());
    }

    #[test]
    fn the_excerpt_is_the_matching_clause_with_its_lead() {
        let body = "2026-11-02 Brandon: the opening says nothing about it at all. The wombat budget is 42 tokens per prompt. Unrelated tail sentence here.";
        let h = head("rule", "2026-11-02 Brandon: the opening says nothing", body);
        let x = excerpt(&h, "what is the wombat budget", 90).unwrap();
        assert_eq!(x.render(&h), "2026-11-02 Brandon: ... The wombat budget is 42 tokens per prompt. ...");
        // The first clause already matching: no trim.
        assert!(excerpt(&h, "opening says", 90).is_none());
        // No term matched: no trim.
        assert!(excerpt(&h, "zebra giraffe", 90).is_none());
    }

    #[test]
    fn overlap_is_the_share_of_the_part_the_whole_holds() {
        assert_eq!(overlap("alpha beta gamma delta epsilon", "alpha beta gamma"), 1.0);
        assert!((overlap("alpha beta gamma", "alpha beta gamma delta epsilon") - 0.6).abs() < 1e-6, "one way");
        assert!((overlap("alpha beta gamma delta", "alpha beta zeta theta") - 0.5).abs() < 1e-6);
        assert_eq!(overlap("alpha", ""), 0.0);
        assert_eq!(overlap("", "alpha"), 0.0);
    }

    /// A seeded xorshift64* generator.
    struct Rng(u64);
    impl Rng {
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
    }

    /// Whether `out` is exactly `pieces` in order, each after a marker (` ... `
    /// or a space; none or `... ` before the first), with none or ` ...`
    /// after the last. Tried every way, as record text may hold the
    /// markers itself.
    fn markers_then_pieces(out: &str, pieces: &[&str]) -> bool {
        fn go(out: &str, pieces: &[&str], markers: &[&str]) -> bool {
            let Some((p, rest)) = pieces.split_first() else { return out.is_empty() || out == " ..." };
            markers.iter().any(|m| out.strip_prefix(m).and_then(|o| o.strip_prefix(p)).is_some_and(|o| go(o, rest, &[" ... ", " "])))
        }
        !pieces.is_empty() && go(out, pieces, &["", "... "])
    }

    const PIECES: &[&str] = &[
        "wombat", "budget", "tokens", "prompt", "gate", "the", "a", "2026-11-02", "Brandon:", ": ", ".", ". ", "; ", "? ", "! ", "\n", "\r\n", "- ",
        "* ", " ", "  ", "\u{e9}", "\u{130}", "\u{1f980}", "\u{2028}", "\u{85}", "e.g.", "v0.2", "...", "#123", "x", "\t", "\u{a0}", "WOMBAT",
    ];

    fn random_text(r: &mut Rng, n: usize) -> String {
        (0..r.below(n)).map(|_| PIECES[r.below(PIECES.len())]).collect()
    }

    /// The verbatim property: every span is a char-bounded range of the
    /// record's text, the spans are ascending and disjoint and hold no line
    /// break, and the rendered pointer is those spans joined only by the
    /// markers.
    #[test]
    fn excerpts_are_exact_substrings_of_the_record() {
        let mut r = Rng(0x5eed_1234_abcd_ef01);
        let mut trimmed = 0;
        for case in 0..5000 {
            let body = random_text(&mut r, 120);
            let title = random_text(&mut r, 12);
            let h = head(if case % 7 == 0 { "memory" } else { "rule" }, &title, &body);
            let q = random_text(&mut r, 8) + " wombat budget";
            let max = 10 + r.below(150);
            for &(s, e) in &segments(&body) {
                assert!(body.is_char_boundary(s) && body.is_char_boundary(e) && s < e, "{body:?}");
                assert!(!body[s..e].contains(is_break), "{body:?}");
            }
            let Some(x) = excerpt(&h, &q, max) else { continue };
            trimmed += 1;
            let src = x.source(&h);
            let mut prev = 0;
            let mut chars = 0;
            for &(s, e) in &x.spans {
                assert!(s >= prev && s < e && e <= src.len(), "{src:?} {x:?}");
                assert!(src.is_char_boundary(s) && src.is_char_boundary(e));
                assert!(!src[s..e].contains(is_break));
                assert!(src.contains(&src[s..e]));
                chars += src[s..e].chars().count();
                prev = e;
            }
            assert!(chars <= max, "{chars} > {max}: {src:?} {x:?}");
            // The rendered text, markers removed, is the spans in order.
            let out = x.render(&h);
            let pieces: Vec<&str> = x.spans.iter().map(|&(s, e)| &src[s..e]).collect();
            assert!(markers_then_pieces(&out, &pieces), "{out:?} is not {pieces:?} with markers");
        }
        assert!(trimmed > 500, "the property ran on {trimmed} excerpts only");
    }
}
