//! Chained compaction keys (journals ATLAS-V03 0199-0208): the card a
//! compaction leaves names every part of the segment it drops by a key that
//! one `recall` resolves from the transcript itself, and older segments are
//! reached through links. Nothing is stored: links, groups and replies are
//! derived from the transcript (append-only) each time.
//!
//! * **Records.** A segment's visible records: user and assistant records
//!   of the main chain (their text and tool-result blocks,
//!   [`crate::checkpoint::turn_text`]) and the `api-request-blob` records
//!   whose text no earlier record of the sub-segment already holds (by
//!   hash). Each passes [`crate::redact`]; the result of a call whose input
//!   names a secret is `[redacted]` whole.
//! * **Links.** A link is a boundary in the transcript: a real
//!   `compact_boundary` record, or a sub-segment link at the first
//!   main-chain user record after the context (`usage_ctx`) reaches
//!   k x `SPACING` inside one real segment (k = 1, 2, ...). A link's key is
//!   `compact:<s8>:b<offset>@hash`, the hash over its line's bytes.
//! * **Pieces and groups.** A sub-segment's records are packed in order
//!   into pieces of at most `ROOM` estimated tokens ([`est`]); a record
//!   larger than that is split at line boundaries into parts,
//!   `turn:<s8>:b<offset>#<k>@hash`. The pieces are split evenly into the
//!   most contiguous groups whose listing fits `CARD_CAP`: groups merge,
//!   no line is dropped. A group's key is the byte range of its records,
//!   `turn:<s8>:b<first>-b<last>@hash`, the hash over their visible text
//!   joined with `\n`.
//! * **Replies.** A group's reply is its key, a digest of the entities of
//!   its later pieces (at most `DIGEST`), the later pieces' keys, then the
//!   first piece raw: at most [`CAP`] estimated tokens, 8,000 counted. A
//!   link's reply is the card of the sub-segment behind it, that
//!   sub-segment's digest and its own last `BACK` links.
//! * **Depth.** The card and each link reply print `chain_depth: N` (the
//!   links before the session's head) and `chain_index: I` (this link's
//!   position from the session start).
//!
//! A key names its session by a prefix of the session id, the transcript
//! file `<session>.jsonl` of the asking project's folder. A card prints the
//! short form, a [`SHORT`]-character prefix and a [`SHORT_HASH`]-digit hash
//! (0242); the long form of earlier cards, an 8-character prefix and a
//! 12-digit hash, still resolves exactly as before. Transcripts sharing a
//! prefix are told apart by the hash. A key whose session is in another
//! project's folder does not resolve.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::error::{Error, Result};
use crate::json::Value;

/// One reply's cap, in [`est`] tokens: 8,000 counted with a 5% margin.
pub const CAP: usize = 7600;
/// The digest's reserve in a reply.
const DIGEST: usize = 1500;
/// The header's reserve in a reply.
const HEAD: usize = 100;
/// The raw room in a reply: one piece.
const ROOM: usize = CAP - DIGEST - HEAD;
/// The group listing's cap on a card.
const CARD_CAP: usize = 2800;
/// The context spacing of sub-segment links.
const SPACING: u64 = 180_000;
/// The links a card or link reply prints, newest first.
const BACK: usize = 3;

const HASH: usize = crate::compact::HASH_LEN;
/// The session prefix a card's keys are written with.
const SHORT: usize = 4;
/// The hash digits of a short-form key; also the fewest a chain key may be
/// written with (plain refs keep [`crate::compact::MIN_HASH`]).
const SHORT_HASH: usize = 6;

/// The hash digits a key with session prefix `s8` is written with: the
/// long form (an 8-character prefix) keeps all [`HASH`], so the keys an old
/// key's reply prints are as before.
fn key_hash(s8: &str, h12: &str) -> String {
    h12[..if s8.len() >= 8 { HASH } else { SHORT_HASH }].to_string()
}

// ---- the reply-cap estimator

fn word_est(w: &[u8]) -> usize {
    if w.iter().any(u8::is_ascii_digit) && w.iter().any(u8::is_ascii_alphabetic) {
        return (w.len() * 5).div_ceil(7);
    }
    let (mut n, mut i) = (0, 0);
    while i < w.len() {
        let c = w[i];
        let run = |i: usize, f: fn(&u8) -> bool| i + w[i..].iter().take_while(|b| f(b)).count();
        if c.is_ascii_uppercase() && w.get(i + 1).is_some_and(u8::is_ascii_lowercase) {
            let j = run(i + 1, u8::is_ascii_lowercase);
            n += (j - i).div_ceil(3);
            i = j;
        } else if c.is_ascii_lowercase() {
            let j = run(i, u8::is_ascii_lowercase);
            n += (j - i).div_ceil(3);
            i = j;
        } else if c.is_ascii_uppercase() {
            let j = run(i, u8::is_ascii_uppercase);
            n += (j - i).div_ceil(2);
            i = j;
        } else {
            let j = run(i, u8::is_ascii_digit);
            n += (j - i).div_ceil(2);
            i = j;
        }
    }
    n
}

/// An upper bound on the model's token count for transcript text (probed
/// against the real counter, journal 0200; random base64 is the one class
/// it undercounts). An ASCII alphanumeric word holding a letter and a
/// digit costs ceil(len * 5 / 7); any other word is split into runs (lower,
/// upper, digits; a capitalised word is one lower run) costing ceil(len /
/// 3), ceil(len / 2) and ceil(len / 2). ASCII punctuation, a newline or a
/// tab costs 1; a space after a non-space is free, further spaces cost
/// ceil(run / 4); a non-ASCII char costs its UTF-8 bytes.
///
/// It exists for the reply cap only (sizing pieces, groups and replies).
pub fn est(t: &str) -> usize {
    let b = t.as_bytes();
    let (mut n, mut i) = (0, 0);
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphanumeric() {
            let j = i + b[i..].iter().take_while(|x| x.is_ascii_alphanumeric()).count();
            n += word_est(&b[i..j]);
            i = j;
        } else if c == b' ' {
            let j = i + b[i..].iter().take_while(|x| **x == b' ').count();
            let after_text = i > 0 && !t[..i].chars().next_back().is_some_and(char::is_whitespace);
            let extra = if after_text { j - i - 1 } else { j - i };
            n += extra.div_ceil(4);
            i = j;
        } else if c.is_ascii() {
            n += 1;
            i += 1;
        } else {
            let k = t[i..].chars().next().map_or(1, char::len_utf8);
            n += k;
            i += k;
        }
    }
    n
}

// ---- the extractor: what a model needs to act, class by class

const CLASSES: [(&str, &str); 8] = [
    ("path", r"(?:[A-Za-z]:)?(?:[\w.\-]+[/\\])+[\w.\-]+"),
    ("file", r"\b[\w\-]+\.(?:rs|md|py|toml|json|jsonl|ts|tsx|js|sh|ps1|txt|yaml|yml|wgsl|glsl|hlsl|png|jpg|blend|glb|fbx|sqlite|log|csv|html|lock|cfg|ron)\b"),
    ("sha", r"\b[0-9a-f]{7,40}\b"),
    ("uuid", r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"),
    ("issue", r"#\d{1,6}\b"),
    ("key", r"\b[a-z][\w\-]*:[\w\-]+(?::[\w\-.]+)+"),
    ("symbol", r"\b(?:[A-Za-z_]\w*::)+\w+|\b[a-z]+(?:_[a-z0-9]+)+\b|\b[A-Z][A-Z0-9]+(?:_[A-Z0-9]+)+\b|\b[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+\b"),
    ("number", r"\b\d[\d.,]*\s?(?:ms|us|ns|s|KB|KiB|MB|MiB|GB|k|M|%|x|px|fps|tokens|bytes|B)\b|\b\d{4}-\d{2}-\d{2}\b|\b\d[\d.,]*-[A-Za-z]+\b"),
];

fn classes() -> &'static [crate::regex::Regex] {
    static RX: OnceLock<Vec<crate::regex::Regex>> = OnceLock::new();
    RX.get_or_init(|| CLASSES.iter().map(|(_, p)| crate::regex::Regex::new(p).expect("a fixed extractor pattern")).collect())
}

/// The entities of `text`, class by class in [`CLASSES`] order, each class
/// in text order (repeats kept). A sha needs a digit and a letter.
fn entities(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for (k, rx) in classes().iter().enumerate() {
        for m in rx.find_iter(text) {
            let e = m.as_str();
            if CLASSES[k].0 == "sha" && !(e.bytes().any(|b| b.is_ascii_digit()) && e.bytes().any(|b| b.is_ascii_lowercase())) {
                continue;
            }
            out.push(e);
        }
    }
    out
}

/// A digest: the entities of `text` not in `skip` (or seen), class by
/// class, as many as fit `cap` estimated tokens (one more each for its
/// separator). Stops at the first that does not fit.
fn entity_digest(text: &str, skip: &str, cap: usize) -> String {
    let mut seen = std::collections::HashSet::new();
    let (mut out, mut n) = (Vec::new(), 0);
    for e in entities(text) {
        if !seen.insert(e) || skip.contains(e) {
            continue;
        }
        let m = est(e) + 1;
        if n + m > cap {
            break;
        }
        out.push(e);
        n += m;
    }
    if out.is_empty() { String::new() } else { format!("digest: {}\n", out.join(" ")) }
}

// ---- records

/// One visible record.
#[derive(Debug, Clone)]
pub struct Rec {
    /// Its byte offset in the transcript.
    pub off: u64,
    /// Its visible text (what the hash covers).
    pub vis: String,
    /// The text a reply prints: redacted.
    pub text: String,
}

fn has(hay: &[u8], needle: &str) -> bool {
    crate::scan::find(hay, needle.as_bytes()).is_some()
}

fn hash12(b: &[u8]) -> String {
    crate::sha256::hex(b)[..HASH].to_string()
}

/// The visible records of transcript bytes `seg` (starting at byte `base`
/// of the file), which start a sub-segment: a blob whose text an earlier
/// record of it holds is left out.
pub fn records(seg: &[u8], base: u64) -> Vec<Rec> {
    let deny = crate::redact::denylist();
    let mut secret = std::collections::HashSet::new();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < seg.len() {
        let end = crate::scan::memchr(b'\n', &seg[at..]).map_or(seg.len(), |j| at + j);
        let raw = &seg[at..end];
        let off = base + at as u64;
        at = end + 1;
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if has(raw, "\"isSidechain\":true") {
            continue;
        }
        let blob = raw.starts_with(b"{\"type\":\"api-request-blob\"");
        if !blob && !(has(raw, "\"type\":\"user\"") || has(raw, "\"type\":\"assistant\"")) {
            continue;
        }
        let Ok(v) = crate::json::parse(&String::from_utf8_lossy(raw)) else { continue };
        let blocks: &[Value] = v.pointer("/message/content").and_then(Value::as_array).map_or(&[], Vec::as_slice);
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                let input = b.get("input").cloned().unwrap_or(Value::Null).to_string();
                if input.lines().any(|x| crate::redact::is_secret_line(x, deny)) || crate::redact::is_secret_line(&input, deny) {
                    if let Some(id) = b.get("id").and_then(Value::as_str) {
                        secret.insert(id.to_string());
                    }
                }
            }
        }
        let vis = crate::checkpoint::turn_text(&v);
        if vis.trim().is_empty() {
            continue;
        }
        let h = crate::sha256::hex(vis.as_bytes());
        if !seen.insert(h) && blob {
            continue;
        }
        let sec = blocks.iter().any(|b| b.get("tool_use_id").and_then(Value::as_str).is_some_and(|id| secret.contains(id)));
        let text = if sec { crate::redact::REDACTED.to_string() } else { crate::redact::redact_with(&vis, deny) };
        out.push(Rec { off, vis, text });
    }
    out
}

// ---- pieces and groups

/// A piece: records `first..=last` packed whole, or one part of one record.
#[derive(Debug, Clone)]
struct Piece {
    /// Its first record.
    first: usize,
    /// Its last record (the same one for a part).
    last: usize,
    /// Its text, as a reply prints it.
    text: String,
    /// Its key.
    key: String,
}

/// The key of records `rs`: their byte range and the hash of their visible
/// text joined with `\n`.
fn range_key(s8: &str, rs: &[Rec]) -> String {
    let joined: Vec<&str> = rs.iter().map(|r| r.vis.as_str()).collect();
    format!("turn:{s8}:b{}-b{}@{}", rs[0].off, rs[rs.len() - 1].off, key_hash(s8, &hash12(joined.join("\n").as_bytes())))
}

fn split_long(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for ln in text.split('\n') {
        if est(ln) < ROOM {
            out.push(ln.to_string());
        } else {
            split_line(ln, &mut out);
        }
    }
    out
}

/// One line cut into chunks of at most `ROOM - 1` estimated tokens, in one
/// pass: a running [`est`] over its tokens (an alphanumeric run, a space
/// run, one other char), which equals the chunk's own estimate because a
/// space run's cost depends only on the char before it in the chunk. An
/// alphanumeric run too long for a chunk is cut into chunks of its own
/// (its estimate is at most its length).
fn split_line(ln: &str, out: &mut Vec<String>) {
    let max = ROOM - 1;
    let b = ln.as_bytes();
    let (mut start, mut n, mut i) = (0usize, 0usize, 0usize);
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_alphanumeric() {
            let j = i + b[i..].iter().take_while(|x| x.is_ascii_alphanumeric()).count();
            if j - i > max {
                if i > start {
                    out.push(ln[start..i].to_string());
                }
                let mut a = i;
                while j - a > max {
                    out.push(ln[a..a + max].to_string());
                    a += max;
                }
                (start, n, i) = (a, word_est(&b[a..j]), j);
                continue;
            }
            let m = word_est(&b[i..j]);
            if n + m > max && i > start {
                out.push(ln[start..i].to_string());
                (start, n) = (i, 0);
            }
            n += m;
            i = j;
        } else if c == b' ' {
            let j = i + b[i..].iter().take_while(|x| **x == b' ').count();
            let cost = |from: usize| {
                let after_text = i > from && !ln[..i].chars().next_back().is_some_and(char::is_whitespace);
                (if after_text { j - i - 1 } else { j - i }).div_ceil(4)
            };
            if n + cost(start) > max && i > start {
                out.push(ln[start..i].to_string());
                (start, n) = (i, 0);
            }
            n += cost(start);
            i = j;
        } else {
            let k = if c.is_ascii() { 1 } else { ln[i..].chars().next().map_or(1, char::len_utf8) };
            if n + k > max && i > start {
                out.push(ln[start..i].to_string());
                (start, n) = (i, 0);
            }
            n += k;
            i += k;
        }
    }
    out.push(ln[start..].to_string());
}

/// Packs records into pieces of at most `ROOM` estimated tokens.
fn pieces(s8: &str, recs: &[Rec]) -> Vec<Piece> {
    let mut out = Vec::new();
    let (mut start, mut used) = (0usize, 0usize);
    let flush = |out: &mut Vec<Piece>, a: usize, b: usize| {
        if a < b {
            let text: String = recs[a..b].iter().map(|r| format!("{}\n", r.text)).collect();
            out.push(Piece { first: a, last: b - 1, text, key: range_key(s8, &recs[a..b]) });
        }
    };
    for (i, r) in recs.iter().enumerate() {
        let n = est(&r.text) + 1;
        if n > ROOM {
            flush(&mut out, start, i);
            let h = key_hash(s8, &hash12(r.vis.as_bytes()));
            let (mut buf, mut u, mut k) = (Vec::<String>::new(), 0, 1);
            for ln in split_long(&r.text) {
                let m = est(&ln) + 1;
                if u + m > ROOM && !buf.is_empty() {
                    out.push(Piece { first: i, last: i, text: buf.join("\n") + "\n", key: format!("turn:{s8}:b{}#{k}@{h}", r.off) });
                    buf.clear();
                    u = 0;
                    k += 1;
                }
                buf.push(ln);
                u += m;
            }
            if !buf.is_empty() {
                out.push(Piece { first: i, last: i, text: buf.join("\n") + "\n", key: format!("turn:{s8}:b{}#{k}@{h}", r.off) });
            }
            start = i + 1;
            used = 0;
            continue;
        }
        if used + n > ROOM {
            flush(&mut out, start, i);
            start = i;
            used = 0;
        }
        used += n;
    }
    flush(&mut out, start, recs.len());
    out
}

/// A record's first three distinct file names (the heading's candidates).
fn names(text: &str) -> Vec<String> {
    let head: String = text.chars().take(2000).collect();
    let mut out: Vec<String> = Vec::new();
    for e in entities(&head) {
        let e = e.replace('\\', "/");
        let e = e.trim_end_matches('/');
        let n = e.rsplit('/').next().unwrap_or(e);
        if n.contains('.') && n.len() > 3 && n.len() <= 40 && !out.iter().any(|x| x == n) {
            out.push(n.to_string());
            if out.len() >= 3 {
                break;
            }
        }
    }
    out
}

fn heading(cands: &[Vec<String>], n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in cands {
        for x in c {
            if !out.contains(x) {
                out.push(x.clone());
            }
            if out.len() >= n {
                return out;
            }
        }
    }
    out
}

/// The groups of a sub-segment: piece index ranges, as even as integer
/// division allows.
fn split(n: usize, g: usize) -> Vec<(usize, usize)> {
    (0..g).map(|i| (i * n / g, (i + 1) * n / g)).filter(|(a, b)| a < b).collect()
}

/// Groups `recs` into the most groups whose listing fits `CARD_CAP`,
/// then checks each group's reply against [`CAP`] with its own `next:`
/// line counted ([`group_est`]); where a group's reply would not fit, more
/// groups are taken (the reply cap is the tool's limit, so it wins over
/// the card's).
fn listing(s8: &str, recs: &[Rec]) -> String {
    listing_within(s8, recs, CARD_CAP)
}

/// [`listing`] under a card cap of `card_cap` (tests use a tight one).
fn listing_within(s8: &str, recs: &[Rec], card_cap: usize) -> String {
    let ps = pieces(s8, recs);
    let cands: Vec<Vec<String>> = recs.iter().map(|r| names(&r.text)).collect();
    let build = |g: usize| {
        let mut groups = Vec::new();
        let mut text = String::from("groups (recall a key for its digest and text):\n");
        for (a, b) in split(ps.len(), g) {
            let key = if b - a == 1 { ps[a].key.clone() } else { range_key(s8, &recs[ps[a].first..=ps[b - 1].last]) };
            let first = ps[a].first;
            let last = (first + 6).min(ps[b - 1].last + 1);
            let h = heading(&cands[first..last], 3);
            let head: String = h.first().map_or(String::new(), |x| x.chars().take(28).collect());
            text.push_str(&if head.is_empty() { format!("- {key}\n") } else { format!("- {key} {head}\n") });
            groups.push((a, b, key));
        }
        (groups, text)
    };
    if ps.is_empty() {
        return String::new();
    }
    let mut g = ps.len();
    let (mut groups, mut text) = build(g);
    while g > 1 && est(&text) > card_cap {
        g -= 1;
        (groups, text) = build(g);
    }
    while g < ps.len() && groups.iter().any(|(a, b, key)| group_bound(s8, recs, &ps, *a, *b, key) > CAP) {
        g += 1;
        (groups, text) = build(g);
    }
    text
}

/// A group reply's header.
fn group_head(key: &str, ps: &[Piece]) -> String {
    format!("{key} {} records (current)\n", ps[ps.len() - 1].last + 1 - ps[0].first)
}

/// A group's `next:` line: its later pieces' keys (none for one piece).
fn next_line(ps: &[Piece]) -> String {
    if ps.len() < 2 {
        return String::new();
    }
    format!("next: {}\n", ps[1..].iter().map(|p| p.key.as_str()).collect::<Vec<_>>().join(" "))
}

/// An upper bound on a group's reply in [`est`] tokens: its header, the
/// digest's reserve (and the line's own `digest: ` and newline), its
/// `next:` line and its first piece.
fn group_est(key: &str, ps: &[Piece]) -> usize {
    if ps.len() < 2 {
        // One piece: its key's reply is that piece, with no digest.
        return est(&group_head(key, ps)) + est(&ps[0].text);
    }
    est(&group_head(key, ps)) + DIGEST + 4 + est(&next_line(ps)) + est(&ps[0].text)
}

/// An upper bound on the reply to group `ps[a..b]`'s key `key`. A group of
/// more than one piece is keyed by its records' range, and the resolver
/// re-packs that range whole: where the group starts or ends inside a
/// record's parts, the reply holds all of that record's parts, so the
/// bound is taken on the pieces the resolver builds.
fn group_bound(s8: &str, recs: &[Rec], ps: &[Piece], a: usize, b: usize, key: &str) -> usize {
    let part = |p: &Piece| p.key.contains('#');
    if b - a > 1 && (part(&ps[a]) || part(&ps[b - 1])) {
        group_est(key, &pieces(s8, &recs[ps[a].first..=ps[b - 1].last]))
    } else {
        group_est(key, &ps[a..b])
    }
}

/// A group's reply body (after its header): the digest of its later
/// pieces, their keys, then its first piece raw.
/// The digest line's byte range is returned beside it (`None` when the
/// digest is empty).
fn group_body(ps: &[Piece]) -> (String, Option<std::ops::Range<usize>>) {
    let raw = &ps[0].text;
    let rest: String = ps[1..].iter().map(|p| p.text.as_str()).collect();
    let mut out = entity_digest(&rest, raw, DIGEST);
    let d = (!out.is_empty()).then_some(0..out.len());
    out.push_str(&next_line(ps));
    out.push_str(raw);
    (out, d)
}

// ---- links

/// One link.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    /// The byte offset of its line.
    off: u64,
    /// The byte just past its line's newline.
    next: u64,
    /// A real compact_boundary (else a sub-segment link).
    real: bool,
    /// Its key.
    key: String,
    /// Its line's time.
    ts: String,
}

impl Link {
    /// Where the sub-segment after it starts: the record after a real
    /// boundary, or the sub-segment link's own record.
    fn starts(&self) -> u64 {
        if self.real { self.next } else { self.off }
    }
}

/// A main-chain assistant line's context, read from its last `"usage":{`
/// object without parsing the record.
fn ctx_fast(raw: &[u8]) -> Option<u64> {
    let pat = b"\"usage\":{";
    let at = raw.windows(pat.len()).rposition(|w| w == pat)? + pat.len() - 1;
    let mut depth = 0i32;
    let mut end = raw.len();
    for (i, &c) in raw[at..].iter().enumerate() {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = at + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let v = crate::json::parse(&String::from_utf8_lossy(&raw[at..end])).ok()?;
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens") + n("output_tokens"))
}

fn ts_fast(raw: &[u8]) -> String {
    let pat = b"\"timestamp\":\"";
    raw.windows(pat.len())
        .rposition(|w| w == pat)
        .map(|i| {
            let s = &raw[i + pat.len()..];
            String::from_utf8_lossy(&s[..s.iter().position(|b| *b == b'"').unwrap_or(0)]).into_owned()
        })
        .unwrap_or_default()
}

/// How far into the sub-segment behind a link its heading is looked for.
const HEADING_BYTES: u64 = 4 << 20;

/// How much of a record's start the link scan searches for its own keys.
const HEAD_BYTES: usize = 512;

/// What one line means to the link rule.
enum Ev {
    /// A real boundary: its line's offset, length (no newline) and bytes'
    /// hash, and its time.
    Boundary(u64, u64, String, String),
    /// A main-chain request's context.
    Ctx(u64),
    /// A main-chain user record: its offset and length.
    User(u64, u64),
}

/// A line's event, if any.
fn event(raw: &[u8], off: u64) -> Option<Ev> {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    if raw.starts_with(b"{\"type\":\"") && !(raw.starts_with(b"{\"type\":\"user\"") || raw.starts_with(b"{\"type\":\"assistant\"")) {
        return None;
    }
    // A record's own keys come first (parentUuid, isSidechain, its type or
    // subtype, a message's role): only the head is searched, so a long
    // record costs its newline search and little more.
    let head = &raw[..raw.len().min(HEAD_BYTES)];
    if has(head, "\"isSidechain\":true") {
        None
    } else if has(head, "\"subtype\":\"compact_boundary\"") {
        Some(Ev::Boundary(off, raw.len() as u64, hash12(raw), ts_fast(raw)))
    } else if has(head, "\"role\":\"assistant\"") {
        ctx_fast(raw).map(Ev::Ctx)
    } else if has(head, "\"type\":\"user\"") {
        Some(Ev::User(off, raw.len() as u64))
    } else {
        None
    }
}

/// The events of the lines that start in bytes `a..b` of a file, streamed.
fn events(path: &Path, a: u64, b: u64) -> Option<Vec<Ev>> {
    let mut f = std::fs::File::open(path).ok()?;
    // From the byte before `a`: a line starts at `a` only after a newline.
    let from = a.saturating_sub(1);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 4 << 20];
    let (mut have, mut at) = (0usize, from);
    let mut skip = a > 0;
    loop {
        if have == buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
        let k = f.read(&mut buf[have..]).ok()?;
        have += k;
        let mut start = 0;
        while let Some(j) = crate::scan::memchr(b'\n', &buf[start..have]) {
            let off = at + start as u64;
            if skip {
                skip = false;
            } else if off >= b {
                return Some(out);
            } else {
                out.extend(event(&buf[start..start + j], off));
            }
            start += j + 1;
        }
        if k == 0 {
            let off = at + start as u64;
            if start < have && !skip && off < b {
                out.extend(event(&buf[start..have], off));
            }
            return Some(out);
        }
        buf.copy_within(start..have, 0);
        have -= start;
        at += start as u64;
    }
}

/// Every link in a transcript up to byte `until`, streamed.
/// Lines starting at or past `until` are not read. A long file is read in
/// parallel slices (each its lines' events), then the rule runs over the
/// events in order.
fn links(path: &Path, s8: &str, until: u64) -> Option<Vec<Link>> {
    let until = until.min(std::fs::metadata(path).ok()?.len());
    let n = if until < (64 << 20) { 1 } else { std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as u64 };
    links_in(path, s8, until, n)
}

/// [`links`] over `n` slices.
fn links_in(path: &Path, s8: &str, until: u64, n: u64) -> Option<Vec<Link>> {
    let cuts: Vec<u64> = (0..=n).map(|i| until * i / n).collect();
    let parts: Vec<Option<Vec<Ev>>> = std::thread::scope(|s| {
        let hs: Vec<_> = cuts.windows(2).map(|w| s.spawn(move || events(path, w[0], w[1]))).collect();
        hs.into_iter().map(|h| h.join().ok().flatten()).collect()
    });
    let (mut k, mut pending, mut out) = (1u64, false, Vec::new());
    for ev in parts.into_iter().collect::<Option<Vec<_>>>()?.into_iter().flatten() {
        match ev {
            Ev::Boundary(off, len, h, ts) => {
                out.push(Link { off, next: off + len + 1, real: true, key: format!("compact:{s8}:b{off}@{}", key_hash(s8, &h)), ts });
                k = 1;
                pending = false;
            }
            Ev::Ctx(c) => {
                if c >= k * SPACING {
                    pending = true;
                    k = c / SPACING + 1;
                }
            }
            Ev::User(off, len) if pending => {
                let raw = read_range(path, off, off + len)?;
                let raw = raw.strip_suffix(b"\r").unwrap_or(&raw);
                out.push(Link { off, next: off + len + 1, real: false, key: format!("compact:{s8}:b{off}@{}", key_hash(s8, &hash12(raw))), ts: ts_fast(raw) });
                pending = false;
            }
            Ev::User(..) => {}
        }
    }
    Some(out)
}

fn read_range(path: &Path, a: u64, b: u64) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(a)).ok()?;
    let mut out = Vec::new();
    f.take(b.saturating_sub(a)).read_to_end(&mut out).ok()?;
    Some(out)
}

fn minute(ts: &str) -> &str {
    ts.get(..16).unwrap_or(ts)
}

/// The line a card or reply prints for link `i` of `ls`: its key, the time
/// range of the sub-segment behind it and the first file names there.
fn link_line(path: &Path, ls: &[Link], i: usize) -> String {
    let from = if i == 0 { 0 } else { ls[i - 1].starts() };
    let t0 = if i == 0 { String::new() } else { ls[i - 1].ts.clone() };
    // The heading: the first records' file names, read 256 KiB at a time
    // until two are found, at most HEADING_BYTES in.
    let mut cands = Vec::new();
    let (mut at, end) = (from, ls[i].off);
    while at < end && at < from + HEADING_BYTES && heading(&cands, 2).len() < 2 {
        let Some(buf) = read_range(path, at, (at + (256 << 10)).min(end)) else { break };
        // Whole lines only, unless one line fills the window.
        let n = if at + (buf.len() as u64) < end { buf.iter().rposition(|b| *b == b'\n').map_or(buf.len(), |j| j + 1) } else { buf.len() };
        // Turn records only (a blob repeats them), parsed one at a time.
        for raw in buf[..n].split(|b| *b == b'\n') {
            if heading(&cands, 2).len() >= 2 {
                break;
            }
            let head = &raw[..raw.len().min(HEAD_BYTES)];
            if raw.starts_with(b"{\"type\":\"") || has(head, "\"isSidechain\":true") || !(has(head, "\"type\":\"user\"") || has(head, "\"role\":\"assistant\"")) {
                continue;
            }
            if let Ok(v) = crate::json::parse(&String::from_utf8_lossy(raw)) {
                cands.push(names(&crate::redact::redact(&crate::checkpoint::turn_text(&v))));
            }
        }
        at += n.max(1) as u64;
    }
    let h = heading(&cands, 2).join(", ");
    let h: String = h.chars().take(48).collect();
    format!("- {} {}..{} {h}\n", ls[i].key, minute(&t0), ls[i].ts.get(11..16).unwrap_or(""))
}

fn links_block(path: &Path, ls: &[Link], before: usize) -> String {
    let back: Vec<usize> = (before.saturating_sub(BACK)..before).rev().collect();
    let mut out = String::new();
    if !back.is_empty() {
        out.push_str("links (recall a key for that segment's card and digest):\n");
    }
    for i in back {
        out.push_str(&link_line(path, ls, i));
    }
    out
}

/// The chain part of a card for a transcript as it stands: the current
/// sub-segment's group listing, the chain fields and the last `BACK`
/// links, its keys naming the session of the file's name. `None` when the
/// transcript cannot be read.
pub fn card(path: &Path) -> Option<String> {
    card_and_head(path).map(|(c, _)| c)
}

/// [`card`], and where the transcript's current sub-segment starts (its
/// last link's start, or 0), from one link scan.
pub fn card_and_head(path: &Path) -> Option<(String, u64)> {
    // The keys name the transcript read (a forked session's parent, when
    // its own is not written yet).
    let s8: String = path.file_stem()?.to_string_lossy().chars().take(SHORT).collect();
    let len = std::fs::metadata(path).ok()?.len();
    let ls = links(path, &s8, len)?;
    let from = ls.last().map_or(0, Link::starts);
    let seg = read_range(path, from, len)?;
    let l = listing(&s8, &records(&seg, from));
    let n = ls.len();
    Some((format!("{l}chain_depth: {n} chain_index: {n}\n{}", links_block(path, &ls, n)), from))
}

// ---- keys and the resolver

/// A chain key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Key {
    /// `turn:<s8>:b<a>-b<b>@hash`: the records from byte `a` to the record
    /// at byte `b`.
    #[allow(missing_docs)]
    Range { s8: String, a: u64, b: u64, hash: String },
    /// `turn:<s8>:b<off>#<k>@hash`: part `k` of the record at `off`.
    #[allow(missing_docs)]
    Part { s8: String, off: u64, k: usize, hash: String },
    /// `compact:<s8>:b<off>@hash`: a link.
    #[allow(missing_docs)]
    Link { s8: String, off: u64, hash: String },
}

fn s8_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 8 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn byte(s: &str) -> Option<u64> {
    s.strip_prefix('b').filter(|x| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit()))?.parse().ok()
}

/// Parses a chain key; `None` for any other ref (a plain turn ref is not
/// one).
pub(crate) fn parse_key(s: &str) -> Option<Key> {
    let (name, hash) = s.rsplit_once('@')?;
    if hash.len() < SHORT_HASH || hash.len() > 64 || !hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let hash = hash.to_string();
    if let Some(rest) = name.strip_prefix("compact:") {
        let (s8, at) = rest.split_once(':')?;
        if !s8_ok(s8) {
            return None;
        }
        return Some(Key::Link { s8: s8.to_string(), off: byte(at)?, hash });
    }
    let rest = name.strip_prefix("turn:")?;
    let (s8, at) = rest.split_once(':')?;
    if !s8_ok(s8) {
        return None;
    }
    let s8 = s8.to_string();
    if let Some((a, b)) = at.split_once('-') {
        return Some(Key::Range { s8, a: byte(a)?, b: byte(b)?, hash });
    }
    if let Some((o, k)) = at.split_once('#') {
        return Some(Key::Part { s8, off: byte(o)?, k: k.parse().ok().filter(|k| *k > 0)?, hash });
    }
    None
}

/// Whether a written hash names a 12-digit one.
fn same(h12: &str, written: &str) -> bool {
    h12.starts_with(written) || written.starts_with(h12)
}

/// The transcripts of session `s8` in a project folder: every
/// `<session>.jsonl` there whose name starts with `s8`, sorted.
fn transcripts_in(dir: &Path, s8: &str) -> std::result::Result<Vec<PathBuf>, Miss> {
    let rd = std::fs::read_dir(dir).map_err(|e| Miss::NoTranscript(format!("no transcripts folder {}: {e}", dir.display())))?;
    let mut hits: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(s8) && n.ends_with(".jsonl") && e.path().is_file()
        })
        .map(|e| e.path())
        .collect();
    if hits.is_empty() {
        return Err(Miss::NoTranscript(format!("no transcript of session {s8} in this project ({})", dir.display())));
    }
    hits.sort();
    Ok(hits)
}

/// Why a chain key does not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Miss {
    /// The text is not a chain key.
    NotAKey(String),
    /// The key's transcript is not in the project folder, or cannot be read.
    NoTranscript(String),
    /// The transcript is there, but the key no longer names what its hash
    /// covers (a misquoted or stale key).
    Dangling(String),
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Miss::NotAKey(m) | Miss::NoTranscript(m) | Miss::Dangling(m) => f.write_str(m),
        }
    }
}

impl From<Miss> for Error {
    fn from(m: Miss) -> Error {
        match m {
            Miss::NotAKey(s) => Error::InvalidArgument(s),
            Miss::NoTranscript(s) | Miss::Dangling(s) => Error::NotFound(s),
        }
    }
}

fn dangling(key: &str, why: &str) -> Miss {
    Miss::Dangling(format!("ref {key:?} is dangling: {why}"))
}

fn unreadable(key: &str) -> Miss {
    Miss::NoTranscript(format!("ref {key:?}: its transcript cannot be read"))
}

/// The sub-segment holding byte `at`: its start, and its end (the next
/// link's start, or `len`).
fn sub_segment(ls: &[Link], at: u64, len: u64) -> (u64, u64) {
    let i = ls.partition_point(|l| l.starts() <= at);
    let from = if i == 0 { 0 } else { ls[i - 1].starts() };
    let to = ls.get(i).map_or(len, |l| l.off);
    (from, to)
}

/// One recall of a chain key against project folder `dir`: the reply, at
/// most [`CAP`] estimated tokens.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for text that is not a chain key, and
/// [`Error::NotFound`] when the key's transcript is not in `dir` or the key
/// no longer names what its hash covers.
pub fn resolve(dir: &Path, key: &str) -> Result<String> {
    Ok(resolve_parts(dir, key)?.capped())
}

/// A reply before the cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The reply's text, uncapped.
    pub text: String,
    /// The byte range in `text` of the reply's own `digest: ` line, newline
    /// included (`None` when it has none).
    pub digest: Option<std::ops::Range<usize>>,
}

impl Reply {
    /// The text as [`resolve`] returns it: cut to [`CAP`] at a line, with a
    /// `[cut]` line, when it is over.
    pub fn capped(&self) -> String {
        cap(self.text.clone())
    }

    /// Whether the cap cuts this reply.
    pub fn is_cut(&self) -> bool {
        est(&self.text) > CAP
    }

    /// [`Reply::capped`] without the reply's own digest line (the "digest
    /// not credited" reading, 0199). Lines of the raw text that start with
    /// `digest: ` stay.
    pub fn capped_without_digest(&self) -> String {
        let mut s = self.capped();
        let kept = if self.is_cut() { s.len() - CUT.len() } else { s.len() };
        if let Some(r) = self.digest.clone().filter(|r| r.end <= kept) {
            s.replace_range(r, "");
        }
        s
    }
}

/// The reply to a chain key against project folder `dir`, uncapped, with
/// the place of its own digest line ([`resolve`] is [`Reply::capped`] over
/// it). Several transcripts of the folder may share the key's 8-character
/// session prefix: the one transcript the key's hash verifies in is read.
///
/// # Errors
///
/// A [`Miss`]: not a chain key; no transcript (none of the session in
/// `dir`, or one cannot be read); dangling (the key verifies in none of
/// them, or in more than one).
pub fn resolve_parts(dir: &Path, key: &str) -> std::result::Result<Reply, Miss> {
    let k = parse_key(key).ok_or_else(|| Miss::NotAKey(format!("{key:?} is not a chain key")))?;
    let s8 = match &k {
        Key::Range { s8, .. } | Key::Part { s8, .. } | Key::Link { s8, .. } => s8.clone(),
    };
    let mut found = Vec::new();
    let mut misses = Vec::new();
    for path in transcripts_in(dir, &s8)? {
        match reply_in(&path, key, &k, &s8) {
            Ok(r) => found.push(r),
            Err(m) => misses.push(m),
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(misses.iter().find(|m| matches!(m, Miss::NoTranscript(_))).unwrap_or(&misses[0]).clone()),
        n => Err(Miss::Dangling(format!("ref {key:?} verifies in {n} transcripts of session {s8}"))),
    }
}

/// [`resolve_parts`] in one transcript.
fn reply_in(path: &Path, key: &str, k: &Key, s8: &str) -> std::result::Result<Reply, Miss> {
    let len = std::fs::metadata(path).map_err(|_| unreadable(key))?.len();
    let ls = links(path, s8, len).ok_or_else(|| unreadable(key))?;
    let recs_of = |from: u64, to: u64| read_range(path, from, to).map(|b| records(&b, from)).ok_or_else(|| unreadable(key));
    let out = match k.clone() {
        Key::Link { off, hash, .. } => {
            let i = ls.iter().position(|l| l.off == off).ok_or_else(|| dangling(key, "no link starts there"))?;
            if !ls[i].key.rsplit_once('@').is_some_and(|(_, h)| same(h, &hash)) {
                return Err(dangling(key, "the link's line hashes otherwise"));
            }
            let from = if i == 0 { 0 } else { ls[i - 1].starts() };
            let recs = recs_of(from, off)?;
            let l = listing(s8, &recs);
            let all: String = recs.iter().map(|r| format!("{}\n", r.text)).collect();
            let t0 = if i == 0 { String::new() } else { ls[i - 1].ts.clone() };
            let mut text = format!("{key} {}..{} chain_depth: {} chain_index: {i}\n{l}", minute(&t0), minute(&ls[i].ts), ls.len());
            let d = entity_digest(&all, &l, DIGEST);
            let at = text.len();
            text.push_str(&d);
            let digest = (!d.is_empty()).then_some(at..text.len());
            text.push_str(&links_block(path, &ls, i));
            Reply { text, digest }
        }
        Key::Range { a, b, hash, .. } => {
            let (from, to) = sub_segment(&ls, a, len);
            if b < a || b >= to {
                return Err(dangling(key, "the range leaves its sub-segment"));
            }
            let recs = recs_of(from, to)?;
            let (i, j) = (recs.iter().position(|r| r.off == a), recs.iter().position(|r| r.off == b));
            let (Some(i), Some(j)) = (i, j) else { return Err(dangling(key, "no record starts at an end of the range")) };
            let rs = &recs[i..=j];
            if !range_key(s8, rs).rsplit_once('@').is_some_and(|(_, h)| same(h, &hash)) {
                return Err(dangling(key, "the records hash otherwise"));
            }
            let ps = pieces(s8, rs);
            let head = group_head(key, &ps);
            let (body, d) = group_body(&ps);
            Reply { digest: d.map(|r| r.start + head.len()..r.end + head.len()), text: head + &body }
        }
        Key::Part { off, k, hash, .. } => {
            let (from, to) = sub_segment(&ls, off, len);
            let recs = recs_of(from, to)?;
            let r = recs.iter().find(|r| r.off == off).ok_or_else(|| dangling(key, "no record starts there"))?;
            if !same(&hash12(r.vis.as_bytes()), &hash) {
                return Err(dangling(key, "the record hashes otherwise"));
            }
            let p = pieces(s8, std::slice::from_ref(r)).into_iter().find(|p| matches!(parse_key(&p.key), Some(Key::Part { k: pk, .. }) if pk == k)).ok_or_else(|| dangling(key, "no such part"))?;
            Reply { text: format!("{key} (current)\n{}", p.text), digest: None }
        }
    };
    Ok(out)
}

/// The line [`cap`] ends a cut reply with.
const CUT: &str = "[cut]\n";

/// The reply cap, enforced: a reply over [`CAP`] (none is, by
/// construction) is cut at a line.
fn cap(s: String) -> String {
    if est(&s) <= CAP {
        return s;
    }
    let mut out = String::new();
    let mut n = 0;
    for l in s.split_inclusive('\n') {
        let m = est(l);
        if n + m + 2 > CAP {
            break;
        }
        out.push_str(l);
        n += m;
    }
    out.push_str(CUT);
    out
}

/// The chain keys a text writes, in order, each once.
pub fn written_keys(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '#' | '@'))) {
        let w = w.trim_matches(|c: char| matches!(c, ':' | '-'));
        if parse_key(w).is_some() && !out.iter().any(|x| x == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// Whether a key names text of an older sub-segment than the one starting
/// at byte `cur`: a link always does.
pub fn older(key: &str, cur: u64) -> bool {
    match parse_key(key) {
        Some(Key::Link { .. }) => true,
        Some(Key::Range { a, .. }) => a < cur,
        Some(Key::Part { off, .. }) => off < cur,
        None => false,
    }
}

/// Claude Code's projects folder: `CLAUDE_CONFIG_DIR`, else `~/.claude`,
/// then `projects`.
pub fn projects_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = env("CLAUDE_CONFIG_DIR").or_else(|| env(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| h.join(".claude")))?;
    Some(base.join("projects"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn js(s: &str) -> String {
        Value::String(s.to_string()).to_string()
    }
    fn user(text: &str, ts: &str) -> String {
        format!(r#"{{"parentUuid":null,"isSidechain":false,"type":"user","message":{{"role":"user","content":{}}},"timestamp":"{ts}"}}"#, js(text))
    }
    fn assistant(text: &str, ctx: u64, ts: &str) -> String {
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"message":{{"role":"assistant","content":[{{"type":"text","text":{}}}],"usage":{{"input_tokens":{ctx},"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0,"cache_creation":{{"ephemeral_1h_input_tokens":0}}}}}},"type":"assistant","timestamp":"{ts}"}}"#,
            js(text)
        )
    }
    fn boundary(ts: &str) -> String {
        format!(r#"{{"parentUuid":null,"isSidechain":false,"type":"system","subtype":"compact_boundary","content":"Conversation compacted","timestamp":"{ts}"}}"#)
    }
    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atlas-chain-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn ts(m: u32) -> String {
        format!("2026-11-05T10:{m:02}:00.000Z")
    }

    /// Two sub-segment links in the first real segment, the boundary, one
    /// more after it.
    fn session() -> Vec<String> {
        vec![
            user("read src/start_here.rs", &ts(0)),
            assistant("reading src/start_here.rs", 100_000, &ts(1)),
            user("now open docs/first_link.md", &ts(2)),
            assistant("opened docs/first_link.md", 190_000, &ts(3)),
            user("next look at src/second_part.rs", &ts(4)),
            assistant("second_part.rs holds fn alpha_beta", 200_000, &ts(5)),
            user("and src/third_part.rs", &ts(6)),
            assistant("third_part.rs at 0123abc", 370_000, &ts(7)),
            user("then tools/fourth_part.py", &ts(8)),
            boundary(&ts(9)),
            user("after the compaction: src/fifth_part.rs", &ts(10)),
            assistant("fifth", 50_000, &ts(11)),
            assistant("fifth again", 200_000, &ts(12)),
            user("the head segment: src/head_only.rs", &ts(13)),
        ]
    }

    #[test]
    fn est_matches_the_python_estimator() {
        for (t, n) in [
            ("hello world", 4),
            ("src/chain.rs:42", 8),
            ("  indented\n    more", 8),
            ("HTTPServer x86_64 0123456789abcdef", 22),
            ("turn:43f55b2e:b1024-b2048@0123456789ab", 29),
            ("caf\u{e9}  ok\tdone", 8),
            ("CamelCase ABC def 12345", 10),
        ] {
            assert_eq!(est(t), n, "{t:?}");
        }
    }

    #[test]
    fn links_are_real_boundaries_and_context_crossings() {
        let d = dir("links");
        let p = d.join("aaaaaaaa-1111.jsonl");
        let lines = session();
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let ls = links(&p, "aaaaaaaa", u64::MAX).unwrap();
        // Read in slices, the events fold to the same links.
        for n in 2..=7 {
            assert_eq!(links_in(&p, "aaaaaaaa", std::fs::metadata(&p).unwrap().len(), n).unwrap(), ls, "{n} slices");
        }
        let off = |i: usize| lines[..i].iter().map(|l| l.len() as u64 + 1).sum::<u64>();
        // 190k crosses 180k: the next user record; 370k crosses 360k (k
        // jumps past it); the boundary; 200k after the boundary.
        let want: Vec<(u64, bool)> = vec![(off(4), false), (off(8), false), (off(9), true), (off(13), false)];
        assert_eq!(ls.iter().map(|l| (l.off, l.real)).collect::<Vec<_>>(), want);
        assert!(ls.iter().all(|l| l.key.starts_with("compact:aaaaaaaa:b")));
        let c = card(&p).unwrap();
        assert!(c.contains("chain_depth: 4 chain_index: 4"), "{c}");
        assert!(c.contains("head_only.rs"), "the head sub-segment's group heading: {c}");
        assert_eq!(c.matches("\n- compact:").count(), 3, "the last three links: {c}");
        assert!(!c.contains(&ls[0].key), "the oldest link is one walk away: {c}");
        // A link's reply: the sub-segment behind it (lines 4..8), its digest
        // and its own back-links.
        let r = resolve(&d, &ls[1].key).unwrap();
        assert!(r.contains("chain_depth: 4 chain_index: 1"), "{r}");
        assert!(r.contains(&ls[0].key) && !r.contains(&ls[2].key), "{r}");
        assert!(r.contains("second_part.rs") && !r.contains("fifth_part"), "{r}");
        // A group's reply returns its records' text.
        let g = c.lines().find(|l| l.starts_with("- turn:")).unwrap().split(' ').nth(1).unwrap().to_string();
        let r = resolve(&d, &g).unwrap();
        assert!(r.contains("the head segment: src/head_only.rs"), "{r}");
        // A changed hash is dangling.
        let bad = format!("{}@000000000000", g.rsplit_once('@').unwrap().0);
        assert!(matches!(resolve(&d, &bad), Err(Error::NotFound(_))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_record_larger_than_a_reply_splits_into_parts() {
        let d = dir("parts");
        let p = d.join("bbbbbbbb-2222.jsonl");
        let big: String = (0..3000).map(|i| format!("line{i} of a long tool output\n")).collect();
        std::fs::write(&p, [user("start", &ts(0)), user(&big, &ts(1)), user("end", &ts(2))].join("\n") + "\n").unwrap();
        let recs = records(&std::fs::read(&p).unwrap(), 0);
        let ps = pieces("bbbbbbbb", &recs);
        let parts: Vec<&Piece> = ps.iter().filter(|x| x.key.contains('#')).collect();
        assert!(parts.len() >= 2, "{}", ps.len());
        // Every byte of the record is in some part, each part under the room.
        let joined: String = parts.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(joined.replace('\n', ""), big.replace('\n', ""));
        assert!(ps.iter().all(|x| est(&x.text) <= ROOM + 1));
        let r = resolve(&d, &parts[1].key).unwrap();
        assert!(r.contains(parts[1].text.lines().next().unwrap()) && est(&r) <= CAP);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_key_resolves_only_inside_its_project() {
        let root = dir("projects");
        let (a, b) = (root.join("H--proj-a"), root.join("H--proj-b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let earlier = [user("alpha session: src/alpha_only.rs", &ts(0)), assistant("alpha", 190_000, &ts(1)), user("alpha tail", &ts(2))];
        let current = [user("beta session: src/beta_only.rs", &ts(3)), assistant("beta", 190_000, &ts(4)), user("beta tail", &ts(5))];
        std::fs::write(a.join("aaaaaaaa-1111.jsonl"), earlier.join("\n") + "\n").unwrap();
        std::fs::write(a.join("cccccccc-3333.jsonl"), current.join("\n") + "\n").unwrap();
        std::fs::write(b.join("dddddddd-4444.jsonl"), earlier.join("\n") + "\n").unwrap();
        let prev = links(&a.join("aaaaaaaa-1111.jsonl"), "aaaaaaaa", u64::MAX).unwrap().pop().unwrap().key;
        let head = links(&a.join("cccccccc-3333.jsonl"), "cccccccc", u64::MAX).unwrap().pop().unwrap().key;
        let other = links(&b.join("dddddddd-4444.jsonl"), "dddddddd", u64::MAX).unwrap().pop().unwrap().key;
        // 1. A link into another session of the same project resolves.
        assert!(resolve(&a, &prev).unwrap().contains("alpha_only.rs"));
        // 2. A session whose transcript is only in another project's folder
        // is refused.
        assert!(matches!(resolve(&a, &other), Err(Error::NotFound(_))));
        // 3. The head: this session's card and digest only.
        let r = resolve(&a, &head).unwrap();
        assert!(r.contains("beta_only.rs") && !r.contains("alpha"), "{r}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A record of about `ROOM - 2` estimated tokens whose words are distinct
    /// shas (digest entities), so a group's digest fills its reserve.
    fn full_record(i: usize) -> String {
        let mut t = format!("record {i} src/rec_{i}.rs\n");
        let mut k = 0usize;
        while est(&t) + 1 + 6 < ROOM - 1 {
            t.push_str(&format!("{:07x}a ", i * 100_000 + k));
            k += 1;
            if k.is_multiple_of(16) {
                t.push('\n');
            }
        }
        t
    }

    /// `n` full pieces, then a boundary and a short head segment.
    fn big_session(n: usize) -> Vec<String> {
        let mut v: Vec<String> = (0..n).map(|i| user(&full_record(i), &ts((i % 50) as u32))).collect();
        v.push(boundary(&ts(51)));
        v.push(user("the head: src/after.rs", &ts(52)));
        v
    }

    #[test]
    fn group_and_link_replies_stay_under_the_cap_uncut() {
        let d = dir("cap");
        let p = d.join("eeeeeeee-5555.jsonl");
        std::fs::write(&p, big_session(170).join("\n") + "\n").unwrap();
        let ls = links(&p, "eeeeeeee", u64::MAX).unwrap();
        assert_eq!(ls.len(), 1);
        let link = resolve_parts(&d, &ls[0].key).unwrap();
        assert!(!link.is_cut() && est(&link.text) <= CAP, "link reply {} est", est(&link.text));
        // Every group the link reply lists: a multi-piece group carries its
        // digest and next: line and stays under the cap.
        let groups: Vec<String> = link.text.lines().filter_map(|l| l.strip_prefix("- turn:")).map(|l| format!("turn:{}", l.split(' ').next().unwrap())).collect();
        assert!(groups.len() > 1);
        let mut multi = 0;
        for g in &groups {
            let r = resolve_parts(&d, g).unwrap();
            assert!(!r.is_cut() && est(&r.text) <= CAP && !r.capped().contains(CUT), "{g}: {} est", est(&r.text));
            if r.text.contains("\nnext: ") {
                multi += 1;
                assert!(r.digest.is_some(), "{g}: a full digest");
            }
        }
        assert!(multi > 0, "some group holds several pieces");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A card cap tight enough that the card alone would put 15 full pieces
    /// in a group: such a reply (a full digest, a 14-key `next:` line and a
    /// full first piece) is over CAP, past what HEAD reserves. The listing
    /// takes more groups until every group's reply fits.
    #[test]
    fn the_reply_cap_wins_over_the_card_cap() {
        let d = dir("cap-wins");
        let p = d.join("hhhhhhhh-8888.jsonl");
        std::fs::write(&p, big_session(60).join("\n") + "\n").unwrap();
        let recs = records(&std::fs::read(&p).unwrap(), 0);
        let l = listing_within("hhhhhhhh", &recs[..60], 160);
        let groups: Vec<String> = l.lines().filter_map(|l| l.strip_prefix("- turn:")).map(|l| format!("turn:{}", l.split(' ').next().unwrap())).collect();
        assert!(groups.len() > 1 && groups.len() < 60, "{} groups", groups.len());
        for g in &groups {
            let r = resolve_parts(&d, g).unwrap();
            assert!(!r.is_cut() && est(&r.text) <= CAP, "{g}: {} est", est(&r.text));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_the_replys_own_digest_line_is_stripped() {
        let d = dir("digest");
        let p = d.join("ffffffff-6666.jsonl");
        // Two full pieces then one whose raw text holds a `digest: ` line.
        let raw = "digest: from an earlier recall, raw text\nsrc/raw_line.rs";
        let lines = [user(&full_record(0), &ts(0)), user(raw, &ts(1)), user(&full_record(1), &ts(2))];
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let recs = records(&std::fs::read(&p).unwrap(), 0);
        // The group from the raw record: its own digest (of the later
        // piece) goes, the raw `digest: ` line stays.
        let key = range_key("ffffffff", &recs[1..]);
        let r = resolve_parts(&d, &key).unwrap();
        let d0 = r.digest.clone().expect("a digest of the later piece");
        assert!(r.text[d0.clone()].starts_with("digest: ") && r.text[d0].ends_with('\n'));
        let nd = r.capped_without_digest();
        assert!(nd.contains("digest: from an earlier recall") && nd.matches("digest: ").count() == 1, "{nd}");
        // A one-piece group has an empty digest: line 2 is raw text, kept.
        let one = range_key("ffffffff", &recs[1..2]);
        let r = resolve_parts(&d, &one).unwrap();
        assert_eq!(r.digest, None);
        assert_eq!(r.capped_without_digest(), r.capped());
        assert_eq!(r.text.lines().nth(1), Some("digest: from an earlier recall, raw text"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_long_line_splits_in_one_pass_under_the_room() {
        // Words, space runs, punctuation, a non-ASCII char and one run longer
        // than a chunk.
        let mut ln = String::new();
        for i in 0..6000 {
            ln.push_str(&format!("word{i}  x\u{e9}, CamelCase   "));
        }
        ln.push_str(&"a1".repeat(ROOM * 2));
        ln.push_str(" tail");
        let parts = split_long(&ln);
        assert!(parts.len() > 2);
        assert_eq!(parts.concat(), ln);
        assert!(parts.iter().all(|p| est(p) < ROOM), "{:?}", parts.iter().map(|p| est(p)).collect::<Vec<_>>());
        // A line that fits is kept whole, and lines stay lines.
        assert_eq!(split_long("short\nline"), ["short", "line"]);
    }

    #[test]
    fn sessions_that_share_a_prefix_resolve_by_the_hash() {
        let d = dir("prefix");
        let (p1, p2) = (d.join("abcdefgh-1111.jsonl"), d.join("abcdefgh-2222.jsonl"));
        std::fs::write(&p1, [user("one: src/first_session.rs", &ts(0)), user("one more", &ts(1))].join("\n") + "\n").unwrap();
        std::fs::write(&p2, [user("two: src/second_session.rs", &ts(0)), user("two more", &ts(1))].join("\n") + "\n").unwrap();
        for (p, want) in [(&p1, "first_session.rs"), (&p2, "second_session.rs")] {
            let recs = records(&std::fs::read(p).unwrap(), 0);
            let key = range_key("abcdefgh", &recs);
            assert!(resolve(&d, &key).unwrap().contains(want), "{key}");
        }
        // A key that verifies in neither is dangling; one that verifies in
        // both (the same text twice) is refused too.
        let bad = format!("{}@000000000000", range_key("abcdefgh", &records(&std::fs::read(&p1).unwrap(), 0)).rsplit_once('@').unwrap().0);
        assert!(matches!(resolve_parts(&d, &bad), Err(Miss::Dangling(_))));
        std::fs::copy(&p1, d.join("abcdefgh-3333.jsonl")).unwrap();
        let key = range_key("abcdefgh", &records(&std::fs::read(&p1).unwrap(), 0));
        assert!(matches!(resolve_parts(&d, &key), Err(Miss::Dangling(_))));
        assert!(matches!(resolve_parts(&d, "turn:zzzzzzzz:b0-b1@0123456789ab"), Err(Miss::NoTranscript(_))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_card_key_still_resolves_after_the_transcript_grows() {
        let d = dir("grow");
        let p = d.join("gggggggg-7777.jsonl");
        let first = [user("start: src/grow_start.rs", &ts(0)), assistant("read it", 100_000, &ts(1)), user("then src/grow_more.rs", &ts(2))];
        std::fs::write(&p, first.join("\n") + "\n").unwrap();
        let c = card(&p).unwrap();
        let keys: Vec<String> = written_keys(&c);
        assert!(!keys.is_empty(), "{c}");
        let before: Vec<String> = keys.iter().map(|k| resolve(&d, k).unwrap()).collect();
        // Past the next 180k crossing: a sub-segment link now ends the
        // segment the keys name.
        let more = [assistant("grown", 190_000, &ts(3)), user("after the link: src/grow_after.rs", &ts(4)), assistant("again", 200_000, &ts(5))];
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, (more.join("\n") + "\n").as_bytes()).unwrap();
        drop(f);
        assert_eq!(links(&p, "gggggggg", u64::MAX).unwrap().len(), 1);
        for (k, b) in keys.iter().zip(&before) {
            assert_eq!(&resolve(&d, k).unwrap(), b, "{k}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A group that starts or ends inside a record's parts: its real reply
    /// (the resolver re-packs the whole records) is at most its bound, and
    /// the fixture holds a group the bound on its own pieces under-counts.
    #[test]
    fn the_bound_on_a_group_built_from_parts_holds() {
        let d = dir("bound");
        let p = d.join("bbbbbbbb-9999.jsonl");
        let long = "lorem ipsum dolor sit amet: src/bound_long.rs\n".repeat(3000);
        let mid = "a shorter record: src/bound_mid.rs\n".repeat(400);
        let lines = [user("start: src/bound_start.rs", &ts(0)), assistant(&mid, 50_000, &ts(1)), assistant(&long, 50_000, &ts(2)), user(&mid, &ts(3)), user("then src/bound_more.rs", &ts(4))];
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let bytes = std::fs::read(&p).unwrap();
        let recs = records(&bytes, 0);
        let ps = pieces("bbbbbbbb", &recs);
        assert!(ps.iter().filter(|x| x.key.contains('#')).count() >= 3, "{:?}", ps.iter().map(|x| &x.key).collect::<Vec<_>>());
        let mut under = 0;
        for a in 0..ps.len() {
            for b in a + 2..=ps.len() {
                let key = range_key("bbbbbbbb", &recs[ps[a].first..=ps[b - 1].last]);
                let real = est(&resolve_parts(&d, &key).unwrap().text);
                assert!(real <= group_bound("bbbbbbbb", &recs, &ps, a, b, &key), "{key}: {real}");
                under += usize::from(real > group_est(&key, &ps[a..b]));
            }
        }
        assert!(under > 0, "no group in the fixture under-counts on its own pieces");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A record too long for one reply is listed by part keys, and every
    /// key on the card, part keys included, resolves.
    #[test]
    fn every_key_on_a_card_with_parts_resolves() {
        let d = dir("allparts");
        let p = d.join("pppppppp-8888.jsonl");
        let long = "lorem ipsum dolor sit amet: src/parts_long.rs\n".repeat(3000);
        let recs = [user("start: src/parts_start.rs", &ts(0)), assistant(&long, 50_000, &ts(1)), user("then src/parts_more.rs", &ts(2))];
        std::fs::write(&p, recs.join("\n") + "\n").unwrap();
        let c = card(&p).unwrap();
        let keys = written_keys(&c);
        assert!(keys.iter().filter(|k| k.contains('#')).count() >= 2, "{c}");
        for k in &keys {
            // The short form: a 4-character prefix and a 6-digit hash.
            assert!(k.split(':').nth(1) == Some("pppp") && k.rsplit_once('@').unwrap().1.len() == SHORT_HASH, "{k}");
            let r = resolve(&d, k);
            assert!(r.is_ok(), "{k}: {r:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A text with every long-form key in it written in the short form.
    fn to_short(text: &str) -> String {
        let mut out = text.to_string();
        for k in written_keys(text) {
            let (name, h) = k.rsplit_once('@').unwrap();
            let (kind, rest) = name.split_once(':').unwrap();
            let (s8, at) = rest.split_once(':').unwrap();
            if s8.len() == 8 {
                out = out.replace(&k, &format!("{kind}:{}:{at}@{}", &s8[..SHORT], &h[..SHORT_HASH]));
            }
        }
        out
    }

    /// A long-form key (8-character prefix, 12-digit hash, as cards wrote
    /// them before 0242) resolves to the same records as its short form:
    /// groups, parts and links, the reply alike but for the keys' form.
    #[test]
    fn long_and_short_keys_resolve_to_the_same_records() {
        let d = dir("forms");
        let p = d.join("qqqqqqqq-1234.jsonl");
        let long = "lorem ipsum dolor sit amet: src/forms_long.rs\n".repeat(3000);
        let mut lines = session();
        lines.insert(2, assistant(&long, 60_000, &ts(1)));
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let bytes = std::fs::read(&p).unwrap();
        let ls = links(&p, "qqqqqqqq", u64::MAX).unwrap();
        let (from, to) = (0, ls[0].off);
        let recs = records(&bytes[from as usize..to as usize], from);
        let (pl, ps) = (pieces("qqqqqqqq", &recs), pieces("qqqq", &recs));
        assert!(pl.iter().any(|x| x.key.contains('#')));
        let mut keys: Vec<(String, String)> = pl.iter().zip(&ps).map(|(a, b)| (a.key.clone(), b.key.clone())).collect();
        keys.push((range_key("qqqqqqqq", &recs), range_key("qqqq", &recs)));
        let short_links = links(&p, "qqqq", u64::MAX).unwrap();
        keys.extend(ls.iter().zip(&short_links).map(|(a, b)| (a.key.clone(), b.key.clone())));
        for (lk, sk) in &keys {
            assert_eq!(&to_short(lk), sk);
            let (rl, rs) = (resolve(&d, lk).unwrap(), resolve(&d, sk).unwrap());
            assert!(rl.contains(lk.as_str()) && !rl.contains(sk.as_str()), "{lk}: the long key's reply prints the long form");
            assert_eq!(to_short(&rl), rs, "{lk}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A resumed session's card is built from its parent's transcript (its
    /// own is not written yet): its keys name the parent, and resolve in the
    /// project once the child's transcript is there too. A short key quoted
    /// in another session's text (a journal line) resolves the same way.
    #[test]
    fn a_short_key_from_a_parents_card_resolves_in_the_project() {
        let d = dir("resumed");
        let parent = d.join("abcd1111-0000-4000-8000-000000000001.jsonl");
        std::fs::write(&parent, session().join("\n") + "\n").unwrap();
        let c = card(&parent).unwrap();
        let keys = written_keys(&c);
        assert!(keys.iter().any(|k| k.starts_with("turn:abcd:")) && keys.iter().any(|k| k.starts_with("compact:abcd:")), "{c}");
        let before: Vec<String> = keys.iter().map(|k| resolve(&d, k).unwrap()).collect();
        // The child, sharing the 4-character prefix, then a third session
        // whose journal line quotes the keys.
        let child = d.join("abcd2222-0000-4000-8000-000000000002.jsonl");
        std::fs::write(&child, [user("resumed: src/child_only.rs", &ts(20)), assistant("child", 10_000, &ts(21))].join("\n") + "\n").unwrap();
        let other = d.join("ffff3333-0000-4000-8000-000000000003.jsonl");
        std::fs::write(&other, user(&format!("journal: see {} for the head", keys[0]), &ts(22)) + "\n").unwrap();
        let quoted = written_keys(&std::fs::read_to_string(&other).unwrap());
        assert!(quoted.contains(&keys[0]), "{quoted:?}");
        for (k, b) in keys.iter().zip(&before) {
            assert_eq!(&resolve(&d, k).unwrap(), b, "{k}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Every long key on the 6c15905 reference cards still resolves, and a
    /// key the reference rescore hopped to replies at the same size. Cases
    /// are named by ATLAS_CHAIN_REF_CASES, a file of `card<TAB>dir<TAB>
    /// rescore.json` lines, the last field empty when no sizes are compared
    /// (the cards hold session text, so they are not checked in).
    #[test]
    #[ignore = "needs ATLAS_CHAIN_REF_CASES"]
    fn every_long_key_on_the_reference_cards_resolves() {
        let cases = std::fs::read_to_string(std::env::var("ATLAS_CHAIN_REF_CASES").expect("ATLAS_CHAIN_REF_CASES")).unwrap();
        let (mut n, mut sized, mut bad) = (0, 0, Vec::new());
        for line in cases.lines().filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split('\t').collect();
            let c = std::fs::read_to_string(f[0]).unwrap();
            let j = f.get(2).filter(|x| !x.is_empty()).map_or(Value::Null, |x| crate::json::parse(&std::fs::read_to_string(x).unwrap()).unwrap());
            let want: std::collections::HashMap<String, u64> = j
                .get("chain_replies")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .filter_map(|r| Some((r.get("key")?.as_str()?.to_string(), r.get("est_uncapped")?.as_u64()?)))
                .collect();
            for k in written_keys(&c) {
                assert!(k.rsplit_once('@').unwrap().1.len() == HASH, "{k}: a long key");
                n += 1;
                match resolve_parts(Path::new(f[1]), &k) {
                    Ok(r) => {
                        if let Some(w) = want.get(&k) {
                            sized += 1;
                            if est(&r.text) as u64 != *w {
                                bad.push(format!("{}: {k}: est {} vs {w}", f[0], est(&r.text)));
                            }
                        }
                    }
                    Err(e) => bad.push(format!("{}: {k}: {e}", f[0])),
                }
            }
        }
        eprintln!("{n} long keys resolved, {sized} compared by size");
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// Every key on the card of a real transcript resolves (part keys
    /// included). The transcript is named by ATLAS_CHAIN_SEGMENT (a
    /// recording holds session text, so it is not checked in).
    #[test]
    #[ignore = "needs ATLAS_CHAIN_SEGMENT, a transcript path"]
    fn every_key_on_a_real_card_resolves() {
        let p = PathBuf::from(std::env::var("ATLAS_CHAIN_SEGMENT").expect("ATLAS_CHAIN_SEGMENT"));
        let c = card(&p).unwrap();
        let keys = written_keys(&c);
        assert!(keys.iter().any(|k| k.contains('#')), "no part key on the card:\n{c}");
        let bad: Vec<String> = keys.iter().filter_map(|k| resolve_parts(p.parent().unwrap(), k).err().map(|e| format!("{k}: {e:?}"))).collect();
        assert!(bad.is_empty(), "{bad:#?}");
    }

    #[test]
    fn keys_parse_and_plain_turn_refs_are_not_chain_keys() {
        assert_eq!(parse_key("turn:43f55b2e:b10-b20@0123456789ab"), Some(Key::Range { s8: "43f55b2e".into(), a: 10, b: 20, hash: "0123456789ab".into() }));
        assert_eq!(parse_key("turn:43f55b2e:b10#2@0123456789ab"), Some(Key::Part { s8: "43f55b2e".into(), off: 10, k: 2, hash: "0123456789ab".into() }));
        assert_eq!(parse_key("compact:43f55b2e:b10@0123456789ab"), Some(Key::Link { s8: "43f55b2e".into(), off: 10, hash: "0123456789ab".into() }));
        assert_eq!(parse_key("turn:43f5:b10-b20@012345"), Some(Key::Range { s8: "43f5".into(), a: 10, b: 20, hash: "012345".into() }));
        assert_eq!(parse_key("compact:43f5:b10@012345"), Some(Key::Link { s8: "43f5".into(), off: 10, hash: "012345".into() }));
        assert_eq!(parse_key("turn:43f5:b10#1@01234"), None, "a chain key's hash has at least 6 digits");
        // A plain ref still needs 8 digits: the short chain key is not one.
        assert!(crate::compact::parse_ref("turn:43f5:b10-b20@012345").is_none());
        assert_eq!(parse_key("turn:43f55b2e:b10@0123456789ab"), None);
        assert_eq!(parse_key("turn:43f55b2e:b10#0@0123456789ab"), None);
        assert_eq!(parse_key("compact:43f55b2e:10@0123456789ab"), None);
    }
}

