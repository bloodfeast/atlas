//! atlas's SQLite binding: the one module the rest of the crate (and
//! `xtask`, and the tests) reaches SQLite through.
//!
//! Two backends give the same surface, chosen by feature, never both:
//!
//! * `rusqlite-backend` (the default): rusqlite with its bundled SQLite,
//!   re-exported.
//! * `owned-sqlite`: atlas's own binding over the vendored amalgamation
//!   (`vendor/sqlite`, compiled by `build.rs`): the C declarations in `ffi`,
//!   one thin function per call in `raw` (the only module that calls SQLite),
//!   and the safe wrapper in `conn`, `stmt`, `row`, `bind` and `error`. No
//!   lock sits anywhere in it: a [`Connection`] is `Send` and not `Sync`, so
//!   one thread owns it, its statement cache and every statement, row and
//!   transaction borrowed from it.
//!
//! The surface is the part of rusqlite's that atlas uses, with the same
//! names and the same error text, so call sites read the same on either
//! backend. Three helpers stand in for the places where the backends'
//! types differ: [`is_busy`], [`is_no_rows`] and [`execute_each`].

#[cfg(all(feature = "rusqlite-backend", feature = "owned-sqlite"))]
compile_error!(
    "features `rusqlite-backend` and `owned-sqlite` are both on: atlas links exactly one SQLite. \
     For the owned binding pass `--no-default-features --features owned-sqlite`."
);
#[cfg(not(any(feature = "rusqlite-backend", feature = "owned-sqlite")))]
compile_error!("no SQLite backend: turn on `rusqlite-backend` (the default) or `owned-sqlite`.");

#[cfg(all(feature = "rusqlite-backend", not(feature = "owned-sqlite")))]
mod rusqlite_backend {
    pub use rusqlite::{
        params, params_from_iter, types, CachedStatement, Connection, Error, ErrorCode, OpenFlags, OptionalExtension, Params, Result, Row,
        Rows, Statement, ToSql, Transaction, TransactionBehavior,
    };

    /// Whether `e` is SQLite's BUSY (another connection holds the lock).
    pub fn is_busy(e: &Error) -> bool {
        matches!(e, Error::SqliteFailure(f, _) if f.code == ErrorCode::DatabaseBusy)
    }

    /// Whether `e` is "the query returned no rows".
    pub fn is_no_rows(e: &Error) -> bool {
        matches!(e, Error::QueryReturnedNoRows)
    }

    /// Runs each statement of `sql` in turn (none takes parameters), calling
    /// `after` once each has run.
    ///
    /// # Errors
    ///
    /// The first statement that fails, or the first error `after` returns;
    /// nothing later runs.
    pub fn execute_each<E: From<Error>>(
        conn: &Connection,
        sql: &str,
        mut after: impl FnMut() -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        use rusqlite::fallible_iterator::FallibleIterator as _;
        let mut batch = rusqlite::Batch::new(conn, sql);
        while let Some(mut st) = batch.next()? {
            st.raw_execute()?;
            after()?;
        }
        Ok(())
    }
}
#[cfg(all(feature = "rusqlite-backend", not(feature = "owned-sqlite")))]
pub use rusqlite_backend::*;

#[cfg(all(feature = "owned-sqlite", not(feature = "rusqlite-backend")))]
mod bind;
#[cfg(all(feature = "owned-sqlite", not(feature = "rusqlite-backend")))]
mod conn;
#[cfg(all(feature = "owned-sqlite", not(feature = "rusqlite-backend")))]
mod error;
#[cfg(all(feature = "owned-sqlite", not(feature = "rusqlite-backend")))]
#[allow(unsafe_code)]
mod ffi;
#[cfg(all(feature = "owned-sqlite", not(feature = "rusqlite-backend")))]
#[allow(unsafe_code)]
mod raw;
#[cfg(all(feature = "owned-sqlite", not(feature = "rusqlite-backend")))]
mod row;
#[cfg(all(feature = "owned-sqlite", not(feature = "rusqlite-backend")))]
mod stmt;

#[cfg(all(feature = "owned-sqlite", not(feature = "rusqlite-backend")))]
mod owned {
    pub use super::bind::{params_from_iter, Params, ParamsFromIter, ToSql};
    pub use super::conn::{Connection, OpenFlags, Transaction, TransactionBehavior};
    pub use super::error::{Error, OptionalExtension, Result};
    pub use super::row::{MappedRows, Row, Rows};
    pub use super::stmt::{CachedStatement, Statement};
    pub use crate::__atlas_db_params as params;

    /// The value types of a row and of a parameter.
    pub mod types {
        pub use super::super::bind::{FromSql, FromSqlError, Type, Value, ValueRef};
    }

    /// Whether `e` is SQLite's BUSY (another connection holds the lock).
    pub fn is_busy(e: &Error) -> bool {
        e.primary() == super::ffi::SQLITE_BUSY
    }

    /// Whether `e` is "the query returned no rows".
    pub fn is_no_rows(e: &Error) -> bool {
        matches!(e, Error::NoRows)
    }

    /// Runs each statement of `sql` in turn (none takes parameters), calling
    /// `after` once each has run.
    ///
    /// # Errors
    ///
    /// The first statement that fails, or the first error `after` returns;
    /// nothing later runs.
    pub fn execute_each<E: From<Error>>(
        conn: &Connection,
        sql: &str,
        mut after: impl FnMut() -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let mut rest = sql;
        while let Some((mut st, tail)) = conn.prepare_first(rest)? {
            st.raw_execute()?;
            after()?;
            rest = tail;
        }
        Ok(())
    }
}
#[cfg(all(feature = "owned-sqlite", not(feature = "rusqlite-backend")))]
pub use owned::*;
