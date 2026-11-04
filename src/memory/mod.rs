//! The memory store: records atlas is authoritative for (memories, rules,
//! handoffs, lane journals), kept apart from the derived index.
//!
//! Durability rules, each enforced below the code that writes:
//!
//! * One SQLite file of its own (`memory.store`), `synchronous=FULL`. The
//!   index database can be deleted and rebuilt; this one never is.
//! * `versions` is append-only and `records` rows are never deleted: SQLite
//!   triggers refuse the statements, whatever the caller.
//! * Every version carries the SHA-256 of its text and a per-record hash
//!   chain (`chain = sha256(previous chain || sha)`), so an edit to a stored
//!   version is detectable.
//! * Every committed version is appended to `<store>.jsonl` beside the file:
//!   a plain-text copy that [`check`] replays.
//! * A reversal is a supersede: the old record stays, marked, and stays
//!   recallable.
//!
//! See `docs/design/atlas-c.md`.

use std::fs::OpenOptions;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Error, Result};
use crate::json::{self, Value};

pub mod archive;
pub mod check;
pub mod export;
pub mod import;
pub mod recall;
pub mod select;
pub mod settings;
pub mod sync;
pub mod tree;
pub mod vectors;

/// The schema. `versions` and `records` are guarded by triggers.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS records(
    rid INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    sub TEXT NOT NULL DEFAULT '',
    scope TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,
    head INTEGER NOT NULL,
    created TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS records_scope ON records(scope, kind, rid);
CREATE TABLE IF NOT EXISTS versions(
    vid INTEGER PRIMARY KEY,
    rid INTEGER NOT NULL REFERENCES records(rid),
    seq INTEGER NOT NULL,
    time TEXT NOT NULL,
    title TEXT NOT NULL,
    meta TEXT NOT NULL,
    body TEXT NOT NULL,
    sha TEXT NOT NULL,
    chain TEXT NOT NULL,
    reason TEXT NOT NULL,
    author TEXT NOT NULL,
    client TEXT NOT NULL,
    pid INTEGER NOT NULL,
    src TEXT, src_path TEXT, src_line INTEGER, src_sha TEXT,
    UNIQUE(rid, seq)
);
CREATE TABLE IF NOT EXISTS supersedes(
    new_rid INTEGER NOT NULL, old_rid INTEGER NOT NULL, time TEXT NOT NULL, reason TEXT NOT NULL,
    PRIMARY KEY(new_rid, old_rid)
);
CREATE INDEX IF NOT EXISTS supersedes_old ON supersedes(old_rid, time);
CREATE TABLE IF NOT EXISTS refs(rid INTEGER NOT NULL, kind TEXT NOT NULL, target TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS refs_target ON refs(kind, target);
CREATE TABLE IF NOT EXISTS placement(export TEXT NOT NULL, ord INTEGER NOT NULL, rid INTEGER NOT NULL, PRIMARY KEY(export, ord));
CREATE INDEX IF NOT EXISTS placement_rid ON placement(rid);
CREATE TABLE IF NOT EXISTS export_files(export TEXT NOT NULL, path TEXT NOT NULL, sha TEXT NOT NULL, time TEXT NOT NULL, PRIMARY KEY(export, path));
CREATE TABLE IF NOT EXISTS mem_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS retirements(rid INTEGER NOT NULL, time TEXT NOT NULL, reason TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS retirements_rid ON retirements(rid, time);
CREATE VIRTUAL TABLE IF NOT EXISTS records_fts USING fts5(title, body, tokenize='unicode61');
CREATE VIRTUAL TABLE IF NOT EXISTS records_vocab USING fts5vocab(records_fts, 'row');
CREATE TRIGGER IF NOT EXISTS versions_no_update BEFORE UPDATE ON versions BEGIN SELECT RAISE(ABORT, 'versions are append-only'); END;
CREATE TRIGGER IF NOT EXISTS versions_no_delete BEFORE DELETE ON versions BEGIN SELECT RAISE(ABORT, 'versions are append-only'); END;
CREATE TRIGGER IF NOT EXISTS records_no_delete BEFORE DELETE ON records BEGIN SELECT RAISE(ABORT, 'records are never deleted'); END;
CREATE TRIGGER IF NOT EXISTS records_head_forward BEFORE UPDATE OF head ON records WHEN NEW.head < OLD.head
    BEGIN SELECT RAISE(ABORT, 'a record head only moves forward'); END;
CREATE TRIGGER IF NOT EXISTS records_key_fixed BEFORE UPDATE OF key, kind, rid ON records
    BEGIN SELECT RAISE(ABORT, 'a record key and kind never change'); END;
";

/// The schema's version (`PRAGMA user_version`); a change re-runs [`SCHEMA`],
/// whose statements are all `IF NOT EXISTS`.
pub const SCHEMA_VERSION: i64 = 2;

/// The most words one search sends to FTS5 (the rarest ones).
pub const SEARCH_TERMS: usize = 12;

/// Placement ords are spaced so a record can be put between two others.
pub const ORD_STEP: i64 = 1 << 16;

/// Record kinds the tools accept.
pub const KINDS: &[&str] = &["memory", "rule", "section", "block", "handoff", "journal", "decision"];

/// Journal entry kinds (`records.sub` for kind `journal`).
pub const ENTRIES: &[&str] = &["brief", "decision", "measurement", "commit", "question", "answered", "next", "note", "cap", "checkpoint"];

/// Where an imported text came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// The source's name (an export name, or a tool).
    pub src: String,
    /// The file, relative to the source root.
    pub path: String,
    /// The 1-based line the text starts on.
    pub line: i64,
    /// SHA-256 of the whole source file at import.
    pub sha: String,
}

/// The text of a new version and who wrote it.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    /// One line: what the record is.
    pub title: String,
    /// Metadata text (a memory's frontmatter, verbatim; `key: value` lines
    /// otherwise).
    pub meta: String,
    /// The text, exact.
    pub body: String,
    /// Why this version is written.
    pub reason: String,
    /// Who asked for it.
    pub author: String,
    /// The MCP client or command that wrote it.
    pub client: String,
    /// Where the text came from, for imports.
    pub provenance: Option<Provenance>,
    /// The version's time when the source states one (UTC, as [`now`]
    /// writes it); `None` is the time of the write.
    pub time: Option<String>,
}

/// A record's current version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// Row id.
    pub rid: i64,
    /// The stable key.
    pub key: String,
    /// The kind (see [`KINDS`]).
    pub kind: String,
    /// The sub-kind: a journal entry kind or a memory type.
    pub sub: String,
    /// The lane, or the export a block belongs to.
    pub scope: String,
    /// `active`, `superseded`, or `retired` (its source item was removed).
    pub status: String,
    /// Version number, from 1.
    pub seq: i64,
    /// When this version was written (UTC).
    pub time: String,
    /// One line.
    pub title: String,
    /// Metadata text.
    pub meta: String,
    /// The text.
    pub body: String,
    /// SHA-256 of this version's text.
    pub sha: String,
    /// The hash chain at this version.
    pub chain: String,
    /// The key of the record that superseded this one, if any.
    pub superseded_by: Option<String>,
    /// When it was superseded.
    pub superseded_time: Option<String>,
    /// Where the head version came from in its source, `path:line`, for a
    /// record imported or synced from a file.
    pub anchor: Option<String>,
    /// The source or export that `anchor` is in (`intent`), when known.
    pub anchor_src: Option<String>,
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The record's key.
    pub key: String,
    /// The version written.
    pub seq: i64,
    /// Its SHA-256.
    pub sha: String,
    /// False when the text equals the head and nothing was written.
    pub changed: bool,
}

/// Where a new record goes in an export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// At the end.
    End,
    /// At the start.
    Start,
    /// Right after the record with this key.
    After(String),
}

/// The SHA-256 of a version's text: title, meta and body framed by lengths,
/// so no two different triples hash alike.
pub fn version_sha(title: &str, meta: &str, body: &str) -> String {
    let mut buf = Vec::with_capacity(title.len() + meta.len() + body.len() + 48);
    for part in [title, meta, body] {
        buf.extend_from_slice(&(part.len() as u64).to_le_bytes());
        buf.extend_from_slice(part.as_bytes());
    }
    crate::sha256::hex(&buf)
}

/// The next link of a record's hash chain.
pub fn chain_next(prev: &str, sha: &str) -> String {
    let mut s = String::with_capacity(prev.len() + sha.len());
    s.push_str(prev);
    s.push_str(sha);
    crate::sha256::hex(s.as_bytes())
}

/// The current UTC time with milliseconds.
pub fn now() -> String {
    crate::discussion::utc_now_ms()
}

/// The memory store.
pub struct MemoryStore {
    conn: Connection,
    path: PathBuf,
    jsonl: PathBuf,
    /// Document frequency per FTS term, with the `data_version` and
    /// `total_changes` it was read at (any commit, from any connection,
    /// changes one of them).
    vocab: std::cell::RefCell<Option<((i64, u64), std::collections::HashMap<String, i64>)>>,
}

/// The shortest `expected_sha` prefix a revision accepts.
pub const MIN_SHA_PREFIX: usize = 12;

/// The recorded end of the `.jsonl` copy's committed lines.
fn jsonl_end(c: &Connection) -> Result<Option<u64>> {
    let v: Option<String> = c.query_row("SELECT value FROM mem_meta WHERE key = 'jsonl_end'", [], |r| r.get(0)).optional()?;
    Ok(v.and_then(|s| s.parse().ok()))
}

/// Whether one `.jsonl` line could be a committed one: the store holds what
/// it says (a version's key, seq and sha; a supersede's row; a placement; a
/// refs row; a status). None for a line that says nothing checkable (a
/// respace, which only comes with a place).
fn line_in_store(c: &Connection, line: &str) -> rusqlite::Result<Option<bool>> {
    let Ok(v) = json::parse(line) else { return Ok(Some(false)) };
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let rid: Option<i64> = c.prepare_cached("SELECT rid FROM records WHERE key = ?1")?.query_row([s("key")], |r| r.get(0)).optional()?;
    let Some(rid) = rid else { return Ok(Some(false)) };
    let one = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<bool> { Ok(c.prepare_cached(sql)?.query_row(p, |_| Ok(())).optional()?.is_some()) };
    Ok(Some(match v.get("op").and_then(Value::as_str) {
        Some("create" | "version") => {
            let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(-1);
            one("SELECT 1 FROM versions WHERE rid = ?1 AND seq = ?2 AND sha = ?3", &[&rid, &seq, &s("sha")])?
        }
        Some("retire") => one("SELECT 1 FROM records WHERE rid = ?1 AND status = 'retired'", &[&rid])?,
        Some("reactivate") => one("SELECT 1 FROM records WHERE rid = ?1 AND status = 'active'", &[&rid])?,
        Some("supersede") => one("SELECT 1 FROM supersedes WHERE new_rid = ?1 AND time = ?2", &[&rid, &s("time")])?,
        Some("place") => {
            let ord = v.get("ord").and_then(Value::as_i64).unwrap_or(i64::MIN);
            one("SELECT 1 FROM placement WHERE export = ?1 AND ord = ?2 AND rid = ?3", &[&s("export"), &ord, &rid])?
        }
        Some("unplace") => !one("SELECT 1 FROM placement WHERE export = ?1 AND rid = ?2", &[&s("export"), &rid])?,
        Some("refs") => {
            let mut all = true;
            for p in v.get("refs").and_then(Value::as_array).into_iter().flatten() {
                let (k, t) = (p.get(0).and_then(Value::as_str).unwrap_or(""), p.get(1).and_then(Value::as_str).unwrap_or(""));
                all &= one("SELECT 1 FROM refs WHERE rid = ?1 AND kind = ?2 AND target = ?3", &[&rid, &k, &t])?;
            }
            all
        }
        Some("respace") => return Ok(None),
        _ => false,
    }))
}

/// What one `.jsonl` line past the recorded end is, by the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineState {
    /// The store holds what it says: a committed line.
    Held,
    /// The store has never held it: its record is not in the store, or it
    /// is a version the store does not have. Only a batch that appended and
    /// never committed leaves such a line past the end.
    Foreign,
    /// Neither: a line that says nothing checkable (a respace), one whose
    /// state a later line may have changed (a retire, a refs row), or one
    /// that does not parse.
    Unsure,
}

fn line_state(c: &Connection, line: &str) -> rusqlite::Result<LineState> {
    let Ok(v) = json::parse(line) else {
        // A torn append that a whole line from another writer was appended
        // onto: that line is judged on its own, so it is never cut.
        return match line.rfind("{\"op\":") {
            Some(p) if p > 0 && line_state(c, &line[p..])? == LineState::Held => Ok(LineState::Held),
            _ => Ok(LineState::Unsure),
        };
    };
    let key = v.get("key").and_then(Value::as_str).unwrap_or("");
    let rid: Option<i64> = c.prepare_cached("SELECT rid FROM records WHERE key = ?1")?.query_row([key], |r| r.get(0)).optional()?;
    if v.get("op").and_then(Value::as_str) == Some("respace") {
        return Ok(LineState::Unsure);
    }
    let Some(rid) = rid else { return Ok(LineState::Foreign) };
    let version = matches!(v.get("op").and_then(Value::as_str), Some("create" | "version"));
    if version {
        let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(-1);
        let sha = v.get("sha").and_then(Value::as_str).unwrap_or("");
        let held = c.prepare_cached("SELECT 1 FROM versions WHERE rid = ?1 AND seq = ?2 AND sha = ?3")?.query_row(params![rid, seq, sha], |_| Ok(())).optional()?.is_some();
        return Ok(if held { LineState::Held } else { LineState::Foreign });
    }
    Ok(if line_in_store(c, line)? == Some(true) { LineState::Held } else { LineState::Unsure })
}

/// The `.jsonl` copy past its recorded end, judged line by line against
/// the store (see [`MemoryStore::copy_tail`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyTail {
    /// Where the committed lines end: just past the last line the store
    /// holds, or the recorded end when it holds none past it.
    pub end: u64,
    /// Whole lines before `end` that the store has never held (each a byte
    /// range, newline included): a batch that appended and never committed,
    /// followed by lines a writer that does not record the end (an atlas
    /// older than 0.2) committed after it.
    pub uncommitted: Vec<std::ops::Range<u64>>,
}

/// Judges the lines of `bytes` (the copy as read) past the recorded `end`.
///
/// Uncommitted lines only ever come from a batch killed between its append
/// and its commit, and every 0.2 batch first deals with what such a batch
/// left, under the write lock, so they are the copy's last lines unless an
/// older writer (which appends after its commit and records no end)
/// appended after them. So: everything past the last line the store holds
/// is uncommitted (a whole batch, or a torn last line), and is cut; a line
/// before it is kept, since a committed line is never cut, and one the
/// store has never held is reported in `uncommitted` instead.
///
/// # Errors
///
/// [`Error::Db`].
fn copy_tail(c: &Connection, bytes: &[u8], end: u64) -> Result<CopyTail> {
    let len = bytes.len() as u64;
    if len <= end {
        return Ok(CopyTail { end: len, uncommitted: Vec::new() });
    }
    let mut at = end;
    let mut keep = end;
    let mut foreign: Vec<std::ops::Range<u64>> = Vec::new();
    // The key of the last line the store has never held: the lines after it
    // for the same record (its refs) came in the same batch, though the
    // store may hold a row like theirs from an earlier one.
    let mut foreign_key: Option<String> = None;
    // Only whole lines: a last line without its newline is a torn append,
    // never committed.
    for line in bytes[end as usize..].split_inclusive(|b| *b == b'\n').filter(|l| l.ends_with(b"\n")) {
        let r = at..at + line.len() as u64;
        at = r.end;
        let text = std::str::from_utf8(line).ok().map(str::trim_end);
        let parsed = text.and_then(|t| json::parse(t).ok());
        let key = parsed.as_ref().and_then(|v| v.get("key")).and_then(Value::as_str).map(str::to_string);
        let version = parsed.as_ref().and_then(|v| v.get("op")).and_then(Value::as_str).is_some_and(|o| o == "create" || o == "version");
        let mut state = match text {
            Some(t) => line_state(c, t)?,
            None => LineState::Unsure,
        };
        if state != LineState::Foreign && !version && key.is_some() && key == foreign_key {
            state = LineState::Foreign;
        }
        foreign_key = if state == LineState::Foreign { key } else { None };
        match state {
            LineState::Held => keep = r.end,
            LineState::Foreign => foreign.push(r),
            LineState::Unsure => {}
        }
    }
    foreign.retain(|r| r.end <= keep);
    Ok(CopyTail { end: keep, uncommitted: foreign })
}

/// The line that takes an uncommitted line's place in the copy, at the same
/// length: a no-op every reader skips.
fn void_line(len: usize) -> Vec<u8> {
    const VOID: &[u8] = b"{\"op\":\"void\"}";
    let mut v = if len > VOID.len() { VOID.to_vec() } else { Vec::new() };
    v.resize(len.saturating_sub(1), b' ');
    v.push(b'\n');
    v
}

/// Records where the copy's committed lines end when nothing has yet: a
/// fresh store, or one an atlas older than 0.2 wrote (whose copy holds only
/// committed lines, appended after each commit). Without it a fresh store's
/// first batch killed between its append and its commit left a line that
/// read as an older writer's committed one.
fn record_jsonl_end(c: &Connection, jsonl: &Path) -> Result<()> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    let made = (|| -> Result<()> {
        if jsonl_end(c)?.is_none() {
            let len = std::fs::metadata(jsonl).map_or(0, |m| m.len());
            c.prepare_cached("INSERT INTO mem_meta(key, value) VALUES('jsonl_end', ?1)")?.execute([len.to_string()])?;
        }
        Ok(())
    })();
    match made {
        Ok(()) => Ok(c.execute_batch("COMMIT")?),
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// The `.jsonl` copy beside a store file.
pub fn jsonl_path(store: &Path) -> PathBuf {
    let mut p = store.as_os_str().to_owned();
    p.push(".jsonl");
    PathBuf::from(p)
}

const HEAD_SQL: &str = "SELECT r.rid, r.key, r.kind, r.sub, r.scope, r.status, v.seq, v.time, v.title, v.meta, v.body, v.sha, v.chain,
    (SELECT n.key FROM supersedes s JOIN records n ON n.rid = s.new_rid WHERE s.old_rid = r.rid ORDER BY s.time DESC LIMIT 1),
    (SELECT s.time FROM supersedes s WHERE s.old_rid = r.rid ORDER BY s.time DESC LIMIT 1),
    v.src_path, v.src_line, v.src
    FROM records r JOIN versions v ON v.vid = r.head";

fn head_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Head> {
    Ok(Head {
        rid: row.get(0)?,
        key: row.get(1)?,
        kind: row.get(2)?,
        sub: row.get(3)?,
        scope: row.get(4)?,
        status: row.get(5)?,
        seq: row.get(6)?,
        time: row.get(7)?,
        title: row.get(8)?,
        meta: row.get(9)?,
        body: row.get(10)?,
        sha: row.get(11)?,
        chain: row.get(12)?,
        superseded_by: row.get(13)?,
        superseded_time: row.get(14)?,
        anchor: match (row.get::<_, Option<String>>(15)?, row.get::<_, Option<i64>>(16)?) {
            (Some(p), Some(l)) if !p.is_empty() && l > 0 => Some(format!("{p}:{l}")),
            _ => None,
        },
        anchor_src: row.get::<_, Option<String>>(17)?.filter(|s| !s.is_empty()),
    })
}

/// One stored version, for `memory_log` and the integrity check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow {
    /// Version row id.
    pub vid: i64,
    /// The record's key.
    pub key: String,
    /// The record's kind.
    pub kind: String,
    /// The record's scope.
    pub scope: String,
    /// Version number.
    pub seq: i64,
    /// When written.
    pub time: String,
    /// Title.
    pub title: String,
    /// Meta.
    pub meta: String,
    /// Body.
    pub body: String,
    /// Stored sha.
    pub sha: String,
    /// Stored chain.
    pub chain: String,
    /// Why.
    pub reason: String,
    /// Who asked.
    pub author: String,
}

/// A query's filters for [`MemoryStore::log`].
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    /// Only this key.
    pub key: Option<String>,
    /// Only this kind.
    pub kind: Option<String>,
    /// Only this scope.
    pub scope: Option<String>,
    /// Only versions at or after this time (string compare on UTC).
    pub since: Option<String>,
    /// Only versions by this author.
    pub author: Option<String>,
    /// At most this many rows.
    pub limit: usize,
}

/// One lane's state for `journal_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneStatus {
    /// The lane.
    pub lane: String,
    /// Journal and handoff records.
    pub entries: i64,
    /// Time of the newest one.
    pub last: String,
    /// The newest commit entry's title.
    pub last_commit: String,
    /// Questions not yet answered.
    pub open_questions: i64,
    /// Handoff records.
    pub handoffs: i64,
}

impl MemoryStore {
    /// Opens (creating if needed) a store file.
    ///
    /// # Arguments
    ///
    /// * `path` - the SQLite file. Its folder is created if missing.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] or [`Error::Io`] if it cannot be opened or its schema made.
    pub fn open(path: &Path) -> Result<MemoryStore> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        // The schema is made once; a store at the current version opens
        // with two pragma reads (start to first answer is an aim of 5 ms).
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            // Two sessions can open a fresh store at once. Switching to WAL
            // answers SQLITE_BUSY at once (the busy handler is not consulted
            // for a journal mode change), so it is retried until the busy
            // timeout; the schema is then made under the write lock, and
            // only by the first session to take it.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                match conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0)) {
                    Ok(_) => break,
                    Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DatabaseBusy && std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let made = (|| -> rusqlite::Result<()> {
                let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
                if v != SCHEMA_VERSION {
                    conn.execute_batch(SCHEMA)?;
                    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
                }
                conn.execute_batch("COMMIT")
            })();
            if let Err(e) = made {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
        }
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        let jsonl = jsonl_path(path);
        if jsonl_end(&conn)?.is_none() {
            record_jsonl_end(&conn, &jsonl)?;
        }
        Ok(MemoryStore { conn, path: path.to_path_buf(), jsonl, vocab: Default::default() })
    }

    /// An in-memory store with no `.jsonl` copy (round-trip checks, tests).
    ///
    /// # Errors
    ///
    /// [`Error::Db`] if the schema cannot be made.
    pub fn in_memory() -> Result<MemoryStore> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(MemoryStore { conn, path: PathBuf::new(), jsonl: PathBuf::new(), vocab: Default::default() })
    }

    /// The connection (read paths in other modules).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The store file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The `.jsonl` copy.
    pub fn jsonl(&self) -> &Path {
        &self.jsonl
    }

    /// Starts a write batch: one transaction (it takes the write lock), and
    /// the batch's `.jsonl` lines appended under that lock on commit.
    ///
    /// The store records where the copy's committed lines end
    /// (`jsonl_end`, written in each batch's transaction, and when the store
    /// is opened if nothing has yet). Lines past it are judged against the
    /// store here, under the lock, before this batch appends (see
    /// [`copy_tail`]): those after the last line the store holds were
    /// appended by a batch that never committed (a crash between the append
    /// and the commit) and are cut off; one the store has never held that
    /// lines committed by an older writer follow is overwritten in place,
    /// at the same length, with a `void` line, so no committed line moves.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] if the transaction cannot start; [`Error::Io`] if the
    /// copy cannot be cut back.
    pub fn writer(&mut self) -> Result<Writer<'_>> {
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !self.jsonl.as_os_str().is_empty() {
            let end = jsonl_end(&tx)?;
            if let (Some(end), Ok(m)) = (end, std::fs::metadata(&self.jsonl)) {
                if m.len() > end {
                    let bytes = std::fs::read(&self.jsonl)?;
                    let tail = copy_tail(&tx, &bytes, end)?;
                    if tail.end < bytes.len() as u64 || !tail.uncommitted.is_empty() {
                        let mut f = OpenOptions::new().write(true).open(&self.jsonl)?;
                        for r in &tail.uncommitted {
                            f.seek(std::io::SeekFrom::Start(r.start))?;
                            f.write_all(&void_line((r.end - r.start) as usize))?;
                        }
                        if tail.end < bytes.len() as u64 {
                            f.set_len(tail.end)?;
                        }
                        f.sync_data()?;
                    }
                }
            }
        }
        Ok(Writer { tx, log: String::new(), jsonl: self.jsonl.clone(), pid: std::process::id() as i64 })
    }

    /// Where the `.jsonl` copy's committed lines end, in bytes (`None` for a
    /// store written before this was recorded).
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn jsonl_end(&self) -> Result<Option<u64>> {
        jsonl_end(&self.conn)
    }

    /// The committed lines of `bytes` (the `.jsonl` copy as read): where
    /// they end, and the lines before that end the store has never held
    /// (see [`copy_tail`]); the whole copy for a store whose end was never
    /// recorded.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn copy_tail(&self, bytes: &[u8]) -> Result<CopyTail> {
        match jsonl_end(&self.conn)? {
            Some(end) => copy_tail(&self.conn, bytes, end),
            None => Ok(CopyTail { end: bytes.len() as u64, uncommitted: Vec::new() }),
        }
    }

    /// Whether atlas has written export `export`'s file `rel` (relative to
    /// the export folder) at least once.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn exported(&self, export: &str, rel: &Path) -> Result<bool> {
        let rel = rel.to_string_lossy().replace('\\', "/");
        Ok(self.conn.prepare_cached("SELECT 1 FROM export_files WHERE export = ?1 AND path = ?2")?.query_row([export, rel.as_str()], |_| Ok(())).optional()?.is_some())
    }

    /// A record's head.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn head(&self, key: &str) -> Result<Option<Head>> {
        Ok(self.conn.prepare_cached(&format!("{HEAD_SQL} WHERE r.key = ?1"))?.query_row([key], head_row).optional()?)
    }

    /// Heads by row ids, in the order given; missing ids are skipped.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn heads_by_rid(&self, rids: &[i64]) -> Result<Vec<Head>> {
        let mut st = self.conn.prepare_cached(&format!("{HEAD_SQL} WHERE r.rid = ?1"))?;
        let mut out = Vec::with_capacity(rids.len());
        for r in rids {
            if let Some(h) = st.query_row([r], head_row).optional()? {
                out.push(h);
            }
        }
        Ok(out)
    }

    /// Every head of a kind (and scope, when given), oldest record first.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn heads(&self, kind: Option<&str>, scope: Option<&str>) -> Result<Vec<Head>> {
        let sql = format!("{HEAD_SQL} WHERE (?1 IS NULL OR r.kind = ?1) AND (?2 IS NULL OR r.scope = ?2) ORDER BY r.rid");
        let mut st = self.conn.prepare_cached(&sql)?;
        let rows = st.query_map(params![kind, scope], head_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every version of a record, oldest first.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn history(&self, key: &str) -> Result<Vec<VersionRow>> {
        self.log(&LogFilter { key: Some(key.to_string()), limit: usize::MAX, ..LogFilter::default() }).map(|mut v| {
            v.reverse();
            v
        })
    }

    /// Versions newest first, filtered.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn log(&self, f: &LogFilter) -> Result<Vec<VersionRow>> {
        let sql = "SELECT v.vid, r.key, r.kind, r.scope, v.seq, v.time, v.title, v.meta, v.body, v.sha, v.chain, v.reason, v.author
            FROM versions v JOIN records r ON r.rid = v.rid
            WHERE (?1 IS NULL OR r.key = ?1) AND (?2 IS NULL OR r.kind = ?2) AND (?3 IS NULL OR r.scope = ?3)
              AND (?4 IS NULL OR v.time >= ?4) AND (?5 IS NULL OR v.author = ?5)
            ORDER BY v.vid DESC LIMIT ?6";
        let limit = i64::try_from(f.limit).unwrap_or(i64::MAX);
        let mut st = self.conn.prepare_cached(sql)?;
        let rows = st
            .query_map(params![f.key, f.kind, f.scope, f.since, f.author, limit], |r| {
                Ok(VersionRow {
                    vid: r.get(0)?,
                    key: r.get(1)?,
                    kind: r.get(2)?,
                    scope: r.get(3)?,
                    seq: r.get(4)?,
                    time: r.get(5)?,
                    title: r.get(6)?,
                    meta: r.get(7)?,
                    body: r.get(8)?,
                    sha: r.get(9)?,
                    chain: r.get(10)?,
                    reason: r.get(11)?,
                    author: r.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Full-text search over active heads, best first, with the bm25 score
    /// negated (higher is better) and the number of distinct query terms the
    /// record contains.
    ///
    /// # Arguments
    ///
    /// * `terms` - query words (already split and cleaned; ORed).
    /// * `kind`, `scope` - optional filters.
    /// * `limit` - the most rows.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn search(&self, terms: &[String], kind: Option<&str>, scope: Option<&str>, limit: usize) -> Result<Vec<(Head, f64, usize)>> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // A long prompt is searched by its rarest words only: they carry the
        // signal, and every common word adds a long posting list to scan.
        let picked = self.rarest(terms, SEARCH_TERMS)?;
        if picked.is_empty() {
            return Ok(Vec::new());
        }
        let q = picked.iter().map(|t| format!("\"{}\"", t.replace('"', ""))).collect::<Vec<_>>().join(" OR ");
        // Rank in FTS5 alone, then fetch the few heads: joining inside the
        // ranked query measured 3.4x slower.
        let sql = "SELECT rowid, -bm25(records_fts, 2.0, 1.0) FROM records_fts WHERE records_fts MATCH ?1 ORDER BY bm25(records_fts, 2.0, 1.0) LIMIT ?2";
        let ranked: Vec<(i64, f64)> = self.conn.prepare_cached(sql)?.query_map(params![q, (limit * 2 + 8) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut st = self.conn.prepare_cached(
            "SELECT r.rid, r.key, r.kind, r.sub, r.scope, r.status, v.seq, v.time, v.title, v.meta, v.body, v.sha, v.chain, NULL, NULL, v.src_path, v.src_line, v.src
             FROM records r JOIN versions v ON v.vid = r.head WHERE r.rid = ?1",
        )?;
        let mut rows: Vec<(Head, f64)> = Vec::with_capacity(limit);
        for (rid, score) in ranked {
            let Some(h) = st.query_row([rid], head_row).optional()? else { continue };
            if h.status != "active" || h.kind == "block" || kind.is_some_and(|k| k != h.kind) || scope.is_some_and(|s| s != h.scope) {
                continue;
            }
            rows.push((h, score));
            if rows.len() == limit {
                break;
            }
        }
        let mut out = Vec::with_capacity(rows.len());
        for (h, score) in rows {
            let hay = format!("{}
{}", h.title, h.body).to_lowercase();
            let matched = terms.iter().filter(|t| contains_word(&hay, t)).count();
            out.push((h, score, matched));
        }
        Ok(out)
    }

    /// Up to `n` of `terms` that occur in the most records' fewest: words in
    /// no record are dropped, the rest ordered by document frequency.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn rarest(&self, terms: &[String], n: usize) -> Result<Vec<String>> {
        let stamp = self.stamp()?;
        let mut cache = self.vocab.borrow_mut();
        if cache.as_ref().is_none_or(|(s, _)| *s != stamp) {
            // fts5vocab has no index on `term`: one pass into a map beats a
            // scan per word (measured 173 us per lookup on 12k terms).
            let mut st = self.conn.prepare_cached("SELECT term, doc FROM records_vocab")?;
            let map = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<rusqlite::Result<_>>()?;
            *cache = Some((stamp, map));
        }
        let map = &cache.as_ref().map(|(_, m)| m).ok_or_else(|| Error::Db("vocabulary".into()))?;
        let mut df: Vec<(i64, &String)> = terms.iter().filter_map(|t| map.get(t).map(|d| (*d, t))).collect();
        df.sort();
        Ok(df.into_iter().take(n).map(|(_, t)| t.clone()).collect())
    }

    /// The store's state as `data_version` and this connection's
    /// `total_changes`: any commit, from any connection, changes one of them.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn stamp(&self) -> Result<(i64, u64)> {
        let version: i64 = self.conn.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        Ok((version, self.conn.total_changes()))
    }

    /// The journal and handoff records of a lane, oldest first.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn lane(&self, lane: &str) -> Result<Vec<Head>> {
        let sql = format!("{HEAD_SQL} WHERE r.scope = ?1 AND r.kind IN ('journal', 'handoff') ORDER BY r.rid");
        let mut st = self.conn.prepare_cached(&sql)?;
        let rows = st.query_map([lane], head_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Keys of questions in a lane that an `answered` entry refs.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn answered(&self, lane: &str) -> Result<Vec<String>> {
        let sql = "SELECT f.target FROM refs f JOIN records r ON r.rid = f.rid WHERE f.kind = 'answers' AND r.scope = ?1";
        let mut st = self.conn.prepare_cached(sql)?;
        let rows = st.query_map([lane], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// Every lane with a journal or a handoff, newest activity first.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn lanes(&self) -> Result<Vec<LaneStatus>> {
        let sql = "SELECT r.scope, COUNT(*), MAX(v.time),
                COALESCE((SELECT v2.title FROM records r2 JOIN versions v2 ON v2.vid = r2.head
                          WHERE r2.scope = r.scope AND r2.kind = 'journal' AND r2.sub = 'commit' ORDER BY r2.rid DESC LIMIT 1), ''),
                SUM(r.kind = 'journal' AND r.sub = 'question'
                    AND r.key NOT IN (SELECT f.target FROM refs f WHERE f.kind = 'answers')),
                SUM(r.kind = 'handoff')
            FROM records r JOIN versions v ON v.vid = r.head
            WHERE r.kind IN ('journal', 'handoff') AND r.scope <> ''
            GROUP BY r.scope ORDER BY MAX(v.time) DESC";
        let mut st = self.conn.prepare_cached(sql)?;
        let rows = st
            .query_map([], |r| {
                Ok(LaneStatus {
                    lane: r.get(0)?,
                    entries: r.get(1)?,
                    last: r.get(2)?,
                    last_commit: r.get(3)?,
                    open_questions: r.get(4)?,
                    handoffs: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The row ids placed in an export, in order.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn placement(&self, export: &str) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare_cached("SELECT rid FROM placement WHERE export = ?1 ORDER BY ord")?;
        let rows = st.query_map([export], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(rows)
    }

    /// The exports a record is placed in.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn exports_of(&self, key: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT DISTINCT p.export FROM placement p JOIN records r ON r.rid = p.rid WHERE r.key = ?1")?;
        let rows = st.query_map([key], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// Counts of records by kind.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn counts(&self) -> Result<Vec<(String, i64)>> {
        let mut st = self.conn.prepare_cached("SELECT kind, COUNT(*) FROM records GROUP BY kind ORDER BY kind")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A value from the store's own key/value table.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.prepare_cached("SELECT value FROM mem_meta WHERE key = ?1")?.query_row([key], |r| r.get(0)).optional()?)
    }

    /// Sets a value in the store's own key/value table.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.prepare_cached("INSERT INTO mem_meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value")?.execute([key, value])?;
        Ok(())
    }

    /// The next free key `<prefix><n>` (n zero-padded to `width`).
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn next_key(&self, prefix: &str, width: usize) -> Result<String> {
        next_key(&self.conn, prefix, width)
    }
}

fn next_key(conn: &Connection, prefix: &str, width: usize) -> Result<String> {
    let pattern = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
    let mut st = conn.prepare_cached("SELECT key FROM records WHERE key LIKE ?1 ESCAPE '\\'")?;
    let keys = st.query_map([pattern], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let max = keys.iter().filter_map(|k| k[prefix.len()..].parse::<u64>().ok()).max().unwrap_or(0);
    Ok(format!("{prefix}{:0width$}", max + 1))
}

/// Whether `hay` holds `word` with no letter or digit either side (both
/// already lowercase).
pub fn contains_word(hay: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let b = hay.as_bytes();
    let mut from = 0;
    while let Some(i) = hay[from..].find(word) {
        let s = from + i;
        let e = s + word.len();
        let before = s == 0 || !b[s - 1].is_ascii_alphanumeric();
        let after = e >= b.len() || !b[e].is_ascii_alphanumeric();
        if before && after {
            return true;
        }
        from = s + 1;
        while from < hay.len() && !hay.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

/// One write batch. Dropped without [`Writer::commit`], nothing is written.
pub struct Writer<'a> {
    tx: rusqlite::Transaction<'a>,
    log: String,
    jsonl: PathBuf,
    pid: i64,
}

impl Writer<'_> {
    fn insert_version(&mut self, rid: i64, seq: i64, prev_chain: &str, d: &Draft) -> Result<(i64, String)> {
        let sha = version_sha(&d.title, &d.meta, &d.body);
        let chain = chain_next(prev_chain, &sha);
        let time = d.time.clone().unwrap_or_else(now);
        let p = d.provenance.clone().unwrap_or_default();
        let has_p = d.provenance.is_some();
        self.tx
            .prepare_cached(
                "INSERT INTO versions(rid, seq, time, title, meta, body, sha, chain, reason, author, client, pid, src, src_path, src_line, src_sha)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            )?
            .execute(params![
                rid,
                seq,
                time,
                d.title,
                d.meta,
                d.body,
                sha,
                chain,
                d.reason,
                d.author,
                d.client,
                self.pid,
                has_p.then_some(&p.src),
                has_p.then_some(&p.path),
                has_p.then_some(p.line),
                has_p.then_some(&p.sha)
            ])?;
        let vid = self.tx.last_insert_rowid();
        self.tx.prepare_cached("DELETE FROM records_fts WHERE rowid = ?1")?.execute([rid])?;
        self.tx.prepare_cached("INSERT INTO records_fts(rowid, title, body) VALUES(?1, ?2, ?3)")?.execute(params![rid, d.title, d.body])?;
        Ok((vid, sha))
    }

    fn rid_of(&self, key: &str) -> Result<Option<(i64, String, String)>> {
        Ok(self
            .tx
            .prepare_cached("SELECT r.rid, v.sha, v.chain FROM records r JOIN versions v ON v.vid = r.head WHERE r.key = ?1")?
            .query_row([key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?)
    }

    /// Creates a record with its first version.
    ///
    /// # Arguments
    ///
    /// * `key` - the new key; it must be unused.
    /// * `kind`, `sub`, `scope` - see [`Head`].
    /// * `draft` - the text.
    /// * `place` - an export and where in it, if the record is placed.
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyExists`] for a used key; [`Error::NotFound`] when
    /// placing after a key not in the export; [`Error::Db`].
    pub fn create(&mut self, key: &str, kind: &str, sub: &str, scope: &str, draft: &Draft, place: Option<(&str, Place)>) -> Result<Written> {
        if self.rid_of(key)?.is_some() {
            return Err(Error::AlreadyExists(format!("record {key:?}")));
        }
        let time = now();
        // head is set once the version exists.
        self.tx
            .prepare_cached("INSERT INTO records(key, kind, sub, scope, status, head, created) VALUES(?1, ?2, ?3, ?4, 'active', 0, ?5)")?
            .execute(params![key, kind, sub, scope, time])?;
        let rid = self.tx.last_insert_rowid();
        let (vid, sha) = self.insert_version(rid, 1, "", draft)?;
        self.tx.prepare_cached("UPDATE records SET head = ?1 WHERE rid = ?2")?.execute([vid, rid])?;
        let mut placed = Value::Null;
        if let Some((export, at)) = place {
            let ord = self.place(export, rid, &at)?;
            placed = crate::json!({"export": export, "ord": ord});
        }
        self.journal_line("create", key, kind, sub, scope, 1, &sha, draft, placed, &[]);
        Ok(Written { key: key.to_string(), seq: 1, sha, changed: true })
    }

    /// Appends a version to an existing record.
    ///
    /// # Arguments
    ///
    /// * `key` - the record.
    /// * `expected_sha` - the head's sha the caller read; `None` skips the
    ///   check (imports only).
    /// * `draft` - the new text.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown key; [`Error::InvalidArgument`] for
    /// an `expected_sha` shorter than [`MIN_SHA_PREFIX`] hex digits;
    /// [`Error::StaleRecord`] when the head moved; [`Error::Db`].
    pub fn revise(&mut self, key: &str, expected_sha: Option<&str>, draft: &Draft) -> Result<Written> {
        let (rid, sha, chain) = self.rid_of(key)?.ok_or_else(|| Error::NotFound(format!("record {key:?}")))?;
        if let Some(e) = expected_sha {
            // The ok line shows 16 hex digits: any prefix of at least 12
            // names the head (case aside), as the full 64 do.
            let e = e.trim().to_ascii_lowercase();
            if e.len() < MIN_SHA_PREFIX || e.len() > sha.len() || !e.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::InvalidArgument(format!("expected_sha {e:?}: give the head's sha, or at least its first {MIN_SHA_PREFIX} hex digits (the ok line and recall show 16)")));
            }
            if !sha.starts_with(&e) {
                return Err(Error::StaleRecord { key: key.to_string(), expected: e, actual: sha });
            }
        }
        let new_sha = version_sha(&draft.title, &draft.meta, &draft.body);
        let seq: i64 = self.tx.prepare_cached("SELECT MAX(seq) FROM versions WHERE rid = ?1")?.query_row([rid], |r| r.get(0))?;
        if new_sha == sha {
            return Ok(Written { key: key.to_string(), seq, sha, changed: false });
        }
        let (vid, new_sha) = self.insert_version(rid, seq + 1, &chain, draft)?;
        self.tx.prepare_cached("UPDATE records SET head = ?1 WHERE rid = ?2")?.execute([vid, rid])?;
        let (kind, sub, scope): (String, String, String) =
            self.tx.prepare_cached("SELECT kind, sub, scope FROM records WHERE rid = ?1")?.query_row([rid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        self.journal_line("version", key, &kind, &sub, &scope, seq + 1, &new_sha, draft, Value::Null, &[]);
        Ok(Written { key: key.to_string(), seq: seq + 1, sha: new_sha, changed: true })
    }

    /// Creates a record that supersedes others; each old record is marked
    /// `superseded` and keeps every version.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown old key, [`Error::InvalidArgument`]
    /// for one already superseded, and as [`Writer::create`].
    pub fn supersede(&mut self, old: &[String], key: &str, kind: &str, sub: &str, scope: &str, draft: &Draft, place: Option<(&str, Place)>) -> Result<Written> {
        let mut olds = Vec::new();
        for o in old {
            let (rid, _, _) = self.rid_of(o)?.ok_or_else(|| Error::NotFound(format!("record {o:?}")))?;
            let status: String = self.tx.prepare_cached("SELECT status FROM records WHERE rid = ?1")?.query_row([rid], |r| r.get(0))?;
            if status != "active" {
                return Err(Error::InvalidArgument(format!("record {o:?} is already {status}")));
            }
            olds.push(rid);
        }
        let w = self.create(key, kind, sub, scope, draft, place)?;
        let (new_rid, _, _) = self.rid_of(key)?.ok_or_else(|| Error::Db("new record vanished".into()))?;
        let time = now();
        for rid in &olds {
            self.tx
                .prepare_cached("INSERT INTO supersedes(new_rid, old_rid, time, reason) VALUES(?1, ?2, ?3, ?4)")?
                .execute(params![new_rid, rid, time, draft.reason])?;
            self.tx.prepare_cached("UPDATE records SET status = 'superseded' WHERE rid = ?1")?.execute([rid])?;
        }
        let line = json::object(128, |w| {
            w.field("op", "supersede").field("key", key).field("old", &old.to_vec()).field("time", &time);
        });
        self.log.push_str(&line);
        self.log.push('\n');
        Ok(w)
    }

    /// Retires a record whose source item is gone: it keeps every version,
    /// leaves every export it was placed in, and stops being recalled by
    /// query (`recall {key}` still answers it).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown key, [`Error::InvalidArgument`]
    /// for one that is not active; [`Error::Db`].
    pub fn retire(&mut self, key: &str, reason: &str) -> Result<()> {
        self.set_status(key, "active", "retired", reason)?;
        let (rid, _, _) = self.rid_of(key)?.ok_or_else(|| Error::NotFound(format!("record {key:?}")))?;
        self.tx.prepare_cached("DELETE FROM placement WHERE rid = ?1")?.execute([rid])?;
        self.tx.prepare_cached("INSERT INTO retirements(rid, time, reason) VALUES(?1, ?2, ?3)")?.execute(params![rid, now(), reason])?;
        Ok(())
    }

    /// Makes a retired record active again (its source item came back).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown key, [`Error::InvalidArgument`]
    /// for one that is not retired; [`Error::Db`].
    pub fn reactivate(&mut self, key: &str, reason: &str) -> Result<()> {
        self.set_status(key, "retired", "active", reason)
    }

    fn set_status(&mut self, key: &str, from: &str, to: &str, reason: &str) -> Result<()> {
        let (rid, _, _) = self.rid_of(key)?.ok_or_else(|| Error::NotFound(format!("record {key:?}")))?;
        let status: String = self.tx.prepare_cached("SELECT status FROM records WHERE rid = ?1")?.query_row([rid], |r| r.get(0))?;
        if status != from {
            return Err(Error::InvalidArgument(format!("record {key:?} is {status}, not {from}")));
        }
        self.tx.prepare_cached("UPDATE records SET status = ?1 WHERE rid = ?2")?.execute(params![to, rid])?;
        let op = if to == "retired" { "retire" } else { "reactivate" };
        let time = now();
        let line = json::object(128, |w| {
            w.field("op", op).field("key", key).field("time", &time).field("reason", reason);
        });
        self.log.push_str(&line);
        self.log.push('\n');
        Ok(())
    }

    /// Places an existing record in an export (a record already there is
    /// left where it is).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::Db`].
    pub fn place_record(&mut self, export: &str, key: &str, at: Place) -> Result<()> {
        let (rid, _, _) = self.rid_of(key)?.ok_or_else(|| Error::NotFound(format!("record {key:?}")))?;
        let there: Option<i64> = self.tx.prepare_cached("SELECT ord FROM placement WHERE export = ?1 AND rid = ?2")?.query_row(params![export, rid], |r| r.get(0)).optional()?;
        if there.is_some() {
            return Ok(());
        }
        let ord = self.place(export, rid, &at)?;
        let line = json::object(96, |w| {
            w.field("op", "place").field("key", key).field("export", export).field("ord", &ord);
        });
        self.log.push_str(&line);
        self.log.push('\n');
        Ok(())
    }

    /// Takes a record out of an export; the record itself is untouched.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::Db`].
    pub fn unplace_record(&mut self, export: &str, key: &str) -> Result<()> {
        let (rid, _, _) = self.rid_of(key)?.ok_or_else(|| Error::NotFound(format!("record {key:?}")))?;
        self.tx.prepare_cached("DELETE FROM placement WHERE export = ?1 AND rid = ?2")?.execute(params![export, rid])?;
        let line = json::object(96, |w| {
            w.field("op", "unplace").field("key", key).field("export", export);
        });
        self.log.push_str(&line);
        self.log.push('\n');
        Ok(())
    }

    /// Adds references from a record (`kind` such as `lane`, `commit`,
    /// `answers`, `prior`).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::Db`].
    pub fn add_refs(&mut self, key: &str, refs: &[(String, String)]) -> Result<()> {
        let (rid, _, _) = self.rid_of(key)?.ok_or_else(|| Error::NotFound(format!("record {key:?}")))?;
        for (k, t) in refs {
            self.tx.prepare_cached("INSERT INTO refs(rid, kind, target) VALUES(?1, ?2, ?3)")?.execute(params![rid, k, t])?;
        }
        if !refs.is_empty() {
            let pairs: Vec<Value> = refs.iter().map(|(k, t)| crate::json!([k, t])).collect();
            let line = json::object(128, |w| {
                w.field("op", "refs").field("key", key).field("refs", &Value::Array(pairs));
            });
            self.log.push_str(&line);
            self.log.push('\n');
        }
        Ok(())
    }

    fn place(&mut self, export: &str, rid: i64, at: &Place) -> Result<i64> {
        let ords: Vec<i64> = {
            let mut st = self.tx.prepare_cached("SELECT ord FROM placement WHERE export = ?1 ORDER BY ord")?;
            let v = st.query_map([export], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
            v
        };
        let ord = match at {
            Place::End => ords.last().map_or(ORD_STEP, |l| l + ORD_STEP),
            Place::Start => ords.first().map_or(ORD_STEP, |f| f - ORD_STEP),
            Place::After(k) => {
                let (after_rid, _, _) = self.rid_of(k)?.ok_or_else(|| Error::NotFound(format!("record {k:?}")))?;
                let a: i64 = self
                    .tx
                    .prepare_cached("SELECT ord FROM placement WHERE export = ?1 AND rid = ?2")?
                    .query_row(params![export, after_rid], |r| r.get(0))
                    .optional()?
                    .ok_or_else(|| Error::NotFound(format!("record {k:?} is not in export {export:?}")))?;
                let next = ords.iter().copied().find(|o| *o > a);
                match next {
                    None => a + ORD_STEP,
                    Some(n) if n - a >= 2 => a + (n - a) / 2,
                    Some(_) => {
                        // No gap left: respace the whole export, then retry.
                        self.respace(export)?;
                        return self.place(export, rid, at);
                    }
                }
            }
        };
        self.tx.prepare_cached("INSERT INTO placement(export, ord, rid) VALUES(?1, ?2, ?3)")?.execute(params![export, ord, rid])?;
        Ok(ord)
    }

    fn respace(&mut self, export: &str) -> Result<()> {
        let rows: Vec<(i64, i64)> = {
            let mut st = self.tx.prepare_cached("SELECT ord, rid FROM placement WHERE export = ?1 ORDER BY ord")?;
            let v = st.query_map([export], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        self.tx.prepare_cached("DELETE FROM placement WHERE export = ?1")?.execute([export])?;
        for (i, (_, rid)) in rows.iter().enumerate() {
            self.tx.prepare_cached("INSERT INTO placement(export, ord, rid) VALUES(?1, ?2, ?3)")?.execute(params![export, (i as i64 + 1) * ORD_STEP, rid])?;
        }
        let line = json::object(64, |w| {
            w.field("op", "respace").field("export", export);
        });
        self.log.push_str(&line);
        self.log.push('\n');
        Ok(())
    }

    /// A record's head as this batch sees it.
    ///
    /// # Errors
    ///
    /// `QueryReturnedNoRows` for an unknown key.
    pub fn head(&self, key: &str) -> rusqlite::Result<Head> {
        self.tx.prepare_cached(&format!("{HEAD_SQL} WHERE r.key = ?1"))?.query_row([key], head_row)
    }

    /// The exports a record is placed in, as this batch sees them.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn exports_of(&self, key: &str) -> Result<Vec<String>> {
        let mut st = self.tx.prepare_cached("SELECT DISTINCT p.export FROM placement p JOIN records r ON r.rid = p.rid WHERE r.key = ?1")?;
        let rows = st.query_map([key], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// The next free key `<prefix><n>` inside this batch.
    ///
    /// # Errors
    ///
    /// [`Error::Db`].
    pub fn next_key(&self, prefix: &str, width: usize) -> Result<String> {
        next_key(&self.tx, prefix, width)
    }

    #[allow(clippy::too_many_arguments)]
    fn journal_line(&mut self, op: &str, key: &str, kind: &str, sub: &str, scope: &str, seq: i64, sha: &str, d: &Draft, placed: Value, _extra: &[()]) {
        let mut line = String::with_capacity(d.body.len() + 256);
        let mut w = json::ObjectWriter::new(&mut line);
        w.field("op", op)
            .field("key", key)
            .field("kind", kind)
            .field("sub", sub)
            .field("scope", scope)
            .field("seq", &seq)
            .field("sha", sha)
            .field("title", &d.title)
            .field("meta", &d.meta)
            .field("body", &d.body)
            .field("reason", &d.reason)
            .field("author", &d.author);
        if !placed.is_null() {
            w.field("place", &placed);
        }
        w.end();
        self.log.push_str(&line);
        self.log.push('\n');
    }

    /// Appends the batch's lines to the `.jsonl` copy and syncs it, then
    /// commits, all under the batch's write lock: the copy holds the lines
    /// in commit order, and a crash between the append and the commit
    /// leaves only lines past the recorded end, which the next batch cuts
    /// off (see [`MemoryStore::writer`]). A crash after the commit leaves
    /// nothing to repair.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the copy cannot be appended, or [`Error::Db`] if the
    /// commit fails; either way nothing is committed and the copy is cut
    /// back to where it was.
    pub fn commit(self) -> Result<()> {
        let Writer { tx, log, jsonl, .. } = self;
        if log.is_empty() || jsonl.as_os_str().is_empty() {
            tx.commit()?;
            return Ok(());
        }
        let mut f = OpenOptions::new().create(true).append(true).open(&jsonl)?;
        let before = f.metadata()?.len();
        let end = before + log.len() as u64;
        tx.prepare_cached("INSERT INTO mem_meta(key, value) VALUES('jsonl_end', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value")?.execute([end.to_string()])?;
        let cut_back = |f: &std::fs::File| {
            let _ = f.set_len(before);
        };
        if let Err(e) = f.write_all(log.as_bytes()).and_then(|()| f.sync_data()) {
            cut_back(&f);
            return Err(e.into());
        }
        if let Err(e) = tx.commit() {
            cut_back(&f);
            return Err(e.into());
        }
        Ok(())
    }
}
