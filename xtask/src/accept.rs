//! The binary leak gate's judgement (the release process, Q2), and the
//! accepted-hits file.
//!
//! Every place of every term in a release binary is found by the matcher
//! (`leak.rs`), then explained against the source the binary was built from
//! (`explain.rs`): a place whole literal values of different files packed
//! end to end make (the linker's string pool, in an order that changes per
//! build and per target), reaching past the term on both sides, passes and
//! is listed with its pieces; a place a dependency's or the toolchain's
//! literal value holds, with [`MIN_CONTEXT`] bytes beyond the term, is
//! third-party text, reported by crate and file, and passes only by an entry
//! here; a join (known strings end to end that are not such a split) passes
//! only by an entry keyed on its pieces; every other place stops the release
//! and can never be accepted: one the tree's source holds (the text gate
//! refuses it there too), a dependency's string that is not a literal value
//! or holds too little beyond the term, and one the source does not explain
//! at all (a path, a user or host name, an environment value the build put
//! in). A text file, a path, the exported tree and the wiki pages are never
//! explained or accepted.
//!
//! # The file
//!
//! Private, beside the denylist (`leak-accepted.txt` in the denylist's
//! folder, or the path ATLAS_LEAK_ACCEPTED names). One entry per line,
//! `TERM-NUMBER SHA256` (white space between); blank lines and lines
//! starting with `#` are skipped. A line of any other shape, a digest that is
//! not 64 hex digits, or a number that is not a term of the denylist is
//! refused, naming the line number only.
//!
//! # The digest
//!
//! SHA-256, in lowercase hex, of the bytes
//! `atlas-accept-src-v1 NUL term-number NUL sha256hex(term) NUL crate NUL string`,
//! where the term number is decimal, `term` the normalised term, `crate` the
//! crate's name (`rust` for the toolchain's library) and `string` the known
//! string that holds the place, as the source spells it once decoded. Source
//! content only: no offset, no neighbouring bytes, no version or file path,
//! so the entry holds on every rebuild and target, and voids when that
//! string changes. Binding the term's own digest means renumbering the
//! denylist cannot point an entry at another term. A join's entry is
//! [`join_digest`]: `atlas-accept-join-v1 NUL term-number NUL sha256hex(term)`
//! then, per piece in order, `NUL crate NUL length NUL string`. Entries of
//! the earlier forms (digests of the binary's bytes around a place) match
//! nothing; the gate says once how many entries matched no place.

use std::collections::BTreeSet;

use crate::explain::{Corpus, Kind, Piece, Verdict, MIN_CONTEXT};
use crate::leak::{line_col, readings, Matcher, Norm};

type R<T = ()> = Result<T, String>;

/// The digest's domain, so it is never another digest's input.
const DOMAIN: &str = "atlas-accept-src-v1";

/// The domain of a join's digest.
const JOIN_DOMAIN: &str = "atlas-accept-join-v1";

/// The default file name, in the denylist's folder.
pub const DEFAULT_NAME: &str = "leak-accepted.txt";

/// One place a term occurs: in a file's contents, or only in its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The file, relative to the folder checked.
    pub file: String,
    /// The term's number in the denylist.
    pub term: usize,
    /// (byte offset, line, column) in the file; `None` for a path hit.
    pub at: Option<(usize, usize, usize)>,
    /// The byte just after the place (0 for a path hit).
    pub end: usize,
    /// Found in the UTF-8 reading (a UTF-16 one is never explained).
    pub utf8: bool,
}

impl Place {
    /// The report line, as `Matcher::hits` writes it: the place and the
    /// term's number, never the term.
    pub fn line(&self) -> String {
        match self.at {
            Some((b, l, c)) => format!("{}:{l}:{c} (byte {b}): denylist term #{}", self.file, self.term),
            None => format!("{} (path): denylist term #{}", self.file, self.term),
        }
    }
}

/// The digest of an entry for a place a known string of crate `krate` holds.
pub fn digest(term_no: usize, term: &str, krate: &str, string: &[u8]) -> String {
    let mut h = atlas::sha256::Hasher::new();
    h.update(DOMAIN.as_bytes());
    h.update(b"\0");
    h.update(term_no.to_string().as_bytes());
    h.update(b"\0");
    h.update(atlas::sha256::hex(term.as_bytes()).as_bytes());
    h.update(b"\0");
    h.update(krate.as_bytes());
    h.update(b"\0");
    h.update(string);
    h.hex()
}

/// Every place of every term in `files`: each occurrence in each reading of
/// the contents, overlapping ones included, and the path when it holds the
/// term, whatever the contents hold.
pub fn places(m: &Matcher, files: &[(String, Vec<u8>)]) -> Vec<Place> {
    let mut out = Vec::new();
    for (rel, body) in files {
        let name_chars: Vec<(usize, char)> = rel.char_indices().collect();
        let name = m.normalise(&name_chars);
        let reads: Vec<(Vec<(usize, char)>, Norm)> = readings(body)
            .into_iter()
            .map(|r| {
                let n = m.normalise(&r);
                (r, n)
            })
            .collect();
        // The UTF-8 reading is the first, but after a UTF-16 byte-order mark.
        let utf16_bom = body.starts_with(&[0xFF, 0xFE]) || body.starts_with(&[0xFE, 0xFF]);
        for term in m.terms() {
            let k = term.num;
            let mut seen = BTreeSet::new();
            for (i, (r, n)) in reads.iter().enumerate() {
                let mut from = 0;
                while let Some(p) = term.find(&n.text, from) {
                    let at = n.at[p];
                    let last = n.at[p + term.text.len() - 1];
                    let end = r.partition_point(|&(o, _)| o <= last);
                    let end = r.get(end).map_or(body.len(), |&(o, _)| o);
                    if seen.insert((at, end)) {
                        let (line, col) = line_col(r, at);
                        out.push(Place { file: rel.clone(), term: k, at: Some((at, line, col)), end, utf8: i == 0 && !utf16_bom });
                    }
                    from = p + n.text[p..].chars().next().map_or(1, char::len_utf8);
                }
            }
            if term.find(&name.text, 0).is_some() {
                out.push(Place { file: rel.clone(), term: k, at: None, end: 0, utf8: false });
            }
        }
    }
    out
}

/// The entries of an accepted-hits file: (term number, digest).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Accepted(pub BTreeSet<(usize, String)>);

impl Accepted {
    /// Parses a file's text against the matcher's terms; refuses a line of
    /// another shape or a number that is not a term, by line number only.
    pub fn parse(text: &str, m: &Matcher) -> R<Accepted> {
        let mut out = BTreeSet::new();
        for (i, l) in text.lines().enumerate() {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let bad = || format!("the accepted-hits file, line {}: not `TERM-NUMBER SHA256`", i + 1);
            let mut parts = l.split_whitespace();
            let (Some(n), Some(d), None) = (parts.next(), parts.next(), parts.next()) else { return Err(bad()) };
            let n: usize = n.parse().map_err(|_| bad())?;
            if d.len() != 64 || !d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                return Err(bad());
            }
            if !m.terms().iter().any(|t| t.num == n) {
                return Err(format!("the accepted-hits file, line {}: term #{n} is not a term of the denylist", i + 1));
            }
            out.insert((n, d.to_string()));
        }
        Ok(Accepted(out))
    }

    /// The file's lines for these entries, in order.
    pub fn lines(&self) -> String {
        self.0.iter().map(|(n, d)| format!("{n}  {d}\n")).collect()
    }
}

/// Whether bytes start as an executable: ELF, PE (`MZ`, and `PE\0\0` where
/// the DOS header's e_lfanew points) or Mach-O (thin, in either byte order,
/// or universal with 1 to 20 architectures, which a Java class file's
/// version field is not).
pub fn is_executable(b: &[u8]) -> bool {
    let u32_at = |i: usize, be: bool| b.get(i..i + 4).map(|x| if be { u32::from_be_bytes([x[0], x[1], x[2], x[3]]) } else { u32::from_le_bytes([x[0], x[1], x[2], x[3]]) });
    match b.get(..4) {
        Some([0x7f, b'E', b'L', b'F'] | [0xcf, 0xfa, 0xed, 0xfe] | [0xce, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xcf] | [0xfe, 0xed, 0xfa, 0xce]) => true,
        Some([0xca, 0xfe, 0xba, 0xbe]) => u32_at(4, true).is_some_and(|n| (1..=20).contains(&n)),
        Some([b'M', b'Z', _, _]) => u32_at(0x3c, false).and_then(|o| b.get(o as usize..o as usize + 4)).is_some_and(|s| s == b"PE\0\0"),
        _ => false,
    }
}

/// A place's verdict: `None` for a place that is never explained (a path,
/// a text file, a UTF-16 reading).
pub fn verdict(p: &Place, files: &[(String, Vec<u8>)], corpus: &Corpus) -> Option<Verdict> {
    let (at, _, _) = p.at?;
    let body = &files.iter().find(|(f, _)| *f == p.file)?.1;
    (p.utf8 && is_executable(body)).then(|| corpus.explain(body, at, p.end))
}

/// The digest of an entry for a place a chain of known strings makes
/// ([`Verdict::Join`]): each piece's crate and string, in order.
pub fn join_digest(term_no: usize, term: &str, pieces: &[(&str, &[u8])]) -> String {
    let mut h = atlas::sha256::Hasher::new();
    h.update(JOIN_DOMAIN.as_bytes());
    h.update(b"\0");
    h.update(term_no.to_string().as_bytes());
    h.update(b"\0");
    h.update(atlas::sha256::hex(term.as_bytes()).as_bytes());
    for (krate, string) in pieces {
        h.update(b"\0");
        h.update(krate.as_bytes());
        h.update(b"\0");
        h.update(string.len().to_string().as_bytes());
        h.update(b"\0");
        h.update(string);
    }
    h.hex()
}

/// Whether a known string or chain may be an entry's key: it holds
/// [`MIN_CONTEXT`] bytes beyond the place, so an entry for it does not
/// accept the term wherever else it shows up.
fn enough_context(context: usize) -> bool {
    context >= MIN_CONTEXT
}

/// The entry that would accept a place, when one can: a place a
/// dependency's or the toolchain's literal value holds with context around
/// the term, or a [`Verdict::Join`] whose pieces hold context beyond it.
pub fn entry(m: &Matcher, p: &Place, v: &Option<Verdict>) -> Option<(usize, String)> {
    let term = m.terms().iter().find(|t| t.num == p.term)?;
    match v {
        Some(Verdict::Contained { string, origin, literal: true, context, .. }) if origin.kind != Kind::Tree && enough_context(*context) => Some((p.term, digest(p.term, &term.text, &origin.name, string))),
        Some(Verdict::Join(pieces, _)) => {
            let (at, _, _) = p.at?;
            let total: usize = pieces.iter().map(|x| x.string.len()).sum();
            enough_context(total.saturating_sub(p.end - at)).then(|| {
                let keys: Vec<(&str, &[u8])> = pieces.iter().map(|x| (x.origin.name.as_str(), x.string.as_slice())).collect();
                (p.term, join_digest(p.term, &term.text, &keys))
            })
        }
        _ => None,
    }
}

/// Whether text holds a term of the matcher.
fn holds_term(m: &Matcher, s: &str) -> bool {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let n = m.normalise(&chars);
    m.terms().iter().any(|t| t.find(&n.text, 0).is_some())
}

/// A source file as a report names it: crate, file and line, unless that
/// text holds a term (a file named after one), which is never printed.
fn source(m: &Matcher, krate: &str, file: &str, line: usize) -> String {
    let s = if krate == "tree" { format!("the tree's {file}:{line}") } else { format!("{krate} {file}:{line}") };
    if holds_term(m, &s) {
        "a source file whose name holds a denylist term".into()
    } else {
        s
    }
}

/// Why a place passes or stops, after its report line.
pub fn reason(m: &Matcher, v: &Option<Verdict>) -> String {
    let chain = |ps: &[Piece]| ps.iter().map(|x| source(m, &x.origin.krate, &x.origin.file, x.line)).collect::<Vec<_>>().join(" + ");
    match v {
        None => String::new(),
        Some(Verdict::Split(ps)) => format!(" (literals packed side by side: {})", chain(ps)),
        Some(Verdict::Join(ps, why)) => format!(" (known strings end to end, but {why}: {}; the owner checks the place, and only an entry passes it)", chain(ps)),
        Some(Verdict::Unexplained) => " (not explained by the source: what the build put there, a path, a user or host name, the environment)".into(),
        Some(Verdict::Contained { origin, line, .. }) if origin.kind == Kind::Tree => format!(" (in {}: the source holds the term)", source(m, &origin.krate, &origin.file, *line)),
        Some(Verdict::Contained { origin, line, literal: false, .. }) => format!(" (in {}: third-party text that is not a literal value: never accepted)", source(m, &origin.krate, &origin.file, *line)),
        Some(Verdict::Contained { origin, line, context, .. }) if !enough_context(*context) => format!(" (in {}: third-party text with {context} byte(s) beside the term, fewer than {MIN_CONTEXT}: never accepted)", source(m, &origin.krate, &origin.file, *line)),
        Some(Verdict::Contained { origin, line, .. }) => format!(" (in {}: third-party text, accepted only by an entry)", source(m, &origin.krate, &origin.file, *line)),
    }
}

/// What a binary gate found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Judged {
    /// The report lines of places that stop, each with its reason.
    pub leaks: Vec<String>,
    /// The report lines of places literals packed side by side make, each
    /// with its pieces.
    pub split: Vec<String>,
    /// The report lines of places an entry accepted, with crate and file.
    pub accepted: Vec<String>,
    /// Entries no place used.
    pub unused: usize,
}

/// Judges every place (see the module docs).
pub fn judge(m: &Matcher, places: &[Place], files: &[(String, Vec<u8>)], acc: &Accepted, corpus: &Corpus) -> Judged {
    let mut used = BTreeSet::new();
    let mut j = Judged::default();
    for p in places {
        let v = verdict(p, files, corpus);
        let line = format!("{}{}", p.line(), reason(m, &v));
        if matches!(v, Some(Verdict::Split(_))) {
            j.split.push(line);
            continue;
        }
        match entry(m, p, &v).filter(|k| acc.0.contains(k)) {
            Some(k) => {
                j.accepted.push(line);
                used.insert(k);
            }
            None => j.leaks.push(line),
        }
    }
    j.unused = acc.0.len() - used.len();
    j
}

#[cfg(test)]
#[path = "accept_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "accept_join_tests.rs"]
mod join_tests;
