//! Windows change notification: one thread per root waiting on an
//! overlapped `ReadDirectoryChangesW` over the whole subtree. Every `unsafe`
//! block is a plain FFI call whose invariants are stated beside it.
//!
//! Windows records a directory's changes only from the first
//! `ReadDirectoryChangesW` on its handle; what changed before that call is
//! never reported. So [`Backend::spawn`] returns only once its thread has
//! issued the first read (overlapped, so issuing it does not block):
//! a change made as soon as `spawn` returns is seen, however late a busy
//! machine schedules the thread. Every later read is issued on the same
//! handle, whose buffer keeps what changes between two reads.
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
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
// FILE_NAME | DIR_NAME | SIZE | LAST_WRITE | CREATION.
const FILTER: u32 = 0x0001 | 0x0002 | 0x0008 | 0x0010 | 0x0040;
const ERROR_NOTIFY_ENUM_DIR: u32 = 1022;
const ERROR_OPERATION_ABORTED: u32 = 995;
const ERROR_IO_PENDING: u32 = 997;
/// 64 KiB: the most a network share accepts.
const BUF_WORDS: usize = 16 * 1024;

/// The `OVERLAPPED` structure (its offset fields share a union with a
/// pointer; a directory read uses neither).
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *mut c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
    fn CreateEventW(sa: *mut c_void, manual_reset: i32, initial: i32, name: *const u16) -> Handle;
    fn ReadDirectoryChangesW(
        dir: Handle,
        buf: *mut c_void,
        len: u32,
        subtree: i32,
        filter: u32,
        returned: *mut u32,
        overlapped: *mut Overlapped,
        routine: *mut c_void,
    ) -> i32;
    fn GetOverlappedResult(h: Handle, overlapped: *mut Overlapped, transferred: *mut u32, wait: i32) -> i32;
    fn CancelIoEx(h: Handle, overlapped: *mut c_void) -> i32;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLastError() -> u32;
}

/// A directory handle opened for overlapped reads, the buffer and the
/// `OVERLAPPED` of its one read, and the read's event. Boxed, so the
/// buffer and the `OVERLAPPED` keep their addresses while a read is
/// pending; never dropped with a read pending (see [`run`]).
struct Read {
    dir: Handle,
    ov: Overlapped,
    buf: Vec<u32>,
}

// SAFETY: the handles are process-wide kernel handles, valid on any
// thread; the struct moves to the watching thread whole, before any read
// is issued, and is used there alone.
unsafe impl Send for Read {}

impl Read {
    /// Issues the read. On failure, the error code; nothing is pending
    /// then.
    fn arm(&mut self) -> Result<(), u32> {
        // SAFETY: `buf` (of the length passed) and `ov` live in this boxed
        // struct, which is neither moved nor dropped while the read is
        // pending: [`run`] returns only once no read is pending.
        let ok = unsafe {
            ReadDirectoryChangesW(
                self.dir,
                self.buf.as_mut_ptr().cast(),
                (self.buf.len() * 4) as u32,
                1,
                FILTER,
                std::ptr::null_mut(),
                &mut self.ov,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            return Ok(());
        }
        // SAFETY: reads this thread's last-error value; no arguments.
        match unsafe { GetLastError() } {
            ERROR_IO_PENDING => Ok(()),
            e => Err(e),
        }
    }

    /// Waits for the pending read: the bytes it returned, or its error
    /// code (`ERROR_OPERATION_ABORTED` when cancelled). Nothing is pending
    /// afterwards.
    fn wait(&mut self) -> Result<u32, u32> {
        let mut got: u32 = 0;
        // SAFETY: `ov` is the `OVERLAPPED` of the read pending on `dir`;
        // waiting (the last argument) returns only once it completed.
        let ok = unsafe { GetOverlappedResult(self.dir, &mut self.ov, &mut got, 1) };
        if ok != 0 {
            Ok(got)
        } else {
            // SAFETY: as in `arm`.
            Err(unsafe { GetLastError() })
        }
    }
}

impl Drop for Read {
    fn drop(&mut self) {
        // SAFETY: both handles were opened by `Backend::spawn`, are owned
        // here and are closed exactly once, here; no read is pending.
        unsafe {
            CloseHandle(self.ov.event);
            CloseHandle(self.dir);
        }
    }
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
    /// Opens the root and starts its thread, and returns once the thread
    /// has issued the first read: a change made as soon as this returns is
    /// seen.
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
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if h as isize == INVALID_HANDLE_VALUE || h.is_null() {
            return None;
        }
        // A manual-reset event, unsignalled; each read resets it as it
        // starts.
        // SAFETY: null attributes and name are permitted; the result is
        // checked.
        let event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            // SAFETY: opened above, closed once, here.
            unsafe { CloseHandle(h) };
            return None;
        }
        // From here the handles are the box's: dropping it closes them.
        let mut read = Box::new(Read {
            dir: h,
            ov: Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event },
            buf: vec![0u32; BUF_WORDS],
        });
        let handle = h as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let (armed_tx, armed_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let flag = Arc::clone(&stop);
        let spawned = std::thread::Builder::new().name(format!("atlas-watch-{index}")).spawn(move || {
            let armed = read.arm().is_ok();
            let _ = armed_tx.send(armed);
            if armed {
                run(&mut read, index, &*shared, &flag);
            }
            drop(read);
            let _ = tx.send(());
        });
        // A thread that never started dropped the box with no read issued,
        // closing the handles.
        spawned.ok()?;
        match armed_rx.recv() {
            Ok(true) => Some(Backend { handle, stop, done: Some(rx) }),
            _ => None,
        }
    }

    /// Asks the thread to exit and cancels its pending read. A cancel
    /// that lands before the thread issues its next read is retried a few
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

/// Waits for each read, reports it, and issues the next, until stopped or
/// the watch fails. It returns only when no read is pending: after a wait,
/// or after a read that failed to start.
fn run(read: &mut Read, index: usize, shared: &dyn Sink, stop: &AtomicBool) {
    loop {
        let done = read.wait();
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match done {
            Err(ERROR_NOTIFY_ENUM_DIR) => shared.mark(index, Vec::new(), true),
            Err(ERROR_OPERATION_ABORTED) => return,
            Err(_) => {
                shared.fail(index);
                return;
            }
            // The system dropped the changes: walk.
            Ok(0) => shared.mark(index, Vec::new(), true),
            Ok(got) => {
                let bytes: Vec<u8> = read.buf.iter().flat_map(|w| w.to_ne_bytes()).take(got as usize).collect();
                shared.mark(index, parse(&bytes), false);
            }
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match read.arm() {
            Ok(()) => {}
            // What changed since the last read overflowed: walk, read on.
            Err(ERROR_NOTIFY_ENUM_DIR) => {
                shared.mark(index, Vec::new(), true);
                if read.arm().is_err() {
                    shared.fail(index);
                    return;
                }
            }
            Err(_) => {
                shared.fail(index);
                return;
            }
        }
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
