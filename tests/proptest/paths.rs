//! Path rules: the code index's skipped folders, remote URLs, folder keys and
//! fitted names.
use atlas::code;
use atlas::project::{fit_name, key_within, normalize_remote};
use proptest::prelude::*;

fn dir() -> impl Strategy<Value = String> {
    "[a-z]{1,6}".prop_filter("not a skipped folder", |d| d != "target")
}

proptest! {
    #[test]
    fn a_skipped_folder_anywhere_hides_a_file(pre in prop::collection::vec(dir(), 0..3), skip in prop_oneof![Just("target".to_string()), Just("node_modules".to_string()), "\\.[a-z]{1,6}"], post in prop::collection::vec(dir(), 0..3), name in "[a-z]{1,8}") {
        let mut parts = pre;
        parts.push(skip);
        parts.extend(post);
        parts.push(format!("{name}.rs"));
        prop_assert!(!code::relevant(&parts.join("/")), "{}", parts.join("/"));
    }

    #[test]
    fn a_rust_file_outside_skipped_folders_is_relevant(dirs in prop::collection::vec(dir(), 0..4), name in "[a-z]{1,8}") {
        let mut parts = dirs;
        parts.push(format!("{name}.rs"));
        prop_assert!(code::relevant(&parts.join("/")), "{}", parts.join("/"));
    }

    #[test]
    fn one_remote_in_any_form_normalizes_alike(host in "[a-z]{1,8}\\.[a-z]{2,3}", owner in "[A-Za-z0-9-]{1,8}", repo in "[A-Za-z0-9_-]{1,10}") {
        let https = normalize_remote(&format!("https://{host}/{owner}/{repo}.git"));
        prop_assert_eq!(&https, &normalize_remote(&format!("git@{host}:{owner}/{repo}")));
        prop_assert_eq!(&https, &normalize_remote(&format!("HTTPS://{}/{owner}/{repo}/", host.to_uppercase())));
        prop_assert_eq!(normalize_remote(&https), https.clone());
    }

    #[test]
    fn a_key_is_within_itself_and_its_folders_only(root in "/[a-z]{1,6}(/[a-z]{1,6}){0,2}", child in "[a-z]{1,6}", tail in "[a-z]{1,4}") {
        let (inside, beside) = (format!("{root}/{child}"), format!("{root}{tail}"));
        prop_assert!(key_within(&root, &root));
        prop_assert!(key_within(&inside, &root));
        prop_assert!(!key_within(&beside, &root));
        prop_assert!(!key_within(&root, &inside));
    }

    #[test]
    fn a_fitted_name_fits_and_is_free(base in "\\PC{0,40}", max in 4usize..24, taken in prop::collection::vec("[a-z]{1,8}", 0..5)) {
        let free = |c: &str| !taken.iter().any(|t| t == c);
        let n = fit_name(&base, max, free);
        prop_assert!(n.len() <= max, "{:?} is {} bytes, max {}", n, n.len(), max);
        prop_assert!(free(&n), "{:?} is taken", n);
    }
}
