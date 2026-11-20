//! `project::checkout` reads `.git` files that anyone can write: junk never
//! makes it panic or hang, and an answer is the folder itself or one above it.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use atlas::project::checkout;
use proptest::prelude::*;

fn temp() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let t = std::env::temp_dir().join(format!("atlas-proptest-checkout-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(&t).unwrap();
    t
}

fn junk() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("gitdir:".to_string()),
        Just("gitdir:    ".to_string()),
        Just("gitdir: ../admin\n".to_string()),
        Just("gitdir: ../admin\r\n".to_string()),
        Just("\u{feff}gitdir: ../admin\n".to_string()),
        Just("gitdir: .\n".to_string()),
        Just("gitdir: .git\n".to_string()),
        Just("gitdir: /nonexistent/x\n".to_string()),
        "\\PC{0,24}",
        "gitdir: \\PC{0,16}",
    ]
}

fn link() -> impl Strategy<Value = String> {
    prop_oneof![Just("../..".to_string()), Just(String::new()), Just(".".to_string()), Just("..".to_string()), Just("/".to_string()), Just("../top/.git".to_string()), "\\PC{0,12}"]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn junk_git_files_never_break_checkout(dot in junk(), dot_is_dir in any::<bool>(), commondir in prop::option::of(link()), back in prop::option::of(link()), depth in 0usize..3) {
        let t = temp();
        let (top, admin) = (t.join("top"), t.join("admin"));
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(&top).unwrap();
        if dot_is_dir {
            fs::create_dir_all(top.join(".git")).unwrap();
        } else {
            fs::write(top.join(".git"), &dot).unwrap();
        }
        if let Some(c) = &commondir {
            fs::write(admin.join("commondir"), c).unwrap();
        }
        if let Some(b) = &back {
            fs::write(admin.join("gitdir"), b).unwrap();
        }
        let start = ["a", "b", "c"][..depth].iter().fold(top.clone(), |p, d| p.join(d));
        fs::create_dir_all(&start).unwrap();
        let got = checkout(&start);
        let start = atlas::paths::canonical(&start);
        let _ = fs::remove_dir_all(&t);
        if let Some(c) = got {
            prop_assert!(start.starts_with(&c.top), "{} is not below {}", start.display(), c.top.display());
        }
    }
}
