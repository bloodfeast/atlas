//! Write-side records: rulings and entries with explicit fields, each
//! rendering its exact export line.
//!
//! ```text
//! atlas ruling --by NAME [--defines ID ...] [--ref REF ...] [--date YYYY-MM-DD] TEXT
//! atlas entry --title TITLE [--ref REF ...] [--date YYYY-MM-DD] [--time HH:MM] TEXT
//! ```
//!
//! A ruling renders `- DATE BY: TEXT`, and when it defines identifiers, the
//! marker `<!-- atlas defines: ID ... -->` after it (invisible when the
//! markdown is rendered). The registry reads the marker as a definition of
//! rank `field`, above every prose position, so atlas never infers what a
//! ruling defines. An entry renders `- DATE TIME TITLE: TEXT`.
//!
//! Each command writes the record to the memory store (a `rule` under
//! `ruling:r`, or a `section` under `entry:s`, with `defines:ID` and
//! `ref:REF` refs), appends the line to its file under a dated heading atlas
//! owns (`## DATE rulings (atlas)`, `## DATE entries (atlas)`), and indexes
//! the file at once. The line never inherits a foreign heading: when the
//! file's last heading is not atlas's heading for that date, a new one is
//! written first.
//!
//! The files: `lean.records.rulings` and `lean.records.entries` in the
//! configuration (relative to its folder), else `INTENT.md` and `LOG.md`
//! in the first vault, so the commands work on an empty project.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::json::Value;
use crate::memory::settings::Settings;
use crate::memory::{Draft, MemoryStore};
use crate::registry::DEFINES_MARK;
use crate::store::Store;

/// A ruling: who ruled what, on which date, defining which identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruling {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// Who ruled.
    pub by: String,
    /// Identifiers the ruling defines.
    pub defines: Vec<String>,
    /// Other records or ids it refers to.
    pub refs: Vec<String>,
    /// The ruling, one line.
    pub text: String,
}

/// A log entry (restore-point style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// `HH:MM` (UTC unless given).
    pub time: String,
    /// The title.
    pub title: String,
    /// The entry, one line.
    pub text: String,
    /// Other records or ids it refers to.
    pub refs: Vec<String>,
}

fn one_line(what: &str, s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        return Err(format!("{what} is empty"));
    }
    if s.contains('\n') || s.contains('\r') {
        return Err(format!("{what} must be one line (its export is one list item)"));
    }
    if s.contains("<!--") || s.contains("-->") {
        return Err(format!("{what} may not hold an HTML comment"));
    }
    Ok(())
}

fn check_date(d: &str) -> Result<(), String> {
    let b = d.as_bytes();
    let ok = b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if ok { Ok(()) } else { Err(format!("date {d:?}: expected YYYY-MM-DD")) }
}

impl Ruling {
    /// Checks the fields render to one unambiguous line.
    ///
    /// # Errors
    ///
    /// A message naming the bad field.
    pub fn validate(&self) -> Result<(), String> {
        check_date(&self.date)?;
        one_line("--by", &self.by)?;
        if self.by.contains(':') {
            return Err("--by may not hold a colon".into());
        }
        one_line("the text", &self.text)?;
        for id in &self.defines {
            if id.is_empty() || id.len() > 128 || id.chars().any(|c| c.is_whitespace()) || id.contains("-->") {
                return Err(format!("--defines {id:?}: one identifier, no spaces"));
            }
        }
        Ok(())
    }

    /// The exact export line (no line break).
    pub fn line(&self) -> String {
        let mut l = format!("- {} {}: {}", self.date, self.by.trim(), self.text.trim());
        if !self.defines.is_empty() {
            l.push_str(&format!(" {DEFINES_MARK} {} -->", self.defines.join(" ")));
        }
        l
    }

    /// The heading atlas owns for this ruling's date.
    pub fn heading(&self) -> String {
        format!("{} rulings (atlas)", self.date)
    }
}

impl Entry {
    /// Checks the fields render to one unambiguous line.
    ///
    /// # Errors
    ///
    /// A message naming the bad field.
    pub fn validate(&self) -> Result<(), String> {
        check_date(&self.date)?;
        let t = self.time.as_bytes();
        if !(t.len() == 5 && t[2] == b':' && t[..2].iter().chain(&t[3..]).all(u8::is_ascii_digit)) {
            return Err(format!("time {:?}: expected HH:MM", self.time));
        }
        one_line("--title", &self.title)?;
        if self.title.contains(':') {
            return Err("--title may not hold a colon".into());
        }
        one_line("the text", &self.text)
    }

    /// The exact export line (no line break).
    pub fn line(&self) -> String {
        format!("- {} {} {}: {}", self.date, self.time, self.title.trim(), self.text.trim())
    }

    /// The heading atlas owns for this entry's date.
    pub fn heading(&self) -> String {
        format!("{} entries (atlas)", self.date)
    }
}

/// The text of the last heading outside fenced code, if any.
fn last_heading(text: &str) -> Option<String> {
    let mut fence: Option<&str> = None;
    let mut last = None;
    for line in text.lines() {
        let t = line.trim_start();
        let mark = if t.starts_with("```") { Some("```") } else if t.starts_with("~~~") { Some("~~~") } else { None };
        match (fence, mark) {
            (None, Some(m)) => {
                fence = Some(m);
                continue;
            }
            (Some(f), Some(m)) if f == m => {
                fence = None;
                continue;
            }
            (Some(_), _) => continue,
            _ => {}
        }
        if let Some((_, h)) = crate::chunk::heading(line) {
            last = Some(h.trim().to_string());
        }
    }
    last
}

/// What [`append_under`] adds to a file holding `text`: the line, after a
/// new `## heading` unless the file's last heading is already `heading`.
pub fn suffix(text: &str, heading: &str, line: &str) -> String {
    let mut s = String::new();
    if !text.is_empty() && !text.ends_with('\n') {
        s.push('\n');
    }
    if last_heading(text).as_deref() != Some(heading) {
        if !text.is_empty() && !text.ends_with("\n\n") {
            s.push('\n');
        }
        s.push_str(&format!("## {heading}\n\n"));
    }
    s.push_str(line);
    s.push('\n');
    s
}

/// Appends `line` to `path` under `## heading` (written first when the
/// file's last heading is another one), creating the file if needed.
/// Returns the 1-based line the record landed on.
///
/// # Errors
///
/// A message when the file cannot be read or appended to.
pub fn append_under(path: &Path, heading: &str, line: &str) -> Result<usize, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let add = suffix(&text, heading, line);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    // Append mode: another writer appending at the same time is not lost.
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(add.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{text}{add}").lines().count())
}

/// The file rulings (`which` = `rulings`) or entries go to.
///
/// # Errors
///
/// A message when neither the configuration nor a vault names one.
pub fn target(which: &str, lean: Option<&Value>, config: Option<&Path>, store: &Store) -> Result<PathBuf, String> {
    if let Some(p) = lean.and_then(|l| l.get("records")).and_then(|r| r.get(which)).and_then(Value::as_str) {
        let p = PathBuf::from(p);
        return Ok(if p.is_absolute() { p } else { config.and_then(Path::parent).unwrap_or(Path::new(".")).join(p) });
    }
    let v = store.vaults().all().first().ok_or(format!("no file for {which}: set lean.records.{which} in the configuration, or configure a vault"))?;
    Ok(v.root.join(if which == "rulings" { "INTENT.md" } else { "LOG.md" }))
}

struct Parsed {
    flags: Vec<(String, String)>,
    words: Vec<String>,
}

fn parse(args: Vec<String>, multi: &[&str], single: &[&str]) -> Result<Parsed, String> {
    let mut flags = Vec::new();
    let mut words = Vec::new();
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next() {
        if a == "--" {
            words.extend(it.by_ref());
            break;
        }
        let name = a.strip_prefix("--").map(str::to_string);
        match name {
            Some(n) if multi.contains(&n.as_str()) => {
                let mut any = false;
                while let Some(v) = it.peek().filter(|v| !v.starts_with("--")) {
                    // The text is the last word(s): --defines takes words
                    // until the next flag, so give the text after `--`
                    // or after another flag.
                    flags.push((n.clone(), v.clone()));
                    any = true;
                    it.next();
                    if n != "defines" && n != "ref" {
                        break;
                    }
                }
                if !any {
                    return Err(format!("--{n} needs a value"));
                }
            }
            Some(n) if single.contains(&n.as_str()) => {
                let v = it.next().ok_or(format!("--{n} needs a value"))?;
                flags.push((n, v));
            }
            Some(n) => return Err(format!("unknown flag --{n}")),
            None => words.push(a),
        }
    }
    Ok(Parsed { flags, words })
}

fn today() -> (String, String) {
    let t = crate::discussion::utc_now_ms();
    (t[..10].to_string(), t[11..16].to_string())
}

/// The `atlas ruling` and `atlas entry` subcommands.
///
/// # Arguments
///
/// * `word` - `ruling` or `entry`.
/// * `args` - the words after it.
/// * `store` - the opened index.
/// * `mem` - the memory store settings.
/// * `config` - the configuration file, for relative paths.
///
/// # Errors
///
/// A message for the user.
pub fn cli(word: &str, args: Vec<String>, store: &mut Store, mem: &Settings, config: Option<&Path>) -> Result<ExitCode, String> {
    let usage = "usage: atlas ruling --by NAME [--defines ID ...] [--ref REF ...] [--date YYYY-MM-DD] -- TEXT\n       atlas entry --title TITLE [--ref REF ...] [--date YYYY-MM-DD] [--time HH:MM] -- TEXT";
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{usage}");
        return Ok(if args.is_empty() { ExitCode::FAILURE } else { ExitCode::SUCCESS });
    }
    let p = parse(args, &["defines", "ref"], &["by", "title", "date", "time"]).map_err(|e| format!("{e}\n{usage}"))?;
    let get = |k: &str| p.flags.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let all = |k: &str| -> Vec<String> { p.flags.iter().filter(|(n, _)| n == k).map(|(_, v)| v.clone()).collect() };
    let text = p.words.join(" ");
    let (d, t) = today();
    let date = get("date").unwrap_or(d);
    let refs = all("ref");
    let (line, heading, which, kind, scope, prefix, title, author, defines) = match word {
        "ruling" => {
            let r = Ruling { date, by: get("by").ok_or(format!("--by is required\n{usage}"))?, defines: all("defines"), refs: refs.clone(), text };
            r.validate()?;
            let title: String = r.text.chars().take(120).collect();
            (r.line(), r.heading(), "rulings", "rule", "rulings", "ruling:r", title, r.by.clone(), r.defines.clone())
        }
        _ => {
            let e = Entry { date, time: get("time").unwrap_or(t), title: get("title").ok_or(format!("--title is required\n{usage}"))?, text, refs: refs.clone() };
            e.validate()?;
            (e.line(), e.heading(), "entries", "section", "entries", "entry:s", e.title.clone(), "atlas".to_string(), Vec::new())
        }
    };
    let path = target(which, mem.lean.as_ref(), config, store)?;
    let at = append_under(&path, &heading, &line)?;
    // A canonical Windows root carries the verbatim prefix; it is noise here.
    let shown = path.display().to_string().trim_start_matches(r"\\?\").to_string();
    // The record, with its fields as refs.
    let mut ms = MemoryStore::open(&mem.store).map_err(|e| e.to_string())?;
    let mut w = ms.writer().map_err(|e| e.to_string())?;
    let key = w.next_key(prefix, 5).map_err(|e| e.to_string())?;
    let draft = Draft {
        title,
        meta: format!("file: {shown}\nline: {at}\n"),
        body: format!("{line}\n"),
        reason: format!("atlas {word}"),
        author,
        client: "atlas-cli".into(),
        provenance: None,
        time: None,
    };
    w.create(&key, kind, "", scope, &draft, None).map_err(|e| e.to_string())?;
    let mut rr: Vec<(String, String)> = defines.iter().map(|d| ("defines".to_string(), d.clone())).collect();
    rr.extend(refs.iter().map(|r| ("ref".to_string(), r.clone())));
    w.add_refs(&key, &rr).map_err(|e| e.to_string())?;
    w.commit().map_err(|e| e.to_string())?;
    println!("{key} -> {shown}:{at} under \"## {heading}\"");
    println!("{line}");
    // Indexed now, so lookup sees it before any other tool runs.
    store.rescan().map_err(|e| e.to_string())?;
    store.rescan_sources(None).map_err(|e| e.to_string())?;
    let mut ok = true;
    for id in &defines {
        let l = store.lookup(id, 1, crate::store::DefText::None).map_err(|e| e.to_string())?;
        match l.definitions.iter().find(|h| h.def.as_deref() == Some("field") && h.line == at as i64) {
            Some(h) => println!("indexed: {id} defined at {}/{}:{} ({})", h.source, h.path, h.line, h.heading_path),
            None => {
                ok = false;
                println!("not indexed: {id}: {shown} is not inside a configured vault or source, so lookup does not see it");
            }
        }
    }
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ruling() -> Ruling {
        Ruling { date: "2026-10-31".into(), by: "Owner".into(), defines: vec!["ALPHA-LEAN".into()], refs: vec![], text: "the alpha plan is adopted.".into() }
    }

    #[test]
    fn renders_exact_lines() {
        assert_eq!(ruling().line(), "- 2026-10-31 Owner: the alpha plan is adopted. <!-- atlas defines: ALPHA-LEAN -->");
        let mut r = ruling();
        r.defines.clear();
        assert_eq!(r.line(), "- 2026-10-31 Owner: the alpha plan is adopted.");
        let e = Entry { date: "2026-10-31".into(), time: "18:05".into(), title: "Checkpoint".into(), text: "two lanes running.".into(), refs: vec![] };
        assert_eq!(e.line(), "- 2026-10-31 18:05 Checkpoint: two lanes running.");
        assert!(e.validate().is_ok());
    }

    #[test]
    fn rejects_fields_that_would_not_round_trip() {
        let mut r = ruling();
        r.text = "two\nlines".into();
        assert!(r.validate().is_err());
        let mut r = ruling();
        r.by = "a: b".into();
        assert!(r.validate().is_err());
        let mut r = ruling();
        r.date = "31/10/2026".into();
        assert!(r.validate().is_err());
        let mut r = ruling();
        r.text = "hide <!-- this -->".into();
        assert!(r.validate().is_err());
        let mut r = ruling();
        r.defines = vec!["TWO WORDS".into()];
        assert!(r.validate().is_err());
    }

    #[test]
    fn a_new_heading_unless_the_last_one_is_ours() {
        let h = "2026-10-31 rulings (atlas)";
        assert_eq!(suffix("", h, "- x"), "## 2026-10-31 rulings (atlas)\n\n- x\n");
        assert_eq!(suffix("# T\n\n## 2026-10-23 OTHER\n\n- old", h, "- x"), "\n\n## 2026-10-31 rulings (atlas)\n\n- x\n");
        assert_eq!(suffix("## 2026-10-31 rulings (atlas)\n\n- a\n", h, "- x"), "- x\n");
        // A heading inside fenced code is not the last heading.
        assert_eq!(suffix("## 2026-10-31 rulings (atlas)\n\n```\n## fake\n```\n", h, "- x"), "- x\n");
        // Another day's atlas heading is foreign to this one.
        assert_eq!(suffix("## 2026-10-30 rulings (atlas)\n\n- a\n", h, "- x"), "\n## 2026-10-31 rulings (atlas)\n\n- x\n");
    }
}
