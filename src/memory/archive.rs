//! Snapshot retention: roll-up compaction into an archive.
//!
//! Snapshots (`memory-YYYYMMDD-HHMMSS.sqlite`, see [`super::check::snapshot`])
//! are kept until the snapshot folder passes its budget (`memory.snapshot_cap_mb`,
//! default 1 GB). Past it, the oldest snapshots are folded, one at a time and
//! oldest first, into `archive.sqlite` in the same folder, and only then
//! removed. The archive keeps every distinct record version once (keyed by
//! record key, version number and SHA-256), with its record, its supersede
//! rows and its refs, so the folder keeps every version any snapshot ever
//! held at the cost of the versions that differ, not a whole copy per day.
//!
//! The archive is indexed by record and time, and [`as_of`] answers "what did
//! record X look like at time T" from the archive, the remaining snapshots and
//! (optionally) the live store together: the newest version of X written at or
//! before T, and whether X had been superseded by then. Every source holds
//! whole, append-only histories, so the answer does not depend on which
//! snapshots have been folded (tested).
//!
//! A fold is one transaction: each version's SHA-256 is recomputed from its
//! text before it is copied (a damaged snapshot is refused and left in
//! place), every version of the snapshot is then looked up in the archive by
//! key, number and SHA-256, and the snapshot file is removed only after that
//! check and the commit.

use std::path::{Path, PathBuf};

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::error::{Error, Result};

use super::version_sha;

/// The archive's file name inside the snapshot folder.
pub const ARCHIVE_FILE: &str = "archive.sqlite";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rolled(
    sid INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    taken TEXT NOT NULL,
    records INTEGER NOT NULL,
    versions INTEGER NOT NULL,
    added INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    time TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS records(
    key TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    sub TEXT NOT NULL,
    scope TEXT NOT NULL,
    created TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS versions(
    key TEXT NOT NULL,
    seq INTEGER NOT NULL,
    sha TEXT NOT NULL,
    time TEXT NOT NULL,
    title TEXT NOT NULL,
    meta TEXT NOT NULL,
    body TEXT NOT NULL,
    chain TEXT NOT NULL,
    reason TEXT NOT NULL,
    author TEXT NOT NULL,
    client TEXT NOT NULL,
    pid INTEGER NOT NULL,
    src TEXT, src_path TEXT, src_line INTEGER, src_sha TEXT,
    first_sid INTEGER NOT NULL,
    UNIQUE(key, seq, sha)
);
CREATE INDEX IF NOT EXISTS versions_time ON versions(key, time, seq);
CREATE TABLE IF NOT EXISTS supersedes(
    new_key TEXT NOT NULL, old_key TEXT NOT NULL, time TEXT NOT NULL, reason TEXT NOT NULL,
    PRIMARY KEY(new_key, old_key)
);
CREATE INDEX IF NOT EXISTS supersedes_old ON supersedes(old_key, time);
CREATE TABLE IF NOT EXISTS refs(key TEXT NOT NULL, kind TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY(key, kind, target));
CREATE TRIGGER IF NOT EXISTS archive_versions_no_update BEFORE UPDATE ON versions BEGIN SELECT RAISE(ABORT, 'archived versions are append-only'); END;
CREATE TRIGGER IF NOT EXISTS archive_versions_no_delete BEFORE DELETE ON versions BEGIN SELECT RAISE(ABORT, 'archived versions are append-only'); END;
";

/// One snapshot file in the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapFile {
    /// The file.
    pub path: PathBuf,
    /// Its size in bytes.
    pub bytes: u64,
    /// Its time stamp, `YYYYMMDDHHMMSS`, from the name.
    pub stamp: String,
    /// The collision counter from the name (0 for none).
    pub n: u32,
}

impl SnapFile {
    /// The stamp as UTC ISO 8601 (`YYYY-MM-DDTHH:MM:SSZ`).
    pub fn taken(&self) -> String {
        let s = &self.stamp;
        format!("{}-{}-{}T{}:{}:{}Z", &s[0..4], &s[4..6], &s[6..8], &s[8..10], &s[10..12], &s[12..14])
    }
}

/// Parses a snapshot file name: `memory-YYYYMMDD-HHMMSS.sqlite`, with an
/// optional `-N` collision counter (`memory-YYYYMMDDHHMMSS-N.sqlite` is the
/// older spelling of the same).
///
/// # Returns
///
/// The 14-digit stamp and the counter, or `None` for any other file.
pub fn parse_name(name: &str) -> Option<(String, u32)> {
    let rest = name.strip_prefix("memory-")?.strip_suffix(".sqlite")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').filter(char::is_ascii_digit).collect();
    if digits.len() < 14 {
        return None;
    }
    let stamp = digits[..14].to_string();
    // Everything after the 14 stamp digits (and the one separator inside
    // them) is the counter, if any.
    let mut seen = 0;
    let mut cut = rest.len();
    for (i, c) in rest.char_indices() {
        if c.is_ascii_digit() {
            seen += 1;
            if seen == 14 {
                cut = i + 1;
                break;
            }
        }
    }
    let tail = &rest[cut..];
    let n = match tail {
        "" => 0,
        t => t.strip_prefix('-')?.parse().ok()?,
    };
    Some((stamp, n))
}

/// The snapshots in `dir`, oldest first (by stamp, then counter).
///
/// # Errors
///
/// [`Error::Io`] when the folder cannot be read (a missing folder is empty).
pub fn snapshots(dir: &Path) -> Result<Vec<SnapFile>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for e in rd.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some((stamp, n)) = parse_name(&name) {
            let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(SnapFile { path: e.path(), bytes, stamp, n });
        }
    }
    out.sort_by(|a, b| (&a.stamp, a.n).cmp(&(&b.stamp, b.n)));
    Ok(out)
}

/// What one fold did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folded {
    /// The snapshot folded.
    pub snapshot: PathBuf,
    /// Versions it held.
    pub versions: i64,
    /// Of those, versions the archive did not have yet.
    pub added: i64,
}

fn open_archive(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA synchronous=FULL;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

fn open_ro(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?)
}

/// Folds one snapshot into the archive (created if absent). The snapshot
/// file is not removed; [`roll_up`] does that after this returns.
///
/// # Errors
///
/// [`Error::InvalidArgument`] when the file is not a snapshot name, or is
/// not newer than the last snapshot folded (folding keeps time order);
/// [`Error::Io`] when a version's text does not match its SHA-256 or a
/// version is missing from the archive afterwards (nothing is committed);
/// [`Error::Db`].
pub fn fold(archive: &Path, snap: &Path) -> Result<Folded> {
    let name = snap.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stamp, n) = parse_name(&name).ok_or_else(|| Error::InvalidArgument(format!("{} is not a snapshot", snap.display())))?;
    let file = SnapFile { path: snap.to_path_buf(), bytes: std::fs::metadata(snap)?.len(), stamp, n };
    let mut a = open_archive(archive)?;
    let last: Option<String> = a.query_row("SELECT name FROM rolled ORDER BY sid DESC LIMIT 1", [], |r| r.get(0)).optional()?;
    if let Some(last) = last {
        let order = parse_name(&last).unwrap_or_default();
        if (file.stamp.clone(), file.n) <= order {
            return Err(Error::InvalidArgument(format!("{name} is not newer than {last}, the last snapshot folded")));
        }
    }
    let s = open_ro(snap)?;
    let tx = a.transaction()?;
    let sid: i64 = tx.query_row("SELECT COALESCE(MAX(sid), 0) + 1 FROM rolled", [], |r| r.get(0))?;
    let records: i64 = s.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
    {
        let mut st = s.prepare("SELECT key, kind, sub, scope, created FROM records")?;
        let mut rows = st.query([])?;
        let mut ins = tx.prepare("INSERT OR IGNORE INTO records(key, kind, sub, scope, created) VALUES(?1, ?2, ?3, ?4, ?5)")?;
        while let Some(r) = rows.next()? {
            let v: Vec<SqlValue> = (0..5).map(|i| r.get(i)).collect::<rusqlite::Result<_>>()?;
            ins.execute(rusqlite::params_from_iter(v))?;
        }
    }
    let mut versions = 0i64;
    let mut added = 0i64;
    {
        let mut st = s.prepare(
            "SELECT r.key, v.seq, v.sha, v.time, v.title, v.meta, v.body, v.chain, v.reason, v.author, v.client, v.pid,
                    v.src, v.src_path, v.src_line, v.src_sha
             FROM versions v JOIN records r ON r.rid = v.rid ORDER BY v.vid",
        )?;
        let mut rows = st.query([])?;
        let mut ins = tx.prepare(
            "INSERT OR IGNORE INTO versions(key, seq, sha, time, title, meta, body, chain, reason, author, client, pid,
                                            src, src_path, src_line, src_sha, first_sid)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        )?;
        while let Some(r) = rows.next()? {
            let key: String = r.get(0)?;
            let seq: i64 = r.get(1)?;
            let sha: String = r.get(2)?;
            let (title, meta, body): (String, String, String) = (r.get(4)?, r.get(5)?, r.get(6)?);
            if version_sha(&title, &meta, &body) != sha {
                return Err(Error::Io(format!("{name}: {key} v{seq}: text does not match its sha; the snapshot is not folded")));
            }
            let mut v: Vec<SqlValue> = (0..16).map(|i| r.get(i)).collect::<rusqlite::Result<_>>()?;
            v.push(SqlValue::Integer(sid));
            added += ins.execute(rusqlite::params_from_iter(v))? as i64;
            versions += 1;
        }
    }
    {
        let mut st = s.prepare(
            "SELECT n.key, o.key, s.time, s.reason FROM supersedes s JOIN records n ON n.rid = s.new_rid JOIN records o ON o.rid = s.old_rid",
        )?;
        let mut rows = st.query([])?;
        let mut ins = tx.prepare("INSERT OR IGNORE INTO supersedes(new_key, old_key, time, reason) VALUES(?1, ?2, ?3, ?4)")?;
        while let Some(r) = rows.next()? {
            let v: Vec<SqlValue> = (0..4).map(|i| r.get(i)).collect::<rusqlite::Result<_>>()?;
            ins.execute(rusqlite::params_from_iter(v))?;
        }
        let mut st = s.prepare("SELECT r.key, f.kind, f.target FROM refs f JOIN records r ON r.rid = f.rid")?;
        let mut rows = st.query([])?;
        let mut ins = tx.prepare("INSERT OR IGNORE INTO refs(key, kind, target) VALUES(?1, ?2, ?3)")?;
        while let Some(r) = rows.next()? {
            let v: Vec<SqlValue> = (0..3).map(|i| r.get(i)).collect::<rusqlite::Result<_>>()?;
            ins.execute(rusqlite::params_from_iter(v))?;
        }
    }
    // Every version of the snapshot is now in the archive, by key, number
    // and SHA-256 (the archive's own rows were SHA-checked when added).
    {
        let mut st = s.prepare("SELECT r.key, v.seq, v.sha FROM versions v JOIN records r ON r.rid = v.rid")?;
        let mut rows = st.query([])?;
        let mut look = tx.prepare("SELECT 1 FROM versions WHERE key = ?1 AND seq = ?2 AND sha = ?3")?;
        while let Some(r) = rows.next()? {
            let (key, seq, sha): (String, i64, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
            if look.query_row(params![key, seq, sha], |_| Ok(())).optional()?.is_none() {
                return Err(Error::Io(format!("{name}: {key} v{seq} is missing from the archive after the fold")));
            }
        }
    }
    tx.execute(
        "INSERT INTO rolled(sid, name, taken, records, versions, added, bytes, time) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![sid, name, file.taken(), records, versions, added, file.bytes as i64, super::now()],
    )?;
    tx.commit()?;
    Ok(Folded { snapshot: snap.to_path_buf(), versions, added })
}

/// Folds the oldest snapshots into the archive, removing each after its
/// fold, while the snapshots and the archive together exceed `cap_bytes`.
/// The newest snapshot (and `keep`) is never folded.
///
/// # Returns
///
/// The folds, oldest first.
///
/// # Errors
///
/// As [`fold`], and [`Error::Io`] when a folded file cannot be removed.
pub fn roll_up(dir: &Path, cap_bytes: u64, keep: Option<&Path>) -> Result<Vec<Folded>> {
    let archive = dir.join(ARCHIVE_FILE);
    let mut done = Vec::new();
    loop {
        let snaps = snapshots(dir)?;
        let archived = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
        let total: u64 = snaps.iter().map(|s| s.bytes).sum::<u64>() + archived;
        if total <= cap_bytes || snaps.len() < 2 {
            break;
        }
        let oldest = &snaps[0];
        if keep.is_some_and(|k| k == oldest.path) {
            break;
        }
        let f = fold(&archive, &oldest.path)?;
        std::fs::remove_file(&oldest.path)?;
        done.push(f);
    }
    Ok(done)
}

/// A record as it stood at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsOf {
    /// The record key.
    pub key: String,
    /// Its kind.
    pub kind: String,
    /// Its scope.
    pub scope: String,
    /// The version in force: the newest written at or before the time.
    pub seq: i64,
    /// When that version was written.
    pub time: String,
    /// Its title.
    pub title: String,
    /// Its meta lines.
    pub meta: String,
    /// Its body.
    pub body: String,
    /// Its SHA-256.
    pub sha: String,
    /// The record that superseded it by then, with the time, if any.
    pub superseded: Option<(String, String)>,
}

/// The sources [`as_of`] reads for a snapshot folder: the archive (if
/// present), the snapshots oldest first, then `store` if given.
///
/// # Errors
///
/// As [`snapshots`].
pub fn sources(dir: &Path, store: Option<&Path>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let archive = dir.join(ARCHIVE_FILE);
    if archive.exists() {
        out.push(archive);
    }
    out.extend(snapshots(dir)?.into_iter().map(|s| s.path));
    out.extend(store.map(Path::to_path_buf));
    Ok(out)
}

type Candidate = (String, i64, String, AsOf);

fn from_source(conn: &Connection, key: &str, t: &str) -> Result<(Option<Candidate>, Option<(String, String)>)> {
    let is_archive: bool = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'rolled'", [], |r| r.get::<_, i64>(0))? > 0;
    let (vsql, ssql) = if is_archive {
        (
            "SELECT r.key, r.kind, r.scope, v.seq, v.time, v.title, v.meta, v.body, v.sha
             FROM versions v JOIN records r ON r.key = v.key
             WHERE v.key = ?1 AND v.time <= ?2 ORDER BY v.time DESC, v.seq DESC, v.sha DESC LIMIT 1",
            "SELECT new_key, time FROM supersedes WHERE old_key = ?1 AND time <= ?2 ORDER BY time, new_key LIMIT 1",
        )
    } else {
        (
            "SELECT r.key, r.kind, r.scope, v.seq, v.time, v.title, v.meta, v.body, v.sha
             FROM versions v JOIN records r ON r.rid = v.rid
             WHERE r.key = ?1 AND v.time <= ?2 ORDER BY v.time DESC, v.seq DESC, v.sha DESC LIMIT 1",
            "SELECT n.key, s.time FROM supersedes s JOIN records o ON o.rid = s.old_rid JOIN records n ON n.rid = s.new_rid
             WHERE o.key = ?1 AND s.time <= ?2 ORDER BY s.time, n.key LIMIT 1",
        )
    };
    let v = conn
        .query_row(vsql, params![key, t], |r| {
            let a = AsOf {
                key: r.get(0)?,
                kind: r.get(1)?,
                scope: r.get(2)?,
                seq: r.get(3)?,
                time: r.get(4)?,
                title: r.get(5)?,
                meta: r.get(6)?,
                body: r.get(7)?,
                sha: r.get(8)?,
                superseded: None,
            };
            Ok((a.time.clone(), a.seq, a.sha.clone(), a))
        })
        .optional()?;
    let s = conn.query_row(ssql, params![key, t], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    Ok((v, s))
}

/// What record `key` looked like at time `t`, from every source together:
/// the newest version written at or before `t` (by time, then version
/// number, then SHA-256), and the first supersede of it at or before `t`.
///
/// # Arguments
///
/// * `sources` - archives, snapshots and stores (see [`sources`]).
/// * `key` - the record key.
/// * `t` - UTC ISO 8601, compared as text with the stored times
///   (`2026-10-31` is the start of that day).
///
/// # Returns
///
/// `None` when no source has a version of `key` at or before `t`.
///
/// # Errors
///
/// [`Error::Db`] when a source cannot be opened or read.
pub fn as_of(sources: &[PathBuf], key: &str, t: &str) -> Result<Option<AsOf>> {
    let mut best: Option<Candidate> = None;
    let mut sup: Option<(String, String)> = None;
    for p in sources {
        let conn = open_ro(p)?;
        let (v, s) = from_source(&conn, key, t)?;
        if let Some(c) = v {
            if best.as_ref().is_none_or(|b| (&c.0, c.1, &c.2) > (&b.0, b.1, &b.2)) {
                best = Some(c);
            }
        }
        if let Some((nk, st)) = s {
            if sup.as_ref().is_none_or(|(bk, bt)| (&st, &nk) < (bt, bk)) {
                sup = Some((nk, st));
            }
        }
    }
    Ok(best.map(|(_, _, _, mut a)| {
        a.superseded = sup;
        a
    }))
}

/// Every distinct record version the sources hold, as `(key, seq, sha)`,
/// sorted: what a roll-up must preserve.
///
/// # Errors
///
/// [`Error::Db`].
pub fn version_set(sources: &[PathBuf]) -> Result<Vec<(String, i64, String)>> {
    let mut all = std::collections::BTreeSet::new();
    for p in sources {
        let conn = open_ro(p)?;
        let is_archive: bool = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'rolled'", [], |r| r.get::<_, i64>(0))? > 0;
        let sql = if is_archive {
            "SELECT key, seq, sha FROM versions"
        } else {
            "SELECT r.key, v.seq, v.sha FROM versions v JOIN records r ON r.rid = v.rid"
        };
        let mut st = conn.prepare(sql)?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        for r in rows {
            all.insert(r?);
        }
    }
    Ok(all.into_iter().collect())
}

/// Every record key the sources hold, sorted.
///
/// # Errors
///
/// [`Error::Db`].
pub fn keys(sources: &[PathBuf]) -> Result<Vec<String>> {
    let mut all = std::collections::BTreeSet::new();
    for p in sources {
        let conn = open_ro(p)?;
        let mut st = conn.prepare("SELECT key FROM records")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            all.insert(r?);
        }
    }
    Ok(all.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_names_parse_in_time_order() {
        assert_eq!(parse_name("memory-20261031-125210.sqlite"), Some(("20261031125210".into(), 0)));
        assert_eq!(parse_name("memory-20261031-125210-2.sqlite"), Some(("20261031125210".into(), 2)));
        assert_eq!(parse_name("memory-20261031125210-1.sqlite"), Some(("20261031125210".into(), 1)));
        assert_eq!(parse_name("archive.sqlite"), None);
        assert_eq!(parse_name("memory-2026.sqlite"), None);
        assert_eq!(parse_name("memory-20261031-125210.sqlite-wal"), None);
        assert_eq!(parse_name("memory-20261031-125210x.sqlite"), None);
    }
}
