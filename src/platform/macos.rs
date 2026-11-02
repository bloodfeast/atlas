//! macOS change notification: one FSEvents stream per root, with per-file
//! events (`kFSEventStreamCreateFlagFileEvents`), scheduled on a CFRunLoop
//! that belongs to one thread per root.
//!
//! The bindings are owned `extern "C"` declarations against CoreServices
//! (FSEvents) and CoreFoundation, transcribed from `FSEvents.h`,
//! `CFArray.h`, `CFString.h` and `CFRunLoop.h` of the macOS SDK. Every
//! `unsafe` block states its invariant beside it.
//!
//! Each event path is absolute (the stream is created without
//! `kFSEventStreamCreateFlagUseCFTypes`, so the callback receives a C array
//! of NUL-terminated UTF-8 paths). A path under the root becomes a dirty
//! path relative to it; anything that says events were lost or coalesced
//! above the file level (`MustScanSubDirs`, `UserDropped`, `KernelDropped`,
//! `RootChanged`, `Unmount`, `EventIdsWrapped`), and any path that is not
//! under the root, marks the root for a full walk instead.

use std::ffi::{c_char, c_void, CStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;

use crate::watch::Sink;

type CFIndex = isize;
type CFTimeInterval = f64;
type Boolean = u8;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFRunLoopRef = *mut c_void;
type FSEventStreamRef = *mut c_void;
type ConstFSEventStreamRef = *const c_void;
type FSEventStreamEventId = u64;
type FSEventStreamEventFlags = u32;
type FSEventStreamCreateFlags = u32;

/// `FSEventStreamCallback`.
type Callback = extern "C" fn(
    stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const FSEventStreamEventFlags,
    event_ids: *const FSEventStreamEventId,
);

/// `FSEventStreamContext`: version 0, the info pointer, and three optional
/// callbacks this binding leaves null (the info outlives the stream).
#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

/// `CFArrayCallBacks`: only its address is used, to pass
/// `kCFTypeArrayCallBacks`.
#[repr(C)]
#[allow(dead_code)]
struct CFArrayCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW: FSEventStreamEventId = 0xFFFF_FFFF_FFFF_FFFF;
const K_CREATE_FLAG_NO_DEFER: FSEventStreamCreateFlags = 0x0000_0002;
const K_CREATE_FLAG_WATCH_ROOT: FSEventStreamCreateFlags = 0x0000_0004;
const K_CREATE_FLAG_FILE_EVENTS: FSEventStreamCreateFlags = 0x0000_0010;
const K_EVENT_FLAG_MUST_SCAN_SUB_DIRS: FSEventStreamEventFlags = 0x0000_0001;
const K_EVENT_FLAG_USER_DROPPED: FSEventStreamEventFlags = 0x0000_0002;
const K_EVENT_FLAG_KERNEL_DROPPED: FSEventStreamEventFlags = 0x0000_0004;
const K_EVENT_FLAG_EVENT_IDS_WRAPPED: FSEventStreamEventFlags = 0x0000_0008;
const K_EVENT_FLAG_HISTORY_DONE: FSEventStreamEventFlags = 0x0000_0010;
const K_EVENT_FLAG_ROOT_CHANGED: FSEventStreamEventFlags = 0x0000_0020;
const K_EVENT_FLAG_UNMOUNT: FSEventStreamEventFlags = 0x0000_0080;
/// Flags after which the root's files can no longer be trusted one by one.
const LOST: FSEventStreamEventFlags = K_EVENT_FLAG_MUST_SCAN_SUB_DIRS
    | K_EVENT_FLAG_USER_DROPPED
    | K_EVENT_FLAG_KERNEL_DROPPED
    | K_EVENT_FLAG_EVENT_IDS_WRAPPED
    | K_EVENT_FLAG_ROOT_CHANGED
    | K_EVENT_FLAG_UNMOUNT;
/// Seconds FSEvents may hold events to coalesce them. With `NoDefer` the
/// first event of a burst is delivered at once and the rest within this.
const LATENCY: CFTimeInterval = 0.03;
/// The longest the run loop sleeps before it looks at the stop flag again,
/// in seconds. `CFRunLoopStop` ends a running loop at once; this only bounds
/// a stop that arrives before the loop first runs.
const RUN_SLICE: CFTimeInterval = 0.5;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: u32,
        is_external_representation: Boolean,
    ) -> CFStringRef;
    fn CFArrayCreate(alloc: CFAllocatorRef, values: *const *const c_void, num_values: CFIndex, callbacks: *const CFArrayCallBacks) -> CFArrayRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: CFTimeInterval, return_after_source_handled: Boolean) -> i32;
    fn CFRunLoopStop(rl: CFRunLoopRef);
}

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn FSEventStreamCreate(
        allocator: CFAllocatorRef,
        callback: Callback,
        context: *mut FSEventStreamContext,
        paths_to_watch: CFArrayRef,
        since_when: FSEventStreamEventId,
        latency: CFTimeInterval,
        flags: FSEventStreamCreateFlags,
    ) -> FSEventStreamRef;
    fn FSEventStreamScheduleWithRunLoop(stream: FSEventStreamRef, run_loop: CFRunLoopRef, run_loop_mode: CFStringRef);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> Boolean;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

/// What the callback needs, owned by the watching thread for as long as
/// the stream exists.
struct Ctx {
    /// The root's path bytes, with no trailing `/`.
    root: Vec<u8>,
    index: usize,
    sink: Arc<dyn Sink>,
}

/// One watching thread over one root.
#[derive(Debug)]
pub(crate) struct Backend {
    /// The thread's run loop, as an integer so the backend stays `Send`.
    /// Valid until the thread exits; `CFRunLoopStop` may be called on it
    /// from any thread.
    run_loop: usize,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Backend {
    pub(crate) fn spawn(root: &Path, index: usize, sink: Arc<dyn Sink>) -> Option<Backend> {
        let mut bytes = root.as_os_str().as_bytes().to_vec();
        while bytes.len() > 1 && bytes.ends_with(b"/") {
            bytes.pop();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Option<usize>>();
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name(format!("atlas-watch-{index}"))
            .spawn(move || {
                let ctx = Box::new(Ctx { root: bytes, index, sink });
                let Some(stream) = open_stream(&ctx) else {
                    let _ = tx.send(None);
                    return;
                };
                // SAFETY: called on this thread, which owns its run loop.
                let rl = unsafe { CFRunLoopGetCurrent() };
                let _ = tx.send(Some(rl as usize));
                while !thread_stop.load(Ordering::Acquire) {
                    // SAFETY: `kCFRunLoopDefaultMode` is a constant string
                    // exported by CoreFoundation; the loop is this thread's.
                    unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, RUN_SLICE, 0) };
                }
                // SAFETY: `stream` was created, scheduled and started on
                // this thread and is torn down once, in the documented order
                // (stop, invalidate, release); no callback runs after
                // `FSEventStreamInvalidate` returns, so `ctx` is dropped
                // only after it.
                unsafe {
                    FSEventStreamStop(stream);
                    FSEventStreamInvalidate(stream);
                    FSEventStreamRelease(stream);
                }
                drop(ctx);
            })
            .ok()?;
        match rx.recv() {
            Ok(Some(run_loop)) => Some(Backend { run_loop, stop, thread: Some(thread) }),
            _ => {
                let _ = thread.join();
                None
            }
        }
    }

    /// Sets the stop flag, stops the run loop and waits for the thread.
    pub(crate) fn stop(&mut self) {
        let Some(t) = self.thread.take() else { return };
        self.stop.store(true, Ordering::Release);
        // SAFETY: `run_loop` is the watching thread's run loop, alive until
        // that thread exits, which `join` below waits for; `CFRunLoopStop`
        // is documented as callable from any thread.
        unsafe { CFRunLoopStop(self.run_loop as CFRunLoopRef) };
        let _ = t.join();
    }
}

/// Creates, schedules on this thread's run loop and starts a stream over
/// `ctx.root`. `ctx` must outlive the stream.
fn open_stream(ctx: &Ctx) -> Option<FSEventStreamRef> {
    // SAFETY: `ctx.root` is readable for its length; the result is checked
    // and released below.
    let path = unsafe {
        CFStringCreateWithBytes(std::ptr::null(), ctx.root.as_ptr(), ctx.root.len() as CFIndex, K_CF_STRING_ENCODING_UTF8, 0)
    };
    if path.is_null() {
        return None;
    }
    let values = [path];
    // SAFETY: `values` holds one live CFString; `kCFTypeArrayCallBacks`
    // makes the array retain it, so it can be released right after.
    let paths = unsafe { CFArrayCreate(std::ptr::null(), values.as_ptr(), 1, &kCFTypeArrayCallBacks) };
    // SAFETY: `path` was created above and is released once; the array
    // holds its own reference.
    unsafe { CFRelease(path) };
    if paths.is_null() {
        return None;
    }
    let mut context = FSEventStreamContext {
        version: 0,
        info: ctx as *const Ctx as *mut c_void,
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
    };
    // SAFETY: the context is copied by the call; its `info` points at a
    // `Ctx` the caller keeps alive until the stream is invalidated; `paths`
    // is a live CFArray of CFStrings.
    let stream = unsafe {
        FSEventStreamCreate(
            std::ptr::null(),
            on_events,
            &mut context,
            paths,
            K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
            LATENCY,
            K_CREATE_FLAG_FILE_EVENTS | K_CREATE_FLAG_NO_DEFER | K_CREATE_FLAG_WATCH_ROOT,
        )
    };
    // SAFETY: the stream keeps its own copy of the paths.
    unsafe { CFRelease(paths) };
    if stream.is_null() {
        return None;
    }
    // SAFETY: `stream` is live; the run loop is this thread's and the mode
    // a CoreFoundation constant.
    let started = unsafe {
        FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
        FSEventStreamStart(stream)
    };
    if started == 0 {
        // SAFETY: a scheduled stream that failed to start is invalidated and
        // released once.
        unsafe {
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);
        }
        return None;
    }
    Some(stream)
}

/// The stream callback: runs on the watching thread, inside its run loop.
extern "C" fn on_events(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const FSEventStreamEventFlags,
    _event_ids: *const FSEventStreamEventId,
) {
    if info.is_null() || event_paths.is_null() || event_flags.is_null() || num_events == 0 {
        return;
    }
    // SAFETY: `info` is the `Ctx` passed at creation, alive until the stream
    // is invalidated, and only read here. Without UseCFTypes, `event_paths`
    // is an array of `num_events` NUL-terminated C strings and `event_flags`
    // an array of `num_events` flags, both valid for this call.
    let (ctx, paths, flags) = unsafe {
        (
            &*(info as *const Ctx),
            std::slice::from_raw_parts(event_paths as *const *const c_char, num_events),
            std::slice::from_raw_parts(event_flags, num_events),
        )
    };
    let mut out = Vec::new();
    let mut overflow = false;
    for (&p, &f) in paths.iter().zip(flags) {
        if f & LOST != 0 {
            overflow = true;
            continue;
        }
        if f & K_EVENT_FLAG_HISTORY_DONE != 0 || p.is_null() {
            continue;
        }
        // SAFETY: a non-null entry of the path array is a NUL-terminated
        // string valid for this call.
        let abs = unsafe { CStr::from_ptr(p) }.to_bytes();
        match relative(&ctx.root, abs) {
            Rel::Inside(rel) => {
                if !rel.split('/').any(|c| c.starts_with('.')) {
                    out.push(rel);
                }
            }
            Rel::Root | Rel::Outside => overflow = true,
        }
    }
    if overflow || !out.is_empty() {
        ctx.sink.mark(ctx.index, out, overflow);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Rel {
    Inside(String),
    Root,
    Outside,
}

/// Where an event path stands relative to the root. FSEvents reports the
/// path as the file system spells it, so a root spelt with another case on
/// a case-insensitive volume still matches, ASCII-case-insensitively.
fn relative(root: &[u8], abs: &[u8]) -> Rel {
    let abs = abs.strip_suffix(b"/").unwrap_or(abs);
    if abs.len() < root.len() || !abs[..root.len()].eq_ignore_ascii_case(root) {
        return Rel::Outside;
    }
    let rest = &abs[root.len()..];
    if rest.is_empty() {
        return Rel::Root;
    }
    let rest = if root == b"/" { rest } else if let Some(r) = rest.strip_prefix(b"/") { r } else { return Rel::Outside };
    match std::str::from_utf8(rest) {
        Ok(s) if !s.is_empty() => Rel::Inside(s.to_string()),
        _ => Rel::Outside,
    }
}

#[cfg(test)]
mod tests {
    use super::{relative, Rel};

    #[test]
    fn paths_resolve_against_the_root() {
        let root = b"/private/var/folders/x/atlas";
        assert_eq!(relative(root, b"/private/var/folders/x/atlas/sub/n.md"), Rel::Inside("sub/n.md".into()));
        assert_eq!(relative(root, b"/private/var/folders/x/atlas"), Rel::Root);
        assert_eq!(relative(root, b"/private/var/folders/x/atlas/"), Rel::Root);
        assert_eq!(relative(root, b"/private/var/folders/x/atlas2/n.md"), Rel::Outside);
        assert_eq!(relative(root, b"/elsewhere/n.md"), Rel::Outside);
        assert_eq!(relative(root, b"/Private/var/folders/x/Atlas/n.md"), Rel::Inside("n.md".into()));
    }
}
