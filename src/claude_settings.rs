//! `atlas settings`: the Claude Code settings atlas offers, and atlas's own
//! switches, each explained, diffed, backed up and reversible.
//!
//! ```text
//! atlas settings list                 every setting: what, cost, measured basis, current value
//! atlas settings set ID VALUE         any allowed value, at any time (warns on risky ones)
//! atlas settings off ID               a Claude Code setting: atlas's value is removed and
//!                                     Claude Code's default applies; an atlas switch: off
//! atlas settings undo                 restores the newest backup (and consumes it)
//! atlas settings offer [--yes ID... | --no]
//!                                     asks about each Claude Code setting; --yes applies
//!                                     the named ones without asking, --no declines them
//! atlas settings orchestrate [--yes OPTIONS | --no]
//!                                     asks whether to work in orchestrated mode, then
//!                                     its options (see `orchestrated.rs`)
//! ```
//!
//! The manifest ships as data ([`MANIFEST`], `config/claude_settings.json`).
//! Four kinds of entry:
//!
//! * `setting`: a top-level key of Claude Code's `settings.json`;
//! * `env`: a key of that file's `env` object;
//! * `atlas`: a dotted key of atlas's own configuration (`update.enabled`,
//!   `update.pin`, read by [`crate::update`]);
//! * `flag`: an atlas switch kept as a flag beside the index, or in the
//!   user-level state folder of a configuration with projects (`routing`:
//!   [`crate::hook_rewrite::set_routing`] writes it, and
//!   [`crate::hook_rewrite::routing_on`] reads it).
//!
//! atlas's switches are on by default and explained at install.
//! `atlas settings on|off update` is answered by the updater's own switch in
//! `main.rs`; `set update on|off` here does the same with a backup.
//!
//! Behaviour kept from the reference installer: opt-in per Claude Code
//! setting with what it does, what it costs and the measured basis shown
//! first; the exact change shown before writing, nothing written on no; a
//! timestamped backup before every write; idempotent (an applied setting is
//! reported, not rewritten; other keys are never touched); a setting with a
//! `min_version` is skipped with a note when `claude --version` is older.
//!
//! Writing. `settings.json` is parsed and re-encoded with two-space
//! indentation (the layout Claude Code writes). When the file as found does
//! not re-encode to the same bytes, atlas refuses to rewrite it and prints
//! the one edit to make by hand. atlas's own configuration is hand-edited,
//! so it is changed by splicing the one value into the text; the result is
//! parsed and must equal the old value with only that change, or nothing is
//! written. Every write is atomic (a temporary file, then a rename).
//!
//! Paths: `$CLAUDE_CONFIG_DIR/settings.json`, else `<home>/.claude/settings.json`
//! (home is `USERPROFILE` on Windows, `HOME` elsewhere). Backups of
//! `settings.json`: `<that folder>/atlas-backups/settings.YYYYMMDD-HHMMSS.json`
//! (UTC). Backups of atlas's configuration stay out of Claude Code's folder:
//! `atlas-backups/atlas.YYYYMMDD-HHMMSS.json` beside the configuration file.
//! A backup ending in `.absent` records that the file did not exist, so undo
//! removes it. Undo takes the newest backup of either kind, each read from
//! its own folder. A `vault.YYYYMMDD-HHMMSS.absent` record beside an atlas
//! backup names a vault folder `add-project --vault` registered; undo keeps
//! that folder unless atlas created it and it holds only atlas's note.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::json::{Map, Value};

/// The manifest, compiled in.
pub const MANIFEST: &str = include_str!("../config/claude_settings.json");

/// The state file recording what was offered (manifest ids, folders,
/// unmapped repos), beside the index database, or in the user-level state
/// folder of a configuration with projects.
pub const OFFER_STATE: &str = "settings-offer.json";

/// Where an entry's value lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A top-level key of Claude Code's `settings.json`.
    Setting,
    /// A key of `settings.json`'s `env` object.
    Env,
    /// A dotted key of atlas's own configuration.
    Atlas,
}

/// One manifest entry.
#[derive(Debug, Clone)]
pub struct Item {
    /// The id the commands take.
    pub id: String,
    /// Where the value lives.
    pub kind: Kind,
    /// The key (dotted for [`Kind::Atlas`]).
    pub key: String,
    /// The value atlas offers (for an atlas switch, its default); `None`
    /// for an entry that has no offered value (a pin).
    pub value: Option<Value>,
    /// `bool`, `version`, or empty (a string checked by `choices`/`range`).
    pub typ: String,
    /// The oldest Claude Code version the setting works in.
    pub min_version: Option<(u32, u32, u32)>,
    /// A two- or three-word name for the one-line offer.
    pub short: String,
    /// The title.
    pub title: String,
    /// What it does.
    pub what: String,
    /// What it costs.
    pub cost: String,
    /// The measured basis.
    pub basis: String,
    /// A documentation link.
    pub docs: String,
    /// The allowed values, when listed.
    pub choices: Vec<String>,
    /// The allowed range: min, max, integer only.
    pub range: Option<(f64, f64, bool)>,
    /// Warnings for risky values.
    pub warn: Vec<(String, String)>,
    /// What applies without atlas's value.
    pub default_note: String,
    /// An atlas switch kept as a flag file beside the index rather than in
    /// the configuration (`routing`: [`crate::hook_rewrite::set_routing`]).
    pub flag: bool,
}

impl Item {
    /// True for an atlas switch that is on unless turned off.
    pub fn default_on(&self) -> bool {
        self.kind == Kind::Atlas && self.value.is_some()
    }
}

/// The manifest's entries.
///
/// # Errors
///
/// A message when the manifest is malformed (a test keeps it well formed).
pub fn manifest() -> Result<Vec<Item>, String> {
    let v = crate::json::parse(MANIFEST).map_err(|e| format!("manifest: {}", e.msg))?;
    let list = v.get("settings").and_then(Value::as_array).ok_or("manifest: no settings list")?;
    let mut out = Vec::new();
    for s in list {
        let t = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let kind = match t("kind").as_str() {
            "setting" => Kind::Setting,
            "env" => Kind::Env,
            "atlas" | "flag" => Kind::Atlas,
            k => return Err(format!("manifest: {}: unknown kind {k:?}", t("id"))),
        };
        let range = s.get("range").map(|r| {
            let n = |k: &str| r.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            (n("min"), n("max"), r.get("integer").and_then(Value::as_bool).unwrap_or(false))
        });
        let warn = s.get("warn").and_then(Value::as_object).map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).unwrap_or_default();
        let min_version = match t("min_version") {
            m if m.is_empty() => None,
            m => Some(parse_version(&m).ok_or_else(|| format!("manifest: {}: bad min_version", t("id")))?),
        };
        out.push(Item {
            id: t("id"),
            kind,
            key: t("key"),
            value: s.get("value").cloned(),
            typ: t("type"),
            min_version,
            short: t("short"),
            title: t("title"),
            what: t("what"),
            cost: t("cost"),
            basis: t("basis"),
            docs: t("docs"),
            choices: s.get("choices").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default(),
            range,
            warn,
            default_note: t("default_note"),
            flag: t("kind") == "flag",
        });
    }
    Ok(out)
}

/// The first `major.minor.patch` in a text (`claude --version` prints
/// `2.1.316 (Claude Code)`); a leading `v` is allowed.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let rest = &text[i..];
            let mut parts = rest.splitn(3, '.');
            let a = parts.next()?;
            let (Some(m), Some(p)) = (parts.next(), parts.next()) else { return None };
            let p: String = p.chars().take_while(char::is_ascii_digit).collect();
            if let (Ok(a), Ok(m), Ok(p)) = (a.parse(), m.parse(), p.parse::<u32>()) {
                return Some((a, m, p));
            }
            return None;
        }
        i += 1;
    }
    None
}

// ------------------------------------------------------------ the switches

/// Turns a flag switch on or off. Routing is the only one: its off flag
/// lives beside the index ([`crate::hook_rewrite::set_routing`]), so no
/// configuration is rewritten and the hooks see it on their next event.
fn flag_write(p: &Paths, io: &mut Io, s: &Item, on: bool) -> Result<bool, String> {
    let dir = p.flag_dir().ok_or("no index database is known for the routing flag")?;
    match s.key.as_str() {
        "routing" => crate::hook_rewrite::set_routing_in(&dir, on).map_err(|e| e.to_string())?,
        k => return Err(format!("internal: unknown flag {k:?}")),
    }
    let _ = writeln!(io.out, "[{}] turned {}", s.id, if on { "on" } else { "off" });
    let cfg_off = atlas_config_value(p).get("lean").and_then(|l| l.get("routing")).and_then(Value::as_bool) == Some(false);
    if on && cfg_off {
        let _ = writeln!(io.out, "  note: the configuration also sets lean.routing to false, which keeps routing off; remove it there");
    }
    Ok(true)
}

// ------------------------------------------------------------------ paths

/// [`Paths`] with the lock on the configuration files a command changes: only
/// this reaches the functions that write them, so a writer cannot run outside
/// the lock. A command asks its questions first, takes this once, and makes
/// every read its write rests on after that. See [`crate::fsio::FileLock`].
#[derive(Debug)]
pub struct Locked<'a> {
    paths: &'a Paths,
    // Dropped in this order; atlas.json is always taken first.
    _settings: Option<crate::fsio::FileLock>,
    _atlas: Option<crate::fsio::FileLock>,
}

impl std::ops::Deref for Locked<'_> {
    type Target = Paths;
    fn deref(&self) -> &Paths {
        self.paths
    }
}

/// The files the commands read and write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Claude Code's configuration folder (holds `settings.json`).
    pub claude_dir: PathBuf,
    /// atlas's configuration file, when there is one.
    pub atlas_config: Option<PathBuf>,
    /// The offer state file.
    pub state: Option<PathBuf>,
    /// The index database (the routing flag lives beside it without
    /// projects).
    pub db: Option<PathBuf>,
    /// The user-level state folder of a configuration with projects: the
    /// offer state and the routing flag live there, shared by every project.
    pub user_state: Option<PathBuf>,
    /// The project named with `--project` (or `ATLAS_PROJECT`): the one
    /// `add-project` adds a folder to, over the folder's own project.
    pub project: Option<String>,
}

impl Paths {
    /// Holds no lock: a dry run writes nothing, and must not make a lock file.
    fn unlocked(&self) -> Locked<'_> {
        Locked { paths: self, _settings: None, _atlas: None }
    }

    /// Locks atlas's configuration file (nothing is held without one).
    ///
    /// # Errors
    ///
    /// A message naming the lock file when it stays held past the deadline.
    pub fn lock_config(&self) -> Result<Locked<'_>, String> {
        let atlas = self.atlas_config.as_deref().map(crate::fsio::FileLock::acquire).transpose()?;
        Ok(Locked { paths: self, _settings: None, _atlas: atlas })
    }

    /// Locks Claude Code's `settings.json`. Only atlas against atlas: Claude
    /// Code writes the file itself and takes no lock.
    fn lock_settings(&self) -> Result<Locked<'_>, String> {
        Ok(Locked { paths: self, _settings: Some(crate::fsio::FileLock::acquire(&self.settings())?), _atlas: None })
    }

    /// Locks both files, atlas's first (the fixed order; only undo holds two).
    /// With no backups in Claude Code's folder there is nothing of its to
    /// restore, and that folder gets no lock file.
    fn lock_both(&self) -> Result<Locked<'_>, String> {
        let atlas = self.atlas_config.as_deref().map(crate::fsio::FileLock::acquire).transpose()?;
        let settings = self.backups().is_dir().then(|| crate::fsio::FileLock::acquire(&self.settings())).transpose()?;
        Ok(Locked { paths: self, _settings: settings, _atlas: atlas })
    }

    /// `$CLAUDE_CONFIG_DIR`, else `<home>/.claude`; `None` when no home is
    /// known. The offer state goes beside `db`.
    pub fn detect(atlas_config: Option<&Path>, db: &Path) -> Option<Paths> {
        Paths::detect_in(atlas_config, db, None)
    }

    /// [`Paths::detect`], with the offer state and the routing flag in
    /// `user_state` when given (a configuration with projects; see
    /// [`crate::project::user_state_dir`]).
    pub fn detect_in(atlas_config: Option<&Path>, db: &Path, user_state: Option<&Path>) -> Option<Paths> {
        let env = |n: &str| std::env::var_os(n).filter(|v| !v.is_empty()).map(PathBuf::from);
        let claude_dir = env("CLAUDE_CONFIG_DIR").or_else(|| env(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| h.join(".claude")))?;
        let state = match user_state {
            Some(u) => Some(u.join(OFFER_STATE)),
            None => db.parent().map(|d| d.join(OFFER_STATE)),
        };
        Some(Paths { claude_dir, atlas_config: atlas_config.map(Path::to_path_buf), state, db: Some(db.to_path_buf()), user_state: user_state.map(Path::to_path_buf), project: None })
    }

    /// The folder holding the routing flag: the user-level state folder,
    /// else the index database's folder.
    pub fn flag_dir(&self) -> Option<PathBuf> {
        self.user_state.clone().or_else(|| self.db.as_deref().and_then(Path::parent).map(Path::to_path_buf))
    }

    /// Claude Code's user settings file.
    pub fn settings(&self) -> PathBuf {
        self.claude_dir.join("settings.json")
    }

    /// The folder backups of Claude Code's `settings.json` go to.
    pub fn backups(&self) -> PathBuf {
        self.claude_dir.join(BACKUPS)
    }

    /// The folder backups of atlas's configuration go to: beside the
    /// configuration file, never in Claude Code's folder.
    pub fn atlas_backups(&self) -> Option<PathBuf> {
        Some(self.atlas_config.as_deref()?.parent()?.join(BACKUPS))
    }

    /// Where backups with this stem (`settings` or `atlas`) are kept.
    fn backups_for(&self, stem: &str) -> Result<PathBuf, String> {
        match stem {
            "atlas" | "project" | "vault" => self.atlas_backups().ok_or_else(|| "no atlas configuration is in use to back up".to_string()),
            _ => Ok(self.backups()),
        }
    }
}

// --------------------------------------------------------------- the files

/// A JSON file as found: its text (`None` when absent) and its value.
struct Doc {
    path: PathBuf,
    text: Option<String>,
    value: Value,
}

fn load(path: &Path) -> Result<Doc, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value = if text.trim().is_empty() { Value::Object(Map::new()) } else { crate::json::parse(&text).map_err(|e| format!("{}: not JSON ({} at byte {})", path.display(), e.msg, e.at))? };
            if !value.is_object() {
                return Err(format!("{}: not a JSON object", path.display()));
            }
            Ok(Doc { path: path.to_path_buf(), text: Some(text), value })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Doc { path: path.to_path_buf(), text: None, value: Value::Object(Map::new()) }),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The text `settings.json` is rewritten to, or `None` when the file as
/// found does not re-encode to its own bytes (atlas then refuses).
fn reencode(doc: &Doc, new: &Value) -> Option<String> {
    let pretty = |v: &Value| crate::json::to_string_pretty(v);
    let Some(text) = &doc.text else { return Some(pretty(new) + "\n") };
    if text.trim().is_empty() {
        return Some(pretty(new) + "\n");
    }
    let crlf = text.contains("\r\n");
    let norm = if crlf { text.replace("\r\n", "\n") } else { text.clone() };
    let (core, nl) = match norm.strip_suffix('\n') {
        Some(c) => (c, "\n"),
        None => (norm.as_str(), ""),
    };
    if core != pretty(&doc.value) {
        return None;
    }
    let out = pretty(new) + nl;
    Some(if crlf { out.replace('\n', "\r\n") } else { out })
}

// --------------------------------------------- splicing atlas's own config

fn ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

fn skip_str(b: &[u8], mut i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn skip_val(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => skip_str(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = skip_str(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// One member of an object in the text: its key, where the key starts, and
/// where its value starts and ends.
struct Member {
    key: String,
    key_at: usize,
    val_at: usize,
    val_end: usize,
}

/// The members of the object opening at `open`, and the index of its `}`.
fn members(text: &str, open: usize) -> Option<(Vec<Member>, usize)> {
    let b = text.as_bytes();
    if b.get(open) != Some(&b'{') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = ws(b, open + 1);
    if b.get(i) == Some(&b'}') {
        return Some((out, i));
    }
    loop {
        let key_at = i;
        let ke = skip_str(b, i)?;
        let key = crate::json::parse(&text[key_at..ke]).ok()?.as_str()?.to_string();
        i = ws(b, ke);
        if b.get(i) != Some(&b':') {
            return None;
        }
        let val_at = ws(b, i + 1);
        let val_end = skip_val(b, val_at)?;
        out.push(Member { key, key_at, val_at, val_end });
        i = ws(b, val_end);
        match b.get(i) {
            Some(b',') => i = ws(b, i + 1),
            Some(b'}') => return Some((out, i)),
            _ => return None,
        }
    }
}

/// A value on one line: `{"a": 1, "b": [1, 2]}`.
fn inline(v: &Value) -> String {
    match v {
        Value::Object(m) => format!("{{{}}}", m.iter().map(|(k, x)| format!("{}: {}", Value::String(k.clone()), inline(x))).collect::<Vec<_>>().join(", ")),
        Value::Array(a) => format!("[{}]", a.iter().map(inline).collect::<Vec<_>>().join(", ")),
        x => x.to_string(),
    }
}

/// The leading whitespace of the line holding byte `at`, when only
/// whitespace precedes it on that line.
fn indent_at(text: &str, at: usize) -> Option<&str> {
    let start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    let lead = &text[start..at];
    lead.chars().all(|c| c == ' ' || c == '\t').then_some(lead)
}

/// `text` with the value at `path` set to `new` (or removed when `None`),
/// every other byte kept. `None` when the text cannot be spliced (a parent
/// on the path is not an object, or the text is not well formed).
pub fn splice(text: &str, path: &[&str], new: Option<&Value>) -> Option<String> {
    let b = text.as_bytes();
    let mut open = ws(b, 0);
    for (d, key) in path.iter().enumerate() {
        let (mems, close) = members(text, open)?;
        let last = d + 1 == path.len();
        if let Some(i) = mems.iter().position(|m| m.key == *key) {
            let m = &mems[i];
            if !last {
                open = m.val_at;
                if b.get(open) != Some(&b'{') {
                    return None;
                }
                continue;
            }
            return Some(match new {
                Some(v) => format!("{}{}{}", &text[..m.val_at], inline(v), &text[m.val_end..]),
                None => {
                    let (a, z) = if i > 0 {
                        (mems[i - 1].val_end, m.val_end)
                    } else if mems.len() > 1 {
                        (m.key_at, mems[1].key_at)
                    } else {
                        (open + 1, close)
                    };
                    format!("{}{}", &text[..a], &text[z..])
                }
            });
        }
        let Some(v) = new else { return Some(text.to_string()) };
        // Absent from here down: one member holding the rest of the path.
        let mut val = v.clone();
        for k in path[d + 1..].iter().rev() {
            let mut m = Map::new();
            m.insert((*k).to_string(), val);
            val = Value::Object(m);
        }
        let member = format!("{}: {}", Value::String((*key).to_string()), inline(&val));
        return Some(match mems.last() {
            Some(l) => {
                let sep = match indent_at(text, l.key_at) {
                    Some(ind) => format!(",\n{ind}"),
                    None => ", ".to_string(),
                };
                format!("{}{sep}{member}{}", &text[..l.val_end], &text[l.val_end..])
            }
            None => {
                let base = indent_at(text, open).unwrap_or("");
                format!("{}{{\n{base}  {member}\n{base}}}{}", &text[..open], &text[close + 1..])
            }
        });
    }
    None
}

/// `v` with the dotted path set (or removed).
fn set_path(v: &mut Value, path: &[&str], new: Option<&Value>) {
    let Some((k, rest)) = path.split_first() else { return };
    let Some(m) = v.as_object_mut() else { return };
    if rest.is_empty() {
        match new {
            Some(x) => {
                m.insert((*k).to_string(), x.clone());
            }
            None => {
                m.remove(k);
            }
        }
        return;
    }
    if !m.contains_key(k) {
        if new.is_none() {
            return;
        }
        m.insert((*k).to_string(), Value::Object(Map::new()));
    }
    if let Some(c) = m.get_mut(k) {
        set_path(c, rest, new);
    }
}

fn get_path<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| v.get(*k))
}

// ----------------------------------------------------------------- output

/// Where the commands read and write: the terminal, or a test's buffers.
pub struct Io<'a> {
    /// Where messages go.
    pub out: &'a mut dyn Write,
    /// Where answers come from.
    pub input: &'a mut dyn BufRead,
    /// Whether a person can answer (else the offer only describes).
    pub interactive: bool,
    /// Claude Code's version (`claude --version`), when known.
    pub version: Option<(u32, u32, u32)>,
    /// Show what `add-project` would write, and write nothing.
    pub dry_run: bool,
}

macro_rules! say {
    ($io:expr, $($t:tt)*) => {{ let _ = writeln!($io.out, $($t)*); }};
}

mod orchestrated;
pub use orchestrated::orchestrate_line;

fn show(v: Option<&Value>) -> String {
    match v {
        None => "not set".into(),
        Some(Value::Bool(true)) => "on".into(),
        Some(Value::Bool(false)) => "off".into(),
        Some(x) => x.to_string(),
    }
}

/// The value an entry has now (`None`: unset).
fn current<'a>(s: &Item, claude: &'a Value, atlas: &'a Value) -> Option<&'a Value> {
    match s.kind {
        Kind::Setting => claude.get(s.key.as_str()),
        Kind::Env => claude.get("env").and_then(|e| e.get(s.key.as_str())),
        Kind::Atlas => get_path(atlas, &s.key.split('.').collect::<Vec<_>>()),
    }
}

/// The value in effect: an unset atlas switch has its default.
fn effective<'a>(s: &'a Item, have: Option<&'a Value>) -> Option<&'a Value> {
    match (s.kind, have) {
        (Kind::Atlas, None) => s.value.as_ref(),
        (_, h) => h,
    }
}

fn applied(s: &Item, have: Option<&Value>) -> bool {
    s.value.is_some() && effective(s, have) == s.value.as_ref()
}

fn describe(s: &Item, have: Option<&Value>) -> String {
    let state = if s.kind == Kind::Atlas {
        match have {
            None => format!("{} by default", show(effective(s, have))),
            h => show(h),
        }
    } else if applied(s, have) {
        "applied".into()
    } else {
        match have {
            Some(h) => format!("currently {}", h),
            None => "not set".into(),
        }
    };
    let mut d = format!("\n[{}] {}  ({state})\n  What: {}\n  Cost: {}\n  Measured: {}\n", s.id, s.title, s.what, s.cost, s.basis);
    let note = if s.default_note.is_empty() { String::new() } else { format!(" ({})", s.default_note) };
    match (s.kind, &s.value) {
        (Kind::Atlas, _) if s.flag => d.push_str(&format!("  Where: a flag beside atlas's index{note}\n")),
        (Kind::Atlas, _) => d.push_str(&format!("  Where: atlas configuration \"{}\"{note}\n", s.key)),
        (k, Some(v)) => {
            let place = if k == Kind::Env { format!("env \"{}\"", s.key) } else { format!("\"{}\"", s.key) };
            d.push_str(&format!("  Change: {place}: {} -> {v}{note}\n", show(have)));
        }
        (_, None) => {}
    }
    d.push_str(&format!("  Change any time: atlas settings set {} <value>, or atlas settings off {}", s.id, s.id));
    if !s.docs.is_empty() {
        d.push_str(&format!("\n  Docs: {}", s.docs));
    }
    d
}

/// The changed lines between two texts (one region: common lines at either
/// end are dropped, a line of context kept on each side).
pub fn diff(label: &str, old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        suf += 1;
    }
    let mut out = format!("--- {label}\n+++ {label} (after)\n");
    if pre > 0 {
        out.push_str(&format!(" {}\n", a[pre - 1]));
    }
    for l in &a[pre..a.len() - suf] {
        out.push_str(&format!("-{l}\n"));
    }
    for l in &b[pre..b.len() - suf] {
        out.push_str(&format!("+{l}\n"));
    }
    if suf > 0 {
        out.push_str(&format!(" {}\n", a[a.len() - suf]));
    }
    out
}

// ---------------------------------------------------------------- backups

fn stamp() -> String {
    // utc_now_ms: 2026-10-31T12:52:03.123Z
    let t = crate::discussion::utc_now_ms();
    let d: String = t.chars().filter(char::is_ascii_digit).collect();
    format!("{}-{}", &d[..8], &d[8..14])
}

/// The backup folder's name, in either place.
const BACKUPS: &str = "atlas-backups";

/// A backup file's sort key: its stamp and collision number.
fn backup_key(name: &str) -> Option<(String, u32)> {
    let (_, rest) = name.split_once('.')?;
    let (core, _) = rest.rsplit_once('.')?;
    match core.split_once('-').and_then(|(_, t)| t.split_once('-')) {
        Some((_, n)) => Some((core[..core.len() - n.len() - 1].to_string(), n.parse().ok()?)),
        None => Some((core.to_string(), 0)),
    }
}

/// Copies `path` into the backup folder (or records that it was absent).
/// Returns the backup and its stamp. Given `core` (the stamp of a backup
/// just made), the backup joins that one: one undo restores both.
fn backup(p: &Paths, stem: &str, path: &Path, core: Option<&str>) -> Result<(PathBuf, String), String> {
    let dir = p.backups_for(stem)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let at = |e: &dyn std::fmt::Display, f: &Path| format!("{}: {e}", f.display());
    let (ext, body) = match std::fs::read(path) {
        Ok(b) => ("json", b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ("absent", path.display().to_string().into_bytes()),
        Err(e) => return Err(at(&e, path)),
    };
    if let Some(c) = core {
        // One change that writes a file twice (a migration, then the
        // registration) keeps the first backup: undo restores the file as
        // it was before the whole change.
        let first = ["json", "absent"].iter().map(|x| dir.join(format!("{stem}.{c}.{x}"))).find(|f| f.exists());
        if let Some(f) = first {
            return Ok((f, c.to_string()));
        }
        let dst = dir.join(format!("{stem}.{c}.{ext}"));
        crate::fsio::create_exclusive(&dst, &body).map_err(|e| at(&e, &dst))?;
        return Ok((dst, c.to_string()));
    }
    let s = stamp();
    // A stamp and number are unique across both kinds and both folders, so
    // undo orders every backup by them alone. The name is claimed atomically
    // with its contents: the two files have separate locks, so another
    // process may be backing up in this very second. The check after the
    // claim catches a claim of the same number in the other folder: of two
    // such claims at least one sees the other, and moves on.
    let folders: Vec<PathBuf> = STEMS.iter().filter_map(|k| p.backups_for(k).ok()).collect();
    let holder = |core: &str, me: Option<&Path>| {
        folders.iter().any(|f| {
            STEMS.iter().any(|k| {
                ["json", "absent"].iter().any(|x| {
                    let other = f.join(format!("{k}.{core}.{x}"));
                    me != Some(other.as_path()) && other.exists()
                })
            })
        })
    };
    let mut n = 0;
    loop {
        let core = if n == 0 { s.clone() } else { format!("{s}-{n}") };
        n += 1;
        if holder(&core, None) {
            continue;
        }
        let dst = dir.join(format!("{stem}.{core}.{ext}"));
        match crate::fsio::create_exclusive(&dst, &body) {
            Ok(()) if holder(&core, Some(&dst)) => {
                let _ = std::fs::remove_file(&dst);
            }
            Ok(()) => return Ok((dst, core)),
            Err(crate::error::Error::AlreadyExists(_)) => {}
            Err(e) => return Err(at(&e, &dst)),
        }
    }
}

/// The kinds of backup: Claude Code's settings, atlas's configuration, a
/// project file atlas created (always an `.absent` backup, holding the
/// file's path, so undo removes it), and a vault folder atlas registered
/// (an `.absent` record; see [`undo_vault`]).
const STEMS: [&str; 5] = ["settings", "atlas", "project", "vault", "claudemd"];

/// The newest backups of any kind, each kind read from its own folder:
/// one, or the files of one change that wrote several (they share a stamp).
fn newest_backups(p: &Paths) -> Vec<PathBuf> {
    let mut all: Vec<((String, u32), PathBuf)> = Vec::new();
    // atlas and project backups share a folder; the name prefix keeps them apart.
    for stem in STEMS {
        let Ok(rd) = p.backups_for(stem).and_then(|d| std::fs::read_dir(d).map_err(|e| e.to_string())) else { continue };
        let prefix = format!("{stem}.");
        all.extend(rd.filter_map(|e| e.ok()).filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let ok = name.starts_with(&prefix) && (name.ends_with(".json") || name.ends_with(".absent"));
            ok.then(|| backup_key(&name).map(|k| (k, e.path()))).flatten()
        }));
    }
    all.sort();
    let Some(newest) = all.last().map(|x| x.0.clone()) else { return Vec::new() };
    all.into_iter().filter(|x| x.0 == newest).map(|x| x.1).collect()
}

/// Restores one backup (and consumes it): a copy is written back, an
/// `.absent` record removes the file atlas created.
fn restore(p: &Locked, io: &mut Io, b: &Path) -> Result<(), String> {
    let name = b.file_name().unwrap_or_default().to_string_lossy().to_string();
    if name.starts_with("vault.") {
        let record = std::fs::read_to_string(b).map_err(|e| format!("{}: {e}", b.display()))?;
        say!(io, "{}", undo_vault(&record));
        let _ = std::fs::remove_file(b);
        return Ok(());
    }
    if name.starts_with("claudemd.") {
        say!(io, "{}", undo_compact_md(b));
        return Ok(());
    }
    let target = if name.starts_with("project.") {
        // A project file atlas created: the backup holds its path.
        PathBuf::from(std::fs::read_to_string(b).map_err(|e| format!("{}: {e}", b.display()))?.trim())
    } else if name.starts_with("atlas.") {
        p.atlas_config.clone().ok_or("the backup is of atlas's configuration, and none is in use")?
    } else {
        p.settings()
    };
    if name.ends_with(".absent") {
        if target.exists() {
            std::fs::remove_file(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        }
        let _ = std::fs::remove_file(b);
        say!(io, "removed {} (it did not exist before atlas wrote it)", target.display());
    } else {
        let text = std::fs::read(b).map_err(|e| format!("{}: {e}", b.display()))?;
        crate::fsio::replace_atomic(&target, &text).map_err(|e| format!("{}: {e}", target.display()))?;
        let _ = std::fs::remove_file(b);
        say!(io, "restored {} from {}", target.display(), b.display());
    }
    Ok(())
}

// ----------------------------------------------------------------- writing

/// Writes one file: backup, then an atomic replace.
fn commit(p: &Locked, io: &mut Io, stem: &str, path: &Path, text: &str, label: &str) -> Result<(), String> {
    commit_in(p, io, stem, path, text, label, None).map(|_| ())
}

/// [`commit`], the backup joining the change stamped `core` when given.
/// Returns the stamp.
fn commit_in(p: &Locked, io: &mut Io, stem: &str, path: &Path, text: &str, label: &str, core: Option<&str>) -> Result<String, String> {
    let (saved, core) = backup(p, stem, path, core)?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    crate::fsio::replace_atomic(path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    note_label(&saved, label);
    say!(io, "{label}; backup {}", saved.display());
    Ok(core)
}

/// Records what a change was, beside its backup, for `atlas settings undo`
/// to show (`STEM.STAMP.label`, one line per file the change wrote).
fn note_label(saved: &Path, label: &str) {
    let name = saved.file_name().unwrap_or_default().to_string_lossy().to_string();
    let Some((head, _)) = name.rsplit_once('.') else { return };
    let f = saved.with_file_name(format!("{head}.label"));
    let mut text = std::fs::read_to_string(&f).unwrap_or_default();
    if !text.lines().any(|l| l == label) {
        text.push_str(label);
        text.push('\n');
        let _ = std::fs::write(&f, text);
    }
}

/// The label file of a backup (see [`note_label`]).
fn label_file(b: &Path) -> PathBuf {
    let name = b.file_name().unwrap_or_default().to_string_lossy().to_string();
    let head = name.rsplit_once('.').map_or(name.as_str(), |(h, _)| h).to_string();
    b.with_file_name(format!("{head}.label"))
}

/// What `atlas settings undo` would revert, for the user to confirm: the
/// change's time and what it was, then one line per file.
fn undo_summary(p: &Paths, all: &[PathBuf]) -> String {
    let core = all.first().and_then(|b| b.file_name()).and_then(|n| backup_key(&n.to_string_lossy())).map(|k| k.0).unwrap_or_default();
    let d: String = core.chars().filter(char::is_ascii_digit).collect();
    let when = if d.len() >= 14 { format!("{}-{}-{} {}:{}:{} UTC", &d[..4], &d[4..6], &d[6..8], &d[8..10], &d[10..12], &d[12..14]) } else { core.clone() };
    let mut labels: Vec<String> = Vec::new();
    for b in all {
        for l in std::fs::read_to_string(label_file(b)).unwrap_or_default().lines() {
            if !labels.iter().any(|x| x == l) {
                labels.push(l.to_string());
            }
        }
    }
    let what = if labels.is_empty() { String::new() } else { format!(": {}", labels.join("; ")) };
    let mut out = format!("atlas settings undo reverts the newest atlas change, made {when}{what}\n");
    for b in all {
        let name = b.file_name().unwrap_or_default().to_string_lossy().to_string();
        let line = if name.starts_with("vault.") {
            let folder = std::fs::read_to_string(b).unwrap_or_default().lines().next().unwrap_or("").trim().to_string();
            format!("unregister vault {folder} (its folder is removed only if atlas made it and it holds no notes)")
        } else if name.starts_with("claudemd.") {
            let note = std::fs::read_to_string(format!("{}.path", b.display())).ok().or_else(|| std::fs::read_to_string(b).ok()).unwrap_or_default();
            format!("remove atlas's Compact instructions from {}", note.trim())
        } else {
            let target = if name.starts_with("project.") {
                PathBuf::from(std::fs::read_to_string(b).unwrap_or_default().trim())
            } else if name.starts_with("atlas.") {
                p.atlas_config.clone().unwrap_or_default()
            } else {
                p.settings()
            };
            if name.ends_with(".absent") {
                format!("remove {} (it did not exist before)", target.display())
            } else {
                let now = std::fs::read_to_string(&target).unwrap_or_default();
                let then = std::fs::read_to_string(b).unwrap_or_default();
                let (gone, back) = line_changes(&now, &then);
                format!("restore {} to its earlier copy ({gone} lines removed, {back} lines put back)", target.display())
            }
        };
        out.push_str("  - ");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Lines of `now` missing from `then`, and lines of `then` missing from
/// `now` (each line counted as often as it occurs).
fn line_changes(now: &str, then: &str) -> (usize, usize) {
    let mut count: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for l in now.lines() {
        *count.entry(l).or_default() += 1;
    }
    for l in then.lines() {
        *count.entry(l).or_default() -= 1;
    }
    let gone = count.values().filter(|v| **v > 0).map(|v| *v as usize).sum();
    let back = count.values().filter(|v| **v < 0).map(|v| (-*v) as usize).sum();
    (gone, back)
}

fn manual(s: &Item, v: Option<&Value>) -> String {
    match (s.kind, v) {
        (Kind::Setting, Some(v)) => format!("set \"{}\": {v} at the top level", s.key),
        (Kind::Setting, None) => format!("remove \"{}\" at the top level", s.key),
        (Kind::Env, Some(v)) => format!("set \"{}\": {v} inside \"env\"", s.key),
        (Kind::Env, None) => format!("remove \"{}\" inside \"env\"", s.key),
        (Kind::Atlas, Some(v)) => format!("set \"{}\" to {v}", s.key),
        (Kind::Atlas, None) => format!("remove \"{}\"", s.key),
    }
}

/// Applies changes to Claude Code's settings.json in one write.
fn write_claude(p: &Locked, io: &mut Io, changes: &[(&Item, Option<Value>)], label: &str) -> Result<bool, String> {
    let doc = load(&p.settings())?;
    let mut v = doc.value.clone();
    for (s, val) in changes {
        let m = v.as_object_mut().ok_or("settings.json is not an object")?;
        match (s.kind, val) {
            (Kind::Setting, Some(x)) => {
                m.insert(s.key.clone(), x.clone());
            }
            (Kind::Setting, None) => {
                m.remove(&s.key);
            }
            (Kind::Env, Some(x)) => {
                let e = m.entry_or_null("env");
                if !e.is_object() {
                    *e = Value::Object(Map::new());
                }
                if let Some(o) = e.as_object_mut() {
                    o.insert(s.key.clone(), x.clone());
                }
            }
            (Kind::Env, None) => {
                if let Some(o) = m.get_mut("env").and_then(Value::as_object_mut) {
                    o.remove(&s.key);
                    if o.is_empty() {
                        m.remove("env");
                    }
                }
            }
            (Kind::Atlas, _) => return Err("internal: an atlas switch in settings.json".into()),
        }
    }
    let Some(text) = reencode(&doc, &v) else {
        say!(io, "atlas will not rewrite {}: re-encoding it would change more than this setting.", doc.path.display());
        for (s, val) in changes {
            say!(io, "  make this edit by hand: {}", manual(s, val.as_ref()));
        }
        return Ok(false);
    };
    let old = doc.text.clone().unwrap_or_default();
    let _ = write!(io.out, "{}", diff(&doc.path.display().to_string(), &old, &text));
    commit(p, io, "settings", &doc.path, &text, label)?;
    say!(io, "Settings read at session start take effect in the next session.");
    Ok(true)
}

/// Sets (or removes) one atlas switch in atlas's configuration.
fn write_atlas(p: &Locked, io: &mut Io, s: &Item, val: Option<&Value>, label: &str) -> Result<bool, String> {
    let keys: Vec<&str> = s.key.split('.').collect();
    edit_config(p, io, &keys, val, label, &manual(s, val))
}

/// Sets (or removes, with `None`) the value at `keys` in atlas's
/// configuration, keeping every other byte: shows the diff, backs the file
/// up (so `atlas settings undo` restores it) and replaces it atomically.
/// Returns false, having printed `manual_edit`, when the text cannot be
/// changed that way.
///
/// # Errors
///
/// A message when the file cannot be read, backed up or written.
pub fn edit_config(p: &Locked, io: &mut Io, keys: &[&str], val: Option<&Value>, label: &str, manual_edit: &str) -> Result<bool, String> {
    edit_config_in(p, io, keys, val, label, manual_edit, None)
}

/// [`edit_config`], the backup joining the change stamped `core` when
/// given (one undo restores both).
fn edit_config_in(p: &Locked, io: &mut Io, keys: &[&str], val: Option<&Value>, label: &str, manual_edit: &str, core: Option<&str>) -> Result<bool, String> {
    Ok(!matches!(edit_config_many(p, io, &[(keys, val.cloned())], label, manual_edit, core)?, Written::Refused))
}

/// What [`edit_config_many`] did.
enum Written {
    /// The text could not be changed byte-exactly; the manual edit was shown.
    Refused,
    /// The file already held every value.
    Unchanged,
    /// Written, under this backup stamp.
    Stamp(String),
}

/// atlas's configuration file, its text, and the text after `edits`
/// spliced in byte-exactly; `None` when that cannot be done.
///
/// # Errors
///
/// A message when there is no configuration file or it cannot be read.
fn config_after(p: &Paths, edits: &[(&[&str], Option<Value>)]) -> Result<Option<(PathBuf, String, String)>, String> {
    let path = p.atlas_config.clone().ok_or("atlas's switches live in its configuration file, and none is in use (--vault flags only)")?;
    let doc = load(&path)?;
    let text = doc.text.clone().unwrap_or_else(|| "{}\n".into());
    let mut want = doc.value.clone();
    let mut new = Some(text.clone());
    for (keys, val) in edits {
        set_path(&mut want, keys, val.as_ref());
        new = new.and_then(|t| splice(&t, keys, val.as_ref()));
    }
    Ok(new.filter(|t| crate::json::parse(t).ok().as_ref() == Some(&want)).map(|n| (path, text, n)))
}

/// Several [`edit_config`] changes in one write: each is spliced into the
/// text in turn, so one backup (and one `atlas settings undo`) covers them
/// all.
fn edit_config_many(p: &Locked, io: &mut Io, edits: &[(&[&str], Option<Value>)], label: &str, manual_edit: &str, core: Option<&str>) -> Result<Written, String> {
    let Some((path, text, new)) = config_after(p, edits)? else {
        let path = p.atlas_config.clone().unwrap_or_default();
        say!(io, "atlas will not rewrite {}: the change could not be made without touching other text.", path.display());
        say!(io, "  make this edit by hand: {manual_edit}");
        return Ok(Written::Refused);
    };
    if new == text {
        say!(io, "nothing to change");
        return Ok(Written::Unchanged);
    }
    let _ = write!(io.out, "{}", diff(&path.display().to_string(), &text, &new));
    let stamp = commit_in(p, io, "atlas", &path, &new, label, core)?;
    say!(io, "atlas reads its configuration at start: restart Claude Code (or reconnect atlas with /mcp).");
    Ok(Written::Stamp(stamp))
}

// ---------------------------------------------------------------- checking

/// The value as stored, or a message. Choices and ranges come from the
/// manifest.
fn check(s: &Item, raw: &str) -> Result<Value, String> {
    match s.typ.as_str() {
        "bool" => {
            return match raw.to_ascii_lowercase().as_str() {
                "on" | "true" | "yes" | "1" => Ok(Value::Bool(true)),
                "off" | "false" | "no" | "0" => Ok(Value::Bool(false)),
                _ => Err(format!("{}: choose on or off", s.id)),
            }
        }
        "version" => {
            let v = raw.strip_prefix('v').unwrap_or(raw);
            let ok = v.split('.').count() == 3 && v.split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()));
            return if ok { Ok(Value::String(v.to_string())) } else { Err(format!("{}: needs a version like 0.2.0", s.id)) };
        }
        _ => {}
    }
    if !s.choices.is_empty() && !s.choices.iter().any(|c| c == raw) {
        return Err(format!("{}: choose one of {}", s.id, s.choices.join(", ")));
    }
    if let Some((lo, hi, integer)) = s.range {
        let n: f64 = if integer { raw.parse::<i64>().map(|n| n as f64).map_err(|_| format!("{}: needs a whole number", s.id))? } else { raw.parse().map_err(|_| format!("{}: needs a number", s.id))? };
        if n < lo || n > hi {
            return Err(format!("{}: needs {lo} to {hi}", s.id));
        }
        let text = if integer { format!("{}", n as i64) } else { format!("{n}") };
        // Environment values are strings; a settings key keeps the number.
        return Ok(if s.kind == Kind::Env { Value::String(text) } else { crate::json::parse(&text).unwrap_or(Value::String(text)) });
    }
    Ok(Value::String(raw.to_string()))
}

// --------------------------------------------------------------- commands

fn atlas_config_value(p: &Paths) -> Value {
    p.atlas_config.as_deref().and_then(|c| load(c).ok()).map(|d| d.value).unwrap_or(Value::Object(Map::new()))
}

/// Whether the experimental compaction features are on (`compact.enabled`
/// in atlas's configuration; default off).
pub fn compaction_on(p: &Paths) -> bool {
    atlas_config_value(p).pointer("/compact/enabled").and_then(Value::as_bool) == Some(true)
}

/// What `atlas settings compact-md` says while compaction is off.
pub const COMPACTION_OFF: &str = "The Compact instructions go with atlas's experimental compaction features, which are off. To try them, set \"compact\": {\"enabled\": true} in atlas's configuration, then run this again. Nothing was changed.";

/// atlas's configuration as the switches read it: the file, plus each flag
/// switch's state under its own key (`routing: false` when routing is off).
fn atlas_value(p: &Paths) -> Value {
    let mut v = atlas_config_value(p);
    if let Some(dir) = p.flag_dir() {
        let lean = v.get("lean").cloned();
        if !crate::hook_rewrite::routing_on_in(&dir, lean.as_ref()) {
            if let Some(m) = v.as_object_mut() {
                m.insert("routing".into(), Value::Bool(false));
            }
        }
    }
    v
}

/// The lock a change to `s` needs: the file it lives in. A flag switch lives
/// in neither (its own flag file is written whole), so it needs none.
fn lock_for<'a>(p: &'a Paths, s: &Item) -> Result<Locked<'a>, String> {
    match s.kind {
        _ if s.flag => Ok(p.unlocked()),
        Kind::Atlas => p.lock_config(),
        Kind::Setting | Kind::Env => p.lock_settings(),
    }
}

fn find<'a>(items: &'a [Item], id: Option<&String>) -> Result<&'a Item, String> {
    let ids: Vec<&str> = items.iter().map(|s| s.id.as_str()).collect();
    match id {
        Some(i) => items.iter().find(|s| s.id == *i).ok_or_else(|| format!("unknown id {i:?}; ids: {}", ids.join(", "))),
        None => Err(format!("name a setting; ids: {}", ids.join(", "))),
    }
}

/// The usage of `atlas settings`.
pub const HELP: &str = "usage: atlas settings list | set ID VALUE | off ID | undo [--yes] | offer [--yes ID... | --no] | orchestrate [--yes OPTIONS | --no] | compact-md [DIR] [--yes] | add-project --no DIR | add-project [--vault | --vault-at FOLDER | --new [--name NAME] [--vault-at FOLDER | --no-vault] | --to PROJECT] [--dry-run] (DIR | --yes) | migrate-projects NAME";

/// Runs one `atlas settings` command against `p`, talking through `io`.
///
/// # Errors
///
/// A message for the user (an unknown id, a bad value, an unreadable file).
pub fn run(args: &[String], p: &Paths, io: &mut Io) -> Result<ExitCode, String> {
    let items = manifest()?;
    let word = args.first().map(String::as_str).unwrap_or("list");
    // A no reads nothing from settings.json: a settings.json that does not parse must not stop
    // one being recorded, or the question it answers would return for good.
    let answers_no = matches!(word, "offer" | "orchestrate" | "add-project") && args.iter().any(|a| a == "--no");
    let claude = if answers_no { Value::Null } else { load(&p.settings())?.value };
    let atlas = atlas_value(p);
    match word {
        "list" => {
            say!(io, "Claude Code settings: {}", p.settings().display());
            match &p.atlas_config {
                Some(c) => say!(io, "atlas configuration: {}", c.display()),
                None => say!(io, "atlas configuration: none (--vault flags only)"),
            }
            for s in &items {
                say!(io, "{}", describe(s, current(s, &claude, &atlas)));
            }
            Ok(ExitCode::SUCCESS)
        }
        "set" => {
            let s = find(&items, args.get(1))?;
            let raw = args.get(2).ok_or("set needs ID VALUE")?;
            let v = check(s, raw)?;
            // `false` is not off for this one: only its own word reaches it.
            if s.id == "memory-rules" && v == Value::Bool(false) && !raw.eq_ignore_ascii_case("false") {
                return Err(format!("{}: {raw:?} is not taken here. To go back to the default: atlas settings off {}. The value false also stops what the configuration names by hand: atlas settings set {} false", s.id, s.id, s.id));
            }
            // By the value, so every spelling `check` accepts is warned about.
            let word = match &v {
                Value::String(t) => t.clone(),
                other => other.to_string(),
            };
            if let Some((_, w)) = s.warn.iter().find(|(k, _)| *k == word) {
                say!(io, "warning: {w}");
            }
            // Read again under the lock: `claude` and `atlas` above came before it.
            let held = lock_for(p, s)?;
            let (claude, atlas) = (load(&p.settings())?.value, atlas_value(p));
            let have = current(s, &claude, &atlas);
            // A flag switch has no stored "on": its default is the value.
            if have == Some(&v) || (s.flag && effective(s, have) == Some(&v)) {
                say!(io, "[{}] already {}", s.id, show(Some(&v)));
                return Ok(ExitCode::SUCCESS);
            }
            if s.flag {
                let ok = flag_write(p, io, s, v == Value::Bool(true))?;
                return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
            }
            let label = format!("[{}] set to {}", s.id, show(Some(&v)));
            let ok = if s.kind == Kind::Atlas { write_atlas(&held, io, s, Some(&v), &label)? } else { write_claude(&held, io, &[(s, Some(v))], &label)? };
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        "off" => {
            let s = find(&items, args.get(1))?;
            let held = lock_for(p, s)?;
            let (claude, atlas) = (load(&p.settings())?.value, atlas_value(p));
            let have = current(s, &claude, &atlas);
            // An on-by-default switch is turned off; anything else is removed.
            let target = s.default_on().then_some(Value::Bool(false));
            if have == target.as_ref() {
                say!(io, "[{}] already off; nothing to change", s.id);
                return Ok(ExitCode::SUCCESS);
            }
            if s.flag {
                let ok = flag_write(p, io, s, false)?;
                return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
            }
            let note = if s.default_note.is_empty() { String::new() } else { format!(" ({})", s.default_note) };
            let label = if target.is_some() { format!("[{}] turned off", s.id) } else { format!("[{}] removed{note}", s.id) };
            let ok = if s.kind == Kind::Atlas { write_atlas(&held, io, s, target.as_ref(), &label)? } else { write_claude(&held, io, &[(s, None)], &label)? };
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        "undo" => {
            let yes = args.iter().skip(1).any(|a| a == "--yes");
            // With --yes nothing is asked, so the lock comes first. A question
            // is never asked under it: the list is read again after the answer.
            let mut held = if yes { Some(p.lock_both()?) } else { None };
            // The newest change: one file, or every file one command wrote.
            let all = newest_backups(p);
            if all.is_empty() {
                return Err("no atlas backup to restore".into());
            }
            // Shown first and confirmed: the newest backup can be older and
            // unrelated to what the user just ran (a command that changed
            // nothing makes none).
            let _ = write!(io.out, "{}", undo_summary(p, &all));
            if !yes {
                if !io.interactive {
                    say!(io, "Nothing reverted: there is no terminal here to confirm in. Run `atlas settings undo --yes` to revert it.");
                    return Ok(ExitCode::FAILURE);
                }
                let _ = write!(io.out, "Revert it? [y/N] ");
                let _ = io.out.flush();
                let mut line = String::new();
                let _ = io.input.read_line(&mut line);
                if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    say!(io, "nothing reverted");
                    return Ok(ExitCode::SUCCESS);
                }
            }
            let held = match held.take() {
                Some(h) => h,
                None => p.lock_both()?,
            };
            if newest_backups(p) != all {
                say!(io, "Nothing reverted: another atlas change came after the one shown. Run `atlas settings undo` again to see the newest.");
                return Ok(ExitCode::FAILURE);
            }
            for b in &all {
                let _ = std::fs::remove_file(label_file(b));
                restore(&held, io, b)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        "offer" if args.iter().any(|a| a == "--no") => {
            if args.len() != 2 {
                return Err(format!("offer: --no takes nothing else\n{HELP}"));
            }
            if !mark_answered(p, &items) {
                return Err("offer: could not record the answer".into());
            }
            say!(io, "No setting was changed, and atlas will not ask about these again; `atlas settings offer` asks any time.");
            Ok(ExitCode::SUCCESS)
        }
        "offer" => offer(p, io, &items, &claude, &atlas, args.iter().position(|a| a == "--yes").map(|i| &args[i + 1..])),
        "orchestrate" => orchestrated::command(p, io, &items, &atlas, &args[1..]),
        "add-project" => {
            let mut new = false;
            let mut to: Option<String> = None;
            let mut name: Option<String> = None;
            let mut dir: Option<PathBuf> = None;
            let mut vault: Option<VaultAt> = None;
            let mut no_vault = false;
            let mut dry_run = false;
            let mut yes = false;
            let mut no = false;
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--no" => no = true,
                    "--vault" => vault = Some(VaultAt::Default),
                    "--vault-at" => vault = Some(VaultAt::Folder(PathBuf::from(it.next().ok_or("--vault-at needs a folder")?))),
                    "--no-vault" => no_vault = true,
                    "--new" => new = true,
                    "--to" => to = Some(it.next().ok_or("--to needs a project name")?.clone()),
                    "--name" => name = Some(it.next().ok_or("--name needs a project name")?.clone()),
                    "--dry-run" => dry_run = true,
                    "--yes" => yes = true,
                    d if !d.starts_with("--") && dir.is_none() => dir = Some(PathBuf::from(d)),
                    other => return Err(format!("add-project: unknown argument {other:?}\n{HELP}")),
                }
            }
            // The answer no to the session's question: recorded, nothing else.
            if no {
                if new || to.is_some() || name.is_some() || vault.is_some() || no_vault || dry_run || yes {
                    return Err("add-project: --no takes a folder and nothing else".into());
                }
                let dir = dir.ok_or("add-project: --no needs a folder")?;
                let key = project_key(&dir);
                if !mark_project(p, &key) {
                    return Err(format!("add-project: could not record the answer for {key}"));
                }
                say!(io, "atlas will not ask about {key} again; `atlas settings add-project \"{key}\"` adds it any time.");
                return Ok(ExitCode::SUCCESS);
            }
            // With no folder named and no --yes, the command only shows what
            // it would do: a bare `add-project` is a question, not a change.
            let bare = dir.is_none() && !yes;
            let dir = match dir {
                Some(d) => d,
                None => std::env::current_dir().map_err(|e| format!("the current folder: {e}"))?,
            };
            if bare {
                say!(
                    io,
                    "add-project with no folder named writes nothing; below is what it would do for the current folder ({}). To make the change, name the folder (`atlas settings add-project .` for this one) or pass --yes.\n{HELP}\n",
                    project_key(&dir)
                );
            }
            io.dry_run = dry_run || bare;
            if (vault.is_some() || no_vault) && to.is_some() {
                return Err("add-project: --vault, --vault-at and --no-vault go with a single folder or --new, not with --to".into());
            }
            if no_vault && (vault.is_some() || !new) {
                return Err("add-project: --no-vault goes with --new alone (without --vault, a single folder gets no vault)".into());
            }
            let ok = if new {
                // A new project gets its vault unless told otherwise.
                let at = if no_vault { None } else { Some(vault.unwrap_or(VaultAt::Default)) };
                add_project_new(p, io, &dir, name.as_deref(), at.as_ref())?
            } else if let Some(t) = to {
                add_to_project(p, io, &dir, &t)?
            } else {
                add_project_vault(p, io, &dir, vault.as_ref())?
            };
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        "compact-md" => {
            if !compaction_on(p) {
                say!(io, "{COMPACTION_OFF}");
                return Ok(ExitCode::FAILURE);
            }
            let yes = args.iter().any(|a| a == "--yes");
            let dir = match args.iter().skip(1).find(|a| !a.starts_with("--")) {
                Some(d) => PathBuf::from(d),
                None => std::env::current_dir().map_err(|e| format!("the current folder: {e}"))?,
            };
            Ok(if compact_md(p, io, &dir, yes)? { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        "migrate-projects" => {
            let name = args.get(1).ok_or("migrate-projects needs the project's name")?;
            Ok(if migrate_projects(p, io, name)? { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        "-h" | "--help" | "help" => {
            say!(io, "{HELP}");
            Ok(ExitCode::SUCCESS)
        }
        w => Err(format!("unknown settings command {w:?}\n{HELP}")),
    }
}

fn offer(p: &Paths, io: &mut Io, items: &[Item], claude: &Value, atlas: &Value, yes: Option<&[String]>) -> Result<ExitCode, String> {
    if let Some(ids) = yes {
        if ids.is_empty() {
            return Err(format!("offer --yes names the settings to turn on; `offer --no` declines them all\n{HELP}"));
        }
        for id in ids {
            if find(items, Some(id))?.kind == Kind::Atlas {
                say!(io, "[{id}] is an atlas switch, not changed here: `atlas settings set {id} VALUE`");
            }
        }
    }
    let mut todo: Vec<(&Item, Option<Value>)> = Vec::new();
    // Settings this Claude Code cannot take: nobody was asked, or a yes could not land.
    let mut too_new: Vec<&str> = Vec::new();
    for s in items.iter().filter(|s| s.kind != Kind::Atlas) {
        let have = current(s, claude, atlas);
        if applied(s, have) {
            say!(io, "[{}] already applied", s.id);
            continue;
        }
        if let (Some(need), Some(have)) = (s.min_version, io.version) {
            if have < need {
                say!(io, "[{}] skipped: needs Claude Code {}.{}.{} or newer", s.id, need.0, need.1, need.2);
                too_new.push(&s.id);
                continue;
            }
        }
        match yes {
            Some(ids) => {
                if ids.contains(&s.id) {
                    todo.push((s, s.value.clone()));
                }
            }
            None => {
                say!(io, "{}", describe(s, have));
                if io.interactive {
                    let _ = write!(io.out, "  Turn this on? [y/N] ");
                    let _ = io.out.flush();
                    let mut line = String::new();
                    let _ = io.input.read_line(&mut line);
                    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                        todo.push((s, s.value.clone()));
                    }
                }
            }
        }
    }
    if yes.is_none() {
        for s in items.iter().filter(|s| s.default_on()) {
            let state = show(effective(s, current(s, claude, atlas)));
            // A build from source never replaces itself, whatever the
            // setting says: it is not shown as on.
            let state = if s.id == "update" && !crate::update::release_build() { format!("{state} in the configuration, but off here: a build from source never updates itself") } else { state };
            say!(io, "\n[{}] {} ({state}; on by default)\n  What: {}\n  Cost: {}\n  Turn off: atlas settings off {}", s.id, s.title, s.what, s.cost, s.id);
        }
    }
    // Whether to work in orchestrated mode, and its options (a terminal
    // asks; without one the options are listed).
    if yes.is_none() {
        if io.interactive {
            orchestrated::prompt(p, io, items, atlas)?;
        } else {
            orchestrated::describe(io, atlas);
        }
    }
    // Recorded as offered only when someone could answer: without a terminal
    // (and without --yes) nothing was asked, so the next terminal run asks.
    if yes.is_none() && io.interactive {
        // A terminal asked the orchestrated question too. Experimental and never
        // asked, so memory rules are not recorded as offered. A setting skipped as
        // too new was not asked; a yes waits for its write, below.
        let waits = |s: &Item| too_new.contains(&s.id.as_str()) || todo.iter().any(|(t, _)| t.id == s.id);
        mark_offered(p, items.iter().filter(|s| s.id != "memory-rules" && !waits(s)).map(|s| s.id.as_str()));
    }
    if yes.is_none() && !io.interactive {
        say!(io, "\nNothing went wrong. No setting was changed because there is no terminal here to answer in (input is not a terminal), and atlas only changes a setting you say yes to. To choose, run `atlas settings offer` in a terminal, apply chosen ones with `atlas settings offer --yes ID...`, or decline them with `atlas settings offer --no`.");
        return Ok(ExitCode::SUCCESS);
    }
    // A named setting that could not land leaves the yes unanswered: the setting
    // stays off, and the question must still be there after an upgrade. An atlas
    // switch, named by hand, answers nothing either.
    let missed = yes.is_some_and(|ids| ids.iter().any(|id| too_new.contains(&id.as_str())));
    let answers = yes.is_some_and(|ids| !missed && !ids.iter().any(|id| items.iter().any(|s| s.id == *id && s.kind == Kind::Atlas)));
    if missed {
        say!(io, "The question stays, at each new session until the upgrade; `atlas settings offer --no` stops it for good.");
    }
    if todo.is_empty() {
        if answers {
            mark_answered(p, items);
        }
        say!(io, "nothing to change");
        return Ok(if missed { ExitCode::FAILURE } else { ExitCode::SUCCESS });
    }
    let label = format!("applied: {}", todo.iter().map(|(s, _)| s.id.as_str()).collect::<Vec<_>>().join(", "));
    let ok = write_claude(&p.lock_settings()?, io, &todo, &label)?;
    // A yes that did not land is no answer: the question stays.
    if ok && answers {
        mark_answered(p, items);
    } else if ok && yes.is_none() {
        mark_offered(p, todo.iter().map(|(s, _)| s.id.as_str()));
    }
    Ok(if ok && !missed { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

// ------------------------------------------------------- the first-run offer

fn offered(p: &Paths) -> Vec<String> {
    let Some(s) = &p.state else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(s) else { return Vec::new() };
    crate::json::parse(&text).ok().and_then(|v| v.get("offered").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())).unwrap_or_default()
}

/// Records ids as offered. Returns false when the state cannot be written.
fn mark_offered<'a>(p: &Paths, ids: impl Iterator<Item = &'a str>) -> bool {
    let Some(s) = &p.state else { return false };
    let mut all = offered(p);
    for id in ids {
        if !all.iter().any(|x| x == id) {
            all.push(id.to_string());
        }
    }
    // The state's other keys (the folders and repos asked about) are kept.
    let mut v = std::fs::read_to_string(s).ok().and_then(|t| crate::json::parse(&t).ok()).filter(Value::is_object).unwrap_or(Value::Object(Map::new()));
    if let Some(o) = v.as_object_mut() {
        o.insert("offered".into(), Value::Array(all.into_iter().map(Value::String).collect()));
    }
    if let Some(d) = s.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    crate::fsio::replace_atomic(s, (crate::json::to_string_pretty(&v) + "\n").as_bytes()).is_ok()
}

/// Records the settings as answered, by a yes to some or a no to all. Not the
/// orchestrated question, which this answer did not touch and which has its own
/// `--yes` and `--no`; not memory rules, experimental and never asked. Returns
/// false when the state cannot be written.
fn mark_answered(p: &Paths, items: &[Item]) -> bool {
    mark_offered(p, items.iter().filter(|s| s.id != "memory-rules" && s.id != orchestrated::ID).map(|s| s.id.as_str()))
}

/// The SessionStart line: the Claude Code settings not yet answered and not
/// applied, and atlas's on-by-default switches not yet explained. The settings
/// are a question, so it returns until `atlas settings offer` answers it (`--yes`
/// or `--no`); the switches are a notice nobody can answer, so they are marked
/// when shown. `None` when there is nothing to say, or when the state cannot be
/// recorded: an answer would be lost and the line would never stop.
pub fn session_start_line(p: &Paths) -> Option<String> {
    let items = manifest().ok()?;
    let seen = offered(p);
    let claude = load(&p.settings()).map(|d| d.value).unwrap_or(Value::Null);
    let fresh = |s: &&Item| !seen.contains(&s.id);
    let offer: Vec<&Item> = items.iter().filter(fresh).filter(|s| s.kind != Kind::Atlas && !applied(s, current(s, &claude, &Value::Null))).collect();
    let defaults: Vec<&Item> = items.iter().filter(fresh).filter(|s| s.default_on()).collect();
    if offer.is_empty() && defaults.is_empty() {
        return None;
    }
    if !can_record(p) || (!defaults.is_empty() && !mark_offered(p, defaults.iter().map(|s| s.id.as_str()))) {
        return None;
    }
    let mut line = String::from("atlas:");
    if !offer.is_empty() {
        let names: Vec<&str> = offer.iter().map(|s| s.short.as_str()).collect();
        let n = offer.len();
        line.push_str(&format!(
            " {n} Claude Code setting{} {} offered ({}); ask the user, then run `atlas settings offer --yes ID` or `offer --no`; asked again next session.",
            if n == 1 { "" } else { "s" },
            if n == 1 { "is" } else { "are" },
            names.join(", ")
        ));
    }
    if !defaults.is_empty() {
        let names: Vec<&str> = defaults.iter().map(|s| s.id.as_str()).collect();
        line.push_str(&format!(" On by default: {} (`atlas settings off ID`).", names.join(", ")));
    }
    Some(line)
}

// ------------------------------------------- the Compact instructions block

/// The first line of the block atlas adds to a project's `CLAUDE.md`.
pub const COMPACT_BEGIN: &str = "<!-- atlas:compact-instructions";
/// Its last line.
pub const COMPACT_END: &str = "<!-- /atlas:compact-instructions -->";

/// The instructions for Claude Code's compaction summary: keys and deltas.
/// The PreCompact hook returns this text as it stands, so it holds no markers
/// and no heading; [`compact_block`] is the same text for a `CLAUDE.md`.
pub const COMPACT_INSTRUCTIONS: &str = "When compacting, write keys and deltas, not restated content. The summary's hard limit is 3,000 characters (about 1,250 tokens), under half of a usual summary; a longer one has failed, so count characters, not words: prose for rulings, decisions, state and next steps; keys for detail.
- After compaction atlas injects a card that names every part of this segment by a key one recall returns (turn:...-...@hash groups, older segments by compact:...@hash links), plus lanes, open questions, owed steps, rulings and files touched. Do not repeat what it lists, and never restate tool output: its exact text is one recall away.
- Where a card key or a record key already holds something (a discussion, a plan, a file's content, an earlier step's detail), write that key in place of the content, never both.
- Rulings and decisions: one line each, in the user's own few words, with the record key that holds it (key@hash) when there is one.
- State: what is done, what is in flight, what failed, each with its key (a journal key, a src:path:a-b@hash ref, a file:line, a commit or PR number).
- Files: one line per file, its path, line range and what changed. No code, except the few lines the next step will edit.
- Keep exact only what the next step needs inline: an error line, a number, a command.
- End with the current step and the next action.";

/// The block `atlas settings compact-md` adds to a project's `CLAUDE.md`. In
/// `CLAUDE.md` alone its effect is unproven (`docs/design/compaction.md`,
/// section 1).
pub fn compact_block() -> String {
    format!("{COMPACT_BEGIN} (added by `atlas settings compact-md`; `atlas settings undo` removes it) -->
## Compact instructions

{COMPACT_INSTRUCTIONS}
{COMPACT_END}
")
}

/// Whether a `CLAUDE.md` text already holds the block.
fn has_compact_block(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with(COMPACT_BEGIN))
}

/// `atlas settings compact-md [DIR] [--yes]`: shows the Compact instructions
/// block and the change to `DIR/CLAUDE.md`, asks once (or applies with
/// `--yes`), and writes nothing on no. The file is backed up first; `atlas
/// settings undo` removes the block (and the file, when atlas created it).
/// Returns false when the write failed.
///
/// # Errors
///
/// A message when the file cannot be read or written.
pub fn compact_md(p: &Paths, io: &mut Io, dir: &Path, yes: bool) -> Result<bool, String> {
    let path = dir.join("CLAUDE.md");
    let old = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if old.as_deref().is_some_and(has_compact_block) {
        say!(io, "[compact-md] already in {}", path.display());
        return Ok(true);
    }
    let base = old.clone().unwrap_or_default();
    let sep = if base.is_empty() || base.ends_with("\n\n") { "" } else if base.ends_with('\n') { "\n" } else { "\n\n" };
    let new = format!("{base}{sep}{}", compact_block());
    if !yes {
        say!(
            io,
            "\n[compact-md] Compact instructions for {}\n  What: instructions for Claude Code's compaction summary (cite atlas keys, write deltas), beside the checkpoint card atlas injects after a compaction.\n  Cost: about 380 tokens of CLAUDE.md in every session and agent of this project.\n  Measured: the same kind of text passed as the /compact argument moved summary size; in CLAUDE.md alone it showed no reliable effect; a PreCompact hook returning it steered the summary in two one-sample manual tests, and with compact.enabled atlas's hook returns it, so a project with this block gets the text twice and should drop it. The benefit of this block on a bare or automatic /compact is unproven; see docs/design/compaction.md.\n{}",
            path.display(),
            diff(&path.display().to_string(), old.as_deref().unwrap_or(""), &new)
        );
        if !io.interactive {
            say!(io, "Nothing went wrong. CLAUDE.md was not changed because there is no terminal here to answer in (input is not a terminal). To choose, run `atlas settings compact-md` in a terminal, or add it with `atlas settings compact-md --yes`.");
            return Ok(true);
        }
        let _ = write!(io.out, "  Add it? [y/N] ");
        let _ = io.out.flush();
        let mut line = String::new();
        let _ = io.input.read_line(&mut line);
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            say!(io, "nothing written");
            return Ok(true);
        }
    }
    let (saved, _) = backup(p, "claudemd", &path, None)?;
    // The undo record names the file (a copy alone would not).
    let note = PathBuf::from(format!("{}.path", saved.display()));
    std::fs::write(&note, path.display().to_string()).map_err(|e| format!("{}: {e}", note.display()))?;
    note_label(&saved, &format!("compact-md: added the Compact instructions to {}", path.display()));
    crate::fsio::replace_atomic(&path, new.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    say!(io, "[compact-md] added to {}; backup {} (`atlas settings undo` removes the block)", path.display(), saved.display());
    Ok(true)
}

/// `CLAUDE.md` text without atlas's block (and the blank line before it).
pub fn strip_compact_block(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let a = lines.iter().position(|l| l.trim_start().starts_with(COMPACT_BEGIN))?;
    let b = a + lines[a..].iter().position(|l| l.trim() == COMPACT_END)?;
    let mut head = lines[..a].concat();
    if head.ends_with("\n\n") {
        head.pop();
    }
    Some(head + &lines[b + 1..].concat())
}

/// Undoes a `claudemd` backup: removes the block from the file it names,
/// and removes the file when atlas created it and nothing else is left.
fn undo_compact_md(b: &Path) -> String {
    let absent = b.extension().is_some_and(|x| x == "absent");
    let note = PathBuf::from(format!("{}.path", b.display()));
    let path = std::fs::read_to_string(&note).ok().or_else(|| absent.then(|| std::fs::read_to_string(b).ok()).flatten()).map(|s| PathBuf::from(s.trim()));
    let _ = std::fs::remove_file(b);
    let _ = std::fs::remove_file(&note);
    let Some(path) = path else { return "the Compact instructions backup names no file; nothing changed".into() };
    let Ok(text) = std::fs::read_to_string(&path) else { return format!("{} is gone; nothing to remove", path.display()) };
    let Some(rest) = strip_compact_block(&text) else { return format!("{} no longer holds atlas's Compact instructions; nothing removed", path.display()) };
    if absent && rest.trim().is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => format!("removed {} (atlas created it for the Compact instructions)", path.display()),
            Err(e) => format!("{}: {e}", path.display()),
        };
    }
    match crate::fsio::replace_atomic(&path, rest.as_bytes()) {
        Ok(()) => format!("removed the Compact instructions from {}", path.display()),
        Err(e) => format!("{}: {e}", path.display()),
    }
}

// ------------------------------------------------------ the project offer

/// Whether an answer could be recorded in the state file. A question whose answer
/// cannot be would come back every session and its `--no` would fail, so it is
/// not asked; the command still says so when run by hand.
fn can_record(p: &Paths) -> bool {
    let Some(s) = &p.state else { return false };
    if let Some(d) = s.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    crate::fsio::can_replace(s)
}

/// The projects already offered (the state file's `projects`).
fn offered_projects(p: &Paths) -> Vec<String> {
    let Some(s) = &p.state else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(s) else { return Vec::new() };
    crate::json::parse(&text).ok().and_then(|v| v.get("projects").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())).unwrap_or_default()
}

/// Records a project folder as offered, keeping the state's other keys.
fn mark_project(p: &Paths, dir: &str) -> bool {
    let Some(s) = &p.state else { return false };
    let mut v = std::fs::read_to_string(s).ok().and_then(|t| crate::json::parse(&t).ok()).filter(Value::is_object).unwrap_or(Value::Object(Map::new()));
    let mut all = offered_projects(p);
    if !all.iter().any(|x| x == dir) {
        all.push(dir.to_string());
    }
    if let Some(o) = v.as_object_mut() {
        o.insert("projects".into(), Value::Array(all.into_iter().map(Value::String).collect()));
    }
    if let Some(d) = s.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    crate::fsio::replace_atomic(s, (crate::json::to_string_pretty(&v) + "\n").as_bytes()).is_ok()
}

/// A folder as the offer records it: canonical (see the path rule in
/// `crate::project`), `/`-separated, no trailing `/`, in its on-disk case.
/// Compared with [`same_folder`], never as text.
fn project_key(dir: &Path) -> String {
    let s = crate::paths::canonical(dir).to_string_lossy().replace('\\', "/");
    s.trim_end_matches('/').to_string()
}

/// The folders atlas already indexes: every vault, source and code root of
/// the configuration and of each of its projects, and each project's roots,
/// project file folder and repositories (listed path and local checkout).
pub fn configured_roots(c: &crate::config::Config) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    for b in std::iter::once(c).chain(c.user.projects.iter().map(|x| x.body.as_ref())) {
        v.extend(b.vaults.iter().map(|x| x.path.clone()).chain(b.sources.iter().map(|x| x.root.clone())));
        if let Some(code) = &b.code {
            v.extend(code.roots.iter().map(|r| r.root.clone()));
        }
    }
    for p in &c.user.projects {
        v.extend(crate::project::project_roots(p, &c.user.checkouts));
        v.extend(p.file.as_ref().and_then(|f| f.parent()).map(Path::to_path_buf));
        v.extend(p.repos.iter().filter_map(|r| r.path.clone()));
    }
    v
}

/// Whether a folder is worth offering: a project, not a home folder, a
/// drive root or a folder atlas already indexes (or one inside it).
fn offerable(dir: &Path, roots: &[PathBuf]) -> bool {
    let key = project_key(dir);
    if !dir.is_dir() || key.matches('/').count() < 2 || crate::project::too_wide(dir, crate::dirs::home().as_deref()) {
        return false;
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| project_key(Path::new(&h)));
    if home.as_deref().is_some_and(|h| same_folder(&key, h)) {
        return false;
    }
    let k = crate::project::path_key(dir);
    !roots.iter().any(|r| crate::project::key_within(&k, &crate::project::path_key(r)))
}

/// Whether two folders are one: their canonical keys are equal (so a folder
/// recorded before through one link matches it reached through another).
fn same_folder(a: &str, b: &str) -> bool {
    crate::project::path_key(Path::new(a)) == crate::project::path_key(Path::new(b))
}

/// Whether a session has no user to answer a question, from what Claude Code
/// sets for its hooks. `CLAUDE_CODE_SESSION_ATTENDED` is its own word on it, and
/// `CLAUDE_CODE_ENTRYPOINT` covers a version without that: measured on 2.1.319,
/// `1` and `cli` interactive, `0` and `sdk-cli` under `-p`, in a console or on
/// pipes, started from another session or not (`sdk-ts` with `0` from the SDK).
/// The hook's stdin says nothing usable and a terminal test sees the hook's own
/// pipes. With neither variable, as with a hook run by hand, a user is assumed.
pub fn unattended_session(env: impl Fn(&str) -> Option<String>) -> bool {
    match env("CLAUDE_CODE_SESSION_ATTENDED").as_deref() {
        Some("1") => false,
        Some("0") => true,
        _ => matches!(env("CLAUDE_CODE_ENTRYPOINT").as_deref(), Some("sdk-cli" | "sdk-ts" | "sdk-py")),
    }
}

/// The SessionStart line offering the session's folder as a source, until it
/// is answered; `None` when it is not offerable, was answered before, atlas has
/// no configuration file to add it to, or the answer could not be recorded.
///
/// The question folds in the project's vault: yes adds the source and
/// creates the vault at the default folder ([`default_vault`]), another
/// folder puts the vault there, and no records itself with `add-project --no`.
/// Nothing is recorded when the line is made: a line is often never relayed
/// (a task-shaped first prompt, a session with no user), and a question marked
/// asked then was never asked at all.
pub fn project_line(p: &Paths, cwd: &Path, roots: &[PathBuf]) -> Option<String> {
    let cfg = p.atlas_config.as_ref()?;
    if cwd.as_os_str().is_empty() || !offerable(cwd, roots) {
        return None;
    }
    let key = project_key(cwd);
    if offered_projects(p).iter().any(|x| same_folder(x, &key)) || !can_record(p) {
        return None;
    }
    let c = crate::config::Config::load(cfg).ok()?;
    let vault = default_vault(p, &c, &key)?;
    let name = key.rsplit('/').next().unwrap_or(&key);
    let at = project_key(&vault.path);
    // With projects and none for this folder, a yes must name one or it is refused.
    let named = p.project.as_deref().filter(|n| !n.is_empty());
    let (run, which) = if c.user.scoped && matches!(crate::project::active(&c, cwd, named), Ok(None)) {
        let names: Vec<&str> = c.user.projects.iter().map(|x| x.name.as_str()).collect();
        ("atlas --project NAME settings", format!(" NAME is the project the user picks: {}.", names.join(", ")))
    } else {
        ("atlas settings", String::new())
    };
    Some(format!(
        "atlas: this project ({name}) is not indexed. In your first reply, whatever the task, ask the user in one line: \"create a vault for this project at {at}? yes / other folder / no\" (yes is the default; yes and other folder also add the project as a source). On yes run `{run} add-project --vault \"{key}\"`, on other folder `{run} add-project --vault-at \"FOLDER\" \"{key}\"`, on no `atlas settings add-project --no \"{key}\"`;{which} `atlas settings undo` reverses a yes. With no answer, or no user to ask, run nothing: atlas asks again next session."
    ))
}

/// A configuration name for a folder: its last component, lowercased, with
/// anything but letters, digits and `-` as `-`, at most 32 characters (it
/// names a source), made unique against `taken`.
fn project_name(dir: &str, taken: &[String]) -> String {
    let last = dir.rsplit('/').next().unwrap_or("project");
    let mut base: String = last.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    base = base.trim_matches('-').to_string();
    if base.is_empty() {
        base = "project".into();
    }
    let free = |n: &str| !taken.iter().any(|t| t.eq_ignore_ascii_case(n)) && !taken.iter().any(|t| t.eq_ignore_ascii_case(&format!("{n}-code")));
    crate::project::fit_name(&base, crate::project::MAX_SHORT_NAME, free)
}

/// Whether a folder holds code the code index would read: Rust, WGSL, or a
/// language of [`crate::langs`] (a source file or a unit marker such as
/// `go.mod`).
fn has_code(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file()
        || crate::code::walk(dir).is_ok_and(|w| !w.rust.is_empty() || !w.wgsl.is_empty() || !w.source.is_empty() || !crate::code::units(&w).is_empty())
}

// ------------------------------------------------------- the project vault

/// Where `atlas settings add-project` puts the project's vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultAt {
    /// [`default_vault`]: `vaults/<name>` beside atlas's configuration file.
    Default,
    /// A folder the user named (created when absent; relative to the
    /// current folder).
    Folder(PathBuf),
}

/// The note atlas writes into a vault folder it creates.
pub const VAULT_NOTE: &str = "README.md";

/// A vault about to be registered: its configuration name and folder.
struct VaultPlan {
    name: String,
    path: PathBuf,
}

/// Every name in use across vaults, sources and code roots (a project's
/// own included).
fn taken_names(c: &crate::config::Config) -> Vec<String> {
    let mut taken: Vec<String> = Vec::new();
    for b in std::iter::once(c).chain(c.user.projects.iter().map(|x| x.body.as_ref())) {
        taken.extend(b.vaults.iter().map(|v| v.name.clone()).chain(b.sources.iter().map(|s| s.name.clone())));
        if let Some(code) = &b.code {
            taken.extend(code.roots.iter().map(|r| r.name.clone()));
        }
    }
    taken
}

/// The configured vaults' folders: atlas needs their last components
/// distinct, so a new vault's folder name must be free too.
fn vault_folders(c: &crate::config::Config) -> Vec<PathBuf> {
    std::iter::once(c).chain(c.user.projects.iter().map(|x| x.body.as_ref())).flat_map(|b| b.vaults.iter().map(|v| v.path.clone())).collect()
}

fn folder_name(p: &Path) -> String {
    crate::paths::canonical(p).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()
}

/// A vault name for a project folder: `<folder>-notes`, lowercased, short
/// enough for a vault name (32 characters), and free against `taken` and
/// the vault folder names `folders`.
fn vault_name(key: &str, taken: &[String], folders: &[String]) -> String {
    let last = key.rsplit('/').next().unwrap_or("project");
    let mut base: String = last.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    base.truncate(20);
    let mut base = base.trim_matches('-').to_string();
    if base.is_empty() {
        base = "project".into();
    }
    let base = format!("{base}-notes");
    let free = |n: &str| !taken.iter().chain(folders).any(|t| t.eq_ignore_ascii_case(n));
    if free(&base) {
        return base;
    }
    (2..).map(|i| format!("{base}-{i}")).find(|n| free(n)).unwrap_or(base)
}

/// The vault `add-project --vault` makes for an unindexed project folder
/// `key`: named after the folder, in `vaults/<name>` beside atlas's
/// configuration file (where the first run's `notes` vault is), so atlas's
/// writes never land in the project's repository and its Markdown source
/// never indexes the vault twice. `None` without a configuration file.
fn default_vault(p: &Paths, c: &crate::config::Config, key: &str) -> Option<VaultPlan> {
    let base = p.atlas_config.as_deref()?.parent()?.join("vaults");
    let mut taken = taken_names(c);
    // The source add-project adds takes its name first.
    taken.push(project_name(key, &taken));
    let folders: Vec<String> = vault_folders(c).iter().map(|f| folder_name(f)).collect();
    let name = vault_name(key, &taken, &folders);
    Some(VaultPlan { path: base.join(&name), name })
}

/// [`default_vault`] named after `label` alone (no source name to step
/// around): an indexed project's folder key, or a new project's name.
fn default_vault_named(p: &Paths, c: &crate::config::Config, label: &str) -> Option<VaultPlan> {
    let base = p.atlas_config.as_deref()?.parent()?.join("vaults");
    let folders: Vec<String> = vault_folders(c).iter().map(|f| folder_name(f)).collect();
    let name = vault_name(label, &taken_names(c), &folders);
    Some(VaultPlan { path: base.join(&name), name })
}

/// The vault to register for project folder `key`, named after `label`
/// (the folder key, or a project's name), checked: `None` (said) when that
/// folder is a vault already.
///
/// # Errors
///
/// A message when the folder cannot be a vault: a file, the project folder
/// or a folder holding it, inside another vault or source, or sharing its
/// last name with another vault's folder.
fn plan_vault(p: &Paths, io: &mut Io, c: &crate::config::Config, key: &str, label: &str, indexed: bool, at: &VaultAt) -> Result<Option<VaultPlan>, String> {
    if *at == VaultAt::Default {
        // The project's own default vault, registered before: nothing to add
        // (a second one would only take a numbered name).
        if let Some(d) = p.atlas_config.as_deref().and_then(Path::parent) {
            let own = crate::project::path_key(&d.join("vaults").join(vault_name(label, &[], &[])));
            if vault_folders(c).iter().any(|v| crate::project::path_key(v) == own) {
                say!(io, "{} has its vault already; no vault to add", key);
                return Ok(None);
            }
        }
    }
    let plan = match at {
        VaultAt::Default if !indexed => default_vault(p, c, key).ok_or("atlas has no configuration file (--vault flags only)")?,
        // An indexed project: its source keeps its name.
        VaultAt::Default => default_vault_named(p, c, label).ok_or("atlas has no configuration file (--vault flags only)")?,
        VaultAt::Folder(f) => {
            let path = crate::paths::canonical(f);
            let mut taken = taken_names(c);
            if !indexed {
                taken.push(project_name(key, &taken));
            }
            VaultPlan { name: vault_name(label, &taken, &[]), path }
        }
    };
    let shown = project_key(&plan.path);
    if plan.path.is_file() {
        return Err(format!("{shown}: a file, not a folder"));
    }
    let vk = crate::project::path_key(&plan.path);
    if vault_folders(c).iter().any(|v| crate::project::path_key(v) == vk) {
        say!(io, "{shown} is a vault already; no vault to add");
        return Ok(None);
    }
    if crate::project::key_within(&crate::project::path_key(Path::new(key)), &vk) {
        return Err(format!("{shown} is the project folder or holds it; its Markdown is the project's source. Pick a folder inside the project or outside it"));
    }
    let inside = vault_folders(c).into_iter().chain(c.sources.iter().map(|s| s.root.clone())).find(|r| crate::project::key_within(&vk, &crate::project::path_key(r)));
    if let Some(r) = inside {
        return Err(format!("{shown} is inside {}, which atlas indexes already; pick another folder", project_key(&r)));
    }
    let folder = folder_name(&plan.path);
    if vault_folders(c).iter().any(|v| folder_name(v).eq_ignore_ascii_case(&folder)) {
        return Err(format!("another vault's folder is also named {folder:?}, and atlas needs vault folder names distinct; pick a folder with another name"));
    }
    Ok(Some(plan))
}

/// The note a new vault starts with.
fn vault_note(folder: &str, key: &str) -> String {
    format!(
        "# {folder}\n\nThe notes vault atlas made for the project at `{key}`. atlas indexes every note here, so Claude finds them\nthrough recall and search, and atlas writes its discussion threads here too.\n\nOpen this folder in Obsidian with \"Open folder as vault\". Delete this note whenever you like.\n"
    )
}

/// Creates the vault folder and its note when the folder is absent.
/// Returns the note's SHA-256 when it created the folder, `None` when the
/// folder was there (it is used as it is, nothing written into it).
fn create_vault(plan: &VaultPlan, key: &str) -> Result<Option<String>, String> {
    if plan.path.is_dir() {
        return Ok(None);
    }
    std::fs::create_dir_all(&plan.path).map_err(|e| format!("{}: {e}", plan.path.display()))?;
    let note = vault_note(&folder_name(&plan.path), key);
    crate::fsio::create_exclusive(&plan.path.join(VAULT_NOTE), note.as_bytes()).map_err(|e| format!("{}: {e}", plan.path.display()))?;
    Ok(Some(crate::sha256::hex(note.as_bytes())))
}

/// Whether a vault folder atlas created still holds only what atlas wrote:
/// nothing, or its note unchanged.
fn pristine(folder: &Path, note_sha: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(folder) else { return false };
    let entries: Vec<std::fs::DirEntry> = rd.filter_map(Result::ok).collect();
    match entries.as_slice() {
        [] => true,
        [e] => e.file_name() == VAULT_NOTE && e.path().is_file() && std::fs::read(e.path()).is_ok_and(|b| crate::sha256::hex(&b) == note_sha),
        _ => false,
    }
}

/// Undoes a vault folder record (`vault.<stamp>.absent`: the folder, then
/// `created` with the note's SHA-256, or `existed`). The registration
/// itself goes with the configuration backup of the same stamp. A folder
/// atlas created that still holds only its note is removed; anything else
/// is kept, and the message says which: user notes are never deleted.
fn undo_vault(record: &str) -> String {
    let mut l = record.lines();
    let folder = PathBuf::from(l.next().unwrap_or("").trim());
    let created = l.next().map(str::trim) == Some("created");
    let sha = l.next().unwrap_or("").trim();
    let shown = folder.display();
    if !created {
        return format!("vault {shown}: unregistered; the folder was there before atlas registered it and is kept");
    }
    if !folder.exists() {
        return format!("vault {shown}: unregistered; the folder is gone already");
    }
    if pristine(&folder, sha) {
        let _ = std::fs::remove_file(folder.join(VAULT_NOTE));
        if std::fs::remove_dir(&folder).is_ok() {
            return format!("vault {shown}: unregistered, and its folder removed (atlas created it and it held no notes of yours)");
        }
    }
    format!("vault {shown}: unregistered; the folder is kept because it holds notes (delete it by hand if you no longer want them)")
}

/// `atlas settings add-project DIR`: adds a folder to atlas's configuration
/// as a source of its Markdown files and, when it holds code (Rust, WGSL,
/// Go, TypeScript, JavaScript), as a code root; then indexes the source.
/// With `projects`, both go into the project's own entry (see
/// [`target_project`]), never the top level every project reads. The code
/// root is indexed by the server's background indexer when it next starts.
/// With [`Io::dry_run`] the change is shown and nothing is written.
///
/// # Errors
///
/// A message when there is no configuration file, the folder is in no
/// project of a configuration with projects, or the configuration after
/// the change would not run (nothing is written then).
pub fn add_project(p: &Paths, io: &mut Io, dir: &Path) -> Result<bool, String> {
    add_project_vault(p, io, dir, None)
}

/// Finishes a configuration write that registered vault `plan` (folder
/// made first; `created` holds its note's SHA-256 when atlas made it).
/// Refused: a folder atlas just made goes too (nothing half done), and
/// `false`. Written: the `vault.<stamp>.absent` record goes beside the
/// configuration backup of the same stamp, so one undo unregisters the
/// vault and says what it does with the folder; the vault is reported.
fn vault_written(p: &Paths, io: &mut Io, written: &Written, plan: Option<&VaultPlan>, created: Option<&str>) -> Result<bool, String> {
    let Some(v) = plan else { return Ok(!matches!(written, Written::Refused)) };
    match written {
        Written::Refused => {
            if created.is_some_and(|sha| pristine(&v.path, sha)) {
                let _ = std::fs::remove_file(v.path.join(VAULT_NOTE));
                let _ = std::fs::remove_dir(&v.path);
            }
            return Ok(false);
        }
        Written::Stamp(core) => {
            let record = match created {
                Some(sha) => format!("{}\ncreated\n{sha}\n", v.path.display()),
                None => format!("{}\nexisted\n", v.path.display()),
            };
            let r = p.backups_for("vault")?.join(format!("vault.{core}.absent"));
            std::fs::write(&r, record).map_err(|e| format!("{}: {e}", r.display()))?;
        }
        Written::Unchanged => {}
    }
    let how = if created.is_some() { format!("created, with {VAULT_NOTE}") } else { "an existing folder, left as it is".into() };
    say!(io, "vault {}: {} ({how}); open it in Obsidian with \"Open folder as vault\"", v.name, project_key(&v.path));
    Ok(true)
}

/// [`add_project`], with a vault for the project's notes when `vault` is
/// given (`--vault`, `--vault-at FOLDER`): the folder is created when
/// absent, with one note, and registered under `vaults`. The source, the
/// code root and the vault are one configuration write, so one `atlas
/// settings undo` reverts them all; the undo removes a vault folder atlas
/// created only while it holds nothing but that note. An indexed project
/// gets the vault alone.
///
/// # Errors
///
/// As [`add_project`], and a message when the vault folder cannot be one
/// (see [`plan_vault`]) or cannot be created.
pub fn add_project_vault(p: &Paths, io: &mut Io, dir: &Path, vault: Option<&VaultAt>) -> Result<bool, String> {
    let cfg = p.atlas_config.clone().ok_or("atlas has no configuration file to add the project to (--vault flags only)")?;
    let key = project_key(dir);
    if !Path::new(&key).is_dir() {
        return Err(format!("{key}: not a folder"));
    }
    let c = crate::config::Config::load(&cfg).map_err(|e| e.to_string())?;
    let indexed = !offerable(Path::new(&key), &configured_roots(&c));
    if indexed && vault.is_none() {
        say!(io, "{key} is already indexed (or is inside a folder that is); nothing to change");
        return Ok(true);
    }
    // Where the entries go: the project's own lists (top-level ones are in
    // every project), or the top level of a configuration without projects.
    // This may ask, so it comes before the lock.
    let target = match target_project(p, io, &c, Path::new(&key))? {
        Target::Top => None,
        Target::Project(n) => Some(n),
        Target::Cancelled => {
            say!(io, "cancelled; nothing written");
            return Ok(true);
        }
    };
    // A walk of the folder: not under the lock, and it depends on nothing there.
    let code = has_code(Path::new(&key));
    // From here on every decision rests on the file as read under the lock.
    let held = if io.dry_run { p.unlocked() } else { p.lock_config()? };
    let c = crate::config::Config::load(&cfg).map_err(|e| e.to_string())?;
    // `add-project --new` may have moved the setup into projects during the wait:
    // a top-level entry would then join every project.
    if target.is_none() && c.user.scoped {
        return Err(format!("{}: moved into projects while this waited; nothing written. Run it again.", cfg.display()));
    }
    let indexed = !offerable(Path::new(&key), &configured_roots(&c));
    if indexed && vault.is_none() {
        say!(io, "{key} is already indexed (or is inside a folder that is); nothing to change");
        return Ok(true);
    }
    let plan = match vault {
        Some(at) => plan_vault(p, io, &c, &key, &key, indexed, at)?,
        None => None,
    };
    if indexed && plan.is_none() {
        say!(io, "{key} is already indexed (or is inside a folder that is); nothing to change");
        return Ok(true);
    }
    let name = project_name(&key, &taken_names(&c));
    let doc = load(&cfg)?.value;
    // The entries to add, by list (`sources`, `code.roots`, `vaults`).
    let mut adds: Vec<(&[&str], Value)> = Vec::new();
    let mut did: Vec<String> = Vec::new();
    let mut manual: Vec<String> = Vec::new();
    let to = |list: &str| match &target {
        Some(t) => format!("{list} of project {t:?} (under \"projects\")"),
        None => list.to_string(),
    };
    if !indexed {
        let mut exclude = vec![Value::String("target/**".into()), Value::String("node_modules/**".into())];
        // A vault inside the project is the vault's, not the source's.
        if let Some(v) = &plan {
            let vk = project_key(&v.path);
            if crate::project::key_within(&crate::project::path_key(&v.path), &crate::project::path_key(Path::new(&key))) && vk.len() > key.len() {
                exclude.push(Value::String(format!("{}/**", &vk[key.len() + 1..])));
            }
        }
        adds.push((&["sources"], crate::json!({"name": name.clone(), "root": key.clone(), "include": ["**/*.md"], "exclude": Value::Array(exclude)})));
        did.push(format!("source {name}"));
        manual.push(format!("add {{\"name\": \"{name}\", \"root\": \"{key}\", \"include\": [\"**/*.md\"]}} to {}", to("\"sources\"")));
        if code {
            adds.push((&["code", "roots"], crate::json!({"name": format!("{name}-code"), "root": key.clone()})));
            did.push(format!("code root {name}-code"));
            manual.push(format!("add {{\"name\": \"{name}-code\", \"root\": \"{key}\"}} to {}", to("\"code\".\"roots\"")));
        }
    }
    if let Some(v) = &plan {
        let vk = project_key(&v.path);
        adds.push((&["vaults"], crate::json!({"name": v.name.clone(), "path": vk.clone()})));
        did.push(format!("vault {}", v.name));
        manual.push(format!("add {{\"name\": \"{}\", \"path\": \"{vk}\"}} to {}", v.name, to("\"vaults\"")));
    }
    let edits = list_edits(&doc, target.as_deref(), &adds)?;
    let manual = manual.join("; ");
    let label = match &target {
        Some(t) => format!("project {t}: added {}", did.join(", ")),
        None => format!("added {}", did.join(", ")),
    };
    // Every check that can fail runs on the text that would be written,
    // before anything is: a refusal leaves the configuration as it was.
    let Some((_, _, new)) = config_after(p, &edits)? else {
        say!(io, "atlas will not rewrite {}: the change could not be made without touching other text.", cfg.display());
        say!(io, "  make this edit by hand: {manual}");
        return Ok(false);
    };
    let base = cfg.parent().map(Path::to_path_buf).unwrap_or_default();
    let would = |e: String| format!("{label} would leave a configuration atlas cannot run ({e}); nothing written to {}", cfg.display());
    let next = crate::config::Config::parse(&new, &base).map_err(|e| would(e.to_string()))?;
    let resolved = crate::project::resolve(&next, Path::new(&key), target.as_deref()).map_err(|e| would(e.to_string()))?.config;
    let setup = crate::store::AtlasSetup::from_config(&resolved).map_err(|e| would(e.to_string()))?;
    let specs: Vec<(String, PathBuf)> = resolved.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
    // The planned vault's folder is made below; the others are checked now.
    let existing: Vec<(String, PathBuf)> = specs.iter().filter(|(n, _)| plan.as_ref().is_none_or(|v| &v.name != n)).cloned().collect();
    if !existing.is_empty() || plan.is_none() {
        crate::paths::Vaults::new(&existing).map_err(|e| would(e.to_string()))?;
    }
    let db = resolved.db.clone().or_else(|| p.db.clone()).ok_or_else(|| would("no database path is known".into()))?;
    if io.dry_run {
        let into = match &target {
            Some(t) => format!("project {t}"),
            None => "the top level (this configuration has no projects)".into(),
        };
        dry_config(p, io, &edits, &label, &into, &manual)?;
        if let Some(v) = plan.as_ref().filter(|v| !v.path.is_dir()) {
            say!(io, "dry run: would create the vault folder {} with {VAULT_NOTE}", project_key(&v.path));
        }
        if !indexed {
            say!(io, "dry run: would index source {name} into {}", db.display());
        }
        return Ok(true);
    }
    let mut created: Option<String> = None;
    if let Some(v) = &plan {
        // The folder first: a configured vault whose folder is missing
        // stops the server.
        created = create_vault(v, &key)?;
    }
    let vaults = match crate::paths::Vaults::new(&specs) {
        Ok(v) => v,
        Err(e) => {
            if let (Some(v), Some(sha)) = (&plan, created.as_deref()) {
                if pristine(&v.path, sha) {
                    let _ = std::fs::remove_file(v.path.join(VAULT_NOTE));
                    let _ = std::fs::remove_dir(&v.path);
                }
            }
            return Err(would(e.to_string()));
        }
    };
    let written = edit_config_many(&held, io, &edits, &label, &manual, None)?;
    if !vault_written(p, io, &written, plan.as_ref(), created.as_deref())? {
        return Ok(false);
    }
    // The index is slow and reads the configuration as written: not under the lock.
    drop(held);
    mark_project(p, &key);
    if indexed {
        return Ok(true);
    }
    // Index the source now; the code root is indexed when the server starts,
    // the vault when a tool first reads it.
    let mut store = crate::store::Store::open_with(&db, vaults, setup).map_err(|e| e.to_string())?;
    let s = store.rescan_sources(Some(&[name.as_str()])).map_err(|e| e.to_string())?;
    say!(io, "indexed source {name}: {} Markdown file{}", s.files, if s.files == 1 { "" } else { "s" });
    Ok(true)
}

/// Where `add-project` puts a folder's entries (see [`target_project`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// The top level: the configuration has no projects.
    Top,
    /// This project's entry under `projects`.
    Project(String),
    /// The user cancelled the project choice: nothing is written.
    Cancelled,
}

/// The project `add-project` adds a folder to: [`Target::Top`] for a
/// configuration without projects, else the project named with
/// `--project`, else the folder's own (see [`crate::project::active`]).
/// A folder in none of them is asked about in a terminal (every configured
/// project listed, and cancel; one project is asked about too, never
/// picked silently); without a terminal, or in a dry run, it is refused.
///
/// # Errors
///
/// A message when the folder is in no project and none is named, chosen or
/// the default, or its project is a project file not yet registered.
fn target_project(p: &Paths, io: &mut Io, c: &crate::config::Config, dir: &Path) -> Result<Target, String> {
    if !c.user.scoped {
        return Ok(Target::Top);
    }
    let named = p.project.as_deref().filter(|n| !n.is_empty());
    let Some(a) = crate::project::active(c, dir, named).map_err(|e| e.to_string())? else {
        let names: Vec<String> = c.user.projects.iter().map(|x| x.name.clone()).collect();
        if io.interactive && !io.dry_run && !names.is_empty() {
            say!(io, "{} is in no project. Which project should it be added to?", project_key(dir));
            for (i, n) in names.iter().enumerate() {
                say!(io, "  {}) {n}", i + 1);
            }
            say!(io, "  c) cancel");
            let _ = write!(io.out, "  Project [1-{}, name, or c]: ", names.len());
            let _ = io.out.flush();
            let mut line = String::new();
            let _ = io.input.read_line(&mut line);
            return Ok(match pick_project(&names, &line) {
                Some(n) => Target::Project(n),
                None => Target::Cancelled,
            });
        }
        return Err(format!(
            "{} is in no project and the configuration has no default_project, so atlas does not know which project to add it to; nothing written. Run it in a terminal to choose one, name one with `atlas --project NAME settings add-project \"{}\"`, or make a project with `atlas settings add-project --new`",
            project_key(dir),
            project_key(dir)
        ));
    };
    if !c.user.projects.iter().any(|x| x.name == a.spec.name) {
        let file = a.spec.file.as_deref().map(|f| f.display().to_string()).unwrap_or_default();
        let folder = a.spec.file.as_deref().and_then(Path::parent).map(project_key).unwrap_or_default();
        return Err(format!("project {:?} comes from {file}, which the atlas configuration does not list; nothing written. Register it first with `atlas settings add-project --new \"{folder}\"`", a.spec.name));
    }
    Ok(Target::Project(a.spec.name))
}

/// The project an answer to the project question picks: its number in
/// `names` (from 1) or its name (any case); `None` for cancel (`c`,
/// `cancel`), an empty answer, or anything else.
fn pick_project(names: &[String], answer: &str) -> Option<String> {
    let a = answer.trim();
    if let Ok(i) = a.parse::<usize>() {
        return i.checked_sub(1).and_then(|i| names.get(i)).cloned();
    }
    names.iter().find(|n| n.eq_ignore_ascii_case(a)).cloned()
}

/// One edit of atlas's configuration: the key path, and the new value (`None`
/// removes it).
type ConfigEdit = (&'static [&'static str], Option<Value>);

/// The configuration edits that append each `(list, entry)` of `adds`: to
/// the top-level lists, or with `project`, to that entry of `projects` (one
/// edit of `projects`). A list or object missing on the way is made.
///
/// # Errors
///
/// A message when the project has no entry, or a key on the way is not
/// what it should be.
fn list_edits(doc: &Value, project: Option<&str>, adds: &[(&[&str], Value)]) -> Result<Vec<ConfigEdit>, String> {
    fn append(v: &mut Value, keys: &[&str], item: Value) -> Result<(), String> {
        let (last, path) = keys.split_last().ok_or("internal: an empty key path")?;
        let mut at = v;
        for k in path {
            let m = at.as_object_mut().ok_or_else(|| format!("{k:?}'s parent is not an object"))?;
            let e = m.entry_or_null(k);
            if e.is_null() {
                *e = Value::Object(Map::new());
            }
            at = e;
        }
        let m = at.as_object_mut().ok_or_else(|| format!("the parent of {last:?} is not an object"))?;
        let list = m.entry_or_null(last);
        if list.is_null() {
            *list = Value::Array(Vec::new());
        }
        list.as_array_mut().ok_or_else(|| format!("{last:?} is not a list"))?.push(item);
        Ok(())
    }
    let mut want = doc.clone();
    match project {
        Some(name) => {
            let projects = want.as_object_mut().and_then(|m| m.get_mut("projects")).and_then(Value::as_array_mut).ok_or("the configuration has no \"projects\" list")?;
            let entry = projects
                .iter_mut()
                .find(|x| x.get("name").and_then(Value::as_str) == Some(name))
                .ok_or_else(|| format!("project {name:?} has no entry under \"projects\""))?;
            for (keys, item) in adds {
                append(entry, keys, item.clone())?;
            }
            Ok(vec![(&["projects"], want.get("projects").cloned())])
        }
        None => {
            let had_code = doc.get("code").is_some_and(Value::is_object);
            let mut out: Vec<ConfigEdit> = Vec::new();
            for (keys, item) in adds {
                append(&mut want, keys, item.clone())?;
            }
            for (keys, _) in adds {
                let e: ConfigEdit = match *keys {
                    ["sources"] => (&["sources"], want.get("sources").cloned()),
                    ["vaults"] => (&["vaults"], want.get("vaults").cloned()),
                    ["code", "roots"] if had_code => (&["code", "roots"], want.get("code").and_then(|c| c.get("roots")).cloned()),
                    ["code", "roots"] => (&["code"], want.get("code").cloned()),
                    other => return Err(format!("internal: no edit for {other:?}")),
                };
                out.push(e);
            }
            Ok(out)
        }
    }
}

/// The project offer `atlas --setup` makes after the settings: asks whether
/// to add `dir` as a source (a terminal only); yes adds and indexes it, no
/// keeps the configuration as it is. Asked at most once per folder.
///
/// # Errors
///
/// A message when adding the project fails.
pub fn offer_project_interactive(config: Option<&Path>, db: &Path, user_state: Option<&Path>, dir: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let Some(p) = Paths::detect_in(config, db, user_state) else { return Ok(()) };
    if p.atlas_config.is_none() || !offerable(dir, roots) {
        return Ok(());
    }
    let key = project_key(dir);
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    let mut io = Io { out: &mut out, input: &mut input, interactive, version: None, dry_run: false };
    project_prompt(&p, &mut io, Path::new(&key))
}

/// The yes/no question itself (see [`offer_project_interactive`]).
fn project_prompt(p: &Paths, io: &mut Io, dir: &Path) -> Result<(), String> {
    let key = project_key(dir);
    let cfg = p.atlas_config.as_deref().ok_or("atlas has no configuration file")?;
    let c = crate::config::Config::load(cfg).map_err(|e| e.to_string())?;
    let at = default_vault(p, &c, &key).map(|v| project_key(&v.path)).ok_or("atlas has no configuration file")?;
    say!(io, "\nThis folder is not indexed: {}", dir.display());
    say!(io, "  atlas can add it as a source (its Markdown files, and its code as a code root) and create a vault for its notes.");
    if !io.interactive {
        say!(io, "  To add it later: atlas settings add-project --vault \"{}\" (or --vault-at FOLDER)", dir.display());
        return Ok(());
    }
    let _ = write!(io.out, "  Create a vault for this project at {at}? [Y]es / [o]ther folder / [n]o: ");
    let _ = io.out.flush();
    let mut line = String::new();
    let _ = io.input.read_line(&mut line);
    mark_project(p, &key);
    match line.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => {
            add_project_vault(p, io, dir, Some(&VaultAt::Default))?;
        }
        "o" | "other" | "other folder" => {
            let _ = write!(io.out, "  Folder: ");
            let _ = io.out.flush();
            let mut f = String::new();
            let _ = io.input.read_line(&mut f);
            let f = f.trim().trim_matches('"');
            if f.is_empty() {
                say!(io, "  no folder given; kept as it is. `atlas settings add-project --vault-at FOLDER` adds it any time");
            } else {
                add_project_vault(p, io, dir, Some(&VaultAt::Folder(PathBuf::from(f))))?;
            }
        }
        _ => say!(io, "  kept as it is; `atlas settings add-project --vault` adds it any time"),
    }
    Ok(())
}

// ------------------------------------------------- projects (see crate::project)

/// What the SessionStart project offer knows about the configured projects.
#[derive(Debug, Clone, Default)]
pub struct OfferCtx {
    /// The configuration is project-scoped (has `projects`).
    pub scoped: bool,
    /// The configured projects' names.
    pub projects: Vec<String>,
    /// Every folder that is already a project root, project file folder or
    /// repository checkout.
    pub known: Vec<PathBuf>,
    /// The active project's name (empty for none).
    pub project: String,
    /// The active project's repositories with no local checkout.
    pub unmapped: Vec<String>,
}

impl OfferCtx {
    /// The context for a user configuration and the active project's graph.
    pub fn new(c: &crate::config::Config, g: &crate::project::Graph) -> OfferCtx {
        let mut known = Vec::new();
        for p in &c.user.projects {
            known.extend(crate::project::project_roots(p, &c.user.checkouts));
            known.extend(p.file.as_ref().and_then(|f| f.parent()).map(Path::to_path_buf));
        }
        known.extend(g.roots.iter().cloned());
        OfferCtx { scoped: c.user.scoped, projects: c.user.projects.iter().map(|p| p.name.clone()).collect(), known, project: g.name.clone(), unmapped: g.unmapped.clone() }
    }

    /// The context for a configuration without projects: `roots` are the
    /// folders it indexes already.
    pub fn unscoped(roots: &[PathBuf]) -> OfferCtx {
        OfferCtx { scoped: false, projects: Vec::new(), known: roots.to_vec(), project: String::new(), unmapped: Vec::new() }
    }
}

/// The repositories a folder offers as a project: its child git
/// repositories, else the folder itself when it is one. A folder that is a
/// checkout and holds child repositories offers only the children: a
/// workspace folder may be a checkout of an unrelated repository, which is
/// never a member unless listed in `repos` by hand.
fn offered_repos(dir: &Path) -> Vec<(String, PathBuf, Option<String>)> {
    let kids = crate::project::child_repos(dir);
    if !kids.is_empty() {
        return kids;
    }
    if crate::project::is_repo(dir) {
        let remote = crate::project::read_remote(dir);
        return vec![(crate::project::repo_name(Some(dir), remote.as_deref()), dir.to_path_buf(), remote)];
    }
    kids
}

/// Whether `dir` is a folder of repositories: not one itself, with at least
/// one one level down (the SessionStart offer makes it a new project).
pub fn folder_of_repos(dir: &Path) -> bool {
    !crate::project::is_repo(dir) && !crate::project::child_repos(dir).is_empty()
}

/// The SessionStart line for a folder that is no project's root: the git
/// repositories in it (one level down), and the three answers. Asked until
/// answered, as [`project_line`] is; `None` when the folder is a known root,
/// holds a project file, has no repositories, or was answered before.
pub fn repos_line(p: &Paths, cwd: &Path, ctx: &OfferCtx) -> Option<String> {
    let c = crate::config::Config::load(p.atlas_config.as_ref()?).ok()?;
    if cwd.as_os_str().is_empty() || !cwd.is_dir() || cwd.join(crate::project::PROJECT_FILE).is_file() {
        return None;
    }
    if !ctx.scoped && !offerable(cwd, &ctx.known) {
        return None;
    }
    // `add-project --new` refuses these, so they are never offered.
    if crate::project::too_wide(cwd, crate::dirs::home().as_deref()) {
        return None;
    }
    let key = project_key(cwd);
    if ctx.known.iter().any(|k| same_folder(&project_key(k), &key)) {
        return None;
    }
    let repos: Vec<_> = offered_repos(cwd).into_iter().filter(|(_, d, _)| !ctx.known.iter().any(|k| same_folder(&project_key(k), &project_key(d)))).collect();
    if repos.is_empty() || offered_projects(p).iter().any(|x| same_folder(x, &key)) || !can_record(p) {
        return None;
    }
    let names: Vec<&str> = repos.iter().map(|r| r.0.as_str()).collect();
    let shown = if names.len() > 12 { format!("{}, +{} more", names[..12].join(", "), names.len() - 12) } else { names.join(", ") };
    let n = repos.len();
    let plural = if n == 1 { "" } else { "s" };
    let folder = key.rsplit('/').next().unwrap_or(&key);
    let existing = if ctx.projects.is_empty() { "none yet".to_string() } else { ctx.projects.join(", ") };
    // A new project gets its vault (see add_project_new); the line names
    // where.
    let vault = default_vault_named(p, &c, &project_name(&key, &ctx.projects)).map(|v| project_key(&v.path))?;
    if !ctx.scoped {
        // No projects yet: add-project --new moves the current setup into
        // a project of its own first (named after its configuration folder, or
        // by the user in a terminal), so "add to an existing project" is no answer.
        return Some(format!(
            "atlas: this folder ({folder}) holds {n} git repo{plural} ({shown}) and atlas has no projects yet. In your first reply, whatever the task, ask the user in one line: new project here with these {n} repo{plural} and a vault for its notes at {vault} (atlas first moves its current setup into a project of its own) / skip. On new run `atlas settings add-project --new \"{key}\"` (add `--vault-at \"FOLDER\"` if the user names another vault folder, `--no-vault` if they want none), on skip `atlas settings add-project --no \"{key}\"`; `atlas settings undo` reverses new, the move included. With no answer, or no user to ask, run nothing: atlas asks again next session."
        ));
    }
    Some(format!(
        "atlas: this folder ({folder}) is no atlas project and holds {n} git repo{plural} ({shown}). In your first reply, whatever the task, ask the user in one line: new project here with these {n} repo{plural} and a vault for its notes at {vault} / add to an existing project ({existing}) / skip. On new run `atlas settings add-project --new \"{key}\"` (add `--vault-at \"FOLDER\"` if the user names another vault folder, `--no-vault` if they want none), on add `atlas settings add-project --to NAME \"{key}\"`, on skip `atlas settings add-project --no \"{key}\"`; `atlas settings undo` reverses either. With no answer, or no user to ask, run nothing: atlas asks again next session."
    ))
}

/// The one-time SessionStart line naming the active project's repositories
/// that have no local checkout; each is reported once.
pub fn unmapped_line(p: &Paths, ctx: &OfferCtx) -> Option<String> {
    let s = p.state.as_ref()?;
    let mut v = std::fs::read_to_string(s).ok().and_then(|t| crate::json::parse(&t).ok()).filter(Value::is_object).unwrap_or(Value::Object(Map::new()));
    let seen: Vec<String> = v.get("unmapped").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let fresh: Vec<&String> = ctx.unmapped.iter().filter(|u| !seen.contains(&format!("{}:{u}", ctx.project))).collect();
    if fresh.is_empty() {
        return None;
    }
    let mut all = seen.clone();
    all.extend(fresh.iter().map(|u| format!("{}:{u}", ctx.project)));
    if let Some(o) = v.as_object_mut() {
        o.insert("unmapped".into(), Value::Array(all.into_iter().map(Value::String).collect()));
    }
    if let Some(d) = s.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    crate::fsio::replace_atomic(s, (crate::json::to_string_pretty(&v) + "\n").as_bytes()).ok()?;
    let list: Vec<&str> = fresh.iter().map(|s| s.as_str()).collect();
    Some(format!(
        "atlas: project {} lists repos with no local checkout: {}. They are left out; to include one, map its remote to its folder under \"checkouts\" in the atlas configuration.",
        ctx.project,
        list.join(", ")
    ))
}

/// The vault a new project `name` (folder `key`) gets: see [`plan_vault`],
/// named after the project.
fn plan_project_vault(p: &Paths, io: &mut Io, c: &crate::config::Config, key: &str, name: &str, vault: Option<&VaultAt>) -> Result<Option<VaultPlan>, String> {
    match vault {
        Some(at) => plan_vault(p, io, c, key, name, true, at),
        None => Ok(None),
    }
}

/// Registers project `name` (its file in folder `key`) in the user
/// configuration as `{name, file}`, with `plan` under its `vaults` (the
/// folder made first). The backups share `core` when given. `Ok(false)`
/// when the configuration could not be changed byte-exactly (a vault folder
/// atlas just made is removed again).
fn register_project(p: &Locked, io: &mut Io, key: &str, name: &str, plan: Option<VaultPlan>, core: Option<&str>) -> Result<bool, String> {
    let cfg = p.atlas_config.clone().ok_or("atlas has no configuration file (--vault flags only)")?;
    let file = format!("{key}/{}", crate::project::PROJECT_FILE);
    let mut entry = Map::new();
    entry.insert("name".into(), Value::String(name.into()));
    entry.insert("file".into(), Value::String(file.clone()));
    let mut manual = format!("add {{\"name\": {name:?}, \"file\": {file:?}");
    let mut label = format!("project {name}: registered");
    if let Some(v) = &plan {
        let vk = project_key(&v.path);
        entry.insert("vaults".into(), crate::json!([{"name": v.name.clone(), "path": vk.clone()}]));
        manual.push_str(&format!(", \"vaults\": [{{\"name\": {:?}, \"path\": {vk:?}}}]", v.name));
        label.push_str(&format!(", with vault {}", v.name));
    }
    manual.push_str("} to \"projects\"");
    let mut projects = load(&cfg)?.value.get("projects").and_then(Value::as_array).cloned().unwrap_or_default();
    projects.push(Value::Object(entry));
    let edits: [(&[&str], Option<Value>); 1] = [(&["projects"], Some(Value::Array(projects)))];
    if io.dry_run {
        let ok = dry_config(p, io, &edits, &label, &format!("project {name}"), &manual)?;
        if let Some(v) = plan.as_ref().filter(|v| !v.path.is_dir()) {
            say!(io, "dry run: would create the vault folder {} with {VAULT_NOTE}", project_key(&v.path));
        }
        return Ok(ok);
    }
    let created = match &plan {
        Some(v) => create_vault(v, key)?,
        None => None,
    };
    let written = edit_config_many(p, io, &edits, &label, &manual, core)?;
    vault_written(p, io, &written, plan.as_ref(), created.as_deref())
}

/// `--dry-run`: shows the change `edits` would make to atlas's
/// configuration (into `into`: a project, or the top level) and writes
/// nothing. False, having shown `manual`, when it could not be made
/// byte-exactly.
///
/// # Errors
///
/// A message when there is no configuration file or it cannot be read.
fn dry_config(p: &Paths, io: &mut Io, edits: &[(&[&str], Option<Value>)], label: &str, into: &str, manual: &str) -> Result<bool, String> {
    let Some((path, old, new)) = config_after(p, edits)? else {
        say!(io, "dry run: the change could not be made without touching other text; it would have to be made by hand: {manual}");
        return Ok(false);
    };
    let _ = write!(io.out, "{}", diff(&path.display().to_string(), &old, &new));
    say!(io, "dry run: would write {} ({label}, into {into}); nothing written", path.display());
    Ok(true)
}

/// `atlas settings add-project --new [--name NAME] [DIR]`: writes
/// `DIR/atlas.project.json` naming the git repositories in DIR (path and
/// remote each), which makes DIR a project root, and registers it in the
/// user configuration as `{name, file}`, so a folder inside one of its
/// repositories' checkouts elsewhere resolves to it and the other projects
/// read its memory folder. The project gets its vault with it, unless
/// `vault` is `None` (`--no-vault`): by default `vaults/<name>-notes`
/// beside atlas's configuration file, as [`add_project_vault`] places one,
/// so it is never inside a repository; registered in the project's entry,
/// so only that project sees it. One undo removes all three (the vault
/// folder only while it holds nothing but atlas's note). A project file
/// already there is registered (with its vault) when it is not yet.
///
/// # Errors
///
/// A message when the folder is too wide to be a project root
/// ([`crate::project::too_wide`]), holds no repositories, the vault folder cannot be
/// one (see [`plan_vault`]), or a file cannot be written.
pub fn add_project_new(p: &Paths, io: &mut Io, dir: &Path, name: Option<&str>, vault: Option<&VaultAt>) -> Result<bool, String> {
    let cfg = p.atlas_config.clone().ok_or("atlas has no configuration file (--vault flags only)")?;
    let key = project_key(dir);
    if crate::project::too_wide(Path::new(&key), crate::dirs::home().as_deref()) {
        return Err(format!(
            "{key} is your home folder, a folder holding it, or a drive root: as a project root it would claim every folder below it, and none would be asked about. Nothing written. Move your repositories into a folder of their own and run this there, or add each with `atlas settings add-project \"REPO\"`"
        ));
    }
    let base = cfg.parent().map(Path::to_path_buf).unwrap_or_default();
    let new_name = name.map(str::to_string).unwrap_or_else(|| project_name(&key, &[]));
    // The one question comes before the lock.
    let c = crate::config::Config::load(&cfg).map_err(|e| e.to_string())?;
    let asked = (!c.user.scoped).then(|| existing_setup_name(io, &base, &new_name));
    // One lock for the whole command: the revert below restores the text read
    // under it, so a revert cannot lose what another process wrote.
    let held = if io.dry_run { p.unlocked() } else { p.lock_config()? };
    let c = crate::config::Config::load(&cfg).map_err(|e| e.to_string())?;
    if c.user.scoped {
        return add_project_new_scoped(&held, io, &c, dir, name, vault, None);
    }
    // A configuration without projects (every fresh install) is moved into
    // one first, as `migrate-projects` would, under the same backup stamp:
    // one `atlas settings undo` reverses the move and the new project.
    let doc = load(&cfg)?;
    let old = doc.text.clone().unwrap_or_default();
    let db = p.db.clone().unwrap_or_else(crate::dirs::default_db);
    let existing = asked.unwrap_or_else(|| project_name(&project_key(&base), std::slice::from_ref(&new_name)));
    let moved = match crate::project::migrate_value(&doc.value, &base, &existing, &db, Some(&p.claude_dir), crate::dirs::home().as_deref()) {
        Some(v) => v,
        // Nothing to move (no vaults, sources or code yet): an empty list.
        None => {
            let mut v = doc.value.clone();
            v.as_object_mut().ok_or("atlas's configuration is not a JSON object")?.insert("projects".into(), Value::Array(Vec::new()));
            v
        }
    };
    let text = crate::json::to_string_pretty(&moved) + "\n";
    let c2 = crate::config::Config::parse(&text, &base).map_err(|e| format!("the configuration moved into projects would not load: {e}"))?;
    let what = if c2.user.projects.is_empty() { "this configuration has no projects yet: adding the projects list".to_string() } else { format!("this configuration has no projects yet: moving the current setup into project {existing} first (as `atlas settings migrate-projects {existing}` would)") };
    say!(io, "{what}");
    let _ = write!(io.out, "{}", diff(&cfg.display().to_string(), &old, &text));
    if io.dry_run {
        say!(io, "dry run: would write {} ({}), then the new project; nothing written", cfg.display(), if c2.user.projects.is_empty() { "the projects list" } else { "the move" });
        return Ok(true);
    }
    let core = commit_in(&held, io, "atlas", &cfg, &text, &format!("moved the configuration into project {existing}"), None)?;
    let res = add_project_new_scoped(&held, io, &c2, dir, name, vault, Some(&core));
    if !matches!(res, Ok(true)) {
        // Nothing half done: the configuration goes back as it was.
        let _ = crate::fsio::replace_atomic(&cfg, old.as_bytes());
        if let Ok(d) = p.backups_for("atlas") {
            let _ = std::fs::remove_file(d.join(format!("atlas.{core}.json")));
        }
        say!(io, "the move into projects was reverted");
    }
    res
}

/// The name the existing setup gets when `add-project --new` moves it into
/// a project. The default is the name of the folder holding atlas's
/// configuration (`base`), made a valid project name and unique against the
/// new project's (`-2`, `-3`, ...). In a terminal (not on a dry run) the
/// user is asked, the default taken on an empty answer or end of input; an
/// answer that is no valid name, or the new project's, is asked again.
fn existing_setup_name(io: &mut Io, base: &Path, new_name: &str) -> String {
    let taken = [new_name.to_string()];
    let default = project_name(&project_key(base), &taken);
    if !io.interactive || io.dry_run {
        return default;
    }
    loop {
        let _ = write!(io.out, "  Name for the project the current setup moves into [{default}]: ");
        let _ = io.out.flush();
        let mut line = String::new();
        if !matches!(io.input.read_line(&mut line), Ok(n) if n > 0) {
            return default;
        }
        let answer = line.trim();
        if answer.is_empty() {
            return default;
        }
        if !crate::project::valid_name(answer) {
            say!(io, "  {answer:?} is no project name: 1 to 64 characters of a-z A-Z 0-9 - _ (not starting with _)");
        } else if answer.eq_ignore_ascii_case(new_name) {
            say!(io, "  {answer:?} is the new project's name; pick another");
        } else {
            return answer.to_string();
        }
    }
}


/// [`add_project_new`] on a project-scoped configuration `c`; `core` is the
/// backup stamp of a move into projects made just before (one undo).
fn add_project_new_scoped(p: &Locked, io: &mut Io, c: &crate::config::Config, dir: &Path, name: Option<&str>, vault: Option<&VaultAt>, core0: Option<&str>) -> Result<bool, String> {
    let key = project_key(dir);
    let dir = Path::new(&key);
    let file = dir.join(crate::project::PROJECT_FILE);
    let fkey = crate::project::path_key(&file);
    if file.exists() {
        // A project file written before projects were registered, or by hand:
        // registered now when it is not yet.
        if c.user.projects.iter().any(|x| x.file.as_deref().map(crate::project::path_key).as_deref() == Some(fkey.as_str())) {
            say!(io, "{} exists and is registered already; nothing to change", file.display());
            return Ok(true);
        }
        let pname = crate::project::load_project_file(&file).map_err(|e| e.to_string())?.name;
        if c.user.projects.iter().any(|x| x.name.eq_ignore_ascii_case(&pname)) {
            return Err(format!("{} names project {pname:?}, and another project has that name", file.display()));
        }
        let plan = plan_project_vault(p, io, c, &key, &pname, vault)?;
        return register_project(p, io, &key, &pname, plan, core0);
    }
    let repos = offered_repos(dir);
    if repos.is_empty() {
        return Err(format!("{key}: holds no git repositories"));
    }
    let taken: Vec<String> = c.user.projects.iter().map(|x| x.name.clone()).collect();
    let name = match name {
        Some(n) => n.to_string(),
        None => project_name(&key, &taken),
    };
    if taken.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
        return Err(format!("a project named {name:?} exists; pass --name or use --to {name}"));
    }
    // Checked before anything is written.
    let plan = plan_project_vault(p, io, c, &key, &name, vault)?;
    let text = crate::project::project_file_text(&name, dir, &repos);
    let _ = write!(io.out, "{}", diff(&file.display().to_string(), "", &text));
    if io.dry_run {
        say!(io, "dry run: would write {} (project {name}: {} repos); nothing written", file.display(), repos.len());
        return register_project(p, io, &key, &name, plan, None);
    }
    let core = commit_in(p, io, "project", &file, &text, &format!("project {name}: {} repos", repos.len()), core0)?;
    // Registered in the user configuration too, under the same backup
    // stamp (one undo removes both): a folder inside one of its repos'
    // checkouts elsewhere resolves to it, and the other projects read its
    // memory folder for memories marked `scope: user`.
    if !register_project(p, io, &key, &name, plan, Some(&core))? {
        // Nothing half done: the file goes with its backup.
        let _ = std::fs::remove_file(&file);
        if let Ok(d) = p.backups_for("project") {
            let _ = std::fs::remove_file(d.join(format!("project.{core}.absent")));
        }
        return Ok(false);
    }
    mark_project(p, &key);
    say!(io, "Add \"role\" and \"talks_to\" to each repo in {} as you like; atlas reads them at the next session.", file.display());
    Ok(true)
}

/// `atlas settings add-project --to PROJECT [DIR]`: adds the git
/// repositories in DIR (or DIR itself) to a project's `repos` in the user
/// configuration (backed up; undo restores it).
///
/// # Errors
///
/// A message when the folder holds no repositories or the configuration
/// cannot be changed.
pub fn add_to_project(p: &Paths, io: &mut Io, dir: &Path, project: &str) -> Result<bool, String> {
    let cfg = p.atlas_config.clone().ok_or("atlas has no configuration file (--vault flags only)")?;
    let held = if io.dry_run { p.unlocked() } else { p.lock_config()? };
    let c = crate::config::Config::load(&cfg).map_err(|e| e.to_string())?;
    if !c.user.scoped {
        return Err("this configuration has no projects yet: run `atlas settings migrate-projects NAME` first".into());
    }
    let key = project_key(dir);
    let repos = offered_repos(Path::new(&key));
    if repos.is_empty() {
        return Err(format!("{key}: holds no git repositories"));
    }
    let doc = load(&cfg)?.value;
    let mut projects = doc.get("projects").and_then(Value::as_array).cloned().unwrap_or_default();
    let i = match projects.iter().position(|x| x.get("name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(project))) {
        Some(i) => i,
        None => {
            projects.push(crate::json!({"name": project}));
            projects.len() - 1
        }
    };
    let entry = projects[i].as_object_mut().ok_or("a projects entry is not an object")?;
    let list = entry.entry_or_null("repos");
    if !list.is_array() {
        *list = Value::Array(Vec::new());
    }
    let arr = list.as_array_mut().ok_or("repos is not a list")?;
    let mut added = 0;
    for (n, d, r) in &repos {
        if arr.iter().any(|x| x.get("name").and_then(Value::as_str) == Some(n.as_str())) {
            continue;
        }
        let mut m = Map::new();
        m.insert("name".into(), Value::String(n.clone()));
        m.insert("path".into(), Value::String(project_key(d)));
        if let Some(r) = r {
            m.insert("remote".into(), Value::String(r.clone()));
        }
        arr.push(Value::Object(m));
        added += 1;
    }
    if added == 0 {
        say!(io, "every repo in {key} is in project {project} already; nothing to change");
        // Still an answer: the folder's question ends here.
        if !io.dry_run {
            mark_project(p, &key);
        }
        return Ok(true);
    }
    let manual = format!("add the repos of {key} to \"repos\" of project {project:?}");
    if io.dry_run {
        return dry_config(p, io, &[(&["projects"], Some(Value::Array(projects)))], &format!("project {project}: added {added} repos"), &format!("project {project}"), &manual);
    }
    let ok = edit_config(&held, io, &["projects"], Some(&Value::Array(projects)), &format!("project {project}: added {added} repos"), &manual)?;
    if ok {
        mark_project(p, &key);
    }
    Ok(ok)
}

/// `atlas settings migrate-projects NAME`: moves a configuration without
/// projects into one project named NAME (see
/// [`crate::project::migrate_value`]): its database and memory store stay
/// where they are, and it becomes `default_project`. Backed up first;
/// `atlas settings undo` restores the file. Running it again changes nothing.
///
/// # Errors
///
/// A message when the configuration cannot be read, the result would not
/// load, or the file cannot be written.
pub fn migrate_projects(p: &Paths, io: &mut Io, name: &str) -> Result<bool, String> {
    let cfg = p.atlas_config.clone().ok_or("atlas has no configuration file (--vault flags only)")?;
    let held = p.lock_config()?;
    let doc = load(&cfg)?;
    let base = cfg.parent().map(Path::to_path_buf).unwrap_or_default();
    let db = p.db.clone().unwrap_or_else(crate::dirs::default_db);
    let Some(new) = crate::project::migrate_value(&doc.value, &base, name, &db, Some(&p.claude_dir), crate::dirs::home().as_deref()) else {
        say!(io, "{} has projects already (or nothing to move); nothing to change", cfg.display());
        return Ok(true);
    };
    let text = crate::json::to_string_pretty(&new) + "\n";
    crate::config::Config::parse(&text, &base).map_err(|e| format!("the migrated configuration would not load: {e}"))?;
    let old = doc.text.clone().unwrap_or_default();
    let _ = write!(io.out, "{}", diff(&cfg.display().to_string(), &old, &text));
    commit(&held, io, "atlas", &cfg, &text, &format!("moved the configuration into project {name}"))?;
    say!(io, "atlas reads its configuration at start: restart Claude Code (or reconnect atlas with /mcp).");
    Ok(true)
}

// ----------------------------------------------------------- the terminal

fn claude_version() -> Option<(u32, u32, u32)> {
    let run = |cmd: &str, args: &[&str]| -> Option<String> {
        let mut c = std::process::Command::new(cmd);
        c.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        let child = c.spawn().ok()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(Ok(o)) => Some(String::from_utf8_lossy(&o.stdout).to_string()),
            _ => None,
        }
    };
    let out = run("claude", &["--version"]).or_else(|| if cfg!(windows) { run("cmd", &["/C", "claude", "--version"]) } else { None })?;
    parse_version(&out)
}

/// The `atlas settings` subcommand.
///
/// # Arguments
///
/// * `args` - the words after `settings`.
/// * `config` - atlas's configuration file, when one is in use.
/// * `db` - the index database (the offer state goes beside it).
/// * `user_state` - the user-level state folder (a configuration with
///   projects).
/// * `project` - the project named with `--project` (or `ATLAS_PROJECT`).
///
/// # Errors
///
/// A message for the user when the subcommand cannot run.
pub fn cli(args: Vec<String>, config: Option<&Path>, db: &Path, user_state: Option<&Path>, project: Option<&str>) -> Result<ExitCode, String> {
    let mut p = Paths::detect_in(config, db, user_state).ok_or("no home folder is known (set HOME, USERPROFILE or CLAUDE_CONFIG_DIR)")?;
    p.project = project.map(str::to_string);
    let interactive = std::io::stdin().is_terminal();
    // Only an offer that can write needs the version (the min_version gate).
    let writes = interactive || args.iter().any(|a| a == "--yes");
    let version = if args.first().map(String::as_str) == Some("offer") && writes { claude_version() } else { None };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    run(&args, &p, &mut Io { out: &mut out, input: &mut input, interactive, version, dry_run: false })
}

/// The interactive offer `atlas --setup` runs after its model step. Without
/// a terminal it only describes, and writes nothing.
///
/// # Errors
///
/// A message for the user when the offer cannot run.
pub fn offer_interactive(config: Option<&Path>, db: &Path, user_state: Option<&Path>) -> Result<(), String> {
    let Some(p) = Paths::detect_in(config, db, user_state) else {
        println!("settings: no home folder is known; run `atlas settings offer` later");
        return Ok(());
    };
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    println!("\nClaude Code settings atlas offers (each is opt-in; `atlas settings` changes them any time):");
    // Without a terminal nothing is written, so Claude Code is not spawned.
    let version = if interactive { claude_version() } else { None };
    run(&["offer".to_string()], &p, &mut Io { out: &mut out, input: &mut input, interactive, version, dry_run: false }).map(|_| ())
}

// ------------------------------------------------------------ hook drift

/// The plugin's hook declarations, the reference every live atlas hook's
/// input is checked against.
pub const PLUGIN_HOOKS: &str = include_str!("../hooks/hooks.json");

/// An atlas hook as declared: an `mcp_tool` hook on atlas's `hook` tool (a
/// server named `atlas` or `plugin:...:atlas`) with its input fields, or a
/// command hook running `atlas --hook` (`None`: it gets the whole event).
fn atlas_hook(h: &Value) -> Option<Option<std::collections::BTreeSet<String>>> {
    let s = |k: &str| h.get(k).and_then(Value::as_str).unwrap_or("");
    let server = s("server");
    match s("type") {
        "mcp_tool" if s("tool") == "hook" && (server == "atlas" || server.ends_with(":atlas")) => {
            Some(Some(h.get("input").and_then(Value::as_object).map(|m| m.keys().cloned().collect()).unwrap_or_default()))
        }
        "command" if s("command").contains("atlas") && s("command").contains("--hook") => Some(None),
        _ => None,
    }
}

/// Every atlas hook of a settings value, by event, in file order.
fn atlas_hooks(settings: &Value) -> Vec<(String, Option<std::collections::BTreeSet<String>>)> {
    let mut out = Vec::new();
    let Some(events) = settings.get("hooks").and_then(Value::as_object) else { return out };
    for (event, groups) in events {
        for g in groups.as_array().into_iter().flatten() {
            for h in g.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                if let Some(f) = atlas_hook(h) {
                    out.push((event.clone(), f));
                }
            }
        }
    }
    out
}

/// How the atlas hooks in a Claude Code settings value differ from
/// `hooks/hooks.json`, one line per finding: an event the plugin hooks with
/// no live atlas hook, an `mcp_tool` hook whose input fields differ (the
/// missing and extra fields), and an atlas hook on an event the plugin does
/// not hook. A command hook is compared by its event only. Empty when they
/// match or the settings hold no atlas hook (the plugin's own hooks are then
/// the ones in use).
pub fn hook_drift(settings: &Value) -> Vec<String> {
    let live = atlas_hooks(settings);
    if live.is_empty() {
        return Vec::new();
    }
    let reference = crate::json::parse(PLUGIN_HOOKS).map(|v| atlas_hooks(&v)).unwrap_or_default();
    let mut out = Vec::new();
    let mut events: Vec<&str> = Vec::new();
    for (event, want) in &reference {
        if events.contains(&event.as_str()) {
            continue;
        }
        events.push(event);
        let here: Vec<&Option<std::collections::BTreeSet<String>>> = live.iter().filter(|(e, _)| e == event).map(|(_, f)| f).collect();
        if here.is_empty() {
            out.push(format!("{event}: no atlas hook"));
            continue;
        }
        let Some(want) = want else { continue };
        for fields in here.into_iter().flatten() {
            let missing: Vec<&str> = want.difference(fields).map(String::as_str).collect();
            let extra: Vec<&str> = fields.difference(want).map(String::as_str).collect();
            if missing.is_empty() && extra.is_empty() {
                continue;
            }
            let mut line = format!("{event}:");
            if !missing.is_empty() {
                line.push_str(&format!(" missing {}", missing.join(", ")));
            }
            if !extra.is_empty() {
                line.push_str(&format!("{} extra {}", if missing.is_empty() { "" } else { ";" }, extra.join(", ")));
            }
            out.push(line);
        }
    }
    for (event, _) in &live {
        if !events.contains(&event.as_str()) {
            events.push(event);
            out.push(format!("{event}: an atlas hook on an event hooks/hooks.json does not hook"));
        }
    }
    out
}

/// The drift of the live user settings' atlas hooks, read from `path`
/// (a missing or unreadable file has none).
pub fn hook_drift_at(path: &Path) -> Vec<String> {
    load(path).map(|d| hook_drift(&d.value)).unwrap_or_default()
}

/// Whether a settings value has atlas hooks, but none of its PostToolUse atlas
/// groups would see `AskUserQuestion`.
fn ask_answers_missed(settings: &Value) -> bool {
    if atlas_hooks(settings).is_empty() {
        return false;
    }
    let groups = settings.pointer("/hooks/PostToolUse").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    !groups.iter().any(|g| {
        let atlas = g.get("hooks").and_then(Value::as_array).is_some_and(|hs| hs.iter().any(|h| atlas_hook(h).is_some()));
        let m = g.get("matcher").and_then(Value::as_str).unwrap_or("*");
        atlas && (m == "*" || m.split('|').any(|x| x == "AskUserQuestion"))
    })
}

/// The warning for a session's first prompt when the live atlas hooks
/// drifted from `hooks/hooks.json`; `None` when they match. Only idea questions
/// use the Stop hook, so a settings install that lacks it hears nothing unless
/// `asks_on` (`atlas doctor` still lists it). With `ideas_on`, an
/// `AskUserQuestion` the PostToolUse matcher leaves out is said, or typed answers
/// and labels never come back.
pub fn hook_drift_line(p: &Paths, ideas_on: bool, asks_on: bool) -> Option<String> {
    let mut d = hook_drift_at(&p.settings());
    if !asks_on {
        d.retain(|l| !l.starts_with("Stop:"));
    }
    if ideas_on && load(&p.settings()).is_ok_and(|s| ask_answers_missed(&s.value)) {
        d.push("PostToolUse: the matcher leaves out AskUserQuestion".to_string());
    }
    (!d.is_empty()).then(|| {
        format!(
            "atlas: the atlas hooks in {} differ from the plugin's hooks/hooks.json ({}); a hook missing a field gives atlas less to go on (a PreToolUse without tool_use_id once made the agent cap count agents twice). Tell the user; `atlas doctor` lists it.",
            p.settings().display(),
            d.join("; ")
        )
    })
}

/// Whether Claude Code has atlas installed as a plugin, read from
/// `plugins/installed_plugins.json` in its folder: `{"plugins": {"atlas@MARKET":
/// [{"version": ...}]}}`, a shape seen in a real install and not a documented
/// one, so anything else is an error. No file is `Ok(None)`; `Ok(Some(v))`
/// holds the installed version, when it says.
fn plugin_install(claude_dir: &Path) -> Result<Option<String>, String> {
    let path = claude_dir.join("plugins").join("installed_plugins.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let v = crate::json::parse(&text).map_err(|e| format!("{}: not JSON ({} at byte {})", path.display(), e.msg, e.at))?;
    let plugins = v.get("plugins").and_then(Value::as_object).ok_or_else(|| format!("{}: no `plugins` object", path.display()))?;
    for (key, installs) in plugins {
        if key.split('@').next() != Some("atlas") {
            continue;
        }
        let list = installs.as_array().ok_or_else(|| format!("{}: {key} is not a list", path.display()))?;
        if let Some(first) = list.first() {
            return Ok(Some(first.get("version").and_then(Value::as_str).unwrap_or("unknown").to_string()));
        }
    }
    Ok(None)
}

/// How long Claude Code holds a server off after a failed start, as measured:
/// it was still failing at 14.9 minutes and connected at 15.2.
const HELD_OFF_MS: i64 = 15 * 60 * 1000;

/// The doctor's lines for an atlas server Claude Code is holding off: after one
/// failed start it records the server in `mcp-needs-auth-cache.json` and does not
/// start it for 15 minutes, even with the cause fixed, and atlas cannot change that.
/// Read only, and not a documented file, so anything unexpected in it says nothing.
fn held_off_lines(claude_dir: &Path, now_ms: i64) -> Vec<String> {
    let path = claude_dir.join("mcp-needs-auth-cache.json");
    let Some(v) = std::fs::read_to_string(&path).ok().and_then(|t| crate::json::parse(&t).ok()) else { return Vec::new() };
    let Some(entries) = v.as_object() else { return Vec::new() };
    let mut out = Vec::new();
    for (server, e) in entries {
        if server.rsplit(':').next() != Some("atlas") {
            continue;
        }
        let Some(at) = e.get("timestamp").and_then(Value::as_i64) else { continue };
        let left = at + HELD_OFF_MS - now_ms;
        if left > 0 {
            let minutes = (left + 59_999) / 60_000;
            out.push(format!(
                "server: Claude Code recorded a failed start of {server} and will not start it for about {minutes} more minutes, even once the cause is fixed, unless the fix changed its command. A new session after that connects, and nothing atlas does brings it sooner."
            ));
        }
    }
    out
}

/// The doctor's lines about project roots wide enough to claim every folder
/// under them, so that no folder there is asked about: what a setup from
/// before this check may hold. Named, never rewritten, and no failure, since
/// such a root can be meant. `config` is only read: the doctor writes nothing,
/// and a first run's template holds no roots.
fn wide_root_lines(config: Option<&Path>) -> Vec<String> {
    let home = crate::dirs::home();
    let mut out = Vec::new();
    if let Some(cp) = config.filter(|c| c.is_file()) {
        match crate::config::Config::load(cp) {
            Ok(c) => {
                for p in &c.user.projects {
                    for r in p.roots.iter().filter(|r| crate::project::too_wide(r, home.as_deref())) {
                        out.push(format!(
                            "roots: project {} in {} has the root {}, which holds your home folder or is a drive root, so no folder under it is asked about. To narrow it, list the project's own folders under `roots`.",
                            p.name,
                            cp.display(),
                            r.display()
                        ));
                    }
                }
            }
            Err(e) => out.push(format!("roots: not checked, {} did not load: {e}", cp.display())),
        }
    }
    if let Some(f) = home.as_deref().and_then(crate::project::find_project_file) {
        out.push(format!("roots: the project file {} sits at or above your home folder, so every folder under it reads that project. To narrow it, move the file into the project's own folder.", f.display()));
    }
    out
}

/// `atlas doctor`: says what it could check of the live Claude Code settings'
/// atlas hooks against `hooks/hooks.json`, one honest line for each case: no
/// settings file, a malformed one, no atlas hooks in it, a plugin install
/// (whose hooks live in the plugin), or real hooks that match or differ. Also
/// names a project root that is too wide in `config`, and a server Claude Code is
/// holding off. Exit 1 when the hooks
/// differ or when the settings or plugin file could not be read. Writes nothing.
///
/// # Errors
///
/// A message when no Claude Code folder is known.
pub fn doctor_cli(args: &[String], config: Option<&Path>) -> Result<ExitCode, String> {
    if let Some(a) = args.first() {
        if a == "-h" || a == "--help" {
            println!("usage: atlas doctor   says what it can check of the atlas hooks and project roots; writes nothing");
            return Ok(ExitCode::SUCCESS);
        }
        return Err(format!("atlas doctor takes no arguments, not {a:?}"));
    }
    let env = |n: &str| std::env::var_os(n).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = env("CLAUDE_CONFIG_DIR").or_else(|| env(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| h.join(".claude"))).ok_or("no Claude Code folder: set CLAUDE_CONFIG_DIR")?;
    let path = dir.join("settings.json");
    let mut failed = false;
    match load(&path) {
        Err(e) => {
            println!("hooks: not checked, the settings file could not be read: {e}");
            failed = true;
        }
        Ok(doc) if atlas_hooks(&doc.value).is_empty() => {
            if doc.text.is_some() {
                println!("hooks: {} holds no atlas hooks, so there was nothing there to check", path.display());
            } else {
                println!("hooks: there is no {}, so there was nothing there to check", path.display());
            }
        }
        Ok(doc) => {
            let d = hook_drift(&doc.value);
            if d.is_empty() {
                println!("hooks: the atlas hooks in {} match hooks/hooks.json", path.display());
            } else {
                println!("hooks: the atlas hooks in {} differ from hooks/hooks.json:", path.display());
                for line in &d {
                    println!("  {line}");
                }
                failed = true;
            }
        }
    }
    match plugin_install(&dir) {
        Ok(Some(version)) => println!("plugin: atlas {version} is installed as a plugin; its hooks are in the plugin and were not checked here"),
        Ok(None) => {}
        Err(e) => {
            println!("plugin: cannot tell whether atlas is installed as a plugin: {e}");
            failed = true;
        }
    }
    for line in wide_root_lines(config) {
        println!("{line}");
    }
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    for line in held_off_lines(&dir, now_ms) {
        println!("{line}");
    }
    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_stop_hook_is_drift_only_with_ideas_on() {
        let d = std::env::temp_dir().join(format!("atlas-drift-ideas-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut live = crate::json::parse(PLUGIN_HOOKS).unwrap();
        let hooks = live.as_object_mut().and_then(|m| m.get_mut("hooks")).and_then(Value::as_object_mut).unwrap();
        hooks.remove("Stop");
        let p = Paths { claude_dir: d.clone(), atlas_config: None, state: None, db: None, user_state: None, project: None };
        std::fs::write(p.settings(), live.to_string()).unwrap();
        assert_eq!(hook_drift_line(&p, false, false), None, "off adds nothing to context");
        assert_eq!(hook_drift_line(&p, true, false), None, "no questions, so no use for Stop");
        assert!(hook_drift_line(&p, true, true).is_some_and(|l| l.contains("Stop: no atlas hook")));
        std::fs::write(p.settings(), PLUGIN_HOOKS.replace("|AskUserQuestion", "")).unwrap();
        assert!(hook_drift_line(&p, true, false).is_some_and(|l| l.contains("leaves out AskUserQuestion")));
        assert_eq!(hook_drift_line(&p, false, false), None);
    }

    use super::*;

    #[test]
    fn the_project_answer_picks_by_number_or_name_and_anything_else_cancels() {
        let names = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(pick_project(&names, "2\n"), Some("beta".into()));
        assert_eq!(pick_project(&names, " Alpha \r\n"), Some("alpha".into()));
        for a in ["c\n", "cancel", "", "0", "3", "gamma", "-1"] {
            assert_eq!(pick_project(&names, a), None, "{a:?}");
        }
        // One project is still a question: an empty answer picks nothing.
        assert_eq!(pick_project(&names[..1], "\n"), None);
        assert_eq!(pick_project(&names[..1], "1"), Some("alpha".into()));
    }

    #[test]
    fn the_manifest_is_well_formed() {
        let m = manifest().unwrap();
        let ids: Vec<&str> = m.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["subagent-cache-1h", "goal-checkin-120", "routing", "update", "update-pin", "orchestrate", "memory-rules", "agents-builders", "agents-reviewers", "agents-research", "context-mode", "context-limit", "context-handoff"]);
        for s in &m {
            assert!(!s.title.is_empty() && !s.what.is_empty() && !s.cost.is_empty() && !s.basis.is_empty() && !s.short.is_empty(), "{}", s.id);
            if s.kind != Kind::Atlas {
                assert!(s.value.is_some() && !s.docs.is_empty(), "{}", s.id);
                // The offered value passes its own check.
                let raw = s.value.as_ref().and_then(Value::as_str).unwrap();
                assert_eq!(check(s, raw).ok().as_ref(), s.value.as_ref());
            }
        }
        assert_eq!(m[0].min_version, Some((2, 1, 242)));
    }

    #[test]
    fn reads_versions() {
        assert_eq!(parse_version("2.1.316 (Claude Code)"), Some((2, 1, 316)));
        assert_eq!(parse_version("v0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("claude 2.10"), None);
    }

    #[test]
    fn checks_values_against_the_manifest() {
        let m = manifest().unwrap();
        let by = |id: &str| m.iter().find(|s| s.id == id).unwrap();
        assert!(check(by("subagent-cache-1h"), "2h").is_err());
        assert_eq!(check(by("goal-checkin-120"), "0").unwrap(), Value::String("0".into()));
        assert!(check(by("goal-checkin-120"), "1441").is_err());
        assert!(check(by("goal-checkin-120"), "1.5").is_err());
        assert_eq!(check(by("routing"), "off").unwrap(), Value::Bool(false));
        assert_eq!(check(by("update-pin"), "v0.1.3").unwrap(), Value::String("0.1.3".into()));
        assert!(check(by("update-pin"), "latest").is_err());
    }

    #[test]
    fn splices_one_value_and_keeps_every_other_byte() {
        let text = "{\n  \"vaults\": [{\"name\": \"notes\", \"path\": \"notes\"}],\n  \"ids\": []\n}\n";
        let v = Value::Bool(false);
        let out = splice(text, &["lean", "routing", "enabled"], Some(&v)).unwrap();
        assert_eq!(out, "{\n  \"vaults\": [{\"name\": \"notes\", \"path\": \"notes\"}],\n  \"ids\": [],\n  \"lean\": {\"routing\": {\"enabled\": false}}\n}\n");
        // Into an existing section, then back out.
        let out2 = splice(&out, &["lean", "update", "pin"], Some(&Value::String("0.1.0".into()))).unwrap();
        assert!(out2.contains("{\"routing\": {\"enabled\": false}, \"update\": {\"pin\": \"0.1.0\"}}"), "{out2}");
        let out3 = splice(&out2, &["lean", "update", "pin"], None).unwrap();
        assert!(out3.contains("\"update\": {}"), "{out3}");
        let out4 = splice(&out3, &["lean", "routing", "enabled"], Some(&Value::Bool(true))).unwrap();
        assert!(out4.contains("{\"routing\": {\"enabled\": true}"), "{out4}");
        // Removing the first, a middle and the only member.
        assert_eq!(splice("{\"a\": 1, \"b\": 2}", &["a"], None).unwrap(), "{\"b\": 2}");
        assert_eq!(splice("{\"a\": 1, \"b\": 2, \"c\": 3}", &["b"], None).unwrap(), "{\"a\": 1, \"c\": 3}");
        assert_eq!(splice("{\"a\": 1}", &["a"], None).unwrap(), "{}");
        assert_eq!(splice("{}", &["a"], Some(&Value::Bool(true))).unwrap(), "{\n  \"a\": true\n}");
        // A string holding braces and an escaped quote does not confuse it.
        let tricky = "{\"s\": \"}{\\\"\", \"t\": 1}";
        assert_eq!(splice(tricky, &["t"], Some(&Value::Bool(true))).unwrap(), "{\"s\": \"}{\\\"\", \"t\": true}");
        // A non-object parent cannot be spliced.
        assert!(splice("{\"lean\": 3}", &["lean", "x"], Some(&Value::Bool(true))).is_none());
    }

    #[test]
    fn diffs_one_region() {
        let d = diff("f", "{\n  \"a\": 1\n}\n", "{\n  \"a\": 1,\n  \"b\": 2\n}\n");
        assert_eq!(d, "--- f\n+++ f (after)\n {\n-  \"a\": 1\n+  \"a\": 1,\n+  \"b\": 2\n }\n");
    }

    /// The terminal question `atlas --setup` asks: an empty answer is yes,
    /// `o` takes a folder, `n` changes nothing, and without a terminal it
    /// only says how, writing nothing and recording nothing.
    #[test]
    fn the_setup_question_defaults_to_yes() {
        let base = std::env::temp_dir().join(format!("atlas-setup-vault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("atlas");
        std::fs::create_dir_all(home.join("notes")).unwrap();
        std::fs::write(home.join("atlas.json"), "{\n  \"vaults\": [{\"name\": \"notes\", \"path\": \"notes\"}]\n}\n").unwrap();
        let p = Paths { claude_dir: base.join("claude"), atlas_config: Some(home.join("atlas.json")), state: Some(home.join("state").join(OFFER_STATE)), db: Some(home.join("state").join("atlas.sqlite")), user_state: None, project: None };
        let ask = |dir: &Path, answers: &str, interactive: bool| -> String {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("README.md"), "# Readme\n").unwrap();
            let mut out = Vec::new();
            let mut input = answers.as_bytes();
            project_prompt(&p, &mut Io { out: &mut out, input: &mut input, interactive, version: None, dry_run: false }, dir).unwrap();
            String::from_utf8(out).unwrap()
        };
        let vaults = || crate::config::Config::load(&home.join("atlas.json")).unwrap().vaults.into_iter().map(|v| v.name).collect::<Vec<_>>();
        let quiet = ask(&base.join("work").join("quiet"), "", false);
        assert!(quiet.contains("add-project --vault") && !quiet.contains("[Y]es"), "{quiet}");
        assert!(offered_projects(&p).is_empty(), "no terminal: not recorded as asked");
        let yes = ask(&base.join("work").join("alpha"), "\n", true);
        assert!(yes.contains("? [Y]es / [o]ther folder / [n]o: ") && yes.contains("vault alpha-notes:"), "{yes}");
        assert!(home.join("vaults").join("alpha-notes").join(VAULT_NOTE).is_file());
        let other = base.join("elsewhere").join("beta-vault");
        let o = ask(&base.join("work").join("beta"), &format!("o\n{}\n", other.display()), true);
        assert!(o.contains("Folder: ") && other.join(VAULT_NOTE).is_file(), "{o}");
        let no = ask(&base.join("work").join("gamma"), "n\n", true);
        assert!(no.contains("kept as it is"), "{no}");
        assert_eq!(vaults(), ["notes", "alpha-notes", "beta-notes"]);
        assert_eq!(offered_projects(&p).len(), 3, "each answer is recorded");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn orders_backups_by_stamp_then_number() {
        assert_eq!(backup_key("settings.20261031-120000.json"), Some(("20261031-120000".into(), 0)));
        assert_eq!(backup_key("atlas.20261031-120000-2.absent"), Some(("20261031-120000".into(), 2)));
        assert!(backup_key("settings.20261031-120000.json") < backup_key("settings.20261031-120000-1.json"));
    }
}
