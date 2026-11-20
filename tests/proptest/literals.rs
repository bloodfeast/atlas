//! Literal text through atlas's own regex engine and its glob: an escaped
//! text matches exactly where it occurs, and a plain name globs itself.
use atlas::glob;
use atlas::regex::{self, Regex};
use proptest::prelude::*;

proptest! {
    #[test]
    fn an_escaped_text_matches_exactly_where_it_occurs(needle in "\\PC{1,6}", hay in "\\PC{0,24}") {
        let re = Regex::new(&regex::escape(&needle)).unwrap();
        prop_assert_eq!(re.is_match(&hay), hay.contains(&needle), "{:?} in {:?}", needle, hay);
        let around = format!("{hay}{needle}{hay}");
        prop_assert!(re.is_match(&around));
    }

    #[test]
    fn a_plain_name_globs_itself_in_any_case(name in "[A-Za-z0-9_.-]{1,12}") {
        prop_assert!(glob::matches(&name, &name));
        prop_assert!(glob::matches(&name.to_lowercase(), &name.to_uppercase()), "{}", name);
        prop_assert!(glob::matches("*", &name));
    }
}
