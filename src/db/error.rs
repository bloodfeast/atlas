//! Errors of the owned binding. `Display` writes the text rusqlite writes
//! for the same failure: atlas turns every database error into its message
//! (`Error::Db`), prints it, and in a few places matches on it ("no such
//! table", "locked"), so the text is behaviour.

use std::fmt;
use std::path::PathBuf;

use super::bind::Type;

/// A result of the binding.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What went wrong in a database call.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// SQLite failed a call. `code` is the extended result code; `msg` is
    /// SQLite's message, copied at the failing call.
    Sqlite {
        /// The extended result code.
        code: i32,
        /// SQLite's message.
        msg: Box<str>,
    },
    /// SQLite rejected a statement's text and said where: `offset` is the
    /// byte in `sql` the error is at.
    SqlInput {
        /// The extended result code.
        code: i32,
        /// SQLite's message.
        msg: Box<str>,
        /// The statement text.
        sql: Box<str>,
        /// The byte of `sql` the error is at.
        offset: i32,
    },
    /// A query that must return a row returned none.
    NoRows,
    /// `execute` ran a statement that returned a row.
    ExecuteReturnedResults,
    /// One statement was asked for and the text holds more.
    MultipleStatement,
    /// A column index past the statement's last column.
    InvalidColumnIndex(usize),
    /// A column holds a datatype the requested type does not read from.
    InvalidColumnType {
        /// The column, from 0.
        idx: usize,
        /// The column's name.
        name: Box<str>,
        /// The datatype stored.
        ty: Type,
    },
    /// A stored integer outside the requested type's range.
    IntOverflow {
        /// The column, from 0.
        idx: usize,
        /// The stored integer.
        value: i64,
    },
    /// A TEXT column that is not UTF-8.
    Utf8 {
        /// The column, from 0.
        idx: usize,
        /// Where the text stops being UTF-8.
        err: std::str::Utf8Error,
    },
    /// The statement takes `want` parameters and `got` were given.
    ParamCount {
        /// How many were given.
        got: usize,
        /// How many the statement takes.
        want: usize,
    },
    /// An unsigned parameter above `i64::MAX`.
    ParamRange,
    /// A database path that is not UTF-8 (Windows only).
    InvalidPath(PathBuf),
    /// A database path with a NUL byte in it.
    Nul(std::ffi::NulError),
}

impl Error {
    /// The primary result code of a SQLite failure (`code & 0xff`), 0 for
    /// an error that did not come from SQLite.
    pub fn primary(&self) -> i32 {
        match self {
            Error::Sqlite { code, .. } | Error::SqlInput { code, .. } => code & 0xff,
            _ => 0,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sqlite { msg, .. } => write!(f, "{msg}"),
            Error::SqlInput { msg, sql, offset, .. } => write!(f, "{msg} in {sql} at offset {offset}"),
            Error::NoRows => write!(f, "Query returned no rows"),
            Error::ExecuteReturnedResults => write!(f, "Execute returned results - did you mean to call query?"),
            Error::MultipleStatement => write!(f, "Multiple statements provided"),
            Error::InvalidColumnIndex(i) => write!(f, "Invalid column index: {i}"),
            Error::InvalidColumnType { idx, name, ty } => write!(f, "Invalid column type {ty} at index: {idx}, name: {name}"),
            Error::IntOverflow { idx, value } => write!(f, "Integer {value} out of range at index {idx}"),
            Error::Utf8 { idx, err } => write!(f, "{err} at index {idx}"),
            Error::ParamCount { got, want } => write!(f, "Wrong number of parameters passed to query. Got {got}, needed {want}"),
            Error::ParamRange => write!(f, "out of range integral type conversion attempted"),
            Error::InvalidPath(p) => write!(f, "Invalid path: {}", p.to_string_lossy()),
            Error::Nul(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Reads "no row" as `None` instead of an error.
pub trait OptionalExtension<T> {
    /// `Ok(None)` for [`Error::NoRows`]; anything else unchanged.
    ///
    /// # Errors
    ///
    /// Every error but [`Error::NoRows`].
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(Error::NoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
