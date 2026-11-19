//! Values in and out: what a parameter binds as ([`ToSql`], [`Params`]) and
//! what a column reads as ([`FromSql`]), with rusqlite's conversion rules.
//!
//! A column is read as the type SQLite stores, never converted by SQLite: an
//! INTEGER reads as an integer type (range-checked) or a float, a REAL as a
//! float, TEXT as a string (UTF-8 validated on every read: SQLite does not
//! guarantee it), a BLOB as bytes, NULL as `None`. Anything else is
//! [`FromSqlError::InvalidType`].

use std::fmt;

use super::error::{Error, Result};
use super::stmt::Statement;

/// The datatype of a stored value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    /// NULL.
    Null,
    /// A signed 64-bit integer.
    Integer,
    /// A 64-bit float.
    Real,
    /// Text.
    Text,
    /// Bytes.
    Blob,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Type::Null => "Null",
            Type::Integer => "Integer",
            Type::Real => "Real",
            Type::Text => "Text",
            Type::Blob => "Blob",
        })
    }
}

/// An owned value of any datatype.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// NULL.
    Null,
    /// A signed 64-bit integer.
    Integer(i64),
    /// A 64-bit float.
    Real(f64),
    /// Text.
    Text(String),
    /// Bytes.
    Blob(Vec<u8>),
}

/// A borrowed value of any datatype. Text is bytes: SQLite does not promise
/// UTF-8, so the reader validates (see [`FromSql`] for `String`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueRef<'a> {
    /// NULL.
    Null,
    /// A signed 64-bit integer.
    Integer(i64),
    /// A 64-bit float.
    Real(f64),
    /// Text, as its bytes.
    Text(&'a [u8]),
    /// Bytes.
    Blob(&'a [u8]),
}

impl ValueRef<'_> {
    /// The value's datatype.
    pub fn data_type(&self) -> Type {
        match self {
            ValueRef::Null => Type::Null,
            ValueRef::Integer(_) => Type::Integer,
            ValueRef::Real(_) => Type::Real,
            ValueRef::Text(_) => Type::Text,
            ValueRef::Blob(_) => Type::Blob,
        }
    }
}

impl<'a> From<&'a Value> for ValueRef<'a> {
    fn from(v: &'a Value) -> ValueRef<'a> {
        match v {
            Value::Null => ValueRef::Null,
            Value::Integer(i) => ValueRef::Integer(*i),
            Value::Real(r) => ValueRef::Real(*r),
            Value::Text(s) => ValueRef::Text(s.as_bytes()),
            Value::Blob(b) => ValueRef::Blob(b),
        }
    }
}

// ---- Parameters ----

/// A value that binds to a statement parameter.
pub trait ToSql {
    /// The value as SQLite takes it, borrowed from `self`.
    ///
    /// # Errors
    ///
    /// [`Error::ParamRange`] for an unsigned integer above `i64::MAX`.
    fn to_sql(&self) -> Result<ValueRef<'_>>;
}

macro_rules! to_sql_integer {
    ($($t:ty),*) => {$(
        impl ToSql for $t {
            fn to_sql(&self) -> Result<ValueRef<'_>> {
                Ok(ValueRef::Integer(i64::from(*self)))
            }
        }
    )*};
}
to_sql_integer!(i8, i16, i32, i64, u8, u16, u32, bool);

macro_rules! to_sql_checked {
    ($($t:ty),*) => {$(
        impl ToSql for $t {
            fn to_sql(&self) -> Result<ValueRef<'_>> {
                i64::try_from(*self).map(ValueRef::Integer).map_err(|_| Error::ParamRange)
            }
        }
    )*};
}
to_sql_checked!(isize, u64, usize);

impl ToSql for f64 {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Real(*self))
    }
}

impl ToSql for f32 {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Real(f64::from(*self)))
    }
}

impl ToSql for str {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Text(self.as_bytes()))
    }
}

impl ToSql for String {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Text(self.as_bytes()))
    }
}

impl ToSql for [u8] {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Blob(self))
    }
}

impl ToSql for Vec<u8> {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Blob(self))
    }
}

impl<const N: usize> ToSql for [u8; N] {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Blob(self))
    }
}

impl ToSql for Value {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::from(self))
    }
}

impl<T: ToSql> ToSql for Option<T> {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        match self {
            Some(v) => v.to_sql(),
            None => Ok(ValueRef::Null),
        }
    }
}

impl<T: ToSql + ?Sized> ToSql for &T {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        (**self).to_sql()
    }
}

impl<T: ToSql + ?Sized> ToSql for Box<T> {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        (**self).to_sql()
    }
}

impl<T: ToSql + ToOwned + ?Sized> ToSql for std::borrow::Cow<'_, T> {
    fn to_sql(&self) -> Result<ValueRef<'_>> {
        self.as_ref().to_sql()
    }
}

/// The parameters of one execution, bound in order from `?1`.
pub trait Params {
    /// Binds every parameter of `stmt`.
    ///
    /// # Errors
    ///
    /// [`Error::ParamCount`] when the statement takes a different number,
    /// before anything is stepped; a value that does not bind.
    #[doc(hidden)]
    fn bind_in(self, stmt: &mut Statement<'_>) -> Result<()>;
}

impl Params for () {
    fn bind_in(self, stmt: &mut Statement<'_>) -> Result<()> {
        stmt.ensure_parameter_count(0)
    }
}

// `[]` with no element type: the one zero-length array that is `Params`.
impl Params for [&(dyn ToSql + Send + Sync); 0] {
    fn bind_in(self, stmt: &mut Statement<'_>) -> Result<()> {
        stmt.ensure_parameter_count(0)
    }
}

impl Params for &[&dyn ToSql] {
    fn bind_in(self, stmt: &mut Statement<'_>) -> Result<()> {
        stmt.bind_iter(self)
    }
}

macro_rules! params_array {
    ($($n:literal)*) => {$(
        impl<T: ToSql> Params for [T; $n] {
            fn bind_in(self, stmt: &mut Statement<'_>) -> Result<()> {
                stmt.ensure_parameter_count($n)?;
                for (i, v) in self.iter().enumerate() {
                    stmt.bind_value(i + 1, v.to_sql()?)?;
                }
                Ok(())
            }
        }
        impl<T: ToSql + ?Sized> Params for &[&T; $n] {
            fn bind_in(self, stmt: &mut Statement<'_>) -> Result<()> {
                stmt.ensure_parameter_count($n)?;
                for (i, v) in self.iter().enumerate() {
                    stmt.bind_value(i + 1, v.to_sql()?)?;
                }
                Ok(())
            }
        }
    )*};
}
params_array!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);

macro_rules! params_tuple {
    ($n:literal: $($t:ident $i:tt),+) => {
        impl<$($t: ToSql),+> Params for ($($t,)+) {
            fn bind_in(self, stmt: &mut Statement<'_>) -> Result<()> {
                stmt.ensure_parameter_count($n)?;
                $(stmt.bind_value($i + 1, self.$i.to_sql()?)?;)+
                Ok(())
            }
        }
    };
}
params_tuple!(1: A 0);
params_tuple!(2: A 0, B 1);
params_tuple!(3: A 0, B 1, C 2);
params_tuple!(4: A 0, B 1, C 2, D 3);
params_tuple!(5: A 0, B 1, C 2, D 3, E 4);
params_tuple!(6: A 0, B 1, C 2, D 3, E 4, F 5);
params_tuple!(7: A 0, B 1, C 2, D 3, E 4, F 5, G 6);
params_tuple!(8: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
params_tuple!(9: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
params_tuple!(10: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
params_tuple!(11: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
params_tuple!(12: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);
params_tuple!(13: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11, M 12);
params_tuple!(14: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11, M 12, N 13);
params_tuple!(15: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11, M 12, N 13, O 14);
params_tuple!(16: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11, M 12, N 13, O 14, P 15);

/// Parameters from an iterator (a variable-length list); see
/// [`params_from_iter`].
#[derive(Clone, Debug)]
pub struct ParamsFromIter<I>(I);

/// Binds the items of `iter` in order.
pub fn params_from_iter<I>(iter: I) -> ParamsFromIter<I>
where
    I: IntoIterator,
    I::Item: ToSql,
{
    ParamsFromIter(iter)
}

impl<I> Params for ParamsFromIter<I>
where
    I: IntoIterator,
    I::Item: ToSql,
{
    fn bind_in(self, stmt: &mut Statement<'_>) -> Result<()> {
        stmt.bind_iter(self.0)
    }
}

/// `params![a, b]`: a parameter list of mixed types, as `&[&dyn ToSql]`.
#[doc(hidden)]
#[macro_export]
macro_rules! __atlas_db_params {
    () => {
        &[] as &[&dyn $crate::db::ToSql]
    };
    ($($param:expr),+ $(,)?) => {
        &[$(&$param as &dyn $crate::db::ToSql),+] as &[&dyn $crate::db::ToSql]
    };
}

// ---- Columns ----

/// Why a stored value does not read as the type asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum FromSqlError {
    /// The stored datatype is not one the type reads from.
    InvalidType,
    /// An integer outside the type's range.
    OutOfRange(i64),
    /// TEXT that is not UTF-8.
    Utf8(std::str::Utf8Error),
}

/// A type a column reads as.
pub trait FromSql: Sized {
    /// Converts the stored value.
    ///
    /// # Errors
    ///
    /// See [`FromSqlError`].
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError>;
}

impl FromSql for i64 {
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
        match value {
            ValueRef::Integer(i) => Ok(i),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

macro_rules! from_sql_integer {
    ($($t:ty),*) => {$(
        impl FromSql for $t {
            fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
                let i = i64::column_result(value)?;
                <$t>::try_from(i).map_err(|_| FromSqlError::OutOfRange(i))
            }
        }
    )*};
}
from_sql_integer!(i8, i16, i32, isize, u8, u16, u32, u64, usize);

impl FromSql for bool {
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
        i64::column_result(value).map(|i| i != 0)
    }
}

impl FromSql for f64 {
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
        match value {
            ValueRef::Integer(i) => Ok(i as f64),
            ValueRef::Real(r) => Ok(r),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl FromSql for f32 {
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
        f64::column_result(value).map(|r| r as f32)
    }
}

impl FromSql for String {
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
        match value {
            ValueRef::Text(t) => std::str::from_utf8(t).map(str::to_string).map_err(FromSqlError::Utf8),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl FromSql for Box<str> {
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
        String::column_result(value).map(String::into_boxed_str)
    }
}

impl FromSql for Vec<u8> {
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
        match value {
            ValueRef::Blob(b) => Ok(b.to_vec()),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl<T: FromSql> FromSql for Option<T> {
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
        match value {
            ValueRef::Null => Ok(None),
            v => T::column_result(v).map(Some),
        }
    }
}

impl FromSql for Value {
    fn column_result(value: ValueRef<'_>) -> std::result::Result<Self, FromSqlError> {
        Ok(match value {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => Value::Integer(i),
            ValueRef::Real(r) => Value::Real(r),
            ValueRef::Text(t) => Value::Text(std::str::from_utf8(t).map_err(FromSqlError::Utf8)?.to_string()),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
        })
    }
}
