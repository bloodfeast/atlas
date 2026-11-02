//! The atlas layer of the store: every vault note and every read-only
//! source file as a *document*, cut into sections, with the identifier
//! registry over them; freshness checks; and the durable misses log.
//!
//! The tables here are derived (rebuilt from the files) except `misses`,
//! which is never dropped and is mirrored line for line to `misses.jsonl`
//! beside the database. The derived tables are wiped and rebuilt whenever the
//! rules that produced them change: the identifier patterns, the sources,
//! the vault set, or the set of names (see [`crate::registry::Names`]); a
//! fingerprint of all of them is kept in `meta`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::regex::Regex;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use crate::json::Value;

use super::{begin, RescanStats, Store, MAX_INDEX};
use crate::chunk::{self, ChunkRule};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::markdown;
use crate::paths::{self, Found, Vaults};
use crate::registry::{self, DocInfo, Names, PatternSpec, Rules};
use crate::sha256;
use crate::sources::Source;
use crate::watch::{Snapshot, Watcher};

/// Bumped whenever the atlas tables change shape or meaning. atlas-7: a
/// leading ISO date on a list item is markup, so dated rulings define their
/// identifiers (the rows of an atlas-6 index are re-derived).
const ATLAS_SCHEMA: &str = "atlas-7";

/// File name of the append-only JSONL mirror of the `misses` table.
pub const MISSES_LOG: &str = "misses.jsonl";

/// The most text a definition or section result carries, in bytes.
pub const MAX_SECTION_TEXT: usize = 8 << 10;

/// The most definitions a lookup returns.
pub const MAX_DEFINITIONS: usize = 50;

pub(super) const ATLAS_DERIVED: &str = "
CREATE TABLE IF NOT EXISTS docs(
    id INTEGER PRIMARY KEY,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    path TEXT NOT NULL,
    mtime_ns INTEGER NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    doc_type TEXT,
    date TEXT,
    indexed TEXT NOT NULL,
    UNIQUE(source, path)
);
CREATE TABLE IF NOT EXISTS sections(
    id INTEGER PRIMARY KEY,
    doc_id INTEGER NOT NULL,
    ord INTEGER NOT NULL,
    level INTEGER NOT NULL,
    heading_path TEXT NOT NULL,
    label TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    date TEXT,
    sha TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS sections_doc ON sections(doc_id, ord);
CREATE VIRTUAL TABLE IF NOT EXISTS sections_fts USING fts5(heading_path, body, tokenize = 'unicode61');
CREATE TABLE IF NOT EXISTS idents(
    doc_id INTEGER NOT NULL,
    ident TEXT NOT NULL COLLATE NOCASE,
    kind TEXT NOT NULL,
    def TEXT,
    rank INTEGER NOT NULL,
    section_id INTEGER,
    line INTEGER NOT NULL,
    date TEXT,
    context TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idents_ident ON idents(ident, rank);
CREATE INDEX IF NOT EXISTS idents_doc ON idents(doc_id);
CREATE TABLE IF NOT EXISTS ident_stats(
    ident TEXT NOT NULL COLLATE NOCASE,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    defs INTEGER NOT NULL,
    occ INTEGER NOT NULL,
    last TEXT,
    PRIMARY KEY(ident, kind, source)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS ident_dirty(
    ident TEXT NOT NULL COLLATE NOCASE,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    PRIMARY KEY(ident, kind, source)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS lookups(
    fid TEXT NOT NULL,
    rank INTEGER NOT NULL,
    date TEXT NOT NULL,
    mtime_ns INTEGER NOT NULL,
    source TEXT NOT NULL,
    path TEXT NOT NULL,
    line INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    doc_id INTEGER NOT NULL,
    doc_kind TEXT NOT NULL,
    ident TEXT NOT NULL,
    kind TEXT NOT NULL,
    def TEXT,
    heading_path TEXT NOT NULL,
    section_start INTEGER NOT NULL,
    section_end INTEGER NOT NULL,
    context TEXT NOT NULL,
    section_id INTEGER,
    n INTEGER NOT NULL,
    text TEXT,
    PRIMARY KEY(fid, rank DESC, date DESC, mtime_ns DESC, source, path, line, seq)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS lookups_doc ON lookups(doc_id);
CREATE TABLE IF NOT EXISTS lookup_counts(
    fid TEXT PRIMARY KEY,
    defs INTEGER NOT NULL,
    lines INTEGER NOT NULL,
    total INTEGER NOT NULL
) WITHOUT ROWID;
";

/// Takes a document's rows out of `lookup_counts` before its `lookups` rows
/// are deleted.
const UNCOUNT_DOC: &str = "UPDATE lookup_counts SET defs = defs - x.d, lines = lines - x.l, total = total - x.t
     FROM (SELECT fid, SUM(rank > 0) AS d, SUM(rank = 0) AS l, SUM(CASE WHEN rank = 0 THEN n ELSE 0 END) AS t
           FROM lookups WHERE doc_id = ?1 GROUP BY fid) AS x
     WHERE lookup_counts.fid = x.fid";

/// Adds a document's rows to `lookup_counts`.
const COUNT_DOC: &str = "INSERT INTO lookup_counts(fid, defs, lines, total) VALUES(?1, ?2, ?3, ?4)
     ON CONFLICT(fid) DO UPDATE SET defs = defs + excluded.defs, lines = lines + excluded.lines, total = total + excluded.total";

/// Marks the identifiers of a document for a recount in `ident_stats`.
const MARK_DIRTY: &str = "INSERT OR IGNORE INTO ident_dirty(ident, kind, source)
     SELECT i.ident, i.kind, d.source FROM idents i JOIN docs d ON d.id = i.doc_id WHERE i.doc_id = ?1";

/// Recounts the marked identifiers: `ident_stats` holds, per identifier,
/// kind and source, the definitions, occurrences and newest date, so `ids`
/// reads a few hundred summary rows instead of grouping every occurrence.
const RECOUNT: &str = "
DELETE FROM ident_stats WHERE (ident, kind, source) IN (SELECT ident, kind, source FROM ident_dirty);
INSERT INTO ident_stats(ident, kind, source, defs, occ, last)
    SELECT MIN(i.ident), i.kind, d.source, SUM(i.rank > 0), COUNT(*), MAX(i.date)
    FROM ident_dirty x JOIN idents i ON i.ident = x.ident AND i.kind = x.kind JOIN docs d ON d.id = i.doc_id AND d.source = x.source
    GROUP BY i.ident, i.kind, d.source;
DELETE FROM ident_dirty;
";

pub(super) const ATLAS_DURABLE: &str = "
CREATE TABLE IF NOT EXISTS misses(
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    tool TEXT NOT NULL,
    query TEXT NOT NULL,
    returned TEXT NOT NULL,
    found TEXT NOT NULL,
    reason TEXT NOT NULL,
    client TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open',
    resolved_time TEXT,
    resolution TEXT
);
";

/// The sectioning rule for vault notes: every heading.
pub(super) static VAULT_CHUNK: ChunkRule = ChunkRule { top_level_items: false, max_level: 6 };

const ATLAS_DROP: &str =
    "DELETE FROM docs; DELETE FROM sections; DELETE FROM sections_fts; DELETE FROM idents; DELETE FROM ident_stats; DELETE FROM ident_dirty; DELETE FROM lookups; DELETE FROM lookup_counts;";

/// Everything the atlas layer is configured with.
#[derive(Debug, Clone)]
pub struct AtlasSetup {
    /// The read-only sources.
    pub sources: Vec<Source>,
    /// The compiled identifier rules.
    pub rules: Rules,
    /// The patterns as configured (kept for the fingerprint).
    pub specs: Vec<PatternSpec>,
    /// Frontmatter keys naming a vault note's identifier.
    pub vault_id_keys: Vec<String>,
    /// See [`Config::refresh_limit`].
    pub refresh_limit: usize,
    /// Watch the roots for changes, so a check reads only the dirty set
    /// (see [`crate::watch`]). Off by default for a bare setup (tests
    /// change files and query at once); on by default from a configuration.
    pub watch: bool,
    /// How long a watched root goes before it is walked anyway.
    pub rescan_interval: Duration,
    /// The index file of each source that lists names, as (source, path
    /// below its root): a memory folder's `MEMORY.md` lists the memories
    /// and is not one, so its stem is no name (else every "memory" in the
    /// corpus would be an identifier). From the memory exports' `index`.
    pub name_indexes: Vec<(String, String)>,
}

impl Default for AtlasSetup {
    fn default() -> Self {
        AtlasSetup {
            sources: Vec::new(),
            rules: Rules { patterns: Vec::new(), wikilinks: true },
            specs: Vec::new(),
            vault_id_keys: vec!["id".to_string()],
            refresh_limit: crate::config::DEFAULT_REFRESH_LIMIT,
            watch: false,
            rescan_interval: Duration::from_secs(crate::config::DEFAULT_RESCAN_INTERVAL_S),
            name_indexes: Vec::new(),
        }
    }
}

impl AtlasSetup {
    /// Builds the setup from a configuration.
    ///
    /// # Arguments
    ///
    /// * `c` - the parsed configuration.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a bad source (see [`Source::new`]) or
    /// pattern (see [`Rules::new`]).
    pub fn from_config(c: &Config) -> Result<AtlasSetup> {
        let mut sources = Vec::with_capacity(c.sources.len());
        for s in &c.sources {
            sources.push(Source::new(
                &s.name,
                &s.root,
                s.include.clone(),
                s.exclude.clone(),
                s.chunk.clone(),
                s.id_keys.clone().unwrap_or_else(|| vec!["id".to_string()]),
                s.names_kind.clone(),
            )?);
        }
        let wikilinks = c.wikilinks.unwrap_or(true);
        // A memory export syncs from the source of its name; its index file
        // is that source's list of the names, not a name.
        let name_indexes = c
            .memory
            .iter()
            .flat_map(|m| m.exports.iter())
            .filter(|x| x.shape == "memory" && c.sources.iter().any(|s| s.name == x.name && s.names_kind.is_some()))
            .filter_map(|x| Some((x.name.clone(), x.index.clone()?)))
            .collect();
        Ok(AtlasSetup {
            sources,
            rules: Rules::new(&c.ids, wikilinks)?,
            specs: c.ids.clone(),
            vault_id_keys: c.vault_id_keys.clone().unwrap_or_else(|| vec!["id".to_string()]),
            refresh_limit: c.refresh_limit(),
            watch: c.watch.unwrap_or(true),
            rescan_interval: Duration::from_secs(c.rescan_interval_s.map(|n| n as u64).unwrap_or(crate::config::DEFAULT_RESCAN_INTERVAL_S)),
            name_indexes,
        })
    }

    /// Whether `rel` is the index file of names source `source` (see
    /// [`AtlasSetup::name_indexes`]).
    pub fn is_name_index(&self, source: &str, rel: &str) -> bool {
        self.name_indexes.iter().any(|(s, p)| s == source && p.eq_ignore_ascii_case(rel))
    }
}

/// The atlas state a [`Store`] carries.
#[derive(Debug)]
pub(super) struct Atlas {
    pub(super) setup: AtlasSetup,
    pub(super) names: Names,
    /// When this process last walked each source or vault.
    pub(super) last_scan: HashMap<String, SystemTime>,
    pub(super) misses_log: PathBuf,
    /// The change watcher over every root, vaults first then sources, in
    /// the order of [`Store::atlas_names`]; `None` when watching is off.
    pub(super) watch: Option<Watcher>,
    /// When each root was last walked whole while watched: the baseline the
    /// dirty set is relative to.
    pub(super) walked: HashMap<String, Instant>,
    /// The indexed path maps a full walk compares against, kept between
    /// walks while the database is unchanged.
    pub(super) known: std::cell::RefCell<KnownCache>,
    /// Why this process found the derived rows wiped (`schema rebuild`,
    /// `index rules changed`), if it did; the freshness summary names it.
    pub(super) rebuilt: Option<&'static str>,
}

/// A path and its indexed (mtime, size, sha256).
pub(super) type Known = HashMap<String, (i64, i64, String)>;

/// The vault side of [`KnownCache`]: every served note keyed by (vault,
/// path), and the notes whose atlas row is missing or out of step.
pub(super) type VaultKnown = (HashMap<(String, String), (i64, i64, String)>, HashSet<(String, String)>);

/// The indexed path maps of the last full walks, so a root without a watch
/// does not read every row of it back from SQLite on every walk. Valid while
/// [`db_stamp`] is unchanged: no commit by another connection
/// (`PRAGMA data_version`) and no row changed by this one
/// (`sqlite3_total_changes`). Anything else drops every map.
#[derive(Debug)]
pub(super) struct KnownCache {
    /// False to rebuild every map on every walk (the bench's "before").
    pub(super) on: bool,
    stamp: Option<(i64, u64)>,
    sources: HashMap<String, std::sync::Arc<Known>>,
    pub(super) vaults: Option<std::sync::Arc<VaultKnown>>,
}

impl Default for KnownCache {
    fn default() -> Self {
        KnownCache { on: true, stamp: None, sources: HashMap::new(), vaults: None }
    }
}

impl KnownCache {
    /// Drops every map when the database changed since they were read, and
    /// reports whether the cache may be used at all.
    pub(super) fn validate(&mut self, conn: &Connection) -> Result<bool> {
        if !self.on {
            return Ok(false);
        }
        let stamp = db_stamp(conn)?;
        if self.stamp != Some(stamp) {
            self.sources.clear();
            self.vaults = None;
            self.stamp = Some(stamp);
        }
        Ok(true)
    }
}

/// What changes when the database does: `PRAGMA data_version` (commits by
/// other connections) and this connection's total row changes.
pub(super) fn db_stamp(conn: &Connection) -> Result<(i64, u64)> {
    let v: i64 = conn.prepare_cached("PRAGMA data_version")?.query_row([], |r| r.get(0))?;
    Ok((v, conn.total_changes()))
}

impl Plan {
    /// The same root, walked whole: what a skip or a dirty set becomes when
    /// a walk happens anyway.
    pub(super) fn into_walk(self) -> Plan {
        match self {
            Plan::Skip(i, seq) => Plan::Walk(Some((i, seq))),
            Plan::Paths(i, snap) => Plan::Walk(Some((i, snap.seq))),
            w => w,
        }
    }
}

/// What a check of one root has to do.
#[derive(Debug)]
pub(super) enum Plan {
    /// Watched, nothing changed: skip it (root index, sequence seen).
    Skip(usize, u64),
    /// Watched: only these paths changed.
    Paths(usize, Snapshot),
    /// Walk it whole; the watcher root and the sequence seen before the
    /// walk, to clear once the walk is applied.
    Walk(Option<(usize, u64)>),
}

/// How a query treats files that changed on disk since they were indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Check, and re-index stale files on the spot when there are at most
    /// the refresh limit of them.
    Auto,
    /// Check and report, but do not re-index.
    Check,
    /// Neither check nor re-index; report only the last scan times.
    Off,
}

impl Refresh {
    /// Parses `auto`, `check` or `off`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for anything else.
    pub fn parse(s: &str) -> Result<Refresh> {
        match s {
            "auto" => Ok(Refresh::Auto),
            "check" => Ok(Refresh::Check),
            "off" => Ok(Refresh::Off),
            o => Err(Error::InvalidArgument(format!("refresh {o:?}; use auto, check or off"))),
        }
    }
}

/// A file whose indexed copy differs from the disk.
#[derive(Debug, Clone)]
pub struct StaleFile {
    /// Path below the source or vault root.
    pub path: String,
    /// `changed` (content hash differs), `new` (not indexed yet) or
    /// `removed` (indexed but gone).
    pub state: &'static str,
}

crate::impl_to_json!(StaleFile { path, state });

/// How current the index is for one source or vault.
#[derive(Debug, Clone)]
pub struct Freshness {
    /// Source or vault short name.
    pub source: String,
    /// `vault` or `source`.
    pub kind: &'static str,
    /// Files indexed for it.
    pub files: usize,
    /// When this process last walked it (UTC), if it has.
    pub last_scan: Option<String>,
    /// Newest time any of its files was indexed (UTC).
    pub last_indexed: Option<String>,
    /// True when the disk was compared with the index for this answer.
    pub checked: bool,
    /// Files that differed from the index, each by name while there are at
    /// most [`STALE_LIST_CAP`] of them; empty past the cap, where
    /// `stale_count` and `reason` stand for the list.
    pub stale: Vec<StaleFile>,
    /// How many files differed from the index.
    pub stale_count: usize,
    /// Past the cap, one phrase for the whole set: why it is stale when
    /// known (`schema rebuild`, `first index`), else the count per state.
    pub reason: Option<String>,
    /// True when the stale files were re-indexed before the answer was
    /// computed, so the answer reflects them.
    pub refreshed: bool,
}

crate::impl_to_json!(Freshness { source, kind, files, last_scan, last_indexed, checked, stale, stale_count, reason, refreshed });

/// The most stale files a freshness entry names one by one. Past it the
/// entry is a count and a reason, so a rebuild of a thousand files does not
/// put a thousand lines into every answer.
pub const STALE_LIST_CAP: usize = 8;

/// One identifier occurrence, as `lookup` returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct IdHit {
    /// Source or vault short name.
    pub source: String,
    /// `vault` or `source`.
    pub source_kind: String,
    /// Path below the root.
    pub path: String,
    /// 1-based line.
    pub line: i64,
    /// The identifier as written here.
    pub id: String,
    /// The identifier kind.
    pub kind: String,
    /// Why this is a definition (`frontmatter`, `file`, `heading`,
    /// `table_row`, `list_item`), or absent for a mention.
    pub def: Option<String>,
    /// The date on the line, else of its section, else of its document.
    pub date: Option<String>,
    /// The section's heading path.
    pub heading_path: String,
    /// First line of the section.
    pub section_start: i64,
    /// Last line of the section.
    pub section_end: i64,
    /// The line, trimmed and capped.
    pub context: String,
    /// For a definition: the defining text (the section, the list item or
    /// the row), capped at [`MAX_SECTION_TEXT`].
    pub text: Option<String>,
    /// How many occurrences this line stands for: mentions in one
    /// section are merged into the first, with this count.
    pub count: i64,
    section_id: Option<i64>,
}

crate::impl_to_json!(IdHit { source, source_kind, path, line, id, kind, date, heading_path, section_start, section_end, context, count } skip_none { def, text });

/// Which definitions of a lookup carry their defining text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefText {
    /// None of them.
    None,
    /// The strongest one (what the plain-text answer shows).
    First,
    /// Every one returned.
    All,
}

/// The answer to a lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct Lookup {
    /// Definitions, strongest first, then newest.
    pub definitions: Vec<IdHit>,
    /// Mentions, newest first where dates exist.
    pub mentions: Vec<IdHit>,
    /// All mentions, including those past the limit.
    pub total_mentions: usize,
    /// Merged mention lines (one per section), including those past the
    /// limit.
    pub mention_lines: usize,
}

crate::impl_to_json!(Lookup { definitions, mentions, total_mentions, mention_lines });

/// One identifier in an `ids` listing.
#[derive(Debug, Clone)]
pub struct IdSummary {
    /// The identifier.
    pub id: String,
    /// Its kind.
    pub kind: String,
    /// How many definitions.
    pub definitions: i64,
    /// How many occurrences in all (definitions included).
    pub occurrences: i64,
    /// The newest date of any occurrence.
    pub last_date: Option<String>,
}

crate::impl_to_json!(IdSummary { id, kind, definitions, occurrences, last_date });

/// One section search hit.
#[derive(Debug, Clone)]
pub struct SectionHit {
    /// The section's row id.
    pub id: i64,
    /// Source or vault short name.
    pub source: String,
    /// `vault` or `source`.
    pub source_kind: String,
    /// Path below the root.
    pub path: String,
    /// The headings above and including this section.
    pub heading_path: String,
    /// The heading text or the item's first line.
    pub label: String,
    /// First line, 1-based.
    pub line_start: i64,
    /// Last line, inclusive.
    pub line_end: i64,
    /// The section's date, if it has one.
    pub date: Option<String>,
    /// The section text, capped at [`MAX_SECTION_TEXT`].
    pub text: String,
    /// True when `text` was cut.
    pub truncated: bool,
}

crate::impl_to_json!(SectionHit { source, source_kind, path, heading_path, label, line_start, line_end, date, text, truncated });

/// One indexed source or vault, for `list_sources`.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    /// Short name.
    pub name: String,
    /// `vault` or `source`.
    pub kind: &'static str,
    /// The canonical root.
    pub root: String,
    /// Include globs (sources only).
    pub include: Vec<String>,
    /// Files indexed.
    pub files: usize,
    /// Sections indexed.
    pub sections: usize,
    /// When this process last walked it.
    pub last_scan: Option<String>,
    /// Newest index time of any file.
    pub last_indexed: Option<String>,
}

/// A recorded miss: something grep found that atlas did not return.
#[derive(Debug, Clone)]
pub struct Miss {
    /// Stable id.
    pub id: i64,
    /// UTC time it was reported.
    pub time: String,
    /// The atlas tool that missed (`lookup`, `ids`, `search`).
    pub tool: String,
    /// What was asked.
    pub query: String,
    /// What atlas returned.
    pub returned: String,
    /// Where grep (or a reader) found it.
    pub found: String,
    /// Free text: why atlas missed, if known.
    pub reason: String,
    /// The MCP client that reported it.
    pub client: String,
    /// `open` or `resolved`.
    pub status: String,
    /// When it was resolved.
    pub resolved_time: Option<String>,
    /// The resolution note.
    pub resolution: Option<String>,
}

crate::impl_to_json!(Miss { id, time, tool, query, returned, found, reason, client, status, resolved_time, resolution });

impl crate::json::ToJson for SourceInfo {
    fn to_json(&self) -> Value {
        let mut m = crate::json::Map::new();
        m.insert("name".into(), self.name.to_json());
        m.insert("kind".into(), self.kind.to_json());
        m.insert("root".into(), self.root.to_json());
        if !self.include.is_empty() {
            m.insert("include".into(), self.include.to_json());
        }
        m.insert("files".into(), self.files.to_json());
        m.insert("sections".into(), self.sections.to_json());
        m.insert("last_scan".into(), self.last_scan.to_json());
        m.insert("last_indexed".into(), self.last_indexed.to_json());
        Value::Object(m)
    }
}

impl crate::json::FromJson for Miss {
    /// Reads a miss back from the JSONL mirror.
    fn from_json(v: &Value) -> std::result::Result<Miss, String> {
        let s = |k: &str| v[k].as_str().map(str::to_string).ok_or_else(|| format!("miss.{k} must be a string"));
        let o = |k: &str| v[k].as_str().map(str::to_string);
        Ok(Miss {
            id: v["id"].as_i64().ok_or("miss.id must be an integer")?,
            time: s("time")?,
            tool: s("tool")?,
            query: s("query")?,
            returned: s("returned")?,
            found: s("found")?,
            reason: s("reason")?,
            client: s("client")?,
            status: s("status")?,
            resolved_time: o("resolved_time"),
            resolution: o("resolution"),
        })
    }
}

/// A UTC timestamp, `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn utc(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

fn now() -> String {
    utc(SystemTime::now())
}

/// A date at the start of a frontmatter value (`2026-09-10T03:55:40Z`
/// gives `2026-09-10`).
fn leading_date(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    let head = s.get(..10)?;
    chunk::find_date(head).filter(|d| *d == head).map(str::to_string)
}

fn cap_text(s: &str) -> (String, bool) {
    if s.len() <= MAX_SECTION_TEXT {
        (s.to_string(), false)
    } else {
        (chunk::cap(s, MAX_SECTION_TEXT).to_string(), true)
    }
}

impl Atlas {
    pub(super) fn new(setup: AtlasSetup, db_path: &Path) -> Atlas {
        Atlas {
            setup,
            names: Names::default(),
            last_scan: HashMap::new(),
            misses_log: db_path.with_file_name(MISSES_LOG),
            watch: None,
            walked: HashMap::new(),
            known: Default::default(),
            rebuilt: None,
        }
    }

    /// Starts the watcher over the vault and source roots, when watching
    /// is on.
    pub(super) fn start_watch(&mut self, vaults: &Vaults) {
        if !self.setup.watch {
            return;
        }
        let roots: Vec<PathBuf> =
            vaults.all().iter().map(|v| v.root.clone()).chain(self.setup.sources.iter().map(|s| s.root.clone())).collect();
        self.watch = Some(Watcher::start(&roots));
    }

    /// What a check of `name` (watcher root `index`) has to do.
    pub(super) fn plan(&self, name: &str, index: usize) -> Plan {
        let Some(w) = &self.watch else { return Plan::Walk(None) };
        let snap = w.snapshot(index);
        if !snap.healthy {
            return Plan::Walk(None);
        }
        let due = self.walked.get(name).is_none_or(|t| t.elapsed() >= self.setup.rescan_interval);
        if due || snap.full {
            return Plan::Walk(Some((index, snap.seq)));
        }
        if snap.paths.is_empty() {
            return Plan::Skip(index, snap.seq);
        }
        Plan::Paths(index, snap)
    }

    /// Records that a root was walked (or its dirty paths applied) as of
    /// the plan, clearing what the watcher reported up to then.
    pub(super) fn settled(&mut self, name: &str, plan: &Plan, walked_at: Instant) {
        let Some(w) = &self.watch else { return };
        match plan {
            Plan::Walk(Some((i, seq))) => {
                w.clear(*i, *seq);
                self.walked.insert(name.to_string(), walked_at);
            }
            Plan::Paths(i, snap) => w.clear(*i, snap.seq),
            _ => {}
        }
    }

    pub(super) fn source(&self, name: &str) -> Option<&Source> {
        self.setup.sources.iter().find(|s| s.name == name)
    }

    /// Walks the sources that list names and collects their stems.
    pub(super) fn compute_names(&self) -> Names {
        let mut names = Names::default();
        for s in &self.setup.sources {
            if let Some(kind) = &s.names_kind {
                if let Ok(files) = s.walk() {
                    for f in files.iter().filter(|f| !self.setup.is_name_index(&s.name, &f.rel)) {
                        names.insert(stem_of(&f.rel), kind);
                    }
                }
            }
        }
        names
    }

    /// A hash of everything that decides the derived atlas rows.
    pub(super) fn fingerprint(&self, vaults: &Vaults) -> String {
        let mut s = String::from(ATLAS_SCHEMA);
        s.push_str(&format!("{:?}", self.setup.specs));
        s.push_str(&format!("|wl={}|vk={:?}|ni={:?}", self.setup.rules.wikilinks, self.setup.vault_id_keys, self.setup.name_indexes));
        for v in vaults.all() {
            s.push_str(&format!("|v:{}={}", v.name, v.root.display()));
        }
        for src in &self.setup.sources {
            s.push_str(&format!(
                "|s:{}={}|{:?}|{:?}|{:?}|{:?}|{:?}",
                src.name,
                src.root.display(),
                src.include,
                src.exclude,
                src.chunk,
                src.id_keys,
                src.names_kind
            ));
        }
        s.push_str("|names:");
        for n in self.names.sorted() {
            s.push_str(n);
            s.push(',');
        }
        sha256::hex(s.as_bytes())
    }
}

fn stem_of(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name)
}

/// Drops derived tables whose shape changed, before `ATLAS_DERIVED` creates
/// them again (`CREATE TABLE IF NOT EXISTS` leaves an old shape alone). The
/// rows are derived; the fingerprint, which names the schema, then wipes the
/// rest and the next scan rebuilds them.
pub(super) fn migrate_derived(tx: &Transaction<'_>) -> Result<()> {
    let has_text: bool =
        tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('lookups') WHERE name = 'text')", [], |r| r.get(0))?;
    let has_table: bool =
        tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'lookups')", [], |r| r.get(0))?;
    if has_table && !has_text {
        tx.execute_batch("DROP TABLE lookups; DELETE FROM lookup_counts;")?;
    }
    // atlas-6 keys each section by the hash of its text (the vector cache
    // key, see `crate::semantic`); an older table gains the column empty and
    // the fingerprint change rebuilds every row.
    let has_sections: bool =
        tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sections')", [], |r| r.get(0))?;
    let has_sha: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('sections') WHERE name = 'sha')", [], |r| r.get(0))?;
    if has_sections && !has_sha {
        tx.execute_batch("ALTER TABLE sections ADD COLUMN sha TEXT NOT NULL DEFAULT '';")?;
    }
    Ok(())
}

/// Checks the stored fingerprint and wipes the derived atlas rows when it
/// changed.
///
/// # Returns
///
/// `None` when nothing was wiped; otherwise why: `first index` (no earlier
/// fingerprint), `schema rebuild` (the derived schema version changed) or
/// `index rules changed` (patterns, sources, vaults or names).
pub(super) fn ensure_fingerprint(tx: &Transaction<'_>, fp: &str) -> Result<Option<&'static str>> {
    let get = |k: &str| -> Result<Option<String>> { Ok(tx.query_row("SELECT value FROM meta WHERE key=?1", [k], |r| r.get(0)).optional()?) };
    let old = get("atlas")?;
    if old.as_deref() == Some(fp) {
        return Ok(None);
    }
    let why = match (&old, get("atlas_schema")?) {
        (None, _) => "first index",
        (Some(_), Some(v)) if v == ATLAS_SCHEMA => "index rules changed",
        _ => "schema rebuild",
    };
    tx.execute_batch(ATLAS_DROP)?;
    tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('atlas', ?1)", [fp])?;
    tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('atlas_schema', ?1)", [ATLAS_SCHEMA])?;
    Ok(Some(why))
}

/// Stores the names set and the open stamp (see `Store::open_with`).
pub(super) fn write_stamp(tx: &Transaction<'_>, names: &Names, stamp: &str) -> Result<()> {
    tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('atlas_names', ?1)", [names.encode()])?;
    tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('open_stamp', ?1)", [stamp])?;
    Ok(())
}

/// The rules a document is indexed with.
pub(super) struct DocRules<'a> {
    pub(super) source: &'a str,
    pub(super) kind: &'static str,
    pub(super) chunk: &'a ChunkRule,
    pub(super) id_keys: &'a [String],
    pub(super) names_kind: Option<&'a str>,
}

/// Indexes one document: its `docs` row, sections and identifiers.
#[allow(clippy::too_many_arguments)]
pub(super) fn index_doc(
    tx: &Transaction<'_>,
    rules: &Rules,
    names: &Names,
    dr: &DocRules<'_>,
    rel: &str,
    text: &str,
    mtime: i64,
    size: i64,
    sha: &str,
) -> Result<()> {
    let fm = markdown::parse_frontmatter_nested(text);
    let stem = stem_of(rel);
    let title = ["title", "name"].iter().find_map(|k| fm.get(*k).and_then(Value::as_str)).unwrap_or(stem).to_string();
    let description = fm.get("description").and_then(Value::as_str).map(str::to_string);
    let doc_type = fm
        .get("metadata")
        .and_then(|m| m.get("type"))
        .or_else(|| fm.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let doc_date = ["date", "created", "opened", "updated", "modified"]
        .iter()
        .find_map(|k| fm.get(*k).and_then(leading_date))
        .or_else(|| fm.get("metadata").and_then(|m| m.get("modified")).and_then(leading_date));
    let id: i64 = tx.prepare_cached(
        "INSERT INTO docs(source, kind, path, mtime_ns, size, sha256, title, description, doc_type, date, indexed)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(source, path) DO UPDATE SET kind = excluded.kind, mtime_ns = excluded.mtime_ns, size = excluded.size,
         sha256 = excluded.sha256, title = excluded.title, description = excluded.description, doc_type = excluded.doc_type,
         date = excluded.date, indexed = excluded.indexed RETURNING id",
    )?
    .query_row(params![dr.source, dr.kind, rel, mtime, size, sha, title, description, doc_type, doc_date, now()], |r| r.get(0))?;
    clear_doc_rows(tx, id)?;
    let chunks = chunk::chunks(text, dr.chunk);
    let mut section_ids = Vec::with_capacity(chunks.len());
    {
        let mut st = tx.prepare_cached(
            "INSERT INTO sections(doc_id, ord, level, heading_path, label, line_start, line_end, date, sha) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id",
        )?;
        let mut fts = tx.prepare_cached("INSERT INTO sections_fts(rowid, heading_path, body) VALUES(?1, ?2, ?3)")?;
        for (ord, c) in chunks.iter().enumerate() {
            let date = c.date.as_deref().or(doc_date.as_deref());
            let sha = crate::semantic::section_sha(&c.heading_path, &text[c.start..c.end]);
            let sid: i64 = st.query_row(
                params![id, ord as i64, c.level as i64, c.heading_path, c.label, c.line_start as i64, c.line_end as i64, date, sha],
                |r| r.get(0),
            )?;
            fts.execute(params![sid, c.heading_path, &text[c.start..c.end]])?;
            section_ids.push(sid);
        }
    }
    let info = DocInfo { stem, id_keys: dr.id_keys, names_kind: dr.names_kind };
    let occs = registry::extract(rules, names, text, &chunks, &info);
    let spans = markdown::line_spans(text);
    let mut st = tx.prepare_cached(
        "INSERT INTO idents(doc_id, ident, kind, def, rank, section_id, line, date, context) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    // Each distinct identifier folded once (as NOCASE folds, ASCII only):
    // `fids` holds the folded forms, numbered by `fid_of`.
    let mut fids: Vec<String> = Vec::new();
    let mut fid_of: HashMap<String, u32> = HashMap::new();
    let mut fold_of: HashMap<&str, u32> = HashMap::new();
    // The lookup rows: a definition is a row of its own; mentions of one
    // folded identifier in one section merge into the row of the first, with
    // a count. `occs` is in line order, so the first occurrence of a group is
    // the one with the smallest line.
    let mut rows: Vec<LookupRow<'_>> = Vec::with_capacity(occs.len());
    let mut groups: HashMap<(u32, i64), usize> = HashMap::new();
    for (i, o) in occs.iter().enumerate() {
        let (ident, kind, line, def, chunk) = (o.ident.as_str(), o.kind.as_str(), o.line, o.def, o.chunk);
        let context = match (def, spans.get(line.saturating_sub(1))) {
            (Some(registry::Why::File), _) => title.clone(),
            (_, Some(&(s, e, _))) => {
                // Centred on the identifier, so a long line still shows it.
                let line = &text[s..e];
                let at = crate::text::find_ci(line, ident).unwrap_or(0);
                crate::text::window(line, at, ident.len(), crate::text::SNIPPET_CHARS)
            }
            _ => String::new(),
        };
        let chunk_date = chunks.get(chunk).and_then(|c| c.date.as_deref());
        let date = o.date.as_deref().or(chunk_date).or(doc_date.as_deref());
        let rank = def.map(registry::Why::rank).unwrap_or(0);
        let section_id = section_ids.get(chunk).copied();
        st.execute(params![id, ident, kind, def.map(registry::Why::as_str), rank, section_id, line as i64, date, context])?;
        let fid = *fold_of.entry(ident).or_insert_with(|| {
            *fid_of.entry(ident.to_ascii_lowercase()).or_insert_with_key(|f| {
                fids.push(f.clone());
                fids.len() as u32 - 1
            })
        });
        if rank == 0 {
            let key = (fid, section_id.unwrap_or(-(line as i64)));
            if let Some(&k) = groups.get(&key) {
                rows[k].n += 1;
                continue;
            }
            groups.insert(key, rows.len());
        }
        // The section's heading and lines, as the reference's LEFT JOIN on
        // `sections` gives them (empty and 0 without a section).
        let c = section_id.and(chunks.get(chunk));
        // A definition carries its text, cut and capped as the answer
        // shows it, so a lookup with text reads one column.
        let def_text = def.map(|why| match c {
            None => context.clone(),
            Some(c) => cut_definition(text, c, chunks.get(chunk + 1), why, line, &context),
        });
        rows.push(LookupRow {
            fid,
            rank,
            date: date.unwrap_or(""),
            line: line as i64,
            seq: i as i64,
            ident,
            kind,
            def: def.map(registry::Why::as_str),
            heading_path: c.map_or("", |c| c.heading_path.as_str()),
            section_start: c.map_or(0, |c| c.line_start as i64),
            section_end: c.map_or(0, |c| c.line_end as i64),
            context,
            section_id,
            n: 1,
            text: def_text,
        });
    }
    tx.prepare_cached(MARK_DIRTY)?.execute([id])?;
    write_lookups(tx, id, dr, rel, mtime, &fids, &rows)?;
    Ok(())
}

/// One row of `lookups`: a definition, or the mentions of one identifier in
/// one section merged into their first. Strings are borrowed from the
/// occurrences and the document's sections.
struct LookupRow<'r> {
    /// Index of the folded identifier in the document's `fids`.
    fid: u32,
    rank: i64,
    date: &'r str,
    line: i64,
    seq: i64,
    ident: &'r str,
    kind: &'r str,
    def: Option<&'static str>,
    heading_path: &'r str,
    section_start: i64,
    section_end: i64,
    context: String,
    section_id: Option<i64>,
    n: i64,
    /// A definition's text (see `cut_definition`); `None` for mentions.
    text: Option<String>,
}

/// The text a definition stands for, from the document itself: the whole
/// section for a heading, the item (with its continuation lines) for a list
/// item, the row for a table row, and the section and the next one for a
/// file or frontmatter definition; capped at [`MAX_SECTION_TEXT`]. The same
/// text [`Store::definition_text`] reads back from the index (the tests hold
/// the two equal on every definition).
///
/// # Arguments
///
/// * `text` - the document.
/// * `c` - the definition's section.
/// * `next` - the section after it, if any.
/// * `why` - why it is a definition.
/// * `line` - its 1-based line.
/// * `context` - its stored context line.
fn cut_definition(text: &str, c: &chunk::Chunk, next: Option<&chunk::Chunk>, why: registry::Why, line: usize, context: &str) -> String {
    let body = &text[c.start..c.end];
    match why.as_str() {
        "heading" => cap_text(body).0,
        "table_row" => cap_text(context).0,
        "list_item" => cap_text(&item_block(body, line.saturating_sub(c.line_start))).0,
        _ => match next {
            Some(n) if body.len() < MAX_SECTION_TEXT => {
                let mut s = String::with_capacity(body.len() + (n.end - n.start));
                s.push_str(body);
                s.push_str(&text[n.start..n.end]);
                cap_text(&s).0
            }
            _ => cap_text(body).0,
        },
    }
}

/// Writes a document's `lookups` rows and adds them to `lookup_counts`.
fn write_lookups(
    tx: &Transaction<'_>,
    doc_id: i64,
    dr: &DocRules<'_>,
    rel: &str,
    mtime: i64,
    fids: &[String],
    rows: &[LookupRow<'_>],
) -> Result<()> {
    let mut st = tx.prepare_cached(
        "INSERT INTO lookups(fid, rank, date, mtime_ns, source, path, line, seq, doc_id, doc_kind, ident, kind, def, heading_path,
         section_start, section_end, context, section_id, n, text) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
    )?;
    // (defs, lines, total) per folded identifier.
    let mut counts: Vec<(i64, i64, i64)> = vec![(0, 0, 0); fids.len()];
    for r in rows {
        st.execute(params![
            fids[r.fid as usize],
            r.rank,
            r.date,
            mtime,
            dr.source,
            rel,
            r.line,
            r.seq,
            doc_id,
            dr.kind,
            r.ident,
            r.kind,
            r.def,
            r.heading_path,
            r.section_start,
            r.section_end,
            r.context,
            r.section_id,
            r.n,
            r.text
        ])?;
        let c = &mut counts[r.fid as usize];
        if r.rank > 0 {
            c.0 += 1;
        } else {
            c.1 += 1;
            c.2 += r.n;
        }
    }
    let mut st = tx.prepare_cached(COUNT_DOC)?;
    for (fid, (d, l, t)) in fids.iter().zip(counts) {
        if d > 0 || l > 0 {
            st.execute(params![fid, d, l, t])?;
        }
    }
    Ok(())
}

fn clear_doc_rows(tx: &Transaction<'_>, id: i64) -> Result<()> {
    tx.prepare_cached("DELETE FROM sections_fts WHERE rowid IN (SELECT id FROM sections WHERE doc_id = ?1)")?.execute([id])?;
    tx.prepare_cached("DELETE FROM sections WHERE doc_id = ?1")?.execute([id])?;
    tx.prepare_cached(MARK_DIRTY)?.execute([id])?;
    tx.prepare_cached("DELETE FROM idents WHERE doc_id = ?1")?.execute([id])?;
    tx.prepare_cached(UNCOUNT_DOC)?.execute([id])?;
    tx.prepare_cached("DELETE FROM lookups WHERE doc_id = ?1")?.execute([id])?;
    Ok(())
}

/// Removes a document and its rows.
pub(super) fn delete_doc(tx: &Transaction<'_>, source: &str, path: &str) -> Result<()> {
    let id: Option<i64> =
        tx.prepare_cached("SELECT id FROM docs WHERE source = ?1 AND path = ?2")?.query_row(params![source, path], |r| r.get(0)).optional()?;
    if let Some(id) = id {
        clear_doc_rows(tx, id)?;
        tx.prepare_cached("DELETE FROM docs WHERE id = ?1")?.execute([id])?;
    }
    Ok(())
}

/// Updates a document's mtime after a touch that left its content alone.
pub(super) fn touch_doc(tx: &Transaction<'_>, source: &str, path: &str, mtime: i64, size: i64) -> Result<()> {
    tx.prepare_cached("UPDATE docs SET mtime_ns = ?3, size = ?4 WHERE source = ?1 AND path = ?2")?
        .execute(params![source, path, mtime, size])?;
    // The lookup rows order by the file's mtime too.
    tx.prepare_cached("UPDATE lookups SET mtime_ns = ?3 WHERE doc_id = (SELECT id FROM docs WHERE source = ?1 AND path = ?2)")?
        .execute(params![source, path, mtime])?;
    Ok(())
}

fn known_docs(conn: &Connection, source: &str) -> Result<Known> {
    let mut st = conn.prepare_cached("SELECT path, mtime_ns, size, sha256 FROM docs WHERE source = ?1")?;
    let rows = st.query_map([source], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?;
    let mut out = HashMap::new();
    for r in rows {
        let (k, v) = r?;
        out.insert(k, v);
    }
    Ok(out)
}

/// The difference between a walk and the indexed rows: stale files, and
/// files whose mtime moved but whose content did not.
struct Diff {
    stale: Vec<StaleFile>,
    touched: Vec<(String, i64, i64)>,
}

/// The result of checking a dirty set: the files that exist and matter,
/// and the paths that are gone; `None` when a folder changed and only a
/// walk can say what is under it.
pub(super) type Dirty = Option<(Vec<Found>, Vec<String>)>;

impl Store {
    /// Change events the watcher has seen so far, or `None` when watching
    /// is off.
    pub fn watch_events(&self) -> Option<u64> {
        self.atlas.watch.as_ref().map(Watcher::events)
    }

    /// Blocks until the watcher has seen more than `seen` events, or
    /// `timeout` passes (a condition variable wait). For tests and callers
    /// that must see a change they just made before they query.
    ///
    /// # Returns
    ///
    /// True when a new event arrived; false on timeout or with no watcher.
    pub fn wait_for_change(&self, seen: u64, timeout: Duration) -> bool {
        self.atlas.watch.as_ref().is_some_and(|w| w.wait_past(seen, timeout))
    }

    /// The watcher root index of a vault or source name.
    pub(super) fn watch_index(&self, name: &str) -> Option<usize> {
        self.vaults.all().iter().position(|v| v.name == name).or_else(|| {
            self.atlas.setup.sources.iter().position(|s| s.name == name).map(|i| i + self.vaults.all().len())
        })
    }

    /// What a check of `name` has to do.
    pub(super) fn plan_for(&self, name: &str) -> Plan {
        match self.watch_index(name) {
            Some(i) => self.atlas.plan(name, i),
            None => Plan::Walk(None),
        }
    }

    /// Stats the dirty paths of a root: the files a walk would list, and the
    /// paths that no longer exist.
    ///
    /// # Arguments
    ///
    /// * `name` - the vault or source.
    /// * `paths` - dirty paths below its root.
    pub(super) fn stat_dirty(&self, name: &str, paths: &[String]) -> Result<Dirty> {
        let (root, vault) = match self.vaults.all().iter().find(|v| v.name == name) {
            Some(v) => (v.root.clone(), true),
            None => match self.atlas.source(name) {
                Some(s) => (s.root.clone(), false),
                None => return Err(Error::NotFound(format!("source {name:?}"))),
            },
        };
        let wants = |rel: &str| -> bool {
            if rel.split('/').any(|c| c.is_empty() || c.starts_with('.')) {
                return false;
            }
            if vault {
                rel.ends_with(".md") && rel.rsplit('/').next().is_some_and(|n| n.len() > 3 && paths::component_ok(n))
            } else {
                self.atlas.source(name).is_some_and(|s| s.wants(rel))
            }
        };
        let mut found = Vec::new();
        let mut gone = Vec::new();
        for rel in paths {
            let full = root.join(rel);
            match fs::symlink_metadata(&full) {
                Ok(m) if m.is_dir() => return Ok(None),
                Ok(m) if m.is_file() => {
                    if wants(rel) && (!vault || m.len() <= MAX_INDEX) {
                        found.push(Found { rel: rel.clone(), size: m.len(), mtime_ns: paths::mtime_ns(&m) });
                    } else if wants(rel) {
                        gone.push(rel.clone());
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    if wants(rel) {
                        gone.push(rel.clone());
                    } else if !rel.rsplit('/').next().unwrap_or(rel).contains('.') {
                        // A removed or renamed folder: its files are not
                        // reported one by one.
                        return Ok(None);
                    }
                }
            }
        }
        Ok(Some((found, gone)))
    }

    /// The diff for a dirty set only: each existing file against its row,
    /// and the gone paths that had rows.
    fn diff_dirty(&self, name: &str, found: &[Found], gone: &[String]) -> Result<Diff> {
        let mut st = self.conn.prepare_cached("SELECT mtime_ns, size, sha256 FROM docs WHERE source = ?1 AND path = ?2")?;
        let mut stale = Vec::new();
        let mut touched = Vec::new();
        for f in found {
            let row: Option<(i64, i64, String)> = st.query_row(params![name, f.rel], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            match row {
                None => stale.push(StaleFile { path: f.rel.clone(), state: "new" }),
                Some((m, s, _)) if m == f.mtime_ns && s == f.size as i64 => {}
                Some((_, _, sha)) => match self.read_doc(name, &f.rel) {
                    Ok((bytes, _)) if sha256::hex(&bytes) == sha => touched.push((f.rel.clone(), f.mtime_ns, f.size as i64)),
                    _ => stale.push(StaleFile { path: f.rel.clone(), state: "changed" }),
                },
            }
        }
        for g in gone {
            let had: Option<i64> = st.query_row(params![name, g], |r| r.get(0)).optional()?;
            if had.is_some() {
                stale.push(StaleFile { path: g.clone(), state: "removed" });
            }
        }
        Ok(Diff { stale, touched })
    }

    fn atlas_walk(&self, name: &str) -> Result<(&'static str, Vec<Found>)> {
        if let Ok(v) = self.vaults.get(name) {
            if v.name == name {
                let files = paths::walk(v)?.into_iter().filter(|f| f.size <= MAX_INDEX).collect();
                return Ok(("vault", files));
            }
        }
        match self.atlas.source(name) {
            Some(s) => Ok(("source", s.walk()?)),
            None => Err(Error::NotFound(format!("source {name:?}"))),
        }
    }

    fn read_doc(&self, name: &str, rel: &str) -> Result<(Vec<u8>, i64)> {
        if let Some(s) = self.atlas.source(name) {
            return s.read(rel);
        }
        let v = self.vaults.get(name)?;
        let res = paths::resolve_existing(v, rel)?;
        let bytes = fs::read(&res.full)?;
        let mtime = fs::metadata(&res.full).map(|m| paths::mtime_ns(&m)).unwrap_or(0);
        Ok((bytes, mtime))
    }

    /// The indexed paths of one source, from the cache while the database
    /// is unchanged (see [`KnownCache`]).
    fn known_for(&self, name: &str) -> Result<std::sync::Arc<Known>> {
        let mut cache = self.atlas.known.borrow_mut();
        let usable = cache.validate(&self.conn)?;
        if let Some(k) = cache.sources.get(name).filter(|_| usable) {
            return Ok(k.clone());
        }
        let k = std::sync::Arc::new(known_docs(&self.conn, name)?);
        if usable {
            cache.sources.insert(name.to_string(), k.clone());
        }
        Ok(k)
    }

    /// Keeps (true, the default) or rebuilds on every walk (false) the
    /// indexed path maps a full walk compares against. Only `atlas --bench`
    /// turns it off, to measure both ways in one run.
    pub fn set_path_cache(&mut self, on: bool) {
        let mut cache = self.atlas.known.borrow_mut();
        *cache = KnownCache { on, ..KnownCache::default() };
    }

    fn diff(&self, name: &str, files: &[Found]) -> Result<Diff> {
        let known = self.known_for(name)?;
        let mut stale = Vec::new();
        let mut touched = Vec::new();
        let mut seen: HashSet<&str> = HashSet::with_capacity(files.len());
        for f in files {
            seen.insert(f.rel.as_str());
            match known.get(&f.rel) {
                None => stale.push(StaleFile { path: f.rel.clone(), state: "new" }),
                Some((m, s, sha)) => {
                    if *m == f.mtime_ns && *s == f.size as i64 {
                        continue;
                    }
                    match self.read_doc(name, &f.rel) {
                        Ok((bytes, _)) if sha256::hex(&bytes) == *sha => touched.push((f.rel.clone(), f.mtime_ns, f.size as i64)),
                        _ => stale.push(StaleFile { path: f.rel.clone(), state: "changed" }),
                    }
                }
            }
        }
        let mut removed: Vec<&String> = known.keys().filter(|k| !seen.contains(k.as_str())).collect();
        removed.sort();
        stale.extend(removed.into_iter().map(|p| StaleFile { path: p.clone(), state: "removed" }));
        Ok(Diff { stale, touched })
    }

    /// Brings the read-only sources' rows in line with the disk.
    ///
    /// Walks each source and compares every file's mtime and size with its
    /// row; only files that differ are read, and only those whose hash
    /// differs are re-indexed. When the set of names changed (a file was
    /// added to or removed from a source with `names_kind`), every document
    /// is re-indexed, vault notes included, since any of them may mention a
    /// new name.
    ///
    /// # Arguments
    ///
    /// * `only` - the source names to scan, or `None` for all.
    ///
    /// # Returns
    ///
    /// What changed, summed over the sources.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if a source root cannot be read; [`Error::Db`].
    pub fn rescan_sources(&mut self, only: Option<&[&str]>) -> Result<RescanStats> {
        let t0 = Instant::now();
        let wanted = |n: &str| only.is_none_or(|o| o.contains(&n));
        let names_touched = self.atlas.setup.sources.iter().any(|s| s.names_kind.is_some() && wanted(&s.name));
        let mut full = false;
        if names_touched {
            let names = self.atlas.compute_names();
            if names != self.atlas.names {
                self.atlas.names = names;
                let fp = self.atlas.fingerprint(&self.vaults);
                let tx = begin(&mut self.conn)?;
                let why = ensure_fingerprint(&tx, &fp)?;
                full = why.is_some();
                if why.is_some() {
                    self.atlas.rebuilt = why;
                }
                write_stamp(&tx, &self.atlas.names, &format!("{}|{}|{fp}", super::SCHEMA, super::DURABLE_VERSION))?;
                tx.commit()?;
            }
        }
        let names: Vec<String> =
            self.atlas.setup.sources.iter().filter(|s| full || wanted(&s.name)).map(|s| s.name.clone()).collect();
        let mut stats = RescanStats::default();
        for name in &names {
            let s = self.scan_one(name)?;
            stats.files += s.files;
            stats.parsed += s.parsed;
            stats.touched += s.touched;
            stats.removed += s.removed;
        }
        if full {
            let v = self.rescan()?;
            stats.files += v.files;
            stats.parsed += v.parsed;
        }
        stats.elapsed_us = t0.elapsed().as_micros();
        Ok(stats)
    }

    /// Turns on keeping the memory store in step with the sources it was
    /// imported from (see [`crate::memory::sync`]): every rescan or
    /// refresh that finds a mapped source changed re-imports it, and the
    /// first one in a process checks each mapped source once. `None` turns
    /// it off.
    pub fn set_memory_sync(&mut self, settings: Option<crate::memory::settings::Settings>) {
        let roots: Vec<(String, std::path::PathBuf)> = self.atlas.setup.sources.iter().map(|s| (s.name.clone(), s.root.clone())).collect();
        self.memsync = settings.and_then(|s| crate::memory::sync::Syncer::new(s, &roots));
    }

    /// The memory syncs run since the last call (empty when nothing changed).
    pub fn take_memory_sync_reports(&mut self) -> Vec<crate::memory::sync::Report> {
        self.memsync.as_mut().map(|m| m.take_reports()).unwrap_or_default()
    }

    /// The synced sources' names (the memory tools refresh these first).
    pub fn memory_sync_sources(&self) -> Vec<String> {
        self.atlas.setup.sources.iter().filter(|s| self.memsync.as_ref().is_some_and(|m| m.target(&s.name).is_some())).map(|s| s.name.clone()).collect()
    }

    /// After a source's rows are current: syncs its export when it changed
    /// or has not been checked in this process. Fails open: an error is
    /// printed and the next change tries again.
    fn memory_sync_after(&mut self, name: &str, changed: bool) {
        if !self.memsync.as_ref().is_some_and(|m| m.needs(name, changed)) {
            return;
        }
        let fp = self.index_fingerprint(name).unwrap_or(None);
        if let Some(m) = self.memsync.as_mut() {
            if let Err(e) = m.check(name, fp) {
                eprintln!("atlas: memory sync {name}: {e}");
            }
        }
        // The memory tree wrote into the source: its rows are stale.
        if self.memsync.as_mut().is_some_and(|m| m.take_rewritten(name)) {
            if let Err(e) = self.scan_one_index(name) {
                eprintln!("atlas: memory tree {name}: rescan: {e}");
            }
        }
    }

    /// A synced source's fingerprint from the indexed hashes of the files
    /// its export reads (see [`crate::memory::sync::fingerprint`]); `None`
    /// when the index lacks the file.
    fn index_fingerprint(&self, name: &str) -> Result<Option<String>> {
        let Some(t) = self.memsync.as_ref().and_then(|m| m.target(name)) else { return Ok(None) };
        let known = self.known_for(name)?;
        let files: Vec<(String, String)> = match t.spec.shape {
            crate::memory::export::Shape::MemoryDir => {
                known.iter().filter(|(k, _)| !k.contains('/') && k.ends_with(".md")).map(|(k, v)| (k.clone(), v.2.clone())).collect()
            }
            _ => {
                let f = t.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                match known.get(&f) {
                    Some(v) => vec![(f, v.2.clone())],
                    None => return Ok(None),
                }
            }
        };
        Ok(Some(crate::memory::sync::fingerprint(&files)))
    }

    fn scan_one(&mut self, name: &str) -> Result<RescanStats> {
        let s = self.scan_one_index(name)?;
        self.memory_sync_after(name, s.parsed + s.removed > 0);
        Ok(s)
    }

    fn scan_one_index(&mut self, name: &str) -> Result<RescanStats> {
        let mut plan = self.plan_for(name);
        let at = Instant::now();
        if let Plan::Paths(i, snap) = &plan {
            if let Some((found, gone)) = self.stat_dirty(name, &snap.paths)? {
                let diff = self.diff_dirty(name, &found, &gone)?;
                let mut stats = RescanStats { files: found.len(), touched: diff.touched.len(), ..Default::default() };
                if !diff.stale.is_empty() || !diff.touched.is_empty() {
                    self.apply_source(name, &diff, &mut stats)?;
                }
                self.atlas.settled(name, &plan, at);
                return Ok(stats);
            }
            plan = Plan::Walk(Some((*i, snap.seq)));
        }
        if let Plan::Skip(..) = plan {
            return Ok(RescanStats::default());
        }
        let (_, files) = self.atlas_walk(name)?;
        self.atlas.last_scan.insert(name.to_string(), SystemTime::now());
        let diff = self.diff(name, &files)?;
        let mut stats = RescanStats { files: files.len(), touched: diff.touched.len(), ..Default::default() };
        if !diff.stale.is_empty() || !diff.touched.is_empty() {
            self.apply_source(name, &diff, &mut stats)?;
        }
        self.atlas.settled(name, &plan, at);
        Ok(stats)
    }

    fn apply_source(&mut self, name: &str, diff: &Diff, stats: &mut RescanStats) -> Result<()> {
        let mut loaded = Vec::new();
        for f in &diff.stale {
            if f.state == "removed" {
                continue;
            }
            if let Ok((bytes, mtime)) = self.read_doc(name, &f.path) {
                loaded.push((f.path.clone(), bytes, mtime));
            }
        }
        let src = self.atlas.source(name).ok_or_else(|| Error::NotFound(format!("source {name:?}")))?;
        let dr = DocRules { source: &src.name, kind: "source", chunk: &src.chunk, id_keys: &src.id_keys, names_kind: src.names_kind.as_deref() };
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for f in &diff.stale {
            if f.state == "removed" {
                delete_doc(&tx, name, &f.path)?;
                stats.removed += 1;
            }
        }
        for (rel, m, s) in &diff.touched {
            touch_doc(&tx, name, rel, *m, *s)?;
        }
        // The index file of a names source defines no name of its own.
        let index_dr = DocRules { names_kind: None, ..dr };
        for (rel, bytes, mtime) in &loaded {
            let text = String::from_utf8_lossy(bytes);
            let dr = if self.atlas.setup.is_name_index(name, rel) { &index_dr } else { &dr };
            index_doc(&tx, &self.atlas.setup.rules, &self.atlas.names, dr, rel, &text, *mtime, bytes.len() as i64, &sha256::hex(bytes))?;
            stats.parsed += 1;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every vault and source name, vaults first.
    pub fn atlas_names(&self) -> Vec<String> {
        self.vaults.all().iter().map(|v| v.name.clone()).chain(self.atlas.setup.sources.iter().map(|s| s.name.clone())).collect()
    }

    /// The read-only sources.
    pub fn sources(&self) -> &[Source] {
        &self.atlas.setup.sources
    }

    /// Compares the disk with the index for some sources and vaults, and
    /// re-indexes what changed when that is cheap.
    ///
    /// # Arguments
    ///
    /// * `touched` - the source and vault names an answer drew on; empty
    ///   means all of them (an empty answer may be empty because of a stale
    ///   file anywhere).
    /// * `refresh` - see [`Refresh`].
    ///
    /// # Returns
    ///
    /// One entry per name, and whether anything was re-indexed (in which
    /// case the caller recomputes its answer).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] and [`Error::Db`].
    pub fn freshness(&mut self, touched: &[String], refresh: Refresh) -> Result<(Vec<Freshness>, bool)> {
        self.freshness_with(touched, refresh, true)
    }

    /// [`Store::freshness`], with the per-source counts and index times
    /// (two queries per source) filled in only when `detail` is set.
    ///
    /// # Errors
    ///
    /// As [`Store::freshness`].
    pub fn freshness_with(&mut self, touched: &[String], refresh: Refresh, detail: bool) -> Result<(Vec<Freshness>, bool)> {
        let all;
        let names: &[String] = if touched.is_empty() {
            all = self.atlas_names();
            &all
        } else {
            touched
        };
        let mut out = Vec::with_capacity(names.len());
        let mut diffs: Vec<(String, Diff, Plan, Instant)> = Vec::new();
        for name in names {
            let kind = if self.atlas.source(name).is_some() { "source" } else { "vault" };
            if refresh == Refresh::Off {
                out.push(self.fresh_entry(name, kind, false, Vec::new(), detail)?);
                continue;
            }
            let mut plan = self.plan_for(name);
            let at = Instant::now();
            let d = loop {
                match &plan {
                    Plan::Skip(..) => break Diff { stale: Vec::new(), touched: Vec::new() },
                    Plan::Paths(_, snap) => match self.stat_dirty(name, &snap.paths)? {
                        Some((found, gone)) => break self.diff_dirty(name, &found, &gone)?,
                        None => plan = Plan::Walk(match &plan {
                            Plan::Paths(i, snap) => Some((*i, snap.seq)),
                            _ => None,
                        }),
                    },
                    Plan::Walk(_) => {
                        let (_, files) = self.atlas_walk(name)?;
                        self.atlas.last_scan.insert(name.clone(), SystemTime::now());
                        break self.diff(name, &files)?;
                    }
                }
            };
            out.push(self.fresh_entry(name, kind, true, d.stale.clone(), detail)?);
            if d.stale.is_empty() && d.touched.is_empty() {
                // Nothing to apply: the dirty set is accounted for.
                self.atlas.settled(name, &plan, at);
                self.memory_sync_after(name, false);
            } else {
                diffs.push((name.clone(), d, plan, at));
            }
        }
        // A source with nothing indexed yet is indexed whole whatever its
        // size: the first query after a cold start must not come back empty.
        let cold: Vec<bool> = diffs.iter().map(|(_, d, _, _)| !d.stale.is_empty() && d.stale.iter().all(|s| s.state == "new")).collect();
        let total: usize = diffs.iter().zip(&cold).filter(|(_, c)| !**c).map(|((_, d, _, _), _)| d.stale.len()).sum();
        let mut refreshed = false;
        if refresh == Refresh::Auto && !diffs.is_empty() && total <= self.atlas.setup.refresh_limit {
            let mut vault_stale = false;
            for (name, d, plan, at) in &diffs {
                if self.atlas.source(name).is_some() {
                    let mut st = RescanStats::default();
                    self.apply_source(name, d, &mut st)?;
                    self.atlas.settled(name, plan, *at);
                    self.memory_sync_after(name, !d.stale.is_empty());
                } else {
                    vault_stale = true;
                }
            }
            if vault_stale {
                self.rescan()?;
            }
            refreshed = diffs.iter().any(|(_, d, _, _)| !d.stale.is_empty());
            for f in &mut out {
                if f.stale_count > 0 {
                    f.refreshed = true;
                    if detail {
                        f.files = self.doc_count(&f.source)?;
                        f.last_indexed = self.last_indexed(&f.source)?;
                    }
                }
            }
        }
        Ok((out, refreshed))
    }

    fn doc_count(&self, name: &str) -> Result<usize> {
        let n: i64 = self.conn.prepare_cached("SELECT COUNT(*) FROM docs WHERE source = ?1")?.query_row([name], |r| r.get(0))?;
        Ok(n as usize)
    }

    fn last_indexed(&self, name: &str) -> Result<Option<String>> {
        Ok(self.conn.prepare_cached("SELECT MAX(indexed) FROM docs WHERE source = ?1")?.query_row([name], |r| r.get(0))?)
    }

    fn fresh_entry(&self, name: &str, kind: &'static str, checked: bool, mut stale: Vec<StaleFile>, detail: bool) -> Result<Freshness> {
        let stale_count = stale.len();
        let mut reason = None;
        if stale_count > STALE_LIST_CAP {
            // Every file new against an empty index: the whole source is being
            // built, and the reason is why (recorded when the rows were wiped).
            reason = Some(if stale.iter().all(|s| s.state == "new") && self.doc_count(name)? == 0 {
                self.atlas.rebuilt.unwrap_or("first index").to_string()
            } else {
                let mut parts = Vec::new();
                for state in ["changed", "new", "removed"] {
                    let n = stale.iter().filter(|s| s.state == state).count();
                    if n > 0 {
                        parts.push(format!("{n} {state}"));
                    }
                }
                parts.join(", ")
            });
            stale = Vec::new();
        }
        Ok(Freshness {
            source: name.to_string(),
            kind,
            files: if detail { self.doc_count(name)? } else { 0 },
            last_scan: self.atlas.last_scan.get(name).map(|t| utc(*t)),
            last_indexed: if detail { self.last_indexed(name)? } else { None },
            checked,
            stale,
            stale_count,
            reason,
            refreshed: false,
        })
    }

    /// Looks an identifier up: its definitions, then its mentions.
    ///
    /// # Arguments
    ///
    /// * `id` - the identifier, matched case-insensitively.
    /// * `limit` - the most mentions returned.
    /// * `with_text` - which definitions carry their defining text (stored
    ///   at index time, so this is a column of the same statement).
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn lookup(&self, id: &str, limit: usize, with_text: DefText) -> Result<Lookup> {
        // `lookups` holds every identifier's rows already merged per section
        // and already in answer order (its primary key is the order), so
        // this is a range scan that stops after the definitions and `limit`
        // mentions; `lookup_counts` holds the totals past the limit.
        let fid = id.to_ascii_lowercase();
        let mut definitions = Vec::new();
        let mut mentions = Vec::new();
        let (mut lines, mut total) = (0i64, 0i64);
        // One statement: the totals joined to the rows, which come in key
        // order (the plan is two primary-key searches, no sort). Stepping
        // stops once `limit` mentions are read, so the rest are never
        // decoded.
        let mut st = self.conn.prepare_cached(
            "SELECT l.rank, l.source, l.doc_kind, l.path, l.line, l.ident, l.kind, l.def, l.date, l.heading_path, l.section_start,
             l.section_end, l.context, l.section_id, l.n, c.lines, c.total, CASE WHEN ?2 THEN l.text END
             FROM lookup_counts c JOIN lookups l ON l.fid = c.fid WHERE c.fid = ?1
             ORDER BY l.rank DESC, l.date DESC, l.mtime_ns DESC, l.source, l.path, l.line, l.seq",
        )?;
        let want = |n: usize| match with_text {
            DefText::None => false,
            DefText::First => n == 0,
            DefText::All => true,
        };
        let mut rows = st.query(params![&fid, with_text != DefText::None])?;
        while let Some(r) = rows.next()? {
            let rank: i64 = r.get(0)?;
            if rank == 0 {
                if mentions.len() >= limit {
                    // Totals are on every row; read them before stopping.
                    if lines == 0 {
                        (lines, total) = (r.get(15)?, r.get(16)?);
                    }
                    break;
                }
            } else if definitions.len() >= MAX_DEFINITIONS {
                continue;
            }
            (lines, total) = (r.get(15)?, r.get(16)?);
            let date: String = r.get(8)?;
            let hit = IdHit {
                source: r.get(1)?,
                source_kind: r.get(2)?,
                path: r.get(3)?,
                line: r.get(4)?,
                id: r.get(5)?,
                kind: r.get(6)?,
                def: r.get(7)?,
                date: (!date.is_empty()).then_some(date),
                heading_path: r.get(9)?,
                section_start: r.get(10)?,
                section_end: r.get(11)?,
                context: r.get(12)?,
                text: if rank > 0 && want(definitions.len()) { r.get(17)? } else { None },
                count: r.get(14)?,
                section_id: r.get(13)?,
            };
            if rank == 0 { mentions.push(hit) } else { definitions.push(hit) }
        }
        Ok(Lookup { definitions, mentions, total_mentions: total.max(0) as usize, mention_lines: lines.max(0) as usize })
    }

    /// Every source holding rows for an identifier, definitions or
    /// mentions, including rows past any lookup's mention limit.
    ///
    /// A lookup checks these for staleness, not only the sources of the rows
    /// it shows: a stale file whose rows all fall past the limit (or that
    /// would gain a definition when reindexed) must still be refreshed.
    ///
    /// # Arguments
    ///
    /// * `id` - the identifier, matched case-insensitively.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn lookup_sources(&self, id: &str) -> Result<Vec<String>> {
        // A range scan of the `lookups` primary key (it starts with `fid`).
        let fid = id.to_ascii_lowercase();
        let mut st = self.conn.prepare_cached("SELECT DISTINCT source FROM lookups WHERE fid = ?1")?;
        let rows = st.query_map([&fid], |r| r.get(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// The lookup as one grouping statement over `idents`, without the
    /// pre-merged `lookups` table: the reference [`Store::lookup`] is held
    /// to (tests compare the two on every identifier) and the "before" arm
    /// of `atlas --bench`.
    ///
    /// # Arguments
    ///
    /// * `id` - the identifier, matched case-insensitively.
    /// * `limit` - the most mention lines returned.
    /// * `with_text` - which definitions get their text, read back from the
    ///   sections (the reference for the stored text).
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn lookup_reference(&self, id: &str, limit: usize, with_text: DefText) -> Result<Lookup> {
        // One statement: definitions one row each, mentions merged per
        // section (the bare columns come from the row holding MIN(line),
        // which SQLite guarantees for a lone MIN aggregate), best first.
        let mut st = self.conn.prepare_cached(
            "SELECT i.rank, d.source, d.kind, d.path, MIN(i.line), i.ident, i.kind, i.def, i.date, ifnull(s.heading_path, ''),
             ifnull(s.line_start, 0), ifnull(s.line_end, 0), i.context, i.section_id, COUNT(*)
             FROM idents i JOIN docs d ON d.id = i.doc_id LEFT JOIN sections s ON s.id = i.section_id
             WHERE i.ident = ?1
             GROUP BY CASE WHEN i.rank > 0 THEN -i.rowid ELSE i.doc_id END, CASE WHEN i.rank > 0 THEN 0 ELSE ifnull(i.section_id, -i.line) END
             ORDER BY i.rank DESC, (i.date IS NULL), i.date DESC, d.mtime_ns DESC, d.source, d.path, 5",
        )?;
        let mut rows = st.query([id])?;
        let mut definitions = Vec::new();
        let mut mentions = Vec::new();
        let (mut total, mut lines) = (0usize, 0usize);
        while let Some(r) = rows.next()? {
            let rank: i64 = r.get(0)?;
            let count: i64 = r.get(14)?;
            if rank == 0 {
                total += count as usize;
                lines += 1;
                if mentions.len() >= limit {
                    continue;
                }
            } else if definitions.len() >= MAX_DEFINITIONS {
                continue;
            }
            let hit = IdHit {
                source: r.get(1)?,
                source_kind: r.get(2)?,
                path: r.get(3)?,
                line: r.get(4)?,
                id: r.get(5)?,
                kind: r.get(6)?,
                def: r.get(7)?,
                date: r.get(8)?,
                heading_path: r.get(9)?,
                section_start: r.get(10)?,
                section_end: r.get(11)?,
                context: r.get(12)?,
                text: None,
                count,
                section_id: r.get(13)?,
            };
            if rank == 0 { mentions.push(hit) } else { definitions.push(hit) }
        }
        let n = match with_text {
            DefText::None => 0,
            DefText::First => 1,
            DefText::All => definitions.len(),
        };
        for d in definitions.iter_mut().take(n) {
            d.text = Some(self.definition_text(d)?);
        }
        Ok(Lookup { definitions, mentions, total_mentions: total, mention_lines: lines })
    }

    fn section_body(&self, id: i64) -> Result<String> {
        Ok(self
            .conn
            .prepare_cached("SELECT body FROM sections_fts WHERE rowid = ?1")?
            .query_row([id], |r| r.get(0))
            .optional()?
            .unwrap_or_default())
    }

    /// The text a definition stands for: the whole section for a heading,
    /// the item (with its continuation lines) for a list item, the row for
    /// a table row, and the first two sections for a file or frontmatter
    /// definition.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn definition_text(&self, d: &IdHit) -> Result<String> {
        let Some(sid) = d.section_id else { return Ok(d.context.clone()) };
        let body = self.section_body(sid)?;
        let text = match d.def.as_deref() {
            Some("heading") => body,
            Some("table_row") => d.context.clone(),
            Some("list_item") => item_block(&body, (d.line - d.section_start).max(0) as usize),
            _ => {
                let next: Option<i64> = self
                    .conn
                    .prepare_cached(
                        "SELECT s2.id FROM sections s1 JOIN sections s2 ON s2.doc_id = s1.doc_id AND s2.ord = s1.ord + 1 WHERE s1.id = ?1",
                    )?
                    .query_row([sid], |r| r.get(0))
                    .optional()?;
                match next {
                    Some(n) if body.len() < MAX_SECTION_TEXT => body + &self.section_body(n)?,
                    _ => body,
                }
            }
        };
        Ok(cap_text(&text).0)
    }

    /// Lists identifiers.
    ///
    /// # Arguments
    ///
    /// * `prefix` - only identifiers starting with this (case-insensitive).
    /// * `pattern` - only identifiers this regex matches.
    /// * `kind` - only this kind.
    /// * `source` - only occurrences in this source or vault.
    /// * `limit` - the most returned.
    ///
    /// # Returns
    ///
    /// The identifiers in natural order (at most `limit`), and how many
    /// matched in all.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn ids(
        &self,
        prefix: Option<&str>,
        pattern: Option<&Regex>,
        kind: Option<&str>,
        source: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<IdSummary>, usize)> {
        // Bring the summary rows up to date first (a no-op read when clean).
        let dirty: bool = self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM ident_dirty)")?.query_row([], |r| r.get(0))?;
        if dirty {
            self.conn.execute_batch(&format!("BEGIN IMMEDIATE; {RECOUNT} COMMIT;"))?;
        }
        let like = prefix.map(|p| format!("{}%", p.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let mut st = self.conn.prepare_cached(
            "SELECT MIN(ident), kind, SUM(defs), SUM(occ), MAX(last) FROM ident_stats
             WHERE (?1 IS NULL OR ident LIKE ?1 ESCAPE '\\') AND (?2 IS NULL OR kind = ?2) AND (?3 IS NULL OR source = ?3)
             GROUP BY ident, kind",
        )?;
        let rows = st.query_map(params![like, kind, source], |r| {
            Ok(IdSummary { id: r.get(0)?, kind: r.get(1)?, definitions: r.get(2)?, occurrences: r.get(3)?, last_date: r.get(4)? })
        })?;
        let mut out = Vec::new();
        for r in rows {
            let r = r?;
            if pattern.is_none_or(|p| p.is_match(&r.id)) {
                out.push(r);
            }
        }
        out.sort_by(|a, b| crate::index::natural_cmp(&a.id, &b.id).then_with(|| a.kind.cmp(&b.kind)));
        let total = out.len();
        out.truncate(limit);
        Ok((out, total))
    }

    /// Full-text search over sections, best first.
    ///
    /// # Arguments
    ///
    /// * `query` - an FTS5 query over the heading path and the body.
    /// * `source` - only this source or vault.
    /// * `folder` - only paths under this folder.
    /// * `limit` - the most sections returned.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for FTS5 syntax errors, [`Error::Db`]
    /// otherwise.
    pub fn search_sections(&self, query: &str, source: Option<&str>, folder: Option<&str>, limit: usize) -> Result<Vec<SectionHit>> {
        let like = folder.filter(|f| !f.is_empty()).map(folder_like);
        let mut st = self.conn.prepare_cached(
            "SELECT d.source, d.kind, d.path, s.heading_path, s.label, s.line_start, s.line_end, s.date, f.body, s.id
             FROM sections_fts f JOIN sections s ON s.id = f.rowid JOIN docs d ON d.id = s.doc_id
             WHERE sections_fts MATCH ?1 AND (?2 IS NULL OR d.source = ?2) AND (?3 IS NULL OR d.path LIKE ?3 ESCAPE '\\')
             ORDER BY bm25(sections_fts, 4.0, 1.0) LIMIT ?4",
        )?;
        let fts_err = |e: rusqlite::Error| Error::InvalidArgument(format!("fts query: {e}"));
        let rows = st
            .query_map(params![query, source, like, limit as i64], |r| {
                let body: String = r.get(8)?;
                let (text, truncated) = cap_text(&body);
                Ok(SectionHit {
                    id: r.get(9)?,
                    source: r.get(0)?,
                    source_kind: r.get(1)?,
                    path: r.get(2)?,
                    heading_path: r.get(3)?,
                    label: r.get(4)?,
                    line_start: r.get(5)?,
                    line_end: r.get(6)?,
                    date: r.get(7)?,
                    text,
                    truncated,
                })
            })
            .map_err(fts_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(fts_err)?);
        }
        Ok(out)
    }

    /// Sections by row id, in the order given (ids no longer present are
    /// skipped), filtered like [`Store::search_sections`].
    ///
    /// # Arguments
    ///
    /// * `ids` - section ids, best first.
    /// * `source` - only this source or vault.
    /// * `folder` - only paths under this folder.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] from SQLite.
    pub fn sections_by_id(&self, ids: &[i64], source: Option<&str>, folder: Option<&str>) -> Result<Vec<SectionHit>> {
        let like = folder.filter(|f| !f.is_empty()).map(folder_like);
        let mut st = self.conn.prepare_cached(
            "SELECT d.source, d.kind, d.path, s.heading_path, s.label, s.line_start, s.line_end, s.date, f.body, s.id
             FROM sections s JOIN docs d ON d.id = s.doc_id JOIN sections_fts f ON f.rowid = s.id
             WHERE s.id = ?1 AND (?2 IS NULL OR d.source = ?2) AND (?3 IS NULL OR d.path LIKE ?3 ESCAPE '\\')",
        )?;
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let hit = st
                .query_row(params![id, source, like], |r| {
                    let body: String = r.get(8)?;
                    let (text, truncated) = cap_text(&body);
                    Ok(SectionHit {
                        id: r.get(9)?,
                        source: r.get(0)?,
                        source_kind: r.get(1)?,
                        path: r.get(2)?,
                        heading_path: r.get(3)?,
                        label: r.get(4)?,
                        line_start: r.get(5)?,
                        line_end: r.get(6)?,
                        date: r.get(7)?,
                        text,
                        truncated,
                    })
                })
                .optional()?;
            out.extend(hit);
        }
        Ok(out)
    }

    /// Every `(source, path)` with a section matching an FTS5 query, for
    /// the recall check.
    ///
    /// # Errors
    ///
    /// As [`Store::search_sections`].
    pub fn docs_matching(&self, query: &str) -> Result<HashSet<(String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT DISTINCT d.source, d.path FROM sections_fts f JOIN sections s ON s.id = f.rowid JOIN docs d ON d.id = s.doc_id WHERE sections_fts MATCH ?1",
        )?;
        let rows = st.query_map([query], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| Error::InvalidArgument(format!("fts query: {e}")))?;
        let mut out = HashSet::new();
        for r in rows {
            out.insert(r.map_err(|e| Error::InvalidArgument(format!("fts query: {e}")))?);
        }
        Ok(out)
    }

    /// Every indexed `(source, path)`, sorted.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn docs(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT source, path FROM docs ORDER BY source, path")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every identifier occurrence in one document: `(lowercased
    /// identifier, line)`, for the recall check.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn doc_idents(&self, source: &str, path: &str) -> Result<HashSet<(String, i64)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT lower(i.ident), i.line FROM idents i JOIN docs d ON d.id = i.doc_id WHERE d.source = ?1 AND d.path = ?2",
        )?;
        let rows = st.query_map(params![source, path], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The names identifiers in force.
    pub fn names(&self) -> &Names {
        &self.atlas.names
    }

    /// The compiled identifier rules.
    pub fn rules(&self) -> &Rules {
        &self.atlas.setup.rules
    }

    /// Reads a document from its vault or source through the confined
    /// readers, for the recall check.
    ///
    /// # Errors
    ///
    /// Path errors, [`Error::Io`].
    pub fn read_document(&self, source: &str, path: &str) -> Result<Vec<u8>> {
        Ok(self.read_doc(source, path)?.0)
    }

    /// The vaults and sources with their counts and scan times.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn list_sources(&self) -> Result<Vec<SourceInfo>> {
        let mut out = Vec::new();
        let sections = |name: &str| -> Result<usize> {
            let n: i64 = self
                .conn
                .prepare_cached("SELECT COUNT(*) FROM sections s JOIN docs d ON d.id = s.doc_id WHERE d.source = ?1")?
                .query_row([name], |r| r.get(0))?;
            Ok(n as usize)
        };
        for v in self.vaults.all() {
            out.push(SourceInfo {
                name: v.name.clone(),
                kind: "vault",
                root: crate::paths::strip_verbatim(v.root.clone()).display().to_string(),
                include: Vec::new(),
                files: self.doc_count(&v.name)?,
                sections: sections(&v.name)?,
                last_scan: self.atlas.last_scan.get(&v.name).map(|t| utc(*t)),
                last_indexed: self.last_indexed(&v.name)?,
            });
        }
        for s in &self.atlas.setup.sources {
            out.push(SourceInfo {
                name: s.name.clone(),
                kind: "source",
                root: crate::paths::strip_verbatim(s.root.clone()).display().to_string(),
                include: s.include.clone(),
                files: self.doc_count(&s.name)?,
                sections: sections(&s.name)?,
                last_scan: self.atlas.last_scan.get(&s.name).map(|t| utc(*t)),
                last_indexed: self.last_indexed(&s.name)?,
            });
        }
        Ok(out)
    }

    /// Records a miss: something found by grep or by reading that atlas did
    /// not return. Misses are never deleted.
    ///
    /// # Arguments
    ///
    /// * `tool`, `query`, `returned`, `found`, `reason` - see [`Miss`].
    /// * `client` - the MCP client name.
    ///
    /// # Returns
    ///
    /// The recorded miss.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the JSONL mirror cannot be appended, [`Error::Db`].
    pub fn report_miss(&mut self, tool: &str, query: &str, returned: &str, found: &str, reason: &str, client: &str) -> Result<Miss> {
        let tx = begin(&mut self.conn)?;
        let time = now();
        let id: i64 = tx.query_row(
            "INSERT INTO misses(time, tool, query, returned, found, reason, client) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
            params![time, tool, query, returned, found, reason, client],
            |r| r.get(0),
        )?;
        let m = Miss {
            id,
            time,
            tool: tool.into(),
            query: query.into(),
            returned: returned.into(),
            found: found.into(),
            reason: reason.into(),
            client: client.into(),
            status: "open".into(),
            resolved_time: None,
            resolution: None,
        };
        append_line(&self.atlas.misses_log, &crate::json!({"event": "report", "miss": m}))?;
        tx.commit()?;
        Ok(m)
    }

    /// Marks a miss resolved, with a note. The row stays.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown id, [`Error::InvalidArgument`] if
    /// it is already resolved, [`Error::Io`], [`Error::Db`].
    pub fn resolve_miss(&mut self, id: i64, note: &str, client: &str) -> Result<Miss> {
        let tx = begin(&mut self.conn)?;
        let status: Option<String> = tx.query_row("SELECT status FROM misses WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        match status.as_deref() {
            None => return Err(Error::NotFound(format!("miss {id}"))),
            Some("resolved") => return Err(Error::InvalidArgument(format!("miss {id} is already resolved"))),
            _ => {}
        }
        let time = now();
        tx.execute("UPDATE misses SET status = 'resolved', resolved_time = ?2, resolution = ?3 WHERE id = ?1", params![id, time, note])?;
        append_line(&self.atlas.misses_log, &crate::json!({"event": "resolve", "id": id, "time": time, "note": note, "client": client}))?;
        tx.commit()?;
        Ok(self.miss_rows("id = ?1", params![id], 1)?.0.remove(0))
    }

    /// Lists misses, newest first.
    ///
    /// # Arguments
    ///
    /// * `status` - `open`, `resolved`, or `None` for both.
    /// * `limit` - the most returned.
    ///
    /// # Returns
    ///
    /// The misses and how many matched.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn misses(&self, status: Option<&str>, limit: usize) -> Result<(Vec<Miss>, usize)> {
        self.miss_rows("(?1 IS NULL OR status = ?1)", params![status], limit)
    }

    fn miss_rows(&self, filter: &str, args: &[&dyn rusqlite::ToSql], limit: usize) -> Result<(Vec<Miss>, usize)> {
        let total: i64 = self.conn.query_row(&format!("SELECT COUNT(*) FROM misses WHERE {filter}"), args, |r| r.get(0))?;
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT id, time, tool, query, returned, found, reason, client, status, resolved_time, resolution FROM misses WHERE {filter} ORDER BY id DESC"
        ))?;
        let rows = st.query_map(args, |r| {
            Ok(Miss {
                id: r.get(0)?,
                time: r.get(1)?,
                tool: r.get(2)?,
                query: r.get(3)?,
                returned: r.get(4)?,
                found: r.get(5)?,
                reason: r.get(6)?,
                client: r.get(7)?,
                status: r.get(8)?,
                resolved_time: r.get(9)?,
                resolution: r.get(10)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows.take(limit) {
            out.push(r?);
        }
        Ok((out, total as usize))
    }
}

/// Replays `misses.jsonl` into an empty `misses` table, so a deleted
/// database loses no miss.
pub(super) fn restore_misses(tx: &Transaction<'_>, log: &Path) -> Result<()> {
    let n: i64 = tx.query_row("SELECT COUNT(*) FROM misses", [], |r| r.get(0))?;
    if n > 0 {
        return Ok(());
    }
    let Ok(text) = fs::read_to_string(log) else { return Ok(()) };
    for line in text.lines() {
        let Ok(v) = crate::json::parse(line) else { continue };
        match v.get("event").and_then(Value::as_str) {
            Some("report") => {
                let Ok(m) = crate::json::from_value::<Miss>(v["miss"].clone()) else { continue };
                tx.execute(
                    "INSERT OR IGNORE INTO misses(id, time, tool, query, returned, found, reason, client) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![m.id, m.time, m.tool, m.query, m.returned, m.found, m.reason, m.client],
                )?;
            }
            Some("resolve") => {
                tx.execute(
                    "UPDATE misses SET status = 'resolved', resolved_time = ?2, resolution = ?3 WHERE id = ?1",
                    params![v["id"].as_i64(), v["time"].as_str(), v["note"].as_str()],
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn append_line(log: &Path, v: &Value) -> Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    let mut f = fs::OpenOptions::new().append(true).create(true).open(log)?;
    f.write_all(line.as_bytes())?;
    f.sync_data()?;
    Ok(())
}

/// A list item and its continuation lines, from line `at` (0-based) of a
/// section body.
fn item_block(body: &str, at: usize) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let Some(first) = lines.get(at) else { return String::new() };
    let indent = first.len() - first.trim_start().len();
    let mut end = at + 1;
    while end < lines.len() {
        let l = lines[end];
        let ind = l.len() - l.trim_start().len();
        if l.trim().is_empty() {
            // A blank line continues the item only if the next line is indented under it.
            match lines.get(end + 1) {
                Some(n) if !n.trim().is_empty() && n.len() - n.trim_start().len() > indent => {
                    end += 1;
                    continue;
                }
                _ => break,
            }
        }
        if ind <= indent {
            break;
        }
        end += 1;
    }
    lines[at..end].join("\n")
}

/// The LIKE pattern for "paths under `folder`": `%`, `_` and the escape
/// character in the folder are literal (`ESCAPE '\\'`), so a folder filter
/// never widens to other folders.
fn folder_like(folder: &str) -> String {
    format!("{}/%", like_literal(folder.trim_end_matches('/')))
}

/// `s` with the LIKE wildcards and the escape character escaped by `\`.
fn like_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc(UNIX_EPOCH + std::time::Duration::from_millis(1_761_825_600_123)), "2025-10-30T12:00:00.123Z");
        assert_eq!(utc(UNIX_EPOCH + std::time::Duration::from_secs(951_782_400)), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn item_blocks_keep_continuations() {
        let body = "- a: one\n  more a\n\n  still a\n- b\n";
        assert_eq!(item_block(body, 0), "- a: one\n  more a\n\n  still a");
        assert_eq!(item_block(body, 4), "- b");
    }
}
