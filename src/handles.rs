//! The handle store: a large tool or command output kept on disk under a
//! short id (`h:` plus 10 hex characters), so the model gets a digest and
//! pulls slices by handle instead of reading the whole output.
//!
//! On disk: `<data dir>/handles/<sha[0..2]>/<sha>` holds the raw bytes,
//! content-addressed and written as `.part` then renamed. The row lives in
//! the index database's durable `handles` table
//! ([`crate::store::HANDLES_SCHEMA`]).
//!
//! A handle id is a pure function of the bytes, the kind and the command,
//! so the same output stored twice is the same handle (a hook answer is
//! reproducible). A collision with different bytes takes two more hex
//! characters.
//!
//! Every slice is capped (`lean.handles.slice_bytes`, default 8 KB) and
//! ends with a "truncated, N more lines" line when cut. Eviction runs at
//! insert: least recently used beyond `lean.handles.cap_mb` (default 512),
//! and anything unused for `lean.handles.max_age_days` (default 7).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use rusqlite::{params, Connection, OptionalExtension};

use crate::digest::{self, Item, Shape};
use crate::error::{Error, Result};
use crate::json::Value;
use crate::regex::RegexBuilder;

/// Default slice cap in bytes.
pub const DEFAULT_SLICE_BYTES: usize = 8 * 1024;
/// Default store cap in MB.
pub const DEFAULT_CAP_MB: u64 = 512;
/// Default age limit in days.
pub const DEFAULT_MAX_AGE_DAYS: u64 = 7;
/// Default size at which a tool result or command output is swapped for a
/// digest plus handle, in bytes.
pub const DEFAULT_THRESHOLD_BYTES: usize = 12_000;
/// Default swap threshold for cargo build and test output, whose digest
/// keeps every failure and the tail, in bytes.
pub const DEFAULT_BUILD_THRESHOLD_BYTES: usize = 3_000;
/// The largest output stored, in bytes; beyond it the tail is kept.
pub const MAX_CAPTURE: usize = 256 << 20;

/// The handle store's settings (`lean.handles`, `lean.digests`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandleCfg {
    /// Slice cap in bytes.
    pub slice_bytes: usize,
    /// Store cap in bytes.
    pub cap_bytes: u64,
    /// Age limit.
    pub max_age: Duration,
    /// Swap threshold in bytes.
    pub threshold_bytes: usize,
    /// Swap threshold for cargo build and test output, in bytes.
    pub build_threshold_bytes: usize,
    /// Digest budget in bytes.
    pub digest_bytes: usize,
}

impl Default for HandleCfg {
    fn default() -> HandleCfg {
        HandleCfg {
            slice_bytes: DEFAULT_SLICE_BYTES,
            cap_bytes: DEFAULT_CAP_MB << 20,
            max_age: Duration::from_secs(DEFAULT_MAX_AGE_DAYS * 86_400),
            threshold_bytes: DEFAULT_THRESHOLD_BYTES,
            build_threshold_bytes: DEFAULT_BUILD_THRESHOLD_BYTES,
            digest_bytes: digest::DEFAULT_MAX_BYTES,
        }
    }
}

impl HandleCfg {
    /// Reads `lean.handles.{slice_bytes, cap_mb, max_age_days,
    /// threshold_bytes, build_threshold_bytes}` and `lean.digests.max_bytes`; absent or malformed
    /// values keep their defaults.
    pub fn from_lean(lean: Option<&Value>) -> HandleCfg {
        let mut c = HandleCfg::default();
        let num = |sec: &str, k: &str| lean.and_then(|l| l.get(sec)).and_then(|h| h.get(k)).and_then(Value::as_u64);
        if let Some(v) = num("handles", "slice_bytes") {
            c.slice_bytes = (v as usize).clamp(256, 1 << 20);
        }
        if let Some(v) = num("handles", "cap_mb") {
            c.cap_bytes = v.clamp(1, 1 << 20) << 20;
        }
        if let Some(v) = num("handles", "max_age_days") {
            c.max_age = Duration::from_secs(v.clamp(1, 3650) * 86_400);
        }
        if let Some(v) = num("handles", "threshold_bytes") {
            c.threshold_bytes = (v as usize).clamp(1024, 16 << 20);
        }
        if let Some(v) = num("handles", "build_threshold_bytes") {
            c.build_threshold_bytes = (v as usize).clamp(1024, 16 << 20);
        }
        if let Some(v) = num("digests", "max_bytes") {
            c.digest_bytes = (v as usize).clamp(200, 64 << 10);
        }
        c
    }
}

/// What a stored output was.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// `run` (atlas run), `bash`, `read`, `grep` or `glob`.
    pub kind: String,
    /// What a digest header names (the command, or a path).
    pub label: String,
    /// The command, or the tool input that produced it.
    pub cmd: String,
    /// The working directory.
    pub cwd: String,
    /// The session or subagent.
    pub session: String,
    /// The exit code, when known.
    pub exit_code: Option<i64>,
    /// Wall time in milliseconds, when known.
    pub ms: Option<i64>,
}

/// A stored output's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// `h:` plus hex.
    pub id: String,
    /// SHA-256 of the bytes.
    pub sha: String,
    /// Bytes.
    pub bytes: u64,
    /// Lines.
    pub lines: u64,
    /// What it was.
    pub meta: Meta,
    /// UTC creation time.
    pub created: String,
}

/// A slice selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sel {
    /// Lines `a..=b`, 1-based.
    Lines(usize, usize),
    /// Lines matching a pattern, with context lines around each.
    Grep {
        /// The pattern (the owned regex subset).
        re: String,
        /// Context lines.
        context: usize,
    },
    /// One item by name (a test, `error:2`, a file, `Type::fn`).
    Item(String),
    /// Every failure block.
    Failures,
    /// The first n lines.
    Head(usize),
    /// The last n lines.
    Tail(usize),
    /// A PR diff file part's default read from line k (its `next page`),
    /// any other output's lines from k.
    Page(usize),
    /// No selector: a PR diff file part read diff-native and paged at hunk
    /// boundaries ([`crate::pr_digest::file_slice`]), any other output's
    /// first 40 lines.
    Whole,
}

/// The handle store.
pub struct Handles {
    conn: Connection,
    dir: PathBuf,
    /// The settings it runs with.
    pub cfg: HandleCfg,
}

fn now() -> String {
    crate::store::utc(SystemTime::now())
}

impl Handles {
    /// Opens the store beside the index database `db`, creating the table
    /// and the folder as needed.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] from SQLite, [`Error::Io`] for the folder.
    pub fn open(db: &Path, cfg: HandleCfg) -> Result<Handles> {
        if let Some(p) = db.parent() {
            std::fs::create_dir_all(p).map_err(|e| Error::Io(format!("{}: {e}", p.display())))?;
        }
        let conn = crate::store::open_raw(db)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute_batch(crate::store::HANDLES_SCHEMA)?;
        let dir = db.parent().unwrap_or(Path::new(".")).join("handles");
        Ok(Handles { conn, dir, cfg })
    }

    /// The index database connection (for the token ledger and misses).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    fn blob(&self, sha: &str) -> PathBuf {
        self.dir.join(&sha[..2]).join(sha)
    }

    /// Stores an output and returns its row; the same bytes, kind and
    /// command give the same id.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] writing the file, [`Error::Db`] from SQLite.
    pub fn put(&mut self, data: &[u8], m: &Meta) -> Result<Row> {
        let data = if data.len() > MAX_CAPTURE { &data[data.len() - MAX_CAPTURE..] } else { data };
        let sha = crate::sha256::hex(data);
        let path = self.blob(&sha);
        if !path.is_file() {
            let parent = path.parent().unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(format!("{}: {e}", parent.display())))?;
            let part = path.with_extension(format!("{}.part", std::process::id()));
            std::fs::write(&part, data).map_err(|e| Error::Io(format!("{}: {e}", part.display())))?;
            if let Err(e) = std::fs::rename(&part, &path) {
                let _ = std::fs::remove_file(&part);
                if !path.is_file() {
                    return Err(Error::Io(format!("{}: {e}", path.display())));
                }
            }
        }
        let text = String::from_utf8_lossy(data);
        let lines = digest::line_count(&text) as u64;
        let key = crate::sha256::hex(format!("{sha}|{}|{}", m.kind, m.cmd).as_bytes());
        let t = now();
        let id = crate::store::guarded(&self.conn, || {
            for n in (10..=64).step_by(2) {
                let cand = format!("h:{}", &key[..n]);
                let have: Option<String> = self.conn.query_row("SELECT sha FROM handles WHERE id = ?1", [&cand], |r| r.get(0)).optional()?;
                match have {
                    None => {
                        self.conn.execute(
                            "INSERT INTO handles(id, sha, kind, label, cmd, cwd, session, created, last_used, bytes, lines, exit_code, ms)
                             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10, ?11, ?12)",
                            params![cand, sha, m.kind, m.label, m.cmd, m.cwd, m.session, t, data.len() as i64, lines as i64, m.exit_code, m.ms],
                        )?;
                        return Ok(cand);
                    }
                    Some(s) if s == sha => {
                        self.conn.execute("UPDATE handles SET last_used = ?2 WHERE id = ?1", params![cand, t])?;
                        return Ok(cand);
                    }
                    Some(_) => continue,
                }
            }
            Ok(String::new())
        })?;
        if !id.is_empty() && !m.session.is_empty() {
            crate::store::guarded(&self.conn, || {
                self.conn.execute("INSERT OR IGNORE INTO handle_sessions(id, session) VALUES(?1, ?2)", params![id, m.session])?;
                Ok(())
            })?;
        }
        if id.is_empty() {
            return Err(Error::Db("handle id space exhausted".into()));
        }
        self.evict()?;
        self.row(&id)
    }

    /// A handle's row.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown id.
    pub fn row(&self, id: &str) -> Result<Row> {
        let id = if id.starts_with("h:") { id.to_string() } else { format!("h:{id}") };
        self.conn
            .query_row(
                "SELECT id, sha, kind, label, cmd, cwd, session, created, bytes, lines, exit_code, ms FROM handles WHERE id = ?1",
                [&id],
                |r| {
                    Ok(Row {
                        id: r.get(0)?,
                        sha: r.get(1)?,
                        meta: Meta {
                            kind: r.get(2)?,
                            label: r.get(3)?,
                            cmd: r.get(4)?,
                            cwd: r.get(5)?,
                            session: r.get(6)?,
                            exit_code: r.get(10)?,
                            ms: r.get(11)?,
                        },
                        created: r.get(7)?,
                        bytes: r.get::<_, i64>(8)? as u64,
                        lines: r.get::<_, i64>(9)? as u64,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("handle {id} (evicted, or from another machine)")))
    }

    /// Whether `session` (a session or subagent id) already holds a read
    /// handle for `path`: a whole Read of it again is the agent coming back
    /// for the text, so it passes whole.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] from SQLite.
    pub fn has_read(&self, path: &str, session: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM handles h WHERE h.kind = 'read' AND h.cmd = ?1
                 AND (h.session = ?2 OR EXISTS(SELECT 1 FROM handle_sessions s WHERE s.id = h.id AND s.session = ?2)) LIMIT 1",
                params![path, session],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// A handle's row and text; marks it used.
    ///
    /// # Errors
    ///
    /// As [`Handles::row`], and [`Error::Io`] when the file is gone.
    pub fn text(&self, id: &str) -> Result<(Row, String)> {
        let row = self.row(id)?;
        let b = std::fs::read(self.blob(&row.sha)).map_err(|e| Error::NotFound(format!("handle {} file: {e}", row.id)))?;
        crate::store::guarded(&self.conn, || Ok(self.conn.execute("UPDATE handles SET last_used = ?2 WHERE id = ?1", params![row.id, now()])?))?;
        Ok((row, String::from_utf8_lossy(&b).into_owned()))
    }

    /// The shape a row's output is digested and sliced as.
    pub fn shape_of(row: &Row, text: &str) -> Shape {
        digest::shape(&row.meta.kind, &row.meta.cmd, text)
    }

    /// The digest of a stored output.
    ///
    /// # Errors
    ///
    /// As [`Handles::text`].
    pub fn digest(&self, id: &str) -> Result<String> {
        let (row, text) = self.text(id)?;
        let shape = Handles::shape_of(&row, &text);
        Ok(digest::digest(&row.id, &row.meta.label, shape, &text, row.meta.exit_code.map(|c| c as i32), self.cfg.digest_bytes))
    }

    /// One slice of a stored output, capped at `slice_bytes`, its lines
    /// numbered as in the output.
    ///
    /// # Errors
    ///
    /// As [`Handles::text`]; [`Error::InvalidArgument`] for a bad pattern.
    pub fn slice(&self, id: &str, sel: &Sel) -> Result<String> {
        let (row, text) = self.text(id)?;
        if row.meta.kind == crate::pr_digest::KIND {
            // A PR diff's file part: its stored bytes, without line numbers.
            use crate::pr_digest::FileSel;
            let from = match sel {
                Sel::Whole => Some(FileSel::View(1)),
                Sel::Page(k) => Some(FileSel::View(*k)),
                Sel::Lines(a, b) => Some(FileSel::Raw(*a, *b)),
                _ => None,
            };
            let backed = crate::pr_digest::row_backed(&row.meta);
            if let Some(s) = from.and_then(|f| crate::pr_digest::file_slice(&row.id, &text, f, self.cfg.slice_bytes, backed)) {
                return Ok(s);
            }
        }
        let lines: Vec<&str> = text.lines().collect();
        let n = lines.len();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut note = String::new();
        let mut focus: Option<crate::regex::Regex> = None;
        match sel {
            Sel::Lines(a, b) => {
                let a = (*a).max(1);
                let b = (*b).min(n);
                if a <= b {
                    ranges.push((a, b));
                }
            }
            Sel::Head(k) => ranges.push((1, (*k).min(n))),
            Sel::Whole => ranges.push((1, 40.min(n))),
            Sel::Tail(k) => ranges.push((n.saturating_sub(*k) + 1, n)),
            Sel::Page(k) => {
                if (*k).max(1) <= n {
                    ranges.push(((*k).max(1), n));
                }
            }
            Sel::Grep { re, context } => {
                let r = RegexBuilder::new(re).build().map_err(|e| Error::InvalidArgument(format!("pattern {re:?}: {e}")))?;
                for (i, l) in lines.iter().enumerate() {
                    r.check_text(l.len()).map_err(|e| Error::InvalidArgument(format!("pattern {re:?} on line {}: {e}", i + 1)))?;
                    if r.is_match(l) {
                        let a = (i + 1).saturating_sub(*context).max(1);
                        let b = (i + 1 + context).min(n);
                        match ranges.last_mut() {
                            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
                            _ => ranges.push((a, b)),
                        }
                    }
                }
                focus = Some(r);
            }
            Sel::Item(name) => {
                let items = digest::items(Handles::shape_of(&row, &text), &text);
                let found = find_items(&items, name);
                match found.len() {
                    0 => {
                        let mut names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
                        names.dedup();
                        let some: Vec<&str> = names.into_iter().take(12).collect();
                        return Ok(format!("{}: no item {name:?}; items include: {}\n", row.id, some.join(", ")));
                    }
                    1 => ranges.push((found[0].first, found[0].last)),
                    _ => {
                        // A name for both a failure line and its output block
                        // answers the block; otherwise every match.
                        if let Some(b) = found.iter().find(|i| i.status == "failed-output") {
                            ranges.push((b.first, b.last));
                        } else {
                            note = format!("{} items named {name:?}\n", found.len());
                            for f in found.iter().take(8) {
                                ranges.push((f.first, f.last));
                            }
                        }
                    }
                }
            }
            Sel::Failures => {
                let items = digest::items(Handles::shape_of(&row, &text), &text);
                for it in items.iter().filter(|i| i.status == "failed-output" || i.status == "error") {
                    ranges.push((it.first, it.last));
                }
                if ranges.is_empty() {
                    return Ok(format!("{}: no failure blocks\n", row.id));
                }
            }
        }
        if ranges.is_empty() {
            return Ok(format!("{}: nothing selected ({n} lines)\n", row.id));
        }
        ranges.sort_unstable();
        Ok(render(&row.id, &lines, &ranges, &note, self.cfg.slice_bytes, focus.as_ref()))
    }

    /// The most recently used handles, newest first.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] from SQLite.
    pub fn list(&self, limit: usize) -> Result<Vec<Row>> {
        let ids: Vec<String> = {
            let mut st = self.conn.prepare("SELECT id FROM handles ORDER BY last_used DESC, id LIMIT ?1")?;
            let v = st.query_map([limit as i64], |r| r.get(0))?.collect::<std::result::Result<Vec<String>, _>>()?;
            v
        };
        ids.iter().map(|i| self.row(i)).collect()
    }

    /// Removes handles unused for longer than the age limit, then the least
    /// recently used beyond the size cap. Returns how many went.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] from SQLite.
    pub fn evict(&mut self) -> Result<usize> {
        let cutoff = crate::store::utc(SystemTime::now().checked_sub(self.cfg.max_age).unwrap_or(SystemTime::UNIX_EPOCH));
        let mut gone: Vec<(String, String)> = Vec::new();
        {
            let mut st = self.conn.prepare("SELECT id, sha, bytes FROM handles ORDER BY last_used DESC, id")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
            let mut total: u64 = 0;
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for r in rows {
                let (id, sha, bytes) = r?;
                if seen.insert(sha.clone()) {
                    total += bytes as u64;
                }
                if total > self.cfg.cap_bytes {
                    gone.push((id, sha));
                }
            }
        }
        {
            let mut st = self.conn.prepare("SELECT id, sha FROM handles WHERE last_used < ?1")?;
            let old = st.query_map([&cutoff], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for r in old {
                let x = r?;
                if !gone.contains(&x) {
                    gone.push(x);
                }
            }
        }
        if gone.is_empty() {
            return Ok(0);
        }
        // One guarded transaction for every delete; a blob no handle still
        // names is removed only after it commits.
        let orphans: Vec<&str> = crate::store::guarded(&self.conn, || {
            let mut orphans = Vec::new();
            for (id, sha) in &gone {
                self.conn.execute("DELETE FROM handles WHERE id = ?1", [id])?;
                self.conn.execute("DELETE FROM handle_sessions WHERE id = ?1", [id])?;
                let left: i64 = self.conn.query_row("SELECT COUNT(*) FROM handles WHERE sha = ?1", [sha], |r| r.get(0))?;
                if left == 0 && !orphans.contains(&sha.as_str()) {
                    orphans.push(sha.as_str());
                }
            }
            Ok(orphans)
        })?;
        for sha in orphans {
            let _ = std::fs::remove_file(self.blob(sha));
        }
        Ok(gone.len())
    }
}

/// Items matching a name: exact first, then a `::`, `.` or `/` suffix.
fn find_items<'a>(items: &'a [Item], name: &str) -> Vec<&'a Item> {
    let exact: Vec<&Item> = items.iter().filter(|i| i.name == name).collect();
    if !exact.is_empty() {
        return exact;
    }
    items
        .iter()
        .filter(|i| {
            i.name.ends_with(&format!("::{name}")) || i.name.ends_with(&format!(".{name}")) || i.name.replace('\\', "/").ends_with(&format!("/{name}"))
        })
        .collect()
}

/// The largest index at most `i` that is a char boundary of `s`.
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The byte range `[s, e)` of a line over the cap that is shown: at most
/// `room` bytes, from the start, or around the first match of `focus` (a
/// `grep` slice) when it matches.
pub fn clip_line(line: &str, room: usize, focus: Option<&crate::regex::Regex>) -> (usize, usize) {
    let from = focus.and_then(|r| r.find(line)).map_or(0, |m| m.start().saturating_sub(room / 4));
    let s = floor_boundary(line, from);
    (s, floor_boundary(line, s + room))
}

/// The note after a line [`clip_line`] cut: its size, the bytes shown, and
/// (unless a pattern already placed the cut) how to see another part.
pub fn long_line_note(id: &str, k: usize, len: usize, (s, e): (usize, usize), focused: bool) -> String {
    if focused {
        format!("line {k} is {len} bytes; bytes {s}-{e} shown, around the first match\n")
    } else {
        format!("line {k} is {len} bytes; bytes {s}-{e} shown (recall {id} grep=RE shows the part around a match)\n")
    }
}

/// Numbered lines of the ranges (sorted by start), with `--` between
/// ranges that do not touch, capped at `cap` bytes with a "truncated" line
/// naming the next range when cut. Overlapping ranges (a test's
/// failure block and a compiler error inside it, both `--failures` items)
/// are shown once. A first line alone over the cap is cut to it
/// ([`clip_line`], around `focus`'s first match) with a
/// [`long_line_note`].
fn render(id: &str, lines: &[&str], ranges: &[(usize, usize)], note: &str, cap: usize, focus: Option<&crate::regex::Regex>) -> String {
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(a, b) in ranges {
        match merged.last_mut() {
            Some(last) if b >= a && last.1 >= last.0 && a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let ranges = &merged[..];
    // An empty range (b < a) counts no lines.
    let total: usize = ranges.iter().map(|(a, b)| (b + 1).saturating_sub(*a)).sum();
    let mut out = String::from(note);
    let mut shown = 0;
    let mut prev_end = 0;
    for &(a, b) in ranges {
        if prev_end != 0 && a > prev_end + 1 {
            out.push_str("--\n");
        }
        for k in a..=b {
            let last = ranges.last().map_or(b, |r| r.1);
            let line = format!("{k}\t{}\n", lines[k - 1]);
            if out.len() + line.len() > cap && shown > 0 {
                let left = total - shown;
                out.push_str(&format!("truncated, {left} more lines (recall {id} lines={k}-{last})\n"));
                return out;
            }
            if out.len() + line.len() > cap {
                // One line alone over the cap: cut it, and say so.
                let text = lines[k - 1];
                let room = cap.saturating_sub(out.len() + format!("{k}\t\n").len()).max(64);
                let (s, e) = clip_line(text, room, focus);
                let focused = focus.is_some_and(|r| r.is_match(text));
                out.push_str(&format!("{k}\t{}\n", &text[s..e]));
                out.push_str(&long_line_note(id, k, text.len(), (s, e), focused));
                shown += 1;
                if total > shown {
                    let next = if k < b { k + 1 } else { ranges.iter().map(|r| r.0).find(|&s| s > k).unwrap_or(last) };
                    out.push_str(&format!("truncated, {} more lines (recall {id} lines={next}-{last})\n", total - shown));
                }
                return out;
            }
            out.push_str(&line);
            shown += 1;
        }
        prev_end = b;
    }
    out
}

/// Parses `A-B`, `A:B` or `A,B` (1-based, inclusive); `A` alone is one line.
fn parse_range(s: &str) -> Option<(usize, usize)> {
    let s = s.trim();
    match s.split_once(['-', ':', ',']) {
        Some((a, b)) => {
            let a = a.trim().parse().ok()?;
            let b = if b.trim().is_empty() { usize::MAX } else { b.trim().parse().ok()? };
            Some((a, b))
        }
        None => s.parse().ok().map(|a| (a, a)),
    }
}

/// The selector arguments `recall` takes with an `h:` key, each one of
/// `atlas slice`'s flags without its dashes.
pub const SLICE_ARGS: &[&str] = &["lines", "item", "failures", "grep", "context", "head", "tail", "page", "digest"];

/// What a `recall h:ID` call asks of a stored output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// One slice.
    Slice(Sel),
    /// The digest again.
    Digest,
}

/// What each value-taking selector's value is, for a refusal.
fn value_of(k: &str) -> &'static str {
    match k {
        "lines" => "A-B",
        "item" => "NAME",
        "grep" => "RE",
        "page" => "K",
        _ => "N",
    }
}

/// Parses `recall`'s selector arguments, as (name, value) pairs in the
/// order given: `lines=A-B`, `item=NAME`, `failures=true`, `grep=RE` (with
/// `context=N`), `head=N`, `tail=N`, `page=K` or `digest=true`. At most one
/// selector; none is [`Sel::Whole`]. One surrounding pair of double quotes
/// around a value is dropped (`grep="^diff --git"` copied whole).
///
/// # Errors
///
/// A message naming the argument that is unknown, repeated, malformed or
/// without its value.
pub fn parse_args(pairs: &[(String, String)]) -> std::result::Result<Ask, String> {
    let flag = |k: &str, v: &str| match v {
        "true" | "" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{k} is true or false, not {v:?}")),
    };
    let mut words: Vec<String> = Vec::new();
    let mut chosen: Vec<&str> = Vec::new();
    let mut digest = false;
    let mut grep = false;
    let mut context = false;
    for (k, v) in pairs {
        let k = k.as_str();
        let v = v.trim();
        let v = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(v);
        if k == "|" {
            return Err("one selector at a time: a hint's `|` separates alternatives, so pick one (key \"h:ID lines=A-B\")".into());
        }
        if matches!(k, "context" | "lines" | "item" | "grep" | "head" | "tail" | "page") && v.is_empty() {
            return Err(format!("{k} needs a value: {k}={}", value_of(k)));
        }
        match k {
            "failures" | "digest" => {
                if !flag(k, v)? {
                    continue;
                }
                if k == "digest" {
                    digest = true;
                } else {
                    words.push("--failures".into());
                }
            }
            "context" => {
                context = true;
                words.extend(["--context".to_string(), v.to_string()]);
                continue;
            }
            "lines" | "item" | "grep" | "head" | "tail" | "page" => {
                grep |= k == "grep";
                words.extend([format!("--{k}"), v.to_string()]);
            }
            _ => return Err(format!("unknown selector {k:?}; use one of {}", SLICE_ARGS.join(", "))),
        }
        chosen.push(k);
    }
    if chosen.len() > 1 {
        return Err(format!("one selector at a time, not {}", chosen.join(" and ")));
    }
    if context && !grep {
        return Err("context goes with grep".into());
    }
    if digest {
        return Ok(Ask::Digest);
    }
    // The CLI's messages name its flags; these name the arguments.
    parse_sel(&words).map(Ask::Slice).map_err(|e| e.strip_prefix("--").map_or(e.clone(), str::to_string))
}

/// The handle form of a `recall` key: the key from `h:` on, when it is one,
/// also as a hint line or a command reads (`recall h:ID ...`,
/// `recall key=h:ID ...`, `atlas slice h:ID ...`).
pub fn handle_key(key: &str) -> Option<&str> {
    let mut k = key.trim();
    for p in ["recall ", "atlas slice "] {
        if let Some(r) = k.strip_prefix(p) {
            k = r.trim_start();
        }
    }
    let k = k.strip_prefix("key=").unwrap_or(k);
    k.starts_with("h:").then_some(k)
}

/// Splits a `recall` key that carries its selectors after the id, as a hint
/// line writes them (`h:ID lines=A-B`, `h:ID grep="a b"`, `h:ID failures`)
/// or as the CLI's flags (`h:ID --lines A-B`), into the id and (name, value)
/// pairs for [`parse_args`]. Double quotes group words. A bare
/// value-taking name (`h:ID grep`) has the empty value [`parse_args`]
/// refuses; a hint's ` | ` between alternatives is kept as a `|` pair, which
/// it refuses too.
///
/// # Errors
///
/// A message for a flag without its value or an unclosed quote.
pub fn split_key(key: &str) -> std::result::Result<(String, Vec<(String, String)>), String> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut any) = (false, false);
    for c in key.trim().chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if quoted {
        return Err(format!("key {key:?}: unclosed quote"));
    }
    if any {
        words.push(cur);
    }
    let mut it = words.into_iter();
    let id = it.next().unwrap_or_default();
    let mut pairs = Vec::new();
    while let Some(w) = it.next() {
        if let Some(name) = w.strip_prefix("--") {
            let name = if name == "symbol" { "item" } else { name };
            let v = if matches!(name, "failures" | "digest") { "true".to_string() } else { it.next().ok_or_else(|| format!("{w} needs a value"))? };
            pairs.push((name.to_string(), v));
        } else if let Some((k, v)) = w.split_once('=') {
            pairs.push((k.to_string(), v.to_string()));
        } else if matches!(w.as_str(), "failures" | "digest") {
            pairs.push((w, "true".to_string()));
        } else {
            pairs.push((w, String::new()));
        }
    }
    Ok((id, pairs))
}

/// Parses the selector words of `atlas slice` (`recall`'s arguments go
/// through [`parse_args`] to here).
///
/// # Errors
///
/// A message naming the bad word.
pub fn parse_sel(words: &[String]) -> std::result::Result<Sel, String> {
    let mut it = words.iter();
    let mut sel: Option<Sel> = None;
    let mut context = 2usize;
    while let Some(w) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| format!("{w} needs a value"));
        match w.as_str() {
            "--lines" | "-l" => {
                let v = val()?;
                let (a, b) = parse_range(&v).ok_or_else(|| format!("--lines {v:?}: expected A-B"))?;
                if b < a {
                    return Err(format!("--lines {v:?}: the range runs backwards (A must be at most B)"));
                }
                sel = Some(Sel::Lines(a, b));
            }
            "--item" | "--symbol" | "-i" => sel = Some(Sel::Item(val()?)),
            "--failures" | "-f" => sel = Some(Sel::Failures),
            "--grep" | "-g" => sel = Some(Sel::Grep { re: val()?, context: 0 }),
            "--context" | "-C" => context = val()?.parse().map_err(|_| "--context needs a number".to_string())?,
            "--head" => sel = Some(Sel::Head(val()?.parse().map_err(|_| "--head needs a number".to_string())?)),
            "--tail" => sel = Some(Sel::Tail(val()?.parse().map_err(|_| "--tail needs a number".to_string())?)),
            "--page" => sel = Some(Sel::Page(val()?.parse().map_err(|_| "--page needs a number".to_string())?)),
            o => return Err(format!("unknown selector {o:?}")),
        }
    }
    Ok(match sel {
        Some(Sel::Grep { re, .. }) => Sel::Grep { re, context: context.min(20) },
        Some(s) => s,
        None => Sel::Whole,
    })
}

/// How hint lines name a slice. Every hint is written once, as `recall`'s
/// arguments; [`to_cli`] is the only other form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintForm {
    /// `recall h:ID lines=A-B`: the tool a model calls (output routed to a
    /// model: a hook, or `atlas run` with its output captured).
    Recall,
    /// `atlas slice h:ID --lines A-B`: the command a person at a terminal
    /// types.
    Cli,
}

impl HintForm {
    /// The form for this process's standard output: the command on a
    /// terminal, `recall` otherwise.
    pub fn for_stdout() -> HintForm {
        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            HintForm::Cli
        } else {
            HintForm::Recall
        }
    }

    /// `text` with its hints in this form.
    pub fn apply(self, text: &str) -> String {
        match self {
            HintForm::Recall => text.to_string(),
            HintForm::Cli => to_cli(text),
        }
    }
}

/// Rewrites every `recall h:ID sel=v | sel=v` in `text` as the CLI's
/// `atlas slice h:ID --sel v | --sel v` (`failures=true` and `digest=true`
/// as bare flags). A hint ends at the first word that is not a selector,
/// or at `)`, `;` or `,`.
pub fn to_cli(text: &str) -> String {
    const PAT: &str = "recall h:";
    let stop = |c: char| c.is_whitespace() || matches!(c, ')' | ';' | ',');
    let mut out = String::with_capacity(text.len() + 64);
    let mut rest = text;
    while let Some(i) = rest.find(PAT) {
        out.push_str(&rest[..i]);
        out.push_str("atlas slice ");
        let s = &rest[i + "recall ".len()..];
        let id_end = s.find(stop).unwrap_or(s.len());
        out.push_str(&s[..id_end]);
        let mut s = &s[id_end..];
        let mut first = true;
        loop {
            let (sep, after) = if let Some(a) = s.strip_prefix(" | ").filter(|_| !first) {
                (" | ", a)
            } else if let Some(a) = s.strip_prefix(' ') {
                (" ", a)
            } else {
                break;
            };
            let Some((name, v)) = after.split_once('=') else { break };
            if !SLICE_ARGS.contains(&name) {
                break;
            }
            let v_end =
                if let Some(q) = v.strip_prefix('"') { q.find('"').map_or(v.len(), |e| e + 2) } else { v.find(stop).unwrap_or(v.len()) };
            let value = &v[..v_end];
            out.push_str(sep);
            if matches!(name, "failures" | "digest") && value == "true" {
                out.push_str(&format!("--{name}"));
            } else {
                out.push_str(&format!("--{name} {value}"));
            }
            s = &v[v_end..];
            first = false;
        }
        rest = s;
    }
    out.push_str(rest);
    out
}

/// The usage of `atlas slice`.
pub const SLICE_USAGE: &str = "usage: atlas slice h:ID [--lines A-B | --item NAME | --failures | --grep RE [--context N] | --head N | --tail N | --page K | --digest]\n       atlas slice --list [N]";

/// The `atlas slice` subcommand: one slice of a stored output.
///
/// # Arguments
///
/// * `args` - the words after `slice`.
/// * `db` - the index database; the handle store lives beside it.
/// * `lean` - the configuration's `lean` section, if any.
///
/// # Errors
///
/// A message for the user when the subcommand cannot run.
pub fn cli(args: Vec<String>, db: &std::path::Path, lean: Option<&crate::json::Value>) -> std::result::Result<ExitCode, String> {
    let Some(first) = args.first() else { return Err(SLICE_USAGE.into()) };
    let h = Handles::open(db, HandleCfg::from_lean(lean)).map_err(|e| e.to_string())?;
    if first == "--list" {
        let n = args.get(1).and_then(|n| n.parse().ok()).unwrap_or(20);
        for r in h.list(n).map_err(|e| e.to_string())? {
            let label: String = r.meta.label.chars().take(80).collect();
            println!("{} {} {} lines {} B {label}", r.id, r.meta.kind, r.lines, r.bytes);
        }
        return Ok(ExitCode::SUCCESS);
    }
    if first == "-h" || first == "--help" {
        println!("{SLICE_USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let rest = &args[1..];
    let out = if rest.iter().any(|w| w == "--digest") {
        h.digest(first).map_err(|e| e.to_string())?
    } else {
        let sel = parse_sel(rest)?;
        h.slice(first, &sel).map_err(|e| e.to_string())?
    };
    print!("{}", HintForm::for_stdout().apply(&out));
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backwards_line_range_is_refused() {
        let words = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        let e = parse_sel(&words("--lines 5-1")).unwrap_err();
        assert!(e.contains("runs backwards"), "{e}");
        assert!(parse_sel(&words("--lines 1-5")).is_ok());
        assert!(parse_sel(&words("--lines 5-5")).is_ok());
    }

    #[test]
    fn recall_arguments_parse_as_the_cli_flags() {
        let p = |v: &[(&str, &str)]| parse_args(&v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>());
        assert_eq!(p(&[]), Ok(Ask::Slice(Sel::Whole)));
        assert_eq!(p(&[("lines", "3-9")]), Ok(Ask::Slice(Sel::Lines(3, 9))));
        assert_eq!(p(&[("grep", "x"), ("context", "3")]), Ok(Ask::Slice(Sel::Grep { re: "x".into(), context: 3 })));
        assert_eq!(p(&[("failures", "true")]), Ok(Ask::Slice(Sel::Failures)));
        assert_eq!(p(&[("failures", "false")]), Ok(Ask::Slice(Sel::Whole)));
        assert_eq!(p(&[("digest", "true")]), Ok(Ask::Digest));
        assert!(p(&[("lines", "1-2"), ("tail", "3")]).unwrap_err().contains("lines and tail"));
        assert!(p(&[("context", "3")]).unwrap_err().contains("grep"));
        assert!(p(&[("lines", "5-1")]).unwrap_err().starts_with("lines \"5-1\""));
        assert!(p(&[("nope", "1")]).unwrap_err().contains("nope"));
        let (id, pairs) = split_key("h:ab grep=\"a b\" --lines 2-3 failures").unwrap();
        assert_eq!(id, "h:ab");
        let want: Vec<(String, String)> = [("grep", "a b"), ("lines", "2-3"), ("failures", "true")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert_eq!(pairs, want);
        assert!(split_key("h:ab --item").is_err());
        assert!(split_key("h:ab grep=\"x").is_err());
    }

    #[test]
    fn hints_read_as_the_cli_on_a_terminal() {
        assert_eq!(
            to_cli("recall h:1 failures=true | item=NAME | lines=A-B | grep=RE\n"),
            "atlas slice h:1 --failures | --item NAME | --lines A-B | --grep RE\n"
        );
        assert_eq!(to_cli("build: 2 errors (recall h:2 item=error:1)\n"), "build: 2 errors (atlas slice h:2 --item error:1)\n");
        assert_eq!(
            to_cli("a file: recall h:F; a hunk: recall h:F lines=A-B; in a file read = N hidden lines=A-B is context; whole output h:9\n"),
            "a file: atlas slice h:F; a hunk: atlas slice h:F --lines A-B; in a file read = N hidden lines=A-B is context; whole output h:9\n"
        );
        assert_eq!(to_cli("3 more files: recall h:3 grep=\"^diff --git\"\n"), "3 more files: atlas slice h:3 --grep \"^diff --git\"\n");
        assert_eq!(
            to_cli("line 1 is 9 bytes; bytes 0-4 shown (recall h:4 grep=RE shows the part)\n"),
            "line 1 is 9 bytes; bytes 0-4 shown (atlas slice h:4 --grep RE shows the part)\n"
        );
        assert_eq!(to_cli("no hint; recall {key} is a record\n"), "no hint; recall {key} is a record\n");
    }

    #[test]
    fn a_line_over_the_cap_is_cut_around_the_match() {
        let long = format!("{}NEEDLE{}", "a".repeat(5000), "b".repeat(5000));
        let refs = [long.as_str(), "next"];
        let out = render("h:1", &refs, &[(1, 2)], "", 1000, None);
        assert!(
            out.starts_with("1\taaa") && out.contains("line 1 is 10006 bytes; bytes 0-") && out.contains("recall h:1 grep=RE"),
            "{out}"
        );
        assert!(out.trim_end().ends_with("truncated, 1 more lines (recall h:1 lines=2-2)"), "{out}");
        let re = crate::regex::RegexBuilder::new("NEEDLE").build().unwrap();
        let out = render("h:1", &refs, &[(1, 1)], "", 1000, Some(&re));
        assert!(out.contains("NEEDLE") && out.contains("around the first match") && out.len() < 1200, "{out}");
        // A multibyte line is cut on a char boundary.
        let wide = "\u{e9}".repeat(3000);
        let out = render("h:1", &[wide.as_str()], &[(1, 1)], "", 501, None);
        assert!(out.contains("line 1 is 6000 bytes"), "{out}");
    }

    #[test]
    fn ranges_parse() {
        assert_eq!(parse_range("3-9"), Some((3, 9)));
        assert_eq!(parse_range("7"), Some((7, 7)));
        assert_eq!(parse_range("5-"), Some((5, usize::MAX)));
        assert_eq!(parse_range("x"), None);
    }

    #[test]
    fn slices_are_capped_with_a_next_range() {
        let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let out = render("h:1", &refs, &[(1, 100)], "", 200, None);
        assert!(out.starts_with("1\tline 1\n"));
        assert!(out.contains("truncated, "));
        assert!(out.trim_end().ends_with("-100)"));
    }

    #[test]
    fn overlapping_ranges_are_shown_once() {
        let lines: Vec<String> = (1..=12).map(|i| format!("l{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let out = render("h:1", &refs, &[(2, 6), (4, 5), (5, 8), (11, 12)], "", 10_000, None);
        assert_eq!(out, "2\tl2\n3\tl3\n4\tl4\n5\tl5\n6\tl6\n7\tl7\n8\tl8\n--\n11\tl11\n12\tl12\n");
    }
}
