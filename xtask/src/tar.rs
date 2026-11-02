//! Extracts the tar archive `git archive --format=tar` writes: ustar
//! headers, the pax global header carrying the commit id (skipped, as tar
//! skips it), pax extended headers for long paths, directories and regular
//! files. A symbolic link is refused: the tracked tree holds none, and a
//! release must not quietly drop one.

use std::path::{Component, Path};

fn field(h: &[u8], at: usize, len: usize) -> &[u8] {
    let f = &h[at..at + len];
    let end = f.iter().position(|&b| b == 0).unwrap_or(len);
    &f[..end]
}

fn octal(h: &[u8], at: usize, len: usize) -> Result<u64, String> {
    let s = std::str::from_utf8(field(h, at, len)).map_err(|_| "tar: a numeric field is not text".to_string())?;
    let s = s.trim_matches(|c| c == ' ' || c == '\0');
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| format!("tar: bad octal field {s:?}"))
}

/// The `path` record of a pax extended header, if any.
fn pax_path(data: &[u8]) -> Result<Option<String>, String> {
    let mut rest = data;
    let mut path = None;
    while !rest.is_empty() {
        let sp = rest.iter().position(|&b| b == b' ').ok_or("tar: bad pax record")?;
        let len: usize = std::str::from_utf8(&rest[..sp]).ok().and_then(|s| s.parse().ok()).ok_or("tar: bad pax record length")?;
        let rec = rest.get(sp + 1..len).ok_or("tar: pax record past its header")?;
        let rec = rec.strip_suffix(b"\n").unwrap_or(rec);
        if let Some(v) = rec.strip_prefix(b"path=") {
            path = Some(String::from_utf8(v.to_vec()).map_err(|_| "tar: a path is not UTF-8")?);
        }
        rest = &rest[len..];
    }
    Ok(path)
}

/// A relative path with no `..`, so nothing lands outside `dest`.
fn safe(name: &str) -> Result<&Path, String> {
    let p = Path::new(name);
    if p.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
        Ok(p)
    } else {
        Err(format!("tar: unsafe path {name:?}"))
    }
}

/// Extracts `archive` into `dest` (created if missing).
pub fn extract(archive: &[u8], dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut at = 0;
    let mut long_name: Option<String> = None;
    while at + 512 <= archive.len() {
        let h = &archive[at..at + 512];
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let size = octal(h, 124, 12)? as usize;
        let mode = octal(h, 100, 8)?;
        let kind = h[156];
        let data = archive.get(at + 512..at + 512 + size).ok_or("tar: an entry runs past the end")?;
        at += 512 + size.div_ceil(512) * 512;
        let name = match long_name.take() {
            Some(n) => n,
            None => {
                let base = String::from_utf8(field(h, 0, 100).to_vec()).map_err(|_| "tar: a name is not UTF-8")?;
                let prefix = String::from_utf8(field(h, 345, 155).to_vec()).map_err(|_| "tar: a prefix is not UTF-8")?;
                if prefix.is_empty() {
                    base
                } else {
                    format!("{prefix}/{base}")
                }
            }
        };
        match kind {
            b'g' => {}
            b'x' => long_name = pax_path(data)?,
            b'5' => {
                let p = dest.join(safe(&name)?);
                std::fs::create_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            }
            b'0' | 0 => {
                let p = dest.join(safe(&name)?);
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
                }
                std::fs::write(&p, data).map_err(|e| format!("{}: {e}", p.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode as u32 & 0o777))
                        .map_err(|e| format!("{}: {e}", p.display()))?;
                }
                let _ = mode;
            }
            b'2' => return Err(format!("tar: {name} is a symbolic link, which the release does not carry")),
            k => return Err(format!("tar: {name}: unsupported entry type {:?}", k as char)),
        }
    }
    Ok(())
}
