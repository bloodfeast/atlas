//! The session transcript as an opt-in source (part (c) of
//! `docs/design/compaction.md`): after a compaction, the exact text of an
//! earlier turn (an error line, a number, a command's output) is one
//! `recall` away instead of a re-run.
//!
//! Off by default (`memory.transcripts`). When on, the PreCompact hook has
//! the session's transcript indexed once its answer is out (never on the
//! hook's own path), incrementally from where the last pass stopped.
//!
//! * A turn is one transcript record's visible text
//!   ([`crate::checkpoint::turn_text`]: text and tool-result blocks); thinking
//!   blocks are never read. Its id is `turn:<session8>:<line>` and its ref
//!   hashes the visible text, as the checkpoint's result refs do.
//! * Stored text is redacted ([`crate::redact`]), and the result of a tool
//!   call whose target names a secret (a read of `.env`, a key file) is
//!   stored as `[redacted]` whole. A turn is cut at [`MAX_TURN`] bytes.
//! * Every row carries its project: the transcript's folder name, Claude
//!   Code's encoding of the session's working folder. Lookups pass the
//!   asking project and see its rows only, so a transcript never surfaces
//!   in another project.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::compact::TurnAt;
use crate::error::Result;
use crate::json::Value;

/// The most bytes of one turn kept.
pub const MAX_TURN: usize = 32 * 1024;

/// The tables, in the index database.
pub const TABLES: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS turns USING fts5(text, project UNINDEXED, session8 UNINDEXED, line UNINDEXED, role UNINDEXED, hash UNINDEXED);
CREATE TABLE IF NOT EXISTS turns_seen(path TEXT PRIMARY KEY, offset INTEGER NOT NULL, lines INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS turns_secret(path TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(path, id));
";

/// Claude Code's folder name for a working folder: every character but an
/// ASCII letter or digit becomes `-`.
pub fn project_of_cwd(dir: &Path) -> String {
    dir.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// A transcript's project: the name of the folder it is in.
pub fn project_of_transcript(path: &Path) -> String {
    path.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Indexes the records of a transcript added since the last pass. Returns
/// the turns added. Only complete lines are read, so a record still being
/// written is taken next time.
///
/// # Errors
///
/// [`crate::Error::StoreTooNew`] when a newer atlas upgraded the store since
/// this one opened it (nothing written, no table made),
/// [`crate::Error::Db`] or [`crate::Error::Io`].
pub fn index(conn: &Connection, path: &Path) -> Result<usize> {
    use std::io::{Seek, SeekFrom};
    // One write transaction, checked under the lock at its start, holds the
    // tables, a rewrite's deletes and the new turns: a refusal or an error
    // anywhere writes none of them.
    let tx = crate::store::begin_shared(conn)?;
    tx.execute_batch(TABLES)?;
    let key = path.to_string_lossy().into_owned();
    let (offset, line): (u64, i64) = tx.query_row("SELECT offset, lines FROM turns_seen WHERE path = ?1", [&key], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?))).optional()?.unwrap_or((0, 0));
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let added = if len < offset {
        // Rewritten: start over.
        tx.execute("DELETE FROM turns WHERE project = ?1 AND session8 = ?2", params![project_of_transcript(path), session8(path)])?;
        tx.execute("DELETE FROM turns_secret WHERE path = ?1", [&key])?;
        index_from(&tx, path, &key, &mut f, 0, 0)?
    } else {
        f.seek(SeekFrom::Start(offset))?;
        index_from(&tx, path, &key, &mut f, offset, line)?
    };
    tx.commit()?;
    Ok(added)
}

fn session8(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().chars().take(8).collect()).unwrap_or_default()
}

fn index_from(conn: &Connection, path: &Path, key: &str, f: &mut std::fs::File, offset: u64, mut line: i64) -> Result<usize> {
    use std::io::Read;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let text = String::from_utf8_lossy(&buf[..end]);
    let project = project_of_transcript(path);
    let s8 = session8(path);
    let deny = crate::redact::denylist();
    // Calls whose target names a secret, from earlier passes too.
    let mut secret_calls: HashSet<String> = {
        let mut st = conn.prepare_cached("SELECT id FROM turns_secret WHERE path = ?1")?;
        let ids = st.query_map([key], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        ids
    };
    let mut added = 0;
    {
        let mut ins = conn.prepare_cached("INSERT INTO turns(text, project, session8, line, role, hash) VALUES(?1, ?2, ?3, ?4, ?5, ?6)")?;
        for l in text.lines() {
            line += 1;
            if l.contains("\"isSidechain\":true") || !(l.contains("\"type\":\"user\"") || l.contains("\"type\":\"assistant\"")) {
                continue;
            }
            let Ok(v) = crate::json::parse(l) else { continue };
            let role = v.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            let blocks = v.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
            // A call whose target names a secret: its result is kept out whole.
            for b in &blocks {
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let input = b.get("input").cloned().unwrap_or(Value::Null).to_string();
                    if input.lines().any(|x| crate::redact::is_secret_line(x, deny)) || crate::redact::is_secret_line(&input, deny) {
                        if let Some(id) = b.get("id").and_then(Value::as_str) {
                            secret_calls.insert(id.to_string());
                            conn.execute("INSERT OR IGNORE INTO turns_secret(path, id) VALUES(?1, ?2)", params![key, id])?;
                        }
                    }
                }
            }
            let visible = crate::checkpoint::turn_text(&v);
            if visible.trim().is_empty() {
                continue;
            }
            let hash = crate::sha256::hex(visible.as_bytes())[..crate::compact::HASH_LEN].to_string();
            let secret = blocks.iter().any(|b| b.get("tool_use_id").and_then(Value::as_str).is_some_and(|id| secret_calls.contains(id)));
            let mut kept = if secret { crate::redact::REDACTED.to_string() } else { crate::redact::redact_with(&visible, deny) };
            if kept.len() > MAX_TURN {
                let mut cut = MAX_TURN;
                while !kept.is_char_boundary(cut) {
                    cut -= 1;
                }
                kept.truncate(cut);
                kept.push_str("\n[cut]");
            }
            ins.execute(params![kept, project, s8, line, role, hash])?;
            added += 1;
        }
    }
    conn.execute("INSERT INTO turns_seen(path, offset, lines) VALUES(?1, ?2, ?3) ON CONFLICT(path) DO UPDATE SET offset = excluded.offset, lines = excluded.lines", params![key, (offset + end as u64) as i64, line])?;
    Ok(added)
}

/// One stored turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// `turn:<session8>:<line>`.
    pub id: String,
    /// `user` or `assistant`.
    pub role: String,
    /// The hash of its visible text.
    pub hash: String,
    /// The text as stored (redacted).
    pub text: String,
}

/// A turn of `project` by session and line; `None` when not indexed (or in
/// another project).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn get(conn: &Connection, project: &str, s8: &str, line: usize) -> Result<Option<Turn>> {
    conn.execute_batch(TABLES)?;
    let t = conn
        .query_row("SELECT role, hash, text FROM turns WHERE project = ?1 AND session8 = ?2 AND line = ?3", params![project, s8, line as i64], |r| {
            Ok(Turn { id: format!("turn:{s8}:{line}"), role: r.get(0)?, hash: r.get(1)?, text: r.get(2)? })
        })
        .optional()?;
    Ok(t)
}

/// Full-text search over `project`'s turns, best first.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn search(conn: &Connection, project: &str, query: &str, limit: usize) -> Result<Vec<Turn>> {
    conn.execute_batch(TABLES)?;
    // Each word quoted: the query is words, never FTS5 syntax.
    let q: Vec<String> = query.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).map(|w| format!("\"{w}\"")).collect();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare_cached("SELECT session8, line, role, hash, text FROM turns WHERE turns MATCH ?1 AND project = ?2 ORDER BY bm25(turns) LIMIT ?3")?;
    let rows = st
        .query_map(params![q.join(" "), project, limit as i64], |r| {
            Ok(Turn { id: format!("turn:{}:{}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?), role: r.get(2)?, hash: r.get(3)?, text: r.get(4)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Resolves a turn ref against a transcript file directly (the session's
/// own, at SessionStart after a compaction): `Some(true)` when the record
/// at `at` hashes to `hash`, `Some(false)` when it does not, `None` when the
/// file has no such record.
pub fn check(path: &Path, at: TurnAt, hash: &str) -> Option<bool> {
    let l = record(path, at)?;
    let v = crate::json::parse(&String::from_utf8_lossy(&l)).ok()?;
    let h = crate::sha256::hex(crate::checkpoint::turn_text(&v).as_bytes());
    Some(h.starts_with(hash))
}

/// The record at `at` in a transcript, without its newline. By line the
/// file is read up to it; by byte offset only the record itself is read,
/// and an offset that does not start a record (not 0, and not just past a
/// newline) names none.
pub fn record(path: &Path, at: TurnAt) -> Option<Vec<u8>> {
    use std::io::{BufRead, Read, Seek, SeekFrom};
    match at {
        TurnAt::Line(line) => {
            let text = std::fs::read(path).ok()?;
            text.split(|b| *b == b'\n').nth(line.checked_sub(1)?).map(<[u8]>::to_vec)
        }
        TurnAt::Byte(offset) => {
            let mut f = std::fs::File::open(path).ok()?;
            if offset > 0 {
                let mut before = [0u8];
                f.seek(SeekFrom::Start(offset - 1)).ok()?;
                f.read_exact(&mut before).ok()?;
                if before[0] != b'\n' {
                    return None;
                }
            }
            let mut r = std::io::BufReader::new(f);
            let mut l = Vec::new();
            if r.read_until(b'\n', &mut l).ok()? == 0 {
                return None;
            }
            if l.last() == Some(&b'\n') {
                l.pop();
            }
            Some(l)
        }
    }
}

/// The 1-based line of the record starting at byte `offset` of a
/// transcript: how a byte ref finds its indexed turn. `None` when the
/// offset does not start a record. Reads the whole history before it, so
/// it is for `recall`, never a hook's path.
pub fn line_at(path: &Path, offset: u64) -> Option<usize> {
    use std::io::Read;
    record(path, TurnAt::Byte(offset))?;
    let mut r = std::fs::File::open(path).ok()?.take(offset);
    let mut buf = vec![0; 1 << 20];
    let mut n = 1;
    loop {
        let k = r.read(&mut buf).ok()?;
        if k == 0 {
            return Some(n);
        }
        n += crate::scan::count(b'\n', &buf[..k]);
    }
}

/// The indexed transcript of `project` whose session id starts with
/// `session8`, if one is indexed.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn path_of(conn: &Connection, project: &str, s8: &str) -> Result<Option<std::path::PathBuf>> {
    conn.execute_batch(TABLES)?;
    let mut st = conn.prepare_cached("SELECT path FROM turns_seen")?;
    let paths = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(paths.into_iter().map(std::path::PathBuf::from).find(|p| project_of_transcript(p) == project && session8(p) == s8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_visible_redacted_turns_per_project_incrementally() {
        let dir = std::env::temp_dir().join(format!("atlas-turns-{}", std::process::id()));
        let a = dir.join("H--proj-a");
        let b = dir.join("H--proj-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // Synthesized records; the key below is not real.
        let recs = [
            r#"{"type":"user","message":{"content":"run the build"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hidden plan"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/w/.env"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"DATABASE_HOST=db.internal"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t9","content":"error E0308 at frame 4127\nAPI_TOKEN=abcdef0123456789xyz"}]}}"#,
        ];
        let pa = a.join("12345678-aaaa.jsonl");
        std::fs::write(&pa, recs[..2].join("\n") + "\n").unwrap();
        std::fs::write(b.join("87654321-bbbb.jsonl"), recs[0].to_string() + "\n").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(index(&conn, &pa).unwrap(), 1, "the thinking-only and tool-call records hold no visible text");
        // Appended records are read from where the last pass stopped.
        std::fs::write(&pa, recs.join("\n") + "\n").unwrap();
        assert_eq!(index(&conn, &pa).unwrap(), 2);
        assert_eq!(index(&conn, &pa).unwrap(), 0);
        index(&conn, &b.join("87654321-bbbb.jsonl")).unwrap();
        let t3 = get(&conn, "H--proj-a", "12345678", 3).unwrap().unwrap();
        assert_eq!(t3.text, "[redacted]", "the result of reading a secret file is kept out whole");
        let t4 = get(&conn, "H--proj-a", "12345678", 4).unwrap().unwrap();
        assert_eq!(t4.text, "error E0308 at frame 4127\n[redacted]");
        assert_eq!(check(&pa, TurnAt::Line(4), &t4.hash), Some(true));
        // By byte offset: the same record, found without the lines before it.
        let off = recs[..3].iter().map(|r| r.len() as u64 + 1).sum::<u64>();
        assert_eq!(check(&pa, TurnAt::Byte(off), &t4.hash), Some(true));
        assert_eq!(check(&pa, TurnAt::Byte(off), "0123456789ab"), Some(false));
        assert_eq!(check(&pa, TurnAt::Byte(off + 1), &t4.hash), None, "an offset inside a record names none");
        let eof = std::fs::metadata(&pa).unwrap().len();
        assert_eq!(record(&pa, TurnAt::Byte(eof)), None, "an offset at the end names no record");
        assert_eq!(record(&pa, TurnAt::Byte(eof + 10)), None, "an offset past the end names none");
        assert_eq!(check(&pa, TurnAt::Byte(eof), &t4.hash), None);
        assert_eq!(line_at(&pa, off), Some(4));
        assert_eq!(line_at(&pa, 0), Some(1));
        assert_eq!(path_of(&conn, "H--proj-a", "12345678").unwrap(), Some(pa.clone()));
        assert_eq!(path_of(&conn, "H--proj-b", "12345678").unwrap(), None);
        assert!(!format!("{:?}", search(&conn, "H--proj-a", "hidden plan", 5).unwrap()).contains("hidden"));
        // Search sees the asking project only.
        assert_eq!(search(&conn, "H--proj-a", "4127", 5).unwrap().len(), 1);
        assert!(search(&conn, "H--proj-b", "4127", 5).unwrap().is_empty());
        assert_eq!(search(&conn, "H--proj-b", "build", 5).unwrap().len(), 1);
        assert!(get(&conn, "H--proj-b", "12345678", 4).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod storeguard_tests {
    use super::*;
    use crate::store::skew_support::{dump, make_index_newer};

    /// Indexing on a store a newer atlas upgraded since this one opened it
    /// writes no turn, makes no table and, for a rewritten transcript,
    /// deletes nothing.
    #[test]
    fn indexing_checks_the_version_under_the_lock() {
        let dir = std::env::temp_dir().join(format!("atlas-storeguard-turns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join("H--proj");
        std::fs::create_dir_all(&proj).unwrap();
        let p = proj.join("12345678-aaaa.jsonl");
        let rec = r#"{"type":"user","message":{"content":"run the build"}}"#;
        std::fs::write(&p, format!("{rec}\n{rec}\n")).unwrap();
        let c = Connection::open_in_memory().unwrap();
        assert_eq!(index(&c, &p).unwrap(), 2);
        // Rewritten shorter: the start-over deletes are refused with the rest.
        std::fs::write(&p, format!("{rec}\n")).unwrap();
        make_index_newer(&c);
        let before = dump(&c);
        assert!(index(&c, &p).expect_err("refused").is_store_too_new());
        assert!(c.is_autocommit(), "rolled back");
        assert_eq!(dump(&c), before);
        let c = Connection::open_in_memory().unwrap();
        make_index_newer(&c);
        let before = dump(&c);
        assert!(index(&c, &p).expect_err("refused").is_store_too_new());
        assert_eq!(dump(&c), before, "no table made");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
