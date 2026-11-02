//! `atlas settings`: the Claude Code settings atlas offers, and atlas's own
//! switches, each explained, diffed, backed up and reversible.
//!
//! ```text
//! atlas settings list                 every setting: what, cost, measured basis, current value
//! atlas settings set ID VALUE         any allowed value, at any time (warns on risky ones)
//! atlas settings off ID               a Claude Code setting: atlas's value is removed and
//!                                     Claude Code's default applies; an atlas switch: off
//! atlas settings undo                 restores the newest backup (and consumes it)
//! atlas settings offer [--yes ID...]  asks about each Claude Code setting; --yes applies
//!                                     the named ones without asking
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
//! its own folder.

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
}

impl Paths {
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
        Some(Paths { claude_dir, atlas_config: atlas_config.map(Path::to_path_buf), state, db: Some(db.to_path_buf()), user_state: user_state.map(Path::to_path_buf) })
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
            "atlas" | "project" => self.atlas_backups().ok_or_else(|| "no atlas configuration is in use to back up".to_string()),
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
}

macro_rules! say {
    ($io:expr, $($t:tt)*) => {{ let _ = writeln!($io.out, $($t)*); }};
}

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
    let ext = if path.exists() { "json" } else { "absent" };
    let (dst, core) = match core {
        Some(c) => (dir.join(format!("{stem}.{c}.{ext}")), c.to_string()),
        None => {
            let s = stamp();
            // A stamp and number are unique across both kinds and both
            // folders, so undo orders every backup by them alone.
            let folders: Vec<PathBuf> = STEMS.iter().filter_map(|k| p.backups_for(k).ok()).collect();
            let mut n = 0;
            loop {
                let core = if n == 0 { s.clone() } else { format!("{s}-{n}") };
                let taken = folders.iter().any(|f| STEMS.iter().any(|k| ["json", "absent"].iter().any(|x| f.join(format!("{k}.{core}.{x}")).exists())));
                if !taken {
                    break (dir.join(format!("{stem}.{core}.{ext}")), core);
                }
                n += 1;
            }
        }
    };
    if ext == "json" {
        std::fs::copy(path, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    } else {
        std::fs::write(&dst, path.display().to_string()).map_err(|e| format!("{}: {e}", dst.display()))?;
    }
    Ok((dst, core))
}

/// The kinds of backup: Claude Code's settings, atlas's configuration, and
/// a project file atlas created (always an `.absent` backup, holding the
/// file's path, so undo removes it).
const STEMS: [&str; 3] = ["settings", "atlas", "project"];

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
fn restore(p: &Paths, io: &mut Io, b: &Path) -> Result<(), String> {
    let name = b.file_name().unwrap_or_default().to_string_lossy().to_string();
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
fn commit(p: &Paths, io: &mut Io, stem: &str, path: &Path, text: &str, label: &str) -> Result<(), String> {
    commit_in(p, io, stem, path, text, label, None).map(|_| ())
}

/// [`commit`], the backup joining the change stamped `core` when given.
/// Returns the stamp.
fn commit_in(p: &Paths, io: &mut Io, stem: &str, path: &Path, text: &str, label: &str, core: Option<&str>) -> Result<String, String> {
    let (saved, core) = backup(p, stem, path, core)?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    crate::fsio::replace_atomic(path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    say!(io, "{label}; backup {}", saved.display());
    Ok(core)
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
fn write_claude(p: &Paths, io: &mut Io, changes: &[(&Item, Option<Value>)], label: &str) -> Result<bool, String> {
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
fn write_atlas(p: &Paths, io: &mut Io, s: &Item, val: Option<&Value>, label: &str) -> Result<bool, String> {
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
pub fn edit_config(p: &Paths, io: &mut Io, keys: &[&str], val: Option<&Value>, label: &str, manual_edit: &str) -> Result<bool, String> {
    edit_config_in(p, io, keys, val, label, manual_edit, None)
}

/// [`edit_config`], the backup joining the change stamped `core` when
/// given (one undo restores both).
fn edit_config_in(p: &Paths, io: &mut Io, keys: &[&str], val: Option<&Value>, label: &str, manual_edit: &str, core: Option<&str>) -> Result<bool, String> {
    let path = p.atlas_config.clone().ok_or("atlas's switches live in its configuration file, and none is in use (--vault flags only)")?;
    let doc = load(&path)?;
    let text = doc.text.clone().unwrap_or_else(|| "{}\n".into());
    let mut want = doc.value.clone();
    set_path(&mut want, keys, val);
    let spliced = splice(&text, keys, val).filter(|t| crate::json::parse(t).ok().as_ref() == Some(&want));
    let Some(new) = spliced else {
        say!(io, "atlas will not rewrite {}: the change could not be made without touching other text.", path.display());
        say!(io, "  make this edit by hand: {manual_edit}");
        return Ok(false);
    };
    if new == text {
        say!(io, "nothing to change");
        return Ok(true);
    }
    let _ = write!(io.out, "{}", diff(&path.display().to_string(), &text, &new));
    commit_in(p, io, "atlas", &path, &new, label, core)?;
    say!(io, "atlas reads its configuration at start: restart Claude Code (or reconnect atlas with /mcp).");
    Ok(true)
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
            return if ok { Ok(Value::String(v.to_string())) } else { Err(format!("{}: needs a version like 0.1.0", s.id)) };
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

fn find<'a>(items: &'a [Item], id: Option<&String>) -> Result<&'a Item, String> {
    let ids: Vec<&str> = items.iter().map(|s| s.id.as_str()).collect();
    match id {
        Some(i) => items.iter().find(|s| s.id == *i).ok_or_else(|| format!("unknown id {i:?}; ids: {}", ids.join(", "))),
        None => Err(format!("name a setting; ids: {}", ids.join(", "))),
    }
}

const HELP: &str = "usage: atlas settings list | set ID VALUE | off ID | undo | offer [--yes ID...] | add-project [--new [--name NAME] | --to PROJECT] [DIR] | migrate-projects NAME";

/// Runs one `atlas settings` command against `p`, talking through `io`.
///
/// # Errors
///
/// A message for the user (an unknown id, a bad value, an unreadable file).
pub fn run(args: &[String], p: &Paths, io: &mut Io) -> Result<ExitCode, String> {
    let items = manifest()?;
    let claude = load(&p.settings())?.value;
    let atlas = atlas_value(p);
    let word = args.first().map(String::as_str).unwrap_or("list");
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
            if let Some((_, w)) = s.warn.iter().find(|(k, _)| k == raw) {
                say!(io, "warning: {w}");
            }
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
            let ok = if s.kind == Kind::Atlas { write_atlas(p, io, s, Some(&v), &label)? } else { write_claude(p, io, &[(s, Some(v))], &label)? };
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        "off" => {
            let s = find(&items, args.get(1))?;
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
            let ok = if s.kind == Kind::Atlas { write_atlas(p, io, s, target.as_ref(), &label)? } else { write_claude(p, io, &[(s, None)], &label)? };
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        "undo" => {
            // The newest change: one file, or every file one command wrote.
            let all = newest_backups(p);
            if all.is_empty() {
                return Err("no atlas backup to restore".into());
            }
            for b in &all {
                restore(p, io, b)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        "offer" => offer(p, io, &items, &claude, &atlas, args.iter().position(|a| a == "--yes").map(|i| &args[i + 1..])),
        "add-project" => {
            let mut new = false;
            let mut to: Option<String> = None;
            let mut name: Option<String> = None;
            let mut dir: Option<PathBuf> = None;
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--new" => new = true,
                    "--to" => to = Some(it.next().ok_or("--to needs a project name")?.clone()),
                    "--name" => name = Some(it.next().ok_or("--name needs a project name")?.clone()),
                    d if !d.starts_with("--") && dir.is_none() => dir = Some(PathBuf::from(d)),
                    other => return Err(format!("add-project: unknown argument {other:?}\n{HELP}")),
                }
            }
            let dir = match dir {
                Some(d) => d,
                None => std::env::current_dir().map_err(|e| format!("the current folder: {e}"))?,
            };
            let ok = if new {
                add_project_new(p, io, &dir, name.as_deref())?
            } else if let Some(t) = to {
                add_to_project(p, io, &dir, &t)?
            } else {
                add_project(p, io, &dir)?
            };
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
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
        for id in ids {
            find(items, Some(id))?;
        }
    }
    let mut todo: Vec<(&Item, Option<Value>)> = Vec::new();
    for s in items.iter().filter(|s| s.kind != Kind::Atlas) {
        let have = current(s, claude, atlas);
        if applied(s, have) {
            say!(io, "[{}] already applied", s.id);
            continue;
        }
        if let (Some(need), Some(have)) = (s.min_version, io.version) {
            if have < need {
                say!(io, "[{}] skipped: needs Claude Code {}.{}.{} or newer", s.id, need.0, need.1, need.2);
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
            say!(io, "\n[{}] {} ({state}; on by default)\n  What: {}\n  Cost: {}\n  Turn off: atlas settings off {}", s.id, s.title, s.what, s.cost, s.id);
        }
    }
    mark_offered(p, items.iter().map(|s| s.id.as_str()));
    if yes.is_none() && !io.interactive {
        say!(io, "\nnothing written: run `atlas settings offer` in a terminal to answer, or `atlas settings offer --yes ID...`");
        return Ok(ExitCode::SUCCESS);
    }
    if todo.is_empty() {
        say!(io, "nothing to change");
        return Ok(ExitCode::SUCCESS);
    }
    let label = format!("applied: {}", todo.iter().map(|(s, _)| s.id.as_str()).collect::<Vec<_>>().join(", "));
    let ok = write_claude(p, io, &todo, &label)?;
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
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

/// The one-time SessionStart line: the Claude Code settings not yet offered
/// and not applied, and atlas's on-by-default switches not yet explained.
/// The ids are marked offered, so it is never repeated; `None` when there is
/// nothing to say (or the state cannot be recorded, so a line is never
/// repeated every session).
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
    if !mark_offered(p, offer.iter().chain(defaults.iter()).map(|s| s.id.as_str())) {
        return None;
    }
    let mut line = String::from("atlas:");
    if !offer.is_empty() {
        let names: Vec<&str> = offer.iter().map(|s| s.short.as_str()).collect();
        let n = offer.len();
        line.push_str(&format!(
            " {n} Claude Code setting{} {} offered ({}); ask the user, then run `atlas settings offer --yes ID` or `atlas settings list`.",
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

// ------------------------------------------------------ the project offer

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
/// the configuration.
pub fn configured_roots(c: &crate::config::Config) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = c.vaults.iter().map(|x| x.path.clone()).chain(c.sources.iter().map(|x| x.root.clone())).collect();
    if let Some(code) = &c.code {
        v.extend(code.roots.iter().map(|r| r.root.clone()));
    }
    v
}

/// Whether a folder is worth offering: a project, not a home folder, a
/// drive root or a folder atlas already indexes (or one inside it).
fn offerable(dir: &Path, roots: &[PathBuf]) -> bool {
    let key = project_key(dir);
    if !dir.is_dir() || key.matches('/').count() < 2 {
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

/// The one-time SessionStart line offering the session's folder as a
/// source, at most once per folder; `None` when it is not offerable, was
/// offered before, or atlas has no configuration file to add it to.
pub fn project_line(p: &Paths, cwd: &Path, roots: &[PathBuf]) -> Option<String> {
    p.atlas_config.as_ref()?;
    if cwd.as_os_str().is_empty() || !offerable(cwd, roots) {
        return None;
    }
    let key = project_key(cwd);
    if offered_projects(p).iter().any(|x| same_folder(x, &key)) || !mark_project(p, &key) {
        return None;
    }
    let name = key.rsplit('/').next().unwrap_or(&key);
    Some(format!("atlas: this project ({name}) is not indexed. Ask the user whether to add it as an atlas source; on yes run `atlas settings add-project` in it, on no do nothing."))
}

/// A configuration name for a folder: its last component, lowercased, with
/// anything but letters, digits and `-` as `-`, made unique against `taken`.
fn project_name(dir: &str, taken: &[String]) -> String {
    let last = dir.rsplit('/').next().unwrap_or("project");
    let mut base: String = last.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    base = base.trim_matches('-').to_string();
    if base.is_empty() {
        base = "project".into();
    }
    let free = |n: &str| !taken.iter().any(|t| t.eq_ignore_ascii_case(n)) && !taken.iter().any(|t| t.eq_ignore_ascii_case(&format!("{n}-code")));
    if free(&base) {
        return base;
    }
    (2..).map(|i| format!("{base}-{i}")).find(|n| free(n)).unwrap_or(base)
}

/// Whether a folder holds Rust or WGSL the code index would read.
fn has_code(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file() || crate::code::walk(dir).is_ok_and(|w| !w.rust.is_empty() || !w.wgsl.is_empty())
}

/// `atlas settings add-project [DIR]`: adds a folder (default: the current
/// one) to atlas's configuration as a source of its Markdown files and,
/// when it holds Rust or WGSL, as a code root; then indexes the source.
/// The code root is indexed by the server's background indexer when it
/// next starts.
///
/// # Errors
///
/// A message when there is no configuration file or it cannot be changed.
pub fn add_project(p: &Paths, io: &mut Io, dir: &Path) -> Result<bool, String> {
    let cfg = p.atlas_config.clone().ok_or("atlas has no configuration file to add the project to (--vault flags only)")?;
    let key = project_key(dir);
    if !Path::new(&key).is_dir() {
        return Err(format!("{key}: not a folder"));
    }
    let c = crate::config::Config::load(&cfg).map_err(|e| e.to_string())?;
    if !offerable(Path::new(&key), &configured_roots(&c)) {
        say!(io, "{key} is already indexed (or is inside a folder that is); nothing to change");
        return Ok(true);
    }
    let mut taken: Vec<String> = c.vaults.iter().map(|v| v.name.clone()).chain(c.sources.iter().map(|s| s.name.clone())).collect();
    if let Some(code) = &c.code {
        taken.extend(code.roots.iter().map(|r| r.name.clone()));
    }
    let name = project_name(&key, &taken);
    let doc = load(&cfg)?.value;
    let mut sources = doc.get("sources").and_then(Value::as_array).cloned().unwrap_or_default();
    sources.push(crate::json!({"name": name, "root": key, "include": ["**/*.md"], "exclude": ["target/**", "node_modules/**"]}));
    let manual = format!("add {{\"name\": \"{name}\", \"root\": \"{key}\", \"include\": [\"**/*.md\"]}} to \"sources\"");
    if !edit_config(p, io, &["sources"], Some(&Value::Array(sources)), &format!("added source {name}"), &manual)? {
        return Ok(false);
    }
    mark_project(p, &key);
    if has_code(Path::new(&key)) {
        let root = crate::json!({"name": format!("{name}-code"), "root": key});
        let doc = load(&cfg)?.value;
        let (keys, val): (Vec<&str>, Value) = match doc.get("code").and_then(|c| c.get("roots")).and_then(Value::as_array) {
            Some(r) => {
                let mut r = r.clone();
                r.push(root);
                (vec!["code", "roots"], Value::Array(r))
            }
            None if doc.get("code").is_some_and(Value::is_object) => (vec!["code", "roots"], Value::Array(vec![root])),
            None => (vec!["code"], crate::json!({"roots": [root]})),
        };
        let manual = format!("add {{\"name\": \"{name}-code\", \"root\": \"{key}\"}} to \"code\".\"roots\"");
        edit_config(p, io, &keys, Some(&val), &format!("added code root {name}-code"), &manual)?;
    }
    // Index the source now; the code root is indexed when the server starts.
    let c = crate::config::Config::load(&cfg).map_err(|e| e.to_string())?;
    let setup = crate::store::AtlasSetup::from_config(&c).map_err(|e| e.to_string())?;
    let vaults = crate::paths::Vaults::new(&c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect::<Vec<_>>()).map_err(|e| e.to_string())?;
    let db = c.db.clone().or_else(|| p.db.clone()).ok_or("no database path is known")?;
    let mut store = crate::store::Store::open_with(&db, vaults, setup).map_err(|e| e.to_string())?;
    let s = store.rescan_sources(Some(&[name.as_str()])).map_err(|e| e.to_string())?;
    say!(io, "indexed source {name}: {} Markdown file{}", s.files, if s.files == 1 { "" } else { "s" });
    Ok(true)
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
    let mut io = Io { out: &mut out, input: &mut input, interactive, version: None };
    project_prompt(&p, &mut io, Path::new(&key))
}

/// The yes/no question itself (see [`offer_project_interactive`]).
fn project_prompt(p: &Paths, io: &mut Io, dir: &Path) -> Result<(), String> {
    say!(io, "\nThis folder is not indexed: {}", dir.display());
    say!(io, "  atlas can add it as a source (its Markdown files, and its Rust and WGSL as a code root).");
    if !io.interactive {
        say!(io, "  To add it later: atlas settings add-project \"{}\"", dir.display());
        return Ok(());
    }
    let _ = write!(io.out, "  Add it? [y/N] ");
    let _ = io.out.flush();
    let mut line = String::new();
    let _ = io.input.read_line(&mut line);
    mark_project(p, &project_key(dir));
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        add_project(p, io, dir)?;
    } else {
        say!(io, "  kept as it is (the notes vault only); `atlas settings add-project` adds it any time");
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

/// The one-time SessionStart line for a folder that is no project's root:
/// the git repositories in it (one level down), and the three answers.
/// Asked once per folder; `None` when the folder is a known root, holds a
/// project file, has no repositories, or was asked about before.
pub fn repos_line(p: &Paths, cwd: &Path, ctx: &OfferCtx) -> Option<String> {
    p.atlas_config.as_ref()?;
    if cwd.as_os_str().is_empty() || !cwd.is_dir() || cwd.join(crate::project::PROJECT_FILE).is_file() {
        return None;
    }
    let key = project_key(cwd);
    if ctx.known.iter().any(|k| same_folder(&project_key(k), &key)) {
        return None;
    }
    let repos: Vec<_> = offered_repos(cwd).into_iter().filter(|(_, d, _)| !ctx.known.iter().any(|k| same_folder(&project_key(k), &project_key(d)))).collect();
    if repos.is_empty() || offered_projects(p).iter().any(|x| same_folder(x, &key)) || !mark_project(p, &key) {
        return None;
    }
    let names: Vec<&str> = repos.iter().map(|r| r.0.as_str()).collect();
    let shown = if names.len() > 12 { format!("{}, +{} more", names[..12].join(", "), names.len() - 12) } else { names.join(", ") };
    let n = repos.len();
    let plural = if n == 1 { "" } else { "s" };
    let folder = key.rsplit('/').next().unwrap_or(&key);
    let existing = if ctx.projects.is_empty() { "none yet".to_string() } else { ctx.projects.join(", ") };
    Some(format!(
        "atlas: this folder ({folder}) is no atlas project and holds {n} git repo{plural} ({shown}). Ask the user once, in one line: new project here with these {n} repo{plural} / add to an existing project ({existing}) / skip. On new run `atlas settings add-project --new \"{key}\"`, on add `atlas settings add-project --to NAME \"{key}\"`, on skip nothing; `atlas settings undo` reverses either."
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

/// `atlas settings add-project --new [--name NAME] [DIR]`: writes
/// `DIR/atlas.project.json` naming the git repositories in DIR (path and
/// remote each), which makes DIR a project root, and registers it in the
/// user configuration as `{name, file}`, so a folder inside one of its
/// repositories' checkouts elsewhere resolves to it and the other projects
/// read its memory folder. One undo removes both. A project file already
/// there is registered when it is not yet.
///
/// # Errors
///
/// A message when the configuration is not project-scoped, the folder holds
/// a project file already or no repositories, or the file cannot be written.
pub fn add_project_new(p: &Paths, io: &mut Io, dir: &Path, name: Option<&str>) -> Result<bool, String> {
    let cfg = p.atlas_config.clone().ok_or("atlas has no configuration file (--vault flags only)")?;
    let c = crate::config::Config::load(&cfg).map_err(|e| e.to_string())?;
    if !c.user.scoped {
        return Err("this configuration has no projects yet: run `atlas settings migrate-projects NAME` first (it moves the current setup into project NAME)".into());
    }
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
        let mut projects = load(&cfg)?.value.get("projects").and_then(Value::as_array).cloned().unwrap_or_default();
        projects.push(crate::json!({"name": pname.clone(), "file": key.clone() + "/" + crate::project::PROJECT_FILE}));
        let manual = format!("add {{\"name\": {:?}, \"file\": {:?}}} to \"projects\"", pname, file.display().to_string());
        return edit_config(p, io, &["projects"], Some(&Value::Array(projects)), &format!("project {pname}: registered"), &manual);
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
    let text = crate::project::project_file_text(&name, dir, &repos);
    let _ = write!(io.out, "{}", diff(&file.display().to_string(), "", &text));
    let core = commit_in(p, io, "project", &file, &text, &format!("project {name}: {} repos", repos.len()), None)?;
    // Registered in the user configuration too, under the same backup
    // stamp (one undo removes both): a folder inside one of its repos'
    // checkouts elsewhere resolves to it, and the other projects read its
    // memory folder for memories marked `scope: user`.
    let mut projects = load(&cfg)?.value.get("projects").and_then(Value::as_array).cloned().unwrap_or_default();
    projects.push(crate::json!({"name": name.clone(), "file": key.clone() + "/" + crate::project::PROJECT_FILE}));
    let manual = format!("add {{\"name\": {:?}, \"file\": {:?}}} to \"projects\"", name, file.display().to_string());
    let registered = edit_config_in(p, io, &["projects"], Some(&Value::Array(projects)), &format!("project {name}: registered"), &manual, Some(&core))?;
    if !registered {
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
        return Ok(true);
    }
    let manual = format!("add the repos of {key} to \"repos\" of project {project:?}");
    let ok = edit_config(p, io, &["projects"], Some(&Value::Array(projects)), &format!("project {project}: added {added} repos"), &manual)?;
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
    let doc = load(&cfg)?;
    let base = cfg.parent().map(Path::to_path_buf).unwrap_or_default();
    let db = p.db.clone().unwrap_or_else(crate::dirs::default_db);
    let Some(new) = crate::project::migrate_value(&doc.value, &base, name, &db, Some(&p.claude_dir)) else {
        say!(io, "{} has projects already (or nothing to move); nothing to change", cfg.display());
        return Ok(true);
    };
    let text = crate::json::to_string_pretty(&new) + "\n";
    crate::config::Config::parse(&text, &base).map_err(|e| format!("the migrated configuration would not load: {e}"))?;
    let old = doc.text.clone().unwrap_or_default();
    let _ = write!(io.out, "{}", diff(&cfg.display().to_string(), &old, &text));
    commit(p, io, "atlas", &cfg, &text, &format!("moved the configuration into project {name}"))?;
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
///
/// # Errors
///
/// A message for the user when the subcommand cannot run.
pub fn cli(args: Vec<String>, config: Option<&Path>, db: &Path, user_state: Option<&Path>) -> Result<ExitCode, String> {
    let p = Paths::detect_in(config, db, user_state).ok_or("no home folder is known (set HOME, USERPROFILE or CLAUDE_CONFIG_DIR)")?;
    let interactive = std::io::stdin().is_terminal();
    // Only an offer that can write needs the version (the min_version gate).
    let writes = interactive || args.iter().any(|a| a == "--yes");
    let version = if args.first().map(String::as_str) == Some("offer") && writes { claude_version() } else { None };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    run(&args, &p, &mut Io { out: &mut out, input: &mut input, interactive, version })
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
    run(&["offer".to_string()], &p, &mut Io { out: &mut out, input: &mut input, interactive, version }).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_is_well_formed() {
        let m = manifest().unwrap();
        let ids: Vec<&str> = m.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["subagent-cache-1h", "goal-checkin-120", "routing", "update", "update-pin"]);
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

    #[test]
    fn orders_backups_by_stamp_then_number() {
        assert_eq!(backup_key("settings.20261031-120000.json"), Some(("20261031-120000".into(), 0)));
        assert_eq!(backup_key("atlas.20261031-120000-2.absent"), Some(("20261031-120000".into(), 2)));
        assert!(backup_key("settings.20261031-120000.json") < backup_key("settings.20261031-120000-1.json"));
    }
}
