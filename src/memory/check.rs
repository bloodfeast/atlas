//! The integrity check and snapshots.
//!
//! `--memory-check` runs four checks and lists every disagreement:
//!
//! 1. **Chains:** every version's sha recomputes from its text and every
//!    record's hash chain verifies from version 1; each head is the record's
//!    newest version.
//! 2. **Copy:** the `.jsonl` copy replays to the same heads (key, version,
//!    sha, status) and holds one line per version.
//! 3. **Disk:** each export renders to the bytes in the export folder.
//! 4. **Round trip:** each rendered export splits back into exactly its
//!    records (see [`super::export::round_trip`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::json::Value;

use super::export::{self, ExportSpec};
use super::{chain_next, version_sha, MemoryStore};

/// What the check found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Records checked.
    pub records: usize,
    /// Versions checked.
    pub versions: usize,
    /// Lines in the `.jsonl` copy.
    pub copy_lines: usize,
    /// Export files compared with the folder.
    pub files: usize,
    /// Every disagreement, one line each, prefixed by its check.
    pub problems: Vec<String>,
}

impl CheckReport {
    /// True when nothing disagreed.
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Runs the four checks.
///
/// # Arguments
///
/// * `store` - the store.
/// * `specs` - the configured exports.
/// * `dir` - the export folder, when the disk check should run.
/// * `copy` - whether to replay the `.jsonl` copy (off for in-memory stores).
///
/// # Errors
///
/// [`Error::Db`] or [`Error::Io`] when a check cannot run at all.
pub fn check(store: &MemoryStore, specs: &[ExportSpec], dir: Option<&Path>, copy: bool) -> Result<CheckReport> {
    let mut rep = CheckReport::default();
    let conn = store.conn();
    // 1. chains
    let mut heads: HashMap<String, (i64, String, String)> = HashMap::new();
    {
        let mut st = conn.prepare(
            "SELECT r.key, r.head, r.status, v.vid, v.seq, v.title, v.meta, v.body, v.sha, v.chain
             FROM versions v JOIN records r ON r.rid = v.rid ORDER BY v.rid, v.seq",
        )?;
        let mut rows = st.query([])?;
        let mut cur_key = String::new();
        let mut prev_chain = String::new();
        let mut expect_seq = 1i64;
        let mut last_vid = 0i64;
        let mut cur_head = 0i64;
        let finish = |key: &str, head: i64, last: i64, problems: &mut Vec<String>| {
            if !key.is_empty() && head != last {
                problems.push(format!("chain: {key}: head is version row {head}, newest is {last}"));
            }
        };
        while let Some(r) = rows.next()? {
            let key: String = r.get(0)?;
            let head: i64 = r.get(1)?;
            let status: String = r.get(2)?;
            let vid: i64 = r.get(3)?;
            let seq: i64 = r.get(4)?;
            let (title, meta, body): (String, String, String) = (r.get(5)?, r.get(6)?, r.get(7)?);
            let (sha, chain): (String, String) = (r.get(8)?, r.get(9)?);
            if key != cur_key {
                finish(&cur_key, cur_head, last_vid, &mut rep.problems);
                cur_key = key.clone();
                prev_chain.clear();
                expect_seq = 1;
                rep.records += 1;
            }
            rep.versions += 1;
            cur_head = head;
            last_vid = vid;
            if seq != expect_seq {
                rep.problems.push(format!("chain: {key}: version {seq} where {expect_seq} was expected"));
            }
            expect_seq = seq + 1;
            let want = version_sha(&title, &meta, &body);
            if want != sha {
                rep.problems.push(format!("chain: {key} v{seq}: stored sha does not match its text"));
            }
            let want_chain = chain_next(&prev_chain, &sha);
            if want_chain != chain {
                rep.problems.push(format!("chain: {key} v{seq}: hash chain broken"));
            }
            prev_chain = chain;
            heads.insert(key, (seq, sha, status));
        }
        finish(&cur_key, cur_head, last_vid, &mut rep.problems);
    }
    // 2. the copy
    if copy && !store.jsonl().as_os_str().is_empty() {
        let text = std::fs::read_to_string(store.jsonl()).unwrap_or_default();
        let mut replay: HashMap<String, (i64, String, String)> = HashMap::new();
        let mut version_lines = 0usize;
        for (i, line) in text.lines().enumerate() {
            rep.copy_lines += 1;
            let v: Value = match crate::json::parse(line) {
                Ok(v) => v,
                Err(e) => {
                    rep.problems.push(format!("copy: line {}: {e}", i + 1));
                    continue;
                }
            };
            let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            match v.get("op").and_then(Value::as_str) {
                Some("create" | "version") => {
                    version_lines += 1;
                    let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(0);
                    let sha = s("sha");
                    if version_sha(&s("title"), &s("meta"), &s("body")) != sha {
                        rep.problems.push(format!("copy: line {}: text does not match its sha", i + 1));
                    }
                    let status = replay.get(&s("key")).map(|x| x.2.clone()).unwrap_or_else(|| "active".into());
                    replay.insert(s("key"), (seq, sha, status));
                }
                Some(op @ ("retire" | "reactivate")) => {
                    if let Some(e) = replay.get_mut(&s("key")) {
                        e.2 = if op == "retire" { "retired".into() } else { "active".into() };
                    }
                }
                Some("supersede") => {
                    for o in v.get("old").and_then(Value::as_array).into_iter().flatten() {
                        if let Some(e) = o.as_str().and_then(|k| replay.get_mut(k)) {
                            e.2 = "superseded".into();
                        }
                    }
                }
                _ => {}
            }
        }
        if version_lines != rep.versions {
            rep.problems.push(format!("copy: {} version lines for {} stored versions", version_lines, rep.versions));
        }
        let mut keys: Vec<&String> = heads.keys().collect();
        keys.sort();
        for k in keys {
            if replay.get(k) != heads.get(k) {
                rep.problems.push(format!("copy: {k}: replayed head differs from the store"));
            }
        }
    }
    // 3. disk, 4. round trip
    for spec in specs {
        if let Some(d) = dir {
            for (rel, text) in export::render(store, spec)? {
                rep.files += 1;
                let full = d.join(&rel);
                match std::fs::read(&full) {
                    Ok(b) if b == text.as_bytes() => {}
                    Ok(b) => rep.problems.push(format!("disk: {} differs from its export at line {}", full.display(), export::first_diff_line(&b, text.as_bytes()))),
                    Err(_) => rep.problems.push(format!("disk: {} is missing", full.display())),
                }
            }
        }
        for e in export::round_trip(store, spec)? {
            rep.problems.push(format!("round trip: {e}"));
        }
    }
    Ok(rep)
}

/// What a snapshot did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The snapshot file.
    pub path: PathBuf,
    /// Its size in bytes.
    pub bytes: u64,
    /// Records in it (verified equal to the store's).
    pub records: i64,
    /// Older snapshots folded into the archive (and then removed) to stay
    /// under the size budget, oldest first.
    pub rolled: Vec<super::archive::Folded>,
}

/// Copies the store with `VACUUM INTO`, verifies the copy (SQLite
/// `integrity_check` and the record count), then, while the folder's
/// snapshots and archive total more than `cap_bytes`, folds the oldest
/// snapshots into the archive and removes them (never the one just made;
/// see [`super::archive::roll_up`]).
///
/// The file is `memory-YYYYMMDD-HHMMSS.sqlite`, or with `-N` appended when
/// that name is taken.
///
/// # Errors
///
/// [`Error::Db`] or [`Error::Io`]; [`Error::Io`] also when the copy fails
/// verification (the bad copy is left in place for inspection).
pub fn snapshot(store: &MemoryStore, dir: &Path, cap_bytes: u64) -> Result<Snapshot> {
    std::fs::create_dir_all(dir)?;
    let stamp: String = super::now().chars().filter(|c| c.is_ascii_digit()).take(14).collect();
    let base = format!("memory-{}-{}", &stamp[..8.min(stamp.len())], &stamp[8.min(stamp.len())..]);
    let mut path = dir.join(format!("{base}.sqlite"));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.sqlite"));
        n += 1;
    }
    let target = path.to_string_lossy().into_owned();
    store.conn().execute("VACUUM INTO ?1", [&target])?;
    let records: i64 = store.conn().query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
    let copy = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let ic: String = copy.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let copied: i64 = copy.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
    drop(copy);
    if ic != "ok" || copied != records {
        return Err(Error::Io(format!("snapshot {} failed verification: integrity {ic}, {copied} of {records} records", path.display())));
    }
    let bytes = std::fs::metadata(&path)?.len();
    let rolled = super::archive::roll_up(dir, cap_bytes, Some(&path))?;
    store.meta_set("last_snapshot", &super::now())?;
    let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    store.meta_set("last_snapshot_epoch", &epoch.to_string())?;
    Ok(Snapshot { path, bytes, records, rolled })
}
