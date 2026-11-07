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
//!
//! Another session may be writing throughout. Chains, copy and round trip read
//! one snapshot of the store (a read transaction), so what they find stands
//! whatever writers do. The disk check cannot: exports are written after the
//! commit, under a lock of their own, so a file can be newer or older than any
//! snapshot. It compares first with no lock, then takes each mismatch again
//! under the store's write lock, where no commit can arrive, and again past the
//! longest write seen, reporting only what persists. Writers running is never
//! a reason to skip or soften a finding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    /// What is wrong but repairs itself, one line each: lines of the copy
    /// the store never committed, which the next write voids.
    pub warnings: Vec<String>,
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
    // One snapshot from its first read on; a drop on an early `?` ends it.
    let snapshot = conn.unchecked_transaction()?;
    // 1. chains (the first read: the copy below must not be older than it)
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
        // Only the committed lines: a batch appends before it commits, so a
        // writer mid-batch (or one that crashed there) has lines past the
        // recorded end that are not the store's yet. A line the store never
        // held with committed lines after it (a crash, then an older
        // writer's append) is left out and reported as repairable.
        let mut bytes = std::fs::read(store.jsonl()).unwrap_or_default();
        let tail = store.copy_tail(&bytes)?;
        bytes.truncate(tail.end.min(bytes.len() as u64) as usize);
        for r in tail.uncommitted.iter().rev() {
            let line = bytes[..r.start as usize].iter().filter(|b| **b == b'\n').count() + 1;
            rep.warnings.push(format!("uncommitted: copy line {line} was never committed (a batch killed before its commit); the next write voids it"));
            bytes.drain(r.start as usize..r.end as usize);
        }
        rep.warnings.reverse();
        let text = String::from_utf8_lossy(&bytes).into_owned();
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
    // 4. round trip
    for spec in specs {
        for e in export::round_trip(store, spec)? {
            rep.problems.push(format!("round trip: {e}"));
        }
    }
    drop(snapshot);
    // 3. disk, outside any snapshot
    if let Some(d) = dir {
        disk_check(store, specs, d, &mut rep)?;
    }
    Ok(rep)
}

/// How long a disk mismatch is taken again: past the longest write seen beside
/// this check (4.1 s), between a commit and its export.
const DISK_SETTLE: Duration = Duration::from_secs(6);
/// The pause between two looks at the mismatches left.
const DISK_PAUSE: Duration = Duration::from_millis(100);

/// What is wrong with an export's file on disk, given what it should hold.
fn disk_problem(store: &MemoryStore, spec: &ExportSpec, dir: &Path, rel: &Path, text: &str, empty: &[(PathBuf, String)]) -> Result<Option<String>> {
    let full = dir.join(rel);
    Ok(match std::fs::read(&full) {
        Ok(b) if b == text.as_bytes() => None,
        Ok(b) => Some(format!("disk: {} differs from its export at line {}", full.display(), export::first_diff_line(&b, text.as_bytes()))),
        // An export file that would hold no records (it renders as it does
        // from an empty store) is not missing when atlas never wrote it: a
        // fresh store's exports are written by its first write.
        Err(_) if empty.iter().any(|(r, t)| r == rel && t == text) && !store.exported(&spec.name, rel)? => None,
        Err(_) => Some(format!("disk: {} is missing", full.display())),
    })
}

/// A file's bytes as a stamp, to see whether it changed between two looks.
fn stamp(path: &Path) -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let bytes = std::fs::read(path).ok()?;
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
}

/// The disk check. One pass with no lock, which a quiet store passes at once;
/// then each mismatch again under the write lock, one export at a time, so the
/// lock is never held across a full render of every export, until it clears
/// or [`DISK_SETTLE`] has passed.
///
/// A mismatched file whose bytes change between two looks is being exported to:
/// with writers never pausing, a commit can always be ahead of its export, and an
/// export replaces the whole file, so whatever was wrong is gone. Only a file that
/// stays wrong and stays still is reported.
fn disk_check(store: &MemoryStore, specs: &[ExportSpec], dir: &Path, rep: &mut CheckReport) -> Result<()> {
    type Left = (usize, Vec<PathBuf>, Vec<(PathBuf, String)>);
    let mut left: Vec<Left> = Vec::new();
    let mut found: HashMap<(usize, PathBuf), String> = HashMap::new();
    let mut seen: HashMap<(usize, PathBuf), Option<u64>> = HashMap::new();
    for (i, spec) in specs.iter().enumerate() {
        let empty = export::render(&MemoryStore::in_memory()?, spec)?;
        let mut bad = Vec::new();
        for (rel, text) in export::render(store, spec)? {
            rep.files += 1;
            if let Some(p) = disk_problem(store, spec, dir, &rel, &text, &empty)? {
                found.insert((i, rel.clone()), p);
                seen.insert((i, rel.clone()), stamp(&dir.join(&rel)));
                bad.push(rel);
            }
        }
        if !bad.is_empty() {
            left.push((i, bad, empty));
        }
    }
    let start = Instant::now();
    while !left.is_empty() && start.elapsed() < DISK_SETTLE {
        for (i, bad, empty) in &mut left {
            let spec = &specs[*i];
            // Busy past the store's own timeout: this round says nothing.
            let Ok(locked) = rusqlite::Transaction::new_unchecked(store.conn(), rusqlite::TransactionBehavior::Immediate) else { continue };
            let now: HashMap<PathBuf, String> = export::render(store, spec)?.into_iter().collect();
            let mut still = Vec::new();
            for rel in bad.iter() {
                // A file the render no longer has is not expected any more.
                let Some(text) = now.get(rel) else {
                    found.remove(&(*i, rel.clone()));
                    continue;
                };
                match disk_problem(store, spec, dir, rel, text, empty)? {
                    Some(_) if seen.get(&(*i, rel.clone())) != Some(&stamp(&dir.join(rel))) => {
                        found.remove(&(*i, rel.clone()));
                    }
                    Some(p) => {
                        found.insert((*i, rel.clone()), p);
                        still.push(rel.clone());
                    }
                    None => {
                        found.remove(&(*i, rel.clone()));
                    }
                }
            }
            drop(locked);
            *bad = still;
        }
        left.retain(|(_, bad, _)| !bad.is_empty());
        if !left.is_empty() {
            std::thread::sleep(DISK_PAUSE);
        }
    }
    let mut problems: Vec<((usize, PathBuf), String)> = found.into_iter().collect();
    problems.sort();
    rep.problems.extend(problems.into_iter().map(|(_, p)| p));
    Ok(())
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
/// The record count is compared only when no other connection committed
/// between the live count and the copy (`PRAGMA data_version` unchanged), so
/// a write from another session is never mistaken for a bad copy. A copy
/// that raced a commit is removed and tried again, up to three times; if
/// every try raced, the snapshot is skipped (`Ok(None)`), nothing is
/// recorded, and the next write tries again.
///
/// The file is `memory-YYYYMMDD-HHMMSS.sqlite`, or with `-N` appended when
/// that name is taken.
///
/// # Errors
///
/// [`Error::Db`] or [`Error::Io`]; [`Error::Io`] also when the copy fails
/// verification (the bad copy is removed).
pub fn snapshot(store: &MemoryStore, dir: &Path, cap_bytes: u64) -> Result<Option<Snapshot>> {
    std::fs::create_dir_all(dir)?;
    for _ in 0..SNAPSHOT_TRIES {
        if let Some((path, records)) = snapshot_once(store, dir)? {
            let bytes = std::fs::metadata(&path)?.len();
            let rolled = super::archive::roll_up(dir, cap_bytes, Some(&path))?;
            store.meta_set("last_snapshot", &super::now())?;
            let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            store.meta_set("last_snapshot_epoch", &epoch.to_string())?;
            return Ok(Some(Snapshot { path, bytes, records, rolled }));
        }
    }
    Ok(None)
}

/// Tries per [`snapshot`] call before it gives up until the next write.
const SNAPSHOT_TRIES: usize = 3;

/// One copy: `Some((path, records))` when it verified, `None` when another
/// connection committed during the copy (the copy is removed).
fn snapshot_once(store: &MemoryStore, dir: &Path) -> Result<Option<(PathBuf, i64)>> {
    let stamp: String = super::now().chars().filter(|c| c.is_ascii_digit()).take(14).collect();
    let base = format!("memory-{}-{}", &stamp[..8.min(stamp.len())], &stamp[8.min(stamp.len())..]);
    let mut path = dir.join(format!("{base}.sqlite"));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.sqlite"));
        n += 1;
    }
    let target = path.to_string_lossy().into_owned();
    let version = |c: &rusqlite::Connection| c.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0));
    let before = version(store.conn())?;
    let records: i64 = store.conn().query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
    if let Err(e) = store.conn().execute("VACUUM INTO ?1", [&target]) {
        let _ = std::fs::remove_file(&path);
        return Err(e.into());
    }
    let raced = version(store.conn())? != before;
    let verdict = (|| -> Result<(String, i64)> {
        let copy = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let ic: String = copy.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let copied: i64 = copy.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
        Ok((ic, copied))
    })();
    let (ic, copied) = match verdict {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };
    if ic == "ok" && raced {
        // The copy is one consistent read, but the live count may come from
        // before or after the other commit: try again rather than guess.
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    if ic != "ok" || copied != records {
        let _ = std::fs::remove_file(&path);
        return Err(Error::Io(format!("snapshot {} failed verification: integrity {ic}, {copied} of {records} records (the copy was removed)", path.display())));
    }
    Ok(Some((path, records)))
}
