//! A connection, its statement cache and its transactions.
//!
//! One thread owns a connection at a time: it is `Send` and not `Sync`, and
//! every statement, row and transaction borrows it, so none of them can
//! reach another thread. That is why nothing here takes a lock: SQLite is
//! opened without its per-connection mutex (`SQLITE_OPEN_NOMUTEX`, always)
//! and the statement cache is a plain map behind a `RefCell`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_int, c_uint, CStr, CString};
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};
use std::mem::ManuallyDrop;
use std::ops::{BitOr, Deref};
use std::path::Path;
use std::time::Duration;

use super::bind::Params;
use super::error::{Error, Result};
use super::row::Row;
use super::stmt::{CachedStatement, Statement};
use super::{ffi, raw};

/// How many statements `prepare_cached` keeps when nothing else is set
/// (rusqlite's default).
const DEFAULT_CACHE_CAPACITY: usize = 16;

/// The busy timeout every connection starts with, in milliseconds
/// (rusqlite sets the same at open).
const DEFAULT_BUSY_TIMEOUT_MS: c_int = 5000;

/// Flags of [`Connection::open_with_flags`], combined with `|`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenFlags(c_int);

impl OpenFlags {
    /// Open for reading only; the file must exist.
    pub const SQLITE_OPEN_READ_ONLY: OpenFlags = OpenFlags(ffi::SQLITE_OPEN_READONLY);
    /// Open for reading and writing.
    pub const SQLITE_OPEN_READ_WRITE: OpenFlags = OpenFlags(ffi::SQLITE_OPEN_READWRITE);
    /// Create the file when it does not exist.
    pub const SQLITE_OPEN_CREATE: OpenFlags = OpenFlags(ffi::SQLITE_OPEN_CREATE);
    /// The path may be a `file:` URI.
    pub const SQLITE_OPEN_URI: OpenFlags = OpenFlags(ffi::SQLITE_OPEN_URI);
    /// No per-connection mutex. Every connection is opened this way whether
    /// the flag is given or not: one thread owns it.
    pub const SQLITE_OPEN_NO_MUTEX: OpenFlags = OpenFlags(ffi::SQLITE_OPEN_NOMUTEX);
}

impl Default for OpenFlags {
    /// Read and write, create, URI paths, no mutex.
    fn default() -> OpenFlags {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX
    }
}

impl BitOr for OpenFlags {
    type Output = OpenFlags;

    fn bitor(self, other: OpenFlags) -> OpenFlags {
        OpenFlags(self.0 | other.0)
    }
}

/// When a transaction takes its lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionBehavior {
    /// At the first statement that needs it.
    Deferred,
    /// The write lock at `BEGIN`.
    Immediate,
    /// The write lock at `BEGIN`, and no other reader outside WAL mode.
    Exclusive,
}

/// The message for result code `rc` of a call on `db` that just failed:
/// the connection's message when it is about that code, else SQLite's
/// general text for the code.
fn message(db: &raw::Db, rc: c_int) -> String {
    if db.errcode() == rc {
        db.errmsg()
    } else {
        raw::errstr(rc)
    }
}

/// The hash of the statement cache's keys: SQL text, a few hundred bytes,
/// eight at a time. The keys are atlas's own statements, so nothing here
/// needs to resist chosen input.
#[derive(Default)]
struct SqlHasher(u64);

impl SqlHasher {
    fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

impl Hasher for SqlHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.mix(u64::from_le_bytes(*w));
        }
        if !rest.is_empty() {
            let mut last = [0_u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.mix(u64::from_le_bytes(last));
        }
        self.mix(bytes.len() as u64);
    }

    fn finish(&self) -> u64 {
        // The multiply leaves the entropy in the high bits; the map reads
        // both ends.
        self.0 ^ (self.0 >> 29)
    }
}

/// The statements `prepare_cached` keeps, by their trimmed SQL text. A
/// statement in use is not in the map: it is taken out with its key and put
/// back when dropped. Over capacity, the one put back longest ago goes.
struct Cache {
    map: HashMap<Box<str>, (raw::Stmt, u64), BuildHasherDefault<SqlHasher>>,
    capacity: usize,
    /// Counts returns; the stamp of each entry.
    tick: u64,
}

impl Cache {
    fn take(&mut self, sql: &str) -> Option<(Box<str>, raw::Stmt)> {
        self.map.remove_entry(sql).map(|(key, (stmt, _))| (key, stmt))
    }

    fn put(&mut self, key: Box<str>, stmt: raw::Stmt) {
        if self.capacity == 0 {
            stmt.finalize();
            return;
        }
        self.tick += 1;
        // The same SQL prepared twice at once comes back twice: the later
        // return replaces the earlier.
        if let Some((older, _)) = self.map.insert(key, (stmt, self.tick)) {
            older.finalize();
        }
        self.trim();
    }

    /// Finalizes the least recently returned statements down to capacity.
    fn trim(&mut self) {
        while self.map.len() > self.capacity {
            let Some(oldest) = self.map.values().map(|entry| entry.1).min() else {
                return;
            };
            if let Some((_, (stmt, _))) = self.map.extract_if(|_, entry| entry.1 == oldest).next() {
                stmt.finalize();
            }
        }
    }

    fn clear(&mut self) {
        for (_, (stmt, _)) in self.map.drain() {
            stmt.finalize();
        }
    }
}

/// An open database.
pub struct Connection {
    /// Closed by `Drop`, after every cached statement is finalized.
    db: ManuallyDrop<raw::Db>,
    cache: RefCell<Cache>,
}

// SAFETY: SQLite is compiled thread-safe (checked at every open:
// `sqlite3_threadsafe() != 0`), which lets a connection be used from any
// thread as long as one thread uses it at a time. Moving the `Connection`
// moves that single use with it: the type is not `Sync` (`raw::Db` holds a
// `NonNull`, and the cache is a `RefCell`), so no `&Connection` crosses a
// thread, and every statement, row and transaction holds a `&Connection`,
// so none of them is `Send` and none can be left behind on the old thread
// (the borrow checker does not let the connection move while they exist).
#[allow(unsafe_code)]
unsafe impl Send for Connection {}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").finish_non_exhaustive()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.cache.get_mut().clear();
        // SAFETY: `drop` runs once and `self.db` is not read after this
        // line. Every statement of this connection is gone by now: the
        // cached ones on the line above, the rest because each borrowed the
        // connection. So the close does not defer.
        #[allow(unsafe_code)]
        let db = unsafe { ManuallyDrop::take(&mut self.db) };
        db.close();
    }
}

impl Connection {
    /// Opens (creating it when missing) the database at `path`, for reading
    /// and writing.
    ///
    /// # Errors
    ///
    /// SQLite's reason; when the file cannot be opened, with the path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Connection> {
        Connection::open_with_flags(path, OpenFlags::default())
    }

    /// Opens a private database in memory.
    ///
    /// # Errors
    ///
    /// SQLite's reason (out of memory).
    pub fn open_in_memory() -> Result<Connection> {
        Connection::open_c(c":memory:", OpenFlags::default())
    }

    /// Opens the database at `path` as `flags` say.
    ///
    /// # Errors
    ///
    /// SQLite's reason; when the file cannot be opened, with the path. A
    /// path SQLite cannot take (not UTF-8 on Windows, or with a NUL in it).
    pub fn open_with_flags<P: AsRef<Path>>(path: P, flags: OpenFlags) -> Result<Connection> {
        let path = path.as_ref();
        #[cfg(unix)]
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
        #[cfg(not(unix))]
        let bytes = path.to_str().ok_or_else(|| Error::InvalidPath(path.to_owned()))?.as_bytes();
        let c_path = CString::new(bytes).map_err(Error::Nul)?;
        Connection::open_c(&c_path, flags)
    }

    fn open_c(path: &CStr, flags: OpenFlags) -> Result<Connection> {
        let misuse = |msg: String| Error::Sqlite { code: ffi::SQLITE_MISUSE, msg: msg.into() };
        if raw::threadsafe() == 0 {
            return Err(misuse("SQLite was compiled or configured for single-threaded use only".into()));
        }
        let linked = raw::libversion_number();
        if linked != ffi::SQLITE_VERSION_NUMBER {
            return Err(misuse(format!(
                "the linked SQLite is version {linked}; this binding was written for {}",
                ffi::SQLITE_VERSION_NUMBER
            )));
        }
        // Extended result codes from the first call, and never a
        // per-connection mutex.
        let bits = flags.0 | ffi::SQLITE_OPEN_EXRESCODE | ffi::SQLITE_OPEN_NOMUTEX;
        let db = match raw::Db::open(path, bits) {
            Ok(db) => db,
            Err((rc, None)) => return Err(Error::Sqlite { code: rc, msg: path.to_string_lossy().into() }),
            Err((rc, Some(db))) => {
                let mut msg = message(&db, rc);
                if rc & 0xff == ffi::SQLITE_CANTOPEN {
                    msg = format!("{msg}: {}", path.to_string_lossy());
                }
                db.close();
                return Err(Error::Sqlite { code: rc, msg: msg.into() });
            }
        };
        let rc = db.busy_timeout(DEFAULT_BUSY_TIMEOUT_MS);
        if rc != ffi::SQLITE_OK {
            let msg = message(&db, rc);
            db.close();
            return Err(Error::Sqlite { code: rc, msg: msg.into() });
        }
        Ok(Connection {
            db: ManuallyDrop::new(db),
            cache: RefCell::new(Cache { map: HashMap::default(), capacity: DEFAULT_CACHE_CAPACITY, tick: 0 }),
        })
    }

    /// The error for result code `rc` of a call that just failed.
    pub(super) fn fail(&self, rc: c_int) -> Error {
        Error::Sqlite { code: rc, msg: message(&self.db, rc).into() }
    }

    /// `Ok` for `SQLITE_OK`, else [`Connection::fail`].
    pub(super) fn check(&self, rc: c_int) -> Result<()> {
        if rc == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(self.fail(rc))
        }
    }

    /// The error of a failed prepare of `sql`. A plain SQL error (not one
    /// of the codes with a meaning of their own) that SQLite located in the
    /// text carries the text and the offset, as rusqlite's does.
    fn fail_prepare(&self, rc: c_int, sql: &str) -> Error {
        let msg = message(&self.db, rc);
        let own_meaning = matches!(rc & 0xff, 2..=15 | 17..=23 | 25 | 26);
        let offset = self.db.error_offset();
        if !own_meaning && offset >= 0 {
            return Error::SqlInput { code: rc, msg: msg.into(), sql: sql.into(), offset };
        }
        Error::Sqlite { code: rc, msg: msg.into() }
    }

    /// Compiles the first statement of `sql`: the statement (`None` when
    /// the text up to the next `;` holds none) and where the rest begins
    /// (0 when nothing is left).
    fn prepare_one(&self, sql: &str, flags: c_uint) -> Result<(Option<raw::Stmt>, usize)> {
        match self.db.prepare(sql, flags) {
            Ok((stmt, tail)) => Ok((stmt, if tail >= sql.len() { 0 } else { tail })),
            Err(rc) => Err(self.fail_prepare(rc, sql)),
        }
    }

    /// Compiles `sql`, which must hold one statement.
    fn prepare_single(&self, sql: &str, flags: c_uint) -> Result<raw::Stmt> {
        let (stmt, tail) = self.prepare_one(sql, flags)?;
        // Anything after the first statement must compile to nothing (white
        // space, comments, empty statements).
        let mut rest = if tail == 0 { None } else { sql.get(tail..) };
        while let Some(text) = rest {
            let extra = match self.prepare_one(text, 0) {
                Ok((None, tail)) => {
                    rest = if tail == 0 { None } else { text.get(tail..) };
                    continue;
                }
                Ok((Some(second), _)) => {
                    second.finalize();
                    Error::MultipleStatement
                }
                Err(e) => e,
            };
            if let Some(first) = stmt {
                first.finalize();
            }
            return Err(extra);
        }
        // No statement at all: what SQLite answers when asked to run one.
        stmt.ok_or_else(|| Error::Sqlite { code: ffi::SQLITE_MISUSE, msg: raw::errstr(ffi::SQLITE_MISUSE).into() })
    }

    /// Compiles one statement.
    ///
    /// # Errors
    ///
    /// SQLite's reason (with the text and offset for a syntax or name
    /// error); [`Error::MultipleStatement`] when `sql` holds more than one.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        self.prepare_single(sql, 0).map(|raw| Statement::new(self, raw, None))
    }

    /// Compiles one statement, or takes it from this connection's cache;
    /// it goes back to the cache when dropped. The key is the text with
    /// surrounding white space removed.
    ///
    /// # Errors
    ///
    /// As [`Connection::prepare`].
    pub fn prepare_cached(&self, sql: &str) -> Result<CachedStatement<'_>> {
        let sql = sql.trim();
        // The cache is not borrowed across the prepare.
        let hit = self.cache.borrow_mut().take(sql);
        let (key, raw) = match hit {
            Some(hit) => hit,
            None => (Box::from(sql), self.prepare_single(sql, ffi::SQLITE_PREPARE_PERSISTENT)?),
        };
        Ok(Statement::new(self, raw, Some(key)))
    }

    /// Returns a cached statement (called by its drop).
    pub(super) fn cache_put(&self, key: Box<str>, stmt: raw::Stmt) {
        stmt.clear_bindings();
        self.cache.borrow_mut().put(key, stmt);
    }

    /// Sets how many statements `prepare_cached` keeps; the least recently
    /// used above that are dropped now.
    pub fn set_prepared_statement_cache_capacity(&self, capacity: usize) {
        let mut cache = self.cache.borrow_mut();
        cache.capacity = capacity;
        cache.trim();
    }

    /// The first statement of `sql` that is one (empty statements and
    /// comments are passed over) and the text after it; `None` when none is
    /// left.
    ///
    /// # Errors
    ///
    /// SQLite's reason for the statement that does not compile.
    pub(super) fn prepare_first<'s>(&self, sql: &'s str) -> Result<Option<(Statement<'_>, &'s str)>> {
        let mut rest = sql;
        while !rest.is_empty() {
            let (stmt, tail) = self.prepare_one(rest, 0)?;
            rest = if tail == 0 { "" } else { rest.get(tail..).unwrap_or("") };
            if let Some(raw) = stmt {
                return Ok(Some((Statement::new(self, raw, None), rest)));
            }
        }
        Ok(None)
    }

    /// Runs one statement that returns no row; the number of rows changed.
    ///
    /// # Errors
    ///
    /// As [`Connection::prepare`] and [`Statement::execute`].
    pub fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize> {
        self.prepare(sql)?.execute(params)
    }

    /// Runs every statement of `sql` in turn, none with parameters; rows
    /// they return are ignored.
    ///
    /// # Errors
    ///
    /// The first statement that fails; nothing after it runs.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let mut rest = sql;
        while !rest.is_empty() {
            let (stmt, tail) = self.prepare_one(rest, 0)?;
            if let Some(stmt) = stmt {
                let rc = stmt.step();
                // The message is read before the finalize.
                let failed = (rc != ffi::SQLITE_ROW && rc != ffi::SQLITE_DONE).then(|| self.fail(rc));
                stmt.finalize();
                if let Some(e) = failed {
                    return Err(e);
                }
            }
            match rest.get(tail..) {
                Some(next) if tail != 0 => rest = next,
                _ => break,
            }
        }
        Ok(())
    }

    /// Runs a query and maps its first row.
    ///
    /// # Errors
    ///
    /// [`Error::NoRows`] when there is none; as [`Connection::prepare`];
    /// the error `f` returns.
    pub fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        self.prepare(sql)?.query_row(params, f)
    }

    /// Sets how long a call waits for another connection's lock before it
    /// fails with BUSY. Zero: it fails at once.
    ///
    /// # Errors
    ///
    /// SQLite's reason.
    ///
    /// # Panics
    ///
    /// A timeout of more than `i32::MAX` milliseconds.
    pub fn busy_timeout(&self, timeout: Duration) -> Result<()> {
        let ms = c_int::try_from(timeout.as_millis()).expect("a busy timeout fits in i32 milliseconds");
        self.check(self.db.busy_timeout(ms))
    }

    /// Sets `handler` as what a call does when another connection holds
    /// the lock it needs: SQLite calls it with the number of earlier calls
    /// for that lock (0 at the first), tries again when it returns `true`
    /// and fails the call with BUSY when it returns `false`. `None`: BUSY at
    /// once. A connection has one busy handler, so this replaces a
    /// [`busy_timeout`](Connection::busy_timeout) and that replaces this. A
    /// handler that panics gives up: the panic is caught and the call fails
    /// with BUSY.
    ///
    /// The handler runs on the thread making the call, inside it, and must
    /// not use this connection.
    ///
    /// # Errors
    ///
    /// SQLite's reason.
    pub fn busy_handler(&self, handler: Option<fn(i32) -> bool>) -> Result<()> {
        self.check(self.db.busy_handler(handler))
    }

    /// The full path of the database file; empty for a database in memory.
    /// `None` when the path is not UTF-8.
    pub fn path(&self) -> Option<&str> {
        self.db.filename().and_then(|name| name.to_str().ok())
    }

    /// The rowid of the last successful INSERT on this connection.
    pub fn last_insert_rowid(&self) -> i64 {
        self.db.last_insert_rowid()
    }

    /// Rows changed by the most recent INSERT, UPDATE or DELETE.
    pub fn changes(&self) -> u64 {
        u64::try_from(self.db.changes()).unwrap_or(0)
    }

    /// Rows changed since the connection opened.
    pub fn total_changes(&self) -> u64 {
        u64::try_from(self.db.total_changes()).unwrap_or(0)
    }

    /// Whether no transaction is open.
    pub fn is_autocommit(&self) -> bool {
        self.db.autocommit()
    }

    /// Begins a DEFERRED transaction. The `&mut` keeps two from being open
    /// at once.
    ///
    /// # Errors
    ///
    /// SQLite's reason.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        Transaction::new_unchecked(self, TransactionBehavior::Deferred)
    }

    /// Begins a transaction with the given locking.
    ///
    /// # Errors
    ///
    /// SQLite's reason (BUSY when the lock is held elsewhere).
    pub fn transaction_with_behavior(&mut self, behavior: TransactionBehavior) -> Result<Transaction<'_>> {
        Transaction::new_unchecked(self, behavior)
    }

    /// Begins a DEFERRED transaction through a shared reference; a second
    /// one while it is open fails at run time.
    ///
    /// # Errors
    ///
    /// SQLite's reason.
    pub fn unchecked_transaction(&self) -> Result<Transaction<'_>> {
        Transaction::new_unchecked(self, TransactionBehavior::Deferred)
    }
}

/// An open transaction. It rolls back when dropped unless
/// [`Transaction::commit`] ran.
#[derive(Debug)]
pub struct Transaction<'conn> {
    conn: &'conn Connection,
}

impl<'conn> Transaction<'conn> {
    /// Begins a transaction on `conn`. Nothing but SQLite checks that none
    /// is open already.
    ///
    /// # Errors
    ///
    /// SQLite's reason (BUSY when the lock is held elsewhere, or a
    /// transaction within a transaction).
    pub fn new_unchecked(conn: &'conn Connection, behavior: TransactionBehavior) -> Result<Transaction<'conn>> {
        conn.execute_batch(match behavior {
            TransactionBehavior::Deferred => "BEGIN DEFERRED",
            TransactionBehavior::Immediate => "BEGIN IMMEDIATE",
            TransactionBehavior::Exclusive => "BEGIN EXCLUSIVE",
        })?;
        Ok(Transaction { conn })
    }

    /// Commits.
    ///
    /// # Errors
    ///
    /// SQLite's reason; the transaction is then rolled back by the drop.
    pub fn commit(self) -> Result<()> {
        self.conn.execute_batch("COMMIT")
    }
}

impl Deref for Transaction<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        // After a commit (or a rollback SQLite did itself) there is nothing
        // open; a failed rollback has nowhere to be reported.
        if !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}
