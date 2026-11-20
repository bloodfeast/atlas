//! Tests of `accept.rs`: the binary leak gate's judgement. Synthetic terms
//! only (`quillfeather`, `marrow vale`).

use super::*;
use crate::explain::Origin;

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

fn origin(kind: Kind, name: &str, file: &str) -> Origin {
    let krate = if kind == Kind::Dep { format!("{name} 1.0.0") } else { name.to_string() };
    Origin { kind, krate, name: name.into(), file: file.into() }
}

/// A corpus of the tree's `main` and a dependency `quire`'s `lib`.
fn corpus(main: &str, dep: &str) -> Corpus {
    let mut c = Corpus::default();
    c.add_text(origin(Kind::Tree, "tree", "src/main.rs"), main);
    c.add_text(origin(Kind::Dep, "quire", "src/lib.rs"), dep);
    c
}

fn run(m: &Matcher, files: &[(String, Vec<u8>)], acc: &Accepted, c: &Corpus) -> Judged {
    judge(m, &places(m, files), files, acc, c)
}

/// (d) Two literals packed side by side make the term: the linker's doing,
/// whatever the order or target. Passes with no entry, and is counted.
#[test]
fn a_term_split_across_two_literals_passes() {
    let db = db();
    let m = Matcher::new(&db, LIST);
    let c = corpus("fn main() { let a = \"lorem quill\"; }", "const B: &str = \"feather ipsum\";");
    for body in ["\0\x01lorem quillfeather ipsum\x02\0", "\0zzlorem quillfeather ipsumzz\0", "\0feather ipsumlorem quillfeather ipsum\0"] {
        let files = vec![("bin/atlas-x".to_string(), elf(body))];
        let j = run(&m, &files, &Accepted::default(), &c);
        assert_eq!((j.leaks.len(), j.split.len()), (0, 1), "{body:?}: {j:?}");
        // Listed by place and term number, with its pieces, never the term.
        assert!(j.split[0].starts_with("bin/atlas-x:1:") && j.split[0].contains("denylist term #1 (literals packed side by side: the tree's src/main.rs:1 + quire 1.0.0 src/lib.rs:1)"), "{j:?}");
    }
    // A multi-word term over a line break between two literals, in a PE.
    let c = corpus("const A: &str = \"see marrow\\n\";", "const B: &str = \"vale road\";");
    let files = vec![("atlas.exe".to_string(), pe("\0see marrow\nvale road\0"))];
    assert_eq!(run(&m, &files, &Accepted::default(), &c).split.len(), 1);
}

/// (a) A literal of the tree that holds the term: stops (the text gate
/// refuses it too), and no entry can accept it.
#[test]
fn a_term_inside_a_tree_literal_stops() {
    let db = db();
    let m = Matcher::new(&db, LIST);
    let c = corpus("fn f() {\n    println!(\"by {} lorem quillfeather ipsum\", x);\n}", "");
    let files = vec![("bin/atlas-x".to_string(), elf("\0by  lorem quillfeather ipsum\0"))];
    let ps = places(&m, &files);
    let v = verdict(&ps[0], &files, &c);
    assert!(matches!(&v, Some(Verdict::Contained { origin, line: 2, .. }) if origin.kind == Kind::Tree), "{v:?}");
    assert_eq!(entry(&m, &ps[0], &v), None);
    let j = run(&m, &files, &Accepted::default(), &c);
    assert_eq!(j.leaks, vec!["bin/atlas-x:1:28 (byte 27): denylist term #1 (in the tree's src/main.rs:2: the source holds the term)".to_string()]);
}

/// (b) A dependency's literal that holds the term: reported by crate and
/// file; an entry keyed on that crate and string accepts it on any build,
/// and no other.
#[test]
fn a_term_inside_a_dependency_literal_stops_until_its_entry() {
    let db = db();
    let m = Matcher::new(&db, LIST);
    let dep = "//! A crate.\npub const AUTHOR: &str = \"quillfeather <q@example.invalid>\";\n";
    let c = corpus("fn main() {}", dep);
    let files = vec![("bin/atlas-x".to_string(), elf("\0\0abcquillfeather <q@example.invalid>xyz\0"))];
    let j = run(&m, &files, &Accepted::default(), &c);
    assert_eq!(j.leaks, vec!["bin/atlas-x:1:22 (byte 21): denylist term #1 (in quire 1.0.0 src/lib.rs:2: third-party text, accepted only by an entry)".to_string()]);
    let key = (1, digest(1, "quillfeather", "quire", b"quillfeather <q@example.invalid>"));
    let acc = Accepted([key.clone()].into_iter().collect());
    let j = run(&m, &files, &acc, &c);
    assert_eq!((j.leaks.len(), j.accepted.len(), j.unused), (0, 1, 0), "{j:?}");
    // Another build packs other strings around it: the entry still holds.
    let moved = vec![("atlas.exe".to_string(), pe("\0\0fn mainquillfeather <q@example.invalid>A crate.\0"))];
    assert_eq!(run(&m, &moved, &acc, &c).leaks.len(), 0);
    // The string changed (a new version of the crate): the entry is unused.
    let c2 = corpus("", &dep.replace("q@", "r@"));
    let files2 = vec![("bin/atlas-x".to_string(), elf("\0quillfeather <r@example.invalid>\0"))];
    let j = run(&m, &files2, &acc, &c2);
    assert_eq!((j.leaks.len(), j.unused), (1, 1));
    // The same string under another crate's name, or another term's number.
    for wrong in [(1, digest(1, "quillfeather", "other", b"quillfeather <q@example.invalid>")), (2, digest(2, "quillfeather", "quire", b"quillfeather <q@example.invalid>"))] {
        assert_eq!(run(&m, &files, &Accepted([wrong].into_iter().collect()), &c).leaks.len(), 1);
    }
}

/// (c) What the build put in: a path with a home folder, an environment
/// value. Not made of known strings, so it stops, and nothing accepts it.
#[test]
fn a_term_the_build_put_in_stops() {
    let db = db();
    let m = Matcher::new(&db, LIST);
    let c = corpus("fn main() { panic!(\"src/main.rs\"); }", "pub fn x() {}");
    for body in ["\0/home/quillfeather/.cargo/registry/src/x/lib.rs\0", "\0C:\\Users\\quillfeather\\src\0", "\0marrow vale\0"] {
        let files = vec![("bin/atlas-x".to_string(), elf(body))];
        let ps = places(&m, &files);
        let v = verdict(&ps[0], &files, &c);
        assert_eq!(v, Some(Verdict::Unexplained), "{body:?}");
        assert_eq!(entry(&m, &ps[0], &v), None);
        let j = run(&m, &files, &Accepted::default(), &c);
        assert!(j.leaks.len() == 1 && j.leaks[0].contains("not explained by the source"), "{j:?}");
    }
    // Half a literal, half what the build put in: still stops.
    let c = corpus("const A: &str = \"lorem quill\";", "");
    let files = vec![("bin/atlas-x".to_string(), elf("\0lorem quillfeather/home\0"))];
    assert_eq!(run(&m, &files, &Accepted::default(), &c).leaks.len(), 1);
    // Strings shorter than MIN_PIECE never explain a place.
    let c = corpus("const A: &str = \"quillfeath\"; const B: &str = \"er\";", "");
    let files = vec![("bin/atlas-x".to_string(), elf("\0quillfeather\0"))];
    assert_eq!(run(&m, &files, &Accepted::default(), &c).leaks.len(), 1);
}

/// A text file, a path and a UTF-16 reading are never explained or accepted.
#[test]
fn text_files_paths_and_utf16_are_never_explained() {
    let db = db();
    let m = Matcher::new(&db, LIST);
    let dep = "pub const A: &str = \"quillfeather\"; pub const B: &str = \"lorem quill\"; pub const C: &str = \"feather\";";
    let c = corpus("", dep);
    let acc = Accepted([(1, digest(1, "quillfeather", "quire", b"quillfeather"))].into_iter().collect());
    let txt = vec![("notes.txt".to_string(), b"lorem quillfeather".to_vec())];
    assert_eq!(run(&m, &txt, &acc, &c).leaks.len(), 1);
    let path = vec![("bin/quillfeather".to_string(), elf("clean"))];
    assert_eq!(run(&m, &path, &acc, &c).leaks, vec!["bin/quillfeather (path): denylist term #1".to_string()]);
    let wide: Vec<u8> = "\0quillfeather".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut b = elf("");
    b.extend_from_slice(&wide);
    let files = vec![("bin/a".to_string(), b)];
    let ps = places(&m, &files);
    assert!(!ps.is_empty() && ps.iter().all(|p| !p.utf8), "{ps:?}");
    assert_eq!(run(&m, &files, &acc, &c).leaks.len(), ps.len());
}

/// Each place is judged on its own: an accepted dependency place does not
/// pass an unexplained one of the same term in the same file.
#[test]
fn every_place_is_judged_on_its_own() {
    let db = db();
    let m = Matcher::new(&db, LIST);
    let c = corpus("", "const A: &str = \"made by quillfeather\";");
    let acc = Accepted([(1, digest(1, "quillfeather", "quire", b"made by quillfeather"))].into_iter().collect());
    let files = vec![("bin/a".to_string(), elf("\0made by quillfeather\0/home/quillfeather\0"))];
    let j = run(&m, &files, &acc, &c);
    assert_eq!((j.leaks.len(), j.accepted.len()), (1, 1), "{j:?}");
}

#[test]
fn a_whole_word_term_lists_only_its_whole_word_places() {
    let db = db();
    let m = Matcher::new(&db, "#[word]\nquill\n");
    let files = vec![("f".to_string(), elf("squill quills quill_x (quill) a-quill"))];
    let ps = places(&m, &files);
    assert_eq!(ps.iter().filter_map(|p| p.at.map(|a| (a.0, p.end))).collect::<Vec<_>>(), vec![(16 + 23, 16 + 28), (16 + 32, 16 + 37)], "{ps:?}");
    assert!(places(&m, &[("bin/quills".to_string(), elf("clean"))]).is_empty(), "a path inside a word");
    // Overlapping places are each listed.
    let m = Matcher::new(&db, "abab\n");
    assert_eq!(places(&m, &[("f".to_string(), elf("ababab"))]).len(), 2);
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
    // The format is part of every accepted-hits file: a change here voids
    // every entry, so it must be deliberate.
    assert_eq!(digest(7, "ab", "quire", b"x ab y"), atlas::sha256::hex(format!("atlas-accept-src-v1\x007\x00{}\x00quire\x00x ab y", atlas::sha256::hex(b"ab")).as_bytes()));
    assert!(is_executable(b"\x7fELF..") && is_executable(&pe("x")) && is_executable(&[0xcf, 0xfa, 0xed, 0xfe]) && !is_executable(b"#!/bin/sh"));
    // Text that starts with MZ, and a Java class file, are not.
    assert!(!is_executable(format!("MZ text{}", " ".repeat(80)).as_bytes()) && !is_executable(b"MZ"));
    assert!(!is_executable(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52]) && is_executable(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2]));
    assert_eq!(
        join_digest(7, "ab", &[("tree", b"x a"), ("quire", b"b y")]),
        atlas::sha256::hex(format!("atlas-accept-join-v1\x007\x00{}\x00tree\x003\x00x a\x00quire\x003\x00b y", atlas::sha256::hex(b"ab")).as_bytes())
    );
}

/// A join stops, and an entry keyed on its pieces passes it on any build
/// that packs them so; a join with too little beyond the term gets none.
#[test]
fn a_join_passes_only_by_its_entry() {
    let db = db();
    let m = Matcher::new(&db, LIST);
    let c = corpus("fn lorem_quill() {}", "const B: &str = \"feather ipsum\";");
    let files = vec![("bin/atlas-x".to_string(), elf("\0lorem_quillfeather ipsum\0"))];
    let ps = places(&m, &files);
    let v = verdict(&ps[0], &files, &c);
    assert!(matches!(&v, Some(Verdict::Join(..))), "{v:?}");
    let j = run(&m, &files, &Accepted::default(), &c);
    assert!(j.leaks.len() == 1 && j.leaks[0].contains("not a literal value") && j.leaks[0].contains("the tree's src/main.rs:1 + quire 1.0.0 src/lib.rs:1"), "{j:?}");
    let key = (1, join_digest(1, "quillfeather", &[("tree", b"lorem_quill"), ("quire", b"feather ipsum")]));
    assert_eq!(entry(&m, &ps[0], &v), Some(key.clone()));
    let acc = Accepted([key].into_iter().collect());
    assert_eq!((run(&m, &files, &acc, &c).leaks.len(), run(&m, &files, &acc, &c).accepted.len()), (0, 1));
    // The pieces hold one byte beyond the term: no entry.
    let c = corpus("fn quill() {}", "const B: &str = \"feather!\";");
    let files = vec![("bin/atlas-x".to_string(), elf("\0quillfeather!\0"))];
    let ps = places(&m, &files);
    assert_eq!(entry(&m, &ps[0], &verdict(&ps[0], &files, &c)), None);
}
