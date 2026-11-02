//! Link building and resolution across vaults.
//!
//! Inside one vault a link is a wikilink. Between vaults it is an Obsidian
//! URI, `obsidian://open?vault=<folder name>&file=<path without .md>`, with
//! both values percent-encoded (`/` as `%2F`, space as `%20`) and a heading
//! carried as `%23Heading` on the file value, as the Obsidian URI docs give
//! it. The vault name in a URI is the vault's folder name.

use std::collections::HashMap;

use crate::markdown::{LinkKind, RawLink};
use crate::paths::Vaults;

/// Percent-encodes everything except `A-Z a-z 0-9 - _ . ~`.
///
/// Parentheses and `!` are encoded too (unlike `encodeURIComponent`), so the
/// result is safe inside a Markdown link destination.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` escapes. Malformed escapes and invalid UTF-8 are kept as
/// written. `+` is left alone, as Obsidian does.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// What an `obsidian://` URI points at.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UriTarget {
    /// The `vault` value (a vault name, which is its folder name).
    pub vault: Option<String>,
    /// The `file` value, decoded, without `#heading`.
    pub file: Option<String>,
    /// The `path` value (an absolute file system path), decoded.
    pub abs_path: Option<String>,
    /// A `#heading` carried in the file value.
    pub heading: Option<String>,
}

/// Parses an `obsidian://open?...` or `obsidian://vault/<vault>/<file>` URI.
/// Returns `None` for other actions.
pub fn parse_uri(uri: &str) -> Option<UriTarget> {
    let rest = uri.strip_prefix("obsidian://")?;
    let mut t = UriTarget::default();
    let file_raw: Option<String>;
    if let Some(q) = rest.strip_prefix("open?").or_else(|| rest.strip_prefix("open/?")) {
        let mut file = None;
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = percent_decode(v);
            match k {
                "vault" => t.vault = Some(v),
                "file" => file = Some(v),
                "path" => t.abs_path = Some(v),
                _ => {}
            }
        }
        file_raw = file;
    } else {
        let v = rest.strip_prefix("vault/")?;
        let v = percent_decode(v);
        let (vault, file) = v.split_once('/').unwrap_or((v.as_str(), ""));
        t.vault = Some(vault.to_string());
        file_raw = (!file.is_empty()).then(|| file.to_string());
    }
    if let Some(f) = file_raw {
        match f.split_once('#') {
            Some((a, h)) => {
                t.file = Some(a.to_string());
                t.heading = (!h.is_empty()).then(|| h.to_string());
            }
            None => t.file = Some(f),
        }
    }
    Some(t)
}

/// Builds the cross-vault link Obsidian opens with one click.
///
/// # Arguments
///
/// * `vault_folder` - the target vault's folder name.
/// * `path` - the target note's vault-relative path, `.md` included.
/// * `heading` - an optional heading to open at.
/// * `text` - the link text.
pub fn uri_link(vault_folder: &str, path: &str, heading: Option<&str>, text: &str) -> String {
    let file = path.strip_suffix(".md").unwrap_or(path);
    let file = match heading {
        Some(h) => format!("{file}#{h}"),
        None => file.to_string(),
    };
    format!("[{}](obsidian://open?vault={}&file={})", escape_link_text(text), percent_encode(vault_folder), percent_encode(&file))
}

/// Builds a same-vault wikilink: `[[path/without/ext#heading|text]]`. The
/// alias is left out when it equals the target.
pub fn wikilink(path: &str, heading: Option<&str>, text: Option<&str>) -> String {
    let file = path.strip_suffix(".md").unwrap_or(path);
    let mut target = file.to_string();
    if let Some(h) = heading {
        target.push('#');
        target.push_str(h);
    }
    match text {
        Some(t) if t != target => format!("[[{target}|{t}]]"),
        _ => format!("[[{target}]]"),
    }
}

/// Escapes `[` and `]` in Markdown link text.
pub fn escape_link_text(s: &str) -> String {
    s.replace('[', "\\[").replace(']', "\\]")
}

/// The file stem of a vault-relative path: `Ideas/PI-001 X.md` gives
/// `PI-001 X`.
pub fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name)
}

/// The folder of a vault-relative path (empty at the root).
pub fn folder(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

const ATTACHMENT_EXT: [&str; 22] = [
    "png",
    "jpg",
    "jpeg",
    "gif",
    "svg",
    "webp",
    "bmp",
    "avif",
    "pdf",
    "mp3",
    "wav",
    "ogg",
    "m4a",
    "flac",
    "mp4",
    "webm",
    "mov",
    "mkv",
    "canvas",
    "base",
    "excalidraw",
    "pgm",
];

/// True if a link target names an attachment rather than a note.
pub fn is_attachment(target: &str) -> bool {
    let name = target.rsplit('/').next().unwrap_or(target);
    match name.rsplit_once('.') {
        Some((_, ext)) => ATTACHMENT_EXT.iter().any(|a| a.eq_ignore_ascii_case(ext)),
        None => false,
    }
}

/// Where a link landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A note in a served vault: `(vault short name, vault-relative path)`.
    Note(String, String),
    /// An attachment (image, PDF, ...), not tracked.
    Attachment,
    /// No served note matches.
    Missing,
}

/// Resolves link targets against every note in every served vault, the way
/// Obsidian does: case-insensitive paths, bare names by file name (closest
/// folder first, then the shortest path), Markdown links relative to the
/// note first and then to the vault root.
#[derive(Debug, Default)]
pub struct Resolver {
    /// vault -> lowercased path without `.md` -> real path.
    by_path: HashMap<String, HashMap<String, String>>,
    /// vault -> lowercased stem -> real paths, sorted.
    by_stem: HashMap<String, HashMap<String, Vec<String>>>,
    /// vault short name -> folder name.
    folders: Vec<(String, String, String)>,
}

fn lower_no_md(p: &str) -> String {
    let p = p.strip_suffix(".md").or_else(|| p.strip_suffix(".MD")).unwrap_or(p);
    p.to_lowercase()
}

fn normalise(parts: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for c in parts.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            c => out.push(c),
        }
    }
    Some(out.join("/"))
}

impl Resolver {
    /// Builds a resolver.
    ///
    /// # Arguments
    ///
    /// * `vaults` - the served vaults (for folder names and roots).
    /// * `notes` - every indexed note as `(vault short name, path)`.
    pub fn new(vaults: &Vaults, notes: impl IntoIterator<Item = (String, String)>) -> Resolver {
        let mut r = Resolver {
            folders: vaults
                .all()
                .iter()
                .map(|v| (v.name.clone(), v.folder.clone(), v.root.to_string_lossy().replace('\\', "/").to_lowercase()))
                .collect(),
            ..Default::default()
        };
        for (vault, path) in notes {
            r.by_path.entry(vault.clone()).or_default().insert(lower_no_md(&path), path.clone());
            r.by_stem.entry(vault).or_default().entry(stem(&path).to_lowercase()).or_default().push(path);
        }
        for m in r.by_stem.values_mut() {
            for v in m.values_mut() {
                v.sort();
            }
        }
        r
    }

    fn exact(&self, vault: &str, rel: &str) -> Option<String> {
        self.by_path.get(vault)?.get(&lower_no_md(rel)).cloned()
    }

    fn by_name(&self, vault: &str, name: &str, from_folder: &str) -> Option<String> {
        let cands = self.by_stem.get(vault)?.get(&name.to_lowercase())?;
        if let Some(same) = cands.iter().find(|p| folder(p) == from_folder) {
            return Some(same.clone());
        }
        cands.iter().min_by_key(|p| (p.matches('/').count(), p.len(), (*p).clone())).cloned()
    }

    fn wiki(&self, vault: &str, target: &str, from: &str) -> Option<String> {
        let from_folder = folder(from);
        if target.contains('/') {
            let t = target.trim_start_matches('/');
            if let Some(p) = self.exact(vault, t) {
                return Some(p);
            }
            if let Some(n) = normalise(&format!("{from_folder}/{t}")) {
                if let Some(p) = self.exact(vault, &n) {
                    return Some(p);
                }
            }
            let suffix = format!("/{}", lower_no_md(t));
            return self.by_path.get(vault)?.iter().filter(|(k, _)| k.ends_with(&suffix)).map(|(_, v)| v.clone()).min();
        }
        self.by_name(vault, stem(target), from_folder)
    }

    fn vault_by_folder(&self, name: &str) -> Option<&str> {
        self.folders
            .iter()
            .find(|(_, f, _)| f == name)
            .or_else(|| self.folders.iter().find(|(_, f, _)| f.eq_ignore_ascii_case(name)))
            .map(|(n, _, _)| n.as_str())
    }

    /// Resolves one link written in note `from_path` of vault `from_vault`.
    ///
    /// # Returns
    ///
    /// The target note and the heading the link names, if any.
    pub fn resolve(&self, from_vault: &str, from_path: &str, link: &RawLink) -> (Resolution, Option<String>) {
        match link.kind {
            LinkKind::Wiki | LinkKind::Embed => {
                if is_attachment(&link.target) {
                    return (Resolution::Attachment, link.heading.clone());
                }
                let r = self.wiki(from_vault, &link.target, from_path);
                (r.map(|p| Resolution::Note(from_vault.to_string(), p)).unwrap_or(Resolution::Missing), link.heading.clone())
            }
            LinkKind::Md => {
                if is_attachment(&link.target) {
                    return (Resolution::Attachment, link.heading.clone());
                }
                let rel = normalise(&format!("{}/{}", folder(from_path), link.target));
                let hit =
                    rel.and_then(|r| self.exact(from_vault, &r)).or_else(|| self.exact(from_vault, link.target.trim_start_matches('/')));
                (hit.map(|p| Resolution::Note(from_vault.to_string(), p)).unwrap_or(Resolution::Missing), link.heading.clone())
            }
            LinkKind::Uri => {
                let Some(u) = parse_uri(&link.target) else { return (Resolution::Missing, None) };
                if let Some(abs) = &u.abs_path {
                    let abs_l = abs.replace('\\', "/").to_lowercase();
                    for (name, _, root) in &self.folders {
                        if let Some(rest) = abs_l.strip_prefix(&format!("{root}/")) {
                            let hit = self.exact(name, rest).map(|p| Resolution::Note(name.clone(), p));
                            return (hit.unwrap_or(Resolution::Missing), u.heading);
                        }
                    }
                    return (Resolution::Missing, u.heading);
                }
                let Some(vault) = u.vault.as_deref().and_then(|v| self.vault_by_folder(v)) else {
                    return (Resolution::Missing, u.heading);
                };
                let Some(file) = u.file.as_deref() else { return (Resolution::Missing, u.heading) };
                if is_attachment(file) {
                    return (Resolution::Attachment, u.heading);
                }
                let hit = self.exact(vault, file).or_else(|| if file.contains('/') { None } else { self.by_name(vault, file, "") });
                (hit.map(|p| Resolution::Note(vault.to_string(), p)).unwrap_or(Resolution::Missing), u.heading)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_round_trip() {
        let s = "Ideas/PI-001 Cache (LRU) & flip#Head";
        let e = percent_encode(s);
        assert_eq!(e, "Ideas%2FPI-001%20Cache%20%28LRU%29%20%26%20flip%23Head");
        assert_eq!(percent_decode(&e), s);
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn uri_forms() {
        let u = parse_uri("obsidian://open?vault=my%20vault&file=path%2Fto%2Fnote%23Head").unwrap();
        assert_eq!(u.vault.as_deref(), Some("my vault"));
        assert_eq!(u.file.as_deref(), Some("path/to/note"));
        assert_eq!(u.heading.as_deref(), Some("Head"));
        let v = parse_uri("obsidian://vault/my vault/my note").unwrap();
        assert_eq!(v.vault.as_deref(), Some("my vault"));
        assert_eq!(v.file.as_deref(), Some("my note"));
        assert!(parse_uri("obsidian://new?vault=x").is_none());
        let link = uri_link("Archive Box", "Maps/Ridge (v2).md", Some("Layout"), "Ridge [v2]");
        assert_eq!(link, "[Ridge \\[v2\\]](obsidian://open?vault=Archive%20Box&file=Maps%2FRidge%20%28v2%29%23Layout)");
    }
}
