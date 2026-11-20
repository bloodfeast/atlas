//! Tests of `accept.rs` against the ways a real leak could pass as linker
//! packing. Synthetic terms only (`quillfeather`).

use super::*;
use crate::explain::Origin;

fn db() -> crate::ucd::Db {
    crate::ucd::Db::load(&crate::pack_embedder::default_ucd()).unwrap()
}

fn elf(body: &str) -> Vec<u8> {
    let mut b = b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0".to_vec();
    b.extend_from_slice(body.as_bytes());
    b
}

/// A corpus of `(kind, crate, file, text)` parts.
fn corpus(parts: &[(Kind, &str, &str, &str)]) -> Corpus {
    let mut c = Corpus::default();
    for (kind, name, file, text) in parts {
        let krate = if *kind == Kind::Dep { format!("{name} 1.0.0") } else { name.to_string() };
        c.add_text(Origin { kind: *kind, krate, name: name.to_string(), file: file.to_string() }, text);
    }
    c
}

/// The leak lines for a binary of `body` under `acc`.
fn leaks(c: &Corpus, body: &str, acc: &Accepted) -> Vec<String> {
    let db = db();
    let m = Matcher::new(&db, "quillfeather\n");
    let files = vec![("bin/atlas-x".to_string(), elf(body))];
    judge(&m, &places(&m, &files), &files, acc, c).leaks
}

const INJECTED: &str = "\0/home/quillfeather/.cargo/registry/src/x/lib.rs\0";

/// Overlapping 3-byte pieces of identifiers, comments and literals cover
/// the term: not packing, which lays whole literals end to end.
#[test]
fn overlapping_short_pieces_are_unexplained() {
    let c = corpus(&[
        (Kind::Tree, "tree", "src/main.rs", "fn qui() {}\nconst A: &str = \"ill\";\n// lfe\n"),
        (Kind::Dep, "quire", "src/lib.rs", "pub const B: &str = \"eat\"; fn the() {}\n/// her\n"),
    ]);
    let l = leaks(&c, INJECTED, &Accepted::default());
    assert!(l.len() == 1 && l[0].contains("not explained by the source"), "{l:?}");
}

/// A dependency's string that is the term and nothing more: its entry would
/// accept the term wherever the build puts it, so it is never made or
/// honoured; nor one for a string that is not a literal value.
#[test]
fn a_dependency_string_equal_to_the_term_accepts_nothing() {
    for dep in ["pub const N: &str = \"quillfeather\";", "pub fn quillfeather() {}"] {
        let c = corpus(&[(Kind::Dep, "quire", "src/lib.rs", dep)]);
        let acc = Accepted([(1, digest(1, "quillfeather", "quire", b"quillfeather"))].into_iter().collect());
        let l = leaks(&c, INJECTED, &acc);
        assert_eq!(l.len(), 1, "{dep}: {l:?}");
    }
}

/// An identifier is not laid into a binary as a literal: a chain with one
/// stops (a join), and the toolchain's identifiers are not kept at all.
#[test]
fn identifiers_never_pass_as_packing() {
    for kind in [Kind::Tree, Kind::Toolchain] {
        let c = corpus(&[(kind, "x", "src/a.rs", "fn quill() {}"), (Kind::Dep, "quire", "src/lib.rs", "const F: &str = \"feather ipsum\";")]);
        assert_eq!(leaks(&c, "\0quillfeather ipsum\0", &Accepted::default()).len(), 1, "{kind:?}");
    }
}

/// Literals the tree joins in its own source (`concat!`) hold the term:
/// stops as the source's; two of one file side by side stop as a join.
#[test]
fn tree_joins_do_not_pass_as_packing() {
    let c = corpus(&[(Kind::Tree, "tree", "src/main.rs", "const A: &str = concat!(\"lorem quill\", \"feather ipsum\");")]);
    let l = leaks(&c, "\0lorem quillfeather ipsum\0", &Accepted::default());
    assert!(l.len() == 1 && l[0].contains("the source holds the term"), "{l:?}");
    let c = corpus(&[(Kind::Tree, "tree", "src/main.rs", "const A: &str = \"lorem quill\";\nconst B: &str = \"feather ipsum\";")]);
    assert_eq!(leaks(&c, "\0lorem quillfeather ipsum\0", &Accepted::default()).len(), 1);
}

/// Two whole literals of different files that are exactly the halves of a
/// name a build put in a path (`quill` + `feather`): the chain starts and
/// ends with the term, as a word of the build's text would, so it stops.
#[test]
fn literals_that_are_exactly_the_term_stop() {
    let c = corpus(&[(Kind::Tree, "tree", "src/a.rs", "const A: &str = \"quill\";"), (Kind::Dep, "quire", "src/lib.rs", "const B: &str = \"feather\";")]);
    let l = leaks(&c, INJECTED, &Accepted::default());
    assert_eq!(l.len(), 1, "{l:?}");
    // Packed pieces that reach past the term on both sides pass.
    let c = corpus(&[(Kind::Tree, "tree", "src/a.rs", "const A: &str = \"lorem quill\";"), (Kind::Dep, "quire", "src/lib.rs", "const B: &str = \"feather ipsum\";")]);
    assert_eq!(leaks(&c, "\0lorem quillfeather ipsum\0", &Accepted::default()).len(), 0);
}

/// A source file whose path holds a term is never printed.
#[test]
fn a_source_path_with_a_term_is_not_printed() {
    let c = corpus(&[(Kind::Dep, "quire", "src/quillfeather.rs", "pub const A: &str = \"by quillfeather here\";")]);
    let l = leaks(&c, "\0by quillfeather here\0", &Accepted::default());
    assert!(l.len() == 1 && !l[0].contains("quillfeather"), "{l:?}");
}
