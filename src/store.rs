//! The SQLite store: a derived index of the vaults plus the coordination
//! layer shared by every server process.
//!
//! The Markdown files are the source of truth. `notes`, `links`, `tags` and
//! `fts` are rebuilt from them by [`Store::rescan`], so deleting the database
//! loses nothing but the `writes` audit log and the `ids` counters (and the
//! counters are re-derived from the files). Every writer takes the database's
//! write lock (`BEGIN IMMEDIATE`) before it touches a file, so writes from
//! several server processes are serialised, and the rows are updated in the
//! same transaction as the file write.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use crate::json::{Map, Value};

use crate::error::{Error, Result};
use crate::fsio;
use crate::links::{self, Resolution, Resolver};
use crate::markdown::{self, LinkKind, RawLink};
use crate::paths::{self, Resolved, Vault, Vaults};
use crate::sha256;

#[path = "store_atlas.rs"]
mod atlas;
pub use atlas::{utc, AtlasSetup, DefText, Freshness, IdHit, IdSummary, Lookup, Miss, Refresh, SectionHit, SourceInfo, StaleFile, MAX_SECTION_TEXT, MISSES_LOG, STALE_LIST_CAP};
use atlas::{Atlas, DocRules};

/// The derived tables' version; a mismatch rebuilds them, and the atlas
/// tables with them (it is part of the atlas fingerprint).
///
/// It is the table shape (the leading number, bumped when the derived tables
/// change shape), the parse rules (`parse-N`, bumped when markdown, registry,
/// grammar or chunking change what a file indexes as) and the crate version,
/// so an index an older binary wrote rebuilds itself after an update instead
/// of keeping that binary's reading of the files.
const SCHEMA: &str = concat!("1/parse-1/", env!("CARGO_PKG_VERSION"));

/// Bumped whenever the durable tables gain a migration step, so the open
/// fast path runs the checks once more.
const DURABLE_VERSION: &str = "3";

/// Files larger than this are not indexed at all.
const MAX_INDEX: u64 = 8 << 20;

/// File name of the append-only JSONL mirror of the `writes` table. It lives
/// beside the database and survives the database being deleted.
pub const AUDIT_LOG: &str = "writes.jsonl";

/// A `discussion` value meaning "the note this call creates" (used when a
/// thread is opened, so its own audit row names it).
pub const THIS_NOTE: &str = "<this note>";

const DERIVED: &str = "
CREATE TABLE IF NOT EXISTS notes(
    id INTEGER PRIMARY KEY,
    vault TEXT NOT NULL,
    path TEXT NOT NULL,
    mtime_ns INTEGER NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    title TEXT NOT NULL,
    frontmatter TEXT NOT NULL,
    UNIQUE(vault, path)
);
CREATE TABLE IF NOT EXISTS links(
    note_id INTEGER NOT NULL,
    line INTEGER NOT NULL,
    kind TEXT NOT NULL,
    raw TEXT NOT NULL,
    target TEXT NOT NULL,
    heading TEXT,
    to_vault TEXT,
    to_path TEXT,
    attachment INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS links_note ON links(note_id);
CREATE INDEX IF NOT EXISTS links_to ON links(to_vault, to_path);
CREATE TABLE IF NOT EXISTS tags(note_id INTEGER NOT NULL, tag TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS tags_note ON tags(note_id);
CREATE INDEX IF NOT EXISTS tags_tag ON tags(tag);
CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(title, body, tokenize = 'unicode61');
";

/// The handle store's table (see [`crate::handles`]): one row per stored
/// output, whose bytes live in `<data dir>/handles/<sha[0..2]>/<sha>`.
/// Durable, so a derived-table rebuild never orphans the files; the CLI
/// creates it on its own connection without opening the whole store.
pub const HANDLES_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS handles(
    id TEXT PRIMARY KEY,
    sha TEXT NOT NULL,
    kind TEXT NOT NULL,
    label TEXT NOT NULL,
    cmd TEXT NOT NULL,
    cwd TEXT NOT NULL,
    session TEXT NOT NULL DEFAULT '',
    created TEXT NOT NULL,
    last_used TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    lines INTEGER NOT NULL,
    exit_code INTEGER,
    ms INTEGER
);
CREATE INDEX IF NOT EXISTS handles_used ON handles(last_used);
CREATE INDEX IF NOT EXISTS handles_sha ON handles(sha);
";

const DURABLE: &str = "
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS ids(seq TEXT PRIMARY KEY, last INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS indices(
    vault TEXT NOT NULL,
    note TEXT NOT NULL,
    name TEXT NOT NULL,
    spec TEXT NOT NULL,
    last_hash TEXT NOT NULL,
    updated TEXT NOT NULL,
    PRIMARY KEY(vault, note, name)
);
CREATE TABLE IF NOT EXISTS writes(
    id INTEGER PRIMARY KEY,
    time TEXT NOT NULL,
    tool TEXT NOT NULL,
    vault TEXT NOT NULL,
    path TEXT NOT NULL,
    sha_before TEXT,
    sha_after TEXT,
    client TEXT NOT NULL,
    pid INTEGER NOT NULL
);
";

/// One indexed note.
#[derive(Debug, Clone)]
pub struct NoteRow {
    /// Vault short name.
    pub vault: String,
    /// Vault-relative path.
    pub path: String,
    /// Size in bytes.
    pub size: i64,
    /// SHA-256 of the file, hex.
    pub sha256: String,
    /// The file stem, which is what Obsidian shows and links by.
    pub title: String,
    /// Parsed frontmatter.
    pub frontmatter: Map,
}

/// One indexed link.
#[derive(Debug, Clone)]
pub struct LinkRow {
    /// Vault of the note the link is in.
    pub from_vault: String,
    /// Path of the note the link is in.
    pub from_path: String,
    /// 1-based line of the link.
    pub line: i64,
    /// `wiki`, `embed`, `md` or `uri`.
    pub kind: String,
    /// The link as written.
    pub raw: String,
    /// The target as written.
    pub target: String,
    /// The heading the link names, if any.
    pub heading: Option<String>,
    /// Resolved vault, or `None` when the target is missing.
    pub to_vault: Option<String>,
    /// Resolved path, or `None` when the target is missing.
    pub to_path: Option<String>,
}

crate::impl_to_json!(LinkRow { from_vault, from_path, line, kind, raw, target, heading, to_vault, to_path });

/// What a rescan did.
#[derive(Debug, Clone, Default)]
pub struct RescanStats {
    /// Notes found on disk across all vaults.
    pub files: usize,
    /// Notes read and re-parsed because their content changed or was new.
    pub parsed: usize,
    /// Notes whose mtime changed but whose content hash did not.
    pub touched: usize,
    /// Notes that disappeared from disk and left the index.
    pub removed: usize,
    /// Wall time of the rescan in microseconds.
    pub elapsed_us: u128,
}

crate::impl_to_json!(RescanStats { files, parsed, touched, removed, elapsed_us });

/// Who made a write and why. Both go into the `writes` row and the JSONL
/// audit line; neither is written into the note.
#[derive(Debug, Clone)]
pub struct Actor {
    /// The MCP client name from `initialize`.
    pub client: String,
    /// The caller's stated reason for the change.
    pub reason: String,
    /// The discussion thread the write cites (`vault:path`), or empty.
    pub discussion: String,
}

/// One row of the audit log, as `write_log` returns it.
#[derive(Debug, Clone)]
pub struct WriteEntry {
    /// UTC time, `YYYY-MM-DDTHH:MM:SS.sssZ`.
    pub time: String,
    /// The tool that wrote.
    pub tool: String,
    /// Vault short name.
    pub vault: String,
    /// Vault-relative path.
    pub path: String,
    /// MCP client name.
    pub client: String,
    /// The stated reason (empty for rows written before reasons existed).
    pub reason: String,
    /// Hash before the write (`None` for a new note).
    pub sha_before: Option<String>,
    /// Hash after the write (`None` for an id reservation).
    pub sha_after: Option<String>,
    /// The discussion thread the write cites (`vault:path`), or empty.
    pub discussion: String,
}

crate::impl_to_json!(WriteEntry { time, tool, vault, path, client, reason, sha_before, sha_after, discussion });

/// Filters for [`Store::write_log`]; `None` means no filter.
#[derive(Debug, Clone, Default)]
pub struct WriteFilter {
    /// Vault short name.
    pub vault: Option<String>,
    /// Exact vault-relative path.
    pub path: Option<String>,
    /// Exact client name.
    pub client: Option<String>,
    /// Earliest time, inclusive, compared as text against the stored UTC
    /// timestamps (`2026-10-08` and `2026-10-08T12:00` both work).
    pub since: Option<String>,
    /// Latest time, exclusive, compared the same way.
    pub until: Option<String>,
    /// Exact discussion thread, `vault:path`.
    pub discussion: Option<String>,
}

/// The outcome of a write tool.
#[derive(Debug, Clone)]
pub struct WriteResult {
    /// Vault short name.
    pub vault: String,
    /// Vault-relative path written.
    pub path: String,
    /// Hash before the write (`None` for a new note).
    pub sha256_before: Option<String>,
    /// Hash after the write.
    pub sha256: String,
    /// False when the edit produced identical bytes and nothing was written.
    pub changed: bool,
}

crate::impl_to_json!(WriteResult { vault, path, sha256_before, sha256, changed });

struct Parsed {
    title: String,
    frontmatter: String,
    body: String,
    links: Vec<RawLink>,
    tags: Vec<String>,
    sha: String,
    size: i64,
    mtime: i64,
}

fn parse(path: &str, bytes: &[u8], mtime: i64) -> Parsed {
    let text = String::from_utf8_lossy(bytes);
    let fm = markdown::parse_frontmatter(&text);
    Parsed {
        title: links::stem(path).to_string(),
        frontmatter: Value::Object(fm.clone()).to_string(),
        body: text.to_string(),
        links: markdown::extract_links(&text),
        tags: markdown::extract_tags(&text, &fm),
        sha: sha256::hex(bytes),
        size: bytes.len() as i64,
        mtime,
    }
}

fn kind_from(s: &str) -> LinkKind {
    match s {
        "embed" => LinkKind::Embed,
        "md" => LinkKind::Md,
        "uri" => LinkKind::Uri,
        _ => LinkKind::Wiki,
    }
}

/// The database plus the vault set it indexes.
pub struct Store {
    conn: Connection,
    vaults: Vaults,
    /// The append-only JSONL mirror of the `writes` table, beside the database.
    audit_log: PathBuf,
    /// Sources, identifier rules and scan state.
    atlas: Atlas,
    /// Keeps the memory store in step with the sources it was imported
    /// from (see [`crate::memory::sync`]); `None` when off.
    pub(crate) memsync: Option<crate::memory::sync::Syncer>,
}

fn begin(conn: &mut Connection) -> Result<Transaction<'_>> {
    Ok(conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

impl Store {
    /// Opens (or creates) the database and brings its schema up to date.
    ///
    /// # Arguments
    ///
    /// * `db_path` - the SQLite file; its folder is created if missing.
    /// * `vaults` - the vault set this process serves.
    ///
    /// # Returns
    ///
    /// The store. Call [`Store::rescan`] before the first query.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the folder cannot be made, [`Error::Db`] if SQLite
    /// refuses to open the file or set WAL mode.
    pub fn open(db_path: &Path, vaults: Vaults) -> Result<Store> {
        Store::open_with(db_path, vaults, AtlasSetup::default())
    }

    /// Opens (or creates) the database with read-only sources and
    /// identifier rules.
    ///
    /// # Arguments
    ///
    /// * `db_path` - the SQLite file; its folder is created if missing.
    /// * `vaults` - the vault set this process serves.
    /// * `setup` - sources, identifier patterns and limits.
    ///
    /// # Returns
    ///
    /// The store. Call [`Store::rescan`] and [`Store::rescan_sources`]
    /// before the first query.
    ///
    /// # Errors
    ///
    /// As [`Store::open`].
    pub fn open_with(db_path: &Path, vaults: Vaults, setup: AtlasSetup) -> Result<Store> {
        let mut atlas = Atlas::new(setup, db_path);
        if let Some(dir) = db_path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        // Switching a new file to WAL and the first schema write can report
        // SQLITE_BUSY without consulting the busy handler when several
        // processes start at once, so the whole open is retried.
        let audit_log = db_path.with_file_name(AUDIT_LOG);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut wait = 5;
        loop {
            match Store::open_once(db_path, &vaults, &mut atlas) {
                Ok(conn) => {
                    atlas.start_watch(&vaults);
                    return Ok(Store { conn, vaults, audit_log, atlas, memsync: None });
                }
                Err(Error::Db(m)) if (m.contains("locked") || m.contains("busy")) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(wait));
                    wait = (wait * 2).min(100);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn open_once(db_path: &Path, vaults: &Vaults, atlas: &mut Atlas) -> Result<Connection> {
        let mut conn = Connection::open(db_path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Db(format!("could not enable WAL (journal_mode is {mode})")));
        }
        // synchronous=NORMAL: in WAL mode a commit is durable against a process
        // crash and only the last commits can be lost on power failure; the
        // derived rows are rebuilt from the files, and the audit and misses
        // lines are fsynced to their JSONL mirrors separately.
        // mmap_size: reads of the index go through the page cache without a
        // copy (256 MiB covers a large index). cache_size: 16 MiB of pages per
        // connection keeps a warm lookup off the file entirely.
        // temp_store=MEMORY: sorts and GROUP BY temp tables never touch disk.
        conn.execute_batch(
            "PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=OFF; PRAGMA mmap_size=268435456; PRAGMA cache_size=-16384; PRAGMA temp_store=MEMORY;",
        )?;
        // Every tool keeps its statements prepared: the default cache of 16
        // is smaller than the set one server session uses, and a statement
        // pushed out is compiled again on its next call.
        conn.set_prepared_statement_cache_capacity(128);
        // The names set is stored between runs, so an open does not walk the
        // name sources; a source scan re-checks it.
        let stored = |conn: &Connection, key: &str| -> Option<String> {
            conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional().ok().flatten()
        };
        atlas.names = match stored(&conn, "atlas_names") {
            Some(s) => crate::registry::Names::decode(&s),
            None => atlas.compute_names(),
        };
        let fingerprint = atlas.fingerprint(vaults);
        // Fast path: one read when nothing about the schema, the rules or the
        // vault roots changed since the last open, instead of a write
        // transaction of DDL and migration checks.
        let stamp = format!("{SCHEMA}|{DURABLE_VERSION}|{fingerprint}");
        if stored(&conn, "open_stamp").as_deref() == Some(stamp.as_str()) {
            return Ok(conn);
        }
        let misses_log = atlas.misses_log.clone();
        let fingerprint = fingerprint.as_str();
        {
            let tx = begin(&mut conn)?;
            tx.execute_batch(DURABLE)?;
            tx.execute_batch(HANDLES_SCHEMA)?;
            tx.execute_batch(atlas::ATLAS_DURABLE)?;
            migrate_durable(&tx)?;
            atlas::restore_misses(&tx, &misses_log)?;
            let schema: Option<String> = tx.query_row("SELECT value FROM meta WHERE key='schema'", [], |r| r.get(0)).optional()?;
            if schema.as_deref() != Some(SCHEMA) {
                tx.execute_batch(
                    "DROP TABLE IF EXISTS notes; DROP TABLE IF EXISTS links; DROP TABLE IF EXISTS tags; DROP TABLE IF EXISTS fts;",
                )?;
                tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('schema', ?1)", [SCHEMA])?;
                // The atlas rows were read by the same older rules: they go too.
                tx.execute("DELETE FROM meta WHERE key = 'atlas'", [])?;
            }
            tx.execute_batch(DERIVED)?;
            atlas::migrate_derived(&tx)?;
            tx.execute_batch(atlas::ATLAS_DERIVED)?;
            atlas.rebuilt = atlas::ensure_fingerprint(&tx, fingerprint)?;
            if schema.is_some() && schema.as_deref() != Some(SCHEMA) {
                atlas.rebuilt = Some("schema rebuild");
            }
            for v in vaults.all() {
                let key = format!("root:{}", v.name);
                let root = v.root.to_string_lossy().to_string();
                let old: Option<String> = tx.query_row("SELECT value FROM meta WHERE key=?1", [&key], |r| r.get(0)).optional()?;
                if old.as_deref() != Some(root.as_str()) {
                    if old.is_some() {
                        let ids: Vec<i64> = {
                            let mut st = tx.prepare("SELECT id FROM notes WHERE vault=?1")?;
                            let rows = st.query_map([&v.name], |r| r.get(0))?;
                            rows.collect::<std::result::Result<_, _>>()?
                        };
                        for id in ids {
                            delete_rows(&tx, id)?;
                        }
                    }
                    tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)", params![key, root])?;
                }
            }
            atlas::write_stamp(&tx, &atlas.names, &stamp)?;
            tx.commit()?;
        }
        Ok(conn)
    }

    /// The served vaults.
    pub fn vaults(&self) -> &Vaults {
        &self.vaults
    }

    /// Brings the derived tables in line with the files.
    ///
    /// Walks every vault and compares each note's mtime and size with its
    /// row; only notes that differ are read, and only those whose hash
    /// differs are re-parsed. Notes gone from disk leave `notes`, `links`,
    /// `tags` and `fts` (but never `writes`). When the set of note paths
    /// changed, every link is resolved again, since a new note can satisfy
    /// a link that was broken.
    ///
    /// # Returns
    ///
    /// What changed and how long it took.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if a vault root cannot be read; [`Error::Db`] on SQLite
    /// failure.
    pub fn rescan(&mut self) -> Result<RescanStats> {
        let t0 = Instant::now();
        // Watched roots with nothing dirty are skipped; dirty sets are
        // checked path by path; anything else walks every vault.
        let names: Vec<String> = self.vaults.all().iter().map(|v| v.name.clone()).collect();
        let plans: Vec<atlas::Plan> = names.iter().map(|n| self.plan_for(n)).collect();
        let at = Instant::now();
        if plans.iter().all(|p| matches!(p, atlas::Plan::Skip(..))) {
            return Ok(RescanStats { elapsed_us: t0.elapsed().as_micros(), ..Default::default() });
        }
        let mut only: Option<Vec<(String, Vec<paths::Found>, Vec<String>)>> = Some(Vec::new());
        for (n, p) in names.iter().zip(&plans) {
            match p {
                atlas::Plan::Skip(..) => {}
                atlas::Plan::Paths(_, snap) => match self.stat_dirty(n, &snap.paths)? {
                    Some((found, gone)) => {
                        if let Some(o) = &mut only {
                            o.push((n.clone(), found, gone));
                        }
                    }
                    None => only = None,
                },
                atlas::Plan::Walk(_) => only = None,
            }
        }
        let walked = only.is_none();
        let stats = self.rescan_inner(only.as_deref(), t0)?;
        for (n, p) in names.iter().zip(plans) {
            let p = if walked { p.into_walk() } else { p };
            self.atlas.settled(n, &p, at);
        }
        Ok(stats)
    }

    /// The rescan proper: every vault walked when `only` is `None`, else
    /// only the listed files (found, with their stat) and gone paths.
    fn rescan_inner(&mut self, only: Option<&[(String, Vec<paths::Found>, Vec<String>)]>, t0: Instant) -> Result<RescanStats> {
        let served: HashSet<&str> = self.vaults.all().iter().map(|v| v.name.as_str()).collect();
        // A full walk compares against the map of the last one while the
        // database is unchanged (see `atlas::KnownCache`).
        let cache_ok = only.is_none() && self.atlas.known.borrow_mut().validate(&self.conn)?;
        let cached = if cache_ok { self.atlas.known.borrow().vaults.clone() } else { None };
        let kn: std::sync::Arc<atlas::VaultKnown> = match cached {
            Some(k) => k,
            None => {
                let mut known: HashMap<(String, String), (i64, i64, String)> = HashMap::new();
                // Notes whose atlas row is missing or out of step (a database written
                // by an older build) are re-parsed even when their mtime is unchanged.
                let mut unsynced: HashSet<(String, String)> = HashSet::new();
                {
                    const SELECT: &str =
                        "SELECT n.vault, n.path, n.mtime_ns, n.size, n.sha256, d.sha256 FROM notes n LEFT JOIN docs d ON d.source = n.vault AND d.path = n.path";
                    let mut add = |row: ((String, String), (i64, i64, String), Option<String>)| {
                        let (k, v, doc) = row;
                        if served.contains(k.0.as_str()) {
                            if doc.as_deref() != Some(v.2.as_str()) {
                                unsynced.insert(k.clone());
                            }
                            known.insert(k, v);
                        }
                    };
                    let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<((String, String), (i64, i64, String), Option<String>)> {
                        Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?, r.get(4)?), r.get::<_, Option<String>>(5)?))
                    };
                    match only {
                        None => {
                            let mut st = self.conn.prepare_cached(SELECT)?;
                            for row in st.query_map([], map)? {
                                add(row?);
                            }
                        }
                        Some(list) => {
                            let mut st = self.conn.prepare_cached(&format!("{SELECT} WHERE n.vault = ?1 AND n.path = ?2"))?;
                            for (v, found, gone) in list {
                                for p in found.iter().map(|f| &f.rel).chain(gone) {
                                    if let Some(row) = st.query_row(params![v, p], map).optional()? {
                                        add(row);
                                    }
                                }
                            }
                        }
                    }
                }
                let k = std::sync::Arc::new((known, unsynced));
                if cache_ok {
                    self.atlas.known.borrow_mut().vaults = Some(k.clone());
                }
                k
            }
        };
        let (known, unsynced) = (&kn.0, &kn.1);
        let mut stats = RescanStats::default();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut parsed: Vec<(String, String, Parsed)> = Vec::new();
        let mut touched: Vec<(String, String, i64, i64)> = Vec::new();
        for v in self.vaults.all() {
            let files = match only {
                None => {
                    let files = paths::walk(v)?;
                    self.atlas.last_scan.insert(v.name.clone(), std::time::SystemTime::now());
                    files
                }
                Some(list) => list.iter().filter(|(n, _, _)| *n == v.name).flat_map(|(_, f, _)| f.iter().cloned()).collect(),
            };
            for f in files {
                if f.size > MAX_INDEX {
                    continue;
                }
                stats.files += 1;
                let key = (v.name.clone(), f.rel.clone());
                let prev = known.get(&key);
                seen.insert(key);
                let synced = unsynced.is_empty() || !unsynced.contains(&key_ref(&v.name, &f.rel));
                if let Some((m, s, _)) = prev {
                    if *m == f.mtime_ns && *s == f.size as i64 && synced {
                        continue;
                    }
                }
                let Ok(bytes) = fs::read(v.root.join(&f.rel)) else { continue };
                let p = parse(&f.rel, &bytes, f.mtime_ns);
                match prev {
                    Some((_, _, sha)) if *sha == p.sha && synced => touched.push((v.name.clone(), f.rel, p.mtime, p.size)),
                    _ => parsed.push((v.name.clone(), f.rel, p)),
                }
            }
        }
        let removed: Vec<(String, String)> = match only {
            None => known.keys().filter(|k| !seen.contains(*k)).cloned().collect(),
            Some(list) => list
                .iter()
                .flat_map(|(v, _, gone)| gone.iter().map(move |g| (v.clone(), g.clone())))
                .filter(|k| known.contains_key(k))
                .collect(),
        };
        stats.parsed = parsed.len();
        stats.touched = touched.len();
        stats.removed = removed.len();
        if parsed.is_empty() && touched.is_empty() && removed.is_empty() {
            stats.elapsed_us = t0.elapsed().as_micros();
            return Ok(stats);
        }
        let paths_changed = !removed.is_empty() || parsed.iter().any(|(v, p, _)| !known.contains_key(&(v.clone(), p.clone())));
        let tx = begin(&mut self.conn)?;
        for (v, p) in &removed {
            if let Some(id) = note_id(&tx, v, p)? {
                delete_rows(&tx, id)?;
            }
            atlas::delete_doc(&tx, v, p)?;
        }
        for (v, p, m, s) in &touched {
            tx.execute("UPDATE notes SET mtime_ns=?3, size=?4 WHERE vault=?1 AND path=?2", params![v, p, m, s])?;
            atlas::touch_doc(&tx, v, p, *m, *s)?;
        }
        let mut ids = Vec::with_capacity(parsed.len());
        for (v, p, parsed) in &parsed {
            ids.push(upsert(&tx, &self.atlas, v, p, parsed)?);
        }
        if paths_changed {
            resolve(&tx, &self.vaults, None)?;
        } else {
            resolve(&tx, &self.vaults, Some(&ids))?;
        }
        tx.commit()?;
        stats.elapsed_us = t0.elapsed().as_micros();
        Ok(stats)
    }

    /// Notes in a vault (or all vaults) under a folder, optionally filtered
    /// by a glob, sorted by vault and path.
    ///
    /// # Arguments
    ///
    /// * `vault` - a vault short name, or `None` for every served vault.
    /// * `folder` - vault-relative folder; empty for the root.
    /// * `recursive` - include subfolders.
    /// * `glob` - see [`crate::glob::matches`].
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn notes(&self, vault: Option<&str>, folder: &str, recursive: bool, glob: Option<&str>) -> Result<Vec<NoteRow>> {
        let mut st = self.conn.prepare_cached("SELECT vault, path, size, sha256, title, frontmatter FROM notes ORDER BY vault, path")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, String>(5)?))
        })?;
        let served: HashSet<&str> = self.vaults.all().iter().map(|v| v.name.as_str()).collect();
        let prefix = if folder.is_empty() { String::new() } else { format!("{folder}/") };
        let mut out = Vec::new();
        for row in rows {
            let (v, path, size, sha256, title, fm) = row?;
            if !served.contains(v.as_str()) || vault.is_some_and(|x| x != v) {
                continue;
            }
            let Some(rest) = path.strip_prefix(&prefix) else { continue };
            if !recursive && rest.contains('/') {
                continue;
            }
            if let Some(g) = glob {
                if !crate::glob::matches(g, rest) {
                    continue;
                }
            }
            let frontmatter = match crate::json::parse(&fm) {
                Ok(Value::Object(m)) => m,
                _ => Map::new(),
            };
            out.push(NoteRow { vault: v, path, size, sha256, title, frontmatter });
        }
        Ok(out)
    }

    /// True if the index has this note.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn has_note(&self, vault: &str, path: &str) -> Result<bool> {
        Ok(note_id(&self.conn, vault, path)?.is_some())
    }

    /// Indexed text of every note, for substring and regex search.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn bodies(&self, vault: Option<&str>) -> Result<Vec<(String, String, String)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT n.vault, n.path, f.body FROM notes n JOIN fts f ON f.rowid = n.id ORDER BY n.vault, n.path")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let row = row?;
            if vault.is_none_or(|v| v == row.0) && self.vaults.get(&row.0).is_ok() {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Full-text search through FTS5, best match first.
    ///
    /// # Arguments
    ///
    /// * `query` - an FTS5 query (`"exact phrase"`, `a AND b`, `pre*`).
    /// * `vault` - restrict to one vault.
    /// * `limit` - maximum notes returned.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for FTS5 syntax errors, [`Error::Db`]
    /// otherwise.
    pub fn fts(&self, query: &str, vault: Option<&str>, limit: usize) -> Result<Vec<(String, String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT n.vault, n.path, f.body FROM fts f JOIN notes n ON n.id = f.rowid WHERE fts MATCH ?1 AND (?2 IS NULL OR n.vault = ?2) ORDER BY rank LIMIT ?3",
        )?;
        let rows = st
            .query_map(params![query, vault, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| Error::InvalidArgument(format!("fts query: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| Error::InvalidArgument(format!("fts query: {e}")))?);
        }
        Ok(out)
    }

    fn link_rows(&self, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<LinkRow>> {
        let sql = format!(
            "SELECT n.vault, n.path, l.line, l.kind, l.raw, l.target, l.heading, l.to_vault, l.to_path FROM links l JOIN notes n ON n.id = l.note_id WHERE {filter} ORDER BY n.vault, n.path, l.line, l.raw"
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let rows = st.query_map(args, |r| {
            Ok(LinkRow {
                from_vault: r.get(0)?,
                from_path: r.get(1)?,
                line: r.get(2)?,
                kind: r.get(3)?,
                raw: r.get(4)?,
                target: r.get(5)?,
                heading: r.get(6)?,
                to_vault: r.get(7)?,
                to_path: r.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let row = row?;
            if self.vaults.get(&row.from_vault).is_ok() {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Links from any served vault that resolve to this note.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn backlinks(&self, vault: &str, path: &str) -> Result<Vec<LinkRow>> {
        self.link_rows("l.to_vault = ?1 AND l.to_path = ?2", &[&vault, &path])
    }

    /// Links written in this note, resolved.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn outlinks(&self, vault: &str, path: &str) -> Result<Vec<LinkRow>> {
        self.link_rows("n.vault = ?1 AND n.path = ?2", &[&vault, &path])
    }

    /// Links whose target note is missing (attachments are not counted).
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn broken(&self, vault: Option<&str>) -> Result<Vec<LinkRow>> {
        self.link_rows("l.to_path IS NULL AND l.attachment = 0 AND (?1 IS NULL OR n.vault = ?1)", &[&vault])
    }

    /// Today's date in the machine's local time zone, `YYYY-MM-DD`.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn local_date(&self) -> Result<String> {
        Ok(self.conn.query_row("SELECT date('now', 'localtime')", [], |r| r.get(0))?)
    }

    /// Edits an existing note under the write lock.
    ///
    /// Takes the database write lock, reads the note, checks
    /// `expected_sha256`, applies `edit`, writes the result atomically,
    /// updates the note's rows and appends an audit row, then commits. If the
    /// file changes on disk between the read and the write, the edit is
    /// refused rather than overwriting someone else's change.
    ///
    /// # Arguments
    ///
    /// * `tool` - the tool name, for the audit log.
    /// * `who` - the MCP client name and the reason, for the audit log.
    /// * `vault` - the vault.
    /// * `rel` - the note path.
    /// * `expected_sha256` - when given, must equal the file's current hash.
    /// * `edit` - turns the current text into the new text.
    ///
    /// # Returns
    ///
    /// The hashes before and after; `changed` is false (and nothing is
    /// written or logged) when `edit` returns identical text.
    ///
    /// # Errors
    ///
    /// Path errors from [`paths::resolve_existing`], [`Error::StaleSha`],
    /// errors from `edit`, [`Error::TooLarge`] past the note cap,
    /// [`Error::Io`] and [`Error::Db`].
    pub fn modify(
        &mut self,
        tool: &str,
        who: &Actor,
        vault: &Vault,
        rel: &str,
        expected_sha256: Option<&str>,
        edit: impl FnOnce(&str) -> Result<String>,
    ) -> Result<WriteResult> {
        let tx = begin(&mut self.conn)?;
        let res = paths::resolve_existing(vault, rel)?;
        let bytes = fsio::read_capped(&res.full)?;
        let before = sha256::hex(&bytes);
        if let Some(exp) = expected_sha256 {
            if !exp.eq_ignore_ascii_case(&before) {
                return Err(Error::StaleSha { expected: exp.to_string(), actual: before });
            }
        }
        let text = String::from_utf8(bytes).map_err(|_| Error::InvalidArgument("note is not valid UTF-8".into()))?;
        let new = edit(&text)?;
        if new == text {
            tx.commit()?;
            return Ok(WriteResult {
                vault: vault.name.clone(),
                path: res.rel,
                sha256_before: Some(before.clone()),
                sha256: before,
                changed: false,
            });
        }
        let again = sha256::hex(&fsio::read_capped(&res.full)?);
        if again != before {
            return Err(Error::StaleSha { expected: before, actual: again });
        }
        fsio::replace_atomic(&res.full, new.as_bytes())?;
        let after = record_file(&tx, &self.atlas, &self.vaults, vault, &res, new.as_bytes(), false)?;
        audit(&tx, &self.audit_log, tool, who, &vault.name, &res.rel, Some(&before), Some(&after))?;
        tx.commit()?;
        Ok(WriteResult { vault: vault.name.clone(), path: res.rel, sha256_before: Some(before), sha256: after, changed: true })
    }

    /// Creates a new note under the write lock; fails if anything exists at
    /// the path.
    ///
    /// # Arguments
    ///
    /// * `tool`, `who` - for the audit log.
    /// * `vault`, `rel` - where to create it.
    /// * `text` - the contents.
    /// * `create_dirs` - create missing parent folders.
    ///
    /// # Errors
    ///
    /// Path errors from [`paths::resolve_new`], [`Error::AlreadyExists`],
    /// [`Error::TooLarge`], [`Error::Io`] and [`Error::Db`].
    pub fn create(&mut self, tool: &str, who: &Actor, vault: &Vault, rel: &str, text: &str, create_dirs: bool) -> Result<WriteResult> {
        let tx = begin(&mut self.conn)?;
        let res = paths::resolve_new(vault, rel, create_dirs)?;
        fsio::create_exclusive(&res.full, text.as_bytes())?;
        let after = record_file(&tx, &self.atlas, &self.vaults, vault, &res, text.as_bytes(), true)?;
        audit(&tx, &self.audit_log, tool, who, &vault.name, &res.rel, None, Some(&after))?;
        tx.commit()?;
        Ok(WriteResult { vault: vault.name.clone(), path: res.rel, sha256_before: None, sha256: after, changed: true })
    }

    /// Allocates the next id of a numbered sequence such as `PI-244`, and
    /// optionally creates its note, as one step under the write lock.
    ///
    /// The candidate is one past the larger of the stored counter and the
    /// highest number already used by a file name in the folder. A candidate
    /// is skipped while any entry in the folder is named `<id>` or starts with
    /// `<id> ` or `<id>.`; the note is then created with an exclusive create,
    /// and on a collision (a writer outside this server) the next number is
    /// tried. An id is never handed out twice, even if no note is created.
    ///
    /// # Arguments
    ///
    /// * `tool` - the tool name, for the audit log.
    /// * `who` - the client and reason, for the audit log.
    /// * `vault` - the vault.
    /// * `folder` - the existing folder the sequence lives in (`Ideas`).
    /// * `prefix` - the id prefix (`PI-`).
    /// * `digits` - zero-padded width (3 gives `PI-007`).
    /// * `note` - `(name, content)`: create `<folder>/<id> <name>.md` with
    ///   `content`, where every `{{id}}` is replaced by the id.
    ///
    /// # Returns
    ///
    /// The id, and the write result when a note was created.
    ///
    /// # Errors
    ///
    /// Path errors, [`Error::InvalidArgument`] if 1,000 candidates in a row
    /// are taken, [`Error::Io`] and [`Error::Db`].
    #[allow(clippy::too_many_arguments)]
    pub fn next_id(
        &mut self,
        tool: &str,
        who: &Actor,
        vault: &Vault,
        folder: &str,
        prefix: &str,
        digits: usize,
        note: Option<(&str, &str)>,
    ) -> Result<(String, Option<WriteResult>)> {
        let tx = begin(&mut self.conn)?;
        let dir = paths::resolve_folder(vault, folder)?;
        // Ids are unique per vault and prefix, not per folder: a vault that files
        // C-020 under Missions must not hand out C-020 again under Systems.
        let seq = format!("{}/{}", vault.name, prefix);
        let stored: i64 = tx.query_row("SELECT last FROM ids WHERE seq=?1", [&seq], |r| r.get(0)).optional()?.unwrap_or(0);
        let mut names: Vec<String> = fs::read_dir(&dir.full)?.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        names.extend(paths::walk(vault)?.into_iter().filter_map(|f| f.rel.rsplit('/').next().map(str::to_owned)));
        let number_of = |name: &str| -> Option<i64> {
            let rest = name.strip_prefix(prefix)?;
            let d: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if d.is_empty() || d.len() > 12 {
                return None;
            }
            d.parse().ok()
        };
        let on_disk = names.iter().filter_map(|n| number_of(n)).max().unwrap_or(0);
        let mut n = stored.max(on_disk) + 1;
        for _ in 0..1000 {
            let id = format!("{prefix}{n:0digits$}");
            let taken = names.iter().any(|x| x == &id || x.starts_with(&format!("{id} ")) || x.starts_with(&format!("{id}.")));
            if taken {
                n += 1;
                continue;
            }
            let mut written = None;
            if let Some((name, content)) = note {
                let rel = if dir.rel.is_empty() { format!("{id} {name}.md") } else { format!("{}/{id} {name}.md", dir.rel) };
                let res = match paths::resolve_new(vault, &rel, false) {
                    Ok(r) => r,
                    Err(Error::AlreadyExists(_)) => {
                        n += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                let text = content.replace("{{id}}", &id);
                match fsio::create_exclusive(&res.full, text.as_bytes()) {
                    Ok(()) => {}
                    Err(Error::AlreadyExists(_)) => {
                        n += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
                let after = record_file(&tx, &self.atlas, &self.vaults, vault, &res, text.as_bytes(), true)?;
                let who = if who.discussion == THIS_NOTE {
                    &Actor { discussion: format!("{}:{}", vault.name, res.rel), ..who.clone() }
                } else {
                    who
                };
                audit(&tx, &self.audit_log, tool, who, &vault.name, &res.rel, None, Some(&after))?;
                written = Some(WriteResult { vault: vault.name.clone(), path: res.rel, sha256_before: None, sha256: after, changed: true });
            } else {
                let at = if dir.rel.is_empty() { id.clone() } else { format!("{}/{id}", dir.rel) };
                audit(&tx, &self.audit_log, tool, who, &vault.name, &at, None, None)?;
            }
            tx.execute("INSERT INTO ids(seq, last) VALUES(?1, ?2) ON CONFLICT(seq) DO UPDATE SET last = excluded.last", params![seq, n])?;
            tx.commit()?;
            return Ok((id, written));
        }
        Err(Error::InvalidArgument("1,000 consecutive ids are taken".into()))
    }

    /// Records a generated index in the `indices` table.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn record_index(&mut self, vault: &str, note: &str, name: &str, spec: &str, hash: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO indices(vault, note, name, spec, last_hash, updated) VALUES(?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(vault, note, name) DO UPDATE SET spec = excluded.spec, last_hash = excluded.last_hash, updated = excluded.updated",
            params![vault, note, name, spec, hash],
        )?;
        Ok(())
    }

    /// Notes whose indexed text contains an index start marker.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn notes_with_indices(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT n.vault, n.path FROM notes n JOIN fts f ON f.rowid = n.id WHERE instr(f.body, '<!-- vault-mcp:index ') > 0 ORDER BY n.vault, n.path",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let row = row?;
            if self.vaults.get(&row.0).is_ok() {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Reads the audit log, newest first.
    ///
    /// # Arguments
    ///
    /// * `filter` - vault, path, client and time-range filters.
    /// * `limit` - maximum rows returned.
    ///
    /// # Returns
    ///
    /// The matching rows (at most `limit`) and the total number that match.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] on SQLite failure.
    pub fn write_log(&self, filter: &WriteFilter, limit: usize) -> Result<(Vec<WriteEntry>, usize)> {
        const WHERE: &str = "(?1 IS NULL OR vault = ?1) AND (?2 IS NULL OR path = ?2) AND (?3 IS NULL OR client = ?3) \
             AND (?4 IS NULL OR time >= ?4) AND (?5 IS NULL OR time < ?5) AND (?7 IS NULL OR discussion = ?7)";
        let f = filter;
        let args = params![f.vault, f.path, f.client, f.since, f.until, Option::<i64>::None, f.discussion];
        let total: i64 = self.conn.query_row(&format!("SELECT COUNT(*) FROM writes WHERE {WHERE}"), args, |r| r.get(0))?;
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT time, tool, vault, path, client, reason, sha_before, sha_after, discussion FROM writes WHERE {WHERE} ORDER BY id DESC LIMIT ?6"
        ))?;
        let rows = st.query_map(params![f.vault, f.path, f.client, f.since, f.until, limit as i64, f.discussion], |r| {
            Ok(WriteEntry {
                time: r.get(0)?,
                tool: r.get(1)?,
                vault: r.get(2)?,
                path: r.get(3)?,
                client: r.get(4)?,
                reason: r.get(5)?,
                sha_before: r.get(6)?,
                sha_after: r.get(7)?,
                discussion: r.get(8)?,
            })
        })?;
        Ok((rows.collect::<std::result::Result<_, _>>()?, total as usize))
    }

    /// Direct access for tests and diagnostics.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// Brings the durable tables (which are never dropped) up to date in place.
/// Each step checks before it alters, so it is safe on every open.
fn migrate_durable(tx: &Transaction<'_>) -> Result<()> {
    let cols: Vec<String> = {
        let mut st = tx.prepare("SELECT name FROM pragma_table_info('writes')")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    if !cols.iter().any(|c| c == "reason") {
        tx.execute_batch("ALTER TABLE writes ADD COLUMN reason TEXT NOT NULL DEFAULT ''")?;
    }
    if !cols.iter().any(|c| c == "discussion") {
        tx.execute_batch("ALTER TABLE writes ADD COLUMN discussion TEXT NOT NULL DEFAULT ''")?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS writes_note ON writes(vault, path); CREATE INDEX IF NOT EXISTS writes_time ON writes(time); CREATE INDEX IF NOT EXISTS writes_discussion ON writes(discussion);",
    )?;
    Ok(())
}

fn note_id(conn: &Connection, vault: &str, path: &str) -> Result<Option<i64>> {
    Ok(conn.prepare_cached("SELECT id FROM notes WHERE vault=?1 AND path=?2")?.query_row(params![vault, path], |r| r.get(0)).optional()?)
}

fn delete_rows(tx: &Transaction<'_>, id: i64) -> Result<()> {
    tx.execute("DELETE FROM links WHERE note_id=?1", [id])?;
    tx.execute("DELETE FROM tags WHERE note_id=?1", [id])?;
    tx.execute("DELETE FROM fts WHERE rowid=?1", [id])?;
    tx.execute("DELETE FROM notes WHERE id=?1", [id])?;
    Ok(())
}

fn upsert(tx: &Transaction<'_>, atlas: &Atlas, vault: &str, path: &str, p: &Parsed) -> Result<i64> {
    let id: i64 = tx.query_row(
        "INSERT INTO notes(vault, path, mtime_ns, size, sha256, title, frontmatter) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(vault, path) DO UPDATE SET mtime_ns = excluded.mtime_ns, size = excluded.size, sha256 = excluded.sha256,
         title = excluded.title, frontmatter = excluded.frontmatter RETURNING id",
        params![vault, path, p.mtime, p.size, p.sha, p.title, p.frontmatter],
        |r| r.get(0),
    )?;
    tx.execute("DELETE FROM links WHERE note_id=?1", [id])?;
    tx.execute("DELETE FROM tags WHERE note_id=?1", [id])?;
    tx.execute("DELETE FROM fts WHERE rowid=?1", [id])?;
    tx.execute("INSERT INTO fts(rowid, title, body) VALUES(?1, ?2, ?3)", params![id, p.title, p.body])?;
    {
        let mut st = tx.prepare_cached("INSERT INTO links(note_id, line, kind, raw, target, heading) VALUES(?1, ?2, ?3, ?4, ?5, ?6)")?;
        for l in &p.links {
            st.execute(params![id, l.line as i64, l.kind.as_str(), l.raw, l.target, l.heading])?;
        }
        let mut st = tx.prepare_cached("INSERT INTO tags(note_id, tag) VALUES(?1, ?2)")?;
        for t in &p.tags {
            st.execute(params![id, t])?;
        }
    }
    let dr = DocRules { source: vault, kind: "vault", chunk: &atlas::VAULT_CHUNK, id_keys: &atlas.setup.vault_id_keys, names_kind: None };
    atlas::index_doc(tx, &atlas.setup.rules, &atlas.names, &dr, path, &p.body, p.mtime, p.size, &p.sha)?;
    Ok(id)
}

fn key_ref(v: &str, p: &str) -> (String, String) {
    (v.to_string(), p.to_string())
}

/// Resolves links: all of them, or only those of the given notes.
fn resolve(tx: &Transaction<'_>, vaults: &Vaults, only: Option<&[i64]>) -> Result<()> {
    let notes: Vec<(String, String)> = {
        let mut st = tx.prepare_cached("SELECT vault, path FROM notes")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let resolver = Resolver::new(vaults, notes);
    let mut rows: Vec<(i64, String, String, String, String, Option<String>)> = Vec::new();
    {
        let mut st = tx.prepare_cached(
            "SELECT l.rowid, n.vault, n.path, l.kind, l.target, l.heading FROM links l JOIN notes n ON n.id = l.note_id WHERE ?1 IS NULL OR l.note_id = ?1",
        )?;
        let ids: Vec<Option<i64>> = match only {
            None => vec![None],
            Some(ids) => ids.iter().map(|i| Some(*i)).collect(),
        };
        for id in ids {
            let r = st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
            for row in r {
                rows.push(row?);
            }
        }
    }
    let mut up = tx.prepare_cached("UPDATE links SET to_vault=?2, to_path=?3, attachment=?4, heading=?5 WHERE rowid=?1")?;
    for (rowid, vault, path, kind, target, heading) in rows {
        let link = RawLink { kind: kind_from(&kind), raw: String::new(), target, heading, line: 0 };
        let (res, heading) = resolver.resolve(&vault, &path, &link);
        let (tv, tp, att) = match res {
            Resolution::Note(v, p) => (Some(v), Some(p), 0),
            Resolution::Attachment => (None, None, 1),
            Resolution::Missing => (None, None, 0),
        };
        up.execute(params![rowid, tv, tp, att, heading])?;
    }
    Ok(())
}

/// Updates the rows for a file this process just wrote, and returns its hash.
fn record_file(
    tx: &Transaction<'_>,
    atlas: &Atlas,
    vaults: &Vaults,
    vault: &Vault,
    res: &Resolved,
    bytes: &[u8],
    is_new: bool,
) -> Result<String> {
    let mtime = fs::metadata(&res.full).map(|m| paths::mtime_ns(&m)).unwrap_or(0);
    let p = parse(&res.rel, bytes, mtime);
    let sha = p.sha.clone();
    let id = upsert(tx, atlas, &vault.name, &res.rel, &p)?;
    if is_new {
        resolve(tx, vaults, None)?;
    } else {
        resolve(tx, vaults, Some(&[id]))?;
    }
    Ok(sha)
}

/// Records one write: a row in `writes`, and the same record appended as
/// one JSON line to the audit log file, fsynced while the caller still holds
/// the write lock, so lines from several processes never interleave. The
/// file is only ever opened for append; nothing truncates or rewrites it.
#[allow(clippy::too_many_arguments)]
fn audit(
    tx: &Transaction<'_>,
    log: &Path,
    tool: &str,
    who: &Actor,
    vault: &str,
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<()> {
    let pid = std::process::id();
    let time: String = tx.query_row(
        "INSERT INTO writes(time, tool, vault, path, sha_before, sha_after, client, pid, reason, discussion) VALUES(strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING time",
        params![tool, vault, path, before, after, who.client, pid, who.reason, who.discussion],
        |r| r.get(0),
    )?;
    let mut line = crate::json!({
        "time": time, "tool": tool, "vault": vault, "path": path,
        "sha_before": before, "sha_after": after, "client": who.client, "pid": pid, "reason": who.reason, "discussion": who.discussion
    })
    .to_string();
    line.push('\n');
    let mut f = fs::OpenOptions::new().append(true).create(true).open(log)?;
    f.write_all(line.as_bytes())?;
    f.sync_data()?;
    Ok(())
}
