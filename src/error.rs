//! The one error type every fallible operation in the crate returns.

use std::fmt;

/// Why a request failed. Every variant carries a plain message that is
/// returned to the client verbatim as the tool error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// An argument was missing, malformed or out of range.
    InvalidArgument(String),
    /// A path was refused by the confinement rules (absolute, `..`, dot
    /// folder, drive prefix, NUL, `:` stream, wrong extension, or it resolves
    /// outside its vault root).
    PathRejected(String),
    /// The named vault, note, heading or index does not exist.
    NotFound(String),
    /// An exclusive create found the file already there.
    AlreadyExists(String),
    /// The caller's `expected_sha256` does not match the file on disk.
    StaleSha {
        /// The hash the caller sent.
        expected: String,
        /// The hash of the file as it is now.
        actual: String,
    },
    /// The caller's `expected_sha` does not match a memory record's head.
    StaleRecord {
        /// The record.
        key: String,
        /// The hash (or prefix) the caller sent.
        expected: String,
        /// The head version's hash.
        actual: String,
    },
    /// A read, write or result set is over its fixed cap.
    TooLarge(String),
    /// The file system refused an operation.
    Io(String),
    /// The SQLite store refused an operation.
    Db(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            Error::PathRejected(m) => write!(f, "path rejected: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::AlreadyExists(m) => write!(f, "already exists: {m}"),
            Error::StaleSha { expected, actual } => {
                write!(f, "stale edit: expected_sha256 {expected} does not match the current file {actual}; read the note again")
            }
            Error::StaleRecord { key, expected, actual } => {
                write!(f, "stale revision: expected_sha {expected} does not match the head of {key} (sha={}); recall {{key: \"{key}\"}} to read the current version, then revise it", actual.get(..16).unwrap_or(actual))
            }
            Error::TooLarge(m) => write!(f, "too large: {m}"),
            Error::Io(m) => write!(f, "io: {m}"),
            Error::Db(m) => write!(f, "database: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}

/// Shorthand for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;
