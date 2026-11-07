//! Read-only sources: Markdown files outside the vaults that the index reads
//! but no tool ever writes.
//!
//! A source is a canonical root folder plus include globs. Confinement is
//! the same idea as [`crate::paths`], applied to reading only:
//!
//! * the walk never follows a symlink or junction and never enters a dot
//!   folder, so it cannot leave the root;
//! * every file is checked against a hard deny-list ([`is_denied`]) before it
//!   is even stat'ed, and again before it is opened;
//! * a file is opened only after its canonical path is checked to be under
//!   the canonical root;
//! * only text extensions ([`TEXT_EXTENSIONS`]) are indexed.
//!
//! There is no write path: this module has no function that opens a file
//! for writing, and sources are not [`crate::paths::Vault`]s, so no write
//! tool can name one.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::chunk::ChunkRule;
use crate::error::{Error, Result};
use crate::paths::{self, Found};

/// The largest source file that is indexed, in bytes (8 MiB).
pub const MAX_SOURCE: u64 = 8 << 20;

/// File extensions a source may index (compared case-insensitively).
pub const TEXT_EXTENSIONS: &[&str] = &["md", "markdown", "txt"];

/// The deepest folder level a `**` include walks to.
const MAX_DEPTH: usize = 32;

/// The most files one source may hold; a walk past this is an error, so a
/// root set too wide fails loudly instead of indexing half a disk.
pub const MAX_FILES: usize = 20_000;

/// True for file names that are never read or indexed, whatever the
/// configuration says: environment files, keys, certificates and credential
/// stores.
///
/// The list is fixed in code so no configuration can switch it off:
/// `.env`, `.env.*`, `*.env`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`,
/// `*.keystore`, `*.kdbx`, `*.asc`, `*.gpg`, `id_rsa*`, `id_dsa*`, `id_ecdsa*`,
/// `id_ed25519*`, `credentials`, `credentials.*`, `*.credentials`, `.netrc`,
/// `_netrc`, `.npmrc`, `.pypirc`, `secrets.*` and `*.secret`. Matching is
/// case-insensitive.
///
/// The name rules (`id_rsa*` and the other key names, `credentials`,
/// `credentials.*`, `_netrc`, `secrets.*`) do not apply to a Markdown note
/// (`*.md`): a note is never the key or credential file those names guard,
/// so `Secrets.md` and `id_rsa rotation.md` are read. The extension and
/// `.env` rules apply to every name, and a caller checks the name a path
/// resolves to as well as the name given ([`path_denied`]), so a note that
/// is a link to a denied file is still refused. Folder names are checked
/// by [`is_denied_folder`].
///
/// # Arguments
///
/// * `name` - a file name (the last path component).
pub fn is_denied(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if is_denied_folder(&n) {
        return true;
    }
    if n.ends_with(".md") {
        return false;
    }
    ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"].iter().any(|k| n.starts_with(k))
        || n == "credentials"
        || n.starts_with("credentials.")
        || matches!(n.as_str(), ".netrc" | "_netrc" | ".npmrc" | ".pypirc")
        || n.starts_with("secrets.")
}

/// True for folder names on the deny-list: the `.env` and extension rules of
/// [`is_denied`] only (`keys.gpg/`, `prod.env/`), never its name rules, so a
/// `credentials/` or `secrets.d/` folder is walked and each file in it is
/// judged by its own name.
///
/// # Arguments
///
/// * `name` - a folder name (one path component).
pub fn is_denied_folder(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let ext = n.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    n == ".env" || n.starts_with(".env.") || matches!(ext, "env" | "pem" | "key" | "p12" | "pfx" | "jks" | "keystore" | "kdbx" | "asc" | "gpg" | "credentials" | "secret")
}

/// Whether a file atlas is asked to read by path (a CLI file tool, a file
/// Claude Code read) is on the deny-list: its name as given, or the name it
/// resolves to through links, is [`is_denied`].
pub fn path_denied(p: &Path) -> bool {
    let named = |q: &Path| q.file_name().and_then(|n| n.to_str()).is_some_and(is_denied);
    named(p) || std::fs::canonicalize(p).is_ok_and(|c| named(&c))
}

/// What breaks a command into the fragments [`command_names_denied`] judges.
const COMMAND_BREAKS: &str = "|;&()<>$=,:@{}[]";

/// Whether a command, in any shell, names a file on the deny-list. Total and
/// over-matching by design: it never parses the command, so a chain, `sh -c`,
/// `cmd /C`, an unclosed quote or a redirection cannot hide a name it can see,
/// and `grep credentials src` counts too, which costs one digest and no
/// secret. A command can still hide a path (`F=.e; cat ${F}nv`, a link reached
/// after `cd`): this catches named files only.
///
/// The string is read three ways: quotes as separators (`sh -c "cat '.env'"`),
/// quotes deleted (`.e"n"v`), and backslashes and backticks deleted too
/// (`.e\nv`, ``.e`nv``). In each, a fragment's last component is taken at both
/// `/` and `\` on every OS, since `Path::file_name` does not split `\` on Unix.
pub fn command_names_denied(cmd: &str) -> bool {
    let names = |text: &str, quotes: bool| {
        text.split(|c: char| c.is_whitespace() || COMMAND_BREAKS.contains(c) || (quotes && matches!(c, '\'' | '"'))).any(|f| {
            let name = f.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("");
            !name.is_empty() && is_denied(name)
        })
    };
    if names(cmd, true) {
        return true;
    }
    let unquoted = cmd.replace(['\'', '"'], "");
    names(&unquoted, false) || names(&unquoted.replace(['\\', '`'], ""), false)
}

/// Whether a Grep `glob` can only name deny-listed files by its own text: `*.pem`,
/// `.env*`, `*.{pem,key}`. Named files only, as [`command_names_denied`] is.
pub fn glob_names_denied(glob: &str) -> bool {
    let bare = glob.replace(['*', '?'], "");
    command_names_denied(&bare)
        || glob.split('{').skip(1).filter_map(|r| r.split_once('}')).any(|(parts, _)| parts.split(',').any(|p| is_denied(&format!("x.{}", p.trim_start_matches('.')))))
}

/// The words of a command that could be paths, for a check of where links lead:
/// unlike [`command_names_denied`] a drive's colon stays, and `@file` loses its `@`.
pub fn command_path_words(cmd: &str) -> impl Iterator<Item = &str> {
    cmd.split(|c: char| c.is_whitespace() || "|;&()<>=,'\"".contains(c)).map(|w| w.trim_start_matches('@')).filter(|w| !w.is_empty())
}

/// The refusal for a [`path_denied`] file.
pub fn denied_message(p: &Path) -> String {
    format!("{}: atlas never reads .env, key or credential files (the deny-list)", p.display())
}

fn has_text_extension(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, e)| !stem.is_empty() && TEXT_EXTENSIONS.iter().any(|t| e.eq_ignore_ascii_case(t)))
}

/// One read-only source.
#[derive(Debug, Clone)]
pub struct Source {
    /// The short name requests use; unique across vaults and sources.
    pub name: String,
    /// The canonical root folder.
    pub root: PathBuf,
    /// Globs over the path below the root (see [`crate::glob::matches`]); a
    /// file is indexed when any matches. Without `/` a glob matches the file
    /// name, and the walk descends only as deep as the globs reach (`**`
    /// for any depth).
    pub include: Vec<String>,
    /// Globs that drop a file the includes matched.
    pub exclude: Vec<String>,
    /// How the files are cut into sections.
    pub chunk: ChunkRule,
    /// Frontmatter keys whose value names the identifier a file defines.
    pub id_keys: Vec<String>,
    /// When set, every file stem in this source is an identifier of this
    /// kind, defined by its file and mentioned wherever the stem appears as
    /// a whole word.
    pub names_kind: Option<String>,
    depth: usize,
}

impl Source {
    /// Builds a source and checks its root.
    ///
    /// # Arguments
    ///
    /// * `name` - 1 to 32 characters of `[A-Za-z0-9_-]`.
    /// * `root` - an existing folder; canonicalised here.
    /// * `include` - at least one glob, each a relative path pattern.
    /// * `exclude` - globs that drop matched files.
    /// * `chunk` - the sectioning rule.
    /// * `id_keys` - frontmatter keys naming the identifier a file defines.
    /// * `names_kind` - see [`Source::names_kind`].
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a bad name, a missing or non-folder
    /// root, no includes, or a glob that is absolute, climbs with `..`,
    /// starts with `.` in any component, or names a denied file.
    pub fn new(
        name: &str,
        root: &Path,
        include: Vec<String>,
        exclude: Vec<String>,
        chunk: ChunkRule,
        id_keys: Vec<String>,
        names_kind: Option<String>,
    ) -> Result<Source> {
        if name.is_empty() || name.len() > 32 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(Error::InvalidArgument(format!("source name {name:?} must be 1 to 32 characters of A-Z a-z 0-9 _ -")));
        }
        let root = fs::canonicalize(root).map_err(|e| Error::InvalidArgument(format!("source {name}: {}: {e}", root.display())))?;
        if !root.is_dir() {
            return Err(Error::InvalidArgument(format!("source {name}: {} is not a folder", root.display())));
        }
        if include.is_empty() {
            return Err(Error::InvalidArgument(format!("source {name}: give at least one include glob")));
        }
        let mut depth = 0;
        for g in include.iter().chain(&exclude) {
            if g.is_empty() || g.len() > 256 || g.starts_with('/') || g.starts_with('\\') || g.contains(':') || g.contains('\\') {
                return Err(Error::InvalidArgument(format!("source {name}: glob {g:?} must be a relative path pattern with / separators")));
            }
            for part in g.split('/') {
                if part == ".." || part == "." || part.starts_with('.') {
                    return Err(Error::InvalidArgument(format!("source {name}: glob {g:?} may not climb or name a dot path")));
                }
            }
        }
        for g in &include {
            let last = g.rsplit('/').next().unwrap_or(g);
            if !last.contains(['*', '?']) && is_denied(last) {
                return Err(Error::InvalidArgument(format!("source {name}: {g:?} names a file on the deny-list; it is never read")));
            }
            depth = depth.max(if g.contains("**") { MAX_DEPTH } else { g.matches('/').count() });
        }
        Ok(Source { name: name.to_string(), root, include, exclude, chunk, id_keys, names_kind, depth })
    }

    /// True when `rel` is one of this source's files by its globs, the
    /// depth they reach, its extension and the deny-list.
    ///
    /// # Arguments
    ///
    /// * `rel` - a `/`-separated path below the root.
    pub fn wants(&self, rel: &str) -> bool {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        rel.matches('/').count() <= self.depth
            && !is_denied(name)
            && has_text_extension(name)
            && self.include.iter().any(|g| crate::glob::matches(g, rel))
            && !self.exclude.iter().any(|g| crate::glob::matches(g, rel))
    }

    /// Lists the source's files with their size and mtime. Nothing is read.
    ///
    /// # Returns
    ///
    /// Every wanted file, sorted by path. Dot names and denied names are
    /// skipped before any stat, symlinks and junctions are not followed, and
    /// folders deeper than the globs reach are not entered.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the root cannot be read; [`Error::TooLarge`] past
    /// [`MAX_FILES`]. Unreadable subfolders are skipped.
    pub fn walk(&self) -> Result<Vec<Found>> {
        let mut out = Vec::new();
        let mut stack: Vec<(PathBuf, String, usize)> = vec![(self.root.clone(), String::new(), 0)];
        let mut first = true;
        while let Some((dir, prefix, level)) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if first => return Err(e.into()),
                Err(_) => continue,
            };
            first = false;
            for entry in entries.flatten() {
                let Ok(name) = entry.file_name().into_string() else { continue };
                if name.starts_with('.') {
                    continue;
                }
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_symlink() {
                    continue;
                }
                // A folder by the folder rules; a file by its own name (`wants`).
                if if ft.is_dir() { is_denied_folder(&name) } else { is_denied(&name) } {
                    continue;
                }
                let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                if ft.is_dir() {
                    if level < self.depth {
                        stack.push((entry.path(), rel, level + 1));
                    }
                } else if ft.is_file() && self.wants(&rel) {
                    let Ok(meta) = entry.metadata() else { continue };
                    out.push(Found { rel, size: meta.len(), mtime_ns: paths::mtime_ns(&meta) });
                    if out.len() > MAX_FILES {
                        return Err(Error::TooLarge(format!("source {} holds over {MAX_FILES} files; narrow its globs", self.name)));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(out)
    }

    /// Reads one of the source's files.
    ///
    /// # Arguments
    ///
    /// * `rel` - a path from [`Source::walk`].
    ///
    /// # Returns
    ///
    /// The bytes and the file's mtime.
    ///
    /// # Errors
    ///
    /// [`Error::PathRejected`] if the path is not wanted by the source,
    /// names a denied file, has a dot or `..` component, or resolves outside
    /// the root; [`Error::TooLarge`] past [`MAX_SOURCE`]; [`Error::Io`].
    pub fn read(&self, rel: &str) -> Result<(Vec<u8>, i64)> {
        if rel.split('/').any(|c| c.is_empty() || c.starts_with('.') || c.contains(['\\', ':', '\0'])) || !self.wants(rel) {
            return Err(Error::PathRejected(format!("{rel:?} is not a file of source {}", self.name)));
        }
        let full = fs::canonicalize(self.root.join(rel))?;
        let under = full.strip_prefix(&self.root).map_err(|_| Error::PathRejected("path resolves outside the source".into()))?;
        let canon_rel: Vec<&str> = under.components().filter_map(|c| c.as_os_str().to_str()).collect();
        let (last, dirs) = canon_rel.split_last().map_or(("", &[][..]), |(l, d)| (*l, d));
        if canon_rel.iter().any(|c| c.starts_with('.')) || is_denied(last) || last.is_empty() || dirs.iter().any(|d| is_denied_folder(d)) {
            return Err(Error::PathRejected("path resolves into a dot folder or a denied file".into()));
        }
        let mut f = File::open(&full)?;
        let meta = f.metadata()?;
        if !meta.is_file() {
            return Err(Error::PathRejected("not a regular file".into()));
        }
        if meta.len() > MAX_SOURCE {
            return Err(Error::TooLarge(format!("{rel} is {} bytes; sources are capped at {MAX_SOURCE}", meta.len())));
        }
        let mut buf = Vec::with_capacity(meta.len() as usize);
        Read::by_ref(&mut f).take(MAX_SOURCE + 1).read_to_end(&mut buf)?;
        Ok((buf, paths::mtime_ns(&meta)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_glob_naming_denied_files_is_seen() {
        for g in ["*.pem", ".env*", "**/.env*", "*.{pem,key}", "*.{rs,pem}", "{a,b}.{pem,key}", "id_rsa*", "conf/.env.big"] {
            assert!(glob_names_denied(g), "{g}");
        }
        for g in ["", "*.rs", "*.{rs,md}", "src/**", ".envrc"] {
            assert!(!glob_names_denied(g), "{g}");
        }
    }

    #[test]
    fn a_command_naming_a_denied_file_is_seen_however_it_is_written() {
        for c in [
            "cat .env.big",
            "cat \".env.big\"",
            "cat './x y/.env.big'",
            "cat<.env",
            "$(cat .env)",
            "sh -c 'cat .env.big'",
            "bash -lc \"cat '.env'\"",
            "cmd /C type .env.big",
            "powershell -Command gc .\\.env",
            "gc .\\.env",
            "--env-file=.env",
            "curl -d @.env",
            "F=.env; cat $F",
            "cat .env\"",
            "cat ../a/server.pem",
            "cat .env | base64",
            "cd d && cat .env",
            "cat .e\"n\"v",
            "cat .e\\nv",
            "gc .e`nv",
            "type C:\\a\\id_rsa",
            "cat a/b\\.npmrc",
        ] {
            assert!(command_names_denied(c), "{c}");
        }
        for c in ["cat README.md", "cat .envrc", "grep -rn secrets src", "cat src/key.rs", "cargo test --features dev", "", "   ", "\"", "cat"] {
            assert!(!command_names_denied(c), "{c}");
        }
    }

    #[test]
    fn deny_list() {
        for n in [
            ".env",
            ".ENV",
            ".env.local",
            ".env.production",
            "prod.env",
            "server.pem",
            "tls.KEY",
            "id_rsa",
            "id_ed25519.pub",
            "credentials",
            "credentials.json",
            "aws.credentials",
            ".netrc",
            "secrets.toml",
            "store.p12",
        ] {
            assert!(is_denied(n), "{n} should be denied");
        }
        for n in ["notes.md", "environment.md", "keyboard.md", "envelope.txt", "key-ideas.md", "monkey.md", "Secrets.md", "Credentials.md", "id_rsa rotation.md", "credentials.MD"] {
            assert!(!is_denied(n), "{n} should be allowed");
        }
        // The extension and .env rules hold for notes too.
        for n in [".env.md", "notes.md.pem"] {
            assert!(is_denied(n), "{n} should be denied");
        }
        // Folders: only the .env and extension rules.
        for n in ["credentials", "secrets.d", "id_rsa_backups"] {
            assert!(!is_denied_folder(n), "folder {n} should be allowed");
        }
        for n in [".env", "keys.gpg", "prod.env"] {
            assert!(is_denied_folder(n), "folder {n} should be denied");
        }
    }
}
