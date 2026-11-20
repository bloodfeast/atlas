//! The rows of a running query and the columns of one row.
//!
//! [`Rows::next`] lends each row out of the borrow of `Rows` itself, so a
//! row, and any text or blob slice read from it, ends before the next step,
//! the reset and the statement's drop: the three calls after which SQLite's
//! column memory is gone.

use std::ffi::c_int;

use super::bind::{FromSql, FromSqlError, ValueRef};
use super::error::{Error, Result};
use super::ffi;
use super::stmt::Statement;

/// The rows of one run of a statement. Dropped, it resets the statement.
#[must_use = "rows are stepped only as they are asked for"]
pub struct Rows<'stmt> {
    /// `None` once the run is over (the end was reached, or a step failed).
    stmt: Option<&'stmt Statement<'stmt>>,
    row: Option<Row<'stmt>>,
}

impl<'stmt> Rows<'stmt> {
    pub(super) fn new(stmt: &'stmt Statement<'stmt>) -> Rows<'stmt> {
        Rows { stmt: Some(stmt), row: None }
    }

    /// Steps to the next row; `None` at the end, and from then on.
    ///
    /// # Errors
    ///
    /// SQLite's reason for the failed step; the run is over.
    #[allow(clippy::should_implement_trait)] // A lending step, which Iterator cannot express.
    pub fn next(&mut self) -> Result<Option<&Row<'stmt>>> {
        self.row = None;
        if let Some(stmt) = self.stmt {
            match stmt.step() {
                Ok(true) => self.row = Some(Row { stmt }),
                Ok(false) => {
                    self.stmt = None;
                    stmt.reset()?;
                }
                Err(e) => {
                    self.stmt = None;
                    let _ = stmt.reset();
                    return Err(e);
                }
            }
        }
        Ok(self.row.as_ref())
    }

    /// An iterator of `f` over the rows.
    pub fn mapped<T, F>(self, f: F) -> MappedRows<'stmt, F>
    where
        F: FnMut(&Row<'_>) -> Result<T>,
    {
        MappedRows { rows: self, map: f }
    }
}

impl Drop for Rows<'_> {
    fn drop(&mut self) {
        if let Some(stmt) = self.stmt.take() {
            let _ = stmt.reset();
        }
    }
}

/// An iterator of a closure over the rows of a query; see
/// [`Statement::query_map`].
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct MappedRows<'stmt, F> {
    rows: Rows<'stmt>,
    map: F,
}

impl<T, F> Iterator for MappedRows<'_, F>
where
    F: FnMut(&Row<'_>) -> Result<T>,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Result<T>> {
        match self.rows.next() {
            Ok(Some(row)) => Some((self.map)(row)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// How a column is named when it is read: by its position from 0.
pub trait RowIndex {
    /// The position, checked against the statement's column count.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidColumnIndex`] past the last column.
    #[doc(hidden)]
    fn idx(&self, stmt: &Statement<'_>) -> Result<usize>;
}

impl RowIndex for usize {
    fn idx(&self, stmt: &Statement<'_>) -> Result<usize> {
        if *self < stmt.column_count() {
            Ok(*self)
        } else {
            Err(Error::InvalidColumnIndex(*self))
        }
    }
}

/// The current row of a query.
pub struct Row<'stmt> {
    stmt: &'stmt Statement<'stmt>,
}

impl Row<'_> {
    /// Column `idx` as `T`, by the type SQLite stores: nothing is
    /// converted, and text is checked to be UTF-8.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidColumnIndex`]; [`Error::InvalidColumnType`] when the
    /// stored type is not one `T` reads; [`Error::IntOverflow`];
    /// [`Error::Utf8`].
    pub fn get<I: RowIndex, T: FromSql>(&self, idx: I) -> Result<T> {
        let idx = idx.idx(self.stmt)?;
        let value = self.value_ref(idx);
        T::column_result(value).map_err(|e| match e {
            FromSqlError::InvalidType => {
                Error::InvalidColumnType { idx, name: self.stmt.raw().column_name(col(idx)).into(), ty: value.data_type() }
            }
            FromSqlError::OutOfRange(value) => Error::IntOverflow { idx, value },
            FromSqlError::Utf8(err) => Error::Utf8 { idx, err },
        })
    }

    /// Column `idx` as SQLite stores it; text and blobs are borrowed from
    /// SQLite's memory for as long as this row is.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidColumnIndex`].
    pub fn get_ref<I: RowIndex>(&self, idx: I) -> Result<ValueRef<'_>> {
        let idx = idx.idx(self.stmt)?;
        Ok(self.value_ref(idx))
    }

    /// The value of column `idx`, which is in range.
    fn value_ref(&self, idx: usize) -> ValueRef<'_> {
        let raw = self.stmt.raw();
        let i = col(idx);
        match raw.column_type(i) {
            ffi::SQLITE_INTEGER => ValueRef::Integer(raw.column_int64(i)),
            ffi::SQLITE_FLOAT => ValueRef::Real(raw.column_double(i)),
            // SAFETY (both arms): the column's type was read on the line
            // above and is the one asked for, so SQLite converts nothing and
            // no pointer into the row moves. The slice is borrowed from
            // `&self`: a `Row` exists only inside its `Rows`, lent out by
            // `Rows::next(&mut self)`, so it ends before the next step and
            // before the `Rows` drops (the reset); the `Rows` in turn
            // borrows the statement, whose rebinding, rerun and drop (the
            // finalize) all need it back.
            #[allow(unsafe_code)]
            ffi::SQLITE_TEXT => ValueRef::Text(unsafe { raw.column_text(i) }),
            #[allow(unsafe_code)]
            ffi::SQLITE_BLOB => ValueRef::Blob(unsafe { raw.column_blob(i) }),
            // SQLITE_NULL: there is no fifth type.
            _ => ValueRef::Null,
        }
    }
}

/// A checked column position as SQLite takes it. The position is below the
/// column count, which is a `c_int`.
fn col(idx: usize) -> c_int {
    c_int::try_from(idx).unwrap_or(c_int::MAX)
}
