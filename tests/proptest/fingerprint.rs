//! A root's fingerprint (`code::walk(..).fingerprint_with(..)`): the same files
//! give the same fingerprint, a relevant file's change gives another, and a
//! file the walk skips changes nothing.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use atlas::code;
use proptest::prelude::*;

fn temp() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let t = std::env::temp_dir().join(format!("atlas-proptest-fp-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(&t).unwrap();
    t
}

fn fp(root: &Path) -> String {
    code::walk(root).unwrap().fingerprint_with(&[])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn the_fingerprint_follows_relevant_files_only(names in prop::collection::btree_set("[a-z]{1,6}", 1..5), pick in 0usize..5, skipped in prop_oneof![Just("target"), Just(".hidden"), Just("node_modules")]) {
        let root = temp();
        let names: Vec<String> = names.into_iter().collect();
        for n in &names {
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(root.join(format!("src/{n}.rs")), format!("pub fn {n}() {{}}\n")).unwrap();
        }
        let first = fp(&root);
        let again = fp(&root);
        fs::create_dir_all(root.join(skipped)).unwrap();
        fs::write(root.join(skipped).join("x.rs"), "pub fn skipped() {}\n").unwrap();
        let with_skipped = fp(&root);
        let edited = &names[pick % names.len()];
        fs::write(root.join(format!("src/{edited}.rs")), format!("pub fn {edited}() {{}}\npub fn more() {{}}\n")).unwrap();
        let after_edit = fp(&root);
        let _ = fs::remove_dir_all(&root);
        prop_assert_eq!(&again, &first, "the same files");
        prop_assert_eq!(&with_skipped, &first, "a file under {}/", skipped);
        prop_assert_ne!(&after_edit, &first, "an edit of src/{}.rs", edited);
    }
}
