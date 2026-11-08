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

/// Whether [`replace_atomic`] could write `target`, changing nothing: a temporary
/// file can be made beside it, and it is no folder and (on Windows, where the
/// rename would fail) not read-only.
pub fn can_replace(target: &Path) -> bool {
    if target.is_dir() || (cfg!(windows) && fs::metadata(target).is_ok_and(|m| m.permissions().readonly())) {
        return false;
    }
    match write_temp(target, b"") {
        Ok(tmp) => {
            let _ = fs::remove_file(tmp);
            true
        }
        Err(_) => false,
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

/// How long a change waits for another atlas process changing the same file.
pub const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// An exclusive advisory lock guarding the read, backup and write of one
/// file; released when dropped or when the process ends, however it ends.
///
/// The lock is on `<file>.lock`, a dedicated file that is never replaced or
/// removed: the guarded file itself is renamed over by [`replace_atomic`], so
/// a lock on it would guard a file nothing else opens again. A second lock
/// in one process waits on the first, on every OS, so a command takes it once,
/// at its entry, and passes the guard down.
#[derive(Debug)]
pub struct FileLock {
    _file: Option<File>,
}

/// The lock file guarding `target`.
pub fn lock_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    target.with_file_name(name)
}

impl FileLock {
    /// Takes the lock on `target`, waiting up to [`LOCK_WAIT`].
    ///
    /// # Errors
    ///
    /// As [`FileLock::acquire_within`].
    pub fn acquire(target: &Path) -> std::result::Result<FileLock, String> {
        FileLock::acquire_within(target, LOCK_WAIT)
    }

    /// Takes the lock on `target`, waiting up to `wait` and saying so once on
    /// stderr. A file system without locks gets one stderr line and goes on.
    ///
    /// # Errors
    ///
    /// A message naming the lock file when it cannot be opened or stays held
    /// past `wait`: the caller writes nothing.
    pub fn acquire_within(target: &Path, wait: std::time::Duration) -> std::result::Result<FileLock, String> {
        let path = lock_path(target);
        if let Some(d) = path.parent() {
            fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let start = std::time::Instant::now();
        let mut said = false;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(FileLock { _file: Some(file) }),
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(e)) if e.kind() == ErrorKind::Unsupported => {
                    eprintln!("atlas: this file system has no file locks, so {} guards nothing; going on without it", path.display());
                    return Ok(FileLock { _file: None });
                }
                Err(fs::TryLockError::Error(e)) => return Err(format!("{}: {e}", path.display())),
            }
            if start.elapsed() >= wait {
                return Err(format!("{}: another atlas command held this lock for {} s; nothing written. Run the command again when it has finished", path.display(), wait.as_secs()));
            }
            if !said {
                said = true;
                eprintln!("atlas: waiting for {}: another atlas command is changing {}", path.display(), target.display());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
