//! The last-moment catch (#221): when the machine shuts down, the user logs
//! off or the terminal closes, the resident server writes the restart point
//! it keeps prepared ([`prepare`]) to one file ([`arm`]), and logs what it
//! saw. Best effort: a failed write is a log line, never a delay, and the
//! catch returns at once so the default action goes on.
//!
//! * Windows: `SetConsoleCtrlHandler` for `CTRL_CLOSE_EVENT`,
//!   `CTRL_LOGOFF_EVENT` and `CTRL_SHUTDOWN_EVENT`. A process that loads
//!   user32 (or has no console) no longer gets the last two, so then a
//!   hidden top-level window also takes `WM_QUERYENDSESSION` and
//!   `WM_ENDSESSION` (a message-only window gets no broadcast). user32 is
//!   loaded at run time, only for that window: importing it would turn the
//!   console events off for every atlas.
//! * Linux and macOS, one module: `sigaction` for SIGTERM and SIGHUP. The
//!   handler counts the signal in an atomic ([`caught`]) and writes one byte
//!   to a self-pipe; a normal thread writes the file, then restores the
//!   default action and raises the signal again, so the process ends by it
//!   as it would have.
//!
//! A shutdown often closes the server's stdin at the same moment (Claude
//! Code dies too). The end-of-input path then waits, bounded, for a catch
//! that has begun ([`wait_for_catch`]), so the process does not exit under
//! the catch's write.
//!
//! Every `unsafe` block is a plain FFI call whose invariants are stated
//! beside it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Where the catch writes, and what.
struct Slot {
    point: PathBuf,
    log: PathBuf,
    bytes: Option<Vec<u8>>,
}

static SLOT: Mutex<Option<Slot>> = Mutex::new(None);
/// Signalled whenever [`prepare`] sets the bytes (tests wait on it).
static PREPARED: Condvar = Condvar::new();
/// When the server's stdin closed (ms since the epoch), 0 while open.
static STDIN_CLOSED_MS: AtomicU64 = AtomicU64::new(0);
/// Catches begun: counted by the signal or console handler before anything
/// else (an atomic add, async-signal-safe).
static CAUGHT: AtomicU32 = AtomicU32::new(0);
/// Catches finished (written or given up), with [`FINISHED`] signalled.
static DONE: AtomicU32 = AtomicU32::new(0);
static DONE_LOCK: Mutex<()> = Mutex::new(());
static FINISHED: Condvar = Condvar::new();
/// How long the end-of-input path waits for a catch that has begun.
pub(crate) const CATCH_WAIT: Duration = Duration::from_secs(2);
/// The log is cut back to empty past this size.
const LOG_MAX: u64 = 256 * 1024;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn slot() -> std::sync::MutexGuard<'static, Option<Slot>> {
    SLOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sets the file the catch writes and the log it appends to. Prepared bytes
/// are kept.
pub(crate) fn arm(point: PathBuf, log: PathBuf) {
    let mut s = slot();
    let bytes = s.take().and_then(|s| s.bytes);
    *s = Some(Slot { point, log, bytes });
}

/// Sets what the catch writes: the restart point's bytes, or `None` while no
/// agent runs (then the catch writes nothing). While a restart is pending
/// ([`hold`]) the point file follows at once: written, or removed when no
/// agent runs.
pub(crate) fn prepare(bytes: Option<Vec<u8>>) {
    if let Some(s) = slot().as_mut() {
        s.bytes = bytes;
        if HELD.load(Ordering::SeqCst) {
            write_through(s);
        }
    }
    PREPARED.notify_all();
}

/// A restart is pending (#221, the advance warning): the point file is kept
/// on disk and current, so a restart the catch misses still leaves it.
static HELD: AtomicBool = AtomicBool::new(false);

/// What a write while a restart is pending adds to the point: it was kept
/// current, not written by a catch (`crate::restart::Point::held`), so the
/// next start does not take its time for when the machine went down.
const HELD_MARK: &str = r#""held":true"#;
/// What a plain end of input adds while a restart is pending: the server
/// ended, the machine did not go down then (#221 S2).
const ENDED_MARK: &str = r#""held":true,"ended":"stdin closed""#;

/// `bytes`, a JSON object, with the members `marks` added before its
/// closing brace; as they are when there is no closing brace.
fn marked(bytes: &[u8], marks: &str) -> Vec<u8> {
    let Some(end) = bytes.iter().rposition(|&b| b == b'}') else { return bytes.to_vec() };
    let empty = bytes[..end].iter().rev().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
    let mut out = bytes[..end].to_vec();
    if !empty {
        out.push(b',');
    }
    out.extend_from_slice(marks.as_bytes());
    out.extend_from_slice(&bytes[end..]);
    out
}

/// Writes the slot's bytes, marked as kept ([`HELD_MARK`]), to the point
/// file, or removes the file when there are none; never once a catch has
/// begun (the catch's write stands). The outcome, when it is not a plain
/// rewrite.
fn write_through(s: &Slot) -> Option<String> {
    if CAUGHT.load(Ordering::SeqCst) > 0 {
        return None;
    }
    match &s.bytes {
        Some(b) => match std::fs::write(&s.point, marked(b, HELD_MARK)) {
            Ok(()) => None,
            Err(e) => Some(format!("restart point write failed: {}: {e}", s.point.display())),
        },
        None => match std::fs::remove_file(&s.point) {
            Ok(()) => Some("no agents running: restart point removed".to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some(format!("restart point removal failed: {}: {e}", s.point.display())),
        },
    }
}

/// A restart became pending (`on`) or no longer is: while pending, the
/// point file is written now and follows every [`prepare`]; when it clears,
/// the file is removed (unless a catch has begun). `cause` and the outcome
/// go to the log.
pub(crate) fn hold(on: bool, cause: &str) {
    let s = slot();
    let Some(s) = s.as_ref() else { return };
    HELD.store(on, Ordering::SeqCst);
    let outcome = if on {
        match write_through(s) {
            Some(o) => o,
            None if CAUGHT.load(Ordering::SeqCst) > 0 => "a catch has begun: left as it is".to_string(),
            None if s.bytes.is_some() => format!("restart point written to {} and kept current", s.point.display()),
            None => "no agents running: nothing written; kept current from now".to_string(),
        }
    } else if CAUGHT.load(Ordering::SeqCst) > 0 {
        "a catch has begun: left as it is".to_string()
    } else {
        match std::fs::remove_file(&s.point) {
            Ok(()) => "restart point removed".to_string(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "no restart point to remove".to_string(),
            Err(e) => format!("restart point removal failed: {}: {e}", s.point.display()),
        }
    };
    append(&s.log, &format!("{cause}; {outcome}"));
}

/// Waits up to `timeout` for the prepared bytes (`None` while nothing is
/// prepared or the slot is not armed) to satisfy `pred`; whether they did.
/// Tests only: it waits on [`prepare`], it never sleeps.
pub(crate) fn wait_prepared(timeout: Duration, mut pred: impl FnMut(Option<&[u8]>) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    let mut g = slot();
    loop {
        if pred(g.as_ref().and_then(|s| s.bytes.as_deref())) {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        g = PREPARED.wait_timeout(g, left).map_or_else(|e| e.into_inner().0, |r| r.0);
    }
}

fn append(log: &std::path::Path, line: &str) {
    use std::io::Write;
    let fresh = std::fs::metadata(log).is_ok_and(|m| m.len() > LOG_MAX);
    let f = std::fs::OpenOptions::new().create(true).append(!fresh).write(true).truncate(fresh).open(log);
    // A failed log line is dropped: nothing in the catch may wait on a
    // stderr pipe (the slot's lock is held here).
    let _ = f.and_then(|mut f| f.write_all(format!("{} pid {} {line}\n", now_ms(), std::process::id()).as_bytes()));
}

/// Appends one line to the catch's log (when armed).
pub(crate) fn log(line: &str) {
    if let Some(s) = slot().as_ref() {
        append(&s.log, line);
    }
}

/// The server's stdin closed: noted with its time, for the order of stdin
/// close and shutdown signal (ruling journal:ATLAS-RELEASE:0070, Q10). A
/// plain end of input writes no point (Q10); but while a restart is pending
/// the kept point is on disk, so it is rewritten marked ended
/// ([`ENDED_MARK`]), unless a catch has begun: the next start takes it only
/// if the machine booted since, and never as a caught shutdown (#221 S2).
/// A catch that has begun is waited for ([`wait_for_catch`]), since the
/// process exits next.
pub(crate) fn stdin_closed() {
    STDIN_CLOSED_MS.store(now_ms(), Ordering::SeqCst);
    if let Some(s) = slot().as_ref() {
        let held = HELD.load(Ordering::SeqCst) && CAUGHT.load(Ordering::SeqCst) == 0;
        let outcome = match (&s.bytes, held) {
            (Some(b), true) => match std::fs::write(&s.point, marked(b, ENDED_MARK)) {
                Ok(()) => format!("; restart pending: restart point at {} marked ended", s.point.display()),
                Err(e) => format!("; restart point write failed: {}: {e}", s.point.display()),
            },
            _ => String::new(),
        };
        append(&s.log, &format!("stdin closed{outcome}"));
    }
    wait_for_catch(CATCH_WAIT);
}

/// A catch has begun: called first by the signal or console handler.
/// Async-signal-safe (one atomic add).
pub(crate) fn caught() {
    CAUGHT.fetch_add(1, Ordering::SeqCst);
}

/// Waits up to `timeout` while a catch has begun and not finished (on Unix
/// the catch then ends the process by its signal). Returns at once when no
/// catch has begun. Whether every begun catch finished.
pub(crate) fn wait_for_catch(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut g = DONE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if DONE.load(Ordering::SeqCst) >= CAUGHT.load(Ordering::SeqCst) {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        g = FINISHED.wait_timeout(g, left).map_or_else(|e| e.into_inner().0, |r| r.0);
    }
}

/// A begun catch finished: counted, and any waiter woken.
fn finished() {
    let _g = DONE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    DONE.fetch_add(1, Ordering::SeqCst);
    FINISHED.notify_all();
}

/// Tests: run before the catch writes (to hold it mid-catch).
#[cfg(test)]
pub(crate) static BEFORE_WRITE: Mutex<Option<fn()>> = Mutex::new(None);

/// The catch: writes the prepared bytes, if any, to the point file in one
/// write, and logs `cause`, the stdin state and the outcome. True when the
/// file was written. Never waits on anything but the slot's lock, which no
/// one holds for longer than a small file write. Counts as finished for
/// [`wait_for_catch`] once it returns. (Unix counts it finished only after
/// its re-raise, so its thread calls the parts.)
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn catch(cause: &str) -> bool {
    let wrote = catch_inner(cause);
    finished();
    wrote
}

fn catch_inner(cause: &str) -> bool {
    #[cfg(test)]
    if let Some(f) = *BEFORE_WRITE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
        f();
    }
    let s = slot();
    let Some(s) = s.as_ref() else { return false };
    let stdin = match STDIN_CLOSED_MS.load(Ordering::SeqCst) {
        0 => "stdin open".to_string(),
        t => format!("stdin closed {} ms before", now_ms().saturating_sub(t)),
    };
    let (wrote, outcome) = match &s.bytes {
        None => (false, "no agents running: nothing written".to_string()),
        Some(b) => match std::fs::write(&s.point, b) {
            Ok(()) => (true, format!("restart point written to {}", s.point.display())),
            Err(e) => (false, format!("restart point write failed: {}: {e}", s.point.display())),
        },
    };
    append(&s.log, &format!("{cause}; {stdin}; {outcome}"));
    wrote
}

/// An end of session was cancelled after the catch: the point file is
/// removed (it is this server's own) and the outcome logged. One unlink.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn cancelled(cause: &str) {
    let s = slot();
    let Some(s) = s.as_ref() else { return };
    let outcome = match std::fs::remove_file(&s.point) {
        Ok(()) => "restart point removed".to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "no restart point to remove".to_string(),
        Err(e) => format!("restart point removal failed: {}: {e}", s.point.display()),
    };
    append(&s.log, &format!("{cause}; {outcome}"));
}

/// What [`install`] set up, for the server's log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Status {
    /// Windows: whether the process has a console.
    pub console: Option<bool>,
    /// Windows: whether user32 (or gdi32) was loaded before the catch.
    pub user32: Option<bool>,
    /// The console handler (Windows) or the signal handlers (Unix) are on.
    pub handler: bool,
    /// Windows: the hidden end-session window runs.
    pub window: bool,
}

impl Status {
    /// One line for the log.
    pub(crate) fn line(&self) -> String {
        let yn = |b: Option<bool>| match b {
            Some(true) => "yes",
            Some(false) => "no",
            None => "n/a",
        };
        if cfg!(windows) {
            format!(
                "catch installed: console {}, user32 {}, console handler {}, end-session window {}",
                yn(self.console),
                yn(self.user32),
                if self.handler { "on" } else { "off" },
                if self.window { "on" } else { "off" }
            )
        } else {
            format!("catch installed: SIGTERM and SIGHUP {}", if self.handler { "caught" } else { "not caught" })
        }
    }
}

/// Installs the catch for this platform (once per process; again is a
/// no-op that reports the same).
pub(crate) fn install() -> Status {
    static DONE: Mutex<Option<Status>> = Mutex::new(None);
    let mut d = DONE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(s) = d.as_ref() {
        return s.clone();
    }
    let s = sys::install();
    *d = Some(s.clone());
    s
}

#[cfg(all(windows, test))]
pub(crate) use sys::{on_ctrl, on_message, wants_window};

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Status;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;
    const WM_QUERYENDSESSION: u32 = 0x0011;
    const WM_ENDSESSION: u32 = 0x0016;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The console handler: a closed console, a logoff or a shutdown is
    /// caught, then passed on (0) so the default handler ends the process.
    pub(crate) extern "system" fn on_ctrl(kind: u32) -> i32 {
        let cause = match kind {
            CTRL_CLOSE_EVENT => "CTRL_CLOSE",
            CTRL_LOGOFF_EVENT => "CTRL_LOGOFF",
            CTRL_SHUTDOWN_EVENT => "CTRL_SHUTDOWN",
            _ => return 0,
        };
        super::caught();
        super::catch(cause);
        0
    }

    /// Whether this process has a console.
    fn has_console() -> bool {
        let mut list = [0u32; 4];
        // SAFETY: the buffer holds the 4 entries passed; the call fails
        // (returns 0) when the process has no console.
        unsafe { GetConsoleProcessList(list.as_mut_ptr(), 4) != 0 }
    }

    /// Whether user32 or gdi32 is already loaded (the console then gets no
    /// logoff or shutdown event). Loads nothing.
    fn user32_loaded() -> bool {
        ["user32.dll", "gdi32.dll"].iter().any(|m| {
            let w = wide(m);
            // SAFETY: `w` is NUL-terminated and outlives the call, which only
            // looks the module up.
            !unsafe { GetModuleHandleW(w.as_ptr()) }.is_null()
        })
    }

    /// Whether the end-session window is needed: no console (no console
    /// event reaches the process), or user32 loaded (the console gets no
    /// logoff or shutdown event). Ruling journal:ATLAS-RELEASE:0070 Q9.
    pub(crate) fn wants_window(console: bool, user32: bool) -> bool {
        !console || user32
    }

    pub(super) fn install() -> Status {
        let console = has_console();
        let user32 = user32_loaded();
        // SAFETY: `on_ctrl` is an `extern "system"` function that lives for
        // the whole process.
        let handler = unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) } != 0;
        let window = wants_window(console, user32) && spawn_window();
        Status { console: Some(console), user32: Some(user32), handler, window }
    }

    type WndProc = unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize;

    /// `WNDCLASSW`.
    #[repr(C)]
    struct WndClass {
        style: u32,
        proc_: WndProc,
        cls_extra: i32,
        wnd_extra: i32,
        instance: *mut c_void,
        icon: *mut c_void,
        cursor: *mut c_void,
        background: *mut c_void,
        menu_name: *const u16,
        class_name: *const u16,
    }

    /// `MSG`, with room to spare.
    #[repr(C)]
    struct Msg {
        hwnd: *mut c_void,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        x: i32,
        y: i32,
        private: u32,
        spare: [u64; 2],
    }

    type RegisterClassW = unsafe extern "system" fn(*const WndClass) -> u16;
    type CreateWindowExW = unsafe extern "system" fn(
        u32,
        *const u16,
        *const u16,
        u32,
        i32,
        i32,
        i32,
        i32,
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *mut c_void,
    ) -> *mut c_void;
    type GetMessageW = unsafe extern "system" fn(*mut Msg, *mut c_void, u32, u32) -> i32;
    type DispatchMessageW = unsafe extern "system" fn(*const Msg) -> isize;

    /// `DefWindowProcW`, once user32 is loaded.
    static DEF_PROC: AtomicUsize = AtomicUsize::new(0);

    /// The end-session messages' handling: the answer, or `None` for any
    /// other message (the default window procedure's).
    pub(crate) fn on_message(msg: u32, w: usize) -> Option<isize> {
        match msg {
            // Caught at the query, the earliest word of an end of session;
            // the answer lets it go on.
            WM_QUERYENDSESSION => {
                super::caught();
                super::catch("WM_QUERYENDSESSION");
                Some(1)
            }
            WM_ENDSESSION if w != 0 => {
                super::caught();
                super::catch("WM_ENDSESSION");
                Some(0)
            }
            // The end of session was cancelled: the session goes on, so the
            // point the query wrote must not later read as a shutdown.
            WM_ENDSESSION => {
                super::cancelled("WM_ENDSESSION cancelled");
                Some(0)
            }
            _ => None,
        }
    }

    unsafe extern "system" fn wnd_proc(h: *mut c_void, msg: u32, w: usize, l: isize) -> isize {
        if let Some(r) = on_message(msg, w) {
            return r;
        }
        match DEF_PROC.load(Ordering::SeqCst) {
            0 => 0,
            // SAFETY: the address is `DefWindowProcW`'s, read from the loaded
            // user32 before the window existed, with this signature.
            p => unsafe { std::mem::transmute::<usize, WndProc>(p)(h, msg, w, l) },
        }
    }

    /// Starts the hidden top-level window on its own thread, with its
    /// message loop. True once the window exists.
    fn spawn_window() -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("atlas-endsession".into()).spawn(move || {
            let lib = wide("user32.dll");
            // SAFETY: `lib` is NUL-terminated; the module stays loaded for
            // the process's life (never freed). Each address is checked
            // before it is turned into the function type its name documents.
            unsafe {
                let m = LoadLibraryW(lib.as_ptr());
                if m.is_null() {
                    let _ = tx.send(false);
                    return;
                }
                let get = |n: &[u8]| GetProcAddress(m, n.as_ptr()) as usize;
                let (reg, create, getmsg, dispatch, def) = (
                    get(b"RegisterClassW\0"),
                    get(b"CreateWindowExW\0"),
                    get(b"GetMessageW\0"),
                    get(b"DispatchMessageW\0"),
                    get(b"DefWindowProcW\0"),
                );
                if [reg, create, getmsg, dispatch, def].contains(&0) {
                    let _ = tx.send(false);
                    return;
                }
                DEF_PROC.store(def, Ordering::SeqCst);
                let reg = std::mem::transmute::<usize, RegisterClassW>(reg);
                let create = std::mem::transmute::<usize, CreateWindowExW>(create);
                let getmsg = std::mem::transmute::<usize, GetMessageW>(getmsg);
                let dispatch = std::mem::transmute::<usize, DispatchMessageW>(dispatch);
                let name = wide("atlas-endsession");
                let instance = GetModuleHandleW(std::ptr::null());
                let class = WndClass {
                    style: 0,
                    proc_: wnd_proc,
                    cls_extra: 0,
                    wnd_extra: 0,
                    instance,
                    icon: std::ptr::null_mut(),
                    cursor: std::ptr::null_mut(),
                    background: std::ptr::null_mut(),
                    menu_name: std::ptr::null(),
                    class_name: name.as_ptr(),
                };
                if reg(&class) == 0 {
                    let _ = tx.send(false);
                    return;
                }
                // Top-level (no parent) and never shown: a message-only
                // window gets no WM_QUERYENDSESSION.
                let hwnd = create(
                    0,
                    name.as_ptr(),
                    name.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null_mut(),
                );
                if hwnd.is_null() {
                    let _ = tx.send(false);
                    return;
                }
                let _ = tx.send(true);
                let mut msg: Msg = std::mem::zeroed();
                while getmsg(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    dispatch(&msg);
                }
            }
        });
        spawned.is_ok() && rx.recv().unwrap_or(false)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod sys {
    use std::os::raw::{c_int, c_void};
    use std::sync::atomic::{AtomicI32, Ordering};

    use super::Status;

    /// glibc's and musl's `struct sigaction` (x86_64 and aarch64).
    #[cfg(target_os = "linux")]
    #[repr(C)]
    struct SigAction {
        handler: usize,
        mask: [u64; 16],
        flags: c_int,
        restorer: usize,
    }

    /// macOS's `struct sigaction`.
    #[cfg(target_os = "macos")]
    #[repr(C)]
    struct SigAction {
        handler: usize,
        mask: u32,
        flags: c_int,
    }

    #[cfg(target_os = "linux")]
    const SA_RESTART: c_int = 0x1000_0000;
    #[cfg(target_os = "macos")]
    const SA_RESTART: c_int = 0x0002;
    const SIGHUP: c_int = 1;
    const SIGTERM: c_int = 15;
    const SIG_DFL: usize = 0;
    const SIG_IGN: usize = 1;
    const F_SETFD: c_int = 2;
    const FD_CLOEXEC: c_int = 1;

    extern "C" {
        fn sigaction(sig: c_int, act: *const SigAction, old: *mut SigAction) -> c_int;
        fn pipe(fds: *mut c_int) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int;
        fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
        fn raise(sig: c_int) -> c_int;
    }

    /// The self-pipe's write end, -1 until installed.
    static PIPE_W: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn on_signal(sig: c_int) {
        // Async-signal-safe: an atomic add, an atomic load and one write(2)
        // of a byte from the stack. A full pipe drops the byte; one is
        // enough.
        super::caught();
        let b = sig as u8;
        let fd = PIPE_W.load(Ordering::SeqCst);
        if fd >= 0 {
            // SAFETY: `fd` is the pipe's open write end (never closed); one
            // byte from a live local.
            unsafe { write(fd, (&raw const b).cast(), 1) };
        }
    }

    fn action(handler: usize) -> SigAction {
        // SAFETY: a plain C struct for which all zeroes is valid (an empty
        // mask, no flags).
        let mut a: SigAction = unsafe { std::mem::zeroed() };
        a.handler = handler;
        a.flags = SA_RESTART;
        a
    }

    pub(super) fn install() -> Status {
        let mut fds = [0 as c_int; 2];
        // SAFETY: `fds` has room for the two descriptors pipe writes; fcntl
        // takes plain integers.
        let ok = unsafe { pipe(fds.as_mut_ptr()) } == 0;
        if !ok {
            return Status::default();
        }
        // SAFETY: as above.
        unsafe {
            fcntl(fds[0], F_SETFD, FD_CLOEXEC);
            fcntl(fds[1], F_SETFD, FD_CLOEXEC);
        }
        let read_end = fds[0];
        let thread = std::thread::Builder::new().name("atlas-catch".into()).spawn(move || loop {
            let mut b = 0u8;
            // SAFETY: one byte into a live local from the pipe's read end,
            // owned by this thread.
            let n = unsafe { read(read_end, (&raw mut b).cast(), 1) };
            if n < 0 {
                continue; // EINTR
            }
            if n == 0 {
                return;
            }
            let sig = c_int::from(b);
            super::catch_inner(if sig == SIGTERM { "SIGTERM" } else { "SIGHUP" });
            // The default action again, then the signal again: the process
            // ends by it, as it would have without the catch. Counted as
            // finished only after: a waiter at the end of input must not
            // exit 0 first.
            // SAFETY: a default disposition from a zeroed struct; raise
            // takes a plain integer.
            unsafe {
                let dfl = action(SIG_DFL);
                sigaction(sig, &dfl, std::ptr::null_mut());
                raise(sig);
            }
            super::finished();
        });
        if thread.is_err() {
            return Status::default();
        }
        PIPE_W.store(fds[1], Ordering::SeqCst);
        let mut handler = true;
        for sig in [SIGHUP, SIGTERM] {
            let act = action(on_signal as extern "C" fn(c_int) as usize);
            // SAFETY: `act` and `old` are plain C structs of the platform's
            // layout that live across the call; `on_signal` lives for the
            // whole process and is async-signal-safe.
            unsafe {
                let mut old: SigAction = std::mem::zeroed();
                if sigaction(sig, &act, &mut old) != 0 {
                    handler = false;
                    continue;
                }
                // An ignored SIGHUP (`nohup`) stays ignored.
                if sig == SIGHUP && old.handler == SIG_IGN {
                    sigaction(sig, &old, std::ptr::null_mut());
                }
            }
        }
        Status { console: None, user32: None, handler, window: false }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod sys {
    /// No catch on this platform.
    pub(super) fn install() -> super::Status {
        super::Status::default()
    }
}
