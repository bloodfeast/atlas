//! RELEASE-SCAN: the built atlas binary holds no secret-shaped run.
//!
//! v0.2.0's sign step was refused by GitHub push protection: the linker
//! had packed `src/redact.rs`'s prefix literals end to end, and the run
//! `sk_test_` plus thirty-odd token characters in the binary read as a
//! leaked key. `redact.rs` now keeps its prefixes in one space-separated
//! literal; this test scans the binary cargo built for the test run with
//! the release's own matcher (`xtask/src/secret_scan.rs`, included by
//! path), and checks that matcher against synthetic shapes.
//!
//! Every key-shaped fixture is assembled at run time, so no source file
//! holds one (the public repository's pushes are scanned too).

#[path = "../xtask/src/secret_scan.rs"]
mod secret_scan;

use secret_scan::scan;

#[test]
fn the_built_binary_holds_no_secret_shaped_run() {
    let path = env!("CARGO_BIN_EXE_atlas");
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let hits = scan(&bytes);
    assert!(hits.is_empty(), "{path}: {} secret-shaped run(s):\n{}", hits.len(), hits.iter().map(|h| format!("  {h}\n")).collect::<String>());
}

/// v0.2.0's prefix array, as the linker laid it out.
fn v020_rodata() -> Vec<u8> {
    let old = ["sk-", "sk_live_", "sk_test_", "rk_live_", "ghp_", "gho_", "ghs_", "ghu_", "ghr_", "github_pat_", "xoxb-", "xoxp-", "xoxa-", "xoxs-", "glpat-", "AIza", "hf_", "npm_", "pypi-", "dop_v1_", "SG.", "ya29."];
    let mut b = b"\0\x01".to_vec();
    b.extend_from_slice(old.concat().as_bytes());
    b.extend_from_slice(b"modelssourcequote\0");
    b
}

#[test]
fn v020s_packed_prefixes_are_caught() {
    let hits = scan(&v020_rodata());
    assert!(!hits.is_empty());
    assert_eq!(hits[0].offset, 2);
    // From the test key's own prefix as well, as push protection saw it.
    let tail = v020_rodata();
    let at = tail.windows(8).position(|w| w == b"sk_test_".as_slice()).unwrap();
    let hits = scan(&tail[at..]);
    assert_eq!(hits[0].prefix, "sk_test_");
    assert!(hits[0].rule.contains("test secret key"), "{}", hits[0]);
}

#[test]
fn todays_prefix_table_is_not_a_run() {
    let table = " sk- sk_live_ sk_test_ rk_live_ ghp_ gho_ ghs_ ghu_ ghr_ github_pat_ xoxb- xoxp- xoxa- xoxs- glpat- AIza hf_ npm_ pypi- dop_v1_ SG. ya29. ";
    // Packed between two other literals, as the linker may place it.
    let mut b = b"abcdefghijklmnopqrstuvwxyz0123456789".to_vec();
    b.extend_from_slice(table.as_bytes());
    b.extend_from_slice(b"abcdefghijklmnopqrstuvwxyz0123456789");
    assert_eq!(scan(&b), vec![]);
}

fn key(prefix: &str, body: &str) -> Vec<u8> {
    format!(" {prefix}{body} ").into_bytes()
}

#[test]
fn provider_shapes_are_caught_and_near_misses_are_not() {
    let alnum36 = "0123456789abcdefABCDEF0123456789abcd";
    let upper16 = "ABCDEFGHIJ012345";
    let hit = [
        key("ghp", &format!("_{alnum36}")),
        key("github", &format!("_pat_{alnum36}{alnum36}")),
        key("sk_live", "_0123456789abcdefgh"),
        key("rk_test", "_0123456789abcdefgh"),
        key("sk", &format!("-proj-{alnum36}")),
        key("xoxb", "-1234567890-abcdef"),
        key("glpat", "-0123456789abcdefghij"),
        key("AIza", &format!("Sy{alnum36}")),
        key("hf", &format!("_{alnum36}")),
        key("npm", &format!("_{alnum36}")),
        key("dop", &format!("_v1_{alnum36}{alnum36}")),
        key("AKIA", upper16),
        key("ASIA", upper16),
        format!("x\n-----BEGIN RSA PRIVATE KEY-----\n{alnum36}{alnum36}\n").into_bytes(),
    ];
    for b in &hit {
        assert_eq!(scan(b).len(), 1, "{}", String::from_utf8_lossy(b));
    }
    let miss = [
        // Too short for the shape.
        key("ghp", "_0123456789"),
        key("sk", "-abc"),
        key("AKIA", "ABCD0123"),
        // Not at a token start.
        key("xghp", &format!("_{alnum36}")),
        key("task", &format!("-{alnum36}")),
        // The wrong class after the prefix.
        key("AKIA", "abcdefghij012345"),
        // A header with no key under it.
        b"-----BEGIN PRIVATE KEY-----\n".to_vec(),
        format!("-----BEGIN PUBLIC KEY-----\n{alnum36}{alnum36}\n").into_bytes(),
        b"plain text with ghp_ and sk_test_ named in prose".to_vec(),
    ];
    for b in &miss {
        assert_eq!(scan(b), vec![], "{}", String::from_utf8_lossy(b));
    }
}

#[test]
fn a_hit_names_the_place_and_never_the_key() {
    let body = "_SECRETBODY0123456789xyz";
    let mut b = vec![0u8; 100];
    b.extend_from_slice(&key("sk_test", body));
    let hits = scan(&b);
    assert_eq!(hits.len(), 1);
    let line = hits[0].to_string();
    assert!(line.starts_with("byte 101: test secret key"), "{line}");
    assert!(!line.contains("SECRETBODY"), "{line}");
    let lines = secret_scan::scan_files(&[("bin/atlas.exe".into(), b)]);
    assert_eq!(lines, vec![format!("bin/atlas.exe ({line})")]);
}
