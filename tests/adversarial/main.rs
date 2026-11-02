//! The adversarial suite: hostile input on every surface atlas reads,
//! property tests with a seeded generator, and fixed-size fuzz loops (deep
//! runs with `cargo test --release --test adversarial -- --ignored`).
//!
//! One test binary, so the suite adds one link step to `cargo test`.

#[path = "../common/mod.rs"]
mod common;

mod gen;
mod parsers;
mod protocol;
mod embed;
mod hooks;
mod images;
mod paths;
mod queries;
mod store;
mod update;
mod surfaces;
