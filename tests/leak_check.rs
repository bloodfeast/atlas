//! LEAK-CHECK: no tracked file may name anything on a private denylist.
//!
//! The denylist lives outside the repository, in the file named by the
//! `ATLAS_LEAK_DENYLIST` environment variable: one term per line, matched
//! anywhere in every tracked file (file contents and paths). Blank lines and
//! lines starting with `#` are ignored, except that a `#[word]` line makes
//! the term on the next line match as a whole word only (the matcher's
//! module docs, "Whole-word terms"). Without the variable the test is
//! skipped with a notice, so the public repository never has to carry the
//! list it guards against.
//!
//! The matcher is the release's (`xtask/src/leak.rs`, included here by
//! path with the Unicode tables it reads, `xtask/src/ucd.rs`); its module
//! docs say how text and terms are normalised before matching. It is tested
//! below against bypass attempts with synthetic terms only.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

#[allow(dead_code)]
#[path = "../xtask/src/ucd.rs"]
mod ucd;

#[path = "../xtask/src/leak.rs"]
mod leak;

use leak::Matcher;

/// The committed Unicode Character Database, loaded once.
fn db() -> &'static ucd::Db {
    static DB: OnceLock<ucd::Db> = OnceLock::new();
    DB.get_or_init(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("xtask").join("ucd").join("15.1.0");
        ucd::Db::load(&dir).unwrap_or_else(|e| panic!("UCD: {e}"))
    })
}

#[test]
fn tracked_files_name_nothing_on_the_denylist() {
    let Some(list) = std::env::var_os("ATLAS_LEAK_DENYLIST").filter(|v| !v.is_empty()) else {
        eprintln!("leak check skipped: ATLAS_LEAK_DENYLIST is not set");
        return;
    };
    let text = std::fs::read_to_string(&list).unwrap_or_else(|e| panic!("denylist {}: {e}", Path::new(&list).display()));
    let m = Matcher::new(db(), &text);
    assert!(!m.is_empty(), "the denylist holds no terms");
    let root = env!("CARGO_MANIFEST_DIR");
    let out = Command::new("git").args(["-C", root, "ls-files", "-z"]).output().expect("run git ls-files");
    assert!(out.status.success(), "git ls-files failed: {}", String::from_utf8_lossy(&out.stderr));
    let files: Vec<(String, Vec<u8>)> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|rel| {
            let rel = String::from_utf8_lossy(rel).into_owned();
            let body = std::fs::read(Path::new(root).join(&rel)).unwrap_or_default();
            (rel, body)
        })
        .collect();
    let found = m.hits(&files);
    assert!(found.is_empty(), "tracked files name denylisted terms:\n{}", found.join("\n"));
}

// Synthetic terms only: none of these is on any real list.
const LIST: &str = "# synthetic\n\n  quillfeather  \nmarrow vale\nCAFE\u{301}NOIR\n";

fn hits(files: &[(String, Vec<u8>)]) -> Vec<String> {
    Matcher::new(db(), LIST).hits(files)
}

fn caught(body: &str) -> bool {
    caught_bytes(body.as_bytes())
}

fn caught_bytes(body: &[u8]) -> bool {
    !hits(&[("notes.md".into(), body.to_vec())]).is_empty()
}

#[test]
fn the_matcher_catches_case_paths_and_padding() {
    assert_eq!(Matcher::new(db(), LIST).len(), 3, "comments, blanks and padding are dropped");
    assert!(caught("see QuillFeather here"));
    assert!(caught("QUILLFEATHER"));
    assert!(caught("xxquillfeatherxx"), "substring anywhere");
    assert!(caught("the Marrow Vale road"));
    assert!(caught("cafe\u{301}noir"));
    assert!(!caught("quill feather"), "a different spelling is not this term");
    let by_path = hits(&[("docs/QuillFeather/x.md".into(), b"clean".to_vec())]);
    assert_eq!(by_path, vec!["docs/QuillFeather/x.md (path): denylist term #1".to_string()], "a path is checked too");
    let by_body = hits(&[("notes.md".into(), b"see quillfeather".to_vec())]);
    assert_eq!(by_body, vec!["notes.md:1:5 (byte 4): denylist term #1".to_string()], "the report names the file, the place and the term's number, never the term");
}

/// Bypasses that ASCII case folding over raw bytes let through; each is
/// caught after normalising.
#[test]
fn the_matcher_catches_unicode_and_split_bypasses() {
    let bypasses = [
        ("non-ASCII case", "CAF\u{c9}NOIR"),
        ("precomposed vs combining", "caf\u{e9}noir"),
        ("Cyrillic a", "m\u{430}rrow vale"),
        ("full-width letters", "\u{ff51}\u{ff55}\u{ff49}\u{ff4c}\u{ff4c}feather"),
        ("zero-width space", "quill\u{200b}feather"),
        ("soft hyphen", "quill\u{ad}feather"),
        ("split across lines", "marrow\nvale"),
        ("split across lines CRLF", "marrow\r\nvale"),
        ("two spaces", "marrow  vale"),
        ("string concatenation", "\"quill\" + \"feather\""),
        ("UTF-16 file", "q\0u\0i\0l\0l\0f\0e\0a\0t\0h\0e\0r\0"),
    ];
    let missed: Vec<&str> = bypasses.iter().filter(|(_, b)| !caught(b)).map(|(w, _)| *w).collect();
    assert!(missed.is_empty(), "the leak check misses: {missed:?}");
}

/// More of each kind: other scripts' capitals, BOMs, other joiners and
/// concatenation operators, what must not match, and the places reported
/// in the original file.
#[test]
fn the_matcher_normalises_and_reports_original_places() {
    for (what, body) in [
        ("Greek capitals", "QU\u{399}LLFE\u{391}THER"),
        ("Greek and Cyrillic mix", "\u{41c}\u{391}RROW V\u{410}LE"),
        ("full-width capitals", "\u{ff31}\u{ff35}\u{ff29}\u{ff2c}\u{ff2c}FEATHER"),
        ("BOM inside", "quill\u{feff}feather"),
        ("joiners", "q\u{200d}u\u{200c}i\u{2060}llfeather"),
        ("accent added", "qu\u{ed}llfeather"),
        ("tab and NBSP", "marrow\t\u{a0}vale"),
        ("line separator", "marrow\u{2028}vale"),
        ("dot concatenation", "'quill' . 'feather'"),
        ("Lua concatenation", "\"quill\"..\"feather\""),
        ("adjacent literals", "\"quill\" \"feather\""),
    ] {
        assert!(caught(body), "{what}: {body:?}");
    }
    for (what, body) in [("one word vs two", "quill feather"), ("list items", "[\"quill\", \"feather\"]"), ("near miss", "quilfeather")] {
        assert!(!caught(body), "{what}: {body:?}");
    }
    let mut le = vec![0xFF, 0xFE];
    let mut be = vec![0xFE, 0xFF];
    for u in "x\nyy quillfeather".encode_utf16() {
        le.extend(u.to_le_bytes());
        be.extend(u.to_be_bytes());
    }
    let at = |body: Vec<u8>| hits(&[("f.txt".into(), body)]);
    assert_eq!(at(le), vec!["f.txt:2:4 (byte 12): denylist term #1".to_string()], "UTF-16LE with a BOM");
    assert_eq!(at(be), vec!["f.txt:2:4 (byte 12): denylist term #1".to_string()], "UTF-16BE with a BOM");
    let be_bare: Vec<u8> = "quillfeather".encode_utf16().flat_map(u16::to_be_bytes).collect();
    assert!(caught_bytes(&be_bare), "UTF-16BE without a BOM");
    // Byte offsets and columns count the original characters, not the
    // normalised ones.
    let s = "\u{e9}\u{200b}\u{e9}\nab \u{ff31}uill\u{ad}feather";
    assert_eq!(at(s.as_bytes().to_vec()), vec![format!("f.txt:2:4 (byte {}): denylist term #1", s.find('\u{ff31}').unwrap())]);
    assert_eq!(at(b"x\n\"quill\" +\n  \"feather\"".to_vec()), vec!["f.txt:2:2 (byte 3): denylist term #1".to_string()]);
    // A UTF-8 file with a BOM, and invalid bytes beside a term.
    assert!(caught_bytes(b"\xEF\xBB\xBFquillfeather"));
    assert!(caught_bytes(b"\xFF\xC0quillfeather\x80"));
    // A path with a look-alike; a term numbered past a comment.
    assert_eq!(hits(&[("docs/m\u{430}rrow vale.md".into(), b"clean".to_vec())]), vec!["docs/m\u{430}rrow vale.md (path): denylist term #2".to_string()]);
}

/// A whole-word term: `#[word]` on the line before it. `_` and letters or
/// digits are word characters, `-` is not; a term's own edges may be
/// anything (a term ending in `-` still needs a non-word character after).
const WORD_LIST: &str = "#[word]\nquill\n#[word]\nmarrow-\nfeather\n";

fn word_hits(list: &str, body: &str) -> Vec<String> {
    Matcher::new(db(), list).hits(&[("notes.md".into(), body.as_bytes().to_vec())])
}

#[test]
fn a_whole_word_term_matches_only_between_non_word_characters() {
    let m = Matcher::new(db(), WORD_LIST);
    assert_eq!(m.len(), 3, "the marker is a comment line: term numbers are unchanged");
    let term = |body: &str, k: usize| word_hits(WORD_LIST, body).iter().any(|h| h.ends_with(&format!("denylist term #{k}")));
    for body in ["quill", "a quill pen", "(Quill).", "\"quill\"", "a-quill-b", "quill\nx", "x QUILL"] {
        assert!(term(body, 1), "whole word in {body:?}");
    }
    for body in ["quills", "squill", "quill_pen", "my_quill", "quill2", "2quill", "quill\u{e9}"] {
        assert!(!term(body, 1), "inside a word in {body:?}");
    }
    // `-` is not a word character, so a hyphenated compound still names
    // the word; `_` is, so an identifier does not.
    assert!(term("pre-quill", 1) && !term("pre_quill", 1));
    // A term ending in `-`: the character after it counts too.
    for body in ["marrow-", "\"marrow-\"", "the marrow- x", "marrow-."] {
        assert!(term(body, 2), "whole word in {body:?}");
    }
    for body in ["marrow-vale", "`marrow-x`", "marrow-_", "omarrow- x"] {
        assert!(!term(body, 2), "inside a word in {body:?}");
    }
    // A later whole-word place is found past earlier embedded ones, and
    // reported at its own place.
    assert_eq!(word_hits(WORD_LIST, "squill quills quill"), vec!["notes.md:1:15 (byte 14): denylist term #1".to_string()]);
    // Paths too.
    let path = |p: &str| Matcher::new(db(), WORD_LIST).hits(&[(p.into(), b"clean".to_vec())]);
    assert_eq!(path("docs/quill/x.md"), vec!["docs/quill/x.md (path): denylist term #1".to_string()]);
    assert!(path("docs/quills/x.md").is_empty() && path("docs/my_quill.md").is_empty());
}

#[test]
fn the_whole_word_marker_touches_only_the_next_line_and_plain_terms_are_unchanged() {
    // Unmarked terms stay substrings.
    assert!(!word_hits(WORD_LIST, "featherweight").is_empty(), "a plain term after a marked one");
    assert!(!word_hits("quill\n", "squills").is_empty(), "a plain term matches inside words");
    // A marker followed by a blank or a comment line is spent: the term
    // after it is plain (the safe side: a plain term catches more).
    assert!(!word_hits("#[word]\n\nquill\n", "squills").is_empty());
    assert!(!word_hits("#[word]\n# c\nquill\n", "squills").is_empty());
    // Padding around the marker is fine; another spelling is a comment.
    assert!(word_hits("  #[word]  \nquill\n", "squills").is_empty());
    assert!(!word_hits("#[Word]\nquill\n", "squills").is_empty());
    // The marker stands alone on its line: with a term after it, the line
    // is a comment like any other.
    assert!(Matcher::new(db(), "#[word] quill\n").is_empty());
}
