//! The SQLite C API as atlas calls it: the declarations and constants, and
//! nothing else. Written from `vendor/sqlite/sqlite3.h` (SQLite 3.53.2) and
//! SQLite's documentation.
//!
//! The build script reads this file: every `pub const` below is asserted
//! equal to the header's macro of the same name, and every `pub fn` is
//! assigned, with the C type its signature spells, from the header's own
//! declaration, by a C file compiled with the compiler that builds
//! `sqlite3.c` (see `ffi_check` in `build.rs`). A constant or a signature
//! that drifts from the header fails the build. For that reader, each
//! constant and each function stays on one line, in the forms used here.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_double, c_int, c_uchar, c_uint, c_void};
use std::marker::{PhantomData, PhantomPinned};

/// A database connection (`sqlite3`), opaque.
#[repr(C)]
pub struct sqlite3 {
    _opaque: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// A prepared statement (`sqlite3_stmt`), opaque.
#[repr(C)]
pub struct sqlite3_stmt {
    _opaque: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// `sqlite3_destructor_type`: how SQLite disposes of a bound text or blob.
pub type sqlite3_destructor_type = Option<unsafe extern "C" fn(*mut c_void)>;

/// The callback of `sqlite3_busy_handler`, which the header spells in place
/// (`int(*)(void*,int)`): the argument given with it, then the count of
/// earlier calls for this lock; nonzero to try again. The build script
/// spells it `atlas_busy_callback` in the C check.
pub type busy_callback = Option<unsafe extern "C" fn(*mut c_void, c_int) -> c_int>;

/// `SQLITE_TRANSIENT`: SQLite copies the bound bytes before the bind call
/// returns.
pub fn transient() -> sqlite3_destructor_type {
    // SAFETY: SQLite defines SQLITE_TRANSIENT as the all-ones pointer cast to
    // the destructor type and never calls it; it only compares against it.
    // `Option<extern fn>` has the layout of a nullable function pointer, and
    // all-ones is not null, so this is `Some` of a pointer that is never
    // called from Rust.
    unsafe { std::mem::transmute::<isize, sqlite3_destructor_type>(-1) }
}

/// The release this file was written for; checked against the header at
/// build time and against the linked library at the first open.
pub const SQLITE_VERSION_NUMBER: c_int = 3053002;

// Result codes (primary).
pub const SQLITE_OK: c_int = 0;
#[cfg(test)]
pub const SQLITE_ERROR: c_int = 1;
pub const SQLITE_BUSY: c_int = 5;
pub const SQLITE_CANTOPEN: c_int = 14;
pub const SQLITE_TOOBIG: c_int = 18;
pub const SQLITE_MISUSE: c_int = 21;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;

// Flags of sqlite3_open_v2.
pub const SQLITE_OPEN_READONLY: c_int = 0x00000001;
pub const SQLITE_OPEN_READWRITE: c_int = 0x00000002;
pub const SQLITE_OPEN_CREATE: c_int = 0x00000004;
pub const SQLITE_OPEN_URI: c_int = 0x00000040;
pub const SQLITE_OPEN_NOMUTEX: c_int = 0x00008000;
pub const SQLITE_OPEN_EXRESCODE: c_int = 0x02000000;

// Text encoding, flags of sqlite3_prepare_v3, fundamental datatypes.
pub const SQLITE_UTF8: c_uchar = 1;
pub const SQLITE_PREPARE_PERSISTENT: c_uint = 0x01;
pub const SQLITE_INTEGER: c_int = 1;
pub const SQLITE_FLOAT: c_int = 2;
pub const SQLITE_TEXT: c_int = 3;
pub const SQLITE_BLOB: c_int = 4;

// One line per function, whatever its length: the build script reads them.
#[rustfmt::skip]
extern "C" {
    // The library.
    pub fn sqlite3_libversion_number() -> c_int;
    pub fn sqlite3_threadsafe() -> c_int;
    pub fn sqlite3_errstr(code: c_int) -> *const c_char;

    // A connection.
    pub fn sqlite3_open_v2(filename: *const c_char, pp_db: *mut *mut sqlite3, flags: c_int, vfs: *const c_char) -> c_int;
    pub fn sqlite3_close_v2(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_busy_timeout(db: *mut sqlite3, ms: c_int) -> c_int;
    pub fn sqlite3_busy_handler(db: *mut sqlite3, callback: busy_callback, arg: *mut c_void) -> c_int;
    pub fn sqlite3_get_autocommit(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_changes64(db: *mut sqlite3) -> i64;
    pub fn sqlite3_total_changes64(db: *mut sqlite3) -> i64;
    pub fn sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64;
    pub fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    pub fn sqlite3_extended_errcode(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_error_offset(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_db_filename(db: *mut sqlite3, db_name: *const c_char) -> *const c_char;

    // A statement.
    pub fn sqlite3_prepare_v3(db: *mut sqlite3, sql: *const c_char, n_byte: c_int, prep_flags: c_uint, pp_stmt: *mut *mut sqlite3_stmt, tail: *mut *const c_char) -> c_int;
    pub fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_clear_bindings(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_bind_parameter_count(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_column_name(stmt: *mut sqlite3_stmt, n: c_int) -> *const c_char;

    // Parameters.
    pub fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, i: c_int) -> c_int;
    pub fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, i: c_int, value: i64) -> c_int;
    pub fn sqlite3_bind_double(stmt: *mut sqlite3_stmt, i: c_int, value: c_double) -> c_int;
    pub fn sqlite3_bind_text64(stmt: *mut sqlite3_stmt, i: c_int, text: *const c_char, n: u64, destructor: sqlite3_destructor_type, encoding: c_uchar) -> c_int;
    pub fn sqlite3_bind_blob64(stmt: *mut sqlite3_stmt, i: c_int, blob: *const c_void, n: u64, destructor: sqlite3_destructor_type) -> c_int;

    // Columns of the current row.
    pub fn sqlite3_column_type(stmt: *mut sqlite3_stmt, i_col: c_int) -> c_int;
    pub fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, i_col: c_int) -> i64;
    pub fn sqlite3_column_double(stmt: *mut sqlite3_stmt, i_col: c_int) -> c_double;
    pub fn sqlite3_column_text(stmt: *mut sqlite3_stmt, i_col: c_int) -> *const c_uchar;
    pub fn sqlite3_column_blob(stmt: *mut sqlite3_stmt, i_col: c_int) -> *const c_void;
    pub fn sqlite3_column_bytes(stmt: *mut sqlite3_stmt, i_col: c_int) -> c_int;
}
