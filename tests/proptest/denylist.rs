//! `sources::is_denied`: every rule ignores letter case, so a secret's file
//! is refused whatever case its name is written in.
use atlas::sources::is_denied;
use proptest::prelude::*;

fn flip(name: &str, flips: &[bool]) -> String {
    name.chars().zip(flips.iter().cycle()).map(|(c, &f)| if f { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() }).collect()
}

proptest! {
    #[test]
    fn the_rules_ignore_letter_case(name in "[A-Za-z0-9._-]{1,20}", flips in prop::collection::vec(any::<bool>(), 1..8)) {
        prop_assert_eq!(is_denied(&name), is_denied(&flip(&name, &flips)), "{}", name);
    }

    #[test]
    fn a_known_secret_is_denied_in_any_case(i in 0usize..3, flips in prop::collection::vec(any::<bool>(), 1..8)) {
        let name = ["secrets.json", "secretsdata.json", ".env"][i];
        prop_assert!(is_denied(&flip(name, &flips)), "{}", name);
    }
}
