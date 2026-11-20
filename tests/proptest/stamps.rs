//! `store::utc` stamps: their text order is their time order, so every
//! `modified > indexed` comparison of two stamps compares times.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use atlas::store::utc;
use proptest::prelude::*;

/// 2100-01-01, in milliseconds since the epoch.
const MAX_MS: u64 = 4_102_444_800_000;

fn at(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

proptest! {
    #[test]
    fn stamp_order_is_time_order(a in 0..MAX_MS, b in 0..MAX_MS) {
        let (sa, sb) = (utc(at(a)), utc(at(b)));
        prop_assert_eq!(sa.len(), 24, "{}", sa);
        prop_assert_eq!(a.cmp(&b), sa.cmp(&sb), "{} {}", sa, sb);
    }

    #[test]
    fn one_millisecond_gives_one_stamp(ms in 0..MAX_MS, ns in 0u64..1_000_000) {
        prop_assert_eq!(utc(at(ms)), utc(at(ms) + Duration::from_nanos(ns)));
    }
}
