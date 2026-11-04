//! Token accounting: for every answer, the bytes atlas returned and the
//! bytes the same question would have cost without atlas, logged to SQLite
//! by a script-like rule, never estimated by a model.
//!
//! The counterfactual for an answer whose question has a literal (the
//! identifier of `lookup`, the text of `search`, the prefix of `ids`) is:
//!
//! 1. **grep**: the bytes `grep -rniF LITERAL` would print over the same
//!    files atlas indexes (every vault note and source file in the `docs`
//!    table): for each matching line, `PATH:LINE:TEXT\n`, where `PATH` is
//!    the root's short name, `/`, and the path below the root.
//! 2. **reading**: for each file grep matched, the bytes a reader then opens:
//!    the whole file when it has at most [`READ_WINDOW`] lines, else an
//!    80-line window centred on each matching line (40 before, 39 after),
//!    overlapping windows merged.
//!
//! Matching is ASCII case-insensitive, line by line, as `grep -i` on UTF-8
//! text. The other read tools that have a grep equivalent use the same two
//! parts ([`Probe`]):
//!
//! * `section` greps its id (or heading) like `lookup`, over the same
//!   source or file when the question names one.
//! * `grep` is its own counterfactual: the pattern, matched by the same
//!   regex engine per line (`grep -rnE`), plus the reads.
//! * `read_note` reads the whole note (grep 0, read the file's bytes).
//! * `recall {query}` greps the query over the export files (the files the
//!   records are written to; the indexed files when there are no exports).
//! * `recall {key}` and `recall {lane}` read the record where it is written,
//!   the rule the hooks use ([`record_read`]): a memory's whole file; any
//!   other record's text plus 40 lines either side of it in its export; for
//!   a lane, every record of the lane.
//!
//! A question with no grep equivalent (`ids` by regex, `misses`,
//! `list_sources`, `journal_status`) and every write (`journal`,
//! `handoff`, `remember`, ...) logs its returned bytes with no
//! counterfactual.
//!
//! Separately, each subagent start logs the "brief without atlas": the
//! bytes of the rulebook a brief would otherwise tell the agent to read in
//! full, at that moment (`brief_log`, see [`log_brief`]). It is reported on
//! its own line and never enters a per-answer ratio.
//!
//! Bytes become tokens by one ratio, `bytes_per_token` in the configuration
//! (default [`DEFAULT_BYTES_PER_TOKEN`]). The ratio is recalibrated by a
//! script that tokenizes a sample of answers and divides, and never set by
//! judgement.
//!
//! The work is done on a background thread fed through a channel, after the
//! answer has been written: logging costs the answer one channel send.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::SystemTime;

use rusqlite::{params, Connection};

use crate::error::Result;

/// The default bytes-per-token ratio.
pub const DEFAULT_BYTES_PER_TOKEN: f64 = 4.0;

/// Lines in the window a reader opens around a match.
pub const READ_WINDOW: usize = 80;

/// The table the ledger writes. Durable: never dropped with the derived
/// tables.
pub const TOKEN_TABLE: &str = "
CREATE TABLE IF NOT EXISTS token_log(
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    day TEXT NOT NULL,
    tool TEXT NOT NULL,
    returned INTEGER NOT NULL,
    grep INTEGER,
    read INTEGER
);
CREATE INDEX IF NOT EXISTS token_log_day ON token_log(day, tool);
CREATE TABLE IF NOT EXISTS hook_inject(
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    session TEXT NOT NULL,
    agent TEXT NOT NULL,
    transcript TEXT NOT NULL,
    hook TEXT NOT NULL,
    key TEXT NOT NULL,
    fts REAL,
    cos REAL,
    bytes INTEGER NOT NULL,
    read INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS inject_select(
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    hook TEXT NOT NULL,
    key TEXT NOT NULL,
    how TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    full INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS brief_log(
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    day TEXT NOT NULL,
    session TEXT NOT NULL,
    agent TEXT NOT NULL,
    bytes INTEGER NOT NULL
);
";

/// What a grep-and-read reader would have done to answer the same question.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Probe {
    /// No grep equivalent (or a write): no counterfactual.
    #[default]
    None,
    /// `grep -rniF TEXT` over the indexed files, or one source, or the
    /// paths below one prefix, plus the reads.
    Literal {
        /// The text sought.
        text: String,
        /// Only this vault or source.
        source: Option<String>,
        /// Only paths starting with this.
        path: Option<String>,
    },
    /// `grep -rnE PATTERN` over the same files, plus the reads.
    Regex {
        /// The pattern ([`crate::regex`] syntax).
        pattern: String,
        /// Only this vault or source.
        source: Option<String>,
        /// Only paths starting with this.
        path: Option<String>,
    },
    /// `grep -rniF NAME` over the code roots' `.rs` and `.wgsl` files.
    Code(String),
    /// `grep -rniF TEXT` over the export files.
    Exports(String),
    /// Reading these records where they are written ([`record_read`]);
    /// `lane:NAME` is a whole lane.
    Records(Vec<String>),
    /// Reading one whole file of a vault or source.
    File {
        /// The vault or source.
        source: String,
        /// The path below its root.
        path: String,
    },
}

/// One answer to account for.
#[derive(Debug, Clone)]
pub struct Record {
    /// The tool (`search` in sections mode is `search`).
    pub tool: String,
    /// Bytes of the answer text.
    pub returned: usize,
    /// The grep-and-read equivalent, if the question has one.
    pub probe: Probe,
}

/// The counterfactual cost of one literal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counterfactual {
    /// Bytes grep prints.
    pub grep: usize,
    /// Bytes a reader then opens.
    pub read: usize,
}

/// Measures the counterfactual of a literal over one file's text.
///
/// # Arguments
///
/// * `label` - the `PATH` grep prints for this file.
/// * `text` - the file's bytes.
/// * `literal` - the text sought, ASCII case-insensitively.
pub fn measure_file(label: &str, text: &[u8], literal: &[u8]) -> Counterfactual {
    if literal.is_empty() {
        return Counterfactual::default();
    }
    let lit: Vec<u8> = literal.iter().map(u8::to_ascii_lowercase).collect();
    measure_lines(label, text, &|line: &[u8]| line.len() >= lit.len() && line.windows(lit.len()).any(|w| w.eq_ignore_ascii_case(&lit)))
}

/// Measures the counterfactual of a line matcher over one file's text: the
/// grep output for each line it accepts (the line without its `\r`), and
/// the reads (see the module docs).
///
/// # Arguments
///
/// * `label` - the `PATH` grep prints for this file.
/// * `text` - the file's bytes.
/// * `hit` - whether a line (without its `\n`) matches.
pub fn measure_lines(label: &str, text: &[u8], hit: &dyn Fn(&[u8]) -> bool) -> Counterfactual {
    let mut grep = 0usize;
    // (start byte, end byte) of every line, and which lines matched.
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut hits: Vec<usize> = Vec::new();
    let mut start = 0usize;
    for (i, line) in text.split(|&b| b == b'\n').enumerate() {
        let end = start + line.len();
        if start == text.len() && line.is_empty() {
            break;
        }
        lines.push((start, (end + 1).min(text.len())));
        if hit(line) {
            let body = line.strip_suffix(b"\r").unwrap_or(line);
            grep += label.len() + 1 + (i + 1).to_string().len() + 1 + body.len() + 1;
            hits.push(i);
        }
        start = end + 1;
    }
    if hits.is_empty() {
        return Counterfactual::default();
    }
    let read = if lines.len() <= READ_WINDOW {
        text.len()
    } else {
        let mut total = 0usize;
        let mut covered_to = 0usize; // first line not yet counted
        for &h in &hits {
            let lo = h.saturating_sub(READ_WINDOW / 2).max(covered_to);
            let hi = (h + READ_WINDOW / 2).min(lines.len());
            if lo < hi {
                total += lines[hi - 1].1 - lines[lo].0;
                covered_to = hi;
            }
        }
        total
    };
    Counterfactual { grep, read }
}

/// The ledger: a channel into a worker thread that measures and logs.
#[derive(Debug)]
pub struct Ledger {
    tx: Option<mpsc::Sender<Record>>,
    worker: Option<JoinHandle<()>>,
    code: std::sync::Arc<std::sync::Mutex<Vec<(String, PathBuf)>>>,
    memory: std::sync::Arc<std::sync::Mutex<Option<MemoryPaths>>>,
}

/// Where the memory store and its exports are, for the `recall`
/// counterfactuals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPaths {
    /// The store file.
    pub store: PathBuf,
    /// The export folder, if exports are on.
    pub export_dir: Option<PathBuf>,
}

impl Ledger {
    /// Starts the worker.
    ///
    /// # Arguments
    ///
    /// * `db` - the database file; the worker opens its own connection.
    /// * `roots` - every vault and source short name with its root folder.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`] if the table cannot be made.
    pub fn start(db: &Path, roots: Vec<(String, PathBuf)>) -> Result<Ledger> {
        let conn = Connection::open(db)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(TOKEN_TABLE)?;
        let (tx, rx) = mpsc::channel::<Record>();
        let roots: HashMap<String, PathBuf> = roots.into_iter().collect();
        let code = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let code_roots = code.clone();
        let memory = std::sync::Arc::new(std::sync::Mutex::new(None::<MemoryPaths>));
        let memory_paths = memory.clone();
        let worker = std::thread::Builder::new()
            .name("atlas-tokens".into())
            .spawn(move || {
                // The memory store, opened on the first record read.
                let mut mem: Option<(PathBuf, crate::memory::MemoryStore)> = None;
                for r in rx {
                    let paths = || memory_paths.lock().ok().and_then(|g| g.clone());
                    let cf = match &r.probe {
                        Probe::None => None,
                        Probe::Code(l) => Some(measure_code(&code_roots.lock().map(|g| g.clone()).unwrap_or_default(), l)),
                        Probe::Literal { text, source, path } => {
                            let lit = text.as_bytes();
                            (!lit.is_empty()).then(|| measure_all(&conn, &roots, source.as_deref(), path.as_deref(), &|label, t| measure_file(label, t, lit)))
                        }
                        Probe::Regex { pattern, source, path } => crate::regex::Regex::new(pattern).ok().map(|re| {
                            let hit = |line: &[u8]| std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)).is_ok_and(|s| re.is_match(s));
                            measure_all(&conn, &roots, source.as_deref(), path.as_deref(), &|label, t| measure_lines(label, t, &hit))
                        }),
                        Probe::File { source, path } => roots.get(source).and_then(|root| std::fs::metadata(root.join(path)).ok()).map(|m| Counterfactual { grep: 0, read: m.len() as usize }),
                        Probe::Exports(text) => match paths().and_then(|p| p.export_dir) {
                            Some(dir) => Some(measure_exports(&dir, text)),
                            None => Some(measure_all(&conn, &roots, None, None, &|label, t| measure_file(label, t, text.as_bytes()))),
                        },
                        Probe::Records(keys) => match paths() {
                            None => None,
                            Some(p) => {
                                if mem.as_ref().is_none_or(|(at, _)| *at != p.store) {
                                    mem = crate::memory::MemoryStore::open(&p.store).ok().map(|m| (p.store.clone(), m));
                                }
                                mem.as_ref().map(|(_, m)| Counterfactual { grep: 0, read: keys.iter().map(|k| record_read(m, k)).sum() })
                            }
                        },
                    };
                    let _ = log(&conn, &r, cf);
                }
            })
            .ok();
        Ok(Ledger { tx: Some(tx), worker, code, memory })
    }

    /// Sets where the memory store and its exports are, for the `recall`
    /// counterfactuals; without it a `recall` by key or lane logs none.
    pub fn set_memory(&self, paths: MemoryPaths) {
        if let Ok(mut g) = self.memory.lock() {
            *g = Some(paths);
        }
    }

    /// Sets the code roots a `symbol` answer's counterfactual greps.
    pub fn set_code_roots(&self, roots: Vec<(String, PathBuf)>) {
        if let Ok(mut g) = self.code.lock() {
            *g = roots;
        }
    }

    /// Queues one answer for accounting; never blocks on the measurement.
    pub fn record(&self, r: Record) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(r);
        }
    }
}

impl Drop for Ledger {
    fn drop(&mut self) {
        // Closing the channel ends the worker once it has logged the queue.
        self.tx.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// The counterfactual of a literal over the `.rs` and `.wgsl` files of the
/// code roots (what `grep -rniF` over them prints, plus the reads).
///
/// # Arguments
///
/// * `roots` - code root names and folders.
/// * `literal` - the text sought.
pub fn measure_code(roots: &[(String, PathBuf)], literal: &str) -> Counterfactual {
    let mut total = Counterfactual::default();
    for (name, root) in roots {
        let Ok(w) = crate::code::walk(root) else { continue };
        for f in w.rust.iter().chain(&w.wgsl) {
            let Ok(text) = std::fs::read(root.join(&f.path)) else { continue };
            let c = measure_file(&format!("{name}/{}", f.path), &text, literal.as_bytes());
            total.grep += c.grep;
            total.read += c.read;
        }
    }
    total
}

/// The counterfactual of a matcher over every indexed file (the `docs`
/// table), or those of one source, or those below one path prefix.
fn measure_all(conn: &Connection, roots: &HashMap<String, PathBuf>, only: Option<&str>, prefix: Option<&str>, f: &dyn Fn(&str, &[u8]) -> Counterfactual) -> Counterfactual {
    let mut total = Counterfactual::default();
    let Ok(mut st) = conn.prepare_cached("SELECT source, path FROM docs") else { return total };
    let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else { return total };
    let prefix = prefix.map(|p| p.replace('\\', "/"));
    for (source, path) in rows.flatten() {
        if only.is_some_and(|s| s != source) || prefix.as_deref().is_some_and(|p| !path.starts_with(p)) {
            continue;
        }
        let Some(root) = roots.get(&source) else { continue };
        let Ok(text) = std::fs::read(root.join(&path)) else { continue };
        let c = f(&format!("{source}/{path}"), &text);
        total.grep += c.grep;
        total.read += c.read;
    }
    total
}

/// The counterfactual of a literal over the Markdown files of the export
/// folder (dot folders skipped), labelled `exports/PATH`.
pub fn measure_exports(dir: &Path, literal: &str) -> Counterfactual {
    let mut total = Counterfactual::default();
    if literal.is_empty() {
        return total;
    }
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((d, rel)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let sub = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push((e.path(), sub)),
                Ok(t) if t.is_file() && name.ends_with(".md") => {
                    let Ok(text) = std::fs::read(e.path()) else { continue };
                    let c = measure_file(&format!("exports/{sub}"), &text, literal.as_bytes());
                    total.grep += c.grep;
                    total.read += c.read;
                }
                _ => {}
            }
        }
    }
    total
}

/// The bytes a reader opens to read one record where it is written: a
/// memory's whole file (meta and body); any other record's text plus 40
/// lines either side of it in its first export (the window a reader opens);
/// for `lane:NAME`, every record of the lane. 0 for an unknown key.
pub fn record_read(mem: &crate::memory::MemoryStore, key: &str) -> usize {
    if let Some(lane) = key.strip_prefix("lane:") {
        return mem.lane(lane).map(|v| v.iter().map(|h| h.title.len() + h.meta.len() + h.body.len()).sum()).unwrap_or(0);
    }
    let Ok(Some(h)) = mem.head(key) else { return 0 };
    if h.kind == "memory" {
        return h.meta.len() + h.body.len();
    }
    let mut total = h.body.len();
    let Ok(exports) = mem.exports_of(key) else { return total };
    if let Some(e) = exports.first() {
        if let Ok(rids) = mem.placement(e) {
            if let Some(i) = rids.iter().position(|r| *r == h.rid) {
                for dir in [-1i64, 1] {
                    let mut lines = 0usize;
                    let mut j = i as i64 + dir;
                    while lines < 40 && j >= 0 && (j as usize) < rids.len() {
                        if let Ok(Some(n)) = mem.heads_by_rid(&[rids[j as usize]]).map(|mut v| v.pop()) {
                            for l in n.body.split_inclusive('\n') {
                                if lines == 40 {
                                    break;
                                }
                                total += l.len();
                                lines += 1;
                            }
                        }
                        j += dir;
                    }
                }
            }
        }
    }
    total
}

/// Logs one "brief without atlas" row: a subagent started, and reading the
/// rulebook in full would have cost `bytes` at that moment.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn log_brief(conn: &Connection, session: &str, agent: &str, bytes: u64) -> Result<()> {
    let time = crate::store::utc(SystemTime::now());
    let day = time.get(..10).unwrap_or("").to_string();
    conn.prepare_cached("INSERT INTO brief_log(time, day, session, agent, bytes) VALUES(?1, ?2, ?3, ?4, ?5)")?.execute(params![time, day, session, agent, bytes as i64])?;
    Ok(())
}

/// The "brief without atlas" totals: subagent starts, and the tokens
/// reading the rulebook in full at each would have cost.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefRow {
    /// Subagent starts logged.
    pub starts: i64,
    /// Tokens, summed over the starts.
    pub tokens: f64,
    /// The latest start's rulebook, in tokens.
    pub latest: f64,
}

/// The "brief without atlas" totals, or `None` when none are logged.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn brief_report(conn: &Connection, bytes_per_token: f64) -> Result<Option<BriefRow>> {
    conn.execute_batch(TOKEN_TABLE)?;
    let (n, sum): (i64, Option<i64>) = conn.query_row("SELECT COUNT(*), SUM(bytes) FROM brief_log", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if n == 0 {
        return Ok(None);
    }
    let latest: i64 = conn.query_row("SELECT bytes FROM brief_log ORDER BY id DESC LIMIT 1", [], |r| r.get(0))?;
    Ok(Some(BriefRow { starts: n, tokens: sum.unwrap_or(0) as f64 / bytes_per_token, latest: latest as f64 / bytes_per_token }))
}

/// Logs one row whose counterfactual was computed by the caller (the hooks).
///
/// # Arguments
///
/// * `conn` - a connection to the index database (the table exists).
/// * `tool` - the tool or `hook:<event>`.
/// * `returned` - bytes returned.
/// * `cf` - the counterfactual, if the answer has one.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn log_row(conn: &Connection, tool: &str, returned: usize, cf: Option<Counterfactual>) -> Result<()> {
    log(conn, &Record { tool: tool.to_string(), returned, probe: Probe::None }, cf)
}

fn log(conn: &Connection, r: &Record, cf: Option<Counterfactual>) -> Result<()> {
    let time = crate::store::utc(SystemTime::now());
    let day = time.get(..10).unwrap_or("").to_string();
    conn.prepare_cached("INSERT INTO token_log(time, day, tool, returned, grep, read) VALUES(?1, ?2, ?3, ?4, ?5, ?6)")?.execute(params![
        time,
        day,
        r.tool,
        r.returned as i64,
        cf.map(|c| c.grep as i64),
        cf.map(|c| c.read as i64)
    ])?;
    Ok(())
}

/// One row of the token report.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    /// `tool` or a day (`YYYY-MM-DD`).
    pub key: String,
    /// Answers logged.
    pub answers: i64,
    /// Tokens returned.
    pub returned: f64,
    /// Counterfactual tokens (grep plus reading), over the answers that
    /// have one.
    pub counterfactual: f64,
    /// Tokens returned by the answers that have a counterfactual.
    pub returned_comparable: f64,
}

/// Totals by tool and by day.
///
/// # Arguments
///
/// * `conn` - the database.
/// * `bytes_per_token` - the ratio.
///
/// # Errors
///
/// [`crate::Error::Db`] on SQLite failure.
pub fn report(conn: &Connection, bytes_per_token: f64) -> Result<(Vec<ReportRow>, Vec<ReportRow>)> {
    conn.execute_batch(TOKEN_TABLE)?;
    let q = |col: &str| -> Result<Vec<ReportRow>> {
        let mut st = conn.prepare(&format!(
            "SELECT {col}, COUNT(*), SUM(returned), SUM(ifnull(grep, 0) + ifnull(read, 0)), SUM(CASE WHEN grep IS NULL THEN 0 ELSE returned END)
             FROM token_log GROUP BY {col} ORDER BY {col}"
        ))?;
        let rows = st.query_map([], |r| {
            Ok(ReportRow {
                key: r.get(0)?,
                answers: r.get(1)?,
                returned: r.get::<_, i64>(2)? as f64 / bytes_per_token,
                counterfactual: r.get::<_, i64>(3)? as f64 / bytes_per_token,
                returned_comparable: r.get::<_, i64>(4)? as f64 / bytes_per_token,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    };
    Ok((q("tool")?, q("day")?))
}

/// The recall hooks' injections, judged against the "brief without atlas"
/// row: what the pointers cost in all, per prompt that got any, and the
/// largest one session spent.
#[derive(Debug, Clone, PartialEq)]
pub struct InjectRow {
    /// Keys injected.
    pub keys: i64,
    /// Sessions (and subagents) injected into.
    pub sessions: i64,
    /// Tokens injected, summed.
    pub tokens: f64,
    /// The most tokens one session or subagent was injected.
    pub max_session: f64,
}

/// The hooks' injection totals, or `None` when none are logged.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn inject_report(conn: &Connection, bytes_per_token: f64) -> Result<Option<InjectRow>> {
    conn.execute_batch(TOKEN_TABLE)?;
    let (keys, sessions, sum): (i64, i64, Option<i64>) =
        conn.query_row("SELECT COUNT(*), COUNT(DISTINCT session || '/' || agent), SUM(bytes) FROM hook_inject", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    if keys == 0 {
        return Ok(None);
    }
    let max: Option<i64> = conn.query_row("SELECT MAX(b) FROM (SELECT SUM(bytes) AS b FROM hook_inject GROUP BY session, agent)", [], |r| r.get(0))?;
    Ok(Some(InjectRow { keys, sessions, tokens: sum.unwrap_or(0) as f64 / bytes_per_token, max_session: max.unwrap_or(0) as f64 / bytes_per_token }))
}

/// Logs what context selection did to one injected record (`inject_select`,
/// see [`crate::memory::select`]): `how` is `whole`, `trim` or `collapse`;
/// `full` the bytes it would have taken whole.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn log_select(conn: &Connection, hook: &str, key: &str, how: &str, bytes: usize, full: usize) -> Result<()> {
    let time = crate::store::utc(SystemTime::now());
    conn.prepare_cached("INSERT INTO inject_select(time, hook, key, how, bytes, full) VALUES(?1, ?2, ?3, ?4, ?5, ?6)")?.execute(params![time, hook, key, how, bytes as i64, full as i64])?;
    Ok(())
}

/// One row of the context-selection report: an outcome, how many injected
/// records had it, and their tokens as sent and whole.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectRow {
    /// `whole`, `trim` or `collapse`.
    pub how: String,
    /// Records.
    pub n: i64,
    /// Tokens sent.
    pub sent: f64,
    /// Tokens they would have taken whole.
    pub full: f64,
}

/// What context selection did to the hooks' injections, by outcome.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn select_report(conn: &Connection, bytes_per_token: f64) -> Result<Vec<SelectRow>> {
    conn.execute_batch(TOKEN_TABLE)?;
    let mut st = conn.prepare("SELECT how, COUNT(*), SUM(bytes), SUM(full) FROM inject_select GROUP BY how ORDER BY how")?;
    let rows = st.query_map([], |r| {
        Ok(SelectRow { how: r.get(0)?, n: r.get(1)?, sent: r.get::<_, i64>(2)? as f64 / bytes_per_token, full: r.get::<_, i64>(3)? as f64 / bytes_per_token })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// One row of the weekly misses pass.
#[derive(Debug, Clone, PartialEq)]
pub struct MissRow {
    /// The tool, or the hook, that missed.
    pub what: String,
    /// The query, or the injected key.
    pub query: String,
    /// Times seen in the window.
    pub count: i64,
    /// Tokens: what was found elsewhere (misses) or injected (keys).
    pub tokens: f64,
    /// The newest time seen.
    pub last: String,
}

/// The weekly misses pass: the ten queries atlas missed most often since
/// `since` (open `report_miss` records, by tool and query), and the ten
/// injected keys that cost the most tokens in the same window, to check by
/// hand whether each earned its place.
///
/// # Arguments
///
/// * `conn` - the index database.
/// * `since` - UTC time, inclusive (`2026-10-27` or a full timestamp).
/// * `bytes_per_token` - the ratio.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn weekly_misses(conn: &Connection, since: &str, bytes_per_token: f64) -> Result<(Vec<MissRow>, Vec<MissRow>)> {
    conn.execute_batch(TOKEN_TABLE)?;
    let has_misses: bool = conn.query_row("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'misses'", [], |r| r.get(0))?;
    let row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<MissRow> {
        Ok(MissRow { what: r.get(0)?, query: r.get(1)?, count: r.get(2)?, tokens: r.get::<_, i64>(3)? as f64 / bytes_per_token, last: r.get(4)? })
    };
    let misses = if has_misses {
        let mut st = conn.prepare(
            "SELECT tool, query, COUNT(*), SUM(LENGTH(found)), MAX(time) FROM misses WHERE status = 'open' AND time >= ?1
             GROUP BY tool, query ORDER BY COUNT(*) DESC, MAX(time) DESC LIMIT 10",
        )?;
        let rows = st.query_map([since], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    } else {
        Vec::new()
    };
    let mut st = conn.prepare(
        "SELECT hook, key, COUNT(*), SUM(bytes), MAX(time) FROM hook_inject WHERE time >= ?1
         GROUP BY hook, key ORDER BY SUM(bytes) DESC, key LIMIT 10",
    )?;
    let keys = st.query_map([since], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((misses, keys))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_bytes_follow_the_output_shape() {
        let c = measure_file("s/a.md", b"one\nTK-1 here\nthree\ntk-1 again\n", b"TK-1");
        // "s/a.md:2:TK-1 here\n" + "s/a.md:4:tk-1 again\n"
        assert_eq!(c.grep, 19 + 20);
        // Short file: read whole.
        assert_eq!(c.read, 31);
        assert_eq!(measure_file("x", b"nothing", b"TK-1"), Counterfactual::default());
    }

    #[test]
    fn long_files_are_read_in_merged_windows() {
        let mut text = String::new();
        for i in 0..1000 {
            text.push_str(if i == 500 || i == 510 { "hit\n" } else { "abcdefghi\n" });
        }
        let c = measure_file("f", text.as_bytes(), b"HIT");
        // Lines 460..550 (window of 500 is 460..540, of 510 is 470..550,
        // merged): 90 lines, two of them 4 bytes, the rest 10.
        assert_eq!(c.read, 88 * 10 + 2 * 4);
    }

    #[test]
    fn a_line_matcher_counts_like_the_literal_one() {
        let text = b"one\nTK-1 here\nthree\ntk-1 again\n";
        let lit = measure_file("s/a.md", text, b"TK-1");
        let re = crate::regex::Regex::new("(?i)tk-[0-9]").unwrap();
        let by_re = measure_lines("s/a.md", text, &|l: &[u8]| std::str::from_utf8(l).is_ok_and(|s| re.is_match(s)));
        assert_eq!(lit, by_re);
    }

    #[test]
    fn exports_are_grepped_below_the_folder_without_dot_folders() {
        let dir = std::env::temp_dir().join(format!("atlas-exports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("RULES.md"), "a widget rule\nother\n").unwrap();
        std::fs::write(dir.join("memory").join("w.md"), "the Widget\n").unwrap();
        std::fs::write(dir.join(".git").join("x.md"), "widget\n").unwrap();
        std::fs::write(dir.join("memory").join("w.txt"), "widget\n").unwrap();
        let c = measure_exports(&dir, "widget");
        // "exports/RULES.md:1:a widget rule\n" + "exports/memory/w.md:1:the Widget\n"
        assert_eq!(c.grep, "exports/RULES.md:1:a widget rule\n".len() + "exports/memory/w.md:1:the Widget\n".len());
        assert_eq!(c.read, 20 + 11);
        assert_eq!(measure_exports(&dir, ""), Counterfactual::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn brief_rows_stay_out_of_the_tool_report() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        assert_eq!(brief_report(&conn, 4.0).unwrap(), None);
        log_row(&conn, "recall", 400, Some(Counterfactual { grep: 0, read: 4000 })).unwrap();
        log_brief(&conn, "s", "a1", 8000).unwrap();
        log_brief(&conn, "s", "a2", 12000).unwrap();
        let (by_tool, _) = report(&conn, 4.0).unwrap();
        assert_eq!(by_tool.len(), 1);
        assert_eq!(by_tool[0].counterfactual, 1000.0);
        assert_eq!(brief_report(&conn, 4.0).unwrap(), Some(BriefRow { starts: 2, tokens: 5000.0, latest: 3000.0 }));
    }

    #[test]
    fn the_selection_report_sums_each_outcome() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(select_report(&conn, 4.0).unwrap().is_empty());
        log_select(&conn, "UserPromptSubmit", "rules:r00001", "trim", 80, 120).unwrap();
        log_select(&conn, "UserPromptSubmit", "rules:r00002", "trim", 40, 80).unwrap();
        log_select(&conn, "UserPromptSubmit", "mem-a", "collapse", 20, 100).unwrap();
        log_select(&conn, "SubagentStart", "lane:L-1", "whole", 400, 400).unwrap();
        let rows = select_report(&conn, 4.0).unwrap();
        let row = |h: &str| rows.iter().find(|r| r.how == h).cloned().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(row("trim"), SelectRow { how: "trim".into(), n: 2, sent: 30.0, full: 50.0 });
        assert_eq!(row("collapse"), SelectRow { how: "collapse".into(), n: 1, sent: 5.0, full: 25.0 });
        assert_eq!(row("whole"), SelectRow { how: "whole".into(), n: 1, sent: 100.0, full: 100.0 });
    }

    #[test]
    fn crlf_lines_print_without_the_cr() {
        let c = measure_file("f", b"a TK\r\nb\r\n", b"tk");
        assert_eq!(c.grep, "f:1:a TK\n".len());
    }

    #[test]
    fn the_weekly_pass_ranks_misses_and_costly_injections_in_its_window() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        conn.execute_batch(
            "CREATE TABLE misses(id INTEGER PRIMARY KEY, time TEXT NOT NULL, tool TEXT NOT NULL, query TEXT NOT NULL, returned TEXT NOT NULL,
             found TEXT NOT NULL, reason TEXT NOT NULL, client TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'open', resolved_time TEXT);",
        )
        .unwrap();
        let miss = |t: &str, q: &str, status: &str| {
            conn.execute("INSERT INTO misses(time, tool, query, returned, found, reason, client, status) VALUES(?1, 'lookup', ?2, '', 'abcd', 'r', 'c', ?3)", params![t, q, status]).unwrap();
        };
        miss("2026-11-01T10:00:00.000Z", "PI-1", "open");
        miss("2026-11-02T10:00:00.000Z", "PI-1", "open");
        miss("2026-11-02T11:00:00.000Z", "PI-2", "open");
        miss("2026-11-02T12:00:00.000Z", "PI-3", "resolved");
        miss("2026-10-01T10:00:00.000Z", "PI-9", "open");
        let inj = |t: &str, s: &str, k: &str, b: i64| {
            conn.execute("INSERT INTO hook_inject(time, session, agent, transcript, hook, key, fts, cos, bytes, read) VALUES(?1, ?2, '', '', 'UserPromptSubmit', ?3, NULL, NULL, ?4, 0)", params![t, s, k, b]).unwrap();
        };
        inj("2026-11-02T10:00:00.000Z", "a", "big", 400);
        inj("2026-11-02T10:00:00.000Z", "b", "small", 40);
        inj("2026-11-02T10:00:00.000Z", "b", "big", 400);
        let (m, k) = weekly_misses(&conn, "2026-10-27", 4.0).unwrap();
        assert_eq!(m.iter().map(|r| (r.query.as_str(), r.count)).collect::<Vec<_>>(), vec![("PI-1", 2), ("PI-2", 1)]);
        assert_eq!(k.iter().map(|r| (r.query.as_str(), r.count)).collect::<Vec<_>>(), vec![("big", 2), ("small", 1)]);
        assert_eq!(k[0].tokens, 200.0);
        let i = inject_report(&conn, 4.0).unwrap().unwrap();
        assert_eq!((i.keys, i.sessions, i.tokens, i.max_session), (3, 2, 210.0, 110.0));
    }
}
