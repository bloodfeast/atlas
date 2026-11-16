//! The rules template: a starter `CLAUDE.md` and `.claude/atlas-rules.md`
//! that `atlas --setup` offers for the folder it runs in, and that `atlas
//! settings rules-template [DIR] [--yes]` writes into an existing project.
//!
//! The template is written only where the folder has no rules yet, and
//! never over a file: both files are created with `create_new`, so a file
//! that appears after the check is left as it is. `atlas settings undo`
//! removes the files atlas wrote, each only while its bytes are still the
//! ones atlas wrote.
//!
//! The `CLAUDE.md` template holds a placeholder line for atlas's Compact
//! instructions block: while atlas's compaction is on, its PreCompact hook
//! returns the same text, so the placeholder is dropped; while it is off,
//! the block ([`super::compact_block`]) goes in its place, markers included.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::{backup, compact_block, compaction_on, label_file, note_label, Io, Paths};

/// The `CLAUDE.md` template, written into the project as `CLAUDE.md`.
pub const RULES_CLAUDE_MD: &str = include_str!("../../templates/CLAUDE.md");
/// The agent, build and safety rules, written as `.claude/atlas-rules.md`.
pub const RULES_FILE: &str = include_str!("../../templates/atlas-rules.md");
/// The line of [`RULES_CLAUDE_MD`] the Compact instructions block replaces.
pub const COMPACT_PLACEHOLDER: &str = "<!-- atlas:compact-block -->\n";

/// The backup stem of a rules template write (see `atlas settings undo`).
pub const STEM: &str = "rules";

/// What `atlas --setup` says without a terminal.
pub const SETUP_NO_TERMINAL: &str = "Nothing went wrong. No rules template was written because there is no terminal here to answer in (input is not a terminal), and atlas writes it only on a yes. To be asked, run `atlas --setup` in a terminal.";
/// What `atlas settings rules-template` says without a terminal or --yes.
pub const COMMAND_NO_TERMINAL: &str = "Nothing went wrong. No rules template was written because there is no terminal here to answer in (input is not a terminal). To choose, run `atlas settings rules-template` in a terminal, or write it with `atlas settings rules-template --yes`.";

/// The `CLAUDE.md` text to write: the Compact instructions block in the
/// placeholder's place while compaction is off, else no block at all (the
/// hook supplies the text, and a copy would cost its tokens every session).
pub fn claude_md_text(compaction_on: bool) -> String {
    if compaction_on {
        // The placeholder and the blank line after it.
        RULES_CLAUDE_MD.replacen(&format!("{COMPACT_PLACEHOLDER}\n"), "", 1)
    } else {
        RULES_CLAUDE_MD.replacen(COMPACT_PLACEHOLDER, &compact_block(), 1)
    }
}

/// The folders whose `CLAUDE.md` files apply to `dir`: `dir` and each
/// folder above it up to its repository's root (`dir` alone outside one).
fn rule_folders(dir: &Path) -> Vec<PathBuf> {
    let dir = crate::paths::canonical(dir);
    let mut out = Vec::new();
    for d in dir.ancestors() {
        out.push(d.to_path_buf());
        if d.join(".git").exists() {
            return out;
        }
    }
    vec![dir]
}

/// The first rules file found for `dir`, if any: a `CLAUDE.md`,
/// `.claude/CLAUDE.md` or `CLAUDE.local.md` in `dir` or a folder above it
/// inside its repository; `.claude/atlas-rules.md`; the intent file the
/// configuration names for the folder's project; an `INTENT.md` at the
/// repository's top level (or its `.claude` folder).
pub fn existing_rules(p: &Paths, dir: &Path) -> Option<PathBuf> {
    let folders = rule_folders(dir);
    let mut candidates: Vec<PathBuf> = Vec::new();
    for d in &folders {
        candidates.extend(["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"].iter().map(|n| d.join(n)));
    }
    candidates.push(dir.join(".claude").join("atlas-rules.md"));
    let intent = p
        .atlas_config
        .as_deref()
        .and_then(|c| crate::config::Config::load(c).ok())
        .and_then(|c| crate::project::active(&c, dir, None).ok().flatten())
        .and_then(|a| a.spec.intent);
    candidates.extend(intent);
    if let Some(top) = folders.last() {
        candidates.push(top.join("INTENT.md"));
        candidates.push(top.join(".claude").join("INTENT.md"));
    }
    candidates.into_iter().find(|f| f.is_file())
}

/// What [`write_template`] did.
#[derive(Debug, Default)]
pub struct Written {
    /// The files it created, in order, with their bytes' SHA-256.
    pub files: Vec<(PathBuf, String)>,
    /// The `.claude` folder, when it created it.
    pub made_dir: Option<PathBuf>,
    /// A file that appeared after the check, left as it is.
    pub left: Option<PathBuf>,
    /// A write that failed for another reason.
    pub error: Option<String>,
}

/// Creates one file that must not exist yet: `Ok(false)` when it does.
fn create_new(path: &Path, text: &str) -> Result<bool, String> {
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(text.as_bytes()).map_err(at)?;
            f.sync_all().map_err(at)?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(at(e)),
    }
}

/// Creates `.claude/atlas-rules.md`, then `CLAUDE.md`, in `dir`, each only
/// if it does not exist. A rules file that exists stops it before
/// `CLAUDE.md` (whose last line would import a file atlas did not write);
/// nothing is ever truncated, appended to or replaced.
pub fn write_template(dir: &Path, claude_md: &str) -> Written {
    let mut w = Written::default();
    let folder = dir.join(".claude");
    if !folder.is_dir() {
        match std::fs::create_dir(&folder) {
            Ok(()) => w.made_dir = Some(folder.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                w.error = Some(format!("{}: {e}", folder.display()));
                return w;
            }
        }
    }
    for (path, text) in [(folder.join("atlas-rules.md"), RULES_FILE), (dir.join("CLAUDE.md"), claude_md)] {
        match create_new(&path, text) {
            Ok(true) => w.files.push((path, crate::sha256::hex(text.as_bytes()))),
            Ok(false) => {
                w.left = Some(path);
                return w;
            }
            Err(e) => {
                w.error = Some(e);
                return w;
            }
        }
    }
    w
}

/// The undo record beside a backup: one line per file atlas wrote (`file`,
/// its SHA-256, its path) and the folder it made (`dir`, its path).
fn record_file(b: &Path) -> PathBuf {
    PathBuf::from(format!("{}.files", b.display()))
}

/// Shows the offer for `dir`, asks once (or writes with `yes`), and writes
/// nothing where the folder has rules. Returns false when a write failed.
///
/// # Errors
///
/// A message when the undo record cannot be made (nothing is written then).
pub fn rules_template_prompt(p: &Paths, io: &mut Io, dir: &Path, yes: bool, no_terminal: &str) -> Result<bool, String> {
    if let Some(f) = existing_rules(p, dir) {
        say!(io, "existing rules left alone: {}", f.display());
        return Ok(true);
    }
    let md = dir.join("CLAUDE.md");
    let rules = dir.join(".claude").join("atlas-rules.md");
    if !yes {
        if !io.interactive {
            say!(io, "{no_terminal}");
            return Ok(true);
        }
        say!(
            io,
            "\n[rules-template] This folder has no CLAUDE.md or rules file: {}\n  What: atlas's rules template, the token-saving rules and working habits ({}) and the agent, build and safety rules ({}, imported by CLAUDE.md's last line). Edit them to suit the project.\n  Writes: two new files; an existing file is never changed. `atlas settings undo` removes them while they are unedited.",
            dir.display(),
            md.display(),
            rules.display()
        );
        let _ = write!(io.out, "  Write the atlas rules template? [y/N] ");
        let _ = io.out.flush();
        let mut line = String::new();
        let _ = io.input.read_line(&mut line);
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            say!(io, "nothing written");
            return Ok(true);
        }
    }
    let text = claude_md_text(compaction_on(p));
    // The backup is made first: without it there would be no undo.
    let (saved, _) = backup(p, STEM, &md, None)?;
    let w = write_template(dir, &text);
    let mut record = String::new();
    if let Some(d) = &w.made_dir {
        record.push_str(&format!("dir\t{}\n", d.display()));
    }
    for (f, hash) in &w.files {
        record.push_str(&format!("file\t{hash}\t{}\n", f.display()));
    }
    if w.files.is_empty() && w.made_dir.is_none() {
        let _ = std::fs::remove_file(&saved);
    } else {
        std::fs::write(record_file(&saved), &record).map_err(|e| format!("{}: {e}", record_file(&saved).display()))?;
        note_label(&saved, &format!("rules-template: wrote the atlas rules template into {}", dir.display()));
    }
    for (f, _) in &w.files {
        say!(io, "[rules-template] wrote {}", f.display());
    }
    if let Some(f) = &w.left {
        say!(io, "[rules-template] {} appeared after the check; left as it is", f.display());
        if w.files.iter().any(|(x, _)| *x == rules) {
            say!(io, "  CLAUDE.md does not import the rules file; add the line @.claude/atlas-rules.md to it to use it");
        }
    }
    if let Some(e) = &w.error {
        say!(io, "[rules-template] not written: {e}");
    }
    if !w.files.is_empty() {
        say!(io, "  `atlas settings undo` removes the files atlas wrote, while they are unedited");
    }
    Ok(w.error.is_none())
}

/// The undo record's lines: the folder atlas made, and the files with
/// their hashes.
fn read_record(b: &Path) -> (Option<PathBuf>, Vec<(String, PathBuf)>) {
    let text = std::fs::read_to_string(record_file(b)).unwrap_or_default();
    let mut dir = None;
    let mut files = Vec::new();
    for l in text.lines() {
        let mut parts = l.splitn(3, '\t');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("dir"), Some(d), None) => dir = Some(PathBuf::from(d)),
            (Some("file"), Some(h), Some(f)) => files.push((h.to_string(), PathBuf::from(f))),
            _ => {}
        }
    }
    (dir, files)
}

/// Undoes a rules template backup: removes each file atlas wrote whose
/// bytes are unchanged (an edited one is left, with a line saying so), then
/// the `.claude` folder if atlas made it and it is empty.
pub fn undo(b: &Path) -> String {
    let (dir, files) = read_record(b);
    let mut out: Vec<String> = Vec::new();
    for (hash, f) in &files {
        match std::fs::read(f) {
            Err(_) => out.push(format!("{} is gone; nothing to remove", f.display())),
            Ok(bytes) if crate::sha256::hex(&bytes) != *hash => {
                out.push(format!("{} was edited after atlas wrote it; left as it is", f.display()))
            }
            Ok(_) => match std::fs::remove_file(f) {
                Ok(()) => out.push(format!("removed {} (atlas wrote it from the rules template)", f.display())),
                Err(e) => out.push(format!("{}: {e}", f.display())),
            },
        }
    }
    // remove_dir removes only an empty folder.
    if let Some(d) = dir {
        if std::fs::remove_dir(&d).is_ok() {
            out.push(format!("removed {} (atlas made it for the rules template)", d.display()));
        }
    }
    let _ = std::fs::remove_file(record_file(b));
    let _ = std::fs::remove_file(label_file(b));
    let _ = std::fs::remove_file(b);
    if out.is_empty() {
        return "the rules template backup names no file; nothing changed".into();
    }
    out.join("\n")
}

/// One line for `atlas settings undo`'s summary.
pub fn summary(b: &Path) -> String {
    let (_, files) = read_record(b);
    let names: Vec<String> = files.iter().map(|(_, f)| f.display().to_string()).collect();
    format!("remove the rules template files atlas wrote: {} (an edited one is left as it is)", names.join(", "))
}
