//! Path confinement: the only way a request turns into a file system path.
//!
//! Every tool addresses a note as `vault` + a relative path. This module
//! checks the relative path lexically, then resolves it against the vault's
//! canonical root and checks again after canonicalisation, so a symlink or
//! junction inside a vault cannot lead out of it or into a dot folder.

use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// The longest relative path a request may carry, in bytes.
pub const MAX_REL_PATH: usize = 512;

/// One vault the server was launched with.
#[derive(Debug, Clone)]
pub struct Vault {
    /// The short name requests use (`perf`, `campaign`, ...).
    pub name: String,
    /// The vault's folder name. Obsidian uses it as the vault name in
    /// `obsidian://` links, so cross-vault links are built from it.
    pub folder: String,
    /// The canonical root directory. Every resolved path must stay under it.
    pub root: PathBuf,
}

/// The fixed set of vaults, given on the command line at launch and never
/// changed afterwards.
#[derive(Debug, Clone)]
pub struct Vaults {
    list: Vec<Vault>,
}

/// What a relative path must name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A note: the last component must end in `.md`.
    Note,
    /// A folder: may be empty (the vault root).
    Folder,
}

fn valid_short_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Vaults {
    /// Builds the vault set from `(short name, root path)` pairs.
    ///
    /// # Arguments
    ///
    /// * `specs` - one pair per `--vault` argument. Short names are 1 to 32
    ///   characters of `[A-Za-z0-9_-]`.
    ///
    /// # Returns
    ///
    /// The vault set, each root canonicalised.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the list is empty, a name is malformed,
    /// two vaults share a name or a folder name, or a root is not an existing
    /// directory.
    pub fn new(specs: &[(String, PathBuf)]) -> Result<Vaults> {
        if specs.is_empty() {
            return Err(Error::InvalidArgument("at least one --vault NAME=PATH is required".into()));
        }
        let mut list: Vec<Vault> = Vec::with_capacity(specs.len());
        for (name, path) in specs {
            if !valid_short_name(name) {
                return Err(Error::InvalidArgument(format!("vault name {name:?} must be 1 to 32 characters of A-Z a-z 0-9 _ -")));
            }
            let root = fs::canonicalize(path).map_err(|e| Error::InvalidArgument(format!("vault {name}: {}: {e}", path.display())))?;
            if !root.is_dir() {
                return Err(Error::InvalidArgument(format!("vault {name}: {} is not a directory", path.display())));
            }
            let folder = root
                .file_name()
                .and_then(|f| f.to_str())
                .ok_or_else(|| Error::InvalidArgument(format!("vault {name}: root has no UTF-8 folder name")))?
                .to_string();
            if list.iter().any(|v| v.name == *name) {
                return Err(Error::InvalidArgument(format!("vault name {name} given twice")));
            }
            if list.iter().any(|v| v.folder.eq_ignore_ascii_case(&folder)) {
                return Err(Error::InvalidArgument(format!("two vaults share the folder name {folder}")));
            }
            list.push(Vault { name: name.clone(), folder, root });
        }
        Ok(Vaults { list })
    }

    /// All vaults, in launch order.
    pub fn all(&self) -> &[Vault] {
        &self.list
    }

    /// Looks a vault up by short name, or failing that by folder name.
    ///
    /// # Arguments
    ///
    /// * `name` - the short name (`perf`) or the folder name
    ///   (`Research Log`).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no served vault has that name.
    pub fn get(&self, name: &str) -> Result<&Vault> {
        self.list.iter().find(|v| v.name == name).or_else(|| self.list.iter().find(|v| v.folder == name)).ok_or_else(|| {
            let names: Vec<&str> = self.list.iter().map(|v| v.name.as_str()).collect();
            Error::NotFound(format!("vault {name:?}; served vaults are {}", names.join(", ")))
        })
    }

    /// Looks a vault up by its folder name, as found in an `obsidian://` link.
    /// An exact match wins; otherwise a case-insensitive one is accepted.
    pub fn by_folder(&self, folder: &str) -> Option<&Vault> {
        self.list.iter().find(|v| v.folder == folder).or_else(|| self.list.iter().find(|v| v.folder.eq_ignore_ascii_case(folder)))
    }
}

/// The names Windows maps to devices in any folder: the DOS devices, the
/// ports (with the superscript digits Windows also accepts), and the
/// console buffers.
const RESERVED: [&str; 30] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4",
    "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "COM\u{b9}", "COM\u{b2}", "COM\u{b3}", "LPT\u{b9}", "LPT\u{b2}", "LPT\u{b3}", "CONIN$", "CONOUT$",
];

/// True when a path component would be served (the rules of
/// [`check_component`]): used to filter changed paths the watcher reports.
pub fn component_ok(c: &str) -> bool {
    check_component(c).is_ok()
}

fn check_component(c: &str) -> Result<()> {
    if c.is_empty() {
        return Err(Error::PathRejected("empty path component (doubled or trailing separator)".into()));
    }
    if c == "." || c == ".." {
        return Err(Error::PathRejected(format!("{c:?} components are not allowed")));
    }
    if c.starts_with('.') {
        return Err(Error::PathRejected(format!("{c:?}: dot files and dot folders are not served")));
    }
    // Every component, folders and the note alike: a note's name is `.md`,
    // to which only these rules of the deny-list apply.
    if crate::sources::is_denied_folder(c) {
        return Err(Error::PathRejected(format!("{c:?}: .env, key and credential files are never read or written (the deny-list)")));
    }
    if c.ends_with('.') || c.ends_with(' ') {
        return Err(Error::PathRejected(format!("{c:?}: a component may not end in a dot or a space")));
    }
    if let Some(bad) = c.chars().find(|ch| matches!(ch, '<' | '>' | '"' | '|' | '?' | '*') || ch.is_control()) {
        return Err(Error::PathRejected(format!("{c:?}: character {bad:?} is not allowed")));
    }
    let stem = c.split('.').next().unwrap_or(c).trim_end();
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return Err(Error::PathRejected(format!("{c:?} is a reserved device name")));
    }
    Ok(())
}

/// Checks a relative path lexically and splits it into components.
///
/// # Arguments
///
/// * `rel` - the path from the request. `/` and `\` both separate components.
/// * `kind` - [`Kind::Note`] requires a final `.md` component; [`Kind::Folder`]
///   accepts the empty string for the vault root and one trailing `/`.
///
/// # Returns
///
/// The components, in order.
///
/// # Errors
///
/// [`Error::PathRejected`] for an absolute path, a drive prefix or any `:`
/// (which also covers NTFS `:stream` names), NUL or other control characters,
/// `.` or `..` components, any component starting with `.` (so `.obsidian`,
/// `.git` and `.trash` are unreachable), Windows-reserved names and
/// characters, trailing dots or spaces, a path over [`MAX_REL_PATH`] bytes, or
/// a note path not ending in `.md`.
pub fn split_rel(rel: &str, kind: Kind) -> Result<Vec<String>> {
    if rel.len() > MAX_REL_PATH {
        return Err(Error::PathRejected(format!("path longer than {MAX_REL_PATH} bytes")));
    }
    if rel.contains('\0') {
        return Err(Error::PathRejected("NUL in path".into()));
    }
    if rel.contains(':') {
        return Err(Error::PathRejected("':' is not allowed (drive prefixes and streams)".into()));
    }
    if rel.starts_with('/') || rel.starts_with('\\') {
        return Err(Error::PathRejected("absolute paths are not allowed; give a path relative to the vault".into()));
    }
    let mut trimmed = rel;
    if kind == Kind::Folder {
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(t) = trimmed.strip_suffix('/').or_else(|| trimmed.strip_suffix('\\')) {
            trimmed = t;
        }
    }
    let parts: Vec<String> = trimmed.split(['/', '\\']).map(str::to_string).collect();
    for p in &parts {
        check_component(p)?;
    }
    if kind == Kind::Note {
        let last = parts.last().map(String::as_str).unwrap_or("");
        if !last.ends_with(".md") || last.len() <= 3 {
            return Err(Error::PathRejected("only .md notes can be read or written".into()));
        }
    }
    Ok(parts)
}

/// The path of `full` relative to `root`, with `/` separators.
///
/// # Errors
///
/// [`Error::PathRejected`] if `full` is not under `root`, or any component
/// of the remainder is a dot name or not UTF-8.
fn rel_under(root: &Path, full: &Path) -> Result<String> {
    let rest = full.strip_prefix(root).map_err(|_| Error::PathRejected("path resolves outside the vault".into()))?;
    let mut out = Vec::new();
    for c in rest.components() {
        match c {
            Component::Normal(s) => {
                let s = s.to_str().ok_or_else(|| Error::PathRejected("path is not UTF-8".into()))?;
                if s.starts_with('.') {
                    return Err(Error::PathRejected("path resolves into a dot folder".into()));
                }
                if crate::sources::is_denied_folder(s) {
                    return Err(Error::PathRejected("path resolves to a denied file (.env, keys, credentials)".into()));
                }
                out.push(s.to_string());
            }
            _ => return Err(Error::PathRejected("path resolves outside the vault".into())),
        }
    }
    Ok(out.join("/"))
}

/// A path that passed confinement.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The absolute file system path, under the vault's canonical root.
    pub full: PathBuf,
    /// The path relative to the vault root with `/` separators, after
    /// canonicalisation (so it carries the on-disk case).
    pub rel: String,
}

fn walk_dirs(vault: &Vault, dirs: &[String], create: bool) -> Result<PathBuf> {
    let mut cur = vault.root.clone();
    for d in dirs {
        let next = cur.join(d);
        match fs::symlink_metadata(&next) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound && create => {
                fs::create_dir(&next).or_else(|e| if e.kind() == ErrorKind::AlreadyExists { Ok(()) } else { Err(e) })?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(Error::NotFound(format!("folder {d:?} in vault {}", vault.name)));
            }
            Err(e) => return Err(e.into()),
        }
        let canon = fs::canonicalize(&next)?;
        rel_under(&vault.root, &canon)?;
        if !canon.is_dir() {
            return Err(Error::PathRejected(format!("{d:?} is not a folder")));
        }
        cur = canon;
    }
    Ok(cur)
}

/// Resolves an existing note.
///
/// # Arguments
///
/// * `vault` - the vault the request named.
/// * `rel` - the note path relative to the vault root.
///
/// # Returns
///
/// The canonical path and its vault-relative form.
///
/// # Errors
///
/// Everything [`split_rel`] refuses; [`Error::NotFound`] if the note does not
/// exist; [`Error::PathRejected`] if it resolves outside the vault, into a dot
/// folder, to something other than a regular `.md` file.
pub fn resolve_existing(vault: &Vault, rel: &str) -> Result<Resolved> {
    let parts = split_rel(rel, Kind::Note)?;
    let (file, dirs) = parts.split_last().ok_or_else(|| Error::PathRejected("empty path".into()))?;
    let dir = walk_dirs(vault, dirs, false)?;
    let candidate = dir.join(file);
    let mut first_miss = None;
    let mut attempt = 0u32;
    let (full, is_file) = loop {
        match look_up(&dir, file, &candidate) {
            Ok(found) => break found,
            // Windows can miss a note's name for a moment while another
            // process replaces it by rename, at any of the lookups. Look
            // again before calling it missing.
            Err(e) if e.kind() == ErrorKind::NotFound && first_miss.get_or_insert_with(Instant::now).elapsed() < REPLACE_GAP => {
                attempt += 1;
                if attempt <= 4 {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(Error::NotFound(format!("note {rel:?} in vault {}", vault.name)));
            }
            Err(e) => return Err(e.into()),
        }
    };
    let rel = rel_under(&vault.root, &full)?;
    if !rel.ends_with(".md") {
        return Err(Error::PathRejected("resolves to a file that is not a .md note".into()));
    }
    if !is_file {
        return Err(Error::PathRejected("not a regular file".into()));
    }
    Ok(Resolved { full, rel })
}

/// How long [`resolve_existing`] keeps looking for a note after a lookup
/// says it is missing, before it reports [`Error::NotFound`].
///
/// A rename that replaces a note on NTFS is atomic for the note's contents
/// but not for its name: an open by name or a directory search that lands
/// inside the rename can report the file missing, and so can a second lookup
/// straight after the first succeeded (issue #71). This holds for
/// `MoveFileExW` and for a POSIX-semantics `FileRenameInfoEx` alike, so the
/// writer cannot close it and the reader has to. The gap is one rename, but
/// a writer preempted inside it holds it for a scheduler quantum: up to
/// about 32 ms was measured with the machine four times oversubscribed.
/// The budget covers that with margin. Only a note that is really missing
/// pays it in full, on an error path.
const REPLACE_GAP: Duration = Duration::from_millis(100);

/// One lookup of a note: its full path inside `dir`, and whether it (or the
/// file a link leads to) is a regular file. Every step can report
/// [`ErrorKind::NotFound`] for a note being replaced; [`resolve_existing`]
/// retries those.
fn look_up(dir: &Path, file: &str, candidate: &Path) -> std::io::Result<(PathBuf, bool)> {
    let meta = fs::symlink_metadata(candidate)?;
    let full = leaf(dir, file, candidate, &meta)?;
    let is_file = if meta.file_type().is_symlink() { fs::metadata(&full)?.is_file() } else { meta.is_file() };
    Ok((full, is_file))
}

/// The full path of the last component of a note path, inside a folder that
/// [`walk_dirs`] already canonicalised and confined.
///
/// A link is followed (canonicalised) so the caller checks where it leads.
/// Anything else cannot leave `dir`, so it is not canonicalised: that would
/// open the file, and a note another process is replacing at that moment
/// (temp file renamed over it, [`crate::fsio::replace_atomic`]) can resolve
/// through the handle to the replaced file, which NTFS has already moved to
/// `\$Extend\$Deleted`, outside the vault. Its on-disk spelling is read from
/// the directory entry instead.
fn leaf(dir: &Path, file: &str, candidate: &Path, meta: &fs::Metadata) -> std::io::Result<PathBuf> {
    if meta.file_type().is_symlink() {
        return fs::canonicalize(candidate);
    }
    Ok(dir.join(crate::platform::names::on_disk(dir, file)?))
}

/// Resolves the path for a note that is about to be created, creating missing
/// parent folders when asked.
///
/// # Arguments
///
/// * `vault` - the vault the request named.
/// * `rel` - the new note's path relative to the vault root.
/// * `create_dirs` - create missing parent folders (each one is checked
///   against the root as it is made).
///
/// # Returns
///
/// The path the note will have. The file itself is not created here.
///
/// # Errors
///
/// Everything [`split_rel`] refuses; [`Error::NotFound`] for a missing parent
/// when `create_dirs` is false; [`Error::AlreadyExists`] if something is
/// already at that path; [`Error::PathRejected`] if a parent resolves outside
/// the vault.
pub fn resolve_new(vault: &Vault, rel: &str, create_dirs: bool) -> Result<Resolved> {
    let parts = split_rel(rel, Kind::Note)?;
    let (file, dirs) = parts.split_last().ok_or_else(|| Error::PathRejected("empty path".into()))?;
    let dir = walk_dirs(vault, dirs, create_dirs)?;
    let full = dir.join(file);
    match fs::symlink_metadata(&full) {
        Ok(_) => return Err(Error::AlreadyExists(format!("note {rel:?} in vault {}", vault.name))),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let dir_rel = rel_under(&vault.root, &dir)?;
    let rel = if dir_rel.is_empty() { file.clone() } else { format!("{dir_rel}/{file}") };
    Ok(Resolved { full, rel })
}

/// Resolves an existing folder.
///
/// # Arguments
///
/// * `vault` - the vault the request named.
/// * `rel` - the folder relative to the vault root; empty for the root.
///
/// # Returns
///
/// The canonical folder path and its vault-relative form (empty for the root).
///
/// # Errors
///
/// Everything [`split_rel`] refuses, [`Error::NotFound`] if it is missing, and
/// [`Error::PathRejected`] if it resolves outside the vault.
pub fn resolve_folder(vault: &Vault, rel: &str) -> Result<Resolved> {
    resolve_folder_with(vault, rel, false)
}

/// Resolves a folder, creating it (and missing parents) if needed, each one
/// checked against the root as it is made.
///
/// # Errors
///
/// As [`resolve_folder`], except that a missing folder is created.
pub fn ensure_folder(vault: &Vault, rel: &str) -> Result<Resolved> {
    resolve_folder_with(vault, rel, true)
}

fn resolve_folder_with(vault: &Vault, rel: &str, create: bool) -> Result<Resolved> {
    let parts = split_rel(rel, Kind::Folder)?;
    let full = walk_dirs(vault, &parts, create)?;
    let rel = rel_under(&vault.root, &full)?;
    Ok(Resolved { full, rel })
}

/// One `.md` file found by [`walk`].
#[derive(Debug, Clone)]
pub struct Found {
    /// Vault-relative path with `/` separators.
    pub rel: String,
    /// File size in bytes.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
}

/// Lists every `.md` note in a vault.
///
/// Dot names are skipped, and symlinks and junctions are not followed, so
/// the walk never leaves the root.
///
/// # Arguments
///
/// * `vault` - the vault to walk.
///
/// # Returns
///
/// Every note whose path would pass [`split_rel`], sorted by path.
///
/// # Errors
///
/// [`Error::Io`] if the root cannot be read. Unreadable subfolders are
/// skipped.
pub fn walk(vault: &Vault) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(vault.root.clone(), String::new())];
    let mut first = true;
    while let Some((dir, prefix)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if first => return Err(e.into()),
            Err(_) => continue,
        };
        first = false;
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            if check_component(&name).is_err() {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            if ft.is_dir() {
                stack.push((entry.path(), rel));
            } else if ft.is_file() && name.ends_with(".md") && name.len() > 3 {
                let Ok(meta) = entry.metadata() else { continue };
                out.push(Found { rel, size: meta.len(), mtime_ns: mtime_ns(&meta) });
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Modification time of a file in nanoseconds since the Unix epoch (0 if the
/// platform does not report one).
pub fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

// ------------------------------------------------------ canonical identity

/// A path without the verbatim prefix `std::fs::canonicalize` adds on
/// Windows: `\\?\C:\x` becomes `C:\x` and `\\?\UNC\host\share\x` becomes
/// `\\host\share\x`. Any other path (and every path elsewhere) is returned
/// unchanged.
pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return p;
    }
    let Some(s) = p.to_str() else { return p };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p,
    }
}

/// Entries the [`canonical`] cache holds before it starts over.
const CANON_CAP: usize = 4096;

fn canon_cache() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// Drops the [`canonical`] cache (tests that move links between calls).
pub fn clear_canonical_cache() {
    if let Ok(mut c) = canon_cache().lock() {
        c.clear();
    }
}

/// A path as the file system names it: absolute (a relative path is taken
/// from the process's working directory), every symlink and junction
/// resolved, `.` and `..` gone, and on Windows without the verbatim prefix
/// (see [`strip_verbatim`]) and with the on-disk case. A path that does not
/// exist yet is its deepest existing ancestor canonicalised with the rest
/// appended, so a file about to be written has the same identity as after.
/// When nothing of it exists the absolute path comes back unchanged.
///
/// This is what makes two spellings of one folder compare equal: on macOS
/// `/var` and `/tmp` are links into `/private`, so a temporary folder named
/// through `$TMPDIR` and the same folder named by `getcwd` differ as text.
///
/// Results are cached per process (keyed by the absolute path as given),
/// so the hooks pay the file system calls once per path. A link created or
/// moved later is not seen until [`clear_canonical_cache`].
pub fn canonical(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(d) => d.join(p),
            Err(_) => return p.to_path_buf(),
        }
    };
    if let Some(hit) = canon_cache().lock().ok().and_then(|m| m.get(&abs).cloned()) {
        return hit;
    }
    let out = canonical_uncached(&abs);
    if let Ok(mut m) = canon_cache().lock() {
        if m.len() >= CANON_CAP {
            m.clear();
        }
        m.insert(abs, out.clone());
    }
    out
}

fn canonical_uncached(abs: &Path) -> PathBuf {
    let comps: Vec<Component<'_>> = abs.components().collect();
    for k in (1..=comps.len()).rev() {
        // A bare drive prefix (`C:`) names that drive's working directory,
        // not its root: never a stand-in for an ancestor.
        if matches!(comps[k - 1], Component::Prefix(_)) {
            continue;
        }
        let head: PathBuf = comps[..k].iter().collect();
        let Ok(c) = fs::canonicalize(&head) else { continue };
        let mut out = strip_verbatim(c);
        for t in &comps[k..] {
            match t {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                c => out.push(c.as_os_str()),
            }
        }
        return out;
    }
    abs.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_resolves_the_existing_part_and_keeps_the_rest() {
        let base = std::env::temp_dir().join(format!("atlas-canon-{}", std::process::id()));
        fs::create_dir_all(base.join("a")).unwrap();
        let real = strip_verbatim(fs::canonicalize(&base).unwrap());
        assert_eq!(canonical(&base), real);
        assert_eq!(canonical(&base.join("a").join("not-yet").join("x.md")), real.join("a").join("not-yet").join("x.md"));
        assert_eq!(canonical(&base.join("not-yet").join("..").join("a")), real.join("a"));
        assert!(canonical(&base).is_absolute());
        if cfg!(windows) {
            assert!(!canonical(&base).to_string_lossy().starts_with(r"\\?\"), "no verbatim prefix");
            assert_eq!(strip_verbatim(PathBuf::from(r"\\?\C:\w\x")), PathBuf::from(r"C:\w\x"));
            assert_eq!(strip_verbatim(PathBuf::from(r"\\?\UNC\host\share\x")), PathBuf::from(r"\\host\share\x"));
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn lexical_rejections() {
        let bad = [
            "/etc/passwd.md",
            "\\x.md",
            "C:/x.md",
            "C:x.md",
            "a/../b.md",
            "../b.md",
            "./b.md",
            "a//b.md",
            ".obsidian/app.md",
            "x/.git/c.md",
            ".trash/x.md",
            "a.md:stream",
            "a\0.md",
            "x.txt",
            "x.md.",
            "x.md ",
            "CON.md",
            "aux/x.md",
            "a|b.md",
            "\\\\server\\share\\x.md",
            ".md",
            "COM\u{b9}.md",
            "lpt\u{b3}/x.md",
            "CONIN$.md",
            "conout$.md",
        ];
        for b in bad {
            assert!(split_rel(b, Kind::Note).is_err(), "accepted {b:?}");
        }
        assert_eq!(split_rel("Ideas/PI-001 X.md", Kind::Note).unwrap(), vec!["Ideas", "PI-001 X.md"]);
        assert_eq!(split_rel("a\\b.md", Kind::Note).unwrap(), vec!["a", "b.md"]);
        assert!(split_rel("", Kind::Folder).unwrap().is_empty());
        assert_eq!(split_rel("Ideas/", Kind::Folder).unwrap(), vec!["Ideas"]);
        assert!(split_rel(&"a".repeat(600), Kind::Folder).is_err());
    }

    fn scratch_vault(tag: &str) -> (PathBuf, Vault) {
        let dir = std::env::temp_dir().join(format!("atlas-paths-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Threads")).unwrap();
        let v = Vaults::new(&[("v".to_string(), dir.clone())]).unwrap();
        let vault = v.all()[0].clone();
        (dir, vault)
    }

    /// A note another process is replacing (temp file renamed over it) must
    /// keep resolving inside the vault. Canonicalising the note itself opened
    /// a handle that could land on the replaced file, which NTFS had already
    /// moved to `\$Extend\$Deleted`: "path resolves outside the vault" about
    /// 2% of the time under contention. The name itself can also go missing
    /// for a moment inside the rename, which read as "not found" or "not a
    /// regular file" once in a loaded workspace run (issue #71);
    /// [`REPLACE_GAP`] covers that, and the ignored stress test below loads
    /// it hard enough to show it.
    #[test]
    fn a_note_being_replaced_still_resolves_inside_the_vault() {
        let (dir, vault) = scratch_vault("replace");
        let note = dir.join("Threads").join("D-001 Topic.md");
        fs::write(&note, b"v0").unwrap();
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let replaced = std::sync::Arc::new(AtomicU32::new(0));
        let writer = {
            let (stop, replaced, note) = (stop.clone(), replaced.clone(), note.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let n = replaced.load(Ordering::Relaxed);
                    crate::fsio::replace_atomic(&note, format!("v{n}").as_bytes()).unwrap();
                    replaced.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        // Until both sides have done enough: resolves are cheap, and a
        // replace syncs, so either can be the slow one on a given system.
        let start = std::time::Instant::now();
        let (mut resolves, mut bad) = (0u32, Vec::new());
        // Windows also answers a differently cased request with the stored spelling.
        let request = if cfg!(windows) { "threads/D-001 topic.md" } else { "Threads/D-001 Topic.md" };
        while (resolves < 20_000 || replaced.load(Ordering::Relaxed) < 200) && start.elapsed().as_secs() < 120 {
            match resolve_existing(&vault, request) {
                Ok(r) => {
                    if !r.full.starts_with(&vault.root) || r.rel != "Threads/D-001 Topic.md" {
                        bad.push(format!("{} {}", r.full.display(), r.rel));
                    }
                }
                Err(e) => bad.push(e.to_string()),
            }
            resolves += 1;
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        let replaced = replaced.load(Ordering::Relaxed);
        let _ = fs::remove_dir_all(&dir);
        assert!(replaced >= 200 && resolves >= 20_000, "only {resolves} resolves and {replaced} replacements in 120 s");
        assert!(bad.is_empty(), "{} of {resolves} resolves failed while the note was replaced {replaced} times; first: {}", bad.len(), bad[0]);
    }

    /// The heavy form of the test above: several notes, each replaced by its
    /// own writer and resolved by several readers at once, so the machine is
    /// loaded the way a full workspace run loads it. Every failure is tallied
    /// by its message. `ATLAS_STRESS_SECS` sets the length (default 30).
    #[test]
    #[ignore = "stress: seconds of parallel replace and resolve"]
    fn stress_notes_being_replaced_still_resolve() {
        use std::collections::BTreeMap;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        let secs: u64 = std::env::var("ATLAS_STRESS_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let (dir, vault) = scratch_vault("stress");
        let stop = Arc::new(AtomicBool::new(false));
        let (resolves, replaces) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let bad: Arc<Mutex<BTreeMap<String, u64>>> = Arc::default();
        let mut threads = Vec::new();
        for i in 0..8 {
            let name = format!("D-{i:03} Topic.md");
            let note = dir.join("Threads").join(&name);
            fs::write(&note, b"v0").unwrap();
            {
                let (stop, replaces) = (stop.clone(), replaces.clone());
                threads.push(std::thread::spawn(move || {
                    let mut n = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        crate::fsio::replace_atomic(&note, format!("v{n}").as_bytes()).unwrap();
                        n += 1;
                        replaces.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for _ in 0..3 {
                let (stop, resolves, bad, vault, name) = (stop.clone(), resolves.clone(), bad.clone(), vault.clone(), name.clone());
                threads.push(std::thread::spawn(move || {
                    let want = format!("Threads/{name}");
                    while !stop.load(Ordering::Relaxed) {
                        let miss = match resolve_existing(&vault, &want) {
                            Ok(r) if r.full.starts_with(&vault.root) && r.rel == want => None,
                            Ok(r) => Some(format!("wrong: {} {}", r.full.display(), r.rel)),
                            Err(e) => Some(e.to_string().replace(&name, "<note>")),
                        };
                        if let Some(m) = miss {
                            *bad.lock().unwrap().entry(m).or_default() += 1;
                        }
                        resolves.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(secs));
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
        let bad = bad.lock().unwrap();
        let failed: u64 = bad.values().sum();
        let (resolves, replaces) = (resolves.load(Ordering::Relaxed), replaces.load(Ordering::Relaxed));
        eprintln!("stress: {resolves} resolves, {replaces} replaces, {failed} failed");
        for (m, n) in bad.iter() {
            eprintln!("  {n:>8}  {m}");
        }
        assert_eq!(failed, 0, "{failed} of {resolves} resolves failed during {replaces} replaces");
    }

    #[test]
    fn missing_and_non_file_notes_are_refused() {
        let (dir, vault) = scratch_vault("kinds");
        assert!(matches!(resolve_existing(&vault, "Threads/none.md"), Err(Error::NotFound(_))));
        fs::create_dir_all(dir.join("Threads").join("dir.md")).unwrap();
        assert!(matches!(resolve_existing(&vault, "Threads/dir.md"), Err(Error::PathRejected(_))));
        let _ = fs::remove_dir_all(&dir);
    }
}
