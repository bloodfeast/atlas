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
        let conn = Connection::open(db)?;
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
        let mut id = String::new();
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
                    id = cand;
                    break;
                }
                Some(s) if s == sha => {
                    self.conn.execute("UPDATE handles SET last_used = ?2 WHERE id = ?1", params![cand, t])?;
                    id = cand;
                    break;
                }
                Some(_) => continue,
            }
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

    /// A handle's row and text; marks it used.
    ///
    /// # Errors
    ///
    /// As [`Handles::row`], and [`Error::Io`] when the file is gone.
    pub fn text(&self, id: &str) -> Result<(Row, String)> {
        let row = self.row(id)?;
        let b = std::fs::read(self.blob(&row.sha)).map_err(|e| Error::NotFound(format!("handle {} file: {e}", row.id)))?;
        self.conn.execute("UPDATE handles SET last_used = ?2 WHERE id = ?1", params![row.id, now()])?;
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
        let lines: Vec<&str> = text.lines().collect();
        let n = lines.len();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut note = String::new();
        match sel {
            Sel::Lines(a, b) => {
                let a = (*a).max(1);
                let b = (*b).min(n);
                if a <= b {
                    ranges.push((a, b));
                }
            }
            Sel::Head(k) => ranges.push((1, (*k).min(n))),
            Sel::Tail(k) => ranges.push((n.saturating_sub(*k) + 1, n)),
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
        Ok(render(&row.id, &lines, &ranges, &note, self.cfg.slice_bytes))
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
        for (id, sha) in &gone {
            self.conn.execute("DELETE FROM handles WHERE id = ?1", [id])?;
            let left: i64 = self.conn.query_row("SELECT COUNT(*) FROM handles WHERE sha = ?1", [sha], |r| r.get(0))?;
            if left == 0 {
                let _ = std::fs::remove_file(self.blob(sha));
            }
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

/// Numbered lines of the ranges, capped at `cap` bytes, with a
/// "truncated" line naming the next range when cut.
fn render(id: &str, lines: &[&str], ranges: &[(usize, usize)], note: &str, cap: usize) -> String {
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
            let line = format!("{k}\t{}\n", lines[k - 1]);
            if out.len() + line.len() > cap && shown > 0 {
                let left = total - shown;
                out.push_str(&format!("truncated, {left} more lines (atlas slice {id} --lines {k}-{})\n", ranges.last().map_or(b, |r| r.1)));
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

/// Parses the selector words of `atlas slice` (and the MCP `slice` tool's
/// string form).
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
            o => return Err(format!("unknown selector {o:?}")),
        }
    }
    Ok(match sel {
        Some(Sel::Grep { re, .. }) => Sel::Grep { re, context: context.min(20) },
        Some(s) => s,
        None => Sel::Head(40),
    })
}

const SLICE_USAGE: &str = "usage: atlas slice h:ID [--lines A-B | --item NAME | --failures | --grep RE [--context N] | --head N | --tail N | --digest]\n       atlas slice --list [N]";

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
    print!("{out}");
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
        let out = render("h:1", &refs, &[(1, 100)], "", 200);
        assert!(out.starts_with("1\tline 1\n"));
        assert!(out.contains("truncated, "));
        assert!(out.trim_end().ends_with("-100)"));
    }
}
