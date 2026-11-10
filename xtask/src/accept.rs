//! Accepted hits: the leak check's known false positives in release
//! binaries (the release process, Q2). A linker packs unrelated string
//! literals side by side, and the matcher's white-space and concatenation
//! folding (`leak.rs`, rules 5 and 6) can join them into a denylist term,
//! as v0.3.0's flag strings did. Rather than cut the term, the one known
//! place is accepted, by a digest of the term and the text around it; any
//! other place of the same term still stops the release.
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
//! SHA-256, in lowercase hex, of the UTF-8 bytes
//! `atlas-accept-v1 NUL term-number NUL sha256hex(term) NUL window`, where
//! the term number is decimal, `term` is the normalised term and `window` is
//! the normalised text of the place: the match and up to [`WINDOW`]
//! normalised characters on each side, fewer at either end of the reading.
//! Normalised text, not raw bytes or offsets: offsets and string-pool order
//! change between builds, and the false positive comes from the folding.
//! Binding the term's own digest means renumbering the denylist cannot point
//! an entry at another term.
//!
//! # Where entries apply
//!
//! Only to the contents of executables (ELF, PE or Mach-O by their first
//! bytes) in a binary gate: the local `release check`, the CI gate and
//! `sign`'s release assets. Never to a path, the exported tree, the wiki
//! pages or a text file beside the binaries. Every place of every term is
//! listed (overlapping ones too), and each must be accepted on its own, so an
//! accepted first place cannot hide a real leak later in the same file.

use std::collections::BTreeSet;

use crate::leak::{line_col, readings, Matcher, Norm};

type R<T = ()> = Result<T, String>;

/// Normalised characters taken on each side of a match for its digest.
pub const WINDOW: usize = 48;

/// The digest's domain, so it is never another digest's input.
const DOMAIN: &str = "atlas-accept-v1";

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
    /// The place's digest; `None` for a path hit, which is never accepted.
    pub digest: Option<String>,
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

/// The digest of one place (see the module docs).
pub fn digest(term_no: usize, term: &str, window: &str) -> String {
    let mut h = atlas::sha256::Hasher::new();
    h.update(DOMAIN.as_bytes());
    h.update(b"\0");
    h.update(term_no.to_string().as_bytes());
    h.update(b"\0");
    h.update(atlas::sha256::hex(term.as_bytes()).as_bytes());
    h.update(b"\0");
    h.update(window.as_bytes());
    h.hex()
}

/// The match at bytes `start..end` of normalised `text` and up to
/// [`WINDOW`] characters on each side.
fn window(text: &str, start: usize, end: usize) -> &str {
    let from = text[..start].char_indices().rev().nth(WINDOW - 1).map_or(0, |(i, _)| i);
    let to = text[end..].char_indices().nth(WINDOW).map_or(text.len(), |(i, _)| end + i);
    &text[from..to]
}

/// Every place of every term in `files`: each occurrence in each reading of
/// the contents, overlapping ones included (one place per offset and
/// digest), and the path when it holds the term, whatever the contents hold
/// (an accepted place in the contents must not hide a path hit).
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
        for (k, t) in m.terms() {
            let mut seen = BTreeSet::new();
            for (r, n) in &reads {
                let mut from = 0;
                while let Some(i) = n.text[from..].find(t.as_str()) {
                    let p = from + i;
                    let at = n.at[p];
                    let d = digest(*k, t, window(&n.text, p, p + t.len()));
                    if seen.insert((at, d.clone())) {
                        let (line, col) = line_col(r, at);
                        out.push(Place { file: rel.clone(), term: *k, at: Some((at, line, col)), digest: Some(d) });
                    }
                    from = p + n.text[p..].chars().next().map_or(1, char::len_utf8);
                }
            }
            if name.text.contains(t.as_str()) {
                out.push(Place { file: rel.clone(), term: *k, at: None, digest: None });
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
            if !m.terms().iter().any(|(k, _)| *k == n) {
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

/// What a binary gate found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Judged {
    /// The report lines of places not accepted.
    pub leaks: Vec<String>,
    /// Places an entry accepted.
    pub accepted: usize,
    /// Entries no place used.
    pub unused: usize,
}

/// Splits places into accepted and not: a place is accepted only when it is
/// in an executable's contents and its (term, digest) is an entry.
pub fn judge(places: &[Place], files: &[(String, Vec<u8>)], acc: &Accepted) -> Judged {
    let exes: BTreeSet<&str> = files.iter().filter(|(_, b)| is_executable(b)).map(|(f, _)| f.as_str()).collect();
    let mut used = BTreeSet::new();
    let mut j = Judged::default();
    for p in places {
        let key = p.digest.as_ref().filter(|_| exes.contains(p.file.as_str())).map(|d| (p.term, d.clone()));
        match key {
            Some(k) if acc.0.contains(&k) => {
                j.accepted += 1;
                used.insert(k);
            }
            _ => j.leaks.push(p.line()),
        }
    }
    j.unused = acc.0.len() - used.len();
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> crate::ucd::Db {
        crate::ucd::Db::load(&crate::pack_embedder::default_ucd()).unwrap()
    }

    /// An ELF-looking file: the magic, padding, then `body`.
    fn elf(body: &str) -> Vec<u8> {
        let mut b = b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0".to_vec();
        b.extend_from_slice(body.as_bytes());
        b
    }

    /// A PE-looking file: `MZ`, e_lfanew at 0x3c pointing at `PE\0\0`, then `body`.
    fn pe(body: &str) -> Vec<u8> {
        let mut b = vec![0u8; 0x40];
        b[..2].copy_from_slice(b"MZ");
        b[0x3c] = 0x40;
        b.extend_from_slice(b"PE\0\0");
        b.extend_from_slice(body.as_bytes());
        b
    }

    const LIST: &str = "# comment\nquillfeather\nmarrow vale\n";

    fn entries_for(m: &Matcher, files: &[(String, Vec<u8>)]) -> Accepted {
        Accepted(places(m, files).into_iter().filter_map(|p| p.digest.map(|d| (p.term, d))).collect())
    }

    #[test]
    fn the_exact_context_passes_and_one_changed_character_fails() {
        let db = db();
        let m = Matcher::new(&db, LIST);
        // Two pooled literals the concatenation folding joins (rule 6).
        let body = "\0\0--flags\0\"quill\" + \"feather\"\0--other--padding-text-here\0";
        let files = vec![("bin/atlas-x".to_string(), elf(body))];
        let acc = entries_for(&m, &files);
        assert_eq!(acc.0.len(), 1);
        let j = judge(&places(&m, &files), &files, &acc);
        assert_eq!(j, Judged { leaks: vec![], accepted: 1, unused: 0 });

        // One character changed inside the window, 40 characters away.
        let changed = vec![("bin/atlas-x".to_string(), elf(&body.replace("--other", "--oth3r")))];
        let j = judge(&places(&m, &changed), &changed, &acc);
        assert_eq!(j.leaks.len(), 1, "{j:?}");
        assert_eq!((j.accepted, j.unused), (0, 1));
    }

    #[test]
    fn an_unaccepted_second_place_in_the_same_file_fails() {
        let db = db();
        let m = Matcher::new(&db, LIST);
        let one = format!("{}AAAA", "x".repeat(60) + "quillfeather" + &"y".repeat(60));
        let files = vec![("atlas.exe".to_string(), pe(&one))];
        let acc = entries_for(&m, &files);
        let two = format!("{one}{}quillfeather{}", "z".repeat(60), "w".repeat(60));
        let files2 = vec![("atlas.exe".to_string(), pe(&two))];
        let ps = places(&m, &files2);
        assert_eq!(ps.len(), 2, "every place is listed, not only the first");
        let j = judge(&ps, &files2, &acc);
        assert_eq!((j.leaks.len(), j.accepted), (1, 1), "{j:?}");
    }

    #[test]
    fn overlapping_places_are_each_listed() {
        let db = db();
        let m = Matcher::new(&db, "abab\n");
        let files = vec![("f".to_string(), elf("ababab"))];
        assert_eq!(places(&m, &files).len(), 2);
    }

    #[test]
    fn a_wrong_term_number_or_term_digest_fails() {
        let db = db();
        let m = Matcher::new(&db, LIST);
        let files = vec![("bin/a".to_string(), elf(&format!("{}quillfeather{}", "a".repeat(50), "b".repeat(50))))];
        let p = &places(&m, &files)[0];
        let (term, d) = (p.term, p.digest.clone().unwrap());
        // The right digest under another term's number.
        let other = Accepted([(2, d.clone())].into_iter().collect());
        assert_eq!(judge(&places(&m, &files), &files, &other).leaks.len(), 1);
        // A digest made with another term's text under this number.
        let wrong = digest(term, "marrow vale", "the same window");
        let wrong = Accepted([(term, wrong)].into_iter().collect());
        assert_eq!(judge(&places(&m, &files), &files, &wrong).leaks.len(), 1);
        // And the right one passes.
        let right = Accepted([(term, d)].into_iter().collect());
        assert!(judge(&places(&m, &files), &files, &right).leaks.is_empty());
    }

    #[test]
    fn an_accepted_place_outside_an_executable_or_in_a_path_still_fails() {
        let db = db();
        let m = Matcher::new(&db, LIST);
        let text = format!("{}quillfeather{}", "a".repeat(50), "b".repeat(50));
        let exe = vec![("bin/a".to_string(), elf(&text))];
        let acc = entries_for(&m, &exe);
        // The same bytes without the magic: a text file (the tree, the wiki).
        let txt = vec![("wiki/Home.md".to_string(), text.clone().into_bytes())];
        let mut acc_txt = acc.clone();
        acc_txt.0.extend(entries_for(&m, &txt).0);
        let j = judge(&places(&m, &txt), &txt, &acc_txt);
        assert_eq!(j.leaks.len(), 1, "a text file is never accepted");
        // A path hit has no digest.
        let path = vec![("bin/quillfeather".to_string(), elf("clean"))];
        let ps = places(&m, &path);
        assert_eq!(ps, vec![Place { file: "bin/quillfeather".into(), term: 1, at: None, digest: None }]);
        assert_eq!(judge(&ps, &path, &acc).leaks, vec!["bin/quillfeather (path): denylist term #1".to_string()]);
        // A path hit beside an accepted place in the contents still fails.
        let both = vec![("bin/quillfeather".to_string(), elf(&text))];
        let acc_both = entries_for(&m, &both);
        let j = judge(&places(&m, &both), &both, &acc_both);
        assert_eq!((j.leaks, j.accepted), (vec!["bin/quillfeather (path): denylist term #1".to_string()], 1));
    }

    #[test]
    fn malformed_lines_and_unknown_terms_are_refused() {
        let db = db();
        let m = Matcher::new(&db, LIST);
        let d = "a".repeat(64);
        assert_eq!(Accepted::parse(&format!("# c\n\n1 {d}\n2\t{d}\n"), &m).unwrap().0.len(), 2);
        for bad in [format!("1 {}", &d[1..]), format!("1 {}", "A".repeat(64)), format!("x {d}"), format!("1 {d} 3"), "1".into(), format!("1 {}g", &d[1..])] {
            let e = Accepted::parse(&bad, &m).unwrap_err();
            assert!(e.contains("line 1") && e.contains("not `TERM-NUMBER SHA256`"), "{bad}: {e}");
        }
        let e = Accepted::parse(&format!("\n3 {d}\n"), &m).unwrap_err();
        assert!(e.contains("line 2") && e.contains("term #3 is not a term"), "{e}");
        let e = Accepted::parse(&format!("0 {d}\n"), &m).unwrap_err();
        assert!(e.contains("term #0"), "{e}");
    }

    #[test]
    fn the_digest_is_pinned() {
        // The format is part of every accepted-hits file: a change here
        // voids every entry, so it must be deliberate.
        assert_eq!(digest(7, "ab", "x ab y"), atlas::sha256::hex(format!("atlas-accept-v1\x007\x00{}\x00x ab y", atlas::sha256::hex(b"ab")).as_bytes()));
        let long = format!("{}T{}", "a".repeat(100), "b".repeat(100));
        assert_eq!(window(&long, 100, 101),format!("{}T{}", "a".repeat(WINDOW), "b".repeat(WINDOW)));
        assert_eq!(window("xxTyy", 2, 3), "xxTyy", "clamped at both ends");
        assert!(is_executable(b"\x7fELF..") && is_executable(&pe("x")) && is_executable(&[0xcf, 0xfa, 0xed, 0xfe]) && !is_executable(b"#!/bin/sh"));
        // Text that starts with MZ, and a Java class file, are not.
        assert!(!is_executable(format!("MZ text{}", " ".repeat(80)).as_bytes()) && !is_executable(b"MZ"));
        assert!(!is_executable(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52]) && is_executable(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2]));
    }
}
