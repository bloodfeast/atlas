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
//! text. Grep prints at most [`GREP_LINES`] lines in all (the Grep tool's
//! head_limit), and only the files among them are read. The other read
//! tools that have a grep equivalent use the same two parts ([`Probe`]):
//!
//! * `section` greps its id (or heading) like `lookup`, over the same
//!   source or file when the question names one.
//! * `grep` is its own counterfactual: the pattern, matched by the same
//!   regex engine per line (`grep -rnE`), plus the reads.
//! * `symbol` greps its name over the code roots as a whole identifier,
//!   case-sensitively, and reads only the files that define it (`fn`,
//!   `struct`, `enum`, `trait`, `type`, `const`, `static` or `mod NAME`).
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
//! The report caps each answer of the lookup tools ([`CAPPED`]) at the
//! 90th-percentile pre-atlas lookup episode of the latest calibration
//! ([`lookup_cap`]): one answer never saves more than a measured episode
//! spent. With no calibration, or too little pre-atlas history, those
//! tools are credited nothing (#218).
//!
//! Separately, the "brief without atlas": what an agent spent reading the
//! rulebook before atlas, measured from the calibration's pre-atlas
//! subagent transcripts ([`brief_measured`], #219): the bytes of their
//! `Read` calls of the rulebook over every such transcript. Agents never
//! read it whole, so its size is not the yardstick. Each subagent start
//! still logs the rulebook's size at that moment (`brief_log`, see
//! [`log_brief`]): the agents counted ([`brief_report`]) and an "if read
//! whole" line. The measured read is reported per agent, beside the hooks'
//! injections (main sessions per prompt, subagents per agent,
//! [`inject_report`], #212) and against them on the agents both tables hold
//! ([`brief_vs_inject`]); it never enters a per-answer ratio.
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

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

/// The default bytes-per-token ratio.
pub const DEFAULT_BYTES_PER_TOKEN: f64 = 4.0;

/// Lines in the window a reader opens around a match.
pub const READ_WINDOW: usize = 80;

/// The most lines a grep counterfactual prints, over all files: the Grep
/// tool's default head_limit.
pub const GREP_LINES: usize = 250;

/// The most characters of a shell output Claude Code puts into context (it
/// stores a longer one aside): a routed output saves at most these (#215).
/// Fixed, so the report reads the same on every machine.
pub const SHOWN_CHARS: usize = 30_000;

/// The bytes of the first [`SHOWN_CHARS`] characters of an output (all of
/// it when shorter), cut on a character boundary: a routed output's credit.
pub fn shown_bytes(full: &str) -> usize {
    full.char_indices().nth(SHOWN_CHARS).map_or(full.len(), |(i, _)| i)
}

/// The tools whose counterfactual the report caps per answer at
/// [`lookup_cap`] (#218, #210).
pub const CAPPED: &[&str] = &["lookup", "search", "section", "grep", "symbol", "recall", "read_note", "ids"];

/// The per-answer cap of the [`CAPPED`] tools, in bytes: the 90th
/// percentile pre-atlas lookup episode of the latest calibration
/// (`baseline_log`), read at report time. `None` when there is none, it
/// measured no episode, or it had too little pre-atlas history: then those
/// tools are credited nothing. A read: no table is made.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn lookup_cap(conn: &Connection) -> Result<Option<i64>> {
    let has: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'baseline_log')", [], |r| r.get(0))?;
    if !has {
        return Ok(None);
    }
    let too_few = if crate::baseline::has_too_few(conn)? { "too_few" } else { "0" };
    let mut st = conn.prepare(&format!("SELECT p90, episodes, {too_few} FROM baseline_log ORDER BY id DESC LIMIT 1"))?;
    let row: Option<(f64, i64, i64)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.next().transpose()?;
    Ok(row.filter(|(p90, episodes, too_few)| *episodes > 0 && *too_few == 0 && *p90 > 0.0).map(|(p90, _, _)| p90.round() as i64))
}

/// The SQL expression for one `token_log` row's credited counterfactual in
/// bytes (0 for a row without one): `grep + read`, capped at `cap` for the
/// [`CAPPED`] tools, and 0 for them with no cap; 0 for `route:Read` (the
/// ledger cannot see the reads that follow it, #216: the report adds its
/// judged credit, [`read_credit`]); and at most
/// [`SHOWN_CHARS`] bytes for any other `route:*` row (#215: a row logged
/// before the cap holds the whole output's bytes, and the first 30,000
/// characters are at least 30,000 bytes, so this errs low). A
/// `hook:<event>` row credits 0 here: hooks are credited by hit only,
/// added by [`add_hook_credit`] (#217).
pub fn credit_sql(cap: Option<i64>) -> String {
    let capped = CAPPED.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(", ");
    let cap = cap.unwrap_or(0);
    format!(
        "(CASE WHEN grep IS NULL THEN 0 WHEN tool IN ({capped}) THEN min(grep + ifnull(read, 0), {cap}) WHEN tool = 'route:Read' THEN 0 WHEN tool LIKE 'hook:%' THEN 0 \
         WHEN tool LIKE 'route:%' THEN min(grep + ifnull(read, 0), {SHOWN_CHARS}) ELSE grep + ifnull(read, 0) END)"
    )
}

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
    read INTEGER,
    session TEXT,
    transcript TEXT,
    path TEXT,
    tool_use TEXT
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
CREATE INDEX IF NOT EXISTS hook_inject_session ON hook_inject(session, agent);
CREATE INDEX IF NOT EXISTS hook_inject_agent ON hook_inject(agent);
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
    measure_lines_upto(label, text, hit, usize::MAX).0
}

/// [`measure_lines`] for at most `max` matching lines (the rest of the
/// file's matches are not printed, and not read around), with the number
/// of lines it printed.
fn measure_lines_upto(label: &str, text: &[u8], hit: &dyn Fn(&[u8]) -> bool, max: usize) -> (Counterfactual, usize) {
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
        if hits.len() < max && hit(line) {
            let body = line.strip_suffix(b"\r").unwrap_or(line);
            grep += label.len() + 1 + (i + 1).to_string().len() + 1 + body.len() + 1;
            hits.push(i);
        }
        start = end + 1;
    }
    if hits.is_empty() {
        return (Counterfactual::default(), 0);
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
    (Counterfactual { grep, read }, hits.len())
}

/// Whether a byte belongs to an identifier (non-ASCII counts, so a name is
/// never cut out of a longer non-ASCII one).
fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Where `name` stands in `line` as a whole identifier, case-sensitively.
fn word_at(line: &[u8], name: &[u8]) -> Vec<usize> {
    if name.is_empty() || line.len() < name.len() {
        return Vec::new();
    }
    (0..=line.len() - name.len())
        .filter(|&i| {
            &line[i..i + name.len()] == name
                && (i == 0 || !ident_byte(line[i - 1]))
                && line.get(i + name.len()).is_none_or(|b| !ident_byte(*b))
        })
        .collect()
}

/// The keywords that define a name in Rust or WGSL code.
const DEFINES: &[&str] = &["fn", "struct", "enum", "trait", "type", "const", "static", "mod"];

/// Whether a line defines `name`: one of [`DEFINES`] as a word, whitespace,
/// then the name as a whole identifier.
fn defines(line: &[u8], name: &[u8]) -> bool {
    word_at(line, name).into_iter().any(|i| {
        let before = &line[..i];
        let kw = before.trim_ascii_end();
        kw.len() < before.len()
            && DEFINES.iter().any(|d| kw.ends_with(d.as_bytes()) && kw[..kw.len() - d.len()].last().is_none_or(|b| !ident_byte(*b)))
    })
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
        let conn = crate::store::open_raw(db)?;
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
                            (!lit.is_empty()).then(|| {
                                measure_all(&conn, &roots, source.as_deref(), path.as_deref(), &|label, t, max| {
                                    measure_literal_upto(label, t, lit, max)
                                })
                            })
                        }
                        Probe::Regex { pattern, source, path } => crate::regex::Regex::new(pattern).ok().map(|re| {
                            let hit =
                                |line: &[u8]| std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)).is_ok_and(|s| re.is_match(s));
                            measure_all(&conn, &roots, source.as_deref(), path.as_deref(), &|label, t, max| {
                                measure_lines_upto(label, t, &hit, max)
                            })
                        }),
                        Probe::File { source, path } => roots.get(source).and_then(|root| std::fs::metadata(root.join(path)).ok()).map(|m| Counterfactual { grep: 0, read: m.len() as usize }),
                        Probe::Exports(text) => match paths().and_then(|p| p.export_dir) {
                            Some(dir) => Some(measure_exports(&dir, text)),
                            None => Some(measure_all(&conn, &roots, None, None, &|label, t, max| {
                                measure_literal_upto(label, t, text.as_bytes(), max)
                            })),
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

/// [`measure_file`] for at most `max` matching lines.
fn measure_literal_upto(label: &str, text: &[u8], literal: &[u8], max: usize) -> (Counterfactual, usize) {
    if literal.is_empty() {
        return (Counterfactual::default(), 0);
    }
    measure_lines_upto(
        label,
        text,
        &|line: &[u8]| line.len() >= literal.len() && line.windows(literal.len()).any(|w| w.eq_ignore_ascii_case(literal)),
        max,
    )
}

/// The counterfactual of a symbol over the `.rs` and `.wgsl` files of the
/// code roots (#210): grep for the name as a whole identifier,
/// case-sensitively, at most [`GREP_LINES`] lines in all, then read only
/// the files among them that define it (see [`DEFINES`]), by the usual
/// read rule.
///
/// # Arguments
///
/// * `roots` - code root names and folders.
/// * `name` - the identifier sought.
pub fn measure_code(roots: &[(String, PathBuf)], name: &str) -> Counterfactual {
    let mut total = Counterfactual::default();
    let mut left = GREP_LINES;
    let n = name.as_bytes();
    for (root_name, root) in roots {
        let Ok(w) = crate::code::walk(root) else { continue };
        for f in w.rust.iter().chain(&w.wgsl) {
            if left == 0 {
                return total;
            }
            let Ok(text) = std::fs::read(root.join(&f.path)) else { continue };
            let (c, printed) =
                measure_lines_upto(&format!("{root_name}/{}", f.path), &text, &|line: &[u8]| !word_at(line, n).is_empty(), left);
            left -= printed;
            total.grep += c.grep;
            if printed > 0 && text.split(|&b| b == b'\n').any(|line| defines(line, n)) {
                total.read += c.read;
            }
        }
    }
    total
}

/// A per-file matcher for [`measure_all`]: the file's label, its text and
/// the matching lines still left to print; its counterfactual and the
/// lines it printed.
type Matcher<'a> = dyn Fn(&str, &[u8], usize) -> (Counterfactual, usize) + 'a;

/// The counterfactual of a matcher over every indexed file (the `docs`
/// table), or those of one source, or those below one path prefix: at most
/// [`GREP_LINES`] matching lines in all (`f` gets the lines left, and says
/// how many it printed).
fn measure_all(
    conn: &Connection,
    roots: &HashMap<String, PathBuf>,
    only: Option<&str>,
    prefix: Option<&str>,
    f: &Matcher,
) -> Counterfactual {
    let mut total = Counterfactual::default();
    let mut left = GREP_LINES;
    let Ok(mut st) = conn.prepare_cached("SELECT source, path FROM docs") else { return total };
    let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else { return total };
    let prefix = prefix.map(|p| p.replace('\\', "/"));
    for (source, path) in rows.flatten() {
        if only.is_some_and(|s| s != source) || prefix.as_deref().is_some_and(|p| !path.starts_with(p)) {
            continue;
        }
        if left == 0 {
            break;
        }
        let Some(root) = roots.get(&source) else { continue };
        let Ok(text) = std::fs::read(root.join(&path)) else { continue };
        let (c, printed) = f(&format!("{source}/{path}"), &text, left);
        left -= printed;
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

/// Logs one `brief_log` row: a subagent started, and reading the rulebook
/// whole would have cost `bytes` at that moment. The report counts agents by
/// these rows and keeps their sum as "if read whole"; what the brief
/// without atlas costs is measured from pre-atlas reads
/// ([`brief_measured`]).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn log_brief(conn: &Connection, session: &str, agent: &str, bytes: u64) -> Result<()> {
    let time = crate::store::utc(SystemTime::now());
    let day = time.get(..10).unwrap_or("").to_string();
    crate::store::guarded(conn, || {
        conn.prepare_cached("INSERT INTO brief_log(time, day, session, agent, bytes) VALUES(?1, ?2, ?3, ?4, ?5)")?.execute(params![time, day, session, agent, bytes as i64])?;
        Ok(())
    })
}

/// The `brief_log` totals, per agent: the agents that started, and the
/// tokens reading the rulebook whole once each would have cost (the "if
/// read whole" line, not a measurement: agents read parts of it, #219; the
/// measured figure is [`brief_measured`]).
///
/// The unit is the agent (a distinct session and agent id), and each agent
/// reads the rulebook once, as it was at the agent's first start. Claude
/// Code fires `SubagentStart` again each time an agent resumes (a
/// `SendMessage` to it, or a background task waking it), and each firing
/// logs a row; a resumed agent keeps its context, so it would not read the
/// rulebook again. On the live store (2 to 16 Nov 2026) 1,598 agents logged
/// 3,925 rows: the repeats came minutes to hours apart (one within a
/// second), and the hooks injected anything on 10 of the 2,327 repeats
/// against 1,486 of the 1,598 first starts. The rows themselves are kept
/// as `starts`, a secondary figure.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefRow {
    /// Agents (distinct session and agent) with a logged start.
    pub agents: i64,
    /// `SubagentStart` rows logged: the first start of each agent and every
    /// resume after it.
    pub starts: i64,
    /// Tokens, summed over the agents: the rulebook's size at each one's
    /// first start ("if read whole").
    pub tokens: f64,
    /// The latest start's rulebook, in tokens.
    pub latest: f64,
}

impl BriefRow {
    /// Tokens per agent.
    #[must_use]
    pub fn per_agent(&self) -> f64 {
        self.tokens / self.agents.max(1) as f64
    }

    /// Starts that were resumes of an agent already counted.
    #[must_use]
    pub fn resumes(&self) -> i64 {
        self.starts - self.agents
    }
}

/// Each agent's first start in `brief_log`: (session, agent, bytes).
const FIRST_BRIEF: &str = "SELECT b.session, b.agent, b.bytes FROM brief_log b JOIN (SELECT MIN(id) AS id FROM brief_log GROUP BY session, agent) f ON b.id = f.id";

/// The `brief_log` totals ([`BriefRow`], per agent), or `None` when none
/// are logged.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn brief_report(conn: &Connection, bytes_per_token: f64) -> Result<Option<BriefRow>> {
    conn.execute_batch(TOKEN_TABLE)?;
    let starts: i64 = conn.query_row("SELECT COUNT(*) FROM brief_log", [], |r| r.get(0))?;
    if starts == 0 {
        return Ok(None);
    }
    let (agents, sum): (i64, Option<i64>) =
        conn.query_row(&format!("SELECT COUNT(*), SUM(bytes) FROM ({FIRST_BRIEF})"), [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let latest: i64 = conn.query_row("SELECT bytes FROM brief_log ORDER BY id DESC LIMIT 1", [], |r| r.get(0))?;
    Ok(Some(BriefRow { agents, starts, tokens: sum.unwrap_or(0) as f64 / bytes_per_token, latest: latest as f64 / bytes_per_token }))
}

/// The rulebook as agents read it before atlas (#219): measured from the
/// calibration's pre-atlas transcripts, never from the file's size.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefMeasure {
    /// Tokens per pre-atlas subagent: the reads' bytes over every
    /// transcript, readers and non-readers alike.
    pub per_agent: f64,
    /// `Read` calls of the rulebook counted.
    pub reads: i64,
    /// Transcripts with at least one of them.
    pub readers: i64,
    /// Pre-atlas subagent transcripts: the denominator.
    pub transcripts: i64,
    /// The calibration's cutoff (the first atlas call).
    pub cutoff: String,
}

/// A path's text as the rulebook match compares it: backslashes made
/// slashes, `.` and `..` folded, repeated slashes merged and ASCII case
/// folded. Only the text is compared; no file is opened.
fn path_key(p: &str) -> String {
    let p = p.replace('\\', "/").to_ascii_lowercase();
    let absolute = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                // Never above a drive or the root.
                if parts.last().is_some_and(|l| !l.ends_with(':')) {
                    parts.pop();
                }
            }
            c => parts.push(c),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Whether a path's text is absolute: a leading slash or a drive letter.
fn is_absolute_text(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with('\\') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// The "brief without atlas" as measured (#219), or `None` when there is no
/// calibration, no pre-atlas subagent transcript or no read of the
/// rulebook among them.
///
/// The denominator is every pre-atlas subagent transcript, one per agent:
/// a `calibrate_files` row whose path has a `subagents` folder and that
/// holds at least one call before the `calibrate_state` cutoff. Main
/// sessions are left out. The numerator is the bytes of the `Read` calls
/// in those transcripts, before the cutoff, whose `file_path` is
/// `reference`: slashes normalised, case folded, and a relative path joined
/// to the call's working folder ([`path_key`]). The path text is matched;
/// the file is never opened. A read: no table is made.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn brief_measured(conn: &Connection, reference: &Path, bytes_per_token: f64) -> Result<Option<BriefMeasure>> {
    let has: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'calibrate_state')", [], |r| r.get(0))?;
    if !has {
        return Ok(None);
    }
    let cutoff: Option<String> = conn.query_row("SELECT cutoff FROM calibrate_state WHERE id = 1", [], |r| r.get(0)).optional()?;
    let Some(cutoff) = cutoff else { return Ok(None) };
    let subagent = |f: &str| f.replace('\\', "/").split('/').any(|c| c == "subagents");
    let mut st = conn.prepare(
        "SELECT f.file FROM calibrate_files f WHERE EXISTS(SELECT 1 FROM calibrate_calls c WHERE c.file = f.file AND c.time < ?1)",
    )?;
    let files: Vec<String> = st.query_map([&cutoff], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let files: std::collections::BTreeSet<String> = files.into_iter().filter(|f| subagent(f)).collect();
    if files.is_empty() {
        return Ok(None);
    }
    let want = path_key(&reference.to_string_lossy());
    let mut st = conn.prepare("SELECT file, cwd, input, bytes FROM calibrate_calls WHERE name = 'Read' AND time < ?1")?;
    let rows = st.query_map([&cutoff], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?))
    })?;
    let (mut reads, mut bytes) = (0i64, 0i64);
    let mut readers: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for row in rows {
        let (file, cwd, input, b) = row?;
        if !files.contains(&file) {
            continue;
        }
        let Some(path) = input
            .as_deref()
            .and_then(|i| crate::json::parse(i).ok())
            .and_then(|v| v.get("file_path").and_then(crate::json::Value::as_str).map(str::to_string))
        else {
            continue;
        };
        let full = if is_absolute_text(&path) { path } else { format!("{cwd}/{path}") };
        if path_key(&full) == want {
            reads += 1;
            bytes += b;
            readers.insert(file);
        }
    }
    if reads == 0 {
        return Ok(None);
    }
    Ok(Some(BriefMeasure {
        per_agent: bytes as f64 / bytes_per_token / files.len() as f64,
        reads,
        readers: readers.len() as i64,
        transcripts: files.len() as i64,
        cutoff,
    }))
}

/// The brief and the injections on the same agents: those with a row in
/// both `brief_log` and `hook_inject` (a main session logs no brief, and a
/// subagent the hooks never injected into has no injection). The rulebook
/// is the measured pre-atlas read ([`brief_measured`], #219) per agent; its
/// file size is kept as `whole`, "if read whole", and is never a saving.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchRow {
    /// Agents in both tables.
    pub agents: i64,
    /// The rulebook as read before atlas, in tokens: the agents times the
    /// measured per-agent read, or `None` when not measured.
    pub rulebook: Option<f64>,
    /// The rulebook's size at their first starts, in tokens, summed: what
    /// reading it whole once each would have cost, not a measurement.
    pub whole: f64,
    /// What the hooks injected into them, in tokens, summed.
    pub injected: f64,
    /// Of that, what `SubagentStart` injected: the injections that stand in
    /// for the rulebook.
    pub start: f64,
}

impl MatchRow {
    /// Measured rulebook tokens per injected token: `None` when not
    /// measured or nothing was injected.
    #[must_use]
    pub fn ratio(&self) -> Option<f64> {
        self.rulebook.filter(|_| self.injected > 0.0).map(|r| r / self.injected)
    }
}

/// [`MatchRow`]: the brief against the injections on the agents both
/// tables hold, or `None` when no agent is in both. `measured` is the
/// per-agent pre-atlas read ([`brief_measured`]).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn brief_vs_inject(conn: &Connection, bytes_per_token: f64, measured: Option<&BriefMeasure>) -> Result<Option<MatchRow>> {
    conn.execute_batch(TOKEN_TABLE)?;
    let (agents, whole, injected, start): (i64, Option<i64>, Option<i64>, Option<i64>) = conn.query_row(
        &format!(
            "SELECT COUNT(*), SUM(f.bytes), SUM(i.bytes), SUM(i.start) FROM ({FIRST_BRIEF}) f
             JOIN (SELECT session, agent, SUM(bytes) AS bytes, SUM(CASE WHEN hook = 'SubagentStart' THEN bytes ELSE 0 END) AS start
                   FROM hook_inject GROUP BY session, agent) i ON i.session = f.session AND i.agent = f.agent"
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if agents == 0 {
        return Ok(None);
    }
    Ok(Some(MatchRow {
        agents,
        rulebook: measured.map(|m| m.per_agent * agents as f64),
        whole: whole.unwrap_or(0) as f64 / bytes_per_token,
        injected: injected.unwrap_or(0) as f64 / bytes_per_token,
        start: start.unwrap_or(0) as f64 / bytes_per_token,
    }))
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
    crate::store::guarded(conn, || {
        conn.prepare_cached("INSERT INTO token_log(time, day, tool, returned, grep, read) VALUES(?1, ?2, ?3, ?4, ?5, ?6)")?.execute(params![
            time,
            day,
            r.tool,
            r.returned as i64,
            cf.map(|c| c.grep as i64),
            cf.map(|c| c.read as i64)
        ])?;
        Ok(())
    })
}

/// Where a routed Read was read (E4, journal:ATLAS-EFFICIENCY:0006): what
/// the report needs to subtract the same agent's later Reads of the file.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadAt<'a> {
    /// The session id.
    pub session: &'a str,
    /// The transcript holding the Read: a subagent's own, else the session's.
    pub transcript: &'a str,
    /// The file read.
    pub path: &'a str,
    /// The Read's tool call id (empty when the hook was not told it).
    pub tool_use: &'a str,
}

/// Logs a routed Read: the digest returned, the output it replaced (as
/// `read`, `grep` NULL like the hook rows: [`credit_sql`] credits it 0 and
/// its bytes stay out of the comparable ratio, since its credit is added
/// outside the SQL) and where it was read ([`ReadAt`]). The report credits
/// it by [`read_credit`].
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn log_read(conn: &Connection, returned: usize, replaced: usize, at: &ReadAt) -> Result<()> {
    let time = crate::store::utc(SystemTime::now());
    let day = time.get(..10).unwrap_or("").to_string();
    crate::store::guarded(conn, || {
        let insert = || {
            conn.prepare_cached(
                "INSERT INTO token_log(time, day, tool, returned, grep, read, session, transcript, path, tool_use) VALUES(?1, ?2, 'route:Read', ?3, NULL, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![time, day, returned as i64, replaced as i64, at.session, at.transcript, at.path, at.tool_use])
        };
        // A store from before E4 lacks the columns, so the insert fails to
        // prepare: they are added here, under the write lock (an ALTER is a
        // schema change), its rows kept with the new columns NULL, and the
        // row inserted again. A migrated store pays no extra statement.
        if insert().is_err() {
            add_read_columns(conn)?;
            insert()?;
        }
        Ok(())
    })
}

/// The columns [`log_read`] adds to a `token_log` made before E4.
const READ_COLUMNS: [&str; 4] = ["session", "transcript", "path", "tool_use"];

/// Adds the [`READ_COLUMNS`] a `token_log` lacks (none when it has them, so
/// running it twice is harmless). The caller holds the write lock.
fn add_read_columns(conn: &Connection) -> Result<()> {
    let have = read_columns(conn)?;
    for c in READ_COLUMNS {
        if !have.iter().any(|h| h == c) {
            conn.execute_batch(&format!("ALTER TABLE token_log ADD COLUMN {c} TEXT"))?;
        }
    }
    Ok(())
}

/// The columns `token_log` has (a read).
fn read_columns(conn: &Connection) -> Result<Vec<String>> {
    Ok(conn
        .prepare("SELECT name FROM pragma_table_info('token_log')")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

/// A file path as transcripts and the ledger compare it: `/` separators,
/// and case-folded on Windows.
fn read_path_key(p: &str) -> String {
    let s = p.trim().replace('\\', "/");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// One `Read` call in a transcript: its id, file, time and result bytes.
struct ReadUse {
    id: String,
    path: String,
    time: String,
    bytes: i64,
}

/// The `Read` calls of a transcript in order, with their results' bytes
/// (the text as stored), or `None` when it cannot be read.
fn transcript_reads(path: &Path) -> Option<Vec<ReadUse>> {
    use crate::json::Value;
    let text = std::fs::read_to_string(path).ok()?;
    let mut uses: Vec<ReadUse> = Vec::new();
    let mut results: HashMap<String, i64> = HashMap::new();
    for line in text.lines() {
        if !line.contains("\"tool_use\"") && !line.contains("\"tool_result\"") {
            continue;
        }
        let Ok(v) = crate::json::parse(line) else { continue };
        let time = v.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let Some(Value::Array(items)) = v.pointer("/message/content") else { continue };
        for x in items {
            match x.get("type").and_then(Value::as_str) {
                Some("tool_use") if x.get("name").and_then(Value::as_str) == Some("Read") => {
                    let file = x.pointer("/input/file_path").and_then(Value::as_str).unwrap_or("");
                    let id = x.get("id").and_then(Value::as_str).unwrap_or("");
                    uses.push(ReadUse { id: id.to_string(), path: read_path_key(file), time: time.to_string(), bytes: 0 });
                }
                Some("tool_result") => {
                    let id = x.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    let n = match x.get("content") {
                        Some(Value::String(s)) => s.len(),
                        Some(Value::Array(a)) => a.iter().filter_map(|c| c.get("text").and_then(Value::as_str)).map(str::len).sum(),
                        _ => 0,
                    };
                    results.insert(id.to_string(), n as i64);
                }
                _ => {}
            }
        }
    }
    for u in &mut uses {
        u.bytes = results.get(&u.id).copied().unwrap_or(0);
    }
    Some(uses)
}

/// The judged credit of the `route:Read` rows (E4).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadCredit {
    /// Rows logged with a session, transcript and path.
    pub judged: i64,
    /// Rows logged before E4 (no place columns): credited 0.
    pub unjudged: i64,
    /// Rows logged since E4 without a session, transcript or path (the hook
    /// was not told them): credited 0.
    pub unplaced: i64,
    /// Judged rows whose transcript could not be read: credited 0.
    pub missing: i64,
    /// Judged rows whose stored verdict was used (their transcript not read).
    pub stored: i64,
    /// Transcripts read and parsed by this run.
    pub parsed: i64,
    /// Bytes the judged rows (transcript read) replaced.
    pub replaced: i64,
    /// Bytes of later Reads of the same file in the same transcript,
    /// subtracted (at most each row's replaced bytes).
    pub reread: i64,
    /// Bytes credited: replaced less reread.
    pub credit: i64,
    /// Credit bytes by (`route:Read`, day), for [`add_hook_credit`].
    pub by: std::collections::BTreeMap<(String, String), i64>,
}

/// How long a transcript must stay untouched before its rows' verdicts are
/// stored: after that its session is taken as ended, so no later Read can
/// change them, and the verdict outlives the transcript (Claude Code deletes
/// old ones).
pub const SETTLED: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// The stored verdicts: each settled `route:Read` row's re-read bytes, by
/// its `token_log` id.
const VERDICT_TABLE: &str = "CREATE TABLE IF NOT EXISTS read_verdict(id INTEGER PRIMARY KEY, reread INTEGER NOT NULL)";

/// The judged credit of the `route:Read` rows: each row with a session,
/// transcript and path is credited the output it replaced less the bytes of
/// the later Reads of the same file in the same transcript (E3's method);
/// every other row 0. A row whose transcript has been still for
/// [`SETTLED`] has its verdict stored and is never judged again.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn read_credit(conn: &Connection) -> Result<ReadCredit> {
    read_credit_at(conn, SystemTime::now(), SETTLED)
}

/// A `route:Read` row waiting for its verdict.
struct Pending {
    id: i64,
    time: String,
    day: String,
    replaced: i64,
    path: String,
    tool_use: String,
}

/// [`read_credit`] as of `now`, storing the verdicts of transcripts still
/// for `settled`.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn read_credit_at(conn: &Connection, now: SystemTime, settled: std::time::Duration) -> Result<ReadCredit> {
    let mut c = ReadCredit::default();
    let cols = read_columns(conn)?;
    if cols.is_empty() {
        return Ok(c);
    }
    // A store from before E4 has none of the columns: every row is old.
    if !READ_COLUMNS.iter().all(|k| cols.iter().any(|h| h == k)) {
        c.unjudged = conn.query_row("SELECT COUNT(*) FROM token_log WHERE tool = 'route:Read'", [], |r| r.get(0))?;
        return Ok(c);
    }
    conn.execute_batch(VERDICT_TABLE)?;
    type Row = (i64, String, String, i64, Option<String>, Option<String>, Option<String>, Option<String>, Option<i64>);
    let rows: Vec<Row> = conn
        .prepare(
            "SELECT t.id, t.time, t.day, ifnull(t.read, 0), t.session, t.transcript, t.path, t.tool_use, v.reread \
             FROM token_log t LEFT JOIN read_verdict v ON v.id = t.id WHERE t.tool = 'route:Read' ORDER BY t.id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let credit = |c: &mut ReadCredit, day: String, replaced: i64, reread: i64| {
        c.replaced += replaced;
        c.reread += reread;
        c.credit += replaced - reread;
        *c.by.entry(("route:Read".to_string(), day)).or_default() += replaced - reread;
    };
    // The rows still to judge, by transcript: each transcript is read once.
    let mut open: std::collections::BTreeMap<String, Vec<Pending>> = std::collections::BTreeMap::new();
    for (id, time, day, replaced, session, transcript, path, tool_use, stored) in rows {
        let (Some(session), Some(transcript), Some(path)) = (session, transcript, path) else {
            c.unjudged += 1;
            continue;
        };
        if session.is_empty() || transcript.is_empty() || path.is_empty() {
            c.unplaced += 1;
            continue;
        }
        c.judged += 1;
        if let Some(reread) = stored {
            c.stored += 1;
            credit(&mut c, day, replaced, reread);
            continue;
        }
        open.entry(transcript).or_default().push(Pending {
            id,
            time,
            day,
            replaced,
            path: read_path_key(&path),
            tool_use: tool_use.unwrap_or_default(),
        });
    }
    let mut verdicts: Vec<(i64, i64)> = Vec::new();
    for (transcript, rows) in open {
        let p = Path::new(&transcript);
        let still = std::fs::metadata(p)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|d| d >= settled);
        let Some(uses) = transcript_reads(p) else {
            c.missing += rows.len() as i64;
            continue;
        };
        c.parsed += 1;
        for (row, reread) in rows.iter().zip(judge(&rows, &uses)) {
            credit(&mut c, row.day.clone(), row.replaced, reread);
            if still {
                verdicts.push((row.id, reread));
            }
        }
    }
    if !verdicts.is_empty() {
        crate::store::guarded(conn, || {
            let mut st = conn.prepare_cached("INSERT OR IGNORE INTO read_verdict(id, reread) VALUES(?1, ?2)")?;
            for (id, reread) in &verdicts {
                st.execute(params![id, reread])?;
            }
            Ok(())
        })?;
    }
    Ok(c)
}

/// The re-read bytes of each routed row of one transcript (in `rows`
/// order). Each row's own call is found by its id, else (no id) as the
/// first Read of its file in the row's second (the call comes before the
/// hook logs its row, and a re-read after it), else the last Read of the
/// file before that second. Every other Read of a
/// file is subtracted once, from the routed row of that file just before
/// it, and each row's total is capped at what it replaced. A Read before
/// every routed row of its file subtracts nothing.
///
/// Known limit: a re-read is counted at the result bytes the transcript
/// stores, which carry the Read tool's line-number prefixes, while
/// `replaced` is the file's own bytes; a full re-read so subtracts a few
/// bytes a line more than it read back (the cap keeps it at most the row's
/// replaced bytes, so the credit errs low).
fn judge(rows: &[Pending], uses: &[ReadUse]) -> Vec<i64> {
    // Seconds compared as text: `YYYY-MM-DDTHH:MM:SS` on both sides (the
    // transcript adds milliseconds, the row does not).
    let sec = |t: &str| t.get(..19).unwrap_or(t).to_string();
    let at: Vec<Option<usize>> = rows
        .iter()
        .map(|r| {
            let by_id = (!r.tool_use.is_empty()).then(|| uses.iter().position(|u| u.id == r.tool_use)).flatten();
            let s = sec(&r.time);
            by_id.or_else(|| {
                uses.iter()
                    .position(|u| u.path == r.path && sec(&u.time) == s)
                    .or_else(|| uses.iter().rposition(|u| u.path == r.path && sec(&u.time) < s))
            })
        })
        .collect();
    let mut later = vec![0i64; rows.len()];
    for (j, u) in uses.iter().enumerate() {
        if at.contains(&Some(j)) {
            continue;
        }
        // The routed row of this file whose call is the latest before `j`
        // (a row whose call was not found stands before every Read).
        let owner = rows
            .iter()
            .enumerate()
            .filter(|(i, r)| r.path == u.path && at[*i].is_none_or(|a| a < j))
            .max_by_key(|(i, _)| (at[*i].map_or(0, |a| a + 1), *i))
            .map(|(i, _)| i);
        if let Some(i) = owner {
            later[i] += u.bytes;
        }
    }
    rows.iter().zip(later).map(|(r, l)| l.min(r.replaced)).collect()
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
    /// Counterfactual tokens (grep plus reading, as credited: see
    /// [`credit_sql`]), over the answers that have one.
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
    let credit = credit_sql(lookup_cap(conn)?);
    let q = |col: &str| -> Result<Vec<ReportRow>> {
        let mut st = conn.prepare(&format!(
            "SELECT {col}, COUNT(*), SUM(returned), SUM({credit}), SUM(CASE WHEN grep IS NULL THEN 0 ELSE returned END)
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

/// The ledger credit, in tokens, of the `recall` answers that read records
/// where they are written (`recall {key}` and `{lane}`, [`record_read`]),
/// capped as [`report`] caps them. Their rows are the recall rows with no
/// grep part and a read: a query's counterfactual greps first, and a query
/// that matched nothing reads nothing. `token_log` records no agent and no
/// export per answer, so this is every such answer, main sessions and
/// subagents, any record: an upper bound on what overlaps the measured
/// rulebook ([`crate::token_cost::Cost::rulebook_overlap`]).
///
/// # Errors
///
/// [`crate::Error::Db`] on SQLite failure.
pub fn recall_records_credit(conn: &Connection, bytes_per_token: f64) -> Result<f64> {
    // A read: no table is made.
    let has: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'token_log')", [], |r| r.get(0))?;
    if !has {
        return Ok(0.0);
    }
    let credit = credit_sql(lookup_cap(conn)?);
    let bytes: i64 = conn.query_row(
        &format!("SELECT ifnull(SUM({credit}), 0) FROM token_log WHERE tool = 'recall' AND grep = 0 AND read > 0"),
        [],
        |r| r.get(0),
    )?;
    Ok(bytes as f64 / bytes_per_token)
}

/// Adds the hooks' hit credit ([`crate::hook_hits::HitCredit::by`], bytes by
/// `hook:<event>` and day) to report rows keyed by tool (`by_day` false) or
/// by day (`by_day` true), in tokens.
pub fn add_hook_credit(
    rows: &mut [ReportRow],
    credit: &std::collections::BTreeMap<(String, String), i64>,
    by_day: bool,
    bytes_per_token: f64,
) {
    for r in rows {
        let sum: i64 = credit.iter().filter(|((tool, day), _)| if by_day { *day == r.key } else { *tool == r.key }).map(|(_, b)| *b).sum();
        r.counterfactual += sum as f64 / bytes_per_token;
    }
}

/// Totals by day, as [`report`] gives them, without the rows of the tools
/// in `without`: the human report leaves out rows whose bytes another row
/// already counts (see [`crate::token_report`]).
///
/// # Errors
///
/// [`crate::Error::Db`] on SQLite failure.
pub fn report_days_without(conn: &Connection, bytes_per_token: f64, without: &[&str]) -> Result<Vec<ReportRow>> {
    conn.execute_batch(TOKEN_TABLE)?;
    let marks = vec!["?"; without.len()].join(", ");
    let filter = if without.is_empty() { String::new() } else { format!("WHERE tool NOT IN ({marks})") };
    let credit = credit_sql(lookup_cap(conn)?);
    let mut st = conn.prepare(&format!(
        "SELECT day, COUNT(*), SUM(returned), SUM({credit}), SUM(CASE WHEN grep IS NULL THEN 0 ELSE returned END)
         FROM token_log {filter} GROUP BY day ORDER BY day"
    ))?;
    let rows = st.query_map(rusqlite::params_from_iter(without), |r| {
        Ok(ReportRow {
            key: r.get(0)?,
            answers: r.get(1)?,
            returned: r.get::<_, i64>(2)? as f64 / bytes_per_token,
            counterfactual: r.get::<_, i64>(3)? as f64 / bytes_per_token,
            returned_comparable: r.get::<_, i64>(4)? as f64 / bytes_per_token,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The recall hooks' injections, main sessions and subagents apart (#212):
/// a main session (agent id empty) lives for days and many prompts, a
/// subagent for one task, so one figure per "agent" mixed the two. A main
/// session is measured per prompt, by the turn rule of
/// [`crate::token_cost::Turns`]; a subagent per agent.
#[derive(Debug, Clone, PartialEq)]
pub struct InjectRow {
    /// Keys injected.
    pub keys: i64,
    /// Tokens injected, summed.
    pub tokens: f64,
    /// Main sessions injected into.
    pub sessions: i64,
    /// Their prompts, by the turn rule.
    pub prompts: i64,
    /// Tokens injected per prompt of a main session.
    pub per_prompt: crate::token_cost::Spread,
    /// Subagents injected into.
    pub subagents: i64,
    /// Tokens injected per subagent.
    pub per_agent: crate::token_cost::Spread,
    /// The most one main session or one subagent received in all, in tokens.
    pub largest: f64,
    /// Whether [`InjectRow::largest`] is a main session's.
    pub largest_main: bool,
}

/// The hooks' injection totals, or `None` when none are logged.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn inject_report(conn: &Connection, bytes_per_token: f64) -> Result<Option<InjectRow>> {
    conn.execute_batch(TOKEN_TABLE)?;
    let mut st = conn.prepare("SELECT session, agent, time, hook, bytes FROM hook_inject ORDER BY id")?;
    let rows = st.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?))
    })?;
    // Per (session, agent): its rows as (time, hook, bytes).
    type Rows = Vec<(String, String, i64)>;
    let mut scopes: std::collections::BTreeMap<(String, String), Rows> = std::collections::BTreeMap::new();
    let mut keys = 0i64;
    for row in rows {
        let (session, agent, time, hook, bytes) = row?;
        keys += 1;
        scopes.entry((session, agent)).or_default().push((time, hook, bytes));
    }
    if keys == 0 {
        return Ok(None);
    }
    let tok = |b: i64| b as f64 / bytes_per_token;
    let (mut per_prompt, mut per_agent) = (Vec::new(), Vec::new());
    let (mut sessions, mut prompts, mut total) = (0i64, 0i64, 0i64);
    let (mut largest, mut largest_main) = (0i64, false);
    for ((_, agent), list) in &scopes {
        let sum: i64 = list.iter().map(|r| r.2).sum();
        total += sum;
        let main = agent.is_empty();
        // Ties go to the first scope met (sessions sort before their agents).
        if sum > largest {
            (largest, largest_main) = (sum, main);
        }
        if main {
            sessions += 1;
            let turns = crate::token_cost::Turns::of(list.iter().map(|r| (r.0.as_str(), r.1.as_str())));
            if turns.is_empty() {
                continue;
            }
            prompts += turns.len() as i64;
            let mut cost = vec![0i64; turns.len()];
            for (time, _, b) in list {
                cost[turns.index(time)] += b;
            }
            per_prompt.extend(cost.into_iter().map(tok));
        } else {
            per_agent.push(tok(sum));
        }
    }
    let per_agent = crate::token_cost::spread(per_agent);
    Ok(Some(InjectRow {
        keys,
        tokens: tok(total),
        sessions,
        prompts,
        per_prompt: crate::token_cost::spread(per_prompt),
        subagents: per_agent.n as i64,
        per_agent,
        largest: tok(largest),
        largest_main,
    }))
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
    crate::store::guarded(conn, || {
        conn.prepare_cached("INSERT INTO inject_select(time, hook, key, how, bytes, full) VALUES(?1, ?2, ?3, ?4, ?5, ?6)")?.execute(params![time, hook, key, how, bytes as i64, full as i64])?;
        Ok(())
    })
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
        baseline_p90(&conn, 29_292.0);
        log_row(&conn, "recall", 400, Some(Counterfactual { grep: 0, read: 4000 })).unwrap();
        log_brief(&conn, "s", "a1", 8000).unwrap();
        log_brief(&conn, "s", "a2", 12000).unwrap();
        let (by_tool, _) = report(&conn, 4.0).unwrap();
        assert_eq!(by_tool.len(), 1);
        assert_eq!(by_tool[0].counterfactual, 1000.0);
        assert_eq!(brief_report(&conn, 4.0).unwrap(), Some(BriefRow { agents: 2, starts: 2, tokens: 5000.0, latest: 3000.0 }));
    }

    /// One agent started and resumed twice (three rows), one started once,
    /// and one main session the hooks injected into logs no brief: the
    /// brief counts two agents, each rulebook once, and the comparison
    /// holds only the agents in both tables.
    #[test]
    fn brief_and_injections_count_agents_and_a_resume_is_not_a_second_read() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        assert_eq!(brief_vs_inject(&conn, 4.0, None).unwrap(), None);
        // a1: first start at 8000 bytes, then two resumes (the rulebook grew).
        log_brief(&conn, "s", "a1", 8000).unwrap();
        log_brief(&conn, "s", "a1", 8400).unwrap();
        log_brief(&conn, "s", "a1", 8800).unwrap();
        log_brief(&conn, "s", "a2", 12000).unwrap();
        let inj = |s: &str, a: &str, b: i64| {
            conn.execute(
                "INSERT INTO hook_inject(time, session, agent, transcript, hook, key, fts, cos, bytes, read) VALUES('t', ?1, ?2, '', 'SubagentStart', 'k', NULL, NULL, ?3, 0)",
                params![s, a, b],
            )
            .unwrap();
        };
        inj("s", "a1", 400);
        inj("s", "a1", 400);
        // The main session: injected into, no brief.
        inj("s", "", 4000);
        let b = brief_report(&conn, 4.0).unwrap().unwrap();
        assert_eq!(b, BriefRow { agents: 2, starts: 4, tokens: 5000.0, latest: 3000.0 });
        assert_eq!((b.per_agent(), b.resumes()), (2500.0, 2));
        let i = inject_report(&conn, 4.0).unwrap().unwrap();
        assert_eq!((i.keys, i.sessions, i.subagents, i.tokens, i.largest, i.largest_main), (3, 1, 1, 1200.0, 1000.0, true));
        // Only a1 is in both: a2 got no injection, the main session no brief.
        // Not measured (#219): the file size is kept as `whole`, no ratio.
        let m = brief_vs_inject(&conn, 4.0, None).unwrap().unwrap();
        assert_eq!(m, MatchRow { agents: 1, rulebook: None, whole: 2000.0, injected: 200.0, start: 200.0 });
        assert_eq!(m.ratio(), None);
    }

    #[test]
    fn days_without_leave_out_only_the_named_tools() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        baseline_p90(&conn, 29_292.0);
        log_row(&conn, "recall", 400, Some(Counterfactual { grep: 0, read: 4000 })).unwrap();
        log_row(&conn, "hook", 800, None).unwrap();
        log_row(&conn, "hook:PostToolUse", 1200, None).unwrap();
        let (_, all) = report(&conn, 4.0).unwrap();
        assert_eq!((all[0].answers, all[0].returned), (3, 600.0));
        assert_eq!(report_days_without(&conn, 4.0, &[]).unwrap(), all);
        let net = report_days_without(&conn, 4.0, &["hook", "hook:PostToolUse"]).unwrap();
        assert_eq!(net.len(), 1);
        assert_eq!((net[0].answers, net[0].returned, net[0].counterfactual), (1, 100.0, 1000.0));
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

    /// A baseline whose pre-atlas p90 episode is `p90` bytes: the per-answer
    /// cap of the lookup tools (#218).
    fn baseline_p90(conn: &Connection, p90: f64) {
        let b = crate::baseline::Baseline {
            episodes: 96,
            mean: p90 / 2.0,
            median: p90 / 3.0,
            p90,
            rules: Some(1),
            ..crate::baseline::Baseline::default()
        };
        crate::baseline::save(conn, &b).unwrap();
    }

    /// A folder of code files for the `symbol` counterfactual, fresh.
    fn code_root(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atlas-symbol-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            std::fs::write(dir.join(name), text).unwrap();
        }
        dir
    }

    /// #210 (a): a name that is part of another identifier, or differs in
    /// case, is no match.
    #[test]
    fn a_symbol_matches_whole_identifiers_with_case() {
        let dir = code_root(
            "words",
            &[(
                "a.rs",
                "fn cover() {}
let overlap = 1;
fn override_x() {}
let Over = overflow;
",
            )],
        );
        let roots = vec![("r".to_string(), dir.clone())];
        assert_eq!(measure_code(&roots, "over"), Counterfactual::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #210 (b): grep prints every file that mentions the name, but only the
    /// file that defines it is read.
    #[test]
    fn a_symbol_reads_only_the_defining_file() {
        let def = "pub struct Widget {
    n: u32,
}
";
        let user = "fn f() -> Widget {
    todo!()
}
";
        let dir = code_root("def", &[("def.rs", def), ("user.rs", user)]);
        let roots = vec![("r".to_string(), dir.clone())];
        let c = measure_code(&roots, "Widget");
        assert_eq!(
            c.grep,
            "r/def.rs:1:pub struct Widget {
"
            .len()
                + "r/user.rs:1:fn f() -> Widget {
"
                .len()
        );
        assert_eq!(c.read, def.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The grep counterfactual prints at most [`GREP_LINES`] lines, as the
    /// Grep tool's head_limit does.
    #[test]
    fn grep_output_stops_at_the_head_limit() {
        let text: String = (0..300)
            .map(|_| {
                "hit
"
            })
            .collect();
        let dir = code_root("limit", &[("a.rs", &text)]);
        let roots = vec![("r".to_string(), dir.clone())];
        let c = measure_code(&roots, "hit");
        assert_eq!(
            c.grep,
            250 * "r/a.rs:1:hit
"
            .len()
                + (250 - 9)
                + (250 - 99)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #218 (c): a lookup answer whose window would be megabytes is credited
    /// at most the pre-atlas p90 episode; a tool with no cap keeps its own.
    #[test]
    fn a_lookup_answer_is_credited_at_most_the_baseline_p90() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        baseline_p90(&conn, 29_292.0);
        log_row(&conn, "search", 245, Some(Counterfactual { grep: 0, read: 6_660_000 })).unwrap();
        log_row(&conn, "search", 100, Some(Counterfactual { grep: 400, read: 0 })).unwrap();
        let (by_tool, by_day) = report(&conn, 1.0).unwrap();
        assert_eq!(by_tool[0].counterfactual, 29_292.0 + 400.0);
        assert_eq!(by_day[0].counterfactual, 29_292.0 + 400.0);
        assert_eq!(report_days_without(&conn, 1.0, &[]).unwrap()[0].counterfactual, 29_292.0 + 400.0);
        assert_eq!(crate::baseline::AtlasSide::read(&conn).unwrap().counterfactual, 29_292.0 + 400.0);
    }

    /// #218 (d): with no pre-atlas baseline the capped lookup tools are
    /// credited nothing.
    #[test]
    fn with_no_baseline_lookup_credit_is_not_counted() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        log_row(&conn, "search", 245, Some(Counterfactual { grep: 0, read: 6_660_000 })).unwrap();
        log_row(&conn, "route:Bash", 100, Some(Counterfactual { grep: 0, read: 4000 })).unwrap();
        let (by_tool, _) = report(&conn, 1.0).unwrap();
        let cf = |t: &str| by_tool.iter().find(|r| r.key == t).unwrap().counterfactual;
        assert_eq!((cf("search"), cf("route:Bash")), (0.0, 4000.0));
        assert_eq!(crate::baseline::AtlasSide::read(&conn).unwrap().counterfactual, 0.0);
    }

    /// #215 and #216 in the report: a stored routed shell credit over the
    /// cap counts at most 30,000 bytes (what the cap can be at most, read
    /// back without the text), and a route:Read row credits nothing while
    /// its calls and returned bytes still count.
    #[test]
    fn routed_rows_are_capped_and_route_read_credits_nothing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        log_row(&conn, "route:Bash", 100, Some(Counterfactual { grep: 0, read: 67_108_864 })).unwrap();
        log_row(&conn, "route:gh-pr", 100, Some(Counterfactual { grep: 0, read: 40_000 })).unwrap();
        log_row(&conn, "route:PowerShell", 100, Some(Counterfactual { grep: 0, read: 2_000 })).unwrap();
        log_row(&conn, "route:Read", 300, Some(Counterfactual { grep: 0, read: 90_000 })).unwrap();
        let (by_tool, by_day) = report(&conn, 1.0).unwrap();
        let row = |t: &str| by_tool.iter().find(|r| r.key == t).unwrap().clone();
        assert_eq!(row("route:Bash").counterfactual, 30_000.0);
        assert_eq!(row("route:gh-pr").counterfactual, 30_000.0);
        assert_eq!(row("route:PowerShell").counterfactual, 2_000.0);
        let read = row("route:Read");
        assert_eq!((read.answers, read.returned, read.counterfactual), (1, 300.0, 0.0));
        assert_eq!((by_day[0].returned, by_day[0].counterfactual), (600.0, 62_000.0));
        assert_eq!(report_days_without(&conn, 1.0, &[]).unwrap()[0].counterfactual, 62_000.0);
    }

    /// A transcript line: one Read tool call.
    fn read_use(id: &str, time: &str, path: &str, ranged: bool) -> String {
        let input = if ranged { crate::json!({"file_path": path, "offset": 10, "limit": 50}) } else { crate::json!({"file_path": path}) };
        format!(
            "{}\n",
            crate::json!({"type": "assistant", "timestamp": time, "message": {"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "Read", "input": input}]}})
        )
    }

    /// A transcript line: one tool result of `n` bytes.
    fn read_result(id: &str, time: &str, n: usize) -> String {
        format!(
            "{}\n",
            crate::json!({"type": "user", "timestamp": time, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "x".repeat(n)}]}})
        )
    }

    /// E4 (journal:ATLAS-EFFICIENCY:0006): a route:Read row is credited the
    /// output it replaced less the later Reads of the same file in the same
    /// transcript; a row logged before E4 (no session, transcript or path)
    /// is credited 0.
    #[test]
    fn route_read_credit_is_judged_against_later_reads_of_the_file() {
        let dir = std::env::temp_dir().join(format!("atlas-read-credit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (out_a, out_b) = ("C:/u/p/s/tool-results/a.txt", "C:/u/p/s/tool-results/b.txt");
        // a: digested, then read again whole (40,000 bytes back) and once
        // ranged (2,000); b: digested and never read again; another file's
        // Read and a Read of b in another transcript do not count.
        let t1 = dir.join("sess.jsonl");
        let lines = [
            read_use("r1", "2026-11-16T10:00:00Z", out_a, false),
            read_result("r1", "2026-11-16T10:00:01Z", 900),
            read_use("r2", "2026-11-16T10:00:05Z", out_b, false),
            read_result("r2", "2026-11-16T10:00:06Z", 800),
            read_use("r3", "2026-11-16T10:01:00Z", out_a, false),
            read_result("r3", "2026-11-16T10:01:01Z", 40_000),
            read_use("r4", "2026-11-16T10:02:00Z", &out_a.replace('/', "\\"), true),
            read_result("r4", "2026-11-16T10:02:01Z", 2_000),
            read_use("r5", "2026-11-16T10:03:00Z", "C:/w/other.rs", false),
            read_result("r5", "2026-11-16T10:03:01Z", 5_000),
        ];
        std::fs::write(&t1, lines.concat()).unwrap();
        let t2 = dir.join("other.jsonl");
        let later_b = [read_use("q1", "2026-11-16T10:04:00Z", out_b, false), read_result("q1", "2026-11-16T10:04:01Z", 30_000)];
        std::fs::write(&t2, later_b.concat()).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        let (s1, gone) = (t1.to_string_lossy().into_owned(), dir.join("gone.jsonl").to_string_lossy().into_owned());
        fn place<'a>(path: &'a str, id: &'a str, t: &'a str) -> ReadAt<'a> {
            ReadAt { session: "s1", transcript: t, path, tool_use: id }
        }
        log_read(&conn, 900, 40_000, &place(out_a, "r1", &s1)).unwrap();
        log_read(&conn, 800, 30_000, &place(out_b, "r2", &s1)).unwrap();
        // An old row: logged before E4, with its replaced bytes but no place.
        log_row(&conn, "route:Read", 700, Some(Counterfactual { grep: 0, read: 50_000 })).unwrap();
        // A row whose transcript is gone.
        log_read(&conn, 600, 20_000, &place(out_b, "z9", &gone)).unwrap();
        let c = read_credit(&conn).unwrap();
        assert_eq!((c.judged, c.unjudged, c.missing), (3, 1, 1));
        // a nets about 0 (40,000 replaced, 42,000 read back: credit 0);
        // b keeps its whole replaced output.
        assert_eq!((c.replaced, c.reread, c.credit), (70_000, 40_000, 30_000));
        let day = crate::store::utc(SystemTime::now())[..10].to_string();
        assert_eq!(c.by.get(&("route:Read".to_string(), day)), Some(&30_000));
        // In the report rows: added to route:Read's credit, nothing else.
        let (mut by_tool, _) = report(&conn, 1.0).unwrap();
        add_hook_credit(&mut by_tool, &c.by, false, 1.0);
        let read = by_tool.iter().find(|r| r.key == "route:Read").unwrap();
        assert_eq!((read.answers, read.counterfactual), (4, 30_000.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The columns a routed Read logs are added to an older `token_log`
    /// in place, its rows kept.
    #[test]
    fn an_old_token_log_gains_the_read_columns_and_keeps_its_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE token_log(id INTEGER PRIMARY KEY, time TEXT NOT NULL, day TEXT NOT NULL, tool TEXT NOT NULL, returned INTEGER NOT NULL, grep INTEGER, read INTEGER);
             INSERT INTO token_log(time, day, tool, returned, grep, read) VALUES('2026-11-01T00:00:00Z', '2026-11-01', 'route:Read', 300, 0, 9000);",
        )
        .unwrap();
        assert_eq!(read_credit(&conn).unwrap().unjudged, 1, "an old store is read without its columns");
        log_read(&conn, 100, 5000, &ReadAt { session: "s", transcript: "/t/x.jsonl", path: "/f", tool_use: "id" }).unwrap();
        let rows: Vec<(i64, Option<String>, Option<String>)> = conn
            .prepare("SELECT returned, session, path FROM token_log ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(300, None, None), (100, Some("s".into()), Some("/f".into()))]);
    }

    /// Running the E4 migration again (a second server, a second call) adds
    /// nothing and fails nothing; a second routed Read on a migrated old
    /// store needs no migration.
    #[test]
    fn the_read_column_migration_runs_twice_harmlessly() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE token_log(id INTEGER PRIMARY KEY, time TEXT NOT NULL, day TEXT NOT NULL, tool TEXT NOT NULL, returned INTEGER NOT NULL, grep INTEGER, read INTEGER);",
        )
        .unwrap();
        add_read_columns(&conn).unwrap();
        add_read_columns(&conn).unwrap();
        let at = ReadAt { session: "s", transcript: "/t/x.jsonl", path: "/f", tool_use: "id" };
        log_read(&conn, 100, 5000, &at).unwrap();
        log_read(&conn, 100, 5000, &at).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM token_log WHERE session = 's'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    /// A route:Read row logged at `time` (`YYYY-MM-DDTHH:MM:SSZ`).
    fn routed_at(conn: &Connection, time: &str, transcript: &str, path: &str, tool_use: &str, replaced: i64) {
        conn.execute(
            "INSERT INTO token_log(time, day, tool, returned, grep, read, session, transcript, path, tool_use) VALUES(?1, substr(?1, 1, 10), 'route:Read', 500, NULL, ?2, 's', ?3, ?4, ?5)",
            params![time, replaced, transcript, path, tool_use],
        )
        .unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atlas-read-credit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// S3 and N7: without the call's id the routed call is found by time:
    /// the first Read of the file in the row's second (the routed call comes
    /// before the hook logs its row), and a re-read later in that same
    /// second still counts. A Read before the routed call is not subtracted.
    #[test]
    fn without_an_id_the_routed_call_is_found_by_its_second() {
        let dir = scratch("time");
        let a = "C:/u/p/a.txt";
        let t = dir.join("t.jsonl");
        let lines = [
            read_use("e0", "2026-11-16T09:59:00.000Z", a, false),
            read_result("e0", "2026-11-16T09:59:00.500Z", 2_000),
            read_use("u1", "2026-11-16T10:00:00.100Z", a, false),
            read_result("u1", "2026-11-16T10:00:00.300Z", 500),
            read_use("u2", "2026-11-16T10:00:00.900Z", a, false),
            read_result("u2", "2026-11-16T10:00:00.950Z", 5_000),
            read_use("u3", "2026-11-16T10:00:30.000Z", a, true),
            read_result("u3", "2026-11-16T10:00:30.100Z", 3_000),
        ];
        std::fs::write(&t, lines.concat()).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        routed_at(&conn, "2026-11-16T10:00:00Z", &t.to_string_lossy(), a, "", 40_000);
        let c = read_credit(&conn).unwrap();
        assert_eq!((c.judged, c.reread, c.credit), (1, 8_000, 32_000), "{c:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S3 and N4: a Read before a routed row is not subtracted from it, and
    /// each re-read is subtracted once, from the routed row of its file just
    /// before it; another routed call is not a re-read. N6: a row the hook
    /// logged without its place is counted apart from the rows before E4.
    #[test]
    fn each_reread_is_subtracted_once_and_earlier_reads_never() {
        let dir = scratch("once");
        let a = "C:/u/p/a.txt";
        let t = dir.join("t.jsonl");
        let lines = [
            read_use("e0", "2026-11-16T10:00:00Z", a, false),
            read_result("e0", "2026-11-16T10:00:01Z", 7_000),
            read_use("r1", "2026-11-16T10:01:00Z", a, false),
            read_result("r1", "2026-11-16T10:01:01Z", 500),
            read_use("x1", "2026-11-16T10:02:00Z", a, true),
            read_result("x1", "2026-11-16T10:02:01Z", 4_000),
            read_use("r2", "2026-11-16T10:03:00Z", a, false),
            read_result("r2", "2026-11-16T10:03:01Z", 500),
            read_use("x2", "2026-11-16T10:04:00Z", a, true),
            read_result("x2", "2026-11-16T10:04:01Z", 6_000),
        ];
        std::fs::write(&t, lines.concat()).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        let ts = t.to_string_lossy().into_owned();
        routed_at(&conn, "2026-11-16T10:01:01Z", &ts, a, "r1", 5_000);
        routed_at(&conn, "2026-11-16T10:03:01Z", &ts, a, "r2", 40_000);
        log_read(&conn, 100, 9_000, &ReadAt { session: "", transcript: "", path: a, tool_use: "" }).unwrap();
        let c = read_credit(&conn).unwrap();
        assert_eq!((c.judged, c.unjudged, c.unplaced), (2, 0, 1), "{c:?}");
        // r1 replaced 5,000, so a second subtraction of x2 from it would show.
        assert_eq!((c.reread, c.credit), (10_000, 35_000), "r1 less x1, r2 less x2: {c:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S2: a settled transcript's verdicts are stored, so the credit stands
    /// after Claude Code deletes the transcript, and a stored verdict is
    /// never judged again; an open transcript's are not stored. A report
    /// over many rows reads each transcript once.
    #[test]
    fn a_stored_verdict_outlives_its_transcript_and_each_transcript_is_read_once() {
        let dir = scratch("stored");
        let (a, b, c3) = ("C:/u/p/a.txt", "C:/u/p/b.txt", "C:/u/p/c.txt");
        let (t1, t2) = (dir.join("t1.jsonl"), dir.join("t2.jsonl"));
        let one = [
            read_use("r1", "2026-11-16T10:00:00Z", a, false),
            read_result("r1", "2026-11-16T10:00:01Z", 500),
            read_use("r2", "2026-11-16T10:00:02Z", b, false),
            read_result("r2", "2026-11-16T10:00:03Z", 500),
            read_use("r3", "2026-11-16T10:00:04Z", c3, false),
            read_result("r3", "2026-11-16T10:00:05Z", 500),
            read_use("x1", "2026-11-16T10:01:00Z", a, false),
            read_result("x1", "2026-11-16T10:01:01Z", 10_000),
        ];
        std::fs::write(&t1, one.concat()).unwrap();
        let two = [
            read_use("q1", "2026-11-16T11:00:00Z", b, false),
            read_result("q1", "2026-11-16T11:00:01Z", 500),
            read_use("q2", "2026-11-16T11:00:02Z", a, false),
            read_result("q2", "2026-11-16T11:00:03Z", 500),
        ];
        std::fs::write(&t2, two.concat()).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        let (s1, s2) = (t1.to_string_lossy().into_owned(), t2.to_string_lossy().into_owned());
        for (t, path, id) in [(&s1, a, "r1"), (&s1, b, "r2"), (&s1, c3, "r3"), (&s2, b, "q1"), (&s2, a, "q2")] {
            log_read(&conn, 500, 30_000, &ReadAt { session: "s", transcript: t, path, tool_use: id }).unwrap();
        }
        let now = SystemTime::now();
        let open = std::time::Duration::from_secs(1_000_000);
        let first = read_credit_at(&conn, now, open).unwrap();
        assert_eq!((first.judged, first.parsed, first.stored, first.credit), (5, 2, 0, 140_000), "{first:?}");
        let again = read_credit_at(&conn, now, open).unwrap();
        assert_eq!((again.parsed, again.stored), (2, 0), "an open transcript's verdicts are not stored: {again:?}");
        let settled = read_credit_at(&conn, now + std::time::Duration::from_secs(1), std::time::Duration::ZERO).unwrap();
        assert_eq!(settled.credit, first.credit);
        std::fs::remove_file(&t1).unwrap();
        std::fs::remove_file(&t2).unwrap();
        let after = read_credit_at(&conn, now + std::time::Duration::from_secs(1), std::time::Duration::ZERO).unwrap();
        assert_eq!((after.judged, after.missing, after.parsed, after.stored), (5, 0, 0, 5), "{after:?}");
        assert_eq!((after.credit, after.by.clone()), (first.credit, first.by.clone()), "the credit does not fade");
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!((i.keys, i.sessions, i.subagents, i.tokens, i.largest, i.largest_main), (3, 2, 0, 210.0, 110.0, true));
    }

    /// #219: the brief is measured from the pre-atlas reads of the
    /// rulebook, over every pre-atlas subagent transcript, never from the
    /// file's size. Four pre-cutoff subagent transcripts: a1 reads it by a
    /// relative path joined to its working folder (8 KB), a2 with
    /// backslashes and other case (4 KB), a3 reads another file, a4 reads
    /// nothing. A main-session transcript that reads it is left out, and so
    /// is a1's read after the cutoff. 12 KB over 4 agents is 3 KB each; the
    /// file size (760 KB a start) stays the "if read whole" figure.
    #[test]
    fn the_brief_is_measured_from_pre_atlas_subagent_reads_not_the_file_size() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        let reference = Path::new("W:/work/app/.rules/RULES.md");
        // No calibration: not measured.
        assert_eq!(brief_measured(&conn, reference, 1.0).unwrap(), None);
        conn.execute_batch(crate::calibrate::TABLES).unwrap();
        assert_eq!(brief_measured(&conn, reference, 1.0).unwrap(), None);
        conn.execute(
            "INSERT INTO calibrate_state(id, cutoff, source, time) VALUES(1, '2026-10-08T19:20:26.776Z', 'the first atlas call', 'now')",
            [],
        )
        .unwrap();
        let mut seq = 0;
        let mut call = |file: &str, time: &str, cwd: &str, name: &str, path: &str, bytes: i64| {
            conn.execute("INSERT OR IGNORE INTO calibrate_files(file, cwd) VALUES(?1, ?2)", params![file, cwd]).unwrap();
            let input = format!("{{\"file_path\":\"{}\"}}", path.replace('\\', "\\\\"));
            seq += 1;
            conn.execute(
                "INSERT INTO calibrate_calls(file, seq, id, time, cwd, name, input, bytes) VALUES(?1, ?2, 'id', ?3, ?4, ?5, ?6, ?7)",
                params![file, seq, time, cwd, name, input, bytes],
            )
            .unwrap();
        };
        let sub = |a: &str| format!("C:/u/.claude/projects/W--work/s1/subagents/agent-{a}.jsonl");
        let before = "2026-10-01T10:00:00.000Z";
        call(&sub("a1"), before, "W:\\work\\app", "Read", ".rules/RULES.md", 8000);
        call(&sub("a1"), "2026-10-09T10:00:00.000Z", "W:\\work\\app", "Read", "W:/work/app/.rules/RULES.md", 9000);
        call(&sub("a2"), before, "W:/work", "Read", "w:\\WORK\\App\\.rules\\rules.md", 4000);
        call(&sub("a3"), before, "W:/work", "Read", "W:/work/app/docs/NOTES.md", 5000);
        call(&sub("a4"), before, "W:/work", "Bash", "", 100);
        // A main session reads it: not a subagent, left out.
        call("C:/u/.claude/projects/W--work/s1.jsonl", before, "W:/work", "Read", "W:/work/app/.rules/RULES.md", 7000);
        // A subagent whose every call is after the cutoff: not pre-atlas.
        call(&sub("a5"), "2026-10-10T10:00:00.000Z", "W:/work", "Read", "W:/work/app/.rules/RULES.md", 6000);
        let m = brief_measured(&conn, reference, 1.0).unwrap().unwrap();
        assert_eq!(m, BriefMeasure { per_agent: 3000.0, reads: 2, readers: 2, transcripts: 4, cutoff: "2026-10-08T19:20:26.776Z".into() });
        // Another reference: no read of it, not measured.
        assert_eq!(brief_measured(&conn, Path::new("W:/other/RULES.md"), 1.0).unwrap(), None);
        // The brief at 760 KB a start: the file size, kept as "if read whole".
        for a in ["a1", "a2"] {
            log_brief(&conn, "s", a, 760_000).unwrap();
            conn.execute(
                "INSERT INTO hook_inject(time, session, agent, transcript, hook, key, fts, cos, bytes, read) VALUES('t', 's', ?1, '', 'SubagentStart', 'k', NULL, NULL, 1500, 0)",
                params![a],
            )
            .unwrap();
        }
        let b = brief_report(&conn, 1.0).unwrap().unwrap();
        assert_eq!(b.per_agent(), 760_000.0);
        let x = brief_vs_inject(&conn, 1.0, Some(&m)).unwrap().unwrap();
        assert_eq!(x, MatchRow { agents: 2, rulebook: Some(6000.0), whole: 1_520_000.0, injected: 3000.0, start: 3000.0 });
        assert_eq!(x.ratio(), Some(2.0));
        // Not measured: no rulebook and no ratio; the file size is not one.
        let y = brief_vs_inject(&conn, 1.0, None).unwrap().unwrap();
        assert_eq!((y.rulebook, y.ratio(), y.whole), (None, None, 1_520_000.0));
    }

    /// #212: main sessions per prompt and subagents per agent, apart. One
    /// main session with 10 prompts at 1k tokens each, three subagents at
    /// 2k, 3k and 4k: per prompt p50 and max 1k, per subagent p50 3k and
    /// max 4k, and the largest is the main session at 10k.
    #[test]
    fn injections_split_main_sessions_per_prompt_from_subagents_per_agent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TOKEN_TABLE).unwrap();
        let inj = |time: &str, agent: &str, hook: &str, bytes: i64| {
            conn.execute(
                "INSERT INTO hook_inject(time, session, agent, transcript, hook, key, fts, cos, bytes, read) VALUES(?1, 'main', ?2, '', ?3, 'k', NULL, NULL, ?4, 0)",
                params![time, agent, hook, bytes],
            )
            .unwrap();
        };
        for p in 0..10 {
            // Each prompt: 3000 bytes at the prompt, 1000 by a tool call after it.
            inj(&format!("2026-11-15T10:{p:02}:00.000Z"), "", "UserPromptSubmit", 3000);
            inj(&format!("2026-11-15T10:{p:02}:30.000Z"), "", "PreToolUse", 1000);
        }
        for (a, kb) in [("a1", 2), ("a2", 3), ("a3", 4)] {
            inj("2026-11-15T10:05:10.000Z", a, "SubagentStart", kb * 4000);
        }
        let i = inject_report(&conn, 4.0).unwrap().unwrap();
        assert_eq!((i.keys, i.sessions, i.prompts, i.subagents), (23, 1, 10, 3));
        assert_eq!((i.per_prompt.n, i.per_prompt.p50, i.per_prompt.max), (10, 1000.0, 1000.0));
        assert_eq!((i.per_agent.n, i.per_agent.p50, i.per_agent.max), (3, 3000.0, 4000.0));
        assert_eq!((i.largest, i.largest_main), (10_000.0, true));
        assert_eq!(i.tokens, 19_000.0);
    }

    /// Review of #277, finding 1: the recall answers that read records
    /// (grep 0, a read) are told apart from recall by query (grep first)
    /// and from a query that matched nothing, and are capped as the report
    /// caps them.
    #[test]
    fn recall_by_record_credit_is_the_recall_rows_with_a_read_and_no_grep() {
        let conn = Connection::open_in_memory().unwrap();
        // No ledger yet: nothing, and no table made.
        assert_eq!(recall_records_credit(&conn, 4.0).unwrap(), 0.0);
        conn.execute_batch(TOKEN_TABLE).unwrap();
        // No calibration: the capped tools are credited nothing.
        log_row(&conn, "recall", 100, Some(Counterfactual { grep: 0, read: 4000 })).unwrap();
        assert_eq!(recall_records_credit(&conn, 4.0).unwrap(), 0.0);
        conn.execute_batch(
            "CREATE TABLE baseline_log(id INTEGER PRIMARY KEY, time TEXT NOT NULL, before TEXT NOT NULL, transcripts INTEGER NOT NULL, used INTEGER NOT NULL, episodes INTEGER NOT NULL, mean REAL NOT NULL, median REAL NOT NULL, p90 REAL NOT NULL, total INTEGER NOT NULL);
             INSERT INTO baseline_log(time, before, transcripts, used, episodes, mean, median, p90, total) VALUES('2026-11-08T11:40:52Z', '2026-10-08T19:20:49Z', 10, 5, 50, 5000.0, 3000.0, 10000.0, 250000);",
        )
        .unwrap();
        // By key, capped at the p90 episode (10,000 bytes).
        log_row(&conn, "recall", 100, Some(Counterfactual { grep: 0, read: 40_000 })).unwrap();
        // By query, a query that matched nothing, a handle slice, and a
        // read_note whole file: none of them count.
        log_row(&conn, "recall", 100, Some(Counterfactual { grep: 300, read: 2000 })).unwrap();
        log_row(&conn, "recall", 100, Some(Counterfactual { grep: 0, read: 0 })).unwrap();
        log_row(&conn, "recall", 100, None).unwrap();
        log_row(&conn, "read_note", 100, Some(Counterfactual { grep: 0, read: 8000 })).unwrap();
        assert_eq!(recall_records_credit(&conn, 4.0).unwrap(), (4000.0 + 10_000.0) / 4.0);
    }
}
