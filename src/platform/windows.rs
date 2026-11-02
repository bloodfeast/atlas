//! Windows change notification: one thread per root blocked in
//! `ReadDirectoryChangesW` over the whole subtree. Every `unsafe` block is a
//! plain FFI call whose invariants are stated beside it.
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::watch::Sink;

type Handle = *mut c_void;
const INVALID_HANDLE_VALUE: isize = -1;
const FILE_LIST_DIRECTORY: u32 = 0x0001;
const FILE_SHARE_ALL: u32 = 0x0001 | 0x0002 | 0x0004;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
// FILE_NAME | DIR_NAME | SIZE | LAST_WRITE | CREATION.
const FILTER: u32 = 0x0001 | 0x0002 | 0x0008 | 0x0010 | 0x0040;
const ERROR_NOTIFY_ENUM_DIR: u32 = 1022;
const ERROR_OPERATION_ABORTED: u32 = 995;
/// 64 KiB: the most a network share accepts.
const BUF_WORDS: usize = 16 * 1024;

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *mut c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
    fn ReadDirectoryChangesW(
        dir: Handle,
        buf: *mut c_void,
        len: u32,
        subtree: i32,
        filter: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
        routine: *mut c_void,
    ) -> i32;
    fn CancelIoEx(h: Handle, overlapped: *mut c_void) -> i32;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLastError() -> u32;
}

/// One watching thread. It owns the directory handle and closes it when
/// it exits.
#[derive(Debug)]
pub(crate) struct Backend {
    /// The handle as an integer, for `CancelIoEx` from the dropping
    /// thread. Valid until `done` reports the thread has exited.
    handle: usize,
    stop: Arc<AtomicBool>,
    done: Option<mpsc::Receiver<()>>,
}

impl Backend {
    pub(crate) fn spawn(root: &Path, index: usize, shared: Arc<dyn Sink>) -> Option<Backend> {
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the
        // call; the security attributes and template are null, which the
        // API permits.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_ALL,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if h as isize == INVALID_HANDLE_VALUE || h.is_null() {
            return None;
        }
        let handle = h as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let flag = Arc::clone(&stop);
        let spawned = std::thread::Builder::new().name(format!("atlas-watch-{index}")).spawn(move || {
            run(handle as Handle, index, &*shared, &flag);
            // SAFETY: the handle came from CreateFileW above, is owned by
            // this thread, and is closed exactly once, here.
            unsafe { CloseHandle(handle as Handle) };
            let _ = tx.send(());
        });
        if spawned.is_err() {
            // SAFETY: as above; the thread never started, so the handle
            // is closed here instead, once.
            unsafe { CloseHandle(h) };
            return None;
        }
        Some(Backend { handle, stop, done: Some(rx) })
    }

    /// Asks the thread to exit and cancels its blocking read. A cancel
    /// that lands before the thread re-enters the read is retried a few
    /// times; if all miss, the thread exits at the next change under the
    /// root.
    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let Some(done) = self.done.take() else { return };
        for _ in 0..20 {
            // SAFETY: the handle stays open until the thread sends on
            // `done`; a failed cancel (nothing pending) is harmless.
            unsafe { CancelIoEx(self.handle as Handle, std::ptr::null_mut()) };
            match done.recv_timeout(Duration::from_millis(5)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

fn run(h: Handle, index: usize, shared: &dyn Sink, stop: &AtomicBool) {
    let mut buf = vec![0u32; BUF_WORDS];
    while !stop.load(Ordering::SeqCst) {
        let mut got: u32 = 0;
        // SAFETY: `buf` is a DWORD-aligned buffer of the length passed and
        // lives across the call; the call is synchronous (no OVERLAPPED,
        // no completion routine), so nothing writes to `buf` after it
        // returns.
        let ok = unsafe {
            ReadDirectoryChangesW(
                h,
                buf.as_mut_ptr().cast(),
                (buf.len() * 4) as u32,
                1,
                FILTER,
                &mut got,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if ok == 0 {
            // SAFETY: reads this thread's last-error value; no arguments.
            match unsafe { GetLastError() } {
                ERROR_NOTIFY_ENUM_DIR => shared.mark(index, Vec::new(), true),
                ERROR_OPERATION_ABORTED => return,
                _ => {
                    shared.fail(index);
                    return;
                }
            }
            continue;
        }
        if got == 0 {
            // The system dropped the changes: walk.
            shared.mark(index, Vec::new(), true);
            continue;
        }
        let bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_ne_bytes()).take(got as usize).collect();
        shared.mark(index, parse(&bytes), false);
    }
}

/// Decodes a chain of FILE_NOTIFY_INFORMATION records: next-entry
/// offset, action, name length in bytes, then the UTF-16 name.
pub(crate) fn parse(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        let Some(head) = bytes.get(at..at + 12) else { break };
        let next = u32::from_ne_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let len = u32::from_ne_bytes([head[8], head[9], head[10], head[11]]) as usize;
        if let Some(name) = bytes.get(at + 12..at + 12 + len) {
            let units: Vec<u16> = name.chunks_exact(2).map(|c| u16::from_ne_bytes([c[0], c[1]])).collect();
            out.push(String::from_utf16_lossy(&units).replace('\\', "/"));
        }
        if next == 0 {
            break;
        }
        at += next;
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn notify_records_decode() {
        let rec = |next: u32, name: &str| {
            let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_ne_bytes).collect();
            let mut v = Vec::new();
            v.extend(next.to_ne_bytes());
            v.extend(3u32.to_ne_bytes());
            v.extend((units.len() as u32).to_ne_bytes());
            v.extend(units);
            while v.len() % 4 != 0 {
                v.push(0);
            }
            v
        };
        let a = rec(0, "a\\b.md");
        let mut all = rec(a.len() as u32, "a\\b.md");
        all.extend(rec(0, "c é.md"));
        assert_eq!(super::parse(&all), vec!["a/b.md", "c é.md"]);
    }
}
