//! Linux change notification: inotify, one watch per directory under the
//! root (inotify does not watch subtrees), read on one thread that also
//! polls a pipe so [`Backend::stop`] can wake it. Every `unsafe` block is a
//! plain libc call (std already links libc) whose invariants are stated
//! beside it.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_short, c_ulong, c_void, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::watch::Sink;

const IN_CLOEXEC: c_int = 0o2_000_000;
const O_CLOEXEC: c_int = 0o2_000_000;
const IN_MODIFY: u32 = 0x0000_0002;
const IN_ATTRIB: u32 = 0x0000_0004;
const IN_CLOSE_WRITE: u32 = 0x0000_0008;
const IN_MOVED_FROM: u32 = 0x0000_0040;
const IN_MOVED_TO: u32 = 0x0000_0080;
const IN_CREATE: u32 = 0x0000_0100;
const IN_DELETE: u32 = 0x0000_0200;
const IN_DELETE_SELF: u32 = 0x0000_0400;
const IN_MOVE_SELF: u32 = 0x0000_0800;
const IN_Q_OVERFLOW: u32 = 0x0000_4000;
const IN_IGNORED: u32 = 0x0000_8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_ISDIR: u32 = 0x4000_0000;
const MASK: u32 = IN_MODIFY
    | IN_ATTRIB
    | IN_CLOSE_WRITE
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_CREATE
    | IN_DELETE
    | IN_DELETE_SELF
    | IN_MOVE_SELF
    | IN_ONLYDIR;
const POLLIN: c_short = 0x0001;
/// The deepest folder level watched; deeper changes are caught by the
/// safety-net walk.
const MAX_DEPTH: usize = 32;

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

extern "C" {
    fn inotify_init1(flags: c_int) -> c_int;
    fn inotify_add_watch(fd: c_int, path: *const c_char, mask: u32) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn close(fd: c_int) -> c_int;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
    fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
}

/// One watching thread over one root.
#[derive(Debug)]
pub(crate) struct Backend {
    /// The write end of the wake pipe; closed by [`Backend::stop`].
    wake: c_int,
    thread: Option<JoinHandle<()>>,
}

struct Watch {
    fd: c_int,
    root: PathBuf,
    /// Watch descriptor to the folder's path below the root.
    dirs: HashMap<c_int, String>,
}

impl Watch {
    /// Adds a watch on `rel` and every folder below it (no dot folders, no
    /// symlinks). Returns false when the kernel refused a watch (the
    /// per-user limit): the caller marks the root failed.
    fn add_tree(&mut self, rel: &str, depth: usize) -> bool {
        let full = if rel.is_empty() { self.root.clone() } else { self.root.join(rel) };
        let Ok(c) = CString::new(full.as_os_str().as_bytes()) else { return true };
        // SAFETY: `c` is a NUL-terminated path that outlives the call; `fd`
        // is an open inotify descriptor owned by this thread.
        let wd = unsafe { inotify_add_watch(self.fd, c.as_ptr(), MASK) };
        if wd < 0 {
            // A folder that vanished meanwhile is fine; anything else at the
            // root is not.
            return !rel.is_empty();
        }
        self.dirs.insert(wd, rel.to_string());
        if depth >= MAX_DEPTH {
            return true;
        }
        let Ok(entries) = std::fs::read_dir(&full) else { return true };
        for e in entries.flatten() {
            let Ok(name) = e.file_name().into_string() else { continue };
            if name.starts_with('.') {
                continue;
            }
            if e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()) {
                let sub = if rel.is_empty() { name } else { format!("{rel}/{name}") };
                if !self.add_tree(&sub, depth + 1) {
                    return false;
                }
            }
        }
        true
    }
}

impl Backend {
    pub(crate) fn spawn(root: &Path, index: usize, sink: Arc<dyn Sink>) -> Option<Backend> {
        // SAFETY: plain call; the result is checked.
        let fd = unsafe { inotify_init1(IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let mut w = Watch { fd, root: root.to_path_buf(), dirs: HashMap::new() };
        let mut pipe = [0 as c_int; 2];
        // SAFETY: `pipe` has room for the two descriptors pipe2 writes.
        let ok = unsafe { pipe2(pipe.as_mut_ptr(), O_CLOEXEC) } == 0;
        if !ok || !w.add_tree("", 0) || w.dirs.is_empty() {
            // SAFETY: each descriptor was opened above and is closed once.
            unsafe {
                close(fd);
                if ok {
                    close(pipe[0]);
                    close(pipe[1]);
                }
            }
            return None;
        }
        let wake_read = pipe[0];
        let thread = std::thread::Builder::new()
            .name(format!("atlas-watch-{index}"))
            .spawn(move || {
                run(&mut w, wake_read, index, &*sink);
                // SAFETY: the inotify descriptor and the pipe's read end are
                // owned by this thread and closed once, here.
                unsafe {
                    close(w.fd);
                    close(wake_read);
                }
            })
            .ok();
        if thread.is_none() {
            // SAFETY: the thread never started, so its descriptors are
            // closed here instead, once.
            unsafe {
                close(fd);
                close(pipe[0]);
                close(pipe[1]);
            }
            return None;
        }
        Some(Backend { wake: pipe[1], thread })
    }

    /// Wakes the thread through the pipe and waits for it to exit.
    pub(crate) fn stop(&mut self) {
        let Some(t) = self.thread.take() else { return };
        let b = 1u8;
        // SAFETY: `wake` is the open write end of the pipe; one byte from a
        // live local.
        unsafe { write(self.wake, (&b as *const u8).cast(), 1) };
        let _ = t.join();
        // SAFETY: the write end is owned here and closed once.
        unsafe { close(self.wake) };
    }
}

fn run(w: &mut Watch, wake: c_int, index: usize, sink: &dyn Sink) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut fds = [PollFd { fd: w.fd, events: POLLIN, revents: 0 }, PollFd { fd: wake, events: POLLIN, revents: 0 }];
        // SAFETY: `fds` is an array of two initialised pollfd records.
        let n = unsafe { poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            continue; // EINTR
        }
        if fds[1].revents != 0 {
            return;
        }
        if fds[0].revents == 0 {
            continue;
        }
        // SAFETY: `buf` is writable for its whole length and outlives the
        // call.
        let got = unsafe { read(w.fd, buf.as_mut_ptr().cast(), buf.len()) };
        if got <= 0 {
            sink.fail(index);
            return;
        }
        let mut paths = Vec::new();
        let mut overflow = false;
        let mut new_dirs = Vec::new();
        for (wd, mask, name) in parse(&buf[..got as usize]) {
            if mask & IN_Q_OVERFLOW != 0 {
                overflow = true;
                continue;
            }
            if mask & IN_IGNORED != 0 {
                w.dirs.remove(&wd);
                continue;
            }
            let Some(dir) = w.dirs.get(&wd) else { continue };
            let rel = match (dir.is_empty(), name.is_empty()) {
                (_, true) => dir.clone(),
                (true, false) => name,
                (false, false) => format!("{dir}/{name}"),
            };
            if mask & IN_ISDIR != 0 && mask & (IN_CREATE | IN_MOVED_TO) != 0 {
                new_dirs.push(rel.clone());
            }
            if !rel.is_empty() {
                paths.push(rel);
            } else {
                overflow = true; // the root itself moved or went
            }
        }
        for d in new_dirs {
            if !w.add_tree(&d, d.matches('/').count() + 1) {
                sink.fail(index);
                return;
            }
        }
        sink.mark(index, paths, overflow);
    }
}

/// Decodes `struct inotify_event` records: wd, mask, cookie, len, then a
/// NUL-padded name of `len` bytes.
fn parse(bytes: &[u8]) -> Vec<(c_int, u32, String)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(h) = bytes.get(at..at + 16) {
        let wd = c_int::from_ne_bytes([h[0], h[1], h[2], h[3]]);
        let mask = u32::from_ne_bytes([h[4], h[5], h[6], h[7]]);
        let len = u32::from_ne_bytes([h[12], h[13], h[14], h[15]]) as usize;
        let name = bytes.get(at + 16..at + 16 + len).unwrap_or(&[]);
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        out.push((wd, mask, String::from_utf8_lossy(&name[..end]).into_owned()));
        at += 16 + len;
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn events_decode() {
        let mut v = Vec::new();
        for (wd, name) in [(1i32, "a.md"), (2, "")] {
            let mut n = name.as_bytes().to_vec();
            if !n.is_empty() {
                n.resize(16, 0);
            }
            v.extend(wd.to_ne_bytes());
            v.extend(0x2u32.to_ne_bytes());
            v.extend(0u32.to_ne_bytes());
            v.extend((n.len() as u32).to_ne_bytes());
            v.extend(n);
        }
        let got = super::parse(&v);
        assert_eq!(got, vec![(1, 2, "a.md".to_string()), (2, 2, String::new())]);
    }
}
