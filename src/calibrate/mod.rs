//! Calibration: what agents actually spent on rule and handoff lookups
//! before atlas, from real Claude Code transcripts.
//!
//! The token ledger's counterfactual (`atlas::tokens`) is an upper bound:
//! the bytes a literal grep prints plus every file window a reader would
//! open. Calibration measures the other side: the bytes of the tool results
//! agents read when they looked something up in the files atlas now serves,
//! in transcripts dated before atlas. Every number comes from a fixed rule
//! below; nothing is estimated. The result is saved to `baseline_log`, and
//! `--token-report` heads its output with the measured ratio, the upper
//! bound beside it labelled as such.
//!
//! # When it runs
//!
//! In the background when the server starts ([`spawn`]), on its own thread
//! and connection, never on a hook's path: when the store has no baseline,
//! or the latest was measured with older lookup rules ([`RULES_VERSION`];
//! a baseline from newer rules, saved by a newer atlas on the same store,
//! stands) or, under the same rules, other configured targets
//! ([`Plan::fingerprint`]), or has no rules version (saved before they were
//! versioned). One run at a time across
//! processes (a lock file beside the store, taken without waiting). Off with
//! `calibrate.auto: false` or `ATLAS_CALIBRATE=off`. `atlas --calibrate`
//! runs it by hand.
//!
//! # Method (all rules are mechanical)
//!
//! 1. Transcripts: the `*.jsonl` files (main sessions and `subagents/`) in
//!    Claude Code's projects folder (`CLAUDE_CONFIG_DIR`, else `~/.claude`),
//!    read only. With projects configured, a transcript counts when its
//!    first working folder resolves to the active project through a
//!    configured root, repository or project file (the folders whose names
//!    encode those roots are the only ones opened); without projects, every
//!    transcript counts. A transcript whose working folder, or the Claude
//!    Code folder it is filed under, matches a skip (default
//!    [`DEFAULT_SKIPS`]) does not count.
//! 2. Cutoff: only tool calls timestamped before it count, and a transcript
//!    stops counting at its first call to an atlas tool (`mcp__atlas__*`,
//!    `mcp__plugin_atlas_*`) or to its ancestor, the vault server
//!    (`mcp__vault__*`), which served the same vaults. The first run sets it
//!    to the earlier of the store's first atlas answer (the token ledger) and
//!    the first atlas call in those transcripts; with neither, the time of
//!    that run. Later runs keep it.
//! 3. Lookup targets: the configuration's vault and source roots: a path
//!    below a vault root, a path below a source root that one of its
//!    `include` globs matches, or a file whose name matches the handoff
//!    pattern (default: a Markdown file with "handoff" in its name).
//! 4. Lookup calls:
//!    * search: `Grep` whose `path` is a target or below a target root;
//!      `Bash` or `PowerShell` whose command runs grep, rg, findstr or
//!      Select-String and reads only targets: one of its words (shell-split)
//!      is a target path, absolute or relative to the session folder, or has
//!      the file name an include names exactly, or a handoff file name, and
//!      no word names a file (by its extension) that is not a target. A
//!      compound command that also reads other files is not a lookup.
//!    * read: `Read` of a target file; `Bash` or `PowerShell` whose command
//!      runs cat, sed -n, head, tail, awk, less, type or Get-Content and
//!      reads only targets, by the same rule.
//!
//!    Commands that also write (`sed -i`, a `>` redirect to a file, `git`,
//!    `python`, `py`, `Out-File`, `Set-Content`) are not lookups.
//! 5. Copies: a call whose `tool_use` id an earlier transcript (in path
//!    order) already holds is a copy (an eval replay, a resumed or forked
//!    session): it counts nothing and closes the open episode.
//! 6. Episodes: a search call opens a new episode; a read call joins the
//!    open episode when the previous tool call in the same transcript was a
//!    lookup call, else it opens one. An episode is one question answered.
//! 7. Bytes: the UTF-8 bytes of each lookup call's tool result as the agent
//!    received it (the text parts of the result content).
//! 8. The atlas side, read from the token ledger when reported: answers of
//!    the comparable read tools (`atlas::baseline::COMPARABLE`), their
//!    returned bytes, and their counterfactual (grep + read).
//! 9. Enough history: at least [`MIN_EPISODES`] episodes from at least
//!    [`MIN_TRANSCRIPTS`] transcripts, else the report gives the upper bound
//!    only.
//!
//! Ratios per answer: the **measured** ratio is the median bytes of a
//! pre-atlas episode over the median bytes atlas returned per answer (the
//! means are printed beside it); the **upper bound** is the sum of the
//! counterfactual over the sum returned, over answers that have one.
//!
//! # Surviving transcript cleanup
//!
//! Claude Code deletes transcripts after a while, and pre-atlas history never
//! grows, so the first run stores what the rules read of every pre-cutoff
//! call ([`rules::Call`]: tool, kept input keys, folder, result size; never
//! the result's text) in `calibrate_calls`, and every run measures from that
//! table: a rerun after a rules change reclassifies the same calls. A
//! transcript is read once per rules version: one stored under older rules
//! is read again while it is on disk (newer rules may read another input
//! key), and one Claude Code has deleted keeps what was stored, reclassified
//! from its kept keys. A rerun under the same rules and targets never
//! replaces a stamped baseline with one of fewer episodes.
//!
//! The rules version covers [`rules`] and `glob.rs` (a test hashes their
//! code). Changes in `json.rs`, `regex` or the transcript reading here can
//! change a result too and need a version by hand.

pub mod rules;

use std::collections::HashSet;
use std::io::BufRead as _;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::baseline::Baseline;
use crate::config::Config;
use crate::json::Value;
pub use rules::{call_line, classify, episodes, is_atlas_tool, kept_input, result_bytes, shell_words, Call, Episode, Kind, Targets, DEFAULT_HANDOFF};

/// The lookup rules' version, stamped on every baseline: a store whose
/// baseline has another recalibrates at its next start.
pub const RULES_VERSION: u32 = 3;

/// Every rules version with the hash of the rules' code (see the module
/// docs). A changed hash needs a new last row and [`RULES_VERSION`] set to
/// it; the test says which.
pub const RULES_HISTORY: &[(u32, &str)] = &[(1, "d6a3e8272a01199b02db2b05c648f1dbb8d6be36d8a95b1a584974bee4eafe93"), (2, "1331cc237a545c205cf9bc0618a7c3861070929460f5377aa5d359cabf6c9591"), (3, "a62b06f78ea5068f3f18faebaad8c8bffb503e2223c51445741f98d003fffaee")];

/// Fewer episodes than this give no measured ratio: below about 30 the
/// median of a long-tailed size distribution moves by more than a factor
/// of 2 when a handful of episodes are added or removed.
pub const MIN_EPISODES: usize = 30;

/// Fewer transcripts with an episode than this give no measured ratio: a
/// few sessions measure those sessions' habits, not the project's.
pub const MIN_TRANSCRIPTS: usize = 5;

/// The default skips: words that mark a temporary, scratch, eval or
/// replay folder.
pub const DEFAULT_SKIPS: &[&str] = &["tmp", "temp", "scratch", "eval", "evals", "replay", "replays"];

/// The environment variable that turns the background run off (`off`).
pub const OFF_VAR: &str = "ATLAS_CALIBRATE";

/// The stored cutoff, the transcripts read, and their calls.
pub const TABLES: &str = "
CREATE TABLE IF NOT EXISTS calibrate_state(id INTEGER PRIMARY KEY CHECK(id = 1), cutoff TEXT NOT NULL, source TEXT NOT NULL, time TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS calibrate_files(file TEXT PRIMARY KEY, cwd TEXT NOT NULL, rules INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS calibrate_calls(file TEXT NOT NULL, seq INTEGER NOT NULL, id TEXT NOT NULL, time TEXT NOT NULL, cwd TEXT NOT NULL, name TEXT NOT NULL, input TEXT, bytes INTEGER NOT NULL, PRIMARY KEY(file, seq));
";

/// What one transcript holds before a cutoff.
#[derive(Debug, Default)]
pub struct Scan {
    /// The first working folder named in it.
    pub cwd: String,
    /// Its calls before the cutoff and its first atlas call.
    pub calls: Vec<Call>,
    /// When its first atlas call was made, when that came before the cutoff.
    pub atlas_at: Option<String>,
    /// The `accept` test refused its working folder; nothing else was read.
    pub rejected: bool,
}

/// Reads one transcript's calls before `before` (see the module docs),
/// line by line: a transcript whose first timestamp is at or after
/// `before` is not read further, and reading stops once the calls have
/// stopped and every kept call has its result. With `accept`, the first
/// working folder named is tested first. A transcript that cannot be read
/// whole as UTF-8 holds nothing.
pub fn read_calls(path: &Path, before: &str, accept: Option<&dyn Fn(&str) -> bool>) -> Scan {
    let Ok(f) = std::fs::File::open(path) else { return Scan::default() };
    let mut scan = Scan::default();
    let mut results: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut waiting: HashSet<String> = HashSet::new();
    let (mut stopped, mut dated, mut placed) = (false, false, false);
    for line in std::io::BufReader::new(f).lines() {
        let Ok(line) = line else { return Scan::default() };
        let has_cwd = !placed && line.contains("\"cwd\"");
        let has_ts = !dated && line.contains("\"timestamp\"");
        let result = line.contains("\"tool_result\"");
        let tool = result || line.contains("\"tool_use\"");
        if !tool && !has_cwd && !has_ts {
            continue;
        }
        if stopped {
            if waiting.is_empty() {
                break;
            }
            if !result {
                continue;
            }
        }
        let Ok(v) = crate::json::parse(&line) else { continue };
        if has_cwd {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                placed = true;
                scan.cwd = c.to_string();
                if accept.is_some_and(|a| !a(c)) {
                    scan.rejected = true;
                    return scan;
                }
            }
        }
        if has_ts {
            if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
                dated = true;
                if ts >= before {
                    return Scan { cwd: scan.cwd, ..Scan::default() };
                }
            }
        }
        if !tool {
            continue;
        }
        let ts = v.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("");
        let Some(Value::Array(items)) = v.pointer("/message/content") else { continue };
        for x in items {
            let s = |k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            match x.get("type").and_then(Value::as_str) {
                Some("tool_use") if !stopped => {
                    let name = s("name");
                    if ts >= before || is_atlas_tool(&name) {
                        stopped = true;
                        if ts < before {
                            scan.atlas_at = Some(ts.to_string());
                        }
                        continue;
                    }
                    let id = s("id");
                    let input = x.get("input").map(|i| kept_input(&name, i)).unwrap_or(Value::Null);
                    if !rules::kept_keys(&name).is_empty() {
                        waiting.insert(id.clone());
                    }
                    scan.calls.push(Call { id, time: ts.to_string(), cwd: cwd.to_string(), name, input, bytes: 0 });
                }
                Some("tool_result") => {
                    let id = s("tool_use_id");
                    waiting.remove(&id);
                    results.insert(id, result_bytes(x.get("content")));
                }
                _ => {}
            }
        }
    }
    for c in &mut scan.calls {
        if !rules::kept_keys(&c.name).is_empty() {
            c.bytes = results.get(&c.id).copied().unwrap_or(0);
        }
    }
    scan
}

/// Every `*.jsonl` below `dir` (main sessions and `subagents/`), sorted.
pub fn transcripts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() && e.path().extension().is_some_and(|x| x == "jsonl") => out.push(e.path()),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

fn audit_line(w: &mut dyn std::io::Write, file: &str, e: &Episode) {
    let _ = writeln!(w, "{file}\t{}\t{}\t{}", e.time, e.bytes, e.calls.join(" | "));
}

/// Measures every transcript below `dir` whose path holds none of `skip`,
/// with explicit inputs (`cargo xtask calibrate-baseline`): calls before
/// `before`, copies dropped when `dedup` (rule 5), one line per episode to
/// `audit` (transcript, time, bytes, then each call) for a hand check, and
/// progress on stderr every 50 files. Nothing is stored.
pub fn measure_audited(dir: &Path, before: &str, t: &Targets, skip: &[String], dedup: bool, mut audit: Option<&mut dyn std::io::Write>) -> Baseline {
    let mut files = transcripts(dir);
    files.retain(|p| !skip.iter().any(|s| p.to_string_lossy().contains(s.as_str())));
    let mut all: Vec<f64> = Vec::new();
    let mut used = 0;
    let mut seen = HashSet::new();
    for (i, p) in files.iter().enumerate() {
        let calls = read_calls(p, before, None).calls;
        let e = episodes(&calls, t, dedup.then_some(&mut seen));
        used += usize::from(!e.is_empty());
        if let Some(w) = audit.as_mut() {
            let name = p.display().to_string();
            for x in &e {
                audit_line(&mut **w, &name, x);
            }
        }
        all.extend(e.into_iter().map(|x| x.bytes as f64));
        if (i + 1) % 50 == 0 || i + 1 == files.len() {
            eprintln!("[{}/{}] transcripts scanned, {} lookup episodes", i + 1, files.len(), all.len());
        }
    }
    rules::summarize(before, files.len(), used, all)
}

/// A path as the skips compare it: `/`-separated, lowercased, no trailing
/// `/`.
fn skip_key(p: &str) -> String {
    p.replace('\\', "/").to_lowercase().trim_end_matches('/').to_string()
}

fn words(s: &str) -> Vec<&str> {
    s.split(|ch: char| !ch.is_alphanumeric()).filter(|w| !w.is_empty()).collect()
}

/// The skip a working folder matches, if any (see
/// [`crate::config::CalibrateSpec::skip`]): a skip with `/` or `:` is a
/// folder, matched against the whole path; a word matches a whole word of
/// the part of `cwd` below the deepest of `roots` (keys as [`skip_key`]
/// makes them) that holds it, or of all of `cwd` when none does. Words
/// above a project root never skip: a project kept under a `temp` or `eval`
/// folder still has its history.
pub fn skipped(cwd: &str, skips: &[String], roots: &[String]) -> Option<String> {
    let c = skip_key(cwd);
    let below = roots.iter().filter(|r| crate::project::key_within(&c, r)).max_by_key(|r| r.len()).map_or(c.as_str(), |r| &c[r.len()..]);
    let w = words(below);
    skips
        .iter()
        .find(|s| {
            let k = skip_key(s);
            if s.contains(['/', '\\', ':']) {
                crate::project::key_within(&c, &k)
            } else {
                w.contains(&k.as_str())
            }
        })
        .cloned()
}

/// The skip a Claude Code project folder's name (its session folder,
/// encoded) matches, if any: a word as in [`skipped`], in the part of the
/// name after the longest of `prefixes` (root encodings, lowercased) it
/// starts with; a folder when the name is that folder's encoding or one
/// below it. A replay's transcript keeps the working folder of the session
/// it copies, so the folder it is filed under is the only sign of where it
/// ran.
pub fn skipped_folder(name: &str, skips: &[String], prefixes: &[String]) -> Option<String> {
    let n = name.to_lowercase();
    let below = prefixes.iter().filter(|p| under_prefix(&n, p)).max_by_key(|p| p.len()).map_or(n.as_str(), |p| &n[p.len()..]);
    let w = words(below);
    skips
        .iter()
        .find(|s| {
            if s.contains(['/', '\\', ':']) {
                let e = crate::transcripts::project_of_cwd(Path::new(s.trim_end_matches(['/', '\\']))).to_lowercase();
                under_prefix(&n, &e)
            } else {
                w.contains(&s.to_lowercase().as_str())
            }
        })
        .cloned()
}

/// Whether a folder name (lowercased) is the encoding `p` or one below it.
fn under_prefix(n: &str, p: &str) -> bool {
    n == p || n.strip_prefix(p).is_some_and(|r| r.starts_with('-'))
}

/// The name of the Claude Code project folder a transcript is filed under:
/// the component after the last one named `projects`.
pub fn folder_of(file: &Path) -> Option<String> {
    let parts: Vec<String> = file.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let i = parts.iter().rposition(|p| p == "projects")?;
    parts.get(i + 1).cloned()
}

/// What a calibration of one store reads and against what.
pub struct Plan {
    /// The store.
    pub db: PathBuf,
    /// Claude Code's projects folder, when there is a home to find it in.
    pub projects: Option<PathBuf>,
    /// The user configuration, which resolves a transcript's folder.
    user: Config,
    /// The active project (`None` without projects).
    project: Option<String>,
    /// Claude Code's folder names for the project's roots, lowercased.
    prefixes: Vec<String>,
    /// The project's roots as the skips compare them.
    roots: Vec<String>,
    /// The lookup targets.
    pub targets: Targets,
    /// The skips applied.
    pub skips: Vec<String>,
    /// A hash of the targets, skips, project and its roots: a baseline
    /// measured against others is stale.
    pub fingerprint: String,
    /// Whether the server runs it in the background.
    pub auto: bool,
}

impl Plan {
    /// The plan for the store `db` of the configuration `resolved`, which
    /// `user` resolved to the project `active`.
    ///
    /// # Errors
    ///
    /// A handoff pattern the regex engine refuses.
    pub fn new(user: &Config, resolved: &Config, active: Option<&crate::project::Active>, db: &Path) -> Result<Plan, String> {
        let spec = resolved.calibrate.clone().unwrap_or_default();
        let handoff = spec.handoff.clone().unwrap_or_else(|| DEFAULT_HANDOFF.to_string());
        let vaults: Vec<PathBuf> = resolved.vaults.iter().map(|v| v.path.clone()).collect();
        let sources: Vec<(PathBuf, Vec<String>)> = resolved.sources.iter().map(|s| (s.root.clone(), s.include.clone())).collect();
        let targets = Targets::new(&vaults, &sources, &handoff)?;
        let skips = spec.skip.clone().unwrap_or_else(|| DEFAULT_SKIPS.iter().map(|s| s.to_string()).collect());
        let project = active.map(|a| a.spec.name.clone());
        let roots = active.map(|a| crate::project::project_roots(&a.spec, &user.user.checkouts)).unwrap_or_default();
        let prefixes: Vec<String> = roots.iter().map(|r| crate::transcripts::project_of_cwd(r).to_lowercase()).collect();
        let mut fp = format!("project {}\nhandoff {handoff}\n{}", project.as_deref().unwrap_or(""), targets.describe());
        for r in &roots {
            fp.push_str(&format!("root {}\n", r.to_string_lossy().replace('\\', "/")));
        }
        for s in &skips {
            fp.push_str(&format!("skip {s}\n"));
        }
        Ok(Plan {
            db: db.to_path_buf(),
            projects: crate::chain::projects_dir(),
            user: user.clone(),
            project,
            prefixes,
            roots: roots.iter().map(|r| skip_key(&r.to_string_lossy())).collect(),
            targets,
            skips,
            fingerprint: crate::sha256::hex(fp.as_bytes())[..16].to_string(),
            auto: spec.auto.unwrap_or(true),
        })
    }

    /// The project folders in Claude Code's projects folder that can hold
    /// its transcripts: those whose names encode one of its roots or a
    /// folder below one (every folder without projects), sorted; and a line
    /// for each such folder a skip left out.
    pub fn folders(&self) -> (Vec<PathBuf>, Vec<String>) {
        let Some(dir) = &self.projects else { return (Vec::new(), Vec::new()) };
        let Ok(rd) = std::fs::read_dir(dir) else { return (Vec::new(), Vec::new()) };
        let (mut v, mut notes) = (Vec::new(), Vec::new());
        for e in rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if !(self.prefixes.is_empty() || self.prefixes.iter().any(|p| under_prefix(&n, p))) {
                continue;
            }
            match self.skip_folder(&n) {
                Some(s) => notes.push(format!("skipped folder {}: it matches the skip {s:?}", e.file_name().to_string_lossy())),
                None => v.push(e.path()),
            }
        }
        v.sort();
        notes.sort();
        (v, notes)
    }

    /// The skip a Claude Code project folder's name matches, if any.
    pub fn skip_folder(&self, name: &str) -> Option<String> {
        skipped_folder(name, &self.skips, &self.prefixes)
    }

    /// The skip a working folder matches, if any.
    pub fn skip_cwd(&self, cwd: &str) -> Option<String> {
        skipped(cwd, &self.skips, &self.roots)
    }

    /// Whether a transcript whose first working folder is `cwd` counts.
    pub fn belongs(&self, cwd: &str) -> bool {
        self.skip_cwd(cwd).is_none() && self.in_project(cwd)
    }

    /// Whether a working folder resolves to the project (any folder without
    /// projects), skips aside.
    fn in_project(&self, cwd: &str) -> bool {
        if cwd.is_empty() {
            return false;
        }
        match &self.project {
            None => true,
            Some(name) => matches!(
                crate::project::active(&self.user, Path::new(cwd), None),
                Ok(Some(a)) if a.spec.name == *name && matches!(a.why, crate::project::Why::Root | crate::project::Why::File)
            ),
        }
    }

    /// The file whose lock allows one run at a time.
    pub fn lock_target(&self) -> PathBuf {
        self.db.parent().unwrap_or(Path::new(".")).join("calibrate")
    }
}

/// Opens the store for a calibration, waiting on the server's writes.
///
/// # Errors
///
/// The store cannot be opened.
pub fn open(db: &Path) -> Result<Connection, String> {
    let conn = crate::store::open_raw(db).map_err(|e| format!("{}: {e}", db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(30)).map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Why the store needs a calibration, or `None` when its latest baseline
/// was measured with these rules against these targets.
///
/// # Errors
///
/// [`crate::Error::Db`] as text.
pub fn stale(conn: &Connection, plan: &Plan) -> Result<Option<String>, String> {
    let latest = crate::baseline::latest(conn).map_err(|e| e.to_string())?;
    Ok(match latest {
        None => Some("no baseline yet".into()),
        Some((_, b)) => match b.rules {
            None => Some("the stored baseline has no rules version".into()),
            Some(v) if v < RULES_VERSION => Some(format!("the lookup rules changed (v{v} to v{RULES_VERSION})")),
            // Measured by a newer atlas sharing this store: it stands, or
            // the two would recalibrate in turn at every start.
            Some(v) if v > RULES_VERSION => None,
            _ if b.fingerprint.as_deref() != Some(plan.fingerprint.as_str()) => Some("the configured targets changed".into()),
            _ => None,
        },
    })
}

/// What a run did.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The baseline measured.
    pub baseline: Baseline,
    /// The stamped baseline kept in its place: same rules and targets, more
    /// episodes.
    pub kept: Option<Baseline>,
    /// Project folders opened.
    pub folders: usize,
    /// Transcripts read this run.
    pub read: usize,
    /// Of them, transcripts stored (they had calls before the cutoff).
    pub stored: usize,
    /// A line for each project folder and transcript a skip left out, and
    /// which skip.
    pub skipped: Vec<String>,
}

fn db_err(e: rusqlite::Error) -> String {
    e.to_string()
}

/// Runs a calibration on `conn` (see the module docs): reads the
/// transcripts not stored yet, stores their calls, measures from every
/// stored call and saves the baseline. `progress` gets a line every 50
/// transcripts and one for each folder or transcript a skip left out;
/// `audit` one line per episode.
///
/// # Errors
///
/// A store error, as text.
pub fn run(plan: &Plan, conn: &Connection, progress: &mut dyn FnMut(&str), audit: Option<&mut dyn std::io::Write>) -> Result<Outcome, String> {
    // Every write here opens its transaction with the version check under
    // the lock (`store::guarded`, `store::begin_shared`), the tables too: a
    // store a newer atlas upgraded since `open` is not written.
    crate::store::guarded(conn, || {
        conn.execute_batch(TABLES)?;
        crate::baseline::ensure(conn)?;
        conn.execute_batch(crate::tokens::TOKEN_TABLE)?;
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    let stored: Option<(String, String)> = conn.query_row("SELECT cutoff, source FROM calibrate_state WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(db_err)?;
    let ledger: Option<String> = conn.query_row("SELECT min(time) FROM token_log", [], |r| r.get(0)).map_err(db_err)?;
    let now = crate::store::utc(std::time::SystemTime::now());
    let upper = stored.as_ref().map(|s| s.0.clone()).or_else(|| ledger.clone()).unwrap_or_else(|| now.clone());
    // A file stored under older rules kept the input keys those rules
    // read: read it again while it is still on disk (newer rules may read
    // another key); one that is gone keeps what was stored.
    let older: Vec<String> = {
        let mut st = conn.prepare("SELECT file FROM calibrate_files WHERE rules < ?1").map_err(db_err)?;
        let rows = st.query_map([RULES_VERSION], |r| r.get(0)).map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)?
    };
    let again: Vec<&String> = older.iter().filter(|f| Path::new(f.as_str()).is_file()).collect();
    if !again.is_empty() {
        let tx = crate::store::begin_shared(conn).map_err(|e| e.to_string())?;
        for f in again {
            tx.execute("DELETE FROM calibrate_calls WHERE file = ?1", [f]).map_err(db_err)?;
            tx.execute("DELETE FROM calibrate_files WHERE file = ?1", [f]).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
    }
    let known: HashSet<String> = {
        let mut st = conn.prepare("SELECT file FROM calibrate_files").map_err(db_err)?;
        let rows = st.query_map([], |r| r.get(0)).map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)?
    };
    let (folders, mut skipped_notes) = plan.folders();
    let mut files: Vec<PathBuf> = folders.iter().flat_map(|f| transcripts(f)).collect();
    files.sort();
    files.retain(|p| !known.contains(p.to_string_lossy().as_ref()));
    let accept = |c: &str| plan.belongs(c);
    let mut fresh: Vec<(String, Scan)> = Vec::new();
    for (i, p) in files.iter().enumerate() {
        let s = read_calls(p, &upper, Some(&accept));
        let skip = plan.skip_cwd(&s.cwd).filter(|_| plan.in_project(&s.cwd));
        if let Some(k) = skip {
            skipped_notes.push(format!("skipped transcript {}: its working folder {} matches the skip {k:?}", p.display(), s.cwd));
        } else if !s.rejected && plan.belongs(&s.cwd) && (!s.calls.is_empty() || s.atlas_at.is_some()) {
            fresh.push((p.to_string_lossy().into_owned(), s));
        }
        if (i + 1) % 50 == 0 || i + 1 == files.len() {
            progress(&format!("[{}/{}] transcripts read", i + 1, files.len()));
        }
    }
    for n in &skipped_notes {
        progress(n);
    }
    let (cutoff, source) = match stored {
        Some(s) => s,
        None => {
            let first_call = fresh.iter().filter_map(|(_, s)| s.atlas_at.clone()).min();
            let (cutoff, source) = match (ledger, first_call) {
                (Some(l), Some(c)) if c < l => (c, "the first atlas call in the transcripts"),
                (Some(l), _) => (l, "the store's first atlas answer"),
                (None, Some(c)) => (c, "the first atlas call in the transcripts"),
                (None, None) => (now.clone(), "the first calibration (no atlas use before it)"),
            };
            crate::store::guarded(conn, || Ok(conn.execute("INSERT OR IGNORE INTO calibrate_state(id, cutoff, source, time) VALUES(1, ?1, ?2, ?3)", params![cutoff, source, now])?))
                .map_err(|e| e.to_string())?;
            // Another process may have stored one first: that one stands.
            conn.query_row("SELECT cutoff, source FROM calibrate_state WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?))).map_err(db_err)?
        }
    };
    // Read up to `upper`, which can be later than the cutoff that stands;
    // earlier, and what this run read is short: store none of it (the next
    // run reads those transcripts whole).
    if cutoff > upper {
        fresh.clear();
    }
    for (_, s) in &mut fresh {
        if let Some(i) = s.calls.iter().position(|c| c.time >= cutoff) {
            s.calls.truncate(i);
        }
    }
    fresh.retain(|(_, s)| !s.calls.is_empty());
    let stored_now = fresh.len();
    for chunk in fresh.chunks(20) {
        let tx = crate::store::begin_shared(conn).map_err(|e| e.to_string())?;
        for (file, s) in chunk {
            tx.execute("INSERT OR IGNORE INTO calibrate_files(file, cwd, rules) VALUES(?1, ?2, ?3)", params![file, s.cwd, RULES_VERSION]).map_err(db_err)?;
            for (seq, c) in s.calls.iter().enumerate() {
                let input = if c.input.is_null() { None } else { Some(c.input.to_string()) };
                tx.execute(
                    "INSERT OR IGNORE INTO calibrate_calls(file, seq, id, time, cwd, name, input, bytes) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![file, seq as i64, c.id, c.time, c.cwd, c.name, input, c.bytes as i64],
                )
                .map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
    }
    let mut b = measure_stored(plan, conn, &cutoff, audit)?;
    b.rules = Some(RULES_VERSION);
    b.fingerprint = Some(plan.fingerprint.clone());
    b.cutoff_source = Some(source);
    b.skips = Some(plan.skips.join("\n"));
    b.too_few = b.episodes < MIN_EPISODES || b.used < MIN_TRANSCRIPTS;
    let prev = crate::baseline::latest(conn).map_err(|e| e.to_string())?.map(|(_, p)| p);
    let kept = prev.filter(|p| p.rules == b.rules && p.fingerprint == b.fingerprint && b.episodes < p.episodes);
    if kept.is_none() {
        crate::baseline::save(conn, &b).map_err(|e| e.to_string())?;
    }
    Ok(Outcome { baseline: b, kept, folders: folders.len(), read: files.len(), stored: stored_now, skipped: skipped_notes })
}

/// Measures from the stored calls of the transcripts that count.
fn measure_stored(plan: &Plan, conn: &Connection, cutoff: &str, mut audit: Option<&mut dyn std::io::Write>) -> Result<Baseline, String> {
    let files: Vec<(String, String)> = {
        let mut st = conn.prepare("SELECT file, cwd FROM calibrate_files ORDER BY file").map_err(db_err)?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)?
    };
    let mut st = conn.prepare("SELECT id, time, cwd, name, input, bytes FROM calibrate_calls WHERE file = ?1 AND time < ?2 ORDER BY seq").map_err(db_err)?;
    let mut seen = HashSet::new();
    let (mut all, mut used, mut count) = (Vec::new(), 0, 0);
    let counts = |file: &str, cwd: &str| plan.belongs(cwd) && !folder_of(Path::new(file)).is_some_and(|f| plan.skip_folder(&f).is_some());
    for (file, _) in files.iter().filter(|(f, c)| counts(f, c)) {
        let calls: Vec<Call> = st
            .query_map(params![file, cutoff], |r| {
                let input: Option<String> = r.get(4)?;
                Ok(Call {
                    id: r.get(0)?,
                    time: r.get(1)?,
                    cwd: r.get(2)?,
                    name: r.get(3)?,
                    input: input.and_then(|t| crate::json::parse(&t).ok()).unwrap_or(Value::Null),
                    bytes: r.get::<_, i64>(5)? as usize,
                })
            })
            .map_err(db_err)?
            .collect::<Result<_, _>>()
            .map_err(db_err)?;
        count += 1;
        let e = episodes(&calls, &plan.targets, Some(&mut seen));
        used += usize::from(!e.is_empty());
        if let Some(w) = audit.as_mut() {
            for x in &e {
                audit_line(&mut **w, file, x);
            }
        }
        all.extend(e.into_iter().map(|x| x.bytes as f64));
    }
    Ok(rules::summarize(cutoff, count, used, all))
}

/// Calibrates in the background when the store needs it (see the module
/// docs): its own thread and connection, the lock taken without waiting
/// (another process running one means this one does nothing), one stderr
/// line when done.
pub fn spawn(plan: Plan) {
    if !plan.auto || std::env::var(OFF_VAR).is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        return;
    }
    let spawned = std::thread::Builder::new().name("atlas-calibrate".into()).spawn(move || match background(&plan) {
        Ok(Some(line)) => eprintln!("atlas calibrate: {line}"),
        Ok(None) => {}
        Err(e) => eprintln!("atlas calibrate: {e}"),
    });
    if let Err(e) = spawned {
        eprintln!("atlas calibrate: {e}");
    }
}

/// One background run: `None` when nothing was needed or another process
/// holds the lock.
///
/// # Errors
///
/// As [`run`].
pub fn background(plan: &Plan) -> Result<Option<String>, String> {
    let conn = open(&plan.db)?;
    if stale(&conn, plan)?.is_none() {
        return Ok(None);
    }
    let Ok(_lock) = crate::fsio::FileLock::acquire_within(&plan.lock_target(), std::time::Duration::ZERO) else { return Ok(None) };
    // A run in another process may have finished before the lock was free.
    let Some(why) = stale(&conn, plan)? else { return Ok(None) };
    let o = run(plan, &conn, &mut |_| {}, None)?;
    Ok(Some(format!("{why}: {}", summary(&o))))
}

/// One line on a run's result.
pub fn summary(o: &Outcome) -> String {
    let b = &o.baseline;
    let what = if b.too_few {
        format!("too little pre-atlas history for a measured ratio ({} episodes in {} transcripts; needs {MIN_EPISODES} in {MIN_TRANSCRIPTS})", b.episodes, b.used)
    } else {
        format!("{} pre-atlas lookup episodes in {} of {} transcripts, median {:.0} bytes", b.episodes, b.used, b.transcripts, b.median)
    };
    let kept = match &o.kept {
        Some(k) => format!("; kept the stored baseline of {} episodes (same rules and targets)", k.episodes),
        None => String::new(),
    };
    let skipped = match o.skipped.len() {
        0 => String::new(),
        n => format!("; {n} folders or transcripts skipped by the skips (atlas --calibrate names them)"),
    };
    format!("{what}, before {} ({}), rules v{RULES_VERSION}; {} folders, {} transcripts read, {} stored{kept}{skipped}", b.before, b.cutoff_source.as_deref().unwrap_or(""), o.folders, o.read, o.stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> Targets {
        Targets::new(
            &[PathBuf::from("V:/notes vault")],
            &[(PathBuf::from("R:/repo/.claude"), vec!["RULES.md".into()]), (PathBuf::from("R:/repo/docs"), vec!["*.md".into()])],
            DEFAULT_HANDOFF,
        )
        .unwrap()
    }

    /// The code of a source file: the part before its tests, without
    /// comment lines and blank lines, each line trimmed.
    fn code_of(src: &str) -> String {
        let body = src.split("\n#[cfg(test)]").next().unwrap_or(src);
        body.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("//")).collect::<Vec<_>>().join("\n")
    }

    fn rules_hash() -> String {
        crate::sha256::hex(format!("{}\n{}", code_of(include_str!("rules.rs")), code_of(include_str!("../glob.rs"))).as_bytes())
    }

    #[test]
    fn the_rules_version_follows_the_rules() {
        let h = rules_hash();
        let (v, last) = RULES_HISTORY.last().unwrap();
        assert!(RULES_HISTORY.windows(2).all(|w| w[0].0 < w[1].0), "versions only grow");
        assert_eq!(*v, RULES_VERSION, "RULES_VERSION is the last row of RULES_HISTORY");
        assert_eq!(h, *last, "the lookup rules changed: append ({}, {h:?}) to RULES_HISTORY and set RULES_VERSION to {}", v + 1, v + 1);
    }

    #[test]
    fn comments_and_blank_lines_do_not_change_the_hash_but_code_does() {
        let a = "fn f() {\n    let x = 1;\n}\n#[cfg(test)]\nmod t {}\n";
        let b = "// a note\nfn f() {\n\n    // why\n    let x = 1;\n}\n#[cfg(test)]\nmod t { fn other() {} }\n";
        assert_eq!(code_of(a), code_of(b));
        assert_ne!(code_of(a), code_of(&a.replace("x = 1", "x = 2")));
        let src = include_str!("rules.rs");
        assert_ne!(code_of(src), code_of(&src.replacen("\"tee\"", "\"tea\"", 1)), "a word of writes() is in the hash");
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atlas-calibrate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn use_(id: &str, name: &str, input: Value, ts: &str) -> String {
        crate::json!({"type": "assistant", "timestamp": ts, "cwd": "R:/repo", "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}}).to_string()
    }

    fn res(id: &str, n: usize) -> String {
        crate::json!({"type": "user", "timestamp": "2026-01-01T00:00:00Z", "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "x".repeat(n)}]}}).to_string()
    }

    fn scan(p: &Path, before: &str, t: &Targets) -> Vec<usize> {
        episodes(&read_calls(p, before, None).calls, t, None).into_iter().map(|e| e.bytes).collect()
    }

    #[test]
    fn episodes_group_reads_after_a_search() {
        let dir = temp("episodes");
        let p = dir.join("t.jsonl");
        let lines = [
            use_("1", "Grep", crate::json!({"pattern": "a", "path": "R:/repo/docs", "glob": "*.md"}), "2026-01-01T00:00:00Z"),
            res("1", 100),
            use_("2", "Read", crate::json!({"file_path": "R:/repo/docs/a.md"}), "2026-01-01T00:00:01Z"),
            res("2", 1000),
            use_("3", "Bash", crate::json!({"command": "cargo build"}), "2026-01-01T00:00:02Z"),
            res("3", 5),
            use_("4", "Read", crate::json!({"file_path": "R:/repo/.claude/RULES.md"}), "2026-01-01T00:00:03Z"),
            res("4", 40),
            use_("4b", "mcp__vault__read_note", crate::json!({}), "2026-01-01T00:00:03Z"),
            use_("5", "mcp__atlas__recall", crate::json!({}), "2026-01-01T00:00:04Z"),
            use_("6", "Read", crate::json!({"file_path": "R:/repo/.claude/RULES.md"}), "2026-01-01T00:00:05Z"),
            res("6", 7),
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        assert_eq!(scan(&p, "2027", &targets()), vec![1100, 40]);
        assert_eq!(scan(&p, "2026-01-01T00:00:01Z", &targets()), vec![100]);
        let s = read_calls(&p, "2027", None);
        assert_eq!(s.atlas_at.as_deref(), Some("2026-01-01T00:00:03Z"), "the vault server is atlas's ancestor");
        let b = measure_audited(&dir, "2027", &targets(), &[], true, None);
        assert_eq!((b.episodes, b.median, b.total), (2, 570.0, 1140));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_result_after_the_stop_still_counts_and_reading_ends_once_all_are_in() {
        let dir = temp("late");
        let p = dir.join("t.jsonl");
        let lines = [
            use_("1", "Read", crate::json!({"file_path": "R:/repo/docs/a.md"}), "2026-01-01T00:00:00Z"),
            use_("2", "mcp__atlas__lookup", crate::json!({}), "2026-01-01T00:00:01Z"),
            res("1", 30),
            // Past the last awaited result: a line that would not parse is
            // never read, and a later result for an earlier id is ignored.
            "{not json \"tool_result\"".to_string(),
            res("1", 999),
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        assert_eq!(scan(&p, "2027", &targets()), vec![30]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_transcript_that_starts_after_the_cutoff_is_not_read_further() {
        let dir = temp("after");
        let p = dir.join("t.jsonl");
        let lines = [
            crate::json!({"type": "user", "timestamp": "2026-02-01T00:00:00Z", "cwd": "R:/repo"}).to_string(),
            use_("1", "Read", crate::json!({"file_path": "R:/repo/docs/a.md"}), "2026-01-01T00:00:00Z"),
            res("1", 30),
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        assert!(read_calls(&p, "2026-01-15", None).calls.is_empty());
        assert_eq!(read_calls(&p, "2026-03-01", None).calls.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copies_of_a_call_count_once() {
        let dir = temp("copies");
        let one = [use_("1", "Read", crate::json!({"file_path": "R:/repo/docs/a.md"}), "2026-01-01T00:00:00Z"), res("1", 50)];
        std::fs::write(dir.join("a.jsonl"), one.join("\n")).unwrap();
        let replay = [
            use_("1", "Read", crate::json!({"file_path": "R:/repo/docs/a.md"}), "2026-01-01T00:00:00Z"),
            res("1", 50),
            use_("2", "Read", crate::json!({"file_path": "R:/repo/docs/b.md"}), "2026-01-01T00:00:02Z"),
            res("2", 7),
        ];
        std::fs::write(dir.join("b.jsonl"), replay.join("\n")).unwrap();
        let b = measure_audited(&dir, "2027", &targets(), &[], true, None);
        // The copy closes the episode: b.md opens its own.
        assert_eq!((b.episodes, b.total, b.used), (2, 57, 2));
        let b = measure_audited(&dir, "2027", &targets(), &[], false, None);
        assert_eq!((b.episodes, b.total), (2, 107), "without dedup the copy joins: 50, then 50 + 7");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kept_inputs_classify_as_the_whole_input_does() {
        let t = targets();
        let full = crate::json!({"pattern": "x", "path": "R:/repo/docs", "glob": "*.md", "output_mode": "content", "-n": true});
        assert_eq!(classify("Grep", &kept_input("Grep", &full), "", &t), classify("Grep", &full, "", &t));
        assert_eq!(call_line("Grep", &kept_input("Grep", &full)), call_line("Grep", &full));
        assert_eq!(kept_input("Edit", &crate::json!({"old_string": "a"})), Value::Null);
        assert_eq!(kept_input("Read", &crate::json!({"file_path": "a.md", "limit": 4})), crate::json!({"file_path": "a.md"}));
    }

    #[test]
    fn skips_match_whole_words_and_folders() {
        let s: Vec<String> = DEFAULT_SKIPS.iter().map(|x| x.to_string()).collect();
        let none: Vec<String> = Vec::new();
        let sk = |c: &str| skipped(c, &s, &none).is_some();
        let skf = |n: &str| skipped_folder(n, &s, &none).is_some();
        assert!(sk("C:\\Users\\me\\AppData\\Local\\Temp\\x"));
        assert!(sk("Z:/scratch/lane"));
        assert_eq!(skipped("W:/code/tool-eval/compact-work/run/jobs/7", &s, &none).as_deref(), Some("eval"));
        assert!(sk("/tmp/abc"));
        assert!(!sk("W:/code/template"), "a word inside a word is not a match");
        assert!(!sk("W:/code/game"));
        assert!(skf("W--code-tool-eval-compact-work-run-jobs-p00"));
        assert!(skf("C--Users-me-AppData-Local-Temp-x"));
        assert!(!skf("W--code") && !skf("W--code-template"));
        assert!(skipped_folder("Z--work-old-a", &["Z:/work/old".to_string()], &none).is_some());
        assert!(skipped_folder("Z--work-old", &["Z:\\work\\old\\".to_string()], &none).is_some());
        assert!(skipped_folder("Z--work-older", &["Z:/work/old".to_string()], &none).is_none());
        assert_eq!(folder_of(Path::new("C:/u/.claude/projects/W--code/s/subagents/a.jsonl")).as_deref(), Some("W--code"));
        assert_eq!(folder_of(Path::new("/home/u/.claude/projects/-home-u-x/s.jsonl")).as_deref(), Some("-home-u-x"));
        let p = vec!["Z:\\work\\old".to_string()];
        assert!(skipped("z:/work/old/a", &p, &none).is_some());
        assert!(skipped("z:/work/older", &p, &none).is_none());
    }

    #[test]
    fn skip_words_above_the_project_root_do_not_skip() {
        let s: Vec<String> = DEFAULT_SKIPS.iter().map(|x| x.to_string()).collect();
        let roots = vec!["d:/temp/eval/proj".to_string(), "d:/temp/eval/proj/deeper".to_string()];
        assert_eq!(skipped("D:\\Temp\\eval\\proj",&s, &roots), None);
        assert_eq!(skipped("D:/temp/eval/proj/src", &s, &roots), None);
        assert_eq!(skipped("D:/temp/eval/proj/scratch/x", &s, &roots).as_deref(), Some("scratch"));
        assert_eq!(skipped("D:/temp/eval/proj/deeper/tmp", &s, &roots).as_deref(), Some("tmp"));
        assert_eq!(skipped("D:/temp/other", &s, &roots).as_deref(), Some("temp"), "outside every root: the whole path");
        let prefixes = vec!["d--temp-eval-proj".to_string()];
        assert_eq!(skipped_folder("D--temp-eval-proj", &s, &prefixes), None);
        assert_eq!(skipped_folder("D--temp-eval-proj-src", &s, &prefixes), None);
        assert_eq!(skipped_folder("D--temp-eval-proj-eval-p00", &s, &prefixes).as_deref(), Some("eval"));
        assert_eq!(skipped_folder("D--temp-eval-projx", &s, &prefixes).as_deref(), Some("temp"), "not below the prefix: the whole name");
    }

    /// A project `p` rooted at `<dir>/work` with a vault `notes` in it, its
    /// store, and a Claude Code projects folder of its own.
    struct Fx {
        dir: PathBuf,
        root: PathBuf,
        folder: PathBuf,
        raw: Config,
    }

    impl Fx {
        fn new(tag: &str) -> Fx {
            let dir = temp(tag);
            let root = dir.join("work");
            std::fs::create_dir_all(root.join("notes")).unwrap();
            std::fs::create_dir_all(dir.join("state")).unwrap();
            let s = |p: &Path| p.to_string_lossy().replace('\\', "/");
            let cfg = dir.join("atlas.json");
            let text = format!(
                r#"{{"calibrate": {{"skip": ["scratch", "eval"]}}, "projects": [{{"name": "p", "roots": ["{}"], "db": "{}", "vaults": [{{"name": "notes", "path": "{}"}}]}}, {{"name": "q", "roots": ["{}"]}}]}}"#,
                s(&root),
                s(&dir.join("state").join("atlas.sqlite")),
                s(&root.join("notes")),
                s(&dir.join("work-old"))
            );
            std::fs::write(&cfg, text).unwrap();
            let raw = Config::load(&cfg).unwrap();
            let folder = dir.join("claude").join("projects").join(crate::transcripts::project_of_cwd(&root));
            std::fs::create_dir_all(&folder).unwrap();
            Fx { dir, root, folder, raw }
        }

        fn plan(&self) -> Plan {
            let r = crate::project::resolve(&self.raw, &self.root, None).unwrap();
            let mut p = Plan::new(&self.raw, &r.config, r.active.as_ref(), &self.dir.join("state").join("atlas.sqlite")).unwrap();
            p.projects = Some(self.dir.join("claude").join("projects"));
            p
        }

        fn conn(&self) -> Connection {
            open(&self.dir.join("state").join("atlas.sqlite")).unwrap()
        }

        /// A transcript in `folder` whose session ran in `cwd`: `n` lookup
        /// episodes (a vault read, then a build), the first at minute
        /// `start`, then an atlas call at minute `atlas` when given.
        fn transcript(&self, folder: &Path, name: &str, cwd: &Path, start: usize, n: usize, atlas: Option<usize>) {
            std::fs::create_dir_all(folder).unwrap();
            let cwd = cwd.to_string_lossy().replace('\\', "/");
            let note = self.root.join("notes").join("a.md").to_string_lossy().replace('\\', "/");
            let at = |m: usize| format!("2026-01-01T{:02}:{:02}:00Z", m / 60, m % 60);
            let line = |id: String, name: &str, input: Value, m: usize| {
                crate::json!({"type": "assistant", "timestamp": at(m), "cwd": cwd.clone(), "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}}).to_string()
            };
            let res = |id: String, b: usize, m: usize| crate::json!({"type": "user", "timestamp": at(m), "cwd": cwd.clone(), "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "x".repeat(b)}]}}).to_string();
            let mut lines = Vec::new();
            for i in 0..n {
                let m = start + i;
                lines.push(line(format!("{name}-r{i}"), "Read", crate::json!({"file_path": note.clone()}), m));
                lines.push(res(format!("{name}-r{i}"), 100 + i, m));
                lines.push(line(format!("{name}-b{i}"), "Bash", crate::json!({"command": "cargo build"}), m));
                lines.push(res(format!("{name}-b{i}"), 5, m));
            }
            if let Some(m) = atlas {
                lines.push(line(format!("{name}-atlas"), "mcp__atlas__recall", crate::json!({}), m));
            }
            std::fs::write(folder.join(format!("{name}.jsonl")), lines.join("\n")).unwrap();
        }
    }

    #[test]
    fn a_run_finds_the_cutoff_stores_the_calls_and_survives_the_transcripts() {
        let fx = Fx::new("run");
        // Six sessions of six lookups each from minute 0; the first atlas
        // call at minute 100 in one of them; another session's lookups run
        // on past it.
        for i in 0..5 {
            fx.transcript(&fx.folder, &format!("s{i}"), &fx.root, 0, 6, None);
        }
        fx.transcript(&fx.folder, "s5", &fx.root, 0, 6, Some(100));
        fx.transcript(&fx.folder, "s6", &fx.root, 98, 4, None);
        let plan = fx.plan();
        let conn = fx.conn();
        assert_eq!(stale(&conn, &plan).unwrap().as_deref(), Some("no baseline yet"));
        let o = run(&plan, &conn, &mut |_| {}, None).unwrap();
        let b = &o.baseline;
        assert_eq!(b.before, "2026-01-01T01:40:00Z");
        assert_eq!(b.cutoff_source.as_deref(), Some("the first atlas call in the transcripts"));
        // s6 counts its two lookups before minute 100 only.
        assert_eq!((b.episodes, b.used, b.transcripts), (38, 7, 7));
        assert_eq!((b.rules, b.too_few), (Some(RULES_VERSION), false));
        assert_eq!(b.fingerprint.as_deref(), Some(plan.fingerprint.as_str()));
        assert_eq!(stale(&conn, &plan).unwrap(), None);
        assert_eq!(crate::baseline::latest(&conn).unwrap().unwrap().1, *b);

        // A transcript stored is not read again.
        assert_eq!(run(&plan, &conn, &mut |_| {}, None).unwrap().read, 0);
        // Unless older rules stored it and it is still there: then it is
        // read again, and measures the same.
        conn.execute("UPDATE calibrate_files SET rules = 0", []).unwrap();
        let o = run(&plan, &conn, &mut |_| {}, None).unwrap();
        assert_eq!((o.read, o.stored, o.baseline.episodes), (7, 7, 38));

        // A stored call at or after the cutoff (one stored by a run that
        // lost the race to set it) does not count.
        let note = fx.root.join("notes").join("a.md").to_string_lossy().replace('\\', "/");
        let file: String = conn.query_row("SELECT file FROM calibrate_files ORDER BY file LIMIT 1", [], |r| r.get(0)).unwrap();
        let input = crate::json!({"file_path": note}).to_string();
        conn.execute("INSERT INTO calibrate_calls(file, seq, id, time, cwd, name, input, bytes) VALUES(?1, 999, 'late', '2026-01-01T01:40:00Z', '', 'Read', ?2, 50)", params![file, input]).unwrap();
        assert_eq!(run(&plan, &conn, &mut |_| {}, None).unwrap().baseline.episodes, 38);

        // Claude Code deleted the transcripts: a rerun measures the same
        // from the stored calls, and reads nothing.
        std::fs::remove_dir_all(fx.dir.join("claude")).unwrap();
        let again = run(&plan, &conn, &mut |_| {}, None).unwrap();
        assert_eq!((again.read, again.baseline.episodes, again.baseline.before.as_str()), (0, 38, "2026-01-01T01:40:00Z"));

        // Other targets: stale, and measured again from the stored calls.
        let mut other = fx.plan();
        other.targets = Targets::new(&[], &[], DEFAULT_HANDOFF).unwrap();
        other.fingerprint = "other".into();
        assert_eq!(stale(&conn, &other).unwrap().as_deref(), Some("the configured targets changed"));
        let o = run(&other, &conn, &mut |_| {}, None).unwrap();
        assert_eq!((o.baseline.episodes, o.baseline.too_few, o.kept.is_none()), (0, true, true), "fewer episodes under other targets replace");

        // Same rules and targets, fewer episodes (stored calls lost): the
        // stamped baseline stays.
        let o = run(&plan, &conn, &mut |_| {}, None).unwrap();
        assert_eq!(o.baseline.episodes, 38);
        conn.execute("DELETE FROM calibrate_calls WHERE file LIKE '%s0.jsonl'", []).unwrap();
        let o = run(&plan, &conn, &mut |_| {}, None).unwrap();
        assert_eq!((o.baseline.episodes, o.kept.as_ref().map(|k| k.episodes)), (32, Some(38)));
        assert_eq!(crate::baseline::latest(&conn).unwrap().unwrap().1.episodes, 38);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn the_ledger_or_the_first_run_sets_the_cutoff_when_it_is_earlier() {
        let fx = Fx::new("ledger");
        for i in 0..5 {
            fx.transcript(&fx.folder, &format!("s{i}"), &fx.root, 0, 6, Some(200));
        }
        let conn = fx.conn();
        conn.execute_batch(crate::tokens::TOKEN_TABLE).unwrap();
        conn.execute("INSERT INTO token_log(time, day, tool, returned) VALUES('2026-01-01T00:03:30Z', '2026-01-01', 'recall', 10)", []).unwrap();
        let o = run(&fx.plan(), &conn, &mut |_| {}, None).unwrap();
        assert_eq!((o.baseline.before.as_str(), o.baseline.cutoff_source.as_deref()), ("2026-01-01T00:03:30Z", Some("the store's first atlas answer")));
        // Four lookups each before 00:03:30: 20 episodes, too few.
        assert_eq!((o.baseline.episodes, o.baseline.too_few), (20, true));
        let _ = std::fs::remove_dir_all(&fx.dir);

        // An atlas call in the transcripts before the ledger's first answer:
        // the call sets it.
        let fx = Fx::new("both");
        fx.transcript(&fx.folder, "s0", &fx.root, 0, 2, Some(2));
        let conn = fx.conn();
        conn.execute_batch(crate::tokens::TOKEN_TABLE).unwrap();
        conn.execute("INSERT INTO token_log(time, day, tool, returned) VALUES('2026-01-01T00:03:30Z', '2026-01-01', 'recall', 10)", []).unwrap();
        let o = run(&fx.plan(), &conn, &mut |_| {}, None).unwrap();
        assert_eq!((o.baseline.before.as_str(), o.baseline.cutoff_source.as_deref()), ("2026-01-01T00:02:00Z", Some("the first atlas call in the transcripts")));
        let _ = std::fs::remove_dir_all(&fx.dir);

        // No atlas use anywhere: the first run's own time, every call before it.
        let fx = Fx::new("fresh");
        fx.transcript(&fx.folder, "s0", &fx.root, 0, 3, None);
        let conn = fx.conn();
        let o = run(&fx.plan(), &conn, &mut |_| {}, None).unwrap();
        assert_eq!(o.baseline.cutoff_source.as_deref(), Some("the first calibration (no atlas use before it)"));
        assert!(o.baseline.before.as_str() > "2026-02", "{}", o.baseline.before);
        assert_eq!((o.baseline.episodes, o.baseline.too_few), (3, true));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn transcripts_count_by_their_working_folder() {
        let fx = Fx::new("folders");
        let projects = fx.dir.join("claude").join("projects");
        // The project's own folder, and a folder below its root.
        fx.transcript(&fx.folder, "own", &fx.root, 0, 2, None);
        let sub = fx.root.join("sub");
        fx.transcript(&projects.join(crate::transcripts::project_of_cwd(&sub)), "sub", &sub, 0, 2, None);
        // A sibling whose folder name shares the prefix (`work-old`): opened,
        // but its working folder is the root of another project, `q`.
        let sib = fx.dir.join("work-old");
        fx.transcript(&projects.join(crate::transcripts::project_of_cwd(&sib)), "sib", &sib, 0, 2, None);
        // A scratch folder inside the root: skipped.
        let scratch = fx.root.join("scratch");
        fx.transcript(&projects.join(crate::transcripts::project_of_cwd(&scratch)), "scr", &scratch, 0, 2, None);
        // A replay filed under an eval folder below the root, keeping the
        // working folder of the session it copies: never opened.
        let eval = fx.root.join("eval").join("run");
        fx.transcript(&projects.join(crate::transcripts::project_of_cwd(&eval)), "replay", &fx.root, 50, 2, None);
        // A session filed under the project's folder whose working folder is
        // a scratch folder below the root: read, skipped and reported.
        fx.transcript(&fx.folder, "cd", &fx.root.join("scratch").join("b"), 0, 2, None);
        // Another project's scratch session filed there: read, not counted,
        // and not reported as skipped (it would not count anyway).
        fx.transcript(&fx.folder, "qscr", &fx.dir.join("work-old").join("scratch"), 0, 2, None);
        // A folder elsewhere: never opened.
        let away = fx.dir.join("away");
        fx.transcript(&projects.join(crate::transcripts::project_of_cwd(&away)), "away", &away, 0, 2, None);
        let plan = fx.plan();
        let (opened, notes) = plan.folders();
        assert_eq!(opened.len(), 3, "{opened:?}");
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("-eval-run") && notes[1].contains("-scratch") && notes[1].contains("\"scratch\""), "{notes:?}");
        assert!(notes[0].contains("\"eval\""), "{notes:?}");
        let mut audit = Vec::new();
        let mut lines = Vec::new();
        let o = run(&plan, &fx.conn(), &mut |m| lines.push(m.to_string()), Some(&mut audit)).unwrap();
        let audit = String::from_utf8(audit).unwrap();
        assert_eq!((o.folders, o.read, o.stored), (3, 5, 2));
        // The eval and scratch folders and the scratch session, each with its skip.
        assert_eq!(o.skipped.len(), 3, "{:?}", o.skipped);
        assert_eq!(o.skipped[..2], notes[..]);
        assert!(o.skipped[2].contains("cd.jsonl") && o.skipped[2].contains("\"scratch\""), "{:?}", o.skipped);
        assert!(o.skipped.iter().all(|l| lines.contains(l)), "every skip is reported");
        assert_eq!((o.baseline.transcripts, o.baseline.episodes), (2, 4));
        assert!(audit.contains("own.jsonl") && audit.contains("sub.jsonl") && !audit.contains("sib.jsonl"), "{audit}");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn a_held_lock_or_a_fresh_baseline_means_no_background_run() {
        let fx = Fx::new("lock");
        for i in 0..5 {
            fx.transcript(&fx.folder, &format!("s{i}"), &fx.root, 0, 6, None);
        }
        let plan = fx.plan();
        let held = crate::fsio::FileLock::acquire_within(&plan.lock_target(), std::time::Duration::ZERO).unwrap();
        assert_eq!(background(&plan).unwrap(), None);
        assert!(crate::baseline::latest(&fx.conn()).unwrap().is_none());
        drop(held);
        let line = background(&plan).unwrap().unwrap();
        assert!(line.starts_with("no baseline yet: 30 pre-atlas lookup episodes in 5 of 5 transcripts"), "{line}");
        assert_eq!(background(&plan).unwrap(), None, "fresh: nothing to do");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn an_unstamped_or_older_baseline_is_stale() {
        let fx = Fx::new("stale");
        let plan = fx.plan();
        let conn = fx.conn();
        let mut b = Baseline { before: "x".into(), episodes: 40, ..Default::default() };
        crate::baseline::save(&conn, &b).unwrap();
        assert_eq!(stale(&conn, &plan).unwrap().as_deref(), Some("the stored baseline has no rules version"));
        b.rules = Some(RULES_VERSION - 1);
        crate::baseline::save(&conn, &b).unwrap();
        assert!(stale(&conn, &plan).unwrap().unwrap().starts_with("the lookup rules changed"));
        // A newer atlas sharing the store measured it: it stands, whatever
        // its targets, or the two would recalibrate in turn.
        b.rules = Some(RULES_VERSION + 1);
        b.fingerprint = Some("a newer fingerprint".into());
        crate::baseline::save(&conn, &b).unwrap();
        assert_eq!(stale(&conn, &plan).unwrap(), None);
        b.rules = Some(RULES_VERSION);
        b.fingerprint = Some(plan.fingerprint.clone());
        crate::baseline::save(&conn, &b).unwrap();
        assert_eq!(stale(&conn, &plan).unwrap(), None);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn the_fingerprint_follows_targets_skips_and_handoff() {
        let fx = Fx::new("fp");
        let a = fx.plan();
        let r = crate::project::resolve(&fx.raw, &fx.root, None).unwrap();
        let mut c = r.config.clone();
        c.calibrate = Some(crate::config::CalibrateSpec { skip: Some(vec!["tmp".into()]), ..Default::default() });
        let b = Plan::new(&fx.raw, &c, r.active.as_ref(), &a.db).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        c.calibrate = Some(crate::config::CalibrateSpec { skip: Some(vec!["scratch".into()]), handoff: Some("notes".into()), ..Default::default() });
        assert_ne!(a.fingerprint, Plan::new(&fx.raw, &c, r.active.as_ref(), &a.db).unwrap().fingerprint);
        c.calibrate = Some(crate::config::CalibrateSpec { skip: Some(vec!["scratch".into()]), ..Default::default() });
        c.vaults.clear();
        assert_ne!(a.fingerprint, Plan::new(&fx.raw, &c, r.active.as_ref(), &a.db).unwrap().fingerprint);
        c = r.config.clone();
        assert_eq!(a.fingerprint, Plan::new(&fx.raw, &c, r.active.as_ref(), &a.db).unwrap().fingerprint);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    /// A run on a store a newer atlas upgraded since `open` writes nothing:
    /// no table, no stored call, no cutoff and no baseline.
    #[test]
    fn a_run_checks_the_version_under_the_lock() {
        let fx = Fx::new("storeguard");
        for i in 0..5 {
            fx.transcript(&fx.folder, &format!("s{i}"), &fx.root, 0, 6, None);
        }
        let plan = fx.plan();
        let conn = fx.conn();
        crate::store::skew_support::make_index_newer(&conn);
        let before = crate::store::skew_support::dump(&conn);
        let e = run(&plan, &conn, &mut |_| {}, None).expect_err("refused");
        assert!(e.contains("store too new") && e.contains("999/parse-0"), "{e}");
        assert!(conn.is_autocommit(), "rolled back");
        assert_eq!(crate::store::skew_support::dump(&conn), before);
        drop(conn);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }
}
