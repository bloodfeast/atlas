//! A prepared statement: bind its parameters, then run it or read its rows.
//!
//! Every method that binds or steps takes `&mut self`, and the rows of a
//! query borrow the statement until they are dropped, so a statement is
//! never rebound or restarted while a row of it is being read.

use std::cell::Cell;
use std::ffi::c_int;

use super::bind::{Params, ToSql, ValueRef};
use super::conn::Connection;
use super::error::{Error, Result};
use super::row::{MappedRows, Row, Rows};
use super::{ffi, raw};

/// A compiled statement of one connection. Dropped, it is finalized, or,
/// when it came from [`Connection::prepare_cached`], returned to the cache.
pub struct Statement<'conn> {
    conn: &'conn Connection,
    /// `Some` until the drop.
    raw: Option<raw::Stmt>,
    /// The cache key, for a statement the cache lent out.
    key: Option<Box<str>>,
    /// SQLite's column count, read after the prepare and after every step
    /// (only a step compiles a statement again, when the schema changed),
    /// so reading a column does not ask SQLite for it each time.
    columns: Cell<c_int>,
}

/// A statement from [`Connection::prepare_cached`].
pub type CachedStatement<'conn> = Statement<'conn>;

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            match self.key.take() {
                Some(key) => self.conn.cache_put(key, raw),
                None => {
                    raw.finalize();
                }
            }
        }
    }
}

impl<'conn> Statement<'conn> {
    pub(super) fn new(conn: &'conn Connection, raw: raw::Stmt, key: Option<Box<str>>) -> Statement<'conn> {
        let columns = Cell::new(raw.column_count());
        Statement { conn, raw: Some(raw), key, columns }
    }

    /// One step of SQLite's, and the column count as that step left it.
    fn step_raw(&self) -> c_int {
        let raw = self.raw();
        let rc = raw.step();
        self.columns.set(raw.column_count());
        rc
    }

    pub(super) fn raw(&self) -> &raw::Stmt {
        match &self.raw {
            Some(raw) => raw,
            None => unreachable!("a statement is finalized only by its drop"),
        }
    }

    /// The number of columns a row of this statement has.
    pub fn column_count(&self) -> usize {
        usize::try_from(self.columns.get()).unwrap_or(0)
    }

    /// The number of parameters this statement takes.
    pub fn parameter_count(&self) -> usize {
        usize::try_from(self.raw().bind_parameter_count()).unwrap_or(0)
    }

    /// Fails unless the statement takes exactly `n` parameters.
    pub(super) fn ensure_parameter_count(&self, n: usize) -> Result<()> {
        let want = self.parameter_count();
        if want == n {
            Ok(())
        } else {
            Err(Error::ParamCount { got: n, want })
        }
    }

    /// Binds parameter `index` (from 1). SQLite copies text and blobs
    /// before this returns.
    pub(super) fn bind_value(&mut self, index: usize, value: ValueRef<'_>) -> Result<()> {
        // An index past c_int is past every statement's last parameter;
        // SQLite answers that with its range error.
        let i = c_int::try_from(index).unwrap_or(c_int::MAX);
        let raw = self.raw();
        let rc = match value {
            ValueRef::Null => raw.bind_null(i),
            ValueRef::Integer(v) => raw.bind_int64(i, v),
            ValueRef::Real(v) => raw.bind_double(i, v),
            ValueRef::Text(t) => raw.bind_text(i, t),
            ValueRef::Blob(b) => raw.bind_blob(i, b),
        };
        self.conn.check(rc)
    }

    /// Binds the items of `params` in order from `?1`, then fails unless
    /// there were exactly as many as the statement takes.
    pub(super) fn bind_iter<I>(&mut self, params: I) -> Result<()>
    where
        I: IntoIterator,
        I::Item: ToSql,
    {
        let want = self.parameter_count();
        let mut got = 0;
        for p in params {
            got += 1;
            if got > want {
                break;
            }
            self.bind_value(got, p.to_sql()?)?;
        }
        if got == want {
            Ok(())
        } else {
            Err(Error::ParamCount { got, want })
        }
    }

    /// Binds `params` and runs the statement to its end; the number of rows
    /// changed.
    ///
    /// # Errors
    ///
    /// [`Error::ParamCount`] before anything runs;
    /// [`Error::ExecuteReturnedResults`] when the statement yields a row;
    /// SQLite's reason.
    pub fn execute<P: Params>(&mut self, params: P) -> Result<usize> {
        params.bind_in(self)?;
        self.raw_execute()
    }

    /// Runs the statement to its end with the parameters as they are bound
    /// (none, for a statement that takes none).
    ///
    /// # Errors
    ///
    /// As [`Statement::execute`], after the binding.
    pub fn raw_execute(&mut self) -> Result<usize> {
        let rc = self.step_raw();
        let raw = self.raw();
        // Reset whatever happened, so the statement can run again; a failed
        // step's message is still the connection's after the reset.
        let reset = raw.reset();
        match rc {
            ffi::SQLITE_DONE if reset == ffi::SQLITE_OK => Ok(usize::try_from(self.conn.changes()).unwrap_or(usize::MAX)),
            ffi::SQLITE_DONE => Err(self.conn.fail(reset)),
            ffi::SQLITE_ROW => Err(Error::ExecuteReturnedResults),
            rc => Err(self.conn.fail(rc)),
        }
    }

    /// Binds `params` and returns the rows, stepped as they are asked for.
    ///
    /// # Errors
    ///
    /// [`Error::ParamCount`]; a value that does not bind.
    pub fn query<P: Params>(&mut self, params: P) -> Result<Rows<'_>> {
        params.bind_in(self)?;
        Ok(Rows::new(self))
    }

    /// Binds `params` and returns an iterator of `f` over the rows.
    ///
    /// # Errors
    ///
    /// As [`Statement::query`]; each item carries its own step's result.
    pub fn query_map<T, P, F>(&mut self, params: P, f: F) -> Result<MappedRows<'_, F>>
    where
        P: Params,
        F: FnMut(&Row<'_>) -> Result<T>,
    {
        self.query(params).map(|rows| rows.mapped(f))
    }

    /// Binds `params` and maps the first row.
    ///
    /// # Errors
    ///
    /// [`Error::NoRows`] when there is none; as [`Statement::query`]; the
    /// error `f` returns.
    pub fn query_row<T, P, F>(&mut self, params: P, f: F) -> Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let mut rows = self.query(params)?;
        match rows.next()? {
            Some(row) => f(row),
            None => Err(Error::NoRows),
        }
    }

    /// Binds `params` and says whether the query has a row.
    ///
    /// # Errors
    ///
    /// As [`Statement::query`]; SQLite's reason.
    pub fn exists<P: Params>(&mut self, params: P) -> Result<bool> {
        let mut rows = self.query(params)?;
        Ok(rows.next()?.is_some())
    }

    /// Steps once: whether there is a row.
    pub(super) fn step(&self) -> Result<bool> {
        match self.step_raw() {
            ffi::SQLITE_ROW => Ok(true),
            ffi::SQLITE_DONE => Ok(false),
            rc => Err(self.conn.fail(rc)),
        }
    }

    /// Ends the current run, so the statement can be bound and run again.
    pub(super) fn reset(&self) -> Result<()> {
        self.conn.check(self.raw().reset())
    }
}
