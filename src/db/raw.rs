//! One thin function per SQLite call, over two handle types whose pointers
//! are valid for as long as the value exists. This is the only module that
//! calls into SQLite; everything above it is safe code, apart from the two
//! column readers below that hand out SQLite's own memory.
//!
//! Neither handle is `Send` or `Sync` here (a `NonNull` is neither) and
//! neither has a `Drop`: the wrapper above owns the order (every statement
//! finalized, then the connection closed) and decides what may move between
//! threads.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::ptr::{self, NonNull};

use super::ffi;

/// SQLite's English text for a result code (`sqlite3_errstr`).
pub fn errstr(code: c_int) -> String {
    // SAFETY: sqlite3_errstr takes any code and returns a pointer to a
    // static, NUL-terminated string (or null, handled below).
    let p = unsafe { ffi::sqlite3_errstr(code) };
    if p.is_null() {
        return String::new();
    }
    // SAFETY: non-null, static and NUL-terminated, per the line above.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// `SQLITE_VERSION_NUMBER` of the linked library.
pub fn libversion_number() -> c_int {
    // SAFETY: no arguments, no preconditions.
    unsafe { ffi::sqlite3_libversion_number() }
}

/// The threading mode the library was compiled with (`SQLITE_THREADSAFE`).
pub fn threadsafe() -> c_int {
    // SAFETY: no arguments, no preconditions.
    unsafe { ffi::sqlite3_threadsafe() }
}

/// An open connection. Invariant: the pointer came from `sqlite3_open_v2`
/// and has not been closed (closing consumes the value).
pub struct Db(NonNull<ffi::sqlite3>);

impl Db {
    /// Opens `path` with `flags`.
    ///
    /// # Errors
    ///
    /// The result code, and the handle SQLite still returned (it returns
    /// one on most failures, to carry the message), which the caller reads
    /// and then closes.
    pub fn open(path: &CStr, flags: c_int) -> Result<Db, (c_int, Option<Db>)> {
        let mut db: *mut ffi::sqlite3 = ptr::null_mut();
        // SAFETY: `path` is NUL-terminated and lives across the call, `db`
        // is a valid out pointer, and a null VFS name selects the default.
        let rc = unsafe { ffi::sqlite3_open_v2(path.as_ptr(), &mut db, flags, ptr::null()) };
        match (rc, NonNull::new(db)) {
            (ffi::SQLITE_OK, Some(db)) => Ok(Db(db)),
            // OK with no handle does not happen; treated as a failed open.
            (ffi::SQLITE_OK, None) => Err((ffi::SQLITE_CANTOPEN, None)),
            (rc, db) => Err((rc, db.map(Db))),
        }
    }

    /// Closes the connection (`sqlite3_close_v2`, which cannot fail on a
    /// valid handle; with statements still open it defers until the last is
    /// finalized).
    pub fn close(self) -> c_int {
        // SAFETY: the handle is valid (type invariant) and is consumed.
        unsafe { ffi::sqlite3_close_v2(self.0.as_ptr()) }
    }

    pub fn busy_timeout(&self, ms: c_int) -> c_int {
        // SAFETY: valid handle (type invariant).
        unsafe { ffi::sqlite3_busy_timeout(self.0.as_ptr(), ms) }
    }

    /// Sets `handler` as the connection's busy handler in place of the one
    /// it has (a timeout is one too), or with `None` leaves it with none.
    pub fn busy_handler(&self, handler: Option<fn(i32) -> bool>) -> c_int {
        /// What SQLite calls: `arg` is the handler, `count` the calls
        /// already made for this lock. A handler that panics gives up (0):
        /// a panic must not unwind into SQLite's frames.
        unsafe extern "C" fn call(arg: *mut c_void, count: c_int) -> c_int {
            // SAFETY: `arg` is the `fn(i32) -> bool` that `busy_handler`
            // passed with this function, and SQLite hands it back unchanged.
            // A function pointer and a data pointer have the same size and
            // representation on every target atlas builds for (the transmute
            // does not compile otherwise), and the value is never null: this
            // function is registered only with `Some(handler)`.
            let handler = unsafe { std::mem::transmute::<*mut c_void, fn(i32) -> bool>(arg) };
            c_int::from(std::panic::catch_unwind(|| handler(count)).unwrap_or(false))
        }
        let (callback, arg): (ffi::busy_callback, *mut c_void) = match handler {
            Some(f) => (Some(call), f as *mut c_void),
            None => (None, ptr::null_mut()),
        };
        // SAFETY: valid handle (type invariant). `call` reads `arg` only as
        // the function pointer passed here, which lives as long as the
        // program; SQLite keeps both until the next sqlite3_busy_handler or
        // sqlite3_busy_timeout call or the close, and calls `call` only from
        // inside a call on this connection, on that call's thread.
        unsafe { ffi::sqlite3_busy_handler(self.0.as_ptr(), callback, arg) }
    }

    pub fn autocommit(&self) -> bool {
        // SAFETY: valid handle (type invariant).
        unsafe { ffi::sqlite3_get_autocommit(self.0.as_ptr()) != 0 }
    }

    pub fn changes(&self) -> i64 {
        // SAFETY: valid handle (type invariant).
        unsafe { ffi::sqlite3_changes64(self.0.as_ptr()) }
    }

    pub fn total_changes(&self) -> i64 {
        // SAFETY: valid handle (type invariant).
        unsafe { ffi::sqlite3_total_changes64(self.0.as_ptr()) }
    }

    pub fn last_insert_rowid(&self) -> i64 {
        // SAFETY: valid handle (type invariant).
        unsafe { ffi::sqlite3_last_insert_rowid(self.0.as_ptr()) }
    }

    /// The extended result code of the most recent failed call.
    pub fn errcode(&self) -> c_int {
        // SAFETY: valid handle (type invariant).
        unsafe { ffi::sqlite3_extended_errcode(self.0.as_ptr()) }
    }

    /// The message of the most recent failed call, copied.
    pub fn errmsg(&self) -> String {
        // SAFETY: valid handle; sqlite3_errmsg returns a NUL-terminated
        // string owned by the connection, valid until the next call on it.
        // It is copied here, before any other call.
        let p = unsafe { ffi::sqlite3_errmsg(self.0.as_ptr()) };
        if p.is_null() {
            return String::new();
        }
        // SAFETY: non-null and NUL-terminated, per the line above.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    /// The byte offset in the SQL text of the most recent error, or -1.
    pub fn error_offset(&self) -> c_int {
        // SAFETY: valid handle (type invariant).
        unsafe { ffi::sqlite3_error_offset(self.0.as_ptr()) }
    }

    /// The file of the main database as SQLite resolved it (the full
    /// path); empty for a database in memory or a temporary one.
    pub fn filename(&self) -> Option<&CStr> {
        // SAFETY: valid handle (type invariant); the name is NUL-terminated.
        let p = unsafe { ffi::sqlite3_db_filename(self.0.as_ptr(), c"main".as_ptr()) };
        if p.is_null() {
            return None;
        }
        // SAFETY: non-null and NUL-terminated. SQLite keeps the string until
        // the database is detached, which the main database cannot be, or
        // the connection closes, which consumes the value this borrows.
        Some(unsafe { CStr::from_ptr(p) })
    }

    /// Compiles the first statement of `sql`.
    ///
    /// Returns the statement (`None` when the text holds none: empty, white
    /// space or a comment) and the offset in `sql` where the rest begins.
    ///
    /// # Errors
    ///
    /// The result code; the message is on the connection.
    pub fn prepare(&self, sql: &str, flags: c_uint) -> Result<(Option<Stmt>, usize), c_int> {
        let Ok(len) = c_int::try_from(sql.len()) else {
            return Err(ffi::SQLITE_TOOBIG);
        };
        let start = sql.as_ptr().cast::<c_char>();
        let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        // SAFETY: valid handle; `start` points at `len` readable bytes (SQLite
        // reads at most `n_byte` and needs no terminator when it is given);
        // both out pointers are valid.
        let rc = unsafe { ffi::sqlite3_prepare_v3(self.0.as_ptr(), start, len, flags, &mut stmt, &mut tail) };
        if rc != ffi::SQLITE_OK {
            return Err(rc);
        }
        // The tail points into `sql` (or one past its end). The offset is a
        // plain address difference, checked against the length; a tail that
        // is not in range reads as "nothing left".
        let off = if tail.is_null() { sql.len() } else { (tail as usize).wrapping_sub(start as usize) };
        let off = if off <= sql.len() { off } else { sql.len() };
        Ok((NonNull::new(stmt).map(Stmt), off))
    }
}

/// A prepared statement. Invariant: the pointer came from
/// `sqlite3_prepare_v3` on a connection that is still open, and has not
/// been finalized (finalizing consumes the value).
pub struct Stmt(NonNull<ffi::sqlite3_stmt>);

impl Stmt {
    /// Destroys the statement; returns the result code of its last run.
    pub fn finalize(self) -> c_int {
        // SAFETY: valid statement (type invariant), consumed here.
        unsafe { ffi::sqlite3_finalize(self.0.as_ptr()) }
    }

    pub fn step(&self) -> c_int {
        // SAFETY: valid statement (type invariant).
        unsafe { ffi::sqlite3_step(self.0.as_ptr()) }
    }

    pub fn reset(&self) -> c_int {
        // SAFETY: valid statement (type invariant).
        unsafe { ffi::sqlite3_reset(self.0.as_ptr()) }
    }

    pub fn clear_bindings(&self) -> c_int {
        // SAFETY: valid statement (type invariant).
        unsafe { ffi::sqlite3_clear_bindings(self.0.as_ptr()) }
    }

    pub fn bind_parameter_count(&self) -> c_int {
        // SAFETY: valid statement (type invariant).
        unsafe { ffi::sqlite3_bind_parameter_count(self.0.as_ptr()) }
    }

    pub fn column_count(&self) -> c_int {
        // SAFETY: valid statement (type invariant).
        unsafe { ffi::sqlite3_column_count(self.0.as_ptr()) }
    }

    /// The name of result column `i`, copied; empty when out of range.
    pub fn column_name(&self, i: c_int) -> String {
        // SAFETY: valid statement; any index is accepted (null when out of
        // range or out of memory).
        let p = unsafe { ffi::sqlite3_column_name(self.0.as_ptr(), i) };
        if p.is_null() {
            return String::new();
        }
        // SAFETY: non-null and NUL-terminated; valid until the statement is
        // finalized or re-prepared, and copied before either.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    pub fn bind_null(&self, i: c_int) -> c_int {
        // SAFETY: valid statement; SQLite range-checks the index.
        unsafe { ffi::sqlite3_bind_null(self.0.as_ptr(), i) }
    }

    pub fn bind_int64(&self, i: c_int, v: i64) -> c_int {
        // SAFETY: valid statement; SQLite range-checks the index.
        unsafe { ffi::sqlite3_bind_int64(self.0.as_ptr(), i, v) }
    }

    pub fn bind_double(&self, i: c_int, v: f64) -> c_int {
        // SAFETY: valid statement; SQLite range-checks the index.
        unsafe { ffi::sqlite3_bind_double(self.0.as_ptr(), i, v) }
    }

    /// Binds UTF-8 text; SQLite copies it before returning.
    pub fn bind_text(&self, i: c_int, text: &[u8]) -> c_int {
        // SAFETY: valid statement; the pointer is non-null and points at
        // `len` readable bytes (a dangling, aligned pointer for an empty
        // slice, of which zero bytes are read); SQLITE_TRANSIENT makes SQLite
        // copy them during the call, so nothing is kept past `text`'s borrow.
        unsafe {
            ffi::sqlite3_bind_text64(
                self.0.as_ptr(),
                i,
                text.as_ptr().cast::<c_char>(),
                text.len() as u64,
                ffi::transient(),
                ffi::SQLITE_UTF8,
            )
        }
    }

    /// Binds a blob; SQLite copies it before returning. An empty slice binds
    /// a zero-length blob, not NULL (the pointer is never null).
    pub fn bind_blob(&self, i: c_int, blob: &[u8]) -> c_int {
        // SAFETY: as `bind_text`.
        unsafe { ffi::sqlite3_bind_blob64(self.0.as_ptr(), i, blob.as_ptr().cast(), blob.len() as u64, ffi::transient()) }
    }

    /// The datatype code of column `i` of the current row.
    pub fn column_type(&self, i: c_int) -> c_int {
        // SAFETY: valid statement; an out-of-range index reads as NULL.
        unsafe { ffi::sqlite3_column_type(self.0.as_ptr(), i) }
    }

    pub fn column_int64(&self, i: c_int) -> i64 {
        // SAFETY: valid statement; an out-of-range index reads as 0.
        unsafe { ffi::sqlite3_column_int64(self.0.as_ptr(), i) }
    }

    pub fn column_double(&self, i: c_int) -> f64 {
        // SAFETY: valid statement; an out-of-range index reads as 0.0.
        unsafe { ffi::sqlite3_column_double(self.0.as_ptr(), i) }
    }

    /// The bytes of a TEXT column of the current row, in SQLite's memory.
    ///
    /// # Safety
    ///
    /// The caller has just read `column_type(i) == SQLITE_TEXT` (so no type
    /// conversion happens, which would invalidate other pointers into the
    /// row), and the returned slice must not be used after the next `step`,
    /// `reset` or `finalize` of this statement. `&self` alone does not
    /// enforce that (those take `&self` too); the row type above does.
    pub unsafe fn column_text(&self, i: c_int) -> &[u8] {
        // SAFETY: valid statement. The pointer call comes before the length
        // call, as SQLite requires. A null pointer (a zero-length value, or
        // out of memory) reads as empty.
        let p = unsafe { ffi::sqlite3_column_text(self.0.as_ptr(), i) };
        let n = unsafe { ffi::sqlite3_column_bytes(self.0.as_ptr(), i) };
        match usize::try_from(n) {
            // SAFETY: SQLite guarantees `n` readable bytes at a non-null `p`
            // until the row is left (the caller's side of the contract).
            Ok(n) if !p.is_null() && n > 0 => unsafe { std::slice::from_raw_parts(p, n) },
            _ => &[],
        }
    }

    /// The bytes of a BLOB column of the current row, in SQLite's memory.
    ///
    /// # Safety
    ///
    /// As [`Stmt::column_text`], with `SQLITE_BLOB`.
    pub unsafe fn column_blob(&self, i: c_int) -> &[u8] {
        // SAFETY: as `column_text`.
        let p = unsafe { ffi::sqlite3_column_blob(self.0.as_ptr(), i) };
        let n = unsafe { ffi::sqlite3_column_bytes(self.0.as_ptr(), i) };
        match usize::try_from(n) {
            // SAFETY: as `column_text`.
            Ok(n) if !p.is_null() && n > 0 => unsafe { std::slice::from_raw_parts(p.cast::<u8>(), n) },
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(db: &Db, sql: &str) {
        let mut rest = sql;
        while !rest.trim().is_empty() {
            let (stmt, off) = db.prepare(rest, 0).unwrap_or_else(|rc| panic!("prepare {rest:?}: {rc} {}", db.errmsg()));
            if let Some(stmt) = stmt {
                let rc = stmt.step();
                assert!(rc == ffi::SQLITE_DONE || rc == ffi::SQLITE_ROW, "step {rest:?}: {rc} {}", db.errmsg());
                assert_eq!(stmt.finalize(), ffi::SQLITE_OK);
            }
            rest = &rest[off..];
        }
    }

    /// The smoke test of the vendored build: the library is the pinned
    /// release, a database opens, and an FTS5 query (with the vocabulary
    /// module atlas uses) answers.
    #[test]
    fn opens_a_database_and_answers_an_fts5_query() {
        assert_eq!(libversion_number(), ffi::SQLITE_VERSION_NUMBER);
        assert_ne!(threadsafe(), 0, "atlas opens connections on several threads");
        let db = Db::open(
            c":memory:",
            ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_NOMUTEX | ffi::SQLITE_OPEN_EXRESCODE,
        )
        .unwrap_or_else(|(rc, _)| panic!("open: {rc}"));
        exec(
            &db,
            "CREATE VIRTUAL TABLE notes USING fts5(title, body, tokenize='unicode61');
             CREATE VIRTUAL TABLE vocab USING fts5vocab(notes, 'row');
             INSERT INTO notes(title, body) VALUES('cache', 'the eviction policy drops the oldest entry');
             INSERT INTO notes(title, body) VALUES('queue', 'nothing about that here');",
        );
        let (stmt, _) = db
            .prepare("SELECT title, bm25(notes, 2.0, 1.0), length(body) FROM notes WHERE notes MATCH ?1 ORDER BY rank", 0)
            .expect("prepare");
        let stmt = stmt.expect("one statement");
        assert_eq!(stmt.bind_parameter_count(), 1);
        assert_eq!(stmt.column_count(), 3);
        assert_eq!(stmt.bind_text(1, b"eviction"), ffi::SQLITE_OK);
        assert_eq!(stmt.step(), ffi::SQLITE_ROW);
        assert_eq!(stmt.column_type(0), ffi::SQLITE_TEXT);
        // SAFETY: the column is TEXT (checked on the line above) and the
        // slice is copied before the next step.
        let title = unsafe { stmt.column_text(0) }.to_vec();
        assert_eq!(title, b"cache");
        assert_eq!(stmt.column_type(1), ffi::SQLITE_FLOAT);
        assert!(stmt.column_double(1) < 0.0, "bm25 ranks a match below zero");
        assert_eq!(stmt.column_int64(2), 42);
        assert_eq!(stmt.step(), ffi::SQLITE_DONE);
        assert_eq!(stmt.finalize(), ffi::SQLITE_OK);

        let (stmt, _) = db.prepare("SELECT term, doc FROM vocab WHERE term = 'eviction'", 0).expect("prepare vocab");
        let stmt = stmt.expect("one statement");
        assert_eq!(stmt.step(), ffi::SQLITE_ROW);
        assert_eq!(stmt.column_int64(1), 1);
        assert_eq!(stmt.finalize(), ffi::SQLITE_OK);

        // A failed prepare leaves its message and offset on the connection.
        assert_eq!(db.prepare("SELECT * FROM missing", 0).err(), Some(ffi::SQLITE_ERROR));
        assert_eq!(db.errmsg(), "no such table: missing");
        assert_eq!(db.error_offset(), -1, "SQLite records no offset for a missing table");
        assert_eq!(db.prepare("SELECT nope FROM notes", 0).err(), Some(ffi::SQLITE_ERROR));
        assert_eq!(db.errmsg(), "no such column: nope");
        assert_eq!(db.error_offset(), 7);
        assert_eq!(db.close(), ffi::SQLITE_OK);
    }
}
