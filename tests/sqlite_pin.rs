//! The vendored SQLite amalgamation is pinned: `vendor/sqlite/sqlite3.c` and
//! `sqlite3.h` are SQLite's own files, byte for byte, at one release. An edit
//! or an accidental re-vendor fails here; a deliberate upgrade changes these
//! constants (and `SQLITE_VERSION_NUMBER` in `src/db/ffi.rs`) in the same
//! commit.

use std::path::PathBuf;

/// SQLite 3.53.2, from the amalgamation libsqlite3-sys 0.38.2 bundles.
const SQLITE3_C_SHA256: &str = "0a409f1633283fa31a9126b11fbfd64a1991c5d30defad07e5745d4667f5e23d";
const SQLITE3_C_BYTES: usize = 9_507_037;
const SQLITE3_H_SHA256: &str = "9e69a1353a4288450b0d5239ede11fc7f1f4c8e5eb07491fc8317eacb5b7de7e";
const VERSION: &str = "3.53.2";
const VERSION_NUMBER: &str = "3053002";
const SOURCE_ID: &str = "2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24";

fn vendored(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor").join("sqlite").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The value of `#define NAME value` in the header, quotes removed.
fn define<'a>(header: &'a str, name: &str) -> &'a str {
    let line = header
        .lines()
        .find(|l| l.strip_prefix("#define ").and_then(|r| r.strip_prefix(name)).is_some_and(|r| r.starts_with(' ')))
        .unwrap_or_else(|| panic!("sqlite3.h has no #define {name}"));
    line["#define ".len() + name.len()..].trim().trim_matches('"')
}

#[test]
fn the_amalgamation_is_the_pinned_release_byte_for_byte() {
    let c = vendored("sqlite3.c");
    assert_eq!(c.len(), SQLITE3_C_BYTES, "sqlite3.c length");
    assert_eq!(atlas::sha256::hex(&c), SQLITE3_C_SHA256, "sqlite3.c SHA-256");
    let h = vendored("sqlite3.h");
    assert_eq!(atlas::sha256::hex(&h), SQLITE3_H_SHA256, "sqlite3.h SHA-256");
}

#[test]
fn the_header_names_the_pinned_version_and_source_id() {
    let h = String::from_utf8(vendored("sqlite3.h")).expect("sqlite3.h is UTF-8");
    assert_eq!(define(&h, "SQLITE_VERSION"), VERSION);
    assert_eq!(define(&h, "SQLITE_VERSION_NUMBER"), VERSION_NUMBER);
    assert_eq!(define(&h, "SQLITE_SOURCE_ID"), SOURCE_ID);
    // The same id is compiled into sqlite3.c, so the two files are one release.
    let c = vendored("sqlite3.c");
    assert!(c.windows(SOURCE_ID.len()).any(|w| w == SOURCE_ID.as_bytes()), "sqlite3.c does not carry the header's SQLITE_SOURCE_ID");
}
