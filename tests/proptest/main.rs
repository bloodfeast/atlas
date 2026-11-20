//! Property tests across atlas (proptest): each module states a rule its code
//! keeps for any input, not only for the examples the other tests pick.
//!
//! `cargo test --test proptest`; `PROPTEST_CASES=N` runs more cases. A failing
//! case prints its minimal input; proptest also saves it under
//! `tests/proptest/proptest-regressions/` so the next run tries it first.

mod checkout;
mod denylist;
mod fingerprint;
mod grep;
mod hashes;
mod indexer_model;
mod items;
mod json;
mod literals;
mod paths;
mod parity;
mod protobuf;
mod stamps;
