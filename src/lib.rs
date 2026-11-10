//! `atlas`: a local MCP index over Obsidian vaults and read-only Markdown
//! sources.
//!
//! Security properties, each enforced in one place:
//!
//! * No network at query time. The I/O is stdio, the vault folders and the
//!   SQLite files. No dependency opens a socket. The one network access is
//!   the model download at setup ([`models`]): the system `curl`, started
//!   without a shell, fetching pinned `https` URLs whose SHA-256 is checked
//!   before a file is used.
//! * Confinement ([`paths`]). Vault roots are fixed at launch; every request
//!   path is checked lexically and again after canonicalisation.
//! * No deletion and no rename. No tool removes or moves a note.
//! * Additive or exact writes ([`store::Store::modify`],
//!   [`store::Store::create`]), atomic on disk, serialised across processes
//!   by the database write lock, and logged in the `writes` table.
//! * Size caps ([`fsio::MAX_READ`], [`fsio::MAX_WRITE`],
//!   [`tools::MAX_RESULTS`]).
#![deny(missing_docs)]
// Unsafe code is denied everywhere except the modules that need it and allow
// it locally (the change-notification FFI in platform, the vector scans in
// scan).
#![deny(unsafe_code)]

pub mod baseline;
pub mod calibrate;
pub mod chain;
pub mod checkpoint;
pub mod chunk;
pub mod compact;
pub mod claude_settings;
pub mod code;
pub mod code_index;
pub mod config;
pub mod crop;
pub mod digest;
pub mod dirs;
pub mod discussion;
pub mod embed;
pub mod error;
pub mod fsio;
pub mod glob;
pub mod grammar;
pub mod handles;
pub mod hook_hits;
pub mod hook_rewrite;
pub mod hooks;
pub mod ideas;
pub mod index;
pub mod json;
pub mod langs;
pub mod lex;
pub mod links;
pub mod markdown;
pub mod memory;
pub mod models;
pub mod orchestrate;
pub mod outline;
pub mod paths;
pub mod picture;
mod platform;
pub mod pr_digest;
pub mod project;
pub mod proto;
pub mod protobuf;
pub mod protocol;
pub mod lanes;
pub mod queue;
pub mod recall;
pub mod redact;
pub mod records;
pub mod regex;
pub mod report;
pub mod resume;
pub mod reviews;
pub mod registry;
pub mod run;
pub mod scan;
pub mod scanner;
pub mod scip;
pub mod semantic;
pub mod sha256;
pub mod sources;
pub mod store;
pub mod text;
pub mod tokens;
pub mod update;
pub mod watch;
pub mod wgsl;
pub mod tools;
pub mod transcripts;

pub use error::{Error, Result};
