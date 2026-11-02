//! Capped reads and atomic writes.
//!
//! A replace writes a temporary file in the note's own folder and renames it
//! over the note, so a reader sees the old file or the new one and never a
//! torn one. An exclusive create hard-links the finished temporary file to
//! the note's name, which fails if anything is already there. Temporary
//! files start with `.` so neither the walker nor Obsidian sees them.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

/// The largest note the server reads, in bytes (1 MiB).
pub const MAX_READ: u64 = 1 << 20;
/// The largest text a single write request may carry, in bytes (256 KiB).
pub const MAX_WRITE: usize = 256 << 10;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Reads a whole note, refusing anything over [`MAX_READ`].
///
/// # Arguments
///
/// * `path` - a path that already passed confinement.
///
/// # Returns
///
/// The file's bytes.
///
/// # Errors
///
/// [`Error::TooLarge`] over the cap, [`Error::Io`] if the read fails.
pub fn read_capped(path: &Path) -> Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    if len > MAX_READ {
        return Err(Error::TooLarge(format!("note is {len} bytes; the read cap is {MAX_READ}")));
    }
    let mut buf = Vec::with_capacity(len as usize);
    Read::by_ref(&mut f).take(MAX_READ + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_READ {
        return Err(Error::TooLarge(format!("note grew past the read cap of {MAX_READ} bytes")));
    }
    Ok(buf)
}

/// Reads a note as UTF-8 text, refusing anything over [`MAX_READ`].
///
/// # Errors
///
/// As [`read_capped`], plus [`Error::InvalidArgument`] if the file is not
/// valid UTF-8.
pub fn read_text(path: &Path) -> Result<String> {
    String::from_utf8(read_capped(path)?).map_err(|_| Error::InvalidArgument("note is not valid UTF-8".into()))
}

fn temp_path(target: &Path) -> Result<PathBuf> {
    let dir = target.parent().ok_or_else(|| Error::Io("note has no parent folder".into()))?;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    Ok(dir.join(format!(".vault-mcp-{}-{n}-{nanos}.tmp", std::process::id())))
}

fn write_temp(target: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let tmp = temp_path(target)?;
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let res = f.write_all(bytes).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(tmp)
}

fn check_size(bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 > MAX_READ {
        return Err(Error::TooLarge(format!(
            "the note would be {} bytes; notes are capped at {MAX_READ} so they stay readable",
            bytes.len()
        )));
    }
    Ok(())
}

/// Atomically replaces an existing note's contents.
///
/// # Arguments
///
/// * `target` - a confined path to an existing note.
/// * `bytes` - the complete new contents.
///
/// # Errors
///
/// [`Error::TooLarge`] if the result would exceed [`MAX_READ`]; [`Error::Io`]
/// if the temporary file cannot be written or the rename keeps failing (on
/// Windows a rename is retried briefly while another program holds the file
/// open).
pub fn replace_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    check_size(bytes)?;
    let tmp = write_temp(target, bytes)?;
    let mut attempt = 0;
    loop {
        match fs::rename(&tmp, target) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::PermissionDenied && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
    }
}

/// Creates a new note, failing if anything already exists at the path.
///
/// # Arguments
///
/// * `target` - a confined path that should not exist yet.
/// * `bytes` - the complete contents.
///
/// # Errors
///
/// [`Error::AlreadyExists`] if the path is taken (checked atomically by the
/// file system, so two processes cannot both win); [`Error::TooLarge`] over
/// [`MAX_READ`]; [`Error::Io`] otherwise.
pub fn create_exclusive(target: &Path, bytes: &[u8]) -> Result<()> {
    check_size(bytes)?;
    let tmp = write_temp(target, bytes)?;
    let linked = fs::hard_link(&tmp, target);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(Error::AlreadyExists(format!("{}", target.file_name().map(|f| f.to_string_lossy()).unwrap_or_default())))
        }
        Err(e) if e.kind() == ErrorKind::Unsupported => {
            // File systems without hard links: create_new is still exclusive,
            // only not atomic with respect to the contents.
            let mut f = OpenOptions::new().write(true).create_new(true).open(target).map_err(|e| {
                if e.kind() == ErrorKind::AlreadyExists {
                    Error::AlreadyExists(target.display().to_string())
                } else {
                    e.into()
                }
            })?;
            f.write_all(bytes)?;
            f.sync_all()?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
