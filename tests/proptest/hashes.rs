//! `atlas::sha256`: hashing in pieces equals hashing at once, and `ct_eq` is
//! equality.
use atlas::sha256::{self, Hasher};
use proptest::prelude::*;

proptest! {
    #[test]
    fn hashing_in_pieces_equals_hashing_at_once(data in prop::collection::vec(any::<u8>(), 0..300), cuts in prop::collection::vec(0usize..300, 0..6)) {
        let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(data.len())).collect();
        cuts.sort_unstable();
        let mut h = Hasher::new();
        let mut from = 0;
        for c in cuts {
            h.update(&data[from..c]);
            from = c;
        }
        h.update(&data[from..]);
        prop_assert_eq!(h.hex(), sha256::hex(&data));
    }

    #[test]
    fn ct_eq_is_equality(a in prop::collection::vec(any::<u8>(), 0..16), b in prop::collection::vec(any::<u8>(), 0..16)) {
        prop_assert_eq!(sha256::ct_eq(&a, &b), a == b);
        prop_assert!(sha256::ct_eq(&a, &a));
    }
}
